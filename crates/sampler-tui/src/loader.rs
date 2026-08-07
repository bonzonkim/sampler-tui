use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sampler_audio::{SampleBuffer, decode_path, prepare_sample};
use sampler_core::PadId;

use crate::app::{PREVIEW_COLUMNS, PreviewColumn};
use crate::file_picker::{DirectoryEntry, DirectoryEntryKind};

pub(crate) const WORKER_CHANNEL_CAPACITY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerRequest {
    ScanDirectory {
        request_id: u64,
        path: PathBuf,
        show_hidden: bool,
    },
    LoadSample {
        pad: PadId,
        generation: u64,
        path: PathBuf,
        engine_rate: u32,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerResult {
    Scanned {
        request_id: u64,
        path: PathBuf,
        result: Result<Vec<DirectoryEntry>, String>,
    },
    Loaded {
        pad: PadId,
        generation: u64,
        path: PathBuf,
        result: Result<LoadedSample, String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedSample {
    pub buffer: Arc<SampleBuffer>,
    pub source_rate: u32,
    pub source_frames: usize,
    pub duration: Duration,
    pub preview: [PreviewColumn; PREVIEW_COLUMNS],
}

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

    pub fn try_send(&self, request: WorkerRequest) -> Result<(), WorkerSendError> {
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
) -> Result<(), WorkerSendError> {
    let Some(sender) = sender else {
        return Err(WorkerSendError::WorkerClosed);
    };
    sender.try_send(request).map_err(|error| match error {
        TrySendError::Full(_) => WorkerSendError::WorkerBusy,
        TrySendError::Disconnected(_) => WorkerSendError::WorkerClosed,
    })
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn worker_loop(requests: Receiver<WorkerRequest>, results: SyncSender<WorkerResult>) {
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
                path,
                engine_rate,
            } => WorkerResult::Loaded {
                pad,
                generation,
                result: load_sample(&path, engine_rate),
                path,
            },
            WorkerRequest::Shutdown => break,
        };
        if results.send(result).is_err() {
            break;
        }
    }
}

fn scan_directory(path: &Path, show_hidden: bool) -> Result<Vec<DirectoryEntry>, String> {
    let reader = fs::read_dir(path).map_err(|error| format_error(&error))?;
    let mut entries = Vec::new();
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
        entries.push(DirectoryEntry {
            path: item.path(),
            kind,
        });
    }
    Ok(entries)
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

fn load_sample(path: &Path, engine_rate: u32) -> Result<LoadedSample, String> {
    let decoded = decode_path(path).map_err(|error| format_error(&error))?;
    let source_rate = decoded.sample_rate;
    let source_frames = decoded.frames();
    let duration = source_duration(source_frames, source_rate);
    let buffer =
        Arc::new(prepare_sample(decoded, engine_rate).map_err(|error| format_error(&error))?);
    let preview = build_preview(&buffer);
    Ok(LoadedSample {
        buffer,
        source_rate,
        source_frames,
        duration,
        preview,
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

fn build_preview(buffer: &SampleBuffer) -> [PreviewColumn; PREVIEW_COLUMNS] {
    std::array::from_fn(|column| {
        let frames = buffer.frames();
        let columns = PREVIEW_COLUMNS as u128;
        let start = ((column as u128) * (frames as u128)).div_ceil(columns) as usize;
        let end = (((column + 1) as u128) * (frames as u128)).div_ceil(columns) as usize;
        if start == end {
            return PreviewColumn::default();
        }
        preview_column(&buffer.data()[start * 2..end * 2])
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use sampler_core::{BankId, PadId};

    use super::{
        WorkerHandle, WorkerPanicked, WorkerRequest, WorkerResult, WorkerSendError, frame_duration,
        preview_column, try_send_request, worker_loop,
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct WavFixture(PathBuf);

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

    #[test]
    fn worker_decodes_prepares_and_previews_off_thread() {
        let fixture = wav_fixture(44_100, &[0, i16::MAX, 0, i16::MIN]);
        let mut worker = WorkerHandle::spawn();
        worker
            .try_send(WorkerRequest::LoadSample {
                pad: pad(0, 0),
                generation: 7,
                path: fixture.path().to_owned(),
                engine_rate: 48_000,
            })
            .unwrap();
        let result = worker.recv_timeout(Duration::from_secs(2)).unwrap();
        let WorkerResult::Loaded {
            generation,
            result: Ok(sample),
            ..
        } = result
        else {
            panic!("wrong result")
        };

        assert_eq!(generation, 7);
        assert_eq!(sample.buffer.sample_rate(), 48_000);
        assert_eq!(sample.preview.len(), 64);
        assert!(sample.preview.iter().any(|column| column.max > 0));
        assert!(sample.preview.iter().any(|column| column.min < 0));
        worker.shutdown().unwrap();
    }

    #[test]
    fn preview_handles_fewer_frames_than_columns_without_non_finite_values() {
        let fixture = wav_fixture(48_000, &[i16::MAX]);
        let mut worker = WorkerHandle::spawn();
        worker
            .try_send(WorkerRequest::LoadSample {
                pad: pad(0, 0),
                generation: 1,
                path: fixture.path().to_owned(),
                engine_rate: 48_000,
            })
            .unwrap();
        let WorkerResult::Loaded {
            result: Ok(sample), ..
        } = worker.recv_timeout(Duration::from_secs(2)).unwrap()
        else {
            panic!("wrong result")
        };

        assert_eq!(sample.preview[0].max, 8);
        assert!(
            sample.preview[1..]
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
        assert_eq!(
            try_send_request(Some(&full_sender), WorkerRequest::Shutdown),
            Err(WorkerSendError::WorkerBusy)
        );

        let (closed_sender, closed_receiver) = mpsc::sync_channel(1);
        drop(closed_receiver);
        assert_eq!(
            try_send_request(Some(&closed_sender), WorkerRequest::Shutdown),
            Err(WorkerSendError::WorkerClosed)
        );
        assert_eq!(
            try_send_request(None, WorkerRequest::Shutdown),
            Err(WorkerSendError::WorkerClosed)
        );
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

        assert_eq!(
            worker.try_send(WorkerRequest::Shutdown),
            Err(WorkerSendError::WorkerClosed)
        );
        assert_eq!(worker.join(), Ok(()));
    }

    #[test]
    fn shutdown_reports_a_worker_thread_panic() {
        let mut worker = panicked_worker();

        assert_eq!(worker.shutdown(), Err(WorkerPanicked));
    }
}
