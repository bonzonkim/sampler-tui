//! Full-stack sample editing evidence. WAV decode and recipe rendering use the real worker,
//! commands use the real controller, and auditioning uses a real `AudioEngine`; physical device
//! I/O is substituted. The port also exposes explicit runtime-device and general-install rejection
//! controls, while worker-busy injection exercises App's typed send-error boundary.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hound::{SampleFormat, WavSpec, WavWriter};
use ratatui::{Terminal, backend::TestBackend};
use sampler_audio::{
    AudioController, AudioEngine, Frame, LiveAck, LiveCommandId, PatternSnapshotSlot,
    PatternSwitch, SampleBuffer, SampleSlot, Telemetry, audio_channels,
    audio_channels_with_test_capacities,
};
use sampler_core::{
    BankId, PadId, PadSettings, PatternSlotId, PatternSnapshot, PlaybackMode, SAMPLE_PHASE_SCALE,
    SampleEditRecipe,
};
use sampler_tui::{
    App, AudioPort, CaptureSupport, InputAction, KeyboardCapabilities, Overlay,
    SampleEditRequestError, SampleEditStatus, WorkerHandle, WorkerRequest, WorkerSendError,
};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
const EXPECTED_RECOVERY_CREDITS: usize = 32;

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
}

impl Fixture {
    fn asymmetric_16_frames() -> Self {
        let frames = [
            [0.0625, -0.125],
            [0.25, -0.375],
            [0.5, -0.75],
            [-0.875, 0.125],
            [0.375, -0.25],
            [-0.125, 0.625],
            [0.75, -0.5],
            [-0.25, 0.0625],
            [0.125, -0.375],
            [-0.625, 0.25],
            [0.5, 0.875],
            [-0.75, 0.125],
            [0.25, -0.5],
            [-0.375, 0.75],
            [0.625, -0.125],
            [-0.5, 0.375],
        ];
        Self::write("asymmetric.wav", 48_000, &frames)
    }

    fn rate_mapping_48_000_frames() -> Self {
        let frames = (0..48_000)
            .map(|index| {
                let position = index as f32 / 47_999.0;
                [-0.8 + 1.6 * position, 0.7 - 1.4 * position]
            })
            .collect::<Vec<_>>();
        Self::write("absolute-position.wav", 48_000, &frames)
    }

    fn identity_48_000_frames() -> Self {
        Self::write("identity.wav", 48_000, &vec![[0.5, 0.25]; 48_000])
    }

    fn nonperiodic_impulses_1_024_frames() -> Self {
        let mut frames = vec![[0.0, 0.0]; 1_024];
        frames[300] = [1.0, 1.0];
        frames[700] = [-1.0, -1.0];
        Self::write("nonperiodic-impulses.wav", 48_000, &frames)
    }

    fn write(name: &str, sample_rate: u32, frames: &[[f32; 2]]) -> Self {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "sampler-tui-sample-edit-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("unique fixture directory");
        let path = directory.join(name);
        let mut writer = WavWriter::create(
            &path,
            WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )
        .expect("create WAV fixture");
        for frame in frames {
            writer.write_sample(frame[0]).expect("write left sample");
            writer.write_sample(frame[1]).expect("write right sample");
        }
        writer.finalize().expect("finalize WAV fixture");
        Self { directory, path }
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(&self.path).expect("read fixture")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

#[derive(Default)]
struct ProbeState {
    runtime_failure: Cell<bool>,
    reject_install: Cell<bool>,
    reclaimed: Cell<usize>,
    updates: RefCell<Vec<PadSettings>>,
}

struct ControllerPort {
    sample_rate: u32,
    controller: Rc<RefCell<AudioController>>,
    probe: Rc<ProbeState>,
}

impl ControllerPort {
    fn controller(&self) -> std::cell::RefMut<'_, AudioController> {
        self.controller.borrow_mut()
    }
}

impl AudioPort for ControllerPort {
    fn capture_support(&self) -> CaptureSupport {
        CaptureSupport::Unsupported
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        2
    }

