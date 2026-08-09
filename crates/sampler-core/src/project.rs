//! Portable project document model.

use std::{collections::HashSet, fmt, path::Component, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::{
    BankId, ChokeGroup, DelaySettings, EditablePattern, EventId, MAX_PATTERN_EVENTS,
    MasterMixSettings, Meter, ModelError, PATTERN_SLOT_COUNT, PadId, PadMixSettings, PadSettings,
    PatternEditError, PatternEvent, PatternSlotId, PlaybackMode, Resolution, ReverbSettings,
    SampleEditError, SampleEditRecipe, Tempo, Transport,
};

pub const CURRENT_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectId([u8; 16]);

impl ProjectId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

impl FromStr for ProjectId {
    type Err = ProjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decode_hex(value)
            .map(Self::from_bytes)
            .ok_or(ProjectError::InvalidProjectId)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AssetDigest([u8; 32]);

impl AssetDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for AssetDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

impl FromStr for AssetDigest {
    type Err = ProjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        decode_hex(value)
            .map(Self::from_bytes)
            .ok_or(ProjectError::InvalidAssetDigest)
    }
}

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
    #[error("legacy schema-v1 projects must be migrated before serialization")]
    LegacyNeedsMigration,
    #[error("project id must be exactly 32 lowercase hexadecimal characters")]
    InvalidProjectId,
    #[error("asset digest must be exactly 64 lowercase hexadecimal characters")]
    InvalidAssetDigest,
    #[error("project revision {0} exceeds the portable TOML integer range")]
    InvalidRevision(u64),
    #[error("project or pattern name must not be blank")]
    InvalidName,
    #[error("audio path is not a portable project audio path: {0}")]
    InvalidAudioPath(String),
    #[error("audio path digest does not match the pad asset digest")]
    AssetDigestMismatch,
    #[error("pad {0:?} appears more than once")]
    DuplicatePad(PadId),
    #[error("pattern name appears more than once: {0}")]
    DuplicatePattern(String),
    #[error("pattern slot {0:?} appears more than once")]
    DuplicatePatternSlot(PatternSlotId),
    #[error("project has more than {PATTERN_SLOT_COUNT} patterns")]
    TooManyPatterns,
    #[error("pattern {pattern} has more than {MAX_PATTERN_EVENTS} events")]
    TooManyPatternEvents { pattern: String },
    #[error("invalid project model: {0}")]
    InvalidModel(ModelError),
    #[error("invalid sample edit recipe: {0}")]
    InvalidRecipe(SampleEditError),
    #[error("invalid persisted pattern: {0}")]
    InvalidPattern(PatternEditError),
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut decoded = [0; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let nibble = |byte| match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => unreachable!(),
        };
        decoded[index] = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Some(decoded)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireV1ProjectDocument {
    schema_version: u32,
    name: String,
    #[serde(default)]
    pads: Vec<WireV1ProjectPad>,
    #[serde(default)]
    patterns: Vec<WireV1ProjectPattern>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireV1ProjectPad {
    pad: WirePadId,
    audio_path: String,
    settings: WirePadSettings,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WirePadId {
    bank: u8,
    index: u8,
}

#[derive(Deserialize, Serialize)]
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
struct WireV1ProjectPattern {
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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireMeter {
    numerator: u8,
    denominator: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WirePatternEvent {
    id: u64,
    pad: WirePadId,
    frame: u64,
    velocity: f32,
    duration: Option<u64>,
    original_offset: Option<i64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireRecipe {
    start_phase: u64,
    end_phase: u64,
    reversed: bool,
    normalize: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireV2ProjectDocument {
    schema_version: u32,
    project_id: String,
    name: String,
    revision: u64,
    #[serde(default)]
    pads: Vec<WireV2ProjectPad>,
    #[serde(default)]
    patterns: Vec<WireV2ProjectPattern>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireV2ProjectPad {
    pad: WirePadId,
    audio_path: String,
    asset_digest: String,
    settings: WirePadSettings,
    recipe: WireRecipe,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireV2ProjectPattern {
    slot: u8,
    name: String,
    sample_rate: u32,
    tempo: f64,
    meter: WireMeter,
    bars: u16,
    resolution: Resolution,
    swing: f64,
    quantize_strength: f32,
    #[serde(default)]
    events: Vec<WireV2PatternEvent>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireV2PatternEvent {
    id: u64,
    pad: WirePadId,
    frame: u64,
    raw_frame: u64,
    velocity: f32,
    duration: Option<u64>,
    original_offset: Option<i64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireV3ProjectDocument {
    schema_version: u32,
    project_id: String,
    name: String,
    revision: u64,
    #[serde(default)]
    pads: Vec<WireV3ProjectPad>,
    #[serde(default)]
    patterns: Vec<WireV2ProjectPattern>,
    master_mix: WireMasterMixSettings,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireV3ProjectPad {
    pad: WirePadId,
    audio_path: String,
    asset_digest: String,
    settings: WirePadSettings,
    mix: WirePadMixSettings,
    recipe: WireRecipe,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WirePadMixSettings {
    muted: bool,
    delay_send: f32,
    reverb_send: f32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireMasterMixSettings {
    gain_db: f32,
    delay: WireDelaySettings,
    reverb: WireReverbSettings,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireDelaySettings {
    enabled: bool,
    time_ms: u16,
    feedback: f32,
    return_db: f32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireReverbSettings {
    enabled: bool,
    room_size: f32,
    damping: f32,
    return_db: f32,
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

impl TryFrom<WirePadMixSettings> for PadMixSettings {
    type Error = ProjectError;

    fn try_from(value: WirePadMixSettings) -> Result<Self, Self::Error> {
        PadMixSettings::new(value.muted, value.delay_send, value.reverb_send)
            .map_err(ProjectError::InvalidModel)
    }
}

impl From<PadMixSettings> for WirePadMixSettings {
    fn from(value: PadMixSettings) -> Self {
        Self {
            muted: value.muted,
            delay_send: value.delay_send,
            reverb_send: value.reverb_send,
        }
    }
}

impl TryFrom<WireDelaySettings> for DelaySettings {
    type Error = ProjectError;

    fn try_from(value: WireDelaySettings) -> Result<Self, Self::Error> {
        DelaySettings::new(
            value.enabled,
            value.time_ms,
            value.feedback,
            value.return_db,
        )
        .map_err(ProjectError::InvalidModel)
    }
}

impl From<DelaySettings> for WireDelaySettings {
    fn from(value: DelaySettings) -> Self {
        Self {
            enabled: value.enabled,
            time_ms: value.time_ms,
            feedback: value.feedback,
            return_db: value.return_db,
        }
    }
}

impl TryFrom<WireReverbSettings> for ReverbSettings {
    type Error = ProjectError;

    fn try_from(value: WireReverbSettings) -> Result<Self, Self::Error> {
        ReverbSettings::new(
            value.enabled,
            value.room_size,
            value.damping,
            value.return_db,
        )
        .map_err(ProjectError::InvalidModel)
    }
}

impl From<ReverbSettings> for WireReverbSettings {
    fn from(value: ReverbSettings) -> Self {
        Self {
            enabled: value.enabled,
            room_size: value.room_size,
            damping: value.damping,
            return_db: value.return_db,
        }
    }
}

impl TryFrom<WireMasterMixSettings> for MasterMixSettings {
    type Error = ProjectError;

    fn try_from(value: WireMasterMixSettings) -> Result<Self, Self::Error> {
        MasterMixSettings::new(
            value.gain_db,
            value.delay.try_into()?,
            value.reverb.try_into()?,
        )
        .map_err(ProjectError::InvalidModel)
    }
}

impl From<MasterMixSettings> for WireMasterMixSettings {
    fn from(value: MasterMixSettings) -> Self {
        Self {
            gain_db: value.gain_db,
            delay: value.delay.into(),
            reverb: value.reverb.into(),
        }
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

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectDocument {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub name: String,
    pub revision: u64,
    pub pads: Vec<ProjectPad>,
    pub patterns: Vec<ProjectPattern>,
    pub master_mix: MasterMixSettings,
}

impl ProjectDocument {
    pub fn new_v3(
        project_id: ProjectId,
        name: impl Into<String>,
        revision: u64,
        pads: Vec<ProjectPad>,
        patterns: Vec<ProjectPattern>,
        master_mix: MasterMixSettings,
    ) -> Result<Self, ProjectError> {
        let patterns = complete_sparse_patterns(patterns)?;
        let project = Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            project_id,
            name: name.into(),
            revision,
            pads,
            patterns,
            master_mix,
        };
        project.validate()?;
        Ok(project)
    }

    pub fn to_toml(&self) -> Result<String, ProjectError> {
        self.validate()?;
        let wire = WireV3ProjectDocument::try_from(self)?;
        toml::to_string_pretty(&wire).map_err(|error| ProjectError::TomlEncode(error.to_string()))
    }

    pub fn from_toml(source: &str) -> Result<ParsedProjectDocument, ProjectError> {
        #[derive(Deserialize)]
        struct Header {
            schema_version: u32,
        }

        let header: Header =
            toml::from_str(source).map_err(|error| ProjectError::TomlSyntax(error.to_string()))?;
        match header.schema_version {
            1 => parse_v1_legacy(source),
            2 => parse_v2(source)
                .and_then(migrate_v2_to_v3)
                .map(ParsedProjectDocument::Current),
            3 => parse_v3_current(source).map(ParsedProjectDocument::Current),
            found if found > CURRENT_SCHEMA_VERSION => Err(ProjectError::NewerSchema {
                found,
                supported: CURRENT_SCHEMA_VERSION,
            }),
            found => Err(ProjectError::UnsupportedSchema(found)),
        }
    }

    fn validate(&self) -> Result<(), ProjectError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ProjectError::UnsupportedSchema(self.schema_version));
        }
        if self.name.trim().is_empty() {
            return Err(ProjectError::InvalidName);
        }
        if self.revision > i64::MAX as u64 {
            return Err(ProjectError::InvalidRevision(self.revision));
        }
        if self.patterns.len() > PATTERN_SLOT_COUNT {
            return Err(ProjectError::TooManyPatterns);
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
            if !patterns.insert(pattern.slot) {
                return Err(ProjectError::DuplicatePatternSlot(pattern.slot));
            }
        }
        self.master_mix
            .validate()
            .map_err(ProjectError::InvalidModel)?;
        Ok(())
    }
}

fn parse_v1_legacy(source: &str) -> Result<ParsedProjectDocument, ProjectError> {
    let wire: WireV1ProjectDocument =
        toml::from_str(source).map_err(|error| ProjectError::TomlSyntax(error.to_string()))?;
    LegacyProjectDocument::try_from(wire).map(ParsedProjectDocument::Legacy)
}

fn parse_v2(source: &str) -> Result<WireV2ProjectDocument, ProjectError> {
    toml::from_str(source).map_err(|error| ProjectError::TomlSyntax(error.to_string()))
}

fn parse_v3_current(source: &str) -> Result<ProjectDocument, ProjectError> {
    let wire: WireV3ProjectDocument =
        toml::from_str(source).map_err(|error| ProjectError::TomlSyntax(error.to_string()))?;
    ProjectDocument::try_from(wire)
}

fn complete_sparse_patterns(
    mut patterns: Vec<ProjectPattern>,
) -> Result<Vec<ProjectPattern>, ProjectError> {
    if patterns.len() > PATTERN_SLOT_COUNT {
        return Err(ProjectError::TooManyPatterns);
    }
    let sample_rate = patterns
        .first()
        .map_or(48_000, |pattern| pattern.sample_rate);
    let mut present = [false; PATTERN_SLOT_COUNT];
    for pattern in &patterns {
        let index = usize::from(pattern.slot.get());
        if present[index] {
            return Err(ProjectError::DuplicatePatternSlot(pattern.slot));
        }
        present[index] = true;
    }
    for (index, is_present) in present.into_iter().enumerate() {
        if is_present {
            continue;
        }
        patterns.push(ProjectPattern {
            slot: PatternSlotId::new(u8::try_from(index).expect("pattern slot fits in u8"))
                .expect("bounded pattern slot is valid"),
            name: format!("Pattern {:02}", index + 1),
            sample_rate,
            tempo: Tempo::new(120.0).expect("default tempo is valid"),
            meter: Meter::new(4, 4).expect("default meter is valid"),
            bars: 1,
            resolution: Resolution::Sixteenth,
            swing: 0.5,
            quantize_strength: 0.0,
            events: Vec::new(),
        });
    }
    Ok(patterns)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectPad {
    pub pad: PadId,
    pub audio_path: String,
    pub asset_digest: AssetDigest,
    pub settings: PadSettings,
    pub mix: PadMixSettings,
    pub recipe: SampleEditRecipe,
}

impl ProjectPad {
    pub fn new(
        pad: PadId,
        audio_path: impl Into<String>,
        asset_digest: AssetDigest,
        settings: PadSettings,
        mix: PadMixSettings,
        recipe: SampleEditRecipe,
    ) -> Result<Self, ProjectError> {
        let value = Self {
            pad,
            audio_path: audio_path.into(),
            asset_digest,
            settings,
            mix,
            recipe,
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
        self.mix.validate().map_err(ProjectError::InvalidModel)?;
        self.recipe
            .validate()
            .map_err(ProjectError::InvalidRecipe)?;
        validate_current_audio_path(&self.audio_path, self.asset_digest)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectPattern {
    pub slot: PatternSlotId,
    pub name: String,
    pub sample_rate: u32,
    pub tempo: Tempo,
    pub meter: Meter,
    pub bars: u16,
    pub resolution: Resolution,
    pub swing: f64,
    pub quantize_strength: f32,
    pub events: Vec<ProjectPatternEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProjectPatternEvent {
    pub event: PatternEvent,
    pub raw_frame: u64,
}

impl ProjectPattern {
    pub fn from_editable(editable: &EditablePattern) -> Result<Self, ProjectError> {
        let transport = editable.transport();
        let pattern = Self {
            slot: editable.slot(),
            name: editable.name().to_owned(),
            sample_rate: transport.sample_rate(),
            tempo: transport.tempo(),
            meter: transport.meter(),
            bars: transport.bars(),
            resolution: transport.resolution(),
            swing: transport.swing(),
            quantize_strength: editable.quantize_strength(),
            events: editable
                .persisted_events()
                .map_err(ProjectError::InvalidPattern)?
                .into_iter()
                .map(|(event, raw_frame)| ProjectPatternEvent { event, raw_frame })
                .collect(),
        };
        pattern.validate()?;
        Ok(pattern)
    }

    pub fn to_editable(&self) -> Result<EditablePattern, PatternEditError> {
        let transport = Transport::new(
            self.sample_rate,
            self.tempo,
            self.meter,
            self.bars,
            self.resolution,
        )?
        .with_swing(self.swing)?;
        EditablePattern::from_persisted(
            self.slot,
            self.name.clone(),
            transport,
            self.events
                .iter()
                .map(|persisted| (persisted.event, persisted.raw_frame))
                .collect(),
            self.quantize_strength,
        )
    }

    pub fn slot(&self) -> PatternSlotId {
        self.slot
    }

    fn validate(&self) -> Result<(), ProjectError> {
        if self.name.trim().is_empty() {
            return Err(ProjectError::InvalidName);
        }
        if self.events.len() > MAX_PATTERN_EVENTS {
            return Err(ProjectError::TooManyPatternEvents {
                pattern: self.name.clone(),
            });
        }
        self.to_editable().map(|_| ()).map_err(|error| match error {
            PatternEditError::Model(model) => ProjectError::InvalidModel(model),
            other => ProjectError::InvalidPattern(other),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedProjectDocument {
    Current(ProjectDocument),
    Legacy(LegacyProjectDocument),
}

impl ParsedProjectDocument {
    pub fn current(&self) -> Option<&ProjectDocument> {
        match self {
            Self::Current(project) => Some(project),
            Self::Legacy(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LegacyProjectDocument {
    schema_version: u32,
    name: String,
    pads: Vec<LegacyProjectPad>,
    patterns: Vec<LegacyProjectPattern>,
}

impl LegacyProjectDocument {
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn revision(&self) -> u64 {
        0
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn pads(&self) -> &[LegacyProjectPad] {
        &self.pads
    }

    pub fn patterns(&self) -> &[LegacyProjectPattern] {
        &self.patterns
    }

    pub fn to_toml(&self) -> Result<String, ProjectError> {
        Err(ProjectError::LegacyNeedsMigration)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LegacyProjectPattern {
    slot: PatternSlotId,
    name: String,
    sample_rate: u32,
    tempo: Tempo,
    meter: Meter,
    bars: u16,
    resolution: Resolution,
    swing: f64,
    events: Vec<PatternEvent>,
}

impl LegacyProjectPattern {
    pub fn slot(&self) -> PatternSlotId {
        self.slot
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn events(&self) -> &[PatternEvent] {
        &self.events
    }

    /// Converts schema v1 into an editable pattern while preserving audible event frames.
    ///
    /// Schema v1 did not store quantize strength or an exact raw-frame ledger. Migration
    /// therefore treats each audible frame as its raw frame, resets quantization to zero, and
    /// recomputes `original_offset`. Callers must explicitly opt into this lossy boundary.
    pub fn to_editable_lossy(&self) -> Result<EditablePattern, PatternEditError> {
        let transport = Transport::new(
            self.sample_rate,
            self.tempo,
            self.meter,
            self.bars,
            self.resolution,
        )?
        .with_swing(self.swing)?;
        let events = self
            .events
            .iter()
            .copied()
            .map(|mut event| {
                let raw_frame = event.frame;
                event.original_offset = None;
                (event.quantized(&transport, 0.0), raw_frame)
            })
            .collect();
        EditablePattern::from_persisted(self.slot, self.name.clone(), transport, events, 0.0)
    }

    fn validate(&self) -> Result<(), ProjectError> {
        if self.name.trim().is_empty() {
            return Err(ProjectError::InvalidName);
        }
        if self.events.len() > MAX_PATTERN_EVENTS {
            return Err(ProjectError::TooManyPatternEvents {
                pattern: self.name.clone(),
            });
        }
        let transport = Transport::new(
            self.sample_rate,
            self.tempo,
            self.meter,
            self.bars,
            self.resolution,
        )
        .and_then(|transport| transport.with_swing(self.swing))
        .map_err(ProjectError::InvalidModel)?;
        let mut ids = HashSet::with_capacity(self.events.len());
        for event in &self.events {
            let validated = PatternEvent::new(
                event.id,
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

#[derive(Debug, Clone, PartialEq)]
pub struct LegacyProjectPad {
    pad: PadId,
    audio_path: String,
    settings: PadSettings,
}

impl LegacyProjectPad {
    pub fn pad(&self) -> PadId {
        self.pad
    }

    pub fn audio_path(&self) -> &str {
        &self.audio_path
    }

    pub fn settings(&self) -> PadSettings {
        self.settings
    }

    pub const fn recipe(&self) -> SampleEditRecipe {
        SampleEditRecipe::identity()
    }
}

impl From<PadId> for WirePadId {
    fn from(value: PadId) -> Self {
        Self {
            bank: u8::from(value.bank()),
            index: value.index(),
        }
    }
}

impl From<PadSettings> for WirePadSettings {
    fn from(value: PadSettings) -> Self {
        Self {
            mode: value.mode,
            gain_db: value.gain_db,
            pan: value.pan,
            pitch_semitones: value.pitch_semitones,
            choke_group: value.choke_group.map(ChokeGroup::get),
        }
    }
}

impl From<SampleEditRecipe> for WireRecipe {
    fn from(value: SampleEditRecipe) -> Self {
        Self {
            start_phase: value.start_phase,
            end_phase: value.end_phase,
            reversed: value.reversed,
            normalize: value.normalize,
        }
    }
}

impl TryFrom<WireRecipe> for SampleEditRecipe {
    type Error = ProjectError;

    fn try_from(value: WireRecipe) -> Result<Self, Self::Error> {
        SampleEditRecipe::new(
            value.start_phase,
            value.end_phase,
            value.reversed,
            value.normalize,
        )
        .map_err(ProjectError::InvalidRecipe)
    }
}

impl From<PatternEvent> for WirePatternEvent {
    fn from(value: PatternEvent) -> Self {
        Self {
            id: value.id.0,
            pad: value.pad.into(),
            frame: value.frame,
            velocity: value.velocity,
            duration: value.duration,
            original_offset: value.original_offset,
        }
    }
}

impl From<ProjectPatternEvent> for WireV2PatternEvent {
    fn from(value: ProjectPatternEvent) -> Self {
        Self {
            id: value.event.id.0,
            pad: value.event.pad.into(),
            frame: value.event.frame,
            raw_frame: value.raw_frame,
            velocity: value.event.velocity,
            duration: value.event.duration,
            original_offset: value.event.original_offset,
        }
    }
}

impl TryFrom<WireV2PatternEvent> for ProjectPatternEvent {
    type Error = ProjectError;

    fn try_from(value: WireV2PatternEvent) -> Result<Self, Self::Error> {
        let mut event = PatternEvent::new(
            EventId(value.id),
            value.pad.try_into()?,
            value.frame,
            value.velocity,
            value.duration,
        )
        .map_err(ProjectError::InvalidModel)?;
        event.original_offset = value.original_offset;
        Ok(Self {
            event,
            raw_frame: value.raw_frame,
        })
    }
}

impl TryFrom<WireV2ProjectPad> for ProjectPad {
    type Error = ProjectError;

    fn try_from(value: WireV2ProjectPad) -> Result<Self, Self::Error> {
        let digest = decode_hex(&value.asset_digest)
            .map(AssetDigest::from_bytes)
            .ok_or(ProjectError::InvalidAssetDigest)?;
        ProjectPad::new(
            value.pad.try_into()?,
            value.audio_path,
            digest,
            value.settings.try_into()?,
            PadMixSettings::default(),
            value.recipe.try_into()?,
        )
    }
}

impl TryFrom<WireV3ProjectPad> for ProjectPad {
    type Error = ProjectError;

    fn try_from(value: WireV3ProjectPad) -> Result<Self, Self::Error> {
        let digest = decode_hex(&value.asset_digest)
            .map(AssetDigest::from_bytes)
            .ok_or(ProjectError::InvalidAssetDigest)?;
        ProjectPad::new(
            value.pad.try_into()?,
            value.audio_path,
            digest,
            value.settings.try_into()?,
            value.mix.try_into()?,
            value.recipe.try_into()?,
        )
    }
}

impl TryFrom<WireV2ProjectPattern> for ProjectPattern {
    type Error = ProjectError;

    fn try_from(value: WireV2ProjectPattern) -> Result<Self, Self::Error> {
        let pattern = Self {
            slot: PatternSlotId::new(value.slot).map_err(ProjectError::InvalidPattern)?,
            name: value.name,
            sample_rate: value.sample_rate,
            tempo: Tempo::new(value.tempo).map_err(ProjectError::InvalidModel)?,
            meter: Meter::new(value.meter.numerator, value.meter.denominator)
                .map_err(ProjectError::InvalidModel)?,
            bars: value.bars,
            resolution: value.resolution,
            swing: value.swing,
            quantize_strength: value.quantize_strength,
            events: value
                .events
                .into_iter()
                .map(ProjectPatternEvent::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        };
        pattern.validate()?;
        Ok(pattern)
    }
}

fn migrate_v2_to_v3(value: WireV2ProjectDocument) -> Result<ProjectDocument, ProjectError> {
    if value.schema_version != 2 {
        return Err(ProjectError::UnsupportedSchema(value.schema_version));
    }
    let project_id = decode_hex(&value.project_id)
        .map(ProjectId::from_bytes)
        .ok_or(ProjectError::InvalidProjectId)?;
    ProjectDocument::new_v3(
        project_id,
        value.name,
        value.revision,
        value
            .pads
            .into_iter()
            .map(ProjectPad::try_from)
            .collect::<Result<Vec<_>, _>>()?,
        value
            .patterns
            .into_iter()
            .map(ProjectPattern::try_from)
            .collect::<Result<Vec<_>, _>>()?,
        MasterMixSettings::default(),
    )
}

impl TryFrom<WireV3ProjectDocument> for ProjectDocument {
    type Error = ProjectError;

    fn try_from(value: WireV3ProjectDocument) -> Result<Self, Self::Error> {
        if value.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ProjectError::UnsupportedSchema(value.schema_version));
        }
        let project_id = decode_hex(&value.project_id)
            .map(ProjectId::from_bytes)
            .ok_or(ProjectError::InvalidProjectId)?;
        Self::new_v3(
            project_id,
            value.name,
            value.revision,
            value
                .pads
                .into_iter()
                .map(ProjectPad::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            value
                .patterns
                .into_iter()
                .map(ProjectPattern::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            value.master_mix.try_into()?,
        )
    }
}

impl TryFrom<&ProjectDocument> for WireV3ProjectDocument {
    type Error = ProjectError;

    fn try_from(value: &ProjectDocument) -> Result<Self, Self::Error> {
        value.validate()?;
        Ok(Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            project_id: encode_hex(value.project_id.as_bytes()),
            name: value.name.clone(),
            revision: value.revision,
            pads: value
                .pads
                .iter()
                .map(|pad| WireV3ProjectPad {
                    pad: pad.pad.into(),
                    audio_path: pad.audio_path.clone(),
                    asset_digest: encode_hex(pad.asset_digest.as_bytes()),
                    settings: pad.settings.into(),
                    mix: pad.mix.into(),
                    recipe: pad.recipe.into(),
                })
                .collect(),
            patterns: value
                .patterns
                .iter()
                .map(|pattern| WireV2ProjectPattern {
                    slot: pattern.slot.get(),
                    name: pattern.name.clone(),
                    sample_rate: pattern.sample_rate,
                    tempo: pattern.tempo.bpm(),
                    meter: WireMeter {
                        numerator: pattern.meter.numerator(),
                        denominator: pattern.meter.denominator(),
                    },
                    bars: pattern.bars,
                    resolution: pattern.resolution,
                    swing: pattern.swing,
                    quantize_strength: pattern.quantize_strength,
                    events: pattern.events.iter().copied().map(Into::into).collect(),
                })
                .collect(),
            master_mix: value.master_mix.into(),
        })
    }
}

impl TryFrom<WireV1ProjectDocument> for LegacyProjectDocument {
    type Error = ProjectError;

    fn try_from(value: WireV1ProjectDocument) -> Result<Self, Self::Error> {
        if value.schema_version != 1 {
            return Err(ProjectError::UnsupportedSchema(value.schema_version));
        }
        if value.name.trim().is_empty() {
            return Err(ProjectError::InvalidName);
        }
        if value.patterns.len() > PATTERN_SLOT_COUNT {
            return Err(ProjectError::TooManyPatterns);
        }

        let pads = value
            .pads
            .into_iter()
            .map(|pad| {
                validate_legacy_audio_path(&pad.audio_path)?;
                Ok(LegacyProjectPad {
                    pad: pad.pad.try_into()?,
                    audio_path: pad.audio_path,
                    settings: pad.settings.try_into()?,
                })
            })
            .collect::<Result<Vec<_>, ProjectError>>()?;
        let patterns = value
            .patterns
            .into_iter()
            .enumerate()
            .map(|(slot, pattern)| {
                let pattern = LegacyProjectPattern {
                    slot: PatternSlotId::new(slot as u8).map_err(ProjectError::InvalidPattern)?,
                    name: pattern.name,
                    sample_rate: pattern.sample_rate,
                    tempo: Tempo::new(pattern.tempo).map_err(ProjectError::InvalidModel)?,
                    meter: Meter::new(pattern.meter.numerator, pattern.meter.denominator)
                        .map_err(ProjectError::InvalidModel)?,
                    bars: pattern.bars,
                    resolution: pattern.resolution,
                    swing: pattern.swing,
                    events: pattern
                        .events
                        .into_iter()
                        .map(PatternEvent::try_from)
                        .collect::<Result<Vec<_>, _>>()?,
                };
                pattern.validate()?;
                Ok(pattern)
            })
            .collect::<Result<Vec<_>, ProjectError>>()?;

        let mut seen = HashSet::with_capacity(pads.len());
        for pad in &pads {
            if !seen.insert(pad.pad) {
                return Err(ProjectError::DuplicatePad(pad.pad));
            }
        }
        Ok(Self {
            schema_version: 1,
            name: value.name,
            pads,
            patterns,
        })
    }
}

fn validate_legacy_audio_path(value: &str) -> Result<(), ProjectError> {
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

fn validate_current_audio_path(
    value: &str,
    expected_digest: AssetDigest,
) -> Result<(), ProjectError> {
    let path = Path::new(value);
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Err(ProjectError::InvalidAudioPath(value.to_owned()));
    };
    let Some(Component::Normal(file_name)) = components.next() else {
        return Err(ProjectError::InvalidAudioPath(value.to_owned()));
    };
    if first != "audio" || components.next().is_some() {
        return Err(ProjectError::InvalidAudioPath(value.to_owned()));
    }
    let Some(file_name) = file_name.to_str() else {
        return Err(ProjectError::InvalidAudioPath(value.to_owned()));
    };
    let Some((digest, extension)) = file_name.rsplit_once('.') else {
        return Err(ProjectError::InvalidAudioPath(value.to_owned()));
    };
    if !matches!(extension, "wav" | "aif" | "aiff" | "flac" | "mp3") {
        return Err(ProjectError::InvalidAudioPath(value.to_owned()));
    }
    let digest = decode_hex::<32>(digest).ok_or(ProjectError::InvalidAssetDigest)?;
    if digest != *expected_digest.as_bytes() {
        return Err(ProjectError::AssetDigestMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BankId, DelaySettings, EditablePattern, MasterMixSettings, PadId, PadMixSettings,
        PadSettings, PatternSlotId, ReverbSettings, SAMPLE_PHASE_SCALE, SampleEditRecipe,
    };

    fn digest(byte: u8) -> AssetDigest {
        AssetDigest::from_bytes([byte; 32])
    }

    fn current_audio_path(byte: u8, extension: &str) -> String {
        format!("audio/{}.{extension}", encode_hex(&[byte; 32]))
    }

    fn empty_project(name: &str) -> ProjectDocument {
        ProjectDocument::new_v3(
            ProjectId::from_bytes([0x10; 16]),
            name,
            0,
            Vec::new(),
            Vec::new(),
            MasterMixSettings::default(),
        )
        .unwrap()
    }

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
        let mut project = empty_project("beat-one");
        project.pads.push(
            ProjectPad::new(
                PadId::first(),
                current_audio_path(0x11, "wav"),
                digest(0x11),
                PadSettings::default(),
                PadMixSettings::default(),
                SampleEditRecipe::identity(),
            )
            .unwrap(),
        );
        let encoded = project.to_toml().unwrap();
        assert!(encoded.contains("schema_version = 3"));
        assert_eq!(
            ProjectDocument::from_toml(&encoded).unwrap(),
            ParsedProjectDocument::Current(project)
        );
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
        for path in ["/tmp/kick.wav".to_owned(), "../kick.wav".to_owned()] {
            assert!(
                ProjectPad::new(
                    PadId::first(),
                    path,
                    digest(0x22),
                    PadSettings::default(),
                    PadMixSettings::default(),
                    SampleEditRecipe::identity(),
                )
                .is_err()
            );
        }
        assert!(
            ProjectPad::new(
                PadId::first(),
                current_audio_path(0x22, "wav"),
                digest(0x22),
                PadSettings::default(),
                PadMixSettings::default(),
                SampleEditRecipe::identity(),
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_duplicate_pads_before_serializing() {
        let mut project = empty_project("beat-one");
        let pad = ProjectPad::new(
            PadId::new(BankId::new(0).unwrap(), 1).unwrap(),
            current_audio_path(0x33, "flac"),
            digest(0x33),
            PadSettings::default(),
            PadMixSettings::default(),
            SampleEditRecipe::identity(),
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

    #[test]
    fn project_rejects_more_than_sixteen_patterns() {
        let mut project = empty_project("many");
        project.patterns = (0..17)
            .map(|index| ProjectPattern {
                slot: PatternSlotId::new((index % PATTERN_SLOT_COUNT) as u8).unwrap(),
                name: format!("pattern-{index}"),
                sample_rate: 48_000,
                tempo: Tempo::new(120.0).unwrap(),
                meter: Meter::new(4, 4).unwrap(),
                bars: 1,
                resolution: Resolution::Sixteenth,
                swing: 0.5,
                quantize_strength: 0.0,
                events: Vec::new(),
            })
            .collect();
        assert_eq!(project.to_toml(), Err(ProjectError::TooManyPatterns));
    }

    #[test]
    fn project_rejects_more_than_one_thousand_twenty_four_events() {
        let mut project = empty_project("many-events");
        project.patterns.clear();
        project.patterns.push(ProjectPattern {
            slot: PatternSlotId::new(0).unwrap(),
            name: "dense".into(),
            sample_rate: 48_000,
            tempo: Tempo::new(120.0).unwrap(),
            meter: Meter::new(4, 4).unwrap(),
            bars: 1,
            resolution: Resolution::Sixteenth,
            swing: 0.5,
            quantize_strength: 0.0,
            events: (1..=1_025)
                .map(|id| ProjectPatternEvent {
                    event: PatternEvent::new(EventId(id), PadId::first(), id - 1, 1.0, None)
                        .unwrap(),
                    raw_frame: id - 1,
                })
                .collect(),
        });
        assert_eq!(
            project.to_toml(),
            Err(ProjectError::TooManyPatternEvents {
                pattern: "dense".into()
            })
        );
    }

    const V1_FIXTURE: &str = r#"
schema_version = 1
name = "legacy"

[[pads]]
audio_path = "audio/kick.wav"

[pads.pad]
bank = 0
index = 0

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0

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
frame = 6800
velocity = 1.0

[patterns.events.pad]
bank = 0
index = 0
"#;

    const SCHEMA_V2_LITERAL: &str = r#"
schema_version = 2
project_id = "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a"
name = "literal-v2"
revision = 41

[[pads]]
audio_path = "audio/6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d.wav"
asset_digest = "6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d"

[pads.pad]
bank = 2
index = 3

[pads.settings]
mode = "Gate"
gain_db = -3.0
pan = 0.25
pitch_semitones = 2.0
choke_group = 4

[pads.recipe]
start_phase = 1
end_phase = 4294967296
reversed = true
normalize = true

[[patterns]]
slot = 7
name = "literal pattern"
sample_rate = 48000
tempo = 123.0
bars = 2
resolution = "eighth"
swing = 0.6
quantize_strength = 0.75

[patterns.meter]
numerator = 3
denominator = 4

[[patterns.events]]
id = 9
frame = 0
raw_frame = 0
velocity = 0.75
duration = 2400
original_offset = 0

[patterns.events.pad]
bank = 2
index = 3
"#;

    const SCHEMA_V3_LITERAL: &str = r#"
schema_version = 3
project_id = "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a"
name = "literal-v3"
revision = 42
patterns = []

[master_mix]
gain_db = -6.0

[master_mix.delay]
enabled = true
time_ms = 640
feedback = 0.625
return_db = -9.0

[master_mix.reverb]
enabled = true
room_size = 0.8
damping = 0.2
return_db = -7.0

[[pads]]
audio_path = "audio/5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c.wav"
asset_digest = "5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c"

[pads.pad]
bank = 0
index = 0

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0

[pads.mix]
muted = true
delay_send = 0.25
reverb_send = 0.75

[pads.recipe]
start_phase = 0
end_phase = 4294967296
reversed = false
normalize = false
"#;

    fn quantized_pattern(slot: PatternSlotId) -> EditablePattern {
        let transport = Transport::new(
            48_000,
            Tempo::new(120.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        let mut pattern = EditablePattern::new(slot, "beat", transport).unwrap();
        pattern
            .insert(PatternEvent::new(EventId(9), PadId::first(), 6_800, 0.75, None).unwrap())
            .unwrap();
        pattern.set_quantize_strength(1.0).unwrap();
        pattern
    }

    fn project_pad_with_recipe(digest: AssetDigest) -> ProjectPad {
        ProjectPad::new(
            PadId::first(),
            "audio/5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c.wav",
            digest,
            PadSettings::default(),
            PadMixSettings::default(),
            SampleEditRecipe::new(1, SAMPLE_PHASE_SCALE, true, true).unwrap(),
        )
        .unwrap()
    }

    fn current_document() -> ProjectDocument {
        let id = ProjectId::from_bytes([0x2a; 16]);
        let digest = AssetDigest::from_bytes([0x5c; 32]);
        ProjectDocument::new_v3(
            id,
            "beat",
            41,
            vec![project_pad_with_recipe(digest)],
            vec![
                ProjectPattern::from_editable(&quantized_pattern(PatternSlotId::new(7).unwrap()))
                    .unwrap(),
            ],
            MasterMixSettings::default(),
        )
        .unwrap()
    }

    fn mixer_document_fixture() -> ProjectDocument {
        let pad_mix = PadMixSettings::new(true, 0.25, 0.75).unwrap();
        let master_mix = MasterMixSettings::new(
            -6.0,
            DelaySettings::new(true, 640, 0.625, -9.0).unwrap(),
            ReverbSettings::new(true, 0.8, 0.2, -7.0).unwrap(),
        )
        .unwrap();
        ProjectDocument::new_v3(
            ProjectId::from_bytes([0x2a; 16]),
            "mixer round trip",
            42,
            vec![
                ProjectPad::new(
                    PadId::first(),
                    current_audio_path(0x5c, "wav"),
                    digest(0x5c),
                    PadSettings::default(),
                    pad_mix,
                    SampleEditRecipe::identity(),
                )
                .unwrap(),
            ],
            Vec::new(),
            master_mix,
        )
        .unwrap()
    }

    #[test]
    fn schema_v2_migrates_to_current_dry_mixer_defaults() {
        let ParsedProjectDocument::Current(project) =
            ProjectDocument::from_toml(SCHEMA_V2_LITERAL).unwrap()
        else {
            panic!("v2 must migrate directly to current")
        };

        assert_eq!(project.schema_version, 3);
        assert_eq!(project.project_id, ProjectId::from_bytes([0x2a; 16]));
        assert_eq!(project.name, "literal-v2");
        assert_eq!(project.revision, 41);
        assert_eq!(project.master_mix, MasterMixSettings::default());
        assert_eq!(project.pads.len(), 1);
        let pad = &project.pads[0];
        assert_eq!(pad.pad, PadId::new(BankId::new(2).unwrap(), 3).unwrap());
        assert_eq!(pad.audio_path, current_audio_path(0x6d, "wav"));
        assert_eq!(pad.asset_digest, digest(0x6d));
        assert_eq!(pad.settings.mode, PlaybackMode::Gate);
        assert_eq!(pad.settings.gain_db, -3.0);
        assert_eq!(pad.settings.pan, 0.25);
        assert_eq!(pad.settings.pitch_semitones, 2.0);
        assert_eq!(pad.settings.choke_group, Some(ChokeGroup::new(4).unwrap()));
        assert_eq!(pad.recipe.start_phase, 1);
        assert_eq!(pad.recipe.end_phase, SAMPLE_PHASE_SCALE);
        assert!(pad.recipe.reversed);
        assert!(pad.recipe.normalize);
        assert_eq!(pad.mix, PadMixSettings::default());

        assert_eq!(project.patterns.len(), PATTERN_SLOT_COUNT);
        let pattern = &project.patterns[0];
        assert_eq!(pattern.slot, PatternSlotId::new(7).unwrap());
        assert_eq!(pattern.name, "literal pattern");
        assert_eq!(pattern.sample_rate, 48_000);
        assert_eq!(pattern.tempo, Tempo::new(123.0).unwrap());
        assert_eq!(pattern.meter, Meter::new(3, 4).unwrap());
        assert_eq!(pattern.bars, 2);
        assert_eq!(pattern.resolution, Resolution::Eighth);
        assert_eq!(pattern.swing, 0.6);
        assert_eq!(pattern.quantize_strength, 0.75);
        assert_eq!(pattern.events.len(), 1);
        assert_eq!(pattern.events[0].event.id, EventId(9));
        assert_eq!(pattern.events[0].event.pad, pad.pad);
        assert_eq!(pattern.events[0].event.frame, 0);
        assert_eq!(pattern.events[0].raw_frame, 0);
        assert_eq!(pattern.events[0].event.velocity, 0.75);
        assert_eq!(pattern.events[0].event.duration, Some(2_400));
        assert_eq!(pattern.events[0].event.original_offset, Some(0));
    }

    #[test]
    fn schema_v3_round_trip_preserves_every_mixer_field() {
        let document = mixer_document_fixture();
        let encoded = document.to_toml().unwrap();
        assert!(encoded.contains("schema_version = 3"));
        assert_eq!(
            ProjectDocument::from_toml(&encoded).unwrap(),
            ParsedProjectDocument::Current(document)
        );
    }

    #[test]
    fn schema_v3_literal_preserves_every_mixer_field() {
        let ParsedProjectDocument::Current(project) =
            ProjectDocument::from_toml(SCHEMA_V3_LITERAL).unwrap()
        else {
            panic!("v3 must parse as current")
        };
        assert_eq!(
            project.pads[0].mix,
            PadMixSettings::new(true, 0.25, 0.75).unwrap()
        );
        assert_eq!(
            project.master_mix,
            MasterMixSettings::new(
                -6.0,
                DelaySettings::new(true, 640, 0.625, -9.0).unwrap(),
                ReverbSettings::new(true, 0.8, 0.2, -7.0).unwrap(),
            )
            .unwrap()
        );
    }

    #[test]
    fn schema_v3_rejects_nonfinite_and_out_of_range_mixer_values() {
        let invalid_sources = [
            SCHEMA_V3_LITERAL.replace("delay_send = 0.25", "delay_send = nan"),
            SCHEMA_V3_LITERAL.replace("reverb_send = 0.75", "reverb_send = +inf"),
            SCHEMA_V3_LITERAL.replace("gain_db = -6.0", "gain_db = -inf"),
            SCHEMA_V3_LITERAL.replace("delay_send = 0.25", "delay_send = 1.01"),
            SCHEMA_V3_LITERAL.replace("reverb_send = 0.75", "reverb_send = -0.01"),
            SCHEMA_V3_LITERAL.replace("gain_db = -6.0", "gain_db = 6.01"),
            SCHEMA_V3_LITERAL.replace("time_ms = 640", "time_ms = 9"),
            SCHEMA_V3_LITERAL.replace("feedback = 0.625", "feedback = 0.951"),
            SCHEMA_V3_LITERAL.replacen("return_db = -9.0", "return_db = -60.01", 1),
            SCHEMA_V3_LITERAL.replace("room_size = 0.8", "room_size = 1.01"),
            SCHEMA_V3_LITERAL.replace("damping = 0.2", "damping = -0.01"),
            SCHEMA_V3_LITERAL.replace("return_db = -7.0", "return_db = 6.01"),
        ];
        for invalid in invalid_sources {
            assert!(
                ProjectDocument::from_toml(&invalid).is_err(),
                "accepted invalid mixer wire value: {invalid}"
            );
        }
    }

    #[test]
    fn schema_v3_rejects_unknown_fields_in_every_mixer_table() {
        let invalid_sources = [
            SCHEMA_V3_LITERAL.replace("muted = true", "muted = true\nunknown_pad_mix = 1"),
            SCHEMA_V3_LITERAL.replace("gain_db = -6.0", "gain_db = -6.0\nunknown_master = 1"),
            SCHEMA_V3_LITERAL.replacen("enabled = true", "enabled = true\nunknown_delay = 1", 1),
            SCHEMA_V3_LITERAL.replace("room_size = 0.8", "room_size = 0.8\nunknown_reverb = 1"),
        ];
        for invalid in invalid_sources {
            assert!(
                matches!(
                    ProjectDocument::from_toml(&invalid),
                    Err(ProjectError::TomlSyntax(_))
                ),
                "accepted unknown mixer field: {invalid}"
            );
        }
    }

    #[test]
    fn schema_v3_rejects_duplicate_pads_from_literal_wire_data() {
        let duplicate = format!(
            "{SCHEMA_V3_LITERAL}\n{}",
            r#"
[[pads]]
audio_path = "audio/6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d.wav"
asset_digest = "6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d"

[pads.pad]
bank = 0
index = 0

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0

[pads.mix]
muted = false
delay_send = 0.0
reverb_send = 0.0

[pads.recipe]
start_phase = 0
end_phase = 4294967296
reversed = false
normalize = false
"#
        );
        assert_eq!(
            ProjectDocument::from_toml(&duplicate),
            Err(ProjectError::DuplicatePad(PadId::first()))
        );
    }

    #[test]
    fn schema_four_is_rejected_as_newer_than_schema_three() {
        let schema_four = SCHEMA_V3_LITERAL.replacen("schema_version = 3", "schema_version = 4", 1);
        assert_eq!(
            ProjectDocument::from_toml(&schema_four),
            Err(ProjectError::NewerSchema {
                found: 4,
                supported: 3,
            })
        );
    }

    #[test]
    fn schema_v3_round_trip_preserves_id_revision_recipe_slot_and_raw_quantize_state() {
        let document = current_document();
        let encoded = document.to_toml().unwrap();
        assert!(encoded.contains("raw_frame = 6800"));
        let decoded = ProjectDocument::from_toml(&encoded).unwrap();
        assert_eq!(decoded, ParsedProjectDocument::Current(document.clone()));
        assert_eq!(
            decoded.current().unwrap().patterns[0]
                .to_editable()
                .unwrap()
                .quantize_strength(),
            1.0
        );
        assert_eq!(
            decoded.current().unwrap().patterns[0]
                .to_editable()
                .unwrap()
                .event(EventId(9))
                .unwrap()
                .original_offset,
            Some(800)
        );
    }

    #[test]
    fn schema_v3_completes_sparse_patterns_without_reordering_existing_slots() {
        let existing =
            ProjectPattern::from_editable(&quantized_pattern(PatternSlotId::new(7).unwrap()))
                .unwrap();
        let project = ProjectDocument::new_v3(
            ProjectId::from_bytes([0x4d; 16]),
            "sparse",
            1,
            Vec::new(),
            vec![existing.clone()],
            MasterMixSettings::default(),
        )
        .unwrap();

        assert_eq!(project.patterns.len(), PATTERN_SLOT_COUNT);
        assert_eq!(project.patterns[0], existing);
        assert_eq!(
            project.patterns[1..]
                .iter()
                .map(ProjectPattern::slot)
                .collect::<Vec<_>>(),
            (0..PATTERN_SLOT_COUNT as u8)
                .filter(|slot| *slot != 7)
                .map(|slot| PatternSlotId::new(slot).unwrap())
                .collect::<Vec<_>>()
        );
        for pattern in &project.patterns[1..] {
            assert!(pattern.events.is_empty());
            assert_eq!(pattern.sample_rate, 48_000);
            assert_eq!(pattern.quantize_strength, 0.0);
        }
    }

    #[test]
    fn schema_v1_parses_as_legacy_without_inventing_digest_or_project_id() {
        let parsed = ProjectDocument::from_toml(V1_FIXTURE).unwrap();
        let ParsedProjectDocument::Legacy(legacy) = parsed else {
            panic!("expected v1")
        };
        assert_eq!(legacy.revision(), 0);
        assert_eq!(legacy.pads()[0].recipe(), SampleEditRecipe::identity());
        assert_eq!(legacy.patterns()[0].slot(), PatternSlotId::new(0).unwrap());
        assert_eq!(legacy.to_toml(), Err(ProjectError::LegacyNeedsMigration));

        let with_legacy_offset =
            V1_FIXTURE.replace("velocity = 1.0", "velocity = 1.0\noriginal_offset = 123");
        let ParsedProjectDocument::Legacy(with_legacy_offset) =
            ProjectDocument::from_toml(&with_legacy_offset).unwrap()
        else {
            panic!("expected v1")
        };
        let editable = with_legacy_offset.patterns()[0]
            .to_editable_lossy()
            .unwrap();
        assert_eq!(editable.event(EventId(1)).unwrap().frame, 6_800);
        assert_eq!(
            editable.event(EventId(1)).unwrap().original_offset,
            Some(800)
        );
        assert_eq!(editable.quantize_strength(), 0.0);
    }

    #[test]
    fn fixed_byte_ids_use_exact_lowercase_hex_at_the_wire_boundary() {
        let id = ProjectId::from_bytes([0xab; 16]);
        let digest = AssetDigest::from_bytes([0xcd; 32]);
        assert_eq!(id.to_string(), "ab".repeat(16));
        assert_eq!(digest.to_string(), "cd".repeat(32));
        assert_eq!(id.to_string().parse::<ProjectId>(), Ok(id));
        assert_eq!(digest.to_string().parse::<AssetDigest>(), Ok(digest));
        assert_eq!(
            "AB".repeat(16).parse::<ProjectId>(),
            Err(ProjectError::InvalidProjectId)
        );
        assert_eq!(
            "cd".repeat(31).parse::<AssetDigest>(),
            Err(ProjectError::InvalidAssetDigest)
        );
    }

    #[test]
    fn schema_v3_rejects_noncanonical_or_mismatched_asset_names() {
        let encoded = current_document().to_toml().unwrap();
        let digest = "5c".repeat(32);
        for invalid in [
            encoded.replace(&digest, &digest.to_ascii_uppercase()),
            encoded.replace(&digest, &"5c".repeat(31)),
            encoded.replace(
                &format!("audio/{digest}.wav"),
                &format!("audio/{}.wav", digest.to_ascii_uppercase()),
            ),
            encoded.replace(
                &format!("audio/{digest}.wav"),
                &format!("audio/{}.wav", "5c".repeat(31)),
            ),
            encoded.replace(&format!("audio/{digest}.wav"), "audio/not-a-digest.wav"),
            encoded.replace(
                &format!("audio/{digest}.wav"),
                &format!("audio/{}.wav", "6d".repeat(32)),
            ),
            encoded.replace(".wav", ".ogg"),
        ] {
            assert!(
                ProjectDocument::from_toml(&invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn schema_v3_rejects_duplicate_slots_but_allows_duplicate_names_in_distinct_slots() {
        let first = quantized_pattern(PatternSlotId::new(1).unwrap());
        let second = quantized_pattern(PatternSlotId::new(2).unwrap());
        let patterns = vec![
            ProjectPattern::from_editable(&first).unwrap(),
            ProjectPattern::from_editable(&second).unwrap(),
        ];
        assert!(
            ProjectDocument::new_v3(
                ProjectId::from_bytes([1; 16]),
                "same names are fine",
                0,
                Vec::new(),
                patterns,
                MasterMixSettings::default(),
            )
            .is_ok()
        );

        let duplicate = ProjectPattern::from_editable(&first).unwrap();
        assert_eq!(
            ProjectDocument::new_v3(
                ProjectId::from_bytes([1; 16]),
                "duplicate slot",
                0,
                Vec::new(),
                vec![duplicate.clone(), duplicate],
                MasterMixSettings::default(),
            ),
            Err(ProjectError::DuplicatePatternSlot(
                PatternSlotId::new(1).unwrap()
            ))
        );
    }

    #[test]
    fn schema_v3_rejects_revision_recipe_offsets_max_event_unknown_fields_and_future_schema() {
        assert_eq!(
            ProjectDocument::new_v3(
                ProjectId::from_bytes([1; 16]),
                "revision",
                i64::MAX as u64 + 1,
                Vec::new(),
                Vec::new(),
                MasterMixSettings::default(),
            ),
            Err(ProjectError::InvalidRevision(i64::MAX as u64 + 1))
        );

        let encoded = current_document().to_toml().unwrap();
        let invalid_sources = [
            encoded.replace(
                &format!("end_phase = {SAMPLE_PHASE_SCALE}"),
                &format!("end_phase = {}", SAMPLE_PHASE_SCALE + 1),
            ),
            encoded.replace("original_offset = 800", "original_offset = -6001"),
            encoded.replace("id = 9", &format!("id = {}", u64::MAX)),
            encoded.replacen("name = \"beat\"", "name = \"beat\"\npreview = []", 1),
            encoded.replace("normalize = true", "normalize = true\npreview = []"),
            encoded.replace("pitch_semitones = 0.0", "pitch_semitones = 0.0\npcm = []"),
            encoded.replace("velocity = 0.75", "velocity = 0.75\ngeneration = 9"),
        ];
        for invalid in invalid_sources {
            assert!(
                ProjectDocument::from_toml(&invalid).is_err(),
                "accepted {invalid}"
            );
        }

        let future = encoded.replacen("schema_version = 3", "schema_version = 4", 1);
        assert_eq!(
            ProjectDocument::from_toml(&future),
            Err(ProjectError::NewerSchema {
                found: 4,
                supported: CURRENT_SCHEMA_VERSION,
            })
        );
    }

    #[test]
    fn schema_v3_nested_validation_rejects_candidates_without_partial_documents() {
        let base = current_document();

        let mut duplicate_pad = base.clone();
        duplicate_pad.pads.push(duplicate_pad.pads[0].clone());
        assert!(matches!(
            ProjectDocument::new_v3(
                duplicate_pad.project_id,
                duplicate_pad.name,
                duplicate_pad.revision,
                duplicate_pad.pads,
                duplicate_pad.patterns,
                duplicate_pad.master_mix,
            ),
            Err(ProjectError::DuplicatePad(_))
        ));

        let mut invalid_settings = base.clone();
        invalid_settings.pads[0].settings.pan = 2.0;
        assert_eq!(
            ProjectDocument::new_v3(
                invalid_settings.project_id,
                invalid_settings.name,
                invalid_settings.revision,
                invalid_settings.pads,
                invalid_settings.patterns,
                invalid_settings.master_mix,
            ),
            Err(ProjectError::InvalidModel(ModelError::PanOutOfRange))
        );

        let mut duplicate_event = base.clone();
        let event = duplicate_event.patterns[0].events[0];
        duplicate_event.patterns[0].events.push(event);
        assert_eq!(
            ProjectDocument::new_v3(
                duplicate_event.project_id,
                duplicate_event.name,
                duplicate_event.revision,
                duplicate_event.pads,
                duplicate_event.patterns,
                duplicate_event.master_mix,
            ),
            Err(ProjectError::InvalidModel(ModelError::DuplicateEvent))
        );

        let mut invalid_raw = base.clone();
        invalid_raw.patterns[0].events[0].raw_frame = invalid_raw.patterns[0]
            .to_editable()
            .unwrap()
            .transport()
            .loop_frames();
        assert_eq!(
            ProjectDocument::new_v3(
                invalid_raw.project_id,
                invalid_raw.name,
                invalid_raw.revision,
                invalid_raw.pads,
                invalid_raw.patterns,
                invalid_raw.master_mix,
            ),
            Err(ProjectError::InvalidModel(ModelError::InvalidEvent))
        );
    }
}
