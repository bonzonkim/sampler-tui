use std::cell::RefCell;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags};
use hound::{SampleFormat, WavSpec, WavWriter};
use sampler_audio::{AudioEngine, PatternSwitch, audio_channels};
use sampler_core::{BankId, PadId, PlaybackMode, SAMPLE_PHASE_SCALE, SampleEditRecipe};
use sampler_tui::export::{StagedExportPad, stage_export_samples};
use sampler_tui::terminal::{
    KeyboardEnhancementOps, TerminalLifecycle, run_with_runtime_lifecycle,
};
use sampler_tui::{
    ExportStatusView, InputAction, OfflineExportSnapshot, ProjectStore, WorkerHandle,
    WorkerRequest, parse_midi_message,
};

#[path = "support/mixer_harness.rs"]
mod mixer_harness;

use mixer_harness::{FixtureTree, Harness};

#[derive(Default)]
struct ExportPause {
    armed: bool,
    reached: bool,
    released: bool,
}

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

fn write_long_wav(tree: &FixtureTree, name: &str) -> PathBuf {
    let path = tree.path(name);
    let mut writer = WavWriter::create(
        &path,
        WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        },
    )
    .unwrap();
    for frame in 0..16_384_u32 {
        let phase = (frame % 64) as f32 / 63.0;
        writer.write_sample(0.2 + phase * 0.3).unwrap();
        writer.write_sample(-0.15 - phase * 0.25).unwrap();
    }
    writer.finalize().unwrap();
    path
}

fn set_selected_mode(harness: &mut Harness, command: &str) {
    harness
        .app
        .apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    harness
        .app
        .apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    harness.palette(command);
    harness.app.close_overlay();
    harness
        .app
        .apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    harness
        .app
        .apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
}

fn independent_engine_bits(snapshot: &OfflineExportSnapshot) -> Vec<[u32; 2]> {
    let staged = stage_export_samples(snapshot, &AtomicBool::new(false)).unwrap();
    let (_controller, mut engine) = independent_engine(snapshot, &staged);
    let mut bits = Vec::with_capacity(snapshot.loop_frames().unwrap() as usize);
    engine.render_frames(snapshot.loop_frames().unwrap() as usize, |frame| {
        bits.push([frame[0].to_bits(), frame[1].to_bits()]);
    });
    bits
}

fn independent_engine(
    snapshot: &OfflineExportSnapshot,
    staged: &[StagedExportPad],
) -> (sampler_audio::AudioController, AudioEngine) {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(sampler_tui::EXPORT_SAMPLE_RATE, ports).unwrap();
    for staged_pad in staged {
        controller
            .install(
                staged_pad.pad,
                Arc::clone(&staged_pad.sample),
                staged_pad.settings,
                staged_pad.mix,
            )
            .unwrap();
        engine.render_frames(0, |_| {});
    }
    controller.update_master_mix(snapshot.master_mix()).unwrap();
    engine.render_frames(0, |_| {});
    controller
        .install_pattern(Arc::new(
            snapshot.pattern().to_editable().unwrap().compile().unwrap(),
        ))
        .unwrap();
    controller
        .select_pattern(snapshot.slot(), PatternSwitch::Immediate)
        .unwrap();
    controller.play_pattern().unwrap();
    engine.render_frames(0, |_| {});
    (controller, engine)
}

fn decoded_wav_bits(path: &Path) -> Vec<[u32; 2]> {
    let mut reader = hound::WavReader::open(path).unwrap();
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate, 48_000);
    assert_eq!(reader.spec().bits_per_sample, 32);
    assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
    let samples = reader
        .samples::<f32>()
        .map(|sample| sample.unwrap())
        .collect::<Vec<_>>();
    samples
        .chunks_exact(2)
        .map(|frame| [frame[0].to_bits(), frame[1].to_bits()])
        .collect()
}

fn temporary_entries(root: &Path) -> Vec<PathBuf> {
    fn visit(directory: &Path, output: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, output);
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("sampler-tui-tmp"))
            {
                output.push(path);
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, &mut entries);
    entries
}

