use std::array;
use std::mem;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use sampler_audio::{SampleBuffer, Telemetry, TransportStamp};
use sampler_core::pad::{BANK_COUNT, PADS_PER_BANK};
use sampler_core::{BankId, PadId, PadSettings, PatternSlotId, PlaybackMode, SampleEditRecipe};

use crate::PatternSwitch;
use crate::audio::{AudioPort, open_default_audio};
use crate::file_picker::FilePicker;
use crate::input::{InputAction, KeyboardCapabilities, map_key};
use crate::loader::{
    MAX_DIRECTORY_ENTRIES, WORKER_CHANNEL_CAPACITY, WorkerRequest, WorkerResult, WorkerSendError,
};
use crate::palette::{LineEditor, PaletteCommand, parse_palette};
use crate::pattern::{PatternStatus, PatternWorkspace, WorkspaceView};

pub const PAD_VIEW_COUNT: usize = 160;
/// Fixed worker-generated waveform resolution. Perform uses a bounded 64-column projection.
pub const EDIT_PREVIEW_COLUMNS: usize = 1_024;
pub const PREVIEW_COLUMNS: usize = 64;

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

struct PendingLoad {
    path: PathBuf,
    phase: PendingLoadPhase,
    kind: PendingLoadKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Palette,
    FilePicker,
    DeviceError(String),
    ClearPattern {
        slot: PatternSlotId,
        event_count: usize,
    },
}

