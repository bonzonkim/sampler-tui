use crate::error::SampleError;

pub const SAMPLE_SLOT_COUNT: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SampleSlot(u16);

impl SampleSlot {
    pub fn new(index: usize) -> Result<Self, SampleError> {
        if index < SAMPLE_SLOT_COUNT {
            Ok(Self(index as u16))
        } else {
            Err(SampleError::SlotOutOfRange(index))
        }
    }

    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SampleBuffer {
    sample_rate: u32,
    stereo: Box<[f32]>,
}

impl SampleBuffer {
    pub fn new(sample_rate: u32, stereo: Vec<f32>) -> Result<Self, SampleError> {
        if sample_rate == 0 {
            return Err(SampleError::ZeroRate);
        }
        if stereo.is_empty() {
            return Err(SampleError::Empty);
        }
        if !stereo.len().is_multiple_of(2) {
            return Err(SampleError::OddStereoLength);
        }
        if let Some(sample) = stereo.iter().position(|value| !value.is_finite()) {
            return Err(SampleError::NonFinite { sample });
        }

        Ok(Self {
            sample_rate,
            stereo: stereo.into_boxed_slice(),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn frames(&self) -> usize {
        self.stereo.len() / 2
    }

    pub fn data(&self) -> &[f32] {
        &self.stereo
    }

    pub fn frame_linear(&self, position: f64) -> Option<[f32; 2]> {
        if !position.is_finite() || position.is_sign_negative() || position >= self.frames() as f64
        {
            return None;
        }

        let frame = position as usize;
        let current = [self.stereo[frame * 2], self.stereo[frame * 2 + 1]];
        if frame + 1 == self.frames() {
            return Some(current);
        }

        let fraction = position - frame as f64;
        let next = [
            self.stereo[(frame + 1) * 2],
            self.stereo[(frame + 1) * 2 + 1],
        ];
        Some([
            (f64::from(current[0]) * (1.0 - fraction) + f64::from(next[0]) * fraction) as f32,
            (f64::from(current[1]) * (1.0 - fraction) + f64::from(next[1]) * fraction) as f32,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_stereo_shape_and_finite_values() {
        assert_eq!(
            SampleBuffer::new(0, vec![0.0, 0.0]).unwrap_err(),
            SampleError::ZeroRate
        );
        assert_eq!(
            SampleBuffer::new(48_000, vec![]).unwrap_err(),
            SampleError::Empty
        );
        assert_eq!(
            SampleBuffer::new(48_000, vec![0.0]).unwrap_err(),
            SampleError::OddStereoLength
        );
        assert_eq!(
            SampleBuffer::new(48_000, vec![f32::NAN, 0.0]).unwrap_err(),
            SampleError::NonFinite { sample: 0 }
        );
    }

    #[test]
    fn interpolates_between_stereo_frames() {
        let sample = SampleBuffer::new(48_000, vec![0.0, 1.0, 1.0, -1.0]).unwrap();
        assert_eq!(sample.frames(), 2);
        assert_eq!(sample.frame_linear(0.5), Some([0.5, 0.0]));
        assert_eq!(sample.frame_linear(1.0), Some([1.0, -1.0]));
        assert_eq!(sample.frame_linear(2.0), None);
    }

    #[test]
    fn sample_slots_are_bounded() {
        assert_eq!(SampleSlot::new(255).unwrap().index(), 255);
        assert_eq!(SampleSlot::new(256), Err(SampleError::SlotOutOfRange(256)));
    }
}
