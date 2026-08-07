use std::sync::Arc;

use sampler_audio::{Frame, SampleBuffer, SampleSlot, Telemetry};
use sampler_core::{PadId, PadSettings};

pub trait AudioPort {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn render_horizon(&self) -> Frame;
    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, String>;
    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String>;
    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String>;
    fn stop_pad(&mut self, pad: PadId) -> Result<(), String>;
    fn stop_all(&mut self) -> Result<(), String>;
    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String>;
    fn reclaim_retired(&mut self) -> usize;
    fn latest_telemetry(&mut self) -> Option<Telemetry>;
    fn poll_runtime_error(&mut self) -> Option<String>;
}
