use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

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
    fn new(error: CaptureError, command: CaptureCommand) -> Self {
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

    fn release_arm_reservation(&self) {
        let _ = self.state.compare_exchange(
            CaptureState::ARMED,
            CaptureState::IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

pub struct CaptureController {
    commands: Producer<CaptureCommand>,
    outcomes: Consumer<CaptureOutcome>,
    shared: Arc<CaptureShared>,
}

impl CaptureController {
    pub fn arm(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureSendFailure> {
        let command = CaptureCommand::Arm(buffer);
        match self.shared.reserve_arm() {
            Ok(()) => match self.send(command) {
                Ok(()) => Ok(()),
                Err(failure) => {
                    self.shared.release_arm_reservation();
                    Err(failure)
                }
            },
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

    fn send(&mut self, command: CaptureCommand) -> Result<(), CaptureSendFailure> {
        if self.commands.is_abandoned() {
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
        if self.state != CaptureState::Recording {
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

        if buffer.stereo.len() / 2 == buffer.max_frames {
            self.finish_completed(true);
        }
    }

    pub const fn state(&self) -> CaptureState {
        self.state
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
}
