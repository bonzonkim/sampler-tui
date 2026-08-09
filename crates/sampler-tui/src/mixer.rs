use sampler_core::{
    ChokeGroup, DelaySettings, MasterMixSettings, PadId, PadMixSettings, PadSettings,
    ReverbSettings,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerSection {
    Pad,
    Master,
    Delay,
    Reverb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadField {
    Level,
    Pan,
    Mute,
    Choke,
    DelaySend,
    ReverbSend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterField {
    Level,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayField {
    Enabled,
    Time,
    Feedback,
    Return,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReverbField {
    Enabled,
    Room,
    Damping,
    Return,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerAction {
    PreviousSection,
    NextSection,
    PreviousField,
    NextField,
    Decrement,
    Increment,
    Activate,
    Reset,
    ReturnToPerform,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixerContext {
    pub pad: PadId,
    pub pad_settings: PadSettings,
    pub pad_mix: PadMixSettings,
    pub master_mix: MasterMixSettings,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MixerIntent {
    UpdatePadSettings {
        pad: PadId,
        settings: PadSettings,
    },
    UpdatePadMix {
        pad: PadId,
        settings: PadMixSettings,
    },
    UpdateMasterMix(MasterMixSettings),
    ReturnToPerform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixerCursor {
    section: MixerSection,
    pad_field: PadField,
    master_field: MasterField,
    delay_field: DelayField,
    reverb_field: ReverbField,
}

impl Default for MixerCursor {
    fn default() -> Self {
        Self {
            section: MixerSection::Pad,
            pad_field: PadField::Level,
            master_field: MasterField::Level,
            delay_field: DelayField::Enabled,
            reverb_field: ReverbField::Enabled,
        }
    }
}

impl MixerCursor {
    pub const fn section(&self) -> MixerSection {
        self.section
    }

    pub const fn pad_field(&self) -> PadField {
        self.pad_field
    }

    pub const fn master_field(&self) -> MasterField {
        self.master_field
    }

    pub const fn delay_field(&self) -> DelayField {
        self.delay_field
    }

    pub const fn reverb_field(&self) -> ReverbField {
        self.reverb_field
    }

    pub fn reduce(&mut self, action: MixerAction, context: MixerContext) -> Option<MixerIntent> {
        match action {
            MixerAction::PreviousSection => {
                self.section = match self.section {
                    MixerSection::Pad => MixerSection::Reverb,
                    MixerSection::Master => MixerSection::Pad,
                    MixerSection::Delay => MixerSection::Master,
                    MixerSection::Reverb => MixerSection::Delay,
                };
                None
            }
            MixerAction::NextSection => {
                self.section = match self.section {
                    MixerSection::Pad => MixerSection::Master,
                    MixerSection::Master => MixerSection::Delay,
                    MixerSection::Delay => MixerSection::Reverb,
                    MixerSection::Reverb => MixerSection::Pad,
                };
                None
            }
            MixerAction::PreviousField => {
                self.move_field(-1);
                None
            }
            MixerAction::NextField => {
                self.move_field(1);
                None
            }
            MixerAction::Decrement => self.edit(context, -1),
            MixerAction::Increment => self.edit(context, 1),
            MixerAction::Activate => self.activate(context),
            MixerAction::Reset => self.reset(context),
            MixerAction::ReturnToPerform => Some(MixerIntent::ReturnToPerform),
        }
    }

    fn move_field(&mut self, delta: i8) {
        match self.section {
            MixerSection::Pad => {
                self.pad_field = match (self.pad_field, delta.is_positive()) {
                    (PadField::Level, true) => PadField::Pan,
                    (PadField::Pan, true) => PadField::Mute,
                    (PadField::Mute, true) => PadField::Choke,
                    (PadField::Choke, true) => PadField::DelaySend,
                    (PadField::DelaySend, true) => PadField::ReverbSend,
                    (PadField::ReverbSend, true) => PadField::ReverbSend,
                    (PadField::Level, false) => PadField::Level,
                    (PadField::Pan, false) => PadField::Level,
                    (PadField::Mute, false) => PadField::Pan,
                    (PadField::Choke, false) => PadField::Mute,
                    (PadField::DelaySend, false) => PadField::Choke,
                    (PadField::ReverbSend, false) => PadField::DelaySend,
                };
            }
            MixerSection::Master => {}
            MixerSection::Delay => {
                self.delay_field = match (self.delay_field, delta.is_positive()) {
                    (DelayField::Enabled, true) => DelayField::Time,
                    (DelayField::Time, true) => DelayField::Feedback,
                    (DelayField::Feedback, true) => DelayField::Return,
                    (DelayField::Return, true) => DelayField::Return,
                    (DelayField::Enabled, false) => DelayField::Enabled,
                    (DelayField::Time, false) => DelayField::Enabled,
                    (DelayField::Feedback, false) => DelayField::Time,
                    (DelayField::Return, false) => DelayField::Feedback,
                };
            }
            MixerSection::Reverb => {
                self.reverb_field = match (self.reverb_field, delta.is_positive()) {
                    (ReverbField::Enabled, true) => ReverbField::Room,
                    (ReverbField::Room, true) => ReverbField::Damping,
                    (ReverbField::Damping, true) => ReverbField::Return,
                    (ReverbField::Return, true) => ReverbField::Return,
                    (ReverbField::Enabled, false) => ReverbField::Enabled,
                    (ReverbField::Room, false) => ReverbField::Enabled,
                    (ReverbField::Damping, false) => ReverbField::Room,
                    (ReverbField::Return, false) => ReverbField::Damping,
                };
            }
        }
    }

    fn edit(&self, context: MixerContext, direction: i8) -> Option<MixerIntent> {
        match self.section {
            MixerSection::Pad => self.edit_pad(context, direction),
            MixerSection::Master => {
                let mut master = context.master_mix;
                master.gain_db = stepped(master.gain_db, direction, 1.0, -60.0, 6.0);
                Some(MixerIntent::UpdateMasterMix(valid_master(master)))
            }
            MixerSection::Delay => {
                let mut master = context.master_mix;
                match self.delay_field {
                    DelayField::Enabled => master.delay.enabled = !master.delay.enabled,
                    DelayField::Time => {
                        master.delay.time_ms =
                            stepped_u16(master.delay.time_ms, direction, 10, 10, 2_000);
                    }
                    DelayField::Feedback => {
                        master.delay.feedback =
                            stepped(master.delay.feedback, direction, 0.05, 0.0, 0.95);
                    }
                    DelayField::Return => {
                        master.delay.return_db =
                            stepped(master.delay.return_db, direction, 1.0, -60.0, 6.0);
                    }
                }
                Some(MixerIntent::UpdateMasterMix(valid_master(master)))
            }
            MixerSection::Reverb => {
                let mut master = context.master_mix;
                match self.reverb_field {
                    ReverbField::Enabled => master.reverb.enabled = !master.reverb.enabled,
                    ReverbField::Room => {
                        master.reverb.room_size =
                            stepped(master.reverb.room_size, direction, 0.05, 0.0, 1.0);
                    }
                    ReverbField::Damping => {
                        master.reverb.damping =
                            stepped(master.reverb.damping, direction, 0.05, 0.0, 1.0);
                    }
                    ReverbField::Return => {
                        master.reverb.return_db =
                            stepped(master.reverb.return_db, direction, 1.0, -60.0, 6.0);
                    }
                }
                Some(MixerIntent::UpdateMasterMix(valid_master(master)))
            }
        }
    }

    fn edit_pad(&self, context: MixerContext, direction: i8) -> Option<MixerIntent> {
        match self.pad_field {
            PadField::Level => {
                let mut settings = context.pad_settings;
                settings.gain_db = stepped(settings.gain_db, direction, 1.0, -60.0, 6.0);
                Some(MixerIntent::UpdatePadSettings {
                    pad: context.pad,
                    settings: valid_pad(settings),
                })
            }
            PadField::Pan => {
                let mut settings = context.pad_settings;
                settings.pan = stepped(settings.pan, direction, 0.05, -1.0, 1.0);
                Some(MixerIntent::UpdatePadSettings {
                    pad: context.pad,
                    settings: valid_pad(settings),
                })
            }
            PadField::Mute => {
                let mut mix = context.pad_mix;
                mix.muted = !mix.muted;
                Some(MixerIntent::UpdatePadMix {
                    pad: context.pad,
                    settings: valid_pad_mix(mix),
                })
            }
            PadField::Choke => {
                let mut settings = context.pad_settings;
                settings.choke_group = cycle_choke(settings.choke_group, direction);
                Some(MixerIntent::UpdatePadSettings {
                    pad: context.pad,
                    settings: valid_pad(settings),
                })
            }
            PadField::DelaySend => {
                let mut mix = context.pad_mix;
                mix.delay_send = stepped(mix.delay_send, direction, 0.05, 0.0, 1.0);
                Some(MixerIntent::UpdatePadMix {
                    pad: context.pad,
                    settings: valid_pad_mix(mix),
                })
            }
            PadField::ReverbSend => {
                let mut mix = context.pad_mix;
                mix.reverb_send = stepped(mix.reverb_send, direction, 0.05, 0.0, 1.0);
                Some(MixerIntent::UpdatePadMix {
                    pad: context.pad,
                    settings: valid_pad_mix(mix),
                })
            }
        }
    }

    fn activate(&self, context: MixerContext) -> Option<MixerIntent> {
        match (
            self.section,
            self.pad_field,
            self.delay_field,
            self.reverb_field,
        ) {
            (MixerSection::Pad, PadField::Mute, _, _) => self.edit(context, 1),
            (MixerSection::Pad, PadField::Choke, _, _) => self.edit(context, 1),
            (MixerSection::Delay, _, DelayField::Enabled, _) => self.edit(context, 1),
            (MixerSection::Reverb, _, _, ReverbField::Enabled) => self.edit(context, 1),
            _ => None,
        }
    }

    fn reset(&self, context: MixerContext) -> Option<MixerIntent> {
        match self.section {
            MixerSection::Pad => match self.pad_field {
                PadField::Level | PadField::Pan | PadField::Choke => {
                    let defaults = PadSettings::default();
                    let mut settings = context.pad_settings;
                    match self.pad_field {
                        PadField::Level => settings.gain_db = defaults.gain_db,
                        PadField::Pan => settings.pan = defaults.pan,
                        PadField::Choke => settings.choke_group = defaults.choke_group,
                        _ => unreachable!(),
                    }
                    Some(MixerIntent::UpdatePadSettings {
                        pad: context.pad,
                        settings: valid_pad(settings),
                    })
                }
                PadField::Mute | PadField::DelaySend | PadField::ReverbSend => {
                    let defaults = PadMixSettings::default();
                    let mut mix = context.pad_mix;
                    match self.pad_field {
                        PadField::Mute => mix.muted = defaults.muted,
                        PadField::DelaySend => mix.delay_send = defaults.delay_send,
                        PadField::ReverbSend => mix.reverb_send = defaults.reverb_send,
                        _ => unreachable!(),
                    }
                    Some(MixerIntent::UpdatePadMix {
                        pad: context.pad,
                        settings: valid_pad_mix(mix),
                    })
                }
            },
            MixerSection::Master => {
                let mut master = context.master_mix;
                master.gain_db = MasterMixSettings::default().gain_db;
                Some(MixerIntent::UpdateMasterMix(valid_master(master)))
            }
            MixerSection::Delay => {
                let defaults = DelaySettings::default();
                let mut master = context.master_mix;
                match self.delay_field {
                    DelayField::Enabled => master.delay.enabled = defaults.enabled,
                    DelayField::Time => master.delay.time_ms = defaults.time_ms,
                    DelayField::Feedback => master.delay.feedback = defaults.feedback,
                    DelayField::Return => master.delay.return_db = defaults.return_db,
                }
                Some(MixerIntent::UpdateMasterMix(valid_master(master)))
            }
            MixerSection::Reverb => {
                let defaults = ReverbSettings::default();
                let mut master = context.master_mix;
                match self.reverb_field {
                    ReverbField::Enabled => master.reverb.enabled = defaults.enabled,
                    ReverbField::Room => master.reverb.room_size = defaults.room_size,
                    ReverbField::Damping => master.reverb.damping = defaults.damping,
                    ReverbField::Return => master.reverb.return_db = defaults.return_db,
                }
                Some(MixerIntent::UpdateMasterMix(valid_master(master)))
            }
        }
    }
}

fn stepped(value: f32, direction: i8, step: f32, minimum: f32, maximum: f32) -> f32 {
    (value + f32::from(direction.signum()) * step).clamp(minimum, maximum)
}

fn stepped_u16(value: u16, direction: i8, step: u16, minimum: u16, maximum: u16) -> u16 {
    if direction.is_negative() {
        value.saturating_sub(step).max(minimum)
    } else {
        value.saturating_add(step).min(maximum)
    }
}

fn cycle_choke(value: Option<ChokeGroup>, direction: i8) -> Option<ChokeGroup> {
    let current = value.map_or(0, ChokeGroup::get);
    let next = if direction.is_negative() {
        match current {
            0 => 16,
            1 => 0,
            value => value - 1,
        }
    } else {
        match current {
            16 => 0,
            value => value + 1,
        }
    };
    (next != 0).then(|| ChokeGroup::new(next).expect("bounded choke group"))
}

fn valid_pad(settings: PadSettings) -> PadSettings {
    settings.validate().expect("mixer candidate is bounded");
    settings
}

fn valid_pad_mix(settings: PadMixSettings) -> PadMixSettings {
    settings.validate().expect("mixer candidate is bounded");
    settings
}

fn valid_master(settings: MasterMixSettings) -> MasterMixSettings {
    settings.validate().expect("mixer candidate is bounded");
    settings
}

#[cfg(test)]
mod tests {
    use sampler_core::{
        BankId, ChokeGroup, DelaySettings, MasterMixSettings, PadId, PadMixSettings, PadSettings,
        PlaybackMode, ReverbSettings,
    };

    use super::{
        DelayField, MixerAction, MixerContext, MixerCursor, MixerIntent, MixerSection, PadField,
        ReverbField,
    };

    fn pad() -> PadId {
        PadId::new(BankId::new(0).unwrap(), 3).unwrap()
    }

    fn context() -> MixerContext {
        MixerContext {
            pad: pad(),
            pad_settings: PadSettings::default(),
            pad_mix: PadMixSettings::default(),
            master_mix: MasterMixSettings::default(),
        }
    }

    fn apply(cursor: &mut MixerCursor, action: MixerAction) -> Option<MixerIntent> {
        cursor.reduce(action, context())
    }

    #[test]
    fn sections_wrap_both_directions_and_fields_clamp_at_exact_bounds() {
        let mut cursor = MixerCursor::default();
        assert_eq!(cursor.section(), MixerSection::Pad);
        apply(&mut cursor, MixerAction::PreviousSection);
        assert_eq!(cursor.section(), MixerSection::Reverb);
        apply(&mut cursor, MixerAction::NextSection);
        assert_eq!(cursor.section(), MixerSection::Pad);
        for expected in [
            MixerSection::Master,
            MixerSection::Delay,
            MixerSection::Reverb,
            MixerSection::Pad,
        ] {
            apply(&mut cursor, MixerAction::NextSection);
            assert_eq!(cursor.section(), expected);
        }

        for _ in 0..12 {
            apply(&mut cursor, MixerAction::NextField);
        }
        assert_eq!(cursor.pad_field(), PadField::ReverbSend);
        for _ in 0..12 {
            apply(&mut cursor, MixerAction::PreviousField);
        }
        assert_eq!(cursor.pad_field(), PadField::Level);
    }

    #[test]
    fn pad_fields_emit_literal_steps_toggles_and_choke_cycles() {
        let mut cursor = MixerCursor::default();
        assert_eq!(
            apply(&mut cursor, MixerAction::Increment),
            Some(MixerIntent::UpdatePadSettings {
                pad: pad(),
                settings: PadSettings::new(PlaybackMode::OneShot, 1.0, 0.0, 0.0, None).unwrap(),
            })
        );

        apply(&mut cursor, MixerAction::NextField);
        assert_eq!(
            apply(&mut cursor, MixerAction::Decrement),
            Some(MixerIntent::UpdatePadSettings {
                pad: pad(),
                settings: PadSettings::new(PlaybackMode::OneShot, 0.0, -0.05, 0.0, None).unwrap(),
            })
        );

        apply(&mut cursor, MixerAction::NextField);
        assert_eq!(
            apply(&mut cursor, MixerAction::Activate),
            Some(MixerIntent::UpdatePadMix {
                pad: pad(),
                settings: PadMixSettings::new(true, 0.0, 0.0).unwrap(),
            })
        );

        apply(&mut cursor, MixerAction::NextField);
        assert_eq!(
            apply(&mut cursor, MixerAction::Activate),
            Some(MixerIntent::UpdatePadSettings {
                pad: pad(),
                settings: PadSettings::new(
                    PlaybackMode::OneShot,
                    0.0,
                    0.0,
                    0.0,
                    Some(ChokeGroup::new(1).unwrap()),
                )
                .unwrap(),
            })
        );
        let mut last = context();
        last.pad_settings.choke_group = Some(ChokeGroup::new(16).unwrap());
        assert_eq!(
            cursor.reduce(MixerAction::Activate, last),
            Some(MixerIntent::UpdatePadSettings {
                pad: pad(),
                settings: PadSettings::default(),
            })
        );
        assert_eq!(
            apply(&mut cursor, MixerAction::Decrement),
            Some(MixerIntent::UpdatePadSettings {
                pad: pad(),
                settings: PadSettings::new(
                    PlaybackMode::OneShot,
                    0.0,
                    0.0,
                    0.0,
                    Some(ChokeGroup::new(16).unwrap()),
                )
                .unwrap(),
            })
        );

        apply(&mut cursor, MixerAction::NextField);
        assert_eq!(
            apply(&mut cursor, MixerAction::Increment),
            Some(MixerIntent::UpdatePadMix {
                pad: pad(),
                settings: PadMixSettings::new(false, 0.05, 0.0).unwrap(),
            })
        );
        apply(&mut cursor, MixerAction::NextField);
        assert_eq!(
            apply(&mut cursor, MixerAction::Increment),
            Some(MixerIntent::UpdatePadMix {
                pad: pad(),
                settings: PadMixSettings::new(false, 0.0, 0.05).unwrap(),
            })
        );
    }

    #[test]
    fn master_delay_and_reverb_fields_use_exact_literal_steps() {
        let mut cursor = MixerCursor::default();
        apply(&mut cursor, MixerAction::NextSection);
        assert_eq!(
            apply(&mut cursor, MixerAction::Decrement),
            Some(MixerIntent::UpdateMasterMix(
                MasterMixSettings::new(-1.0, DelaySettings::default(), ReverbSettings::default())
                    .unwrap()
            ))
        );

        apply(&mut cursor, MixerAction::NextSection);
        assert_eq!(cursor.delay_field(), DelayField::Enabled);
        assert_eq!(
            apply(&mut cursor, MixerAction::Activate),
            Some(MixerIntent::UpdateMasterMix(
                MasterMixSettings::new(
                    0.0,
                    DelaySettings::new(true, 250, 0.35, -12.0).unwrap(),
                    ReverbSettings::default(),
                )
                .unwrap()
            ))
        );
        apply(&mut cursor, MixerAction::NextField);
        assert_eq!(
            apply(&mut cursor, MixerAction::Increment),
            Some(MixerIntent::UpdateMasterMix(
                MasterMixSettings::new(
                    0.0,
                    DelaySettings::new(false, 260, 0.35, -12.0).unwrap(),
                    ReverbSettings::default(),
                )
                .unwrap()
            ))
        );
        apply(&mut cursor, MixerAction::NextField);
        let feedback = apply(&mut cursor, MixerAction::Increment).unwrap();
        let MixerIntent::UpdateMasterMix(feedback) = feedback else {
            panic!("feedback must update master mix")
        };
        assert_eq!(feedback.delay.feedback.to_bits(), 0.4_f32.to_bits());
        apply(&mut cursor, MixerAction::NextField);
        assert_eq!(
            apply(&mut cursor, MixerAction::Increment),
            Some(MixerIntent::UpdateMasterMix(
                MasterMixSettings::new(
                    0.0,
                    DelaySettings::new(false, 250, 0.35, -11.0).unwrap(),
                    ReverbSettings::default(),
                )
                .unwrap()
            ))
        );

        apply(&mut cursor, MixerAction::NextSection);
        assert_eq!(cursor.reverb_field(), ReverbField::Enabled);
        assert!(matches!(
            apply(&mut cursor, MixerAction::Activate),
            Some(MixerIntent::UpdateMasterMix(settings)) if settings.reverb.enabled
        ));
        apply(&mut cursor, MixerAction::NextField);
        let room = apply(&mut cursor, MixerAction::Increment).unwrap();
        let MixerIntent::UpdateMasterMix(room) = room else {
            panic!("room must update master mix")
        };
        assert_eq!(room.reverb.room_size.to_bits(), 0.55_f32.to_bits());
        apply(&mut cursor, MixerAction::NextField);
        let damping = apply(&mut cursor, MixerAction::Decrement).unwrap();
        let MixerIntent::UpdateMasterMix(damping) = damping else {
            panic!("damping must update master mix")
        };
        assert_eq!(damping.reverb.damping.to_bits(), 0.45_f32.to_bits());
        apply(&mut cursor, MixerAction::NextField);
        assert!(matches!(
            apply(&mut cursor, MixerAction::Increment),
            Some(MixerIntent::UpdateMasterMix(settings)) if settings.reverb.return_db == -11.0
        ));
    }

    #[test]
    fn reset_restores_only_the_focused_documented_default_and_escape_returns() {
        let mut cursor = MixerCursor::default();
        let mut values = context();
        values.pad_settings = PadSettings::new(PlaybackMode::Loop, -9.0, 0.4, 7.0, None).unwrap();
        assert_eq!(
            cursor.reduce(MixerAction::Reset, values),
            Some(MixerIntent::UpdatePadSettings {
                pad: pad(),
                settings: PadSettings::new(PlaybackMode::Loop, 0.0, 0.4, 7.0, None).unwrap(),
            })
        );
        assert_eq!(
            apply(&mut cursor, MixerAction::ReturnToPerform),
            Some(MixerIntent::ReturnToPerform)
        );
    }

    #[test]
    fn numeric_candidates_clamp_at_every_domain_boundary() {
        let mut cursor = MixerCursor::default();
        let mut values = context();
        values.pad_settings.gain_db = 6.0;
        assert!(matches!(
            cursor.reduce(MixerAction::Increment, values),
            Some(MixerIntent::UpdatePadSettings { settings, .. }) if settings.gain_db == 6.0
        ));
        values.pad_settings.gain_db = -60.0;
        assert!(matches!(
            cursor.reduce(MixerAction::Decrement, values),
            Some(MixerIntent::UpdatePadSettings { settings, .. }) if settings.gain_db == -60.0
        ));
    }
}
