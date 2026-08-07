use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::error::Error;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use sampler_audio::{Frame, SampleBuffer, SampleSlot, Telemetry};
use sampler_core::{BankId, PadId, PadSettings};
use sampler_tui::terminal::{
    EventLoopTerminal, EventLoopWorker, KeyboardEnhancementOps, ShutdownWorker, TerminalLifecycle,
    run_event_loop_with, run_with_runtime_lifecycle,
};
use sampler_tui::{
    App, AudioPort, DirectoryEntry, DirectoryEntryKind, KeyboardCapabilities, LoadedSample,
    MAX_EVENTS_PER_ITERATION, PAD_KEYS, PREVIEW_COLUMNS, PreviewColumn, WorkerRequest,
    WorkerResult, WorkerSendError,
};

#[derive(Debug, Clone, PartialEq)]
enum AudioCall {
    Install(PadId),
    Trigger(PadId, Frame, f32),
    Release(PadId, Frame),
    StopPad(PadId),
    StopAll,
    UpdatePad(PadId),
}

#[derive(Default)]
struct AudioState {
    attempted_calls: usize,
    accepted_calls: Vec<AudioCall>,
    typed_overflows: usize,
    outstanding_calls: usize,
    maintenance_calls: usize,
    telemetry: VecDeque<Telemetry>,
}

struct FakeAudio {
    sample_rate: u32,
    channels: u16,
    command_capacity: usize,
    state: Rc<RefCell<AudioState>>,
    cleanup: Option<Rc<RefCell<Vec<&'static str>>>>,
}

impl FakeAudio {
    fn ready(sample_rate: u32, channels: u16) -> Self {
        Self::with_capacity(sample_rate, channels, usize::MAX)
    }

    fn with_capacity(sample_rate: u32, channels: u16, command_capacity: usize) -> Self {
        Self {
            sample_rate,
            channels,
            command_capacity,
            state: Rc::new(RefCell::new(AudioState::default())),
            cleanup: None,
        }
    }

    fn for_cleanup(cleanup: Rc<RefCell<Vec<&'static str>>>) -> Self {
        let mut audio = Self::ready(48_000, 2);
        audio.cleanup = Some(cleanup);
        audio
    }

    fn state(&self) -> Rc<RefCell<AudioState>> {
        Rc::clone(&self.state)
    }

    fn accept_command(&self, call: AudioCall) -> Result<(), String> {
        let mut state = self.state.borrow_mut();
        state.attempted_calls = state.attempted_calls.saturating_add(1);
        if state.outstanding_calls < self.command_capacity {
            state.outstanding_calls = state.outstanding_calls.saturating_add(1);
            state.accepted_calls.push(call);
            Ok(())
        } else {
            state.typed_overflows = state.typed_overflows.saturating_add(1);
            Err("audio command queue full".to_owned())
        }
    }
}

impl Drop for FakeAudio {
    fn drop(&mut self) {
        if let Some(cleanup) = &self.cleanup {
            cleanup.borrow_mut().push("drop-audio");
        }
    }
}

impl AudioPort for FakeAudio {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn render_horizon(&self) -> Frame {
        0
    }

    fn install(
        &mut self,
        pad: PadId,
        _sample: Arc<SampleBuffer>,
        _settings: PadSettings,
    ) -> Result<SampleSlot, String> {
        self.accept_command(AudioCall::Install(pad))?;
        SampleSlot::new(0).map_err(|error| error.to_string())
    }

    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
        self.accept_command(AudioCall::Trigger(pad, at, velocity))
    }

    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
        self.accept_command(AudioCall::Release(pad, at))
    }

    fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
        self.accept_command(AudioCall::StopPad(pad))
    }

    fn stop_all(&mut self) -> Result<(), String> {
        if let Some(cleanup) = &self.cleanup {
            cleanup.borrow_mut().push("stop-all");
            Ok(())
        } else {
            self.accept_command(AudioCall::StopAll)
        }
    }

    fn update_pad(&mut self, pad: PadId, _settings: PadSettings) -> Result<(), String> {
        self.accept_command(AudioCall::UpdatePad(pad))
    }

    fn reclaim_retired(&mut self) -> usize {
        let mut state = self.state.borrow_mut();
        state.maintenance_calls = state.maintenance_calls.saturating_add(1);
        let reclaimed = state.outstanding_calls;
        state.outstanding_calls = 0;
        reclaimed
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.state.borrow_mut().telemetry.pop_front()
    }

    fn poll_runtime_error(&mut self) -> Option<String> {
        None
    }
}

