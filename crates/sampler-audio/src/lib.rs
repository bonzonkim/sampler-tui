//! Real-time audio boundary for sampler-tui.

mod command;
mod decode;
mod device;
mod engine;
mod error;
mod resample;
mod sample;

pub use command::{
    AudioCommand, AudioController, COMMAND_CAPACITY, CriticalEvent, EnginePorts,
    RETIREMENT_CAPACITY, TELEMETRY_CAPACITY, Telemetry, audio_channels,
    audio_channels_with_test_capacities,
};
pub use decode::{DecodedAudio, decode_path};
pub use device::{AudioSession, write_frames};
pub use engine::AudioEngine;
pub use error::{
    ControlError, DecodeError, DeviceBufferError, DeviceError, EngineError, PrepareError,
    SampleError,
};
pub use resample::prepare_sample;
pub use sample::{SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot};
pub use sampler_core::{Frame, PadId, PadSettings};
