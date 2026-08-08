use std::array;
use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use sampler_audio::{SampleBuffer, Telemetry, TransportStamp};
use sampler_core::pad::{BANK_COUNT, PADS_PER_BANK};
use sampler_core::{
    BankId, PadId, PadSettings, PatternSlotId, PlaybackMode, ProjectDocument, ProjectId,
    SampleEditRecipe,
};

use crate::PatternSwitch;
use crate::audio::{AudioPort, open_default_audio};
use crate::file_picker::FilePicker;
use crate::input::{InputAction, KeyboardCapabilities, map_key};
use crate::loader::{
    EditPreview, LoadPurpose, MAX_DIRECTORY_ENTRIES, ProjectSaveWorkerRequest, ProjectToken,
    RenderedSample, StageProjectSampleRequest, WORKER_CHANNEL_CAPACITY, WorkerRequest,
    WorkerResult, WorkerSendError,
};
use crate::palette::{LineEditor, PaletteCommand, parse_palette};
use crate::pattern::{PatternStatus, PatternWorkspace, WorkspaceView};
use crate::project_session::{
    ProjectOpenError, ProjectOpenPhase, ProjectOpenStage, ProjectSession, ProjectSnapshotError,
    ProjectStageError, RecoveryChoice,
};
use crate::project_store::{
    ProjectProbe, ProjectSavePad, ProjectSaveRequest, ProjectSaveSnapshot, ProjectStoreError,
    SaveKind, SaveReceipt, SourceFingerprint,
};
use crate::sample_editor::{
    SampleEditor, SampleEditorContext, SampleEditorError, SampleEditorIntent, SampleMarker,
};

pub const PAD_VIEW_COUNT: usize = 160;
/// Fixed worker-generated waveform resolution. Perform uses a bounded 64-column projection.
pub const EDIT_PREVIEW_COLUMNS: usize = 1_024;
pub const PREVIEW_COLUMNS: usize = 64;
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSaveError {
    Untitled,
    OperationPending,
    Snapshot(ProjectSnapshotError),
    Entropy(String),
    TokenExhausted,
}

impl fmt::Display for ProjectSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Untitled => formatter.write_str("untitled project requires Save As"),
            Self::OperationPending => formatter.write_str("a project operation is already pending"),
            Self::Snapshot(error) => error.fmt(formatter),
            Self::Entropy(error) => {
                write!(formatter, "could not generate project identity: {error}")
            }
            Self::TokenExhausted => formatter.write_str("project operation token is exhausted"),
        }
    }
}

impl std::error::Error for ProjectSaveError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSaveFailure {
    pub kind: SaveKind,
    pub error: ProjectStoreError,
}

#[derive(Debug, Clone)]
struct PendingProjectSave {
    descriptor: crate::ProjectOperationDescriptor,
    snapshot: ProjectSaveSnapshot,
    save_as: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryCleanup {
    token: ProjectToken,
    directory: PathBuf,
    project_id: ProjectId,
    revision: u64,
}

#[derive(Debug, Clone)]
enum InFlightProjectOperation {
    Save(PendingProjectSave),
    Cleanup(RecoveryCleanup),
}

struct StagedProjectPad {
    path: PathBuf,
    settings: PadSettings,
    loaded: crate::LoadedSample,
}

enum ProjectAdmission {
    StopAll,
    Pads(usize),
    Patterns(usize),
    Complete,
}

struct ProjectOpenCandidate {
    progress: ProjectOpenStage,
    document: ProjectDocument,
    patterns: PatternWorkspace,
    staged_pads: [Option<Box<StagedProjectPad>>; PAD_VIEW_COUNT],
    next_decode: usize,
    decode_in_flight: Option<PadId>,
    saved_revision: u64,
    restored_recovery: bool,
    admission: ProjectAdmission,
}

enum ProjectOpenOperation {
    Probing {
        progress: ProjectOpenStage,
        worker_queued: bool,
    },
    ChoosingRecovery(Box<ProjectRecoveryChoiceState>),
    Staging(Box<ProjectOpenCandidate>),
}

struct ProjectRecoveryChoiceState {
    progress: ProjectOpenStage,
    explicit: Option<ProjectDocument>,
    recovery: ProjectDocument,
    discard_requested: bool,
    discard_queued: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewColumn {
    pub min: i8,
    pub max: i8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadLoadState {
    Empty,
    WaitingForDevice,
    Loading,
    Ready,
    Error(String),
}

pub struct PadView {
    pub source: Option<PathBuf>,
    pub label: String,
    pub settings: PadSettings,
    pub generation: u64,
    pub state: PadLoadState,
    pub sample: Option<Arc<SampleBuffer>>,
    pub preview: [PreviewColumn; PREVIEW_COLUMNS],
    pub active: bool,
}

impl Default for PadView {
    fn default() -> Self {
        Self {
            source: None,
            label: String::new(),
            settings: PadSettings::default(),
            generation: 0,
            state: PadLoadState::Empty,
            sample: None,
            preview: [PreviewColumn::default(); PREVIEW_COLUMNS],
            active: false,
        }
    }
}

enum PendingLoadPhase {
    AwaitingWorker,
    WorkerQueued,
    Ready(crate::loader::LoadedSample),
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingLoadKind {
    User,
    Recovery,
}

impl PendingLoadKind {
    fn purpose(self) -> LoadPurpose {
        match self {
            Self::User => LoadPurpose::User,
            Self::Recovery => LoadPurpose::Recovery,
        }
    }
}

struct PendingLoad {
    path: PathBuf,
    phase: PendingLoadPhase,
    kind: PendingLoadKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEditStatus {
    Idle,
    AwaitingWorker,
    Rendering,
    ReadyToInstall,
    Failed,
    GenerationExhausted,
    UndoAvailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleEditRequestError {
    InvalidRecipe(String),
    AudioUnavailable(String),
    LoadPending,
    EmptyPad,
    NoUndo,
    RecoveryPending,
    GenerationExhausted,
    ProjectRevisionExhausted,
}

impl fmt::Display for SampleEditRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecipe(error) | Self::AudioUnavailable(error) => {
                formatter.write_str(error)
            }
            Self::LoadPending => formatter.write_str("sample load is pending"),
            Self::EmptyPad => formatter.write_str("pad has no committed sample to edit"),
            Self::NoUndo => formatter.write_str("no sample edit to undo"),
            Self::RecoveryPending => {
                formatter.write_str("sample is waiting for device-rate recovery")
            }
            Self::GenerationExhausted => formatter.write_str("sample edit generation is exhausted"),
            Self::ProjectRevisionExhausted => formatter.write_str("project revision is exhausted"),
        }
    }
}

impl std::error::Error for SampleEditRequestError {}

enum PendingEditPhase {
    AwaitingWorker,
    WorkerQueued,
    Ready(RenderedSample),
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingEditKind {
    Apply,
    Undo,
}

struct PendingEdit {
    generation: u64,
    base: Arc<SampleBuffer>,
    base_preview: EditPreview,
    recipe: SampleEditRecipe,
    kind: PendingEditKind,
    phase: PendingEditPhase,
}

struct SampleEditCheckpoint {
    base: Arc<SampleBuffer>,
    rendered: Arc<SampleBuffer>,
    recipe: SampleEditRecipe,
    base_preview: EditPreview,
    rendered_preview: EditPreview,
}

struct SampleCommit {
    base: Option<Arc<SampleBuffer>>,
    source_generation: u64,
    fingerprint: Option<SourceFingerprint>,
    recipe: SampleEditRecipe,
    base_preview: Option<EditPreview>,
    rendered_preview: Option<EditPreview>,
}

impl Default for SampleCommit {
    fn default() -> Self {
        Self {
            base: None,
            source_generation: 0,
            fingerprint: None,
            recipe: SampleEditRecipe::identity(),
            base_preview: None,
            rendered_preview: None,
        }
    }
}

struct SampleEditorState {
    commits: [SampleCommit; PAD_VIEW_COUNT],
    generations: [u64; PAD_VIEW_COUNT],
    pending: [Option<Box<PendingEdit>>; PAD_VIEW_COUNT],
    deferred_results: [Option<Box<WorkerResult>>; PAD_VIEW_COUNT],
    undo: [Option<Box<SampleEditCheckpoint>>; PAD_VIEW_COUNT],
    generation_exhausted: [bool; PAD_VIEW_COUNT],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Palette,
    FilePicker,
    DeviceError(String),
    ProjectOpenProgress,
    ClearPattern {
        slot: PatternSlotId,
        event_count: usize,
    },
    ApplySample {
        pad: PadId,
        before_frames: usize,
        after_frames: usize,
    },
    DiscardSample {
        pad: PadId,
    },
}

#[derive(Debug, Clone, Copy)]
struct PendingPatternTransport {
    playing: bool,
}

#[derive(Clone)]
struct ApplySampleContext {
    pad: PadId,
    pad_generation: u64,
    source: Option<PathBuf>,
    base_frames: usize,
    base_rate: u32,
}

pub struct App {
    active_bank: BankId,
    selected_pad: usize,
    pads: [PadView; PAD_VIEW_COUNT],
    patterns: PatternWorkspace,
    audio: Option<Box<dyn AudioPort>>,
    audio_format: Option<(u32, u16)>,
    held_pad_by_key: [Option<PadId>; PADS_PER_BANK as usize],
    overlay: Option<Overlay>,
    palette: LineEditor,
    palette_error: Option<String>,
    current_dir: PathBuf,
    file_picker: FilePicker,
    pending_worker_requests: Vec<WorkerRequest>,
    recovery_cursor: Option<usize>,
    pending_loads: [Option<Box<PendingLoad>>; PAD_VIEW_COUNT],
    committed_recovery_loads: [Option<Box<PendingLoad>>; PAD_VIEW_COUNT],
    recovery_generations: [u64; PAD_VIEW_COUNT],
    reinstall_pending: [bool; PAD_VIEW_COUNT],
    current_session_bound: [bool; PAD_VIEW_COUNT],
    sample_editor: Box<SampleEditorState>,
    editor: SampleEditor,
    apply_sample_context: Option<ApplySampleContext>,
    edit_result_advanced: bool,
    device_retry_requests: usize,
    keyboard_capabilities: KeyboardCapabilities,
    status: String,
    audio_unavailable_message: Option<String>,
    telemetry: Telemetry,
    meter_left: f32,
    meter_right: f32,
    recorded_ack_count: usize,
    pattern_submission_count: usize,
    pending_pattern_transport: Option<PendingPatternTransport>,
    should_quit: bool,
    project_session: ProjectSession,
    next_project_token: u64,
    pending_explicit_save: Option<PendingProjectSave>,
    pending_autosave_save: Option<PendingProjectSave>,
    in_flight_project: Option<InFlightProjectOperation>,
    pending_recovery_cleanup: VecDeque<RecoveryCleanup>,
    save_as_identity: Option<(PathBuf, ProjectId, String)>,
    project_save_error: Option<ProjectSaveFailure>,
    recovery_cleanup_warning: Option<ProjectStoreError>,
    autosave_retry_clock_pending: bool,
    autosave_retry_since: Option<Instant>,
    project_open: Option<ProjectOpenOperation>,
    project_open_error: Option<ProjectOpenError>,
}

impl App {
    pub fn with_audio(audio: Box<dyn AudioPort>) -> Self {
        Self::new(Some(audio), None)
    }

    pub fn without_audio(error: impl Into<String>) -> Self {
        let error = error.into();
        Self::new(None, Some(error))
    }

    fn new(audio: Option<Box<dyn AudioPort>>, audio_error: Option<String>) -> Self {
        let overlay = audio_error.clone().map(Overlay::DeviceError);
        let audio_format = audio
            .as_ref()
            .map(|audio| (audio.sample_rate(), audio.channels()));
        let pattern_sample_rate = audio_format.map_or(48_000, |(sample_rate, _)| sample_rate);
        let current_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from(std::path::MAIN_SEPARATOR_STR));
        Self {
            active_bank: BankId::new(0).expect("bank zero is valid"),
            selected_pad: 0,
            pads: array::from_fn(|_| PadView::default()),
            patterns: PatternWorkspace::new(pattern_sample_rate),
            audio,
            audio_format,
            held_pad_by_key: [None; PADS_PER_BANK as usize],
            overlay,
            palette: LineEditor::default(),
            palette_error: None,
            file_picker: FilePicker::new(current_dir.clone()),
            current_dir,
            pending_worker_requests: Vec::new(),
            recovery_cursor: None,
            pending_loads: array::from_fn(|_| None),
            committed_recovery_loads: array::from_fn(|_| None),
            recovery_generations: [0; PAD_VIEW_COUNT],
            reinstall_pending: [false; PAD_VIEW_COUNT],
            current_session_bound: [false; PAD_VIEW_COUNT],
            sample_editor: Box::new(SampleEditorState {
                commits: array::from_fn(|_| SampleCommit::default()),
                generations: [0; PAD_VIEW_COUNT],
                pending: array::from_fn(|_| None),
                deferred_results: array::from_fn(|_| None),
                undo: array::from_fn(|_| None),
                generation_exhausted: [false; PAD_VIEW_COUNT],
            }),
            editor: SampleEditor::open_empty(PadId::first(), PadSettings::default()),
            apply_sample_context: None,
            edit_result_advanced: false,
            device_retry_requests: 0,
            keyboard_capabilities: KeyboardCapabilities::default(),
            status: audio_error.clone().unwrap_or_default(),
            audio_unavailable_message: audio_error,
            telemetry: Telemetry {
                active_pads: [0; 3],
                rendered_frame: 0,
                last_triggered_frame: None,
                peak_left: 0.0,
                peak_right: 0.0,
                active_voices: 0,
                late_commands: 0,
                invalid_commands: 0,
                command_overflows: 0,
                pattern_slot: None,
                pattern_generation: None,
                pattern_playing: false,
                pattern_recording: false,
                pattern_origin: None,
                pattern_playhead: 0,
                pattern_loop_count: 0,
                pattern_overflows: 0,
                live_ack_overflows: 0,
            },
            meter_left: 0.0,
            meter_right: 0.0,
            recorded_ack_count: 0,
            pattern_submission_count: 0,
            pending_pattern_transport: None,
            should_quit: false,
            project_session: ProjectSession::new(
                ProjectId::from_bytes([0; 16]),
                None,
                "Untitled",
                0,
            ),
            next_project_token: 1,
            pending_explicit_save: None,
            pending_autosave_save: None,
            in_flight_project: None,
            pending_recovery_cleanup: VecDeque::with_capacity(WORKER_CHANNEL_CAPACITY),
            save_as_identity: None,
            project_save_error: None,
            recovery_cleanup_warning: None,
            autosave_retry_clock_pending: false,
            autosave_retry_since: None,
            project_open: None,
            project_open_error: None,
        }
    }

    pub fn apply(&mut self, action: InputAction) {
        if self.project_open_is_admitting()
            && !matches!(action, InputAction::StopAll | InputAction::PadRelease(_))
        {
            return;
        }
        match action {
            InputAction::PadPress(index) => self.press_pad(index),
            InputAction::PadRelease(index) => self.release_pad(index),
            InputAction::PadStop(index) => self.stop_pad(index),
            InputAction::BankDelta(delta) => self.change_bank(delta),
            InputAction::StopAll => self.stop_all(),
            InputAction::Quit => self.should_quit = true,
        }
    }

    pub fn apply_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.apply_key(key),
            Event::Paste(text) if self.overlay == Some(Overlay::Palette) && !text.is_empty() => {
                self.palette.insert_str(&text);
                self.palette_error = None;
            }
            _ => {}
        }
    }

    pub fn apply_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Repeat {
            return;
        }
        if matches!(self.overlay, Some(Overlay::DeviceError(_)))
            && key.kind == KeyEventKind::Press
            && key.modifiers == KeyModifiers::NONE
            && matches!(key.code, KeyCode::Char('r' | 'R'))
        {
            self.device_retry_requests = self.device_retry_requests.saturating_add(1);
            return;
        }
        if let Some(action) = map_key(key, self.keyboard_capabilities) {
            match action {
                InputAction::Quit
                | InputAction::StopAll
                | InputAction::PadRelease(_)
                | InputAction::PadPress(_)
                | InputAction::PadStop(_) => {
                    self.apply(action);
                    return;
                }
                InputAction::BankDelta(_) => {}
            }
        }
        if self.audio.is_none() && is_explicit_device_retry(key) {
            self.device_retry_requests = self.device_retry_requests.saturating_add(1);
            return;
        }

        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Esc
            && key.modifiers == KeyModifiers::NONE
            && self.overlay.is_some()
        {
            self.cancel_overlay();
            return;
        }

        match self.overlay.as_ref() {
            Some(Overlay::DeviceError(_)) => self.apply_device_error_key(key),
            Some(Overlay::ProjectOpenProgress) => {}
            Some(Overlay::Palette) => self.apply_palette_key(key),
            Some(Overlay::FilePicker) => self.apply_picker_key(key),
            Some(Overlay::Help) => self.apply_help_key(key),
            Some(Overlay::ClearPattern { .. }) => self.apply_clear_pattern_key(key),
            Some(Overlay::ApplySample { .. }) => self.apply_sample_apply_key(key),
            Some(Overlay::DiscardSample { .. }) => self.apply_sample_discard_key(key),
            None => self.apply_workspace_key(key),
        }
    }

    pub fn active_bank(&self) -> BankId {
        self.active_bank
    }

    pub fn selected_pad(&self) -> usize {
        self.selected_pad
    }

    pub fn pads(&self) -> &[PadView; PAD_VIEW_COUNT] {
        &self.pads
    }

    pub fn audio_format(&self) -> Option<(u32, u16)> {
        self.audio_format
    }

    pub fn is_pad_held(&self, index: usize) -> bool {
        let Some(held) = self.held_pad_by_key.get(index).copied().flatten() else {
            return false;
        };
        held.bank() == self.active_bank && usize::from(held.index()) == index
    }

    pub fn release_events_available(&self) -> bool {
        self.keyboard_capabilities.release_events
    }

    pub fn telemetry(&self) -> Telemetry {
        self.telemetry
    }

    pub fn patterns(&self) -> &PatternWorkspace {
        &self.patterns
    }

    pub fn workspace_view(&self) -> WorkspaceView {
        self.patterns.view()
    }

    pub fn sample_editor(&self) -> &SampleEditor {
        &self.editor
    }

    pub fn recorded_ack_count(&self) -> usize {
        self.recorded_ack_count
    }

    pub fn maintain_audio_pattern_submissions(&self) -> usize {
        self.pattern_submission_count
    }

    pub fn meter_levels(&self) -> (f32, f32) {
        (self.meter_left, self.meter_right)
    }

    pub fn project_revision(&self) -> u64 {
        self.project_session.current_revision()
    }

    pub fn request_save(&mut self) -> Result<(), ProjectSaveError> {
        let Some(directory) = self.project_session.directory().map(Path::to_owned) else {
            return Err(ProjectSaveError::Untitled);
        };
        self.ensure_project_request_available()?;
        let snapshot = self
            .project_snapshot()
            .map_err(ProjectSaveError::Snapshot)?;
        let token = self.allocate_project_token()?;
        self.pending_explicit_save = Some(PendingProjectSave {
            descriptor: crate::ProjectOperationDescriptor {
                token,
                kind: SaveKind::Explicit,
                project_id: snapshot.project_id,
                directory,
                revision: snapshot.revision,
            },
            snapshot,
            save_as: false,
        });
        Ok(())
    }

    pub fn request_save_as(
        &mut self,
        directory: impl Into<PathBuf>,
    ) -> Result<(), ProjectSaveError> {
        self.ensure_project_request_available()?;
        let directory = directory.into();
        let (project_id, name) = match &self.save_as_identity {
            Some((previous, project_id, name)) if previous == &directory => {
                (*project_id, name.clone())
            }
            _ => {
                let mut bytes = [0_u8; 16];
                getrandom::fill(&mut bytes)
                    .map_err(|error| ProjectSaveError::Entropy(error.to_string()))?;
                if bytes == [0; 16] {
                    bytes[15] = 1;
                }
                let name = directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Untitled")
                    .to_owned();
                let project_id = ProjectId::from_bytes(bytes);
                self.save_as_identity = Some((directory.clone(), project_id, name.clone()));
                (project_id, name)
            }
        };
        let mut snapshot = self
            .project_snapshot()
            .map_err(ProjectSaveError::Snapshot)?;
        snapshot.project_id = project_id;
        snapshot.name = name;
        let token = self.allocate_project_token()?;
        self.pending_explicit_save = Some(PendingProjectSave {
            descriptor: crate::ProjectOperationDescriptor {
                token,
                kind: SaveKind::Explicit,
                project_id,
                directory,
                revision: snapshot.revision,
            },
            snapshot,
            save_as: true,
        });
        Ok(())
    }

    pub fn maintain_project(&mut self, now: Instant) -> bool {
        if self.project_open.is_some() {
            return self.maintain_project_open(now);
        }
        let mut changed = false;
        if self.autosave_retry_clock_pending {
            self.autosave_retry_clock_pending = false;
            self.autosave_retry_since = Some(now);
            changed = true;
        }
        if self.pending_autosave_save.as_ref().is_some_and(|pending| {
            pending.descriptor.revision < self.project_session.current_revision()
        }) {
            self.pending_autosave_save = None;
            self.project_session.set_pending_autosave(None);
            changed = true;
        }

        if self.in_flight_project.is_none()
            && self.pending_explicit_save.is_none()
            && self.pending_autosave_save.as_ref().is_none_or(|pending| {
                pending.descriptor.revision < self.project_session.current_revision()
            })
            && self.project_session.directory().is_some()
            && self.project_session.current_revision() > self.project_session.autosaved_revision()
            && self.project_session.current_revision() > self.project_session.saved_revision()
        {
            let quiet_since = match (
                self.autosave_retry_since,
                self.project_session.dirty_since(),
            ) {
                (Some(retry), Some(dirty)) => Some(retry.max(dirty)),
                (retry, dirty) => retry.or(dirty),
            };
            if quiet_since
                .is_some_and(|since| now.saturating_duration_since(since) >= AUTOSAVE_DEBOUNCE)
                && let Ok(snapshot) = self.project_snapshot()
                && let Ok(token) = self.allocate_project_token()
            {
                let descriptor = crate::ProjectOperationDescriptor {
                    token,
                    kind: SaveKind::Recovery,
                    project_id: snapshot.project_id,
                    directory: self
                        .project_session
                        .directory()
                        .expect("named project checked above")
                        .to_owned(),
                    revision: snapshot.revision,
                };
                self.project_session
                    .set_pending_autosave(Some(crate::AutosaveDescriptor {
                        revision: descriptor.revision,
                    }));
                self.pending_autosave_save = Some(PendingProjectSave {
                    descriptor,
                    snapshot,
                    save_as: false,
                });
                changed = true;
            }
        }

        if self.in_flight_project.is_some()
            || self.pending_worker_requests.len() >= WORKER_CHANNEL_CAPACITY
        {
            return changed;
        }

        if let Some(save) = self.pending_explicit_save.take() {
            self.enqueue_project_save(save);
            return true;
        }
        if let Some(cleanup) = self.pending_recovery_cleanup.pop_front() {
            self.pending_worker_requests
                .push(WorkerRequest::DiscardRecovery {
                    token: cleanup.token,
                    directory: cleanup.directory.clone(),
                    project_id: cleanup.project_id,
                    revision: cleanup.revision,
                });
            self.in_flight_project = Some(InFlightProjectOperation::Cleanup(cleanup));
            return true;
        }
        if let Some(save) = self.pending_autosave_save.take() {
            self.project_session.set_pending_autosave(None);
            self.enqueue_project_save(save);
            return true;
        }
        changed
    }

    pub fn request_open_project(
        &mut self,
        directory: impl Into<PathBuf>,
    ) -> Result<ProjectToken, ProjectOpenError> {
        if self.project_open.is_some()
            || self.in_flight_project.is_some()
            || self.pending_explicit_save.is_some()
            || self.pending_autosave_save.is_some()
        {
            return Err(ProjectOpenError::OperationPending);
        }
        self.project_snapshot()
            .map_err(|error| ProjectOpenError::UnresolvedState(error.to_string()))?;
        let token = self
            .allocate_project_token()
            .map_err(|_| ProjectOpenError::TokenExhausted)?;
        let directory = directory.into();
        self.project_open_error = None;
        let progress = ProjectOpenStage {
            token,
            directory: directory.clone(),
            project_id: None,
            revision: None,
            phase: ProjectOpenPhase::Probing,
            staged_pads: 0,
            total_pads: 0,
            admitted_actions: 0,
            total_actions: 0,
        };
        let worker_queued =
            self.queue_worker_request(WorkerRequest::ProbeProject { token, directory });
        self.project_open = Some(ProjectOpenOperation::Probing {
            progress,
            worker_queued,
        });
        self.overlay = Some(Overlay::ProjectOpenProgress);
        self.status = "Validating project metadata…".to_owned();
        Ok(token)
    }

    pub fn project_open_stage(&self) -> Option<&ProjectOpenStage> {
        match self.project_open.as_ref()? {
            ProjectOpenOperation::Probing { progress, .. } => Some(progress),
            ProjectOpenOperation::ChoosingRecovery(choice) => Some(&choice.progress),
            ProjectOpenOperation::Staging(candidate) => Some(&candidate.progress),
        }
    }

    pub fn project_open_error(&self) -> Option<&ProjectOpenError> {
        self.project_open_error.as_ref()
    }

    fn fail_project_open(&mut self, error: ProjectOpenError) {
        self.project_open = None;
        self.overlay = None;
        self.status = error.to_string();
        self.project_open_error = Some(error);
    }

    pub fn cancel_project_open(&mut self) -> Result<(), ProjectOpenError> {
        let Some(operation) = self.project_open.as_ref() else {
            return Err(ProjectOpenError::OperationPending);
        };
        if matches!(operation, ProjectOpenOperation::Staging(candidate) if candidate.progress.phase == ProjectOpenPhase::Admitting)
            || matches!(operation, ProjectOpenOperation::ChoosingRecovery(choice) if choice.discard_queued)
        {
            return Err(ProjectOpenError::CancellationLocked);
        }
        self.project_open = None;
        self.project_open_error = None;
        if self.overlay == Some(Overlay::ProjectOpenProgress) {
            self.overlay = None;
        }
        self.status = "Project open cancelled".to_owned();
        Ok(())
    }

    fn maintain_project_open(&mut self, now: Instant) -> bool {
        let Some(mut operation) = self.project_open.take() else {
            return false;
        };
        let mut changed = false;
        match &mut operation {
            ProjectOpenOperation::Probing {
                progress,
                worker_queued,
            } => {
                if !*worker_queued && self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY {
                    self.pending_worker_requests
                        .push(WorkerRequest::ProbeProject {
                            token: progress.token,
                            directory: progress.directory.clone(),
                        });
                    *worker_queued = true;
                    changed = true;
                }
            }
            ProjectOpenOperation::ChoosingRecovery(choice) => {
                if choice.discard_requested
                    && !choice.discard_queued
                    && self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY
                {
                    self.pending_worker_requests
                        .push(WorkerRequest::DiscardRecovery {
                            token: choice.progress.token,
                            directory: choice.progress.directory.clone(),
                            project_id: choice.recovery.project_id,
                            revision: choice.recovery.revision,
                        });
                    choice.discard_queued = true;
                    changed = true;
                }
            }
            ProjectOpenOperation::Staging(candidate) => {
                if candidate.decode_in_flight.is_none()
                    && candidate.next_decode < candidate.document.pads.len()
                    && self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY
                {
                    let Some((engine_rate, _)) = self.audio_format else {
                        self.project_open = Some(operation);
                        return false;
                    };
                    let pad = &candidate.document.pads[candidate.next_decode];
                    let path = candidate.progress.directory.join(&pad.audio_path);
                    self.pending_worker_requests
                        .push(WorkerRequest::StageProjectSample(Box::new(
                            StageProjectSampleRequest {
                                token: candidate.progress.token,
                                pad: pad.pad,
                                revision: candidate.document.revision,
                                path,
                                engine_rate,
                                recipe: pad.recipe,
                            },
                        )));
                    candidate.decode_in_flight = Some(pad.pad);
                    changed = true;
                } else if candidate.decode_in_flight.is_none()
                    && candidate.next_decode == candidate.document.pads.len()
                {
                    let Some(audio) = self.audio.as_mut() else {
                        self.project_open = Some(operation);
                        return false;
                    };
                    match candidate.admission {
                        ProjectAdmission::StopAll => match audio.stop_all() {
                            Ok(()) => {
                                candidate.progress.phase = ProjectOpenPhase::Admitting;
                                candidate.progress.admitted_actions = 1;
                                candidate.admission = ProjectAdmission::Pads(0);
                                self.held_pad_by_key.fill(None);
                                changed = true;
                            }
                            Err(error) => self.status = error,
                        },
                        ProjectAdmission::Pads(offset) => {
                            let pad = pad_from_offset(offset);
                            let result =
                                if let Some(staged) = candidate.staged_pads[offset].as_ref() {
                                    audio
                                        .install(
                                            pad,
                                            Arc::clone(&staged.loaded.rendered),
                                            staged.settings,
                                        )
                                        .map(|_| ())
                                } else {
                                    audio.remove_sample(pad)
                                };
                            match result {
                                Ok(()) => {
                                    let next = offset + 1;
                                    candidate.progress.admitted_actions += 1;
                                    candidate.admission = if next == PAD_VIEW_COUNT {
                                        ProjectAdmission::Patterns(0)
                                    } else {
                                        ProjectAdmission::Pads(next)
                                    };
                                    changed = true;
                                }
                                Err(error) => self.status = error,
                            }
                        }
                        ProjectAdmission::Patterns(submitted) => {
                            let maintenance =
                                candidate.patterns.maintain(audio.as_mut(), self.telemetry);
                            if maintenance.submitted_slot.is_some() {
                                let next = submitted + 1;
                                candidate.progress.admitted_actions += 1;
                                candidate.admission = if next == sampler_core::PATTERN_SLOT_COUNT {
                                    ProjectAdmission::Complete
                                } else {
                                    ProjectAdmission::Patterns(next)
                                };
                                changed = true;
                            }
                            if let Some(status) = maintenance.status {
                                self.status = pattern_status_text(&status);
                            }
                        }
                        ProjectAdmission::Complete => {}
                    }
                }
            }
        }
        if matches!(&operation, ProjectOpenOperation::Staging(candidate) if matches!(candidate.admission, ProjectAdmission::Complete))
        {
            let ProjectOpenOperation::Staging(candidate) = operation else {
                unreachable!()
            };
            self.commit_project_open(candidate, now);
            return true;
        }
        self.project_open = Some(operation);
        changed
    }

    fn project_open_is_admitting(&self) -> bool {
        matches!(
            self.project_open.as_ref(),
            Some(ProjectOpenOperation::Staging(candidate))
                if candidate.progress.phase == ProjectOpenPhase::Admitting
        )
    }

    fn commit_project_open(&mut self, mut candidate: Box<ProjectOpenCandidate>, now: Instant) {
        let mut pads: [PadView; PAD_VIEW_COUNT] = array::from_fn(|_| PadView::default());
        let mut commits: [SampleCommit; PAD_VIEW_COUNT] =
            array::from_fn(|_| SampleCommit::default());
        for offset in 0..PAD_VIEW_COUNT {
            let Some(staged) = candidate.staged_pads[offset].take() else {
                continue;
            };
            let StagedProjectPad {
                path,
                settings,
                loaded,
            } = *staged;
            let label = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned();
            pads[offset] = PadView {
                source: Some(path),
                label,
                settings,
                generation: 1,
                state: PadLoadState::Ready,
                sample: Some(loaded.rendered),
                preview: crate::loader::downsample_preview(&loaded.rendered_preview),
                active: false,
            };
            commits[offset] = SampleCommit {
                base: Some(loaded.base),
                source_generation: 1,
                fingerprint: Some(loaded.fingerprint),
                recipe: loaded.recipe,
                base_preview: Some(loaded.base_preview),
                rendered_preview: Some(loaded.rendered_preview),
            };
        }

        let dirty_since = candidate.restored_recovery.then_some(now);
        let autosaved_revision = if candidate.restored_recovery {
            candidate.document.revision
        } else {
            candidate.saved_revision
        };
        self.pads = pads;
        self.patterns = candidate.patterns;
        self.project_session = ProjectSession::opened(
            candidate.document.project_id,
            candidate.progress.directory,
            candidate.document.name,
            candidate.document.revision,
            candidate.saved_revision,
            autosaved_revision,
            dirty_since,
        );
        self.pending_loads.fill_with(|| None);
        self.committed_recovery_loads.fill_with(|| None);
        self.reinstall_pending.fill(false);
        self.current_session_bound = array::from_fn(|index| self.pads[index].sample.is_some());
        *self.sample_editor = SampleEditorState {
            commits,
            generations: [1; PAD_VIEW_COUNT],
            pending: array::from_fn(|_| None),
            deferred_results: array::from_fn(|_| None),
            undo: array::from_fn(|_| None),
            generation_exhausted: [false; PAD_VIEW_COUNT],
        };
        self.active_bank = BankId::new(0).expect("first bank is valid");
        self.selected_pad = 0;
        self.apply_sample_context = None;
        self.held_pad_by_key.fill(None);
        self.pending_pattern_transport = None;
        self.editor = SampleEditor::open_empty(PadId::first(), self.pads[0].settings);
        self.sync_editor_to_selected_pad();
        self.overlay = None;
        self.project_open_error = None;
        self.status = format!("Opened {}", self.project_session.name());
    }

