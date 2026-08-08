use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sampler_audio::{
    DecodeLimits, SampleBuffer, decode_bytes_with_limits, prepare_sample_with_frame_limit,
};
use sampler_core::{PadId, ProjectId, SampleEditRecipe, apply_sample_edit};

use crate::app::{EDIT_PREVIEW_COLUMNS, PREVIEW_COLUMNS, PreviewColumn};
use crate::file_picker::{DirectoryEntry, DirectoryEntryKind, DirectoryScan, supported_audio_path};
use crate::project_store::{
    ProjectProbe, ProjectSaveRequest, ProjectStore, ProjectStoreError, SaveKind, SaveReceipt,
    SourceFingerprint,
};

pub(crate) const WORKER_CHANNEL_CAPACITY: usize = 8;
pub const MAX_DIRECTORY_ENTRIES: usize = 4_096;
pub const MAX_ENCODED_FILE_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_DECODED_FRAMES: usize = 8_388_608;
pub const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PREPARED_FRAMES: usize = 8_388_608;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPurpose {
    User,
    Recovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectToken(u64);

impl ProjectToken {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StageProjectSampleRequest {
    pub token: ProjectToken,
    pub pad: PadId,
    pub revision: u64,
    pub path: PathBuf,
    pub engine_rate: u32,
    pub recipe: SampleEditRecipe,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerRequest {
    ScanDirectory {
        request_id: u64,
        path: PathBuf,
        show_hidden: bool,
    },
    LoadSample {
        pad: PadId,
        generation: u64,
        purpose: LoadPurpose,
        path: PathBuf,
        engine_rate: u32,
        recipe: SampleEditRecipe,
    },
    EditSample {
        pad: PadId,
        generation: u64,
        base: Arc<SampleBuffer>,
        base_preview: EditPreview,
        recipe: SampleEditRecipe,
    },
    SaveProject(Box<ProjectSaveRequest>),
    ProbeProject {
        token: ProjectToken,
        directory: PathBuf,
    },
    DiscardRecovery {
        token: ProjectToken,
        directory: PathBuf,
        project_id: ProjectId,
        revision: u64,
    },
    StageProjectSample(Box<StageProjectSampleRequest>),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerResult {
    Scanned {
        request_id: u64,
        path: PathBuf,
        result: Result<DirectoryScan, String>,
    },
    Loaded {
        pad: PadId,
        generation: u64,
        purpose: LoadPurpose,
        path: PathBuf,
        result: Result<LoadedSample, LoadSampleError>,
    },
    Edited {
        pad: PadId,
        generation: u64,
        recipe: SampleEditRecipe,
        result: Result<RenderedSample, String>,
    },
    ProjectSaved {
        kind: SaveKind,
        revision: u64,
        result: Result<SaveReceipt, ProjectStoreError>,
    },
    ProjectProbed {
        token: ProjectToken,
        directory: PathBuf,
        result: Result<ProjectProbe, ProjectStoreError>,
    },
    RecoveryDiscarded {
        token: ProjectToken,
        directory: PathBuf,
        project_id: ProjectId,
        revision: u64,
        result: Result<(), ProjectStoreError>,
    },
    ProjectSampleStaged {
        token: ProjectToken,
        pad: PadId,
        revision: u64,
        path: PathBuf,
        recipe: SampleEditRecipe,
        result: Result<LoadedSample, LoadSampleError>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSample {
    pub fingerprint: SourceFingerprint,
    pub base: Arc<SampleBuffer>,
    /// Fixed preview of the immutable base PCM used by the Sample editor.
    pub base_preview: EditPreview,
    pub rendered: Arc<SampleBuffer>,
    /// Fixed preview of rendered playback PCM used to derive the Perform waveform.
    pub rendered_preview: EditPreview,
    pub recipe: SampleEditRecipe,
    pub source_rate: u32,
    pub source_frames: usize,
    pub duration: Duration,
}

/// Fixed-resolution waveform data shared by a worker result and the owning pad tuple.
pub type EditPreview = Arc<[PreviewColumn; EDIT_PREVIEW_COLUMNS]>;

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedSample {
    /// Preview paired with the immutable base owner in this result tuple.
    pub base_preview: EditPreview,
    pub rendered: Arc<SampleBuffer>,
    /// Fixed preview of rendered playback PCM paired with `base_preview`.
    pub rendered_preview: EditPreview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadSampleError {
    Metadata(String),
    EncodedFileTooLarge { bytes: u64, max_bytes: u64 },
    Fingerprint(String),
    Decode(String),
    Prepare(String),
}

impl fmt::Display for LoadSampleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata(error) => write!(formatter, "could not inspect sample payload: {error}"),
            Self::EncodedFileTooLarge { bytes, max_bytes } => write!(
                formatter,
                "encoded sample payload {bytes} bytes exceeds the {max_bytes}-byte encoded input limit"
            ),
            Self::Fingerprint(error) => formatter.write_str(error),
            Self::Decode(error) => formatter.write_str(error),
            Self::Prepare(error) => formatter.write_str(error),
        }
    }
}

impl Error for LoadSampleError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerSendError {
    WorkerBusy,
    WorkerClosed,
}

impl fmt::Display for WorkerSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WorkerBusy => "loader busy",
            Self::WorkerClosed => "loader closed",
        })
    }
}

impl Error for WorkerSendError {}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkerSendFailure {
    kind: WorkerSendError,
    request: WorkerRequest,
}

impl WorkerSendFailure {
    pub fn new(kind: WorkerSendError, request: WorkerRequest) -> Self {
        Self { kind, request }
    }

    pub const fn kind(&self) -> WorkerSendError {
        self.kind
    }

    pub fn request(&self) -> &WorkerRequest {
        &self.request
    }

    pub fn into_request(self) -> WorkerRequest {
        self.request
    }
}

