use std::{
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use rustix::fs::{FileType as RustixFileType, FlockOperation, Mode, OFlags};
use sampler_audio::{EncodedAudioFormat, probe_shared_audio_format};
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

    pub(crate) fn from_encoded_format(path: &Path, format: EncodedAudioFormat) -> Self {
        match format {
            EncodedAudioFormat::Wav => Self::Wav,
            EncodedAudioFormat::Aiff
                if path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("aif")) =>
            {
                Self::Aif
            }
            EncodedAudioFormat::Aiff => Self::Aiff,
            EncodedAudioFormat::Flac => Self::Flac,
            EncodedAudioFormat::Mp3 => Self::Mp3,
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
        ValidatedSource::open(path).map(|source| source.fingerprint)
    }

    /// Fingerprints bytes already read by the loader without reopening `path`.
    pub fn from_encoded_bytes(path: &Path, encoded: &[u8]) -> Result<Self, ProjectStoreError> {
        let extension = SupportedAudioExtension::from_path(path)?;
        Self::from_encoded_bytes_with_extension(path, encoded, extension)
    }

    /// Fingerprints already-read bytes with a container extension established by probing.
    pub fn from_encoded_bytes_with_extension(
        path: &Path,
        encoded: &[u8],
        extension: SupportedAudioExtension,
    ) -> Result<Self, ProjectStoreError> {
        let mut builder = SourceFingerprintBuilder::new(path, extension);
        builder.update(encoded)?;
        Ok(builder.finish())
    }
}

struct SourceFingerprintBuilder<'a> {
    path: &'a Path,
    extension: SupportedAudioExtension,
    hasher: Sha256,
    encoded_bytes: u64,
}

impl<'a> SourceFingerprintBuilder<'a> {
    fn new(path: &'a Path, extension: SupportedAudioExtension) -> Self {
        Self {
            path,
            extension,
            hasher: Sha256::new(),
            encoded_bytes: 0,
        }
    }

    fn update(&mut self, bytes: &[u8]) -> Result<(), ProjectStoreError> {
        self.encoded_bytes = self
            .encoded_bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| ProjectStoreError::SourceTooLarge {
                path: self.path.to_path_buf(),
                bytes: u64::MAX,
                max_bytes: MAX_ENCODED_FILE_BYTES,
            })?;
        if self.encoded_bytes > MAX_ENCODED_FILE_BYTES {
            return Err(ProjectStoreError::SourceTooLarge {
                path: self.path.to_path_buf(),
                bytes: self.encoded_bytes,
                max_bytes: MAX_ENCODED_FILE_BYTES,
            });
        }
        self.hasher.update(bytes);
        Ok(())
    }

    fn finish(self) -> SourceFingerprint {
        SourceFingerprint {
            digest: AssetDigest::from_bytes(self.hasher.finalize().into()),
            encoded_bytes: self.encoded_bytes,
            extension: self.extension,
        }
    }
}

struct ValidatedSource {
    file: File,
    fingerprint: SourceFingerprint,
    path: PathBuf,
}

impl ValidatedSource {
    fn open(path: &Path) -> Result<Self, ProjectStoreError> {
        let (parent, leaf) = open_anchored_parent(path, true)?;
        let owned = rustix::fs::openat(
            &parent,
            &leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| open_source_error(path, error))?;
        ensure_fd_type(&owned, path, RustixFileType::RegularFile)?;
        let mut file = File::from(owned);
        let fingerprint = fingerprint_validated_audio_handle(&mut file, path)?;
        Ok(Self {
            file,
            fingerprint,
            path: path.to_path_buf(),
        })
    }

    fn rewind(&mut self) -> Result<(), ProjectStoreError> {
        self.file
            .rewind()
            .map_err(|error| ProjectStoreError::SourceRead {
                path: self.path.clone(),
                kind: error.kind(),
            })
    }
}

struct ProjectDirectory {
    path: PathBuf,
    file: File,
}

impl ProjectDirectory {
    fn open_existing(path: &Path) -> Result<Self, ProjectStoreError> {
        let (parent, leaf) = open_anchored_parent(path, false)?;
        let owned = rustix::fs::openat(
            &parent,
            &leaf,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| open_path_error(path, error))?;
        ensure_fd_type(&owned, path, RustixFileType::Directory)?;
        let path = fs::canonicalize(path)
            .map_err(|error| filesystem_error("canonicalize directory", path, error))?;
        Ok(Self {
            path,
            file: File::from(owned),
        })
    }

    fn open_audio_directory(&self) -> Result<AudioDirectory, ProjectStoreError> {
        let path = self.path.join("audio");
        let owned = rustix::fs::openat(
            &self.file,
            "audio",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| open_path_error(&path, error))?;
        ensure_fd_type(&owned, &path, RustixFileType::Directory)?;
        Ok(AudioDirectory {
            path,
            file: File::from(owned),
        })
    }

    fn ensure_audio_directory(&self) -> Result<AudioDirectory, ProjectStoreError> {
        match self.open_audio_directory() {
            Ok(audio) => Ok(audio),
            Err(ProjectStoreError::Filesystem {
                kind: io::ErrorKind::NotFound,
                ..
            }) => {
                rustix::fs::mkdirat(&self.file, "audio", Mode::from_raw_mode(0o755)).map_err(
                    |error| {
                        filesystem_error(
                            "create audio directory",
                            &self.path.join("audio"),
                            io::Error::from(error),
                        )
                    },
                )?;
                self.file.sync_all().map_err(|error| {
                    filesystem_error("sync project directory", &self.path, error)
                })?;
                self.open_audio_directory()
            }
            Err(error) => Err(error),
        }
    }

