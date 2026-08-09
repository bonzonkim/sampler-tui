use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction, audioadapter_buffers::direct::InterleavedSlice,
};

use crate::{DecodedAudio, PrepareError, SampleBuffer};

pub fn prepare_sample(
    decoded: DecodedAudio,
    target_rate: u32,
) -> Result<SampleBuffer, PrepareError> {
    prepare_sample_with_frame_limit(decoded, target_rate, usize::MAX)
}

pub fn prepare_sample_with_frame_limit(
    decoded: DecodedAudio,
    target_rate: u32,
    max_output_frames: usize,
) -> Result<SampleBuffer, PrepareError> {
    if target_rate == 0 {
        return Err(PrepareError::ZeroTargetRate);
    }

    let decoded = DecodedAudio::new(decoded.sample_rate, decoded.channels)?;
    let source_rate = decoded.sample_rate;
    let input_frames = decoded.frames();

    if source_rate == target_rate {
        enforce_frame_limit(input_frames, max_output_frames)?;
        let stereo = interleave_stereo(&decoded);
        return Ok(SampleBuffer::new(target_rate, stereo)?);
    }

    let params = SincInterpolationParameters::new(128, WindowFunction::Blackman2)
        .oversampling_factor(256)
        .interpolation(SincInterpolationType::Quadratic);
    let mut resampler = Async::<f32>::new_sinc(
        target_rate as f64 / source_rate as f64,
        1.1,
        &params,
        1024,
        2,
        FixedAsync::Input,
    )
    .map_err(PrepareError::ResamplerConstruction)?;
    let output_frames = resampler.process_all_needed_output_len(input_frames);
    enforce_frame_limit(output_frames, max_output_frames)?;
    let stereo = interleave_stereo(&decoded);
    let input = InterleavedSlice::new(&stereo, 2, input_frames)
        .expect("stereo input is exactly two samples per frame");
    let mut output = vec![0.0; output_frames * 2];
    let mut output_buffer = InterleavedSlice::new_mut(&mut output, 2, output_frames)
        .expect("resampler output allocation is exactly two samples per frame");
    let (_, output_frames) = resampler
        .process_all_into_buffer(&input, &mut output_buffer, input_frames, None)
        .map_err(PrepareError::Resampling)?;
    output.truncate(output_frames * 2);

    Ok(SampleBuffer::new(target_rate, output)?)
}

pub fn resample_stereo_with_frame_limit(
    source_rate: u32,
    stereo: &[f32],
    target_rate: u32,
    max_output_frames: usize,
) -> Result<SampleBuffer, PrepareError> {
    if !stereo.len().is_multiple_of(2) {
        return Ok(SampleBuffer::new(source_rate, stereo.to_vec())?);
    }
    let frames = stereo.len() / 2;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for frame in stereo.chunks_exact(2) {
        left.push(frame[0]);
        right.push(frame[1]);
    }
    prepare_sample_with_frame_limit(
        DecodedAudio::new(source_rate, vec![left, right])?,
        target_rate,
        max_output_frames,
    )
}

fn enforce_frame_limit(frames: usize, max_frames: usize) -> Result<(), PrepareError> {
    if frames > max_frames {
        Err(PrepareError::FrameLimitExceeded { frames, max_frames })
    } else {
        Ok(())
    }
}

fn interleave_stereo(decoded: &DecodedAudio) -> Vec<f32> {
    let mut stereo = Vec::with_capacity(decoded.frames() * 2);
    let left = &decoded.channels[0];
    let right = decoded.channels.get(1).unwrap_or(left);
    for (&left, &right) in left.iter().zip(right) {
        stereo.extend([left, right]);
    }
    stereo
}

#[cfg(test)]
mod tests {
    use crate::DecodedAudio;

    use super::*;

    #[test]
    fn equal_rate_duplicates_mono_exactly() {
        let decoded = DecodedAudio::new(48_000, vec![vec![0.0, 0.5, -0.5]]).unwrap();
        let sample = prepare_sample(decoded, 48_000).unwrap();
        assert_eq!(sample.data(), &[0.0, 0.0, 0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn resampling_preserves_duration_within_one_frame() {
        let source = (0..441).map(|n| (n as f32 * 0.1).sin()).collect();
        let decoded = DecodedAudio::new(44_100, vec![source]).unwrap();
        let sample = prepare_sample(decoded, 48_000).unwrap();
        assert!(sample.frames().abs_diff(480) <= 1);
        assert!(sample.data().iter().all(|value| value.is_finite()));
    }

    #[test]
    fn zero_target_rate_is_rejected() {
        let decoded = DecodedAudio::new(48_000, vec![vec![0.0]]).unwrap();
        assert!(matches!(
            prepare_sample(decoded, 0),
            Err(PrepareError::ZeroTargetRate)
        ));
    }

    #[test]
    fn prepared_payload_is_rejected_before_exceeding_its_frame_budget() {
        let decoded = DecodedAudio::new(48_000, vec![vec![0.0; 3]]).unwrap();

        let result = prepare_sample_with_frame_limit(decoded, 48_000, 2);

        assert!(result.is_err());
    }

    #[test]
    fn checked_stereo_equal_rate_path_is_bit_exact() {
        let stereo = [0.0, -0.0, 0.25, -0.25, 1.0, -1.0];

        let sample = resample_stereo_with_frame_limit(48_000, &stereo, 48_000, 3).unwrap();

        assert_eq!(sample.sample_rate(), 48_000);
        assert_eq!(sample.data(), stereo);
    }

    #[test]
    fn checked_stereo_resampling_enforces_the_prepared_frame_limit() {
        let stereo = (0..441)
            .flat_map(|frame| {
                let value = frame as f32 / 441.0;
                [value, -value]
            })
            .collect::<Vec<_>>();

        let error = resample_stereo_with_frame_limit(44_100, &stereo, 48_000, 480).unwrap_err();

        let PrepareError::FrameLimitExceeded { frames, max_frames } = error else {
            panic!("wrong error: {error:?}")
        };
        assert_eq!((frames, max_frames), (1_786, 480));
    }

    #[test]
    fn checked_stereo_rejects_malformed_and_non_finite_input() {
        assert!(resample_stereo_with_frame_limit(48_000, &[0.0], 48_000, 1).is_err());
        assert!(resample_stereo_with_frame_limit(48_000, &[f32::NAN, 0.0], 48_000, 1).is_err());
    }
}
