//! Sample-clock transport model.

use serde::{Deserialize, Serialize};

use crate::{Frame, ModelError};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tempo(f64);

impl Tempo {
    pub fn new(bpm: f64) -> Result<Self, ModelError> {
        (bpm.is_finite() && (20.0..=300.0).contains(&bpm))
            .then_some(Self(bpm))
            .ok_or(ModelError::TempoOutOfRange)
    }

    pub fn bpm(self) -> f64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meter {
    numerator: u8,
    denominator: u8,
}

impl Meter {
    pub fn new(numerator: u8, denominator: u8) -> Result<Self, ModelError> {
        if !(1..=16).contains(&numerator) || !matches!(denominator, 2 | 4 | 8 | 16) {
            return Err(ModelError::InvalidMeter {
                numerator,
                denominator,
            });
        }
        Ok(Self {
            numerator,
            denominator,
        })
    }

    pub fn numerator(self) -> u8 {
        self.numerator
    }

    pub fn denominator(self) -> u8 {
        self.denominator
    }

    fn quarters_per_bar(self) -> f64 {
        f64::from(self.numerator) * 4.0 / f64::from(self.denominator)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Quarter,
    Eighth,
    Sixteenth,
    ThirtySecond,
}

impl Resolution {
    pub fn steps_per_quarter(self) -> u8 {
        match self {
            Self::Quarter => 1,
            Self::Eighth => 2,
            Self::Sixteenth => 4,
            Self::ThirtySecond => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transport {
    sample_rate: u32,
    tempo: Tempo,
    meter: Meter,
    bars: u16,
    resolution: Resolution,
    swing: f64,
    absolute_frame: Frame,
    playhead: Frame,
    playing: bool,
}

impl Transport {
    pub fn new(
        sample_rate: u32,
        tempo: Tempo,
        meter: Meter,
        bars: u16,
        resolution: Resolution,
    ) -> Result<Self, ModelError> {
        if sample_rate == 0 || !(1..=64).contains(&bars) {
            return Err(ModelError::InvalidTransport);
        }
        Ok(Self {
            sample_rate,
            tempo,
            meter,
            bars,
            resolution,
            swing: 0.5,
            absolute_frame: 0,
            playhead: 0,
            playing: false,
        })
    }

    pub fn with_swing(mut self, swing: f64) -> Result<Self, ModelError> {
        if !swing.is_finite() || !(0.50..=0.75).contains(&swing) {
            return Err(ModelError::SwingOutOfRange);
        }
        self.swing = swing;
        Ok(self)
    }

    pub fn step_frame(self, step: u32) -> Frame {
        let straight = self.straight_step_frames();
        let delay = if step % 2 == 1 {
            (2.0 * self.swing - 1.0) * straight
        } else {
            0.0
        };
        (f64::from(step).mul_add(straight, delay)).round() as Frame
    }

    pub fn loop_frames(self) -> Frame {
        let frames_per_quarter = f64::from(self.sample_rate) * 60.0 / self.tempo.bpm();
        (frames_per_quarter * self.meter.quarters_per_bar() * f64::from(self.bars)).round() as Frame
    }

    pub fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    pub fn tempo(self) -> Tempo {
        self.tempo
    }

    pub fn meter(self) -> Meter {
        self.meter
    }

    pub fn bars(self) -> u16 {
        self.bars
    }

    pub fn resolution(self) -> Resolution {
        self.resolution
    }

    pub fn swing(self) -> f64 {
        self.swing
    }

    pub fn step_count(self) -> u32 {
        (self.meter.quarters_per_bar()
            * f64::from(self.bars)
            * f64::from(self.resolution.steps_per_quarter()))
        .round() as u32
    }

    pub fn play(&mut self) {
        self.playing = true;
    }

    pub fn stop(&mut self) {
        self.playing = false;
    }

    pub fn seek(&mut self, frame: Frame) {
        self.absolute_frame = frame;
        self.playhead = frame % self.loop_frames();
    }

    pub fn advance_to(&mut self, frame: Frame) -> u64 {
        if !self.playing || frame < self.absolute_frame {
            return 0;
        }
        let loop_frames = self.loop_frames();
        let old_loop = self.absolute_frame / loop_frames;
        let new_loop = frame / loop_frames;
        self.absolute_frame = frame;
        self.playhead = frame % loop_frames;
        new_loop - old_loop
    }

    pub fn playhead(self) -> Frame {
        self.playhead
    }

    pub fn absolute_frame(self) -> Frame {
        self.absolute_frame
    }

    pub fn is_playing(self) -> bool {
        self.playing
    }

    fn straight_step_frames(self) -> f64 {
        f64::from(self.sample_rate) * 60.0
            / self.tempo.bpm()
            / f64::from(self.resolution.steps_per_quarter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelError;

    #[test]
    fn one_bar_of_sixteenth_notes_is_sample_accurate() {
        let transport = Transport::new(
            48_000,
            Tempo::new(120.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        assert_eq!(transport.step_frame(0), 0);
        assert_eq!(transport.step_frame(1), 6_000);
        assert_eq!(transport.loop_frames(), 96_000);
    }

    #[test]
    fn swing_delays_each_odd_step_without_changing_bar_length() {
        let straight = Transport::new(
            48_000,
            Tempo::new(120.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        let swung = straight.with_swing(0.60).unwrap();
        assert_eq!(swung.step_frame(1), 7_200);
        assert_eq!(swung.step_frame(2), 12_000);
        assert_eq!(swung.loop_frames(), straight.loop_frames());
    }

    #[test]
    fn advance_wraps_and_reports_crossed_loop_count() {
        let mut transport = Transport::new(
            100,
            Tempo::new(60.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Quarter,
        )
        .unwrap();
        transport.play();
        assert_eq!(transport.advance_to(450), 1);
        assert_eq!(transport.playhead(), 50);
        assert_eq!(transport.absolute_frame(), 450);
    }

    #[test]
    fn stopped_transport_does_not_advance_and_seek_is_loop_bounded() {
        let mut transport = Transport::new(
            100,
            Tempo::new(60.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Quarter,
        )
        .unwrap();
        assert_eq!(transport.advance_to(200), 0);
        assert_eq!(transport.playhead(), 0);
        transport.seek(450);
        assert_eq!(transport.playhead(), 50);
        transport.play();
        transport.stop();
        assert!(!transport.is_playing());
    }

    #[test]
    fn timing_inputs_are_validated() {
        assert_eq!(Tempo::new(f64::NAN), Err(ModelError::TempoOutOfRange));
        assert_eq!(
            Meter::new(4, 3),
            Err(ModelError::InvalidMeter {
                numerator: 4,
                denominator: 3
            })
        );
        let transport = Transport::new(
            48_000,
            Tempo::new(120.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        assert_eq!(transport.with_swing(0.49), Err(ModelError::SwingOutOfRange));
    }
}