    fn apply_project_probe(
        &mut self,
        token: ProjectToken,
        directory: PathBuf,
        result: Result<ProjectProbe, ProjectStoreError>,
    ) -> bool {
        let Some(ProjectOpenOperation::Probing {
            progress,
            worker_queued: true,
        }) = self.project_open.as_ref()
        else {
            return false;
        };
        if progress.token != token || progress.directory != directory {
            return false;
        }
        self.project_open = None;
        let probe = match result {
            Ok(probe) => probe,
            Err(error) => {
                self.fail_project_open(ProjectOpenError::Probe(error));
                return true;
            }
        };
        let explicit = match probe.explicit {
            Some(Ok(document)) => Some(document),
            Some(Err(error)) => {
                if probe
                    .recovery
                    .as_ref()
                    .is_none_or(|recovery| recovery.is_err())
                {
                    self.fail_project_open(ProjectOpenError::Probe(error));
                    return true;
                }
                None
            }
            None => None,
        };
        let recovery = match probe.recovery {
            Some(Ok(document)) => Some(document),
            Some(Err(error)) => {
                self.fail_project_open(ProjectOpenError::Probe(error));
                return true;
            }
            None => None,
        };

        if let (Some(explicit), Some(recovery)) = (&explicit, &recovery)
            && explicit.project_id != recovery.project_id
        {
            self.fail_project_open(ProjectOpenError::RecoveryMismatch);
            return true;
        }
        if let Some(recovery) = recovery
            && explicit
                .as_ref()
                .is_none_or(|explicit| recovery.revision > explicit.revision)
        {
            let progress = ProjectOpenStage {
                token,
                directory: probe.directory,
                project_id: Some(recovery.project_id),
                revision: Some(recovery.revision),
                phase: ProjectOpenPhase::AwaitingRecoveryChoice,
                staged_pads: 0,
                total_pads: recovery.pads.len(),
                admitted_actions: 0,
                total_actions: 1 + PAD_VIEW_COUNT + sampler_core::PATTERN_SLOT_COUNT,
            };
            self.project_open = Some(ProjectOpenOperation::ChoosingRecovery(Box::new(
                ProjectRecoveryChoiceState {
                    progress,
                    explicit,
                    recovery,
                    discard_requested: false,
                    discard_queued: false,
                },
            )));
            self.status = "A newer recovery is available".to_owned();
            return true;
        }
        let Some(document) = explicit else {
            self.fail_project_open(ProjectOpenError::NoUsableDocument);
            return true;
        };
        match self.build_project_open_candidate(
            token,
            probe.directory,
            document.clone(),
            document.revision,
            false,
        ) {
            Ok(operation) => self.project_open = Some(operation),
            Err(error) => {
                self.fail_project_open(error);
                return true;
            }
        }
        self.status = "Staging project audio…".to_owned();
        true
    }

    fn build_project_open_candidate(
        &self,
        token: ProjectToken,
        directory: PathBuf,
        mut document: ProjectDocument,
        saved_revision: u64,
        restored_recovery: bool,
    ) -> Result<ProjectOpenOperation, ProjectOpenError> {
        let Some((sample_rate, _)) = self.audio_format else {
            return Err(ProjectOpenError::AudioUnavailable);
        };
        let mut patterns = PatternWorkspace::new(sample_rate);
        patterns
            .replace_project_patterns(document.patterns.clone())
            .map_err(|error| ProjectOpenError::InvalidPatterns(error.to_string()))?;
        patterns
            .rebuild_sample_rate(sample_rate)
            .map_err(|error| ProjectOpenError::InvalidPatterns(error.to_string()))?;
        document.pads.sort_by_key(|pad| pad_offset(pad.pad));
        let progress = ProjectOpenStage {
            token,
            directory,
            project_id: Some(document.project_id),
            revision: Some(document.revision),
            phase: ProjectOpenPhase::Staging,
            staged_pads: 0,
            total_pads: document.pads.len(),
            admitted_actions: 0,
            total_actions: 1 + PAD_VIEW_COUNT + sampler_core::PATTERN_SLOT_COUNT,
        };
        Ok(ProjectOpenOperation::Staging(Box::new(
            ProjectOpenCandidate {
                progress,
                document,
                patterns,
                staged_pads: array::from_fn(|_| None),
                next_decode: 0,
                decode_in_flight: None,
                saved_revision,
                restored_recovery,
                admission: ProjectAdmission::StopAll,
            },
        )))
    }

    pub fn choose_project_recovery(
        &mut self,
        choice: RecoveryChoice,
    ) -> Result<(), ProjectOpenError> {
        let Some(ProjectOpenOperation::ChoosingRecovery(state)) = self.project_open.take() else {
            return Err(ProjectOpenError::OperationPending);
        };
        let ProjectRecoveryChoiceState {
            progress,
            explicit,
            recovery,
            discard_requested,
            discard_queued,
        } = *state;
        if discard_queued {
            self.project_open = Some(ProjectOpenOperation::ChoosingRecovery(Box::new(
                ProjectRecoveryChoiceState {
                    progress,
                    explicit,
                    recovery,
                    discard_requested,
                    discard_queued,
                },
            )));
            return Err(ProjectOpenError::CancellationLocked);
        }
        match choice {
            RecoveryChoice::Cancel => {
                self.overlay = None;
                self.status = "Project open cancelled".to_owned();
            }
            RecoveryChoice::Restore => {
                let saved_revision = explicit
                    .as_ref()
                    .map_or_else(|| recovery.revision.saturating_sub(1), |doc| doc.revision);
                let candidate = self.build_project_open_candidate(
                    progress.token,
                    progress.directory,
                    recovery,
                    saved_revision,
                    true,
                );
                match candidate {
                    Ok(candidate) => self.project_open = Some(candidate),
                    Err(error) => {
                        self.fail_project_open(error.clone());
                        return Err(error);
                    }
                }
                self.status = "Staging recovered project audio…".to_owned();
            }
            RecoveryChoice::Discard => {
                if explicit.is_none() {
                    self.project_open = Some(ProjectOpenOperation::ChoosingRecovery(Box::new(
                        ProjectRecoveryChoiceState {
                            progress,
                            explicit,
                            recovery,
                            discard_requested,
                            discard_queued,
                        },
                    )));
                    return Err(ProjectOpenError::NoUsableDocument);
                }
                let discard_queued = if self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY
                {
                    self.pending_worker_requests
                        .push(WorkerRequest::DiscardRecovery {
                            token: progress.token,
                            directory: progress.directory.clone(),
                            project_id: recovery.project_id,
                            revision: recovery.revision,
                        });
                    true
                } else {
                    false
                };
                self.project_open = Some(ProjectOpenOperation::ChoosingRecovery(Box::new(
                    ProjectRecoveryChoiceState {
                        progress,
                        explicit,
                        recovery,
                        discard_requested: true,
                        discard_queued,
                    },
                )));
                self.status = "Discarding exact recovery…".to_owned();
            }
        }
        Ok(())
    }

    fn apply_project_sample_staged(
        &mut self,
        token: ProjectToken,
        pad: PadId,
        revision: u64,
        path: PathBuf,
        recipe: SampleEditRecipe,
        result: Result<crate::LoadedSample, crate::LoadSampleError>,
    ) -> bool {
        let Some(ProjectOpenOperation::Staging(candidate)) = self.project_open.as_ref() else {
            return false;
        };
        let Some(expected) = candidate.document.pads.get(candidate.next_decode) else {
            return false;
        };
        let expected_settings = expected.settings;
        let expected_path = candidate.progress.directory.join(&expected.audio_path);
        if candidate.progress.token != token
            || candidate.decode_in_flight != Some(pad)
            || expected.pad != pad
            || candidate.document.revision != revision
            || expected_path != path
            || expected.recipe != recipe
        {
            return false;
        }

        let loaded = match result {
            Ok(loaded)
                if loaded.fingerprint.digest == expected.asset_digest
                    && loaded.recipe == recipe
                    && self.audio_format.is_some_and(|(sample_rate, _)| {
                        loaded.rendered.sample_rate() == sample_rate
                    }) =>
            {
                loaded
            }
            Ok(loaded) => {
                let stage_error = if loaded.fingerprint.digest != expected.asset_digest {
                    ProjectStageError::AssetDigestChanged
                } else if loaded.recipe != recipe {
                    ProjectStageError::RecipeContextChanged
                } else {
                    ProjectStageError::AudioDeviceRateChanged
                };
                self.fail_project_open(ProjectOpenError::Stage {
                    pad,
                    error: stage_error,
                });
                return true;
            }
            Err(error) => {
                self.fail_project_open(ProjectOpenError::Stage {
                    pad,
                    error: ProjectStageError::Load(error),
                });
                return true;
            }
        };

        let Some(ProjectOpenOperation::Staging(candidate)) = self.project_open.as_mut() else {
            return false;
        };
        candidate.staged_pads[pad_offset(pad)] = Some(Box::new(StagedProjectPad {
            path,
            settings: expected_settings,
            loaded,
        }));
        candidate.next_decode += 1;
        candidate.decode_in_flight = None;
        candidate.progress.staged_pads = candidate.next_decode;
        self.status = format!(
            "Staged {}/{} project samples",
            candidate.progress.staged_pads, candidate.progress.total_pads
        );
        true
    }

    fn apply_project_recovery_discarded(
        &mut self,
        token: ProjectToken,
        directory: PathBuf,
        project_id: ProjectId,
        revision: u64,
        result: Result<(), ProjectStoreError>,
    ) -> Option<bool> {
        let ProjectOpenOperation::ChoosingRecovery(choice) = self.project_open.as_ref()? else {
            return Some(false);
        };
        if !choice.discard_requested
            || !choice.discard_queued
            || choice.progress.token != token
            || choice.progress.directory != directory
            || choice.recovery.project_id != project_id
            || choice.recovery.revision != revision
        {
            return Some(false);
        }
        let explicit = choice
            .explicit
            .clone()
            .expect("recovery discard is offered only with an explicit document");
        self.project_open = None;
        if let Err(error) = result {
            self.fail_project_open(ProjectOpenError::RecoveryDiscard(error));
            return Some(true);
        }
        match self.build_project_open_candidate(
            token,
            directory,
            explicit.clone(),
            explicit.revision,
            false,
        ) {
            Ok(operation) => {
                self.project_open = Some(operation);
                self.status = "Staging project audio…".to_owned();
            }
            Err(error) => {
                self.fail_project_open(error);
            }
        }
        Some(true)
    }

    fn restore_busy_project_recovery_discard(
        &mut self,
        token: ProjectToken,
        directory: &Path,
        project_id: ProjectId,
        revision: u64,
        error: WorkerSendError,
    ) -> Option<bool> {
        let ProjectOpenOperation::ChoosingRecovery(choice) = self.project_open.as_mut()? else {
            return Some(false);
        };
        if !choice.discard_requested
            || choice.progress.token != token
            || choice.progress.directory != directory
            || choice.recovery.project_id != project_id
            || choice.recovery.revision != revision
        {
            return Some(false);
        }
        if error == WorkerSendError::WorkerBusy {
            choice.discard_queued = false;
        } else {
            self.project_open = None;
            self.overlay = None;
        }
        Some(true)
    }

    pub fn project_save_error(&self) -> Option<&ProjectSaveFailure> {
        self.project_save_error.as_ref()
    }

    pub fn recovery_cleanup_warning(&self) -> Option<&ProjectStoreError> {
        self.recovery_cleanup_warning.as_ref()
    }

    pub fn project_header(&self) -> String {
        let identity = if self.project_session.directory().is_some() {
            self.project_session.name().to_owned()
        } else {
            "UNTITLED".to_owned()
        };
        let truth = match self.project_session.status() {
            crate::ProjectStatus::Clean => "SAVED",
            crate::ProjectStatus::Modified => "MODIFIED",
            crate::ProjectStatus::Saving(SaveKind::Explicit) => "SAVING",
            crate::ProjectStatus::Saving(SaveKind::Recovery) => "AUTOSAVING",
        };
        let mut header = format!("{identity} · {truth}");
        if self.project_session.pending_autosave().is_some() || self.pending_autosave_save.is_some()
        {
            header.push_str(" · AUTOSAVE PENDING");
        }
        if let Some(failure) = &self.project_save_error {
            let label = if failure.kind == SaveKind::Recovery {
                "AUTOSAVE ERROR"
            } else {
                "SAVE ERROR"
            };
            header.push_str(&format!(" · {label}: {}", failure.error));
        }
        if let Some(warning) = &self.recovery_cleanup_warning {
            header.push_str(&format!(" · RECOVERY CLEANUP WARNING: {warning}"));
        }
        header
    }

    fn ensure_project_request_available(&self) -> Result<(), ProjectSaveError> {
        if self.pending_explicit_save.is_some()
            || self.in_flight_project.is_some()
            || self.pending_recovery_cleanup.len() >= WORKER_CHANNEL_CAPACITY
        {
            Err(ProjectSaveError::OperationPending)
        } else {
            Ok(())
        }
    }

    fn allocate_project_token(&mut self) -> Result<ProjectToken, ProjectSaveError> {
        let token = ProjectToken::new(self.next_project_token);
        self.next_project_token = self
            .next_project_token
            .checked_add(1)
            .ok_or(ProjectSaveError::TokenExhausted)?;
        Ok(token)
    }

    fn enqueue_project_save(&mut self, save: PendingProjectSave) {
        let descriptor = save.descriptor.clone();
        self.pending_worker_requests
            .push(WorkerRequest::SaveProject(Box::new(
                ProjectSaveWorkerRequest {
                    token: descriptor.token,
                    request: ProjectSaveRequest {
                        directory: descriptor.directory.clone(),
                        save_as: save.save_as,
                        kind: descriptor.kind,
                        snapshot: save.snapshot.clone(),
                    },
                },
            )));
        self.project_session.set_in_flight(Some(descriptor));
        self.in_flight_project = Some(InFlightProjectOperation::Save(save));
    }

    pub fn project_snapshot(&self) -> Result<ProjectSaveSnapshot, ProjectSnapshotError> {
        if let Some(operation) = self.project_session.in_flight() {
            return Err(ProjectSnapshotError::PendingProjectOperation(
                operation.token,
            ));
        }
        if self.editor.is_dirty() {
            return Err(ProjectSnapshotError::DirtySampleDraft(self.editor.pad()));
        }
        let mut pads = Vec::with_capacity(PAD_VIEW_COUNT);
        for offset in 0..PAD_VIEW_COUNT {
            let pad = pad_from_offset(offset);
            if self.pending_loads[offset]
                .as_ref()
                .is_some_and(|pending| !matches!(pending.phase, PendingLoadPhase::Failed))
                || self.committed_recovery_loads[offset]
                    .as_ref()
                    .is_some_and(|pending| !matches!(pending.phase, PendingLoadPhase::Failed))
            {
                return Err(ProjectSnapshotError::PendingSampleLoad(pad));
            }
            if self.sample_editor.pending[offset]
                .as_ref()
                .is_some_and(|pending| !matches!(pending.phase, PendingEditPhase::Failed))
            {
                return Err(ProjectSnapshotError::PendingSampleEdit(pad));
            }
            if self.pads[offset].sample.is_none() {
                continue;
            }
            let Some(source_path) = self.pads[offset].source.clone() else {
                return Err(ProjectSnapshotError::UnresolvedSample(pad));
            };
            let Some(fingerprint) = self.sample_editor.commits[offset].fingerprint else {
                return Err(ProjectSnapshotError::UnresolvedSample(pad));
            };
            pads.push(ProjectSavePad {
                pad,
                source_path,
                source_generation: self.sample_editor.commits[offset].source_generation,
                fingerprint,
                settings: self.pads[offset].settings,
                recipe: self.sample_editor.commits[offset].recipe,
            });
        }
        let patterns = self
            .patterns
            .export_project_patterns()
            .map_err(|error| ProjectSnapshotError::InvalidPatterns(error.to_string()))?;
        Ok(ProjectSaveSnapshot {
            project_id: self.project_session.project_id(),
            name: self.project_session.name().to_owned(),
            revision: self.project_session.current_revision(),
            pads,
            patterns,
        })
    }

    pub fn discard_sample_draft(&mut self) {
        self.editor.confirm_discard();
    }

    #[cfg(test)]
    pub(crate) fn editor_mut_for_test(&mut self) -> &mut SampleEditor {
        &mut self.editor
    }

    #[cfg(test)]
    pub(crate) fn patterns_mut_for_test(&mut self) -> &mut PatternWorkspace {
        &mut self.patterns
    }

    pub fn tick(&mut self) {
        const METER_DECAY: f32 = 0.85;

        let next = self
            .audio
            .as_mut()
            .and_then(|audio| audio.latest_telemetry());
        self.meter_left = sanitize_peak(self.meter_left * METER_DECAY);
        self.meter_right = sanitize_peak(self.meter_right * METER_DECAY);
        if let Some(telemetry) = next {
            self.apply_telemetry(telemetry);
        }
    }

    pub fn maintain_audio(&mut self) -> bool {
        if self.audio.is_none() {
            return false;
        }
        self.edit_result_advanced = false;
        let mut changed = self.advance_one_deferred_edit_result();
        let runtime_error = {
            let audio = self.audio.as_mut().expect("audio was checked above");
            audio.reclaim_retired();
            audio.poll_runtime_error()
        };

        if let Some(error) = runtime_error {
            self.fail_audio(error);
            true
        } else if self.project_open.is_some() {
            changed
        } else {
            changed |= self.pump_recovery_requests();
            changed |= self.pump_pending_sample_edit();
            let telemetry = self
                .audio
                .as_mut()
                .and_then(|audio| audio.latest_telemetry());
            if let Some(telemetry) = telemetry {
                changed |= self.apply_telemetry(telemetry);
            }
            let recording_mutation_budget = usize::try_from(
                crate::MAX_PROJECT_REVISION.saturating_sub(self.project_revision()),
            )
            .unwrap_or(usize::MAX);
            let maintenance = {
                let audio = self
                    .audio
                    .as_mut()
                    .expect("audio remains present after a successful poll");
                self.patterns.maintain_with_recording_budget(
                    audio.as_mut(),
                    self.telemetry,
                    recording_mutation_budget,
                )
            };
            for _ in 0..maintenance.committed_mutations {
                self.commit_project_mutation();
            }
            changed |= maintenance.reclaimed_snapshots > 0
                || maintenance.drained_acks > 0
                || maintenance.compiled_slot.is_some()
                || maintenance.submitted_slot.is_some();
            self.recorded_ack_count = self
                .recorded_ack_count
                .saturating_add(maintenance.drained_acks);
            if maintenance.submitted_slot.is_some() {
                self.pattern_submission_count = self.pattern_submission_count.saturating_add(1);
            }
            if let Some(status) = maintenance.status {
                let unsupported_bootstrap = matches!(
                    &status,
                    PatternStatus::AudioCommandFailed { error, .. }
                        if error == "pattern audio is unsupported"
                ) && self.patterns.view() == WorkspaceView::Perform;
                if !unsupported_bootstrap {
                    self.status = pattern_status_text(&status);
                    changed = true;
                }
            }
            changed
        }
    }

    fn advance_one_deferred_edit_result(&mut self) -> bool {
        for offset in 0..PAD_VIEW_COUNT {
            let Some(result) = self.sample_editor.deferred_results[offset].take() else {
                continue;
            };
            let WorkerResult::Edited {
                pad,
                generation,
                recipe,
                result,
            } = *result
            else {
                continue;
            };
            if !self.pending_edit_matches(pad, generation, recipe) {
                continue;
            }
            self.edit_result_advanced = true;
            return self.apply_edited_worker_result(pad, generation, recipe, result);
        }
        false
    }

    fn pending_edit_matches(&self, pad: PadId, generation: u64, recipe: SampleEditRecipe) -> bool {
        self.sample_editor.pending[pad_offset(pad)]
            .as_ref()
            .is_some_and(|pending| {
                pending.generation == generation
                    && pending.recipe == recipe
                    && matches!(pending.phase, PendingEditPhase::WorkerQueued)
            })
    }

    fn pump_pending_sample_edit(&mut self) -> bool {
        for offset in 0..PAD_VIEW_COUNT {
            let phase =
                self.sample_editor.pending[offset]
                    .as_ref()
                    .map(|pending| match pending.phase {
                        PendingEditPhase::AwaitingWorker => 0,
                        PendingEditPhase::Ready(_) => 1,
                        PendingEditPhase::WorkerQueued | PendingEditPhase::Failed => 2,
                    });
            match phase {
                Some(0) => {
                    let pending = self.sample_editor.pending[offset]
                        .as_mut()
                        .expect("pending edit exists for its phase");
                    pending.phase = PendingEditPhase::WorkerQueued;
                    let request = WorkerRequest::EditSample {
                        pad: pad_from_offset(offset),
                        generation: pending.generation,
                        base: Arc::clone(&pending.base),
                        base_preview: Arc::clone(&pending.base_preview),
                        recipe: pending.recipe,
                    };
                    self.queue_worker_request(request);
                    return true;
                }
                Some(1) => return self.install_pending_sample_edit(offset),
                Some(2) | None => {}
                Some(_) => unreachable!("edit phase encoding is exhaustive"),
            }
        }
        false
    }

    fn install_pending_sample_edit(&mut self, offset: usize) -> bool {
        let Some(mut pending) = self.sample_editor.pending[offset].take() else {
            return false;
        };
        let PendingEditPhase::Ready(rendered) = pending.phase else {
            self.sample_editor.pending[offset] = Some(pending);
            return false;
        };
        if let Err(error) = self.ensure_project_mutation_available() {
            pending.phase = PendingEditPhase::Ready(rendered);
            self.sample_editor.pending[offset] = Some(pending);
            self.status = error;
            return true;
        }
        let Some(audio) = self.audio.as_mut() else {
            pending.phase = PendingEditPhase::Ready(rendered);
            self.sample_editor.pending[offset] = Some(pending);
            return false;
        };
        if rendered.rendered.sample_rate() != audio.sample_rate() {
            pending.phase = PendingEditPhase::Ready(rendered);
            self.sample_editor.pending[offset] = Some(pending);
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            return true;
        }
        let pad = pad_from_offset(offset);
        let settings = self.pads[offset].settings;
        if let Err(error) = audio.install(pad, Arc::clone(&rendered.rendered), settings) {
            let kind = pending.kind;
            pending.phase = PendingEditPhase::Ready(rendered);
            self.sample_editor.pending[offset] = Some(pending);
            self.pads[offset].state = PadLoadState::Error(error.clone());
            self.status = error;
            if self.patterns.view() == WorkspaceView::Sample && self.selected_pad_id() == Some(pad)
            {
                match kind {
                    PendingEditKind::Apply => self
                        .editor
                        .observe_apply_failed(SampleEditorError::InstallFailed),
                    PendingEditKind::Undo => self
                        .editor
                        .observe_undo_failed(SampleEditorError::InstallFailed),
                }
            }
            return true;
        }

        let checkpoint = match (
            self.sample_editor.commits[offset].base.as_ref(),
            self.pads[offset].sample.as_ref(),
            self.sample_editor.commits[offset].base_preview.as_ref(),
            self.sample_editor.commits[offset].rendered_preview.as_ref(),
        ) {
            (Some(base), Some(sample), Some(base_preview), Some(rendered_preview)) => {
                Some(Box::new(SampleEditCheckpoint {
                    base: Arc::clone(base),
                    rendered: Arc::clone(sample),
                    recipe: self.sample_editor.commits[offset].recipe,
                    base_preview: Arc::clone(base_preview),
                    rendered_preview: Arc::clone(rendered_preview),
                }))
            }
            _ => None,
        };
        let view = &mut self.pads[offset];
        self.sample_editor.commits[offset].base = Some(pending.base);
        self.sample_editor.commits[offset].base_preview = Some(rendered.base_preview);
        self.sample_editor.commits[offset].recipe = pending.recipe;
        view.sample = Some(rendered.rendered);
        self.sample_editor.commits[offset].rendered_preview =
            Some(Arc::clone(&rendered.rendered_preview));
        view.preview = crate::loader::downsample_preview(&rendered.rendered_preview);
        view.state = PadLoadState::Ready;
        self.current_session_bound[offset] = true;
        match pending.kind {
            PendingEditKind::Apply => self.sample_editor.undo[offset] = checkpoint,
            PendingEditKind::Undo => self.sample_editor.undo[offset] = None,
        }
        self.status = if pending.kind == PendingEditKind::Undo {
            "Undid sample edit".to_owned()
        } else {
            "Applied sample edit".to_owned()
        };
        if self.patterns.view() == WorkspaceView::Sample && self.selected_pad_id() == Some(pad) {
            if pending.kind == PendingEditKind::Undo {
                self.editor.observe_undo_succeeded();
            } else {
                self.editor.observe_apply_succeeded();
            }
            self.sync_editor_to_selected_pad();
        }
        self.commit_project_mutation();
        true
    }

    pub fn pad(&self, pad: PadId) -> &PadView {
        &self.pads[pad_offset(pad)]
    }

    /// Atomically updates a pad's validated settings. Unloaded pads remain a local edit; loaded
    /// pads commit only after audio accepts the corresponding update.
    pub fn update_pad_settings(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        settings.validate().map_err(|error| error.to_string())?;
        let offset = pad_offset(pad);
        if self.pads[offset].settings == settings {
            return Ok(());
        }
        if self.pads[offset].sample.is_none() {
            self.pads[offset].settings = settings;
            return Ok(());
        }
        if !self.current_session_bound[offset] || self.audio.is_none() {
            return Err("loaded sample is not admitted to the current audio session".to_owned());
        }
        self.ensure_project_mutation_available()?;
        self.audio
            .as_mut()
            .expect("current session binding requires an audio controller")
            .update_pad(pad, settings)?;
        self.pads[offset].settings = settings;
        self.commit_project_mutation();
        Ok(())
    }

    fn ensure_project_mutation_available(&self) -> Result<(), String> {
        self.project_session
            .ensure_mutation_available()
            .map_err(|_| "project revision is exhausted".to_owned())
    }

    fn commit_project_mutation(&mut self) {
        self.project_session
            .commit_project_mutation(Instant::now(), || Ok::<(), ()>(()))
            .expect("project mutation was preflighted before its domain commit");
    }

    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    pub fn open_help(&mut self) {
        self.overlay = Some(Overlay::Help);
    }

    pub fn open_palette(&mut self) {
        self.palette.clear();
        self.palette_error = None;
        self.overlay = Some(Overlay::Palette);
    }

    pub fn open_picker(&mut self) {
        let source_parent = self
            .selected_pad_id()
            .and_then(|pad| self.pad(pad).source.as_deref())
            .and_then(|path| path.parent())
            .filter(|path| !path.as_os_str().is_empty())
            .map(ToOwned::to_owned);
        let directory = source_parent.unwrap_or_else(|| self.current_dir.clone());
        self.open_picker_at(directory);
    }

    pub fn open_picker_at(&mut self, directory: impl Into<PathBuf>) {
        let directory = resolve_picker_directory(&self.current_dir, directory.into());
        let request_id = self.file_picker.begin_scan(directory.clone());
        self.queue_worker_request(WorkerRequest::ScanDirectory {
            request_id,
            path: directory,
            show_hidden: self.file_picker.show_hidden(),
        });
        self.overlay = Some(Overlay::FilePicker);
    }

    pub fn close_overlay(&mut self) {
        if matches!(
            self.overlay,
            Some(Overlay::ApplySample { .. } | Overlay::DiscardSample { .. })
        ) {
            self.editor.cancel_confirmation();
            self.apply_sample_context = None;
        }
        if self.overlay == Some(Overlay::Palette) {
            self.palette_error = None;
        }
        if let Some(Overlay::DeviceError(error)) = &self.overlay {
            self.status = format!("{error} · Ctrl+R retries audio");
        }
        self.overlay = None;
    }

    fn cancel_overlay(&mut self) {
        if self.overlay == Some(Overlay::ProjectOpenProgress) {
            let _ = self.cancel_project_open();
            return;
        }
        self.close_overlay();
    }

    pub fn palette_text(&self) -> &str {
        self.palette.text()
    }

    pub fn palette_cursor(&self) -> usize {
        self.palette.cursor()
    }

    pub fn palette_error(&self) -> Option<&str> {
        self.palette_error.as_deref()
    }

    pub fn file_picker(&self) -> &FilePicker {
        &self.file_picker
    }

    pub(crate) fn pad_display_source(&self, offset: usize) -> Option<&Path> {
        self.pads
            .get(offset)
            .and_then(|pad| pad.source.as_deref())
            .or_else(|| {
                self.pending_loads
                    .get(offset)
                    .and_then(Option::as_deref)
                    .map(|pending| pending.path.as_path())
            })
    }

    pub fn take_worker_requests(&mut self) -> Vec<WorkerRequest> {
        mem::take(&mut self.pending_worker_requests)
    }

    pub fn apply_worker_send_error(
        &mut self,
        request: WorkerRequest,
        error: WorkerSendError,
    ) -> bool {
        let affected_offset = match &request {
            WorkerRequest::LoadSample { pad, .. } | WorkerRequest::EditSample { pad, .. } => {
                Some(pad_offset(*pad))
            }
            WorkerRequest::ScanDirectory { .. }
            | WorkerRequest::SaveProject(_)
            | WorkerRequest::ProbeProject { .. }
            | WorkerRequest::DiscardRecovery { .. }
            | WorkerRequest::StageProjectSample(_)
            | WorkerRequest::Shutdown => None,
        };
        let message = error.to_string();
        let applied = match request {
            WorkerRequest::LoadSample {
                pad,
                generation,
                purpose,
                path,
                ..
            } => {
                let offset = pad_offset(pad);
                if let Some(kind) = self.matching_pending_load(offset, generation, purpose, &path) {
                    if error == WorkerSendError::WorkerBusy {
                        if let Some(pending) = self.pending_load_slot_mut(offset, kind).as_mut() {
                            pending.phase = PendingLoadPhase::AwaitingWorker;
                        }
                        self.recovery_cursor.get_or_insert(offset);
                    } else {
                        if let Some(pending) = self.pending_load_slot_mut(offset, kind).as_mut() {
                            pending.phase = PendingLoadPhase::Failed;
                        }
                    }
                    self.pads[offset].state = PadLoadState::Error(message.clone());
                    true
                } else {
                    false
                }
            }
            WorkerRequest::ScanDirectory {
                request_id, path, ..
            } if self.file_picker.pending_directory() == Some(path.as_path()) => self
                .file_picker
                .apply_scan(request_id, Err(message.clone())),
            WorkerRequest::EditSample {
                pad,
                generation,
                recipe,
                ..
            } => {
                let offset = pad_offset(pad);
                let Some(pending) = self.sample_editor.pending[offset].as_mut() else {
                    return false;
                };
                if pending.generation != generation
                    || pending.recipe != recipe
                    || !matches!(pending.phase, PendingEditPhase::WorkerQueued)
                {
                    return false;
                }
                if error == WorkerSendError::WorkerBusy {
                    pending.phase = PendingEditPhase::AwaitingWorker;
                } else {
                    pending.phase = PendingEditPhase::Failed;
                    self.pads[offset].state = PadLoadState::Error(message.clone());
                }
                true
            }
            WorkerRequest::SaveProject(request) => self.restore_busy_project_save(*request, error),
            WorkerRequest::DiscardRecovery {
                token,
                directory,
                project_id,
                revision,
            } => {
                if let Some(applied) = self.restore_busy_project_recovery_discard(
                    token, &directory, project_id, revision, error,
                ) {
                    applied
                } else {
                    self.restore_busy_recovery_cleanup(
                        RecoveryCleanup {
                            token,
                            directory,
                            project_id,
                            revision,
                        },
                        error,
                    )
                }
            }
            WorkerRequest::ProbeProject { token, directory } => {
                let Some(ProjectOpenOperation::Probing {
                    progress,
                    worker_queued,
                }) = self.project_open.as_mut()
                else {
                    return false;
                };
                if progress.token != token || progress.directory != directory {
                    return false;
                }
                *worker_queued = false;
                true
            }
            WorkerRequest::StageProjectSample(request) => {
                let Some(ProjectOpenOperation::Staging(candidate)) = self.project_open.as_mut()
                else {
                    return false;
                };
                if candidate.progress.token != request.token
                    || candidate.decode_in_flight != Some(request.pad)
                    || candidate.document.revision != request.revision
                {
                    return false;
                }
                candidate.decode_in_flight = None;
                true
            }
            WorkerRequest::ScanDirectory { .. } | WorkerRequest::Shutdown => false,
        };
        if applied {
            self.status = message;
            if let Some(offset) = affected_offset {
                self.refresh_editor_for_offset(offset);
            }
        }
        applied
    }

