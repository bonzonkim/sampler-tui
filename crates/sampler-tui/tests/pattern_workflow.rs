//! Full stack pattern workflow evidence.  The port below is deliberately only a thin
//! forwarding adapter: commands and acknowledgements are produced by the real controller and
//! consumed by a real `AudioEngine`; device I/O is the sole substituted boundary.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use sampler_audio::{
    AudioController, AudioEngine, Frame, LiveAck, LiveCommandId, PatternSnapshotSlot,
    PatternSwitch, SampleBuffer, SampleSlot, Telemetry, audio_channels,
};
use sampler_core::{
    BankId, EditablePattern, EventId, Meter, PadId, PadSettings, PatternEvent, PatternSlotId,
    PatternSnapshot, Resolution, Tempo, Transport,
};
use sampler_tui::{
    App, AudioPort, KeyboardCapabilities, LoadedSample, PREVIEW_COLUMNS, PreviewColumn,
    WorkerRequest, WorkerResult,
};

struct ControllerPort {
    sample_rate: u32,
    controller: Rc<RefCell<AudioController>>,
    runtime_failure: Rc<Cell<bool>>,
}

impl ControllerPort {
    fn controller(&self) -> std::cell::RefMut<'_, AudioController> {
        self.controller.borrow_mut()
    }
}

impl AudioPort for ControllerPort {
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
            .map_err(|e| e.to_string())
    }
    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, String> {
        self.controller()
            .install_recovery(pad, sample, settings)
            .map_err(|e| e.to_string())
    }
    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
        self.controller()
            .trigger(pad, at, velocity)
            .map_err(|e| e.to_string())
    }
    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
        self.controller()
            .release(pad, at)
            .map_err(|e| e.to_string())
    }
    fn trigger_live_tracked(&mut self, pad: PadId, velocity: f32) -> Result<LiveCommandId, String> {
        self.controller()
            .trigger_live_tracked(pad, velocity)
            .map_err(|e| e.to_string())
    }
    fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, String> {
        self.controller()
            .release_live_tracked(pad)
            .map_err(|e| e.to_string())
    }
    fn install_pattern(
        &mut self,
        snapshot: Arc<PatternSnapshot>,
    ) -> Result<PatternSnapshotSlot, String> {
        self.controller()
            .install_pattern(snapshot)
            .map_err(|e| e.to_string())
    }
    fn select_pattern(&mut self, slot: PatternSlotId, switch: PatternSwitch) -> Result<(), String> {
        self.controller()
            .select_pattern(slot, switch)
            .map_err(|e| e.to_string())
    }
    fn play_pattern(&mut self) -> Result<(), String> {
        self.controller().play_pattern().map_err(|e| e.to_string())
    }
    fn stop_pattern(&mut self) -> Result<(), String> {
        self.controller().stop_pattern().map_err(|e| e.to_string())
    }
    fn set_record_capture(&mut self, capture: Option<(PatternSlotId, u64)>) -> Result<(), String> {
        self.controller()
            .set_record_capture(capture)
            .map_err(|e| e.to_string())
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
        self.controller().stop_pad(pad).map_err(|e| e.to_string())
    }
    fn stop_all(&mut self) -> Result<(), String> {
        self.controller().stop_all().map_err(|e| e.to_string())
    }
    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        self.controller()
            .update_pad(pad, settings)
            .map_err(|e| e.to_string())
    }
    fn reclaim_retired(&mut self) -> usize {
        self.controller().reclaim_retired()
    }
    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.controller().latest_telemetry()
    }
    fn poll_runtime_error(&mut self) -> Option<String> {
        self.runtime_failure
            .replace(false)
            .then_some("test device disconnected".to_owned())
    }
}

struct PatternHarness {
    app: App,
    engine: AudioEngine,
    controller: Rc<RefCell<AudioController>>,
    runtime_failure: Rc<Cell<bool>>,
}

