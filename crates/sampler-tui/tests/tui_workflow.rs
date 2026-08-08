use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::error::Error;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
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
    EventLoopIteration, EventLoopObserver, EventLoopTerminal, EventLoopWorker,
    KeyboardEnhancementOps, ShutdownWorker, TerminalLifecycle, run_event_loop_with,
    run_event_loop_with_observer, run_with_runtime_lifecycle,
};
use sampler_tui::{
    App, AudioPort, DirectoryEntry, DirectoryEntryKind, DirectoryScan, KeyboardCapabilities,
    LoadedSample, PAD_KEYS, PREVIEW_COLUMNS, PreviewColumn, WorkerRequest, WorkerResult,
    WorkerSendError,
};

#[derive(Debug, Clone, PartialEq)]
struct SampleIdentity {
    sample_rate: u32,
    frames: usize,
    channels: u16,
    first_frame: [f32; 2],
    signal_sum: f32,
}

impl SampleIdentity {
    fn from_buffer(buffer: &SampleBuffer) -> Self {
        Self {
            sample_rate: buffer.sample_rate(),
            frames: buffer.frames(),
            channels: 2,
            first_frame: [buffer.data()[0], buffer.data()[1]],
            signal_sum: buffer.data().iter().sum(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum AudioCall {
    Install {
        pad: PadId,
        sample: SampleIdentity,
        settings: PadSettings,
    },
    Trigger(PadId, Frame, f32),
    Release(PadId, Frame),
    StopPad(PadId),
    StopAll,
    UpdatePad(PadId),
}

#[derive(Debug, Clone)]
struct AcceptedAudioCall {
    id: usize,
    call: AudioCall,
}

#[derive(Default)]
struct AudioState {
    accepted_calls: Vec<AcceptedAudioCall>,
    outstanding_calls: VecDeque<usize>,
    completed_calls: Vec<usize>,
    typed_overflows: usize,
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
        if state.outstanding_calls.len() < self.command_capacity {
            let id = state.accepted_calls.len();
            state.accepted_calls.push(AcceptedAudioCall { id, call });
            state.outstanding_calls.push_back(id);
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
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, String> {
        self.accept_command(AudioCall::Install {
            pad,
            sample: SampleIdentity::from_buffer(&sample),
            settings,
        })?;
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
        let reclaimed = state.outstanding_calls.len();
        let completed = state.outstanding_calls.drain(..).collect::<Vec<_>>();
        state.completed_calls.extend(completed);
        reclaimed
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.state.borrow_mut().telemetry.pop_front()
    }

    fn poll_runtime_error(&mut self) -> Option<String> {
        None
    }
}

#[derive(Clone)]
struct QueuedEvent {
    event: Event,
    user_input: bool,
}

struct LoadScript {
    filename: String,
    buffer: Arc<SampleBuffer>,
}

struct HarnessWorkerState {
    script: Option<LoadScript>,
    results: VecDeque<WorkerResult>,
    requests_sent: usize,
    results_delivered: usize,
    loaded_result_waiting_for_draw: bool,
    polls_while_loaded_waiting: usize,
    loaded_result_draws: usize,
    events: Rc<RefCell<VecDeque<QueuedEvent>>>,
    post_load_events: Rc<RefCell<VecDeque<QueuedEvent>>>,
}

struct TuiHarness {
    width: u16,
    height: u16,
    app: App,
    audio: Rc<RefCell<AudioState>>,
    events: Rc<RefCell<VecDeque<QueuedEvent>>>,
    post_load_events: Rc<RefCell<VecDeque<QueuedEvent>>>,
    worker: Rc<RefCell<HarnessWorkerState>>,
    awaiting_load: bool,
    queued_input_events: usize,
    read_input_events: Rc<Cell<usize>>,
    events_per_iteration: Rc<RefCell<Vec<usize>>>,
    draw_calls: Rc<Cell<usize>>,
    rendered_overflows: Rc<Cell<usize>>,
    last_screen: Rc<RefCell<Option<String>>>,
}

impl TuiHarness {
    fn new(width: u16, height: u16, audio: FakeAudio) -> Self {
        let audio_state = audio.state();
        let mut app = App::with_audio(Box::new(audio));
        app.set_keyboard_capabilities(KeyboardCapabilities {
            release_events: true,
        });
        let events = Rc::new(RefCell::new(VecDeque::new()));
        let post_load_events = Rc::new(RefCell::new(VecDeque::new()));
        let worker = Rc::new(RefCell::new(HarnessWorkerState {
            script: None,
            results: VecDeque::new(),
            requests_sent: 0,
            results_delivered: 0,
            loaded_result_waiting_for_draw: false,
            polls_while_loaded_waiting: 0,
            loaded_result_draws: 0,
            events: Rc::clone(&events),
            post_load_events: Rc::clone(&post_load_events),
        }));
        Self {
            width,
            height,
            app,
            audio: audio_state,
            events,
            post_load_events,
            worker,
            awaiting_load: false,
            queued_input_events: 0,
            read_input_events: Rc::new(Cell::new(0)),
            events_per_iteration: Rc::new(RefCell::new(Vec::new())),
            draw_calls: Rc::new(Cell::new(0)),
            rendered_overflows: Rc::new(Cell::new(0)),
            last_screen: Rc::new(RefCell::new(None)),
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
        self.awaiting_load = true;
    }

    fn deliver_loaded(&mut self, filename: &str, buffer: Arc<SampleBuffer>) {
        assert!(self.awaiting_load, "picker must be opened before loading");
        self.worker.borrow_mut().script = Some(LoadScript {
            filename: filename.to_owned(),
            buffer,
        });
    }

    fn press(&mut self, character: char) {
        self.key(character, KeyEventKind::Press);
    }

    fn release(&mut self, character: char) {
        self.key(character, KeyEventKind::Release);
    }

    fn key(&mut self, character: char, kind: KeyEventKind) {
        let event = QueuedEvent {
            event: Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char(character),
                KeyModifiers::NONE,
                kind,
            )),
            user_input: true,
        };
        if self.awaiting_load {
            self.post_load_events.borrow_mut().push_back(event);
        } else {
            self.events.borrow_mut().push_back(event);
        }
        self.queued_input_events = self.queued_input_events.saturating_add(1);
    }

    fn telemetry(&mut self, telemetry: Telemetry) {
        self.audio.borrow_mut().telemetry.push_back(telemetry);
        self.app.tick();
    }

    fn draw(&mut self) -> String {
        self.run_until_idle(256);
        self.last_screen
            .borrow()
            .clone()
            .expect("event loop must draw the final screen")
    }

    fn run_until_idle(&mut self, max_iterations: usize) {
        self.try_run_until_idle(max_iterations)
            .expect("in-memory event loop must complete");
    }

    fn try_run_until_idle(&mut self, max_iterations: usize) -> io::Result<()> {
        let quit = QueuedEvent {
            event: Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            user_input: false,
        };
        if self.awaiting_load {
            self.post_load_events.borrow_mut().push_back(quit);
        } else {
            self.events.borrow_mut().push_back(quit);
        }
        let mut events = HarnessEvents {
            events: Rc::clone(&self.events),
            user_reads: Rc::clone(&self.read_input_events),
            worker: Rc::clone(&self.worker),
        };
        let mut terminal = HarnessTerminal {
            width: self.width,
            height: self.height,
            draw_calls: Rc::clone(&self.draw_calls),
            rendered_overflows: Rc::clone(&self.rendered_overflows),
            last_screen: Rc::clone(&self.last_screen),
            worker: Rc::clone(&self.worker),
        };
        let mut observer = HarnessObserver {
            events_per_iteration: Rc::clone(&self.events_per_iteration),
            max_iterations,
        };
        let mut worker = HarnessWorker {
            state: Rc::clone(&self.worker),
        };
        let result = run_event_loop_with_observer(
            &mut terminal,
            &mut self.app,
            &mut events,
            &mut worker,
            &mut observer,
        );
        if result.is_ok() {
            assert!(
                self.events.borrow().is_empty(),
                "event loop left unread input"
            );
            assert!(
                self.post_load_events.borrow().is_empty(),
                "event loop left post-load input"
            );
        }
        result
    }

    fn accepted_audio_calls(&self) -> usize {
        self.audio.borrow().accepted_calls.len()
    }

    fn completed_audio_calls(&self) -> usize {
        self.audio.borrow().completed_calls.len()
    }

    fn audio_calls(&self) -> Vec<AudioCall> {
        self.audio
            .borrow()
            .accepted_calls
            .iter()
            .map(|accepted| accepted.call.clone())
            .collect()
    }

    fn silent_losses(&self) -> usize {
        let state = self.audio.borrow();
        let accepted_not_completed = state
            .accepted_calls
            .iter()
            .filter(|accepted| !state.completed_calls.contains(&accepted.id))
            .count();
        let unread_input = self
            .queued_input_events
            .saturating_sub(self.read_input_events.get());
        let pads_never_accepted = (0..PAD_KEYS.len())
            .filter(|index| !self.app.is_pad_held(*index))
            .count();
        let missing_draw_progress = usize::from(self.draw_calls.get() == 0);
        accepted_not_completed + unread_input + pads_never_accepted + missing_draw_progress
    }

    fn visible_overflows(&self) -> usize {
        if self.audio.borrow().typed_overflows > 0 {
            self.rendered_overflows.get()
        } else {
            0
        }
    }

    fn maintenance_calls(&self) -> usize {
        self.audio.borrow().maintenance_calls
    }

    fn events_per_iteration(&self) -> Vec<usize> {
        self.events_per_iteration.borrow().clone()
    }

    fn max_events_per_iteration(&self) -> usize {
        self.events_per_iteration
            .borrow()
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
    }

    fn worker_delivery(&self) -> (usize, usize) {
        let worker = self.worker.borrow();
        (worker.requests_sent, worker.results_delivered)
    }

    fn worker_result_draws(&self) -> usize {
        self.worker.borrow().loaded_result_draws
    }
}

struct HarnessEvents {
    events: Rc<RefCell<VecDeque<QueuedEvent>>>,
    user_reads: Rc<Cell<usize>>,
    worker: Rc<RefCell<HarnessWorkerState>>,
}

impl sampler_tui::EventSource for HarnessEvents {
    fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
        let mut worker = self.worker.borrow_mut();
        if worker.loaded_result_waiting_for_draw {
            worker.polls_while_loaded_waiting = worker.polls_while_loaded_waiting.saturating_add(1);
            if worker.polls_while_loaded_waiting > 1 {
                return Err(io::Error::other(
                    "loaded worker result was not drawn in its iteration",
                ));
            }
        }
        drop(worker);
        Ok(!self.events.borrow().is_empty())
    }

    fn read(&mut self) -> io::Result<Event> {
        let queued = self
            .events
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "no event"))?;
        if queued.user_input {
            self.user_reads.set(self.user_reads.get().saturating_add(1));
        }
        Ok(queued.event)
    }
}

struct HarnessTerminal {
    width: u16,
    height: u16,
    draw_calls: Rc<Cell<usize>>,
    rendered_overflows: Rc<Cell<usize>>,
    last_screen: Rc<RefCell<Option<String>>>,
    worker: Rc<RefCell<HarnessWorkerState>>,
}

impl EventLoopTerminal for HarnessTerminal {
    fn draw(&mut self, app: &App) -> io::Result<()> {
        let draw_calls = self.draw_calls.get().saturating_add(1);
        self.draw_calls.set(draw_calls);
        let screen = render_screen(self.width, self.height, app);
        if screen.contains("audio command queue full") {
            self.rendered_overflows
                .set(self.rendered_overflows.get().saturating_add(1));
        }
        *self.last_screen.borrow_mut() = Some(screen);
        let mut worker = self.worker.borrow_mut();
        if worker.loaded_result_waiting_for_draw {
            worker.loaded_result_waiting_for_draw = false;
            worker.loaded_result_draws = worker.loaded_result_draws.saturating_add(1);
            let events = Rc::clone(&worker.events);
            let post_load_events = Rc::clone(&worker.post_load_events);
            drop(worker);
            let released = post_load_events.borrow_mut().drain(..).collect::<Vec<_>>();
            events.borrow_mut().extend(released);
        }
        Ok(())
    }
}

struct HarnessObserver {
    events_per_iteration: Rc<RefCell<Vec<usize>>>,
    max_iterations: usize,
}

impl EventLoopObserver for HarnessObserver {
    fn iteration_completed(&mut self, iteration: EventLoopIteration) -> io::Result<()> {
        let mut events = self.events_per_iteration.borrow_mut();
        events.push(iteration.events_applied);
        if events.len() >= self.max_iterations && !iteration.should_quit {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "event loop iteration budget exhausted",
            ));
        }
        Ok(())
    }
}