impl fmt::Display for WorkerSendFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(formatter)
    }
}

impl Error for WorkerSendFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerPanicked;

impl fmt::Display for WorkerPanicked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("loader worker panicked")
    }
}

impl Error for WorkerPanicked {}

pub struct WorkerHandle {
    requests: Option<SyncSender<WorkerRequest>>,
    results: Receiver<WorkerResult>,
    worker: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn spawn() -> Self {
        let (requests, request_receiver) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
        let (result_sender, results) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
        let worker = thread::Builder::new()
            .name("sampler-loader".to_owned())
            .spawn(move || worker_loop(request_receiver, result_sender))
            .expect("loader worker thread can be spawned");
        Self {
            requests: Some(requests),
            results,
            worker: Some(worker),
        }
    }

    pub fn try_send(&self, request: WorkerRequest) -> Result<(), WorkerSendFailure> {
        try_send_request(self.requests.as_ref(), request)
    }

    pub fn try_recv(&self) -> Result<WorkerResult, mpsc::TryRecvError> {
        self.results.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<WorkerResult, RecvTimeoutError> {
        self.results.recv_timeout(timeout)
    }

    pub fn request_shutdown(&mut self) {
        if let Some(sender) = self.requests.take() {
            let _ = sender.try_send(WorkerRequest::Shutdown);
            drop(sender);
        }
    }

    pub fn join(&mut self) -> Result<(), WorkerPanicked> {
        if let Some(worker) = self.worker.take() {
            loop {
                match self.results.recv_timeout(Duration::from_millis(10)) {
                    Ok(_) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) if worker.is_finished() => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
            worker.join().map_err(|_| WorkerPanicked)?;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), WorkerPanicked> {
        self.request_shutdown();
        self.join()
    }
}

fn try_send_request(
    sender: Option<&SyncSender<WorkerRequest>>,
    request: WorkerRequest,
) -> Result<(), WorkerSendFailure> {
    let Some(sender) = sender else {
        return Err(WorkerSendFailure::new(
            WorkerSendError::WorkerClosed,
            request,
        ));
    };
    sender.try_send(request).map_err(|error| match error {
        TrySendError::Full(request) => WorkerSendFailure::new(WorkerSendError::WorkerBusy, request),
        TrySendError::Disconnected(request) => {
            WorkerSendFailure::new(WorkerSendError::WorkerClosed, request)
        }
    })
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn worker_loop(requests: Receiver<WorkerRequest>, results: SyncSender<WorkerResult>) {
    worker_loop_with_store(requests, results, Box::new(ProjectStore));
}

trait ProjectStoreBackend: Send {
    fn save(&self, request: ProjectSaveRequest) -> Result<SaveReceipt, ProjectStoreError>;
    fn probe(&self, directory: &Path) -> Result<ProjectProbe, ProjectStoreError>;
    fn discard_recovery(
        &self,
        directory: &Path,
        project_id: ProjectId,
        revision: u64,
    ) -> Result<(), ProjectStoreError>;
}

impl ProjectStoreBackend for ProjectStore {
    fn save(&self, request: ProjectSaveRequest) -> Result<SaveReceipt, ProjectStoreError> {
        ProjectStore::save(self, request)
    }

    fn probe(&self, directory: &Path) -> Result<ProjectProbe, ProjectStoreError> {
        ProjectStore::probe(self, directory)
    }

    fn discard_recovery(
        &self,
        directory: &Path,
        project_id: ProjectId,
        revision: u64,
    ) -> Result<(), ProjectStoreError> {
        ProjectStore::discard_recovery(self, directory, project_id, revision)
    }
}

fn worker_loop_with_store(
    requests: Receiver<WorkerRequest>,
    results: SyncSender<WorkerResult>,
    store: Box<dyn ProjectStoreBackend>,
) {
    while let Ok(request) = requests.recv() {
        let result = match request {
            WorkerRequest::ScanDirectory {
                request_id,
                path,
                show_hidden,
            } => WorkerResult::Scanned {
                request_id,
                result: scan_directory(&path, show_hidden),
                path,
            },
            WorkerRequest::LoadSample {
                pad,
                generation,
                purpose,
                path,
                engine_rate,
                recipe,
            } => WorkerResult::Loaded {
                pad,
                generation,
                purpose,
                result: load_sample(&path, engine_rate, recipe),
                path,
            },
            WorkerRequest::EditSample {
                pad,
                generation,
                base,
                base_preview,
                recipe,
            } => WorkerResult::Edited {
                pad,
                generation,
                recipe,
                result: render_sample_edit(&base, base_preview, recipe),
            },
            WorkerRequest::SaveProject(request) => {
                let kind = request.kind;
                let revision = request.snapshot.revision;
                WorkerResult::ProjectSaved {
                    kind,
                    revision,
                    result: store.save(*request),
                }
            }
            WorkerRequest::ProbeProject { token, directory } => WorkerResult::ProjectProbed {
                token,
                result: store.probe(&directory),
                directory,
            },
            WorkerRequest::DiscardRecovery {
                token,
                directory,
                project_id,
                revision,
            } => WorkerResult::RecoveryDiscarded {
                token,
                result: store.discard_recovery(&directory, project_id, revision),
                directory,
                project_id,
                revision,
            },
            WorkerRequest::StageProjectSample(request) => {
                let StageProjectSampleRequest {
                    token,
                    pad,
                    revision,
                    path,
                    engine_rate,
                    recipe,
                } = *request;
                WorkerResult::ProjectSampleStaged {
                    token,
                    pad,
                    revision,
                    result: load_sample(&path, engine_rate, recipe),
                    path,
                    recipe,
                }
            }
            WorkerRequest::Shutdown => break,
        };
        if results.send(result).is_err() {
            break;
        }
    }
}

fn scan_directory(path: &Path, show_hidden: bool) -> Result<DirectoryScan, String> {
    let reader = fs::read_dir(path).map_err(|error| format_error(&error))?;
    let mut entries = BTreeSet::new();
    let mut truncated = false;
    for item in reader {
        let item = item.map_err(|error| format_error(&error))?;
        if !show_hidden && hidden_name(&item.file_name()) {
            continue;
        }
        let file_type = item.file_type().map_err(|error| format_error(&error))?;
        let kind = if file_type.is_dir() {
            DirectoryEntryKind::Directory
        } else if file_type.is_file() {
            DirectoryEntryKind::File
        } else if file_type.is_symlink() {
            DirectoryEntryKind::Symlink
        } else {
            continue;
        };
        let entry = DirectoryEntry {
            path: item.path(),
            kind,
        };
        if kind == DirectoryEntryKind::File && !supported_audio_path(&entry.path) {
            continue;
        }
        entries.insert(entry);
        if entries.len() > MAX_DIRECTORY_ENTRIES {
            entries.pop_last();
            truncated = true;
        }
    }
    Ok(DirectoryScan::new(entries.into_iter().collect(), truncated))
}

fn hidden_name(name: &std::ffi::OsStr) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().first() == Some(&b'.')
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().starts_with('.')
    }
}

fn load_sample(
    path: &Path,
    engine_rate: u32,
    recipe: SampleEditRecipe,
) -> Result<LoadedSample, LoadSampleError> {
    let file =
        fs::File::open(path).map_err(|error| LoadSampleError::Metadata(format_error(&error)))?;
    let mut encoded = Vec::new();
    file.take(MAX_ENCODED_FILE_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|error| LoadSampleError::Metadata(format_error(&error)))?;
    let encoded_bytes = encoded.len() as u64;
    if encoded_bytes > MAX_ENCODED_FILE_BYTES {
        return Err(LoadSampleError::EncodedFileTooLarge {
            bytes: encoded_bytes,
            max_bytes: MAX_ENCODED_FILE_BYTES,
        });
    }
    let fingerprint = SourceFingerprint::from_encoded_bytes(path, &encoded)
        .map_err(|error| LoadSampleError::Fingerprint(error.to_string()))?;
    let decoded = decode_bytes_with_limits(
        path,
        encoded,
        DecodeLimits {
            max_frames: MAX_DECODED_FRAMES,
            max_bytes: MAX_DECODED_BYTES,
        },
    )
    .map_err(|error| LoadSampleError::Decode(format_error(&error)))?;
    let source_rate = decoded.sample_rate;
    let source_frames = decoded.frames();
    let duration = source_duration(source_frames, source_rate);
    let base = Arc::new(
        prepare_sample_with_frame_limit(decoded, engine_rate, MAX_PREPARED_FRAMES)
            .map_err(|error| LoadSampleError::Prepare(format_error(&error)))?,
    );
    let base_preview = build_preview(&base);
    let rendered =
        render_sample_edit(&base, base_preview, recipe).map_err(LoadSampleError::Prepare)?;
    Ok(LoadedSample {
        fingerprint,
        base,
        base_preview: rendered.base_preview,
        rendered: rendered.rendered,
        rendered_preview: rendered.rendered_preview,
        recipe,
        source_rate,
        source_frames,
        duration,
    })
}

fn render_sample_edit(
    base: &Arc<SampleBuffer>,
    base_preview: EditPreview,
    recipe: SampleEditRecipe,
) -> Result<RenderedSample, String> {
    recipe.validate().map_err(|error| error.to_string())?;
    if recipe == SampleEditRecipe::identity() {
        return Ok(RenderedSample {
            rendered_preview: Arc::clone(&base_preview),
            base_preview,
            rendered: Arc::clone(base),
        });
    }
    let plan = apply_sample_edit(base.sample_rate(), base.data(), recipe)
        .map_err(|error| error.to_string())?;
    let rendered = Arc::new(
        SampleBuffer::new(plan.sample_rate(), plan.into_stereo())
            .map_err(|error| error.to_string())?,
    );
    Ok(RenderedSample {
        base_preview,
        rendered_preview: build_preview(&rendered),
        rendered,
    })
}

fn source_duration(frames: usize, sample_rate: u32) -> Duration {
    frame_duration(frames as u128, sample_rate)
}

fn frame_duration(frames: u128, sample_rate: u32) -> Duration {
    if sample_rate == 0 {
        return Duration::ZERO;
    }
    let rate = u128::from(sample_rate);
    let seconds = frames / rate;
    let remainder = frames % rate;
    match seconds.cmp(&u128::from(u64::MAX)) {
        std::cmp::Ordering::Greater => Duration::MAX,
        std::cmp::Ordering::Equal if remainder > 0 => Duration::MAX,
        std::cmp::Ordering::Equal => Duration::from_secs(u64::MAX),
        std::cmp::Ordering::Less => {
            Duration::new(seconds as u64, (remainder * 1_000_000_000 / rate) as u32)
        }
    }
}

fn build_preview(buffer: &SampleBuffer) -> EditPreview {
    Arc::new(std::array::from_fn(|column| {
        let Some((start, end)) = preview_bin_bounds(buffer.frames(), EDIT_PREVIEW_COLUMNS, column)
        else {
            return PreviewColumn::default();
        };
        if start == end {
            return PreviewColumn::default();
        }
        preview_column(&buffer.data()[start * 2..end * 2])
    }))
}

/// Returns half-open, gap-free frame bounds using checked wide arithmetic.
fn preview_bin_bounds(frames: usize, columns: usize, column: usize) -> Option<(usize, usize)> {
    if columns == 0 || column >= columns {
        return None;
    }
    let frames = u128::try_from(frames).ok()?;
    let columns = u128::try_from(columns).ok()?;
    let start = u128::try_from(column)
        .ok()?
        .checked_mul(frames)?
        .div_ceil(columns);
    let end = u128::try_from(column.checked_add(1)?)
        .ok()?
        .checked_mul(frames)?
        .div_ceil(columns);
    Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?))
}