    fn open_asset(&self, relative: &str) -> Result<ValidatedSource, ProjectStoreError> {
        use std::path::Component;

        let relative = Path::new(relative);
        let components = relative.components().collect::<Vec<_>>();
        if components.len() < 2
            || !matches!(components[0], Component::Normal(first) if first == "audio")
            || components
                .iter()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(ProjectStoreError::DocumentInvalid {
                path: self.path.join(relative),
                message: "asset path escapes the project directory".to_owned(),
            });
        }
        let mut directory = self.open_audio_directory()?;
        for component in &components[1..components.len() - 1] {
            let Component::Normal(component) = component else {
                unreachable!()
            };
            directory = directory.open_directory(component)?;
        }
        let Component::Normal(leaf) = components[components.len() - 1] else {
            unreachable!()
        };
        let display = self.path.join(relative);
        directory.open_leaf(Path::new(leaf), &display)
    }

    fn lock_exclusive(&self) -> Result<ProjectLock, ProjectStoreError> {
        let path = self.path.join(".sampler-tui.lock");
        let mut create_attempts = 0;
        let owned = loop {
            create_attempts += 1;
            match rustix::fs::openat(
                &self.file,
                ".sampler-tui.lock",
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(owned) => break owned,
                Err(error) if error == rustix::io::Errno::EXIST => {
                    match rustix::fs::openat(
                        &self.file,
                        ".sampler-tui.lock",
                        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    ) {
                        Ok(owned) => break owned,
                        Err(error) if error == rustix::io::Errno::NOENT && create_attempts < 4 => {
                            continue;
                        }
                        Err(error) => return Err(open_path_error(&path, error)),
                    }
                }
                Err(error) => return Err(open_path_error(&path, error)),
            }
        };
        ensure_fd_type(&owned, &path, RustixFileType::RegularFile)?;
        let file = File::from(owned);
        rustix::fs::flock(&file, FlockOperation::LockExclusive)
            .map_err(|error| filesystem_error("lock project", &path, io::Error::from(error)))?;
        self.file
            .sync_all()
            .map_err(|error| filesystem_error("sync project lock", &self.path, error))?;
        Ok(ProjectLock { _file: file })
    }
}

fn open_anchored_parent(
    path: &Path,
    source_error: bool,
) -> Result<(File, std::ffi::OsString), ProjectStoreError> {
    let leaf = path
        .file_name()
        .ok_or_else(|| ProjectStoreError::NonRegularFile {
            path: path.to_path_buf(),
        })?
        .to_os_string();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let canonical = fs::canonicalize(parent).map_err(|error| {
        if source_error {
            ProjectStoreError::SourceRead {
                path: path.to_path_buf(),
                kind: error.kind(),
            }
        } else {
            filesystem_error("canonicalize parent", path, error)
        }
    })?;
    let root = rustix::fs::openat(
        rustix::fs::CWD,
        Path::new("/"),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| open_path_error(Path::new("/"), error))?;
    let mut directory = File::from(root);
    for component in canonical.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(component) => {
                let display = Path::new("/").join(component);
                let owned = rustix::fs::openat(
                    &directory,
                    component,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| open_path_error(&display, error))?;
                ensure_fd_type(&owned, &display, RustixFileType::Directory)?;
                directory = File::from(owned);
            }
            _ => {
                return Err(ProjectStoreError::DocumentInvalid {
                    path: path.to_path_buf(),
                    message: "canonical parent contains unsupported components".to_owned(),
                });
            }
        }
    }
    Ok((directory, leaf))
}

struct ProjectLock {
    _file: File,
}

struct AudioDirectory {
    path: PathBuf,
    file: File,
}

impl AudioDirectory {
    fn open_directory(&self, leaf: &std::ffi::OsStr) -> Result<Self, ProjectStoreError> {
        let path = self.path.join(leaf);
        let owned = rustix::fs::openat(
            &self.file,
            leaf,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| open_path_error(&path, error))?;
        ensure_fd_type(&owned, &path, RustixFileType::Directory)?;
        Ok(Self {
            path,
            file: File::from(owned),
        })
    }

    fn open_leaf(
        &self,
        leaf: &Path,
        display_path: &Path,
    ) -> Result<ValidatedSource, ProjectStoreError> {
        if leaf.components().count() != 1
            || !matches!(
                leaf.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(ProjectStoreError::DocumentInvalid {
                path: display_path.to_path_buf(),
                message: "asset leaf is not a single normal component".to_owned(),
            });
        }
        let extension = SupportedAudioExtension::from_path(leaf)?;
        let owned = rustix::fs::openat(
            &self.file,
            leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|error| open_source_error(display_path, error))?;
        ensure_fd_type(&owned, display_path, RustixFileType::RegularFile)?;
        let mut file = File::from(owned);
        let fingerprint = hash_validated_handle(&mut file, display_path, extension)?;
        Ok(ValidatedSource {
            file,
            fingerprint,
            path: display_path.to_path_buf(),
        })
    }

    fn try_open_leaf(
        &self,
        leaf: &Path,
        display_path: &Path,
    ) -> Result<Option<ValidatedSource>, ProjectStoreError> {
        let extension = SupportedAudioExtension::from_path(leaf)?;
        let owned = match rustix::fs::openat(
            &self.file,
            leaf,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(owned) => owned,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(open_source_error(display_path, error)),
        };
        ensure_fd_type(&owned, display_path, RustixFileType::RegularFile)?;
        let mut file = File::from(owned);
        let fingerprint = hash_validated_handle(&mut file, display_path, extension)?;
        Ok(Some(ValidatedSource {
            file,
            fingerprint,
            path: display_path.to_path_buf(),
        }))
    }
}