struct HarnessWorker {
    state: Rc<RefCell<HarnessWorkerState>>,
}

impl EventLoopWorker for HarnessWorker {
    fn try_recv(&mut self) -> Result<WorkerResult, TryRecvError> {
        let mut state = self.state.borrow_mut();
        let Some(result) = state.results.pop_front() else {
            return Err(TryRecvError::Empty);
        };
        state.results_delivered = state.results_delivered.saturating_add(1);
        if matches!(&result, WorkerResult::Loaded { .. }) {
            state.loaded_result_waiting_for_draw = true;
            state.polls_while_loaded_waiting = 0;
        }
        Ok(result)
    }

    fn try_send(&mut self, request: WorkerRequest) -> Result<(), WorkerSendError> {
        const RESULT_CAPACITY: usize = 8;

        let mut state = self.state.borrow_mut();
        if state.results.len() >= RESULT_CAPACITY {
            return Err(WorkerSendError::WorkerBusy);
        }
        state.requests_sent = state.requests_sent.saturating_add(1);
        let script = state
            .script
            .as_ref()
            .expect("worker request requires a configured load script");
        let filename = script.filename.clone();
        let buffer = Arc::clone(&script.buffer);
        match request {
            WorkerRequest::ScanDirectory {
                request_id, path, ..
            } => {
                let sample_path = path.join(&filename);
                state.results.push_back(WorkerResult::Scanned {
                    request_id,
                    path,
                    result: Ok(DirectoryScan::complete(vec![DirectoryEntry {
                        path: sample_path,
                        kind: DirectoryEntryKind::File,
                    }])),
                });
                state.events.borrow_mut().push_back(QueuedEvent {
                    event: Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                    user_input: false,
                });
            }
            WorkerRequest::LoadSample {
                pad,
                generation,
                path,
                engine_rate,
            } => {
                assert_eq!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(filename.as_str())
                );
                assert_eq!(engine_rate, buffer.sample_rate());
                let frames = buffer.frames();
                state.results.push_back(WorkerResult::Loaded {
                    pad,
                    generation,
                    path,
                    result: Ok(LoadedSample {
                        buffer,
                        source_rate: engine_rate,
                        source_frames: frames,
                        duration: Duration::from_secs_f64(frames as f64 / f64::from(engine_rate)),
                        preview: [PreviewColumn::default(); PREVIEW_COLUMNS],
                    }),
                });
            }
            WorkerRequest::Shutdown => panic!("event loop must not send worker shutdown"),
        }
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
        pattern_slot: None,
        pattern_generation: None,
        pattern_playing: false,
        pattern_recording: false,
        pattern_origin: None,
        pattern_playhead: 0,
        pattern_loop_count: 0,
        pattern_overflows: 0,
        live_ack_overflows: 0,
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
        let original_panic_resumed = match (self.outcome, result) {
            (Panic, Err(payload)) => {
                assert_eq!(payload.downcast_ref::<&str>().copied(), Some("draw panic"));
                calls.borrow_mut().push("resume");
                true
            }
            (Panic, Ok(_)) => panic!("panic outcome must resume its panic"),
            (_, Ok(Ok(()))) if matches!(self.outcome, Quit) => false,
            (_, Ok(Err(_))) if !matches!(self.outcome, Quit) => false,
            (_, unexpected) => panic!("unexpected lifecycle outcome: {unexpected:?}"),
        };
        LifecycleResult {
            calls: calls.borrow().clone(),
            alive: alive.get(),
            shutdown_requested: shutdown_requested.get(),
            original_panic_resumed,
        }
    }
}