    fn restore_busy_project_save(
        &mut self,
        request: ProjectSaveWorkerRequest,
        error: WorkerSendError,
    ) -> bool {
        let Some(InFlightProjectOperation::Save(save)) = self.in_flight_project.as_ref() else {
            return false;
        };
        let expected = &save.descriptor;
        if request.token != expected.token
            || request.request.kind != expected.kind
            || request.request.snapshot.project_id != expected.project_id
            || request.request.directory != expected.directory
            || request.request.snapshot.revision != expected.revision
        {
            return false;
        }
        let InFlightProjectOperation::Save(save) = self
            .in_flight_project
            .take()
            .expect("matching save operation is present")
        else {
            unreachable!()
        };
        self.project_session.set_in_flight(None);
        if error == WorkerSendError::WorkerBusy {
            match save.descriptor.kind {
                SaveKind::Explicit => self.pending_explicit_save = Some(save),
                SaveKind::Recovery => {
                    self.project_session
                        .set_pending_autosave(Some(crate::AutosaveDescriptor {
                            revision: save.descriptor.revision,
                        }));
                    self.pending_autosave_save = Some(save);
                }
            }
        } else {
            self.project_save_error = Some(ProjectSaveFailure {
                kind: save.descriptor.kind,
                error: ProjectStoreError::Filesystem {
                    operation: "send project save",
                    path: save.descriptor.directory,
                    kind: std::io::ErrorKind::BrokenPipe,
                },
            });
        }
        true
    }

    fn restore_busy_recovery_cleanup(
        &mut self,
        request: RecoveryCleanup,
        error: WorkerSendError,
    ) -> bool {
        let Some(InFlightProjectOperation::Cleanup(expected)) = self.in_flight_project.as_ref()
        else {
            return false;
        };
        if expected != &request {
            return false;
        }
        self.in_flight_project = None;
        if error == WorkerSendError::WorkerBusy {
            self.pending_recovery_cleanup.push_front(request);
        } else {
            self.recovery_cleanup_warning = Some(ProjectStoreError::Filesystem {
                operation: "send recovery cleanup",
                path: request.directory,
                kind: std::io::ErrorKind::BrokenPipe,
            });
        }
        true
    }

    pub fn device_retry_requests(&self) -> usize {
        self.device_retry_requests
    }

    pub fn take_device_retry_requests(&mut self) -> usize {
        mem::take(&mut self.device_retry_requests)
    }

    pub fn retry_default_device(&mut self) -> bool {
        self.retry_default_device_with(open_default_audio)
    }

    pub fn retry_with(&mut self, audio: Box<dyn AudioPort>) -> bool {
        self.recover_audio(audio);
        true
    }

    pub fn shutdown_audio(&mut self) -> Result<(), String> {
        self.audio_format = None;
        self.recovery_cursor = None;
        self.pending_loads.fill_with(|| None);
        self.committed_recovery_loads.fill_with(|| None);
        self.reinstall_pending.fill(false);
        self.current_session_bound.fill(false);
        self.held_pad_by_key.fill(None);
        for pad in &mut self.pads {
            pad.active = false;
        }
        let Some(mut audio) = self.audio.take() else {
            return Ok(());
        };
        let result = audio.stop_all();
        drop(audio);
        result
    }

    fn retry_default_device_with(
        &mut self,
        open_audio: impl FnOnce() -> Result<Box<dyn AudioPort>, String>,
    ) -> bool {
        match open_audio() {
            Ok(audio) => self.recover_audio(audio),
            Err(error) => self.fail_audio(error),
        }
        true
    }

    pub fn set_keyboard_capabilities(&mut self, capabilities: KeyboardCapabilities) {
        self.keyboard_capabilities = capabilities;
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn begin_load(&mut self, pad: PadId, path: impl Into<PathBuf>) -> Option<WorkerRequest> {
        if let Err(error) = self.ensure_project_mutation_available() {
            self.status = error;
            return None;
        }
        let path = path.into();
        let engine_rate = self.audio.as_ref().map(|audio| audio.sample_rate());
        let offset = pad_offset(pad);
        self.invalidate_pending_edit(offset);
        let view = &mut self.pads[offset];
        view.generation = view.generation.wrapping_add(1);
        view.state = if engine_rate.is_some() {
            PadLoadState::Loading
        } else {
            PadLoadState::WaitingForDevice
        };
        let generation = view.generation;

        let request = if let Some(engine_rate) = engine_rate {
            self.pending_loads[offset] = Some(Box::new(PendingLoad {
                path: path.clone(),
                phase: PendingLoadPhase::WorkerQueued,
                kind: PendingLoadKind::User,
            }));
            Some(WorkerRequest::LoadSample {
                pad,
                generation,
                purpose: LoadPurpose::User,
                path,
                engine_rate,
                recipe: SampleEditRecipe::identity(),
            })
        } else {
            self.pending_loads[offset] = Some(Box::new(PendingLoad {
                path,
                phase: PendingLoadPhase::AwaitingWorker,
                kind: PendingLoadKind::User,
            }));
            self.recovery_cursor = Some(offset);
            None
        };
        self.refresh_editor_for_offset(offset);
        request
    }

    /// Starts a bounded worker render of a new recipe. The pad tuple is not changed until the
    /// audio command accepts the rendered buffer.
    pub fn request_sample_edit(
        &mut self,
        pad: PadId,
        recipe: SampleEditRecipe,
    ) -> Result<(), SampleEditRequestError> {
        recipe
            .validate()
            .map_err(|error| SampleEditRequestError::InvalidRecipe(error.to_string()))?;
        let offset = pad_offset(pad);
        if self.audio.is_none() {
            return Err(SampleEditRequestError::AudioUnavailable(
                self.audio_unavailable_message
                    .clone()
                    .unwrap_or_else(|| "audio device is unavailable".to_owned()),
            ));
        }
        if self.pending_loads[offset].is_some() {
            return Err(SampleEditRequestError::LoadPending);
        }
        let Some(base) = self.sample_editor.commits[offset].base.as_ref().cloned() else {
            return Err(SampleEditRequestError::EmptyPad);
        };
        let Some(base_preview) = self.sample_editor.commits[offset]
            .base_preview
            .as_ref()
            .cloned()
        else {
            return Err(SampleEditRequestError::EmptyPad);
        };
        self.project_session
            .ensure_mutation_available()
            .map_err(|_| SampleEditRequestError::ProjectRevisionExhausted)?;
        self.start_sample_edit(offset, base, base_preview, recipe, PendingEditKind::Apply)
    }

    /// Re-renders and installs the previous recipe through the same worker/audio path as Apply.
    /// The checkpoint remains available until that replacement is admitted.
    pub fn undo_sample_edit(&mut self, pad: PadId) -> Result<(), SampleEditRequestError> {
        let offset = pad_offset(pad);
        if self.audio.is_none() {
            return Err(SampleEditRequestError::AudioUnavailable(
                self.audio_unavailable_message
                    .clone()
                    .unwrap_or_else(|| "audio device is unavailable".to_owned()),
            ));
        }
        let Some(checkpoint) = self.sample_editor.undo[offset].as_ref() else {
            return Err(SampleEditRequestError::NoUndo);
        };
        // At the same device rate the checkpoint's base is the exact prior tuple. After a
        // recovery it intentionally re-renders the old recipe from the newly decoded base.
        let _retained_prior_rendered = Arc::clone(&checkpoint.rendered);
        let _retained_prior_base_preview = Arc::clone(&checkpoint.base_preview);
        let _retained_prior_rendered_preview = Arc::clone(&checkpoint.rendered_preview);
        let (base, base_preview) = if self
            .audio
            .as_ref()
            .is_some_and(|audio| checkpoint.base.sample_rate() == audio.sample_rate())
        {
            (
                Arc::clone(&checkpoint.base),
                Arc::clone(&checkpoint.base_preview),
            )
        } else if let (Some(base), Some(base_preview)) = (
            self.sample_editor.commits[offset].base.as_ref(),
            self.sample_editor.commits[offset].base_preview.as_ref(),
        ) {
            (Arc::clone(base), Arc::clone(base_preview))
        } else {
            return Err(SampleEditRequestError::EmptyPad);
        };
        self.project_session
            .ensure_mutation_available()
            .map_err(|_| SampleEditRequestError::ProjectRevisionExhausted)?;
        self.start_sample_edit(
            offset,
            base,
            base_preview,
            checkpoint.recipe,
            PendingEditKind::Undo,
        )
    }

    pub fn committed_sample_recipe(&self, pad: PadId) -> Option<SampleEditRecipe> {
        let view = self.pad(pad);
        view.sample
            .as_ref()
            .map(|_| self.sample_editor.commits[pad_offset(pad)].recipe)
    }

    pub fn base_sample(&self, pad: PadId) -> Option<&Arc<SampleBuffer>> {
        self.sample_editor.commits[pad_offset(pad)].base.as_ref()
    }

    pub fn edit_preview(&self, pad: PadId) -> Option<&EditPreview> {
        self.sample_editor.commits[pad_offset(pad)]
            .base_preview
            .as_ref()
    }

    pub fn sample_edit_status(&self, pad: PadId) -> SampleEditStatus {
        let offset = pad_offset(pad);
        match self.sample_editor.pending[offset]
            .as_ref()
            .map(|pending| &pending.phase)
        {
            _ if self.sample_editor.generation_exhausted[offset] => {
                SampleEditStatus::GenerationExhausted
            }
            Some(PendingEditPhase::AwaitingWorker) => SampleEditStatus::AwaitingWorker,
            Some(PendingEditPhase::WorkerQueued) => SampleEditStatus::Rendering,
            Some(PendingEditPhase::Ready(_)) => SampleEditStatus::ReadyToInstall,
            Some(PendingEditPhase::Failed) => SampleEditStatus::Failed,
            None if self.sample_editor.undo[offset].is_some() => SampleEditStatus::UndoAvailable,
            None => SampleEditStatus::Idle,
        }
    }

    /// Read-only state for the Sample workspace. The editor intentionally cannot observe or
    /// manipulate generations, buffers, or worker queues through this projection.
    pub fn sample_editor_context(&self, pad: PadId) -> SampleEditorContext {
        let base = self.base_sample(pad);
        SampleEditorContext {
            pad,
            source_generation: self.sample_editor.commits[pad_offset(pad)].source_generation,
            committed: self.committed_sample_recipe(pad),
            base_frames: base.map(|sample| sample.frames()),
            base_rate: base.map(|sample| sample.sample_rate()),
            settings: self.pad(pad).settings,
            edit_status: self.sample_edit_status(pad),
            device_available: self.audio.is_some(),
        }
    }

    fn start_sample_edit(
        &mut self,
        offset: usize,
        base: Arc<SampleBuffer>,
        base_preview: EditPreview,
        recipe: SampleEditRecipe,
        kind: PendingEditKind,
    ) -> Result<(), SampleEditRequestError> {
        let sample_rate = self
            .audio
            .as_ref()
            .map(|audio| audio.sample_rate())
            .ok_or_else(|| {
                SampleEditRequestError::AudioUnavailable("audio device is unavailable".to_owned())
            })?;
        if base.sample_rate() != sample_rate {
            return Err(SampleEditRequestError::RecoveryPending);
        }
        let Some(generation) = self.sample_editor.generations[offset].checked_add(1) else {
            self.sample_editor.generation_exhausted[offset] = true;
            let error = SampleEditRequestError::GenerationExhausted;
            self.status = error.to_string();
            return Err(error);
        };
        self.sample_editor.generations[offset] = generation;
        self.sample_editor.generation_exhausted[offset] = false;
        self.sample_editor.deferred_results[offset] = None;
        self.sample_editor.pending[offset] = Some(Box::new(PendingEdit {
            generation,
            base: Arc::clone(&base),
            base_preview: Arc::clone(&base_preview),
            recipe,
            kind,
            phase: PendingEditPhase::WorkerQueued,
        }));
        self.pads[offset].state = PadLoadState::Loading;
        self.queue_worker_request(WorkerRequest::EditSample {
            pad: pad_from_offset(offset),
            generation,
            base,
            base_preview,
            recipe,
        });
        Ok(())
    }

    fn invalidate_pending_edit(&mut self, offset: usize) {
        self.sample_editor.generations[offset] =
            self.sample_editor.generations[offset].saturating_add(1);
        self.sample_editor.pending[offset] = None;
        self.sample_editor.deferred_results[offset] = None;
    }

    fn suspend_pending_sample_edits(&mut self) {
        for offset in 0..PAD_VIEW_COUNT {
            self.sample_editor.generations[offset] =
                self.sample_editor.generations[offset].saturating_add(1);
            self.sample_editor.deferred_results[offset] = None;
            if let Some(pending) = self.sample_editor.pending[offset].as_mut() {
                pending.phase = PendingEditPhase::Failed;
            }
        }
    }

    pub fn apply_worker_result(&mut self, result: WorkerResult) -> bool {
        let result = match result {
            WorkerResult::ProjectProbed {
                token,
                directory,
                result,
            } => return self.apply_project_probe(token, directory, result),
            WorkerResult::ProjectSampleStaged {
                token,
                pad,
                revision,
                path,
                recipe,
                result,
            } => {
                return self
                    .apply_project_sample_staged(token, pad, revision, path, recipe, result);
            }
            WorkerResult::ProjectSaved {
                token,
                kind,
                project_id,
                directory,
                revision,
                result,
            } => {
                return self
                    .apply_project_saved(token, kind, project_id, directory, revision, result);
            }
            WorkerResult::RecoveryDiscarded {
                token,
                directory,
                project_id,
                revision,
                result,
            } => {
                if let Some(applied) = self.apply_project_recovery_discarded(
                    token,
                    directory.clone(),
                    project_id,
                    revision,
                    result.clone(),
                ) {
                    return applied;
                }
                return self.apply_recovery_cleanup(token, directory, project_id, revision, result);
            }
            result => result,
        };
        if let WorkerResult::Edited {
            pad,
            generation,
            recipe,
            result,
        } = result
        {
            if !self.pending_edit_matches(pad, generation, recipe) {
                return false;
            }
            if self.edit_result_advanced {
                let offset = pad_offset(pad);
                self.sample_editor.deferred_results[offset] =
                    Some(Box::new(WorkerResult::Edited {
                        pad,
                        generation,
                        recipe,
                        result,
                    }));
                return true;
            }
            self.edit_result_advanced = true;
            return self.apply_edited_worker_result(pad, generation, recipe, result);
        }
        let WorkerResult::Loaded {
            pad,
            generation,
            purpose,
            path,
            result,
        } = result
        else {
            let WorkerResult::Scanned {
                request_id,
                path,
                result,
            } = result
            else {
                return false;
            };
            if self.file_picker.pending_directory() != Some(path.as_path()) {
                return false;
            }
            let error = result.as_ref().err().cloned();
            let truncated = result.as_ref().is_ok_and(|scan| scan.truncated());
            let applied = self.file_picker.apply_scan(request_id, result);
            if applied {
                if let Some(error) = error {
                    self.status = error;
                } else if truncated {
                    self.status = format!(
                        "directory results limited to the first {MAX_DIRECTORY_ENTRIES} entries"
                    );
                }
            }
            return applied;
        };
        let offset = pad_offset(pad);
        let Some(kind) = self.matching_pending_load(offset, generation, purpose, &path) else {
            return false;
        };

        let loaded = match result {
            Ok(loaded) => loaded,
            Err(error) => {
                let error = error.to_string();
                if let Some(pending) = self.pending_load_slot_mut(offset, kind).as_mut() {
                    pending.phase = PendingLoadPhase::Failed;
                }
                self.pads[offset].state = PadLoadState::Error(error.clone());
                self.status = error;
                self.refresh_editor_for_offset(offset);
                return true;
            }
        };

        *self.pending_load_slot_mut(offset, kind) = Some(Box::new(PendingLoad {
            path,
            phase: PendingLoadPhase::Ready(loaded),
            kind,
        }));
        let Some(sample_rate) = self.audio.as_ref().map(|audio| audio.sample_rate()) else {
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            self.recovery_cursor = Some(offset);
            self.refresh_editor_for_offset(offset);
            return true;
        };
        if self
            .pending_load_slot(offset, kind)
            .as_ref()
            .and_then(|pending| match &pending.phase {
                PendingLoadPhase::Ready(loaded) => Some(loaded.rendered.sample_rate()),
                _ => None,
            })
            != Some(sample_rate)
        {
            if let Some(pending) = self.pending_load_slot_mut(offset, kind).as_mut() {
                pending.phase = PendingLoadPhase::AwaitingWorker;
            }
            self.pads[offset].state = PadLoadState::Loading;
            self.recovery_cursor = Some(offset);
            self.refresh_editor_for_offset(offset);
            return true;
        }
        self.install_pending_load(offset, kind);
        true
    }

    fn apply_project_saved(
        &mut self,
        token: ProjectToken,
        kind: SaveKind,
        project_id: ProjectId,
        directory: PathBuf,
        revision: u64,
        result: Result<SaveReceipt, ProjectStoreError>,
    ) -> bool {
        let Some(InFlightProjectOperation::Save(save)) = self.in_flight_project.as_ref() else {
            return false;
        };
        let expected = &save.descriptor;
        if expected.token != token
            || expected.kind != kind
            || expected.project_id != project_id
            || expected.directory != directory
            || expected.revision != revision
        {
            return false;
        }
        if let Ok(receipt) = &result
            && (receipt.kind != kind
                || receipt.project_id != project_id
                || receipt.revision != revision)
        {
            return false;
        }

        let InFlightProjectOperation::Save(save) = self
            .in_flight_project
            .take()
            .expect("matching save operation is present")
        else {
            unreachable!()
        };
        self.project_session.set_in_flight(None);
        match result {
            Err(error) => {
                self.project_save_error = Some(ProjectSaveFailure { kind, error });
                if kind == SaveKind::Recovery {
                    self.autosave_retry_clock_pending = true;
                    self.autosave_retry_since = None;
                }
            }
            Ok(receipt) => {
                self.apply_project_asset_mappings(&save.snapshot, &receipt);
                self.project_save_error = None;
                self.autosave_retry_clock_pending = false;
                self.autosave_retry_since = None;
                match kind {
                    SaveKind::Explicit => {
                        if self
                            .pending_autosave_save
                            .as_ref()
                            .is_some_and(|pending| pending.descriptor.revision <= revision)
                        {
                            self.pending_autosave_save = None;
                            self.project_session.set_pending_autosave(None);
                        }
                        if save.save_as {
                            self.project_session.adopt_saved_project(
                                project_id,
                                receipt.directory.clone(),
                                save.snapshot.name,
                                revision,
                            );
                            self.save_as_identity = None;
                        } else {
                            self.project_session.mark_explicit_saved(revision);
                        }
                        if let Ok(cleanup_token) = self.allocate_project_token() {
                            debug_assert!(
                                self.pending_recovery_cleanup.len() < WORKER_CHANNEL_CAPACITY
                            );
                            self.pending_recovery_cleanup.push_back(RecoveryCleanup {
                                token: cleanup_token,
                                directory: receipt.directory,
                                project_id,
                                revision: self.project_session.autosaved_revision(),
                            });
                        }
                    }
                    SaveKind::Recovery => self.project_session.mark_autosaved(revision),
                }
            }
        }
        true
    }

    fn apply_project_asset_mappings(
        &mut self,
        snapshot: &ProjectSaveSnapshot,
        receipt: &SaveReceipt,
    ) {
        for mapping in &receipt.mappings {
            let Some(saved_pad) = snapshot.pads.iter().find(|pad| {
                pad.pad == mapping.pad
                    && pad.source_generation == mapping.source_generation
                    && pad.fingerprint == mapping.fingerprint
            }) else {
                continue;
            };
            let offset = pad_offset(saved_pad.pad);
            if self.sample_editor.commits[offset].source_generation == mapping.source_generation
                && self.sample_editor.commits[offset].fingerprint == Some(mapping.fingerprint)
            {
                self.pads[offset].source = Some(mapping.project_path.clone());
            }
        }
    }

    fn apply_recovery_cleanup(
        &mut self,
        token: ProjectToken,
        directory: PathBuf,
        project_id: ProjectId,
        revision: u64,
        result: Result<(), ProjectStoreError>,
    ) -> bool {
        let Some(InFlightProjectOperation::Cleanup(cleanup)) = self.in_flight_project.as_ref()
        else {
            return false;
        };
        if cleanup.token != token
            || cleanup.directory != directory
            || cleanup.project_id != project_id
            || cleanup.revision != revision
        {
            return false;
        }
        self.in_flight_project = None;
        self.recovery_cleanup_warning = result.err();
        true
    }

    fn apply_edited_worker_result(
        &mut self,
        pad: PadId,
        generation: u64,
        recipe: SampleEditRecipe,
        result: Result<RenderedSample, String>,
    ) -> bool {
        let offset = pad_offset(pad);
        let Some(pending) = self.sample_editor.pending[offset].as_mut() else {
            return false;
        };
        if pending.generation != generation
            || pending.recipe != recipe
            || !matches!(pending.phase, PendingEditPhase::WorkerQueued)
        {
            return false;
        }
        match result {
            Ok(rendered) => pending.phase = PendingEditPhase::Ready(rendered),
            Err(error) => {
                pending.phase = PendingEditPhase::Failed;
                self.pads[offset].state = PadLoadState::Error(error.clone());
                self.status = error;
            }
        }
        self.refresh_editor_for_offset(offset);
        true
    }

    fn press_pad(&mut self, index: usize) {
        self.trigger_pad(index, true);
    }

    fn trigger_pad(&mut self, index: usize, track_physical_hold: bool) {
        if self.held_pad_by_key.get(index).is_some_and(Option::is_some) {
            return;
        }
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        let _ = self.select_pad(index);
        if self.patterns.view() == WorkspaceView::Pattern {
            let step = self.patterns.cursor().step();
            self.patterns.move_cursor_to(pad, step);
        }
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let recording = self.patterns.is_recording();
        let records_duration = self.pads[pad_offset(pad)].settings.mode != PlaybackMode::OneShot;
        let result = if recording {
            audio.trigger_live_tracked(pad, 1.0).map(Some)
        } else {
            audio.trigger_live(pad, 1.0).map(|()| None)
        };
        match result {
            Ok(command)
                if track_physical_hold
                    && (self.keyboard_capabilities.release_events
                        || self.pads[pad_offset(pad)].settings.mode != PlaybackMode::OneShot) =>
            {
                self.held_pad_by_key[index] = Some(pad);
                if let Some(command) = command {
                    self.patterns.note_live_trigger_with_duration(
                        index,
                        command,
                        pad,
                        1.0,
                        records_duration,
                    );
                }
            }
            Ok(command) => {
                if let Some(command) = command {
                    self.patterns.note_live_trigger_with_duration(
                        index,
                        command,
                        pad,
                        1.0,
                        records_duration,
                    );
                }
            }
            Err(error) => self.status = error,
        }
    }

    fn apply_device_error_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press
            && key.modifiers == KeyModifiers::NONE
            && matches!(key.code, KeyCode::Char('r' | 'R'))
        {
            self.device_retry_requests = self.device_retry_requests.saturating_add(1);
        }
    }