    fn render_horizon(&self) -> Frame {
        self.controller.borrow().render_horizon()
    }

    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: sampler_core::PadMixSettings,
    ) -> Result<SampleSlot, String> {
        if self.probe.reject_install.get() {
            return Err("test audio queue full".to_owned());
        }
        self.controller()
            .install(pad, sample, settings, mix)
            .map_err(|error| error.to_string())
    }

    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: sampler_core::PadMixSettings,
    ) -> Result<SampleSlot, String> {
        self.controller()
            .install_recovery(pad, sample, settings, mix)
            .map_err(|error| error.to_string())
    }

    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
        self.controller()
            .trigger(pad, at, velocity)
            .map_err(|error| error.to_string())
    }

    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
        self.controller()
            .release(pad, at)
            .map_err(|error| error.to_string())
    }

    fn trigger_live_tracked(&mut self, pad: PadId, velocity: f32) -> Result<LiveCommandId, String> {
        self.controller()
            .trigger_live_tracked(pad, velocity)
            .map_err(|error| error.to_string())
    }

    fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, String> {
        self.controller()
            .release_live_tracked(pad)
            .map_err(|error| error.to_string())
    }

    fn install_pattern(
        &mut self,
        snapshot: Arc<PatternSnapshot>,
    ) -> Result<PatternSnapshotSlot, String> {
        self.controller()
            .install_pattern(snapshot)
            .map_err(|error| error.to_string())
    }

    fn select_pattern(&mut self, slot: PatternSlotId, switch: PatternSwitch) -> Result<(), String> {
        self.controller()
            .select_pattern(slot, switch)
            .map_err(|error| error.to_string())
    }

    fn play_pattern(&mut self) -> Result<(), String> {
        self.controller()
            .play_pattern()
            .map_err(|error| error.to_string())
    }

    fn stop_pattern(&mut self) -> Result<(), String> {
        self.controller()
            .stop_pattern()
            .map_err(|error| error.to_string())
    }

    fn set_record_capture(&mut self, capture: Option<(PatternSlotId, u64)>) -> Result<(), String> {
        self.controller()
            .set_record_capture(capture)
            .map_err(|error| error.to_string())
    }

    fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
        self.controller().drain_live_acks(output)
    }

    fn reclaim_retired_patterns(&mut self) -> usize {
        let mut reclaimed = 0;
        while self.controller().reclaim_retired_pattern().is_some() {
            reclaimed += 1;
        }
        reclaimed
    }

    fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
        self.controller()
            .stop_pad(pad)
            .map_err(|error| error.to_string())
    }

    fn stop_all(&mut self) -> Result<(), String> {
        self.controller()
            .stop_all()
            .map_err(|error| error.to_string())
    }

    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        self.controller()
            .update_pad(pad, settings)
            .map_err(|error| error.to_string())?;
        self.probe.updates.borrow_mut().push(settings);
        Ok(())
    }

    fn update_pad_mix(
        &mut self,
        pad: PadId,
        settings: sampler_core::PadMixSettings,
    ) -> Result<(), String> {
        self.controller()
            .update_pad_mix(pad, settings)
            .map_err(|error| error.to_string())
    }

    fn update_master_mix(
        &mut self,
        settings: sampler_core::MasterMixSettings,
    ) -> Result<(), String> {
        self.controller()
            .update_master_mix(settings)
            .map_err(|error| error.to_string())
    }

    fn reclaim_retired(&mut self) -> usize {
        let reclaimed = self.controller().reclaim_retired();
        self.probe
            .reclaimed
            .set(self.probe.reclaimed.get().saturating_add(reclaimed));
        reclaimed
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.controller().latest_telemetry()
    }

    fn poll_runtime_error(&mut self) -> Option<String> {
        self.probe
            .runtime_failure
            .replace(false)
            .then_some("test device disconnected".to_owned())
    }
}

struct Harness {
    app: App,
    engine: AudioEngine,
    probe: Rc<ProbeState>,
    worker: WorkerHandle,
}

