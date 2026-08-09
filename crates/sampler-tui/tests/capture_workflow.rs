//! Cross-layer capture evidence. Physical CPAL device I/O is the only substituted boundary:
//! output uses the real engine/controller, input uses the real callback adapter/core, and both
//! workflows use the public App transaction, bounded worker, and project store.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hound::{SampleFormat, WavSpec, WavWriter};
use sampler_audio::{
    AudioController, AudioEngine, CaptureBuffer, CaptureController, CaptureCore, CaptureOutcome,
    CaptureSource, CaptureStatus, Frame, LiveAck, LiveCommandId, PatternSnapshotSlot,
    PatternSwitch, SampleBuffer, SampleSlot, Telemetry, audio_channels_with_test_capacities,
    capture_channels, write_input_device,
};
use sampler_core::{
    AssetDigest, BankId, EditablePattern, EventId, Meter, PadId, PadSettings, PatternEvent,
    PatternSlotId, PatternSnapshot, PlaybackMode, Resolution, SampleEditRecipe, Tempo, Transport,
};
use sampler_tui::audio::CaptureCommandFailure;
use sampler_tui::{
    App, AudioPort, CaptureError, CaptureFailureCause, CaptureFinalizeError, CapturePhase,
    CaptureSupport, FinalizeCaptureRequest, InputAction, KeyboardCapabilities, Overlay,
    ProjectAction, ProjectOpenPhase, ProjectSaveError, ProjectSnapshotError, ProjectStore,
    RecoveryChoice, SourceFingerprint, WorkerHandle, WorkerRequest, WorkerResult,
};
use sha2::{Digest, Sha256};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
const SAMPLE_RATE: u32 = 48_000;
const INPUT_RATE: u32 = 44_100;
const COMMAND_CAPACITY: usize = 4;

struct FixtureTree {
    root: PathBuf,
}

impl FixtureTree {
    fn new() -> Self {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-capture-workflow-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create capture workflow fixture");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn write_constant_wav(&self, name: &str, value: f32) -> PathBuf {
        let path = self.path(name);
        let mut writer = WavWriter::create(
            &path,
            WavSpec {
                channels: 2,
                sample_rate: SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )
        .expect("create source WAV");
        for _ in 0..512 {
            writer.write_sample(value).unwrap();
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();
        path
    }
}

impl Drop for FixtureTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Clone, Copy)]
struct InputIdentity {
    token: u64,
    target: PadId,
    max_frames: usize,
}

#[derive(Default)]
struct PortState {
    output_capture_error: Option<String>,
    input_capture_error: Option<String>,
    install_attempts: usize,
    install_successes: usize,
}

#[derive(Clone)]
struct PortProbe(Rc<RefCell<PortState>>);

impl PortProbe {
    fn fail_capture(&self, source: CaptureSource, message: &str) {
        let mut state = self.0.borrow_mut();
        match source {
            CaptureSource::Resample => state.output_capture_error = Some(message.to_owned()),
            CaptureSource::Input => state.input_capture_error = Some(message.to_owned()),
        }
    }

    fn install_counts(&self) -> (usize, usize) {
        let state = self.0.borrow();
        (state.install_attempts, state.install_successes)
    }
}

struct IntegrationPort {
    output: Rc<RefCell<AudioController>>,
    input: Rc<RefCell<CaptureController>>,
    input_identity: Option<InputIdentity>,
    state: Rc<RefCell<PortState>>,
}

impl IntegrationPort {
    fn output(&self) -> std::cell::RefMut<'_, AudioController> {
        self.output.borrow_mut()
    }

    fn capture_send(result: Result<(), sampler_audio::CaptureSendFailure>) {
        if let Err(failure) = result {
            panic!("real capture command rejected: {:?}", failure.error());
        }
    }
}

impl AudioPort for IntegrationPort {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn channels(&self) -> u16 {
        2
    }