impl PatternHarness {
    fn new(sample_rate: u32) -> Self {
        let (controller, ports) = audio_channels();
        let controller = Rc::new(RefCell::new(controller));
        let runtime_failure = Rc::new(Cell::new(false));
        let engine = AudioEngine::new(sample_rate, ports).expect("valid test engine");
        let mut app = App::with_audio(Box::new(ControllerPort {
            sample_rate,
            controller: Rc::clone(&controller),
            runtime_failure: Rc::clone(&runtime_failure),
        }));
        app.set_keyboard_capabilities(KeyboardCapabilities {
            release_events: true,
        });
        let mut harness = Self {
            app,
            engine,
            controller,
            runtime_failure,
        };
        harness.drain_initial_patterns();
        harness.load_pad(0);
        harness.callback(1);
        harness
    }

    fn drain_initial_patterns(&mut self) {
        // Each maintenance slice may compile or submit exactly one slot, so admission of all
        // sixteen immutable snapshots takes two bounded passes.
        for _ in 0..32 {
            self.app.maintain_audio();
            self.callback(0);
        }
        self.app.maintain_audio();
        self.callback(0);
    }

    fn load_pad(&mut self, index: u8) {
        let pad = pad(index);
        let request = self
            .app
            .begin_load(pad, format!("pad-{index}.wav"))
            .expect("audio available");
        let WorkerRequest::LoadSample {
            generation, path, ..
        } = request
        else {
            panic!("load request");
        };
        let buffer = Arc::new(SampleBuffer::new(48_000, vec![0.25; 1024]).expect("sample"));
        assert!(self.app.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation,
            path,
            result: Ok(LoadedSample {
                buffer,
                source_rate: 48_000,
                source_frames: 512,
                duration: std::time::Duration::from_millis(11),
                preview: [PreviewColumn { min: -1, max: 1 }; PREVIEW_COLUMNS],
            }),
        }));
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app.apply_key(KeyEvent::new_with_kind(
            code,
            modifiers,
            KeyEventKind::Press,
        ));
    }
    fn release(&mut self, code: KeyCode) {
        self.app.apply_key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
    }
    fn callback(&mut self, frames: usize) {
        self.engine.render_frames(frames, |_| {});
    }
    fn ui_iteration(&mut self) {
        self.app.tick();
        self.app.maintain_audio();
    }
    fn palette(&mut self, command: &str) {
        self.key(KeyCode::Char(':'), KeyModifiers::SHIFT);
        self.app
            .apply_terminal_event(crossterm::event::Event::Paste(command.to_owned()));
        self.key(KeyCode::Enter, KeyModifiers::NONE);
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
}

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

#[test]
fn records_overdubs_edits_and_switches_at_the_loop_boundary() {
    let mut harness = PatternHarness::new(48_000);
    harness.key(KeyCode::Char('r'), KeyModifiers::CONTROL);
    harness.callback(1); // transport/capture admission
    harness.key(KeyCode::Char('1'), KeyModifiers::NONE);
    harness.callback(65); // tracked trigger executes at the callback's +64 frame
    harness.release(KeyCode::Char('1'));
    harness.callback(65); // tracked release executes at the callback's +64 frame
    harness.ui_iteration(); // real bounded ack drain, never workspace.apply_ack directly

    let events = harness
        .app
        .patterns()
        .pattern(PatternSlotId::new(0).unwrap())
        .events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].duration, Some(65));
    assert!(
        events[0].frame
            < harness
                .app
                .patterns()
                .selected_pattern()
                .transport()
                .loop_frames()
    );

    harness.palette("quantize 100");
    harness.palette("swing 60");
    harness.key(KeyCode::Tab, KeyModifiers::NONE);
    harness.key(KeyCode::Char('-'), KeyModifiers::NONE);
    harness.ui_iteration();
    harness.callback(0);
    harness.ui_iteration();
    harness.key(KeyCode::Char('.'), KeyModifiers::NONE);
    harness.callback(
        harness
            .app
            .patterns()
            .selected_pattern()
            .transport()
            .loop_frames() as usize
            + 1_601,
    );
    harness.ui_iteration();
    let screen = harness.screen();
    assert!(screen.contains("PATTERN 02"), "{screen}");
    assert!(screen.contains("120.0 BPM"), "{screen}");
    assert_eq!(
        harness.app.telemetry().pattern_slot,
        Some(PatternSlotId::new(1).unwrap()),
        "status={} selected={:?} ready={} telemetry={:?}",
        harness.app.status(),
        harness.app.patterns().selected_slot(),
        harness
            .app
            .patterns()
            .is_slot_ready(PatternSlotId::new(1).unwrap()),
        harness.app.telemetry(),
    );
}