impl Harness {
    fn new(sample_rate: u32) -> Self {
        let (port, engine, probe) = audio_pair(sample_rate);
        let mut app = App::with_audio(Box::new(port));
        app.set_keyboard_capabilities(KeyboardCapabilities {
            release_events: true,
        });
        Self {
            app,
            engine,
            probe,
            worker: WorkerHandle::spawn(),
        }
    }

    fn load(&mut self, pad: PadId, path: &Path) {
        let request = self
            .app
            .begin_load(pad, path)
            .expect("audio-backed load request");
        self.worker.try_send(request).expect("worker accepts load");
        let result = self
            .worker
            .recv_timeout(Duration::from_secs(5))
            .expect("worker load result");
        assert!(self.app.apply_worker_result(result));
        self.engine.render_frames(0, |_| {});
    }

    fn process_one_queued_request(&mut self) -> bool {
        let requests = self.app.take_worker_requests();
        let [request] = requests.as_slice() else {
            panic!("expected one queued request, got {}", requests.len());
        };
        self.worker
            .try_send(request.clone())
            .expect("worker accepts request");
        let result = self
            .worker
            .recv_timeout(Duration::from_secs(5))
            .expect("worker result");
        self.app.apply_worker_result(result)
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app.apply_key(KeyEvent::new_with_kind(
            code,
            modifiers,
            KeyEventKind::Press,
        ));
    }

    fn palette(&mut self, command: &str) {
        self.key(KeyCode::Char(':'), KeyModifiers::SHIFT);
        self.app
            .apply_terminal_event(Event::Paste(command.to_owned()));
        self.key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(self.app.palette_error(), None, "palette command: {command}");
        if self.app.overlay() == Some(&Overlay::Palette) {
            self.key(KeyCode::Esc, KeyModifiers::NONE);
        }
    }

    fn enter_sample(&mut self) {
        self.key(KeyCode::Tab, KeyModifiers::NONE);
        self.key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(
            self.app.workspace_view(),
            sampler_tui::WorkspaceView::Sample
        );
    }

    fn screen(&self) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sampler_tui::ui::render(frame, &self.app))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..24)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen_symbol(&self, x: u16, y: u16) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| sampler_tui::ui::render(frame, &self.app))
            .expect("draw");
        terminal.backend().buffer()[(x, y)].symbol().to_owned()
    }
}

fn audio_pair(sample_rate: u32) -> (ControllerPort, AudioEngine, Rc<ProbeState>) {
    let (controller, ports) = audio_channels();
    let controller = Rc::new(RefCell::new(controller));
    let probe = Rc::new(ProbeState::default());
    let engine = AudioEngine::new(sample_rate, ports).expect("valid engine");
    (
        ControllerPort {
            sample_rate,
            controller,
            probe: Rc::clone(&probe),
        },
        engine,
        probe,
    )
}

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

fn stereo_frame(sample: &SampleBuffer, frame: usize) -> [f32; 2] {
    [sample.data()[frame * 2], sample.data()[frame * 2 + 1]]
}

fn first_callback_frame(sample: [f32; 2]) -> [f32; 2] {
    let pan_gain = (std::f32::consts::PI / 4.0).cos();
    sample.map(|value| {
        let value = value * pan_gain / 32.0;
        value / (1.0 + value.abs())
    })
}

#[test]
fn recovery_port_uses_the_controller_recovery_admission_limit() {
    let (controller, _ports) = audio_channels_with_test_capacities(128, 256, 8);
    let probe = Rc::new(ProbeState::default());
    let mut port = ControllerPort {
        sample_rate: 48_000,
        controller: Rc::new(RefCell::new(controller)),
        probe,
    };

    for _ in 0..EXPECTED_RECOVERY_CREDITS {
        port.install_recovery(
            pad(0),
            Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
            PadSettings::default(),
            sampler_core::PadMixSettings::default(),
        )
        .expect("recovery credit available");
    }
    assert!(
        port.install_recovery(
            pad(0),
            Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
            PadSettings::default(),
            sampler_core::PadMixSettings::default(),
        )
        .is_err(),
        "the 33rd recovery must exhaust the controller's reserved recovery credits"
    );
    port.install(
        pad(1),
        Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
        PadSettings::default(),
        sampler_core::PadMixSettings::default(),
    )
    .expect("recovery credits do not consume ordinary install capacity");
}

