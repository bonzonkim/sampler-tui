use std::sync::mpsc::{self, Receiver, Sender};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};

use crate::capture::CaptureFailureHandle;
use crate::device::drop_stream_before_controller;
use crate::{CaptureController, CaptureCore, InputBufferError, InputDeviceError, capture_channels};

const INPUT_CAPTURE_COMMAND_CAPACITY: usize = 4;
const INPUT_CAPTURE_COMPLETION_CAPACITY: usize = 1;

pub struct InputCaptureSession {
    owner: InputCaptureOwner<Stream, CaptureController>,
    errors: Receiver<cpal::Error>,
    sample_rate: u32,
    channels: u16,
}

struct InputCaptureOwner<S, C> {
    stream: Option<S>,
    controller: C,
}

impl<S, C> Drop for InputCaptureOwner<S, C> {
    fn drop(&mut self) {
        drop_stream_before_controller(&mut self.stream);
    }
}

impl InputCaptureSession {
    pub fn open_default() -> Result<Self, InputDeviceError> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or(InputDeviceError::NoDefaultInputDevice)?;
        let supported_config = device
            .default_input_config()
            .map_err(InputDeviceError::DefaultInputConfig)?;
        let sample_rate = supported_config.sample_rate();
        let channels = supported_config.channels();
        if sample_rate == 0 || channels == 0 {
            return Err(InputDeviceError::UnsupportedConfiguration {
                sample_rate,
                channels,
            });
        }

