//! Cross-layer persistence evidence using the real filesystem worker and audio engine. Physical
//! device I/O is the only substituted boundary.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hound::{SampleFormat, WavSpec, WavWriter};
use sampler_audio::{
    AudioController, AudioEngine, ControlError, Frame, LiveAck, LiveCommandId, PatternSnapshotSlot,
    PatternSwitch, SampleBuffer, SampleSlot, Telemetry, audio_channels_with_test_capacities,
};
use sampler_core::{
    BankId, PadId, PadSettings, PatternSlotId, PatternSnapshot, PlaybackMode, SAMPLE_PHASE_SCALE,
    SampleEditRecipe,
};
use sampler_tui::{
    App, AudioPort, CaptureSupport, InputAction, KeyboardCapabilities, ProjectOpenPhase,
    ProjectStore, RecoveryChoice, WorkerHandle, WorkerRequest,
};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

const FLAC_MONO_8K: &str = concat!(
    "664c6143000000220040004000003700003701f400f000000040e70a3b8c6676f736d5d5de649e17",
    "e3cf84000028200000007265666572656e6365206c6962464c414320312e342e3320323032333036",
    "323300000000fff86408003f5e1309771421861c9edc001861c9ee4001861c9edc001861c9ee4001",
    "861c9edc001861c9ee4001861c9edc00186180b769",
);

struct FixtureTree {
    root: PathBuf,
}