/// Reduces the fixed edit preview to the existing bounded Perform waveform width.
pub fn downsample_preview(preview: &EditPreview) -> [PreviewColumn; PREVIEW_COLUMNS] {
    std::array::from_fn(|column| {
        let Some((start, end)) = preview_bin_bounds(EDIT_PREVIEW_COLUMNS, PREVIEW_COLUMNS, column)
        else {
            return PreviewColumn::default();
        };
        let Some((&first, rest)) = preview[start..end].split_first() else {
            return PreviewColumn::default();
        };
        rest.iter()
            .copied()
            .fold(first, |combined, item| PreviewColumn {
                min: combined.min.min(item.min),
                max: combined.max.max(item.max),
            })
    })
}

fn preview_column(samples: &[f32]) -> PreviewColumn {
    let mut min = 1.0_f32;
    let mut max = -1.0_f32;
    let mut found_finite = false;
    for sample in samples {
        if sample.is_finite() {
            found_finite = true;
            min = min.min(*sample);
            max = max.max(*sample);
        }
    }
    if !found_finite {
        return PreviewColumn::default();
    }
    PreviewColumn {
        min: preview_level(min),
        max: preview_level(max),
    }
}

fn preview_level(sample: f32) -> i8 {
    if !sample.is_finite() {
        return 0;
    }
    let scaled = sample.clamp(-1.0, 1.0) * 8.0;
    if scaled.is_sign_positive() {
        scaled.ceil() as i8
    } else {
        scaled.floor() as i8
    }
}

