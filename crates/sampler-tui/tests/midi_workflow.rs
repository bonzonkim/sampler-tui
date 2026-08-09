//! Direct MIDI workflow evidence through the real app, controller, engine, worker, and store.
//! Physical MIDI and audio devices are the only substituted boundaries.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hound::{SampleFormat, WavSpec, WavWriter};
use sampler_audio::{
    AudioController, AudioEngine, Frame, LiveAck, LiveAckKind, LiveCommandId, PatternSnapshotSlot,
    PatternSwitch, SampleBuffer, SampleSlot, Telemetry, audio_channels,
};
use sampler_core::{
    BankId, MasterMixSettings, MidiSettings, PadId, PadMixSettings, PadSettings, PatternSlotId,
    PatternSnapshot, PlaybackMode,
};
use sampler_tui::{
    App, AudioPort, CaptureSupport, InputAction, MAX_MIDI_DRAIN, MIDI_INGRESS_CAPACITY,
    MidiBackend, MidiBackendPort, MidiConnection, MidiIngressProducer, MidiService,
    MidiServiceError, ProjectOpenPhase, ProjectStore, RecoveryChoice, WorkerHandle, WorkerRequest,
    midi_ingress,
};

const SAMPLE_RATE: u32 = 1_000;
const SCHEMA_V3_PROJECT: &str = include_str!("fixtures/schema-v3-default-midi.toml");
static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct CountingAllocator {
    allocations: AtomicUsize,
    deallocations: AtomicUsize,
}

thread_local! {
    static COUNT_THIS_THREAD: AtomicBool = const { AtomicBool::new(false) };
}

impl CountingAllocator {
    const fn new() -> Self {
        Self {
            allocations: AtomicUsize::new(0),
            deallocations: AtomicUsize::new(0),
        }
    }

    fn reset_and_enable(&self) {
        self.allocations.store(0, Ordering::Relaxed);
        self.deallocations.store(0, Ordering::Relaxed);
        COUNT_THIS_THREAD.with(|enabled| enabled.store(true, Ordering::Release));
    }

    fn disable(&self) {
        COUNT_THIS_THREAD.with(|enabled| enabled.store(false, Ordering::Release));
    }
}

// SAFETY: Every operation delegates to `System` with the original pointer and layout.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_THIS_THREAD.with(|enabled| enabled.load(Ordering::Acquire)) {
            self.allocations.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: The caller upholds `GlobalAlloc::alloc`'s layout requirements.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNT_THIS_THREAD.with(|enabled| enabled.load(Ordering::Acquire)) {
            self.deallocations.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: The caller provides the pointer and layout returned by this allocator.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static COUNTS: CountingAllocator = CountingAllocator::new();

#[derive(Default)]
struct MidiState {
    ports: Vec<MidiBackendPort>,
    connected: Vec<String>,
    producer: Option<MidiIngressProducer>,
    closed: usize,
}

struct VirtualMidiBackend(Rc<RefCell<MidiState>>);
struct VirtualMidiConnection(Rc<RefCell<MidiState>>);

impl MidiConnection for VirtualMidiConnection {
    fn close(self: Box<Self>) {
        let mut state = self.0.borrow_mut();
        state.closed += 1;
        state.producer = None;
    }
}

impl MidiBackend for VirtualMidiBackend {
    fn list_ports(&mut self) -> Result<Vec<MidiBackendPort>, MidiServiceError> {
        Ok(self.0.borrow().ports.clone())
    }

    fn connect(
        &mut self,
        port: &MidiBackendPort,
        producer: MidiIngressProducer,
    ) -> Result<Box<dyn MidiConnection>, MidiServiceError> {
        let mut state = self.0.borrow_mut();
        state.connected.push(port.backend_id.clone());
        state.producer = Some(producer);
        drop(state);
        Ok(Box::new(VirtualMidiConnection(Rc::clone(&self.0))))
    }
}

struct ControllerPort {
    controller: Rc<RefCell<AudioController>>,
    observed_acks: Rc<RefCell<Vec<LiveAck>>>,
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
        SAMPLE_RATE
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
        mix: PadMixSettings,
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
        mix: PadMixSettings,
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

    fn release_owned_live_tracked(
        &mut self,
        pad: PadId,
        target_trigger_id: LiveCommandId,
    ) -> Result<LiveCommandId, String> {
        self.controller()
            .release_owned_live_tracked(pad, target_trigger_id)
            .map_err(|error| error.to_string())
    }

