use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sampler_core::{MasterMixSettings, PadId, PatternSlotId, ProjectId, ProjectPattern};

use crate::ProjectSavePad;

pub const EXPORT_SAMPLE_RATE: u32 = 48_000;
pub const EXPORT_CHUNK_FRAMES: usize = 4_096;

/// A user-facing, one-based pattern slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportPatternSlot(PatternSlotId);

impl ExportPatternSlot {
    pub const fn slot(self) -> PatternSlotId {
        self.0
    }

    pub fn get(self) -> u8 {
        self.0.get() + 1
    }
}

impl TryFrom<u8> for ExportPatternSlot {
    type Error = OfflineExportError;

    fn try_from(one_based: u8) -> Result<Self, Self::Error> {
        let zero_based = one_based
            .checked_sub(1)
            .ok_or(OfflineExportError::PatternSlot(one_based))?;
        PatternSlotId::new(zero_based)
            .map(Self)
            .map_err(|_| OfflineExportError::PatternSlot(one_based))
    }
}

impl From<ExportPatternSlot> for PatternSlotId {
    fn from(value: ExportPatternSlot) -> Self {
        value.slot()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExportToken(u64);

impl ExportToken {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A cooperative cancellation handle shared by the caller and offline worker.
#[derive(Clone, Default)]
pub struct OfflineExportCancellation(Arc<AtomicBool>);

impl std::fmt::Debug for OfflineExportCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OfflineExportCancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl OfflineExportCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// An immutable, device-independent description of exactly one pattern export.
#[derive(Debug, Clone, PartialEq)]
pub struct OfflineExportSnapshot {
    pub project_id: ProjectId,
    pub revision: u64,
    pub slot: PatternSlotId,
    pub pattern: ProjectPattern,
    pub pads: Vec<ProjectSavePad>,
    pub master_mix: MasterMixSettings,
    pub sample_rate: u32,
}

impl OfflineExportSnapshot {
    pub fn new(
        project_id: ProjectId,
        revision: u64,
        slot: PatternSlotId,
        pattern: ProjectPattern,
        pads: Vec<ProjectSavePad>,
        master_mix: MasterMixSettings,
    ) -> Result<Self, OfflineExportError> {
        let snapshot = Self {
            project_id,
            revision,
            slot,
            pattern,
            pads,
            master_mix,
            sample_rate: EXPORT_SAMPLE_RATE,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), OfflineExportError> {
        if self.sample_rate != EXPORT_SAMPLE_RATE {
            return Err(OfflineExportError::SampleRate(self.sample_rate));
        }
        if self.slot != self.pattern.slot() {
            return Err(OfflineExportError::PatternSlotMismatch {
                selected: self.slot,
                pattern: self.pattern.slot(),
            });
        }
        if self.pattern.events.is_empty() {
            return Err(OfflineExportError::EmptyPattern);
        }
        for source in &self.pads {
            if source.source_path.as_os_str().is_empty() {
                return Err(OfflineExportError::MissingPadSource { pad: source.pad });
            }
            if self
                .pads
                .iter()
                .filter(|other| other.pad == source.pad)
                .count()
                > 1
            {
                return Err(OfflineExportError::DuplicatePadSource { pad: source.pad });
            }
            if !self
                .pattern
                .events
                .iter()
                .any(|event| event.event.pad == source.pad)
            {
                return Err(OfflineExportError::UnreferencedPadSource { pad: source.pad });
            }
        }
        for event in &self.pattern.events {
            if !self.pads.iter().any(|source| source.pad == event.event.pad) {
                return Err(OfflineExportError::MissingPadSource {
                    pad: event.event.pad,
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct OfflineExportRequest {
    pub token: ExportToken,
    pub destination: PathBuf,
    pub snapshot: OfflineExportSnapshot,
    pub cancellation: OfflineExportCancellation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineExportReceipt {
    pub token: ExportToken,
    pub destination: PathBuf,
    pub project_id: ProjectId,
    pub revision: u64,
    pub slot: PatternSlotId,
    pub sample_rate: u32,
    pub rendered_frames: u64,
    pub file_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfflineExportError {
    #[error("pattern slot must be 1..=16; received {0}")]
    PatternSlot(u8),
    #[error("selected pattern slot {selected:?} does not match pattern slot {pattern:?}")]
    PatternSlotMismatch {
        selected: PatternSlotId,
        pattern: PatternSlotId,
    },
    #[error("offline export sample rate must be {EXPORT_SAMPLE_RATE}; received {0}")]
    SampleRate(u32),
    #[error("offline export requires a non-empty pattern")]
    EmptyPattern,
    #[error("pattern references pad {pad:?} without a committed source")]
    MissingPadSource { pad: PadId },
    #[error("pattern snapshot contains a duplicate committed source for pad {pad:?}")]
    DuplicatePadSource { pad: PadId },
    #[error("pattern snapshot contains an unreferenced committed source for pad {pad:?}")]
    UnreferencedPadSource { pad: PadId },
    #[error("offline export destination must have a .wav extension: {0}")]
    DestinationExtension(PathBuf),
    #[error("offline export destination already exists: {0}")]
    OutputCollision(PathBuf),
    #[error("offline export destination is a directory: {0}")]
    OutputDirectory(PathBuf),
    #[error("offline export destination parent is unusable: {0}")]
    OutputParent(PathBuf),
    #[error("offline export token space is exhausted")]
    TokenExhausted,
    #[error("offline export was cancelled")]
    Cancelled,
    #[error("offline export source operation failed for {path}: {kind:?}")]
    Source {
        path: PathBuf,
        kind: std::io::ErrorKind,
    },
    #[error("offline export decode failed for {0}")]
    Decode(PathBuf),
    #[error("offline export pattern compilation failed for slot {0:?}")]
    PatternCompile(PatternSlotId),
    #[error("offline export arithmetic overflow")]
    Arithmetic,
    #[error("offline export temporary-file operation failed for {path}: {kind:?}")]
    TemporaryFile {
        path: PathBuf,
        kind: std::io::ErrorKind,
    },
    #[error("offline export encoding failed for {0}")]
    Encode(PathBuf),
    #[error("offline export sync failed for {path}: {kind:?}")]
    Sync {
        path: PathBuf,
        kind: std::io::ErrorKind,
    },
    #[error("offline export publish failed for {path}: {kind:?}")]
    Publish {
        path: PathBuf,
        kind: std::io::ErrorKind,
    },
    #[error("offline export cleanup failed for {path}: {kind:?}")]
    Cleanup {
        path: PathBuf,
        kind: std::io::ErrorKind,
    },
    #[error("offline export renderer is not available")]
    RendererUnavailable,
}

pub fn validate_wav_destination(destination: &Path) -> Result<(), OfflineExportError> {
    if destination
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        Ok(())
    } else {
        Err(OfflineExportError::DestinationExtension(
            destination.to_path_buf(),
        ))
    }
}