    fn render_horizon(&self) -> Frame {
        self.output.borrow().render_horizon()
    }

    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: sampler_core::PadMixSettings,
    ) -> Result<SampleSlot, String> {
        self.state.borrow_mut().install_attempts += 1;
        let result = self
            .output()
            .install(pad, sample, settings, mix)
            .map_err(|error| error.to_string());
        if result.is_ok() {
            self.state.borrow_mut().install_successes += 1;
        }
        result
    }

    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: sampler_core::PadMixSettings,
    ) -> Result<SampleSlot, String> {
        self.state.borrow_mut().install_attempts += 1;
        let result = self
            .output()
            .install_recovery(pad, sample, settings, mix)
            .map_err(|error| error.to_string());
        if result.is_ok() {
            self.state.borrow_mut().install_successes += 1;
        }
        result
    }

    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
        self.output()
            .trigger(pad, at, velocity)
            .map_err(|error| error.to_string())
    }

    fn trigger_live(&mut self, pad: PadId, velocity: f32) -> Result<(), String> {
        self.output()
            .trigger_live(pad, velocity)
            .map_err(|error| error.to_string())
    }

    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
        self.output()
            .release(pad, at)
            .map_err(|error| error.to_string())
    }

    fn release_live(&mut self, pad: PadId) -> Result<(), String> {
        self.output()
            .release_live(pad)
            .map_err(|error| error.to_string())
    }

    fn trigger_live_tracked(&mut self, pad: PadId, velocity: f32) -> Result<LiveCommandId, String> {
        self.output()
            .trigger_live_tracked(pad, velocity)
            .map_err(|error| error.to_string())
    }

    fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, String> {
        self.output()
            .release_live_tracked(pad)
            .map_err(|error| error.to_string())
    }

    fn install_pattern(
        &mut self,
        snapshot: Arc<PatternSnapshot>,
    ) -> Result<PatternSnapshotSlot, String> {
        self.output()
            .install_pattern(snapshot)
            .map_err(|error| error.to_string())
    }

    fn select_pattern(&mut self, slot: PatternSlotId, switch: PatternSwitch) -> Result<(), String> {
        self.output()
            .select_pattern(slot, switch)
            .map_err(|error| error.to_string())
    }

    fn play_pattern(&mut self) -> Result<(), String> {
        self.output()
            .play_pattern()
            .map_err(|error| error.to_string())
    }

    fn stop_pattern(&mut self) -> Result<(), String> {
        self.output()
            .stop_pattern()
            .map_err(|error| error.to_string())
    }

    fn set_record_capture(&mut self, capture: Option<(PatternSlotId, u64)>) -> Result<(), String> {
        self.output()
            .set_record_capture(capture)
            .map_err(|error| error.to_string())
    }

    fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
        self.output().drain_live_acks(output)
    }

    fn reclaim_retired_patterns(&mut self) -> usize {
        let mut reclaimed = 0;
        while self.output().reclaim_retired_pattern().is_some() {
            reclaimed += 1;
        }
        reclaimed
    }

    fn remove_sample(&mut self, pad: PadId) -> Result<(), String> {
        self.output()
            .remove_sample(pad)
            .map_err(|error| error.to_string())
    }

    fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
        self.output()
            .stop_pad(pad)
            .map_err(|error| error.to_string())
    }

    fn stop_all(&mut self) -> Result<(), String> {
        self.output().stop_all().map_err(|error| error.to_string())
    }

    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        self.output()
            .update_pad(pad, settings)
            .map_err(|error| error.to_string())
    }

    fn update_pad_mix(
        &mut self,
        pad: PadId,
        settings: sampler_core::PadMixSettings,
    ) -> Result<(), String> {
        self.output()
            .update_pad_mix(pad, settings)
            .map_err(|error| error.to_string())
    }

    fn update_master_mix(
        &mut self,
        settings: sampler_core::MasterMixSettings,
    ) -> Result<(), String> {
        self.output()
            .update_master_mix(settings)
            .map_err(|error| error.to_string())
    }

    fn reclaim_retired(&mut self) -> usize {
        self.output().reclaim_retired()
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.output().latest_telemetry()
    }

    fn poll_runtime_error(&mut self) -> Option<String> {
        None
    }

    fn capture_support(&self) -> CaptureSupport {
        CaptureSupport::Available
    }

    fn capture_source_rate(&mut self, source: CaptureSource) -> Result<u32, CaptureError> {
        Ok(match source {
            CaptureSource::Resample => SAMPLE_RATE,
            CaptureSource::Input => INPUT_RATE,
        })
    }

    fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureCommandFailure> {
        let source = buffer.source();
        let input_identity = InputIdentity {
            token: buffer.token(),
            target: buffer.target(),
            max_frames: buffer.max_frames(),
        };
        match source {
            CaptureSource::Resample => Self::capture_send(self.output().arm_capture(buffer)),
            CaptureSource::Input => Self::capture_send(self.input.borrow_mut().arm(buffer)),
        }
        if source == CaptureSource::Input {
            self.input_identity = Some(input_identity);
        }
        Ok(())
    }

    fn start_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        match source {
            CaptureSource::Resample => Self::capture_send(self.output().start_capture(token)),
            CaptureSource::Input => Self::capture_send(self.input.borrow_mut().start(token)),
        }
        Ok(())
    }

    fn stop_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        match source {
            CaptureSource::Resample => Self::capture_send(self.output().stop_capture(token)),
            CaptureSource::Input => Self::capture_send(self.input.borrow_mut().stop(token)),
        }
        Ok(())
    }

    fn cancel_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        match source {
            CaptureSource::Resample => Self::capture_send(self.output().cancel_capture(token)),
            CaptureSource::Input => Self::capture_send(self.input.borrow_mut().cancel(token)),
        }
        Ok(())
    }

    fn capture_status(&mut self, source: CaptureSource) -> Option<CaptureStatus> {
        match source {
            CaptureSource::Resample => self.output.borrow().capture_status(),
            CaptureSource::Input => {
                let identity = self.input_identity?;
                let controller = self.input.borrow();
                let progress = controller.progress()?;
                (progress.token == identity.token).then_some(CaptureStatus {
                    token: identity.token,
                    source,
                    target: identity.target,
                    state: controller.state(),
                    frames: progress.frames,
                    max_frames: identity.max_frames,
                    peak: progress.peak,
                    hard_limit: progress.hard_limit,
                })
            }
        }
    }

    fn capture_completion(&mut self, source: CaptureSource) -> Option<CaptureOutcome> {
        let outcome = match source {
            CaptureSource::Resample => self.output.borrow_mut().try_capture_completion(),
            CaptureSource::Input => self.input.borrow_mut().try_next_outcome(),
        };
        if source == CaptureSource::Input && outcome.is_some() {
            self.input_identity = None;
        }
        outcome
    }

    fn capture_runtime_error(&mut self, source: CaptureSource) -> Option<CaptureError> {
        let mut state = self.state.borrow_mut();
        match source {
            CaptureSource::Resample => state
                .output_capture_error
                .take()
                .map(CaptureError::OutputRuntime),
            CaptureSource::Input => state
                .input_capture_error
                .take()
                .map(CaptureError::InputRuntime),
        }
    }
}