struct TuiHarness {
    width: u16,
    height: u16,
    app: App,
    audio: Rc<RefCell<AudioState>>,
    pending_scan: Option<(u64, PathBuf)>,
    events: VecDeque<Event>,
    queued_input_events: usize,
    read_input_events: usize,
    draw_calls: usize,
    rendered_overflows: usize,
}

impl TuiHarness {
    fn new(width: u16, height: u16, audio: FakeAudio) -> Self {
        let audio_state = audio.state();
        let mut app = App::with_audio(Box::new(audio));
        app.set_keyboard_capabilities(KeyboardCapabilities {
            release_events: true,
        });
        Self {
            width,
            height,
            app,
            audio: audio_state,
            pending_scan: None,
            events: VecDeque::new(),
            queued_input_events: 0,
            read_input_events: 0,
            draw_calls: 0,
            rendered_overflows: 0,
        }
    }

    fn with_command_capacity(command_capacity: usize) -> Self {
        Self::new(
            80,
            24,
            FakeAudio::with_capacity(48_000, 2, command_capacity),
        )
    }

    fn open_picker_for_selected(&mut self) {
        self.app.open_picker();
        let requests = self.app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id, path, ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected one directory scan request")
        };
        self.pending_scan = Some((*request_id, path.clone()));
    }

    fn deliver_loaded(&mut self, filename: &str, buffer: Arc<SampleBuffer>) {
        let (request_id, directory) = self
            .pending_scan
            .take()
            .expect("picker scan must be opened first");
        let path = directory.join(filename);
        assert!(self.app.apply_worker_result(WorkerResult::Scanned {
            request_id,
            path: directory,
            result: Ok(vec![DirectoryEntry {
                path: path.clone(),
                kind: DirectoryEntryKind::File,
            }]),
        }));
        self.app
            .apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let requests = self.app.take_worker_requests();
        let [
            WorkerRequest::LoadSample {
                pad,
                generation,
                path: requested_path,
                engine_rate,
            },
        ] = requests.as_slice()
        else {
            panic!("expected one sample load request")
        };
        assert_eq!(*engine_rate, buffer.sample_rate());
        assert_eq!(requested_path, &path);
        let frames = buffer.frames();
        assert!(self.app.apply_worker_result(WorkerResult::Loaded {
            pad: *pad,
            generation: *generation,
            path,
            result: Ok(LoadedSample {
                buffer,
                source_rate: *engine_rate,
                source_frames: frames,
                duration: Duration::from_secs_f64(frames as f64 / f64::from(*engine_rate)),
                preview: [PreviewColumn::default(); PREVIEW_COLUMNS],
            }),
        }));
    }

    fn press(&mut self, character: char) {
        self.key(character, KeyEventKind::Press);
    }

    fn release(&mut self, character: char) {
        self.key(character, KeyEventKind::Release);
    }

    fn key(&mut self, character: char, kind: KeyEventKind) {
        self.events.push_back(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char(character),
            KeyModifiers::NONE,
            kind,
        )));
        self.queued_input_events = self.queued_input_events.saturating_add(1);
    }

    fn telemetry(&mut self, telemetry: Telemetry) {
        self.audio.borrow_mut().telemetry.push_back(telemetry);
        self.app.tick();
    }

    fn draw(&mut self) -> String {
        self.drive_events(256);
        render_screen(self.width, self.height, &self.app)
    }

    fn run_until_idle(&mut self, max_iterations: usize) {
        self.drive_events(max_iterations);
    }

    fn drive_events(&mut self, max_iterations: usize) {
        let input_event_count = self.events.len();
        let required_iterations = input_event_count.div_ceil(MAX_EVENTS_PER_ITERATION) + 1;
        assert!(
            required_iterations <= max_iterations.max(1),
            "event burst exceeded harness iteration bound"
        );
        self.events.push_back(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )));
        let read_calls = Rc::new(Cell::new(0));
        let draw_calls = Rc::new(Cell::new(0));
        let rendered_overflows = Rc::new(Cell::new(0));
        let mut events = HarnessEvents {
            events: std::mem::take(&mut self.events),
            read_calls: Rc::clone(&read_calls),
        };
        let mut terminal = HarnessTerminal {
            width: self.width,
            height: self.height,
            draw_calls: Rc::clone(&draw_calls),
            rendered_overflows: Rc::clone(&rendered_overflows),
        };
        let mut worker = IdleWorker;
        run_event_loop_with(&mut terminal, &mut self.app, &mut events, &mut worker)
            .expect("in-memory event loop must complete");
        assert!(events.events.is_empty(), "event loop left unread input");
        self.read_input_events = self
            .read_input_events
            .saturating_add(read_calls.get().saturating_sub(1));
        self.draw_calls = self.draw_calls.saturating_add(draw_calls.get());
        self.rendered_overflows = self
            .rendered_overflows
            .saturating_add(rendered_overflows.get());
    }

    fn accepted_audio_calls(&self) -> usize {
        self.audio.borrow().accepted_calls.len()
    }

    fn audio_calls(&self) -> Vec<AudioCall> {
        self.audio.borrow().accepted_calls.clone()
    }

    fn silent_losses(&self) -> usize {
        let state = self.audio.borrow();
        let unclassified_audio_calls = state
            .attempted_calls
            .saturating_sub(state.accepted_calls.len() + state.typed_overflows);
        let unread_input = self
            .queued_input_events
            .saturating_sub(self.read_input_events);
        let pads_never_accepted = (0..PAD_KEYS.len())
            .filter(|index| !self.app.is_pad_held(*index))
            .count();
        let missing_draw_progress = usize::from(self.draw_calls == 0);
        unclassified_audio_calls + unread_input + pads_never_accepted + missing_draw_progress
    }

    fn visible_overflows(&self) -> usize {
        if self.audio.borrow().typed_overflows > 0 {
            self.rendered_overflows
        } else {
            0
        }
    }

    fn maintenance_calls(&self) -> usize {
        self.audio.borrow().maintenance_calls
    }
}