#[test]
fn real_worker_apply_preserves_source_and_auditions_the_replaced_buffer() {
    let fixture = Fixture::asymmetric_16_frames();
    let source_before = fixture.bytes();
    let source_path = fixture.path.clone();
    let mut harness = Harness::new(48_000);
    let pad = pad(0);
    harness.load(pad, &fixture.path);
    harness.enter_sample();

    harness.app.apply(InputAction::PadPress(0));
    harness.engine.render_frames(1, |_| {});
    assert_eq!(harness.engine.active_voices(), 1);
    harness.app.apply(InputAction::PadRelease(0));

    harness.palette("trim-start 4");
    harness.palette("trim-end 12");
    harness.palette("reverse on");
    harness.palette("normalize on");
    harness.palette("apply-sample");
    assert!(matches!(
        harness.app.overlay(),
        Some(Overlay::ApplySample { .. })
    ));
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(harness.process_one_queued_request());
    assert!(harness.app.maintain_audio());

    let rendered = harness.app.pad(pad).sample.as_ref().expect("edited sample");
    let target = 10_f32.powf(-1.0 / 20.0);
    let gain = target / 0.875;
    assert_eq!(rendered.frames(), 8);
    assert!((rendered.data()[0] - -0.75 * gain).abs() < 1e-6);
    assert!((rendered.data()[1] - 0.125 * gain).abs() < 1e-6);
    assert!((rendered.data()[14] - 0.375 * gain).abs() < 1e-6);
    assert!((rendered.data()[15] - -0.25 * gain).abs() < 1e-6);
    let peak = rendered
        .data()
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0_f32, f32::max);
    assert!((peak - target).abs() < 1e-6);

    let mut old_voice_frame = [0.0; 2];
    harness.engine.render_stereo(&mut old_voice_frame);
    assert!(old_voice_frame[0] > 0.0 && old_voice_frame[1] < 0.0);
    harness.app.maintain_audio();
    assert_eq!(harness.probe.reclaimed.get(), 0);
    harness.engine.render_frames(32, |_| {});
    harness.app.maintain_audio();
    assert_eq!(harness.probe.reclaimed.get(), 1);

    harness.app.apply(InputAction::PadPress(0));
    let mut audition = Vec::new();
    harness
        .engine
        .render_frames(1, |frame| audition.push(frame));
    let edited_first = audition.last().copied().expect("audition frame");
    assert!(edited_first[0] < 0.0 && edited_first[1] > 0.0);
    assert_eq!(harness.engine.executed_triggers(), 2);

    assert_eq!(harness.app.status(), "Applied sample edit");
    let screen = harness.screen();
    assert!(screen.contains("APPLIED"), "{screen}");
    assert_eq!(
        harness.app.pad(pad).source.as_deref(),
        Some(source_path.as_path())
    );
    assert_eq!(fixture.bytes(), source_before);
    assert_eq!(
        harness.app.committed_sample_recipe(pad),
        Some(
            SampleEditRecipe::new(
                SAMPLE_PHASE_SCALE / 4,
                SAMPLE_PHASE_SCALE * 3 / 4,
                true,
                true,
            )
            .unwrap()
        )
    );
}