struct Harness {
    app: App,
    engine: AudioEngine,
    worker: WorkerHandle,
    controller: Rc<RefCell<AudioController>>,
    input_core: Rc<RefCell<CaptureCore>>,
    probe: PortProbe,
}

impl Harness {
    fn new() -> Self {
        let (controller, ports) = audio_channels_with_test_capacities(COMMAND_CAPACITY, 256, 8);
        let controller = Rc::new(RefCell::new(controller));
        let engine = AudioEngine::new(SAMPLE_RATE, ports).unwrap();
        let (input_controller, input_core) = capture_channels(4, 1);
        let input_controller = Rc::new(RefCell::new(input_controller));
        let input_core = Rc::new(RefCell::new(input_core));
        let state = Rc::new(RefCell::new(PortState::default()));
        let probe = PortProbe(Rc::clone(&state));
        let mut app = App::with_audio(Box::new(IntegrationPort {
            output: Rc::clone(&controller),
            input: input_controller,
            input_identity: None,
            state,
        }));
        app.set_keyboard_capabilities(KeyboardCapabilities {
            release_events: true,
        });
        Self {
            app,
            engine,
            worker: WorkerHandle::spawn(),
            controller,
            input_core,
            probe,
        }
    }

    fn dispatch(&mut self, request: WorkerRequest) {
        self.worker
            .try_send(request)
            .expect("real worker must admit bounded test request");
        let result = self
            .worker
            .recv_timeout(Duration::from_secs(5))
            .expect("real worker result");
        assert!(self.app.apply_worker_result(result));
    }

    fn take_one_request(&mut self) -> WorkerRequest {
        let requests = self.app.take_worker_requests();
        let [request] = requests.try_into().unwrap_or_else(|requests: Vec<_>| {
            panic!("expected one bounded worker request, got {requests:?}")
        });
        request
    }

    fn dispatch_one_queued(&mut self) {
        let request = self.take_one_request();
        self.dispatch(request);
    }

    fn load(&mut self, pad: PadId, path: &Path) {
        let request = self.app.begin_load(pad, path).expect("public load request");
        self.dispatch(request);
        self.engine.render_frames(0, |_| {});
    }

    fn finish_capture(&mut self) {
        for _ in 0..32 {
            if self.app.capture_session().phase().is_none() {
                self.engine.render_frames(0, |_| {});
                return;
            }
            self.app.maintain_capture();
            let requests = self.app.take_worker_requests();
            for request in requests {
                self.dispatch(request);
            }
            self.engine.render_frames(0, |_| {});
        }
        panic!(
            "capture did not finish: phase={:?} status={}",
            self.app.capture_session().phase(),
            self.app.status()
        );
    }

    fn save_as(&mut self, directory: &Path, now: Instant) {
        self.app.request_save_as(directory).unwrap();
        assert!(self.app.maintain_project(now));
        self.dispatch_one_queued();
    }

