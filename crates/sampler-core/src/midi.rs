use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    BankId, ModelError,
    pad::{BANK_COUNT, PADS_PER_BANK},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct MidiNote(u8);

impl MidiNote {
    pub fn new(value: u8) -> Result<Self, ModelError> {
        (value <= 127)
            .then_some(Self(value))
            .ok_or(ModelError::MidiNoteOutOfRange(value))
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for MidiNote {
    type Error = ModelError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<MidiNote> for u8 {
    fn from(value: MidiNote) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct MidiChannel(u8);

impl MidiChannel {
    pub fn new(value: u8) -> Result<Self, ModelError> {
        (1..=16)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(ModelError::MidiChannelOutOfRange(value))
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for MidiChannel {
    type Error = ModelError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<MidiChannel> for u8 {
    fn from(value: MidiChannel) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiChannelFilter {
    Omni,
    Channel(MidiChannel),
}

impl MidiChannelFilter {
    pub fn accepts(self, channel: MidiChannel) -> bool {
        match self {
            Self::Omni => true,
            Self::Channel(expected) => expected == channel,
        }
    }
}

impl Serialize for MidiChannelFilter {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Omni => serializer.serialize_str("omni"),
            Self::Channel(channel) => serializer.serialize_u8(channel.get()),
        }
    }
}

impl<'de> Deserialize<'de> for MidiChannelFilter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawFilter {
            Name(String),
            Channel(u8),
        }

        match RawFilter::deserialize(deserializer)? {
            RawFilter::Name(name) if name == "omni" => Ok(Self::Omni),
            RawFilter::Name(name) => Err(serde::de::Error::custom(format!(
                "invalid MIDI channel filter {name:?}"
            ))),
            RawFilter::Channel(channel) => MidiChannel::new(channel)
                .map(Self::Channel)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MidiBankMap {
    notes: [Option<MidiNote>; PADS_PER_BANK as usize],
}

impl MidiBankMap {
    pub fn new(notes: [Option<MidiNote>; PADS_PER_BANK as usize]) -> Result<Self, ModelError> {
        let candidate = Self { notes };
        candidate.validate()?;
        Ok(candidate)
    }

    pub fn validate(self) -> Result<(), ModelError> {
        let mut assigned = [false; 128];
        for note in self.notes.into_iter().flatten() {
            let index = usize::from(note.get());
            if assigned[index] {
                return Err(ModelError::DuplicateMidiNote(note.get()));
            }
            assigned[index] = true;
        }
        Ok(())
    }

    pub const fn notes(self) -> [Option<MidiNote>; PADS_PER_BANK as usize] {
        self.notes
    }

    pub fn note(self, pad: u8) -> Result<Option<MidiNote>, ModelError> {
        self.notes
            .get(usize::from(pad))
            .copied()
            .ok_or(ModelError::PadOutOfRange(pad))
    }

    pub fn owner(self, note: MidiNote) -> Option<u8> {
        self.notes
            .iter()
            .position(|assigned| *assigned == Some(note))
            .map(|index| index as u8)
    }

    pub fn map(mut self, pad: u8, note: MidiNote) -> Result<Self, ModelError> {
        let index = Self::pad_index(pad)?;
        if self.owner(note).is_some_and(|owner| owner != pad) {
            return Err(ModelError::DuplicateMidiNote(note.get()));
        }
        self.notes[index] = Some(note);
        Ok(self)
    }

    pub fn unmap(mut self, pad: u8) -> Result<Self, ModelError> {
        let index = Self::pad_index(pad)?;
        self.notes[index] = None;
        Ok(self)
    }

    pub fn reset(self) -> Self {
        Self::default()
    }

    pub fn learn_swap(mut self, pad: u8, note: MidiNote) -> Result<Self, ModelError> {
        let target = Self::pad_index(pad)?;
        let displaced = self.notes[target];
        if let Some(owner) = self.owner(note) {
            if owner == pad {
                return Ok(self);
            }
            self.notes[usize::from(owner)] = displaced;
        }
        self.notes[target] = Some(note);
        Ok(self)
    }

    fn pad_index(pad: u8) -> Result<usize, ModelError> {
        (pad < PADS_PER_BANK)
            .then_some(usize::from(pad))
            .ok_or(ModelError::PadOutOfRange(pad))
    }
}

impl Default for MidiBankMap {
    fn default() -> Self {
        Self {
            notes: std::array::from_fn(|index| Some(MidiNote(36 + index as u8))),
        }
    }
}

impl<'de> Deserialize<'de> for MidiBankMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawMidiBankMap {
            notes: [Option<MidiNote>; PADS_PER_BANK as usize],
        }

        let raw = RawMidiBankMap::deserialize(deserializer)?;
        Self::new(raw.notes).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MidiSettings {
    channel: MidiChannelFilter,
    banks: [MidiBankMap; BANK_COUNT as usize],
}

impl MidiSettings {
    pub fn new(
        channel: MidiChannelFilter,
        banks: [MidiBankMap; BANK_COUNT as usize],
    ) -> Result<Self, ModelError> {
        let candidate = Self { channel, banks };
        candidate.validate()?;
        Ok(candidate)
    }

    pub fn validate(self) -> Result<(), ModelError> {
        for bank in self.banks {
            bank.validate()?;
        }
        Ok(())
    }

    pub const fn channel(self) -> MidiChannelFilter {
        self.channel
    }

    pub fn bank(self, bank: BankId) -> MidiBankMap {
        self.banks[usize::from(u8::from(bank))]
    }

    pub fn with_channel(mut self, channel: MidiChannelFilter) -> Self {
        self.channel = channel;
        self
    }

    pub fn map(mut self, bank: BankId, pad: u8, note: MidiNote) -> Result<Self, ModelError> {
        let index = usize::from(u8::from(bank));
        self.banks[index] = self.banks[index].map(pad, note)?;
        Ok(self)
    }

    pub fn unmap(mut self, bank: BankId, pad: u8) -> Result<Self, ModelError> {
        let index = usize::from(u8::from(bank));
        self.banks[index] = self.banks[index].unmap(pad)?;
        Ok(self)
    }

    pub fn reset_bank(mut self, bank: BankId) -> Self {
        self.banks[usize::from(u8::from(bank))] = MidiBankMap::default();
        self
    }

    pub fn learn_swap(mut self, bank: BankId, pad: u8, note: MidiNote) -> Result<Self, ModelError> {
        let index = usize::from(u8::from(bank));
        self.banks[index] = self.banks[index].learn_swap(pad, note)?;
        Ok(self)
    }
}

impl Default for MidiSettings {
    fn default() -> Self {
        Self {
            channel: MidiChannelFilter::Omni,
            banks: [MidiBankMap::default(); BANK_COUNT as usize],
        }
    }
}

impl<'de> Deserialize<'de> for MidiSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawMidiSettings {
            channel: MidiChannelFilter,
            banks: [MidiBankMap; BANK_COUNT as usize],
        }

        let raw = RawMidiSettings::deserialize(deserializer)?;
        Self::new(raw.channel, raw.banks).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use crate::{BankId, ModelError};

    use super::{MidiBankMap, MidiChannel, MidiChannelFilter, MidiNote, MidiSettings};

    const DEFAULT_NOTES: [u8; 16] = [
        36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
    ];

    fn note(value: u8) -> MidiNote {
        MidiNote::new(value).unwrap()
    }

    fn bank(value: u8) -> BankId {
        BankId::new(value).unwrap()
    }

    #[test]
    fn note_and_channel_constructors_accept_exact_boundaries_and_reject_outside_values() {
        assert_eq!(MidiNote::new(0).unwrap().get(), 0);
        assert_eq!(MidiNote::new(127).unwrap().get(), 127);
        assert_eq!(MidiNote::new(128), Err(ModelError::MidiNoteOutOfRange(128)));
        assert_eq!(MidiNote::new(255), Err(ModelError::MidiNoteOutOfRange(255)));

        assert_eq!(MidiChannel::new(1).unwrap().get(), 1);
        assert_eq!(MidiChannel::new(16).unwrap().get(), 16);
        assert_eq!(
            MidiChannel::new(0),
            Err(ModelError::MidiChannelOutOfRange(0))
        );
        assert_eq!(
            MidiChannel::new(17),
            Err(ModelError::MidiChannelOutOfRange(17))
        );
    }

    #[test]
    fn channel_filters_are_copyable_and_accept_omni_or_one_numbered_channel() {
        let omni = MidiChannelFilter::Omni;
        let numbered = MidiChannelFilter::Channel(MidiChannel::new(16).unwrap());
        let copied = [omni, numbered];

        assert!(copied[0].accepts(MidiChannel::new(1).unwrap()));
        assert!(copied[0].accepts(MidiChannel::new(16).unwrap()));
        assert!(copied[1].accepts(MidiChannel::new(16).unwrap()));
        assert!(!copied[1].accepts(MidiChannel::new(15).unwrap()));
    }

    #[test]
    fn defaults_are_omni_and_literal_notes_36_through_51_in_all_ten_banks() {
        let settings = MidiSettings::default();
        assert_eq!(settings.channel(), MidiChannelFilter::Omni);

        for bank_index in 0..10 {
            let map = settings.bank(bank(bank_index));
            let actual = map.notes().map(|assigned| assigned.map(MidiNote::get));
            assert_eq!(actual, DEFAULT_NOTES.map(Some), "bank {bank_index}");
        }
    }

    #[test]
    fn bank_construction_rejects_duplicates_but_settings_allow_reuse_across_banks() {
        let mut duplicate = MidiBankMap::default().notes();
        duplicate[7] = duplicate[0];
        assert_eq!(
            MidiBankMap::new(duplicate),
            Err(ModelError::DuplicateMidiNote(36))
        );

        let shared = MidiBankMap::default().map(0, note(60)).unwrap();
        let banks = [shared; 10];
        let settings = MidiSettings::new(MidiChannelFilter::Omni, banks).unwrap();
        assert_eq!(settings.bank(bank(0)).note(0).unwrap(), Some(note(60)));
        assert_eq!(settings.bank(bank(9)).note(0).unwrap(), Some(note(60)));
    }

    #[test]
    fn map_unmap_and_reset_return_valid_candidates_without_mutating_the_source() {
        let original = MidiBankMap::default();
        let mapped = original.map(0, note(60)).unwrap();
        assert_eq!(original.note(0).unwrap(), Some(note(36)));
        assert_eq!(mapped.note(0).unwrap(), Some(note(60)));
        assert_eq!(
            mapped.map(1, note(60)),
            Err(ModelError::DuplicateMidiNote(60))
        );

        let unmapped = mapped.unmap(0).unwrap();
        assert_eq!(mapped.note(0).unwrap(), Some(note(60)));
        assert_eq!(unmapped.note(0).unwrap(), None);
        assert_eq!(unmapped.reset(), MidiBankMap::default());

        assert_eq!(
            original.map(16, note(60)),
            Err(ModelError::PadOutOfRange(16))
        );
        assert_eq!(original.unmap(16), Err(ModelError::PadOutOfRange(16)));
    }

    #[test]
    fn learn_swap_deterministically_preserves_the_displaced_assignment_and_uniqueness() {
        let original = MidiBankMap::default();
        let swapped = original.learn_swap(0, note(40)).unwrap();
        assert_eq!(swapped.note(0).unwrap(), Some(note(40)));
        assert_eq!(swapped.note(4).unwrap(), Some(note(36)));
        assert_eq!(swapped.owner(note(40)), Some(0));
        assert_eq!(swapped.owner(note(36)), Some(4));

        let unassigned_target = original.unmap(0).unwrap();
        let moved = unassigned_target.learn_swap(0, note(40)).unwrap();
        assert_eq!(moved.note(0).unwrap(), Some(note(40)));
        assert_eq!(moved.note(4).unwrap(), None);

        let unowned = original.learn_swap(0, note(90)).unwrap();
        assert_eq!(unowned.note(0).unwrap(), Some(note(90)));
        assert_eq!(unowned.owner(note(36)), None);
    }

    #[test]
    fn settings_operations_replace_only_the_requested_bank_and_return_candidates() {
        let original = MidiSettings::default();
        let mapped = original.map(bank(3), 2, note(90)).unwrap();
        assert_eq!(original.bank(bank(3)).note(2).unwrap(), Some(note(38)));
        assert_eq!(mapped.bank(bank(3)).note(2).unwrap(), Some(note(90)));
        assert_eq!(mapped.bank(bank(2)), MidiBankMap::default());

        let unmapped = mapped.unmap(bank(3), 2).unwrap();
        assert_eq!(unmapped.bank(bank(3)).note(2).unwrap(), None);
        let swapped = original.learn_swap(bank(4), 0, note(40)).unwrap();
        assert_eq!(swapped.bank(bank(4)).note(0).unwrap(), Some(note(40)));
        assert_eq!(swapped.bank(bank(4)).note(4).unwrap(), Some(note(36)));
        assert_eq!(
            swapped.reset_bank(bank(4)).bank(bank(4)),
            MidiBankMap::default()
        );

        let filtered =
            original.with_channel(MidiChannelFilter::Channel(MidiChannel::new(3).unwrap()));
        assert_eq!(
            filtered.channel(),
            MidiChannelFilter::Channel(MidiChannel::new(3).unwrap())
        );
        assert_eq!(original.channel(), MidiChannelFilter::Omni);
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct NoteFixture {
        note: MidiNote,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct ChannelFixture {
        channel: MidiChannel,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct FilterFixture {
        filter: MidiChannelFilter,
    }

    #[test]
    fn literal_serde_uses_validating_note_channel_and_filter_boundaries() {
        assert_eq!(
            toml::from_str::<NoteFixture>("note = 127").unwrap(),
            NoteFixture { note: note(127) }
        );
        assert!(toml::from_str::<NoteFixture>("note = 128").is_err());
        assert_eq!(
            toml::from_str::<ChannelFixture>("channel = 1").unwrap(),
            ChannelFixture {
                channel: MidiChannel::new(1).unwrap(),
            }
        );
        assert!(toml::from_str::<ChannelFixture>("channel = 0").is_err());
        assert!(toml::from_str::<ChannelFixture>("channel = 17").is_err());

        assert_eq!(
            toml::from_str::<FilterFixture>("filter = \"omni\"").unwrap(),
            FilterFixture {
                filter: MidiChannelFilter::Omni,
            }
        );
        assert_eq!(
            toml::from_str::<FilterFixture>("filter = 16").unwrap(),
            FilterFixture {
                filter: MidiChannelFilter::Channel(MidiChannel::new(16).unwrap()),
            }
        );
        assert!(toml::from_str::<FilterFixture>("filter = 0").is_err());
        assert!(toml::from_str::<FilterFixture>("filter = 17").is_err());

        assert_eq!(
            toml::to_string(&NoteFixture { note: note(127) }).unwrap(),
            "note = 127\n"
        );
        assert_eq!(
            toml::to_string(&FilterFixture {
                filter: MidiChannelFilter::Omni,
            })
            .unwrap(),
            "filter = \"omni\"\n"
        );
    }

    #[test]
    fn bank_and_settings_deserialization_reject_duplicates_and_invalid_nested_values() {
        let valid = format!(
            "notes = [{}]",
            DEFAULT_NOTES.map(|value| value.to_string()).join(", ")
        );
        assert_eq!(
            toml::from_str::<MidiBankMap>(&valid).unwrap(),
            MidiBankMap::default()
        );

        let duplicate = "notes = [36, 36, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51]";
        assert!(toml::from_str::<MidiBankMap>(duplicate).is_err());
        let invalid = "notes = [128, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51]";
        assert!(toml::from_str::<MidiBankMap>(invalid).is_err());

        let default = MidiSettings::default();
        let serialized = toml::to_string(&default).unwrap();
        assert_eq!(
            toml::from_str::<MidiSettings>(&serialized).unwrap(),
            default
        );
        let duplicated_settings = serialized.replacen("36, 37", "36, 36", 1);
        assert!(toml::from_str::<MidiSettings>(&duplicated_settings).is_err());
    }
}