fn format_error(error: &dyn Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use sampler_audio::SampleBuffer;
    use sampler_core::{BankId, PadId, ProjectId, SampleEditRecipe};

    use super::{
        EDIT_PREVIEW_COLUMNS, LoadPurpose, MAX_DIRECTORY_ENTRIES, MAX_ENCODED_FILE_BYTES,
        ProjectStoreBackend, ProjectToken, StageProjectSampleRequest, WORKER_CHANNEL_CAPACITY,
        WorkerHandle, WorkerPanicked, WorkerRequest, WorkerResult, WorkerSendError, build_preview,
        downsample_preview, frame_duration, load_sample, preview_column, scan_directory,
        try_send_request, worker_loop, worker_loop_with_store,
    };
    use crate::{
        DirectoryEntry, ProjectProbe, ProjectSaveRequest, ProjectSaveSnapshot, ProjectStoreError,
        SaveKind, SaveReceipt, SourceFingerprint,
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct WavFixture(PathBuf);

    struct DirectoryFixture(PathBuf);

    impl DirectoryFixture {
        fn new(label: &str) -> Self {
            let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sampler-tui-loader-{label}-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for DirectoryFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    impl WavFixture {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for WavFixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn wav_fixture(sample_rate: u32, samples: &[i16]) -> WavFixture {
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sampler-tui-loader-{}-{serial}.wav",
            std::process::id()
        ));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for sample in samples {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();
        WavFixture(path)
    }

    fn pad(bank: u8, index: u8) -> PadId {
        PadId::new(BankId::new(bank).unwrap(), index).unwrap()
    }

    fn worker_with_capacities(request_capacity: usize, result_capacity: usize) -> WorkerHandle {
        let (requests, request_receiver) = mpsc::sync_channel(request_capacity);
        let (result_sender, results) = mpsc::sync_channel(result_capacity);
        let worker = thread::spawn(move || worker_loop(request_receiver, result_sender));
        WorkerHandle {
            requests: Some(requests),
            results,
            worker: Some(worker),
        }
    }

    fn worker_with_store(store: Box<dyn ProjectStoreBackend>) -> WorkerHandle {
        let (requests, request_receiver) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
        let (result_sender, results) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
        let worker = thread::Builder::new()
            .name("sampler-loader".to_owned())
            .spawn(move || worker_loop_with_store(request_receiver, result_sender, store))
            .unwrap();
        WorkerHandle {
            requests: Some(requests),
            results,
            worker: Some(worker),
        }
    }

    fn panicked_worker() -> WorkerHandle {
        let (requests, request_receiver) = mpsc::sync_channel(8);
        let (result_sender, results) = mpsc::sync_channel(8);
        drop(request_receiver);
        drop(result_sender);
        let worker = thread::spawn(|| panic!("injected loader panic"));
        WorkerHandle {
            requests: Some(requests),
            results,
            worker: Some(worker),
        }
    }

    fn empty_save_request(kind: SaveKind, revision: u64) -> ProjectSaveRequest {
        ProjectSaveRequest {
            directory: PathBuf::from(format!("project-{revision}")),
            save_as: false,
            kind,
            snapshot: ProjectSaveSnapshot {
                project_id: ProjectId::from_bytes([revision as u8; 16]),
                name: format!("project-{revision}"),
                revision,
                pads: Vec::new(),
                patterns: Vec::new(),
            },
        }
    }

    struct ScriptedProjectStore;

    impl ProjectStoreBackend for ScriptedProjectStore {
        fn save(&self, request: ProjectSaveRequest) -> Result<SaveReceipt, ProjectStoreError> {
            assert_eq!(thread::current().name(), Some("sampler-loader"));
            if request.kind == SaveKind::Recovery {
                return Err(ProjectStoreError::Filesystem {
                    operation: "scripted save",
                    path: request.directory,
                    kind: std::io::ErrorKind::PermissionDenied,
                });
            }
            Ok(SaveReceipt {
                directory: request.directory,
                kind: request.kind,
                project_id: request.snapshot.project_id,
                revision: request.snapshot.revision,
                canonical_toml: "scripted".to_owned(),
                mappings: Vec::new(),
            })
        }

        fn probe(&self, directory: &Path) -> Result<ProjectProbe, ProjectStoreError> {
            assert_eq!(thread::current().name(), Some("sampler-loader"));
            if directory.ends_with("bad-probe") {
                return Err(ProjectStoreError::Filesystem {
                    operation: "scripted probe",
                    path: directory.to_owned(),
                    kind: std::io::ErrorKind::NotFound,
                });
            }
            Ok(ProjectProbe {
                directory: directory.to_owned(),
                explicit: None,
                recovery: None,
            })
        }

        fn discard_recovery(
            &self,
            directory: &Path,
            _project_id: ProjectId,
            revision: u64,
        ) -> Result<(), ProjectStoreError> {
            assert_eq!(thread::current().name(), Some("sampler-loader"));
            if revision == 404 {
                return Err(ProjectStoreError::RecoveryMismatch {
                    path: directory.to_owned(),
                });
            }
            Ok(())
        }
    }

    struct PanickingProjectStore;

    impl ProjectStoreBackend for PanickingProjectStore {
        fn save(&self, _request: ProjectSaveRequest) -> Result<SaveReceipt, ProjectStoreError> {
            panic!("injected project store panic")
        }

        fn probe(&self, _directory: &Path) -> Result<ProjectProbe, ProjectStoreError> {
            panic!("injected project store panic")
        }

        fn discard_recovery(
            &self,
            _directory: &Path,
            _project_id: ProjectId,
            _revision: u64,
        ) -> Result<(), ProjectStoreError> {
            panic!("injected project store panic")
        }
    }

    #[test]
    fn worker_decodes_prepares_and_previews_off_thread() {
        let fixture = wav_fixture(44_100, &[0, i16::MAX, 0, i16::MIN]);
        let mut worker = WorkerHandle::spawn();
        worker
            .try_send(WorkerRequest::LoadSample {
                pad: pad(0, 0),
                generation: 7,
                purpose: LoadPurpose::Recovery,
                path: fixture.path().to_owned(),
                engine_rate: 48_000,
                recipe: SampleEditRecipe::identity(),
            })
            .unwrap();
        let result = worker.recv_timeout(Duration::from_secs(2)).unwrap();
        let WorkerResult::Loaded {
            generation,
            purpose,
            result: Ok(sample),
            ..
        } = result
        else {
            panic!("wrong result")
        };

        assert_eq!(generation, 7);
        assert_eq!(purpose, LoadPurpose::Recovery);
        assert_eq!(sample.rendered.sample_rate(), 48_000);
        assert_eq!(
            sample.fingerprint,
            SourceFingerprint::from_path(fixture.path()).unwrap()
        );
        assert_eq!(sample.base_preview.len(), EDIT_PREVIEW_COLUMNS);
        assert!(sample.base_preview.iter().any(|column| column.max > 0));
        assert!(sample.base_preview.iter().any(|column| column.min < 0));
        assert!(Arc::ptr_eq(&sample.base_preview, &sample.rendered_preview));
        worker.shutdown().unwrap();
    }

    #[test]
    fn stage_project_sample_applies_its_recipe_and_echoes_context_on_decode_error() {
        let fixture = wav_fixture(48_000, &[i16::MAX, 0, 0, i16::MIN]);
        let recipe =
            SampleEditRecipe::new(0, sampler_core::SAMPLE_PHASE_SCALE / 2, false, false).unwrap();
        let token = ProjectToken::new(73);
        let project_pad = pad(1, 4);
        let missing = fixture.path().with_file_name("missing-stage.wav");
        let mut worker = WorkerHandle::spawn();

        worker
            .try_send(WorkerRequest::StageProjectSample(Box::new(
                StageProjectSampleRequest {
                    token,
                    pad: project_pad,
                    revision: 19,
                    path: fixture.path().to_owned(),
                    engine_rate: 48_000,
                    recipe,
                },
            )))
            .unwrap();
        worker
            .try_send(WorkerRequest::StageProjectSample(Box::new(
                StageProjectSampleRequest {
                    token,
                    pad: project_pad,
                    revision: 20,
                    path: missing.clone(),
                    engine_rate: 48_000,
                    recipe,
                },
            )))
            .unwrap();

        let WorkerResult::ProjectSampleStaged {
            token: success_token,
            pad: success_pad,
            revision: success_revision,
            path: success_path,
            recipe: success_recipe,
            result: Ok(sample),
        } = worker.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("wrong stage success result")
        };
        assert_eq!(success_token, token);
        assert_eq!(success_pad, project_pad);
        assert_eq!(success_revision, 19);
        assert_eq!(success_path, fixture.path());
        assert_eq!(success_recipe, recipe);
        assert_eq!(sample.recipe, recipe);
        assert!(!Arc::ptr_eq(&sample.base, &sample.rendered));

        let WorkerResult::ProjectSampleStaged {
            token: error_token,
            pad: error_pad,
            revision: error_revision,
            path: error_path,
            recipe: error_recipe,
            result: Err(_),
        } = worker.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("wrong stage error result")
        };
        assert_eq!(error_token, token);
        assert_eq!(error_pad, project_pad);
        assert_eq!(error_revision, 20);
        assert_eq!(error_path, missing);
        assert_eq!(error_recipe, recipe);
        worker.shutdown().unwrap();
    }

    #[test]
    fn project_store_successes_and_errors_echo_operation_context() {
        let mut worker = worker_with_store(Box::new(ScriptedProjectStore));
        let save_success = empty_save_request(SaveKind::Explicit, 31);
        let save_error = empty_save_request(SaveKind::Recovery, 32);
        let probe_token = ProjectToken::new(81);
        let discard_token = ProjectToken::new(82);
        let project_id = ProjectId::from_bytes([9; 16]);

        worker
            .try_send(WorkerRequest::SaveProject(Box::new(save_success)))
            .unwrap();
        worker
            .try_send(WorkerRequest::SaveProject(Box::new(save_error)))
            .unwrap();
        worker
            .try_send(WorkerRequest::ProbeProject {
                token: probe_token,
                directory: PathBuf::from("good-probe"),
            })
            .unwrap();
        worker
            .try_send(WorkerRequest::ProbeProject {
                token: probe_token,
                directory: PathBuf::from("bad-probe"),
            })
            .unwrap();
        worker
            .try_send(WorkerRequest::DiscardRecovery {
                token: discard_token,
                directory: PathBuf::from("project"),
                project_id,
                revision: 33,
            })
            .unwrap();
        worker
            .try_send(WorkerRequest::DiscardRecovery {
                token: discard_token,
                directory: PathBuf::from("project"),
                project_id,
                revision: 404,
            })
            .unwrap();

        assert!(matches!(
            worker.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerResult::ProjectSaved {
                kind: SaveKind::Explicit,
                revision: 31,
                result: Ok(_)
            }
        ));
        assert!(matches!(
            worker.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerResult::ProjectSaved {
                kind: SaveKind::Recovery,
                revision: 32,
                result: Err(_)
            }
        ));
        assert!(matches!(
            worker.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerResult::ProjectProbed { token, result: Ok(_), .. } if token == probe_token
        ));
        assert!(matches!(
            worker.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerResult::ProjectProbed { token, result: Err(_), .. } if token == probe_token
        ));
        assert!(matches!(
            worker.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerResult::RecoveryDiscarded { token, project_id: result_id, revision: 33, result: Ok(()), .. }
                if token == discard_token && result_id == project_id
        ));
        assert!(matches!(
            worker.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerResult::RecoveryDiscarded { token, project_id: result_id, revision: 404, result: Err(_), .. }
                if token == discard_token && result_id == project_id
        ));
        worker.shutdown().unwrap();
    }

    #[test]
    fn full_and_closed_send_errors_retain_the_original_request() {
        let (sender, _receiver) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
        for _ in 0..WORKER_CHANNEL_CAPACITY {
            try_send_request(Some(&sender), WorkerRequest::Shutdown).unwrap();
        }
        let request =
            WorkerRequest::SaveProject(Box::new(empty_save_request(SaveKind::Recovery, 91)));
        let failure = try_send_request(Some(&sender), request.clone()).unwrap_err();
        assert_eq!(failure.kind(), WorkerSendError::WorkerBusy);
        assert_eq!(failure.into_request(), request);

        let request = WorkerRequest::ProbeProject {
            token: ProjectToken::new(92),
            directory: PathBuf::from("closed"),
        };
        let failure = try_send_request(None, request.clone()).unwrap_err();
        assert_eq!(failure.kind(), WorkerSendError::WorkerClosed);
        assert_eq!(failure.into_request(), request);
    }

    #[test]
    fn project_store_panic_keeps_join_terminal_safe() {
        let mut worker = worker_with_store(Box::new(PanickingProjectStore));
        worker
            .try_send(WorkerRequest::ProbeProject {
                token: ProjectToken::new(99),
                directory: PathBuf::from("panic"),
            })
            .unwrap();

        assert_eq!(worker.join(), Err(WorkerPanicked));
        assert_eq!(worker.join(), Ok(()));
        let failure = worker.try_send(WorkerRequest::Shutdown).unwrap_err();
        assert_eq!(failure.kind(), WorkerSendError::WorkerClosed);
        assert_eq!(failure.into_request(), WorkerRequest::Shutdown);
    }

    #[test]
    fn worker_preserves_user_purpose_on_load_error() {
        let mut worker = WorkerHandle::spawn();
        let path = std::env::temp_dir().join(format!(
            "sampler-tui-missing-{}-{}.wav",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        worker
            .try_send(WorkerRequest::LoadSample {
                pad: pad(0, 1),
                generation: 8,
                purpose: LoadPurpose::User,
                path: path.clone(),
                engine_rate: 48_000,
                recipe: SampleEditRecipe::identity(),
            })
            .unwrap();

        let WorkerResult::Loaded {
            pad: result_pad,
            generation,
            purpose,
            path: result_path,
            result: Err(_),
        } = worker.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("wrong result");
        };
        assert_eq!(result_pad, pad(0, 1));
        assert_eq!(generation, 8);
        assert_eq!(purpose, LoadPurpose::User);
        assert_eq!(result_path, path);
        worker.shutdown().unwrap();
    }

    #[test]
    fn identity_load_shares_the_base_and_rendered_owner() {
        let fixture = wav_fixture(48_000, &[0, i16::MAX]);

        let loaded = load_sample(fixture.path(), 48_000, SampleEditRecipe::identity()).unwrap();

        assert!(Arc::ptr_eq(&loaded.base, &loaded.rendered));
        assert_eq!(loaded.recipe, SampleEditRecipe::identity());
        assert_eq!(loaded.base_preview.len(), EDIT_PREVIEW_COLUMNS);
        assert!(Arc::ptr_eq(&loaded.base_preview, &loaded.rendered_preview));
    }

    #[test]
    fn edited_load_keeps_distinct_base_and_rendered_preview_domains() {
        let fixture = wav_fixture(48_000, &[i16::MAX, 0, 0, i16::MIN]);
        let recipe =
            SampleEditRecipe::new(0, sampler_core::SAMPLE_PHASE_SCALE / 2, false, false).unwrap();

        let loaded = load_sample(fixture.path(), 48_000, recipe).unwrap();

        assert!(!Arc::ptr_eq(&loaded.base, &loaded.rendered));
        assert!(!Arc::ptr_eq(&loaded.base_preview, &loaded.rendered_preview));
        assert!(loaded.base_preview.iter().any(|column| column.min < 0));
        assert!(loaded.rendered_preview.iter().all(|column| column.min >= 0));
    }

    #[test]
    fn edit_request_renders_the_base_not_a_previous_render() {
        let base = Arc::new(SampleBuffer::new(48_000, vec![0.25, -0.25, 0.5, -0.5]).unwrap());
        let stale_rendered = Arc::new(SampleBuffer::new(48_000, vec![0.9, 0.9]).unwrap());
        let base_preview = build_preview(&base);
        let recipe =
            SampleEditRecipe::new(0, sampler_core::SAMPLE_PHASE_SCALE / 2, false, false).unwrap();
        let mut worker = WorkerHandle::spawn();

        worker
            .try_send(WorkerRequest::EditSample {
                pad: pad(0, 3),
                generation: 91,
                base: Arc::clone(&base),
                base_preview: Arc::clone(&base_preview),
                recipe,
            })
            .unwrap();
        let WorkerResult::Edited {
            pad: result_pad,
            generation,
            recipe: result_recipe,
            result: Ok(rendered),
        } = worker.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("wrong result")
        };

        assert_eq!(result_pad, pad(0, 3));
        assert_eq!(generation, 91);
        assert_eq!(result_recipe, recipe);
        assert!(Arc::ptr_eq(&rendered.base_preview, &base_preview));
        assert_eq!(rendered.rendered.data(), &base.data()[..2]);
        assert_ne!(rendered.rendered.data(), stale_rendered.data());
        worker.shutdown().unwrap();
    }

    #[test]
    fn edit_preview_has_fixed_columns_and_perform_downsample_is_bounded() {
        let buffer =
            SampleBuffer::new(48_000, vec![-1.0, 1.0, -0.5, 0.5, 0.25, -0.25, 0.75, -0.75])
                .unwrap();

        let preview = build_preview(&buffer);
        let perform = downsample_preview(&preview);

        assert_eq!(preview.len(), EDIT_PREVIEW_COLUMNS);
        assert_eq!(perform.len(), crate::PREVIEW_COLUMNS);
        assert!(preview.iter().all(|column| column.min <= column.max));
        assert!(perform.iter().all(|column| column.min <= column.max));
        assert_eq!(perform[0], crate::PreviewColumn { min: -8, max: 8 });
    }

    #[test]
    fn perform_downsample_preserves_unipolar_and_mixed_extrema() {
        let positive = Arc::new([crate::PreviewColumn { min: 2, max: 4 }; EDIT_PREVIEW_COLUMNS]);
        let negative = Arc::new([crate::PreviewColumn { min: -4, max: -2 }; EDIT_PREVIEW_COLUMNS]);
        let mixed = Arc::new(std::array::from_fn(|index| {
            if index.is_multiple_of(2) {
                crate::PreviewColumn { min: -3, max: -1 }
            } else {
                crate::PreviewColumn { min: 2, max: 4 }
            }
        }));

        assert_eq!(
            downsample_preview(&positive),
            [crate::PreviewColumn { min: 2, max: 4 }; crate::PREVIEW_COLUMNS]
        );
        assert_eq!(
            downsample_preview(&negative),
            [crate::PreviewColumn { min: -4, max: -2 }; crate::PREVIEW_COLUMNS]
        );
        assert_eq!(
            downsample_preview(&mixed),
            [crate::PreviewColumn { min: -3, max: 4 }; crate::PREVIEW_COLUMNS]
        );
    }

    #[test]
    fn perform_downsample_keeps_an_empty_preview_empty() {
        let empty = Arc::new([crate::PreviewColumn::default(); EDIT_PREVIEW_COLUMNS]);

        assert_eq!(
            downsample_preview(&empty),
            [crate::PreviewColumn::default(); crate::PREVIEW_COLUMNS]
        );
    }

    #[test]
    fn worker_channel_capacity_is_exactly_eight() {
        assert_eq!(WORKER_CHANNEL_CAPACITY, 8);
    }

    #[test]
    fn saturated_edit_result_is_released_while_the_application_keeps_the_base_owner() {
        let base = Arc::new(SampleBuffer::new(48_000, vec![0.25, -0.25]).unwrap());
        let base_preview = build_preview(&base);
        let weak = Arc::downgrade(&base);
        let mut worker = worker_with_capacities(WORKER_CHANNEL_CAPACITY, 0);

        worker
            .try_send(WorkerRequest::EditSample {
                pad: pad(0, 0),
                generation: 1,
                base: Arc::clone(&base),
                base_preview,
                recipe: SampleEditRecipe::identity(),
            })
            .unwrap();
        worker.shutdown().unwrap();

        // The application owner survives worker/result-lane teardown; it performs the final drop.
        assert_eq!(Arc::strong_count(&base), 1);
        assert!(weak.upgrade().is_some());
        drop(base);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn failed_edit_result_delivery_leaves_the_final_base_drop_to_the_application_owner() {
        let (request_sender, request_receiver) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
        let (result_sender, result_receiver) = mpsc::sync_channel(0);
        drop(result_receiver);
        let worker = thread::spawn(move || worker_loop(request_receiver, result_sender));
        let base = Arc::new(SampleBuffer::new(48_000, vec![0.25, -0.25]).unwrap());
        let base_preview = build_preview(&base);
        let weak = Arc::downgrade(&base);

        request_sender
            .send(WorkerRequest::EditSample {
                pad: pad(0, 0),
                generation: 2,
                base: Arc::clone(&base),
                base_preview,
                recipe: SampleEditRecipe::identity(),
            })
            .unwrap();
        drop(request_sender);
        worker.join().unwrap();

        // Delivery failure drops the worker-owned result, never the retained application owner.
        assert_eq!(Arc::strong_count(&base), 1);
        drop(base);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn worker_scan_filters_and_sorts_before_returning_to_the_ui() {
        let fixture = DirectoryFixture::new("sorted-scan");
        fs::create_dir(fixture.path().join("beats")).unwrap();
        for name in ["z.mp3", "notes.txt", "A.WAV"] {
            File::create(fixture.path().join(name)).unwrap();
        }

        let scan = scan_directory(fixture.path(), false).unwrap();
        let names = scan
            .entries()
            .iter()
            .map(DirectoryEntry::display_name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["beats", "A.WAV", "z.mp3"]);
        assert!(!scan.truncated());
    }

    #[test]
    fn directory_scan_payload_is_capped_and_reports_truncation() {
        let fixture = DirectoryFixture::new("bounded-scan");
        for index in 0..=MAX_DIRECTORY_ENTRIES {
            fs::create_dir(fixture.path().join(format!("dir-{index:04}"))).unwrap();
        }

        let scan = scan_directory(fixture.path(), false).unwrap();

        assert_eq!(scan.entries().len(), MAX_DIRECTORY_ENTRIES);
        assert!(scan.truncated());
    }

    #[test]
    fn encoded_file_budget_rejects_a_sparse_oversized_payload_before_decode() {
        let fixture = DirectoryFixture::new("encoded-budget");
        let path = fixture.path().join("oversized.wav");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_ENCODED_FILE_BYTES + 1).unwrap();

        let error = load_sample(&path, 48_000, SampleEditRecipe::identity()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("exceeds the 134217728-byte encoded input limit")
        );
    }

    #[test]
    fn preview_handles_fewer_frames_than_columns_without_non_finite_values() {
        let fixture = wav_fixture(48_000, &[i16::MAX]);
        let mut worker = WorkerHandle::spawn();
        worker
            .try_send(WorkerRequest::LoadSample {
                pad: pad(0, 0),
                generation: 1,
                purpose: LoadPurpose::User,
                path: fixture.path().to_owned(),
                engine_rate: 48_000,
                recipe: SampleEditRecipe::identity(),
            })
            .unwrap();
        let WorkerResult::Loaded {
            result: Ok(sample), ..
        } = worker.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("wrong result")
        };

        assert_eq!(sample.base_preview[0].max, 8);
        assert!(
            sample.base_preview[1..]
                .iter()
                .all(|column| column.min == 0 && column.max == 0)
        );
        worker.shutdown().unwrap();
    }

    #[test]
    fn preview_bin_without_finite_samples_is_empty() {
        assert_eq!(
            preview_column(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY]),
            crate::PreviewColumn::default()
        );
    }

    #[test]
    fn exact_maximum_whole_second_duration_keeps_zero_fraction() {
        assert_eq!(
            frame_duration(u128::from(u64::MAX), 1),
            Duration::from_secs(u64::MAX)
        );
        assert_eq!(
            frame_duration(u128::from(u64::MAX) * 2, 2),
            Duration::from_secs(u64::MAX)
        );
    }

    #[test]
    fn duration_saturates_when_maximum_seconds_has_a_fraction_or_is_exceeded() {
        let maximum_seconds = u128::from(u64::MAX);

        assert_eq!(frame_duration(maximum_seconds * 2 + 1, 2), Duration::MAX);
        assert_eq!(frame_duration(maximum_seconds + 1, 1), Duration::MAX);
    }

    #[test]
    fn request_try_send_distinguishes_full_and_closed_channels() {
        let (full_sender, _full_receiver) = mpsc::sync_channel(1);
        try_send_request(Some(&full_sender), WorkerRequest::Shutdown).unwrap();
        let failure = try_send_request(Some(&full_sender), WorkerRequest::Shutdown).unwrap_err();
        assert_eq!(failure.kind(), WorkerSendError::WorkerBusy);
        assert_eq!(failure.into_request(), WorkerRequest::Shutdown);

        let (closed_sender, closed_receiver) = mpsc::sync_channel(1);
        drop(closed_receiver);
        let failure = try_send_request(Some(&closed_sender), WorkerRequest::Shutdown).unwrap_err();
        assert_eq!(failure.kind(), WorkerSendError::WorkerClosed);
        assert_eq!(failure.into_request(), WorkerRequest::Shutdown);
        let failure = try_send_request(None, WorkerRequest::Shutdown).unwrap_err();
        assert_eq!(failure.kind(), WorkerSendError::WorkerClosed);
        assert_eq!(failure.into_request(), WorkerRequest::Shutdown);
    }

    #[test]
    fn shutdown_drains_a_saturated_result_channel_before_joining() {
        let mut worker = worker_with_capacities(8, 0);
        worker
            .try_send(WorkerRequest::ScanDirectory {
                request_id: 1,
                path: std::env::temp_dir().join("sampler-tui-definitely-missing-directory"),
                show_hidden: false,
            })
            .unwrap();
        let (done_sender, done_receiver) = mpsc::channel();
        let shutdown = thread::spawn(move || done_sender.send(worker.shutdown()).unwrap());

        assert_eq!(
            done_receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(())
        );
        shutdown.join().unwrap();
    }

    #[test]
    fn shutdown_can_be_requested_before_the_worker_is_joined() {
        let mut worker = WorkerHandle::spawn();

        worker.request_shutdown();

        let failure = worker.try_send(WorkerRequest::Shutdown).unwrap_err();
        assert_eq!(failure.kind(), WorkerSendError::WorkerClosed);
        assert_eq!(failure.into_request(), WorkerRequest::Shutdown);
        assert_eq!(worker.join(), Ok(()));
    }

    #[test]
    fn shutdown_reports_a_worker_thread_panic() {
        let mut worker = panicked_worker();

        assert_eq!(worker.shutdown(), Err(WorkerPanicked));
    }
}