struct HarnessEvents {
    events: VecDeque<Event>,
    read_calls: Rc<Cell<usize>>,
}

impl sampler_tui::EventSource for HarnessEvents {
    fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
        Ok(!self.events.is_empty())
    }

    fn read(&mut self) -> io::Result<Event> {
        let event = self
            .events
            .pop_front()
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "no event"))?;
        self.read_calls.set(self.read_calls.get().saturating_add(1));
        Ok(event)
    }
}

struct HarnessTerminal {
    width: u16,
    height: u16,
    draw_calls: Rc<Cell<usize>>,
    rendered_overflows: Rc<Cell<usize>>,
}

impl EventLoopTerminal for HarnessTerminal {
    fn draw(&mut self, app: &App) -> io::Result<()> {
        self.draw_calls.set(self.draw_calls.get().saturating_add(1));
        let screen = render_screen(self.width, self.height, app);
        if screen.contains("audio command queue full") {
            self.rendered_overflows
                .set(self.rendered_overflows.get().saturating_add(1));
        }
        Ok(())
    }
}

struct IdleWorker;

impl EventLoopWorker for IdleWorker {
    fn try_recv(&mut self) -> Result<WorkerResult, TryRecvError> {
        Err(TryRecvError::Empty)
    }

    fn try_send(&mut self, _request: WorkerRequest) -> Result<(), WorkerSendError> {
        Ok(())
    }
}

fn render_screen(width: u16, height: u16, app: &App) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal must initialize");
    terminal
        .draw(|frame| sampler_tui::ui::render(frame, app))
        .expect("test terminal draw must succeed");
    let buffer = terminal.backend().buffer();
    let mut screen = String::new();
    for y in 0..height {
        for x in 0..width {
            screen.push_str(buffer[(x, y)].symbol());
        }
        screen.push('\n');
    }
    screen
}

fn pad(bank: u8, index: u8) -> PadId {
    PadId::new(BankId::new(bank).expect("test bank must be valid"), index)
        .expect("test pad must be valid")
}

fn constant_sample(sample_rate: u32, frames: usize) -> Arc<SampleBuffer> {
    Arc::new(
        SampleBuffer::new(sample_rate, vec![0.25; frames.saturating_mul(2)])
            .expect("constant stereo sample must be valid"),
    )
}

fn telemetry_with_peaks(
    rendered_frame: Frame,
    peak_left: f32,
    peak_right: f32,
    active_voices: usize,
) -> Telemetry {
    Telemetry {
        active_pads: [0; 3],
        rendered_frame,
        last_triggered_frame: None,
        peak_left,
        peak_right,
        active_voices,
        late_commands: 0,
        invalid_commands: 0,
        command_overflows: 0,
    }
}

#[derive(Debug, Clone, Copy)]
enum ExitOutcome {
    Quit,
    DrawError,
    ReadError,
    AppError,
    Panic,
}

