#[cfg(test)]
mod tests {
    use super::*;

    const fn scale() -> u64 {
        SAMPLE_PHASE_SCALE
    }

    #[test]
    fn q32_trim_maps_stably_across_device_rates() {
        let recipe = SampleEditRecipe::new(scale() / 4, scale() * 3 / 4, false, false).unwrap();

        assert_eq!(recipe.frame_range(48_000).unwrap(), 12_000..36_000);
        assert_eq!(recipe.frame_range(44_100).unwrap(), 11_025..33_075);
    }

    #[test]
    fn empty_and_reversed_phase_ranges_are_rejected() {
        assert!(SampleEditRecipe::new(scale(), scale(), false, false).is_err());
        assert!(SampleEditRecipe::new(scale(), 0, false, false).is_err());
    }

    #[test]
    fn trim_then_reverse_then_normalize_preserves_stereo_frames() {
        let source = [
            0.25, -0.5, // excluded frame 0
            0.5, -0.25, // frame 1
            -1.0, 0.75, // frame 2
            0.75, -0.25, // excluded frame 3
        ];
        let recipe = SampleEditRecipe::new(scale() / 4, scale() * 3 / 4, true, true).unwrap();

        let rendered = apply_sample_edit(48_000, &source, recipe).unwrap();
        let target = 10_f32.powf(-1.0 / 20.0);

        assert_eq!(rendered.sample_rate(), 48_000);
        assert_eq!(rendered.frame_range(), 1..3);
        assert!((rendered.normalization_gain() - f64::from(target)).abs() < f64::EPSILON);
        assert!((rendered.data()[0] + target).abs() < 1e-6);
        assert!((rendered.data()[1] - 0.75 * target).abs() < 1e-6);
        assert!((rendered.data()[2] - 0.5 * target).abs() < 1e-6);
        assert!((rendered.data()[3] + 0.25 * target).abs() < 1e-6);
    }

    #[test]
    fn normalization_targets_negative_one_dbfs_and_leaves_silence_at_unity_gain() {
        let normalized = apply_sample_edit(
            48_000,
            &[0.5, -0.25, -0.125, 0.25],
            SampleEditRecipe::new(0, scale(), false, true).unwrap(),
        )
        .unwrap();
        let peak = normalized
            .data()
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        assert!((peak - 10_f32.powf(-1.0 / 20.0)).abs() < 1e-6);

        let silent = apply_sample_edit(
            48_000,
            &[0.0, 0.0],
            SampleEditRecipe::new(0, scale(), false, true).unwrap(),
        )
        .unwrap();
        assert_eq!(silent.normalization_gain(), 1.0);
        assert_eq!(silent.data(), &[0.0, 0.0]);
    }

    #[test]
    fn fractional_boundaries_produce_one_complete_stereo_frame() {
        let recipe = SampleEditRecipe::new(scale() / 2, scale() * 2 / 3, false, false).unwrap();
        let rendered = apply_sample_edit(48_000, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], recipe).unwrap();

        assert_eq!(rendered.frame_range(), 1..2);
        assert_eq!(rendered.data(), &[3.0, 4.0]);
    }

    #[test]
    fn malformed_source_and_rate_errors_leave_the_input_unchanged() {
        let cases = [
            (0, vec![1.0, 2.0]),
            (48_000, vec![]),
            (48_000, vec![1.0]),
            (48_000, vec![1.0, f32::NAN]),
        ];

        for (sample_rate, source) in cases {
            let before = source.clone();
            assert!(apply_sample_edit(sample_rate, &source, SampleEditRecipe::identity()).is_err());
            assert_eq!(
                source
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                before
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn public_recipe_literals_and_maximum_frame_counts_are_revalidated_without_overflow() {
        let invalid = SampleEditRecipe {
            start_phase: scale(),
            end_phase: scale(),
            reversed: false,
            normalize: false,
        };
        assert!(invalid.validate().is_err());
        assert_eq!(
            SampleEditRecipe::identity()
                .frame_range(usize::MAX)
                .unwrap(),
            0..usize::MAX
        );
    }

    #[test]
    fn recipes_round_trip_for_later_persistence_but_deserialized_invalid_values_require_validation()
    {
        let recipe = SampleEditRecipe::new(scale() / 4, scale() * 3 / 4, true, true).unwrap();
        let encoded = toml::to_string(&recipe).unwrap();
        let decoded: SampleEditRecipe = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, recipe);

        let invalid: SampleEditRecipe = toml::from_str(
            "start_phase = 4294967296\nend_phase = 4294967296\nreversed = false\nnormalize = false\n",
        )
        .unwrap();
        assert_eq!(invalid.validate(), Err(SampleEditError::InvalidPhaseRange));
    }
}
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::SampleEditError;

/// Number of Q32 phase units in a complete source buffer.
pub const SAMPLE_PHASE_SCALE: u64 = 1_u64 << 32;

/// A reversible, source-rate-independent sample edit recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleEditRecipe {
    pub start_phase: u64,
    pub end_phase: u64,
    pub reversed: bool,
    pub normalize: bool,
}

impl SampleEditRecipe {
    pub fn new(
        start_phase: u64,
        end_phase: u64,
        reversed: bool,
        normalize: bool,
    ) -> Result<Self, SampleEditError> {
        let recipe = Self {
            start_phase,
            end_phase,
            reversed,
            normalize,
        };
        recipe.validate()?;
        Ok(recipe)
    }

