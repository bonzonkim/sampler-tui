use std::error::Error;
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo, catch_unwind, resume_unwind};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::audio::{AudioPort, open_default_audio};
use crate::input::KeyboardCapabilities;
use crate::loader::{WorkerHandle, WorkerRequest, WorkerResult, WorkerSendError};
use crate::{App, ui};

pub const MAX_EVENTS_PER_ITERATION: usize = 64;
const MAX_WORKER_RESULTS_PER_ITERATION: usize = 8;
const TICK_INTERVAL: Duration = Duration::from_millis(16);

type DynError = Box<dyn Error>;
type PanicHook = dyn for<'a> Fn(&PanicHookInfo<'a>) + Send + Sync + 'static;

static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

struct UiPanicCapture {
    _hook_lock: MutexGuard<'static, ()>,
    previous: Arc<PanicHook>,
    captured: Arc<Mutex<Option<String>>>,
}

impl UiPanicCapture {
    fn install() -> Self {
        let hook_lock = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = Arc::<PanicHook>::from(panic::take_hook());
        let captured = Arc::new(Mutex::new(None));
        let ui_thread = thread::current().id();
        let previous_for_hook = Arc::clone(&previous);
        let captured_for_hook = Arc::clone(&captured);
        panic::set_hook(Box::new(move |info| {
            if thread::current().id() == ui_thread {
                let mut captured = captured_for_hook
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *captured = Some(format_panic(info));
            } else {
                previous_for_hook(info);
            }
        }));
        Self {
            _hook_lock: hook_lock,
            previous,
            captured,
        }
    }

    fn restore(self) -> Option<String> {
        let Self {
            _hook_lock,
            previous,
            captured,
        } = self;
        panic::set_hook(Box::new(move |info| previous(info)));
        let report = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(_hook_lock);
        report
    }
}

fn format_panic(info: &PanicHookInfo<'_>) -> String {
    let message = info
        .payload()
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload");
    let thread = thread::current();
    let thread_name = thread.name().unwrap_or("unnamed");
    match info.location() {
        Some(location) => format!(
            "thread '{thread_name}' panicked at {}:{}:{}:\n{message}",
            location.file(),
            location.line(),
            location.column()
        ),
        None => format!("thread '{thread_name}' panicked:\n{message}"),
    }
}

fn report_panic_to_stderr(message: &str) {
    let _ = writeln!(io::stderr().lock(), "{message}");
}

pub trait EventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

#[derive(Debug, Default)]
pub struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

pub trait KeyboardEnhancementOps: Clone {
    fn supports_keyboard_enhancement(&self) -> io::Result<bool>;
    fn push_keyboard_enhancement(&self, flags: KeyboardEnhancementFlags) -> io::Result<()>;
    fn pop_keyboard_enhancement(&self) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CrosstermKeyboardEnhancementOps;

impl KeyboardEnhancementOps for CrosstermKeyboardEnhancementOps {
    fn supports_keyboard_enhancement(&self) -> io::Result<bool> {
        crossterm::terminal::supports_keyboard_enhancement()
    }

    fn push_keyboard_enhancement(&self, flags: KeyboardEnhancementFlags) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(stdout, PushKeyboardEnhancementFlags(flags))
    }

    fn pop_keyboard_enhancement(&self) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(stdout, PopKeyboardEnhancementFlags)
    }
}

pub struct KeyboardEnhancementGuard<O: KeyboardEnhancementOps = CrosstermKeyboardEnhancementOps> {
    ops: O,
    active: bool,
}

impl<O: KeyboardEnhancementOps> KeyboardEnhancementGuard<O> {
    pub fn acquire(ops: O) -> io::Result<Self> {
        let supported = ops.supports_keyboard_enhancement()?;
        if supported {
            ops.push_keyboard_enhancement(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
            )?;
        }
        Ok(Self {
            ops,
            active: supported,
        })
    }

    pub fn release_events(&self) -> bool {
        self.active
    }

    pub fn release(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        self.ops.pop_keyboard_enhancement()
    }
}

impl<O: KeyboardEnhancementOps> Drop for KeyboardEnhancementGuard<O> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

