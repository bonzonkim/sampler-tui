use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use sampler_core::{
    AssetDigest, LegacyProjectDocument, PadId, PadSettings, ParsedProjectDocument, ProjectDocument,
    ProjectId, ProjectPattern, SampleEditRecipe,
};
use sha2::{Digest, Sha256};

use crate::loader::MAX_ENCODED_FILE_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedAudioExtension {
    Wav,
    Aif,
    Aiff,
    Flac,
    Mp3,
}

impl SupportedAudioExtension {
    pub fn from_path(path: &Path) -> Result<Self, ProjectStoreError> {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .ok_or_else(|| ProjectStoreError::UnsupportedExtension {
                path: path.to_path_buf(),
            })?;
        match extension.to_ascii_lowercase().as_str() {
            "wav" => Ok(Self::Wav),
            "aif" => Ok(Self::Aif),
            "aiff" => Ok(Self::Aiff),
            "flac" => Ok(Self::Flac),
            "mp3" => Ok(Self::Mp3),
            _ => Err(ProjectStoreError::UnsupportedExtension {
                path: path.to_path_buf(),
            }),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Aif => "aif",
            Self::Aiff => "aiff",
            Self::Flac => "flac",
            Self::Mp3 => "mp3",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub digest: AssetDigest,
    pub encoded_bytes: u64,
    pub extension: SupportedAudioExtension,
}

impl SourceFingerprint {
    pub fn from_path(path: &Path) -> Result<Self, ProjectStoreError> {
        let extension = SupportedAudioExtension::from_path(path)?;
        let metadata =
            fs::symlink_metadata(path).map_err(|error| ProjectStoreError::SourceRead {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?;
        if metadata.file_type().is_symlink() {
            return Err(ProjectStoreError::SymlinkRejected {
                path: path.to_path_buf(),
            });
        }
        if !metadata.file_type().is_file() {
            return Err(ProjectStoreError::NonRegularFile {
                path: path.to_path_buf(),
            });
        }
        if metadata.len() > MAX_ENCODED_FILE_BYTES {
            return Err(ProjectStoreError::SourceTooLarge {
                path: path.to_path_buf(),
                bytes: metadata.len(),
                max_bytes: MAX_ENCODED_FILE_BYTES,
            });
        }

        let mut file = File::open(path).map_err(|error| ProjectStoreError::SourceRead {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
        let opened = file
            .metadata()
            .map_err(|error| ProjectStoreError::SourceRead {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?;
        if !opened.is_file() {
            return Err(ProjectStoreError::NonRegularFile {
                path: path.to_path_buf(),
            });
        }

        let mut hasher = Sha256::new();
        let mut encoded_bytes = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| ProjectStoreError::SourceRead {
                    path: path.to_path_buf(),
                    kind: error.kind(),
                })?;
            if read == 0 {
                break;
            }
            encoded_bytes = encoded_bytes.checked_add(read as u64).ok_or_else(|| {
                ProjectStoreError::SourceTooLarge {
                    path: path.to_path_buf(),
                    bytes: u64::MAX,
                    max_bytes: MAX_ENCODED_FILE_BYTES,
                }
            })?;
            if encoded_bytes > MAX_ENCODED_FILE_BYTES {
                return Err(ProjectStoreError::SourceTooLarge {
                    path: path.to_path_buf(),
                    bytes: encoded_bytes,
                    max_bytes: MAX_ENCODED_FILE_BYTES,
                });
            }
            hasher.update(&buffer[..read]);
        }

        Ok(Self {
            digest: AssetDigest::from_bytes(hasher.finalize().into()),
            encoded_bytes,
            extension,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSaveSnapshot {
    pub project_id: ProjectId,
    pub name: String,
    pub revision: u64,
    pub pads: Vec<ProjectSavePad>,
    pub patterns: Vec<ProjectPattern>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSavePad {
    pub pad: PadId,
    pub source_path: PathBuf,
    pub source_generation: u64,
    pub fingerprint: SourceFingerprint,
    pub settings: PadSettings,
    pub recipe: SampleEditRecipe,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectSaveRequest {
    pub directory: PathBuf,
    pub save_as: bool,
    pub kind: SaveKind,
    pub snapshot: ProjectSaveSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveKind {
    Explicit,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAssetMapping {
    pub pad: PadId,
    pub source_generation: u64,
    pub fingerprint: SourceFingerprint,
    pub project_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveReceipt {
    pub directory: PathBuf,
    pub kind: SaveKind,
    pub project_id: ProjectId,
    pub revision: u64,
    pub canonical_toml: String,
    pub mappings: Vec<ProjectAssetMapping>,
}

#[derive(Debug)]
pub struct ProjectProbe {
    pub directory: PathBuf,
    pub explicit: Option<Result<ProjectDocument, ProjectStoreError>>,
    pub recovery: Option<Result<ProjectDocument, ProjectStoreError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicWritePoint {
    AfterCreate,
    BeforeFlush,
    BeforeFileSync,
    BeforeRename,
    BeforeDirectorySync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicWriteVisibility {
    PreviousDestinationPreserved,
    NewDestinationVisibleDurabilityUnconfirmed,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectStoreError {
    #[error("could not read source {path}: {kind:?}")]
    SourceRead { path: PathBuf, kind: io::ErrorKind },
    #[error("source is not a regular file: {path}")]
    NonRegularFile { path: PathBuf },
    #[error("unsupported audio extension: {path}")]
    UnsupportedExtension { path: PathBuf },
    #[error("encoded source {path} is {bytes} bytes; maximum is {max_bytes}")]
    SourceTooLarge {
        path: PathBuf,
        bytes: u64,
        max_bytes: u64,
    },
    #[error("source changed since it was committed: {path}")]
    SourceChanged { path: PathBuf },
    #[error("symlink is not allowed: {path}")]
    SymlinkRejected { path: PathBuf },
    #[error("existing content-addressed asset failed integrity verification: {path}")]
    AssetIntegrity { path: PathBuf },
    #[error("save-as target is not empty: {path}")]
    SaveAsTargetNotEmpty { path: PathBuf },
    #[error("atomic write failed for {path} at {point:?}: {kind:?}")]
    AtomicWrite {
        path: PathBuf,
        point: AtomicWritePoint,
        kind: io::ErrorKind,
        visibility: AtomicWriteVisibility,
    },
    #[error("recovery metadata is invalid: {path}")]
    RecoveryInvalid { path: PathBuf },
    #[error("filesystem operation {operation} failed for {path}: {kind:?}")]
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        kind: io::ErrorKind,
    },
    #[error("project document is invalid: {0}")]
    ProjectDocument(#[from] sampler_core::ProjectError),
    #[error("project metadata is invalid at {path}: {message}")]
    DocumentInvalid { path: PathBuf, message: String },
    #[error("schema-v1 migration failed: {message}")]
    Migration { message: String },
    #[error("OS entropy failed while generating a project id: {message}")]
    Entropy { message: String },
    #[error("recovery identity or revision does not match: {path}")]
    RecoveryMismatch { path: PathBuf },
}

pub struct ProjectStore;

impl ProjectStore {
    pub fn save(&self, request: ProjectSaveRequest) -> Result<SaveReceipt, ProjectStoreError> {
        self.save_with_hook(request, |_| None)
    }

    pub fn probe(&self, directory: &Path) -> Result<ProjectProbe, ProjectStoreError> {
        validate_directory(directory)?;
        let directory = fs::canonicalize(directory)
            .map_err(|error| filesystem_error("canonicalize directory", directory, error))?;
        Ok(ProjectProbe {
            explicit: probe_document(&directory, "project.toml"),
            recovery: probe_document(&directory, ".sampler-tui-recovery.toml"),
            directory,
        })
    }

    pub fn discard_recovery(
        &self,
        directory: &Path,
        project_id: ProjectId,
        revision: u64,
    ) -> Result<(), ProjectStoreError> {
        validate_directory(directory)?;
        let directory = fs::canonicalize(directory)
            .map_err(|error| filesystem_error("canonicalize directory", directory, error))?;
        let recovery = directory.join(".sampler-tui-recovery.toml");
        match fs::symlink_metadata(&recovery) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(filesystem_error("inspect recovery", &recovery, error)),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ProjectStoreError::SymlinkRejected { path: recovery });
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(ProjectStoreError::RecoveryInvalid { path: recovery });
            }
            Ok(_) => {}
        }
        let source =
            fs::read_to_string(&recovery).map_err(|_| ProjectStoreError::RecoveryInvalid {
                path: recovery.clone(),
            })?;
        let ParsedProjectDocument::Current(document) = ProjectDocument::from_toml(&source)
            .map_err(|_| ProjectStoreError::RecoveryInvalid {
                path: recovery.clone(),
            })?
        else {
            return Err(ProjectStoreError::RecoveryInvalid { path: recovery });
        };
        if document.project_id != project_id || document.revision != revision {
            return Err(ProjectStoreError::RecoveryMismatch { path: recovery });
        }
        fs::remove_file(&recovery)
            .map_err(|error| filesystem_error("delete recovery", &recovery, error))?;
        sync_directory(&directory)
    }

    fn save_with_hook<F>(
        &self,
        request: ProjectSaveRequest,
        mut hook: F,
    ) -> Result<SaveReceipt, ProjectStoreError>
    where
        F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
    {
        let directory = prepare_project_directory(&request.directory, request.save_as)?;
        let audio_directory = ensure_audio_directory(&directory)?;
        let mut document_pads = Vec::with_capacity(request.snapshot.pads.len());
        let mut mappings = Vec::with_capacity(request.snapshot.pads.len());

        for pad in &request.snapshot.pads {
            let actual = SourceFingerprint::from_path(&pad.source_path)?;
            if actual != pad.fingerprint {
                return Err(ProjectStoreError::SourceChanged {
                    path: pad.source_path.clone(),
                });
            }
            let relative_path = format!(
                "audio/{}.{}",
                pad.fingerprint.digest,
                pad.fingerprint.extension.as_str()
            );
            let project_path = directory.join(&relative_path);
            stage_immutable_asset(
                &pad.source_path,
                &project_path,
                pad.fingerprint,
                &audio_directory,
                &mut hook,
            )?;
            document_pads.push(sampler_core::ProjectPad::new(
                pad.pad,
                relative_path,
                pad.fingerprint.digest,
                pad.settings,
                pad.recipe,
            )?);
            mappings.push(ProjectAssetMapping {
                pad: pad.pad,
                source_generation: pad.source_generation,
                fingerprint: pad.fingerprint,
                project_path,
            });
        }

        let document = ProjectDocument::new_v2(
            request.snapshot.project_id,
            request.snapshot.name,
            request.snapshot.revision,
            document_pads,
            request.snapshot.patterns,
        )?;
        let canonical_toml = document.to_toml()?;
        let destination = directory.join(match request.kind {
            SaveKind::Explicit => "project.toml",
            SaveKind::Recovery => ".sampler-tui-recovery.toml",
        });
        atomic_replace(
            &destination,
            canonical_toml.as_bytes(),
            &directory,
            &mut hook,
        )?;

        Ok(SaveReceipt {
            directory,
            kind: request.kind,
            project_id: document.project_id,
            revision: document.revision,
            canonical_toml,
            mappings,
        })
    }
}

fn probe_document(
    directory: &Path,
    file_name: &str,
) -> Option<Result<ProjectDocument, ProjectStoreError>> {
    let path = directory.join(file_name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => return Some(Err(filesystem_error("inspect metadata", &path, error))),
    };
    if metadata.file_type().is_symlink() {
        return Some(Err(ProjectStoreError::SymlinkRejected { path }));
    }
    if !metadata.is_file() {
        return Some(Err(ProjectStoreError::NonRegularFile { path }));
    }
    Some(read_and_upgrade_document(directory, &path))
}

fn read_and_upgrade_document(
    directory: &Path,
    path: &Path,
) -> Result<ProjectDocument, ProjectStoreError> {
    let source =
        fs::read_to_string(path).map_err(|error| filesystem_error("read metadata", path, error))?;
    let parsed = ProjectDocument::from_toml(&source).map_err(|error| {
        ProjectStoreError::DocumentInvalid {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    match parsed {
        ParsedProjectDocument::Current(document) => {
            verify_current_assets(directory, &document)?;
            Ok(document)
        }
        ParsedProjectDocument::Legacy(document) => migrate_legacy(directory, &document),
    }
}

fn safe_project_asset(directory: &Path, relative: &str) -> Result<PathBuf, ProjectStoreError> {
    use std::path::Component;

    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ProjectStoreError::DocumentInvalid {
            path: directory.join(relative),
            message: "asset path escapes the project directory".to_owned(),
        });
    }
    let mut current = directory.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            unreachable!()
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ProjectStoreError::SymlinkRejected { path: current });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(filesystem_error("inspect asset path", &current, error));
            }
        }
    }
    Ok(directory.join(relative))
}

fn verify_current_assets(
    directory: &Path,
    document: &ProjectDocument,
) -> Result<(), ProjectStoreError> {
    for pad in &document.pads {
        let path = safe_project_asset(directory, &pad.audio_path)?;
        let fingerprint = SourceFingerprint::from_path(&path)?;
        if fingerprint.digest != pad.asset_digest {
            return Err(ProjectStoreError::AssetIntegrity { path });
        }
    }
    Ok(())
}

fn migrate_legacy(
    directory: &Path,
    legacy: &LegacyProjectDocument,
) -> Result<ProjectDocument, ProjectStoreError> {
    let mut sources = Vec::with_capacity(legacy.pads().len());
    for pad in legacy.pads() {
        let source = safe_project_asset(directory, pad.audio_path())?;
        let fingerprint = SourceFingerprint::from_path(&source)?;
        sources.push((pad.clone(), source, fingerprint));
    }

    let mut project_id = [0_u8; 16];
    getrandom::fill(&mut project_id).map_err(|error| ProjectStoreError::Entropy {
        message: error.to_string(),
    })?;
    let audio_directory = ensure_audio_directory(directory)?;
    let mut pads = Vec::with_capacity(sources.len());
    for (pad, source, fingerprint) in sources {
        let relative = format!(
            "audio/{}.{}",
            fingerprint.digest,
            fingerprint.extension.as_str()
        );
        let destination = directory.join(&relative);
        stage_immutable_asset(
            &source,
            &destination,
            fingerprint,
            &audio_directory,
            &mut |_| None,
        )?;
        pads.push(sampler_core::ProjectPad::new(
            pad.pad(),
            relative,
            fingerprint.digest,
            pad.settings(),
            pad.recipe(),
        )?);
    }
    let patterns = legacy
        .patterns()
        .iter()
        .map(|pattern| {
            pattern
                .to_editable_lossy()
                .map_err(|error| ProjectStoreError::Migration {
                    message: error.to_string(),
                })
                .and_then(|editable| {
                    ProjectPattern::from_editable(&editable).map_err(ProjectStoreError::from)
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    ProjectDocument::new_v2(
        ProjectId::from_bytes(project_id),
        legacy.name(),
        legacy.revision(),
        pads,
        patterns,
    )
    .map_err(ProjectStoreError::from)
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempPath {
    path: PathBuf,
    armed: bool,
}

impl TempPath {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn filesystem_error(operation: &'static str, path: &Path, error: io::Error) -> ProjectStoreError {
    ProjectStoreError::Filesystem {
        operation,
        path: path.to_path_buf(),
        kind: error.kind(),
    }
}

fn validate_directory(path: &Path) -> Result<(), ProjectStoreError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| filesystem_error("inspect directory", path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(ProjectStoreError::SymlinkRejected {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_dir() {
        return Err(ProjectStoreError::NonRegularFile {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn prepare_project_directory(path: &Path, save_as: bool) -> Result<PathBuf, ProjectStoreError> {
    if save_as {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(ProjectStoreError::SymlinkRejected {
                        path: path.to_path_buf(),
                    });
                }
                if !metadata.is_dir() {
                    return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                        path: path.to_path_buf(),
                    });
                }
                let mut entries = fs::read_dir(path)
                    .map_err(|error| filesystem_error("read save-as target", path, error))?;
                if entries
                    .next()
                    .transpose()
                    .map_err(|error| filesystem_error("read save-as target", path, error))?
                    .is_some()
                {
                    return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                        path: path.to_path_buf(),
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(path)
                    .map_err(|error| filesystem_error("create save-as target", path, error))?;
            }
            Err(error) => return Err(filesystem_error("inspect save-as target", path, error)),
        }
    } else {
        validate_directory(path)?;
    }
    fs::canonicalize(path).map_err(|error| filesystem_error("canonicalize directory", path, error))
}

fn sync_directory(path: &Path) -> Result<(), ProjectStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| filesystem_error("sync directory", path, error))
}

fn ensure_audio_directory(directory: &Path) -> Result<PathBuf, ProjectStoreError> {
    let audio = directory.join("audio");
    match fs::symlink_metadata(&audio) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ProjectStoreError::SymlinkRejected { path: audio });
            }
            if !metadata.is_dir() {
                return Err(ProjectStoreError::NonRegularFile { path: audio });
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&audio)
                .map_err(|error| filesystem_error("create audio directory", &audio, error))?;
            sync_directory(directory)?;
        }
        Err(error) => return Err(filesystem_error("inspect audio directory", &audio, error)),
    }
    Ok(audio)
}

fn validate_regular_destination(path: &Path) -> Result<bool, ProjectStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ProjectStoreError::SymlinkRejected {
                    path: path.to_path_buf(),
                });
            }
            if !metadata.is_file() {
                return Err(ProjectStoreError::NonRegularFile {
                    path: path.to_path_buf(),
                });
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(filesystem_error("inspect destination", path, error)),
    }
}

fn create_sibling_temp(destination: &Path) -> Result<(File, TempPath), ProjectStoreError> {
    let parent = destination
        .parent()
        .ok_or_else(|| ProjectStoreError::Filesystem {
            operation: "resolve temporary parent",
            path: destination.to_path_buf(),
            kind: io::ErrorKind::InvalidInput,
        })?;
    let base = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    loop {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{base}.sampler-tui-tmp-{}-{nonce}",
            std::process::id()
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, TempPath { path, armed: true })),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(ProjectStoreError::AtomicWrite {
                    path: destination.to_path_buf(),
                    point: AtomicWritePoint::AfterCreate,
                    kind: error.kind(),
                    visibility: AtomicWriteVisibility::PreviousDestinationPreserved,
                });
            }
        }
    }
}

fn atomic_error(
    path: &Path,
    point: AtomicWritePoint,
    kind: io::ErrorKind,
    renamed: bool,
) -> ProjectStoreError {
    ProjectStoreError::AtomicWrite {
        path: path.to_path_buf(),
        point,
        kind,
        visibility: if renamed {
            AtomicWriteVisibility::NewDestinationVisibleDurabilityUnconfirmed
        } else {
            AtomicWriteVisibility::PreviousDestinationPreserved
        },
    }
}

fn checkpoint<F>(
    hook: &mut F,
    point: AtomicWritePoint,
    path: &Path,
    renamed: bool,
) -> Result<(), ProjectStoreError>
where
    F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
{
    if let Some(kind) = hook(point) {
        return Err(atomic_error(path, point, kind, renamed));
    }
    Ok(())
}

fn atomic_replace<F>(
    destination: &Path,
    bytes: &[u8],
    directory: &Path,
    hook: &mut F,
) -> Result<(), ProjectStoreError>
where
    F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
{
    validate_regular_destination(destination)?;
    let (mut file, mut temporary) = create_sibling_temp(destination)?;
    checkpoint(hook, AtomicWritePoint::AfterCreate, destination, false)?;
    file.write_all(bytes).map_err(|error| {
        atomic_error(
            destination,
            AtomicWritePoint::BeforeFlush,
            error.kind(),
            false,
        )
    })?;
    checkpoint(hook, AtomicWritePoint::BeforeFlush, destination, false)?;
    file.flush().map_err(|error| {
        atomic_error(
            destination,
            AtomicWritePoint::BeforeFlush,
            error.kind(),
            false,
        )
    })?;
    checkpoint(hook, AtomicWritePoint::BeforeFileSync, destination, false)?;
    file.sync_all().map_err(|error| {
        atomic_error(
            destination,
            AtomicWritePoint::BeforeFileSync,
            error.kind(),
            false,
        )
    })?;
    drop(file);
    checkpoint(hook, AtomicWritePoint::BeforeRename, destination, false)?;
    fs::rename(&temporary.path, destination).map_err(|error| {
        atomic_error(
            destination,
            AtomicWritePoint::BeforeRename,
            error.kind(),
            false,
        )
    })?;
    temporary.disarm();
    checkpoint(
        hook,
        AtomicWritePoint::BeforeDirectorySync,
        destination,
        true,
    )?;
    File::open(directory)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| {
            atomic_error(
                destination,
                AtomicWritePoint::BeforeDirectorySync,
                error.kind(),
                true,
            )
        })
}

fn verify_existing_asset(
    path: &Path,
    expected: SourceFingerprint,
) -> Result<(), ProjectStoreError> {
    let actual = SourceFingerprint::from_path(path)?;
    if actual != expected {
        return Err(ProjectStoreError::AssetIntegrity {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn stage_immutable_asset<F>(
    source: &Path,
    destination: &Path,
    expected: SourceFingerprint,
    audio_directory: &Path,
    hook: &mut F,
) -> Result<(), ProjectStoreError>
where
    F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
{
    if validate_regular_destination(destination)? {
        return verify_existing_asset(destination, expected);
    }

    let mut input = File::open(source).map_err(|error| ProjectStoreError::SourceRead {
        path: source.to_path_buf(),
        kind: error.kind(),
    })?;
    let (mut output, mut temporary) = create_sibling_temp(destination)?;
    checkpoint(hook, AtomicWritePoint::AfterCreate, destination, false)?;
    let mut hasher = Sha256::new();
    let mut encoded_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| ProjectStoreError::SourceRead {
                path: source.to_path_buf(),
                kind: error.kind(),
            })?;
        if read == 0 {
            break;
        }
        encoded_bytes = encoded_bytes.checked_add(read as u64).ok_or_else(|| {
            ProjectStoreError::SourceChanged {
                path: source.to_path_buf(),
            }
        })?;
        if encoded_bytes > MAX_ENCODED_FILE_BYTES {
            return Err(ProjectStoreError::SourceTooLarge {
                path: source.to_path_buf(),
                bytes: encoded_bytes,
                max_bytes: MAX_ENCODED_FILE_BYTES,
            });
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read]).map_err(|error| {
            atomic_error(
                destination,
                AtomicWritePoint::BeforeFlush,
                error.kind(),
                false,
            )
        })?;
    }
    let copied = SourceFingerprint {
        digest: AssetDigest::from_bytes(hasher.finalize().into()),
        encoded_bytes,
        extension: expected.extension,
    };
    if copied != expected {
        return Err(ProjectStoreError::SourceChanged {
            path: source.to_path_buf(),
        });
    }
    checkpoint(hook, AtomicWritePoint::BeforeFlush, destination, false)?;
    output.flush().map_err(|error| {
        atomic_error(
            destination,
            AtomicWritePoint::BeforeFlush,
            error.kind(),
            false,
        )
    })?;
    checkpoint(hook, AtomicWritePoint::BeforeFileSync, destination, false)?;
    output.sync_all().map_err(|error| {
        atomic_error(
            destination,
            AtomicWritePoint::BeforeFileSync,
            error.kind(),
            false,
        )
    })?;
    drop(output);
    checkpoint(hook, AtomicWritePoint::BeforeRename, destination, false)?;
    match fs::hard_link(&temporary.path, destination) {
        Ok(()) => {
            fs::remove_file(&temporary.path).map_err(|error| {
                atomic_error(
                    destination,
                    AtomicWritePoint::BeforeDirectorySync,
                    error.kind(),
                    true,
                )
            })?;
            temporary.disarm();
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            verify_existing_asset(destination, expected)?;
            return Ok(());
        }
        Err(error) => {
            return Err(atomic_error(
                destination,
                AtomicWritePoint::BeforeRename,
                error.kind(),
                false,
            ));
        }
    }
    checkpoint(
        hook,
        AtomicWritePoint::BeforeDirectorySync,
        destination,
        true,
    )?;
    File::open(audio_directory)
        .and_then(|handle| handle.sync_all())
        .map_err(|error| {
            atomic_error(
                destination,
                AtomicWritePoint::BeforeDirectorySync,
                error.kind(),
                true,
            )
        })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use sampler_core::{PadId, PadSettings, ProjectId, ProjectPattern, SampleEditRecipe};
    use sha2::{Digest, Sha256};

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct ProjectFixture {
        root: PathBuf,
        directory: PathBuf,
        source: PathBuf,
        source_bytes: Vec<u8>,
        store: ProjectStore,
    }

    impl ProjectFixture {
        fn new() -> Self {
            let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "sampler-tui-project-store-{}-{id}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            let directory = root.join("project");
            fs::create_dir(&directory).unwrap();
            let source = root.join("kick.WAV");
            let source_bytes = b"exact encoded source bytes".to_vec();
            fs::write(&source, &source_bytes).unwrap();
            Self {
                root,
                directory,
                source,
                source_bytes,
                store: ProjectStore,
            }
        }

        fn request(&self, revision: u64, kind: SaveKind) -> ProjectSaveRequest {
            let fingerprint = SourceFingerprint::from_path(&self.source).unwrap();
            ProjectSaveRequest {
                directory: self.directory.clone(),
                save_as: false,
                kind,
                snapshot: ProjectSaveSnapshot {
                    project_id: ProjectId::from_bytes([0x41; 16]),
                    name: "fixture".to_owned(),
                    revision,
                    pads: vec![ProjectSavePad {
                        pad: PadId::first(),
                        source_path: self.source.clone(),
                        source_generation: 7,
                        fingerprint,
                        settings: PadSettings::default(),
                        recipe: SampleEditRecipe::identity(),
                    }],
                    patterns: Vec::<ProjectPattern>::new(),
                },
            }
        }

        fn temp_entries(&self) -> Vec<PathBuf> {
            fs::read_dir(&self.directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".sampler-tui-tmp-")
                })
                .collect()
        }
    }

    impl Drop for ProjectFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn sha256(path: &Path) -> sampler_core::AssetDigest {
        let bytes = fs::read(path).unwrap();
        sampler_core::AssetDigest::from_bytes(Sha256::digest(bytes).into())
    }

    #[test]
    fn explicit_save_copies_immutable_assets_and_atomically_replaces_only_project_toml() {
        let fixture = ProjectFixture::new();
        let first = fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        assert_eq!(
            read(&fixture.directory.join("project.toml")),
            first.canonical_toml
        );
        assert!(
            !fixture
                .directory
                .join(".sampler-tui-recovery.toml")
                .exists()
        );
        assert_eq!(
            sha256(&first.mappings[0].project_path),
            first.mappings[0].fingerprint.digest
        );
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.source_bytes);
    }

    #[test]
    fn injected_failure_before_metadata_rename_preserves_the_old_document() {
        let fixture = ProjectFixture::new();
        fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        let before = read(&fixture.directory.join("project.toml"));
        let error = fixture
            .store
            .save_with_hook(fixture.request(2, SaveKind::Explicit), |point| {
                (point == AtomicWritePoint::BeforeRename).then_some(std::io::ErrorKind::Other)
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectStoreError::AtomicWrite {
                visibility: AtomicWriteVisibility::PreviousDestinationPreserved,
                ..
            }
        ));
        assert_eq!(read(&fixture.directory.join("project.toml")), before);
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.source_bytes);
        assert!(fixture.temp_entries().is_empty());
    }

    #[test]
    fn identical_content_deduplicates_and_an_existing_target_is_rehashed() {
        let fixture = ProjectFixture::new();
        let first = fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        let asset = first.mappings[0].project_path.clone();
        let duplicate_source = fixture.root.join("duplicate.wav");
        fs::write(&duplicate_source, &fixture.source_bytes).unwrap();
        let mut duplicate = fixture.request(2, SaveKind::Explicit);
        duplicate.snapshot.pads[0].source_path = duplicate_source;
        fixture.store.save(duplicate).unwrap();
        assert_eq!(
            fs::read_dir(fixture.directory.join("audio"))
                .unwrap()
                .count(),
            1
        );

        let old_document = read(&fixture.directory.join("project.toml"));
        fs::write(&asset, b"corrupt existing target").unwrap();
        let error = fixture
            .store
            .save(fixture.request(3, SaveKind::Explicit))
            .unwrap_err();
        assert!(matches!(error, ProjectStoreError::AssetIntegrity { .. }));
        assert_eq!(read(&fixture.directory.join("project.toml")), old_document);
    }

    #[test]
    fn changed_or_missing_source_fails_before_metadata_rename() {
        for missing in [false, true] {
            let fixture = ProjectFixture::new();
            fixture
                .store
                .save(fixture.request(1, SaveKind::Explicit))
                .unwrap();
            let before = read(&fixture.directory.join("project.toml"));
            let request = fixture.request(2, SaveKind::Explicit);
            if missing {
                fs::remove_file(&fixture.source).unwrap();
            } else {
                fs::write(&fixture.source, b"changed after committed fingerprint").unwrap();
            }
            let error = fixture.store.save(request).unwrap_err();
            assert!(matches!(
                error,
                ProjectStoreError::SourceRead { .. } | ProjectStoreError::SourceChanged { .. }
            ));
            assert_eq!(read(&fixture.directory.join("project.toml")), before);
        }
    }

    #[test]
    fn autosave_changes_only_recovery_and_probe_keeps_independent_outcomes() {
        let fixture = ProjectFixture::new();
        fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        let explicit = read(&fixture.directory.join("project.toml"));
        fixture
            .store
            .save(fixture.request(2, SaveKind::Recovery))
            .unwrap();
        assert_eq!(read(&fixture.directory.join("project.toml")), explicit);
        assert!(
            fixture
                .directory
                .join(".sampler-tui-recovery.toml")
                .exists()
        );

        fs::write(fixture.directory.join("project.toml"), "not valid TOML").unwrap();
        fs::write(
            fixture
                .directory
                .join(".project.toml.sampler-tui-tmp-stale"),
            b"ignored",
        )
        .unwrap();
        let probe = fixture.store.probe(&fixture.directory).unwrap();
        assert!(probe.explicit.unwrap().is_err());
        assert_eq!(
            probe.recovery.unwrap().unwrap().revision,
            2,
            "valid recovery must survive corrupt explicit metadata"
        );
    }

    #[test]
    fn every_pre_rename_failure_preserves_old_metadata_and_cleans_its_temp() {
        for point in [
            AtomicWritePoint::AfterCreate,
            AtomicWritePoint::BeforeFlush,
            AtomicWritePoint::BeforeFileSync,
            AtomicWritePoint::BeforeRename,
        ] {
            let fixture = ProjectFixture::new();
            fixture
                .store
                .save(fixture.request(1, SaveKind::Explicit))
                .unwrap();
            let before = read(&fixture.directory.join("project.toml"));
            let error = fixture
                .store
                .save_with_hook(fixture.request(2, SaveKind::Explicit), |candidate| {
                    (candidate == point).then_some(std::io::ErrorKind::Other)
                })
                .unwrap_err();
            assert!(matches!(
                error,
                ProjectStoreError::AtomicWrite {
                    visibility: AtomicWriteVisibility::PreviousDestinationPreserved,
                    ..
                }
            ));
            assert_eq!(read(&fixture.directory.join("project.toml")), before);
            assert!(fixture.temp_entries().is_empty());
        }
    }

    #[test]
    fn post_rename_directory_sync_failure_reports_visible_but_unconfirmed_document() {
        let fixture = ProjectFixture::new();
        fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        let error = fixture
            .store
            .save_with_hook(fixture.request(2, SaveKind::Explicit), |point| {
                (point == AtomicWritePoint::BeforeDirectorySync)
                    .then_some(std::io::ErrorKind::Other)
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectStoreError::AtomicWrite {
                visibility: AtomicWriteVisibility::NewDestinationVisibleDurabilityUnconfirmed,
                ..
            }
        ));
        let parsed = sampler_core::ProjectDocument::from_toml(&read(
            &fixture.directory.join("project.toml"),
        ))
        .unwrap();
        assert_eq!(parsed.current().unwrap().revision, 2);
    }

    #[test]
    fn save_as_accepts_only_nonexistent_or_empty_directories() {
        let fixture = ProjectFixture::new();
        let mut nonempty = fixture.request(1, SaveKind::Explicit);
        nonempty.save_as = true;
        fs::write(fixture.directory.join("unrelated"), b"keep").unwrap();
        assert!(matches!(
            fixture.store.save(nonempty),
            Err(ProjectStoreError::SaveAsTargetNotEmpty { .. })
        ));

        let target = fixture.root.join("new-project");
        let mut nonexistent = fixture.request(1, SaveKind::Explicit);
        nonexistent.directory = target.clone();
        nonexistent.save_as = true;
        fixture.store.save(nonexistent).unwrap();
        assert!(target.join("project.toml").is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_external_paths_work_when_the_filesystem_permits_them() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let fixture = ProjectFixture::new();
        let non_utf8 = fixture
            .root
            .join(OsString::from_vec(b"source-\xff.wav".to_vec()));
        fs::write(&non_utf8, &fixture.source_bytes).unwrap();
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.snapshot.pads[0].source_path = non_utf8;
        fixture.store.save(request).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_sources_are_rejected() {
        use std::os::unix::fs::symlink;

        let fixture = ProjectFixture::new();
        let link = fixture.root.join("linked.wav");
        symlink(&fixture.source, &link).unwrap();
        let mut linked = fixture.request(1, SaveKind::Explicit);
        linked.snapshot.pads[0].source_path = link;
        assert!(matches!(
            fixture.store.save(linked),
            Err(ProjectStoreError::SymlinkRejected { .. })
        ));
    }

    #[test]
    fn path_escape_is_rejected_and_recovery_deletion_never_deletes_explicit() {
        let fixture = ProjectFixture::new();
        fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        let explicit = read(&fixture.directory.join("project.toml"));
        fs::write(
            fixture.directory.join(".sampler-tui-recovery.toml"),
            "schema_version = 1\nname = \"escape\"\n[[pads]]\naudio_path = \"../outside.wav\"\n[pads.pad]\nbank = 0\nindex = 0\n[pads.settings]\nmode = \"OneShot\"\ngain_db = 0.0\npan = 0.0\npitch_semitones = 0.0\n",
        )
        .unwrap();
        let probe = fixture.store.probe(&fixture.directory).unwrap();
        assert!(probe.recovery.unwrap().is_err());
        assert_eq!(read(&fixture.directory.join("project.toml")), explicit);

        let error = fixture
            .store
            .discard_recovery(&fixture.directory, ProjectId::from_bytes([0x41; 16]), 2)
            .unwrap_err();
        assert!(matches!(error, ProjectStoreError::RecoveryInvalid { .. }));
        assert_eq!(read(&fixture.directory.join("project.toml")), explicit);
    }
}
