//! Real-time audio boundary for sampler-tui.

mod error;
mod sample;

pub use error::SampleError;
pub use sample::{SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot};