#[test]
fn device_rate_retry_rebuilds_all_slots_round_robin_and_keeps_edits() {
    let mut harness = PatternHarness::new(48_000);
    harness.key(KeyCode::Tab, KeyModifiers::NONE);
    harness.key(KeyCode::Right, KeyModifiers::NONE);
    harness.key(KeyCode::Enter, KeyModifiers::NONE);
    let before = harness.app.patterns().selected_pattern().events()[0].frame;
    harness.runtime_failure.set(true);
    harness.ui_iteration();
    assert!(harness.app.audio_format().is_none());
    let (controller, ports) = audio_channels();
    let retry_controller = Rc::new(RefCell::new(controller));
    let retry_failure = Rc::new(Cell::new(false));
    let retry_engine = AudioEngine::new(44_100, ports).unwrap();
    harness.app.retry_with(Box::new(ControllerPort {
        sample_rate: 44_100,
        controller: Rc::clone(&retry_controller),
        runtime_failure: Rc::clone(&retry_failure),
    }));
    harness.engine = retry_engine;
    harness.controller = retry_controller;
    harness.runtime_failure = retry_failure;
    for _ in 0..16 {
        harness.ui_iteration();
        harness.callback(0);
    }
    assert_eq!(harness.app.patterns().sample_rates(), [44_100; 16]);
    assert_eq!(
        harness.app.patterns().selected_pattern().events()[0].frame,
        (before * 44_100 + 24_000) / 48_000
    );
}

#[test]
fn dense_pattern_and_ack_overflow_are_visible_without_silent_loss() {
    let (controller, ports) = audio_channels();
    let controller = Rc::new(RefCell::new(controller));
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let app_port = ControllerPort {
        sample_rate: 48_000,
        controller: Rc::clone(&controller),
        runtime_failure: Rc::new(Cell::new(false)),
    };
    let mut app = App::with_audio(Box::new(app_port));
    app.set_keyboard_capabilities(KeyboardCapabilities {
        release_events: true,
    });
    let transport = Transport::new(
        48_000,
        Tempo::new(300.0).unwrap(),
        Meter::new(1, 8).unwrap(),
        1,
        Resolution::Sixteenth,
    )
    .unwrap();
    let mut editable =
        EditablePattern::new(PatternSlotId::new(0).unwrap(), "dense", transport).unwrap();
    for index in 0..1024_u64 {
        editable
            .insert(PatternEvent::new(EventId(index + 1), pad(0), 0, 1.0, None).unwrap())
            .unwrap();
    }
    let snapshot = Arc::new(editable.compile().unwrap());
    {
        let mut control = controller.borrow_mut();
        control
            .install(
                pad(0),
                Arc::new(SampleBuffer::new(48_000, vec![0.25; 1024]).unwrap()),
                PadSettings::default(),
            )
            .unwrap();
        control.install_pattern(snapshot).unwrap();
        control
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        control.play_pattern().unwrap();
        control
            .set_record_capture(Some((PatternSlotId::new(0).unwrap(), 1024)))
            .unwrap();
    }
    engine.render_frames(0, |_| {});
    // Repeated bounded batches fill the callback-owned acknowledgement lane while the UI
    // intentionally does not drain it; no synthetic ack is ever inserted.
    for _ in 0..5 {
        for _ in 0..64 {
            controller
                .borrow_mut()
                .trigger_live_tracked(pad(0), 1.0)
                .unwrap();
        }
        engine.render_frames(65, |_| {});
    }
    engine.render_frames(3_200, |_| {});
    app.tick();
    app.apply_key(KeyEvent::new_with_kind(
        KeyCode::Tab,
        KeyModifiers::NONE,
        KeyEventKind::Press,
    ));
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| sampler_tui::ui::render(frame, &app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let screen = (0..24)
        .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        app.telemetry().pattern_overflows > 0,
        "{:?}",
        app.telemetry()
    );
    assert!(
        app.telemetry().live_ack_overflows > 0,
        "{:?}",
        app.telemetry()
    );
    assert!(screen.contains("pattern overflow"), "{screen}");
    assert!(screen.contains("record ack overflow"), "{screen}");
}