#[test]
fn settings_backpressure_stale_results_and_one_level_undo_are_failure_atomic() {
    let fixture = Fixture::asymmetric_16_frames();
    let source_before = fixture.bytes();
    let mut harness = Harness::new(48_000);
    let pad = pad(0);
    harness.load(pad, &fixture.path);
    harness.enter_sample();

    harness.palette("mode gate");
    harness.palette("pitch -12");
    harness.palette("mode loop");
    harness.palette("mode oneshot");
    harness.engine.render_frames(0, |_| {});
    let admitted = harness.probe.updates.borrow();
    assert_eq!(admitted.len(), 4);
    assert_eq!(admitted[0].mode, PlaybackMode::Gate);
    assert_eq!(admitted[1].pitch_semitones, -12.0);
    assert_eq!(admitted[2].mode, PlaybackMode::Loop);
    assert_eq!(admitted[3].mode, PlaybackMode::OneShot);
    drop(admitted);
    assert_eq!(harness.app.pad(pad).settings.pitch_semitones, -12.0);

    harness.palette("reverse on");
    harness.palette("apply-sample");
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(harness.process_one_queued_request());
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});
    let first_recipe = harness.app.committed_sample_recipe(pad).unwrap();

    harness.palette("normalize on");
    harness.palette("apply-sample");
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(harness.process_one_queued_request());
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});
    let second_recipe = harness.app.committed_sample_recipe(pad).unwrap();
    assert_ne!(second_recipe, first_recipe);

    let before_busy = Arc::clone(harness.app.pad(pad).sample.as_ref().unwrap());
    let busy_recipe =
        SampleEditRecipe::new(SAMPLE_PHASE_SCALE / 4, SAMPLE_PHASE_SCALE, true, true).unwrap();
    harness.app.request_sample_edit(pad, busy_recipe).unwrap();
    let [busy_request] = harness.app.take_worker_requests().try_into().unwrap();
    assert!(
        harness
            .app
            .apply_worker_send_error(busy_request, WorkerSendError::WorkerBusy)
    );
    assert_eq!(
        harness.app.sample_edit_status(pad),
        SampleEditStatus::AwaitingWorker
    );
    assert!(Arc::ptr_eq(
        harness.app.pad(pad).sample.as_ref().unwrap(),
        &before_busy
    ));
    assert_eq!(
        harness.app.committed_sample_recipe(pad),
        Some(second_recipe)
    );
    assert_eq!(harness.app.sample_editor().draft(), second_recipe);
    assert!(harness.app.maintain_audio());
    assert!(harness.process_one_queued_request());
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});
    assert_eq!(harness.app.committed_sample_recipe(pad), Some(busy_recipe));

    let stale_recipe = SampleEditRecipe::new(0, SAMPLE_PHASE_SCALE / 2, false, false).unwrap();
    let newest_recipe = SampleEditRecipe::new(
        SAMPLE_PHASE_SCALE / 8,
        SAMPLE_PHASE_SCALE * 7 / 8,
        false,
        true,
    )
    .unwrap();
    harness.app.request_sample_edit(pad, stale_recipe).unwrap();
    let [stale_request] = harness.app.take_worker_requests().try_into().unwrap();
    harness.worker.try_send(stale_request).unwrap();
    harness.app.request_sample_edit(pad, newest_recipe).unwrap();
    let [newest_request] = harness.app.take_worker_requests().try_into().unwrap();
    harness.worker.try_send(newest_request).unwrap();
    let before_stale = Arc::clone(harness.app.pad(pad).sample.as_ref().unwrap());
    let stale_result = harness.worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!harness.app.apply_worker_result(stale_result));
    assert!(Arc::ptr_eq(
        harness.app.pad(pad).sample.as_ref().unwrap(),
        &before_stale
    ));
    assert_eq!(harness.app.committed_sample_recipe(pad), Some(busy_recipe));
    let newest_result = harness.worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(harness.app.apply_worker_result(newest_result));
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});
    assert_eq!(
        harness.app.committed_sample_recipe(pad),
        Some(newest_recipe)
    );

    harness.palette("trim-start 3");
    let rejected_draft = harness.app.sample_editor().draft();
    harness.palette("apply-sample");
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(harness.process_one_queued_request());
    let before_rejection = Arc::clone(harness.app.pad(pad).sample.as_ref().unwrap());
    harness.probe.reject_install.set(true);
    assert!(harness.app.maintain_audio());
    assert!(Arc::ptr_eq(
        harness.app.pad(pad).sample.as_ref().unwrap(),
        &before_rejection
    ));
    assert_eq!(
        harness.app.committed_sample_recipe(pad),
        Some(newest_recipe)
    );
    assert_eq!(harness.app.sample_editor().draft(), rejected_draft);
    assert_eq!(
        harness.app.sample_editor().status(),
        sampler_tui::WorkspaceSampleEditorStatus::Error(
            sampler_tui::SampleEditorError::InstallFailed
        )
    );
    harness.probe.reject_install.set(false);
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});
    assert_eq!(
        harness.app.committed_sample_recipe(pad),
        Some(rejected_draft)
    );

    harness.palette("undo-sample");
    assert!(harness.process_one_queued_request());
    let before_failed_undo = Arc::clone(harness.app.pad(pad).sample.as_ref().unwrap());
    harness.probe.reject_install.set(true);
    assert!(harness.app.maintain_audio());
    assert!(Arc::ptr_eq(
        harness.app.pad(pad).sample.as_ref().unwrap(),
        &before_failed_undo
    ));
    assert_eq!(
        harness.app.committed_sample_recipe(pad),
        Some(rejected_draft)
    );
    assert_eq!(
        harness.app.sample_editor().status(),
        sampler_tui::WorkspaceSampleEditorStatus::Error(
            sampler_tui::SampleEditorError::InstallFailed
        )
    );
    harness.probe.reject_install.set(false);
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});
    assert_eq!(
        harness.app.committed_sample_recipe(pad),
        Some(newest_recipe)
    );
    assert_eq!(
        harness.app.undo_sample_edit(pad),
        Err(SampleEditRequestError::NoUndo)
    );
    assert!(harness.app.take_worker_requests().is_empty());
    assert_eq!(
        harness.app.pad(pad).source.as_deref(),
        Some(fixture.path.as_path())
    );
    assert_eq!(fixture.bytes(), source_before);
}