#[derive(Debug, Clone, Copy)]
struct PendingPatternTransport {
    playing: bool,
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
        }
    }

    pub fn apply(&mut self, action: InputAction) {
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
        if let Some(action) = map_key(key, self.keyboard_capabilities) {
            match action {
                InputAction::Quit | InputAction::StopAll | InputAction::PadRelease(_) => {
                    self.apply(action);
                    return;
                }
                InputAction::PadPress(_) | InputAction::PadStop(_) | InputAction::BankDelta(_) => {}
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
            self.close_overlay();
            return;
        }

        match self.overlay.as_ref() {
            Some(Overlay::DeviceError(_)) => self.apply_device_error_key(key),
            Some(Overlay::Palette) => self.apply_palette_key(key),
            Some(Overlay::FilePicker) => self.apply_picker_key(key),
            Some(Overlay::Help) => self.apply_help_key(key),
            Some(Overlay::ClearPattern { .. }) => self.apply_clear_pattern_key(key),
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

    pub fn recorded_ack_count(&self) -> usize {
        self.recorded_ack_count
    }

    pub fn maintain_audio_pattern_submissions(&self) -> usize {
        self.pattern_submission_count
    }

    pub fn meter_levels(&self) -> (f32, f32) {
        (self.meter_left, self.meter_right)
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
        let Some(audio) = self.audio.as_mut() else {
            return false;
        };
        audio.reclaim_retired();
        let runtime_error = audio.poll_runtime_error();

        if let Some(error) = runtime_error {
            self.fail_audio(error);
            true
        } else {
            let mut changed = self.pump_recovery_requests();
            let telemetry = self
                .audio
                .as_mut()
                .and_then(|audio| audio.latest_telemetry());
            if let Some(telemetry) = telemetry {
                changed |= self.apply_telemetry(telemetry);
            }
            let maintenance = {
                let audio = self
                    .audio
                    .as_mut()
                    .expect("audio remains present after a successful poll");
                self.patterns.maintain(audio.as_mut(), self.telemetry)
            };
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

    pub fn pad(&self, pad: PadId) -> &PadView {
        &self.pads[pad_offset(pad)]
    }

    /// Atomically updates a pad's validated settings. Unloaded pads remain a local edit; loaded
    /// pads commit only after audio accepts the corresponding update.
    pub fn update_pad_settings(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        settings.validate().map_err(|error| error.to_string())?;
        let offset = pad_offset(pad);
        let bound_in_current_session = self.current_session_bound[offset];
        if bound_in_current_session && let Some(audio) = self.audio.as_mut() {
            audio.update_pad(pad, settings)?;
        }
        self.pads[offset].settings = settings;
        Ok(())
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
        if self.overlay == Some(Overlay::Palette) {
            self.palette_error = None;
        }
        if let Some(Overlay::DeviceError(error)) = &self.overlay {
            self.status = format!("{error} · Ctrl+R retries audio");
        }
        self.overlay = None;
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
        let message = error.to_string();
        let applied = match request {
            WorkerRequest::LoadSample {
                pad,
                generation,
                path,
                ..
            } => {
                let offset = pad_offset(pad);
                if let Some(kind) = self.matching_pending_load(offset, generation, &path) {
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
            WorkerRequest::EditSample { .. }
            | WorkerRequest::ScanDirectory { .. }
            | WorkerRequest::Shutdown => false,
        };
        if applied {
            self.status = message;
        }
        applied
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
        let path = path.into();
        let engine_rate = self.audio.as_ref().map(|audio| audio.sample_rate());
        let offset = pad_offset(pad);
        let view = &mut self.pads[offset];
        view.generation = view.generation.wrapping_add(1);
        view.state = if engine_rate.is_some() {
            PadLoadState::Loading
        } else {
            PadLoadState::WaitingForDevice
        };
        let generation = view.generation;

        if let Some(engine_rate) = engine_rate {
            self.pending_loads[offset] = Some(Box::new(PendingLoad {
                path: path.clone(),
                phase: PendingLoadPhase::WorkerQueued,
                kind: PendingLoadKind::User,
            }));
            Some(WorkerRequest::LoadSample {
                pad,
                generation,
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
        }
    }

    pub fn apply_worker_result(&mut self, result: WorkerResult) -> bool {
        let WorkerResult::Loaded {
            pad,
            generation,
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
        let Some(kind) = self.matching_pending_load(offset, generation, &path) else {
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
            return true;
        }
        self.install_pending_load(offset, kind);
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
        self.selected_pad = index;
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
                self.active_bank = bank;
                self.overlay = None;
            }
            PaletteCommand::Select(index) => {
                self.selected_pad = index;
                self.overlay = None;
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
        }
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
        }
    }

    fn apply_global_pattern_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Tab if key.modifiers == KeyModifiers::NONE => {
                self.patterns.toggle_view();
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
        self.selected_pad = row * 4 + column;
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
        if let Err(error) = edit(&mut self.patterns) {
            self.status = error.to_string();
        } else {
            self.overlay = None;
        }
    }

    fn selected_pad_id(&self) -> Option<PadId> {
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
        self.selected_pad = index;
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
        let value = u8::try_from(requested).expect("bounded bank fits in u8");
        self.active_bank = BankId::new(value).expect("bounded bank is valid");
        if self.patterns.view() == WorkspaceView::Pattern {
            let cursor = self.patterns.cursor();
            let pad = PadId::new(self.active_bank, cursor.pad().index())
                .expect("existing cursor index is valid");
            self.patterns.move_cursor_to(pad, cursor.step());
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
        self.audio_unavailable_message = Some(error.clone());
        self.held_pad_by_key.fill(None);
        self.patterns.stop_recording();
        self.pending_pattern_transport = None;
        for pad in &mut self.pads {
            pad.active = false;
        }
        self.status = error.clone();
        self.overlay = Some(Overlay::DeviceError(error));
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
        path: &Path,
    ) -> Option<PendingLoadKind> {
        [PendingLoadKind::User, PendingLoadKind::Recovery]
            .into_iter()
            .find(|kind| {
                let expected_generation = match kind {
                    PendingLoadKind::User => self.pads[offset].generation,
                    PendingLoadKind::Recovery => self.recovery_generations[offset],
                };
                expected_generation == generation
                    && self
                        .pending_load_slot(offset, *kind)
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.path == path
                                && matches!(pending.phase, PendingLoadPhase::WorkerQueued)
                        })
            })
    }

    fn install_pending_load(&mut self, offset: usize, kind: PendingLoadKind) {
        let Some(mut pending) = self.pending_load_slot_mut(offset, kind).take() else {
            return;
        };
        let PendingLoadPhase::Ready(loaded) = pending.phase else {
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            return;
        };
        let Some(audio) = self.audio.as_mut() else {
            pending.phase = PendingLoadPhase::Ready(loaded);
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            return;
        };
        if loaded.rendered.sample_rate() != audio.sample_rate() {
            pending.phase = PendingLoadPhase::AwaitingWorker;
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::Loading;
            self.recovery_cursor = Some(offset);
            return;
        }

        let pad = pad_from_offset(offset);
        let settings = self.pads[offset].settings;
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
        view.sample = Some(loaded.rendered);
        view.preview = crate::loader::downsample_preview(&loaded.preview);
        view.state = PadLoadState::Ready;
        self.reinstall_pending[offset] = false;
        self.current_session_bound[offset] = true;
        if kind == PendingLoadKind::User {
            self.committed_recovery_loads[offset] = None;
        }
        let action = if kind == PendingLoadKind::Recovery {
            "Recovered"
        } else {
            "Loaded"
        };
        self.status = format!("{action} {}", label.to_uppercase());
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
    use std::rc::Rc;
    use std::sync::Arc;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use sampler_audio::{
        AudioController, EnginePorts, Frame, LiveAck, LiveCommandId, PatternSnapshotSlot,
        PatternSwitch, SampleBuffer, SampleSlot, Telemetry, audio_channels,
        audio_channels_with_test_capacities,
    };
    use sampler_core::{
        BankId, PadId, PadSettings, PatternSlotId, PatternSnapshot, PlaybackMode, SampleEditRecipe,
    };

    use crate::audio::AudioPort;
    use crate::input::InputAction;

    use crate::DirectoryScan;
    use crate::loader::{
        LoadSampleError, LoadedSample, WorkerRequest, WorkerResult, WorkerSendError,
    };

    use super::{App, EDIT_PREVIEW_COLUMNS, PadLoadState, PreviewColumn};

    #[derive(Debug, Clone, PartialEq)]
    enum AudioCall {
        Install(PadId),
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
        calls: CallLog,
        maintenance: Rc<RefCell<Vec<&'static str>>>,
        runtime_error: Option<String>,
        shutdown: Option<Rc<RefCell<Vec<&'static str>>>>,
        pattern_controller: AudioController,
        _pattern_ports: EnginePorts,
        drain_pattern_queue_after_backpressure: bool,
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
                calls: CallLog(Rc::new(RefCell::new(Vec::new()))),
                maintenance: Rc::new(RefCell::new(Vec::new())),
                runtime_error: None,
                shutdown: None,
                pattern_controller,
                _pattern_ports: pattern_ports,
                drain_pattern_queue_after_backpressure: false,
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
                calls: CallLog(Rc::new(RefCell::new(Vec::new()))),
                maintenance: Rc::new(RefCell::new(Vec::new())),
                runtime_error: None,
                shutdown: None,
                pattern_controller,
                _pattern_ports: pattern_ports,
                drain_pattern_queue_after_backpressure: false,
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

        fn failing_runtime(mut self, error: &str) -> Self {
            self.runtime_error = Some(error.to_owned());
            self
        }

        fn with_shutdown_log(mut self, shutdown: Rc<RefCell<Vec<&'static str>>>) -> Self {
            self.shutdown = Some(shutdown);
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

        fn drain_live_acks(&mut self, _output: &mut [LiveAck]) -> usize {
            0
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
        assert!(!app.apply_worker_result(loaded_at_rate(
            pad(0, 0),
            stale_generation,
            "kick.wav",
            44_100,
        )));
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
                    path,
                    result: Err(LoadSampleError::Decode("unreadable early pad".to_owned())),
                });
            } else {
                completed.push(pad_id);
                app.apply_worker_result(loaded_at_rate(
                    pad_id,
                    generation,
                    path.to_str().unwrap(),
                    44_100,
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
            path,
            ..
        } = request
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad_id,
            generation,
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
        loaded_at_rate(pad, generation, source, 48_000)
    }

    fn loaded_at_rate(pad: PadId, generation: u64, source: &str, sample_rate: u32) -> WorkerResult {
        let rendered = Arc::new(SampleBuffer::new(sample_rate, vec![0.25, -0.25]).unwrap());
        WorkerResult::Loaded {
            pad,
            generation,
            path: source.into(),
            result: Ok(LoadedSample {
                base: Arc::clone(&rendered),
                rendered,
                recipe: SampleEditRecipe::identity(),
                source_rate: sample_rate,
                source_frames: 1,
                duration: std::time::Duration::from_secs_f64(1.0 / f64::from(sample_rate)),
                preview: Arc::new([PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]),
            }),
        }
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
            path: recovery_path,
            ..
        } = recovery
        else {
            panic!("wrong request")
        };
        assert_eq!(recovery_path, path("old.wav"));
        app.apply_worker_result(loaded_at_rate(pad(0, 0), generation, "old.wav", 44_100));
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
    fn help_and_picker_keys_do_not_fall_through_to_pads() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.open_help();
        app.apply_key(key('q', KeyModifiers::NONE, KeyEventKind::Press));
        app.close_overlay();
        app.open_picker();
        app.apply_key(key('q', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(calls.snapshot().is_empty());
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
        assert_eq!(app.palette_error(), None);
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
}
