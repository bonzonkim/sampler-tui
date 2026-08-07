use std::collections::VecDeque;
use std::error::Error;
use std::io;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;

use crate::input::KeyboardCapabilities;
use crate::loader::{WorkerHandle, WorkerRequest, WorkerResult, WorkerSendError};
use crate::{App, ui};

pub const MAX_EVENTS_PER_ITERATION: usize = 64;
const MAX_WORKER_RESULTS_PER_ITERATION: usize = 8;
const TICK_INTERVAL: Duration = Duration::from_millis(16);

type DynError = Box<dyn Error>;

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
    pending_requests: VecDeque<WorkerRequest>,
}

impl LoopState {
    fn new(next_tick: Instant) -> Self {
        Self {
            next_tick,
            dirty: true,
            pending_requests: VecDeque::new(),
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

    if now >= state.next_tick {
        app.tick();
        state.dirty = true;
        state.next_tick = now.checked_add(TICK_INTERVAL).unwrap_or(now);
    }

    state.pending_requests.extend(app.take_worker_requests());
    while let Some(request) = state.pending_requests.front().cloned() {
        match worker.try_send(request) {
            Ok(()) => {
                state.pending_requests.pop_front();
            }
            Err(WorkerSendError::WorkerBusy) => break,
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

    fn run<F, R>(&mut self, run: F) -> R
    where
        F: FnOnce(&mut Self::Terminal) -> R;
}

struct RatatuiTerminalLifecycle;

impl TerminalLifecycle for RatatuiTerminalLifecycle {
    type Terminal = ratatui::DefaultTerminal;

    fn run<F, R>(&mut self, run: F) -> R
    where
        F: FnOnce(&mut Self::Terminal) -> R,
    {
        ratatui::run(run)
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

fn run_with_terminal_lifecycle<L, W, F, J, T, E>(
    lifecycle: &mut L,
    worker: &mut W,
    run: F,
    join: J,
) -> Result<T, E>
where
    L: TerminalLifecycle,
    W: ShutdownWorker,
    F: FnOnce(&mut L::Terminal, &mut W) -> Result<T, E>,
    J: FnOnce(&mut W) -> Result<(), E>,
{
    let primary = lifecycle.run(|terminal| {
        let mut shutdown = WorkerShutdownGuard(worker);
        run(terminal, shutdown.worker())
    });
    let cleanup = join(worker);
    preserve_primary(primary, cleanup)
}

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

fn preserve_primary<T, E>(primary: Result<T, E>, cleanup: Result<(), E>) -> Result<T, E> {
    match primary {
        Err(error) => Err(error),
        Ok(value) => cleanup.map(|()| value),
    }
}

pub fn run_tui() -> Result<(), DynError> {
    let mut app = App::without_audio("audio device is not initialized");
    let mut events = CrosstermEventSource;
    let mut worker = WorkerHandle::spawn();
    let mut lifecycle = RatatuiTerminalLifecycle;

    run_with_terminal_lifecycle(
        &mut lifecycle,
        &mut worker,
        |terminal, worker| {
            run_with_enhancements(CrosstermKeyboardEnhancementOps, |release_events| {
                app.set_keyboard_capabilities(KeyboardCapabilities { release_events });
                run_event_loop(terminal, &mut app, &mut events, worker)
                    .map_err(|error| Box::new(error) as DynError)
            })
        },
        |worker| worker.join().map_err(|error| Box::new(error) as DynError),
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
            Vec::new()
        }

        fn apply_terminal_event(&mut self, _event: Event) {
            self.events_applied += 1;
        }

        fn tick(&mut self) {
            self.ticks += 1;
        }
    }

    #[derive(Default)]
    struct FakeWorker {
        results: VecDeque<WorkerResult>,
    }

    impl EventLoopWorker for FakeWorker {
        fn try_recv(&mut self) -> Result<WorkerResult, std::sync::mpsc::TryRecvError> {
            self.results
                .pop_front()
                .ok_or(std::sync::mpsc::TryRecvError::Empty)
        }

        fn try_send(&mut self, _request: WorkerRequest) -> Result<(), WorkerSendError> {
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

        fn run<F, R>(&mut self, run: F) -> R
        where
            F: FnOnce(&mut Self::Terminal) -> R,
        {
            self.calls.lock().unwrap().push("init");
            let result = run(&mut ());
            self.calls.lock().unwrap().push("restore");
            result
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

    #[test]
    fn worker_is_requested_before_terminal_restore_and_joined_afterward() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut lifecycle = FakeTerminalLifecycle {
            calls: Arc::clone(&calls),
        };
        let mut worker = FakeCleanupWorker {
            calls: Arc::clone(&calls),
        };

        let result: Result<(), &str> = run_with_terminal_lifecycle(
            &mut lifecycle,
            &mut worker,
            |_, _| {
                calls.lock().unwrap().push("loop");
                Err("loop")
            },
            |_| {
                calls.lock().unwrap().push("join");
                Err("join")
            },
        );

        assert_eq!(result, Err("loop"));
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