    fn drain_post_save_cleanup(&mut self, now: Instant) {
        if self.app.maintain_capture() {
            let requests = self.app.take_worker_requests();
            for request in requests {
                self.dispatch(request);
            }
            self.app.maintain_capture();
        }
        if self.app.maintain_project(now) {
            self.dispatch_one_queued();
        }
    }

    fn open(&mut self, directory: &Path, recovery: Option<RecoveryChoice>, now: Instant) {
        self.app.request_open_project(directory).unwrap();
        self.dispatch_one_queued();
        if self
            .app
            .project_open_stage()
            .is_some_and(|stage| stage.phase == ProjectOpenPhase::AwaitingRecoveryChoice)
        {
            self.app
                .choose_project_recovery(recovery.expect("recovery choice"))
                .unwrap();
            let requests = self.app.take_worker_requests();
            for request in requests {
                self.dispatch(request);
            }
        }
        for _ in 0..256 {
            if self.app.project_open_stage().is_none() {
                return;
            }
            self.app.maintain_project(now);
            let requests = self.app.take_worker_requests();
            for request in requests {
                self.dispatch(request);
            }
            self.engine.render_frames(0, |_| {});
            self.app.maintain_audio();
        }
        panic!(
            "project open did not finish: stage={:?} error={:?}",
            self.app.project_open_stage(),
            self.app.project_open_error()
        );
    }

    fn poll_output_capture_commands(&mut self) {
        self.engine.render_frames(0, |_| {});
        self.engine.render_frames(0, |_| {});
    }

    fn poll_input_capture_commands(&mut self) {
        write_input_device::<f32>(&mut self.input_core.borrow_mut(), 2, &[]).unwrap();
        write_input_device::<f32>(&mut self.input_core.borrow_mut(), 2, &[]).unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.worker.shutdown().expect("worker shutdown");
    }
}

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press)
}

fn pattern_snapshot(pattern_pad: PadId) -> Arc<PatternSnapshot> {
    let transport = Transport::new(
        SAMPLE_RATE,
        Tempo::new(300.0).unwrap(),
        Meter::new(1, 8).unwrap(),
        1,
        Resolution::Sixteenth,
    )
    .unwrap();
    let slot = PatternSlotId::new(0).unwrap();
    let mut pattern = EditablePattern::new(slot, "Capture pattern", transport).unwrap();
    pattern
        .insert(PatternEvent::new(EventId(1), pattern_pad, 2, 1.0, None).unwrap())
        .unwrap();
    Arc::new(pattern.compile().unwrap())
}

fn flatten(frames: &[[f32; 2]]) -> Vec<f32> {
    frames.iter().flatten().copied().collect()
}

fn finalize_error(
    request: &FinalizeCaptureRequest,
    generation: u64,
    message: &str,
) -> WorkerResult {
    WorkerResult::CaptureFinalized {
        token: request.token,
        generation,
        target: request.target,
        source: request.source,
        source_rate: request.source_rate,
        engine_rate: request.engine_rate,
        stereo: Arc::clone(&request.stereo),
        hard_limit: request.hard_limit,
        result: Err(CaptureFinalizeError::Prepare(message.to_owned())),
    }
}

fn sha256_asset(bytes: &[u8]) -> AssetDigest {
    AssetDigest::from_bytes(Sha256::digest(bytes).into())
}

