use std::error::Error;
use std::mem;
use std::panic::{self, AssertUnwindSafe, PanicHookInfo, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;

use sampler_core::ProjectDocument;

use crate::export::{
    ExportPatternSlot, ExportToken, OfflineExportError, OfflineExportReceipt,
    OfflineExportSnapshot, stage_export_samples,
};
#[cfg(debug_assertions)]
use crate::export_file::PublisherCheckpoint;
use crate::{AtomicWavPublisher, ProjectProbe, ProjectStore, ProjectStoreError, render_offline};

type PanicHook = dyn for<'a> Fn(&PanicHookInfo<'a>) + Send + Sync + 'static;

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
    match catch_headless_panic(AssertUnwindSafe(|| run_typed(&project, slot, &destination))) {
        Ok(result) => result.map_err(|error| Box::new(error) as Box<dyn Error>),
        Err(payload) => {
            mem::forget(payload);
            Err(Box::new(HeadlessExportError::Export(
                OfflineExportError::ExportPanicked,
            )))
        }
    }
}

fn catch_headless_panic<F, R>(operation: AssertUnwindSafe<F>) -> std::thread::Result<R>
where
    F: FnOnce() -> R,
{
    let hook_lock = crate::PANIC_HOOK_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = Arc::<PanicHook>::from(panic::take_hook());
    let previous_for_other_threads = Arc::clone(&previous);
    let owner = thread::current().id();
    panic::set_hook(Box::new(move |info| {
        if thread::current().id() != owner {
            previous_for_other_threads(info);
        }
    }));
    let outcome = catch_unwind(operation);
    panic::set_hook(Box::new(move |info| previous(info)));
    drop(hook_lock);
    outcome
}

fn run_typed(
    project: &Path,
    slot: ExportPatternSlot,
    destination: &Path,
) -> Result<OfflineExportReceipt, HeadlessExportError> {
    inject_test_panic("before-probe");
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
    let mut publisher = prepare_publisher(destination).map_err(HeadlessExportError::Export)?;
    inject_test_panic("after-prepare");
    let summary = render_offline(&snapshot, &staged, &mut publisher, &cancelled)
        .map_err(HeadlessExportError::Export)?;
    publisher
        .publish(ExportToken::new(1), &snapshot, summary, &cancelled)
        .map_err(HeadlessExportError::Export)
}

fn prepare_publisher(destination: &Path) -> Result<AtomicWavPublisher, OfflineExportError> {
    #[cfg(debug_assertions)]
    if test_panic_checkpoint() == Some("after-link") {
        return AtomicWavPublisher::prepare_with_mutation_hook(destination, |checkpoint| {
            if checkpoint == PublisherCheckpoint::BeforeDirectorySync {
                panic_with_hostile_payload();
            }
        });
    }
    AtomicWavPublisher::prepare(destination)
}

#[cfg(debug_assertions)]
fn test_panic_checkpoint() -> Option<&'static str> {
    match std::env::var("SAMPLER_TUI_TEST_HEADLESS_PANIC")
        .ok()
        .as_deref()
    {
        Some("before-probe") => Some("before-probe"),
        Some("after-prepare") => Some("after-prepare"),
        Some("after-link") => Some("after-link"),
        _ => None,
    }
}

#[cfg(not(debug_assertions))]
const fn test_panic_checkpoint() -> Option<&'static str> {
    None
}

fn inject_test_panic(checkpoint: &'static str) {
    if test_panic_checkpoint() == Some(checkpoint) {
        panic_with_hostile_payload();
    }
}

#[cfg(debug_assertions)]
struct HostilePanicPayload;

#[cfg(debug_assertions)]
impl Drop for HostilePanicPayload {
    fn drop(&mut self) {
        panic!("hostile headless export panic payload destructor");
    }
}

#[cfg(debug_assertions)]
fn panic_with_hostile_payload() -> ! {
    panic::panic_any(HostilePanicPayload)
}

#[cfg(not(debug_assertions))]
fn panic_with_hostile_payload() -> ! {
    unreachable!("headless export panic injection is disabled")
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
