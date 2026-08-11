use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    SupportedBufferSize, SupportedStreamConfig,
};

use crate::{
    AudioController, AudioEngine, DeviceBufferError, DeviceError, audio_channels,
    command::SharedControlState,
};

const LOW_LATENCY_BUFFER_FRAMES: u32 = 64;

pub struct AudioSession {
    _stream: Option<Stream>,
    controller: AudioController,
    errors: Receiver<cpal::Error>,
    sample_rate: u32,
    channels: u16,
}

impl AudioSession {
    pub fn open_default() -> Result<Self, DeviceError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(DeviceError::NoDefaultOutputDevice)?;
        let supported_config = device
            .default_output_config()
            .map_err(DeviceError::DefaultOutputConfig)?;
        let sample_rate = supported_config.sample_rate();
        let channels = supported_config.channels();
        if sample_rate == 0 || channels == 0 {
            return Err(DeviceError::UnsupportedConfiguration {
                sample_rate,
                channels,
            });
        }

        let (controller, ports) = audio_channels();
        let shared = Arc::clone(&ports.shared);
        let engine = AudioEngine::new(sample_rate, ports).map_err(|_| {
            DeviceError::UnsupportedConfiguration {
                sample_rate,
                channels,
            }
        })?;
        let config = low_latency_stream_config(&supported_config);
        let sample_format = supported_config.sample_format();
        let (error_sender, errors) = mpsc::channel();
        let stream = match sample_format {
            SampleFormat::F32 => {
                build_stream::<f32>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::F64 => {
                build_stream::<f64>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::I8 => {
                build_stream::<i8>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::I16 => {
                build_stream::<i16>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::I24 => {
                build_stream::<cpal::I24>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::I32 => {
                build_stream::<i32>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::I64 => {
                build_stream::<i64>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::U8 => {
                build_stream::<u8>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::U16 => {
                build_stream::<u16>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::U24 => {
                build_stream::<cpal::U24>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::U32 => {
                build_stream::<u32>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::U64 => {
                build_stream::<u64>(&device, &config, channels, engine, error_sender, shared)
            }
            SampleFormat::DsdU8 | SampleFormat::DsdU16 | SampleFormat::DsdU32 => {
                return Err(DeviceError::UnsupportedSampleFormat(sample_format));
            }
            _ => return Err(DeviceError::UnsupportedSampleFormat(sample_format)),
        }
        .map_err(DeviceError::BuildStream)?;
        stream.play().map_err(DeviceError::PlayStream)?;

        Ok(Self {
            _stream: Some(stream),
            controller,
            errors,
            sample_rate,
            channels,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn controller_mut(&mut self) -> &mut AudioController {
        &mut self.controller
    }

    pub fn poll_error(&self) -> Option<DeviceError> {
        self.errors.try_recv().ok().map(DeviceError::Runtime)
    }
}

pub(crate) fn low_latency_stream_config(supported: &SupportedStreamConfig) -> StreamConfig {
    let mut config = supported.config();
    if let SupportedBufferSize::Range { min, max } = *supported.buffer_size() {
        config.buffer_size = BufferSize::Fixed(LOW_LATENCY_BUFFER_FRAMES.clamp(min, max));
    }
    config
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        drop_stream_before_controller(&mut self._stream);
    }
}

pub(crate) fn drop_stream_before_controller<T>(stream: &mut Option<T>) {
    drop(stream.take());
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: u16,
    mut engine: AudioEngine,
    error_sender: Sender<cpal::Error>,
    shared: Arc<SharedControlState>,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    device.build_output_stream::<T, _, _>(
        *config,
        move |output, _| write_device(&mut engine, usize::from(channels), output),
        move |error| {
            report_runtime_error(&shared, &error_sender, error);
        },
        None,
    )
}

fn report_runtime_error(
    shared: &SharedControlState,
    error_sender: &Sender<cpal::Error>,
    error: cpal::Error,
) {
    shared.mark_failed();
    let _ = error_sender.send(error);
}

fn write_device<T>(engine: &mut AudioEngine, channels: usize, output: &mut [T])
where
    T: Sample + FromSample<f32>,
{
    if channels == 0 || !output.len().is_multiple_of(channels) {
        output.fill(T::from_sample(0.0));
        return;
    }

    let frame_count = output.len() / channels;
    let mut device_frames = output.chunks_exact_mut(channels);
    engine.render_frames(frame_count, |frame| {
        if let Some(device_frame) = device_frames.next() {
            write_frame(frame, device_frame);
        }
    });
}

pub fn write_frames<T>(
    frames: &[[f32; 2]],
    channels: usize,
    output: &mut [T],
) -> Result<(), DeviceBufferError>
where
    T: Sample + FromSample<f32>,
{
    if channels == 0 {
        return Err(DeviceBufferError::ZeroChannels);
    }
    if !output.len().is_multiple_of(channels) {
        return Err(DeviceBufferError::MisalignedOutput {
            samples: output.len(),
            channels,
        });
    }
    let output_frames = output.len() / channels;
    if frames.len() != output_frames {
        return Err(DeviceBufferError::FrameCountMismatch {
            frames: frames.len(),
            output_frames,
        });
    }

    for (frame, device_frame) in frames
        .iter()
        .copied()
        .zip(output.chunks_exact_mut(channels))
    {
        write_frame(frame, device_frame);
    }
    Ok(())
}

fn write_frame<T>(frame: [f32; 2], device_frame: &mut [T])
where
    T: Sample + FromSample<f32>,
{
    let left = sanitize(frame[0]);
    let right = sanitize(frame[1]);
    match device_frame {
        [mono] => *mono = T::from_sample((left + right) * 0.5),
        [device_left, device_right, remaining @ ..] => {
            *device_left = T::from_sample(left);
            *device_right = T::from_sample(right);
            remaining.fill(T::from_sample(0.0));
        }
        [] => {}
    }
}

fn sanitize(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::{ControlError, PadId};

    #[test]
    fn mono_device_averages_stereo() {
        let frames = [[1.0, -1.0], [0.5, 0.5]];
        let mut output = [0.0_f32; 2];
        write_frames(&frames, 1, &mut output).unwrap();
        assert_eq!(output, [0.0, 0.5]);
    }

    #[test]
    fn multichannel_device_zeros_channels_after_stereo() {
        let frames = [[0.25, -0.25]];
        let mut output = [1.0_f32; 4];
        write_frames(&frames, 4, &mut output).unwrap();
        assert_eq!(output, [0.25, -0.25, 0.0, 0.0]);
    }

    #[test]
    fn signed_and_unsigned_conversion_use_equilibrium() {
        let frames = [[0.0, 0.0]];
        let mut signed = [1_i16; 2];
        let mut unsigned = [0_u16; 2];
        write_frames(&frames, 2, &mut signed).unwrap();
        write_frames(&frames, 2, &mut unsigned).unwrap();
        assert_eq!(signed, [0, 0]);
        assert_eq!(unsigned, [u16::MAX / 2 + 1; 2]);
    }

    #[test]
    fn stereo_device_preserves_left_and_right() {
        let frames = [[-0.75, 0.75]];
        let mut output = [0.0_f32; 2];
        write_frames(&frames, 2, &mut output).unwrap();
        assert_eq!(output, [-0.75, 0.75]);
    }

    #[test]
    fn conversion_sanitizes_non_finite_values_and_clamps_finite_values() {
        let frames = [[f32::NAN, 2.0], [-2.0, f32::INFINITY]];
        let mut output = [1.0_f32; 4];
        write_frames(&frames, 2, &mut output).unwrap();
        assert_eq!(output, [0.0, 1.0, -1.0, 0.0]);
    }

    #[test]
    fn conversion_rejects_invalid_buffer_shapes() {
        let frames = [[0.0, 0.0]];
        assert!(write_frames(&frames, 0, &mut [0.0_f32; 1]).is_err());
        assert!(write_frames(&frames, 2, &mut [0.0_f32; 3]).is_err());
        assert!(write_frames(&frames, 2, &mut [0.0_f32; 4]).is_err());
    }

    #[test]
    fn low_latency_stream_config_requests_sixty_four_frames_when_supported() {
        let supported = cpal::SupportedStreamConfig::new(
            2,
            48_000,
            cpal::SupportedBufferSize::Range {
                min: 32,
                max: 4_096,
            },
            cpal::SampleFormat::F32,
        );

        let config = low_latency_stream_config(&supported);

        assert_eq!(config.buffer_size, cpal::BufferSize::Fixed(64));
    }

    #[test]
    fn consuming_runtime_error_does_not_reopen_the_controller() {
        let (mut controller, ports) = audio_channels();
        let (error_sender, errors) = mpsc::channel();
        report_runtime_error(
            &ports.shared,
            &error_sender,
            cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable),
        );

        assert!(matches!(
            errors.try_recv().ok().map(DeviceError::Runtime),
            Some(DeviceError::Runtime(_))
        ));
        assert!(errors.try_recv().is_err());
        assert_eq!(
            controller.trigger(PadId::first(), 0, 1.0),
            Err(ControlError::ClosedSession)
        );
    }

    #[test]
    fn stream_owner_teardown_drops_stream_before_controller() {
        struct Probe {
            name: &'static str,
            events: Rc<RefCell<Vec<&'static str>>>,
        }

        impl Drop for Probe {
            fn drop(&mut self) {
                self.events.borrow_mut().push(self.name);
            }
        }

        struct Harness {
            stream: Option<Probe>,
            _controller: Probe,
        }

        impl Drop for Harness {
            fn drop(&mut self) {
                drop_stream_before_controller(&mut self.stream);
            }
        }

        let events = Rc::new(RefCell::new(Vec::new()));
        drop(Harness {
            stream: Some(Probe {
                name: "stream",
                events: Rc::clone(&events),
            }),
            _controller: Probe {
                name: "controller",
                events: Rc::clone(&events),
            },
        });

        assert_eq!(&*events.borrow(), &["stream", "controller"]);
    }
}
