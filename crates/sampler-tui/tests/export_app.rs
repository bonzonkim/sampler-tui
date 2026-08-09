use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sampler_core::{BankId, PadId};
use sampler_tui::{
    App, AtomicWavPublisher, ExportPatternSlot, ExportPhase, ExportResultFence, ExportStatusView,
    ExportToken, InputAction, OfflineExportError, OfflineExportReceipt, OfflineExportSnapshot,
    ProjectOpenError, WorkerHandle, WorkerRequest, WorkerResult, WorkerSendError, render_offline,
};

#[path = "support/mixer_harness.rs"]
mod mixer_harness;

use mixer_harness::{FixtureTree, Harness};

#[derive(Debug, Clone, PartialEq)]
struct AppTruth {
    revision: u64,
    header: String,
    bank: sampler_core::BankId,
    selected_pad: usize,
    selected_slot: sampler_core::PatternSlotId,
    pattern_events: usize,
    transport: sampler_core::Transport,
    transport_playing: bool,
    audio_format: Option<(u32, u16)>,
    live_audio: sampler_audio::Telemetry,
}

fn app_truth(app: &App) -> AppTruth {
    AppTruth {
        revision: app.project_revision(),
        header: app.project_header(),
        bank: app.active_bank(),
        selected_pad: app.selected_pad(),
        selected_slot: app.patterns().selected_slot(),
        pattern_events: app.patterns().selected_pattern().events().len(),
        transport: app.patterns().selected_pattern().transport(),
        transport_playing: app.patterns().is_playing(),
        audio_format: app.audio_format(),
        live_audio: app.telemetry(),
    }
}

fn saved_pattern_harness() -> (FixtureTree, Harness, std::path::PathBuf) {
    let tree = FixtureTree::new();
    let source = tree.write_wav("source.wav");
    let project = tree.path("project");
    let destination = tree.path("mix.wav");
    let mut harness = Harness::new();
    let pad = PadId::new(BankId::new(0).unwrap(), 0).unwrap();
    harness.load(pad, &source);
    harness.record_hit(0);
    let now = Instant::now();
    harness.save_as(&project, now);
    if harness.app.maintain_project(now) {
        harness.dispatch_queued();
    }
    (tree, harness, destination)
}

fn assert_no_export_temp(tree: &FixtureTree) {
    assert!(
        fs::read_dir(tree.path("."))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .all(|path| !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("sampler-tui-tmp")))
    );
}