        let sample_kind = input_sample_kind(supported_config.sample_format())?;
        let config = supported_config.config();
        let (controller, core) = capture_channels(
            INPUT_CAPTURE_COMMAND_CAPACITY,
            INPUT_CAPTURE_COMPLETION_CAPACITY,
        );
        let failure = controller.failure_handle();
        let (error_sender, errors) = mpsc::channel();
        let stream = match sample_kind {
            InputSampleKind::F32 => {
                build_stream::<f32>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::F64 => {
                build_stream::<f64>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::I8 => {
                build_stream::<i8>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::I16 => {
                build_stream::<i16>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::I24 => {
                build_stream::<cpal::I24>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::I32 => {
                build_stream::<i32>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::I64 => {
                build_stream::<i64>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::U8 => {
                build_stream::<u8>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::U16 => {
                build_stream::<u16>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::U24 => {
                build_stream::<cpal::U24>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::U32 => {
                build_stream::<u32>(&device, &config, channels, core, error_sender, failure)
            }
            InputSampleKind::U64 => {
                build_stream::<u64>(&device, &config, channels, core, error_sender, failure)
            }
        }
        .map_err(InputDeviceError::BuildStream)?;
        stream.play().map_err(InputDeviceError::PlayStream)?;

        Ok(Self {
            owner: InputCaptureOwner {
                stream: Some(stream),
                controller,
            },
            errors,
            sample_rate,
            channels,
        })
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub const fn channels(&self) -> u16 {
        self.channels
    }

    pub fn controller_mut(&mut self) -> &mut CaptureController {
        &mut self.owner.controller
    }

    pub fn poll_error(&self) -> Option<InputDeviceError> {
        self.errors.try_recv().ok().map(InputDeviceError::Runtime)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputSampleKind {
    F32,
    F64,
    I8,
    I16,
    I24,
    I32,
    I64,
    U8,
    U16,
    U24,
    U32,
    U64,
}

fn input_sample_kind(format: SampleFormat) -> Result<InputSampleKind, InputDeviceError> {
    match format {
        SampleFormat::F32 => Ok(InputSampleKind::F32),
        SampleFormat::F64 => Ok(InputSampleKind::F64),
        SampleFormat::I8 => Ok(InputSampleKind::I8),
        SampleFormat::I16 => Ok(InputSampleKind::I16),
        SampleFormat::I24 => Ok(InputSampleKind::I24),
        SampleFormat::I32 => Ok(InputSampleKind::I32),
        SampleFormat::I64 => Ok(InputSampleKind::I64),
        SampleFormat::U8 => Ok(InputSampleKind::U8),
        SampleFormat::U16 => Ok(InputSampleKind::U16),
        SampleFormat::U24 => Ok(InputSampleKind::U24),
        SampleFormat::U32 => Ok(InputSampleKind::U32),
        SampleFormat::U64 => Ok(InputSampleKind::U64),
        SampleFormat::DsdU8 | SampleFormat::DsdU16 | SampleFormat::DsdU32 => {
            Err(InputDeviceError::UnsupportedSampleFormat(format))
        }
        _ => Err(InputDeviceError::UnsupportedSampleFormat(format)),
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: u16,
    mut core: CaptureCore,
    error_sender: Sender<cpal::Error>,
    failure: CaptureFailureHandle,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    device.build_input_stream::<T, _, _>(
        *config,
        move |input, _| {
            let _ = write_input_device(&mut core, usize::from(channels), input);
        },
        move |error| report_runtime_error(&failure, &error_sender, error),
        None,
    )
}

fn report_runtime_error(
    failure: &CaptureFailureHandle,
    error_sender: &Sender<cpal::Error>,
    error: cpal::Error,
) {
    failure.mark_failed();
    let _ = error_sender.send(error);
}

pub fn write_input_device<T>(
    core: &mut CaptureCore,
    channels: usize,
    input: &[T],
) -> Result<usize, InputBufferError>
where
    T: Sample + Copy,
    f32: FromSample<T>,
{
    core.poll_commands();
    if channels == 0 {
        return Err(InputBufferError::ZeroChannels);
    }
    if !input.len().is_multiple_of(channels) {
        return Err(InputBufferError::MisalignedInput {
            samples: input.len(),
            channels,
        });
    }

    let frame_count = input.len() / channels;
    if core.is_failed() {
        return Ok(frame_count);
    }
    for device_frame in input.chunks_exact(channels) {
        let left = finite_or_zero(f32::from_sample(device_frame[0]));
        let right = if channels == 1 {
            left
        } else {
            finite_or_zero(f32::from_sample(device_frame[1]))
        };
        core.push_frame([left, right]);
    }
    Ok(frame_count)
}

fn finite_or_zero(sample: f32) -> f32 {
    if sample.is_finite() { sample } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc;

    use cpal::{FromSample, I24, SampleFormat, U24};

    use super::*;
    use crate::{
        CaptureBuffer, CaptureError, CaptureOutcome, CaptureSource, CaptureState, PadId,
        capture_channels,
    };

    fn input_buffer(token: u64, max_frames: usize) -> CaptureBuffer {
        CaptureBuffer::try_new(
            token,
            PadId::first(),
            CaptureSource::Input,
            48_000,
            max_frames,
        )
        .unwrap()
    }

    fn record<T>(channels: usize, input: &[T]) -> (usize, Vec<f32>)
    where
        T: cpal::Sample + Copy,
        f32: FromSample<T>,
    {
        let (mut controller, mut core) = capture_channels(4, 1);
        controller
            .arm(input_buffer(1, input.len() / channels))
            .unwrap();
        core.poll_commands();
        controller.start(1).unwrap();

        let frame_count = write_input_device(&mut core, channels, input).unwrap();
        assert_eq!(core.state(), CaptureState::Idle);
        let CaptureOutcome::Completed(completion) = controller.try_next_outcome().unwrap() else {
            panic!("exact frame limit must complete the input capture");
        };
        (frame_count, completion.stereo)
    }

    #[test]
    fn mono_input_is_duplicated_and_reports_exact_source_frame_count() {
        let (frames, stereo) = record(1, &[0.25_f32, -0.5, 0.75]);
        assert_eq!(frames, 3);
        assert_eq!(stereo, [0.25, 0.25, -0.5, -0.5, 0.75, 0.75]);
    }

    #[test]
    fn stereo_and_multichannel_input_use_only_the_first_two_channels() {
        let (stereo_frames, stereo) = record(2, &[0.25_f32, -0.5, 0.75, -1.0]);
        assert_eq!(stereo_frames, 2);
        assert_eq!(stereo, [0.25, -0.5, 0.75, -1.0]);

        let (surround_frames, surround) =
            record(4, &[0.25_f32, -0.5, 99.0, 98.0, 0.75, -1.0, 97.0, 96.0]);
        assert_eq!(surround_frames, 2);
        assert_eq!(surround, [0.25, -0.5, 0.75, -1.0]);
    }

    #[test]
    fn floating_input_sanitizes_non_finite_values_without_clamping_finite_values() {
        let (_, stereo) = record(2, &[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.25]);
        assert_eq!(stereo, [0.0, 0.0, 0.0, 1.25]);
    }

    #[test]
    fn signed_unsigned_and_twenty_four_bit_samples_convert_to_finite_f32() {
        let (_, signed) = record(2, &[i16::MIN, 0, i16::MAX, 0]);
        assert_eq!(signed[0], -1.0);
        assert_eq!(signed[1], 0.0);
        assert!(signed[2].is_finite() && signed[2] > 0.99);

        let equilibrium = u16::MAX / 2 + 1;
        let (_, unsigned) = record(2, &[0_u16, equilibrium, u16::MAX, equilibrium]);
        assert_eq!(unsigned[0], -1.0);
        assert_eq!(unsigned[1], 0.0);
        assert!(unsigned[2].is_finite() && unsigned[2] > 0.99);

        let (_, signed_24) = record(
            2,
            &[
                I24::new(-(1 << 23)).unwrap(),
                I24::new(0).unwrap(),
                I24::new((1 << 23) - 1).unwrap(),
                I24::new(0).unwrap(),
            ],
        );
        assert_eq!(signed_24[0], -1.0);
        assert_eq!(signed_24[1], 0.0);
        assert!(signed_24[2].is_finite() && signed_24[2] > 0.99);

        let (_, unsigned_24) = record(
            2,
            &[
                U24::new(0).unwrap(),
                U24::new(1 << 23).unwrap(),
                U24::new((1 << 24) - 1).unwrap(),
                U24::new(1 << 23).unwrap(),
            ],
        );
        assert_eq!(unsigned_24[0], -1.0);
        assert_eq!(unsigned_24[1], 0.0);
        assert!(unsigned_24[2].is_finite() && unsigned_24[2] > 0.99);
    }

    #[test]
    fn adapter_rejects_malformed_input_with_typed_errors() {
        let (mut controller, mut core) = capture_channels(4, 1);
        controller.arm(input_buffer(5, 1)).unwrap();
        controller.start(5).unwrap();
        assert_eq!(
            write_input_device(&mut core, 0, &[0.0_f32]),
            Err(InputBufferError::ZeroChannels)
        );
        assert_eq!(core.state(), CaptureState::Armed);
        assert_eq!(
            write_input_device(&mut core, 2, &[0.0_f32; 3]),
            Err(InputBufferError::MisalignedInput {
                samples: 3,
                channels: 2,
            })
        );
        assert_eq!(core.state(), CaptureState::Recording);

        controller.cancel(5).unwrap();
        assert_eq!(
            write_input_device(&mut core, 2, &[0.0_f32; 3]),
            Err(InputBufferError::MisalignedInput {
                samples: 3,
                channels: 2,
            })
        );
        let CaptureOutcome::Cancelled(buffer) = controller.try_next_outcome().unwrap() else {
            panic!("malformed callback must still poll the queued cancel command");
        };
        assert!(buffer.stereo().is_empty());
    }

    #[test]
    fn callback_polls_exactly_one_command_before_visiting_input_frames() {
        let (mut controller, mut core) = capture_channels(4, 1);
        controller.arm(input_buffer(7, 2)).unwrap();
        controller.start(7).unwrap();

        assert_eq!(
            write_input_device(&mut core, 2, &[0.25_f32, -0.25]).unwrap(),
            1
        );
        assert_eq!(core.state(), CaptureState::Armed);
        assert!(controller.try_next_outcome().is_none());

        assert_eq!(
            write_input_device(&mut core, 2, &[0.5_f32, -0.5]).unwrap(),
            1
        );
        assert_eq!(core.state(), CaptureState::Recording);
        controller.stop(7).unwrap();
        assert_eq!(
            write_input_device(&mut core, 2, &[0.75_f32, -0.75]).unwrap(),
            1
        );

        let CaptureOutcome::Completed(completion) = controller.try_next_outcome().unwrap() else {
            panic!("stop must publish the captured input");
        };
        assert_eq!(completion.stereo, [0.5, -0.5]);
    }

    #[test]
    fn idle_adapter_writes_no_frames() {
        let (mut controller, mut core) = capture_channels(2, 1);
        assert_eq!(
            write_input_device(&mut core, 2, &[0.25_f32, -0.25]).unwrap(),
            1
        );

        controller.arm(input_buffer(9, 1)).unwrap();
        core.poll_commands();
        controller.start(9).unwrap();
        core.poll_commands();
        controller.stop(9).unwrap();
        core.poll_commands();
        assert_eq!(core.take_error(), Some(CaptureError::EmptyCapture));
        let CaptureOutcome::Cancelled(buffer) = controller.try_next_outcome().unwrap() else {
            panic!("idle callback must leave the armed buffer empty");
        };
        assert!(buffer.stereo().is_empty());
    }

    #[test]
    fn every_output_supported_non_dsd_sample_format_is_dispatched() {
        for format in [
            SampleFormat::F32,
            SampleFormat::F64,
            SampleFormat::I8,
            SampleFormat::I16,
            SampleFormat::I24,
            SampleFormat::I32,
            SampleFormat::I64,
            SampleFormat::U8,
            SampleFormat::U16,
            SampleFormat::U24,
            SampleFormat::U32,
            SampleFormat::U64,
        ] {
            assert!(input_sample_kind(format).is_ok(), "missing {format}");
        }
        for format in [
            SampleFormat::DsdU8,
            SampleFormat::DsdU16,
            SampleFormat::DsdU32,
        ] {
            assert!(matches!(
                input_sample_kind(format),
                Err(InputDeviceError::UnsupportedSampleFormat(rejected)) if rejected == format
            ));
        }
    }

    #[test]
    fn runtime_error_closes_capture_admission_and_reaches_session_polling() {
        let (mut controller, mut core) = capture_channels(4, 1);
        let failure = controller.failure_handle();
        let (error_sender, errors) = mpsc::channel();

        controller.arm(input_buffer(11, 2)).unwrap();
        core.poll_commands();
        controller.start(11).unwrap();
        core.poll_commands();
        write_input_device(&mut core, 2, &[0.25_f32, -0.25]).unwrap();

        report_runtime_error(
            &failure,
            &error_sender,
            cpal::Error::new(cpal::ErrorKind::DeviceNotAvailable),
        );

        assert!(matches!(
            errors.try_recv().ok().map(InputDeviceError::Runtime),
            Some(InputDeviceError::Runtime(_))
        ));
        assert_eq!(
            write_input_device(&mut core, 2, &[0.75_f32, -0.75]).unwrap(),
            1
        );
        assert_eq!(core.state(), CaptureState::Recording);
        assert!(controller.try_next_outcome().is_none());

        let rejected = controller.arm(input_buffer(12, 1)).unwrap_err();
        assert_eq!(rejected.error(), CaptureError::CommandClosed);
        assert!(matches!(
            rejected.into_command(),
            crate::CaptureCommand::Arm(_)
        ));
        drop(core);
    }

    #[test]
    fn input_session_teardown_drops_stream_core_before_controller() {
        struct Probe {
            name: &'static str,
            events: Rc<RefCell<Vec<&'static str>>>,
        }

        impl Drop for Probe {
            fn drop(&mut self) {
                self.events.borrow_mut().push(self.name);
            }
        }

        let events = Rc::new(RefCell::new(Vec::new()));
        drop(InputCaptureOwner {
            stream: Some(Probe {
                name: "stream/core",
                events: Rc::clone(&events),
            }),
            controller: Probe {
                name: "controller",
                events: Rc::clone(&events),
            },
        });

        assert_eq!(&*events.borrow(), &["stream/core", "controller"]);
    }
}