    fn apply_palette_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let text_changed = match key.code {
            KeyCode::Enter => {
                self.execute_palette();
                false
            }
            KeyCode::Left => {
                self.palette.move_left();
                false
            }
            KeyCode::Right => {
                self.palette.move_right();
                false
            }
            KeyCode::Home => {
                self.palette.move_home();
                false
            }
            KeyCode::End => {
                self.palette.move_end();
                false
            }
            KeyCode::Backspace => {
                let prior_len = self.palette.text().len();
                self.palette.backspace();
                self.palette.text().len() != prior_len
            }
            KeyCode::Delete => {
                let prior_len = self.palette.text().len();
                self.palette.delete();
                self.palette.text().len() != prior_len
            }
            KeyCode::Char(character)
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.palette.insert(character);
                true
            }
            _ => return,
        };
        if text_changed {
            self.palette_error = None;
        }
    }

    fn execute_palette(&mut self) {
        let command = match parse_palette(self.palette.text()) {
            Ok(command) => command,
            Err(error) => {
                self.palette_error = Some(error);
                return;
            }
        };
        self.palette_error = None;
        match command {
            PaletteCommand::OpenPicker => self.open_picker(),
            PaletteCommand::LoadPath(path) => {
                self.begin_selected_load(path);
                self.overlay = None;
            }
            PaletteCommand::Bank(bank) => {
                if self.editor.is_dirty() {
                    self.status = "discard sample draft before changing bank".to_owned();
                    return;
                }
                self.active_bank = bank;
                self.sync_editor_to_selected_pad();
                self.overlay = None;
            }
            PaletteCommand::Select(index) => {
                if self.select_pad(index) {
                    self.overlay = None;
                }
            }
            PaletteCommand::StopAll => {
                self.stop_all();
                self.overlay = None;
            }
            PaletteCommand::Help => self.open_help(),
            PaletteCommand::Quit => {
                self.should_quit = true;
                self.overlay = None;
            }
            PaletteCommand::Pattern(slot) => self.select_pattern_slot(usize::from(slot)),
            PaletteCommand::Tempo(tempo) => self.apply_pattern_edit(|patterns| {
                patterns
                    .set_tempo(sampler_core::Tempo::new(tempo).expect("palette validated tempo"))
            }),
            PaletteCommand::Bars(bars) => {
                self.apply_pattern_edit(|patterns| patterns.set_bars(bars))
            }
            PaletteCommand::Resolution(resolution) => {
                self.apply_pattern_edit(|patterns| patterns.set_resolution(resolution))
            }
            PaletteCommand::Swing(swing) => {
                self.apply_pattern_edit(|patterns| patterns.set_swing(swing))
            }
            PaletteCommand::Quantize(strength) => {
                self.apply_pattern_edit(|patterns| patterns.set_quantize(strength))
            }
            PaletteCommand::Record => self.toggle_pattern_recording(),
            PaletteCommand::Play => self.start_pattern_playback(),
            PaletteCommand::Stop => self.stop_pattern_playback(),
            PaletteCommand::ClearPattern => self.open_clear_pattern(),
            PaletteCommand::TrimStart(frame) => self.apply_palette_sample(|editor| {
                editor.set_marker_to_frame(SampleMarker::Start, frame)
            }),
            PaletteCommand::TrimEnd(frame) => self.apply_palette_sample(|editor| {
                editor.set_marker_to_frame(SampleMarker::End, frame)
            }),
            PaletteCommand::Normalize(enabled) => self.apply_palette_sample(|editor| {
                if editor.draft().normalize != enabled {
                    editor.toggle_normalize();
                }
                Ok(())
            }),
            PaletteCommand::Reverse(enabled) => self.apply_palette_sample(|editor| {
                if editor.draft().reversed != enabled {
                    editor.toggle_reverse();
                }
                Ok(())
            }),
            PaletteCommand::Pitch(pitch) => self.apply_palette_editor_settings(|settings| {
                settings.pitch_semitones = pitch;
            }),
            PaletteCommand::Mode(mode) => self.apply_palette_editor_settings(|settings| {
                settings.mode = mode;
            }),
            PaletteCommand::ApplySample => {
                if self.require_sample_workspace() {
                    self.request_editor_apply();
                }
            }
            PaletteCommand::UndoSample => {
                if self.require_sample_workspace() {
                    self.request_editor_undo();
                }
            }
        }
    }

    fn apply_palette_sample(
        &mut self,
        reduce: impl FnOnce(&mut SampleEditor) -> Result<(), String>,
    ) {
        if !self.require_sample_workspace() {
            return;
        }
        if let Err(error) = reduce(&mut self.editor) {
            self.palette_error = Some(error);
        }
    }

    fn apply_palette_editor_settings(&mut self, reduce: impl FnOnce(&mut PadSettings)) {
        if !self.require_sample_workspace() {
            return;
        }
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let mut settings = self.editor.settings();
        reduce(&mut settings);
        match self.update_pad_settings(pad, settings) {
            Ok(()) => self.sync_editor_to_selected_pad(),
            Err(error) => self.palette_error = Some(error),
        }
    }

    fn require_sample_workspace(&mut self) -> bool {
        if self.patterns.view() != WorkspaceView::Sample {
            self.palette_error = Some("sample command requires Sample workspace".to_owned());
            return false;
        }
        if self.editor.committed().is_none() {
            self.palette_error = Some("selected pad is empty".to_owned());
            return false;
        }
        if self.editor_operation_pending() {
            self.palette_error = Some("sample edit is pending".to_owned());
            return false;
        }
        if !self.editor.can_edit() {
            self.palette_error =
                Some("sample editor context must be discarded before editing".to_owned());
            return false;
        }
        true
    }

    fn require_sample_editor_key(&mut self) -> bool {
        if self.editor.committed().is_none() {
            self.status = "selected pad is empty".to_owned();
            return false;
        }
        if self.editor_operation_pending() {
            self.status = "sample edit is pending".to_owned();
            return false;
        }
        if !self.editor.can_edit() {
            return false;
        }
        true
    }

    fn apply_picker_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Up => self.file_picker.move_cursor(-1),
            KeyCode::Down => self.file_picker.move_cursor(1),
            KeyCode::Home => self.file_picker.select_first(),
            KeyCode::End => self.file_picker.select_last(),
            KeyCode::Backspace => self.open_picker_parent(),
            KeyCode::Char('.') if key.modifiers == KeyModifiers::NONE => {
                let request_id = self.file_picker.toggle_hidden();
                self.queue_current_picker_scan(request_id);
            }
            KeyCode::Enter => self.open_picker_selection(),
            _ => {}
        }
    }

    fn apply_help_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press
            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('?')
        {
            self.overlay = None;
        }
    }

    fn apply_workspace_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press && self.apply_global_pattern_key(key) {
            return;
        }
        match self.patterns.view() {
            WorkspaceView::Perform => self.apply_perform_key(key),
            WorkspaceView::Pattern => self.apply_pattern_key(key),
            WorkspaceView::Sample => self.apply_sample_key(key),
        }
    }

    fn apply_global_pattern_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab
                if matches!(
                    (key.code, key.modifiers),
                    (KeyCode::Tab, KeyModifiers::NONE | KeyModifiers::SHIFT)
                        | (KeyCode::BackTab, KeyModifiers::SHIFT)
                ) =>
            {
                if self.patterns.view() == WorkspaceView::Sample && self.editor_operation_pending()
                {
                    return true;
                }
                let view = if key.code == KeyCode::BackTab || key.modifiers == KeyModifiers::SHIFT {
                    self.patterns.view().previous()
                } else {
                    self.patterns.view().next()
                };
                if self.patterns.view() == WorkspaceView::Sample
                    && view != WorkspaceView::Sample
                    && self.editor.is_dirty()
                {
                    self.overlay = Some(Overlay::DiscardSample {
                        pad: self.editor.pad(),
                    });
                    return true;
                }
                self.patterns.set_view(view);
                if view == WorkspaceView::Sample {
                    self.sync_editor_to_selected_pad();
                }
                true
            }
            KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                if self.pattern_transport_is_playing() {
                    self.stop_pattern_playback();
                } else {
                    self.start_pattern_playback();
                }
                true
            }
            KeyCode::Char('r' | 'R') if is_explicit_device_retry(key) => {
                self.toggle_pattern_recording();
                true
            }
            KeyCode::Char(',') if key.modifiers == KeyModifiers::NONE => {
                self.change_pattern_slot(-1);
                true
            }
            KeyCode::Char('.') if key.modifiers == KeyModifiers::NONE => {
                self.change_pattern_slot(1);
                true
            }
            _ => false,
        }
    }

    fn apply_perform_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Char('?')
                    if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
                {
                    self.open_help();
                }
                KeyCode::Char(':')
                    if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
                {
                    self.open_palette();
                }
                KeyCode::Char('l') if key.modifiers == KeyModifiers::NONE => {
                    self.open_picker();
                }
                KeyCode::Left => self.move_selection(-1, 0),
                KeyCode::Right => self.move_selection(1, 0),
                KeyCode::Up => self.move_selection(0, -1),
                KeyCode::Down => self.move_selection(0, 1),
                KeyCode::Enter => self.trigger_pad(self.selected_pad, false),
                _ => {
                    if let Some(action) = map_key(key, self.keyboard_capabilities) {
                        self.apply(action);
                    }
                }
            }
        } else if let Some(action) = map_key(key, self.keyboard_capabilities) {
            self.apply(action);
        }
    }

    fn apply_pattern_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            if let Some(action) = map_key(key, self.keyboard_capabilities) {
                self.apply(action);
            }
            return;
        }
        match key.code {
            KeyCode::Char('?')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.open_help();
            }
            KeyCode::Char(':')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.open_palette();
            }
            KeyCode::Left => self.patterns.move_cursor_steps(-1),
            KeyCode::Right => self.patterns.move_cursor_steps(1),
            KeyCode::Up => self.move_pattern_cursor_pad(-1),
            KeyCode::Down => self.move_pattern_cursor_pad(1),
            KeyCode::PageUp => self.patterns.move_cursor_bar(-1),
            KeyCode::PageDown => self.patterns.move_cursor_bar(1),
            KeyCode::Enter => self.apply_pattern_edit(|patterns| patterns.toggle_step()),
            KeyCode::Delete if key.modifiers == KeyModifiers::CONTROL => self.open_clear_pattern(),
            KeyCode::Delete => self.apply_pattern_edit(|patterns| patterns.delete_step()),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.apply_pattern_edit(|patterns| patterns.adjust_velocity(0.05))
            }
            KeyCode::Char('-') => {
                self.apply_pattern_edit(|patterns| patterns.adjust_velocity(-0.05))
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::NONE => {
                self.apply_pattern_edit(|patterns| patterns.undo_clear())
            }
            _ => {
                if let Some(action) = map_key(key, self.keyboard_capabilities) {
                    self.apply(action);
                }
            }
        }
    }

    fn apply_sample_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.editor_operation_pending() && key.code == KeyCode::Esc {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key
                .modifiers
                .difference(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                .is_empty()
            && matches!(key.code, KeyCode::Char('z' | 'Z'))
        {
            if !self.require_sample_editor_key() {
                return;
            }
            self.request_editor_undo();
            return;
        }
        if matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Enter
                | KeyCode::Char('m' | 'n' | 'u' | 'o' | 'g' | 'l')
        ) && !self.require_sample_editor_key()
        {
            return;
        }
        match key.code {
            KeyCode::Char('?')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.open_help()
            }
            KeyCode::Char(':')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.open_palette()
            }
            KeyCode::Left => self
                .editor
                .move_marker(-1, key.modifiers == KeyModifiers::SHIFT),
            KeyCode::Right => self
                .editor
                .move_marker(1, key.modifiers == KeyModifiers::SHIFT),
            KeyCode::PageUp if key.modifiers == KeyModifiers::NONE => self.editor.zoom_in(),
            KeyCode::PageDown if key.modifiers == KeyModifiers::NONE => self.editor.zoom_out(),
            KeyCode::Char('m') if key.modifiers == KeyModifiers::NONE => {
                self.editor.set_marker(match self.editor.marker() {
                    SampleMarker::Start => SampleMarker::End,
                    SampleMarker::End => SampleMarker::Start,
                });
            }
            KeyCode::Char('n') if key.modifiers == KeyModifiers::NONE => {
                self.editor.toggle_normalize()
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::NONE => {
                self.editor.toggle_reverse()
            }
            KeyCode::Up if key.modifiers == KeyModifiers::NONE => self.adjust_editor_pitch(1),
            KeyCode::Down if key.modifiers == KeyModifiers::NONE => self.adjust_editor_pitch(-1),
            KeyCode::Char('o') if key.modifiers == KeyModifiers::NONE => {
                self.set_editor_mode(PlaybackMode::OneShot)
            }
            KeyCode::Char('g') if key.modifiers == KeyModifiers::NONE => {
                self.set_editor_mode(PlaybackMode::Gate)
            }
            KeyCode::Char('l') if key.modifiers == KeyModifiers::NONE => {
                self.set_editor_mode(PlaybackMode::Loop)
            }
            KeyCode::Enter if key.modifiers == KeyModifiers::NONE => self.request_editor_apply(),
            KeyCode::Esc if key.modifiers == KeyModifiers::NONE => self.request_editor_escape(),
            _ => {}
        }
    }

    fn sync_editor_to_selected_pad(&mut self) {
        if let Some(pad) = self.selected_pad_id() {
            let context = self.sample_editor_context(pad);
            self.editor.sync_context(context);
        }
    }

    fn refresh_editor_for_offset(&mut self, offset: usize) {
        if self.selected_pad_id() == Some(pad_from_offset(offset)) {
            self.sync_editor_to_selected_pad();
        }
    }

    fn request_editor_apply(&mut self) {
        if self.editor_operation_pending() {
            return;
        }
        let retry_error = matches!(
            self.editor.status(),
            crate::sample_editor::SampleEditorStatus::Error(_)
        );
        if retry_error && self.pending_loads[pad_offset(self.editor.pad())].is_some() {
            return;
        }
        let Some(SampleEditorIntent::Apply { pad, recipe }) = self.editor.request_apply() else {
            return;
        };
        let Some(frames) = self.base_sample(pad).map(|sample| sample.frames()) else {
            self.editor
                .observe_apply_failed(SampleEditorError::SelectedPadReplaced);
            return;
        };
        let before_frames = self
            .committed_sample_recipe(pad)
            .and_then(|before| before.frame_range(frames).ok())
            .map_or(0, |range| range.len());
        let after_frames = recipe.frame_range(frames).map_or(0, |range| range.len());
        let Some(base_rate) = self.base_sample(pad).map(|sample| sample.sample_rate()) else {
            self.editor
                .observe_apply_failed(SampleEditorError::SelectedPadReplaced);
            return;
        };
        self.apply_sample_context = Some(ApplySampleContext {
            pad,
            pad_generation: self.pads[pad_offset(pad)].generation,
            source: self.pads[pad_offset(pad)].source.clone(),
            base_frames: frames,
            base_rate,
        });
        self.overlay = Some(Overlay::ApplySample {
            pad,
            before_frames,
            after_frames,
        });
    }

    fn request_editor_undo(&mut self) {
        if self.editor_operation_pending() {
            return;
        }
        if let Some(SampleEditorIntent::Undo { pad }) = self.editor.request_undo()
            && let Err(error) = self.undo_sample_edit(pad)
        {
            self.status = error.to_string();
            self.editor
                .observe_undo_failed(SampleEditorError::InstallFailed);
        }
    }

    fn request_editor_escape(&mut self) {
        match self.editor.escape() {
            SampleEditorIntent::ReturnToPerform => self.patterns.set_view(WorkspaceView::Perform),
            SampleEditorIntent::ConfirmDiscard { pad } => {
                self.overlay = Some(Overlay::DiscardSample { pad })
            }
            _ => {}
        }
    }

    fn apply_sample_apply_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press || key.code != KeyCode::Enter {
            return;
        }
        let Some(Overlay::ApplySample { pad, .. }) = self.overlay.clone() else {
            return;
        };
        let selected = self.selected_pad_id();
        let context_matches = self.apply_sample_context.as_ref().is_some_and(|context| {
            context.pad == pad
                && context.pad_generation == self.pads[pad_offset(pad)].generation
                && context.source == self.pads[pad_offset(pad)].source
                && self.base_sample(pad).is_some_and(|base| {
                    base.frames() == context.base_frames && base.sample_rate() == context.base_rate
                })
        });
        if selected != Some(pad)
            || self.editor.pad() != pad
            || !context_matches
            || self.editor_operation_pending()
        {
            if matches!(
                self.editor.status(),
                crate::sample_editor::SampleEditorStatus::ApplyConfirmation
            ) {
                self.editor
                    .observe_apply_failed(SampleEditorError::SelectedPadReplaced);
            }
            self.status = "sample changed while apply confirmation was open".to_owned();
            self.reject_apply_confirmation();
            return;
        }
        let Some(SampleEditorIntent::Apply { recipe, .. }) = self.editor.confirm_apply() else {
            self.reject_apply_confirmation();
            return;
        };
        match self.request_sample_edit(pad, recipe) {
            Ok(()) => {
                self.editor.observe_pending();
                self.overlay = None;
                self.apply_sample_context = None;
            }
            Err(error) => {
                self.status = error.to_string();
                self.editor
                    .observe_apply_failed(SampleEditorError::InstallFailed);
                self.reject_apply_confirmation();
            }
        }
    }

    /// Applies the terminal half of every rejected Apply confirmation. This deliberately does
    /// not call `cancel_confirmation`: the caller's typed error must remain visible with its
    /// draft intact, while modal and token ownership always disappear together.
    fn reject_apply_confirmation(&mut self) {
        self.overlay = None;
        self.apply_sample_context = None;
    }

    fn apply_sample_discard_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
            self.editor.confirm_discard();
            self.sync_editor_to_selected_pad();
            self.overlay = None;
        }
    }

    fn adjust_editor_pitch(&mut self, delta: i8) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let mut settings = self.editor.settings();
        settings.pitch_semitones = (settings.pitch_semitones + f32::from(delta)).clamp(-24.0, 24.0);
        self.admit_editor_settings(pad, settings);
    }

    fn set_editor_mode(&mut self, mode: PlaybackMode) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let mut settings = self.editor.settings();
        settings.mode = mode;
        self.admit_editor_settings(pad, settings);
    }

    fn admit_editor_settings(&mut self, pad: PadId, settings: PadSettings) {
        match self.update_pad_settings(pad, settings) {
            Ok(()) => self.sync_editor_to_selected_pad(),
            Err(error) => self.status = error,
        }
    }

    fn apply_clear_pattern_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
            let Some(Overlay::ClearPattern { slot, .. }) = self.overlay.clone() else {
                return;
            };
            self.patterns.select_slot(slot);
            self.apply_pattern_edit(|patterns| patterns.clear_selected());
            if let Some(audio) = self.audio.as_mut()
                && let Err(error) = audio.set_record_capture(None)
            {
                self.status = error;
            }
            self.overlay = None;
        }
    }

    fn move_selection(&mut self, horizontal: isize, vertical: isize) {
        let row = self.selected_pad / 4;
        let column = self.selected_pad % 4;
        let row = row.saturating_add_signed(vertical).min(3);
        let column = column.saturating_add_signed(horizontal).min(3);
        self.select_pad(row * 4 + column);
    }

    fn select_pad(&mut self, index: usize) -> bool {
        let Some(pad) = self.pad_in_active_bank(index) else {
            return false;
        };
        if self.editor.pad() != pad && self.editor.is_dirty() {
            self.status = "discard sample draft before selecting another pad".to_owned();
            return false;
        }
        self.selected_pad = index;
        if self.patterns.view() == WorkspaceView::Sample {
            self.sync_editor_to_selected_pad();
        }
        true
    }

    fn editor_operation_pending(&self) -> bool {
        matches!(
            self.sample_edit_status(self.editor.pad()),
            SampleEditStatus::AwaitingWorker
                | SampleEditStatus::Rendering
                | SampleEditStatus::ReadyToInstall
        ) || matches!(
            self.editor.status(),
            crate::sample_editor::SampleEditorStatus::Pending
        )
    }

    fn move_pattern_cursor_pad(&mut self, delta: i8) {
        let cursor = self.patterns.cursor();
        let index = i16::from(cursor.pad().index())
            .saturating_add(i16::from(delta))
            .clamp(0, i16::from(PADS_PER_BANK.saturating_sub(1)));
        let index = u8::try_from(index).expect("clamped pattern pad index fits in u8");
        let pad = PadId::new(self.active_bank, index).expect("bounded pattern pad is valid");
        self.selected_pad = usize::from(index);
        self.patterns.move_cursor_to(pad, cursor.step());
    }

    fn change_pattern_slot(&mut self, delta: i8) {
        let current = i16::from(self.patterns.selected_slot().get());
        let requested = current.saturating_add(i16::from(delta)).clamp(0, 15);
        let slot = PatternSlotId::new(u8::try_from(requested).expect("bounded slot fits in u8"))
            .expect("bounded slot is valid");
        if slot == self.patterns.selected_slot() {
            self.status = if delta < 0 {
                "already at pattern 1".to_owned()
            } else {
                "already at pattern 16".to_owned()
            };
            return;
        }
        self.select_pattern(slot);
    }

    fn select_pattern_slot(&mut self, index: usize) {
        let Some(slot) = u8::try_from(index)
            .ok()
            .and_then(|index| PatternSlotId::new(index).ok())
        else {
            self.palette_error = Some("pattern must be 1..16".to_owned());
            return;
        };
        self.select_pattern(slot);
    }

    fn select_pattern(&mut self, slot: PatternSlotId) {
        if !self.patterns.is_slot_ready(slot) {
            self.patterns.select_slot(slot);
            self.report_pattern_not_ready(slot);
            return;
        }
        let disarm_capture = self
            .patterns
            .record_capture()
            .is_some_and(|(captured_slot, _)| captured_slot != slot);
        let switch = if self.pattern_transport_is_playing() {
            PatternSwitch::NextBoundary
        } else {
            PatternSwitch::Immediate
        };
        if let Some(audio) = self.audio.as_mut() {
            if disarm_capture && let Err(error) = audio.set_record_capture(None) {
                self.status = error;
                return;
            }
            if disarm_capture {
                self.patterns.stop_recording();
            }
            if let Err(error) = audio.select_pattern(slot, switch) {
                self.status = error;
                return;
            }
        } else if disarm_capture {
            self.report_audio_unavailable();
            return;
        }
        self.patterns.select_slot(slot);
        self.overlay = None;
    }

    fn start_pattern_playback(&mut self) {
        let slot = self.patterns.selected_slot();
        if !self.patterns.is_slot_ready(slot) {
            self.report_pattern_not_ready(slot);
            return;
        }
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        if let Err(error) = audio.select_pattern(slot, PatternSwitch::Immediate) {
            self.status = error;
            return;
        }
        if let Err(error) = audio.play_pattern() {
            self.status = error;
            return;
        }
        self.note_pattern_transport_intent(true);
        self.overlay = None;
    }

    fn stop_pattern_playback(&mut self) {
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        if let Err(error) = audio.set_record_capture(None) {
            self.status = error;
            return;
        }
        self.patterns.stop_recording();
        if let Err(error) = audio.stop_pattern() {
            self.status = error;
            return;
        }
        self.note_pattern_transport_intent(false);
        self.overlay = None;
    }

    fn toggle_pattern_recording(&mut self) {
        if self.patterns.capture_state().is_some() {
            self.stop_pattern_recording();
        } else {
            self.start_pattern_recording();
        }
    }

    fn start_pattern_recording(&mut self) {
        let slot = self.patterns.selected_slot();
        if !self.patterns.is_slot_ready(slot) {
            self.report_pattern_not_ready(slot);
            return;
        }
        let generation = self.patterns.selected_pattern().generation();
        let stamp = TransportStamp {
            slot,
            generation,
            origin: (self.telemetry.pattern_slot == Some(slot)
                && self.telemetry.pattern_generation == Some(generation))
            .then_some(self.telemetry.pattern_origin)
            .flatten()
            .unwrap_or(0),
            loop_frames: self.patterns.selected_pattern().transport().loop_frames(),
        };
        let start_transport = !self.pattern_transport_is_playing();
        {
            let Some(audio) = self.audio.as_mut() else {
                self.report_audio_unavailable();
                return;
            };
            if start_transport {
                if let Err(error) = audio.select_pattern(slot, PatternSwitch::Immediate) {
                    self.status = error;
                    return;
                }
                if let Err(error) = audio.play_pattern() {
                    self.status = error;
                    return;
                }
            }
        }
        if start_transport {
            self.note_pattern_transport_intent(true);
        }
        if let Err(error) = self.patterns.start_recording(stamp) {
            self.status = error.to_string();
            return;
        }
        let capture = self.patterns.record_capture();
        let result = self
            .audio
            .as_mut()
            .expect("audio remains present after transport admission")
            .set_record_capture(capture);
        if let Err(error) = result {
            self.patterns.stop_recording();
            self.status = error;
        }
    }

    fn stop_pattern_recording(&mut self) {
        self.patterns.stop_recording();
        if let Some(audio) = self.audio.as_mut()
            && let Err(error) = audio.set_record_capture(None)
        {
            self.status = error;
        }
    }

    fn open_clear_pattern(&mut self) {
        let slot = self.patterns.selected_slot();
        self.overlay = Some(Overlay::ClearPattern {
            slot,
            event_count: self.patterns.selected_pattern().events().len(),
        });
    }

    fn report_pattern_not_ready(&mut self, slot: PatternSlotId) {
        let status = self.patterns.last_status().filter(|status| match status {
            PatternStatus::UpdatePending { slot: status_slot }
            | PatternStatus::SnapshotBackpressured { slot: status_slot }
            | PatternStatus::SnapshotCompileFailed {
                slot: status_slot, ..
            }
            | PatternStatus::AudioCommandFailed {
                slot: status_slot, ..
            } => *status_slot == slot,
        });
        self.status = status.map_or_else(
            || pattern_status_text(&PatternStatus::UpdatePending { slot }),
            pattern_status_text,
        );
    }

    fn pattern_transport_is_playing(&self) -> bool {
        self.pending_pattern_transport
            .map(|intent| intent.playing)
            .unwrap_or(self.telemetry.pattern_playing)
    }

    fn note_pattern_transport_intent(&mut self, playing: bool) {
        self.pending_pattern_transport = Some(PendingPatternTransport { playing });
    }

    fn apply_pattern_edit(
        &mut self,
        edit: impl FnOnce(&mut PatternWorkspace) -> Result<(), sampler_core::PatternEditError>,
    ) {
        if let Err(error) = self.ensure_project_mutation_available() {
            self.status = error;
            return;
        }
        let generation = self.patterns.selected_pattern().generation();
        if let Err(error) = edit(&mut self.patterns) {
            self.status = error.to_string();
        } else {
            if self.patterns.selected_pattern().generation() != generation {
                self.commit_project_mutation();
            }
            self.overlay = None;
        }
    }

    pub fn selected_pad_id(&self) -> Option<PadId> {
        let index = u8::try_from(self.selected_pad).ok()?;
        PadId::new(self.active_bank, index).ok()
    }

    fn begin_selected_load(&mut self, path: PathBuf) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        if let Some(request) = self.begin_load(pad, path) {
            self.queue_worker_request(request);
        }
    }

    fn open_picker_parent(&mut self) {
        let directory = self
            .file_picker
            .pending_directory()
            .unwrap_or_else(|| self.file_picker.directory());
        let Some(parent) = directory.parent().map(ToOwned::to_owned) else {
            self.status = "already at filesystem root".to_owned();
            return;
        };
        self.open_picker_at(parent);
    }

    fn open_picker_selection(&mut self) {
        let Some(entry) = self.file_picker.selected().cloned() else {
            return;
        };
        if entry.is_directory() {
            self.open_picker_at(entry.path);
        } else if entry.is_selectable_file() {
            self.begin_selected_load(entry.path);
            self.overlay = None;
        } else {
            self.status = "entry is not a supported audio file".to_owned();
        }
    }

    fn queue_current_picker_scan(&mut self, request_id: u64) {
        let path = self
            .file_picker
            .pending_directory()
            .unwrap_or_else(|| self.file_picker.directory())
            .to_owned();
        self.queue_worker_request(WorkerRequest::ScanDirectory {
            request_id,
            path,
            show_hidden: self.file_picker.show_hidden(),
        });
    }

    fn release_pad(&mut self, index: usize) {
        if !self.validate_pad_index(index) {
            return;
        }
        let Some(pad) = self.held_pad_by_key[index] else {
            return;
        };
        if self.pads[pad_offset(pad)].settings.mode == PlaybackMode::OneShot
            && self.patterns.is_recording()
        {
            self.held_pad_by_key[index] = None;
            return;
        }
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let recording = self.patterns.is_recording();
        let result = if recording {
            audio.release_live_tracked(pad).map(Some)
        } else {
            audio.release_live(pad).map(|()| None)
        };
        match result {
            Ok(command) => {
                self.held_pad_by_key[index] = None;
                if let Some(command) = command {
                    self.patterns.note_live_release(index, command);
                }
            }
            Err(error) => self.status = error,
        }
    }

    fn stop_pad(&mut self, index: usize) {
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        let _ = self.select_pad(index);
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        match audio.stop_pad(pad) {
            Ok(()) if self.held_pad_by_key[index] == Some(pad) => {
                self.held_pad_by_key[index] = None;
            }
            Ok(()) => {}
            Err(error) => self.status = error,
        }
    }

    fn stop_all(&mut self) {
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        match audio.stop_all() {
            Ok(()) => {
                self.held_pad_by_key.fill(None);
                self.patterns.stop_recording();
                self.note_pattern_transport_intent(false);
            }
            Err(error) => self.status = error,
        }
    }

    fn change_bank(&mut self, delta: i8) {
        let current = i16::from(u8::from(self.active_bank));
        let requested = current + i16::from(delta);
        if requested < 0 {
            self.status = "already at first bank (A)".to_owned();
            return;
        }
        if requested >= i16::from(BANK_COUNT) {
            self.status = "already at last bank (J)".to_owned();
            return;
        }
        let next_bank = BankId::new(u8::try_from(requested).expect("bounded bank fits in u8"))
            .expect("bounded bank is valid");
        if self.editor.is_dirty() {
            self.status = "discard sample draft before changing bank".to_owned();
            return;
        }
        self.active_bank = next_bank;
        if self.patterns.view() == WorkspaceView::Pattern {
            let cursor = self.patterns.cursor();
            let pad = PadId::new(self.active_bank, cursor.pad().index())
                .expect("existing cursor index is valid");
            self.patterns.move_cursor_to(pad, cursor.step());
        }
        if self.patterns.view() == WorkspaceView::Sample {
            self.sync_editor_to_selected_pad();
        }
    }

    fn pad_in_active_bank(&mut self, index: usize) -> Option<PadId> {
        if !self.validate_pad_index(index) {
            return None;
        }
        let index = u8::try_from(index).expect("validated pad index fits in u8");
        Some(PadId::new(self.active_bank, index).expect("validated pad index is valid"))
    }

    fn validate_pad_index(&mut self, index: usize) -> bool {
        if index < usize::from(PADS_PER_BANK) {
            true
        } else {
            self.status = format!("pad {index} is outside 0..16");
            false
        }
    }

    fn report_audio_unavailable(&mut self) {
        self.status = self
            .audio_unavailable_message
            .clone()
            .unwrap_or_else(|| "audio device is unavailable".to_owned());
    }

    fn apply_telemetry(&mut self, telemetry: Telemetry) -> bool {
        let changed = self.telemetry != telemetry;
        if self
            .pending_pattern_transport
            .is_some_and(|intent| telemetry.pattern_playing == intent.playing)
        {
            self.pending_pattern_transport = None;
        }
        self.meter_left = self.meter_left.max(sanitize_peak(telemetry.peak_left));
        self.meter_right = self.meter_right.max(sanitize_peak(telemetry.peak_right));
        self.telemetry = telemetry;
        for bank in 0..BANK_COUNT {
            let bank = BankId::new(bank).expect("bounded bank is valid");
            for index in 0..PADS_PER_BANK {
                let pad = PadId::new(bank, index).expect("bounded pad is valid");
                self.pads[pad_offset(pad)].active = telemetry.is_pad_active(pad);
            }
        }
        changed
    }

    fn queue_worker_request(&mut self, request: WorkerRequest) -> bool {
        if self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY {
            self.pending_worker_requests.push(request);
            true
        } else {
            self.apply_worker_send_error(request, WorkerSendError::WorkerBusy);
            false
        }
    }

    fn fail_audio(&mut self, error: String) {
        self.audio = None;
        self.audio_format = None;
        self.recovery_cursor = None;
        self.reinstall_pending.fill(false);
        self.current_session_bound.fill(false);
        self.suspend_pending_sample_edits();
        self.audio_unavailable_message = Some(error.clone());
        self.held_pad_by_key.fill(None);
        self.patterns.stop_recording();
        self.pending_pattern_transport = None;
        for pad in &mut self.pads {
            pad.active = false;
        }
        self.status = error.clone();
        self.overlay = Some(Overlay::DeviceError(error));
        self.sync_editor_to_selected_pad();
    }

    fn recover_audio(&mut self, audio: Box<dyn AudioPort>) {
        let sample_rate = audio.sample_rate();
        let channels = audio.channels();
        let mut local_error = None;

        self.audio = Some(audio);
        self.audio_format = Some((sample_rate, channels));
        self.audio_unavailable_message = None;
        self.held_pad_by_key.fill(None);
        self.overlay = None;
        self.committed_recovery_loads.fill_with(|| None);
        self.reinstall_pending.fill(false);
        self.current_session_bound.fill(false);
        self.pending_pattern_transport = None;
        if let Some(ProjectOpenOperation::Staging(candidate)) = self.project_open.as_mut() {
            let staged_rate_matches = candidate
                .staged_pads
                .iter()
                .flatten()
                .all(|staged| staged.loaded.rendered.sample_rate() == sample_rate);
            if !staged_rate_matches {
                candidate.staged_pads.fill_with(|| None);
                candidate.next_decode = 0;
                candidate.decode_in_flight = None;
                candidate.progress.staged_pads = 0;
            }
            let mut patterns = PatternWorkspace::new(sample_rate);
            let rebuild = patterns
                .replace_project_patterns(candidate.document.patterns.clone())
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    patterns
                        .rebuild_sample_rate(sample_rate)
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = rebuild {
                self.status = error;
            } else {
                candidate.patterns = patterns;
            }
            candidate.admission = ProjectAdmission::StopAll;
            candidate.progress.admitted_actions = 0;
            self.overlay = Some(Overlay::ProjectOpenProgress);
            self.status = if staged_rate_matches {
                "Audio reconnected; restarting project admission".to_owned()
            } else {
                "Audio rate changed; restaging project audio".to_owned()
            };
            self.sync_editor_to_selected_pad();
            return;
        }
        if let Err(error) = self.patterns.rebuild_sample_rate(sample_rate) {
            self.status = error.to_string();
        }

        for bank in 0..BANK_COUNT {
            let bank = BankId::new(bank).expect("bounded bank is valid");
            for index in 0..PADS_PER_BANK {
                let pad = PadId::new(bank, index).expect("bounded pad is valid");
                let offset = pad_offset(pad);
                let view = &mut self.pads[offset];

                if view
                    .sample
                    .as_ref()
                    .is_some_and(|sample| sample.sample_rate() == sample_rate)
                {
                    self.reinstall_pending[offset] = true;
                } else if let Some(path) = view.source.clone() {
                    self.recovery_generations[offset] = self.recovery_generations[offset]
                        .max(view.generation)
                        .wrapping_add(1);
                    self.committed_recovery_loads[offset] = Some(Box::new(PendingLoad {
                        path,
                        phase: PendingLoadPhase::AwaitingWorker,
                        kind: PendingLoadKind::Recovery,
                    }));
                } else if self.pending_loads[offset].is_none()
                    && (view.sample.is_some() || view.state != PadLoadState::Empty)
                {
                    let error = format!(
                        "cannot reload pad for {sample_rate} Hz because its source path is unavailable"
                    );
                    view.state = PadLoadState::Error(error.clone());
                    local_error = Some(error);
                }

                if let Some(pending) = self.pending_loads[offset].as_mut()
                    && matches!(
                        &pending.phase,
                        PendingLoadPhase::Ready(loaded)
                            if loaded.rendered.sample_rate() != sample_rate
                    )
                {
                    pending.phase = PendingLoadPhase::AwaitingWorker;
                }

                let user_load_active = self.pending_loads[offset]
                    .as_ref()
                    .is_some_and(|pending| !matches!(pending.phase, PendingLoadPhase::Failed));
                if self.reinstall_pending[offset]
                    || self.committed_recovery_loads[offset].is_some()
                    || user_load_active
                {
                    view.state = PadLoadState::Loading;
                }
            }
        }

        self.recovery_cursor = Some(0);
        self.status = local_error.unwrap_or_else(|| "audio device connected".to_owned());
        self.pump_recovery_requests();
        self.sync_editor_to_selected_pad();
    }

    fn pump_recovery_requests(&mut self) -> bool {
        let Some(mut cursor) = self.recovery_cursor else {
            return false;
        };
        let Some(sample_rate) = self.audio.as_ref().map(|audio| audio.sample_rate()) else {
            self.recovery_cursor = None;
            return false;
        };
        let mut visited = 0;

        while visited < PAD_VIEW_COUNT {
            let offset = cursor;
            cursor = (cursor + 1) % PAD_VIEW_COUNT;
            visited += 1;

            if self.reinstall_pending[offset] {
                self.recovery_cursor = Some(cursor);
                self.reinstall_committed_sample(offset);
                return true;
            }

            if let Some(pending) = self.committed_recovery_loads[offset].as_mut() {
                if matches!(
                    &pending.phase,
                    PendingLoadPhase::Ready(loaded)
                        if loaded.rendered.sample_rate() != sample_rate
                ) {
                    pending.phase = PendingLoadPhase::AwaitingWorker;
                }

                match pending.phase {
                    PendingLoadPhase::AwaitingWorker => {
                        let request = WorkerRequest::LoadSample {
                            pad: pad_from_offset(offset),
                            generation: self.recovery_generations[offset],
                            purpose: LoadPurpose::Recovery,
                            path: pending.path.clone(),
                            engine_rate: sample_rate,
                            recipe: self.sample_editor.commits[offset].recipe,
                        };
                        pending.phase = PendingLoadPhase::WorkerQueued;
                        self.pads[offset].state = PadLoadState::Loading;
                        self.recovery_cursor = Some(cursor);
                        self.queue_worker_request(request);
                        return true;
                    }
                    PendingLoadPhase::Ready(_) => {
                        self.recovery_cursor = Some(cursor);
                        self.install_pending_load(offset, PendingLoadKind::Recovery);
                        return true;
                    }
                    PendingLoadPhase::WorkerQueued => continue,
                    PendingLoadPhase::Failed => {}
                }
            }

            if let Some(pending) = self.pending_loads[offset].as_mut() {
                if matches!(
                    &pending.phase,
                    PendingLoadPhase::Ready(loaded)
                        if loaded.rendered.sample_rate() != sample_rate
                ) {
                    pending.phase = PendingLoadPhase::AwaitingWorker;
                }

                match pending.phase {
                    PendingLoadPhase::AwaitingWorker => {
                        let request = WorkerRequest::LoadSample {
                            pad: pad_from_offset(offset),
                            generation: self.pads[offset].generation,
                            purpose: LoadPurpose::User,
                            path: pending.path.clone(),
                            engine_rate: sample_rate,
                            recipe: SampleEditRecipe::identity(),
                        };
                        pending.phase = PendingLoadPhase::WorkerQueued;
                        self.pads[offset].state = PadLoadState::Loading;
                        self.recovery_cursor = Some(cursor);
                        self.queue_worker_request(request);
                        return true;
                    }
                    PendingLoadPhase::Ready(_) => {
                        self.recovery_cursor = Some(cursor);
                        self.install_pending_load(offset, PendingLoadKind::User);
                        return true;
                    }
                    PendingLoadPhase::WorkerQueued | PendingLoadPhase::Failed => continue,
                }
            }
        }

        let still_recovering = self.recovery_action_pending();
        self.recovery_cursor = still_recovering.then_some(cursor);
        false
    }

    fn pending_load_slot(&self, offset: usize, kind: PendingLoadKind) -> &Option<Box<PendingLoad>> {
        match kind {
            PendingLoadKind::User => &self.pending_loads[offset],
            PendingLoadKind::Recovery => &self.committed_recovery_loads[offset],
        }
    }

    fn pending_load_slot_mut(
        &mut self,
        offset: usize,
        kind: PendingLoadKind,
    ) -> &mut Option<Box<PendingLoad>> {
        match kind {
            PendingLoadKind::User => &mut self.pending_loads[offset],
            PendingLoadKind::Recovery => &mut self.committed_recovery_loads[offset],
        }
    }

    fn matching_pending_load(
        &self,
        offset: usize,
        generation: u64,
        purpose: LoadPurpose,
        path: &Path,
    ) -> Option<PendingLoadKind> {
        let kind = match purpose {
            LoadPurpose::User => PendingLoadKind::User,
            LoadPurpose::Recovery => PendingLoadKind::Recovery,
        };
        let expected_generation = match kind {
            PendingLoadKind::User => self.pads[offset].generation,
            PendingLoadKind::Recovery => self.recovery_generations[offset],
        };
        (expected_generation == generation
            && self
                .pending_load_slot(offset, kind)
                .as_ref()
                .is_some_and(|pending| {
                    pending.kind.purpose() == purpose
                        && pending.path == path
                        && matches!(pending.phase, PendingLoadPhase::WorkerQueued)
                }))
        .then_some(kind)
    }

    fn install_pending_load(&mut self, offset: usize, kind: PendingLoadKind) {
        let Some(mut pending) = self.pending_load_slot_mut(offset, kind).take() else {
            return;
        };
        let PendingLoadPhase::Ready(loaded) = pending.phase else {
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            return;
        };
        let Some(audio_sample_rate) = self.audio.as_ref().map(|audio| audio.sample_rate()) else {
            pending.phase = PendingLoadPhase::Ready(loaded);
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            return;
        };
        if loaded.rendered.sample_rate() != audio_sample_rate {
            pending.phase = PendingLoadPhase::AwaitingWorker;
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::Loading;
            self.recovery_cursor = Some(offset);
            return;
        }

        if kind == PendingLoadKind::User
            && let Err(error) = self.ensure_project_mutation_available()
        {
            pending.phase = PendingLoadPhase::Ready(loaded);
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::Error(error.clone());
            self.status = error;
            self.refresh_editor_for_offset(offset);
            return;
        }

        let pad = pad_from_offset(offset);
        let settings = self.pads[offset].settings;
        let audio = self.audio.as_mut().expect("audio availability was checked");
        let install_result = match pending.kind {
            PendingLoadKind::User => audio.install(pad, Arc::clone(&loaded.rendered), settings),
            PendingLoadKind::Recovery => {
                audio.install_recovery(pad, Arc::clone(&loaded.rendered), settings)
            }
        };
        if let Err(error) = install_result {
            pending.phase = PendingLoadPhase::Ready(loaded);
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::Error(error.clone());
            self.status = error;
            self.recovery_cursor.get_or_insert(offset);
            self.refresh_editor_for_offset(offset);
            return;
        }

        let label = pending
            .path
            .file_name()
            .unwrap_or(pending.path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let view = &mut self.pads[offset];
        view.label = label.clone();
        view.source = Some(pending.path);
        if kind == PendingLoadKind::User {
            self.sample_editor.commits[offset].source_generation = view.generation;
            self.sample_editor.commits[offset].fingerprint = Some(loaded.fingerprint);
        }
        self.sample_editor.commits[offset].base = Some(loaded.base);
        self.sample_editor.commits[offset].recipe = loaded.recipe;
        view.sample = Some(loaded.rendered);
        self.sample_editor.commits[offset].base_preview = Some(loaded.base_preview);
        self.sample_editor.commits[offset].rendered_preview =
            Some(Arc::clone(&loaded.rendered_preview));
        view.preview = crate::loader::downsample_preview(&loaded.rendered_preview);
        view.state = PadLoadState::Ready;
        self.reinstall_pending[offset] = false;
        self.current_session_bound[offset] = true;
        if kind == PendingLoadKind::User {
            self.committed_recovery_loads[offset] = None;
            self.sample_editor.undo[offset] = None;
        }
        let action = if kind == PendingLoadKind::Recovery {
            "Recovered"
        } else {
            "Loaded"
        };
        self.status = format!("{action} {}", label.to_uppercase());
        self.refresh_editor_for_offset(offset);
        if kind == PendingLoadKind::User {
            self.commit_project_mutation();
        }
    }

    fn reinstall_committed_sample(&mut self, offset: usize) {
        let pad = pad_from_offset(offset);
        let Some(sample) = self.pads[offset].sample.as_ref().cloned() else {
            self.reinstall_pending[offset] = false;
            return;
        };
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        if sample.sample_rate() != audio.sample_rate() {
            self.reinstall_pending[offset] = false;
            if let Some(path) = self.pads[offset].source.clone() {
                self.recovery_generations[offset] = self.recovery_generations[offset]
                    .max(self.pads[offset].generation)
                    .wrapping_add(1);
                self.committed_recovery_loads[offset] = Some(Box::new(PendingLoad {
                    path,
                    phase: PendingLoadPhase::AwaitingWorker,
                    kind: PendingLoadKind::Recovery,
                }));
                self.pads[offset].state = PadLoadState::Loading;
                self.recovery_cursor = Some(offset);
            }
            return;
        }

        match audio.install_recovery(pad, sample, self.pads[offset].settings) {
            Ok(_) => {
                self.reinstall_pending[offset] = false;
                self.current_session_bound[offset] = true;
                self.pads[offset].state = PadLoadState::Ready;
            }
            Err(error) => {
                self.pads[offset].state = PadLoadState::Error(error.clone());
                self.status = error;
                self.recovery_cursor.get_or_insert(offset);
                self.refresh_editor_for_offset(offset);
            }
        }
    }

    fn recovery_action_pending(&self) -> bool {
        self.reinstall_pending
            .iter()
            .copied()
            .any(|pending| pending)
            || self
                .committed_recovery_loads
                .iter()
                .flatten()
                .any(|pending| {
                    matches!(
                        pending.phase,
                        PendingLoadPhase::AwaitingWorker | PendingLoadPhase::Ready(_)
                    )
                })
            || self.pending_loads.iter().flatten().any(|pending| {
                matches!(
                    pending.phase,
                    PendingLoadPhase::AwaitingWorker | PendingLoadPhase::Ready(_)
                )
            })
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // The stream/session is the only owner visible to the callback. It must be gone before
        // application-owned rendered, base, or undo buffers can perform their final Arc drop.
        drop(self.audio.take());
        for pad in &mut self.pads {
            pad.sample = None;
        }
        for commit in &mut self.sample_editor.commits {
            commit.base = None;
            commit.base_preview = None;
            commit.rendered_preview = None;
        }
        self.sample_editor.pending.fill_with(|| None);
        self.sample_editor.deferred_results.fill_with(|| None);
        self.sample_editor.undo.fill_with(|| None);
    }
}

