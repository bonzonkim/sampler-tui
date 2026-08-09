#![allow(
    dead_code,
    reason = "shared integration-test consumers use disjoint harness capabilities"
)]

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hound::{SampleFormat, WavSpec, WavWriter};
use sampler_audio::{
    AudioController, AudioEngine, CaptureBuffer, CaptureOutcome, CaptureSource, CaptureStatus,
    Frame, LiveAck, LiveCommandId, PatternSnapshotSlot, PatternSwitch, SampleBuffer, SampleSlot,
    Telemetry, audio_channels_with_test_capacities,
};
use sampler_core::{PadId, PadSettings, PatternSlotId, PatternSnapshot, SampleEditRecipe};
use sampler_tui::audio::CaptureCommandFailure;
use sampler_tui::{
    App, AudioPort, CaptureError, CaptureSupport, InputAction, KeyboardCapabilities,
    ProjectOpenPhase, RecoveryChoice, WorkerHandle, WorkerRequest,
};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

const FLAC_MONO_8K: &str = concat!(
    "664c6143000000220040004000003700003701f400f000000040e70a3b8c6676f736d5d5de649e17",
    "e3cf84000028200000007265666572656e6365206c6962464c414320312e342e3320323032333036",
    "323300000000fff86408003f5e1309771421861c9edc001861c9ee4001861c9edc001861c9ee4001",
    "861c9edc001861c9ee4001861c9edc00186180b769",
);

pub(crate) struct FixtureTree {
    root: PathBuf,
}

impl FixtureTree {
    pub(crate) fn new() -> Self {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-project-workflow-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create fixture root");
        Self { root }
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub(crate) fn write_wav(&self, name: &str) -> PathBuf {
        let path = self.path(name);
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
        let mut writer = WavWriter::create(
            &path,
            WavSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )
        .expect("create WAV fixture");
        for frame in frames {
            writer.write_sample(frame[0]).unwrap();
            writer.write_sample(frame[1]).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    pub(crate) fn write_flac(&self, name: &str) -> PathBuf {
        let bytes = FLAC_MONO_8K
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).expect("fixture hex")
            })
            .collect::<Vec<_>>();
        let path = self.path(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for FixtureTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ControllerPort {
    sample_rate: u32,
    controller: Rc<RefCell<AudioController>>,
    runtime_failure: Rc<RefCell<Option<String>>>,
}

impl ControllerPort {
    fn controller(&self) -> std::cell::RefMut<'_, AudioController> {
        self.controller.borrow_mut()
    }
}

impl AudioPort for ControllerPort {
    fn capture_support(&self) -> CaptureSupport {
        CaptureSupport::Available
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

    fn remove_sample(&mut self, pad: PadId) -> Result<(), String> {
        self.controller()
            .remove_sample(pad)
            .map_err(|error| error.to_string())
    }

    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        self.controller()
            .update_pad(pad, settings)
            .map_err(|error| error.to_string())
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
        self.controller().reclaim_retired()
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.controller().latest_telemetry()
    }

    fn poll_runtime_error(&mut self) -> Option<String> {
        self.runtime_failure.borrow_mut().take()
    }

    fn capture_source_rate(&mut self, source: CaptureSource) -> Result<u32, CaptureError> {
        match source {
            CaptureSource::Resample => Ok(self.sample_rate),
            CaptureSource::Input => Err(CaptureError::Unsupported),
        }
    }

    fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureCommandFailure> {
        self.controller()
            .arm_capture(buffer)
            .unwrap_or_else(|failure| panic!("capture arm rejected: {:?}", failure.error()));
        Ok(())
    }

    fn start_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        assert_eq!(source, CaptureSource::Resample);
        self.controller()
            .start_capture(token)
            .unwrap_or_else(|failure| panic!("capture start rejected: {:?}", failure.error()));
        Ok(())
    }

    fn stop_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        assert_eq!(source, CaptureSource::Resample);
        self.controller()
            .stop_capture(token)
            .unwrap_or_else(|failure| panic!("capture stop rejected: {:?}", failure.error()));
        Ok(())
    }

