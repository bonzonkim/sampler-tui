use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use sampler_core::{PadId, ProjectId};

use crate::loader::LoadSampleError;
use crate::project_store::ProjectStoreError;
use crate::{ProjectToken, SaveKind};

pub const MAX_PROJECT_REVISION: u64 = i64::MAX as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryChoice {
    Restore,
    Discard,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectOpenPhase {
    Probing,
    AwaitingRecoveryChoice,
    Staging,
    Admitting,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectOpenStage {
    pub token: ProjectToken,
    pub directory: PathBuf,
    pub project_id: Option<ProjectId>,
    pub revision: Option<u64>,
    pub phase: ProjectOpenPhase,
    pub staged_pads: usize,
    pub total_pads: usize,
    pub admitted_actions: usize,
    pub total_actions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectStageError {
    Load(LoadSampleError),
    AssetDigestChanged,
    RecipeContextChanged,
    AudioDeviceRateChanged,
}

impl std::fmt::Display for ProjectStageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load(error) => error.fmt(formatter),
            Self::AssetDigestChanged => formatter.write_str("asset digest changed during staging"),
            Self::RecipeContextChanged => formatter.write_str("staged recipe context changed"),
            Self::AudioDeviceRateChanged => {
                formatter.write_str("audio device rate changed during staging")
            }
        }
    }
}

impl std::error::Error for ProjectStageError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectOpenError {
    OperationPending,
    CancellationLocked,
    TokenExhausted,
    AudioUnavailable,
    NoUsableDocument,
    RecoveryMismatch,
    UnresolvedState(String),
    Probe(ProjectStoreError),
    RecoveryDiscard(ProjectStoreError),
    InvalidPatterns(String),
    Stage {
        pad: PadId,
        error: ProjectStageError,
    },
    Admission(String),
}

impl std::fmt::Display for ProjectOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OperationPending => formatter.write_str("a project operation is already pending"),
            Self::CancellationLocked => {
                formatter.write_str("project open can no longer be cancelled safely")
            }
            Self::TokenExhausted => formatter.write_str("project operation token is exhausted"),
            Self::AudioUnavailable => formatter.write_str("audio device is unavailable"),
            Self::NoUsableDocument => formatter.write_str("project has no usable document"),
            Self::RecoveryMismatch => {
                formatter.write_str("recovery identity does not match the explicit project")
            }
            Self::UnresolvedState(message) => {
                write!(
                    formatter,
                    "current project state must be resolved before open: {message}"
                )
            }
            Self::Probe(error) => write!(formatter, "project probe failed: {error}"),
            Self::RecoveryDiscard(error) => {
                write!(formatter, "recovery discard failed: {error}")
            }
            Self::InvalidPatterns(message) => {
                write!(formatter, "project patterns are invalid: {message}")
            }
            Self::Stage { pad, error } => {
                write!(formatter, "could not stage pad {pad:?}: {error}")
            }
            Self::Admission(message) => write!(formatter, "audio admission failed: {message}"),
        }
    }
}

impl std::error::Error for ProjectOpenError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    Clean,
    Modified,
    Saving(SaveKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSnapshotError {
    DirtySampleDraft(PadId),
    PendingSampleLoad(PadId),
    PendingSampleEdit(PadId),
    PendingProjectOperation(ProjectToken),
    UnresolvedSample(PadId),
    InvalidPatterns(String),
}

impl std::fmt::Display for ProjectSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DirtySampleDraft(pad) => {
                write!(formatter, "pad {pad:?} has an uncommitted sample draft")
            }
            Self::PendingSampleLoad(pad) => {
                write!(formatter, "pad {pad:?} has a pending sample load")
            }
            Self::PendingSampleEdit(pad) => {
                write!(formatter, "pad {pad:?} has a pending sample edit")
            }
            Self::PendingProjectOperation(token) => {
                write!(formatter, "project operation {} is pending", token.get())
            }
            Self::UnresolvedSample(pad) => write!(
                formatter,
                "pad {pad:?} has incomplete committed source metadata"
            ),
            Self::InvalidPatterns(error) => {
                write!(formatter, "project patterns are invalid: {error}")
            }
        }
    }
}

