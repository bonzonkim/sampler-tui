//! Real-time audio boundary for sampler-tui.

mod command;
mod decode;
mod engine;
mod error;
mod resample;
mod sample;

pub use command::{
    AudioCommand, AudioController, COMMAND_CAPACITY, CriticalEvent, EnginePorts,
    RETIREMENT_CAPACITY, TELEMETRY_CAPACITY, Telemetry, audio_channels,
};
pub use decode::{DecodedAudio, decode_path};
pub use engine::AudioEngine;
pub use error::{ControlError, DecodeError, EngineError, PrepareError, SampleError};
pub use resample::prepare_sample;
pub use sample::{SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot};
pub use sampler_core::{Frame, PadId, PadSettings};