#[test]
fn mixed_resample_save_move_and_fresh_open_preserve_exact_tuple_and_nonzero_rendered_output() {
    let fixture = FixtureTree::new();
    let pattern_wav = fixture.write_constant_wav("pattern.wav", 0.25);
    let live_wav = fixture.write_constant_wav("live.wav", -0.125);
    let project = fixture.path("captured-project");
    let moved = fixture.path("moved-captured-project");
    let target = pad(0);
    let pattern_pad = pad(1);
    let live_pad = pad(2);
    let now = Instant::now();

    let mut source = Harness::new();
    source.load(pattern_pad, &pattern_wav);
    source.load(live_pad, &live_wav);
    let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
    source
        .app
        .update_pad_settings(pattern_pad, looping)
        .unwrap();
    source.app.update_pad_settings(live_pad, looping).unwrap();
    let captured_settings = PadSettings::new(PlaybackMode::Gate, -3.0, 0.25, 2.0, None).unwrap();
    source
        .app
        .update_pad_settings(target, captured_settings)
        .unwrap();
    source.engine.render_frames(0, |_| {});

    source
        .controller
        .borrow_mut()
        .install_pattern(pattern_snapshot(pattern_pad))
        .unwrap();
    source
        .controller
        .borrow_mut()
        .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
        .unwrap();
    source.controller.borrow_mut().play_pattern().unwrap();
    source.app.apply(InputAction::PadPress(2));
    source.engine.render_frames(1, |_| {});

    // Fill the real capacity-one completion ring with an independent stale take. The App's take
    // must remain pending until the stale owner is drained and the callback retries publication.
    source
        .controller
        .borrow_mut()
        .arm_capture(
            CaptureBuffer::try_new(9_999, target, CaptureSource::Resample, SAMPLE_RATE, 1).unwrap(),
        )
        .unwrap();
    source.engine.render_frames(0, |_| {});
    source.controller.borrow_mut().start_capture(9_999).unwrap();
    source.engine.render_frames(1, |_| {});
    source.app.apply(InputAction::PadPress(0));
    source.app.apply(InputAction::PadRelease(0));
    source.engine.render_frames(0, |_| {});

    let revision_before = source.app.project_revision();
    source
        .app
        .request_capture_with_frame_limit(CaptureSource::Resample, 64)
        .unwrap();
    source.poll_output_capture_commands();
    let mut expected_frames = Vec::new();
    source
        .engine
        .render_frames(32, |frame| expected_frames.push(frame));
    assert!(expected_frames.iter().any(|frame| *frame != [0.0, 0.0]));
    assert!(
        expected_frames
            .windows(2)
            .any(|frames| frames[0] != frames[1])
    );
    source.app.stop_capture().unwrap();
    let mut excluded_stop_frame = [0.0; 2];
    source
        .engine
        .render_frames(1, |frame| excluded_stop_frame = frame);
    assert_ne!(excluded_stop_frame, [0.0, 0.0]);

    assert!(!source.app.maintain_capture());
    assert_eq!(
        source.app.capture_session().phase(),
        Some(CapturePhase::Recording),
        "the independent stale completion must not finish the App take"
    );
    source.engine.render_frames(0, |_| {});
    assert!(source.app.maintain_capture());
    assert_eq!(
        source.app.capture_session().phase(),
        Some(CapturePhase::Finalizing)
    );
    assert!(source.app.maintain_capture());
    let request = source.take_one_request();
    assert!(matches!(request, WorkerRequest::FinalizeCapture(_)));
    source.dispatch(request);
    assert!(source.app.maintain_capture());
    assert_eq!(
        source.app.capture_session().phase(),
        Some(CapturePhase::ReadyToInstall)
    );

    let install_counts = source.probe.install_counts();
    for _ in 0..COMMAND_CAPACITY {
        source.controller.borrow_mut().stop_pad(pad(15)).unwrap();
    }
    assert!(source.app.maintain_capture());
    assert_eq!(
        source.app.capture_session().phase(),
        Some(CapturePhase::ReadyToInstall),
        "a full real install lane must retain the exact candidate"
    );
    source.engine.render_frames(0, |_| {});
    assert!(source.app.maintain_capture());
    source.engine.render_frames(0, |_| {});
    let install_counts_after = source.probe.install_counts();
    assert_eq!(install_counts_after.0 - install_counts.0, 2);
    assert_eq!(install_counts_after.1 - install_counts.1, 1);
    assert_eq!(source.app.project_revision(), revision_before + 1);

    let expected_pcm = flatten(&expected_frames);
    let captured = source.app.pad(target).sample.as_ref().unwrap();
    assert_eq!(captured.data(), expected_pcm);
    assert_eq!(source.app.base_sample(target).unwrap().data(), expected_pcm);
    let preview = source.app.pad(target).preview;
    let recipe = source.app.committed_sample_recipe(target).unwrap();
    assert_eq!(recipe, SampleEditRecipe::identity());
    assert_eq!(source.app.pad(target).settings, captured_settings);
    let before_save = source.app.project_snapshot().unwrap();
    let captured_pad = before_save
        .pads
        .iter()
        .find(|saved| saved.pad == target)
        .unwrap();
    let fingerprint = captured_pad.fingerprint;
    let source_generation = captured_pad.source_generation;
    let managed_path = captured_pad.source_path.clone();
    assert!(managed_path.is_file());

    source.save_as(&project, now);
    let canonical_project = fs::canonicalize(&project).unwrap();
    let saved = source.app.project_snapshot().unwrap();
    let saved_pad = saved.pads.iter().find(|saved| saved.pad == target).unwrap();
    assert_eq!(saved_pad.fingerprint, fingerprint);
    assert_eq!(saved_pad.recipe, recipe);
    assert_eq!(saved_pad.settings, captured_settings);
    assert_eq!(saved_pad.source_generation, source_generation);
    assert!(saved_pad.source_path.starts_with(&canonical_project));
    assert_ne!(saved_pad.source_path, managed_path);
    source.drain_post_save_cleanup(now);
    assert!(!managed_path.exists());

    let document = ProjectStore
        .probe(&project)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();
    let persisted = document
        .pads
        .iter()
        .find(|saved| saved.pad == target)
        .unwrap();
    assert_eq!(persisted.asset_digest, fingerprint.digest);
    assert_eq!(persisted.recipe, recipe);
    assert_eq!(persisted.settings, captured_settings);
    let portable_asset = project.join(&persisted.audio_path);
    assert_eq!(
        SourceFingerprint::from_path(&portable_asset).unwrap(),
        fingerprint
    );

    drop(source);
    fs::rename(&project, &moved).unwrap();
    let canonical_moved = fs::canonicalize(&moved).unwrap();
    let mut reopened = Harness::new();
    reopened.open(&moved, None, now);
    let reopened_snapshot = reopened.app.project_snapshot().unwrap();
    let reopened_pad = reopened_snapshot
        .pads
        .iter()
        .find(|saved| saved.pad == target)
        .unwrap();
    assert_eq!(
        reopened_pad.source_path,
        canonical_moved.join(&persisted.audio_path)
    );
    assert_eq!(reopened_pad.fingerprint, fingerprint);
    assert_eq!(reopened_pad.recipe, recipe);
    assert_eq!(reopened_pad.settings, captured_settings);
    assert_eq!(reopened_pad.source_generation, source_generation);
    assert_eq!(
        reopened.app.pad(target).sample.as_ref().unwrap().data(),
        expected_pcm
    );
    assert_eq!(
        reopened.app.base_sample(target).unwrap().data(),
        expected_pcm
    );
    assert_eq!(reopened.app.pad(target).preview, preview);

    let triggers = reopened.engine.executed_triggers();
    reopened.app.apply(InputAction::PadPress(0));
    let mut peak = 0.0_f32;
    reopened.engine.render_frames(128, |frame| {
        peak = peak.max(frame[0].abs()).max(frame[1].abs());
    });
    assert!(reopened.engine.executed_triggers() > triggers);
    assert!(
        peak > 1.0e-4,
        "reopened capture must render a nonzero signal: {peak}"
    );
}