use ExitOutcome::{AppError, DrawError, Panic, Quit, ReadError};

#[derive(Clone)]
struct FakeKeyboardOps {
    calls: Rc<RefCell<Vec<&'static str>>>,
}

impl KeyboardEnhancementOps for FakeKeyboardOps {
    fn supports_keyboard_enhancement(&self) -> io::Result<bool> {
        Ok(true)
    }

    fn push_keyboard_enhancement(&self, _flags: KeyboardEnhancementFlags) -> io::Result<()> {
        Ok(())
    }

    fn pop_keyboard_enhancement(&self) -> io::Result<()> {
        self.calls.borrow_mut().push("pop-keys");
        Ok(())
    }
}

struct FakeTerminal {
    outcome: ExitOutcome,
}

impl EventLoopTerminal for FakeTerminal {
    fn draw(&mut self, _app: &App) -> io::Result<()> {
        match self.outcome {
            DrawError => Err(io::Error::other("draw failed")),
            Panic => panic!("draw panic"),
            Quit | ReadError | AppError => Ok(()),
        }
    }
}

struct FakeTerminalLifecycle {
    outcome: ExitOutcome,
    calls: Rc<RefCell<Vec<&'static str>>>,
}

impl TerminalLifecycle for FakeTerminalLifecycle {
    type Terminal = FakeTerminal;

    fn initialize(&mut self) -> io::Result<Self::Terminal> {
        Ok(FakeTerminal {
            outcome: self.outcome,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        self.calls.borrow_mut().push("restore");
        Ok(())
    }
}

struct FakeEvents {
    outcome: ExitOutcome,
    events: VecDeque<Event>,
}

impl FakeEvents {
    fn new(outcome: ExitOutcome) -> Self {
        let events = if matches!(outcome, Quit) {
            VecDeque::from([Event::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL,
            ))])
        } else {
            VecDeque::new()
        };
        Self { outcome, events }
    }
}

impl sampler_tui::EventSource for FakeEvents {
    fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
        Ok(matches!(self.outcome, ReadError) || !self.events.is_empty())
    }

    fn read(&mut self) -> io::Result<Event> {
        if matches!(self.outcome, ReadError) {
            return Err(io::Error::other("read failed"));
        }
        self.events
            .pop_front()
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "no event"))
    }
}

struct FakeWorker {
    alive: Rc<Cell<bool>>,
    shutdown_requested: Rc<Cell<bool>>,
    calls: Rc<RefCell<Vec<&'static str>>>,
}

impl EventLoopWorker for FakeWorker {
    fn try_recv(&mut self) -> Result<WorkerResult, TryRecvError> {
        Err(TryRecvError::Empty)
    }

    fn try_send(&mut self, _request: WorkerRequest) -> Result<(), WorkerSendError> {
        Ok(())
    }
}

impl ShutdownWorker for FakeWorker {
    fn request_shutdown(&mut self) {
        self.shutdown_requested.set(true);
        self.calls.borrow_mut().push("request-worker");
    }
}

struct LifecycleHarness {
    outcome: ExitOutcome,
}

impl LifecycleHarness {
    fn new(outcome: ExitOutcome) -> Self {
        Self { outcome }
    }

    fn run(self) -> LifecycleResult {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let alive = Rc::new(Cell::new(true));
        let shutdown_requested = Rc::new(Cell::new(false));
        let mut lifecycle = FakeTerminalLifecycle {
            outcome: self.outcome,
            calls: Rc::clone(&calls),
        };
        let mut worker = FakeWorker {
            alive: Rc::clone(&alive),
            shutdown_requested: Rc::clone(&shutdown_requested),
            calls: Rc::clone(&calls),
        };
        let mut events = FakeEvents::new(self.outcome);
        let mut app = App::with_audio(Box::new(FakeAudio::for_cleanup(Rc::clone(&calls))));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let outcome: Result<(), Box<dyn Error>> = run_with_runtime_lifecycle(
                &mut app,
                FakeKeyboardOps {
                    calls: Rc::clone(&calls),
                },
                &mut lifecycle,
                &mut worker,
                |terminal, app, release_events, worker| {
                    app.set_keyboard_capabilities(KeyboardCapabilities { release_events });
                    if matches!(self.outcome, AppError) {
                        return Err(
                            Box::new(io::Error::other("application failed")) as Box<dyn Error>
                        );
                    }
                    run_event_loop_with(terminal, app, &mut events, worker)
                        .map_err(|error| Box::new(error) as Box<dyn Error>)
                },
                |worker| {
                    calls.borrow_mut().push("join-worker");
                    worker.alive.set(false);
                    Ok(())
                },
                |_| calls.borrow_mut().push("report"),
            );
            outcome
        }));
        match (self.outcome, result) {
            (Panic, Err(_)) => {}
            (Panic, Ok(_)) => panic!("panic outcome must resume its panic"),
            (_, Ok(Ok(()))) if matches!(self.outcome, Quit) => {}
            (_, Ok(Err(_))) if !matches!(self.outcome, Quit) => {}
            (_, unexpected) => panic!("unexpected lifecycle outcome: {unexpected:?}"),
        }
        LifecycleResult {
            calls: calls.borrow().clone(),
            alive: alive.get(),
            shutdown_requested: shutdown_requested.get(),
            outcome: self.outcome,
        }
    }
}