trait EventLoopApp {
    fn maintain_audio(&mut self) -> bool;
    fn apply_worker_result(&mut self, result: WorkerResult) -> bool;
    fn take_worker_requests(&mut self) -> Vec<WorkerRequest>;
    fn apply_worker_send_error(&mut self, request: WorkerRequest, error: WorkerSendError) -> bool;
    fn take_device_retry_requests(&mut self) -> usize;
    fn retry_default_device(&mut self) -> bool;
    fn apply_terminal_event(&mut self, event: Event);
    fn tick(&mut self);
}

impl EventLoopApp for App {
    fn maintain_audio(&mut self) -> bool {
        App::maintain_audio(self)
    }

    fn apply_worker_result(&mut self, result: WorkerResult) -> bool {
        App::apply_worker_result(self, result)
    }

    fn take_worker_requests(&mut self) -> Vec<WorkerRequest> {
        App::take_worker_requests(self)
    }

    fn apply_worker_send_error(&mut self, request: WorkerRequest, error: WorkerSendError) -> bool {
        App::apply_worker_send_error(self, request, error)
    }

    fn take_device_retry_requests(&mut self) -> usize {
        App::take_device_retry_requests(self)
    }

    fn retry_default_device(&mut self) -> bool {
        App::retry_default_device(self)
    }

    fn apply_terminal_event(&mut self, event: Event) {
        App::apply_terminal_event(self, event);
    }

    fn tick(&mut self) {
        App::tick(self);
    }
}

trait EventLoopWorker {
    fn try_recv(&mut self) -> Result<WorkerResult, TryRecvError>;
    fn try_send(&mut self, request: WorkerRequest) -> Result<(), WorkerSendError>;
}

impl EventLoopWorker for WorkerHandle {
    fn try_recv(&mut self) -> Result<WorkerResult, TryRecvError> {
        WorkerHandle::try_recv(self)
    }

    fn try_send(&mut self, request: WorkerRequest) -> Result<(), WorkerSendError> {
        WorkerHandle::try_send(self, request)
    }
}

trait EventLoopDrawer<A> {
    fn draw(&mut self, app: &A) -> io::Result<()>;
}

struct TerminalDrawer<'a>(&'a mut ratatui::DefaultTerminal);

impl EventLoopDrawer<App> for TerminalDrawer<'_> {
    fn draw(&mut self, app: &App) -> io::Result<()> {
        self.0.draw(|frame| ui::render(frame, app)).map(|_| ())
    }
}

struct LoopState {
    next_tick: Instant,
    dirty: bool,
}

impl LoopState {
    fn new(next_tick: Instant) -> Self {
        Self {
            next_tick,
            dirty: true,
        }
    }
}

fn run_iteration<A, E, W, D>(
    app: &mut A,
    events: &mut E,
    worker: &mut W,
    drawer: &mut D,
    now: Instant,
    state: &mut LoopState,
) -> io::Result<()>
where
    A: EventLoopApp,
    E: EventSource,
    W: EventLoopWorker,
    D: EventLoopDrawer<A>,
{
    state.dirty |= app.maintain_audio();

    for _ in 0..MAX_WORKER_RESULTS_PER_ITERATION {
        match worker.try_recv() {
            Ok(result) => state.dirty |= app.apply_worker_result(result),
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "loader worker disconnected",
                ));
            }
        }
    }

    for event_index in 0..MAX_EVENTS_PER_ITERATION {
        let timeout = if event_index == 0 {
            state.next_tick.saturating_duration_since(now)
        } else {
            Duration::ZERO
        };
        if !events.poll(timeout)? {
            break;
        }
        app.apply_terminal_event(events.read()?);
        state.dirty = true;
    }

    if app.take_device_retry_requests() > 0 {
        state.dirty |= app.retry_default_device();
    }

    if now >= state.next_tick {
        app.tick();
        state.dirty = true;
        state.next_tick = now.checked_add(TICK_INTERVAL).unwrap_or(now);
    }

    let mut requests = app.take_worker_requests().into_iter();
    while let Some(request) = requests.next() {
        match worker.try_send(request.clone()) {
            Ok(()) => {}
            Err(WorkerSendError::WorkerBusy) => {
                state.dirty |= app.apply_worker_send_error(request, WorkerSendError::WorkerBusy);
                for request in requests {
                    state.dirty |=
                        app.apply_worker_send_error(request, WorkerSendError::WorkerBusy);
                }
                break;
            }
            Err(WorkerSendError::WorkerClosed) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "loader worker closed",
                ));
            }
        }
    }

    if state.dirty {
        drawer.draw(app)?;
        state.dirty = false;
    }
    Ok(())
}