fn resolve_picker_directory(current_dir: &Path, directory: PathBuf) -> PathBuf {
    let absolute = if directory.as_os_str().is_empty() {
        current_dir.to_owned()
    } else if directory.is_absolute() {
        directory
    } else {
        current_dir.join(directory)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn sanitize_peak(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn pattern_status_text(status: &PatternStatus) -> String {
    match status {
        PatternStatus::UpdatePending { slot } => {
            format!("pattern {} update pending", slot.get() + 1)
        }
        PatternStatus::SnapshotBackpressured { slot } => {
            format!("pattern {} update waiting for audio queue", slot.get() + 1)
        }
        PatternStatus::SnapshotCompileFailed { slot, error } => {
            format!("pattern {} compile failed: {error}", slot.get() + 1)
        }
        PatternStatus::AudioCommandFailed { slot, error } => {
            format!("pattern {} audio command failed: {error}", slot.get() + 1)
        }
    }
}

fn is_explicit_device_retry(key: KeyEvent) -> bool {
    let allowed = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    key.kind == KeyEventKind::Press
        && matches!(key.code, KeyCode::Char('r' | 'R'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.difference(allowed).is_empty()
}

fn pad_offset(pad: PadId) -> usize {
    usize::from(u8::from(pad.bank())) * usize::from(PADS_PER_BANK) + usize::from(pad.index())
}

fn pad_from_offset(offset: usize) -> PadId {
    let bank =
        u8::try_from(offset / usize::from(PADS_PER_BANK)).expect("bounded pad bank fits in u8");
    let index =
        u8::try_from(offset % usize::from(PADS_PER_BANK)).expect("bounded pad index fits in u8");
    PadId::new(BankId::new(bank).expect("bounded bank is valid"), index)
        .expect("bounded pad is valid")
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use sampler_audio::{
        AudioController, EnginePorts, Frame, LiveAck, LiveAckKind, LiveCommandId,
        PatternSnapshotSlot, PatternSwitch, SampleBuffer, SampleSlot, Telemetry, TransportStamp,
        audio_channels, audio_channels_with_test_capacities,
    };
    use sampler_core::{
        BankId, PadId, PadSettings, PatternSlotId, PatternSnapshot, PlaybackMode, SampleEditRecipe,
    };

    use crate::audio::AudioPort;
    use crate::input::InputAction;

    use crate::DirectoryScan;
    use crate::loader::{
        LoadPurpose, LoadSampleError, LoadedSample, ProjectSaveWorkerRequest, RenderedSample,
        WorkerHandle, WorkerRequest, WorkerResult, WorkerSendError,
    };

    use super::{
        App, EDIT_PREVIEW_COLUMNS, PREVIEW_COLUMNS, PadLoadState, PreviewColumn, RecoveryCleanup,
        SampleEditStatus,
    };
    use crate::pattern::{PatternWorkspace, WorkspaceView};
    use crate::project_session::ProjectSnapshotError;
    use crate::project_store::{ProjectAssetMapping, ProjectStoreError, SaveKind, SaveReceipt};

    #[derive(Debug, Clone, PartialEq)]
    enum AudioCall {
        Install(PadId),
        RemoveSample(PadId),
        Trigger(PadId, Frame, f32),
        Release(PadId, Frame),
        StopPad(PadId),
        StopAll,
        TrackedTrigger(PadId),
        TrackedRelease(PadId),
        InstallPattern,
        SelectPattern(PatternSlotId, PatternSwitch),
        PlayPattern,
        StopPattern,
        SetRecordCapture(Option<(PatternSlotId, u64)>),
    }

    #[derive(Clone)]
    struct CallLog(Rc<RefCell<Vec<AudioCall>>>);

    impl CallLog {
        fn snapshot(&self) -> Vec<AudioCall> {
            self.0.borrow().clone()
        }

        fn clear(&self) {
            self.0.borrow_mut().clear();
        }
    }

    struct FakeAudio {
        sample_rate: u32,
        channels: u16,
        horizon: Frame,
        horizon_reads: Rc<Cell<usize>>,
        trigger_error: Option<String>,
        release_error: Option<String>,
        stop_pad_error: Option<String>,
        stop_all_error: Option<String>,
        stop_pattern_error: Option<String>,
        capture_error: Option<String>,
        install_error: Option<String>,
        update_error: Option<String>,
        calls: CallLog,
        maintenance: Rc<RefCell<Vec<&'static str>>>,
        runtime_error: Option<String>,
        shutdown: Option<Rc<RefCell<Vec<&'static str>>>>,
        pattern_controller: AudioController,
        _pattern_ports: EnginePorts,
        drain_pattern_queue_after_backpressure: bool,
        live_acks: VecDeque<LiveAck>,
    }

    impl FakeAudio {
        fn ready(sample_rate: u32, channels: u16) -> Self {
            let (pattern_controller, pattern_ports) = audio_channels();
            Self {
                sample_rate,
                channels,
                horizon: 0,
                horizon_reads: Rc::new(Cell::new(0)),
                trigger_error: None,
                release_error: None,
                stop_pad_error: None,
                stop_all_error: None,
                stop_pattern_error: None,
                capture_error: None,
                install_error: None,
                update_error: None,
                calls: CallLog(Rc::new(RefCell::new(Vec::new()))),
                maintenance: Rc::new(RefCell::new(Vec::new())),
                runtime_error: None,
                shutdown: None,
                pattern_controller,
                _pattern_ports: pattern_ports,
                drain_pattern_queue_after_backpressure: false,
                live_acks: VecDeque::new(),
            }
        }

        fn pattern_queue_full_once(sample_rate: u32, channels: u16) -> Self {
            let (mut pattern_controller, pattern_ports) =
                audio_channels_with_test_capacities(1, 256, 64);
            pattern_controller.play_pattern().unwrap();
            Self {
                sample_rate,
                channels,
                horizon: 0,
                horizon_reads: Rc::new(Cell::new(0)),
                trigger_error: None,
                release_error: None,
                stop_pad_error: None,
                stop_all_error: None,
                stop_pattern_error: None,
                capture_error: None,
                install_error: None,
                update_error: None,
                calls: CallLog(Rc::new(RefCell::new(Vec::new()))),
                maintenance: Rc::new(RefCell::new(Vec::new())),
                runtime_error: None,
                shutdown: None,
                pattern_controller,
                _pattern_ports: pattern_ports,
                drain_pattern_queue_after_backpressure: false,
                live_acks: VecDeque::new(),
            }
        }

        fn with_horizon(mut self, horizon: Frame) -> Self {
            self.horizon = horizon;
            self
        }

        fn failing_trigger(mut self, error: &str) -> Self {
            self.trigger_error = Some(error.to_owned());
            self
        }

        fn failing_release_once(mut self, error: &str) -> Self {
            self.release_error = Some(error.to_owned());
            self
        }

        fn failing_stop_pad_once(mut self, error: &str) -> Self {
            self.stop_pad_error = Some(error.to_owned());
            self
        }

        fn failing_stop_all_once(mut self, error: &str) -> Self {
            self.stop_all_error = Some(error.to_owned());
            self
        }

        fn failing_stop_pattern_once(mut self, error: &str) -> Self {
            self.stop_pattern_error = Some(error.to_owned());
            self
        }

        fn failing_capture_once(mut self, error: &str) -> Self {
            self.capture_error = Some(error.to_owned());
            self
        }

        fn call_log(&self) -> CallLog {
            self.calls.clone()
        }

        fn failing_install(mut self, error: &str) -> Self {
            self.install_error = Some(error.to_owned());
            self
        }

        fn failing_update_once(mut self, error: &str) -> Self {
            self.update_error = Some(error.to_owned());
            self
        }

        fn failing_runtime(mut self, error: &str) -> Self {
            self.runtime_error = Some(error.to_owned());
            self
        }

        fn with_shutdown_log(mut self, shutdown: Rc<RefCell<Vec<&'static str>>>) -> Self {
            self.shutdown = Some(shutdown);
            self
        }

        fn with_live_acks(mut self, acks: impl IntoIterator<Item = LiveAck>) -> Self {
            self.live_acks.extend(acks);
            self
        }
    }

    impl Drop for FakeAudio {
        fn drop(&mut self) {
            if let Some(shutdown) = &self.shutdown {
                shutdown.borrow_mut().push("drop-audio");
            }
        }
    }

    impl AudioPort for FakeAudio {
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn render_horizon(&self) -> Frame {
            self.horizon_reads
                .set(self.horizon_reads.get().saturating_add(1));
            self.horizon
        }

        fn install(
            &mut self,
            pad: PadId,
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
        ) -> Result<SampleSlot, String> {
            if let Some(error) = self.install_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::Install(pad));
            SampleSlot::new(0).map_err(|error| error.to_string())
        }

        fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
            if let Some(error) = &self.trigger_error {
                return Err(error.clone());
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::Trigger(pad, at, velocity));
            Ok(())
        }

        fn trigger_live(&mut self, pad: PadId, velocity: f32) -> Result<(), String> {
            if let Some(error) = &self.trigger_error {
                return Err(error.clone());
            }
            self.calls.0.borrow_mut().push(AudioCall::Trigger(
                pad,
                self.horizon.saturating_add(64),
                velocity,
            ));
            Ok(())
        }

        fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::Release(pad, at));
            Ok(())
        }

        fn release_live(&mut self, pad: PadId) -> Result<(), String> {
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::Release(pad, self.horizon.saturating_add(64)));
            Ok(())
        }

        fn trigger_live_tracked(
            &mut self,
            pad: PadId,
            _velocity: f32,
        ) -> Result<LiveCommandId, String> {
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::TrackedTrigger(pad));
            Ok(LiveCommandId::FIRST)
        }

        fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, String> {
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::TrackedRelease(pad));
            Ok(LiveCommandId::FIRST)
        }

        fn install_pattern(
            &mut self,
            snapshot: Arc<PatternSnapshot>,
        ) -> Result<PatternSnapshotSlot, String> {
            self.calls.0.borrow_mut().push(AudioCall::InstallPattern);
            let result = self
                .pattern_controller
                .install_pattern(snapshot)
                .map_err(|error| error.to_string());
            if result.is_err() {
                self.drain_pattern_queue_after_backpressure = true;
            }
            result
        }

        fn select_pattern(
            &mut self,
            slot: PatternSlotId,
            switch: PatternSwitch,
        ) -> Result<(), String> {
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::SelectPattern(slot, switch));
            Ok(())
        }

        fn play_pattern(&mut self) -> Result<(), String> {
            self.calls.0.borrow_mut().push(AudioCall::PlayPattern);
            Ok(())
        }

        fn stop_pattern(&mut self) -> Result<(), String> {
            if let Some(error) = self.stop_pattern_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopPattern);
            Ok(())
        }

        fn set_record_capture(
            &mut self,
            capture: Option<(PatternSlotId, u64)>,
        ) -> Result<(), String> {
            if let Some(error) = self.capture_error.take() {
                return Err(error);
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::SetRecordCapture(capture));
            Ok(())
        }

        fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
            let count = output.len().min(self.live_acks.len());
            for slot in output.iter_mut().take(count) {
                *slot = self
                    .live_acks
                    .pop_front()
                    .expect("bounded ack count was checked");
            }
            count
        }

        fn reclaim_retired_patterns(&mut self) -> usize {
            0
        }

        fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
            if let Some(error) = self.stop_pad_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopPad(pad));
            Ok(())
        }

        fn remove_sample(&mut self, pad: PadId) -> Result<(), String> {
            self.calls.0.borrow_mut().push(AudioCall::RemoveSample(pad));
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), String> {
            if let Some(shutdown) = &self.shutdown {
                shutdown.borrow_mut().push("stop-all");
            }
            if let Some(error) = self.stop_all_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopAll);
            Ok(())
        }

        fn update_pad(&mut self, _pad: PadId, _settings: PadSettings) -> Result<(), String> {
            if let Some(error) = self.update_error.take() {
                return Err(error);
            }
            Ok(())
        }

        fn reclaim_retired(&mut self) -> usize {
            self.maintenance.borrow_mut().push("reclaim");
            if self.drain_pattern_queue_after_backpressure {
                while self._pattern_ports.immediate_commands.pop().is_ok() {}
                while self._pattern_ports.commands.pop().is_ok() {}
                self.drain_pattern_queue_after_backpressure = false;
            }
            0
        }

        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            None
        }

        fn poll_runtime_error(&mut self) -> Option<String> {
            self.maintenance.borrow_mut().push("poll");
            self.runtime_error.take()
        }
    }

    #[test]
    fn audio_maintenance_reclaims_before_polling_runtime_errors() {
        let audio = FakeAudio::ready(48_000, 2);
        let maintenance = Rc::clone(&audio.maintenance);
        let mut app = App::with_audio(Box::new(audio));

        assert!(app.maintain_audio());
        assert_eq!(*maintenance.borrow(), ["reclaim", "poll"]);
    }

    #[test]
    fn an_audio_runtime_error_moves_the_app_to_device_failed_state() {
        let audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(audio));

        assert!(app.maintain_audio());

        assert_eq!(app.audio_format(), None);
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::DeviceError(
                "device disconnected".to_owned()
            ))
        );
        assert_eq!(app.status(), "device disconnected");
    }

    #[test]
    fn shutdown_stops_and_drops_audio_even_when_stop_all_fails() {
        let shutdown = Rc::new(RefCell::new(Vec::new()));
        let audio = FakeAudio::ready(48_000, 2)
            .failing_stop_all_once("stop-all queue is full")
            .with_shutdown_log(Rc::clone(&shutdown));
        let mut app = App::with_audio(Box::new(audio));

        assert_eq!(
            app.shutdown_audio(),
            Err("stop-all queue is full".to_owned())
        );

        assert_eq!(*shutdown.borrow(), ["stop-all", "drop-audio"]);
        assert_eq!(app.audio_format(), None);
    }

    #[test]
    fn runtime_failure_preserves_pads_and_retry_reinstalls_matching_rate() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let request = app.begin_load(pad(0, 0), path("kick.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "kick.wav"));

        app.maintain_audio();

        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::DeviceError(_))
        ));
        assert!(app.pad(pad(0, 0)).sample.is_some());

        let replacement = FakeAudio::ready(48_000, 2);
        let calls = replacement.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement)));

        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
        assert_eq!(
            calls.snapshot().last(),
            Some(&AudioCall::Install(pad(0, 0)))
        );
    }

    #[test]
    fn retry_at_a_new_rate_reloads_from_source_instead_of_reusing_pcm() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let request = app.begin_load(pad(0, 0), path("kick.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "kick.wav"));
        app.maintain_audio();

        let replacement = FakeAudio::ready(44_100, 2);
        let calls = replacement.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement)));

        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Loading);
        assert_eq!(calls.snapshot(), []);
        assert_eq!(
            app.take_worker_requests().last(),
            Some(&WorkerRequest::LoadSample {
                pad: pad(0, 0),
                generation: generation.wrapping_add(1),
                purpose: LoadPurpose::Recovery,
                path: "kick.wav".into(),
                engine_rate: 44_100,
                recipe: SampleEditRecipe::identity(),
            })
        );
    }

    #[test]
    fn later_matching_rate_retry_rejects_an_older_wrong_rate_result() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let request = app.begin_load(pad(0, 0), path("kick.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "kick.wav"));
        let retained = Arc::clone(app.pad(pad(0, 0)).sample.as_ref().unwrap());
        app.maintain_audio();

        let changed_rate = FakeAudio::ready(44_100, 2).failing_runtime("replacement disconnected");
        app.retry_default_device_with(|| Ok(Box::new(changed_rate)));
        let stale_request = app.take_worker_requests().pop().unwrap();
        let WorkerRequest::LoadSample {
            generation: stale_generation,
            purpose: stale_purpose,
            ..
        } = stale_request
        else {
            panic!("wrong request")
        };
        app.maintain_audio();

        let original_rate = FakeAudio::ready(48_000, 2);
        let calls = original_rate.call_log();
        app.retry_default_device_with(|| Ok(Box::new(original_rate)));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        assert!(
            !app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad(0, 0),
                stale_generation,
                stale_purpose,
                "kick.wav",
                44_100,
                1,
                SampleEditRecipe::identity(),
            ))
        );
        assert!(Arc::ptr_eq(
            app.pad(pad(0, 0)).sample.as_ref().unwrap(),
            &retained
        ));
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
    }

    #[test]
    fn recovery_progresses_fairly_after_busy_and_unreadable_pads() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        for index in 0..12 {
            let pad = pad(0, index);
            let source = format!("pad-{index}.wav");
            let request = app.begin_load(pad, &source).unwrap();
            let WorkerRequest::LoadSample { generation, .. } = request else {
                panic!("wrong request")
            };
            app.apply_worker_result(loaded(pad, generation, &source));
        }
        app.maintain_audio();

        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(44_100, 2))));

        let [first] = app.take_worker_requests().try_into().unwrap();
        assert!(matches!(
            first,
            WorkerRequest::LoadSample { pad: pad_id, .. } if pad_id == pad(0, 0)
        ));
        app.apply_worker_send_error(first, WorkerSendError::WorkerBusy);
        assert_eq!(app.status(), "loader busy");

        app.maintain_audio();
        let [second] = app.take_worker_requests().try_into().unwrap();
        assert!(matches!(
            second,
            WorkerRequest::LoadSample { pad: pad_id, .. } if pad_id == pad(0, 1)
        ));

        let mut requests = vec![second];
        let mut completed = Vec::new();
        while let Some(request) = requests.pop() {
            let WorkerRequest::LoadSample {
                pad: pad_id,
                generation,
                purpose,
                path,
                ..
            } = request
            else {
                panic!("wrong request")
            };

            if pad_id == pad(0, 1) {
                app.apply_worker_result(WorkerResult::Loaded {
                    pad: pad_id,
                    generation,
                    purpose,
                    path,
                    result: Err(LoadSampleError::Decode("unreadable early pad".to_owned())),
                });
            } else {
                completed.push(pad_id);
                app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                    pad_id,
                    generation,
                    purpose,
                    path.to_str().unwrap(),
                    44_100,
                    1,
                    SampleEditRecipe::identity(),
                ));
            }

            app.maintain_audio();
            requests.extend(app.take_worker_requests());
        }

        assert_eq!(completed.len(), 11);
        assert!(completed.contains(&pad(0, 0)));
        assert_eq!(app.pad(pad(0, 11)).state, PadLoadState::Ready);
        assert_eq!(
            app.pad(pad(0, 1)).state,
            PadLoadState::Error("unreadable early pad".to_owned())
        );
    }

    #[test]
    fn permanent_worker_error_named_loader_busy_is_not_retried_by_recovery() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let request = app.begin_load(pad(0, 0), "pad.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "pad.wav"));
        app.maintain_audio();
        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(44_100, 2))));

        let request = app.take_worker_requests().pop().unwrap();
        let WorkerRequest::LoadSample {
            pad: pad_id,
            generation,
            purpose,
            path,
            ..
        } = request
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad_id,
            generation,
            purpose,
            path,
            result: Err(LoadSampleError::Decode("loader busy".to_owned())),
        });

        app.maintain_audio();

        assert!(app.take_worker_requests().is_empty());
        assert_eq!(
            app.pad(pad(0, 0)).state,
            PadLoadState::Error("loader busy".to_owned())
        );
    }

    fn pad(bank: u8, index: u8) -> PadId {
        PadId::new(BankId::new(bank).unwrap(), index).unwrap()
    }

    fn path(value: &str) -> &std::path::Path {
        std::path::Path::new(value)
    }

    fn loaded(pad: PadId, generation: u64, source: &str) -> WorkerResult {
        loaded_with_frames(pad, generation, source, 48_000, 1)
    }

    fn loaded_with_frames(
        pad: PadId,
        generation: u64,
        source: &str,
        sample_rate: u32,
        frames: usize,
    ) -> WorkerResult {
        loaded_with_recipe_and_frames(
            pad,
            generation,
            source,
            sample_rate,
            frames,
            SampleEditRecipe::identity(),
        )
    }

    fn loaded_with_recipe_and_frames(
        pad: PadId,
        generation: u64,
        source: &str,
        sample_rate: u32,
        frames: usize,
        recipe: SampleEditRecipe,
    ) -> WorkerResult {
        loaded_with_purpose_recipe_and_frames(
            pad,
            generation,
            LoadPurpose::User,
            source,
            sample_rate,
            frames,
            recipe,
        )
    }

    fn loaded_with_purpose_recipe_and_frames(
        pad: PadId,
        generation: u64,
        purpose: LoadPurpose,
        source: &str,
        sample_rate: u32,
        frames: usize,
        recipe: SampleEditRecipe,
    ) -> WorkerResult {
        let rendered =
            Arc::new(SampleBuffer::new(sample_rate, [0.25, -0.25].repeat(frames)).unwrap());
        WorkerResult::Loaded {
            pad,
            generation,
            purpose,
            path: source.into(),
            result: Ok(LoadedSample {
                fingerprint: crate::SourceFingerprint::from_encoded_bytes(
                    std::path::Path::new("fixture.wav"),
                    &[],
                )
                .unwrap(),
                base: Arc::clone(&rendered),
                base_preview: Arc::new([PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]),
                rendered,
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS],
                ),
                recipe,
                source_rate: sample_rate,
                source_frames: frames,
                duration: std::time::Duration::from_secs_f64(
                    frames as f64 / f64::from(sample_rate),
                ),
            }),
        }
    }

    fn changed_rate_recovery_colliding_with_same_path_user_load() -> (
        App,
        PadId,
        SampleEditRecipe,
        u64,
        WorkerRequest,
        WorkerRequest,
    ) {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "same.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "same.wav")));

        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                generation: edit_generation,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected edit request");
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            *edit_generation,
            recipe,
            48_000,
            vec![-0.4, 0.4],
        )));
        assert!(app.maintain_audio());
        assert_eq!(app.sample_editor.commits[0].recipe, recipe);
        let source_generation = app.sample_editor_context(pad).source_generation;

        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        assert!(app.maintain_audio());
        app.retry_with(Box::new(FakeAudio::ready(44_100, 2)));
        let [recovery] = app.take_worker_requests().try_into().unwrap();
        let user = app.begin_load(pad, "same.wav").unwrap();
        (app, pad, recipe, source_generation, recovery, user)
    }

    fn edited(
        app: &App,
        pad: PadId,
        generation: u64,
        recipe: SampleEditRecipe,
        sample_rate: u32,
        frames: Vec<f32>,
    ) -> WorkerResult {
        let offset = super::pad_offset(pad);
        let base_preview = app.sample_editor.pending[offset]
            .as_ref()
            .map(|pending| Arc::clone(&pending.base_preview))
            .or_else(|| {
                app.sample_editor.commits[offset]
                    .base_preview
                    .as_ref()
                    .map(Arc::clone)
            })
            .expect("an edit result must carry its request's base preview");
        WorkerResult::Edited {
            pad,
            generation,
            recipe,
            result: Ok(RenderedSample {
                base_preview,
                rendered: Arc::new(SampleBuffer::new(sample_rate, frames).unwrap()),
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -5, max: 5 }; EDIT_PREVIEW_COLUMNS],
                ),
            }),
        }
    }

    #[test]
    fn edit_commits_base_rendered_recipe_and_preview_only_after_audio_admission() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let old_rendered = Arc::clone(app.pad(pad).sample.as_ref().unwrap());
        let old_base = Arc::clone(app.base_sample(pad).unwrap());
        let old_source = app.pad(pad).source.clone();
        let old_label = app.pad(pad).label.clone();
        calls.clear();

        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                generation,
                base,
                recipe: sent_recipe,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected one edit request")
        };
        assert!(Arc::ptr_eq(base, &old_base));
        assert_eq!(*sent_recipe, recipe);
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            *generation,
            recipe,
            48_000,
            vec![-0.4, 0.4],
        )));

        assert!(Arc::ptr_eq(
            app.pad(pad).sample.as_ref().unwrap(),
            &old_rendered
        ));
        assert_eq!(
            app.committed_sample_recipe(pad),
            Some(SampleEditRecipe::identity())
        );
        assert_eq!(calls.snapshot(), []);
        assert!(app.maintain_audio());

        assert_eq!(app.committed_sample_recipe(pad), Some(recipe));
        assert_eq!(app.pad(pad).sample.as_ref().unwrap().data(), &[-0.4, 0.4]);
        assert!(Arc::ptr_eq(app.base_sample(pad).unwrap(), &old_base));
        assert_eq!(app.pad(pad).source, old_source);
        assert_eq!(app.pad(pad).label, old_label);
        assert_eq!(
            app.edit_preview(pad).unwrap().as_ref(),
            &[PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]
        );
        assert_eq!(
            app.pad(pad).preview,
            [PreviewColumn { min: -5, max: 5 }; PREVIEW_COLUMNS]
        );
        assert_eq!(
            calls
                .snapshot()
                .into_iter()
                .filter(|call| matches!(call, AudioCall::Install(_)))
                .collect::<Vec<_>>(),
            [AudioCall::Install(pad)]
        );
    }

    #[test]
    fn identity_edit_reuses_the_base_buffer_and_preview_owners() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let base = Arc::clone(app.base_sample(pad).unwrap());
        let base_preview = Arc::clone(app.edit_preview(pad).unwrap());

        app.request_sample_edit(pad, SampleEditRecipe::identity())
            .unwrap();
        let request = app.take_worker_requests().pop().unwrap();
        let mut worker = WorkerHandle::spawn();
        worker.try_send(request).unwrap();
        let result = worker.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(app.apply_worker_result(result));
        assert!(app.maintain_audio());

        assert!(Arc::ptr_eq(app.base_sample(pad).unwrap(), &base));
        assert!(Arc::ptr_eq(app.pad(pad).sample.as_ref().unwrap(), &base));
        assert!(Arc::ptr_eq(app.edit_preview(pad).unwrap(), &base_preview));
        assert!(Arc::ptr_eq(
            app.sample_editor.commits[0]
                .rendered_preview
                .as_ref()
                .unwrap(),
            &base_preview
        ));
        worker.shutdown().unwrap();
    }

    #[test]
    fn stale_worker_and_install_failure_keep_the_previous_tuple_exactly() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let old_rendered = Arc::clone(app.pad(pad).sample.as_ref().unwrap());
        let old_base = Arc::clone(app.base_sample(pad).unwrap());
        let old_recipe = app.committed_sample_recipe(pad).unwrap();
        let old_preview = app.pad(pad).preview;

        let first = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, first).unwrap();
        let first_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        let second = SampleEditRecipe {
            normalize: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, second).unwrap();
        let second_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(!app.apply_worker_result(edited(
            &app,
            pad,
            first_generation,
            first,
            48_000,
            vec![0.2, 0.2]
        )));
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            second_generation,
            second,
            48_000,
            vec![0.3, 0.3]
        )));

        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_install("install full"),
        ));
        assert!(app.maintain_audio());

        assert!(Arc::ptr_eq(
            app.pad(pad).sample.as_ref().unwrap(),
            &old_rendered
        ));
        assert!(Arc::ptr_eq(app.base_sample(pad).unwrap(), &old_base));
        assert_eq!(app.committed_sample_recipe(pad), Some(old_recipe));
        assert_eq!(app.pad(pad).preview, old_preview);
        assert_eq!(
            app.pad(pad).state,
            PadLoadState::Error("install full".to_owned())
        );
        assert!(app.current_session_bound[0]);
    }

    #[test]
    fn device_retry_redecodes_base_and_reapplies_committed_phase_recipe() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe = SampleEditRecipe {
            start_phase: 1,
            end_phase: sampler_core::SAMPLE_PHASE_SCALE,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let edit_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            edit_generation,
            recipe,
            48_000,
            vec![0.1, 0.1]
        )));
        assert!(app.maintain_audio());
        assert_eq!(app.committed_sample_recipe(pad), Some(recipe));
        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        assert!(app.maintain_audio());

        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(44_100, 2))));
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::LoadSample {
                engine_rate,
                recipe: recovered_recipe,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected recovery load")
        };
        assert_eq!(*engine_rate, 44_100);
        assert_eq!(*recovered_recipe, recipe);
    }

    #[test]
    fn undo_reinstalls_the_checkpoint_through_the_worker_and_audio_paths() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let base_preview = Arc::clone(app.edit_preview(pad).unwrap());
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            generation,
            recipe,
            48_000,
            vec![-0.4, 0.4],
        )));
        assert!(app.maintain_audio());
        calls.clear();

        app.undo_sample_edit(pad).unwrap();
        let WorkerRequest::EditSample {
            generation,
            recipe: undo_recipe,
            ..
        } = app.take_worker_requests().pop().unwrap()
        else {
            panic!("wrong request")
        };
        assert_eq!(undo_recipe, SampleEditRecipe::identity());
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            generation,
            undo_recipe,
            48_000,
            vec![0.25, -0.25],
        )));
        assert!(app.maintain_audio());
        assert!(Arc::ptr_eq(app.edit_preview(pad).unwrap(), &base_preview));

        assert_eq!(
            app.committed_sample_recipe(pad),
            Some(SampleEditRecipe::identity())
        );
        assert_eq!(app.sample_edit_status(pad), SampleEditStatus::Idle);
        assert_eq!(
            calls
                .snapshot()
                .into_iter()
                .filter(|call| matches!(call, AudioCall::Install(_)))
                .collect::<Vec<_>>(),
            [AudioCall::Install(pad)]
        );
    }

    #[test]
    fn busy_edit_send_retains_the_candidate_for_one_later_retry() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe = SampleEditRecipe {
            normalize: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let request = app.take_worker_requests().pop().unwrap();
        assert!(app.apply_worker_send_error(request, WorkerSendError::WorkerBusy));
        assert_eq!(
            app.sample_edit_status(pad),
            SampleEditStatus::AwaitingWorker
        );
        assert_eq!(app.status(), "loader busy");

        assert!(app.maintain_audio());
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                recipe: retried_recipe,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected retry")
        };
        assert_eq!(*retried_recipe, recipe);
    }

    #[test]
    fn device_recovery_never_auto_applies_a_confirmed_edit_that_was_interrupted() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let old_request = app.take_worker_requests().pop().unwrap();
        let WorkerRequest::EditSample {
            generation: old_generation,
            ..
        } = old_request
        else {
            panic!("wrong request")
        };

        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        assert!(app.maintain_audio());
        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(48_000, 2))));

        assert_eq!(app.sample_edit_status(pad), SampleEditStatus::Failed);
        assert!(app.take_worker_requests().is_empty());
        assert!(!app.apply_worker_result(edited(
            &app,
            pad,
            old_generation,
            recipe,
            48_000,
            vec![-0.4, 0.4]
        )));
        assert_eq!(
            app.committed_sample_recipe(pad),
            Some(SampleEditRecipe::identity())
        );
    }

    #[test]
    fn edit_generation_exhaustion_never_reuses_zero_or_replaces_the_live_request() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.sample_editor.generations[0] = u64::MAX - 1;
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let WorkerRequest::EditSample {
            generation: max_generation,
            ..
        } = app.take_worker_requests().pop().unwrap()
        else {
            panic!("wrong request")
        };
        assert_eq!(max_generation, u64::MAX);

        assert_eq!(
            app.request_sample_edit(pad, SampleEditRecipe::identity()),
            Err(super::SampleEditRequestError::GenerationExhausted)
        );
        assert_eq!(app.sample_editor.generations[0], u64::MAX);
        assert_eq!(
            app.sample_editor.pending[0]
                .as_ref()
                .map(|pending| pending.generation),
            Some(u64::MAX)
        );
        assert_eq!(
            app.sample_edit_status(pad),
            SampleEditStatus::GenerationExhausted
        );
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn deferred_edit_results_accept_only_the_exact_current_generation() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.sample_editor.generations[0] = 0;
        let recipe = SampleEditRecipe {
            normalize: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let current_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        app.edit_result_advanced = true;

        assert!(!app.apply_worker_result(edited(
            &app,
            pad,
            u64::MAX,
            recipe,
            48_000,
            vec![0.9, 0.9],
        )));
        assert!(app.sample_editor.deferred_results[0].is_none());
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            current_generation,
            recipe,
            48_000,
            vec![0.4, 0.4]
        )));
        assert!(app.sample_editor.deferred_results[0].is_some());

        assert!(app.maintain_audio());
        assert_eq!(app.sample_edit_status(pad), SampleEditStatus::UndoAvailable);
        assert_eq!(app.pad(pad).sample.as_ref().unwrap().data(), &[0.4, 0.4]);
    }

    #[test]
    fn newer_edit_discards_deferred_prior_result_without_spending_next_maintenance_budget() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe_a = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe_a).unwrap();
        let generation_a = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        app.edit_result_advanced = true;
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            generation_a,
            recipe_a,
            48_000,
            vec![-0.4, 0.4]
        )));
        assert!(app.sample_editor.deferred_results[0].is_some());

        let recipe_b = SampleEditRecipe {
            normalize: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe_b).unwrap();
        let generation_b = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(app.sample_editor.deferred_results[0].is_none());

        // Model an already-queued stale result arriving just before maintenance. It must be
        // discarded before the one-result budget is marked consumed.
        let stale_result = edited(&app, pad, generation_a, recipe_a, 48_000, vec![-0.4, 0.4]);
        app.sample_editor.deferred_results[0] = Some(Box::new(stale_result));
        assert!(app.maintain_audio());
        assert!(!app.edit_result_advanced);
        assert!(app.sample_editor.deferred_results[0].is_none());

        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            generation_b,
            recipe_b,
            48_000,
            vec![0.4, 0.4]
        )));
        assert!(matches!(
            app.sample_editor.pending[0]
                .as_ref()
                .map(|pending| &pending.phase),
            Some(super::PendingEditPhase::Ready(_))
        ));
        assert!(app.sample_editor.deferred_results[0].is_none());
    }

    #[test]
    fn exhausted_undo_generation_preserves_the_checkpoint_and_current_tuple() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let edit_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            edit_generation,
            recipe,
            48_000,
            vec![-0.4, 0.4]
        )));
        assert!(app.maintain_audio());
        let current = Arc::clone(app.pad(pad).sample.as_ref().unwrap());
        app.sample_editor.generations[0] = u64::MAX;

        assert_eq!(
            app.undo_sample_edit(pad),
            Err(super::SampleEditRequestError::GenerationExhausted)
        );
        assert!(Arc::ptr_eq(app.pad(pad).sample.as_ref().unwrap(), &current));
        assert_eq!(app.committed_sample_recipe(pad), Some(recipe));
        assert!(app.sample_editor.undo[0].is_some());
        assert!(app.sample_editor.pending[0].is_none());
        assert_eq!(
            app.sample_edit_status(pad),
            SampleEditStatus::GenerationExhausted
        );
    }

    #[test]
    fn app_discards_a_superseded_load_generation() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.begin_load(pad(0, 0), path("old.wav"));
        let old_generation = app.pad(pad(0, 0)).generation;
        app.begin_load(pad(0, 0), path("new.wav"));

        app.apply_worker_result(loaded(pad(0, 0), old_generation, "old.wav"));

        assert_eq!(app.pad(pad(0, 0)).source, None);
        assert_eq!(
            app.pending_loads[0]
                .as_ref()
                .map(|pending| pending.path.as_path()),
            Some(path("new.wav"))
        );
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Loading);
    }

    #[test]
    fn failed_replacement_keeps_committed_source_and_sample_paired() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let first = app.begin_load(pad(0, 0), path("old.wav")).unwrap();
        let WorkerRequest::LoadSample {
            generation: first_generation,
            ..
        } = first
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), first_generation, "old.wav"));
        let committed = Arc::clone(app.pad(pad(0, 0)).sample.as_ref().unwrap());

        let replacement = app.begin_load(pad(0, 0), path("new.wav")).unwrap();
        let WorkerRequest::LoadSample {
            generation: replacement_generation,
            path: result_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 0),
            generation: replacement_generation,
            purpose: LoadPurpose::User,
            path: result_path,
            result: Err(LoadSampleError::Decode(
                "replacement decode failed".to_owned(),
            )),
        });

        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("old.wav")));
        assert!(Arc::ptr_eq(
            app.pad(pad(0, 0)).sample.as_ref().unwrap(),
            &committed
        ));
    }

    #[test]
    fn device_retry_after_a_failed_replacement_reinstalls_the_committed_sample() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let first = app.begin_load(pad(0, 0), "old.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = first else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "old.wav"));

        let replacement = app.begin_load(pad(0, 0), "new.wav").unwrap();
        let WorkerRequest::LoadSample {
            generation,
            path: result_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 0),
            generation,
            purpose: LoadPurpose::User,
            path: result_path,
            result: Err(LoadSampleError::Decode("replacement failed".to_owned())),
        });
        app.maintain_audio();

        let replacement_audio = FakeAudio::ready(48_000, 2);
        let calls = replacement_audio.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement_audio)));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("old.wav")));
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
    }

    #[test]
    fn same_rate_retry_recovers_committed_sample_while_replacement_remains_pending() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let first = app.begin_load(pad(0, 0), "old.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = first else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "old.wav"));
        let replacement = app.begin_load(pad(0, 0), "new.wav").unwrap();
        app.maintain_audio();

        let replacement_audio = FakeAudio::ready(48_000, 2);
        let calls = replacement_audio.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement_audio)));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        let WorkerRequest::LoadSample {
            generation,
            path: result_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 0),
            generation,
            purpose: LoadPurpose::User,
            path: result_path,
            result: Err(LoadSampleError::Decode("replacement failed".to_owned())),
        });

        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("old.wav")));
        assert_eq!(
            app.pad(pad(0, 0)).sample.as_ref().unwrap().sample_rate(),
            48_000
        );
    }

    #[test]
    fn same_rate_recovery_survives_replacement_started_before_maintenance() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        for (index, source) in [(0, "first.wav"), (1, "old.wav")] {
            let pad = pad(0, index);
            let request = app.begin_load(pad, source).unwrap();
            let WorkerRequest::LoadSample { generation, .. } = request else {
                panic!("wrong request")
            };
            assert!(app.apply_worker_result(loaded(pad, generation, source)));
        }
        assert!(app.maintain_audio());

        let replacement_audio = FakeAudio::ready(48_000, 2);
        let calls = replacement_audio.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement_audio)));
        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);

        let replacement = app.begin_load(pad(0, 1), "new.wav").unwrap();
        let WorkerRequest::LoadSample {
            generation,
            path: replacement_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 1),
            generation,
            purpose: LoadPurpose::User,
            path: replacement_path,
            result: Err(LoadSampleError::Decode("replacement failed".to_owned())),
        }));

        assert!(app.maintain_audio());

        assert_eq!(
            calls
                .snapshot()
                .into_iter()
                .filter(|call| matches!(call, AudioCall::Install(_)))
                .collect::<Vec<_>>(),
            [AudioCall::Install(pad(0, 0)), AudioCall::Install(pad(0, 1))]
        );
        assert_eq!(app.pad(pad(0, 1)).source.as_deref(), Some(path("old.wav")));
        assert_eq!(app.pad(pad(0, 1)).state, PadLoadState::Ready);
    }

    #[test]
    fn changed_rate_retry_recovers_committed_source_while_replacement_remains_pending() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let first = app.begin_load(pad(0, 0), "old.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = first else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "old.wav"));
        let replacement = app.begin_load(pad(0, 0), "new.wav").unwrap();
        app.maintain_audio();

        let replacement_audio = FakeAudio::ready(44_100, 2);
        let calls = replacement_audio.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement_audio)));
        let [recovery] = app.take_worker_requests().try_into().unwrap();
        let WorkerRequest::LoadSample {
            generation,
            purpose,
            path: recovery_path,
            ..
        } = recovery
        else {
            panic!("wrong request")
        };
        assert_eq!(recovery_path, path("old.wav"));
        app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
            pad(0, 0),
            generation,
            purpose,
            "old.wav",
            44_100,
            1,
            SampleEditRecipe::identity(),
        ));
        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);

        let WorkerRequest::LoadSample {
            generation,
            path: result_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 0),
            generation,
            purpose: LoadPurpose::User,
            path: result_path,
            result: Err(LoadSampleError::Decode("replacement failed".to_owned())),
        });

        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("old.wav")));
        assert_eq!(
            app.pad(pad(0, 0)).sample.as_ref().unwrap().sample_rate(),
            44_100
        );
    }

    #[test]
    fn matching_load_is_installed_before_replacing_the_pad_sample() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        let request = app.begin_load(pad(0, 0), path("new.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };

        app.apply_worker_result(loaded(pad(0, 0), generation, "new.wav"));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
        assert!(app.pad(pad(0, 0)).sample.is_some());
    }

    #[test]
    fn install_failure_preserves_the_prior_ready_sample() {
        let fake = FakeAudio::ready(48_000, 2).failing_install("install queue is full");
        let mut app = App::with_audio(Box::new(fake));
        let first = Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap());
        app.pads[0].sample = Some(Arc::clone(&first));
        app.pads[0].state = PadLoadState::Ready;
        let request = app.begin_load(pad(0, 0), path("new.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };

        app.apply_worker_result(loaded(pad(0, 0), generation, "new.wav"));

        assert!(Arc::ptr_eq(
            app.pad(pad(0, 0)).sample.as_ref().unwrap(),
            &first
        ));
        assert!(matches!(app.pad(pad(0, 0)).state, PadLoadState::Error(_)));
    }

    #[test]
    fn no_device_retains_the_path_without_creating_a_load_request() {
        let mut app = App::without_audio("no output device");

        let request = app.begin_load(pad(0, 0), path("kick.wav"));

        assert!(request.is_none());
        assert_eq!(app.pad(pad(0, 0)).source, None);
        assert_eq!(
            app.pending_loads[0]
                .as_ref()
                .map(|pending| pending.path.as_path()),
            Some(path("kick.wav"))
        );
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::WaitingForDevice);
    }

    #[test]
    fn no_device_pending_load_is_scheduled_after_retry() {
        let mut app = App::without_audio("no output device");
        app.begin_load(pad(0, 0), "kick.wav");
        let generation = app.pad(pad(0, 0)).generation;

        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(44_100, 2))));

        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::LoadSample {
                pad: pad(0, 0),
                generation,
                purpose: LoadPurpose::User,
                path: "kick.wav".into(),
                engine_rate: 44_100,
                recipe: SampleEditRecipe::identity(),
            }]
        );
        assert_eq!(app.pad(pad(0, 0)).source, None);
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Loading);
    }

    #[test]
    fn pad_press_uses_render_horizon_plus_sixty_four_frames() {
        let fake = FakeAudio::ready(48_000, 2).with_horizon(10_000);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(5));

        assert_eq!(
            calls.snapshot(),
            [AudioCall::Trigger(pad(0, 5), 10_064, 1.0)]
        );
    }

    #[test]
    fn pad_press_uses_the_causal_live_port_without_a_separate_horizon_read() {
        let fake = FakeAudio::ready(48_000, 2).with_horizon(10_000);
        let horizon_reads = Rc::clone(&fake.horizon_reads);
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(5));

        assert_eq!(horizon_reads.get(), 0);
    }

    #[test]
    fn fallback_one_shot_press_rearms_without_a_release_event() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.pads[0].settings =
            PadSettings::new(PlaybackMode::OneShot, 0.0, 0.0, 0.0, None).unwrap();

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadPress(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
        assert!(!app.is_pad_held(0));
    }

    #[test]
    fn bank_navigation_is_bounded_and_release_targets_the_original_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(app.active_bank(), BankId::new(1).unwrap());
        assert_eq!(
            calls.snapshot().last(),
            Some(&AudioCall::Release(pad(0, 0), 64))
        );
    }

    #[test]
    fn duplicate_press_does_not_retrigger_or_replace_the_held_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn failed_release_keeps_the_held_pad_for_retry() {
        let fake = FakeAudio::ready(48_000, 2).failing_release_once("release queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadRelease(0));
        assert!(app.status().contains("release queue is full"));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn recording_pad_lifecycle_tracks_gate_and_loop_releases_but_not_one_shot() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });
        let stamp = sampler_audio::TransportStamp {
            slot: PatternSlotId::new(0).unwrap(),
            generation: 0,
            origin: 0,
            loop_frames: 96_000,
        };

        for (index, mode, expect_release) in [
            (0, PlaybackMode::OneShot, false),
            (1, PlaybackMode::Gate, true),
            (2, PlaybackMode::Loop, true),
        ] {
            app.patterns.start_recording(stamp).unwrap();
            app.pads[index].settings = PadSettings::new(mode, 0.0, 0.0, 0.0, None).unwrap();
            app.apply(InputAction::PadPress(index));
            app.apply(InputAction::PadRelease(index));
            assert!(!app.is_pad_held(index));
            assert_eq!(
                calls
                    .snapshot()
                    .iter()
                    .filter(|call| matches!(call, AudioCall::TrackedRelease(tracked) if *tracked == pad(0, index as u8)))
                    .count(),
                usize::from(expect_release),
                "{mode:?} release semantics",
            );
            calls.clear();
            app.patterns.stop_recording();
        }
    }

    #[test]
    fn failed_stop_pad_keeps_the_slot_held_until_stop_retry_succeeds() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_pad_once("stop queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadStop(0));
        assert!(app.status().contains("stop queue is full"));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadStop(0));
        app.apply(InputAction::PadPress(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopPad(pad(0, 0)),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
    }

    #[test]
    fn bank_switched_stop_does_not_forget_the_original_held_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadStop(0));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopPad(pad(1, 0)),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn failed_stop_all_keeps_slots_held_until_stop_retry_succeeds() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_all_once("stop-all queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::StopAll);
        assert!(app.status().contains("stop-all queue is full"));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::StopAll);
        app.apply(InputAction::PadPress(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopAll,
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
    }

    #[test]
    fn controller_overflow_is_visible_and_nonfatal() {
        let fake = FakeAudio::ready(48_000, 2).failing_trigger("audio command queue is full");
        let mut app = App::with_audio(Box::new(fake));
        app.apply(InputAction::PadPress(0));
        assert!(app.status().contains("queue is full"));
        assert!(!app.should_quit());
    }

    #[test]
    fn scheduling_saturates_at_the_frame_limit() {
        let fake = FakeAudio::ready(48_000, 2).with_horizon(Frame::MAX);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(15));

        assert_eq!(
            calls.snapshot(),
            [AudioCall::Trigger(pad(0, 15), Frame::MAX, 1.0)]
        );
    }

    #[test]
    fn bank_navigation_does_not_wrap_and_reports_both_edges() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.apply(InputAction::BankDelta(-1));
        assert_eq!(app.active_bank(), BankId::new(0).unwrap());
        assert!(app.status().contains("first bank"));

        app.apply(InputAction::BankDelta(9));
        assert_eq!(app.active_bank(), BankId::new(9).unwrap());
        app.apply(InputAction::BankDelta(1));
        assert_eq!(app.active_bank(), BankId::new(9).unwrap());
        assert!(app.status().contains("last bank"));
        assert!(!app.should_quit());
    }

    #[test]
    fn invalid_pad_positions_are_visible_and_nonfatal() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(16));
        app.apply(InputAction::PadRelease(usize::MAX));

        assert!(calls.snapshot().is_empty(), "{:?}", calls.snapshot());
        assert!(app.status().contains("outside 0..16"));
        assert!(!app.should_quit());
    }

    #[test]
    fn missing_audio_keeps_a_complete_browsable_model() {
        let mut app = App::without_audio("no output device");

        assert_eq!(app.active_bank(), BankId::new(0).unwrap());
        assert_eq!(app.pads().len(), super::PAD_VIEW_COUNT);
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::DeviceError("no output device".to_owned()))
        );
        app.apply(InputAction::PadPress(0));
        assert!(app.status().contains("no output device"));
        assert!(!app.should_quit());
    }

    fn key(character: char, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(character), modifiers, kind)
    }

    fn transport_telemetry(rendered_frame: Frame, playing: bool) -> Telemetry {
        Telemetry {
            active_pads: [0; 3],
            rendered_frame,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
            pattern_slot: Some(PatternSlotId::new(0).unwrap()),
            pattern_generation: Some(0),
            pattern_playing: playing,
            pattern_recording: false,
            pattern_origin: playing.then_some(100),
            pattern_playhead: 0,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        }
    }

    #[test]
    fn stale_stopped_telemetry_does_not_cancel_an_accepted_play_intent() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();
        app.apply_telemetry(transport_telemetry(100, false));

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_telemetry(transport_telemetry(101, false));
        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::SelectPattern(
                    PatternSlotId::new(1).unwrap(),
                    PatternSwitch::NextBoundary,
                ),
            ]
        );
    }

    #[test]
    fn stale_playing_telemetry_does_not_cancel_an_accepted_stop_intent() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();
        app.apply_telemetry(transport_telemetry(100, true));

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_telemetry(transport_telemetry(101, true));
        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SetRecordCapture(None),
                AudioCall::StopPattern,
                AudioCall::SelectPattern(PatternSlotId::new(1).unwrap(), PatternSwitch::Immediate),
            ]
        );
    }

    #[test]
    fn play_does_not_admit_transport_before_the_selected_snapshot_is_installed() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("pattern 1 update pending"));
    }

    #[test]
    fn play_waits_for_backpressured_install_then_admits_after_a_later_maintenance_success() {
        let fake = FakeAudio::pattern_queue_full_once(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.maintain_audio();
        calls.clear();
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("waiting for audio queue"));

        app.maintain_audio();
        calls.clear();
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
            ]
        );
    }

    #[test]
    fn editing_an_installed_pattern_invalidates_transport_readiness_until_reinstalled() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        calls.clear();

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("pattern 1 update pending"));
    }

    #[test]
    fn selecting_an_unready_slot_never_admits_a_pattern_switch() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        calls.clear();

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            app.patterns().selected_slot(),
            PatternSlotId::new(1).unwrap()
        );
        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("pattern 2 update pending"));
    }

    #[test]
    fn changing_slot_disarms_a_different_capture_before_selecting_and_next_pad_is_untracked() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        for _ in 0..32 {
            app.maintain_audio();
        }
        let captured = PatternSlotId::new(1).unwrap();
        app.patterns.select_slot(captured);
        app.patterns
            .start_recording(sampler_audio::TransportStamp {
                slot: captured,
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        calls.clear();

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply(InputAction::PadPress(0));

        assert!(!app.patterns().is_recording());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SetRecordCapture(None),
                AudioCall::SelectPattern(PatternSlotId::new(2).unwrap(), PatternSwitch::Immediate),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
    }

    #[test]
    fn selecting_the_current_pattern_cancels_a_pending_other_slot_capture() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        for _ in 0..32 {
            app.maintain_audio();
        }
        let captured = PatternSlotId::new(1).unwrap();
        app.patterns.select_slot(captured);
        app.patterns
            .start_recording(sampler_audio::TransportStamp {
                slot: captured,
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        calls.clear();

        app.apply_key(key(',', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            app.patterns().selected_slot(),
            PatternSlotId::new(0).unwrap()
        );
        assert!(!app.patterns().is_recording());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SetRecordCapture(None),
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
            ]
        );
    }

    #[test]
    fn capture_disarm_failure_aborts_slot_supersede_without_losing_recording() {
        let fake = FakeAudio::ready(48_000, 2).failing_capture_once("capture queue full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        for _ in 0..32 {
            app.maintain_audio();
        }
        let captured = PatternSlotId::new(1).unwrap();
        app.patterns.select_slot(captured);
        app.patterns
            .start_recording(sampler_audio::TransportStamp {
                slot: captured,
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        calls.clear();

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(app.patterns().selected_slot(), captured);
        assert!(app.patterns().is_recording());
        assert!(app.status().contains("capture queue full"));
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn palette_pattern_selection_uses_the_same_capture_disarm_reducer() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        for _ in 0..32 {
            app.maintain_audio();
        }
        let captured = PatternSlotId::new(1).unwrap();
        app.patterns.select_slot(captured);
        app.patterns
            .start_recording(sampler_audio::TransportStamp {
                slot: captured,
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        calls.clear();

        app.apply_key(key(':', KeyModifiers::SHIFT, KeyEventKind::Press));
        app.apply_terminal_event(Event::Paste("pattern 3".into()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!app.patterns().is_recording());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SetRecordCapture(None),
                AudioCall::SelectPattern(PatternSlotId::new(2).unwrap(), PatternSwitch::Immediate),
            ]
        );
    }

    #[test]
    fn control_r_arms_pattern_recording_while_plain_r_remains_a_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        calls.clear();

        app.apply_key(key('r', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key('r', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 7), 64, 1.0),
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate,),
                AudioCall::PlayPattern,
                AudioCall::SetRecordCapture(Some((PatternSlotId::new(0).unwrap(), 0))),
                AudioCall::TrackedTrigger(pad(0, 0)),
            ]
        );
    }

    #[test]
    fn accepted_play_intent_makes_same_batch_slot_change_wait_for_the_next_boundary() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::SelectPattern(
                    PatternSlotId::new(1).unwrap(),
                    PatternSwitch::NextBoundary,
                ),
            ]
        );
    }

    #[test]
    fn accepted_play_intent_makes_a_second_space_stop_before_telemetry_arrives() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::SetRecordCapture(None),
                AudioCall::StopPattern,
            ]
        );
    }

    #[test]
    fn stop_all_replaces_a_pending_play_intent_before_telemetry_arrives() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply(InputAction::StopAll);
        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::StopAll,
                AudioCall::SelectPattern(PatternSlotId::new(1).unwrap(), PatternSwitch::Immediate),
            ]
        );
    }

    #[test]
    fn capture_disarms_before_a_failed_transport_stop() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_pattern_once("stop failed");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        calls.clear();

        app.apply_key(key('r', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(!app.patterns().is_recording());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::SetRecordCapture(Some((PatternSlotId::new(0).unwrap(), 0))),
                AudioCall::SetRecordCapture(None),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
        assert_eq!(app.status(), "stop failed");
    }

    #[test]
    fn retry_at_a_new_rate_rebuilds_all_editable_pattern_slots() {
        let failed = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed));
        app.maintain_audio();

        app.retry_with(Box::new(FakeAudio::ready(44_100, 2)));

        assert_eq!(app.patterns().sample_rates(), [44_100; 16]);
    }

    #[test]
    fn device_modal_retry_wins_over_the_r_pad_key() {
        let mut app = App::without_audio("no output device");

        app.apply_key(key('r', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(app.device_retry_requests(), 1);
        assert_eq!(app.selected_pad(), 0);
    }

    #[test]
    fn dismissed_startup_and_runtime_failures_keep_an_explicit_retry_route() {
        let startup_failure = App::without_audio("no output device");
        let runtime_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut runtime_failure = App::with_audio(Box::new(runtime_audio));
        runtime_failure.maintain_audio();

        for mut app in [startup_failure, runtime_failure] {
            app.apply_key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Press,
            ));
            assert!(app.status().contains("Ctrl+R"));

            app.apply_key(key('r', KeyModifiers::NONE, KeyEventKind::Press));
            assert_eq!(app.selected_pad(), 7);
            assert_eq!(app.device_retry_requests(), 0);

            app.open_help();
            app.apply_key(key('r', KeyModifiers::CONTROL, KeyEventKind::Press));
            assert_eq!(app.device_retry_requests(), 1);
        }
    }

    #[test]
    fn control_q_quits_even_when_a_modal_is_open() {
        let mut app = App::without_audio("no output device");

        app.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));

        assert!(app.should_quit());
    }

    #[test]
    fn pasted_text_only_changes_the_open_palette() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply_terminal_event(Event::Paste("stop-all".into()));
        assert!(calls.snapshot().is_empty());
        app.open_palette();
        app.apply_terminal_event(Event::Paste("stop-all".into()));

        assert_eq!(app.palette_text(), "stop-all");
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn pad_presses_remain_global_over_help_and_picker_overlays() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.open_help();
        app.apply_key(key('q', KeyModifiers::NONE, KeyEventKind::Press));
        app.close_overlay();
        app.open_picker();
        app.apply_key(key('q', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 4), 64, 1.0),
                AudioCall::Trigger(pad(0, 4), 64, 1.0),
            ]
        );
    }

    #[test]
    fn modal_overlay_does_not_swallow_a_held_pad_release() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });
        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Press));
        app.open_help();

        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Release));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
        assert!(!app.is_pad_held(0));
        assert_eq!(app.overlay(), Some(&super::Overlay::Help));
    }

    #[test]
    fn modal_overlay_does_not_swallow_shift_escape_stop_all() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.apply(InputAction::PadPress(0));
        app.open_help();

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ));

        assert_eq!(
            calls.snapshot(),
            [AudioCall::Trigger(pad(0, 0), 64, 1.0), AudioCall::StopAll]
        );
        assert!(!app.is_pad_held(0));
        assert_eq!(app.overlay(), Some(&super::Overlay::Help));
    }

    #[test]
    fn enter_triggers_the_selected_pad_in_perform_mode() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Right,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(calls.snapshot(), [AudioCall::Trigger(pad(0, 1), 64, 1.0)]);
        assert!(!app.is_pad_held(1));
    }

    #[test]
    fn invalid_palette_command_stays_open_with_inline_error() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("select 0".into()));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(app.overlay(), Some(&super::Overlay::Palette));
        assert_eq!(app.palette_error(), Some("select expects 1..=16"));
    }

    #[test]
    fn palette_error_survives_multibyte_and_no_op_cursor_navigation() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("wat한".into()));
        let press = |code| KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press);
        app.apply_key(press(KeyCode::Enter));
        let error = Some("unknown command: wat한");
        assert_eq!(app.palette_error(), error);

        app.apply_key(press(KeyCode::Left));
        assert_eq!(app.palette_cursor(), 3);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::Right));
        assert_eq!(app.palette_cursor(), 6);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::End));
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::Home));
        app.apply_key(press(KeyCode::Home));
        app.apply_key(press(KeyCode::Left));
        app.apply_key(press(KeyCode::Backspace));
        assert_eq!(app.palette_cursor(), 0);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::End));
        app.apply_key(press(KeyCode::Delete));
        app.apply_terminal_event(Event::Paste(String::new()));
        assert_eq!(app.palette_cursor(), 6);
        assert_eq!(app.palette_error(), error);

        app.apply_key(key('x', KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(app.palette_error(), error);
    }

    #[test]
    fn closing_the_palette_clears_its_inline_error() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("wat".into()));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert_eq!(app.palette_error(), Some("unknown command: wat"));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(app.overlay(), None);
        assert_eq!(app.palette_error(), None);
    }

    #[test]
    fn shifted_question_mark_opens_help_without_triggering_a_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply_key(key('?', KeyModifiers::SHIFT, KeyEventKind::Press));

        assert_eq!(app.overlay(), Some(&super::Overlay::Help));
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn picker_for_a_relative_filename_starts_in_the_current_directory() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.begin_load(pad(0, 0), path("kick.wav"));

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one scan request")
        };
        assert_eq!(path, &std::env::current_dir().unwrap());
    }

    #[test]
    fn picker_resolves_a_nested_relative_source_and_backs_up_to_current_directory() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let request = app.begin_load(pad(0, 0), path("samples/kick.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "samples/kick.wav"));

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one nested scan request")
        };
        assert!(path.is_absolute());
        assert_eq!(path, &current_dir.join("samples"));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one parent scan request")
        };
        assert_eq!(path, &current_dir);
    }

    #[test]
    fn empty_relative_picker_directory_maps_to_current_directory_before_parent_navigation() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at("");

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized scan request")
        };
        assert_eq!(path, &current_dir);

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized parent scan request")
        };
        assert_eq!(path, current_dir.parent().unwrap());
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn relative_picker_directory_is_lexically_normalized() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at("samples/../drums/.");

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized scan request")
        };
        assert_eq!(path, &current_dir.join("drums"));
    }

    #[test]
    fn repeated_hidden_toggles_queue_the_pending_directory_and_supersede_prior_scans() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/one");
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id,
                path: committed_path,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected committed-directory scan")
        };
        assert!(app.apply_worker_result(WorkerResult::Scanned {
            request_id: *request_id,
            path: committed_path.clone(),
            result: Ok(DirectoryScan::complete(Vec::new())),
        }));

        app.open_picker_at("/two");
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: first_id,
                path: first_path,
                show_hidden: false,
            },
        ] = requests.as_slice()
        else {
            panic!("expected initial pending scan")
        };
        assert_eq!(first_path, path("/two"));

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: second_id,
                path: second_path,
                show_hidden: true,
            },
        ] = requests.as_slice()
        else {
            panic!("expected first hidden rescan")
        };
        assert_eq!(second_path, path("/two"));

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: third_id,
                path: third_path,
                show_hidden: false,
            },
        ] = requests.as_slice()
        else {
            panic!("expected second hidden rescan")
        };
        assert_eq!(third_path, path("/two"));
        assert!(*first_id < *second_id && *second_id < *third_id);

        assert!(!app.apply_worker_result(WorkerResult::Scanned {
            request_id: *first_id,
            path: first_path.clone(),
            result: Ok(DirectoryScan::complete(Vec::new())),
        }));
        assert!(app.apply_worker_result(WorkerResult::Scanned {
            request_id: *third_id,
            path: third_path.clone(),
            result: Ok(DirectoryScan::complete(Vec::new())),
        }));
        assert_eq!(app.file_picker().directory(), path("/two"));
    }

    #[cfg(unix)]
    #[test]
    fn relative_picker_normalization_preserves_non_unicode_components() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let relative = PathBuf::from(OsString::from_vec(vec![b's', 0x80, b'm', b'p']));
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at(relative.clone());

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one lossless scan request")
        };
        assert_eq!(path, &current_dir.join(relative));
    }

    #[test]
    fn picker_without_a_source_reopens_at_the_current_directory() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/other");
        app.close_overlay();
        app.take_worker_requests();

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one scan request")
        };
        assert_eq!(path, &std::env::current_dir().unwrap());
    }

    #[test]
    fn stale_picker_error_for_the_same_directory_is_silent() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/samples");
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: stale_id,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected one stale scan request")
        };
        let stale_id = *stale_id;
        app.open_picker_at("/samples");
        app.take_worker_requests();

        let applied = app.apply_worker_result(WorkerResult::Scanned {
            request_id: stale_id,
            path: "/samples".into(),
            result: Err("stale failure".to_owned()),
        });

        assert!(!applied);
        assert_eq!(app.status(), "");
    }

    #[test]
    fn rejected_current_scan_clears_pending_state_and_keeps_old_entries() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/samples");
        let request = app.take_worker_requests().pop().unwrap();

        app.apply_worker_send_error(request, WorkerSendError::WorkerBusy);

        assert!(!app.file_picker().is_scanning());
        assert_eq!(app.file_picker().failed_directory(), Some(path("/samples")));
        assert_eq!(app.file_picker().error(), Some("loader busy"));
        assert_eq!(app.status(), "loader busy");
    }

    #[test]
    fn rejected_stale_scan_for_the_same_directory_is_silent() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/samples");
        let stale = app.take_worker_requests().pop().unwrap();
        app.open_picker_at("/samples");
        app.take_worker_requests();

        assert!(!app.apply_worker_send_error(stale, WorkerSendError::WorkerBusy));

        assert!(app.file_picker().is_scanning());
        assert_eq!(app.status(), "");
    }

    #[test]
    fn sample_enter_confirms_apply_without_triggering_a_pad_and_escape_discards_explicitly() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        calls.clear();

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(app.overlay(), Some(super::Overlay::ApplySample { pad: actual, .. }) if *actual == pad)
        );
        assert!(calls.snapshot().is_empty());

        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        assert!(app.sample_editor().draft().normalize);
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay(), Some(&super::Overlay::DiscardSample { pad }));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.sample_editor().draft().normalize);
    }

    #[test]
    fn sample_plain_z_stays_a_global_pad_and_control_z_is_editor_undo() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        calls.clear();

        app.apply_key(key('z', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key('z', KeyModifiers::CONTROL, KeyEventKind::Press));

        assert_eq!(calls.snapshot(), [AudioCall::Trigger(pad(0, 12), 64, 1.0)]);
        assert_eq!(app.status(), "selected pad is empty");
    }

    #[test]
    fn failed_sample_setting_admission_keeps_pad_and_editor_settings_unchanged() {
        let fake = FakeAudio::ready(48_000, 2).failing_update_once("settings queue full");
        let mut app = App::with_audio(Box::new(fake));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let prior = app.pad(pad).settings;

        app.apply_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(app.pad(pad).settings, prior);
        assert_eq!(app.sample_editor().settings(), prior);
        assert_eq!(app.status(), "settings queue full");
    }

    #[test]
    fn palette_sample_commands_reject_an_empty_selected_pad() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("normalize on".into()));

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.palette_error(), Some("selected pad is empty"));
    }

    #[test]
    fn dirty_sample_blocks_view_exit_and_pad_selection_until_discarded() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('2', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        assert_eq!(app.selected_pad(), 0);
        assert_eq!(app.sample_editor().pad(), pad);
        assert!(
            matches!(app.overlay(), Some(super::Overlay::DiscardSample { pad: actual }) if *actual == pad)
        );
    }

    #[test]
    fn backtab_cycles_backward_through_every_workspace_and_keeps_shift_tab_compatibility() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Pattern);
        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Perform);

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
    }

    #[test]
    fn backtab_uses_the_same_dirty_sample_discard_fence_as_shift_tab() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        assert_eq!(app.overlay(), Some(&super::Overlay::DiscardSample { pad }));
    }

    #[test]
    fn sample_apply_pending_rejects_repeated_apply_and_undo_requests() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let pending_generation = app.sample_editor.pending[0].as_ref().unwrap().generation;

        for _ in 0..4 {
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            app.apply_key(key('z', KeyModifiers::CONTROL, KeyEventKind::Press));
        }

        assert_eq!(
            app.sample_editor.pending[0].as_ref().unwrap().generation,
            pending_generation
        );
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Pending
        ));
        assert_eq!(app.overlay(), None);
    }

    #[test]
    fn external_source_replacement_marks_a_dirty_editor_and_requires_discard_before_apply() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "first.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "first.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "replacement.wav").unwrap()
        else {
            panic!("expected replacement load");
        };
        assert!(app.apply_worker_result(loaded_with_frames(
            pad,
            generation,
            "replacement.wav",
            48_000,
            1,
        )));

        assert_eq!(app.pad(pad).sample.as_ref().unwrap().frames(), 1);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Error(
                crate::SampleEditorError::SelectedPadReplaced
            )
        ));
        assert_eq!(app.overlay(), None);
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        let draft = app.sample_editor().draft();
        let settings = app.sample_editor().settings();
        for key_code in [
            KeyCode::Left,
            KeyCode::Char('n'),
            KeyCode::Up,
            KeyCode::Char('o'),
        ] {
            app.apply_key(KeyEvent::new(key_code, KeyModifiers::NONE));
        }
        app.open_palette();
        app.apply_terminal_event(Event::Paste("trim-start 0".into()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.sample_editor().draft(), draft);
        assert_eq!(app.sample_editor().settings(), settings);
        app.close_overlay();

        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.sample_editor().base_frames(), Some(1));
        assert!(!app.sample_editor().is_dirty());
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(app.sample_editor().draft().normalize);
    }

    #[test]
    fn failed_stale_and_rejected_user_loads_keep_the_committed_source_identity() {
        let setup = || {
            let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
            let pad = pad(0, 0);
            let WorkerRequest::LoadSample { generation, .. } =
                app.begin_load(pad, "first.wav").unwrap()
            else {
                panic!("expected initial load");
            };
            assert!(app.apply_worker_result(loaded(pad, generation, "first.wav")));
            let identity = app.sample_editor_context(pad).source_generation;
            (app, pad, identity)
        };

        let (mut decode_failed, pad, identity) = setup();
        let WorkerRequest::LoadSample { generation, .. } =
            decode_failed.begin_load(pad, "decode-failed.wav").unwrap()
        else {
            panic!("expected replacement load");
        };
        assert!(decode_failed.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation,
            purpose: LoadPurpose::User,
            path: "decode-failed.wav".into(),
            result: Err(LoadSampleError::Decode("bad payload".to_owned())),
        }));
        assert_eq!(
            decode_failed.sample_editor_context(pad).source_generation,
            identity
        );

        let (mut stale, pad, identity) = setup();
        let stale_result = stale.begin_load(pad, "stale.wav").unwrap();
        let _newer = stale.begin_load(pad, "newer.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = stale_result else {
            panic!("expected stale load");
        };
        assert!(!stale.apply_worker_result(loaded(pad, generation, "stale.wav")));
        assert_eq!(stale.sample_editor_context(pad).source_generation, identity);

        let (mut rejected, pad, identity) = setup();
        let WorkerRequest::LoadSample { generation, .. } =
            rejected.begin_load(pad, "rejected.wav").unwrap()
        else {
            panic!("expected replacement load");
        };
        rejected.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_install("install rejected"),
        ));
        assert!(rejected.apply_worker_result(loaded(pad, generation, "rejected.wav")));
        assert_eq!(
            rejected.sample_editor_context(pad).source_generation,
            identity
        );
    }

    #[test]
    fn device_rate_recovery_keeps_the_committed_source_identity() {
        let mut app = App::with_audio(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        let pad = pad(0, 0);
        let settings = PadSettings {
            gain_db: -2.0,
            ..PadSettings::default()
        };
        app.update_pad_settings(pad, settings).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let identity = app.sample_editor_context(pad).source_generation;
        let revision = app.project_revision();
        let fingerprint = app.sample_editor.commits[0].fingerprint;
        let recipe = app.sample_editor.commits[0].recipe;
        assert!(app.maintain_audio());

        app.retry_with(Box::new(FakeAudio::ready(44_100, 2)));
        let request = app.take_worker_requests().pop().unwrap();
        let WorkerRequest::LoadSample {
            generation,
            purpose,
            path,
            ..
        } = request
        else {
            panic!("expected recovery load");
        };
        let mut recovery = loaded_with_purpose_recipe_and_frames(
            pad,
            generation,
            purpose,
            path.to_str().unwrap(),
            44_100,
            1,
            SampleEditRecipe::identity(),
        );
        let WorkerResult::Loaded {
            result: Ok(loaded), ..
        } = &mut recovery
        else {
            panic!("expected successful recovery result");
        };
        loaded.fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(std::path::Path::new("changed.wav"), &[1])
                .unwrap();
        assert!(app.apply_worker_result(recovery));

        assert_eq!(app.sample_editor_context(pad).source_generation, identity);
        assert_eq!(app.sample_editor.commits[0].fingerprint, fingerprint);
        assert_eq!(app.sample_editor.commits[0].recipe, recipe);
        assert_eq!(app.pad(pad).settings, settings);
        assert_eq!(app.project_revision(), revision);
    }

    #[test]
    fn recovery_result_precedes_same_path_user_result_without_consuming_the_user_slot() {
        let (mut app, pad, recipe, source_generation, recovery, user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        let WorkerRequest::LoadSample {
            generation: recovery_generation,
            purpose: recovery_purpose,
            path: recovery_path,
            ..
        } = recovery
        else {
            panic!("expected recovery load");
        };
        let WorkerRequest::LoadSample {
            generation: user_generation,
            purpose: user_purpose,
            path: user_path,
            ..
        } = user
        else {
            panic!("expected user load");
        };
        assert_eq!(recovery_generation, user_generation);
        assert_eq!(recovery_path, user_path);
        assert_eq!(recovery_purpose, LoadPurpose::Recovery);
        assert_eq!(user_purpose, LoadPurpose::User);

        assert!(
            app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                recovery_generation,
                recovery_purpose,
                "same.wav",
                44_100,
                2,
                recipe,
            ))
        );

        assert!(app.committed_recovery_loads[0].is_none());
        assert!(matches!(
            app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::WorkerQueued)
        ));
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            source_generation
        );
        assert_eq!(app.sample_editor.commits[0].recipe, recipe);
        assert_eq!(app.base_sample(pad).unwrap().frames(), 2);

        assert!(
            app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                user_generation,
                user_purpose,
                "same.wav",
                44_100,
                3,
                SampleEditRecipe::identity(),
            ))
        );
        assert!(app.pending_loads[0].is_none());
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            user_generation
        );
        assert_eq!(
            app.sample_editor.commits[0].recipe,
            SampleEditRecipe::identity()
        );
        assert_eq!(app.base_sample(pad).unwrap().frames(), 3);
    }

    #[test]
    fn same_path_user_result_precedes_recovery_without_restoring_the_old_recipe() {
        let (mut app, pad, recipe, _source_generation, recovery, user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        let WorkerRequest::LoadSample {
            generation: recovery_generation,
            purpose: recovery_purpose,
            ..
        } = recovery
        else {
            panic!("expected recovery load");
        };
        let WorkerRequest::LoadSample {
            generation: user_generation,
            purpose: user_purpose,
            ..
        } = user
        else {
            panic!("expected user load");
        };

        assert_eq!(recovery_purpose, LoadPurpose::Recovery);
        assert_eq!(user_purpose, LoadPurpose::User);
        assert!(
            app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                user_generation,
                user_purpose,
                "same.wav",
                44_100,
                3,
                SampleEditRecipe::identity(),
            ))
        );
        let committed = Arc::clone(app.base_sample(pad).unwrap());
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            user_generation
        );
        assert_eq!(
            app.sample_editor.commits[0].recipe,
            SampleEditRecipe::identity()
        );
        assert!(app.committed_recovery_loads[0].is_none());

        assert!(
            !app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                recovery_generation,
                recovery_purpose,
                "same.wav",
                44_100,
                2,
                recipe,
            ))
        );
        assert!(Arc::ptr_eq(app.base_sample(pad).unwrap(), &committed));
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            user_generation
        );
        assert_eq!(
            app.sample_editor.commits[0].recipe,
            SampleEditRecipe::identity()
        );
    }

    #[test]
    fn recovery_decode_error_does_not_fail_the_colliding_user_load() {
        let (mut app, pad, recipe, source_generation, recovery, user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        let WorkerRequest::LoadSample {
            generation: recovery_generation,
            purpose: recovery_purpose,
            path,
            ..
        } = recovery
        else {
            panic!("expected recovery load");
        };
        let WorkerRequest::LoadSample {
            generation: user_generation,
            purpose: user_purpose,
            ..
        } = user
        else {
            panic!("expected user load");
        };

        assert!(app.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation: recovery_generation,
            purpose: recovery_purpose,
            path,
            result: Err(LoadSampleError::Decode("recovery decode failed".to_owned())),
        }));

        assert!(matches!(
            app.committed_recovery_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::Failed)
        ));
        assert!(matches!(
            app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::WorkerQueued)
        ));
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            source_generation
        );
        assert_eq!(app.sample_editor.commits[0].recipe, recipe);

        assert_eq!(recovery_purpose, LoadPurpose::Recovery);
        assert_eq!(user_purpose, LoadPurpose::User);
        assert!(
            app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                user_generation,
                user_purpose,
                "same.wav",
                44_100,
                3,
                SampleEditRecipe::identity(),
            ))
        );
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            user_generation
        );
        assert_eq!(
            app.sample_editor.commits[0].recipe,
            SampleEditRecipe::identity()
        );
    }

    #[test]
    fn load_send_errors_mutate_only_the_colliding_request_slot() {
        let (mut busy_app, _pad, _recipe, source_generation, recovery, user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        assert!(busy_app.apply_worker_send_error(recovery, WorkerSendError::WorkerBusy));
        assert!(matches!(
            busy_app.committed_recovery_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::AwaitingWorker)
        ));
        assert!(matches!(
            busy_app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::WorkerQueued)
        ));
        assert!(busy_app.apply_worker_send_error(user, WorkerSendError::WorkerClosed));
        assert!(matches!(
            busy_app.committed_recovery_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::AwaitingWorker)
        ));
        assert!(matches!(
            busy_app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::Failed)
        ));
        assert_eq!(
            busy_app.sample_editor_context(pad(0, 0)).source_generation,
            source_generation
        );

        let (mut closed_app, _pad, recipe, source_generation, recovery, _user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        assert!(closed_app.apply_worker_send_error(recovery, WorkerSendError::WorkerClosed));
        assert!(matches!(
            closed_app.committed_recovery_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::Failed)
        ));
        assert!(matches!(
            closed_app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::WorkerQueued)
        ));
        assert_eq!(
            closed_app
                .sample_editor_context(pad(0, 0))
                .source_generation,
            source_generation
        );
        assert_eq!(closed_app.sample_editor.commits[0].recipe, recipe);
    }

    #[test]
    fn device_failure_and_retry_preserve_the_uncommitted_editor_draft() {
        let mut app = App::with_audio(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(app.maintain_audio());
        assert!(app.sample_editor().draft().normalize);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Error(crate::SampleEditorError::DeviceUnavailable)
        ));

        app.retry_with(Box::new(FakeAudio::ready(48_000, 2)));
        assert!(app.sample_editor().draft().normalize);
        assert!(app.sample_editor().is_dirty());
    }

    #[test]
    fn apply_confirmation_rejects_a_replaced_source_without_queueing_an_edit() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "first.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "first.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::ApplySample { .. })
        ));

        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "second.wav").unwrap()
        else {
            panic!("expected replacement load");
        };
        assert!(app.apply_worker_result(loaded_with_frames(
            pad,
            generation,
            "second.wav",
            48_000,
            2,
        )));
        let edit_generation = app.sample_editor.generations[0];

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.sample_editor.generations[0], edit_generation);
        assert!(app.sample_editor.pending[0].is_none());
        assert!(app.sample_editor().draft().normalize);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Error(
                crate::SampleEditorError::SelectedPadReplaced
            )
        ));
        assert_eq!(app.overlay(), None);
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
    }

    #[test]
    fn apply_confirmation_with_a_changed_editor_state_closes_without_queueing_work() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.editor
            .observe_error(crate::SampleEditorError::InstallFailed);
        let edit_generation = app.sample_editor.generations[0];

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.sample_editor.generations[0], edit_generation);
        assert!(app.sample_editor.pending[0].is_none());
        assert_eq!(app.overlay(), None);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Error(crate::SampleEditorError::InstallFailed)
        ));
    }

    #[test]
    fn apply_rejection_while_replacement_is_pending_closes_confirmation_once_and_keeps_draft() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "first.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "first.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(app.sample_editor().draft().normalize);

        let replacement = app.begin_load(pad, "replacement.wav").unwrap();
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::ApplySample { .. })
        ));
        assert!(app.apply_sample_context.is_some());

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.overlay(), None);
        assert!(app.apply_sample_context.is_none());
        assert!(app.sample_editor().draft().normalize);
        assert!(app.sample_editor.pending[0].is_none());
        let status = app.status().to_owned();
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        assert_eq!(app.status(), status);

        let WorkerRequest::LoadSample { generation, .. } = replacement else {
            panic!("expected replacement load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "replacement.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::ApplySample { .. })
        ));
    }

    #[test]
    fn empty_sample_keys_match_palette_rejection_without_mutating_settings() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let pad = pad(0, 0);
        let prior = app.pad(pad).settings;

        for key_code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('o'),
            KeyCode::Char('g'),
            KeyCode::Char('l'),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('n'),
            KeyCode::Char('u'),
        ] {
            app.apply_key(KeyEvent::new(key_code, KeyModifiers::NONE));
        }

        assert_eq!(app.pad(pad).settings, prior);
        assert_eq!(app.sample_editor().settings(), prior);
        assert_eq!(app.status(), "selected pad is empty");

        app.apply_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.status(), "selected pad is empty");
    }

    #[test]
    fn pending_sample_edit_blocks_navigation_without_opening_discard() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        assert_eq!(app.overlay(), None);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Pending
        ));
    }

    #[test]
    fn palette_exact_trim_rejects_crossing_in_both_directions_without_mutating_the_draft() {
        for (first, crossing) in [
            ("trim-end 3", "trim-start 4"),
            ("trim-start 3", "trim-end 2"),
        ] {
            let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
            let pad = pad(0, 0);
            let WorkerRequest::LoadSample { generation, .. } =
                app.begin_load(pad, "seven.wav").unwrap()
            else {
                panic!("expected load request");
            };
            assert!(app.apply_worker_result(loaded_with_frames(
                pad,
                generation,
                "seven.wav",
                48_000,
                7,
            )));
            app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

            app.open_palette();
            app.apply_terminal_event(Event::Paste(first.to_owned()));
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(app.palette_error(), None);
            let before = app.sample_editor().draft();

            app.open_palette();
            app.apply_terminal_event(Event::Paste(crossing.to_owned()));
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

            assert_eq!(
                app.palette_error(),
                Some("trim marker would cross the other marker")
            );
            assert_eq!(app.sample_editor().draft(), before);
        }
    }

    #[test]
    fn palette_exact_trim_round_trips_non_divisible_frame_counts() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "seven.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded_with_frames(
            pad,
            generation,
            "seven.wav",
            48_000,
            7,
        )));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        for command in ["trim-start 2", "trim-end 5"] {
            app.open_palette();
            app.apply_terminal_event(Event::Paste(command.to_owned()));
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(app.palette_error(), None, "{command}");
        }

        assert_eq!(app.sample_editor().draft().frame_range(7).unwrap(), 2..5);
    }

    fn project_app() -> App {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "project.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded_with_frames(
            pad,
            generation,
            "project.wav",
            48_000,
            7,
        )));
        app.patterns.set_view(WorkspaceView::Sample);
        app.sync_editor_to_selected_pad();
        app
    }

    #[test]
    fn project_revision_advances_only_for_committed_mutations() {
        let mut app = project_app();
        assert_eq!(app.project_revision(), 1);

        let revision = app.project_revision();
        app.patterns.toggle_view();
        app.patterns.select_slot(PatternSlotId::new(1).unwrap());
        app.patterns.move_cursor_steps(1);
        app.apply_telemetry(app.telemetry());
        assert_eq!(app.project_revision(), revision);

        app.patterns.select_slot(PatternSlotId::new(0).unwrap());
        for edit in [
            |patterns: &mut PatternWorkspace| patterns.toggle_step(),
            |patterns: &mut PatternWorkspace| patterns.toggle_step(),
            |patterns: &mut PatternWorkspace| {
                patterns.set_tempo(sampler_core::Tempo::new(124.0).unwrap())
            },
            |patterns: &mut PatternWorkspace| patterns.set_bars(2),
            |patterns: &mut PatternWorkspace| {
                patterns.set_resolution(sampler_core::Resolution::Eighth)
            },
            |patterns: &mut PatternWorkspace| patterns.set_swing(0.6),
            |patterns: &mut PatternWorkspace| patterns.set_quantize(0.5),
            |patterns: &mut PatternWorkspace| patterns.clear_selected(),
            |patterns: &mut PatternWorkspace| patterns.undo_clear(),
        ] {
            let before = app.project_revision();
            app.apply_pattern_edit(edit);
            assert_eq!(app.project_revision(), before + 1);
        }

        let before = app.project_revision();
        let settings = PadSettings {
            gain_db: -3.0,
            ..PadSettings::default()
        };
        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_update_once("settings rejected"),
        ));
        assert!(app.update_pad_settings(pad(0, 0), settings).is_err());
        assert_eq!(app.project_revision(), before);
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2)));
        app.update_pad_settings(pad(0, 0), settings).unwrap();
        assert_eq!(app.project_revision(), before + 1);
    }

    #[test]
    fn admitted_apply_and_undo_each_advance_one_project_revision() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };

        let before_apply = app.project_revision();
        app.request_sample_edit(pad, recipe).unwrap();
        let requests = app.take_worker_requests();
        let [WorkerRequest::EditSample { generation, .. }] = requests.as_slice() else {
            panic!("expected apply edit request");
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            *generation,
            recipe,
            48_000,
            vec![-0.4, 0.4],
        )));
        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), before_apply + 1);

        let before_undo = app.project_revision();
        app.undo_sample_edit(pad).unwrap();
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                generation,
                recipe: undo_recipe,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected undo edit request");
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            *generation,
            *undo_recipe,
            48_000,
            vec![0.25, -0.25],
        )));
        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), before_undo + 1);
    }

    #[test]
    fn snapshot_refuses_dirty_or_pending_sample_state_and_uses_editable_patterns() {
        let mut app = project_app();
        app.editor_mut_for_test().move_marker(1, false);
        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::DirtySampleDraft(pad(0, 0)))
        );

        app.discard_sample_draft();
        app.patterns_mut_for_test().toggle_step().unwrap();
        assert_eq!(app.project_snapshot().unwrap().patterns[0].events.len(), 1);

        let pending_pad = pad(0, 1);
        let _ = app.begin_load(pending_pad, "pending.wav");
        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleLoad(pending_pad))
        );
    }

    #[test]
    fn rejected_audio_admission_keeps_tuple_and_revision_exact() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let offset = super::pad_offset(pad);
        let before_revision = app.project_revision();
        let before_source = app.pads[offset].source.clone();
        let before_generation = app.sample_editor.commits[offset].source_generation;
        let before_recipe = app.sample_editor.commits[offset].recipe;
        let before_fingerprint = app.sample_editor.commits[offset].fingerprint;
        let before_settings = app.pads[offset].settings;

        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_install("install rejected"),
        ));
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "replacement.wav").unwrap()
        else {
            panic!("expected replacement request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "replacement.wav")));

        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.pads[offset].source, before_source);
        assert_eq!(
            app.sample_editor.commits[offset].source_generation,
            before_generation
        );
        assert_eq!(app.sample_editor.commits[offset].recipe, before_recipe);
        assert_eq!(
            app.sample_editor.commits[offset].fingerprint,
            before_fingerprint
        );
        assert_eq!(app.pads[offset].settings, before_settings);
    }

    #[test]
    fn snapshot_refuses_an_exact_pending_project_operation() {
        let mut app = project_app();
        let token = crate::ProjectToken::new(77);
        app.project_session
            .set_in_flight(Some(crate::ProjectOperationDescriptor {
                token,
                kind: crate::SaveKind::Explicit,
                project_id: app.project_session.project_id(),
                directory: "project".into(),
                revision: app.project_revision(),
            }));

        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::PendingProjectOperation(token))
        );
    }

    #[test]
    fn exhausted_revision_refuses_mutation_without_partial_state_change() {
        let mut app = project_app();
        app.project_session
            .set_current_revision_for_test(i64::MAX as u64);
        let before_settings = app.pad(pad(0, 0)).settings;
        let before_generation = app.pad(pad(0, 0)).generation;
        let before_pattern = app.patterns.export_project_patterns().unwrap();

        let settings = PadSettings {
            gain_db: -6.0,
            ..before_settings
        };
        assert!(app.update_pad_settings(pad(0, 0), settings).is_err());
        app.apply_pattern_edit(|patterns| patterns.toggle_step());
        assert!(app.begin_load(pad(0, 0), "refused.wav").is_none());
        assert_eq!(
            app.request_sample_edit(
                pad(0, 0),
                SampleEditRecipe {
                    reversed: true,
                    ..SampleEditRecipe::identity()
                }
            ),
            Err(super::SampleEditRequestError::ProjectRevisionExhausted)
        );

        assert_eq!(app.pad(pad(0, 0)).settings, before_settings);
        assert_eq!(app.pad(pad(0, 0)).generation, before_generation);
        assert_eq!(
            app.patterns.export_project_patterns().unwrap(),
            before_pattern
        );
        assert_eq!(app.project_revision(), i64::MAX as u64);
    }

    #[test]
    fn device_rate_recovery_preserves_same_revision_project_snapshot() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.apply_pattern_edit(|patterns| patterns.toggle_step());
        let before_revision = app.project_revision();
        let before = app.project_snapshot().unwrap();

        assert!(app.retry_with(Box::new(FakeAudio::ready(44_100, 2))));

        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert_eq!(app.patterns.sample_rates(), [44_100; 16]);
    }

    #[test]
    fn unloaded_pad_settings_are_local_and_do_not_advance_project_revision() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let settings = PadSettings {
            gain_db: -4.0,
            ..PadSettings::default()
        };

        app.update_pad_settings(pad(0, 0), settings).unwrap();

        assert_eq!(app.pad(pad(0, 0)).settings, settings);
        assert_eq!(app.project_revision(), 0);
        assert!(app.project_snapshot().unwrap().pads.is_empty());
    }

    #[test]
    fn accepted_record_trigger_and_release_each_advance_one_revision() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let stamp = TransportStamp {
            slot: PatternSlotId::new(0).unwrap(),
            generation: app.patterns.selected_pattern().generation(),
            origin: 1_000,
            loop_frames: app.patterns.selected_pattern().transport().loop_frames(),
        };
        app.patterns.start_recording(stamp).unwrap();
        app.patterns
            .note_live_trigger(0, LiveCommandId::FIRST, pad, 1.0);
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks([
            LiveAck {
                id: LiveCommandId::FIRST,
                pad,
                kind: LiveAckKind::Trigger { velocity: 1.0 },
                frame: 1_120,
                transport: Some(stamp),
            },
        ])));
        let before_trigger = app.project_revision();
        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), before_trigger + 1);

        app.patterns.note_live_release(0, LiveCommandId::FIRST);
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks([
            LiveAck {
                id: LiveCommandId::FIRST,
                pad,
                kind: LiveAckKind::Release,
                frame: 1_240,
                transport: Some(stamp),
            },
        ])));
        let before_release = app.project_revision();
        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), before_release + 1);
        assert_eq!(app.project_snapshot().unwrap().patterns[0].events.len(), 1);
        assert_eq!(
            app.project_snapshot().unwrap().patterns[0].events[0]
                .event
                .duration,
            Some(120)
        );
    }

    #[test]
    fn disconnected_loaded_pad_rejects_settings_without_changing_snapshot_or_revision() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let before = app.project_snapshot().unwrap();
        let before_revision = app.project_revision();
        let before_settings = app.pad(pad).settings;
        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        assert!(app.maintain_audio());
        assert!(!app.current_session_bound[0]);

        let requested = PadSettings {
            gain_db: -8.0,
            ..before_settings
        };
        assert_eq!(
            app.update_pad_settings(pad, requested),
            Err("loaded sample is not admitted to the current audio session".to_owned())
        );

        assert_eq!(app.pad(pad).settings, before_settings);
        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn replacement_decode_failure_restores_the_pre_request_snapshot() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let before = app.project_snapshot().unwrap();
        let before_revision = app.project_revision();
        let WorkerRequest::LoadSample {
            generation,
            purpose,
            path,
            ..
        } = app.begin_load(pad, "broken.wav").unwrap()
        else {
            panic!("expected replacement load request");
        };
        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleLoad(pad))
        );

        assert!(app.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation,
            purpose,
            path,
            result: Err(LoadSampleError::Decode(
                "replacement decode failed".to_owned()
            )),
        }));

        assert!(app.status().contains("replacement decode failed"));
        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn apply_render_failure_restores_the_pre_request_snapshot() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let before = app.project_snapshot().unwrap();
        let before_revision = app.project_revision();
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let requests = app.take_worker_requests();
        let [WorkerRequest::EditSample { generation, .. }] = requests.as_slice() else {
            panic!("expected edit request");
        };
        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleEdit(pad))
        );

        assert!(app.apply_worker_result(WorkerResult::Edited {
            pad,
            generation: *generation,
            recipe,
            result: Err("apply render failed".to_owned()),
        }));

        assert!(app.status().contains("apply render failed"));
        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn snapshot_still_refuses_every_active_sample_operation_phase() {
        let pad = pad(0, 0);

        let mut awaiting_load = project_app();
        awaiting_load.audio = None;
        assert!(awaiting_load.begin_load(pad, "awaiting.wav").is_none());
        assert_eq!(
            awaiting_load.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleLoad(pad))
        );

        let mut ready_load = project_app();
        let WorkerRequest::LoadSample {
            generation,
            purpose,
            path,
            ..
        } = ready_load.begin_load(pad, "ready.wav").unwrap()
        else {
            panic!("expected ready load request");
        };
        ready_load.audio = None;
        assert!(ready_load.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation,
            purpose,
            path,
            result: match loaded(pad, generation, "ready.wav") {
                WorkerResult::Loaded { result, .. } => result,
                _ => unreachable!(),
            },
        }));
        assert_eq!(
            ready_load.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleLoad(pad))
        );

        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        let mut awaiting_edit = project_app();
        awaiting_edit.request_sample_edit(pad, recipe).unwrap();
        let [request] = awaiting_edit.take_worker_requests().try_into().unwrap();
        assert!(awaiting_edit.apply_worker_send_error(request, WorkerSendError::WorkerBusy));
        assert_eq!(
            awaiting_edit.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleEdit(pad))
        );

        let mut ready_edit = project_app();
        ready_edit.request_sample_edit(pad, recipe).unwrap();
        let requests = ready_edit.take_worker_requests();
        let [WorkerRequest::EditSample { generation, .. }] = requests.as_slice() else {
            panic!("expected ready edit request");
        };
        assert!(ready_edit.apply_worker_result(edited(
            &ready_edit,
            pad,
            *generation,
            recipe,
            48_000,
            vec![-0.5, 0.5],
        )));
        assert_eq!(
            ready_edit.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleEdit(pad))
        );
    }

    fn name_project(app: &mut App, directory: &str, now: Instant) -> sampler_core::ProjectId {
        let project_id = sampler_core::ProjectId::from_bytes([0x51; 16]);
        app.project_session = crate::ProjectSession::new(
            project_id,
            Some(directory.into()),
            "Beat",
            app.project_revision(),
        );
        app.project_session
            .commit_project_mutation(now, || Ok::<(), ()>(()))
            .unwrap();
        project_id
    }

    fn take_project_save(app: &mut App) -> ProjectSaveWorkerRequest {
        let requests = app.take_worker_requests();
        let [WorkerRequest::SaveProject(request)] = requests.as_slice() else {
            panic!("expected one project save request");
        };
        (**request).clone()
    }

    fn take_recovery_cleanup(app: &mut App) -> RecoveryCleanup {
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::DiscardRecovery {
                token,
                directory,
                project_id,
                revision,
            },
        ] = requests.as_slice()
        else {
            panic!("expected one recovery cleanup request");
        };
        RecoveryCleanup {
            token: *token,
            directory: directory.clone(),
            project_id: *project_id,
            revision: *revision,
        }
    }

    fn save_result(
        request: &ProjectSaveWorkerRequest,
        mappings: Vec<ProjectAssetMapping>,
    ) -> WorkerResult {
        let save = &request.request;
        WorkerResult::ProjectSaved {
            token: request.token,
            kind: save.kind,
            project_id: save.snapshot.project_id,
            directory: save.directory.clone(),
            revision: save.snapshot.revision,
            result: Ok(SaveReceipt {
                directory: save.directory.clone(),
                kind: save.kind,
                project_id: save.snapshot.project_id,
                revision: save.snapshot.revision,
                canonical_toml: "saved".to_owned(),
                mappings,
            }),
        }
    }

    fn save_error(request: &ProjectSaveWorkerRequest, message: &'static str) -> WorkerResult {
        let save = &request.request;
        WorkerResult::ProjectSaved {
            token: request.token,
            kind: save.kind,
            project_id: save.snapshot.project_id,
            directory: save.directory.clone(),
            revision: save.snapshot.revision,
            result: Err(ProjectStoreError::Filesystem {
                operation: message,
                path: save.directory.clone(),
                kind: std::io::ErrorKind::PermissionDenied,
            }),
        }
    }

    fn project_open_document(
        project_id: sampler_core::ProjectId,
        name: &str,
        revision: u64,
        pads: Vec<sampler_core::ProjectPad>,
    ) -> sampler_core::ProjectDocument {
        sampler_core::ProjectDocument::new_v2(
            project_id,
            name,
            revision,
            pads,
            PatternWorkspace::new(48_000)
                .export_project_patterns()
                .unwrap(),
        )
        .unwrap()
    }

    fn project_open_pad(pad: PadId, settings: PadSettings) -> sampler_core::ProjectPad {
        let fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        sampler_core::ProjectPad::new(
            pad,
            format!("audio/{}.wav", fingerprint.digest),
            fingerprint.digest,
            settings,
            SampleEditRecipe::identity(),
        )
        .unwrap()
    }

    fn staged_project_result(
        request: &crate::StageProjectSampleRequest,
        fingerprint: crate::SourceFingerprint,
    ) -> WorkerResult {
        let rendered = Arc::new(SampleBuffer::new(request.engine_rate, vec![0.25, -0.25]).unwrap());
        WorkerResult::ProjectSampleStaged {
            token: request.token,
            pad: request.pad,
            revision: request.revision,
            path: request.path.clone(),
            recipe: request.recipe,
            result: Ok(LoadedSample {
                fingerprint,
                base: Arc::clone(&rendered),
                base_preview: Arc::new([PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]),
                rendered,
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS],
                ),
                recipe: request.recipe,
                source_rate: request.engine_rate,
                source_frames: 1,
                duration: Duration::from_secs_f64(1.0 / f64::from(request.engine_rate)),
            }),
        }
    }

    fn stage_project_open(app: &mut App, directory: &str, document: sampler_core::ProjectDocument) {
        let token = app.request_open_project(directory).unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: directory.into(),
            result: Ok(crate::ProjectProbe {
                directory: directory.into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        let fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        while app.project_open_stage().unwrap().staged_pads
            < app.project_open_stage().unwrap().total_pads
        {
            assert!(app.maintain_project(Instant::now()));
            let requests = app.take_worker_requests();
            let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
                panic!("expected one staged decode request");
            };
            assert!(app.apply_worker_result(staged_project_result(request, fingerprint)));
        }
    }

    #[test]
    fn project_open_stale_probe_and_cancel_preserve_the_complete_old_tuple() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let token = app.request_open_project("project-b").unwrap();
        let stale = crate::ProjectToken::new(token.get() + 1);
        let candidate = project_open_document(
            sampler_core::ProjectId::from_bytes([0x72; 16]),
            "Project B",
            9,
            Vec::new(),
        );

        assert!(!app.apply_worker_result(WorkerResult::ProjectProbed {
            token: stale,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(candidate.clone())),
                recovery: None,
            }),
        }));
        assert_eq!(app.project_snapshot().unwrap(), before);
        app.cancel_project_open().unwrap();
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert!(!app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(candidate)),
                recovery: None,
            }),
        }));
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_refuses_unresolved_sample_state_before_allocating_a_probe() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pending_pad = pad(0, 2);
        assert!(app.begin_load(pending_pad, "pending.wav").is_some());

        assert!(matches!(
            app.request_open_project("project-b"),
            Err(crate::ProjectOpenError::UnresolvedState(_))
        ));
        assert!(app.project_open_stage().is_none());
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn project_open_stages_one_asset_per_maintenance_without_audio_commands() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let project_id = sampler_core::ProjectId::from_bytes([0x73; 16]);
        let pads = vec![
            project_open_pad(pad(0, 0), PadSettings::default()),
            project_open_pad(
                pad(0, 1),
                PadSettings::new(PlaybackMode::Gate, -3.0, 0.25, 2.0, None).unwrap(),
            ),
        ];
        let document = project_open_document(project_id, "Project B", 4, pads);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));

        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(first)] = requests.as_slice() else {
            panic!("expected exactly one staged decode request");
        };
        assert_eq!(first.pad, pad(0, 0));
        assert!(calls.snapshot().is_empty());
        assert!(!app.maintain_project(Instant::now()));
        assert!(app.take_worker_requests().is_empty());
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn project_open_worker_backpressure_then_device_loss_pauses_staging_without_panicking() {
        let audio = FakeAudio::ready(48_000, 2).failing_runtime("device lost");
        let mut app = App::with_audio(Box::new(audio));
        let before = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x8a; 16]),
            "Project B",
            5,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let [request] = app.take_worker_requests().try_into().unwrap();
        assert!(app.apply_worker_send_error(request, WorkerSendError::WorkerBusy));
        assert!(app.maintain_audio());
        assert_eq!(app.audio_format(), None);

        let maintained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.maintain_project(Instant::now())
        }));
        assert!(matches!(maintained, Ok(false)));
        assert!(app.take_worker_requests().is_empty());
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Staging
        );

        assert!(app.retry_with(Box::new(FakeAudio::ready(48_000, 2))));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
            panic!("expected the paused stage request to restart");
        };
        assert_eq!(request.token, token);
        assert_eq!(request.pad, pad(0, 0));
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_probe_failure_is_retained_as_the_exact_typed_error() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        let error = ProjectStoreError::Filesystem {
            operation: "probe project",
            path: "project-b".into(),
            kind: std::io::ErrorKind::PermissionDenied,
        };

        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Err(error.clone()),
        }));

        assert_eq!(
            app.project_open_error(),
            Some(&crate::ProjectOpenError::Probe(error))
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_build_failures_are_retained_as_typed_errors() {
        let mut unavailable = App::with_audio(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device lost"),
        ));
        let before = unavailable.project_snapshot().unwrap();
        let token = unavailable.request_open_project("project-b").unwrap();
        unavailable.take_worker_requests();
        assert!(unavailable.maintain_audio());
        assert!(
            unavailable.apply_worker_result(WorkerResult::ProjectProbed {
                token,
                directory: "project-b".into(),
                result: Ok(crate::ProjectProbe {
                    directory: "project-b".into(),
                    explicit: Some(Ok(project_open_document(
                        sampler_core::ProjectId::from_bytes([0x8b; 16]),
                        "Project B",
                        3,
                        Vec::new(),
                    ))),
                    recovery: None,
                }),
            })
        );
        assert_eq!(
            unavailable.project_open_error(),
            Some(&crate::ProjectOpenError::AudioUnavailable)
        );
        assert_eq!(unavailable.project_snapshot().unwrap(), before);

        let mut invalid = project_app();
        let before = invalid.project_snapshot().unwrap();
        let token = invalid.request_open_project("project-c").unwrap();
        invalid.take_worker_requests();
        let mut document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x8c; 16]),
            "Project C",
            4,
            Vec::new(),
        );
        document.patterns[0].name.clear();
        assert!(invalid.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-c".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-c".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(matches!(
            invalid.project_open_error(),
            Some(crate::ProjectOpenError::InvalidPatterns(_))
        ));
        assert_eq!(invalid.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_stage_failure_is_retained_as_the_exact_typed_error() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x8d; 16]),
            "Project B",
            5,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
            panic!("expected one staged decode request");
        };
        let load_error = LoadSampleError::Prepare("recipe failed".to_owned());
        assert!(app.apply_worker_result(WorkerResult::ProjectSampleStaged {
            token: request.token,
            pad: request.pad,
            revision: request.revision,
            path: request.path.clone(),
            recipe: request.recipe,
            result: Err(load_error.clone()),
        }));
        assert_eq!(
            app.project_open_error(),
            Some(&crate::ProjectOpenError::Stage {
                pad: pad(0, 0),
                error: crate::ProjectStageError::Load(load_error),
            })
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_rejects_stale_and_digest_mismatched_stage_results_atomically() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let before = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x74; 16]),
            "Project B",
            5,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
            panic!("expected staged decode request");
        };
        let mut stale_request = (**request).clone();
        stale_request.token = crate::ProjectToken::new(token.get() + 1);
        let exact_fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        assert!(
            !app.apply_worker_result(staged_project_result(&stale_request, exact_fingerprint,))
        );
        assert!(app.project_open_stage().is_some());

        let mut mismatched = exact_fingerprint;
        mismatched.digest = sampler_core::AssetDigest::from_bytes([0x99; 32]);
        assert!(app.apply_worker_result(staged_project_result(request, mismatched)));
        assert!(app.project_open_stage().is_none());
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn project_open_collects_all_exact_stage_results_before_audio_admission() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x75; 16]),
            "Project B",
            6,
            vec![
                project_open_pad(pad(0, 0), PadSettings::default()),
                project_open_pad(pad(0, 1), PadSettings::default()),
            ],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        let fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        for expected in [pad(0, 0), pad(0, 1)] {
            assert!(app.maintain_project(Instant::now()));
            let requests = app.take_worker_requests();
            let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
                panic!("expected one staged decode request");
            };
            assert_eq!(request.pad, expected);
            assert!(app.apply_worker_result(staged_project_result(request, fingerprint)));
            assert!(calls.snapshot().is_empty());
        }
        let stage = app.project_open_stage().unwrap();
        assert_eq!(stage.staged_pads, 2);
        assert_eq!(stage.phase, crate::ProjectOpenPhase::Staging);
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn project_open_recovery_prompts_only_for_same_id_higher_revision_and_cancel_preserves() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let project_id = sampler_core::ProjectId::from_bytes([0x76; 16]);
        let explicit = project_open_document(project_id, "Explicit", 4, Vec::new());
        let recovery = project_open_document(project_id, "Recovery", 6, Vec::new());
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();

        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(explicit)),
                recovery: Some(Ok(recovery)),
            }),
        }));
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::AwaitingRecoveryChoice
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
        app.choose_project_recovery(crate::RecoveryChoice::Cancel)
            .unwrap();
        assert!(app.project_open_stage().is_none());
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_recovery_lower_is_ignored_and_other_identity_is_rejected() {
        let project_id = sampler_core::ProjectId::from_bytes([0x77; 16]);
        let mut lower = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let token = lower.request_open_project("project-b").unwrap();
        lower.take_worker_requests();
        assert!(lower.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    7,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        assert_eq!(lower.project_open_stage().unwrap().revision, Some(7));
        assert_eq!(
            lower.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Staging
        );

        let mut mismatch = project_app();
        let before = mismatch.project_snapshot().unwrap();
        let token = mismatch.request_open_project("project-c").unwrap();
        mismatch.take_worker_requests();
        assert!(mismatch.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-c".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-c".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    7,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    sampler_core::ProjectId::from_bytes([0x78; 16]),
                    "Other",
                    8,
                    Vec::new(),
                ))),
            }),
        }));
        assert!(mismatch.project_open_stage().is_none());
        assert_eq!(
            mismatch.project_open_error(),
            Some(&crate::ProjectOpenError::RecoveryMismatch)
        );
        assert!(mismatch.status().contains("recovery identity"));
        assert_eq!(mismatch.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_discard_waits_for_exact_recovery_deletion_before_staging_explicit() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let project_id = sampler_core::ProjectId::from_bytes([0x79; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));

        app.choose_project_recovery(crate::RecoveryChoice::Discard)
            .unwrap();
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::DiscardRecovery {
                token: discard_token,
                directory,
                project_id: discarded_id,
                revision,
            },
        ] = requests.as_slice()
        else {
            panic!("expected exact recovery discard");
        };
        assert_eq!(*discard_token, token);
        assert_eq!(directory, path("project-b"));
        assert_eq!(*discarded_id, project_id);
        assert_eq!(*revision, 6);
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::AwaitingRecoveryChoice
        );
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token,
            directory: "project-b".into(),
            project_id,
            revision: 6,
            result: Ok(()),
        }));
        assert_eq!(app.project_open_stage().unwrap().revision, Some(4));
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Staging
        );
    }

    #[test]
    fn project_open_discard_cannot_be_cancelled_after_exact_deletion_is_queued() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let project_id = sampler_core::ProjectId::from_bytes([0x8e; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        app.choose_project_recovery(crate::RecoveryChoice::Discard)
            .unwrap();
        let requests = app.take_worker_requests();

        assert_eq!(
            app.choose_project_recovery(crate::RecoveryChoice::Cancel),
            Err(crate::ProjectOpenError::CancellationLocked)
        );
        assert_eq!(
            app.cancel_project_open(),
            Err(crate::ProjectOpenError::CancellationLocked)
        );
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::AwaitingRecoveryChoice
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert_eq!(requests.len(), 1);
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token,
            directory: "project-b".into(),
            project_id,
            revision: 6,
            result: Ok(()),
        }));
        assert_eq!(app.project_open_stage().unwrap().revision, Some(4));
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_discard_can_be_cancelled_before_exact_deletion_is_dispatched() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let project_id = sampler_core::ProjectId::from_bytes([0x90; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        app.pending_worker_requests = vec![WorkerRequest::Shutdown; super::WORKER_CHANNEL_CAPACITY];
        app.choose_project_recovery(crate::RecoveryChoice::Discard)
            .unwrap();

        app.choose_project_recovery(crate::RecoveryChoice::Cancel)
            .unwrap();
        assert!(app.project_open_stage().is_none());
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_discard_failure_is_retained_as_the_exact_typed_error() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let project_id = sampler_core::ProjectId::from_bytes([0x8f; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        app.choose_project_recovery(crate::RecoveryChoice::Discard)
            .unwrap();
        app.take_worker_requests();
        let error = ProjectStoreError::Filesystem {
            operation: "discard recovery",
            path: "project-b/.sampler-tui-recovery.toml".into(),
            kind: std::io::ErrorKind::PermissionDenied,
        };

        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token,
            directory: "project-b".into(),
            project_id,
            revision: 6,
            result: Err(error.clone()),
        }));
        assert_eq!(
            app.project_open_error(),
            Some(&crate::ProjectOpenError::RecoveryDiscard(error))
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_admits_stop_pads_and_patterns_one_per_maintenance_then_commits() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let project_id = sampler_core::ProjectId::from_bytes([0x7a; 16]);
        let settings = PadSettings::new(PlaybackMode::Gate, -4.0, 0.25, 3.0, None).unwrap();
        let document = project_open_document(
            project_id,
            "Project B",
            12,
            vec![project_open_pad(pad(0, 1), settings)],
        );
        let old_snapshot = app.project_snapshot().unwrap();
        stage_project_open(&mut app, "project-b", document);

        app.maintain_audio();
        assert!(calls.snapshot().is_empty());
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(calls.snapshot(), [AudioCall::StopAll]);
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Admitting
        );
        app.apply(InputAction::PadPress(0));
        assert_eq!(calls.snapshot(), [AudioCall::StopAll]);
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);

        for offset in 0..super::PAD_VIEW_COUNT {
            let before = calls.snapshot().len();
            assert!(app.maintain_project(Instant::now()));
            let after = calls.snapshot();
            assert_eq!(after.len(), before + 1);
            let expected_pad = super::pad_from_offset(offset);
            if offset == 1 {
                assert_eq!(after.last(), Some(&AudioCall::Install(expected_pad)));
            } else {
                assert_eq!(after.last(), Some(&AudioCall::RemoveSample(expected_pad)));
            }
            assert!(app.project_open_stage().is_some());
            assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
        }

        for index in 0..sampler_core::PATTERN_SLOT_COUNT {
            let before = calls.snapshot().len();
            assert!(app.maintain_project(Instant::now()));
            let after = calls.snapshot();
            assert_eq!(after.len(), before + 1);
            assert_eq!(after.last(), Some(&AudioCall::InstallPattern));
            if index + 1 < sampler_core::PATTERN_SLOT_COUNT {
                assert!(app.project_open_stage().is_some());
                assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
            }
        }

        assert!(app.project_open_stage().is_none());
        assert!(app.overlay().is_none());
        assert_eq!(app.project_revision(), 12);
        assert_eq!(app.project_header(), "Project B · SAVED");
        assert_eq!(app.pad(pad(0, 1)).settings, settings);
        assert_eq!(app.pad(pad(0, 1)).state, PadLoadState::Ready);
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Empty);
    }

    #[test]
    fn project_open_admission_backpressure_retries_the_exact_same_pad_action() {
        let audio = FakeAudio::ready(48_000, 2).failing_install("command queue full");
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x7b; 16]),
            "Project B",
            13,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        stage_project_open(&mut app, "project-b", document);
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(calls.snapshot(), [AudioCall::StopAll]);

        assert!(!app.maintain_project(Instant::now()));
        assert_eq!(calls.snapshot(), [AudioCall::StopAll]);
        assert_eq!(app.project_open_stage().unwrap().admitted_actions, 1);
        assert!(app.status().contains("command queue full"));

        assert!(app.maintain_project(Instant::now()));
        assert_eq!(
            calls.snapshot(),
            [AudioCall::StopAll, AudioCall::Install(pad(0, 0))]
        );
        assert_eq!(app.project_open_stage().unwrap().admitted_actions, 2);
    }

    #[test]
    fn project_open_device_retry_restarts_admission_on_the_empty_engine() {
        let audio = FakeAudio::ready(48_000, 2).failing_runtime("device lost");
        let old_calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let old_snapshot = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x7c; 16]),
            "Project B",
            14,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        stage_project_open(&mut app, "project-b", document);
        assert!(app.maintain_project(Instant::now()));
        assert!(app.maintain_project(Instant::now()));
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(
            old_calls.snapshot(),
            [
                AudioCall::StopAll,
                AudioCall::Install(pad(0, 0)),
                AudioCall::RemoveSample(pad(0, 1)),
            ]
        );
        assert!(app.maintain_audio());
        assert_eq!(app.audio_format(), None);
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);

        let replacement = FakeAudio::ready(48_000, 2);
        let replacement_calls = replacement.call_log();
        assert!(app.retry_with(Box::new(replacement)));
        replacement_calls.clear();
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(replacement_calls.snapshot(), [AudioCall::StopAll]);
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(
            replacement_calls.snapshot(),
            [AudioCall::StopAll, AudioCall::Install(pad(0, 0))]
        );
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
    }

    #[test]
    fn project_open_restore_commits_recovery_as_modified_against_explicit_revision() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let project_id = sampler_core::ProjectId::from_bytes([0x7d; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        app.choose_project_recovery(crate::RecoveryChoice::Restore)
            .unwrap();
        while app.project_open_stage().is_some() {
            assert!(app.maintain_project(Instant::now()));
        }

        assert_eq!(app.project_revision(), 6);
        assert_eq!(app.project_session.saved_revision(), 4);
        assert_eq!(app.project_session.autosaved_revision(), 6);
        assert_eq!(app.project_header(), "Recovery · MODIFIED");
    }

    #[test]
    fn project_open_can_restore_valid_recovery_when_explicit_document_is_corrupt() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let project_id = sampler_core::ProjectId::from_bytes([0x7e; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Err(ProjectStoreError::DocumentInvalid {
                    path: "project-b/project.toml".into(),
                    message: "corrupt TOML".to_owned(),
                })),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    3,
                    Vec::new(),
                ))),
            }),
        }));
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::AwaitingRecoveryChoice
        );
        app.choose_project_recovery(crate::RecoveryChoice::Restore)
            .unwrap();
        assert_eq!(app.project_open_stage().unwrap().revision, Some(3));
    }

    #[test]
    fn project_save_refuses_untitled_dirty_draft_and_pending_operations() {
        let now = Instant::now();
        let mut untitled = project_app();
        assert_eq!(
            untitled.request_save(),
            Err(super::ProjectSaveError::Untitled)
        );

        let mut dirty = project_app();
        name_project(&mut dirty, "named", now);
        dirty.editor_mut_for_test().move_marker(1, false);
        assert!(matches!(
            dirty.request_save(),
            Err(super::ProjectSaveError::Snapshot(
                ProjectSnapshotError::DirtySampleDraft(_)
            ))
        ));

        let mut pending = project_app();
        name_project(&mut pending, "named", now);
        let _ = pending.begin_load(pad(0, 1), "pending.wav");
        assert!(matches!(
            pending.request_save(),
            Err(super::ProjectSaveError::Snapshot(
                ProjectSnapshotError::PendingSampleLoad(_)
            ))
        ));
    }

    #[test]
    fn save_as_reuses_generated_identity_after_an_error() {
        let now = Instant::now();
        let mut app = project_app();
        app.request_save_as("new-project").unwrap();
        assert!(app.maintain_project(now));
        let first = take_project_save(&mut app);
        assert_ne!(first.request.snapshot.project_id.as_bytes(), &[0; 16]);
        assert!(first.request.save_as);
        assert!(app.apply_worker_result(save_error(&first, "save-as")));

        app.request_save_as("new-project").unwrap();
        assert!(app.maintain_project(now + Duration::from_secs(1)));
        let retry = take_project_save(&mut app);
        assert_eq!(
            retry.request.snapshot.project_id,
            first.request.snapshot.project_id
        );
        assert_eq!(retry.request.directory, first.request.directory);
        assert!(retry.request.save_as);
    }

    #[test]
    fn project_save_accepts_only_exact_worker_and_receipt_identity() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        app.request_save().unwrap();
        app.maintain_project(now);
        let request = take_project_save(&mut app);
        let exact = save_result(&request, Vec::new());

        let WorkerResult::ProjectSaved {
            token,
            kind,
            project_id,
            directory,
            revision,
            result,
        } = exact
        else {
            unreachable!()
        };
        for stale in [
            WorkerResult::ProjectSaved {
                token: crate::ProjectToken::new(token.get() + 1),
                kind,
                project_id,
                directory: directory.clone(),
                revision,
                result: result.clone(),
            },
            WorkerResult::ProjectSaved {
                token,
                kind,
                project_id,
                directory: "other".into(),
                revision,
                result: result.clone(),
            },
            WorkerResult::ProjectSaved {
                token,
                kind,
                project_id,
                directory: directory.clone(),
                revision: revision + 1,
                result: result.clone(),
            },
        ] {
            assert!(!app.apply_worker_result(stale));
        }
        let mut wrong_receipt = result.unwrap();
        wrong_receipt.project_id = sampler_core::ProjectId::from_bytes([0x99; 16]);
        assert!(!app.apply_worker_result(WorkerResult::ProjectSaved {
            token,
            kind,
            project_id,
            directory: directory.clone(),
            revision,
            result: Ok(wrong_receipt),
        }));
        assert!(app.apply_worker_result(save_result(&request, Vec::new())));
        assert_eq!(app.project_session.saved_revision(), revision);
    }

    #[test]
    fn save_mapping_requires_exact_generation_and_fingerprint_before_path_adoption() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        let snapshot = app.project_snapshot().unwrap();
        let saved_pad = snapshot.pads[0].clone();
        let original = app.pad(saved_pad.pad).source.clone();
        app.request_save().unwrap();
        app.maintain_project(now);
        let request = take_project_save(&mut app);
        let stale_fingerprint = crate::SourceFingerprint::from_encoded_bytes(
            std::path::Path::new("other.wav"),
            b"other",
        )
        .unwrap();
        assert!(app.apply_worker_result(save_result(
            &request,
            vec![
                ProjectAssetMapping {
                    pad: saved_pad.pad,
                    source_generation: saved_pad.source_generation + 1,
                    fingerprint: saved_pad.fingerprint,
                    project_path: "named/audio/wrong-generation.wav".into(),
                },
                ProjectAssetMapping {
                    pad: saved_pad.pad,
                    source_generation: saved_pad.source_generation,
                    fingerprint: stale_fingerprint,
                    project_path: "named/audio/wrong-digest.wav".into(),
                },
            ],
        )));
        assert_eq!(app.pad(saved_pad.pad).source, original);
    }

    #[test]
    fn explicit_and_recovery_save_adopt_current_internal_paths_but_only_explicit_is_clean() {
        let now = Instant::now();
        let mut recovery = project_app();
        let project_id = name_project(&mut recovery, "named", now);
        recovery.maintain_project(now + Duration::from_secs(2));
        let autosave = take_project_save(&mut recovery);
        assert_eq!(autosave.request.kind, SaveKind::Recovery);
        let pad = autosave.request.snapshot.pads[0].clone();
        let internal = PathBuf::from("named/audio/internal.wav");
        assert!(recovery.apply_worker_result(save_result(
            &autosave,
            vec![ProjectAssetMapping {
                pad: pad.pad,
                source_generation: pad.source_generation,
                fingerprint: pad.fingerprint,
                project_path: internal.clone(),
            }],
        )));
        assert_eq!(
            recovery.pad(pad.pad).source.as_deref(),
            Some(internal.as_path())
        );
        assert_eq!(
            recovery.project_session.autosaved_revision(),
            recovery.project_revision()
        );
        assert_ne!(
            recovery.project_session.saved_revision(),
            recovery.project_revision()
        );
        assert_eq!(recovery.project_session.project_id(), project_id);

        recovery.request_save().unwrap();
        recovery.maintain_project(now + Duration::from_secs(3));
        let explicit = take_project_save(&mut recovery);
        assert!(recovery.apply_worker_result(save_result(&explicit, Vec::new())));
        assert_eq!(
            recovery.project_session.saved_revision(),
            recovery.project_revision()
        );
    }

    #[test]
    fn autosave_debounces_two_seconds_coalesces_when_busy_and_explicit_has_priority() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        assert!(!app.maintain_project(now + Duration::from_millis(1_999)));
        assert!(app.take_worker_requests().is_empty());

        for request_id in 0..crate::loader::WORKER_CHANNEL_CAPACITY as u64 {
            app.pending_worker_requests
                .push(WorkerRequest::ScanDirectory {
                    request_id,
                    path: PathBuf::from("."),
                    show_hidden: false,
                });
        }
        assert!(app.maintain_project(now + Duration::from_secs(2)));
        assert!(app.project_session.pending_autosave().is_some());
        app.pending_worker_requests.clear();
        app.project_session
            .commit_project_mutation(now + Duration::from_secs(3), || Ok::<(), ()>(()))
            .unwrap();
        app.request_save().unwrap();
        app.maintain_project(now + Duration::from_secs(5));
        let request = take_project_save(&mut app);
        assert_eq!(request.request.kind, SaveKind::Explicit);
        assert_eq!(request.request.snapshot.revision, app.project_revision());
    }

    #[test]
    fn autosave_replaces_a_busy_pending_snapshot_with_the_newest_quiet_revision() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        for request_id in 0..crate::loader::WORKER_CHANNEL_CAPACITY as u64 {
            app.pending_worker_requests
                .push(WorkerRequest::ScanDirectory {
                    request_id,
                    path: PathBuf::from("."),
                    show_hidden: false,
                });
        }
        app.maintain_project(now + Duration::from_secs(2));
        let first_revision = app.project_session.pending_autosave().unwrap().revision;

        app.project_session
            .commit_project_mutation(now + Duration::from_secs(3), || Ok::<(), ()>(()))
            .unwrap();
        app.maintain_project(now + Duration::from_secs(5));
        assert!(app.project_revision() > first_revision);
        assert_eq!(
            app.project_session.pending_autosave().unwrap().revision,
            app.project_revision()
        );

        app.pending_worker_requests.clear();
        app.maintain_project(now + Duration::from_secs(5));
        assert_eq!(
            take_project_save(&mut app).request.snapshot.revision,
            app.project_revision()
        );
    }

    #[test]
    fn autosave_withholds_a_stale_pending_snapshot_until_the_newest_revision_is_quiet() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        for request_id in 0..crate::loader::WORKER_CHANNEL_CAPACITY as u64 {
            app.pending_worker_requests
                .push(WorkerRequest::ScanDirectory {
                    request_id,
                    path: PathBuf::from("."),
                    show_hidden: false,
                });
        }
        app.maintain_project(now + Duration::from_secs(2));
        let stale_revision = app.project_session.pending_autosave().unwrap().revision;

        app.project_session
            .commit_project_mutation(now + Duration::from_secs(3), || Ok::<(), ()>(()))
            .unwrap();
        app.pending_worker_requests.clear();
        assert!(app.maintain_project(now + Duration::from_secs(4)));
        assert!(app.take_worker_requests().is_empty());
        assert!(app.project_session.pending_autosave().is_none());
        assert!(app.project_revision() > stale_revision);

        assert!(app.maintain_project(now + Duration::from_secs(5)));
        assert_eq!(
            take_project_save(&mut app).request.snapshot.revision,
            app.project_revision()
        );
    }

    #[test]
    fn explicit_save_cancels_covered_autosave_and_does_not_recreate_recovery_while_clean() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        for request_id in 0..crate::loader::WORKER_CHANNEL_CAPACITY as u64 {
            app.pending_worker_requests
                .push(WorkerRequest::ScanDirectory {
                    request_id,
                    path: PathBuf::from("."),
                    show_hidden: false,
                });
        }
        app.maintain_project(now + Duration::from_secs(2));
        assert!(app.project_session.pending_autosave().is_some());
        app.pending_worker_requests.clear();

        app.request_save().unwrap();
        app.maintain_project(now + Duration::from_secs(2));
        let explicit = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_result(&explicit, Vec::new())));
        assert_eq!(app.project_session.pending_autosave(), None);

        app.pending_recovery_cleanup.clear();
        assert!(!app.maintain_project(now + Duration::from_secs(20)));
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn autosave_error_retries_after_another_quiet_interval_and_untitled_never_autosaves() {
        let now = Instant::now();
        let mut untitled = project_app();
        assert!(!untitled.maintain_project(now + Duration::from_secs(20)));
        assert!(untitled.take_worker_requests().is_empty());

        let mut app = project_app();
        name_project(&mut app, "named", now);
        app.maintain_project(now + Duration::from_secs(2));
        let failed = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_error(&failed, "autosave")));
        assert!(app.project_save_error().is_some());
        app.apply(InputAction::PadPress(99));
        assert!(app.status().contains("outside 0..16"));
        assert!(app.project_header().contains("AUTOSAVE ERROR"));
        assert!(app.maintain_project(now + Duration::from_secs(2)));
        assert!(app.take_worker_requests().is_empty());
        assert!(!app.maintain_project(now + Duration::from_millis(3_999)));
        assert!(app.maintain_project(now + Duration::from_secs(4)));
        let retry = take_project_save(&mut app);
        assert_eq!(retry.request.kind, SaveKind::Recovery);
        assert_eq!(
            retry.request.snapshot.revision,
            failed.request.snapshot.revision
        );
    }

    #[test]
    fn autosave_error_retry_waits_for_two_seconds_after_a_newer_mutation() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        app.maintain_project(now + Duration::from_secs(2));
        let failed = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_error(&failed, "autosave")));
        app.maintain_project(now + Duration::from_secs(2));

        app.project_session
            .commit_project_mutation(now + Duration::from_secs(3), || Ok::<(), ()>(()))
            .unwrap();
        assert!(!app.maintain_project(now + Duration::from_secs(4)));
        assert!(app.take_worker_requests().is_empty());
        assert!(app.maintain_project(now + Duration::from_secs(5)));
        assert_eq!(
            take_project_save(&mut app).request.snapshot.revision,
            app.project_revision()
        );
    }

    #[test]
    fn explicit_save_cleanup_failure_is_a_warning_after_clean_truth() {
        let now = Instant::now();
        let mut app = project_app();
        let project_id = name_project(&mut app, "named", now);
        app.request_save().unwrap();
        app.maintain_project(now);
        let explicit = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_result(&explicit, Vec::new())));
        assert_eq!(app.project_session.saved_revision(), app.project_revision());

        app.maintain_project(now + Duration::from_secs(1));
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::DiscardRecovery {
                token,
                directory,
                project_id: cleanup_project_id,
                revision,
            },
        ] = requests.as_slice()
        else {
            panic!("expected recovery cleanup request");
        };
        assert_eq!(*cleanup_project_id, project_id);
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token: *token,
            directory: directory.clone(),
            project_id,
            revision: *revision,
            result: Err(ProjectStoreError::Filesystem {
                operation: "delete recovery",
                path: directory.clone(),
                kind: std::io::ErrorKind::PermissionDenied,
            }),
        }));
        assert!(app.recovery_cleanup_warning().is_some());
        assert_eq!(app.project_session.saved_revision(), app.project_revision());
    }

    #[test]
    fn recovery_cleanup_interleaving_preserves_fifo_order_and_busy_restores_the_exact_front() {
        let now = Instant::now();
        let mut app = project_app();
        let project_a = name_project(&mut app, "project-a", now);
        app.request_save().unwrap();
        app.maintain_project(now);
        let save_a = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_result(&save_a, Vec::new())));

        app.request_save_as("project-b").unwrap();
        app.maintain_project(now + Duration::from_secs(1));
        let save_b = take_project_save(&mut app);
        let project_b = save_b.request.snapshot.project_id;
        assert!(app.apply_worker_result(save_result(&save_b, Vec::new())));

        app.maintain_project(now + Duration::from_secs(2));
        let cleanup_a = take_recovery_cleanup(&mut app);
        assert_eq!(cleanup_a.directory, PathBuf::from("project-a"));
        assert_eq!(cleanup_a.project_id, project_a);
        assert!(app.apply_worker_send_error(
            WorkerRequest::DiscardRecovery {
                token: cleanup_a.token,
                directory: cleanup_a.directory.clone(),
                project_id: cleanup_a.project_id,
                revision: cleanup_a.revision,
            },
            WorkerSendError::WorkerBusy,
        ));

        app.maintain_project(now + Duration::from_secs(3));
        assert_eq!(take_recovery_cleanup(&mut app), cleanup_a);
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token: cleanup_a.token,
            directory: cleanup_a.directory.clone(),
            project_id: cleanup_a.project_id,
            revision: cleanup_a.revision,
            result: Ok(()),
        }));

        app.maintain_project(now + Duration::from_secs(4));
        let cleanup_b = take_recovery_cleanup(&mut app);
        assert_eq!(cleanup_b.directory, PathBuf::from("project-b"));
        assert_eq!(cleanup_b.project_id, project_b);
    }

    #[test]
    fn explicit_save_is_refused_when_the_bounded_cleanup_backlog_is_full() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        for second in 0..crate::loader::WORKER_CHANNEL_CAPACITY {
            app.request_save().unwrap();
            app.maintain_project(now + Duration::from_secs(second as u64));
            let save = take_project_save(&mut app);
            assert!(app.apply_worker_result(save_result(&save, Vec::new())));
        }

        assert_eq!(
            app.request_save(),
            Err(super::ProjectSaveError::OperationPending)
        );
    }
}