#[test]
fn input_44100_autosave_restart_restore_preserves_duration_and_exact_wav_digest() {
    let fixture = FixtureTree::new();
    let project = fixture.path("input-recovery-project");
    let now = Instant::now();
    let target = pad(0);
    let mut source = Harness::new();
    source.save_as(&project, now);
    source.drain_post_save_cleanup(now);

    let source_frames = 441_usize;
    let mut input = Vec::with_capacity(source_frames * 2);
    for frame in 0..source_frames {
        let left = (frame as f32 / source_frames as f32) * 0.75 - 0.375;
        input.push(left);
        input.push(-left * 0.5);
    }
    source
        .app
        .request_capture_with_frame_limit(CaptureSource::Input, source_frames + 32)
        .unwrap();
    source.poll_input_capture_commands();
    assert_eq!(
        write_input_device(&mut source.input_core.borrow_mut(), 2, &input).unwrap(),
        source_frames
    );
    source.app.stop_capture().unwrap();
    write_input_device::<f32>(&mut source.input_core.borrow_mut(), 2, &[]).unwrap();
    assert!(source.app.maintain_capture());
    source.finish_capture();

    let before_autosave = source.app.project_snapshot().unwrap();
    let before_pad = before_autosave
        .pads
        .iter()
        .find(|saved| saved.pad == target)
        .unwrap();
    let fingerprint = before_pad.fingerprint;
    let managed_path = before_pad.source_path.clone();
    let prepared_pcm = source
        .app
        .pad(target)
        .sample
        .as_ref()
        .unwrap()
        .data()
        .to_vec();
    let prepared_preview = source.app.pad(target).preview;
    let prepared_frames = prepared_pcm.len() / 2;
    let expected_frames =
        (source_frames * SAMPLE_RATE as usize + INPUT_RATE as usize / 2) / INPUT_RATE as usize;
    assert!(prepared_frames.abs_diff(expected_frames) <= 1);

    let autosave_at = now + Duration::from_secs(10);
    assert!(source.app.maintain_project(autosave_at));
    source.dispatch_one_queued();
    assert!(project.join(".sampler-tui-recovery.toml").is_file());
    source.drain_post_save_cleanup(autosave_at);
    assert!(managed_path.exists());

    let recovery = ProjectStore
        .probe(&project)
        .unwrap()
        .recovery
        .unwrap()
        .unwrap();
    let recovered_pad = recovery
        .pads
        .iter()
        .find(|saved| saved.pad == target)
        .unwrap();
    let asset = project.join(&recovered_pad.audio_path);
    let encoded = fs::read(&asset).unwrap();
    assert_eq!(sha256_asset(&encoded), recovered_pad.asset_digest);
    assert_eq!(recovered_pad.asset_digest, fingerprint.digest);
    assert_eq!(SourceFingerprint::from_path(&asset).unwrap(), fingerprint);

    drop(source);
    assert!(
        !managed_path.exists(),
        "restart must remove the worker-private source after the portable recovery asset exists"
    );
    let mut restored = Harness::new();
    restored.open(&project, Some(RecoveryChoice::Restore), autosave_at);
    let restored_snapshot = restored.app.project_snapshot().unwrap();
    let restored_pad = restored_snapshot
        .pads
        .iter()
        .find(|saved| saved.pad == target)
        .unwrap();
    assert_eq!(restored_pad.fingerprint, fingerprint);
    assert_eq!(restored_pad.source_path, fs::canonicalize(&asset).unwrap());
    assert_eq!(
        restored.app.pad(target).sample.as_ref().unwrap().data(),
        prepared_pcm
    );
    assert_eq!(restored.app.pad(target).preview, prepared_preview);
    assert!(
        restored
            .app
            .pad(target)
            .sample
            .as_ref()
            .unwrap()
            .frames()
            .abs_diff(expected_frames)
            <= 1
    );
    assert_eq!(sha256_asset(&fs::read(&asset).unwrap()), fingerprint.digest);
}

