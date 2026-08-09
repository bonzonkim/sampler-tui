use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

use crate::ModelError;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PadMixSettings {
    pub muted: bool,
    pub delay_send: f32,
    pub reverb_send: f32,
}

#[derive(Deserialize)]
struct RawPadMixSettings {
    muted: bool,
    delay_send: f32,
    reverb_send: f32,
}

impl TryFrom<RawPadMixSettings> for PadMixSettings {
    type Error = ModelError;

    fn try_from(raw: RawPadMixSettings) -> Result<Self, Self::Error> {
        Self::new(raw.muted, raw.delay_send, raw.reverb_send)
    }
}

impl<'de> Deserialize<'de> for PadMixSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        RawPadMixSettings::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DelaySettings {
    pub enabled: bool,
    pub time_ms: u16,
    pub feedback: f32,
    pub return_db: f32,
}

#[derive(Deserialize)]
struct RawDelaySettings {
    enabled: bool,
    time_ms: u16,
    feedback: f32,
    return_db: f32,
}

impl TryFrom<RawDelaySettings> for DelaySettings {
    type Error = ModelError;

    fn try_from(raw: RawDelaySettings) -> Result<Self, Self::Error> {
        Self::new(raw.enabled, raw.time_ms, raw.feedback, raw.return_db)
    }
}

impl<'de> Deserialize<'de> for DelaySettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        RawDelaySettings::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ReverbSettings {
    pub enabled: bool,
    pub room_size: f32,
    pub damping: f32,
    pub return_db: f32,
}

#[derive(Deserialize)]
struct RawReverbSettings {
    enabled: bool,
    room_size: f32,
    damping: f32,
    return_db: f32,
}

impl TryFrom<RawReverbSettings> for ReverbSettings {
    type Error = ModelError;

    fn try_from(raw: RawReverbSettings) -> Result<Self, Self::Error> {
        Self::new(raw.enabled, raw.room_size, raw.damping, raw.return_db)
    }
}

impl<'de> Deserialize<'de> for ReverbSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        RawReverbSettings::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MasterMixSettings {
    pub gain_db: f32,
    pub delay: DelaySettings,
    pub reverb: ReverbSettings,
}

#[derive(Deserialize)]
struct RawMasterMixSettings {
    gain_db: f32,
    delay: DelaySettings,
    reverb: ReverbSettings,
}

impl TryFrom<RawMasterMixSettings> for MasterMixSettings {
    type Error = ModelError;

    fn try_from(raw: RawMasterMixSettings) -> Result<Self, Self::Error> {
        Self::new(raw.gain_db, raw.delay, raw.reverb)
    }
}

