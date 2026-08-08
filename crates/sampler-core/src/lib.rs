pub mod error;
pub mod pad;
pub mod pattern;
pub mod project;
pub mod sample_edit;
pub mod transport;
pub mod voice;

pub use error::{PatternCompileError, PatternEditError, SampleEditError};
pub use pad::{BankId, ChokeGroup, ModelError, PadId, PadSettings, PlaybackMode};
pub use pattern::{
    EditablePattern, EventId, FIRST_LOOP_VALID_MASK_WORDS, MAX_PATTERN_ACTIONS, MAX_PATTERN_EVENTS,
    PATTERN_SLOT_COUNT, Pattern, PatternAction, PatternActionKind, PatternEvent, PatternSlotId,
    PatternSnapshot, ScheduleResult, ScheduledEvent,
};
pub use project::{
    AssetDigest, CURRENT_SCHEMA_VERSION, LegacyProjectDocument, LegacyProjectPad,
    LegacyProjectPattern, ParsedProjectDocument, ProjectDocument, ProjectError, ProjectId,
    ProjectPad, ProjectPattern, ProjectPatternEvent,
};
pub use sample_edit::{SAMPLE_PHASE_SCALE, SampleEditPlan, SampleEditRecipe, apply_sample_edit};
pub use transport::{Meter, Resolution, Tempo, Transport};
pub use voice::{Allocation, Voice, VoiceAllocator, VoiceId, VoiceRequest};

pub type Frame = u64;