#[test]
fn cancellation_empty_limit_worker_staleness_and_save_refusal_preserve_transaction_truth() {
    let target = pad(0);

    let mut cancelled = Harness::new();
    cancelled
        .app
        .request_capture_with_frame_limit(CaptureSource::Resample, 4)
        .unwrap();
    cancelled.poll_output_capture_commands();
    cancelled.engine.render_frames(1, |_| {});
    cancelled.app.cancel_capture().unwrap();
    cancelled.engine.render_frames(0, |_| {});
    assert!(cancelled.app.maintain_capture());
    assert_eq!(cancelled.app.capture_session().phase(), None);
    assert!(cancelled.app.pad(target).sample.is_none());
    assert_eq!(cancelled.app.project_revision(), 0);

    let mut empty = Harness::new();
    empty
        .app
        .request_capture_with_frame_limit(CaptureSource::Resample, 4)
        .unwrap();
    empty.poll_output_capture_commands();
    empty.app.stop_capture().unwrap();
    empty.engine.render_frames(0, |_| {});
    assert!(empty.app.maintain_capture());
    assert_eq!(
        empty.app.capture_session().phase(),
        Some(CapturePhase::Failed)
    );
    assert_eq!(empty.app.status(), CaptureError::EmptyCapture.to_string());
    assert!(empty.app.pad(target).sample.is_none());
    assert_eq!(empty.app.project_revision(), 0);
    empty.app.cancel_capture().unwrap();

    let mut limited = Harness::new();
    limited
        .app
        .request_capture_with_frame_limit(CaptureSource::Resample, 3)
        .unwrap();
    limited.poll_output_capture_commands();
    limited.engine.render_frames(3, |_| {});
    assert!(limited.app.maintain_capture());
    limited.finish_capture();
    assert_eq!(limited.app.pad(target).sample.as_ref().unwrap().frames(), 3);
    assert_eq!(limited.app.status(), "Captured sample installed · MAX");
    assert_eq!(limited.app.project_revision(), 1);

    let fixture = FixtureTree::new();
    let refused_path = fixture.path("refused-partial-save");
    let mut failed = Harness::new();
    failed
        .app
        .request_capture_with_frame_limit(CaptureSource::Resample, 8)
        .unwrap();
    failed.poll_output_capture_commands();
    assert!(matches!(
        failed.app.request_save_as(&refused_path),
        Err(ProjectSaveError::Snapshot(
            ProjectSnapshotError::UnresolvedCapture(CapturePhase::Recording)
        ))
    ));
    failed.engine.render_frames(2, |_| {});
    failed.app.stop_capture().unwrap();
    failed.engine.render_frames(0, |_| {});
    assert!(failed.app.maintain_capture());
    assert!(failed.app.maintain_capture());
    let WorkerRequest::FinalizeCapture(request) = failed.take_one_request() else {
        panic!("capture must queue finalization")
    };

    assert!(failed.app.apply_worker_result(finalize_error(
        &request,
        request.generation + 1,
        "independent stale generation",
    )));
    assert!(!failed.app.maintain_capture());
    assert_eq!(
        failed.app.capture_session().phase(),
        Some(CapturePhase::Finalizing)
    );
    assert!(failed.app.apply_worker_result(finalize_error(
        &request,
        request.generation,
        "injected worker failure",
    )));
    assert!(failed.app.maintain_capture());
    assert_eq!(
        failed.app.capture_session().phase(),
        Some(CapturePhase::Failed)
    );
    assert!(failed.app.status().contains("injected worker failure"));
    assert!(failed.app.pad(target).sample.is_none());
    assert_eq!(failed.app.project_revision(), 0);

    failed.app.retry_capture_finalization().unwrap();
    assert!(failed.app.maintain_capture());
    let retry = failed.take_one_request();
    let WorkerRequest::FinalizeCapture(retry_request) = &retry else {
        panic!("retry must queue exact finalization")
    };
    assert!(retry_request.generation > request.generation);
    failed.dispatch(retry);
    failed.finish_capture();
    assert!(failed.app.pad(target).sample.is_some());
    assert_eq!(failed.app.project_revision(), 1);
}