    pub const fn identity() -> Self {
        Self {
            start_phase: 0,
            end_phase: SAMPLE_PHASE_SCALE,
            reversed: false,
            normalize: false,
        }
    }

    /// Revalidates a recipe assembled through a public literal or deserialization.
    pub fn validate(self) -> Result<(), SampleEditError> {
        if self.start_phase >= self.end_phase || self.end_phase > SAMPLE_PHASE_SCALE {
            return Err(SampleEditError::InvalidPhaseRange);
        }
        Ok(())
    }

    /// Maps the Q32 trim boundaries to a source buffer's complete stereo frames.
    pub fn frame_range(self, source_frames: usize) -> Result<Range<usize>, SampleEditError> {
        self.validate()?;
        if source_frames == 0 {
            return Err(SampleEditError::EmptySource);
        }

        let source_frames = source_frames as u128;
        let scale = u128::from(SAMPLE_PHASE_SCALE);
        let start = u128::from(self.start_phase)
            .checked_mul(source_frames)
            .ok_or(SampleEditError::ArithmeticOverflow)?
            / scale;
        let end = u128::from(self.end_phase)
            .checked_mul(source_frames)
            .ok_or(SampleEditError::ArithmeticOverflow)?
            .div_ceil(scale);
        let start = start.min(source_frames);
        let end = end.min(source_frames);
        let start = usize::try_from(start).map_err(|_| SampleEditError::ArithmeticOverflow)?;
        let end = usize::try_from(end).map_err(|_| SampleEditError::ArithmeticOverflow)?;

        (start < end)
            .then_some(start..end)
            .ok_or(SampleEditError::EmptyFrameRange)
    }
}

/// Validated stereo PCM generated by applying one edit recipe to a base buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleEditPlan {
    sample_rate: u32,
    stereo: Vec<f32>,
    frame_range: Range<usize>,
    normalization_gain: f64,
}

impl SampleEditPlan {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn frames(&self) -> usize {
        self.stereo.len() / 2
    }

    pub fn data(&self) -> &[f32] {
        &self.stereo
    }

    pub fn into_stereo(self) -> Vec<f32> {
        self.stereo
    }

    pub fn frame_range(&self) -> Range<usize> {
        self.frame_range.clone()
    }

    pub fn normalization_gain(&self) -> f64 {
        self.normalization_gain
    }
}

/// Applies trim, reverse, and normalization to an immutable stereo source buffer.
pub fn apply_sample_edit(
    sample_rate: u32,
    source: &[f32],
    recipe: SampleEditRecipe,
) -> Result<SampleEditPlan, SampleEditError> {
    recipe.validate()?;
    validate_source(sample_rate, source)?;

    let frame_range = recipe.frame_range(source.len() / 2)?;
    let sample_start = frame_range
        .start
        .checked_mul(2)
        .ok_or(SampleEditError::ArithmeticOverflow)?;
    let sample_end = frame_range
        .end
        .checked_mul(2)
        .ok_or(SampleEditError::ArithmeticOverflow)?;
    let mut stereo = source
        .get(sample_start..sample_end)
        .ok_or(SampleEditError::ArithmeticOverflow)?
        .to_vec();

    if recipe.reversed {
        reverse_stereo_frames(&mut stereo);
    }

    let normalization_gain = if recipe.normalize {
        normalize(&mut stereo)?
    } else {
        1.0
    };
    if stereo.iter().any(|sample| !sample.is_finite()) {
        return Err(SampleEditError::NonFiniteOutput);
    }

    Ok(SampleEditPlan {
        sample_rate,
        stereo,
        frame_range,
        normalization_gain,
    })
}

fn validate_source(sample_rate: u32, source: &[f32]) -> Result<(), SampleEditError> {
    if sample_rate == 0 {
        return Err(SampleEditError::ZeroSampleRate);
    }
    if source.is_empty() {
        return Err(SampleEditError::EmptySource);
    }
    if !source.len().is_multiple_of(2) {
        return Err(SampleEditError::OddStereoLength);
    }
    if let Some(sample) = source.iter().position(|sample| !sample.is_finite()) {
        return Err(SampleEditError::NonFiniteSource { sample });
    }
    Ok(())
}

fn reverse_stereo_frames(stereo: &mut [f32]) {
    let frames = stereo.len() / 2;
    for frame in 0..(frames / 2) {
        let other = frames - 1 - frame;
        stereo.swap(frame * 2, other * 2);
        stereo.swap(frame * 2 + 1, other * 2 + 1);
    }
}

fn normalize(stereo: &mut [f32]) -> Result<f64, SampleEditError> {
    let peak = stereo
        .iter()
        .copied()
        .map(|sample| f64::from(sample).abs())
        .fold(0.0_f64, f64::max);
    if peak == 0.0 {
        return Ok(1.0);
    }

    let gain = f64::from(10_f32.powf(-1.0 / 20.0)) / peak;
    if !gain.is_finite() {
        return Err(SampleEditError::NonFiniteOutput);
    }
    for sample in stereo {
        *sample = (f64::from(*sample) * gain) as f32;
        if !sample.is_finite() {
            return Err(SampleEditError::NonFiniteOutput);
        }
    }
    Ok(gain)
}
