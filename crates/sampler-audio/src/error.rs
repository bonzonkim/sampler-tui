use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("could not open {path}: {message}")]
    Open {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("could not probe {path}: {message}")]
    Probe {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("no audio track in {path}")]
    NoAudioTrack { path: std::path::PathBuf },
    #[error("unsupported codec in {path}: {message}")]
    UnsupportedCodec {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("unsupported channel count: {0}")]
    UnsupportedChannels(usize),
    #[error("audio format changed during decoding")]
    ChangingFormat,
    #[error("could not decode {path}: {message}")]
    Decode {
        path: std::path::PathBuf,
        message: String,
    },
    #[error("decoded audio must not be empty")]
    Empty,
    #[error("decoded audio contains a non-finite sample")]
    NonFinite,
}

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

#[derive(Debug, Error)]
pub enum PrepareError {
    #[error("target sample rate must be non-zero")]
    ZeroTargetRate,
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("could not create resampler: {0}")]
    ResamplerConstruction(#[source] rubato::ResamplerConstructionError),
    #[error("could not resample audio: {0}")]
    Resampling(#[source] rubato::ResampleError),
    #[error(transparent)]
    Sample(#[from] SampleError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ControlError {
    #[error("audio command queue is full")]
    CommandQueueFull,
    #[error("no free sample slot is available")]
    NoFreeSampleSlot,
    #[error("trigger velocity must be finite and in 0.0..=1.0")]
    InvalidVelocity,
}
