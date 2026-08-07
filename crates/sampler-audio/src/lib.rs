//! Real-time audio boundary for sampler-tui.

mod decode;
mod error;
mod sample;

pub use decode::{DecodedAudio, decode_path};
pub use error::{DecodeError, SampleError};
pub use sample::{SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot};