fn relative_path(from: &std::path::Path, to: &std::path::Path) -> std::path::PathBuf {
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = std::path::PathBuf::new();
    for component in &from_components[common..] {
        if matches!(component, std::path::Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &to_components[common..] {
        relative.push(component.as_os_str());
    }
    relative
}

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: std::path::PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-export-app-{label}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn destination(&self) -> std::path::PathBuf {
        self.root.join("mix.wav")
    }

    fn temp_entries(&self) -> Vec<std::path::PathBuf> {
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

#[test]
fn empty_pattern_export_is_rejected_without_mutating_app_or_filesystem() {
    let fixture = Fixture::new("empty");
    let destination = fixture.destination();
    let mut app = App::without_audio("test audio unavailable");
    let truth = app_truth(&app);

    let result = app.start_export(destination.clone());

    assert_eq!(result, Err(OfflineExportError::EmptyPattern));
    assert_eq!(app_truth(&app), truth);
    assert!(app.export_operation().is_none());
    assert!(app.take_worker_requests().is_empty());
    assert!(!destination.exists());
    assert!(fixture.temp_entries().is_empty());
}

#[test]
fn valid_admission_atomically_installs_one_immutable_request_and_operation() {
    let (tree, mut harness, destination) = saved_pattern_harness();
    let revision = harness.app.project_revision();
    let project_id = harness.app.project_snapshot().unwrap().project_id;
    let slot = harness.app.patterns().selected_slot();

    let token = harness.app.start_export(destination.clone()).unwrap();

    let operation = harness.app.export_operation().unwrap();
    assert_eq!(operation.token(), token);
    assert_eq!(operation.project_id(), project_id);
    assert_eq!(operation.revision(), revision);
    assert_eq!(operation.slot(), slot);
    assert_eq!(operation.destination(), destination);
    let requests = harness.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = requests.as_slice() else {
        panic!("valid admission must queue exactly one export request: {requests:?}")
    };
    assert_eq!(request.token(), token);
    assert_eq!(request.destination(), destination);
    assert_eq!(request.snapshot().project_id(), project_id);
    assert_eq!(request.snapshot().revision(), revision);
    assert_eq!(request.snapshot().slot(), slot);
    assert!(!destination.exists());
    assert_no_export_temp(&tree);
}

#[test]
fn valid_untitled_project_snapshots_a_committed_source_outside_the_app_directory() {
    let tree = FixtureTree::new();
    let source = tree.write_wav("loose-source.wav");
    let destination = tree.path("untitled.wav");
    let mut harness = Harness::new();
    let pad = PadId::new(BankId::new(0).unwrap(), 0).unwrap();
    harness.load(pad, &source);
    harness.record_hit(0);
    let truth = app_truth(&harness.app);

    let token = harness.app.start_export(destination.clone()).unwrap();

    assert_eq!(app_truth(&harness.app), truth);
    let requests = harness.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = requests.as_slice() else {
        panic!("expected one export request")
    };
    assert_eq!(request.token(), token);
    assert_eq!(request.snapshot().pads()[0].source_path, source);
    assert!(!destination.exists());
    assert_no_export_temp(&tree);
}

#[test]
fn named_dirty_project_resolves_a_new_relative_source_against_the_app_directory() {
    let (tree, mut harness, destination) = saved_pattern_harness();
    let replacement = tree.write_wav("relative-replacement.wav");
    let current_dir = std::env::current_dir().unwrap();
    let relative = relative_path(&current_dir, &replacement);
    assert!(relative.is_relative());
    let pad = PadId::new(BankId::new(0).unwrap(), 0).unwrap();
    harness.load(pad, &relative);
    let truth = app_truth(&harness.app);

    harness.app.start_export(destination.clone()).unwrap();

    assert_eq!(app_truth(&harness.app), truth);
    let requests = harness.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = requests.as_slice() else {
        panic!("expected one export request")
    };
    assert_eq!(
        request.snapshot().pads()[0].source_path,
        current_dir.join(relative)
    );
    assert!(!destination.exists());
    assert_no_export_temp(&tree);
}

#[test]
fn busy_worker_returns_the_exact_export_request_to_app_owned_retry_state() {
    let (_tree, mut harness, destination) = saved_pattern_harness();
    let token = harness.app.start_export(destination.clone()).unwrap();
    let mut requests = harness.app.take_worker_requests();
    let request = requests.pop().expect("expected export request");
    assert!(requests.is_empty());
    let expected = request.clone();

    assert!(
        harness
            .app
            .apply_worker_send_error(request, WorkerSendError::WorkerBusy)
    );

    assert_eq!(harness.app.take_worker_requests(), vec![expected]);
    let operation = harness.app.export_operation().unwrap();
    assert_eq!(operation.token(), token);
    assert_eq!(operation.destination(), destination);
    assert_eq!(operation.phase(), ExportPhase::Queued);

    let (_tree, mut cancelling, destination) = saved_pattern_harness();
    cancelling.app.start_export(destination).unwrap();
    let request = cancelling.app.take_worker_requests().pop().unwrap();
    assert!(cancelling.app.cancel_export());
    let cancelling_status = cancelling.app.status().to_owned();
    assert!(
        cancelling
            .app
            .apply_worker_send_error(request, WorkerSendError::WorkerBusy)
    );
    assert_eq!(
        cancelling.app.export_operation().unwrap().phase(),
        ExportPhase::Cancelling
    );
    assert_eq!(cancelling.app.status(), cancelling_status);
}

#[test]
fn active_export_and_existing_destination_rejections_preserve_exact_app_truth() {
    let (active_tree, mut active, destination) = saved_pattern_harness();
    active.app.start_export(destination.clone()).unwrap();
    let operation = active.app.export_operation().unwrap().clone();
    let truth = app_truth(&active.app);

    assert_eq!(
        active.app.start_export("second.wav"),
        Err(OfflineExportError::OperationPending)
    );
    assert_eq!(app_truth(&active.app), truth);
    assert_eq!(active.app.export_operation(), Some(&operation));
    assert_eq!(active.app.take_worker_requests().len(), 1);
    assert!(!destination.exists());
    assert_no_export_temp(&active_tree);

    let (collision_tree, mut collision, destination) = saved_pattern_harness();
    fs::write(&destination, b"owned destination").unwrap();
    let truth = app_truth(&collision.app);
    assert_eq!(
        collision.app.start_export(destination.clone()),
        Err(OfflineExportError::DestinationExists(destination.clone()))
    );
    assert_eq!(app_truth(&collision.app), truth);
    assert!(collision.app.export_operation().is_none());
    assert!(collision.app.take_worker_requests().is_empty());
    assert_eq!(fs::read(destination).unwrap(), b"owned destination");
    assert_no_export_temp(&collision_tree);
}

#[test]
fn missing_source_project_save_and_project_open_rejections_preserve_exact_app_truth() {
    let (missing_tree, mut missing, destination) = saved_pattern_harness();
    let source = missing.app.project_snapshot().unwrap().pads[0]
        .source_path
        .clone();
    fs::remove_file(source).unwrap();
    let truth = app_truth(&missing.app);
    assert!(matches!(
        missing.app.start_export(destination.clone()),
        Err(OfflineExportError::MissingPadSource { .. })
    ));
    assert_eq!(app_truth(&missing.app), truth);
    assert!(missing.app.export_operation().is_none());
    assert!(missing.app.take_worker_requests().is_empty());
    assert!(!destination.exists());
    assert_no_export_temp(&missing_tree);

    let (saving_tree, mut saving, destination) = saved_pattern_harness();
    saving.app.request_save().unwrap();
    let truth = app_truth(&saving.app);
    assert!(matches!(
        saving.app.start_export(destination.clone()),
        Err(OfflineExportError::UnresolvedAppState(_))
    ));
    assert_eq!(app_truth(&saving.app), truth);
    assert!(saving.app.export_operation().is_none());
    assert!(!destination.exists());
    assert_no_export_temp(&saving_tree);

    let (tree, mut opening, destination) = saved_pattern_harness();
    opening
        .app
        .request_open_project(tree.path("another-project"))
        .unwrap();
    let truth = app_truth(&opening.app);
    assert!(matches!(
        opening.app.start_export(destination.clone()),
        Err(OfflineExportError::UnresolvedAppState(_))
    ));
    assert_eq!(app_truth(&opening.app), truth);
    assert!(opening.app.export_operation().is_none());
    assert!(!destination.exists());
    assert_no_export_temp(&tree);
}

#[test]
fn unresolved_capture_and_full_app_worker_queue_reject_without_export_side_effects() {
    let (capture_tree, mut capture, destination) = saved_pattern_harness();
    capture
        .app
        .request_capture_with_frame_limit(sampler_audio::CaptureSource::Resample, 64)
        .unwrap();
    capture.engine.render_frames(0, |_| {});
    let truth = app_truth(&capture.app);
    assert!(matches!(
        capture.app.start_export(destination.clone()),
        Err(OfflineExportError::UnresolvedAppState(_))
    ));
    assert_eq!(app_truth(&capture.app), truth);
    assert!(capture.app.export_operation().is_none());
    assert!(!destination.exists());
    assert_no_export_temp(&capture_tree);

    let (tree, mut busy, destination) = saved_pattern_harness();
    for index in 0..8 {
        busy.app
            .open_picker_at(tree.path(&format!("queued-{index}")));
    }
    let truth = app_truth(&busy.app);
    assert_eq!(
        busy.app.start_export(destination.clone()),
        Err(OfflineExportError::WorkerBusy)
    );
    assert_eq!(app_truth(&busy.app), truth);
    assert!(busy.app.export_operation().is_none());
    assert_eq!(busy.app.take_worker_requests().len(), 8);
    assert!(!destination.exists());
    assert_no_export_temp(&tree);
}

#[test]
fn export_progress_and_terminal_results_are_fenced_by_the_full_admission_tuple() {
    let (_tree, mut harness, destination) = saved_pattern_harness();
    let token = harness.app.start_export(destination.clone()).unwrap();
    let operation = harness.app.export_operation().unwrap().clone();
    harness.app.take_worker_requests();

    assert!(
        !harness
            .app
            .maintain_export(Some(WorkerResult::ExportProgress {
                token,
                fence: Arc::new(ExportResultFence {
                    project_id: sampler_core::ProjectId::from_bytes([99; 16]),
                    revision: operation.revision(),
                    slot: operation.slot(),
                    destination: destination.clone(),
                }),
                completed_units: 3,
                total_units: 8,
            }))
    );
    assert_eq!(
        harness.app.export_operation().unwrap().phase(),
        ExportPhase::Queued
    );

    assert!(
        harness
            .app
            .maintain_export(Some(WorkerResult::ExportProgress {
                token,
                fence: Arc::new(ExportResultFence {
                    project_id: operation.project_id(),
                    revision: operation.revision(),
                    slot: operation.slot(),
                    destination: destination.clone(),
                }),
                completed_units: 3,
                total_units: 8,
            }))
    );
    assert_eq!(
        harness.app.export_operation().unwrap().phase(),
        ExportPhase::Running {
            completed_units: 3,
            total_units: 8,
        }
    );

    let stale_receipt = OfflineExportReceipt {
        token,
        destination: destination.clone(),
        project_id: operation.project_id(),
        revision: operation.revision(),
        slot: sampler_core::PatternSlotId::new(1).unwrap(),
        sample_rate: 48_000,
        rendered_frames: 96_000,
        file_bytes: 768_080,
    };
    assert!(
        !harness
            .app
            .apply_worker_result(WorkerResult::ExportFinished {
                token,
                project_id: operation.project_id(),
                revision: operation.revision(),
                slot: stale_receipt.slot,
                destination: destination.clone(),
                result: Ok(stale_receipt),
            })
    );
    assert!(harness.app.export_operation().is_some());

    assert!(
        harness
            .app
            .apply_worker_result(WorkerResult::ExportFinished {
                token,
                project_id: operation.project_id(),
                revision: operation.revision(),
                slot: operation.slot(),
                destination,
                result: Err(OfflineExportError::Cancelled),
            })
    );
    assert!(harness.app.export_operation().is_none());
}

#[test]
fn export_keeps_admission_revision_while_live_app_advances_to_revision_eight() {
    let tree = FixtureTree::new();
    let source = tree.write_wav("revision-source.wav");
    let project = tree.path("revision-project");
    let destination = tree.path("revision-seven.wav");
    let reference = tree.path("revision-seven-reference.wav");
    let revision_eight_reference = tree.path("revision-eight-reference.wav");
    let gate = Arc::new((Mutex::new((false, false)), Condvar::new()));
    let hook_gate = Arc::clone(&gate);
    let worker = WorkerHandle::spawn_with_project_asset_open_hook(move || {
        assert_eq!(std::thread::current().name(), Some("sampler-loader"));
        let (lock, changed) = &*hook_gate;
        let mut state = lock.lock().unwrap();
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).unwrap();
        }
    });
    let mut harness = Harness::new_with_worker(worker);
    let pad = PadId::new(BankId::new(0).unwrap(), 0).unwrap();
    harness.load(pad, &source);
    harness.record_hit(0);
    let mut level = -1;
    while harness.app.project_revision() < 7 {
        harness.palette(&format!("master-level {level}"));
        level = if level == -1 { -2 } else { -1 };
    }
    assert_eq!(harness.app.project_revision(), 7);
    let now = Instant::now();
    harness.save_as(&project, now);
    if harness.app.maintain_project(now) {
        harness.dispatch_queued();
    }

    let token = harness.app.start_export(destination.clone()).unwrap();
    let mut requests = harness.app.take_worker_requests().into_iter();
    let Some(WorkerRequest::Export(request)) = requests.next() else {
        panic!("expected immutable export request")
    };
    assert!(requests.next().is_none());
    let snapshot = request.snapshot().clone();
    harness
        .worker
        .try_send(WorkerRequest::Export(request))
        .unwrap();
    {
        let (lock, changed) = &*gate;
        let state = lock.lock().unwrap();
        let (state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.0)
            .unwrap();
        assert!(!timeout.timed_out(), "export did not reach source staging");
        drop(state);
    }

    harness.palette("master-level -3");
    assert_eq!(harness.app.project_revision(), 8);
    let revision_eight_project = harness.app.project_snapshot().unwrap();
    let revision_eight_snapshot = OfflineExportSnapshot::from_save_snapshot(
        &std::env::current_dir().unwrap(),
        &revision_eight_project,
        ExportPatternSlot::try_from(snapshot.slot().get() + 1).unwrap(),
    )
    .unwrap();
    {
        let (lock, changed) = &*gate;
        let mut state = lock.lock().unwrap();
        state.1 = true;
        changed.notify_all();
    }
    let terminal = harness.worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(harness.app.apply_worker_result(terminal));

    let cancelled = AtomicBool::new(false);
    let staged = sampler_tui::export::stage_export_samples(&snapshot, &cancelled).unwrap();
    let mut publisher = AtomicWavPublisher::prepare(&reference).unwrap();
    let summary = render_offline(&snapshot, &staged, &mut publisher, &cancelled).unwrap();
    publisher
        .publish(ExportToken::new(999), &snapshot, summary, &cancelled)
        .unwrap();
    let revision_eight_staged =
        sampler_tui::export::stage_export_samples(&revision_eight_snapshot, &cancelled).unwrap();
    let mut revision_eight_publisher =
        AtomicWavPublisher::prepare(&revision_eight_reference).unwrap();
    let revision_eight_summary = render_offline(
        &revision_eight_snapshot,
        &revision_eight_staged,
        &mut revision_eight_publisher,
        &cancelled,
    )
    .unwrap();
    revision_eight_publisher
        .publish(
            ExportToken::new(998),
            &revision_eight_snapshot,
            revision_eight_summary,
            &cancelled,
        )
        .unwrap();

    assert_eq!(
        fs::read(&destination).unwrap(),
        fs::read(&reference).unwrap()
    );
    assert_ne!(
        fs::read(&destination).unwrap(),
        fs::read(&revision_eight_reference).unwrap()
    );
    assert_eq!(harness.app.project_revision(), 8);
    assert!(harness.app.status().contains("revision 7"));
    assert!(matches!(
        harness.app.export_status_view(),
        Some(ExportStatusView::Completed { receipt })
            if receipt.token == token && receipt.revision == 7
    ));
}

#[test]
fn escape_cancels_only_a_focused_export_status_and_waits_for_terminal_ack() {
    let (_tree, mut focused, destination) = saved_pattern_harness();
    let token = focused.app.start_export(destination.clone()).unwrap();
    let requests = focused.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = requests.as_slice() else {
        panic!("expected export request")
    };
    let cancel = request.cancellation();
    focused
        .app
        .apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(cancel.is_cancelled());
    assert_eq!(
        focused.app.export_operation().unwrap().phase(),
        ExportPhase::Cancelling
    );
    assert!(focused.app.export_operation().is_some());

    let operation = focused.app.export_operation().unwrap().clone();
    assert!(
        focused
            .app
            .maintain_export(Some(WorkerResult::ExportProgress {
                token,
                fence: Arc::new(operation.result_fence()),
                completed_units: 1,
                total_units: 2,
            }))
    );
    assert_eq!(
        focused.app.export_operation().unwrap().phase(),
        ExportPhase::Cancelling
    );

    assert!(
        focused
            .app
            .apply_worker_result(WorkerResult::ExportFinished {
                token,
                project_id: operation.project_id(),
                revision: operation.revision(),
                slot: operation.slot(),
                destination,
                result: Err(OfflineExportError::Cancelled),
            })
    );
    assert!(focused.app.export_operation().is_none());

    let (_tree, mut obscured, destination) = saved_pattern_harness();
    obscured.app.start_export(destination).unwrap();
    let requests = obscured.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = requests.as_slice() else {
        panic!("expected export request")
    };
    let cancel = request.cancellation();
    obscured.app.open_help();
    obscured
        .app
        .apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    obscured
        .app
        .apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!cancel.is_cancelled());
    assert_eq!(
        obscured.app.export_operation().unwrap().phase(),
        ExportPhase::Queued
    );
}

