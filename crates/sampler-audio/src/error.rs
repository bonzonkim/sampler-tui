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
    #[error("decoded audio has {frames} frames, exceeding the {max_frames}-frame limit")]
    FrameLimitExceeded { frames: usize, max_frames: usize },
    #[error("decoded audio needs {bytes} bytes, exceeding the {max_bytes}-byte limit")]
    ByteLimitExceeded { bytes: usize, max_bytes: usize },
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
    #[error("prepared audio has {frames} frames, exceeding the {max_frames}-frame limit")]
    FrameLimitExceeded { frames: usize, max_frames: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ControlError {
    #[error("audio session is closed after a runtime failure")]
    ClosedSession,
    #[error("audio command queue is full")]
    CommandQueueFull,
    #[error("no free sample slot is available")]
    NoFreeSampleSlot,
    #[error("no free pattern snapshot slot is available")]
    NoFreePatternSlot,
    #[error("live command identifiers are exhausted")]
    LiveCommandIdExhausted,
    #[error("trigger velocity must be finite and in 0.0..=1.0")]
    InvalidVelocity,
    #[error("audio settings are invalid")]
    InvalidSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EngineError {
    #[error("engine sample rate must be non-zero")]
    ZeroSampleRate,
    #[error("audio settings are invalid")]
    InvalidSettings,
    #[error("effect buffer size exceeds the platform limit")]
    EffectBufferSizeOverflow,
    #[error("could not allocate effect buffer storage")]
    EffectBufferAllocationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CaptureError {
    #[error("could not allocate capture storage")]
    AllocationFailed,
    #[error("capture sample rate must be non-zero")]
    ZeroSampleRate,
    #[error("capture frame limit must be non-zero")]
    ZeroFrameLimit,
    #[error("capture frame limit {max_frames} exceeds the maximum")]
    FrameLimitTooLarge { max_frames: usize },
    #[error("capture command queue is full")]
    CommandFull,
    #[error("capture command queue is closed")]
    CommandClosed,
    #[error("capture command is invalid in the current state")]
    InvalidState,
    #[error("capture command token {received} does not match active token {expected}")]
    StaleToken { expected: u64, received: u64 },
    #[error("capture contains no frames")]
    EmptyCapture,
    #[error("capture completion is pending controller reclamation")]
    CompletionPending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DeviceBufferError {
    #[error("device channel count must be non-zero")]
    ZeroChannels,
    #[error("output buffer length {samples} is not divisible by {channels} channels")]
    MisalignedOutput { samples: usize, channels: usize },
    #[error("{frames} frames cannot fill an output buffer containing {output_frames} frames")]
    FrameCountMismatch { frames: usize, output_frames: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InputBufferError {
    #[error("input device channel count must be non-zero")]
    ZeroChannels,
    #[error("input buffer length {samples} is not divisible by {channels} channels")]
    MisalignedInput { samples: usize, channels: usize },
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("no default output device is available")]
    NoDefaultOutputDevice,
    #[error("could not query the default output configuration: {0}")]
    DefaultOutputConfig(#[source] cpal::Error),
    #[error("unsupported output configuration: {channels} channels at {sample_rate} Hz")]
    UnsupportedConfiguration { sample_rate: u32, channels: u16 },
    #[error("unsupported output sample format: {0}")]
    UnsupportedSampleFormat(cpal::SampleFormat),
    #[error("could not build the output stream: {0}")]
    BuildStream(#[source] cpal::Error),
    #[error("could not start the output stream: {0}")]
    PlayStream(#[source] cpal::Error),
    #[error("output stream failed: {0}")]
    Runtime(#[source] cpal::Error),
}

#[derive(Debug, Error)]
pub enum InputDeviceError {
    #[error("no default input device is available")]
    NoDefaultInputDevice,
    #[error("could not query the default input configuration: {0}")]
    DefaultInputConfig(#[source] cpal::Error),
    #[error("unsupported input configuration: {channels} channels at {sample_rate} Hz")]
    UnsupportedConfiguration { sample_rate: u32, channels: u16 },
    #[error("unsupported input sample format: {0}")]
    UnsupportedSampleFormat(cpal::SampleFormat),
    #[error("could not build the input stream: {0}")]
    BuildStream(#[source] cpal::Error),
    #[error("could not start the input stream: {0}")]
    PlayStream(#[source] cpal::Error),
    #[error("input stream failed: {0}")]
    Runtime(#[source] cpal::Error),
}