    fn release_owned_live_batch(
        &mut self,
        releases: &[(PadId, LiveCommandId)],
    ) -> Result<Vec<LiveCommandId>, String> {
        self.controller()
            .release_owned_live_batch(releases)
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
        let drained = self.controller().drain_live_acks(output);
        self.observed_acks
            .borrow_mut()
            .extend_from_slice(&output[..drained]);
        drained
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

    fn update_pad_mix(&mut self, pad: PadId, settings: PadMixSettings) -> Result<(), String> {
        self.controller()
            .update_pad_mix(pad, settings)
            .map_err(|error| error.to_string())
    }

    fn update_master_mix(&mut self, settings: MasterMixSettings) -> Result<(), String> {
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
        None
    }
}

struct FixtureTree {
    root: PathBuf,
}

impl FixtureTree {
    fn new() -> Self {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-midi-workflow-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("create MIDI workflow fixture root");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn write_wav(&self, name: &str) -> PathBuf {
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
        .expect("create WAV fixture");
        for index in 0..2_000 {
            let left = if index % 11 == 0 { 0.5 } else { 0.125 };
            writer.write_sample(left).unwrap();
            writer.write_sample(-left * 0.5).unwrap();
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

struct Harness {
    app: App,
    engine: AudioEngine,
    worker: WorkerHandle,
    midi: Rc<RefCell<MidiState>>,
    observed_acks: Rc<RefCell<Vec<LiveAck>>>,
}

impl Harness {
    fn new(ports: &[(&str, &str)], now: Instant) -> Self {
        let midi = Rc::new(RefCell::new(MidiState {
            ports: ports
                .iter()
                .map(|(backend_id, name)| MidiBackendPort {
                    backend_id: (*backend_id).to_owned(),
                    name: (*name).to_owned(),
                })
                .collect(),
            ..MidiState::default()
        }));
        let mut service = MidiService::new(Box::new(VirtualMidiBackend(Rc::clone(&midi))));
        service.startup(now).expect("virtual MIDI startup");

        let (controller, engine_ports) = audio_channels();
        let controller = Rc::new(RefCell::new(controller));
        let observed_acks = Rc::new(RefCell::new(Vec::new()));
        let engine = AudioEngine::new(SAMPLE_RATE, engine_ports).unwrap();
        let app = App::with_audio_and_midi(
            Box::new(ControllerPort {
                controller,
                observed_acks: Rc::clone(&observed_acks),
            }),
            service,
        );
        let mut harness = Self {
            app,
            engine,
            worker: WorkerHandle::spawn(),
            midi,
            observed_acks,
        };
        for _ in 0..40 {
            harness.app.maintain_audio();
            harness.engine.render_frames(0, |_| {});
        }
        harness
    }

    fn dispatch(&mut self, request: WorkerRequest) {
        self.worker
            .try_send(request)
            .expect("worker request admitted");
        let result = self
            .worker
            .recv_timeout(Duration::from_secs(5))
            .expect("worker result");
        assert!(self.app.apply_worker_result(result));
    }

    fn dispatch_queued(&mut self) -> usize {
        let requests = self.app.take_worker_requests();
        assert!(requests.len() <= 1, "project work stays bounded");
        let count = requests.len();
        for request in requests {
            self.dispatch(request);
        }
        count
    }

    fn load(&mut self, pad: PadId, path: &Path) {
        let request = self.app.begin_load(pad, path).expect("load request");
        self.dispatch(request);
        self.engine.render_frames(0, |_| {});
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app
            .apply_terminal_event(Event::Key(KeyEvent::new_with_kind(
                code,
                modifiers,
                KeyEventKind::Press,
            )));
    }

    fn palette(&mut self, command: &str) {
        self.key(KeyCode::Char(':'), KeyModifiers::SHIFT);
        self.app
            .apply_terminal_event(Event::Paste(command.to_owned()));
        self.key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(self.app.palette_error(), None, "{command}");
        self.engine.render_frames(0, |_| {});
    }

    fn send(&mut self, message: &[u8]) {
        self.midi
            .borrow_mut()
            .producer
            .as_mut()
            .expect("virtual port connected")
            .try_push_message(message);
    }

    fn midi_slice(&mut self, now: Instant) {
        assert!(self.app.maintain_midi(now));
    }

    fn save_as(&mut self, directory: &Path, now: Instant) {
        self.app.request_save_as(directory).unwrap();
        assert!(self.app.maintain_project(now));
        assert_eq!(self.dispatch_queued(), 1);
    }

    fn autosave(&mut self, now: Instant) {
        let mut dispatched = 0;
        for _ in 0..2 {
            if self.app.maintain_project(now + Duration::from_secs(3)) {
                dispatched += self.dispatch_queued();
            }
        }
        assert!(dispatched >= 1, "recovery autosave must be written");
    }

    fn open(&mut self, directory: &Path, recovery: Option<RecoveryChoice>, now: Instant) {
        self.app.request_open_project(directory).unwrap();
        assert_eq!(self.dispatch_queued(), 1);
        if self
            .app
            .project_open_stage()
            .is_some_and(|stage| stage.phase == ProjectOpenPhase::AwaitingRecoveryChoice)
        {
            self.app
                .choose_project_recovery(recovery.expect("recovery choice"))
                .unwrap();
            self.dispatch_queued();
        }
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
            "project open did not finish: stage={:?} status={} error={:?}",
            self.app.project_open_stage(),
            self.app.status(),
            self.app.project_open_error()
        );
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.worker.shutdown().expect("worker shutdown");
    }
}

fn pad(bank: u8, index: u8) -> PadId {
    PadId::new(BankId::new(bank).unwrap(), index).unwrap()
}

fn render_dry_bits(app: &App, target: PadId) -> Vec<[u32; 2]> {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(SAMPLE_RATE, ports).unwrap();
    controller
        .update_master_mix(app.master_mix())
        .expect("master mix admitted");
    let view = app.pad(target);
    controller
        .install(
            target,
            Arc::clone(view.sample.as_ref().expect("loaded target")),
            view.settings,
            app.pad_mix(target),
        )
        .unwrap();
    controller.trigger_live(target, 1.0).unwrap();
    let mut bits = Vec::with_capacity(256);
    engine.render_frames(256, |frame| {
        bits.push([frame[0].to_bits(), frame[1].to_bits()]);
    });
    bits
}

fn assert_default_midi_map(settings: MidiSettings) {
    assert_eq!(settings, MidiSettings::default());
    for bank in 0..10 {
        let map = settings.bank(BankId::new(bank).unwrap());
        for index in 0..16 {
            assert_eq!(map.note(index).unwrap().unwrap().get(), 36 + index);
        }
    }
}

#[test]
fn callback_ingress_allocates_nothing_and_rapid_drains_are_strictly_bounded() {
    let (mut producer, mut consumer) = midi_ingress();
    let note_on = [0x90, 60, 100];

    COUNTS.reset_and_enable();
    producer.try_push_message(&[0xb0, 1, 127]);
    for _ in 0..MIDI_INGRESS_CAPACITY {
        producer.try_push_message(&note_on);
    }
    producer.try_push_message(&note_on);
    COUNTS.disable();

    assert_eq!(COUNTS.allocations.load(Ordering::Relaxed), 0);
    assert_eq!(COUNTS.deallocations.load(Ordering::Relaxed), 0);
    assert_eq!(consumer.take_lost_count(), 1);
    let mut events = [sampler_tui::MidiEvent::NoteOff {
        channel: sampler_core::MidiChannel::new(1).unwrap(),
        note: sampler_core::MidiNote::new(0).unwrap(),
    }; MAX_MIDI_DRAIN * 2];
    for _ in 0..MIDI_INGRESS_CAPACITY / MAX_MIDI_DRAIN {
        assert_eq!(consumer.drain_into(&mut events), MAX_MIDI_DRAIN);
    }
    assert_eq!(consumer.drain_into(&mut events), 0);
}

#[test]
fn literal_schema_v3_defaults_exact_midi_map_and_preserves_dry_render_bits() {
    let fixture = FixtureTree::new();
    let wav = fixture.write_wav("legacy.wav");
    let project = fixture.path("legacy-project");
    let now = Instant::now();
    let mut source = Harness::new(&[], now);
    source.load(pad(0, 0), &wav);
    let dry_before = render_dry_bits(&source.app, pad(0, 0));
    source.save_as(&project, now);
    let document = ProjectStore
        .probe(&project)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();
    let saved_pad = &document.pads[0];
    let literal = SCHEMA_V3_PROJECT
        .replace("__PROJECT_ID__", &document.project_id.to_string())
        .replace("__AUDIO_PATH__", &saved_pad.audio_path)
        .replace("__ASSET_DIGEST__", &saved_pad.asset_digest.to_string());
    fs::write(project.join("project.toml"), &literal).unwrap();
    drop(source);

    let mut migrated = Harness::new(&[], now);
    migrated.open(&project, None, now);
    assert_default_midi_map(migrated.app.midi_settings());
    assert_eq!(render_dry_bits(&migrated.app, pad(0, 0)), dry_before);
    assert_eq!(
        fs::read_to_string(project.join("project.toml")).unwrap(),
        literal
    );
}

#[test]
fn continuous_midi_workflow_survives_overflow_move_recovery_and_reconnect() {
    let fixture = FixtureTree::new();
    let wav = fixture.write_wav("performance.wav");
    let project = fixture.path("midi-project");
    let moved = fixture.path("moved-midi-project");
    let now = Instant::now();
    let mut source = Harness::new(
        &[("virtual-a", "Virtual A"), ("virtual-b", "Virtual B")],
        now,
    );
    assert!(!source.app.midi_connected());
    assert!(source.app.midi_status_text().unwrap().contains("2 ports"));
    source.palette("midi-connect 1");
    assert!(source.app.midi_connected());
    assert_eq!(source.midi.borrow().connected, ["virtual-b"]);

    source.load(pad(0, 0), &wav);
    source.load(pad(1, 1), &wav);
    source
        .app
        .update_pad_settings(
            pad(0, 0),
            PadSettings {
                mode: PlaybackMode::Gate,
                ..PadSettings::default()
            },
        )
        .unwrap();
    source.engine.render_frames(0, |_| {});

    source.palette("select 1");
    source.palette("midi-learn");
    source.send(&[0x90, 60, 100]);
    source.midi_slice(now);
    source.app.apply(InputAction::BankDelta(1));
    source.palette("select 2");
    source.palette("midi-learn");
    source.send(&[0x90, 72, 100]);
    source.midi_slice(now);
    assert_eq!(
        source
            .app
            .midi_settings()
            .bank(BankId::new(0).unwrap())
            .owner(sampler_core::MidiNote::new(60).unwrap()),
        Some(0)
    );
    assert_eq!(
        source
            .app
            .midi_settings()
            .bank(BankId::new(1).unwrap())
            .owner(sampler_core::MidiNote::new(72).unwrap()),
        Some(1)
    );
    let learned_map = MidiSettings::default()
        .learn_swap(
            BankId::new(0).unwrap(),
            0,
            sampler_core::MidiNote::new(60).unwrap(),
        )
        .unwrap()
        .learn_swap(
            BankId::new(1).unwrap(),
            1,
            sampler_core::MidiNote::new(72).unwrap(),
        )
        .unwrap();
    assert_eq!(source.app.midi_settings(), learned_map);

    // Learn events are ordinary Note On messages and therefore also audition their new pads.
    // Release those auditions before arming recording so its acknowledgement set is unambiguous.
    source.send(&[0x80, 60, 0]);
    source.send(&[0x80, 72, 0]);
    source.midi_slice(now);
    source.engine.render_frames(129, |_| {});
    source.observed_acks.borrow_mut().clear();

    source.app.apply(InputAction::BankDelta(-1));
    source.key(KeyCode::Char('r'), KeyModifiers::CONTROL);
    source.engine.render_frames(4, |_| {});
    source.app.maintain_audio();
    let trigger_expected = source.engine.rendered_frame() + 64;
    source.send(&[0x90, 60, 50]);
    source.midi_slice(now);
    source.engine.render_frames(65, |_| {});
    source.engine.render_frames(9, |_| {});
    let release_expected = source.engine.rendered_frame() + 64;
    source.send(&[0x80, 60, 0]);
    source.midi_slice(now);
    source.engine.render_frames(65, |_| {});
    source.app.maintain_audio();
    let acks = source.observed_acks.borrow();
    let trigger_ack = acks
        .iter()
        .find(
            |ack| matches!(ack.kind, LiveAckKind::Trigger { velocity } if velocity == 50.0 / 127.0),
        )
        .expect("recorded MIDI trigger acknowledgement");
    let release_ack = acks
        .iter()
        .find(|ack| matches!(ack.kind, LiveAckKind::Release) && ack.pad == pad(0, 0))
        .expect("recorded MIDI release acknowledgement");
    assert_eq!(trigger_ack.frame, trigger_expected);
    assert_eq!(release_ack.frame, release_expected);
    drop(acks);
    let recorded = &source.app.project_snapshot().unwrap().patterns[0].events[0];
    assert_eq!(recorded.event.pad, pad(0, 0));
    assert_eq!(recorded.event.velocity, 50.0 / 127.0);
    assert_eq!(
        recorded.event.duration,
        Some(release_expected - trigger_expected)
    );
    source.key(KeyCode::Char('r'), KeyModifiers::CONTROL);
    source.app.apply(InputAction::StopAll);
    source.engine.render_frames(0, |_| {});

    source.send(&[0x90, 60, 127]);
    source.midi_slice(now);
    source.engine.render_frames(65, |_| {});
    assert_eq!(source.engine.active_voices(), 1);
    source.app.apply(InputAction::BankDelta(1));
    source.send(&[0x80, 60, 0]);
    source.midi_slice(now);
    source.engine.render_frames(129, |_| {});
    assert_eq!(
        source.engine.active_voices(),
        0,
        "note-off after a bank change releases the trigger-time pad"
    );

    source.app.apply(InputAction::BankDelta(-1));
    source.send(&[0x90, 60, 127]);
    source.midi_slice(now);
    source.engine.render_frames(65, |_| {});
    for _ in 0..=MIDI_INGRESS_CAPACITY {
        source.send(&[0x90, 60, 1]);
    }
    source.midi_slice(now);
    for _ in 0..8 {
        source.app.maintain_midi(now);
    }
    source.engine.render_frames(129, |_| {});
    assert_eq!(source.engine.active_voices(), 0);
    assert!(
        source
            .app
            .midi_status_text()
            .unwrap()
            .contains("held notes released")
    );
    let before_clean = source.engine.executed_triggers();
    source.send(&[0x90, 60, 64]);
    source.midi_slice(now);
    source.engine.render_frames(65, |_| {});
    assert_eq!(source.engine.executed_triggers(), before_clean + 1);
    source.send(&[0x80, 60, 0]);
    source.midi_slice(now);
    source.engine.render_frames(129, |_| {});

    source.midi.borrow_mut().ports.clear();
    assert!(
        source
            .app
            .maintain_midi_service(now + Duration::from_secs(2))
    );
    assert!(!source.app.midi_connected());
    assert!(
        source
            .app
            .midi_status_text()
            .unwrap()
            .contains("disappeared")
    );
    source.midi.borrow_mut().ports = vec![MidiBackendPort {
        backend_id: "virtual-b".to_owned(),
        name: "Virtual B".to_owned(),
    }];
    source
        .app
        .maintain_midi_service(now + Duration::from_secs(4));
    source.palette("midi-connect 0");
    assert!(source.app.midi_connected());
    assert_eq!(source.midi.borrow().connected, ["virtual-b", "virtual-b"]);
    let before_reconnect_hit = source.engine.executed_triggers();
    source.send(&[0x90, 60, 100]);
    source.midi_slice(now + Duration::from_secs(4));
    source.engine.render_frames(65, |_| {});
    assert_eq!(source.engine.executed_triggers(), before_reconnect_hit + 1);
    source.send(&[0x80, 60, 0]);
    source.midi_slice(now + Duration::from_secs(4));
    source.engine.render_frames(129, |_| {});

    let exact_map = learned_map;
    let dry_bits = render_dry_bits(&source.app, pad(0, 0));
    source.save_as(&project, now);
    fs::rename(&project, &moved).unwrap();
    drop(source);

    let mut reopened = Harness::new(&[("fresh", "Fresh Virtual")], now);
    reopened.open(&moved, None, now);
    assert_eq!(reopened.app.midi_settings(), exact_map);
    assert_eq!(render_dry_bits(&reopened.app, pad(0, 0)), dry_bits);
    reopened.palette("select 1");
    reopened.palette("midi-learn");
    reopened.send(&[0x90, 61, 90]);
    reopened.midi_slice(now);
    let recovery_map = reopened.app.midi_settings();
    let expected_recovery_map = exact_map
        .learn_swap(
            BankId::new(0).unwrap(),
            0,
            sampler_core::MidiNote::new(61).unwrap(),
        )
        .unwrap();
    assert_eq!(recovery_map, expected_recovery_map);
    reopened.autosave(now);
    drop(reopened);

    let mut restored = Harness::new(&[("restore", "Restore Virtual")], now);
    restored.open(&moved, Some(RecoveryChoice::Restore), now);
    assert_eq!(restored.app.midi_settings(), expected_recovery_map);
    assert_eq!(
        restored
            .app
            .midi_settings()
            .bank(BankId::new(0).unwrap())
            .owner(sampler_core::MidiNote::new(61).unwrap()),
        Some(0)
    );
    assert_eq!(
        restored
            .app
            .midi_settings()
            .bank(BankId::new(1).unwrap())
            .owner(sampler_core::MidiNote::new(72).unwrap()),
        Some(1)
    );
    assert_eq!(render_dry_bits(&restored.app, pad(0, 0)), dry_bits);
}