impl std::error::Error for ProjectSnapshotError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectMutationError<E> {
    RevisionExhausted,
    Mutation(E),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectOperationDescriptor {
    pub token: ProjectToken,
    pub kind: SaveKind,
    pub project_id: ProjectId,
    pub directory: PathBuf,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutosaveDescriptor {
    pub revision: u64,
}

#[derive(Debug, Clone)]
pub struct ProjectSession {
    project_id: ProjectId,
    directory: Option<PathBuf>,
    name: String,
    current_revision: u64,
    saved_revision: u64,
    autosaved_revision: u64,
    dirty_since: Option<Instant>,
    in_flight: Option<ProjectOperationDescriptor>,
    pending_autosave: Option<AutosaveDescriptor>,
}

impl ProjectSession {
    pub(crate) fn new(
        project_id: ProjectId,
        directory: Option<PathBuf>,
        name: impl Into<String>,
        revision: u64,
    ) -> Self {
        assert!(
            revision <= MAX_PROJECT_REVISION,
            "project revision is portable"
        );
        Self {
            project_id,
            directory,
            name: name.into(),
            current_revision: revision,
            saved_revision: revision,
            autosaved_revision: revision,
            dirty_since: None,
            in_flight: None,
            pending_autosave: None,
        }
    }

    pub(crate) fn opened(
        project_id: ProjectId,
        directory: PathBuf,
        name: impl Into<String>,
        current_revision: u64,
        saved_revision: u64,
        autosaved_revision: u64,
        dirty_since: Option<Instant>,
    ) -> Self {
        assert!(current_revision <= MAX_PROJECT_REVISION);
        assert!(saved_revision <= MAX_PROJECT_REVISION);
        assert!(autosaved_revision <= MAX_PROJECT_REVISION);
        Self {
            project_id,
            directory: Some(directory),
            name: name.into(),
            current_revision,
            saved_revision,
            autosaved_revision,
            dirty_since,
            in_flight: None,
            pending_autosave: None,
        }
    }

    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }
    pub fn directory(&self) -> Option<&Path> {
        self.directory.as_deref()
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub const fn current_revision(&self) -> u64 {
        self.current_revision
    }
    pub const fn saved_revision(&self) -> u64 {
        self.saved_revision
    }
    pub const fn autosaved_revision(&self) -> u64 {
        self.autosaved_revision
    }
    pub const fn dirty_since(&self) -> Option<Instant> {
        self.dirty_since
    }
    pub fn in_flight(&self) -> Option<&ProjectOperationDescriptor> {
        self.in_flight.as_ref()
    }
    pub const fn pending_autosave(&self) -> Option<&AutosaveDescriptor> {
        self.pending_autosave.as_ref()
    }

    pub fn status(&self) -> ProjectStatus {
        if let Some(operation) = &self.in_flight {
            ProjectStatus::Saving(operation.kind)
        } else if self.current_revision == self.saved_revision {
            ProjectStatus::Clean
        } else {
            ProjectStatus::Modified
        }
    }

    pub fn ensure_mutation_available(&self) -> Result<(), ProjectMutationError<()>> {
        self.current_revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_PROJECT_REVISION)
            .map(|_| ())
            .ok_or(ProjectMutationError::RevisionExhausted)
    }

    pub fn commit_project_mutation<T, E>(
        &mut self,
        now: Instant,
        mutation: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, ProjectMutationError<E>> {
        let next = self
            .current_revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_PROJECT_REVISION)
            .ok_or(ProjectMutationError::RevisionExhausted)?;
        let result = mutation().map_err(ProjectMutationError::Mutation)?;
        self.current_revision = next;
        self.dirty_since = Some(now);
        Ok(result)
    }

    pub fn set_in_flight(&mut self, operation: Option<ProjectOperationDescriptor>) {
        self.in_flight = operation;
    }

    pub fn set_pending_autosave(&mut self, autosave: Option<AutosaveDescriptor>) {
        match autosave {
            None => self.pending_autosave = None,
            Some(candidate)
                if self
                    .pending_autosave
                    .is_none_or(|pending| candidate.revision > pending.revision) =>
            {
                self.pending_autosave = Some(candidate);
            }
            Some(_) => {}
        }
    }

    pub(crate) fn adopt_saved_project(
        &mut self,
        project_id: ProjectId,
        directory: PathBuf,
        name: String,
        revision: u64,
    ) {
        self.project_id = project_id;
        self.directory = Some(directory);
        self.name = name;
        self.mark_explicit_saved(revision);
    }

    pub(crate) fn mark_explicit_saved(&mut self, revision: u64) {
        self.saved_revision = revision;
        if self.current_revision == revision {
            self.dirty_since = None;
        }
    }

    pub(crate) fn mark_autosaved(&mut self, revision: u64) {
        self.autosaved_revision = revision;
        self.pending_autosave = None;
    }

    #[cfg(test)]
    pub(crate) fn set_current_revision_for_test(&mut self, revision: u64) {
        self.current_revision = revision;
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        path::PathBuf,
        time::{Duration, Instant},
    };

    use sampler_core::ProjectId;

    use super::*;

    #[test]
    fn committed_mutations_advance_exactly_once_and_failed_mutations_do_not() {
        let now = Instant::now();
        let mut session =
            ProjectSession::new(ProjectId::from_bytes([0x41; 16]), None, "Untitled", 7);

        let committed = session
            .commit_project_mutation(now, || Ok::<_, &'static str>("committed"))
            .unwrap();
        assert_eq!(committed, "committed");
        assert_eq!(session.current_revision(), 8);
        assert_eq!(session.dirty_since(), Some(now));
        assert_eq!(session.status(), ProjectStatus::Modified);

        let error = session
            .commit_project_mutation(now + Duration::from_secs(1), || Err::<(), _>("rejected"))
            .unwrap_err();
        assert_eq!(error, ProjectMutationError::Mutation("rejected"));
        assert_eq!(session.current_revision(), 8);
        assert_eq!(session.dirty_since(), Some(now));
    }

    #[test]
    fn revision_exhaustion_refuses_the_mutation_atomically() {
        let mut session = ProjectSession::new(
            ProjectId::from_bytes([0x42; 16]),
            Some(PathBuf::from("project")),
            "Beat",
            i64::MAX as u64,
        );
        let mutated = Cell::new(false);

        let error = session
            .commit_project_mutation(Instant::now(), || {
                mutated.set(true);
                Ok::<_, ()>(())
            })
            .unwrap_err();

        assert_eq!(error, ProjectMutationError::RevisionExhausted);
        assert!(!mutated.get());
        assert_eq!(session.current_revision(), i64::MAX as u64);
    }

    #[test]
    fn session_tracks_only_persistence_metadata_and_exact_operation_descriptors() {
        let id = ProjectId::from_bytes([0x43; 16]);
        let directory = PathBuf::from("beat-project");
        let mut session = ProjectSession::new(id, Some(directory.clone()), "Beat", 11);
        assert_eq!(session.project_id(), id);
        assert_eq!(session.directory(), Some(directory.as_path()));
        assert_eq!(session.name(), "Beat");
        assert_eq!(session.saved_revision(), 11);
        assert_eq!(session.autosaved_revision(), 11);
        assert_eq!(session.in_flight(), None);
        assert_eq!(session.pending_autosave(), None);

        let operation = ProjectOperationDescriptor {
            token: crate::ProjectToken::new(19),
            kind: crate::SaveKind::Explicit,
            project_id: id,
            directory,
            revision: 11,
        };
        session.set_in_flight(Some(operation.clone()));
        session.set_pending_autosave(Some(AutosaveDescriptor { revision: 12 }));
        session.set_pending_autosave(Some(AutosaveDescriptor { revision: 10 }));
        assert_eq!(session.in_flight(), Some(&operation));
        assert_eq!(
            session.pending_autosave(),
            Some(&AutosaveDescriptor { revision: 12 })
        );
    }
}
