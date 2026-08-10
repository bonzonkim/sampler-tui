use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sampler_audio::SampleBuffer;
use sampler_core::{
    AssetDigest, MasterMixSettings, PadId, PatternSlotId, ProjectDocument, ProjectId,
    ProjectPattern,
};

use crate::project_store::AnchoredDirectoryIdentity;
use crate::{ProjectSavePad, ProjectSaveSnapshot, ProjectStore, ProjectStoreError};

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
pub struct ExportCancel(Arc<AtomicBool>);

impl std::fmt::Debug for ExportCancel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExportCancel")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl PartialEq for ExportCancel {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ExportCancel {}

impl ExportCancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn as_atomic(&self) -> &AtomicBool {
        &self.0
    }
}

pub type OfflineExportCancellation = ExportCancel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportResultFence {
    pub project_id: ProjectId,
    pub revision: u64,
    pub slot: PatternSlotId,
    pub destination: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportPhase {
    Queued,
    Running {
        completed_units: u64,
        total_units: u64,
    },
    Cancelling,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOperation {
    pub(crate) token: ExportToken,
    pub(crate) project_id: ProjectId,
    pub(crate) revision: u64,
    pub(crate) slot: PatternSlotId,
    pub(crate) destination: PathBuf,
    pub(crate) cancel: ExportCancel,
    pub(crate) phase: ExportPhase,
}

impl ExportOperation {
    pub const fn token(&self) -> ExportToken {
        self.token
    }

    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn slot(&self) -> PatternSlotId {
        self.slot
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub const fn phase(&self) -> ExportPhase {
        self.phase
    }

    pub fn result_fence(&self) -> ExportResultFence {
        ExportResultFence {
            project_id: self.project_id,
            revision: self.revision,
            slot: self.slot,
            destination: self.destination.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportStatusView {
    Active {
        operation: ExportOperation,
        focused: bool,
    },
    Completed {
        receipt: OfflineExportReceipt,
    },
    Cancelled {
        fence: ExportResultFence,
    },
    Failed {
        fence: ExportResultFence,
        error: OfflineExportError,
    },
}

/// An immutable, device-independent description of exactly one pattern export.
#[derive(Debug, Clone, PartialEq)]
pub struct OfflineExportSnapshot {
    project_id: ProjectId,
    revision: u64,
    slot: PatternSlotId,
    pattern: ProjectPattern,
    pads: Vec<ProjectSavePad>,
    master_mix: MasterMixSettings,
    sample_rate: u32,
    project_sources: Vec<ExportPadSource>,
}

#[derive(Debug, Clone, PartialEq)]
struct ExportPadSource {
    descriptor: ProjectSavePad,
    authority: ExportSourceAuthority,
}

#[derive(Debug, Clone, PartialEq)]
enum ExportSourceAuthority {
    Project {
        directory: PathBuf,
        directory_identity: AnchoredDirectoryIdentity,
        relative: String,
        expected_digest: AssetDigest,
    },
    Loose {
        parent_identity: AnchoredDirectoryIdentity,
    },
}

impl OfflineExportSnapshot {
    pub fn new(
        project_id: ProjectId,
        revision: u64,
        slot: PatternSlotId,
        pattern: ProjectPattern,
        pads: Vec<ProjectSavePad>,
        master_mix: MasterMixSettings,
        sample_rate: u32,
    ) -> Result<Self, OfflineExportError> {
        let snapshot = Self {
            project_id,
            revision,
            slot,
            pattern,
            pads,
            master_mix,
            sample_rate,
            project_sources: Vec::new(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Builds an immutable, canonical-rate snapshot from a persisted project document.
    pub fn from_document(
        directory: &Path,
        document: &ProjectDocument,
        slot: ExportPatternSlot,
    ) -> Result<Self, OfflineExportError> {
        let slot = slot.slot();
        let source_pattern = document
            .patterns
            .iter()
            .find(|pattern| pattern.slot() == slot)
            .ok_or(OfflineExportError::PatternUnavailable(slot))?;
        let mut editable = source_pattern
            .to_editable()
            .map_err(|_| OfflineExportError::PatternCompile(slot))?;
        editable
            .rebuild_sample_rate(EXPORT_SAMPLE_RATE)
            .map_err(|_| OfflineExportError::PatternCompile(slot))?;
        let pattern = ProjectPattern::from_editable(&editable)
            .map_err(|_| OfflineExportError::PatternCompile(slot))?;

        let mut referenced = Vec::new();
        for event in &pattern.events {
            if referenced.contains(&event.event.pad) {
                continue;
            }
            referenced.push(event.event.pad);
        }

        let directory_identity = ProjectStore
            .project_directory_identity(directory)
            .map_err(OfflineExportError::ProjectStore)?;
        let mut project_sources = Vec::with_capacity(referenced.len());
        let mut pads = Vec::with_capacity(referenced.len());
        for pad in referenced {
            let project_pad = document
                .pads
                .iter()
                .find(|candidate| candidate.pad == pad)
                .ok_or(OfflineExportError::MissingPadSource { pad })?
                .clone();
            let asset = ProjectStore
                .read_project_asset_after_open(
                    directory,
                    &project_pad.audio_path,
                    project_pad.asset_digest,
                    Some(directory_identity),
                    || {},
                )
                .map_err(OfflineExportError::ProjectStore)?;
            let source_path = asset.path;
            let fingerprint = asset.fingerprint;
            if fingerprint.digest != project_pad.asset_digest {
                return Err(OfflineExportError::ProjectStore(
                    ProjectStoreError::AssetIntegrity { path: source_path },
                ));
            }
            let source = ProjectSavePad {
                pad: project_pad.pad,
                source_path,
                source_generation: 0,
                fingerprint,
                settings: project_pad.settings,
                mix: project_pad.mix,
                recipe: project_pad.recipe,
            };
            project_sources.push(ExportPadSource {
                descriptor: source.clone(),
                authority: ExportSourceAuthority::Project {
                    directory: directory.to_path_buf(),
                    directory_identity,
                    relative: project_pad.audio_path,
                    expected_digest: project_pad.asset_digest,
                },
            });
            pads.push(source);
        }

        let mut snapshot = Self::new(
            document.project_id,
            document.revision,
            slot,
            pattern,
            pads,
            document.master_mix,
            EXPORT_SAMPLE_RATE,
        )?;
        snapshot.project_sources = project_sources;
        Ok(snapshot)
    }

    /// Builds an immutable export snapshot from the App's committed save model.
    pub fn from_save_snapshot(
        directory: &Path,
        project: &ProjectSaveSnapshot,
        slot: ExportPatternSlot,
    ) -> Result<Self, OfflineExportError> {
        Self::from_save_snapshot_with_project_directory(directory, Some(directory), project, slot)
    }

    pub(crate) fn from_save_snapshot_with_project_directory(
        loose_directory: &Path,
        project_directory: Option<&Path>,
        project: &ProjectSaveSnapshot,
        slot: ExportPatternSlot,
    ) -> Result<Self, OfflineExportError> {
        let slot = slot.slot();
        let source_pattern = project
            .patterns
            .iter()
            .find(|pattern| pattern.slot() == slot)
            .ok_or(OfflineExportError::PatternUnavailable(slot))?;
        let mut editable = source_pattern
            .to_editable()
            .map_err(|_| OfflineExportError::PatternCompile(slot))?;
        editable
            .rebuild_sample_rate(EXPORT_SAMPLE_RATE)
            .map_err(|_| OfflineExportError::PatternCompile(slot))?;
        let pattern = ProjectPattern::from_editable(&editable)
            .map_err(|_| OfflineExportError::PatternCompile(slot))?;

        let mut referenced = Vec::new();
        for event in &pattern.events {
            if !referenced.contains(&event.event.pad) {
                referenced.push(event.event.pad);
            }
        }

        let mut pads = Vec::with_capacity(referenced.len());
        let mut project_sources = Vec::with_capacity(referenced.len());
        let project_authority = project_directory
            .map(|directory| {
                let identity = ProjectStore
                    .project_directory_identity(directory)
                    .map_err(OfflineExportError::ProjectStore)?;
                let absolute = std::path::absolute(directory).map_err(|error| {
                    OfflineExportError::ProjectStore(ProjectStoreError::Filesystem {
                        operation: "resolve project directory",
                        path: directory.to_path_buf(),
                        kind: error.kind(),
                    })
                })?;
                Ok::<_, OfflineExportError>((directory.to_path_buf(), absolute, identity))
            })
            .transpose()?;
        for pad_id in referenced {
            let source = project
                .pads
                .iter()
                .find(|candidate| candidate.pad == pad_id)
                .ok_or(OfflineExportError::MissingPadSource { pad: pad_id })?
                .clone();
            if source.source_path.as_os_str().is_empty() {
                return Err(OfflineExportError::MissingPadSource { pad: pad_id });
            }
            let resolved = if source.source_path.is_absolute() {
                source.source_path.clone()
            } else {
                loose_directory.join(&source.source_path)
            };
            let metadata = std::fs::symlink_metadata(&resolved)
                .map_err(|_| OfflineExportError::MissingPadSource { pad: pad_id })?;
            if !metadata.file_type().is_file() {
                return Err(OfflineExportError::MissingPadSource { pad: pad_id });
            }
            let mut source = source;
            source.source_path = resolved;
            let source_project_authority =
                project_authority
                    .as_ref()
                    .and_then(|(directory, absolute, directory_identity)| {
                        lexical_project_asset(absolute, &source.source_path)
                            .map(|relative| (directory, *directory_identity, relative))
                    });
            let authority =
                if let Some((directory, directory_identity, relative)) = source_project_authority {
                    let asset = ProjectStore
                        .read_project_asset_after_open(
                            directory,
                            &relative,
                            source.fingerprint.digest,
                            Some(directory_identity),
                            || {},
                        )
                        .map_err(OfflineExportError::ProjectStore)?;
                    if asset.fingerprint != source.fingerprint {
                        return Err(OfflineExportError::ProjectStore(
                            ProjectStoreError::AssetIntegrity {
                                path: source.source_path.clone(),
                            },
                        ));
                    }
                    ExportSourceAuthority::Project {
                        directory: directory.to_path_buf(),
                        directory_identity,
                        relative,
                        expected_digest: source.fingerprint.digest,
                    }
                } else {
                    let parent_identity = ProjectStore
                        .committed_source_parent_identity(&source.source_path)
                        .map_err(OfflineExportError::ProjectStore)?;
                    ExportSourceAuthority::Loose { parent_identity }
                };
            project_sources.push(ExportPadSource {
                descriptor: source.clone(),
                authority,
            });
            pads.push(source);
        }

        let mut snapshot = Self::new(
            project.project_id,
            project.revision,
            slot,
            pattern,
            pads,
            project.master_mix,
            EXPORT_SAMPLE_RATE,
        )?;
        snapshot.project_sources = project_sources;
        Ok(snapshot)
    }

    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn slot(&self) -> PatternSlotId {
        self.slot
    }

    pub fn pattern(&self) -> &ProjectPattern {
        &self.pattern
    }

    pub fn pads(&self) -> &[ProjectSavePad] {
        &self.pads
    }

    pub const fn master_mix(&self) -> MasterMixSettings {
        self.master_mix
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub(crate) fn has_source_authority(&self) -> bool {
        !self.project_sources.is_empty() && self.project_sources.len() == self.pads.len()
    }

    /// Returns the exact canonical transport loop length represented by this snapshot.
    pub fn loop_frames(&self) -> Result<u64, OfflineExportError> {
        self.pattern
            .to_editable()
            .map(|pattern| pattern.transport().loop_frames())
            .map_err(|_| OfflineExportError::PatternCompile(self.slot))
    }

    pub fn into_parts(
        self,
    ) -> (
        ProjectId,
        u64,
        PatternSlotId,
        ProjectPattern,
        Vec<ProjectSavePad>,
        MasterMixSettings,
        u32,
    ) {
        (
            self.project_id,
            self.revision,
            self.slot,
            self.pattern,
            self.pads,
            self.master_mix,
            self.sample_rate,
        )
    }

    fn validate(&self) -> Result<(), OfflineExportError> {
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

fn lexical_project_asset(directory: &Path, source: &Path) -> Option<String> {
    use std::path::Component;

    if !directory.is_absolute() || !source.is_absolute() {
        return None;
    }
    let relative = source.strip_prefix(directory).ok()?;
    let components = relative.components().collect::<Vec<_>>();
    if components.len() < 2
        || !matches!(components[0], Component::Normal(first) if first == "audio")
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    relative.to_str().map(str::to_owned)
}

/// A fully decoded, edited, and canonical-rate pad ready for an offline engine.
#[derive(Debug, Clone)]
pub struct StagedExportPad {
    pub pad: PadId,
    pub sample: Arc<SampleBuffer>,
    pub settings: sampler_core::PadSettings,
    pub mix: sampler_core::PadMixSettings,
}

/// Stages every committed source in an immutable export snapshot through the project-open path.
pub fn stage_export_samples(
    snapshot: &OfflineExportSnapshot,
    cancelled: &AtomicBool,
) -> Result<Vec<StagedExportPad>, OfflineExportError> {
    stage_export_samples_with_observers(snapshot, cancelled, || {}, || {})
}

#[cfg(test)]
fn stage_export_samples_with_hook<F>(
    snapshot: &OfflineExportSnapshot,
    cancelled: &AtomicBool,
    after_stage: F,
) -> Result<Vec<StagedExportPad>, OfflineExportError>
where
    F: FnMut(),
{
    stage_export_samples_with_observers(snapshot, cancelled, || {}, after_stage)
}

pub(crate) fn stage_export_samples_with_observers<F, G>(
    snapshot: &OfflineExportSnapshot,
    cancelled: &AtomicBool,
    mut after_open: F,
    after_stage: G,
) -> Result<Vec<StagedExportPad>, OfflineExportError>
where
    F: FnMut(),
    G: FnMut(),
{
    let mut after_stage = after_stage;
    if snapshot.project_sources.is_empty() {
        return Err(OfflineExportError::SnapshotNotProjectBacked);
    }
    let mut staged = Vec::with_capacity(snapshot.project_sources.len());
    for source in &snapshot.project_sources {
        if cancelled.load(Ordering::Acquire) {
            return Err(OfflineExportError::Cancelled);
        }
        let sample = match &source.authority {
            ExportSourceAuthority::Project {
                directory,
                directory_identity,
                relative,
                expected_digest,
            } => crate::loader::decode_committed_project_asset(
                crate::loader::CommittedProjectAssetRequest {
                    directory,
                    asset_path: relative,
                    expected_digest: *expected_digest,
                    expected_directory: Some(*directory_identity),
                    target_rate: EXPORT_SAMPLE_RATE,
                    recipe: source.descriptor.recipe,
                },
                cancelled,
                &mut after_open,
            ),
            ExportSourceAuthority::Loose { parent_identity } => {
                crate::loader::decode_committed_source_pad_after_open(
                    &source.descriptor,
                    *parent_identity,
                    EXPORT_SAMPLE_RATE,
                    cancelled,
                    &mut after_open,
                )
            }
        }
        .map_err(|error| match error {
            ProjectStoreError::Cancelled => OfflineExportError::Cancelled,
            error => OfflineExportError::ProjectStore(error),
        })?;
        if cancelled.load(Ordering::Acquire) {
            return Err(OfflineExportError::Cancelled);
        }
        staged.push(StagedExportPad {
            pad: source.descriptor.pad,
            sample: sample.rendered,
            settings: source.descriptor.settings,
            mix: source.descriptor.mix,
        });
        after_stage();
    }
    Ok(staged)
}

#[derive(Debug, Clone, PartialEq)]
pub struct OfflineExportRequest {
    token: ExportToken,
    destination: PathBuf,
    snapshot: Box<OfflineExportSnapshot>,
    cancellation: OfflineExportCancellation,
}

impl OfflineExportRequest {
    pub fn new(
        token: ExportToken,
        destination: PathBuf,
        snapshot: OfflineExportSnapshot,
        cancellation: OfflineExportCancellation,
    ) -> Result<Self, OfflineExportError> {
        validate_wav_destination(&destination)?;
        snapshot.validate()?;
        Ok(Self {
            token,
            destination,
            snapshot: Box::new(snapshot),
            cancellation,
        })
    }

    pub const fn token(&self) -> ExportToken {
        self.token
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn snapshot(&self) -> &OfflineExportSnapshot {
        &self.snapshot
    }

    pub fn cancellation(&self) -> OfflineExportCancellation {
        self.cancellation.clone()
    }

    pub fn into_parts(
        self,
    ) -> (
        ExportToken,
        PathBuf,
        OfflineExportSnapshot,
        OfflineExportCancellation,
    ) {
        (
            self.token,
            self.destination,
            *self.snapshot,
            self.cancellation,
        )
    }
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
    #[error("an offline export operation is already active")]
    OperationPending,
    #[error("offline export admission is blocked by unresolved App state: {0}")]
    UnresolvedAppState(String),
    #[error("offline export worker is busy")]
    WorkerBusy,
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
    #[error("project does not contain selected pattern slot {0:?}")]
    PatternUnavailable(PatternSlotId),
    #[error("pattern references pad {pad:?} without a committed source")]
    MissingPadSource { pad: PadId },
    #[error("pattern snapshot contains a duplicate committed source for pad {pad:?}")]
    DuplicatePadSource { pad: PadId },
    #[error("pattern snapshot contains an unreferenced committed source for pad {pad:?}")]
    UnreferencedPadSource { pad: PadId },
    #[error("offline export destination must have a .wav extension: {0}")]
    DestinationExtension(PathBuf),
    #[error("offline export destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("offline export destination is a directory: {0}")]
    OutputDirectory(PathBuf),
    #[error("offline export destination parent is unusable: {0}")]
    OutputParent(PathBuf),
    #[error("could not inspect offline export destination {path}: {kind:?}")]
    DestinationAccess {
        path: PathBuf,
        kind: std::io::ErrorKind,
    },
    #[error("offline export token space is exhausted")]
    TokenExhausted,
    #[error("offline export was cancelled")]
    Cancelled,
    #[error("offline export request panicked")]
    ExportPanicked,
    #[error("offline export snapshot was not created from a persisted project")]
    SnapshotNotProjectBacked,
    #[error("offline export project source staging failed: {0}")]
    ProjectStore(#[from] ProjectStoreError),
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    use hound::{SampleFormat, WavSpec, WavWriter};
    use sampler_core::{
        AssetDigest, BankId, EventId, Meter, PadMixSettings, PadSettings, PatternEvent,
        ProjectPatternEvent, Resolution, SampleEditRecipe, Tempo,
    };

    use crate::{ProjectSavePad, SourceFingerprint, SupportedAudioExtension};

    use super::*;

    fn pad(index: u8) -> PadId {
        PadId::new(BankId::new(0).unwrap(), index).unwrap()
    }

    fn descriptor(pad: PadId) -> ProjectSavePad {
        ProjectSavePad {
            pad,
            source_path: PathBuf::from("source.wav"),
            source_generation: 1,
            fingerprint: SourceFingerprint {
                digest: AssetDigest::from_bytes([0; 32]),
                encoded_bytes: 1,
                extension: SupportedAudioExtension::Wav,
            },
            settings: PadSettings::default(),
            mix: PadMixSettings::default(),
            recipe: SampleEditRecipe::identity(),
        }
    }

    fn pattern(slot: PatternSlotId, event_pad: Option<PadId>) -> ProjectPattern {
        ProjectPattern {
            slot,
            name: "Export".to_owned(),
            sample_rate: EXPORT_SAMPLE_RATE,
            tempo: Tempo::new(120.0).unwrap(),
            meter: Meter::new(4, 4).unwrap(),
            bars: 1,
            resolution: Resolution::Sixteenth,
            swing: 0.5,
            quantize_strength: 0.0,
            events: event_pad
                .into_iter()
                .map(|pad| ProjectPatternEvent {
                    event: PatternEvent::new(EventId(1), pad, 0, 1.0, None).unwrap(),
                    raw_frame: 0,
                })
                .collect(),
        }
    }

    fn snapshot() -> OfflineExportSnapshot {
        let slot = PatternSlotId::new(0).unwrap();
        OfflineExportSnapshot::new(
            ProjectId::from_bytes([1; 16]),
            7,
            slot,
            pattern(slot, Some(pad(0))),
            vec![descriptor(pad(0))],
            MasterMixSettings::default(),
            EXPORT_SAMPLE_RATE,
        )
        .unwrap()
    }

    fn committed_source_pad(directory: &Path, index: u8) -> ExportPadSource {
        let source = directory.join(format!("source-{index}.wav"));
        let mut writer = WavWriter::create(
            &source,
            WavSpec {
                channels: 2,
                sample_rate: EXPORT_SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )
        .unwrap();
        writer.write_sample(index as f32 / 16.0).unwrap();
        writer.write_sample(-(index as f32 / 16.0)).unwrap();
        writer.finalize().unwrap();
        let fingerprint = SourceFingerprint::from_path(&source).unwrap();
        let descriptor = ProjectSavePad {
            pad: pad(index),
            source_path: source,
            source_generation: u64::from(index),
            fingerprint,
            settings: PadSettings::default(),
            mix: PadMixSettings::default(),
            recipe: SampleEditRecipe::identity(),
        };
        let parent_identity = ProjectStore
            .committed_source_parent_identity(&descriptor.source_path)
            .unwrap();
        ExportPadSource {
            descriptor,
            authority: ExportSourceAuthority::Loose { parent_identity },
        }
    }

    #[test]
    fn staging_checks_cancellation_between_committed_pads() {
        let directory = std::env::temp_dir().join(format!(
            "sampler-tui-export-stage-between-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("audio")).unwrap();
        let mut snapshot = snapshot();
        snapshot.project_sources = vec![
            committed_source_pad(&directory, 1),
            committed_source_pad(&directory, 7),
        ];
        let cancelled = AtomicBool::new(false);

        let result = stage_export_samples_with_hook(&snapshot, &cancelled, || {
            cancelled.store(true, Ordering::Release);
        });

        assert!(matches!(result, Err(OfflineExportError::Cancelled)));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn snapshot_rejects_a_pattern_from_another_slot() {
        let selected = PatternSlotId::new(0).unwrap();
        let pattern_slot = PatternSlotId::new(1).unwrap();
        assert_eq!(
            OfflineExportSnapshot::new(
                ProjectId::from_bytes([1; 16]),
                7,
                selected,
                pattern(pattern_slot, Some(pad(0))),
                vec![descriptor(pad(0))],
                MasterMixSettings::default(),
                EXPORT_SAMPLE_RATE,
            ),
            Err(OfflineExportError::PatternSlotMismatch {
                selected,
                pattern: pattern_slot,
            })
        );
    }

    #[test]
    fn snapshot_rejects_an_empty_pattern() {
        let slot = PatternSlotId::new(0).unwrap();
        assert_eq!(
            OfflineExportSnapshot::new(
                ProjectId::from_bytes([1; 16]),
                7,
                slot,
                pattern(slot, None),
                Vec::new(),
                MasterMixSettings::default(),
                EXPORT_SAMPLE_RATE,
            ),
            Err(OfflineExportError::EmptyPattern)
        );
    }

    #[test]
    fn snapshot_rejects_a_missing_committed_pad_descriptor() {
        let slot = PatternSlotId::new(0).unwrap();
        assert_eq!(
            OfflineExportSnapshot::new(
                ProjectId::from_bytes([1; 16]),
                7,
                slot,
                pattern(slot, Some(pad(0))),
                Vec::new(),
                MasterMixSettings::default(),
                EXPORT_SAMPLE_RATE,
            ),
            Err(OfflineExportError::MissingPadSource { pad: pad(0) })
        );
    }

    #[test]
    fn snapshot_rejects_a_referenced_descriptor_without_a_source_path() {
        let slot = PatternSlotId::new(0).unwrap();
        let mut committed = descriptor(pad(0));
        committed.source_path = PathBuf::new();
        assert_eq!(
            OfflineExportSnapshot::new(
                ProjectId::from_bytes([1; 16]),
                7,
                slot,
                pattern(slot, Some(pad(0))),
                vec![committed],
                MasterMixSettings::default(),
                EXPORT_SAMPLE_RATE,
            ),
            Err(OfflineExportError::MissingPadSource { pad: pad(0) })
        );
    }

    #[test]
    fn snapshot_rejects_duplicate_committed_pad_descriptors() {
        let slot = PatternSlotId::new(0).unwrap();
        assert_eq!(
            OfflineExportSnapshot::new(
                ProjectId::from_bytes([1; 16]),
                7,
                slot,
                pattern(slot, Some(pad(0))),
                vec![descriptor(pad(0)), descriptor(pad(0))],
                MasterMixSettings::default(),
                EXPORT_SAMPLE_RATE,
            ),
            Err(OfflineExportError::DuplicatePadSource { pad: pad(0) })
        );
    }

    #[test]
    fn snapshot_rejects_an_unreferenced_committed_pad_descriptor() {
        let slot = PatternSlotId::new(0).unwrap();
        assert_eq!(
            OfflineExportSnapshot::new(
                ProjectId::from_bytes([1; 16]),
                7,
                slot,
                pattern(slot, Some(pad(0))),
                vec![descriptor(pad(0)), descriptor(pad(1))],
                MasterMixSettings::default(),
                EXPORT_SAMPLE_RATE,
            ),
            Err(OfflineExportError::UnreferencedPadSource { pad: pad(1) })
        );
    }

    #[test]
    fn snapshot_rejects_a_noncanonical_sample_rate() {
        let slot = PatternSlotId::new(0).unwrap();
        assert_eq!(
            OfflineExportSnapshot::new(
                ProjectId::from_bytes([1; 16]),
                7,
                slot,
                pattern(slot, Some(pad(0))),
                vec![descriptor(pad(0))],
                MasterMixSettings::default(),
                44_100,
            ),
            Err(OfflineExportError::SampleRate(44_100))
        );
    }

    #[test]
    fn request_rejects_a_non_wav_destination() {
        let error = OfflineExportRequest::new(
            ExportToken::new(1),
            PathBuf::from("mix.flac"),
            snapshot(),
            OfflineExportCancellation::default(),
        );
        assert_eq!(
            error.unwrap_err(),
            OfflineExportError::DestinationExtension(PathBuf::from("mix.flac"))
        );
    }
}
