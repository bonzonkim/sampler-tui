use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering};

use rtrb::{Consumer, Producer, PushError, RingBuffer};
use sampler_core::PadId;

use crate::CaptureError;

pub const MAX_CAPTURE_FRAMES: usize = 8_388_608;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    Resample,
    Input,
}

#[derive(Debug)]
pub struct CaptureBuffer {
    token: u64,
    target: PadId,
    source: CaptureSource,
    sample_rate: u32,
    max_frames: usize,
    stereo: Vec<f32>,
}

impl CaptureBuffer {
    pub fn try_new(
        token: u64,
        target: PadId,
        source: CaptureSource,
        sample_rate: u32,
        max_frames: usize,
    ) -> Result<Self, CaptureError> {
        if sample_rate == 0 {
            return Err(CaptureError::ZeroSampleRate);
        }
        if max_frames == 0 {
            return Err(CaptureError::ZeroFrameLimit);
        }
        if max_frames > MAX_CAPTURE_FRAMES {
            return Err(CaptureError::FrameLimitTooLarge { max_frames });
        }

        let mut stereo = Vec::new();
        stereo
            .try_reserve_exact(max_frames * 2)
            .map_err(|_| CaptureError::AllocationFailed)?;
        Ok(Self {
            token,
            target,
            source,
            sample_rate,
            max_frames,
            stereo,
        })
    }

    pub const fn token(&self) -> u64 {
        self.token
    }

    pub const fn target(&self) -> PadId {
        self.target
    }

    pub const fn source(&self) -> CaptureSource {
        self.source
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub const fn max_frames(&self) -> usize {
        self.max_frames
    }

    pub fn stereo(&self) -> &[f32] {
        &self.stereo
    }
}

#[derive(Debug)]
pub enum CaptureCommand {
    Arm(CaptureBuffer),
    Start { token: u64 },
    Stop { token: u64 },
    Cancel { token: u64 },
}

#[derive(Debug)]
pub struct CaptureSendFailure {
    error: CaptureError,
    command: CaptureCommand,
}

impl CaptureSendFailure {
    pub(crate) fn new(error: CaptureError, command: CaptureCommand) -> Self {
        Self { error, command }
    }

    pub const fn error(&self) -> CaptureError {
        self.error
    }

