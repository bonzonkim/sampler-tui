use crate::{EventId, ModelError};

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