#[test]
fn device_rate_recovery_redecodes_one_pad_at_a_time_without_applying_the_draft() {
    let fixture = Fixture::rate_mapping_48_000_frames();
    let identity_fixture = Fixture::identity_48_000_frames();
    let source_before = fixture.bytes();
    let identity_source_before = identity_fixture.bytes();
    let mut harness = Harness::new(48_000);
    let first = pad(0);
    let second = pad(1);
    harness.load(first, &fixture.path);
    harness.load(second, &identity_fixture.path);
    let original_base_preview = Arc::clone(harness.app.edit_preview(first).unwrap());
    harness.enter_sample();

    harness.palette("trim-start 12000");
    harness.palette("trim-end 36000");
    harness.palette("apply-sample");
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(harness.process_one_queued_request());
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});
    let committed = SampleEditRecipe::new(
        SAMPLE_PHASE_SCALE / 4,
        SAMPLE_PHASE_SCALE * 3 / 4,
        false,
        false,
    )
    .unwrap();
    assert_eq!(harness.app.committed_sample_recipe(first), Some(committed));
    assert!(Arc::ptr_eq(
        harness.app.edit_preview(first).unwrap(),
        &original_base_preview
    ));

    harness.palette("reverse on");
    let uncommitted = harness.app.sample_editor().draft();
    assert!(uncommitted.reversed);
    assert_ne!(uncommitted, committed);
    harness.probe.runtime_failure.set(true);
    assert!(harness.app.maintain_audio());
    assert_eq!(harness.app.audio_format(), None);
    assert_eq!(harness.app.sample_editor().draft(), uncommitted);
    assert_eq!(harness.app.committed_sample_recipe(first), Some(committed));

    let (recovered_port, recovered_engine, recovered_probe) = audio_pair(44_100);
    assert!(harness.app.retry_with(Box::new(recovered_port)));
    harness.engine = recovered_engine;
    harness.probe = recovered_probe;

    let [first_recovery] = harness.app.take_worker_requests().try_into().unwrap();
    let WorkerRequest::LoadSample {
        pad: recovery_pad,
        engine_rate,
        recipe,
        ..
    } = &first_recovery
    else {
        panic!("expected recovery load");
    };
    assert_eq!(*recovery_pad, first);
    assert_eq!(*engine_rate, 44_100);
    assert_eq!(*recipe, committed);
    assert_eq!(
        harness.app.base_sample(first).unwrap().sample_rate(),
        48_000
    );
    assert_eq!(
        harness.app.base_sample(second).unwrap().sample_rate(),
        48_000
    );
    assert_eq!(harness.app.sample_editor().draft(), uncommitted);

    harness.worker.try_send(first_recovery).unwrap();
    let first_result = harness.worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(harness.app.apply_worker_result(first_result));
    harness.engine.render_frames(0, |_| {});
    assert_eq!(
        harness.app.base_sample(first).unwrap().sample_rate(),
        44_100
    );
    assert_eq!(harness.app.base_sample(first).unwrap().frames(), 44_100);
    assert!(!Arc::ptr_eq(
        harness.app.edit_preview(first).unwrap(),
        &original_base_preview
    ));
    assert_eq!(
        committed
            .frame_range(harness.app.base_sample(first).unwrap().frames())
            .unwrap(),
        11_025..33_075
    );
    assert_eq!(
        harness.app.pad(first).sample.as_ref().unwrap().frames(),
        22_050
    );
    let recovered_base = harness.app.base_sample(first).unwrap();
    let recovered = harness.app.pad(first).sample.as_ref().unwrap();
    let correct_first = stereo_frame(recovered_base, 11_025);
    let correct_last = stereo_frame(recovered_base, 33_074);
    for wrong_start in [10_878, 11_024, 11_026, 11_172] {
        assert_ne!(correct_first, stereo_frame(recovered_base, wrong_start));
    }
    for wrong_last in [32_927, 33_073, 33_075, 33_221] {
        assert_ne!(correct_last, stereo_frame(recovered_base, wrong_last));
    }
    assert_ne!(correct_first, correct_last);
    assert_eq!(stereo_frame(recovered, 0), correct_first);
    assert_eq!(
        stereo_frame(recovered, recovered.frames() - 1),
        correct_last
    );
    assert_eq!(
        harness.app.base_sample(second).unwrap().sample_rate(),
        48_000
    );
    assert_eq!(harness.app.committed_sample_recipe(first), Some(committed));
    assert_eq!(harness.app.sample_editor().draft(), uncommitted);
    assert_eq!(
        harness.app.sample_editor().status(),
        sampler_tui::WorkspaceSampleEditorStatus::Dirty
    );

    assert!(harness.app.maintain_audio());
    let [second_recovery] = harness.app.take_worker_requests().try_into().unwrap();
    let WorkerRequest::LoadSample {
        pad: recovery_pad,
        engine_rate,
        recipe,
        ..
    } = &second_recovery
    else {
        panic!("expected second recovery load");
    };
    assert_eq!(*recovery_pad, second);
    assert_eq!(*engine_rate, 44_100);
    assert_eq!(*recipe, SampleEditRecipe::identity());
    assert_eq!(
        harness.app.base_sample(second).unwrap().sample_rate(),
        48_000
    );

    harness.worker.try_send(second_recovery).unwrap();
    let second_result = harness.worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(harness.app.apply_worker_result(second_result));
    harness.engine.render_frames(0, |_| {});
    assert_eq!(
        harness.app.base_sample(second).unwrap().sample_rate(),
        44_100
    );
    assert_eq!(harness.app.base_sample(second).unwrap().frames(), 44_100);
    let identity_base = harness.app.base_sample(second).unwrap();
    let identity = harness.app.pad(second).sample.as_ref().unwrap();
    assert!(Arc::ptr_eq(identity, identity_base));
    assert_eq!(&identity.data()[..2], &identity_base.data()[..2]);
    assert_eq!(
        &identity.data()[identity.data().len() - 2..],
        &identity_base.data()[identity_base.data().len() - 2..]
    );
    assert!(identity.data()[0] > 0.0 && identity.data()[1] > 0.0);
    assert_eq!(harness.app.committed_sample_recipe(first), Some(committed));
    assert_eq!(harness.app.sample_editor().draft(), uncommitted);
    assert_ne!(harness.app.sample_editor().draft(), committed);
    assert_eq!(
        harness.app.sample_editor().status(),
        sampler_tui::WorkspaceSampleEditorStatus::Dirty
    );

    harness.app.apply(InputAction::PadPress(1));
    let mut identity_audition = Vec::new();
    harness
        .engine
        .render_frames(1, |frame| identity_audition.push(frame));
    assert!(identity_audition.last().unwrap()[0] > 0.0);
    assert!(identity_audition.last().unwrap()[1] > 0.0);
    harness.app.apply(InputAction::PadRelease(1));
    harness.app.apply(InputAction::StopAll);
    harness.engine.render_frames(65, |_| {});

    harness.app.apply(InputAction::PadPress(0));
    let mut committed_audition = Vec::new();
    harness
        .engine
        .render_frames(1, |frame| committed_audition.push(frame));
    let committed_first = *committed_audition.last().unwrap();
    let expected_callback = first_callback_frame(correct_first);
    assert!((committed_first[0] - expected_callback[0]).abs() < 1e-6);
    assert!((committed_first[1] - expected_callback[1]).abs() < 1e-6);
    assert!(committed_first[0] < 0.0 && committed_first[1] > 0.0);
    assert_eq!(harness.engine.executed_triggers(), 2);
    assert_eq!(fixture.bytes(), source_before);
    assert_eq!(identity_fixture.bytes(), identity_source_before);

    harness.app.apply(InputAction::StopAll);
    harness.engine.render_frames(65, |_| {});
    harness.palette("apply-sample");
    assert!(matches!(
        harness.app.overlay(),
        Some(Overlay::ApplySample { pad, .. }) if *pad == first
    ));
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(harness.process_one_queued_request());
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});
    assert_eq!(
        harness.app.committed_sample_recipe(first),
        Some(uncommitted)
    );
    assert_eq!(
        harness.app.sample_editor().status(),
        sampler_tui::WorkspaceSampleEditorStatus::UndoAvailable
    );
    let applied = harness.app.pad(first).sample.as_ref().unwrap();
    assert_eq!(stereo_frame(applied, 0), correct_last);
    assert_eq!(stereo_frame(applied, applied.frames() - 1), correct_first);
    assert_eq!(fixture.bytes(), source_before);
}