    pub fn into_command(self) -> CaptureCommand {
        self.command
    }
}

#[derive(Debug)]
pub struct CaptureCompletion {
    pub token: u64,
    pub target: PadId,
    pub source: CaptureSource,
    pub sample_rate: u32,
    pub stereo: Vec<f32>,
    pub hard_limit: bool,
    pub peak: f32,
}

#[derive(Debug)]
pub enum CaptureOutcome {
    Completed(CaptureCompletion),
    Cancelled(CaptureBuffer),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureState {
    Idle,
    Armed,
    Recording,
    CompletionPending,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureProgressSnapshot {
    pub token: u64,
    pub frames: usize,
    pub peak: f32,
    pub hard_limit: bool,
}

const PROGRESS_SNAPSHOT_ATTEMPTS: usize = 3;

impl CaptureState {
    const IDLE: u8 = 0;
    const ARMED: u8 = 1;
    const RECORDING: u8 = 2;
    const COMPLETION_PENDING: u8 = 3;

    fn from_raw(value: u8) -> Self {
        match value {
            Self::ARMED => Self::Armed,
            Self::RECORDING => Self::Recording,
            Self::COMPLETION_PENDING => Self::CompletionPending,
            _ => Self::Idle,
        }
    }

    const fn raw(self) -> u8 {
        match self {
            Self::Idle => Self::IDLE,
            Self::Armed => Self::ARMED,
            Self::Recording => Self::RECORDING,
            Self::CompletionPending => Self::COMPLETION_PENDING,
        }
    }
}

struct CaptureShared {
    state: AtomicU8,
    failed: AtomicBool,
    progress_sequence: AtomicUsize,
    progress_published: AtomicBool,
    token_low: AtomicU32,
    token_high: AtomicU32,
    frames: AtomicUsize,
    peak_bits: AtomicU32,
    hard_limit: AtomicBool,
}

impl CaptureShared {
    fn state(&self) -> CaptureState {
        CaptureState::from_raw(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, state: CaptureState) {
        self.state.store(state.raw(), Ordering::Release);
    }

    fn reserve_arm(&self) -> Result<(), CaptureState> {
        self.state
            .compare_exchange(
                CaptureState::IDLE,
                CaptureState::ARMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(CaptureState::from_raw)
    }

    fn mark_failed(&self) {
        self.failed.store(true, Ordering::Release);
    }

    fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    fn release_arm_reservation(&self) {
        let _ = self.state.compare_exchange(
            CaptureState::ARMED,
            CaptureState::IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn reset_progress(&self, token: u64) {
        self.publish_progress(token, 0, 0.0, false);
    }

    fn publish_progress(&self, token: u64, frames: usize, peak: f32, hard_limit: bool) {
        self.progress_sequence.fetch_add(1, Ordering::AcqRel);
        self.token_low.store(token as u32, Ordering::Relaxed);
        self.token_high
            .store((token >> u32::BITS) as u32, Ordering::Relaxed);
        self.frames.store(frames, Ordering::Relaxed);
        self.peak_bits.store(peak.to_bits(), Ordering::Relaxed);
        self.hard_limit.store(hard_limit, Ordering::Relaxed);
        self.progress_published.store(true, Ordering::Relaxed);
        self.progress_sequence.fetch_add(1, Ordering::Release);
    }

    fn progress(&self) -> Option<CaptureProgressSnapshot> {
        for _ in 0..PROGRESS_SNAPSHOT_ATTEMPTS {
            let before = self.progress_sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                continue;
            }
            let published = self.progress_published.load(Ordering::Relaxed);
            let token_low = self.token_low.load(Ordering::Relaxed);
            let token_high = self.token_high.load(Ordering::Relaxed);
            let frames = self.frames.load(Ordering::Relaxed);
            let peak = f32::from_bits(self.peak_bits.load(Ordering::Relaxed));
            let hard_limit = self.hard_limit.load(Ordering::Relaxed);
            let after = self.progress_sequence.load(Ordering::Acquire);
            if before == after {
                return published.then_some(CaptureProgressSnapshot {
                    token: u64::from(token_low) | (u64::from(token_high) << u32::BITS),
                    frames,
                    peak,
                    hard_limit,
                });
            }
        }
        None
    }
}

pub struct CaptureController {
    commands: Producer<CaptureCommand>,
    outcomes: Consumer<CaptureOutcome>,
    shared: Arc<CaptureShared>,
}

#[derive(Clone)]
pub(crate) struct CaptureFailureHandle {
    shared: Arc<CaptureShared>,
}

impl CaptureFailureHandle {
    pub(crate) fn mark_failed(&self) {
        self.shared.mark_failed();
    }
}

impl CaptureController {
    pub fn arm(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureSendFailure> {
        let command = CaptureCommand::Arm(buffer);
        if self.shared.is_failed() {
            return Err(CaptureSendFailure::new(
                CaptureError::CommandClosed,
                command,
            ));
        }
        match self.shared.reserve_arm() {
            Ok(()) => match self.send(command) {
                Ok(()) => Ok(()),
                Err(failure) => {
                    self.shared.release_arm_reservation();
                    Err(failure)
                }
            },
            Err(_) if self.shared.is_failed() => Err(CaptureSendFailure::new(
                CaptureError::CommandClosed,
                command,
            )),
            Err(CaptureState::CompletionPending) => Err(CaptureSendFailure::new(
                CaptureError::CompletionPending,
                command,
            )),
            Err(CaptureState::Idle | CaptureState::Armed | CaptureState::Recording) => {
                Err(CaptureSendFailure::new(CaptureError::InvalidState, command))
            }
        }
    }

    pub fn start(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.send(CaptureCommand::Start { token })
    }

    pub fn stop(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.send(CaptureCommand::Stop { token })
    }

    pub fn cancel(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.send(CaptureCommand::Cancel { token })
    }

    pub fn try_next_outcome(&mut self) -> Option<CaptureOutcome> {
        self.outcomes.pop().ok()
    }

    pub fn state(&self) -> CaptureState {
        self.shared.state()
    }

    pub fn progress(&self) -> Option<CaptureProgressSnapshot> {
        self.shared.progress()
    }

    pub(crate) fn failure_handle(&self) -> CaptureFailureHandle {
        CaptureFailureHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    fn send(&mut self, command: CaptureCommand) -> Result<(), CaptureSendFailure> {
        if self.shared.is_failed() || self.commands.is_abandoned() {
            return Err(CaptureSendFailure::new(
                CaptureError::CommandClosed,
                command,
            ));
        }
        self.commands.push(command).map_err(|error| match error {
            PushError::Full(command) => CaptureSendFailure::new(CaptureError::CommandFull, command),
        })
    }
}

pub struct CaptureCore {
    commands: Consumer<CaptureCommand>,
    outcomes: Producer<CaptureOutcome>,
    shared: Arc<CaptureShared>,
    state: CaptureState,
    active: Option<CaptureBuffer>,
    peak: f32,
    pending: Option<CaptureOutcome>,
    last_error: Option<CaptureError>,
}

impl CaptureCore {
    pub fn poll_commands(&mut self) {
        if self.shared.is_failed() {
            return;
        }
        if self.pending.is_some() {
            self.flush_pending();
            return;
        }

        let Ok(command) = self.commands.pop() else {
            return;
        };
        self.handle_command(command);
    }

    pub fn push_frame(&mut self, frame: [f32; 2]) {
        if self.shared.is_failed() || self.state != CaptureState::Recording {
            return;
        }
        let Some(buffer) = self.active.as_mut() else {
            return;
        };
        if buffer.stereo.len() + 2 > buffer.stereo.capacity() {
            return;
        }

        let left = finite_or_zero(frame[0]);
        let right = finite_or_zero(frame[1]);
        buffer.stereo.push(left);
        buffer.stereo.push(right);
        self.peak = self.peak.max(left.abs()).max(right.abs());
        let frames = buffer.stereo.len() / 2;
        let hard_limit = frames == buffer.max_frames;
        self.shared
            .publish_progress(buffer.token, frames, self.peak, hard_limit);

        if hard_limit {
            self.finish_completed(true);
        }
    }

    pub const fn state(&self) -> CaptureState {
        self.state
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.shared.is_failed()
    }

    pub fn take_error(&mut self) -> Option<CaptureError> {
        self.last_error.take()
    }

    fn handle_command(&mut self, command: CaptureCommand) {
        match command {
            CaptureCommand::Arm(buffer) => self.arm(buffer),
            CaptureCommand::Start { token } => self.start(token),
            CaptureCommand::Stop { token } => self.stop(token),
            CaptureCommand::Cancel { token } => self.cancel(token),
        }
    }

    fn arm(&mut self, buffer: CaptureBuffer) {
        if self.state != CaptureState::Idle {
            self.record_error(CaptureError::InvalidState);
            self.publish(CaptureOutcome::Cancelled(buffer));
            return;
        }
        self.active = Some(buffer);
        self.peak = 0.0;
        self.shared.reset_progress(
            self.active
                .as_ref()
                .expect("armed capture remains active")
                .token,
        );
        self.set_state(CaptureState::Armed);
    }

    fn start(&mut self, token: u64) {
        if !self.matches_token(token) {
            return;
        }
        if self.state != CaptureState::Armed {
            self.record_error(CaptureError::InvalidState);
            return;
        }
        self.set_state(CaptureState::Recording);
    }

    fn stop(&mut self, token: u64) {
        if !self.matches_token(token) {
            return;
        }
        if self.state != CaptureState::Recording {
            self.record_error(CaptureError::InvalidState);
            return;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|buffer| buffer.stereo.is_empty())
        {
            self.record_error(CaptureError::EmptyCapture);
            self.finish_cancelled();
            return;
        }
        self.finish_completed(false);
    }

    fn cancel(&mut self, token: u64) {
        if !self.matches_token(token) {
            return;
        }
        if !matches!(self.state, CaptureState::Armed | CaptureState::Recording) {
            self.record_error(CaptureError::InvalidState);
            return;
        }
        self.finish_cancelled();
    }

    fn matches_token(&mut self, token: u64) -> bool {
        let Some(buffer) = self.active.as_ref() else {
            self.record_error(CaptureError::InvalidState);
            return false;
        };
        if buffer.token != token {
            self.record_error(CaptureError::StaleToken {
                expected: buffer.token,
                received: token,
            });
            return false;
        }
        true
    }

    fn finish_completed(&mut self, hard_limit: bool) {
        let buffer = self
            .active
            .take()
            .expect("recording capture must own a buffer");
        let completion = CaptureCompletion {
            token: buffer.token,
            target: buffer.target,
            source: buffer.source,
            sample_rate: buffer.sample_rate,
            stereo: buffer.stereo,
            hard_limit,
            peak: self.peak,
        };
        self.publish(CaptureOutcome::Completed(completion));
    }

    fn finish_cancelled(&mut self) {
        let buffer = self
            .active
            .take()
            .expect("active capture must own a buffer");
        self.publish(CaptureOutcome::Cancelled(buffer));
    }

    fn publish(&mut self, outcome: CaptureOutcome) {
        match self.outcomes.push(outcome) {
            Ok(()) => self.set_state(CaptureState::Idle),
            Err(PushError::Full(outcome)) => {
                self.pending = Some(outcome);
                self.set_state(CaptureState::CompletionPending);
            }
        }
    }

    fn flush_pending(&mut self) {
        let pending = self
            .pending
            .take()
            .expect("pending capture outcome must exist");
        match self.outcomes.push(pending) {
            Ok(()) => self.set_state(CaptureState::Idle),
            Err(PushError::Full(pending)) => self.pending = Some(pending),
        }
    }

    fn record_error(&mut self, error: CaptureError) {
        self.last_error = Some(error);
    }

    fn set_state(&mut self, state: CaptureState) {
        self.state = state;
        self.shared.set_state(state);
    }
}

pub fn capture_channels(
    command_capacity: usize,
    completion_capacity: usize,
) -> (CaptureController, CaptureCore) {
    let (command_producer, command_consumer) = RingBuffer::new(command_capacity);
    let (outcome_producer, outcome_consumer) = RingBuffer::new(completion_capacity);
    let shared = Arc::new(CaptureShared {
        state: AtomicU8::new(CaptureState::IDLE),
        failed: AtomicBool::new(false),
        progress_sequence: AtomicUsize::new(0),
        progress_published: AtomicBool::new(false),
        token_low: AtomicU32::new(0),
        token_high: AtomicU32::new(0),
        frames: AtomicUsize::new(0),
        peak_bits: AtomicU32::new(0.0_f32.to_bits()),
        hard_limit: AtomicBool::new(false),
    });
    (
        CaptureController {
            commands: command_producer,
            outcomes: outcome_consumer,
            shared: Arc::clone(&shared),
        },
        CaptureCore {
            commands: command_consumer,
            outcomes: outcome_producer,
            shared,
            state: CaptureState::Idle,
            active: None,
            peak: 0.0,
            pending: None,
            last_error: None,
        },
    )
}

fn finite_or_zero(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sampler_core::PadId;

    fn buffer(token: u64, max_frames: usize) -> CaptureBuffer {
        CaptureBuffer::try_new(
            token,
            PadId::first(),
            CaptureSource::Resample,
            48_000,
            max_frames,
        )
        .unwrap()
    }

    #[test]
    fn buffer_validates_bounds_and_reserves_stereo_capacity() {
        assert!(matches!(
            CaptureBuffer::try_new(1, PadId::first(), CaptureSource::Input, 0, 1),
            Err(CaptureError::ZeroSampleRate)
        ));
        assert!(matches!(
            CaptureBuffer::try_new(1, PadId::first(), CaptureSource::Input, 48_000, 0),
            Err(CaptureError::ZeroFrameLimit)
        ));
        let too_large = CaptureBuffer::try_new(
            1,
            PadId::first(),
            CaptureSource::Input,
            48_000,
            MAX_CAPTURE_FRAMES + 1,
        )
        .unwrap_err();
        assert_eq!(
            too_large,
            CaptureError::FrameLimitTooLarge {
                max_frames: MAX_CAPTURE_FRAMES + 1,
            }
        );

        let buffer = buffer(7, 3);
        assert_eq!(buffer.stereo.len(), 0);
        assert!(buffer.stereo.capacity() >= 6);
        assert_eq!(buffer.max_frames, 3);
    }

    #[test]
    fn recording_preserves_the_preallocated_buffer_and_stop_excludes_next_frame() {
        let (mut controller, mut core) = capture_channels(4, 2);
        let buffer = buffer(11, 3);
        let allocation = buffer.stereo.as_ptr();

        controller.arm(buffer).unwrap();
        core.poll_commands();
        controller.start(11).unwrap();
        core.poll_commands();
        assert_eq!(core.state(), CaptureState::Recording);

        core.push_frame([0.25, f32::NAN]);
        core.push_frame([f32::INFINITY, -0.5]);
        controller.stop(11).unwrap();
        core.poll_commands();
        core.push_frame([0.75, 0.75]);

        let CaptureOutcome::Completed(completion) = controller.try_next_outcome().unwrap() else {
            panic!("stop must publish a completion");
        };
        assert_eq!(completion.stereo.as_ptr(), allocation);
        assert_eq!(completion.stereo, vec![0.25, 0.0, 0.0, -0.5]);
        assert_eq!(completion.peak, 0.5);
        assert!(!completion.hard_limit);
        assert_eq!(core.state(), CaptureState::Idle);
    }

    #[test]
    fn exact_limit_finishes_with_a_hard_limit_completion() {
        let (mut controller, mut core) = capture_channels(4, 1);
        controller.arm(buffer(12, 2)).unwrap();
        core.poll_commands();
        controller.start(12).unwrap();
        core.poll_commands();

        core.push_frame([0.1, -0.2]);
        assert_eq!(core.state(), CaptureState::Recording);
        core.push_frame([0.3, -0.4]);

        let CaptureOutcome::Completed(completion) = controller.try_next_outcome().unwrap() else {
            panic!("limit must publish a completion");
        };
        assert_eq!(completion.stereo, vec![0.1, -0.2, 0.3, -0.4]);
        assert!(completion.hard_limit);
        assert_eq!(completion.peak, 0.4);
        assert_eq!(core.state(), CaptureState::Idle);
    }

    #[test]
    fn cancellation_and_stale_tokens_preserve_ownership() {
        let (mut controller, mut core) = capture_channels(4, 1);
        let buffer = buffer(13, 4);
        let allocation = buffer.stereo.as_ptr();
        controller.arm(buffer).unwrap();
        core.poll_commands();

        controller.start(99).unwrap();
        core.poll_commands();
        assert_eq!(
            core.take_error(),
            Some(CaptureError::StaleToken {
                expected: 13,
                received: 99,
            })
        );
        assert_eq!(core.state(), CaptureState::Armed);

        controller.cancel(13).unwrap();
        core.poll_commands();
        let CaptureOutcome::Cancelled(returned) = controller.try_next_outcome().unwrap() else {
            panic!("cancel must return the original buffer");
        };
        assert_eq!(returned.stereo.as_ptr(), allocation);
        assert_eq!(returned.token, 13);
        assert_eq!(core.state(), CaptureState::Idle);
    }

    #[test]
    fn completion_backpressure_retains_pending_capture_and_rejects_new_arm() {
        let (mut controller, mut core) = capture_channels(4, 1);

        controller.arm(buffer(21, 1)).unwrap();
        core.poll_commands();
        controller.start(21).unwrap();
        core.poll_commands();
        core.push_frame([0.1, 0.1]);

        controller.arm(buffer(22, 1)).unwrap();
        core.poll_commands();
        controller.start(22).unwrap();
        core.poll_commands();
        core.push_frame([0.2, 0.2]);

        assert_eq!(core.state(), CaptureState::CompletionPending);
        let rejected = buffer(23, 1);
        let allocation = rejected.stereo.as_ptr();
        let failure = controller.arm(rejected).unwrap_err();
        assert_eq!(failure.error(), CaptureError::CompletionPending);
        let CaptureCommand::Arm(returned) = failure.into_command() else {
            panic!("arm failure must return the original command");
        };
        assert_eq!(returned.stereo.as_ptr(), allocation);

        assert!(matches!(
            controller.try_next_outcome(),
            Some(CaptureOutcome::Completed(CaptureCompletion {
                token: 21,
                ..
            }))
        ));
        core.poll_commands();
        assert!(matches!(
            controller.try_next_outcome(),
            Some(CaptureOutcome::Completed(CaptureCompletion {
                token: 22,
                ..
            }))
        ));
        assert_eq!(core.state(), CaptureState::Idle);
    }

    #[test]
    fn command_backpressure_returns_the_owned_command() {
        let (mut controller, mut core) = capture_channels(1, 1);
        controller.start(31).unwrap();
        let failure = controller.arm(buffer(31, 1)).unwrap_err();
        assert_eq!(failure.error(), CaptureError::CommandFull);
        assert!(matches!(failure.into_command(), CaptureCommand::Arm(_)));
        assert_eq!(controller.state(), CaptureState::Idle);

        core.poll_commands();
        controller.arm(buffer(31, 1)).unwrap();
    }

    #[test]
    fn closed_arm_admission_rolls_back_the_shared_state() {
        let (mut controller, core) = capture_channels(1, 1);
        drop(core);

        let failure = controller.arm(buffer(32, 1)).unwrap_err();
        assert_eq!(failure.error(), CaptureError::CommandClosed);
        assert!(matches!(failure.into_command(), CaptureCommand::Arm(_)));
        assert_eq!(controller.state(), CaptureState::Idle);
    }

    #[test]
    fn arm_while_active_returns_the_original_buffer() {
        let (mut controller, mut core) = capture_channels(2, 1);
        controller.arm(buffer(41, 1)).unwrap();
        core.poll_commands();

        let rejected = buffer(42, 1);
        let allocation = rejected.stereo.as_ptr();
        let failure = controller.arm(rejected).unwrap_err();
        assert_eq!(failure.error(), CaptureError::InvalidState);
        let CaptureCommand::Arm(returned) = failure.into_command() else {
            panic!("arm failure must return the original command");
        };
        assert_eq!(returned.stereo.as_ptr(), allocation);
        assert_eq!(core.state(), CaptureState::Armed);
    }

    #[test]
    fn queued_second_arm_is_rejected_without_disturbing_the_first_take() {
        let (mut controller, mut core) = capture_channels(2, 2);
        let first = buffer(51, 1);
        let first_allocation = first.stereo.as_ptr();
        controller.arm(first).unwrap();

        let second = buffer(52, 1);
        let second_allocation = second.stereo.as_ptr();
        let failure = controller.arm(second).unwrap_err();
        assert_eq!(failure.error(), CaptureError::InvalidState);
        let CaptureCommand::Arm(returned) = failure.into_command() else {
            panic!("second arm rejection must return the original command");
        };
        assert_eq!(returned.stereo.as_ptr(), second_allocation);

        core.poll_commands();
        assert_eq!(core.state(), CaptureState::Armed);
        controller.start(51).unwrap();
        core.poll_commands();
        controller.cancel(51).unwrap();
        core.poll_commands();

        let CaptureOutcome::Cancelled(returned) = controller.try_next_outcome().unwrap() else {
            panic!("first take must remain cancellable");
        };
        assert_eq!(returned.stereo.as_ptr(), first_allocation);
        assert_eq!(core.state(), CaptureState::Idle);
    }

    #[test]
    fn controller_progress_reports_exact_callback_frames_peak_and_hard_limit() {
        let (mut controller, mut core) = capture_channels(4, 1);
        controller.arm(buffer(61, 2)).unwrap();
        core.poll_commands();
        controller.start(61).unwrap();
        core.poll_commands();

        assert_eq!(
            controller.progress(),
            Some(CaptureProgressSnapshot {
                token: 61,
                frames: 0,
                peak: 0.0,
                hard_limit: false,
            })
        );
        core.push_frame([0.25, -0.5]);
        assert_eq!(
            controller.progress(),
            Some(CaptureProgressSnapshot {
                token: 61,
                frames: 1,
                peak: 0.5,
                hard_limit: false,
            })
        );
        core.push_frame([1.25, -0.75]);
        assert_eq!(
            controller.progress(),
            Some(CaptureProgressSnapshot {
                token: 61,
                frames: 2,
                peak: 1.25,
                hard_limit: true,
            })
        );
    }

    #[test]
    fn controller_progress_changes_take_identity_only_after_callback_polls_arm() {
        let (mut controller, mut core) = capture_channels(4, 2);
        controller.arm(buffer(71, 1)).unwrap();
        core.poll_commands();
        controller.start(71).unwrap();
        core.poll_commands();
        core.push_frame([0.25, -0.75]);
        let CaptureOutcome::Completed(_) = controller.try_next_outcome().unwrap() else {
            panic!("first hard-limit take must complete");
        };

        controller.arm(buffer(72, 2)).unwrap();
        assert_eq!(
            controller.progress(),
            Some(CaptureProgressSnapshot {
                token: 71,
                frames: 1,
                peak: 0.75,
                hard_limit: true,
            }),
            "enqueuing Arm must not attribute prior callback progress to the new take",
        );

        core.poll_commands();
        assert_eq!(
            controller.progress(),
            Some(CaptureProgressSnapshot {
                token: 72,
                frames: 0,
                peak: 0.0,
                hard_limit: false,
            }),
        );
    }

    #[test]
    fn controller_progress_returns_none_when_fixed_snapshot_attempts_remain_torn() {
        let (controller, _core) = capture_channels(1, 1);
        controller
            .shared
            .progress_sequence
            .store(1, Ordering::Release);

        assert_eq!(controller.progress(), None);
    }
}