fn owned_temporary_entries(destination: &Path) -> Vec<PathBuf> {
    let leaf = destination.file_name().unwrap().to_string_lossy();
    let prefix = format!(".{leaf}.sampler-tui-tmp-");
    fs::read_dir(destination.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect()
}

fn wait_for_owned_temporary(destination: &Path, timeout: Duration) -> PathBuf {
    let deadline = Instant::now() + timeout;
    loop {
        let entries = owned_temporary_entries(destination);
        if let [temporary] = entries.as_slice() {
            return temporary.clone();
        }
        assert!(entries.is_empty(), "more than one owned export temporary");
        assert!(
            !destination.exists(),
            "export completed before cancellation observation"
        );
        assert!(
            Instant::now() < deadline,
            "publisher temporary did not appear before timeout"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[derive(Clone)]
struct RestoreKeyboard {
    calls: Rc<RefCell<Vec<&'static str>>>,
}

impl KeyboardEnhancementOps for RestoreKeyboard {
    fn supports_keyboard_enhancement(&self) -> io::Result<bool> {
        Ok(true)
    }

    fn push_keyboard_enhancement(&self, _flags: KeyboardEnhancementFlags) -> io::Result<()> {
        self.calls.borrow_mut().push("keys-on");
        Ok(())
    }

    fn pop_keyboard_enhancement(&self) -> io::Result<()> {
        self.calls.borrow_mut().push("keys-off");
        Ok(())
    }
}

struct RestoreTerminal {
    calls: Rc<RefCell<Vec<&'static str>>>,
}

impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        self.calls.borrow_mut().push("drop-terminal");
    }
}

struct RestoreLifecycle {
    calls: Rc<RefCell<Vec<&'static str>>>,
}

impl TerminalLifecycle for RestoreLifecycle {
    type Terminal = RestoreTerminal;

    fn initialize(&mut self) -> io::Result<Self::Terminal> {
        self.calls.borrow_mut().push("terminal-on");
        Ok(RestoreTerminal {
            calls: Rc::clone(&self.calls),
        })
    }

    fn show_cursor(&mut self, _terminal: &mut Self::Terminal) -> io::Result<()> {
        self.calls.borrow_mut().push("show-cursor");
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        self.calls.borrow_mut().push("terminal-off");
        Ok(())
    }
}

#[test]
fn continuous_real_app_worker_store_engine_and_headless_export_workflow() {
    let tree = FixtureTree::new();
    let gate_source = write_long_wav(&tree, "gate-source.wav");
    let one_shot_source = write_long_wav(&tree, "one-shot-source.wav");
    let first_destination = tree.path("revision-n.wav");
    let project = tree.path("saved-project");
    let moved_project = tree.path("moved-project");
    let headless_destination = tree.path("headless.wav");
    let cancelled_destination = tree.path("cancelled.wav");
    let pause = Arc::new((Mutex::new(ExportPause::default()), Condvar::new()));
    let hook_pause = Arc::clone(&pause);
    let worker = WorkerHandle::spawn_with_project_asset_open_hook(move || {
        let (lock, changed) = &*hook_pause;
        let mut state = lock.lock().unwrap();
        if !state.armed {
            return;
        }
        state.reached = true;
        changed.notify_all();
        while !state.released {
            let (next, timeout) = changed.wait_timeout(state, Duration::from_secs(5)).unwrap();
            state = next;
            if timeout.timed_out() {
                break;
            }
        }
    });
    let mut harness = Harness::new_with_worker(worker);

    harness.load(pad(0), &gate_source);
    harness.edit(
        pad(0),
        SampleEditRecipe::new(0, SAMPLE_PHASE_SCALE, true, true).unwrap(),
    );
    set_selected_mode(&mut harness, "mode gate");
    harness.palette("delay-send 0.75");
    harness.palette("select 2");
    harness.load(pad(1), &one_shot_source);
    set_selected_mode(&mut harness, "mode oneshot");
    harness.palette("reverb-send 0.7");
    harness.palette("master-level -1");
    harness.palette("delay-enable on");
    harness.palette("delay-time 10");
    harness.palette("delay-feedback 0.4");
    harness.palette("delay-return -6");
    harness.palette("reverb-enable on");
    harness.palette("reverb-room 0.8");
    harness.palette("reverb-damping 0.35");
    harness.palette("reverb-return -9");
    harness.record_hit(0);
    harness.engine.render_frames(12_000, |_| {});
    harness.app.tick();
    harness.app.maintain_audio();
    harness.record_hit(1);
    assert_eq!(harness.app.patterns().selected_pattern().events().len(), 2);

    let revision_n = harness.app.project_revision();
    {
        let (lock, _) = &*pause;
        lock.lock().unwrap().armed = true;
    }

    let token = harness.app.start_export(first_destination.clone()).unwrap();
    let export_requests = harness.app.take_worker_requests();
    let [WorkerRequest::Export(request)] = export_requests.as_slice() else {
        panic!("expected one immutable export request")
    };
    let request = request.clone();
    let revision_n_snapshot = request.snapshot().clone();
    assert_eq!(revision_n_snapshot.revision(), revision_n);
    assert_eq!(
        revision_n_snapshot
            .pads()
            .iter()
            .find(|source| source.pad == pad(0))
            .unwrap()
            .settings
            .mode,
        PlaybackMode::Gate
    );
    assert_eq!(
        revision_n_snapshot
            .pads()
            .iter()
            .find(|source| source.pad == pad(1))
            .unwrap()
            .settings
            .mode,
        PlaybackMode::OneShot
    );
    assert!(revision_n_snapshot.master_mix().delay.enabled);
    assert!(revision_n_snapshot.master_mix().reverb.enabled);
    assert_eq!(revision_n_snapshot.pattern().events.len(), 2);
    harness
        .worker
        .try_send(WorkerRequest::Export(request))
        .unwrap();
    {
        let (lock, changed) = &*pause;
        let state = lock.lock().unwrap();
        let (state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.reached)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "export never reached committed-source staging"
        );
        drop(state);
    }

    if let Some(progress) = harness.worker.try_recv_export_progress() {
        assert!(harness.app.maintain_export(Some(progress)));
    }
    let wet_master = harness.app.master_mix();
    let mut dry_probe_master = wet_master;
    dry_probe_master.delay.enabled = false;
    dry_probe_master.reverb.enabled = false;
    harness
        .controller
        .borrow_mut()
        .update_master_mix(dry_probe_master)
        .unwrap();
    harness.controller.borrow_mut().stop_pattern().unwrap();
    harness.app.apply(InputAction::StopAll);
    harness.engine.render_frames(128, |_| {});
    let before_live = harness.engine.rendered_frame();

    let mut gate_held_peak = 0.0_f32;
    harness.app.apply(InputAction::PadPress(0));
    harness.engine.render_frames(128, |frame| {
        gate_held_peak = gate_held_peak.max(frame[0].abs()).max(frame[1].abs());
    });
    harness.app.apply(InputAction::PadRelease(0));
    harness.engine.render_frames(128, |_| {});
    let mut gate_released_peak = 0.0_f32;
    harness.engine.render_frames(128, |frame| {
        gate_released_peak = gate_released_peak.max(frame[0].abs()).max(frame[1].abs());
    });
    assert!(gate_held_peak > 0.0, "Gate pad must sound while held");
    assert_eq!(gate_released_peak, 0.0, "Gate pad must stop on release");

    let mut one_shot_held_peak = 0.0_f32;
    harness
        .app
        .apply_midi_event(parse_midi_message(&[0x90, 37, 100]).unwrap());
    harness.engine.render_frames(128, |frame| {
        one_shot_held_peak = one_shot_held_peak.max(frame[0].abs()).max(frame[1].abs());
    });
    harness
        .app
        .apply_midi_event(parse_midi_message(&[0x80, 37, 0]).unwrap());
    harness.engine.render_frames(128, |_| {});
    let mut one_shot_released_peak = 0.0_f32;
    harness.engine.render_frames(128, |frame| {
        one_shot_released_peak = one_shot_released_peak
            .max(frame[0].abs())
            .max(frame[1].abs());
    });
    assert!(
        one_shot_held_peak > 0.0,
        "parsed MIDI OneShot must sound while held"
    );
    assert!(
        one_shot_released_peak > 0.0,
        "OneShot must continue after parsed MIDI Note Off"
    );
    assert!(harness.engine.rendered_frame() >= before_live + 768);
    harness
        .controller
        .borrow_mut()
        .update_master_mix(wet_master)
        .unwrap();
    harness.engine.render_frames(0, |_| {});
    assert_eq!(harness.app.project_revision(), revision_n);
    harness.palette("master-level -3");
    assert_eq!(harness.app.project_revision(), revision_n + 1);

    {
        let (lock, changed) = &*pause;
        let mut state = lock.lock().unwrap();
        state.released = true;
        changed.notify_all();
    }
    let terminal = harness
        .worker
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    assert!(harness.app.apply_worker_result(terminal));
    let receipt = match harness.app.export_status_view() {
        Some(ExportStatusView::Completed { receipt }) => receipt,
        status => panic!("expected successful revision-N export, got {status:?}"),
    };
    assert_eq!(receipt.token, token);
    assert_eq!(receipt.revision, revision_n);
    assert_eq!(harness.app.project_revision(), revision_n + 1);
    let expected_bits = independent_engine_bits(&revision_n_snapshot);
    assert_eq!(decoded_wav_bits(&first_destination), expected_bits);

    let now = Instant::now();
    harness.save_as(&project, now);
    if harness.app.maintain_project(now) {
        harness.dispatch_queued();
    }
    fs::rename(&project, &moved_project).unwrap();
    let headless = Command::new(env!("CARGO_BIN_EXE_sampler-tui"))
        .arg("export")
        .arg(&moved_project)
        .arg("1")
        .arg(&headless_destination)
        .output()
        .unwrap();
    assert!(
        headless.status.success(),
        "headless process failed: {}",
        String::from_utf8_lossy(&headless.stderr)
    );
    let headless_stdout = String::from_utf8(headless.stdout).unwrap();
    assert!(headless_stdout.contains("pattern=1 rate=48000"));
    assert!(headless_stdout.contains(&format!("revision={}", revision_n + 1)));
    assert!(!decoded_wav_bits(&headless_destination).is_empty());

    drop(harness);
    let mut reopened = Harness::new();
    reopened.open(&moved_project, None, now + Duration::from_secs(1));
    reopened.palette("tempo 20");
    reopened.palette("bars 8");
    let cancellation_revision = reopened.app.project_revision();
    let cancel_token = reopened
        .app
        .start_export(cancelled_destination.clone())
        .unwrap();
    let cancel_requests = reopened.app.take_worker_requests();
    let [WorkerRequest::Export(cancel_request)] = cancel_requests.as_slice() else {
        panic!("expected cancellable export request")
    };
    let cancel_request = cancel_request.clone();
    reopened
        .worker
        .try_send(WorkerRequest::Export(cancel_request))
        .unwrap();
    let observed_temporary =
        wait_for_owned_temporary(&cancelled_destination, Duration::from_secs(5));
    assert!(observed_temporary.exists());
    assert!(reopened.app.cancel_export());
    assert_eq!(cancel_token.get(), 1);

    let calls = Rc::new(RefCell::new(Vec::new()));
    let mut lifecycle = RestoreLifecycle {
        calls: Rc::clone(&calls),
    };
    let result: Result<(), Box<dyn Error>> = run_with_runtime_lifecycle(
        &mut reopened.app,
        RestoreKeyboard {
            calls: Rc::clone(&calls),
        },
        &mut lifecycle,
        &mut reopened.worker,
        |_, _, release_events, _| {
            assert!(release_events);
            calls.borrow_mut().push("run");
            Ok(())
        },
        |worker| {
            calls.borrow_mut().push("join-worker");
            worker
                .shutdown()
                .map_err(|error| Box::new(error) as Box<dyn Error>)
        },
        |_| panic!("workflow lifecycle must not panic"),
    );
    result.unwrap();
    assert_eq!(
        *calls.borrow(),
        [
            "terminal-on",
            "keys-on",
            "run",
            "join-worker",
            "keys-off",
            "show-cursor",
            "drop-terminal",
            "terminal-off",
        ]
    );
    assert!(!cancelled_destination.exists());
    assert!(!observed_temporary.exists());
    assert!(owned_temporary_entries(&cancelled_destination).is_empty());
    assert!(first_destination.exists());
    assert!(headless_destination.exists());
    assert!(temporary_entries(&tree.path(".")).is_empty());
    assert!(cancellation_revision > revision_n + 1);

    let probe = ProjectStore.probe(&moved_project).unwrap();
    assert_eq!(probe.explicit.unwrap().unwrap().revision, revision_n + 1);
}