pub fn run_event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    events: &mut impl EventSource,
    worker: &mut WorkerHandle,
) -> io::Result<()> {
    let now = Instant::now();
    let mut state = LoopState::new(now.checked_add(TICK_INTERVAL).unwrap_or(now));
    let mut drawer = TerminalDrawer(terminal);
    while !app.should_quit() {
        run_iteration(app, events, worker, &mut drawer, Instant::now(), &mut state)?;
    }
    Ok(())
}

trait TerminalLifecycle {
    type Terminal;

    fn initialize(&mut self) -> io::Result<Self::Terminal>;
    fn restore(&mut self) -> io::Result<()>;
}

#[derive(Default)]
struct RatatuiTerminalLifecycle {
    raw_mode: bool,
    alternate_screen: bool,
}

impl TerminalLifecycle for RatatuiTerminalLifecycle {
    type Terminal = ratatui::DefaultTerminal;

    fn initialize(&mut self) -> io::Result<Self::Terminal> {
        enable_raw_mode()?;
        self.raw_mode = true;

        let mut stdout = io::stdout();
        self.alternate_screen = true;
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            return preserve_primary(Err(error), self.restore());
        }

        match ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => Ok(terminal),
            Err(error) => preserve_primary(Err(error), self.restore()),
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        let raw_mode = if self.raw_mode {
            let result = disable_raw_mode();
            if result.is_ok() {
                self.raw_mode = false;
            }
            result
        } else {
            Ok(())
        };

        let alternate_screen = if self.alternate_screen {
            let mut stdout = io::stdout();
            let result = execute!(stdout, LeaveAlternateScreen);
            if result.is_ok() {
                self.alternate_screen = false;
            }
            result
        } else {
            Ok(())
        };

        preserve_primary(raw_mode, alternate_screen)
    }
}

trait ShutdownWorker {
    fn request_shutdown(&mut self);
}

impl ShutdownWorker for WorkerHandle {
    fn request_shutdown(&mut self) {
        WorkerHandle::request_shutdown(self);
    }
}

struct WorkerShutdownGuard<'a, W: ShutdownWorker>(&'a mut W);

impl<W: ShutdownWorker> WorkerShutdownGuard<'_, W> {
    fn worker(&mut self) -> &mut W {
        self.0
    }
}

impl<W: ShutdownWorker> Drop for WorkerShutdownGuard<'_, W> {
    fn drop(&mut self) {
        self.0.request_shutdown();
    }
}

fn run_with_terminal_lifecycle<L, W, F, J, R, T, E>(
    lifecycle: &mut L,
    worker: &mut W,
    run: F,
    join: J,
    report_panic: R,
) -> Result<T, E>
where
    L: TerminalLifecycle,
    W: ShutdownWorker,
    F: FnOnce(&mut L::Terminal, &mut W) -> Result<T, E>,
    J: FnOnce(&mut W) -> Result<(), E>,
    R: FnOnce(&str),
    E: From<io::Error>,
{
    let mut terminal = lifecycle.initialize().map_err(E::from)?;
    let panic_capture = UiPanicCapture::install();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut shutdown = WorkerShutdownGuard(worker);
        run(&mut terminal, shutdown.worker())
    }));
    let terminal_cleanup = lifecycle.restore().map_err(E::from);
    let cleanup = join(worker);
    let panic_report = panic_capture.restore();

    match outcome {
        Ok(primary) => preserve_primary(preserve_primary(primary, terminal_cleanup), cleanup),
        Err(payload) => {
            drop(terminal_cleanup);
            drop(cleanup);
            report_panic(
                panic_report
                    .as_deref()
                    .unwrap_or("thread panicked with no captured diagnostic"),
            );
            resume_unwind(payload)
        }
    }
}

#[cfg(test)]
fn run_with_enhancements<O, F, T, E>(ops: O, run: F) -> Result<T, E>
where
    O: KeyboardEnhancementOps,
    F: FnOnce(bool) -> Result<T, E>,
    E: From<io::Error>,
{
    let mut guard = KeyboardEnhancementGuard::acquire(ops).map_err(E::from)?;
    let primary = run(guard.release_events());
    let cleanup = guard.release().map_err(E::from);
    preserve_primary(primary, cleanup)
}