fn open_source_error(path: &Path, error: rustix::io::Errno) -> ProjectStoreError {
    if error == rustix::io::Errno::LOOP {
        ProjectStoreError::SymlinkRejected {
            path: path.to_path_buf(),
        }
    } else {
        ProjectStoreError::SourceRead {
            path: path.to_path_buf(),
            kind: io::Error::from(error).kind(),
        }
    }
}

fn open_path_error(path: &Path, error: rustix::io::Errno) -> ProjectStoreError {
    if error == rustix::io::Errno::LOOP {
        ProjectStoreError::SymlinkRejected {
            path: path.to_path_buf(),
        }
    } else {
        filesystem_error("open no-follow path", path, io::Error::from(error))
    }
}

fn open_optional_regular_at(
    directory: &File,
    relative: &Path,
    display_path: &Path,
) -> Result<Option<File>, ProjectStoreError> {
    let owned = match rustix::fs::openat(
        directory,
        relative,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(owned) => owned,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(open_path_error(display_path, error)),
    };
    ensure_fd_type(&owned, display_path, RustixFileType::RegularFile)?;
    Ok(Some(File::from(owned)))
}

fn ensure_fd_type(
    fd: &impl std::os::fd::AsFd,
    path: &Path,
    expected: RustixFileType,
) -> Result<(), ProjectStoreError> {
    let stat = rustix::fs::fstat(fd)
        .map_err(|error| filesystem_error("fstat", path, io::Error::from(error)))?;
    if RustixFileType::from_raw_mode(stat.st_mode) != expected {
        return Err(ProjectStoreError::NonRegularFile {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn hash_validated_handle(
    file: &mut File,
    path: &Path,
    extension: SupportedAudioExtension,
) -> Result<SourceFingerprint, ProjectStoreError> {
    let mut builder = SourceFingerprintBuilder::new(path, extension);
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
        builder.update(&buffer[..read])?;
    }
    file.rewind()
        .map_err(|error| ProjectStoreError::SourceRead {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    Ok(builder.finish())
}

fn fingerprint_validated_audio_handle(
    file: &mut File,
    path: &Path,
) -> Result<SourceFingerprint, ProjectStoreError> {
    let mut encoded = Vec::new();
    file.take(MAX_ENCODED_FILE_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|error| ProjectStoreError::SourceRead {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    if encoded.len() as u64 > MAX_ENCODED_FILE_BYTES {
        return Err(ProjectStoreError::SourceTooLarge {
            path: path.to_path_buf(),
            bytes: encoded.len() as u64,
            max_bytes: MAX_ENCODED_FILE_BYTES,
        });
    }
    let encoded = Arc::<[u8]>::from(encoded);
    let format = probe_shared_audio_format(path, Arc::clone(&encoded)).map_err(|_| {
        ProjectStoreError::UnsupportedExtension {
            path: path.to_path_buf(),
        }
    })?;
    let extension = SupportedAudioExtension::from_encoded_format(path, format);
    let fingerprint =
        SourceFingerprint::from_encoded_bytes_with_extension(path, &encoded, extension)?;
    file.rewind()
        .map_err(|error| ProjectStoreError::SourceRead {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;
    Ok(fingerprint)
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

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectProbe {
    pub directory: PathBuf,
    pub explicit: Option<Result<ProjectDocument, ProjectStoreError>>,
    pub recovery: Option<Result<ProjectDocument, ProjectStoreError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicWritePoint {
    AfterSaveAsPreflight,
    AfterSourceFingerprint,
    BeforeParentDirectorySync,
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
        let project = ProjectDirectory::open_existing(directory)?;
        let _lock = project.lock_exclusive()?;
        let directory = project.path.clone();
        Ok(ProjectProbe {
            explicit: probe_document(&project, "project.toml"),
            recovery: probe_document(&project, ".sampler-tui-recovery.toml"),
            directory,
        })
    }

    pub fn discard_recovery(
        &self,
        directory: &Path,
        project_id: ProjectId,
        revision: u64,
    ) -> Result<(), ProjectStoreError> {
        self.discard_recovery_with_hook(directory, project_id, revision, || {})
    }

    fn discard_recovery_with_hook<F>(
        &self,
        directory: &Path,
        project_id: ProjectId,
        revision: u64,
        hook: F,
    ) -> Result<(), ProjectStoreError>
    where
        F: FnOnce(),
    {
        let project = ProjectDirectory::open_existing(directory)?;
        let _lock = project.lock_exclusive()?;
        let directory = project.path.clone();
        let recovery = directory.join(".sampler-tui-recovery.toml");
        let Some(mut recovery_file) = open_optional_regular_at(
            &project.file,
            Path::new(".sampler-tui-recovery.toml"),
            &recovery,
        )?
        else {
            return Ok(());
        };
        let mut source = String::new();
        recovery_file.read_to_string(&mut source).map_err(|_| {
            ProjectStoreError::RecoveryInvalid {
                path: recovery.clone(),
            }
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
        hook();
        rustix::fs::unlinkat(
            &project.file,
            ".sampler-tui-recovery.toml",
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|error| filesystem_error("delete recovery", &recovery, io::Error::from(error)))?;
        project
            .file
            .sync_all()
            .map_err(|error| filesystem_error("sync directory", &directory, error))
    }

    fn save_with_hook<F>(
        &self,
        request: ProjectSaveRequest,
        mut hook: F,
    ) -> Result<SaveReceipt, ProjectStoreError>
    where
        F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
    {
        let directory = prepare_project_directory(&request.directory, request.save_as, &mut hook)?;
        if request.save_as {
            checkpoint(
                &mut hook,
                AtomicWritePoint::AfterSaveAsPreflight,
                &directory,
                false,
            )?;
        }
        let project = ProjectDirectory::open_existing(&directory)?;
        let _lock = project.lock_exclusive()?;
        if request.save_as {
            recheck_save_as_target(&project)?;
        }
        let directory = project.path.clone();
        let audio_directory = project.ensure_audio_directory()?;
        let mut document_pads = Vec::with_capacity(request.snapshot.pads.len());
        let mut mappings = Vec::with_capacity(request.snapshot.pads.len());

        for pad in &request.snapshot.pads {
            let mut source = match ValidatedSource::open(&pad.source_path) {
                Err(ProjectStoreError::UnsupportedExtension { .. }) => {
                    return Err(ProjectStoreError::SourceChanged {
                        path: pad.source_path.clone(),
                    });
                }
                result => result?,
            };
            if source.fingerprint != pad.fingerprint {
                return Err(ProjectStoreError::SourceChanged {
                    path: pad.source_path.clone(),
                });
            }
            checkpoint(
                &mut hook,
                AtomicWritePoint::AfterSourceFingerprint,
                &pad.source_path,
                false,
            )?;
            let relative_path = format!(
                "audio/{}.{}",
                pad.fingerprint.digest,
                pad.fingerprint.extension.as_str()
            );
            let project_path = directory.join(&relative_path);
            stage_immutable_asset(
                &mut source,
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
        atomic_replace(&destination, canonical_toml.as_bytes(), &project, &mut hook)?;

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
    project: &ProjectDirectory,
    file_name: &str,
) -> Option<Result<ProjectDocument, ProjectStoreError>> {
    let path = project.path.join(file_name);
    let mut file = match open_optional_regular_at(&project.file, Path::new(file_name), &path) {
        Ok(Some(file)) => file,
        Ok(None) => return None,
        Err(error) => return Some(Err(error)),
    };
    Some(read_and_upgrade_document(project, &mut file, &path))
}

fn read_and_upgrade_document(
    project: &ProjectDirectory,
    file: &mut File,
    path: &Path,
) -> Result<ProjectDocument, ProjectStoreError> {
    let mut source = String::new();
    file.read_to_string(&mut source)
        .map_err(|error| filesystem_error("read metadata", path, error))?;
    let parsed = ProjectDocument::from_toml(&source).map_err(|error| {
        ProjectStoreError::DocumentInvalid {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    match parsed {
        ParsedProjectDocument::Current(document) => {
            verify_current_assets(project, &document)?;
            Ok(document)
        }
        ParsedProjectDocument::Legacy(document) => migrate_legacy(project, &document),
    }
}

fn verify_current_assets(
    project: &ProjectDirectory,
    document: &ProjectDocument,
) -> Result<(), ProjectStoreError> {
    for pad in &document.pads {
        let source = project.open_asset(&pad.audio_path)?;
        if source.fingerprint.digest != pad.asset_digest {
            return Err(ProjectStoreError::AssetIntegrity { path: source.path });
        }
    }
    Ok(())
}

fn migrate_legacy(
    project: &ProjectDirectory,
    legacy: &LegacyProjectDocument,
) -> Result<ProjectDocument, ProjectStoreError> {
    let mut sources = Vec::with_capacity(legacy.pads().len());
    for pad in legacy.pads() {
        let source = project.open_asset(pad.audio_path())?;
        sources.push((pad.clone(), source));
    }

    let mut project_id = [0_u8; 16];
    getrandom::fill(&mut project_id).map_err(|error| ProjectStoreError::Entropy {
        message: error.to_string(),
    })?;
    let audio_directory = project.ensure_audio_directory()?;
    let mut pads = Vec::with_capacity(sources.len());
    for (pad, mut source) in sources {
        let fingerprint = source.fingerprint;
        let relative = format!(
            "audio/{}.{}",
            fingerprint.digest,
            fingerprint.extension.as_str()
        );
        let destination = project.path.join(&relative);
        stage_immutable_asset(
            &mut source,
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

struct AnchoredTemp {
    directory: File,
    leaf: std::ffi::OsString,
    armed: bool,
}

impl AnchoredTemp {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AnchoredTemp {
    fn drop(&mut self) {
        if self.armed {
            let _ = rustix::fs::unlinkat(&self.directory, &self.leaf, rustix::fs::AtFlags::empty());
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

fn prepare_project_directory<F>(
    path: &Path,
    save_as: bool,
    hook: &mut F,
) -> Result<PathBuf, ProjectStoreError>
where
    F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
{
    if save_as {
        match fs::symlink_metadata(path) {
            Ok(_) => validate_empty_save_as_target(path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(path) {
                Ok(()) => {
                    checkpoint(
                        hook,
                        AtomicWritePoint::BeforeParentDirectorySync,
                        path,
                        true,
                    )?;
                    let parent = path
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or(Path::new("."));
                    File::open(parent)
                        .and_then(|directory| directory.sync_all())
                        .map_err(|error| {
                            atomic_error(
                                path,
                                AtomicWritePoint::BeforeParentDirectorySync,
                                error.kind(),
                                true,
                            )
                        })?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    validate_empty_save_as_target(path)?;
                    drop(ProjectDirectory::open_existing(path)?);
                }
                Err(error) => {
                    return Err(filesystem_error("create save-as target", path, error));
                }
            },
            Err(error) => return Err(filesystem_error("inspect save-as target", path, error)),
        }
    } else {
        validate_directory(path)?;
    }
    fs::canonicalize(path).map_err(|error| filesystem_error("canonicalize directory", path, error))
}

fn validate_empty_save_as_target(path: &Path) -> Result<(), ProjectStoreError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| filesystem_error("inspect save-as target", path, error))?;
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
    let mut entries =
        fs::read_dir(path).map_err(|error| filesystem_error("read save-as target", path, error))?;
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
    Ok(())
}

fn recheck_save_as_target(project: &ProjectDirectory) -> Result<(), ProjectStoreError> {
    let entries = fs::read_dir(&project.path)
        .map_err(|error| filesystem_error("recheck save-as target", &project.path, error))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| filesystem_error("recheck save-as target", &project.path, error))?;
        if entry.file_name() != ".sampler-tui.lock" {
            return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                path: project.path.clone(),
            });
        }
        let lock_path = project.path.join(".sampler-tui.lock");
        if open_optional_regular_at(&project.file, Path::new(".sampler-tui.lock"), &lock_path)?
            .is_none()
        {
            return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                path: project.path.clone(),
            });
        }
    }
    Ok(())
}

fn create_anchored_temp(
    directory: &File,
    directory_path: &Path,
    destination: &Path,
) -> Result<(File, AnchoredTemp), ProjectStoreError> {
    let base = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("asset");
    loop {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let leaf = std::ffi::OsString::from(format!(
            ".{base}.sampler-tui-tmp-{}-{nonce}",
            std::process::id()
        ));
        match rustix::fs::openat(
            directory,
            &leaf,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(owned) => {
                let directory = directory.try_clone().map_err(|error| {
                    filesystem_error("clone directory handle", directory_path, error)
                })?;
                return Ok((
                    File::from(owned),
                    AnchoredTemp {
                        directory,
                        leaf,
                        armed: true,
                    },
                ));
            }
            Err(rustix::io::Errno::EXIST) => continue,
            Err(error) => {
                return Err(atomic_error(
                    destination,
                    AtomicWritePoint::AfterCreate,
                    io::Error::from(error).kind(),
                    false,
                ));
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
    project: &ProjectDirectory,
    hook: &mut F,
) -> Result<(), ProjectStoreError>
where
    F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
{
    let leaf = destination.file_name().map(Path::new).ok_or_else(|| {
        ProjectStoreError::DocumentInvalid {
            path: destination.to_path_buf(),
            message: "metadata destination has no leaf".to_owned(),
        }
    })?;
    let _ = open_optional_regular_at(&project.file, leaf, destination)?;
    let (mut file, mut temporary) =
        create_anchored_temp(&project.file, &project.path, destination)?;
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
    rustix::fs::renameat(&project.file, &temporary.leaf, &project.file, leaf).map_err(|error| {
        atomic_error(
            destination,
            AtomicWritePoint::BeforeRename,
            io::Error::from(error).kind(),
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
    project.file.sync_all().map_err(|error| {
        atomic_error(
            destination,
            AtomicWritePoint::BeforeDirectorySync,
            error.kind(),
            true,
        )
    })
}

fn verify_existing_asset(
    audio: &AudioDirectory,
    leaf: &Path,
    path: &Path,
    expected: SourceFingerprint,
) -> Result<(), ProjectStoreError> {
    let Some(actual) = audio.try_open_leaf(leaf, path)? else {
        return Err(ProjectStoreError::SourceRead {
            path: path.to_path_buf(),
            kind: io::ErrorKind::NotFound,
        });
    };
    if actual.fingerprint != expected {
        return Err(ProjectStoreError::AssetIntegrity {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn stage_immutable_asset<F>(
    source: &mut ValidatedSource,
    destination: &Path,
    expected: SourceFingerprint,
    audio: &AudioDirectory,
    hook: &mut F,
) -> Result<(), ProjectStoreError>
where
    F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
{
    let leaf = destination.file_name().map(Path::new).ok_or_else(|| {
        ProjectStoreError::DocumentInvalid {
            path: destination.to_path_buf(),
            message: "asset destination has no leaf".to_owned(),
        }
    })?;
    if let Some(actual) = audio.try_open_leaf(leaf, destination)? {
        if actual.fingerprint != expected {
            return Err(ProjectStoreError::AssetIntegrity {
                path: destination.to_path_buf(),
            });
        }
        return Ok(());
    }

    source.rewind()?;
    let (mut output, mut temporary) = create_anchored_temp(&audio.file, &audio.path, destination)?;
    checkpoint(hook, AtomicWritePoint::AfterCreate, destination, false)?;
    let mut hasher = Sha256::new();
    let mut encoded_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read =
            source
                .file
                .read(&mut buffer)
                .map_err(|error| ProjectStoreError::SourceRead {
                    path: source.path.clone(),
                    kind: error.kind(),
                })?;
        if read == 0 {
            break;
        }
        encoded_bytes = encoded_bytes.checked_add(read as u64).ok_or_else(|| {
            ProjectStoreError::SourceChanged {
                path: source.path.clone(),
            }
        })?;
        if encoded_bytes > MAX_ENCODED_FILE_BYTES {
            return Err(ProjectStoreError::SourceTooLarge {
                path: source.path.clone(),
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
            path: source.path.clone(),
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
    match rustix::fs::linkat(
        &audio.file,
        &temporary.leaf,
        &audio.file,
        leaf,
        rustix::fs::AtFlags::empty(),
    ) {
        Ok(()) => {
            rustix::fs::unlinkat(&audio.file, &temporary.leaf, rustix::fs::AtFlags::empty())
                .map_err(|error| {
                    atomic_error(
                        destination,
                        AtomicWritePoint::BeforeDirectorySync,
                        io::Error::from(error).kind(),
                        true,
                    )
                })?;
            temporary.disarm();
        }
        Err(rustix::io::Errno::EXIST) => {
            verify_existing_asset(audio, leaf, destination, expected)?;
            return Ok(());
        }
        Err(error) => {
            return Err(atomic_error(
                destination,
                AtomicWritePoint::BeforeRename,
                io::Error::from(error).kind(),
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
    audio.file.sync_all().map_err(|error| {
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
        io::Cursor,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use sampler_core::{PadId, PadSettings, ProjectId, ProjectPattern, SampleEditRecipe};
    use sha2::{Digest, Sha256};

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn wav_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let cursor = Cursor::new(&mut bytes);
            let mut writer = hound::WavWriter::new(
                cursor,
                hound::WavSpec {
                    channels: 1,
                    sample_rate: 48_000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            for sample in [0_i16, i16::MAX, 0, i16::MIN] {
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }
        bytes
    }

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
            let source_bytes = wav_bytes();
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
            let mut directories = vec![self.directory.clone()];
            let audio = self.directory.join("audio");
            if audio.is_dir() {
                directories.push(audio);
            }
            directories
                .into_iter()
                .flat_map(|directory| fs::read_dir(directory).unwrap())
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

    #[test]
    fn save_as_parent_sync_failure_reports_visible_directory_with_unconfirmed_durability() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("new-project-parent-sync-fault");
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        let error = fixture
            .store
            .save_with_hook(request, |point| {
                (point == AtomicWritePoint::BeforeParentDirectorySync)
                    .then_some(io::ErrorKind::Other)
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectStoreError::AtomicWrite {
                point: AtomicWritePoint::BeforeParentDirectorySync,
                visibility: AtomicWriteVisibility::NewDestinationVisibleDurabilityUnconfirmed,
                ..
            }
        ));
        assert!(target.is_dir());
        assert!(!target.join("project.toml").exists());
    }

    #[test]
    fn save_as_accepts_an_existing_empty_directory() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("existing-empty-project");
        fs::create_dir(&target).unwrap();
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        fixture.store.save(request).unwrap();
        assert!(target.join("project.toml").is_file());
    }

    #[test]
    fn committed_source_length_and_extension_must_match_before_copy() {
        let fixture = ProjectFixture::new();
        let mut wrong_length = fixture.request(1, SaveKind::Explicit);
        wrong_length.snapshot.pads[0].fingerprint.encoded_bytes += 1;
        assert!(matches!(
            fixture.store.save(wrong_length),
            Err(ProjectStoreError::SourceChanged { .. })
        ));
        let mut wrong_extension = fixture.request(1, SaveKind::Explicit);
        wrong_extension.snapshot.pads[0].fingerprint.extension = SupportedAudioExtension::Aiff;
        assert!(matches!(
            fixture.store.save(wrong_extension),
            Err(ProjectStoreError::SourceChanged { .. })
        ));
        let mut forged_digest = fixture.request(1, SaveKind::Explicit);
        forged_digest.snapshot.pads[0].fingerprint.digest = AssetDigest::from_bytes([0xa5; 32]);
        assert!(matches!(
            fixture.store.save(forged_digest),
            Err(ProjectStoreError::SourceChanged { .. })
        ));
        assert!(!fixture.directory.join("project.toml").exists());
        assert_eq!(fs::read(&fixture.source).unwrap(), fixture.source_bytes);
    }

    #[test]
    fn asset_temp_failures_clean_up_without_creating_metadata() {
        for point in [
            AtomicWritePoint::AfterCreate,
            AtomicWritePoint::BeforeFlush,
            AtomicWritePoint::BeforeFileSync,
            AtomicWritePoint::BeforeRename,
        ] {
            let fixture = ProjectFixture::new();
            let error = fixture
                .store
                .save_with_hook(fixture.request(1, SaveKind::Explicit), |candidate| {
                    (candidate == point).then_some(io::ErrorKind::Other)
                })
                .unwrap_err();
            assert!(matches!(
                error,
                ProjectStoreError::AtomicWrite {
                    visibility: AtomicWriteVisibility::PreviousDestinationPreserved,
                    ..
                }
            ));
            assert!(!fixture.directory.join("project.toml").exists());
            assert!(fixture.temp_entries().is_empty());
            assert_eq!(fs::read(&fixture.source).unwrap(), fixture.source_bytes);
        }
    }

    #[test]
    fn v1_probe_hashes_real_asset_generates_id_and_never_rewrites_metadata() {
        let fixture = ProjectFixture::new();
        let audio = fixture.directory.join("audio");
        fs::create_dir(&audio).unwrap();
        let legacy_asset = audio.join("legacy.WAV");
        fs::write(&legacy_asset, &fixture.source_bytes).unwrap();
        let legacy = r#"schema_version = 1
name = "legacy"

[[pads]]
audio_path = "audio/legacy.WAV"

[pads.pad]
bank = 0
index = 0

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0
"#;
        fs::write(fixture.directory.join("project.toml"), legacy).unwrap();
        let before = read(&fixture.directory.join("project.toml"));

        let probe = fixture.store.probe(&fixture.directory).unwrap();
        let migrated = probe.explicit.unwrap().unwrap();
        assert_ne!(migrated.project_id, ProjectId::from_bytes([0; 16]));
        assert_eq!(migrated.revision, 0);
        assert_eq!(migrated.pads.len(), 1);
        let canonical = fixture.directory.join(&migrated.pads[0].audio_path);
        assert_eq!(fs::read(&canonical).unwrap(), fixture.source_bytes);
        assert_eq!(
            SourceFingerprint::from_path(&canonical).unwrap().digest,
            migrated.pads[0].asset_digest
        );
        assert_eq!(read(&fixture.directory.join("project.toml")), before);
    }

    #[test]
    fn current_probe_rejects_missing_and_nonregular_assets() {
        for nonregular in [false, true] {
            let fixture = ProjectFixture::new();
            let receipt = fixture
                .store
                .save(fixture.request(1, SaveKind::Explicit))
                .unwrap();
            let asset = &receipt.mappings[0].project_path;
            fs::remove_file(asset).unwrap();
            if nonregular {
                fs::create_dir(asset).unwrap();
            }
            let probe = fixture.store.probe(&fixture.directory).unwrap();
            assert!(probe.explicit.unwrap().is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn current_probe_rejects_symlink_assets() {
        use std::os::unix::fs::symlink;

        let fixture = ProjectFixture::new();
        let receipt = fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        let asset = &receipt.mappings[0].project_path;
        fs::remove_file(asset).unwrap();
        symlink(&fixture.source, asset).unwrap();
        let probe = fixture.store.probe(&fixture.directory).unwrap();
        assert!(matches!(
            probe.explicit.unwrap(),
            Err(ProjectStoreError::SymlinkRejected { .. })
        ));
    }

    #[test]
    fn filesystem_asset_resolver_rejects_parent_traversal() {
        let fixture = ProjectFixture::new();
        fs::create_dir(fixture.directory.join("audio")).unwrap();
        fs::write(fixture.directory.join("outside.wav"), b"outside").unwrap();
        let project = ProjectDirectory::open_existing(&fixture.directory).unwrap();
        assert!(matches!(
            project.open_asset("audio/../outside.wav"),
            Err(ProjectStoreError::DocumentInvalid { .. })
        ));
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

    #[cfg(unix)]
    #[test]
    fn validated_source_handle_survives_path_substitution() {
        use std::{io::Read as _, os::unix::fs::symlink};

        let fixture = ProjectFixture::new();
        let mut opened = ValidatedSource::open(&fixture.source).unwrap();
        let original = fixture.root.join("original.wav");
        let replacement = fixture.root.join("replacement.wav");
        fs::write(&replacement, b"replacement bytes").unwrap();
        fs::rename(&fixture.source, &original).unwrap();
        symlink(&replacement, &fixture.source).unwrap();

        opened.rewind().unwrap();
        let mut actual = Vec::new();
        opened.file.read_to_end(&mut actual).unwrap();
        assert_eq!(actual, fixture.source_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn save_copies_the_validated_handle_after_source_path_substitution() {
        use std::{cell::Cell, os::unix::fs::symlink};

        let fixture = ProjectFixture::new();
        let original = fixture.root.join("opened-source.wav");
        let replacement = fixture.root.join("replacement.wav");
        fs::write(&replacement, b"replacement bytes").unwrap();
        let substituted = Cell::new(false);
        let receipt = fixture
            .store
            .save_with_hook(fixture.request(1, SaveKind::Explicit), |point| {
                if point == AtomicWritePoint::AfterSourceFingerprint {
                    fs::rename(&fixture.source, &original).unwrap();
                    symlink(&replacement, &fixture.source).unwrap();
                    substituted.set(true);
                }
                None
            })
            .unwrap();
        assert!(substituted.get());
        assert_eq!(
            fs::read(&receipt.mappings[0].project_path).unwrap(),
            fixture.source_bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_source_is_rejected_without_blocking() {
        use std::{
            process::Command,
            time::{Duration, Instant},
        };

        let fixture = ProjectFixture::new();
        let fifo = fixture.root.join("source.wav");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let started = Instant::now();
        assert!(matches!(
            ValidatedSource::open(&fifo),
            Err(ProjectStoreError::NonRegularFile { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn project_asset_leaf_remains_anchored_to_open_audio_directory() {
        use std::os::unix::fs::symlink;

        let fixture = ProjectFixture::new();
        fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        let project = ProjectDirectory::open_existing(&fixture.directory).unwrap();
        let audio = project.open_audio_directory().unwrap();
        let mapping = fixture.request(1, SaveKind::Explicit).snapshot.pads[0].fingerprint;
        let leaf = format!("{}.{}", mapping.digest, mapping.extension.as_str());

        let held = fixture.root.join("held-audio");
        let external = fixture.root.join("external-audio");
        fs::create_dir(&external).unwrap();
        fs::write(external.join(&leaf), b"replacement bytes").unwrap();
        fs::rename(fixture.directory.join("audio"), &held).unwrap();
        symlink(&external, fixture.directory.join("audio")).unwrap();

        let opened = audio
            .open_leaf(Path::new(&leaf), &held.join(&leaf))
            .unwrap();
        assert_eq!(opened.fingerprint, mapping);
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

    #[test]
    fn recovery_discard_requires_exact_identity_and_revision() {
        let fixture = ProjectFixture::new();
        fixture
            .store
            .save(fixture.request(3, SaveKind::Explicit))
            .unwrap();
        fixture
            .store
            .save(fixture.request(4, SaveKind::Recovery))
            .unwrap();
        let recovery = fixture.directory.join(".sampler-tui-recovery.toml");
        let explicit = fixture.directory.join("project.toml");
        let before = read(&recovery);
        let explicit_before = read(&explicit);
        assert!(matches!(
            fixture.store.discard_recovery(
                &fixture.directory,
                ProjectId::from_bytes([0x42; 16]),
                4,
            ),
            Err(ProjectStoreError::RecoveryMismatch { .. })
        ));
        assert_eq!(read(&recovery), before);
        assert_eq!(read(&explicit), explicit_before);
        assert!(matches!(
            fixture.store.discard_recovery(
                &fixture.directory,
                ProjectId::from_bytes([0x41; 16]),
                5,
            ),
            Err(ProjectStoreError::RecoveryMismatch { .. })
        ));
        assert_eq!(read(&recovery), before);
        assert_eq!(read(&explicit), explicit_before);
        fixture
            .store
            .discard_recovery(&fixture.directory, ProjectId::from_bytes([0x41; 16]), 4)
            .unwrap();
        assert!(!recovery.exists());
        assert_eq!(read(&explicit), explicit_before);
    }

    #[test]
    fn project_open_probe_isolates_two_real_projects_across_corrupt_missing_and_digest_errors() {
        let project_a = ProjectFixture::new();
        let project_b = ProjectFixture::new();
        project_a
            .store
            .save(project_a.request(1, SaveKind::Explicit))
            .unwrap();
        let saved_b = project_b
            .store
            .save(project_b.request(2, SaveKind::Explicit))
            .unwrap();
        let project_a_toml = read(&project_a.directory.join("project.toml"));

        fs::write(project_b.directory.join("project.toml"), "not = [valid").unwrap();
        assert!(
            project_b
                .store
                .probe(&project_b.directory)
                .unwrap()
                .explicit
                .unwrap()
                .is_err()
        );
        assert_eq!(
            read(&project_a.directory.join("project.toml")),
            project_a_toml
        );
        assert!(
            project_a
                .store
                .probe(&project_a.directory)
                .unwrap()
                .explicit
                .unwrap()
                .is_ok()
        );

        fs::write(
            project_b.directory.join("project.toml"),
            &saved_b.canonical_toml,
        )
        .unwrap();
        fs::remove_file(&saved_b.mappings[0].project_path).unwrap();
        assert!(
            project_b
                .store
                .probe(&project_b.directory)
                .unwrap()
                .explicit
                .unwrap()
                .is_err()
        );

        fs::write(
            &saved_b.mappings[0].project_path,
            b"different encoded audio bytes",
        )
        .unwrap();
        assert!(matches!(
            project_b
                .store
                .probe(&project_b.directory)
                .unwrap()
                .explicit
                .unwrap(),
            Err(ProjectStoreError::AssetIntegrity { .. })
        ));
        assert_eq!(
            read(&project_a.directory.join("project.toml")),
            project_a_toml
        );
    }

    #[test]
    fn concurrent_save_as_requests_cannot_overwrite_the_first_winner() {
        use std::sync::{Arc, Barrier};

        let fixture = ProjectFixture::new();
        let target = fixture.root.join("contended-save-as");
        let mut first = fixture.request(1, SaveKind::Explicit);
        first.directory = target.clone();
        first.save_as = true;
        first.snapshot.project_id = ProjectId::from_bytes([0x51; 16]);
        let mut second = first.clone();
        second.snapshot.project_id = ProjectId::from_bytes([0x52; 16]);
        let barrier = Arc::new(Barrier::new(2));

        let (first_result, second_result) = std::thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first_thread = scope.spawn(move || {
                ProjectStore.save_with_hook(first, |point| {
                    if point == AtomicWritePoint::AfterSaveAsPreflight {
                        first_barrier.wait();
                    }
                    None
                })
            });
            let second_barrier = Arc::clone(&barrier);
            let second_thread = scope.spawn(move || {
                ProjectStore.save_with_hook(second, |point| {
                    if point == AtomicWritePoint::AfterSaveAsPreflight {
                        second_barrier.wait();
                    }
                    None
                })
            });
            (first_thread.join().unwrap(), second_thread.join().unwrap())
        });

        let (winner, loser) = match (first_result, second_result) {
            (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
            outcomes => panic!("expected exactly one winner, got {outcomes:?}"),
        };
        assert!(
            matches!(&loser, ProjectStoreError::SaveAsTargetNotEmpty { .. }),
            "unexpected loser: {loser:?}"
        );
        let parsed = ProjectDocument::from_toml(&read(&target.join("project.toml"))).unwrap();
        assert_eq!(parsed.current().unwrap().project_id, winner.project_id);
    }

    #[test]
    fn discard_revision_n_cannot_delete_concurrent_recovery_n_plus_one() {
        use std::{
            sync::mpsc::{self, RecvTimeoutError},
            time::Duration,
        };

        let fixture = ProjectFixture::new();
        fixture
            .store
            .save(fixture.request(1, SaveKind::Recovery))
            .unwrap();
        let directory = fixture.directory.clone();
        let newer = fixture.request(2, SaveKind::Recovery);
        let (validated_tx, validated_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (saved_tx, saved_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let discard_directory = directory.clone();
            let discard = scope.spawn(move || {
                ProjectStore.discard_recovery_with_hook(
                    &discard_directory,
                    ProjectId::from_bytes([0x41; 16]),
                    1,
                    || {
                        validated_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    },
                )
            });
            validated_rx.recv().unwrap();
            let save = scope.spawn(move || {
                let result = ProjectStore.save(newer);
                saved_tx.send(()).unwrap();
                result
            });
            assert_eq!(
                saved_rx.recv_timeout(Duration::from_millis(100)),
                Err(RecvTimeoutError::Timeout),
                "save must wait for discard's project lock"
            );
            release_tx.send(()).unwrap();
            discard.join().unwrap().unwrap();
            save.join().unwrap().unwrap();
        });
        let probe = fixture.store.probe(&directory).unwrap();
        assert_eq!(probe.recovery.unwrap().unwrap().revision, 2);
    }

    #[cfg(unix)]
    #[test]
    fn project_lock_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let fixture = ProjectFixture::new();
        let outside = fixture.root.join("outside-lock");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, fixture.directory.join(".sampler-tui.lock")).unwrap();
        assert!(matches!(
            fixture.store.save(fixture.request(1, SaveKind::Explicit)),
            Err(ProjectStoreError::SymlinkRejected { .. })
        ));
        assert_eq!(fs::read(outside).unwrap(), b"outside");
    }
}
