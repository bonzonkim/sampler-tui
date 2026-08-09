use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use hound::{SampleFormat, WavSpec, WavWriter};
use sampler_core::{
    BankId, EventId, MasterMixSettings, Meter, MidiSettings, PadId, PadMixSettings, PadSettings,
    PatternEvent, PatternSlotId, ProjectDocument, ProjectId, ProjectPad, ProjectPattern,
    ProjectPatternEvent, Resolution, SampleEditRecipe, Tempo, Transport,
};
use sampler_tui::loader::PROGRESS_CHANNEL_CAPACITY;
use sampler_tui::{
    ExportCancel, ExportPatternSlot, ExportToken, OfflineExportError, OfflineExportRequest,
    OfflineExportSnapshot, SourceFingerprint, WorkerHandle, WorkerRequest, WorkerResult,
    WorkerSendError,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-export-worker-{label}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("audio")).unwrap();
        Self { root }
    }

    fn request(&self, token: u64, destination: &str) -> (OfflineExportRequest, ExportCancel) {
        self.request_with_transport(token, destination, 240.0, 1)
    }

    fn request_with_transport(
        &self,
        token: u64,
        destination: &str,
        bpm: f64,
        bars: u16,
    ) -> (OfflineExportRequest, ExportCancel) {
        let pad = pad(0);
        let source = self.root.join(format!("source-{token}.wav"));
        write_wav(&source);
        let fingerprint = SourceFingerprint::from_path(&source).unwrap();
        let relative = format!("audio/{}.wav", fingerprint.digest);
        fs::rename(&source, self.root.join(&relative)).unwrap();
        let project_pad = ProjectPad::new(
            pad,
            relative,
            fingerprint.digest,
            PadSettings::default(),
            PadMixSettings::default(),
            SampleEditRecipe::identity(),
        )
        .unwrap();
        let slot = PatternSlotId::new(0).unwrap();
        let tempo = Tempo::new(bpm).unwrap();
        let meter = Meter::new(4, 4).unwrap();
        let transport = Transport::new(48_000, tempo, meter, bars, Resolution::Sixteenth)
            .unwrap()
            .with_swing(0.5)
            .unwrap();
        let pattern = ProjectPattern {
            slot,
            name: "worker export".to_owned(),
            sample_rate: 48_000,
            tempo,
            meter,
            bars,
            resolution: Resolution::Sixteenth,
            swing: 0.5,
            quantize_strength: 0.0,
            events: vec![ProjectPatternEvent {
                event: PatternEvent::new(EventId(1), pad, 0, 1.0, None)
                    .unwrap()
                    .quantized(&transport, 0.0),
                raw_frame: 0,
            }],
        };
        let document = ProjectDocument::new_v4(
            ProjectId::from_bytes([token as u8; 16]),
            "worker export",
            token,
            vec![project_pad],
            vec![pattern],
            MasterMixSettings::default(),
            MidiSettings::default(),
        )
        .unwrap();
        let snapshot = OfflineExportSnapshot::from_document(
            &self.root,
            &document,
            ExportPatternSlot::try_from(1).unwrap(),
        )
        .unwrap();
        let cancel = ExportCancel::default();
        let request = OfflineExportRequest::new(
            ExportToken::new(token),
            self.root.join(destination),
            snapshot,
            cancel.clone(),
        )
        .unwrap();
        (request, cancel)
    }

    fn temp_entries(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains("sampler-tui-tmp"))
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

fn write_wav(path: &Path) {
    let mut writer = WavWriter::create(
        path,
        WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        },
    )
    .unwrap();
    for frame in 0..1_024 {
        let sample = frame as f32 / 1_024.0;
        writer.write_sample(sample).unwrap();
        writer.write_sample(-sample).unwrap();
    }
    writer.finalize().unwrap();
}

#[test]
fn export_worker_returns_owned_request_when_busy_and_never_creates_a_second_temp() {
    let blocker = Fixture::new("busy-blocker");
    let retry = Fixture::new("busy-retry");
    let (blocking_request, blocking_cancel) = blocker.request(11, "blocking.wav");
    let (retry_request, _) = retry.request(12, "retry.wav");
    let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let hook_gate = Arc::clone(&gate);
    let mut worker = WorkerHandle::spawn_with_project_asset_open_hook(move || {
        let (lock, changed) = &*hook_gate;
        let mut state = lock.lock().unwrap();
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).unwrap();
        }
    });
    worker
        .try_send(WorkerRequest::Export(blocking_request))
        .unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        while !state.0 {
            state = changed.wait(state).unwrap();
        }
    }
    for request_id in 0..8 {
        worker
            .try_send(WorkerRequest::ScanDirectory {
                request_id,
                path: retry.root.join("missing"),
                show_hidden: false,
            })
            .unwrap();
    }

    let failure = worker
        .try_send(WorkerRequest::Export(retry_request.clone()))
        .unwrap_err();

    assert_eq!(failure.kind(), WorkerSendError::WorkerBusy);
    let returned = failure.into_request();
    assert_eq!(returned, WorkerRequest::Export(retry_request));
    assert!(!retry.root.join("retry.wav").exists());
    assert!(retry.temp_entries().is_empty());

    blocking_cancel.cancel();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        state.1 = true;
        changed.notify_all();
    }
    for _ in 0..9 {
        worker.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    worker.try_send(returned).unwrap();
    let finished = worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        finished,
        WorkerResult::ExportFinished {
            token,
            result: Ok(ref receipt),
            ..
        } if token == ExportToken::new(12) && receipt.token == token
    ));
    assert!(retry.root.join("retry.wav").is_file());
    assert!(retry.temp_entries().is_empty());
    worker.shutdown().unwrap();
}