#[test]
fn independent_output_and_input_device_errors_abort_without_pad_or_revision_mutation() {
    for source in [CaptureSource::Resample, CaptureSource::Input] {
        let mut harness = Harness::new();
        harness
            .app
            .request_capture_with_frame_limit(source, 8)
            .unwrap();
        match source {
            CaptureSource::Resample => harness.poll_output_capture_commands(),
            CaptureSource::Input => harness.poll_input_capture_commands(),
        }
        harness.probe.fail_capture(source, "device disconnected");
        assert!(harness.app.maintain_capture());
        assert_eq!(
            harness.app.capture_session().phase(),
            Some(CapturePhase::Failed)
        );
        assert_eq!(
            harness.app.capture_session().failure_cause(),
            Some(CaptureFailureCause::DeviceRuntime)
        );
        assert!(harness.app.pad(pad(0)).sample.is_none());
        assert_eq!(harness.app.project_revision(), 0);
        harness.app.cancel_capture().unwrap();
        assert_eq!(harness.app.capture_session().phase(), None);
    }
}

#[derive(Clone, Copy)]
enum LifecycleChoice {
    Finalize,
    Discard,
    Cancel,
}

fn begin_action(app: &mut App, action: ProjectAction, open_path: &Path) {
    match action {
        ProjectAction::Quit => app.apply(InputAction::Quit),
        ProjectAction::Open => app.request_open_project_interactive(open_path),
    }
    assert!(matches!(
        app.overlay(),
        Some(Overlay::ResolveCapture { action: shown }) if *shown == action
    ));
}

#[test]
fn quit_and_open_finalize_discard_cancel_choices_preserve_explicit_ordering() {
    let fixture = FixtureTree::new();
    for action in [ProjectAction::Quit, ProjectAction::Open] {
        for choice in [
            LifecycleChoice::Finalize,
            LifecycleChoice::Discard,
            LifecycleChoice::Cancel,
        ] {
            let mut harness = Harness::new();
            harness
                .app
                .request_capture_with_frame_limit(CaptureSource::Resample, 8)
                .unwrap();
            harness.poll_output_capture_commands();
            harness.engine.render_frames(1, |_| {});
            begin_action(&mut harness.app, action, &fixture.path("next-project"));

            match choice {
                LifecycleChoice::Finalize => {
                    harness.app.apply_key(press(KeyCode::Enter));
                    harness.engine.render_frames(0, |_| {});
                    assert!(!harness.app.should_quit());
                    assert!(harness.app.project_open_stage().is_none());
                    assert!(harness.app.maintain_capture());
                    harness.finish_capture();
                    assert!(matches!(
                        harness.app.overlay(),
                        Some(Overlay::UnsavedProject { action: shown }) if *shown == action
                    ));
                    assert!(!harness.app.should_quit());
                    assert!(harness.app.project_open_stage().is_none());
                }
                LifecycleChoice::Discard => {
                    harness.app.apply_key(press(KeyCode::Backspace));
                    harness.engine.render_frames(0, |_| {});
                    assert!(harness.app.maintain_capture());
                    assert_eq!(harness.app.capture_session().phase(), None);
                    match action {
                        ProjectAction::Quit => assert!(harness.app.should_quit()),
                        ProjectAction::Open => assert!(harness.app.project_open_stage().is_some()),
                    }
                }
                LifecycleChoice::Cancel => {
                    harness.app.apply_key(press(KeyCode::Esc));
                    assert_eq!(
                        harness.app.capture_session().phase(),
                        Some(CapturePhase::Recording)
                    );
                    assert!(!harness.app.should_quit());
                    assert!(harness.app.project_open_stage().is_none());
                    harness.app.cancel_capture().unwrap();
                    harness.engine.render_frames(0, |_| {});
                    assert!(harness.app.maintain_capture());
                }
            }
        }
    }
}