struct LifecycleResult {
    calls: Vec<&'static str>,
    alive: bool,
    shutdown_requested: bool,
    original_panic_resumed: bool,
}

impl LifecycleResult {
    fn cleanup_order(&self) -> Vec<&'static str> {
        self.calls.clone()
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

    fn original_panic_resumed(&self) -> bool {
        self.original_panic_resumed
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
            AudioCall::Install {
                pad: pad(0, 0),
                sample: SampleIdentity {
                    sample_rate: 48_000,
                    frames: 256,
                    channels: 2,
                    first_frame: [0.25, 0.25],
                    signal_sum: 128.0,
                },
                settings: PadSettings::default(),
            },
            AudioCall::Trigger(pad(0, 0), 64, 1.0),
            AudioCall::Release(pad(0, 0), 64),
            AudioCall::Trigger(pad(1, 4), 64, 1.0),
        ]
    );
    assert_eq!(harness.worker_delivery(), (2, 2));
    assert_eq!(harness.worker_result_draws(), 1);
    assert_eq!(harness.events_per_iteration(), [0, 1, 0, 5]);
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
    assert_eq!(harness.max_events_per_iteration(), 64);
    assert_eq!(
        harness.events_per_iteration(),
        [vec![64; 16], vec![1]].concat()
    );
    assert_eq!(
        harness.completed_audio_calls(),
        harness.accepted_audio_calls()
    );
}

#[test]
fn max_iterations_is_an_enforced_execution_bound() {
    let mut harness = TuiHarness::with_command_capacity(8);
    for index in 0..65 {
        harness.press(PAD_KEYS[index % PAD_KEYS.len()]);
    }

    let error = harness.try_run_until_idle(1).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(harness.events_per_iteration(), [64]);
}

#[test]
fn every_exit_path_restores_before_reporting_and_joins_worker() {
    for outcome in [Quit, DrawError, ReadError, AppError, Panic] {
        let result = LifecycleHarness::new(outcome).run();
        let mut expected = vec![
            "stop-all",
            "drop-audio",
            "pop-keys",
            "request-worker",
            "restore",
            "join-worker",
        ];
        if matches!(outcome, Panic) {
            expected.extend(["report", "resume"]);
        }
        assert_eq!(result.cleanup_order(), expected);
        assert!(!result.worker_is_alive());
        assert!(result.shutdown_was_requested());
        assert_eq!(result.original_panic_resumed(), matches!(outcome, Panic));
    }
}
