use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cpal::SupportedBufferSize;
use cpal::traits::{DeviceTrait, HostTrait};
use sampler_audio::{
    AudioSession, CaptureBuffer, CaptureCompletion, CaptureOutcome, CaptureSource,
    InputCaptureSession, PadId, PadSettings, SampleBuffer,
};
use sampler_core::{BankId, PadMixSettings, PlaybackMode};

const OUTPUT_DEVICE: &str = "MacBook Air Speakers";
const INPUT_DEVICE: &str = "MacBook Air Microphone";
const TEST_TONE_HZ: f64 = 997.0;

fn minimum_buffer_frames(size: &SupportedBufferSize) -> Option<u32> {
    match *size {
        SupportedBufferSize::Range { min, .. } => Some(min),
        SupportedBufferSize::Unknown => None,
    }
}

fn wait_for_input_frames(
    input: &mut InputCaptureSession,
    minimum: usize,
    timeout: Duration,
) -> usize {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(progress) = input.controller_mut().progress()
            && progress.frames >= minimum
        {
            return progress.frames;
        }
        assert!(
            Instant::now() < deadline,
            "input capture did not reach {minimum} frames"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn stop_and_wait_for_capture(input: &mut InputCaptureSession, token: u64) -> CaptureCompletion {
    input.controller_mut().stop(token).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(outcome) = input.controller_mut().try_next_outcome() {
            let CaptureOutcome::Completed(completion) = outcome else {
                panic!("input capture was cancelled instead of completed");
            };
            return completion;
        }
        assert!(
            Instant::now() < deadline,
            "input capture did not complete after stop"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn tone_magnitude(stereo: &[f32], sample_rate: u32, start: usize, end: usize) -> f64 {
    let frames = stereo.len() / 2;
    let start = start.min(frames);
    let end = end.min(frames).max(start);
    let count = end - start;
    if count == 0 {
        return 0.0;
    }
    let mut cosine = 0.0;
    let mut sine = 0.0;
    for frame in start..end {
        let mono = f64::from((stereo[frame * 2] + stereo[frame * 2 + 1]) * 0.5);
        let phase = std::f64::consts::TAU * TEST_TONE_HZ * frame as f64 / f64::from(sample_rate);
        cosine += mono * phase.cos();
        sine += mono * phase.sin();
    }
    2.0 * cosine.hypot(sine) / count as f64
}

fn tone_sample(sample_rate: u32, frames: usize, amplitude: f32) -> Arc<SampleBuffer> {
    let mut stereo = Vec::with_capacity(frames * 2);
    for frame in 0..frames {
        let phase = std::f64::consts::TAU * TEST_TONE_HZ * frame as f64 / f64::from(sample_rate);
        let value = (phase.sin() as f32) * amplitude;
        stereo.extend_from_slice(&[value, value]);
    }
    Arc::new(SampleBuffer::new(sample_rate, stereo).unwrap())
}

fn recorded_loop(completion: &CaptureCompletion, start: usize, frames: usize) -> Arc<SampleBuffer> {
    let available = completion.stereo.len() / 2;
    let start = start.min(available.saturating_sub(1));
    let end = start.saturating_add(frames).min(available);
    Arc::new(
        SampleBuffer::new(
            completion.sample_rate,
            completion.stereo[start * 2..end * 2].to_vec(),
        )
        .unwrap(),
    )
}

#[test]
#[ignore = "requires physical MacBook Air speakers, microphone, and microphone permission"]
fn macbook_air_microphone_recording_replays_and_stops_through_speakers() {
    assert_eq!(std::env::consts::OS, "macos");
    let host = cpal::default_host();
    let output_device = host.default_output_device().expect("default output device");
    let input_device = host.default_input_device().expect("default input device");
    let output_description = output_device.description().unwrap();
    let input_description = input_device.description().unwrap();
    assert_eq!(output_description.name(), OUTPUT_DEVICE);
    assert_eq!(input_description.name(), INPUT_DEVICE);

    let output_config = output_device.default_output_config().unwrap();
    let input_config = input_device.default_input_config().unwrap();
    let output_min = minimum_buffer_frames(output_config.buffer_size());
    let input_min = minimum_buffer_frames(input_config.buffer_size());
    eprintln!(
        "hardware: output={OUTPUT_DEVICE} {}Hz/{}ch min_buffer={output_min:?}; input={INPUT_DEVICE} {}Hz/{}ch min_buffer={input_min:?}",
        output_config.sample_rate(),
        output_config.channels(),
        input_config.sample_rate(),
        input_config.channels(),
    );
    assert!(output_min.is_none_or(|frames| frames <= 128));
    assert!(input_min.is_none_or(|frames| frames <= 128));

    eprintln!("opening 64-frame output stream");
    let mut output = AudioSession::open_default().unwrap();
    eprintln!("opening input stream with default buffer negotiation");
    let mut input = InputCaptureSession::open_default().unwrap();
    eprintln!("both audio streams are running");
    assert_eq!(output.sample_rate(), input.sample_rate());
    let sample_rate = output.sample_rate();
    let rate = sample_rate as usize;
    let first_token = 41;
    input
        .controller_mut()
        .arm(
            CaptureBuffer::try_new(
                first_token,
                PadId::first(),
                CaptureSource::Input,
                sample_rate,
                rate * 3,
            )
            .unwrap(),
        )
        .unwrap();
    input.controller_mut().start(first_token).unwrap();
    let tone_submitted_at = wait_for_input_frames(&mut input, rate / 4, Duration::from_secs(2));

    let source_pad = PadId::first();
    output
        .controller_mut()
        .install(
            source_pad,
            tone_sample(sample_rate, rate * 3 / 4, 0.08),
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();
    let trigger_horizon = output.controller_mut().render_horizon();
    output
        .controller_mut()
        .trigger_live(source_pad, 1.0)
        .unwrap();
    wait_for_input_frames(&mut input, tone_submitted_at + rate, Duration::from_secs(3));
    let first = stop_and_wait_for_capture(&mut input, first_token);
    assert!(output.poll_error().is_none());
    assert!(input.poll_error().is_none());

    let baseline = tone_magnitude(&first.stereo, sample_rate, rate / 20, rate / 5);
    let detected = tone_magnitude(
        &first.stereo,
        sample_rate,
        tone_submitted_at + rate / 8,
        tone_submitted_at + rate * 5 / 8,
    );
    eprintln!(
        "physical capture: peak={:.6} baseline_997hz={baseline:.6} detected_997hz={detected:.6} trigger_horizon={trigger_horizon}",
        first.peak,
    );
    assert!(
        first.peak > 0.001,
        "microphone capture stayed effectively silent"
    );
    assert!(
        detected > 0.0005,
        "speaker tone was not detected by the microphone"
    );
    assert!(
        detected > baseline * 3.0,
        "997Hz tone did not rise enough above baseline"
    );

    let recorded_start = tone_submitted_at + rate / 8;
    let recorded = recorded_loop(&first, recorded_start, rate / 2);
    let replay_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
    let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
    output
        .controller_mut()
        .install(replay_pad, recorded, looping, PadMixSettings::default())
        .unwrap();

    let second_token = 42;
    input
        .controller_mut()
        .arm(
            CaptureBuffer::try_new(
                second_token,
                replay_pad,
                CaptureSource::Input,
                sample_rate,
                rate * 3,
            )
            .unwrap(),
        )
        .unwrap();
    input.controller_mut().start(second_token).unwrap();
    let replay_submitted_at = wait_for_input_frames(&mut input, rate / 4, Duration::from_secs(2));
    let replay_horizon = output.controller_mut().render_horizon();
    output
        .controller_mut()
        .trigger_live(replay_pad, 1.0)
        .unwrap();
    let stop_submitted_at = wait_for_input_frames(
        &mut input,
        replay_submitted_at + rate * 3 / 4,
        Duration::from_secs(3),
    );
    output.controller_mut().stop_pad(replay_pad).unwrap();
    wait_for_input_frames(
        &mut input,
        stop_submitted_at + rate / 2,
        Duration::from_secs(2),
    );
    let second = stop_and_wait_for_capture(&mut input, second_token);
    assert!(output.poll_error().is_none());
    assert!(input.poll_error().is_none());

    let replay_baseline = tone_magnitude(
        &second.stereo,
        sample_rate,
        rate / 20,
        replay_submitted_at.saturating_sub(rate / 20),
    );
    let replay_active = tone_magnitude(
        &second.stereo,
        sample_rate,
        replay_submitted_at + rate / 8,
        stop_submitted_at.saturating_sub(rate / 8),
    );
    let after_stop = tone_magnitude(
        &second.stereo,
        sample_rate,
        stop_submitted_at + rate / 5,
        stop_submitted_at + rate * 2 / 5,
    );
    eprintln!(
        "physical replay/stop: baseline_997hz={replay_baseline:.6} active_997hz={replay_active:.6} after_stop_997hz={after_stop:.6} replay_horizon={replay_horizon}"
    );
    assert!(
        replay_active > 0.00001,
        "recorded microphone sample was not replayed at a measurable level"
    );
    assert!(
        after_stop < replay_active * 0.25,
        "speaker output did not become quiet promptly after stop"
    );
}
