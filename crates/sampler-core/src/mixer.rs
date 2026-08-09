use serde::{Deserialize, Serialize};

use crate::ModelError;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PadMixSettings {
    pub muted: bool,
    pub delay_send: f32,
    pub reverb_send: f32,
}

impl PadMixSettings {
    pub fn new(muted: bool, delay_send: f32, reverb_send: f32) -> Result<Self, ModelError> {
        if !is_send(delay_send) || !is_send(reverb_send) {
            return Err(ModelError::SendOutOfRange);
        }
        Ok(Self {
            muted,
            delay_send,
            reverb_send,
        })
    }

    /// Revalidates settings assembled through deserialization or a public struct literal.
    pub fn validate(self) -> Result<(), ModelError> {
        Self::new(self.muted, self.delay_send, self.reverb_send).map(|_| ())
    }
}

impl Default for PadMixSettings {
    fn default() -> Self {
        Self {
            muted: false,
            delay_send: 0.0,
            reverb_send: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DelaySettings {
    pub enabled: bool,
    pub time_ms: u16,
    pub feedback: f32,
    pub return_db: f32,
}

impl DelaySettings {
    pub fn new(
        enabled: bool,
        time_ms: u16,
        feedback: f32,
        return_db: f32,
    ) -> Result<Self, ModelError> {
        if !(10..=2_000).contains(&time_ms) {
            return Err(ModelError::DelayTimeOutOfRange);
        }
        if !feedback.is_finite() || !(0.0..=0.95).contains(&feedback) {
            return Err(ModelError::FeedbackOutOfRange);
        }
        if !is_gain(return_db) {
            return Err(ModelError::ReturnGainOutOfRange);
        }
        Ok(Self {
            enabled,
            time_ms,
            feedback,
            return_db,
        })
    }

    /// Revalidates settings assembled through deserialization or a public struct literal.
    pub fn validate(self) -> Result<(), ModelError> {
        Self::new(self.enabled, self.time_ms, self.feedback, self.return_db).map(|_| ())
    }
}

impl Default for DelaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            time_ms: 250,
            feedback: 0.35,
            return_db: -12.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReverbSettings {
    pub enabled: bool,
    pub room_size: f32,
    pub damping: f32,
    pub return_db: f32,
}

impl ReverbSettings {
    pub fn new(
        enabled: bool,
        room_size: f32,
        damping: f32,
        return_db: f32,
    ) -> Result<Self, ModelError> {
        if !is_effect_parameter(room_size) || !is_effect_parameter(damping) {
            return Err(ModelError::EffectParameterOutOfRange);
        }
        if !is_gain(return_db) {
            return Err(ModelError::ReturnGainOutOfRange);
        }
        Ok(Self {
            enabled,
            room_size,
            damping,
            return_db,
        })
    }

    /// Revalidates settings assembled through deserialization or a public struct literal.
    pub fn validate(self) -> Result<(), ModelError> {
        Self::new(self.enabled, self.room_size, self.damping, self.return_db).map(|_| ())
    }
}

impl Default for ReverbSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            room_size: 0.5,
            damping: 0.5,
            return_db: -12.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MasterMixSettings {
    pub gain_db: f32,
    pub delay: DelaySettings,
    pub reverb: ReverbSettings,
}

impl MasterMixSettings {
    pub fn new(
        gain_db: f32,
        delay: DelaySettings,
        reverb: ReverbSettings,
    ) -> Result<Self, ModelError> {
        if !is_gain(gain_db) {
            return Err(ModelError::MasterGainOutOfRange);
        }
        delay.validate()?;
        reverb.validate()?;
        Ok(Self {
            gain_db,
            delay,
            reverb,
        })
    }

    /// Revalidates settings assembled through deserialization or a public struct literal.
    pub fn validate(self) -> Result<(), ModelError> {
        Self::new(self.gain_db, self.delay, self.reverb).map(|_| ())
    }
}

impl Default for MasterMixSettings {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            delay: DelaySettings::default(),
            reverb: ReverbSettings::default(),
        }
    }
}

fn is_send(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn is_effect_parameter(value: f32) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

fn is_gain(value: f32) -> bool {
    value.is_finite() && (-60.0..=6.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixer_defaults_are_dry_compatible_and_effects_are_disabled() {
        assert_eq!(
            PadMixSettings::default(),
            PadMixSettings::new(false, 0.0, 0.0).unwrap()
        );
        let master = MasterMixSettings::default();
        assert_eq!(master.gain_db, 0.0);
        assert!(!master.delay.enabled);
        assert!(!master.reverb.enabled);
    }

    #[test]
    fn mixer_settings_reject_each_nonfinite_and_out_of_range_boundary() {
        assert!(PadMixSettings::new(false, 1.0, 0.0).is_ok());
        assert!(PadMixSettings::new(false, 1.01, 0.0).is_err());
        assert!(PadMixSettings::new(false, f32::NAN, 0.0).is_err());
        assert!(DelaySettings::new(true, 10, 0.0, -60.0).is_ok());
        assert!(DelaySettings::new(true, 2_000, 0.95, 6.0).is_ok());
        assert!(DelaySettings::new(true, 2_001, 0.95, 6.0).is_err());
        assert!(ReverbSettings::new(true, 1.0, 1.0, 6.0).is_ok());
        assert!(ReverbSettings::new(true, 1.01, 0.5, 0.0).is_err());
    }

    #[test]
    fn mixer_validate_rejects_invalid_public_struct_literals() {
        assert!(
            PadMixSettings {
                muted: false,
                delay_send: 0.0,
                reverb_send: f32::INFINITY,
            }
            .validate()
            .is_err()
        );
        assert!(
            DelaySettings {
                enabled: true,
                time_ms: 250,
                feedback: f32::NAN,
                return_db: -12.0,
            }
            .validate()
            .is_err()
        );
        assert!(
            ReverbSettings {
                enabled: true,
                room_size: 0.5,
                damping: f32::NEG_INFINITY,
                return_db: -12.0,
            }
            .validate()
            .is_err()
        );
        assert!(
            MasterMixSettings {
                gain_db: 0.0,
                delay: DelaySettings::default(),
                reverb: ReverbSettings {
                    enabled: false,
                    room_size: 0.5,
                    damping: 0.5,
                    return_db: 6.1,
                },
            }
            .validate()
            .is_err()
        );
    }
}