    fn cancel_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        assert_eq!(source, CaptureSource::Resample);
        self.controller()
            .cancel_capture(token)
            .unwrap_or_else(|failure| panic!("capture cancel rejected: {:?}", failure.error()));
        Ok(())
    }

    fn capture_status(&mut self, source: CaptureSource) -> Option<CaptureStatus> {
        (source == CaptureSource::Resample)
            .then(|| self.controller.borrow().capture_status())
            .flatten()
    }

    fn capture_completion(&mut self, source: CaptureSource) -> Option<CaptureOutcome> {
        (source == CaptureSource::Resample)
            .then(|| self.controller().try_capture_completion())
            .flatten()
    }
}

pub(crate) struct Harness {
    pub(crate) app: App,
    pub(crate) engine: AudioEngine,
    pub(crate) worker: WorkerHandle,
    pub(crate) controller: Rc<RefCell<AudioController>>,
    #[allow(
        dead_code,
        reason = "runtime device-loss support is consumed by the mixer_fx_workflow path module"
    )]
    runtime_failure: Rc<RefCell<Option<String>>>,
}

impl Harness {
    pub(crate) fn new() -> Self {
        Self::new_with_worker(WorkerHandle::spawn())
    }

    pub(crate) fn new_with_worker(worker: WorkerHandle) -> Self {
        let (controller, ports) = audio_channels_with_test_capacities(8, 256, 8);
        let controller = Rc::new(RefCell::new(controller));
        let engine = AudioEngine::new(48_000, ports).unwrap();
        let runtime_failure = Rc::new(RefCell::new(None));
        let mut app = App::with_audio(Box::new(ControllerPort {
            sample_rate: 48_000,
            controller: Rc::clone(&controller),
            runtime_failure: Rc::clone(&runtime_failure),
        }));
        app.set_keyboard_capabilities(KeyboardCapabilities {
            release_events: true,
        });
        let mut harness = Self {
            app,
            engine,
            worker,
            controller,
            runtime_failure,
        };
        for _ in 0..40 {
            harness.app.maintain_audio();
            harness.engine.render_frames(0, |_| {});
        }
        harness
    }

    fn dispatch(&mut self, request: WorkerRequest) -> bool {
        self.worker
            .try_send(request)
            .expect("worker request admitted");
        let result = self
            .worker
            .recv_timeout(Duration::from_secs(5))
            .expect("worker result");
        self.app.apply_worker_result(result)
    }

    pub(crate) fn dispatch_queued(&mut self) -> usize {
        let requests = self.app.take_worker_requests();
        assert!(requests.len() <= 1, "project work is bounded per iteration");
        let count = requests.len();
        for request in requests {
            assert!(self.dispatch(request));
        }
        count
    }

    pub(crate) fn load(&mut self, pad: PadId, path: &Path) {
        let request = self.app.begin_load(pad, path).expect("load request");
        assert!(self.dispatch(request));
        self.engine.render_frames(0, |_| {});
    }

    pub(crate) fn edit(&mut self, pad: PadId, recipe: SampleEditRecipe) {
        self.app.request_sample_edit(pad, recipe).unwrap();
        assert_eq!(self.dispatch_queued(), 1);
        assert!(self.app.maintain_audio());
        self.engine.render_frames(0, |_| {});
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app.apply_key(KeyEvent::new_with_kind(
            code,
            modifiers,
            KeyEventKind::Press,
        ));
    }

    pub(crate) fn palette(&mut self, command: &str) {
        self.key(KeyCode::Char(':'), KeyModifiers::SHIFT);
        self.app
            .apply_terminal_event(Event::Paste(command.to_owned()));
        self.key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(self.app.palette_error(), None, "{command}");
        self.engine.render_frames(0, |_| {});
    }

    pub(crate) fn record_hit(&mut self, index: usize) {
        self.key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        self.engine.render_frames(1, |_| {});
        self.app.apply(InputAction::PadPress(index));
        self.engine.render_frames(65, |_| {});
        self.app.apply(InputAction::PadRelease(index));
        self.engine.render_frames(65, |_| {});
        self.app.tick();
        self.app.maintain_audio();
        self.engine.render_frames(0, |_| {});
        self.key(KeyCode::Char('r'), KeyModifiers::CONTROL);
    }