#[test]
fn slow_progress_consumer_observes_one_coalesced_value_and_one_terminal_result() {
    let fixture = Fixture::new("slow-progress");
    let (request, _) = fixture.request(21, "mix.wav");
    let token = request.token();
    let mut worker = WorkerHandle::spawn();
    worker.try_send(WorkerRequest::Export(request)).unwrap();

    let terminal = worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        terminal,
        WorkerResult::ExportFinished {
            token: result_token,
            result: Ok(_),
            ..
        } if result_token == token
    ));
    assert!(worker.try_recv().is_err());

    let progress = std::iter::from_fn(|| worker.try_recv_export_progress()).collect::<Vec<_>>();
    assert!(progress.len() <= PROGRESS_CHANNEL_CAPACITY);
    assert_eq!(progress.len(), 1);
    assert!(progress.iter().all(|result| matches!(
        result,
        WorkerResult::ExportProgress {
            token: result_token,
            completed_units,
            total_units,
            ..
        } if *result_token == token && completed_units == total_units
    )));
    assert!(fixture.root.join("mix.wav").is_file());
    worker.shutdown().unwrap();
}

#[test]
fn closed_worker_returns_the_exact_export_request_without_creating_a_temp() {
    let fixture = Fixture::new("closed-request");
    let (request, _) = fixture.request(27, "closed.wav");
    let mut worker = WorkerHandle::spawn();
    worker.request_shutdown();

    let failure = worker
        .try_send(WorkerRequest::Export(request.clone()))
        .unwrap_err();

    assert_eq!(failure.kind(), WorkerSendError::WorkerClosed);
    assert_eq!(failure.into_request(), WorkerRequest::Export(request));
    assert!(!fixture.root.join("closed.wav").exists());
    assert!(fixture.temp_entries().is_empty());
    worker.join().unwrap();
}

#[test]
fn admitted_shutdown_closes_export_admission_and_cancels_every_earlier_export() {
    let first = Fixture::new("shutdown-marker-first");
    let later = Fixture::new("shutdown-marker-later");
    let (first_request, _) = first.request(28, "first.wav");
    let (later_request, _) = later.request(29, "later.wav");
    let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let hook_gate = Arc::clone(&gate);
    let mut worker = WorkerHandle::spawn_with_project_asset_open_hook(move || {
        let (lock, changed) = &*hook_gate;
        let mut state = lock.lock().unwrap();
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).unwrap();
        }
    });
    worker
        .try_send(WorkerRequest::Export(first_request))
        .unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        while !state.0 {
            state = changed.wait(state).unwrap();
        }
    }

    worker.try_send(WorkerRequest::Shutdown).unwrap();
    let later_send = worker.try_send(WorkerRequest::Export(later_request.clone()));
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        state.1 = true;
        changed.notify_all();
    }
    let terminal = worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        worker.recv_timeout(Duration::from_secs(5)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    );
    worker.join().unwrap();

    let failure = later_send.unwrap_err();
    assert_eq!(failure.kind(), WorkerSendError::WorkerClosed);
    assert_eq!(failure.into_request(), WorkerRequest::Export(later_request));
    assert!(matches!(
        terminal,
        WorkerResult::ExportFinished {
            token,
            result: Err(OfflineExportError::Cancelled),
            ..
        } if token == ExportToken::new(28)
    ));
    assert!(!first.root.join("first.wav").exists());
    assert!(!later.root.join("later.wav").exists());
    assert!(first.temp_entries().is_empty());
    assert!(later.temp_entries().is_empty());
}

#[test]
fn an_old_cancel_and_terminal_token_cannot_cancel_or_identify_a_newer_export() {
    let fixture = Fixture::new("stale-token");
    let (old_request, old_cancel) = fixture.request(31, "old.wav");
    let (new_request, _) = fixture.request(32, "new.wav");
    old_cancel.cancel();
    let mut worker = WorkerHandle::spawn();
    worker.try_send(WorkerRequest::Export(old_request)).unwrap();
    worker.try_send(WorkerRequest::Export(new_request)).unwrap();

    assert!(matches!(
        worker.recv_timeout(Duration::from_secs(5)).unwrap(),
        WorkerResult::ExportFinished {
            token,
            revision: 31,
            result: Err(OfflineExportError::Cancelled),
            ..
        } if token == ExportToken::new(31)
    ));
    assert!(matches!(
        worker.recv_timeout(Duration::from_secs(5)).unwrap(),
        WorkerResult::ExportFinished {
            token,
            revision: 32,
            result: Ok(ref receipt),
            ..
        } if token == ExportToken::new(32) && receipt.token == token && receipt.revision == 32
    ));
    assert!(!fixture.root.join("old.wav").exists());
    assert!(fixture.root.join("new.wav").is_file());
    assert!(fixture.temp_entries().is_empty());
    worker.shutdown().unwrap();
}

#[test]
fn shutdown_during_render_joins_before_return_and_cleans_the_owned_temp() {
    let fixture = Fixture::new("shutdown-render");
    let (request, _) = fixture.request_with_transport(61, "shutdown.wav", 20.0, 64);
    let token = request.token();
    let mut worker = WorkerHandle::spawn();
    worker.try_send(WorkerRequest::Export(request)).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(WorkerResult::ExportProgress {
            token: progress_token,
            completed_units,
            ..
        }) = worker.try_recv_export_progress()
            && progress_token == token
            && completed_units > 1
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "render progress timed out"
        );
        std::hint::spin_loop();
    }

    worker.request_shutdown();
    worker.join().unwrap();

    assert!(!fixture.root.join("shutdown.wav").exists());
    assert!(fixture.temp_entries().is_empty());
}