#[test]
fn quit_and_open_cancel_export_then_continue_only_after_matching_cleanup_ack() {
    let (_tree, mut quitting, destination) = saved_pattern_harness();
    let token = quitting.app.start_export(destination.clone()).unwrap();
    let requests = quitting.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = requests.as_slice() else {
        panic!("expected export request")
    };
    let cancel = request.cancellation();
    let operation = quitting.app.export_operation().unwrap().clone();
    quitting.app.apply(InputAction::Quit);
    assert!(cancel.is_cancelled());
    assert!(!quitting.app.should_quit());
    assert!(
        quitting
            .app
            .apply_worker_result(WorkerResult::ExportFinished {
                token,
                project_id: operation.project_id(),
                revision: operation.revision(),
                slot: operation.slot(),
                destination,
                result: Err(OfflineExportError::Cancelled),
            })
    );
    assert!(quitting.app.should_quit());

    let (tree, mut opening, destination) = saved_pattern_harness();
    let token = opening.app.start_export(destination.clone()).unwrap();
    let requests = opening.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = requests.as_slice() else {
        panic!("expected export request")
    };
    let cancel = request.cancellation();
    let operation = opening.app.export_operation().unwrap().clone();
    let next_project = tree.path("next-project");
    opening
        .app
        .request_open_project_interactive(next_project.clone());
    assert!(cancel.is_cancelled());
    assert!(opening.app.project_open_stage().is_none());
    assert!(
        opening
            .app
            .apply_worker_result(WorkerResult::ExportFinished {
                token,
                project_id: operation.project_id(),
                revision: operation.revision(),
                slot: operation.slot(),
                destination,
                result: Err(OfflineExportError::Cancelled),
            })
    );
    assert_eq!(
        opening.app.project_open_stage().unwrap().directory,
        next_project
    );
    assert!(matches!(
        opening.app.take_worker_requests().as_slice(),
        [WorkerRequest::ProbeProject { directory, .. }] if directory == &next_project
    ));

    let (tree, mut direct, destination) = saved_pattern_harness();
    direct.app.start_export(destination).unwrap();
    let requests = direct.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = requests.as_slice() else {
        panic!("expected export request")
    };
    let cancel = request.cancellation();
    assert_eq!(
        direct.app.request_open_project(tree.path("direct-open")),
        Err(ProjectOpenError::OperationPending)
    );
    assert!(cancel.is_cancelled());
    assert_eq!(
        direct.app.export_operation().unwrap().phase(),
        ExportPhase::Cancelling
    );
    assert!(direct.app.take_worker_requests().is_empty());
}

#[test]
fn palette_export_uses_the_same_atomic_app_admission() {
    let (_tree, mut harness, destination) = saved_pattern_harness();
    harness.palette(&format!("export {}", destination.display()));

    assert!(harness.app.export_operation().is_some());
    assert!(matches!(
        harness.app.take_worker_requests().as_slice(),
        [WorkerRequest::Export(request)] if request.destination() == destination
    ));
}