trait AudioShutdown {
    type Error;

    fn shutdown_audio(&mut self) -> Result<(), Self::Error>;
}

impl AudioShutdown for App {
    type Error = DynError;

    fn shutdown_audio(&mut self) -> Result<(), Self::Error> {
        App::shutdown_audio(self).map_err(|error| Box::new(io::Error::other(error)) as DynError)
    }
}

struct AudioShutdownGuard<'a, A: AudioShutdown> {
    app: &'a mut A,
    active: bool,
}

impl<'a, A: AudioShutdown> AudioShutdownGuard<'a, A> {
    fn new(app: &'a mut A) -> Self {
        Self { app, active: true }
    }

    fn app(&mut self) -> &mut A {
        self.app
    }

    fn shutdown(&mut self) -> Result<(), A::Error> {
        self.active = false;
        self.app.shutdown_audio()
    }
}

impl<A: AudioShutdown> Drop for AudioShutdownGuard<'_, A> {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            let _ = self.app.shutdown_audio();
        }
    }
}

fn run_with_audio_and_enhancements<A, O, F, T, E>(app: &mut A, ops: O, run: F) -> Result<T, E>
where
    A: AudioShutdown<Error = E>,
    O: KeyboardEnhancementOps,
    F: FnOnce(&mut A, bool) -> Result<T, E>,
    E: From<io::Error>,
{
    let mut keyboard = match KeyboardEnhancementGuard::acquire(ops) {
        Ok(keyboard) => keyboard,
        Err(error) => {
            let primary = Err(E::from(error));
            return preserve_primary(primary, app.shutdown_audio());
        }
    };
    let release_events = keyboard.release_events();
    let mut audio = AudioShutdownGuard::new(app);
    let primary = run(audio.app(), release_events);
    let cleanup = audio.shutdown();
    let primary = preserve_primary(primary, cleanup);
    let cleanup = keyboard.release().map_err(E::from);
    preserve_primary(primary, cleanup)
}

fn preserve_primary<T, E>(primary: Result<T, E>, cleanup: Result<(), E>) -> Result<T, E> {
    match primary {
        Err(error) => Err(error),
        Ok(value) => cleanup.map(|()| value),
    }
}

fn app_with_default_audio(open_audio: impl FnOnce() -> Result<Box<dyn AudioPort>, String>) -> App {
    match open_audio() {
        Ok(audio) => App::with_audio(audio),
        Err(error) => App::without_audio(error),
    }
}

