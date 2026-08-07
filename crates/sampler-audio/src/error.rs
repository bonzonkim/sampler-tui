use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SampleError {
    #[error("sample rate must be non-zero")]
    ZeroRate,
    #[error("sample data must not be empty")]
    Empty,
    #[error("interleaved stereo data must contain an even number of samples")]
    OddStereoLength,
    #[error("sample {sample} is not finite")]
    NonFinite { sample: usize },
    #[error("sample slot {0} is out of range")]
    SlotOutOfRange(usize),
}