struct LifecycleResult {
    calls: Vec<&'static str>,
    alive: bool,
    shutdown_requested: bool,
    outcome: ExitOutcome,
}

impl LifecycleResult {
    fn cleanup_order(&self) -> Vec<&'static str> {
        self.calls
            .iter()
            .copied()
            .filter(|call| !matches!(*call, "request-worker" | "report"))
            .collect()
    }

    fn worker_is_alive(&self) -> bool {
        self.alive
    }

    fn shutdown_was_requested(&self) -> bool {
        self.shutdown_requested
            && self
                .calls
                .iter()
                .filter(|call| **call == "request-worker")
                .count()
                == 1
    }

    fn shutdown_precedes_restore(&self) -> bool {
        let shutdown = self.calls.iter().position(|call| *call == "request-worker");
        let restore = self.calls.iter().position(|call| *call == "restore");
        matches!((shutdown, restore), (Some(shutdown), Some(restore)) if shutdown < restore)
    }

    fn reported_after_cleanup(&self) -> bool {
        let reports = self
            .calls
            .iter()
            .enumerate()
            .filter(|(_, call)| **call == "report")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if matches!(self.outcome, Panic) {
            reports.len() == 1
                && self
                    .calls
                    .iter()
                    .position(|call| *call == "join-worker")
                    .is_some_and(|join| reports[0] > join)
        } else {
            reports.is_empty()
        }
    }
}

#[test]
fn loads_plays_releases_switches_banks_and_renders_status() {
    let mut harness = TuiHarness::new(80, 24, FakeAudio::ready(48_000, 2));
    harness.open_picker_for_selected();
    harness.deliver_loaded("kick.wav", constant_sample(48_000, 256));
    harness.press('1');
    harness.release('1');
    harness.press(']');
    harness.press('q');
    harness.telemetry(telemetry_with_peaks(512, 0.8, 0.4, 2));
    let screen = harness.draw();

    assert!(screen.contains("BANK B"));
    assert!(screen.contains("KICK.WAV"));
    assert!(screen.contains("Voices 02"));
    assert_eq!(harness.accepted_audio_calls(), 4);
    assert_eq!(
        harness.audio_calls(),
        [
            AudioCall::Install(pad(0, 0)),
            AudioCall::Trigger(pad(0, 0), 64, 1.0),
            AudioCall::Release(pad(0, 0), 64),
            AudioCall::Trigger(pad(1, 4), 64, 1.0),
        ]
    );
}

#[test]
fn rapid_sixteen_pad_input_is_bounded_and_loss_is_typed() {
    let mut harness = TuiHarness::with_command_capacity(8);
    for _ in 0..64 {
        for key in PAD_KEYS {
            harness.press(key);
        }
    }
    harness.run_until_idle(256);
    assert_eq!(harness.silent_losses(), 0);
    assert!(harness.visible_overflows() > 0);
    assert!(harness.maintenance_calls() >= 1);
}

#[test]
fn every_exit_path_restores_before_reporting_and_joins_worker() {
    for outcome in [Quit, DrawError, ReadError, AppError, Panic] {
        let result = LifecycleHarness::new(outcome).run();
        assert_eq!(
            result.cleanup_order(),
            [
                "stop-all",
                "drop-audio",
                "pop-keys",
                "restore",
                "join-worker"
            ]
        );
        assert!(!result.worker_is_alive());
        assert!(result.shutdown_was_requested());
        assert!(result.shutdown_precedes_restore());
        assert!(result.reported_after_cleanup());
    }
}