pub fn run_tui() -> Result<(), DynError> {
    let mut app = app_with_default_audio(open_default_audio);
    let mut events = CrosstermEventSource;
    let mut worker = WorkerHandle::spawn();
    let mut lifecycle = RatatuiTerminalLifecycle::default();

    run_with_terminal_lifecycle(
        &mut lifecycle,
        &mut worker,
        |terminal, worker| {
            run_with_audio_and_enhancements(
                &mut app,
                CrosstermKeyboardEnhancementOps,
                |app, release_events| {
                    app.set_keyboard_capabilities(KeyboardCapabilities { release_events });
                    run_event_loop(terminal, app, &mut events, worker)
                        .map_err(|error| Box::new(error) as DynError)
                },
            )
        },
        |worker| worker.join().map_err(|error| Box::new(error) as DynError),
        report_panic_to_stderr,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    };

    use super::*;

    #[derive(Default)]
    struct FakeEvents {
        events: VecDeque<Event>,
        poll_timeouts: Vec<Duration>,
    }

    impl FakeEvents {
        fn with_events(events: impl IntoIterator<Item = Event>) -> Self {
            Self {
                events: events.into_iter().collect(),
                poll_timeouts: Vec::new(),
            }
        }
    }

    impl EventSource for FakeEvents {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            self.poll_timeouts.push(timeout);
            Ok(!self.events.is_empty())
        }

        fn read(&mut self) -> io::Result<Event> {
            self.events
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "no event"))
        }
    }

    #[derive(Default)]
    struct FakeApp {
        maintenance_calls: usize,
        worker_results: usize,
        events_applied: usize,
        ticks: usize,
        device_retry_requests: usize,
        device_retry_attempts: usize,
        worker_requests: Vec<WorkerRequest>,
        worker_send_errors: Vec<(WorkerRequest, WorkerSendError)>,
    }

    impl EventLoopApp for FakeApp {
        fn maintain_audio(&mut self) -> bool {
            self.maintenance_calls += 1;
            false
        }

        fn apply_worker_result(&mut self, _result: WorkerResult) -> bool {
            self.worker_results += 1;
            true
        }

        fn take_worker_requests(&mut self) -> Vec<WorkerRequest> {
            std::mem::take(&mut self.worker_requests)
        }

        fn apply_worker_send_error(
            &mut self,
            request: WorkerRequest,
            error: WorkerSendError,
        ) -> bool {
            self.worker_send_errors.push((request, error));
            true
        }

        fn apply_terminal_event(&mut self, _event: Event) {
            self.events_applied += 1;
        }

        fn tick(&mut self) {
            self.ticks += 1;
        }

        fn take_device_retry_requests(&mut self) -> usize {
            std::mem::take(&mut self.device_retry_requests)
        }

        fn retry_default_device(&mut self) -> bool {
            self.device_retry_attempts += 1;
            true
        }
    }

    #[derive(Default)]
    struct FakeWorker {
        results: VecDeque<WorkerResult>,
        send_error: Option<WorkerSendError>,
        sent: usize,
    }

    impl EventLoopWorker for FakeWorker {
        fn try_recv(&mut self) -> Result<WorkerResult, std::sync::mpsc::TryRecvError> {
            self.results
                .pop_front()
                .ok_or(std::sync::mpsc::TryRecvError::Empty)
        }

        fn try_send(&mut self, _request: WorkerRequest) -> Result<(), WorkerSendError> {
            if let Some(error) = self.send_error {
                return Err(error);
            }
            self.sent += 1;
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeDrawer {
        draws: usize,
    }

    impl EventLoopDrawer<FakeApp> for FakeDrawer {
        fn draw(&mut self, _app: &FakeApp) -> io::Result<()> {
            self.draws += 1;
            Ok(())
        }
    }

    fn pad_press(character: char) -> Event {
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char(character),
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))
    }

    fn iteration(
        app: &mut FakeApp,
        events: &mut FakeEvents,
        worker: &mut FakeWorker,
        drawer: &mut FakeDrawer,
        now: Instant,
        state: &mut LoopState,
    ) -> io::Result<()> {
        run_iteration(app, events, worker, drawer, now, state)
    }

    #[test]
    fn event_burst_is_bounded_and_maintenance_still_runs() {
        let now = Instant::now();
        let mut events = FakeEvents::with_events((0..500).map(|_| pad_press('1')));
        let mut app = FakeApp::default();
        let mut worker = FakeWorker::default();
        let mut drawer = FakeDrawer::default();
        let mut state = LoopState::new(now);

        iteration(
            &mut app,
            &mut events,
            &mut worker,
            &mut drawer,
            now,
            &mut state,
        )
        .unwrap();

        assert_eq!(app.events_applied, MAX_EVENTS_PER_ITERATION);
        assert_eq!(app.maintenance_calls, 1);
        assert_eq!(events.events.len(), 500 - MAX_EVENTS_PER_ITERATION);
    }

    #[test]
    fn worker_result_burst_is_bounded() {
        let now = Instant::now();
        let mut app = FakeApp::default();
        let mut events = FakeEvents::default();
        let mut worker = FakeWorker {
            results: (0..20).map(|_| failed_scan()).collect(),
            ..FakeWorker::default()
        };
        let mut drawer = FakeDrawer::default();
        let mut state = LoopState::new(now);

        iteration(
            &mut app,
            &mut events,
            &mut worker,
            &mut drawer,
            now,
            &mut state,
        )
        .unwrap();

        assert_eq!(app.worker_results, MAX_WORKER_RESULTS_PER_ITERATION);
        assert_eq!(worker.results.len(), 12);
    }

    #[test]
    fn a_past_tick_deadline_uses_a_zero_poll_timeout_and_ticks_once() {
        let now = Instant::now();
        let mut app = FakeApp::default();
        let mut events = FakeEvents::default();
        let mut worker = FakeWorker::default();
        let mut drawer = FakeDrawer::default();
        let mut state = LoopState::new(now - TICK_INTERVAL);

        iteration(
            &mut app,
            &mut events,
            &mut worker,
            &mut drawer,
            now,
            &mut state,
        )
        .unwrap();

        assert_eq!(events.poll_timeouts.first(), Some(&Duration::ZERO));
        assert_eq!(app.ticks, 1);
    }

    #[test]
    fn a_clean_iteration_does_not_draw() {
        let now = Instant::now();
        let mut app = FakeApp::default();
        let mut events = FakeEvents::default();
        let mut worker = FakeWorker::default();
        let mut drawer = FakeDrawer::default();
        let mut state = LoopState::new(now + TICK_INTERVAL);
        state.dirty = false;

        iteration(
            &mut app,
            &mut events,
            &mut worker,
            &mut drawer,
            now,
            &mut state,
        )
        .unwrap();

        assert_eq!(drawer.draws, 0);
    }

    #[test]
    fn retry_request_burst_opens_the_default_device_once() {
        let mut app = FakeApp {
            device_retry_requests: 3,
            ..FakeApp::default()
        };
        let mut events = FakeEvents::default();
        let mut worker = FakeWorker::default();
        let mut drawer = FakeDrawer::default();
        let now = Instant::now();
        let mut state = LoopState::new(now + Duration::from_secs(1));

        run_iteration(
            &mut app,
            &mut events,
            &mut worker,
            &mut drawer,
            now,
            &mut state,
        )
        .unwrap();

        assert_eq!(app.device_retry_attempts, 1);
        assert_eq!(app.device_retry_requests, 0);
        assert_eq!(drawer.draws, 1);
    }

    #[test]
    fn full_worker_is_visible_without_retaining_an_unbounded_terminal_queue() {
        let mut app = FakeApp {
            worker_requests: vec![WorkerRequest::Shutdown; 8],
            ..FakeApp::default()
        };
        let mut events = FakeEvents::default();
        let mut worker = FakeWorker {
            send_error: Some(WorkerSendError::WorkerBusy),
            ..FakeWorker::default()
        };
        let mut drawer = FakeDrawer::default();
        let now = Instant::now();
        let mut state = LoopState::new(now + Duration::from_secs(1));

        run_iteration(
            &mut app,
            &mut events,
            &mut worker,
            &mut drawer,
            now,
            &mut state,
        )
        .unwrap();

        assert_eq!(app.worker_send_errors.len(), 8);
        assert!(
            app.worker_send_errors
                .iter()
                .all(|(_, error)| *error == WorkerSendError::WorkerBusy)
        );
        assert_eq!(drawer.draws, 1);

        worker.send_error = None;
        run_iteration(
            &mut app,
            &mut events,
            &mut worker,
            &mut drawer,
            now,
            &mut state,
        )
        .unwrap();
        assert_eq!(worker.sent, 0);
    }

    #[test]
    fn startup_audio_failure_preserves_the_browsable_app_model() {
        let mut app = app_with_default_audio(|| Err("no output device".to_owned()));

        assert_eq!(app.audio_format(), None);
        assert_eq!(
            app.overlay(),
            Some(&crate::Overlay::DeviceError("no output device".to_owned()))
        );
        assert_eq!(app.pads().len(), crate::PAD_VIEW_COUNT);

        app.close_overlay();
        app.open_help();
        assert_eq!(app.overlay(), Some(&crate::Overlay::Help));
    }

    #[derive(Clone)]
    struct FakeEnhancementOps {
        supported: bool,
        fail_push: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
        flags: Arc<Mutex<Vec<KeyboardEnhancementFlags>>>,
    }

    impl FakeEnhancementOps {
        fn supported() -> Self {
            Self {
                supported: true,
                fail_push: false,
                calls: Arc::new(Mutex::new(Vec::new())),
                flags: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn unsupported() -> Self {
            Self {
                supported: false,
                ..Self::supported()
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl KeyboardEnhancementOps for FakeEnhancementOps {
        fn supports_keyboard_enhancement(&self) -> io::Result<bool> {
            self.calls.lock().unwrap().push("query");
            Ok(self.supported)
        }

        fn push_keyboard_enhancement(&self, flags: KeyboardEnhancementFlags) -> io::Result<()> {
            self.calls.lock().unwrap().push("push");
            self.flags.lock().unwrap().push(flags);
            if self.fail_push {
                Err(io::Error::other("push failed"))
            } else {
                Ok(())
            }
        }

        fn pop_keyboard_enhancement(&self) -> io::Result<()> {
            self.calls.lock().unwrap().push("pop");
            Ok(())
        }
    }

    #[test]
    fn keyboard_enhancements_are_popped_after_success_error_and_panic() {
        let success = FakeEnhancementOps::supported();
        run_with_enhancements(success.clone(), |_| io::Result::Ok(())).unwrap();
        assert_eq!(success.calls(), ["query", "push", "pop"]);

        let error = FakeEnhancementOps::supported();
        assert!(
            run_with_enhancements(error.clone(), |_| {
                io::Result::<()>::Err(io::Error::other("loop"))
            })
            .is_err()
        );
        assert_eq!(error.calls(), ["query", "push", "pop"]);

        let panic = FakeEnhancementOps::supported();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _ = run_with_enhancements(panic.clone(), |_| -> io::Result<()> {
                panic!("loop panic")
            });
        }));
        assert!(outcome.is_err());
        assert_eq!(panic.calls(), ["query", "push", "pop"]);
    }

    #[test]
    fn unsupported_terminals_do_not_enable_release_events_or_push_and_pop() {
        let ops = FakeEnhancementOps::unsupported();
        let release_events = run_with_enhancements(ops.clone(), io::Result::<bool>::Ok).unwrap();

        assert!(!release_events);
        assert_eq!(ops.calls(), ["query"]);
    }

    #[test]
    fn a_failed_push_is_not_popped_and_successful_cleanup_only_pops_once() {
        let mut failing = FakeEnhancementOps::supported();
        failing.fail_push = true;
        assert!(run_with_enhancements(failing.clone(), io::Result::<bool>::Ok).is_err());
        assert_eq!(failing.calls(), ["query", "push"]);

        let successful = FakeEnhancementOps::supported();
        {
            let mut guard = KeyboardEnhancementGuard::acquire(successful.clone()).unwrap();
            guard.release().unwrap();
            guard.release().unwrap();
        }
        assert_eq!(successful.calls(), ["query", "push", "pop"]);
    }

    #[test]
    fn enhancement_push_uses_disambiguation_and_event_type_reporting() {
        let ops = FakeEnhancementOps::supported();
        run_with_enhancements(ops.clone(), |_| io::Result::Ok(())).unwrap();

        assert_eq!(
            *ops.flags.lock().unwrap(),
            [KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES]
        );
    }

    #[test]
    fn a_primary_error_wins_when_cleanup_also_fails() {
        let result = preserve_primary::<(), _>(Err("loop"), Err("cleanup"));
        assert_eq!(result, Err("loop"));
        assert_eq!(preserve_primary(Ok(7), Err("cleanup")), Err("cleanup"));
    }

    struct FakeTerminalLifecycle {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl TerminalLifecycle for FakeTerminalLifecycle {
        type Terminal = ();

        fn initialize(&mut self) -> io::Result<Self::Terminal> {
            self.calls.lock().unwrap().push("init");
            Ok(())
        }

        fn restore(&mut self) -> io::Result<()> {
            self.calls.lock().unwrap().push("restore");
            Ok(())
        }
    }

    struct FakeCleanupWorker {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ShutdownWorker for FakeCleanupWorker {
        fn request_shutdown(&mut self) {
            self.calls.lock().unwrap().push("request");
        }
    }

    struct FakeShutdownApp {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl AudioShutdown for FakeShutdownApp {
        type Error = io::Error;

        fn shutdown_audio(&mut self) -> Result<(), Self::Error> {
            self.calls.lock().unwrap().push("stop-all");
            self.calls.lock().unwrap().push("drop-audio");
            Ok(())
        }
    }

    #[derive(Clone)]
    struct OrderedEnhancementOps {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl KeyboardEnhancementOps for OrderedEnhancementOps {
        fn supports_keyboard_enhancement(&self) -> io::Result<bool> {
            self.calls.lock().unwrap().push("query");
            Ok(true)
        }

        fn push_keyboard_enhancement(&self, _flags: KeyboardEnhancementFlags) -> io::Result<()> {
            self.calls.lock().unwrap().push("push");
            Ok(())
        }

        fn pop_keyboard_enhancement(&self) -> io::Result<()> {
            self.calls.lock().unwrap().push("pop");
            Ok(())
        }
    }

    #[test]
    fn audio_stops_and_drops_before_keyboard_and_terminal_restoration() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut lifecycle = FakeTerminalLifecycle {
            calls: Arc::clone(&calls),
        };
        let mut worker = FakeCleanupWorker {
            calls: Arc::clone(&calls),
        };
        let mut app = FakeShutdownApp {
            calls: Arc::clone(&calls),
        };
        let enhancements = OrderedEnhancementOps {
            calls: Arc::clone(&calls),
        };

        let result: io::Result<()> = run_with_terminal_lifecycle(
            &mut lifecycle,
            &mut worker,
            |_, _| {
                run_with_audio_and_enhancements(&mut app, enhancements, |_, _| {
                    calls.lock().unwrap().push("loop");
                    Err(io::Error::other("loop"))
                })
            },
            |_| {
                calls.lock().unwrap().push("join");
                Err(io::Error::other("join"))
            },
            |_| {},
        );

        assert_eq!(result.unwrap_err().to_string(), "loop");
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "init",
                "query",
                "push",
                "loop",
                "stop-all",
                "drop-audio",
                "pop",
                "request",
                "restore",
                "join",
            ]
        );
    }

    #[test]
    fn panic_still_stops_and_drops_audio_before_popping_keyboard_enhancements() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut app = FakeShutdownApp {
            calls: Arc::clone(&calls),
        };
        let enhancements = OrderedEnhancementOps {
            calls: Arc::clone(&calls),
        };

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _: io::Result<()> =
                run_with_audio_and_enhancements(&mut app, enhancements, |_, _| {
                    calls.lock().unwrap().push("loop");
                    panic!("loop panic")
                });
        }));

        assert!(outcome.is_err());
        assert_eq!(
            *calls.lock().unwrap(),
            ["query", "push", "loop", "stop-all", "drop-audio", "pop"]
        );
    }

    #[test]
    fn production_lifecycle_contains_panic_through_terminal_restore_and_worker_join() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut lifecycle = FakeTerminalLifecycle {
            calls: Arc::clone(&calls),
        };
        let mut worker = FakeCleanupWorker {
            calls: Arc::clone(&calls),
        };
        let mut app = FakeShutdownApp {
            calls: Arc::clone(&calls),
        };
        let enhancements = OrderedEnhancementOps {
            calls: Arc::clone(&calls),
        };

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let _: io::Result<()> = run_with_terminal_lifecycle(
                &mut lifecycle,
                &mut worker,
                |_, _| {
                    run_with_audio_and_enhancements(&mut app, enhancements, |_, _| {
                        calls.lock().unwrap().push("loop");
                        panic!("loop panic")
                    })
                },
                |_| {
                    calls.lock().unwrap().push("join");
                    Err(io::Error::other("join"))
                },
                |message| {
                    assert!(message.contains("loop panic"));
                    calls.lock().unwrap().push("report");
                },
            );
        }));

        let panic = outcome.unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"loop panic"));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "init",
                "query",
                "push",
                "loop",
                "stop-all",
                "drop-audio",
                "pop",
                "request",
                "restore",
                "join",
                "report",
            ]
        );
    }

    #[test]
    fn worker_is_requested_before_terminal_restore_and_joined_afterward() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut lifecycle = FakeTerminalLifecycle {
            calls: Arc::clone(&calls),
        };
        let mut worker = FakeCleanupWorker {
            calls: Arc::clone(&calls),
        };

        let result: io::Result<()> = run_with_terminal_lifecycle(
            &mut lifecycle,
            &mut worker,
            |_, _| {
                calls.lock().unwrap().push("loop");
                Err(io::Error::other("loop"))
            },
            |_| {
                calls.lock().unwrap().push("join");
                Err(io::Error::other("join"))
            },
            |_| {},
        );

        assert_eq!(result.unwrap_err().to_string(), "loop");
        assert_eq!(
            *calls.lock().unwrap(),
            ["init", "loop", "request", "restore", "join"]
        );
    }

    fn failed_scan() -> WorkerResult {
        WorkerResult::Scanned {
            request_id: 1,
            path: "missing".into(),
            result: Err("missing".to_owned()),
        }
    }
}
