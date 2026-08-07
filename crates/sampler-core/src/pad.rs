use serde::{Deserialize, Serialize};

pub const BANK_COUNT: u8 = 10;
pub const PADS_PER_BANK: u8 = 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("bank {0} is outside 0..10")]
    BankOutOfRange(u8),
    #[error("pad {0} is outside 0..16")]
    PadOutOfRange(u8),
    #[error("choke group must be in 1..=16")]
    ChokeGroupOutOfRange,
    #[error("gain must be finite and in -60..=6 dB")]
    GainOutOfRange,
    #[error("pan must be finite and in -1..=1")]
    PanOutOfRange,
    #[error("pitch must be finite and in -24..=24 semitones")]
    PitchOutOfRange,
    #[error("tempo must be finite and in 20..=300 BPM")]
    TempoOutOfRange,
    #[error("unsupported meter {numerator}/{denominator}")]
    InvalidMeter { numerator: u8, denominator: u8 },
    #[error("sample rate and pattern length must be non-zero and bars in 1..=64")]
    InvalidTransport,
    #[error("swing must be finite and in 0.50..=0.75")]
    SwingOutOfRange,
    #[error("event is invalid or outside the pattern")]
    InvalidEvent,
    #[error("event id already exists")]
    DuplicateEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct BankId(u8);

impl BankId {
    pub fn new(value: u8) -> Result<Self, ModelError> {
        (value < BANK_COUNT)
            .then_some(Self(value))
            .ok_or(ModelError::BankOutOfRange(value))
    }
}

impl TryFrom<u8> for BankId {
    type Error = ModelError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<BankId> for u8 {
    fn from(value: BankId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PadId {
    bank: BankId,
    index: u8,
}

impl PadId {
    pub fn new(bank: BankId, index: u8) -> Result<Self, ModelError> {
        (index < PADS_PER_BANK)
            .then_some(Self { bank, index })
            .ok_or(ModelError::PadOutOfRange(index))
    }

    pub fn bank(self) -> BankId {
        self.bank
    }

    pub fn index(self) -> u8 {
        self.index
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackMode {
    Gate,
    OneShot,
    Loop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct ChokeGroup(u8);

impl ChokeGroup {
    pub fn new(value: u8) -> Result<Self, ModelError> {
        (1..=16)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(ModelError::ChokeGroupOutOfRange)
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for ChokeGroup {
    type Error = ModelError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ChokeGroup> for u8 {
    fn from(value: ChokeGroup) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PadSettings {
    pub mode: PlaybackMode,
    pub gain_db: f32,
    pub pan: f32,
    pub pitch_semitones: f32,
    pub choke_group: Option<ChokeGroup>,
}

impl PadSettings {
    pub fn new(
        mode: PlaybackMode,
        gain_db: f32,
        pan: f32,
        pitch_semitones: f32,
        choke_group: Option<ChokeGroup>,
    ) -> Result<Self, ModelError> {
        if !gain_db.is_finite() || !(-60.0..=6.0).contains(&gain_db) {
            return Err(ModelError::GainOutOfRange);
        }
        if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
            return Err(ModelError::PanOutOfRange);
        }
        if !pitch_semitones.is_finite() || !(-24.0..=24.0).contains(&pitch_semitones) {
            return Err(ModelError::PitchOutOfRange);
        }
        Ok(Self {
            mode,
            gain_db,
            pan,
            pitch_semitones,
            choke_group,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_ids_are_bounded_to_ten_banks_and_sixteen_pads() {
        assert_eq!(PadId::new(BankId::new(9).unwrap(), 15).unwrap().index(), 15);
        assert_eq!(BankId::new(10), Err(ModelError::BankOutOfRange(10)));
        assert_eq!(
            PadId::new(BankId::new(0).unwrap(), 16),
            Err(ModelError::PadOutOfRange(16))
        );
    }

    #[test]
    fn settings_reject_values_the_mixer_cannot_represent() {
        assert!(PadSettings::new(PlaybackMode::Gate, 6.0, 0.0, 0.0, None).is_ok());
        assert_eq!(
            PadSettings::new(PlaybackMode::OneShot, 6.1, 0.0, 0.0, None),
            Err(ModelError::GainOutOfRange)
        );
        assert_eq!(
            PadSettings::new(PlaybackMode::Loop, 0.0, -1.1, 0.0, None),
            Err(ModelError::PanOutOfRange)
        );
    }

    #[test]
    fn ids_expose_validated_values_and_choke_groups_are_one_based() {
        let bank = BankId::new(2).unwrap();
        let pad = PadId::new(bank, 7).unwrap();
        assert_eq!(u8::from(bank), 2);
        assert_eq!(pad.bank(), bank);
        assert_eq!(pad.index(), 7);
        assert_eq!(ChokeGroup::new(1).unwrap().get(), 1);
        assert_eq!(ChokeGroup::new(0), Err(ModelError::ChokeGroupOutOfRange));
    }

    #[test]
    fn settings_reject_non_finite_values() {
        assert_eq!(
            PadSettings::new(PlaybackMode::Gate, f32::NAN, 0.0, 0.0, None),
            Err(ModelError::GainOutOfRange)
        );
        assert_eq!(
            PadSettings::new(PlaybackMode::Gate, 0.0, f32::INFINITY, 0.0, None),
            Err(ModelError::PanOutOfRange)
        );
        assert_eq!(
            PadSettings::new(PlaybackMode::Gate, 0.0, 0.0, -24.1, None),
            Err(ModelError::PitchOutOfRange)
        );
    }
}
