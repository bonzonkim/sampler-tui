//! Real-time audio boundary for sampler-tui.

mod command;
mod decode;
mod device;
mod engine;
mod error;
mod resample;
mod sample;

pub use command::{
    AudioCommand, AudioController, COMMAND_CAPACITY, CommandConsumer, CriticalEvent, EnginePorts,
    LIVE_ACK_CAPACITY, LiveAck, LiveAckKind, LiveCommandId, PATTERN_RETIREMENT_CAPACITY,
    PATTERN_SNAPSHOT_SLOT_COUNT, PatternRetirement, PatternSnapshotSlot, PatternSwitch,
    RETIREMENT_CAPACITY, TELEMETRY_CAPACITY, Telemetry, TransportStamp, audio_channels,
    audio_channels_with_test_capacities,
};
pub use decode::{DecodeLimits, DecodedAudio, decode_path, decode_path_with_limits};
pub use device::{AudioSession, write_frames};
pub use engine::AudioEngine;
pub use error::{
    ControlError, DecodeError, DeviceBufferError, DeviceError, EngineError, PrepareError,
    SampleError,
};
pub use resample::{prepare_sample, prepare_sample_with_frame_limit};
pub use sample::{SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot};
pub use sampler_core::{Frame, PadId, PadSettings};