impl FixtureTree {
    fn new() -> Self {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-project-workflow-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create fixture root");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn write_wav(&self, name: &str) -> PathBuf {
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

    fn write_flac(&self, name: &str) -> PathBuf {
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
    ) -> Result<SampleSlot, String> {
        self.controller()
            .install(pad, sample, settings)
            .map_err(|error| error.to_string())
    }

    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, String> {
        self.controller()
            .install_recovery(pad, sample, settings)
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

    fn reclaim_retired(&mut self) -> usize {
        self.controller().reclaim_retired()
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.controller().latest_telemetry()
    }

    fn poll_runtime_error(&mut self) -> Option<String> {
        None
    }
}

struct Harness {
    app: App,
    engine: AudioEngine,
    worker: WorkerHandle,
    controller: Rc<RefCell<AudioController>>,
}

impl Harness {
    fn new() -> Self {
        Self::new_with_worker(WorkerHandle::spawn())
    }

    fn new_with_worker(worker: WorkerHandle) -> Self {
        let (controller, ports) = audio_channels_with_test_capacities(8, 256, 8);
        let controller = Rc::new(RefCell::new(controller));
        let engine = AudioEngine::new(48_000, ports).unwrap();
        let mut app = App::with_audio(Box::new(ControllerPort {
            sample_rate: 48_000,
            controller: Rc::clone(&controller),
        }));
        app.set_keyboard_capabilities(KeyboardCapabilities {
            release_events: true,
        });
        let mut harness = Self {
            app,
            engine,
            worker,
            controller,
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

    fn dispatch_queued(&mut self) -> usize {
        let requests = self.app.take_worker_requests();
        assert!(requests.len() <= 1, "project work is bounded per iteration");
        let count = requests.len();
        for request in requests {
            assert!(self.dispatch(request));
        }
        count
    }

    fn load(&mut self, pad: PadId, path: &Path) {
        let request = self.app.begin_load(pad, path).expect("load request");
        assert!(self.dispatch(request));
        self.engine.render_frames(0, |_| {});
    }

    fn edit(&mut self, pad: PadId, recipe: SampleEditRecipe) {
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

    fn palette(&mut self, command: &str) {
        self.key(KeyCode::Char(':'), KeyModifiers::SHIFT);
        self.app
            .apply_terminal_event(Event::Paste(command.to_owned()));
        self.key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(self.app.palette_error(), None, "{command}");
    }

    fn record_hit(&mut self, index: usize) {
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

    fn save_as(&mut self, directory: &Path, now: Instant) {
        self.app.request_save_as(directory).unwrap();
        assert!(self.app.maintain_project(now));
        assert_eq!(self.dispatch_queued(), 1);
    }

    fn open(&mut self, directory: &Path, choice: Option<RecoveryChoice>, now: Instant) {
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

    fn finish_open(&mut self, now: Instant) {
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

    fn autosave(&mut self, now: Instant) {
        // A successful explicit save queues exact recovery cleanup ahead of later autosave work.
        // Drain that bounded item, then the recovery save at the same already-quiet timestamp.
        for _ in 0..2 {
            assert!(self.app.maintain_project(now + Duration::from_secs(3)));
            assert_eq!(self.dispatch_queued(), 1);
        }
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

fn copy_project(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    fs::copy(
        source.join("project.toml"),
        destination.join("project.toml"),
    )
    .unwrap();
    fs::create_dir(destination.join("audio")).unwrap();
    for entry in fs::read_dir(source.join("audio")).unwrap() {
        let entry = entry.unwrap();
        fs::copy(
            entry.path(),
            destination.join("audio").join(entry.file_name()),
        )
        .unwrap();
    }
}

fn assert_failed_open_preserves(harness: &mut Harness, directory: &Path, now: Instant) {
    let before = harness.app.project_snapshot().unwrap();
    harness.app.request_open_project(directory).unwrap();
    assert_eq!(harness.dispatch_queued(), 1);
    for _ in 0..32 {
        if harness.app.project_open_stage().is_none() {
            break;
        }
        harness.app.maintain_project(now);
        harness.dispatch_queued();
    }
    assert!(harness.app.project_open_error().is_some());
    assert_eq!(harness.app.project_snapshot().unwrap(), before);
}

#[test]
fn real_save_move_and_fresh_open_preserve_the_portable_project_tuple() {
    let fixture = FixtureTree::new();
    let wav = fixture.write_wav("asymmetric.wav");
    let flac = fixture.write_flac("asymmetric.flac");
    let wav_before = fs::read(&wav).unwrap();
    let flac_before = fs::read(&flac).unwrap();
    let project = fixture.path("portable-project");
    let moved = fixture.path("renamed-project");
    let now = Instant::now();

    let mut source = Harness::new();
    source.load(pad(0), &wav);
    source.load(pad(7), &flac);
    let recipe_a = SampleEditRecipe::new(
        SAMPLE_PHASE_SCALE / 4,
        SAMPLE_PHASE_SCALE * 3 / 4,
        true,
        true,
    )
    .unwrap();
    let recipe_b = SampleEditRecipe::new(0, SAMPLE_PHASE_SCALE / 2, false, true).unwrap();
    source.edit(pad(0), recipe_a);
    source.edit(pad(7), recipe_b);
    let rendered_a = source.app.pad(pad(0)).sample.as_ref().unwrap();
    let rendered_b = source.app.pad(pad(7)).sample.as_ref().unwrap();
    assert_ne!(
        rendered_a.data(),
        source.app.base_sample(pad(0)).unwrap().data(),
        "the WAV evidence must be nonidentity rendered PCM"
    );
    assert_ne!(
        rendered_b.data(),
        source.app.base_sample(pad(7)).unwrap().data(),
        "the FLAC evidence must be nonidentity rendered PCM"
    );
    let rendered_a_data = rendered_a.data().to_vec();
    let rendered_b_data = rendered_b.data().to_vec();
    let rendered_a_endpoints = [
        [rendered_a_data[0], rendered_a_data[1]],
        [
            rendered_a_data[rendered_a_data.len() - 2],
            rendered_a_data[rendered_a_data.len() - 1],
        ],
    ];
    let rendered_b_endpoints = [
        [rendered_b_data[0], rendered_b_data[1]],
        [
            rendered_b_data[rendered_b_data.len() - 2],
            rendered_b_data[rendered_b_data.len() - 1],
        ],
    ];
    let preview_a = source.app.pad(pad(0)).preview;
    let preview_b = source.app.pad(pad(7)).preview;
    let settings_a = PadSettings::new(PlaybackMode::Gate, -3.0, -0.25, -7.0, None).unwrap();
    let settings_b = PadSettings::new(PlaybackMode::Loop, -6.0, 0.5, 12.0, None).unwrap();
    source.app.update_pad_settings(pad(0), settings_a).unwrap();
    source.app.update_pad_settings(pad(7), settings_b).unwrap();
    source.palette("pattern 1");
    source.palette("tempo 137");
    source.palette("swing 63");
    source.palette("quantize 80");
    source.record_hit(0);
    source.palette("pattern 9");
    source.palette("tempo 91");
    source.palette("bars 2");
    source.palette("resolution 1/32");
    source.record_hit(7);
    let editable_before_save = source.app.project_snapshot().unwrap();
    let fingerprint_a = editable_before_save
        .pads
        .iter()
        .find(|saved_pad| saved_pad.pad == pad(0))
        .unwrap()
        .fingerprint;
    let fingerprint_b = editable_before_save
        .pads
        .iter()
        .find(|saved_pad| saved_pad.pad == pad(7))
        .unwrap()
        .fingerprint;

    source.save_as(&project, now);
    let saved_snapshot = source.app.project_snapshot().unwrap();
    let explicit_bytes = fs::read(project.join("project.toml")).unwrap();
    let explicit_text = std::str::from_utf8(&explicit_bytes).unwrap();
    assert!(explicit_text.contains("schema_version = 3"));
    let probe = ProjectStore.probe(&project).unwrap();
    let document = probe.explicit.unwrap().unwrap();
    assert_eq!(document.pads.len(), 2);
    assert_eq!(document.patterns.len(), 16);
    for saved_pad in &document.pads {
        assert!(saved_pad.audio_path.starts_with("audio/"));
        assert!(
            saved_pad
                .audio_path
                .contains(&saved_pad.asset_digest.to_string())
        );
        assert!(project.join(&saved_pad.audio_path).is_file());
    }
    assert!(document.patterns[0].events.iter().all(|event| {
        event.raw_frame
            <= document.patterns[0]
                .to_editable()
                .unwrap()
                .transport()
                .loop_frames()
    }));
    assert!(document.patterns[8].events.iter().all(|event| {
        event.raw_frame
            <= document.patterns[8]
                .to_editable()
                .unwrap()
                .transport()
                .loop_frames()
    }));
    assert_eq!(fs::read(&wav).unwrap(), wav_before);
    assert_eq!(fs::read(&flac).unwrap(), flac_before);

    fs::rename(&project, &moved).unwrap();
    let mut reopened = Harness::new();
    reopened.open(&moved, None, now);
    let after_open = reopened.app.project_snapshot().unwrap();
    assert_eq!(after_open.project_id, saved_snapshot.project_id);
    assert_eq!(after_open.pads.len(), 2);
    assert_eq!(after_open.patterns, editable_before_save.patterns);
    assert_eq!(reopened.app.pad(pad(0)).settings, settings_a);
    assert_eq!(reopened.app.pad(pad(7)).settings, settings_b);
    assert_eq!(reopened.app.committed_sample_recipe(pad(0)), Some(recipe_a));
    assert_eq!(reopened.app.committed_sample_recipe(pad(7)), Some(recipe_b));
    let reopened_a = reopened.app.pad(pad(0)).sample.as_ref().unwrap();
    let reopened_b = reopened.app.pad(pad(7)).sample.as_ref().unwrap();
    assert_eq!(reopened_a.data(), rendered_a_data);
    assert_eq!(reopened_b.data(), rendered_b_data);
    assert_eq!(
        [
            [reopened_a.data()[0], reopened_a.data()[1]],
            [
                reopened_a.data()[reopened_a.data().len() - 2],
                reopened_a.data()[reopened_a.data().len() - 1],
            ],
        ],
        rendered_a_endpoints
    );
    assert_eq!(
        [
            [reopened_b.data()[0], reopened_b.data()[1]],
            [
                reopened_b.data()[reopened_b.data().len() - 2],
                reopened_b.data()[reopened_b.data().len() - 1],
            ],
        ],
        rendered_b_endpoints
    );
    assert_eq!(reopened.app.pad(pad(0)).preview, preview_a);
    assert_eq!(reopened.app.pad(pad(7)).preview, preview_b);
    assert_eq!(
        after_open
            .pads
            .iter()
            .find(|saved_pad| saved_pad.pad == pad(0))
            .unwrap()
            .fingerprint,
        fingerprint_a
    );
    assert_eq!(
        after_open
            .pads
            .iter()
            .find(|saved_pad| saved_pad.pad == pad(7))
            .unwrap()
            .fingerprint,
        fingerprint_b
    );
    assert!(reopened.app.pad(pad(15)).sample.is_none());
    reopened.app.apply(InputAction::PadPress(15));
    reopened.engine.render_frames(65, |_| {});
    assert_eq!(reopened.engine.active_voices(), 0);
    let triggers = reopened.engine.executed_triggers();
    reopened.app.apply(InputAction::PadPress(0));
    let mut wav_output = [0.0; 2];
    reopened
        .engine
        .render_frames(65, |frame| wav_output = frame);
    assert!(reopened.engine.executed_triggers() > triggers);
    assert!(wav_output[0] < 0.0 && wav_output[1] > 0.0);
    reopened.app.apply(InputAction::PadRelease(0));
    reopened.engine.render_frames(65, |_| {});
    let triggers = reopened.engine.executed_triggers();
    reopened.app.apply(InputAction::PadPress(7));
    let mut flac_peak = [0.0_f32; 2];
    reopened.engine.render_frames(512, |frame| {
        flac_peak[0] = flac_peak[0].max(frame[0].abs());
        flac_peak[1] = flac_peak[1].max(frame[1].abs());
    });
    assert!(reopened.engine.executed_triggers() > triggers);
    assert!(
        flac_peak[0] > 1.0e-4 && flac_peak[1] > flac_peak[0],
        "rendered FLAC trigger must be audible with rightward pan: {flac_peak:?}"
    );
    assert_eq!(fs::read(&wav).unwrap(), wav_before);
    assert_eq!(fs::read(&flac).unwrap(), flac_before);
}

#[test]
fn real_autosave_restore_discard_and_failed_open_preserve_explicit_or_running_truth() {
    let fixture = FixtureTree::new();
    let wav = fixture.write_wav("source.wav");
    let source_bytes = fs::read(&wav).unwrap();
    let project = fixture.path("recovery-project");
    let now = Instant::now();
    let mut source = Harness::new();
    source.load(pad(0), &wav);
    source.save_as(&project, now);
    let explicit_bytes = fs::read(project.join("project.toml")).unwrap();

    source.palette("tempo 139");
    source.palette("tempo 144");
    source.palette("tempo 149");
    let recovery_snapshot = source.app.project_snapshot().unwrap();
    source.autosave(now);
    assert_eq!(
        fs::read(project.join("project.toml")).unwrap(),
        explicit_bytes
    );
    assert!(project.join(".sampler-tui-recovery.toml").is_file());

    let mut restored = Harness::new();
    restored.open(&project, Some(RecoveryChoice::Restore), now);
    assert_eq!(
        restored.app.project_snapshot().unwrap().patterns,
        recovery_snapshot.patterns
    );
    assert!(restored.app.project_header().contains("MODIFIED"));
    drop(restored);

    let mut discarded = Harness::new();
    discarded.open(&project, Some(RecoveryChoice::Discard), now);
    assert!(!project.join(".sampler-tui-recovery.toml").exists());
    assert_eq!(
        discarded.app.project_snapshot().unwrap().patterns,
        ProjectStore
            .probe(&project)
            .unwrap()
            .explicit
            .unwrap()
            .unwrap()
            .patterns
    );

    let saved_document = ProjectStore
        .probe(&project)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();
    let asset = &saved_document.pads[0].audio_path;

    let changed = fixture.path("changed-project");
    copy_project(&project, &changed);
    fs::write(changed.join(asset), b"changed asset").unwrap();
    assert_failed_open_preserves(&mut discarded, &changed, now);

    let missing = fixture.path("missing-project");
    copy_project(&project, &missing);
    fs::remove_file(missing.join(asset)).unwrap();
    assert_failed_open_preserves(&mut discarded, &missing, now);

    let corrupt = fixture.path("corrupt-project");
    copy_project(&project, &corrupt);
    fs::write(corrupt.join("project.toml"), b"not = [valid").unwrap();
    assert_failed_open_preserves(&mut discarded, &corrupt, now);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let linked = fixture.path("linked-project");
        copy_project(&project, &linked);
        fs::remove_file(linked.join(asset)).unwrap();
        symlink(&wav, linked.join(asset)).unwrap();
        assert_failed_open_preserves(&mut discarded, &linked, now);
    }

    assert_eq!(fs::read(&wav).unwrap(), source_bytes);
    assert_eq!(
        fs::read(project.join("project.toml")).unwrap(),
        explicit_bytes
    );
}

#[test]
fn real_worker_opens_sparse_v1_pattern_as_exact_slot_zero_plus_defaults() {
    let fixture = FixtureTree::new();
    let source = fixture.write_wav("legacy-source.wav");
    let project = fixture.path("legacy-project");
    fs::create_dir_all(project.join("audio")).unwrap();
    fs::copy(source, project.join("audio/kick.wav")).unwrap();
    fs::write(
        project.join("project.toml"),
        r#"
schema_version = 1
name = "Legacy Sparse"

[[pads]]
audio_path = "audio/kick.wav"

[pads.pad]
bank = 0
index = 0

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0

[[patterns]]
name = "Legacy Beat"
sample_rate = 48000
tempo = 133.0
bars = 2
resolution = "eighth"
swing = 0.61

[patterns.meter]
numerator = 4
denominator = 4

[[patterns.events]]
id = 9
frame = 6800
velocity = 0.75

[patterns.events.pad]
bank = 0
index = 0
"#,
    )
    .unwrap();

    let mut opened = Harness::new();
    opened.open(&project, None, Instant::now());
    let patterns = opened.app.patterns().export_project_patterns().unwrap();
    assert_eq!(patterns.len(), sampler_core::PATTERN_SLOT_COUNT);
    assert_eq!(patterns[0].slot, PatternSlotId::new(0).unwrap());
    assert_eq!(patterns[0].name, "Legacy Beat");
    assert_eq!(patterns[0].tempo.bpm(), 133.0);
    assert_eq!(patterns[0].bars, 2);
    assert_eq!(patterns[0].events.len(), 1);
    assert_eq!(patterns[0].events[0].event.frame, 6_800);
    assert_eq!(patterns[0].events[0].raw_frame, 6_800);
    for (index, pattern) in patterns.iter().enumerate().skip(1) {
        assert_eq!(pattern.slot, PatternSlotId::new(index as u8).unwrap());
        assert_eq!(pattern.name, format!("Pattern {:02}", index + 1));
        assert!(pattern.events.is_empty());
        assert_eq!(pattern.sample_rate, 48_000);
        assert_eq!(pattern.tempo.bpm(), 120.0);
    }
}

#[test]
fn project_open_install_backpressure_keeps_the_old_tuple_until_retry_commits() {
    let fixture = FixtureTree::new();
    let wav = fixture.write_wav("source.wav");
    let project = fixture.path("backpressure-project");
    let now = Instant::now();
    let mut source = Harness::new();
    source.load(pad(0), &wav);
    source.save_as(&project, now);

    let mut target = Harness::new();
    let old = target.app.project_snapshot().unwrap();
    target.app.request_open_project(&project).unwrap();
    assert_eq!(target.dispatch_queued(), 1);
    while target
        .app
        .project_open_stage()
        .is_some_and(|stage| stage.phase != ProjectOpenPhase::Admitting)
    {
        target.app.maintain_project(now);
        target.dispatch_queued();
    }
    let blocked_progress = target.app.project_open_stage().unwrap().clone();
    assert_eq!(
        blocked_progress.admitted_actions, 1,
        "StopAll admitted first"
    );
    let mut saturated_commands = 0;
    loop {
        match target.controller.borrow_mut().stop_pad(pad(15)) {
            Ok(()) => saturated_commands += 1,
            Err(ControlError::CommandQueueFull) => break,
            Err(error) => panic!("unexpected controller saturation error: {error}"),
        }
    }
    assert_eq!(saturated_commands, 8);
    target.app.maintain_project(now);
    assert!(target.app.status().contains("queue is full"));
    assert_eq!(target.app.project_open_stage().unwrap(), &blocked_progress);
    assert_eq!(target.app.project_snapshot().unwrap(), old);
    assert!(target.app.pad(pad(0)).sample.is_none());

    target.engine.render_frames(0, |_| {});
    target.app.maintain_audio();
    assert!(target.app.maintain_project(now));
    assert_eq!(
        target.app.project_open_stage().unwrap().admitted_actions,
        blocked_progress.admitted_actions + 1,
        "the exact blocked pad install advances once after queue drain"
    );
    target.finish_open(now);
    assert!(target.app.pad(pad(0)).sample.is_some());
    assert_ne!(
        target.app.project_snapshot().unwrap().project_id,
        old.project_id
    );
}

#[cfg(unix)]
#[test]
fn probe_then_symlink_replacement_fails_secure_stage_without_replacing_old_tuple() {
    use std::os::unix::fs::symlink;

    let fixture = FixtureTree::new();
    let source_path = fixture.write_wav("source.wav");
    let project = fixture.path("stage-race-project");
    let now = Instant::now();
    let mut source = Harness::new();
    source.load(pad(0), &source_path);
    source.save_as(&project, now);
    let document = ProjectStore
        .probe(&project)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();
    let asset = project.join(&document.pads[0].audio_path);

    let mut target = Harness::new();
    let old = target.app.project_snapshot().unwrap();
    target.app.request_open_project(&project).unwrap();
    assert_eq!(target.dispatch_queued(), 1, "probe completes first");
    fs::remove_file(&asset).unwrap();
    symlink(&source_path, &asset).unwrap();
    assert!(target.app.maintain_project(now));
    assert_eq!(target.dispatch_queued(), 1, "secure stage reads next");

    assert!(target.app.project_open_error().is_some());
    assert_eq!(target.app.project_snapshot().unwrap(), old);
}

#[cfg(unix)]
#[test]
fn stage_fails_if_project_directory_path_changes_after_asset_fd_open() {
    use std::os::unix::fs::symlink;

    let fixture = FixtureTree::new();
    let source_path = fixture.write_wav("directory-race-source.wav");
    let project = fixture.path("directory-race-project");
    let moved = fixture.path("opened-directory-race-project");
    let now = Instant::now();
    let mut source = Harness::new();
    source.load(pad(0), &source_path);
    source.save_as(&project, now);

    let opened = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let worker_opened = Arc::clone(&opened);
    let worker_resume = Arc::clone(&resume);
    let worker = WorkerHandle::spawn_with_project_asset_open_hook(move || {
        worker_opened.wait();
        worker_resume.wait();
    });
    let mut target = Harness::new_with_worker(worker);
    let old = target.app.project_snapshot().unwrap();
    let old_header = target.app.project_header();
    target.app.request_open_project(&project).unwrap();
    assert_eq!(target.dispatch_queued(), 1, "probe completes first");
    assert!(target.app.maintain_project(now));
    let [request] = target.app.take_worker_requests().try_into().unwrap();
    target.worker.try_send(request).unwrap();

    opened.wait();
    fs::rename(&project, &moved).unwrap();
    symlink(&moved, &project).unwrap();
    resume.wait();
    let result = target.worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(target.app.apply_worker_result(result));

    assert!(matches!(
        target.app.project_open_error(),
        Some(sampler_tui::ProjectOpenError::Stage {
            error: sampler_tui::ProjectStageError::Load(
                sampler_tui::LoadSampleError::ProjectAsset(
                    sampler_tui::ProjectStoreError::Filesystem {
                        operation: "verify project directory identity",
                        ..
                    }
                )
            ),
            ..
        })
    ));
    assert_eq!(target.app.project_snapshot().unwrap(), old);
    assert_eq!(target.app.project_header(), old_header);
}