#[test]
fn sample_waveform_stays_in_the_base_domain_after_trim_and_reverse_at_every_zoom_anchor() {
    let fixture = Fixture::nonperiodic_impulses_1_024_frames();
    let mut harness = Harness::new(48_000);
    harness.load(pad(0), &fixture.path);
    harness.enter_sample();
    harness.palette("trim-start 256");
    harness.palette("trim-end 768");
    harness.palette("reverse on");
    harness.palette("apply-sample");
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(harness.process_one_queued_request());
    assert!(harness.app.maintain_audio());
    harness.engine.render_frames(0, |_| {});

    // The 76-column waveform maps base frames 300 and 700 to absolute x=24 and x=54.
    // A rendered-domain preview would instead place the reversed impulses near x=71 and x=11.
    assert_eq!(harness.screen_symbol(24, 5), "+");
    assert_eq!(harness.screen_symbol(54, 11), "-");

    harness.key(KeyCode::Char('m'), KeyModifiers::NONE);
    harness.key(KeyCode::PageUp, KeyModifiers::NONE);
    // Start-anchored zoom is base frames 0..512; frame 300 maps to absolute x=46.
    assert_eq!(harness.screen_symbol(46, 5), "+");

    harness.key(KeyCode::Char('m'), KeyModifiers::NONE);
    harness.key(KeyCode::PageDown, KeyModifiers::NONE);
    harness.key(KeyCode::PageUp, KeyModifiers::NONE);
    // End-anchored zoom is base frames 512..1024; frame 700 maps to absolute x=30.
    assert_eq!(harness.screen_symbol(30, 11), "-");
}