    pub(crate) fn save_as(&mut self, directory: &Path, now: Instant) {
        self.app.request_save_as(directory).unwrap();
        assert!(self.app.maintain_project(now));
        assert_eq!(self.dispatch_queued(), 1);
    }

    pub(crate) fn open(&mut self, directory: &Path, choice: Option<RecoveryChoice>, now: Instant) {
        self.app.request_open_project(directory).unwrap();
        assert_eq!(self.dispatch_queued(), 1);
        if self
            .app
            .project_open_stage()
            .is_some_and(|stage| stage.phase == ProjectOpenPhase::AwaitingRecoveryChoice)
        {
            self.app
                .choose_project_recovery(choice.expect("recovery choice"))
                .unwrap();
            self.dispatch_queued();
        }
        self.finish_open(now);
    }

    #[allow(
        dead_code,
        reason = "continuous capture support is consumed by the mixer_fx_workflow path module"
    )]
    pub(crate) fn resample_and_install(&mut self, frames: usize) -> Vec<f32> {
        self.app
            .request_capture_with_frame_limit(CaptureSource::Resample, frames + 16)
            .unwrap();
        self.engine.render_frames(0, |_| {});
        self.engine.render_frames(0, |_| {});
        let mut expected = Vec::with_capacity(frames * 2);
        self.engine
            .render_frames(frames, |frame| expected.extend_from_slice(&frame));
        self.app.stop_capture().unwrap();
        self.engine.render_frames(0, |_| {});
        for _ in 0..32 {
            self.app.maintain_capture();
            self.dispatch_queued();
            self.engine.render_frames(0, |_| {});
            if self.app.capture_session().phase().is_none() {
                return expected;
            }
        }
        panic!(
            "resample did not install: {:?} {}",
            self.app.capture_session().phase(),
            self.app.status()
        );
    }

    #[allow(
        dead_code,
        reason = "runtime device-loss support is consumed by the mixer_fx_workflow path module"
    )]
    pub(crate) fn fail_runtime(&mut self, message: &str) {
        *self.runtime_failure.borrow_mut() = Some(message.to_owned());
        self.app.maintain_audio();
    }

    #[allow(
        dead_code,
        reason = "runtime retry support is consumed by the mixer_fx_workflow path module"
    )]
    pub(crate) fn retry_fresh_audio(&mut self) -> bool {
        let (controller, ports) = audio_channels_with_test_capacities(8, 256, 8);
        let controller = Rc::new(RefCell::new(controller));
        let engine = AudioEngine::new(48_000, ports).unwrap();
        let runtime_failure = Rc::new(RefCell::new(None));
        let admitted = self.app.retry_with(Box::new(ControllerPort {
            sample_rate: 48_000,
            controller: Rc::clone(&controller),
            runtime_failure: Rc::clone(&runtime_failure),
        }));
        if admitted {
            self.controller = controller;
            self.engine = engine;
            self.runtime_failure = runtime_failure;
            for _ in 0..256 {
                self.app.maintain_audio();
                self.engine.render_frames(0, |_| {});
            }
        }
        admitted
    }

    pub(crate) fn finish_open(&mut self, now: Instant) {
        for _ in 0..256 {
            if self.app.project_open_stage().is_none() {
                return;
            }
            self.app.maintain_project(now);
            self.dispatch_queued();
            self.engine.render_frames(0, |_| {});
            self.app.maintain_audio();
        }
        panic!(
            "project open did not complete: stage={:?} status={} error={:?}",
            self.app.project_open_stage(),
            self.app.status(),
            self.app.project_open_error()
        );
    }

    pub(crate) fn autosave(&mut self, now: Instant) {
        // A successful explicit save queues exact recovery cleanup ahead of later autosave work.
        // Drain that bounded item, then the recovery save at the same already-quiet timestamp.
        let mut dispatched = 0;
        for _ in 0..2 {
            if self.app.maintain_project(now + Duration::from_secs(3)) {
                dispatched += self.dispatch_queued();
            }
        }
        assert!(
            dispatched >= 1,
            "autosave must dispatch bounded project work"
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.worker.shutdown().expect("worker shutdown");
    }
}
