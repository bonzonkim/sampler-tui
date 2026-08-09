use crate::{EventId, ModelError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SampleEditError {
    #[error("sample edit phases must satisfy start < end <= Q32 scale")]
    InvalidPhaseRange,
    #[error("sample rate must be non-zero")]
    ZeroSampleRate,
    #[error("source PCM must contain at least one stereo frame")]
    EmptySource,
    #[error("source PCM must contain complete stereo frames")]
    OddStereoLength,
    #[error("source PCM sample {sample} is not finite")]
    NonFiniteSource { sample: usize },
    #[error("sample edit frame arithmetic overflowed")]
    ArithmeticOverflow,
    #[error("sample edit output allocation failed")]
    AllocationFailed,
    #[error("sample edit phases selected no source frames")]
    EmptyFrameRange,
    #[error("sample edit produced a non-finite output sample")]
    NonFiniteOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternEditError {
    #[error("pattern slot is outside 0..16")]
    InvalidSlot,
    #[error("pattern name must not be blank")]
    InvalidName,
    #[error("pattern already has the maximum 1,024 events")]
    Full,
    #[error("quantize strength must be finite and in 0.0..=1.0")]
    InvalidQuantizeStrength,
    #[error("pattern event is missing its raw frame")]
    MissingRawFrame,
    #[error("pattern frame arithmetic overflowed")]
    ArithmeticOverflow,
    #[error("pattern loop frame sizes must be non-zero")]
    InvalidLoopFrames,
    #[error("held-note transport resize history is full")]
    HeldRecordingResizeHistoryFull,
    #[error("pattern generation overflowed")]
    GenerationOverflow,
    #[error("pattern event {0:?} does not exist")]
    EventNotFound(EventId),
    #[error("pattern event velocity must be finite and in 0.0..=1.0")]
    InvalidVelocity,
    #[error("pattern has no clear checkpoint to restore")]
    NothingToUndo,
    #[error(transparent)]
    Model(#[from] ModelError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternCompileError {
    #[error("pattern event is invalid or outside the pattern")]
    InvalidEvent,
    #[error("pattern snapshot exceeds the maximum 2,048 actions")]
    TooManyActions,
    #[error("pattern frame arithmetic overflowed")]
    ArithmeticOverflow,
}