impl<'de> Deserialize<'de> for MasterMixSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        RawMasterMixSettings::deserialize(deserializer)?
            .try_into()
            .map_err(D::Error::custom)
    }
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
        assert_eq!(master.delay.time_ms, 250);
        assert_eq!(master.delay.feedback, 0.35);
        assert_eq!(master.delay.return_db, -12.0);
        assert!(!master.reverb.enabled);
        assert_eq!(master.reverb.room_size, 0.5);
        assert_eq!(master.reverb.damping, 0.5);
        assert_eq!(master.reverb.return_db, -12.0);
    }

    #[test]
    fn pad_mix_settings_cover_each_send_boundary() {
        for value in [0.0, 1.0] {
            assert!(PadMixSettings::new(false, value, 0.5).is_ok());
            assert!(PadMixSettings::new(false, 0.5, value).is_ok());
        }
        for value in [
            -f32::EPSILON,
            1.0 + f32::EPSILON,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert_eq!(
                PadMixSettings::new(false, value, 0.5),
                Err(ModelError::SendOutOfRange)
            );
            assert_eq!(
                PadMixSettings::new(false, 0.5, value),
                Err(ModelError::SendOutOfRange)
            );
        }
    }

    #[test]
    fn delay_settings_cover_time_feedback_and_return_boundaries() {
        for time_ms in [10, 2_000] {
            assert!(DelaySettings::new(true, time_ms, 0.5, 0.0).is_ok());
        }
        for time_ms in [9, 2_001] {
            assert_eq!(
                DelaySettings::new(true, time_ms, 0.5, 0.0),
                Err(ModelError::DelayTimeOutOfRange)
            );
        }

        for feedback in [0.0, 0.95] {
            assert!(DelaySettings::new(true, 250, feedback, 0.0).is_ok());
        }
        for feedback in [
            -f32::EPSILON,
            0.95 + f32::EPSILON,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert_eq!(
                DelaySettings::new(true, 250, feedback, 0.0),
                Err(ModelError::FeedbackOutOfRange)
            );
        }

        for return_db in [-60.0, 6.0] {
            assert!(DelaySettings::new(true, 250, 0.5, return_db).is_ok());
        }
        for return_db in [-60.01, 6.01, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                DelaySettings::new(true, 250, 0.5, return_db),
                Err(ModelError::ReturnGainOutOfRange)
            );
        }
    }

    #[test]
    fn reverb_settings_cover_room_damping_and_return_boundaries() {
        for value in [0.0, 1.0] {
            assert!(ReverbSettings::new(true, value, 0.5, 0.0).is_ok());
            assert!(ReverbSettings::new(true, 0.5, value, 0.0).is_ok());
        }
        for value in [
            -f32::EPSILON,
            1.0 + f32::EPSILON,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert_eq!(
                ReverbSettings::new(true, value, 0.5, 0.0),
                Err(ModelError::EffectParameterOutOfRange)
            );
            assert_eq!(
                ReverbSettings::new(true, 0.5, value, 0.0),
                Err(ModelError::EffectParameterOutOfRange)
            );
        }

        for return_db in [-60.0, 6.0] {
            assert!(ReverbSettings::new(true, 0.5, 0.5, return_db).is_ok());
        }
        for return_db in [-60.01, 6.01, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                ReverbSettings::new(true, 0.5, 0.5, return_db),
                Err(ModelError::ReturnGainOutOfRange)
            );
        }
    }

    #[test]
    fn master_settings_cover_gain_boundaries_and_nested_errors() {
        for gain_db in [-60.0, 6.0] {
            assert!(
                MasterMixSettings::new(
                    gain_db,
                    DelaySettings::default(),
                    ReverbSettings::default()
                )
                .is_ok()
            );
        }
        for gain_db in [-60.01, 6.01, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                MasterMixSettings::new(
                    gain_db,
                    DelaySettings::default(),
                    ReverbSettings::default()
                ),
                Err(ModelError::MasterGainOutOfRange)
            );
        }

        let invalid_delay_time = DelaySettings {
            time_ms: 9,
            ..DelaySettings::default()
        };
        assert_eq!(
            MasterMixSettings::new(0.0, invalid_delay_time, ReverbSettings::default()),
            Err(ModelError::DelayTimeOutOfRange)
        );
        let invalid_feedback = DelaySettings {
            feedback: f32::NAN,
            ..DelaySettings::default()
        };
        assert_eq!(
            MasterMixSettings::new(0.0, invalid_feedback, ReverbSettings::default()),
            Err(ModelError::FeedbackOutOfRange)
        );
        let invalid_delay_return = DelaySettings {
            return_db: 6.01,
            ..DelaySettings::default()
        };
        assert_eq!(
            MasterMixSettings::new(0.0, invalid_delay_return, ReverbSettings::default()),
            Err(ModelError::ReturnGainOutOfRange)
        );
        let invalid_room_size = ReverbSettings {
            room_size: f32::INFINITY,
            ..ReverbSettings::default()
        };
        assert_eq!(
            MasterMixSettings::new(0.0, DelaySettings::default(), invalid_room_size),
            Err(ModelError::EffectParameterOutOfRange)
        );
        let invalid_reverb_return = ReverbSettings {
            return_db: -60.01,
            ..ReverbSettings::default()
        };
        assert_eq!(
            MasterMixSettings::new(0.0, DelaySettings::default(), invalid_reverb_return),
            Err(ModelError::ReturnGainOutOfRange)
        );
    }

    #[test]
    fn mixer_validate_rejects_invalid_public_struct_literals() {
        assert_eq!(
            PadMixSettings {
                muted: false,
                delay_send: 0.0,
                reverb_send: f32::INFINITY,
            }
            .validate(),
            Err(ModelError::SendOutOfRange)
        );
        assert_eq!(
            DelaySettings {
                enabled: true,
                time_ms: 250,
                feedback: f32::NAN,
                return_db: -12.0,
            }
            .validate(),
            Err(ModelError::FeedbackOutOfRange)
        );
        assert_eq!(
            ReverbSettings {
                enabled: true,
                room_size: 0.5,
                damping: f32::NEG_INFINITY,
                return_db: -12.0,
            }
            .validate(),
            Err(ModelError::EffectParameterOutOfRange)
        );
        assert_eq!(
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
            .validate(),
            Err(ModelError::ReturnGainOutOfRange)
        );
    }

    #[test]
    fn mixer_deserialization_rejects_invalid_top_level_values() {
        assert_toml_rejects::<PadMixSettings>(
            "muted = false\ndelay_send = nan\nreverb_send = 0.0\n",
            "send must be finite and in 0..=1",
        );
        assert_toml_rejects::<DelaySettings>(
            "enabled = true\ntime_ms = 250\nfeedback = +inf\nreturn_db = -12.0\n",
            "delay feedback must be finite and in 0..=0.95",
        );
        assert_toml_rejects::<ReverbSettings>(
            "enabled = true\nroom_size = 1.01\ndamping = 0.5\nreturn_db = -12.0\n",
            "effect parameter must be finite and in 0..=1",
        );
        assert_toml_rejects::<MasterMixSettings>(
            "gain_db = 6.01\n[delay]\nenabled = false\ntime_ms = 250\nfeedback = 0.35\nreturn_db = -12.0\n[reverb]\nenabled = false\nroom_size = 0.5\ndamping = 0.5\nreturn_db = -12.0\n",
            "master gain must be finite and in -60..=6 dB",
        );
    }

    #[test]
    fn master_deserialization_rejects_invalid_nested_values() {
        assert_toml_rejects::<MasterMixSettings>(
            "gain_db = 0.0\n[delay]\nenabled = false\ntime_ms = 9\nfeedback = 0.35\nreturn_db = -12.0\n[reverb]\nenabled = false\nroom_size = 0.5\ndamping = 0.5\nreturn_db = -12.0\n",
            "delay time must be in 10..=2000 ms",
        );
        assert_toml_rejects::<MasterMixSettings>(
            "gain_db = 0.0\n[delay]\nenabled = false\ntime_ms = 250\nfeedback = 0.35\nreturn_db = -12.0\n[reverb]\nenabled = false\nroom_size = 0.5\ndamping = -inf\nreturn_db = -12.0\n",
            "effect parameter must be finite and in 0..=1",
        );
    }

    fn assert_toml_rejects<T>(document: &str, expected_message: &str)
    where
        T: serde::de::DeserializeOwned,
    {
        let error = match toml::from_str::<T>(document) {
            Ok(_) => panic!("invalid mixer settings were deserialized"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains(expected_message),
            "unexpected deserialize error: {error}"
        );
    }
}
