//! Real-time audio boundary for sampler-tui.

mod capture;
mod command;
mod decode;
mod device;
mod engine;
mod error;
mod input;
mod resample;
mod sample;

pub use capture::{
    CaptureBuffer, CaptureCommand, CaptureCompletion, CaptureController, CaptureCore,
    CaptureOutcome, CaptureSendFailure, CaptureSource, CaptureState, MAX_CAPTURE_FRAMES,
    capture_channels,
};
pub use command::{
    AudioCommand, AudioController, COMMAND_CAPACITY, CaptureStatus, CommandConsumer, CriticalEvent,
    EnginePorts, LIVE_ACK_CAPACITY, LiveAck, LiveAckKind, LiveCommandId,
    PATTERN_RETIREMENT_CAPACITY, PATTERN_SNAPSHOT_SLOT_COUNT, PatternRetirement,
    PatternSnapshotSlot, PatternSwitch, RETIREMENT_CAPACITY, TELEMETRY_CAPACITY, Telemetry,
    TransportStamp, audio_channels, audio_channels_with_test_capacities,
};
pub use decode::{
    DecodeLimits, DecodedAudio, EncodedAudioFormat, decode_bytes_with_limits, decode_path,
    decode_path_with_limits, decode_shared_bytes_with_limits, probe_shared_audio_format,
};
pub use device::{AudioSession, write_frames};
pub use engine::AudioEngine;
pub use error::{
    CaptureError, ControlError, DecodeError, DeviceBufferError, DeviceError, EngineError,
    InputBufferError, InputDeviceError, PrepareError, SampleError,
};
pub use input::{InputCaptureSession, write_input_device};
pub use resample::{
    prepare_sample, prepare_sample_with_frame_limit, resample_stereo_with_frame_limit,
};
pub use sample::{SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot};
pub use sampler_core::{Frame, PadId, PadSettings};
