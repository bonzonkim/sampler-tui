//! Portable project document model.

use std::{collections::HashSet, path::Component, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    BankId, ChokeGroup, EventId, Meter, ModelError, PadId, PadSettings, PatternEvent, PlaybackMode,
    Resolution, Tempo, Transport,
};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectError {
    #[error("project TOML is invalid: {0}")]
    TomlSyntax(String),
    #[error("project could not be encoded: {0}")]
    TomlEncode(String),
    #[error("project schema {found} is newer than supported schema {supported}")]
    NewerSchema { found: u32, supported: u32 },
    #[error("project schema {0} is unsupported")]
    UnsupportedSchema(u32),
    #[error("project or pattern name must not be blank")]
    InvalidName,
    #[error("audio path is not a portable project audio path: {0}")]
    InvalidAudioPath(String),
    #[error("pad {0:?} appears more than once")]
    DuplicatePad(PadId),
    #[error("pattern name appears more than once: {0}")]
    DuplicatePattern(String),
    #[error("invalid project model: {0}")]
    InvalidModel(ModelError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProjectDocument {
    schema_version: u32,
    name: String,
    #[serde(default)]
    pads: Vec<WireProjectPad>,
    #[serde(default)]
    patterns: Vec<WireProjectPattern>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProjectPad {
    pad: WirePadId,
    audio_path: String,
    settings: WirePadSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePadId {
    bank: u8,
    index: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePadSettings {
    mode: PlaybackMode,
    gain_db: f32,
    pan: f32,
    pitch_semitones: f32,
    choke_group: Option<u8>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProjectPattern {
    name: String,
    sample_rate: u32,
    tempo: f64,
    meter: WireMeter,
    bars: u16,
    resolution: Resolution,
    swing: f64,
    #[serde(default)]
    events: Vec<WirePatternEvent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMeter {
    numerator: u8,
    denominator: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePatternEvent {
    id: u64,
    pad: WirePadId,
    frame: u64,
    velocity: f32,
    duration: Option<u64>,
    original_offset: Option<i64>,
}

impl TryFrom<WirePadId> for PadId {
    type Error = ProjectError;

    fn try_from(value: WirePadId) -> Result<Self, Self::Error> {
        let bank = BankId::new(value.bank).map_err(ProjectError::InvalidModel)?;
        PadId::new(bank, value.index).map_err(ProjectError::InvalidModel)
    }
}

impl TryFrom<WirePadSettings> for PadSettings {
    type Error = ProjectError;

    fn try_from(value: WirePadSettings) -> Result<Self, Self::Error> {
        let choke_group = value
            .choke_group
            .map(ChokeGroup::new)
            .transpose()
            .map_err(ProjectError::InvalidModel)?;
        PadSettings::new(
            value.mode,
            value.gain_db,
            value.pan,
            value.pitch_semitones,
            choke_group,
        )
        .map_err(ProjectError::InvalidModel)
    }
}

impl TryFrom<WireProjectPad> for ProjectPad {
    type Error = ProjectError;

    fn try_from(value: WireProjectPad) -> Result<Self, Self::Error> {
        ProjectPad::new(
            value.pad.try_into()?,
            value.audio_path,
            value.settings.try_into()?,
        )
    }
}

impl TryFrom<WirePatternEvent> for PatternEvent {
    type Error = ProjectError;

    fn try_from(value: WirePatternEvent) -> Result<Self, Self::Error> {
        let mut event = PatternEvent::new(
            EventId(value.id),
            value.pad.try_into()?,
            value.frame,
            value.velocity,
            value.duration,
        )
        .map_err(ProjectError::InvalidModel)?;
        event.original_offset = value.original_offset;
        Ok(event)
    }
}

impl TryFrom<WireProjectPattern> for ProjectPattern {
    type Error = ProjectError;

    fn try_from(value: WireProjectPattern) -> Result<Self, Self::Error> {
        let pattern = ProjectPattern {
            name: value.name,
            sample_rate: value.sample_rate,
            tempo: Tempo::new(value.tempo).map_err(ProjectError::InvalidModel)?,
            meter: Meter::new(value.meter.numerator, value.meter.denominator)
                .map_err(ProjectError::InvalidModel)?,
            bars: value.bars,
            resolution: value.resolution,
            swing: value.swing,
            events: value
                .events
                .into_iter()
                .map(PatternEvent::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        };
        pattern.validate()?;
        Ok(pattern)
    }
}

impl TryFrom<WireProjectDocument> for ProjectDocument {
    type Error = ProjectError;

    fn try_from(value: WireProjectDocument) -> Result<Self, Self::Error> {
        let project = ProjectDocument {
            schema_version: value.schema_version,
            name: value.name,
            pads: value
                .pads
                .into_iter()
                .map(ProjectPad::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            patterns: value
                .patterns
                .into_iter()
                .map(ProjectPattern::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        };
        project.validate()?;
        Ok(project)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDocument {
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub pads: Vec<ProjectPad>,
    #[serde(default)]
    pub patterns: Vec<ProjectPattern>,
}

impl ProjectDocument {
    pub fn empty(name: impl Into<String>) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            name: name.into(),
            pads: Vec::new(),
            patterns: Vec::new(),
        }
    }

    pub fn to_toml(&self) -> Result<String, ProjectError> {
        self.validate()?;
        toml::to_string_pretty(self).map_err(|error| ProjectError::TomlEncode(error.to_string()))
    }

    pub fn from_toml(source: &str) -> Result<Self, ProjectError> {
        #[derive(Deserialize)]
        struct Header {
            schema_version: u32,
        }

        let header: Header =
            toml::from_str(source).map_err(|error| ProjectError::TomlSyntax(error.to_string()))?;
        match header.schema_version {
            0 => return Err(ProjectError::UnsupportedSchema(0)),
            found if found > CURRENT_SCHEMA_VERSION => {
                return Err(ProjectError::NewerSchema {
                    found,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
            _ => {}
        }
        let project: WireProjectDocument =
            toml::from_str(source).map_err(|error| ProjectError::TomlSyntax(error.to_string()))?;
        project.try_into()
    }

    fn validate(&self) -> Result<(), ProjectError> {
        if self.schema_version == 0 {
            return Err(ProjectError::UnsupportedSchema(0));
        }
        if self.schema_version > CURRENT_SCHEMA_VERSION {
            return Err(ProjectError::NewerSchema {
                found: self.schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        if self.name.trim().is_empty() {
            return Err(ProjectError::InvalidName);
        }

        let mut pads = HashSet::with_capacity(self.pads.len());
        for pad in &self.pads {
            pad.validate()?;
            if !pads.insert(pad.pad) {
                return Err(ProjectError::DuplicatePad(pad.pad));
            }
        }

        let mut patterns = HashSet::with_capacity(self.patterns.len());
        for pattern in &self.patterns {
            pattern.validate()?;
            if !patterns.insert(pattern.name.as_str()) {
                return Err(ProjectError::DuplicatePattern(pattern.name.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPad {
    pub pad: PadId,
    pub audio_path: String,
    pub settings: PadSettings,
}

impl ProjectPad {
    pub fn new(
        pad: PadId,
        audio_path: impl Into<String>,
        settings: PadSettings,
    ) -> Result<Self, ProjectError> {
        let value = Self {
            pad,
            audio_path: audio_path.into(),
            settings,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ProjectError> {
        PadId::new(self.pad.bank(), self.pad.index()).map_err(ProjectError::InvalidModel)?;
        PadSettings::new(
            self.settings.mode,
            self.settings.gain_db,
            self.settings.pan,
            self.settings.pitch_semitones,
            self.settings.choke_group,
        )
        .map_err(ProjectError::InvalidModel)?;
        validate_audio_path(&self.audio_path)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPattern {
    pub name: String,
    pub sample_rate: u32,
    pub tempo: Tempo,
    pub meter: Meter,
    pub bars: u16,
    pub resolution: Resolution,
    pub swing: f64,
    #[serde(default)]
    pub events: Vec<PatternEvent>,
}

impl ProjectPattern {
    fn validate(&self) -> Result<(), ProjectError> {
        if self.name.trim().is_empty() {
            return Err(ProjectError::InvalidName);
        }
        let transport = Transport::new(
            self.sample_rate,
            Tempo::new(self.tempo.bpm()).map_err(ProjectError::InvalidModel)?,
            Meter::new(self.meter.numerator(), self.meter.denominator())
                .map_err(ProjectError::InvalidModel)?,
            self.bars,
            self.resolution,
        )
        .and_then(|transport| transport.with_swing(self.swing))
        .map_err(ProjectError::InvalidModel)?;

        let mut ids = HashSet::with_capacity(self.events.len());
        for event in &self.events {
            let validated = PatternEvent::new(
                EventId(event.id.0),
                event.pad,
                event.frame,
                event.velocity,
                event.duration,
            )
            .map_err(ProjectError::InvalidModel)?;
            if validated.frame >= transport.loop_frames() {
                return Err(ProjectError::InvalidModel(ModelError::InvalidEvent));
            }
            if !ids.insert(validated.id) {
                return Err(ProjectError::InvalidModel(ModelError::DuplicateEvent));
            }
        }
        Ok(())
    }
}

fn validate_audio_path(value: &str) -> Result<(), ProjectError> {
    let path = Path::new(value);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(first)) if first == "audio")
        || components.any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ProjectError::InvalidAudioPath(value.to_owned()));
    }
    let supported = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "wav" | "aif" | "aiff" | "flac" | "mp3"
            )
        });
    supported
        .then_some(())
        .ok_or_else(|| ProjectError::InvalidAudioPath(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BankId, PadId, PadSettings};

    fn project_with_event_pad(index: u8) -> String {
        format!(
            r#"
schema_version = 1
name = "unsafe"
pads = []

[[patterns]]
name = "beat"
sample_rate = 48000
tempo = 120.0
bars = 1
resolution = "sixteenth"
swing = 0.5

[patterns.meter]
numerator = 4
denominator = 4

[[patterns.events]]
id = 1
frame = 0
velocity = 1.0

[patterns.events.pad]
bank = 0
index = {index}
"#
        )
    }

    #[test]
    fn project_round_trip_preserves_portable_relative_paths() {
        let mut project = ProjectDocument::empty("beat-one");
        project.pads.push(
            ProjectPad::new(PadId::first(), "audio/kick.wav", PadSettings::default()).unwrap(),
        );
        let encoded = project.to_toml().unwrap();
        assert!(encoded.contains("schema_version = 1"));
        assert_eq!(ProjectDocument::from_toml(&encoded).unwrap(), project);
    }

    #[test]
    fn rejects_newer_schema_with_actionable_version_data() {
        let source = "schema_version = 99\nname = \"future\"\n\npads = []\npatterns = []\n";
        assert_eq!(
            ProjectDocument::from_toml(source).unwrap_err(),
            ProjectError::NewerSchema {
                found: 99,
                supported: CURRENT_SCHEMA_VERSION
            }
        );
    }

    #[test]
    fn rejects_absolute_or_parent_traversing_audio_paths() {
        assert!(ProjectPad::new(PadId::first(), "/tmp/kick.wav", PadSettings::default()).is_err());
        assert!(ProjectPad::new(PadId::first(), "../kick.wav", PadSettings::default()).is_err());
        assert!(ProjectPad::new(PadId::first(), "audio/kick.wav", PadSettings::default()).is_ok());
    }

    #[test]
    fn rejects_duplicate_pads_before_serializing() {
        let mut project = ProjectDocument::empty("beat-one");
        let pad = ProjectPad::new(
            PadId::new(BankId::new(0).unwrap(), 1).unwrap(),
            "audio/snare.flac",
            PadSettings::default(),
        )
        .unwrap();
        project.pads.extend([pad.clone(), pad]);
        assert_eq!(
            project.to_toml().unwrap_err(),
            ProjectError::DuplicatePad(PadId::new(BankId::new(0).unwrap(), 1).unwrap())
        );
    }

    #[test]
    fn rejects_zero_and_malformed_schema_documents() {
        let zero = "schema_version = 0\nname = \"old\"\npads = []\npatterns = []\n";
        assert_eq!(
            ProjectDocument::from_toml(zero).unwrap_err(),
            ProjectError::UnsupportedSchema(0)
        );
        assert!(matches!(
            ProjectDocument::from_toml("not = [valid"),
            Err(ProjectError::TomlSyntax(_))
        ));
    }

    #[test]
    fn rejects_out_of_range_pattern_event_pad_as_model_error() {
        assert_eq!(
            ProjectDocument::from_toml(&project_with_event_pad(16)).unwrap_err(),
            ProjectError::InvalidModel(ModelError::PadOutOfRange(16))
        );
    }

    #[test]
    fn invalid_pattern_numbers_are_classified_as_model_errors() {
        let valid = project_with_event_pad(0);
        let cases = [
            (
                valid.replace("tempo = 120.0", "tempo = 10.0"),
                ModelError::TempoOutOfRange,
            ),
            (
                valid.replace("denominator = 4", "denominator = 3"),
                ModelError::InvalidMeter {
                    numerator: 4,
                    denominator: 3,
                },
            ),
            (
                valid.replace("swing = 0.5", "swing = 0.9"),
                ModelError::SwingOutOfRange,
            ),
            (
                valid.replace("frame = 0", "frame = 96000"),
                ModelError::InvalidEvent,
            ),
        ];
        for (source, expected) in cases {
            assert_eq!(
                ProjectDocument::from_toml(&source).unwrap_err(),
                ProjectError::InvalidModel(expected)
            );
        }
    }
}
