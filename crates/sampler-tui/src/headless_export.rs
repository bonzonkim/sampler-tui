use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use sampler_core::ProjectDocument;

use crate::export::{
    ExportPatternSlot, ExportToken, OfflineExportError, OfflineExportReceipt,
    OfflineExportSnapshot, stage_export_samples,
};
use crate::{AtomicWavPublisher, ProjectProbe, ProjectStore, ProjectStoreError, render_offline};

#[derive(Debug, thiserror::Error)]
enum HeadlessExportError {
    #[error("could not probe project directory {path}")]
    Probe {
        path: PathBuf,
        #[source]
        source: ProjectStoreError,
    },
    #[error("project has no explicit saved document: {0}")]
    MissingExplicit(PathBuf),
    #[error("could not load explicit project document from {path}")]
    Explicit {
        path: PathBuf,
        #[source]
        source: ProjectStoreError,
    },
    #[error("could not load recovery document from {path}")]
    Recovery {
        path: PathBuf,
        #[source]
        source: ProjectStoreError,
    },
    #[error("recovery document belongs to a different project: {0}")]
    RecoveryMismatch(PathBuf),
    #[error(
        "project has newer recovery revision {recovery_revision} than explicit revision {explicit_revision}: {path}"
    )]
    NewerRecovery {
        path: PathBuf,
        explicit_revision: u64,
        recovery_revision: u64,
    },
    #[error("offline pattern export failed")]
    Export(#[source] OfflineExportError),
}

/// Exports one persisted pattern synchronously without starting a worker, TUI, or device session.
pub fn run(
    project: PathBuf,
    slot: ExportPatternSlot,
    destination: PathBuf,
) -> Result<OfflineExportReceipt, Box<dyn Error>> {
    run_typed(&project, slot, &destination).map_err(|error| Box::new(error) as Box<dyn Error>)
}

fn run_typed(
    project: &Path,
    slot: ExportPatternSlot,
    destination: &Path,
) -> Result<OfflineExportReceipt, HeadlessExportError> {
    let probe = ProjectStore
        .probe(project)
        .map_err(|source| HeadlessExportError::Probe {
            path: project.to_path_buf(),
            source,
        })?;
    let (directory, document) = select_explicit_document(probe)?;
    let snapshot = OfflineExportSnapshot::from_document(&directory, &document, slot)
        .map_err(HeadlessExportError::Export)?;
    let cancelled = AtomicBool::new(false);
    let staged =
        stage_export_samples(&snapshot, &cancelled).map_err(HeadlessExportError::Export)?;
    let mut publisher =
        AtomicWavPublisher::prepare(destination).map_err(HeadlessExportError::Export)?;
    let summary = render_offline(&snapshot, &staged, &mut publisher, &cancelled)
        .map_err(HeadlessExportError::Export)?;
    publisher
        .publish(ExportToken::new(1), &snapshot, summary, &cancelled)
        .map_err(HeadlessExportError::Export)
}

fn select_explicit_document(
    probe: ProjectProbe,
) -> Result<(PathBuf, ProjectDocument), HeadlessExportError> {
    let ProjectProbe {
        directory,
        explicit,
        recovery,
    } = probe;
    let explicit_path = directory.join("project.toml");
    let document = match explicit {
        Some(Ok(document)) => document,
        Some(Err(source)) => {
            return Err(HeadlessExportError::Explicit {
                path: explicit_path,
                source,
            });
        }
        None => return Err(HeadlessExportError::MissingExplicit(directory)),
    };

    if let Some(recovery) = recovery {
        let recovery_path = directory.join(".sampler-tui-recovery.toml");
        let recovery = recovery.map_err(|source| HeadlessExportError::Recovery {
            path: recovery_path,
            source,
        })?;
        if recovery.project_id != document.project_id {
            return Err(HeadlessExportError::RecoveryMismatch(directory));
        }
        if recovery.revision > document.revision {
            return Err(HeadlessExportError::NewerRecovery {
                path: directory,
                explicit_revision: document.revision,
                recovery_revision: recovery.revision,
            });
        }
    }

    Ok((directory, document))
}
