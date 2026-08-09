use std::{
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use rustix::fs::{FileType as RustixFileType, FlockOperation, Mode, OFlags};
use sampler_audio::{EncodedAudioFormat, probe_shared_audio_format};
use sampler_core::{
    AssetDigest, LegacyProjectDocument, MasterMixSettings, PadId, PadMixSettings, PadSettings,
    ParsedProjectDocument, ProjectDocument, ProjectId, ProjectPattern, SampleEditRecipe,
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

pub(crate) struct ProjectAssetBytes {
    pub(crate) path: PathBuf,
    pub(crate) encoded: Arc<[u8]>,
    pub(crate) fingerprint: SourceFingerprint,
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
        let project = Self {
            path,
            file: File::from(owned),
        };
        project.revalidate_path_identity()?;
        Ok(project)
    }

    fn revalidate_path_identity(&self) -> Result<(), ProjectStoreError> {
        let opened = rustix::fs::fstat(&self.file).map_err(|error| {
            filesystem_error(
                "inspect opened project directory",
                &self.path,
                io::Error::from(error),
            )
        })?;
        let (parent, leaf) = open_anchored_parent(&self.path, false)?;
        let current_file = rustix::fs::openat(
            &parent,
            &leaf,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| ProjectStoreError::Filesystem {
            operation: "verify project directory identity",
            path: self.path.clone(),
            kind: io::Error::from(error).kind(),
        })?;
        let current = rustix::fs::fstat(&current_file).map_err(|error| {
            filesystem_error(
                "inspect current project directory",
                &self.path,
                io::Error::from(error),
            )
        })?;
        if opened.st_dev != current.st_dev || opened.st_ino != current.st_ino {
            return Err(ProjectStoreError::Filesystem {
                operation: "verify project directory identity",
                path: self.path.clone(),
                kind: io::ErrorKind::Other,
            });
        }
        Ok(())
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

impl ProjectStore {
    pub(crate) fn read_project_asset_after_open<F>(
        &self,
        directory: &Path,
        relative: &str,
        expected_digest: AssetDigest,
        after_open: F,
    ) -> Result<ProjectAssetBytes, ProjectStoreError>
    where
        F: FnOnce(),
    {
        let project = ProjectDirectory::open_existing(directory)?;
        let mut source = project.open_asset(relative)?;
        after_open();
        if source.fingerprint.digest != expected_digest {
            return Err(ProjectStoreError::AssetIntegrity { path: source.path });
        }
        source.rewind()?;
        let mut encoded = Vec::with_capacity(source.fingerprint.encoded_bytes as usize);
        source
            .file
            .take(MAX_ENCODED_FILE_BYTES + 1)
            .read_to_end(&mut encoded)
            .map_err(|error| ProjectStoreError::SourceRead {
                path: source.path.clone(),
                kind: error.kind(),
            })?;
        let fingerprint = SourceFingerprint::from_encoded_bytes_with_extension(
            &source.path,
            &encoded,
            source.fingerprint.extension,
        )?;
        if fingerprint != source.fingerprint || fingerprint.digest != expected_digest {
            return Err(ProjectStoreError::AssetIntegrity { path: source.path });
        }
        project.revalidate_path_identity()?;
        Ok(ProjectAssetBytes {
            path: source.path,
            encoded: Arc::from(encoded),
            fingerprint,
        })
    }
}

pub(crate) fn open_anchored_parent(
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

pub(crate) fn revalidate_anchored_parent(
    path: &Path,
    opened: &File,
) -> Result<(), ProjectStoreError> {
    let (current, _) = open_anchored_parent(path, false)?;
    let expected = rustix::fs::fstat(opened).map_err(|error| {
        filesystem_error(
            "inspect opened parent directory",
            path,
            io::Error::from(error),
        )
    })?;
    let actual = rustix::fs::fstat(&current).map_err(|error| {
        filesystem_error(
            "inspect current parent directory",
            path,
            io::Error::from(error),
        )
    })?;
    if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
        return Err(ProjectStoreError::Filesystem {
            operation: "verify destination parent identity",
            path: path.to_path_buf(),
            kind: io::ErrorKind::Other,
        });
    }
    Ok(())
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
    pub master_mix: MasterMixSettings,
    pub midi: sampler_core::MidiSettings,
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
    pub mix: PadMixSettings,
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
    OwnerAfterTempCreate,
    OwnerBeforePublish,
    OwnerAfterPublish,
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
    #[error("could not decode committed project asset {path}: {message}")]
    Decode { path: PathBuf, message: String },
    #[error("committed project asset staging was cancelled")]
    Cancelled,
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
    #[error("could not allocate a unique temporary name for {path} after {attempts} attempts")]
    TempNameExhausted { path: PathBuf, attempts: usize },
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
        let document_pads = request
            .snapshot
            .pads
            .iter()
            .map(|pad| {
                sampler_core::ProjectPad::new(
                    pad.pad,
                    format!(
                        "audio/{}.{}",
                        pad.fingerprint.digest,
                        pad.fingerprint.extension.as_str()
                    ),
                    pad.fingerprint.digest,
                    pad.settings,
                    pad.mix,
                    pad.recipe,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let document = ProjectDocument::new_v4(
            request.snapshot.project_id,
            request.snapshot.name.clone(),
            request.snapshot.revision,
            document_pads,
            request.snapshot.patterns.clone(),
            request.snapshot.master_mix,
            request.snapshot.midi,
        )?;
        let directory = prepare_project_directory(
            &request.directory,
            request.save_as,
            request.snapshot.project_id,
            &mut hook,
        )?;
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
            claim_or_validate_save_as_target(&project, request.snapshot.project_id, &mut hook)?;
        }
        let directory = project.path.clone();
        let audio_directory = project.ensure_audio_directory()?;
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
            mappings.push(ProjectAssetMapping {
                pad: pad.pad,
                source_generation: pad.source_generation,
                fingerprint: pad.fingerprint,
                project_path,
            });
        }

        let canonical_toml = document.to_toml()?;
        let destination = directory.join(match request.kind {
            SaveKind::Explicit => "project.toml",
            SaveKind::Recovery => ".sampler-tui-recovery.toml",
        });
        project.revalidate_path_identity()?;
        let publication = if request.save_as && request.kind == SaveKind::Explicit {
            MetadataPublication::NoReplace
        } else {
            MetadataPublication::Replace
        };
        atomic_replace(
            &destination,
            canonical_toml.as_bytes(),
            &project,
            publication,
            &mut hook,
        )?;
        project.revalidate_path_identity()?;
        if request.save_as {
            let _ = remove_save_as_owner(&project);
        }
        project.revalidate_path_identity()?;

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
            sampler_core::PadMixSettings::default(),
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
    ProjectDocument::new_v4(
        ProjectId::from_bytes(project_id),
        legacy.name(),
        legacy.revision(),
        pads,
        patterns,
        sampler_core::MasterMixSettings::default(),
        sampler_core::MidiSettings::default(),
    )
    .map_err(ProjectStoreError::from)
}

const MAX_TEMP_CREATE_ATTEMPTS: usize = 16;

pub(crate) struct AnchoredTemp {
    directory: File,
    identity: File,
    leaf: std::ffi::OsString,
    path: PathBuf,
    armed: bool,
}

impl AnchoredTemp {
    fn disarm(&mut self) {
        self.armed = false;
    }

    pub(crate) fn verify_path_identity(&self) -> Result<(), ProjectStoreError> {
        let actual = rustix::fs::statat(
            &self.directory,
            &self.leaf,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|error| ProjectStoreError::Filesystem {
            operation: "verify temporary file identity",
            path: self.path.clone(),
            kind: io::Error::from(error).kind(),
        })?;
        let expected = rustix::fs::fstat(&self.identity).map_err(|error| {
            filesystem_error(
                "inspect opened temporary file",
                &self.path,
                io::Error::from(error),
            )
        })?;
        if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
            return Err(ProjectStoreError::Filesystem {
                operation: "verify temporary file identity",
                path: self.path.clone(),
                kind: io::ErrorKind::Other,
            });
        }
        Ok(())
    }

    pub(crate) fn unlink_owned(&mut self) -> Result<(), ProjectStoreError> {
        self.verify_path_identity()?;
        rustix::fs::unlinkat(&self.directory, &self.leaf, rustix::fs::AtFlags::empty()).map_err(
            |error| filesystem_error("remove temporary file", &self.path, io::Error::from(error)),
        )?;
        self.disarm();
        Ok(())
    }

    pub(crate) fn verify_destination_identity(
        &self,
        directory: &File,
        leaf: &Path,
        destination: &Path,
        point: AtomicWritePoint,
    ) -> Result<(), ProjectStoreError> {
        let actual = rustix::fs::statat(directory, leaf, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| {
                atomic_error(destination, point, io::Error::from(error).kind(), true)
            })?;
        let expected = rustix::fs::fstat(&self.identity).map_err(|error| {
            atomic_error(destination, point, io::Error::from(error).kind(), true)
        })?;
        if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
            return Err(atomic_error(destination, point, io::ErrorKind::Other, true));
        }
        Ok(())
    }

    pub(crate) fn identity(&self) -> &File {
        &self.identity
    }

    pub(crate) fn link_noreplace(
        &self,
        destination_directory: &File,
        destination_leaf: &Path,
    ) -> Result<NoReplacePublication, rustix::io::Errno> {
        match rustix::fs::linkat(
            &self.directory,
            &self.leaf,
            destination_directory,
            destination_leaf,
            rustix::fs::AtFlags::empty(),
        ) {
            Ok(()) => Ok(NoReplacePublication::Published),
            Err(rustix::io::Errno::EXIST) => Ok(NoReplacePublication::DestinationExists),
            Err(error) => Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoReplacePublication {
    Published,
    DestinationExists,
}

impl Drop for AnchoredTemp {
    fn drop(&mut self) {
        if self.armed && self.verify_path_identity().is_ok() {
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
    project_id: ProjectId,
    hook: &mut F,
) -> Result<PathBuf, ProjectStoreError>
where
    F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
{
    if save_as {
        match fs::symlink_metadata(path) {
            Ok(_) => validate_save_as_target(path, project_id)?,
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
                    validate_save_as_target(path, project_id)?;
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

fn validate_save_as_target(path: &Path, project_id: ProjectId) -> Result<(), ProjectStoreError> {
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
    let project = ProjectDirectory::open_existing(path)?;
    inspect_save_as_target(&project, project_id).map(|_| ())
}

const SAVE_AS_OWNER: &str = ".sampler-tui-save-as-owner";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SaveAsTargetState {
    Empty,
    Owned,
}

fn inspect_save_as_target(
    project: &ProjectDirectory,
    project_id: ProjectId,
) -> Result<SaveAsTargetState, ProjectStoreError> {
    let entries = fs::read_dir(&project.path)
        .map_err(|error| filesystem_error("recheck save-as target", &project.path, error))?;
    let mut owner = None;
    let mut has_audio = false;
    for entry in entries {
        let entry = entry
            .map_err(|error| filesystem_error("recheck save-as target", &project.path, error))?;
        match entry.file_name().to_str() {
            Some(".sampler-tui.lock") => {
                let path = project.path.join(".sampler-tui.lock");
                if open_optional_regular_at(&project.file, Path::new(".sampler-tui.lock"), &path)?
                    .is_none()
                {
                    return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                        path: project.path.clone(),
                    });
                }
            }
            Some(SAVE_AS_OWNER) => {
                let path = project.path.join(SAVE_AS_OWNER);
                let Some(mut file) =
                    open_optional_regular_at(&project.file, Path::new(SAVE_AS_OWNER), &path)?
                else {
                    return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                        path: project.path.clone(),
                    });
                };
                let mut value = String::new();
                file.read_to_string(&mut value)
                    .map_err(|error| filesystem_error("read save-as owner", &path, error))?;
                owner = value.trim().parse::<ProjectId>().ok();
            }
            Some("audio") => {
                project.open_audio_directory()?;
                has_audio = true;
            }
            _ => {
                return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                    path: project.path.clone(),
                });
            }
        }
    }
    match owner {
        Some(owner) if owner == project_id => Ok(SaveAsTargetState::Owned),
        None if !has_audio => Ok(SaveAsTargetState::Empty),
        _ => Err(ProjectStoreError::SaveAsTargetNotEmpty {
            path: project.path.clone(),
        }),
    }
}

fn claim_or_validate_save_as_target<F>(
    project: &ProjectDirectory,
    project_id: ProjectId,
    hook: &mut F,
) -> Result<(), ProjectStoreError>
where
    F: FnMut(AtomicWritePoint) -> Option<io::ErrorKind>,
{
    if inspect_save_as_target(project, project_id)? == SaveAsTargetState::Owned {
        return Ok(());
    }
    let path = project.path.join(SAVE_AS_OWNER);
    let (mut file, mut temporary) = create_anchored_temp(&project.file, &project.path, &path)?;
    checkpoint(hook, AtomicWritePoint::OwnerAfterTempCreate, &path, false)?;
    file.write_all(project_id.to_string().as_bytes())
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            atomic_error(
                &path,
                AtomicWritePoint::OwnerBeforePublish,
                error.kind(),
                false,
            )
        })?;
    drop(file);
    checkpoint(hook, AtomicWritePoint::OwnerBeforePublish, &path, false)?;
    project.revalidate_path_identity()?;
    temporary.verify_path_identity()?;
    match temporary.link_noreplace(&project.file, Path::new(SAVE_AS_OWNER)) {
        Ok(NoReplacePublication::Published) => {}
        Ok(NoReplacePublication::DestinationExists) => {
            return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                path: project.path.clone(),
            });
        }
        Err(error) => {
            return Err(atomic_error(
                &path,
                AtomicWritePoint::OwnerBeforePublish,
                io::Error::from(error).kind(),
                false,
            ));
        }
    }
    let _ = temporary.unlink_owned();
    checkpoint(hook, AtomicWritePoint::OwnerAfterPublish, &path, true)?;
    temporary.verify_destination_identity(
        &project.file,
        Path::new(SAVE_AS_OWNER),
        &path,
        AtomicWritePoint::OwnerAfterPublish,
    )?;
    project
        .file
        .sync_all()
        .map_err(|error| filesystem_error("sync save-as owner", &project.path, error))
}

fn remove_save_as_owner(project: &ProjectDirectory) -> Result<(), ProjectStoreError> {
    let path = project.path.join(SAVE_AS_OWNER);
    rustix::fs::unlinkat(&project.file, SAVE_AS_OWNER, rustix::fs::AtFlags::empty())
        .map_err(|error| filesystem_error("remove save-as owner", &path, io::Error::from(error)))?;
    project
        .file
        .sync_all()
        .map_err(|error| filesystem_error("sync save-as owner removal", &project.path, error))
}

pub(crate) fn create_anchored_temp(
    directory: &File,
    directory_path: &Path,
    destination: &Path,
) -> Result<(File, AnchoredTemp), ProjectStoreError> {
    create_anchored_temp_with_entropy(directory, directory_path, destination, |bytes| {
        getrandom::fill(bytes).map_err(|error| ProjectStoreError::Entropy {
            message: error.to_string(),
        })
    })
}

fn create_anchored_temp_with_entropy<F>(
    directory: &File,
    directory_path: &Path,
    destination: &Path,
    fill_entropy: F,
) -> Result<(File, AnchoredTemp), ProjectStoreError>
where
    F: FnMut(&mut [u8; 32]) -> Result<(), ProjectStoreError>,
{
    create_anchored_temp_with_entropy_and_clone(
        directory,
        directory_path,
        destination,
        fill_entropy,
        |_, file| file.try_clone(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TempClonePoint {
    DirectoryHandle,
    WriterHandle,
}

fn create_anchored_temp_with_entropy_and_clone<F, C>(
    directory: &File,
    directory_path: &Path,
    destination: &Path,
    mut fill_entropy: F,
    mut clone_file: C,
) -> Result<(File, AnchoredTemp), ProjectStoreError>
where
    F: FnMut(&mut [u8; 32]) -> Result<(), ProjectStoreError>,
    C: FnMut(TempClonePoint, &File) -> io::Result<File>,
{
    // Clone directory authority before creating an entry. Once CREATE|EXCL succeeds, ownership is
    // immediately armed around the returned file; the only later clone is therefore drop-safe.
    let owned_directory = clone_file(TempClonePoint::DirectoryHandle, directory)
        .map_err(|error| filesystem_error("clone directory handle", directory_path, error))?;
    let base = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("asset");
    for _ in 0..MAX_TEMP_CREATE_ATTEMPTS {
        let mut entropy = [0_u8; 32];
        fill_entropy(&mut entropy)?;
        let mut nonce = String::with_capacity(64);
        for byte in entropy {
            use std::fmt::Write as _;
            write!(&mut nonce, "{byte:02x}").expect("writing to String cannot fail");
        }
        let leaf = std::ffi::OsString::from(format!(".{base}.sampler-tui-tmp-{nonce}"));
        match rustix::fs::openat(
            &owned_directory,
            &leaf,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(owned) => {
                let temporary = AnchoredTemp {
                    directory: owned_directory,
                    identity: File::from(owned),
                    path: directory_path.join(&leaf),
                    leaf,
                    armed: true,
                };
                let writer = clone_file(TempClonePoint::WriterHandle, &temporary.identity)
                    .map_err(|error| {
                        filesystem_error("clone temporary writer handle", destination, error)
                    })?;
                return Ok((writer, temporary));
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
    Err(ProjectStoreError::TempNameExhausted {
        path: destination.to_path_buf(),
        attempts: MAX_TEMP_CREATE_ATTEMPTS,
    })
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
    publication: MetadataPublication,
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
    let existing = open_optional_regular_at(&project.file, leaf, destination)?;
    if publication == MetadataPublication::NoReplace && existing.is_some() {
        return Err(ProjectStoreError::SaveAsTargetNotEmpty {
            path: project.path.clone(),
        });
    }
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
    project.revalidate_path_identity()?;
    temporary.verify_path_identity()?;
    match publication {
        MetadataPublication::Replace => {
            rustix::fs::renameat(&project.file, &temporary.leaf, &project.file, leaf).map_err(
                |error| {
                    atomic_error(
                        destination,
                        AtomicWritePoint::BeforeRename,
                        io::Error::from(error).kind(),
                        false,
                    )
                },
            )?;
            temporary.disarm();
        }
        MetadataPublication::NoReplace => match temporary.link_noreplace(&project.file, leaf) {
            Ok(NoReplacePublication::Published) => {
                let _ = temporary.unlink_owned();
            }
            Ok(NoReplacePublication::DestinationExists) => {
                return Err(ProjectStoreError::SaveAsTargetNotEmpty {
                    path: project.path.clone(),
                });
            }
            Err(error) => {
                return Err(atomic_error(
                    destination,
                    AtomicWritePoint::BeforeRename,
                    io::Error::from(error).kind(),
                    false,
                ));
            }
        },
    }
    checkpoint(
        hook,
        AtomicWritePoint::BeforeDirectorySync,
        destination,
        true,
    )?;
    temporary.verify_destination_identity(
        &project.file,
        leaf,
        destination,
        AtomicWritePoint::BeforeDirectorySync,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataPublication {
    Replace,
    NoReplace,
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
    temporary.verify_path_identity()?;
    match temporary.link_noreplace(&audio.file, leaf) {
        Ok(NoReplacePublication::Published) => {
            let _ = temporary.unlink_owned();
        }
        Ok(NoReplacePublication::DestinationExists) => {
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
    temporary.verify_destination_identity(
        &audio.file,
        leaf,
        destination,
        AtomicWritePoint::BeforeDirectorySync,
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
        io::{self, Cursor},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use sampler_core::{
        BankId, MidiChannel, MidiChannelFilter, MidiNote, MidiSettings, PadId, PadSettings,
        ProjectId, ProjectPattern, SampleEditRecipe,
    };
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
                    master_mix: sampler_core::MasterMixSettings::default(),
                    midi: sampler_core::MidiSettings::default(),
                    pads: vec![ProjectSavePad {
                        pad: PadId::first(),
                        source_path: self.source.clone(),
                        source_generation: 7,
                        fingerprint,
                        settings: PadSettings::default(),
                        mix: sampler_core::PadMixSettings::default(),
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
    fn explicit_and_recovery_saves_probe_the_exact_midi_settings() {
        let fixture = ProjectFixture::new();
        let midi = MidiSettings::default()
            .with_channel(MidiChannelFilter::Channel(MidiChannel::new(11).unwrap()))
            .learn_swap(BankId::new(0).unwrap(), 2, MidiNote::new(81).unwrap())
            .unwrap()
            .learn_swap(BankId::new(7).unwrap(), 15, MidiNote::new(12).unwrap())
            .unwrap()
            .unmap(BankId::new(3).unwrap(), 4)
            .unwrap();

        let mut explicit = fixture.request(4, SaveKind::Explicit);
        explicit.snapshot.midi = midi;
        fixture.store.save(explicit).unwrap();
        let mut recovery = fixture.request(5, SaveKind::Recovery);
        recovery.snapshot.midi = midi;
        fixture.store.save(recovery).unwrap();

        let probe = fixture.store.probe(&fixture.directory).unwrap();
        assert_eq!(probe.explicit.unwrap().unwrap().midi, midi);
        assert_eq!(probe.recovery.unwrap().unwrap().midi, midi);
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
    fn metadata_commit_rejects_replaced_temp_without_deleting_foreign_replacement() {
        use std::cell::RefCell;

        let fixture = ProjectFixture::new();
        fixture
            .store
            .save(fixture.request(1, SaveKind::Explicit))
            .unwrap();
        let before = read(&fixture.directory.join("project.toml"));
        let foreign = ProjectDocument::new_v4(
            ProjectId::from_bytes([0x7f; 16]),
            "foreign temp",
            90,
            Vec::new(),
            Vec::new(),
            sampler_core::MasterMixSettings::default(),
            sampler_core::MidiSettings::default(),
        )
        .unwrap()
        .to_toml()
        .unwrap();
        let replaced = RefCell::new(None);

        let result =
            fixture
                .store
                .save_with_hook(fixture.request(2, SaveKind::Explicit), |point| {
                    if point == AtomicWritePoint::BeforeRename {
                        let temp = fixture
                            .temp_entries()
                            .into_iter()
                            .find(|path| {
                                path.file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .starts_with(".project.toml.sampler-tui-tmp-")
                            })
                            .unwrap();
                        fs::remove_file(&temp).unwrap();
                        fs::write(&temp, &foreign).unwrap();
                        *replaced.borrow_mut() = Some(temp);
                    }
                    None
                });

        assert!(matches!(result, Err(ProjectStoreError::Filesystem { .. })));
        assert_eq!(read(&fixture.directory.join("project.toml")), before);
        let replacement = replaced.into_inner().unwrap();
        assert_eq!(fs::read_to_string(replacement).unwrap(), foreign);
    }

    #[test]
    fn temporary_leaf_uses_256_bit_lower_hex_entropy() {
        let fixture = ProjectFixture::new();
        let project = ProjectDirectory::open_existing(&fixture.directory).unwrap();
        let destination = fixture.directory.join("project.toml");
        let (_file, temporary) =
            create_anchored_temp(&project.file, &project.path, &destination).unwrap();
        let leaf = temporary.leaf.to_string_lossy();
        let suffix = leaf.strip_prefix(".project.toml.sampler-tui-tmp-").unwrap();

        assert_eq!(suffix.len(), 64);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(suffix, suffix.to_ascii_lowercase());
    }

    #[test]
    fn temporary_name_collisions_retry_to_a_distinct_entropy_value() {
        use std::cell::Cell;

        let fixture = ProjectFixture::new();
        let project = ProjectDirectory::open_existing(&fixture.directory).unwrap();
        let destination = fixture.directory.join("project.toml");
        let collision = fixture
            .directory
            .join(format!(".project.toml.sampler-tui-tmp-{}", "00".repeat(32)));
        fs::write(&collision, b"foreign collision").unwrap();
        let attempts = Cell::new(0_u8);

        let (_file, temporary) = create_anchored_temp_with_entropy(
            &project.file,
            &project.path,
            &destination,
            |entropy| {
                let attempt = attempts.get();
                attempts.set(attempt + 1);
                entropy.fill(attempt);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(attempts.get(), 2);
        assert_ne!(temporary.path, collision);
        assert_eq!(fs::read(collision).unwrap(), b"foreign collision");
    }

    #[test]
    fn temporary_name_collision_exhaustion_is_bounded_and_preserves_foreign_entry() {
        use std::cell::Cell;

        let fixture = ProjectFixture::new();
        let project = ProjectDirectory::open_existing(&fixture.directory).unwrap();
        let destination = fixture.directory.join("project.toml");
        let collision = fixture
            .directory
            .join(format!(".project.toml.sampler-tui-tmp-{}", "00".repeat(32)));
        fs::write(&collision, b"foreign collision").unwrap();
        let attempts = Cell::new(0_usize);

        let result = create_anchored_temp_with_entropy(
            &project.file,
            &project.path,
            &destination,
            |entropy| {
                attempts.set(attempts.get() + 1);
                entropy.fill(0);
                Ok(())
            },
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("collisions must exhaust the bounded retry budget"),
        };

        assert_eq!(attempts.get(), MAX_TEMP_CREATE_ATTEMPTS);
        assert!(matches!(
            error,
            ProjectStoreError::TempNameExhausted {
                attempts: MAX_TEMP_CREATE_ATTEMPTS,
                ..
            }
        ));
        assert_eq!(fs::read(collision).unwrap(), b"foreign collision");
    }

    #[test]
    fn every_temp_handle_clone_failure_preserves_the_exact_directory_entry_set() {
        for failing in [
            TempClonePoint::DirectoryHandle,
            TempClonePoint::WriterHandle,
        ] {
            let fixture = ProjectFixture::new();
            fs::write(fixture.directory.join("foreign-entry"), b"foreign").unwrap();
            let project = ProjectDirectory::open_existing(&fixture.directory).unwrap();
            let destination = fixture.directory.join("project.toml");
            let before = fs::read_dir(&fixture.directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>();

            let result = create_anchored_temp_with_entropy_and_clone(
                &project.file,
                &project.path,
                &destination,
                |entropy| {
                    entropy.fill(0x5a);
                    Ok(())
                },
                |point, file| {
                    if point == failing {
                        Err(io::Error::other("injected clone failure"))
                    } else {
                        file.try_clone()
                    }
                },
            );
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("{failing:?} must fail"),
            };
            assert_eq!(
                error,
                ProjectStoreError::Filesystem {
                    operation: match failing {
                        TempClonePoint::DirectoryHandle => "clone directory handle",
                        TempClonePoint::WriterHandle => "clone temporary writer handle",
                    },
                    path: match failing {
                        TempClonePoint::DirectoryHandle => project.path.clone(),
                        TempClonePoint::WriterHandle => destination.clone(),
                    },
                    kind: io::ErrorKind::Other,
                }
            );

            let after = fs::read_dir(&fixture.directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(after, before, "{failing:?}");
            assert_eq!(
                fs::read(fixture.directory.join("foreign-entry")).unwrap(),
                b"foreign"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn writer_clone_emfile_cleanup_does_not_require_another_descriptor() {
        const CHILD: &str = "SAMPLER_TUI_EMFILE_TEMP_CLEANUP_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "project_store::tests::writer_clone_emfile_cleanup_does_not_require_another_descriptor",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child stdout:\n{}\nchild stderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        struct LimitGuard(libc::rlimit);

        impl Drop for LimitGuard {
            fn drop(&mut self) {
                // SAFETY: The saved limit came from a successful getrlimit call in this process.
                assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) }, 0);
            }
        }

        let fixture = ProjectFixture::new();
        fs::write(fixture.directory.join("foreign-entry"), b"foreign").unwrap();
        let project = ProjectDirectory::open_existing(&fixture.directory).unwrap();
        let destination = fixture.directory.join("project.toml");
        let before = fs::read_dir(&fixture.directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();

        let mut saved = std::mem::MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: `saved` points to writable storage for one rlimit value.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, saved.as_mut_ptr()) },
            0
        );
        // SAFETY: getrlimit succeeded and initialized `saved`.
        let saved = unsafe { saved.assume_init() };
        let _guard = LimitGuard(saved);
        let lowered = libc::rlimit {
            rlim_cur: saved.rlim_cur.min(128),
            rlim_max: saved.rlim_max,
        };
        // SAFETY: The hard limit is unchanged and the soft limit does not exceed it.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);

        let observed_limit_error = std::cell::Cell::new(None);
        let exhausted = std::cell::RefCell::new(Vec::new());
        let result = create_anchored_temp_with_entropy_and_clone(
            &project.file,
            &project.path,
            &destination,
            |entropy| {
                entropy.fill(0x6b);
                Ok(())
            },
            |point, file| {
                if point == TempClonePoint::DirectoryHandle {
                    return file.try_clone();
                }
                loop {
                    match File::open("/dev/null") {
                        Ok(opened) => exhausted.borrow_mut().push(opened),
                        Err(error) => {
                            observed_limit_error.set(error.raw_os_error());
                            break;
                        }
                    }
                }
                let error = file.try_clone().unwrap_err();
                assert!(matches!(
                    error.raw_os_error(),
                    Some(code) if code == libc::EMFILE || code == libc::ENFILE
                ));
                Err(error)
            },
        );
        assert!(matches!(
            observed_limit_error.get(),
            Some(code) if code == libc::EMFILE || code == libc::ENFILE
        ));
        assert!(matches!(result, Err(ProjectStoreError::Filesystem { .. })));
        exhausted.borrow_mut().clear();

        let after = fs::read_dir(&fixture.directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(after, before);
        assert_eq!(
            fs::read(fixture.directory.join("foreign-entry")).unwrap(),
            b"foreign"
        );
    }

    #[test]
    fn asset_publication_rejects_replaced_temp_without_deleting_foreign_replacement() {
        use std::cell::RefCell;

        let fixture = ProjectFixture::new();
        let request = fixture.request(1, SaveKind::Explicit);
        let fingerprint = request.snapshot.pads[0].fingerprint;
        let final_asset = fixture.directory.join(format!(
            "audio/{}.{}",
            fingerprint.digest,
            fingerprint.extension.as_str()
        ));
        let foreign = b"foreign asset temp";
        let replaced = RefCell::new(None);

        let result = fixture.store.save_with_hook(request, |point| {
            if point == AtomicWritePoint::BeforeRename {
                let temp = fixture
                    .temp_entries()
                    .into_iter()
                    .find(|path| {
                        path.parent()
                            .is_some_and(|parent| parent.ends_with("audio"))
                    })
                    .unwrap();
                fs::remove_file(&temp).unwrap();
                fs::write(&temp, foreign).unwrap();
                *replaced.borrow_mut() = Some(temp);
            }
            None
        });

        assert!(matches!(result, Err(ProjectStoreError::Filesystem { .. })));
        assert!(!final_asset.exists());
        assert_eq!(fs::read(replaced.into_inner().unwrap()).unwrap(), foreign);
    }

    #[test]
    fn metadata_postpublication_swap_never_reports_success() {
        let fixture = ProjectFixture::new();
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.snapshot.pads.clear();
        let destination = fixture.directory.join("project.toml");
        let foreign = b"foreign postpublication metadata";

        let result = fixture.store.save_with_hook(request, |point| {
            if point == AtomicWritePoint::BeforeDirectorySync {
                fs::remove_file(&destination).unwrap();
                fs::write(&destination, foreign).unwrap();
            }
            None
        });

        assert!(matches!(
            result,
            Err(ProjectStoreError::AtomicWrite {
                visibility: AtomicWriteVisibility::NewDestinationVisibleDurabilityUnconfirmed,
                ..
            })
        ));
        assert_eq!(fs::read(destination).unwrap(), foreign);
    }

    #[test]
    fn asset_postpublication_swap_never_reports_success() {
        let fixture = ProjectFixture::new();
        let request = fixture.request(1, SaveKind::Explicit);
        let fingerprint = request.snapshot.pads[0].fingerprint;
        let destination = fixture.directory.join(format!(
            "audio/{}.{}",
            fingerprint.digest,
            fingerprint.extension.as_str()
        ));
        let foreign = b"foreign postpublication asset";

        let result = fixture.store.save_with_hook(request, |point| {
            if point == AtomicWritePoint::BeforeDirectorySync {
                fs::remove_file(&destination).unwrap();
                fs::write(&destination, foreign).unwrap();
            }
            None
        });

        assert!(matches!(
            result,
            Err(ProjectStoreError::AtomicWrite {
                visibility: AtomicWriteVisibility::NewDestinationVisibleDurabilityUnconfirmed,
                ..
            })
        ));
        assert_eq!(fs::read(destination).unwrap(), foreign);
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
        assert_eq!(migrated.midi, sampler_core::MidiSettings::default());
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
    fn save_fails_closed_when_project_path_is_replaced_after_open() {
        use std::cell::Cell;

        let fixture = ProjectFixture::new();
        let opened_directory = fixture.root.join("opened-project");
        let substituted = Cell::new(false);
        let result =
            fixture
                .store
                .save_with_hook(fixture.request(1, SaveKind::Explicit), |point| {
                    if point == AtomicWritePoint::AfterSourceFingerprint
                        && !substituted.replace(true)
                    {
                        fs::rename(&fixture.directory, &opened_directory).unwrap();
                        fs::create_dir(&fixture.directory).unwrap();
                    }
                    None
                });

        assert!(substituted.get());
        assert!(matches!(result, Err(ProjectStoreError::Filesystem { .. })));
        assert!(!fixture.directory.join("project.toml").exists());
        assert!(!fixture.directory.join("audio").exists());
        assert!(!opened_directory.join("project.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn empty_project_swap_before_metadata_commit_preserves_both_directories() {
        use std::os::unix::fs::symlink;

        let fixture = ProjectFixture::new();
        let opened_directory = fixture.root.join("opened-empty-project");
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.snapshot.pads.clear();

        let result = fixture.store.save_with_hook(request, |point| {
            if point == AtomicWritePoint::BeforeRename {
                fs::rename(&fixture.directory, &opened_directory).unwrap();
                symlink(&opened_directory, &fixture.directory).unwrap();
            }
            None
        });

        assert!(matches!(result, Err(ProjectStoreError::Filesystem { .. })));
        assert!(!opened_directory.join("project.toml").exists());
        assert!(
            fs::read_dir(&opened_directory).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("tmp"))
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
    fn failed_first_save_as_retries_with_the_same_project_identity_only() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("retry-save-as");
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        request.snapshot.project_id = ProjectId::from_bytes([0x61; 16]);

        let error = ProjectStore
            .save_with_hook(request.clone(), |point| {
                (point == AtomicWritePoint::BeforeRename).then_some(io::ErrorKind::Other)
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectStoreError::AtomicWrite {
                point: AtomicWritePoint::BeforeRename,
                visibility: AtomicWriteVisibility::PreviousDestinationPreserved,
                ..
            }
        ));

        let mut other_identity = request.clone();
        other_identity.snapshot.project_id = ProjectId::from_bytes([0x62; 16]);
        assert!(matches!(
            ProjectStore.save(other_identity),
            Err(ProjectStoreError::SaveAsTargetNotEmpty { .. })
        ));
        let receipt = ProjectStore.save(request).unwrap();
        assert_eq!(receipt.project_id, ProjectId::from_bytes([0x61; 16]));
        assert_eq!(
            ProjectDocument::from_toml(&read(&target.join("project.toml")))
                .unwrap()
                .current()
                .unwrap()
                .project_id,
            receipt.project_id
        );
    }

    #[test]
    fn first_save_as_never_replaces_foreign_metadata_published_after_owner_claim() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("foreign-save-as");
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        request.snapshot.project_id = ProjectId::from_bytes([0x71; 16]);
        let foreign = ProjectDocument::new_v4(
            ProjectId::from_bytes([0x72; 16]),
            "foreign",
            99,
            Vec::new(),
            Vec::new(),
            sampler_core::MasterMixSettings::default(),
            sampler_core::MidiSettings::default(),
        )
        .unwrap()
        .to_toml()
        .unwrap();

        let result = fixture.store.save_with_hook(request, |point| {
            if point == AtomicWritePoint::AfterSourceFingerprint {
                fs::write(target.join("project.toml"), &foreign).unwrap();
            }
            None
        });

        assert!(matches!(
            result,
            Err(ProjectStoreError::SaveAsTargetNotEmpty { .. })
        ));
        assert_eq!(
            fs::read_to_string(target.join("project.toml")).unwrap(),
            foreign
        );
    }

    #[test]
    fn durable_save_as_succeeds_when_owner_cleanup_finds_the_claim_missing() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("missing-owner-cleanup");
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        request.snapshot.pads.clear();

        let receipt = fixture.store.save_with_hook(request, |point| {
            if point == AtomicWritePoint::BeforeDirectorySync {
                fs::remove_file(target.join(SAVE_AS_OWNER)).unwrap();
            }
            None
        });

        let receipt = receipt.expect("durable metadata must define Save-As success");
        assert_eq!(receipt.directory, fs::canonicalize(&target).unwrap());
        assert!(target.join("project.toml").is_file());
        assert!(
            ProjectStore
                .probe(&target)
                .unwrap()
                .explicit
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn owner_claim_failure_before_publication_cleans_temp_and_same_id_retries() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("owner-prepublish-failure");
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        request.snapshot.pads.clear();

        let error = fixture
            .store
            .save_with_hook(request.clone(), |point| {
                (point == AtomicWritePoint::OwnerAfterTempCreate).then_some(io::ErrorKind::Other)
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectStoreError::AtomicWrite {
                point: AtomicWritePoint::OwnerAfterTempCreate,
                visibility: AtomicWriteVisibility::PreviousDestinationPreserved,
                ..
            }
        ));
        assert!(!target.join(SAVE_AS_OWNER).exists());
        assert!(
            fs::read_dir(&target).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("tmp"))
        );
        assert_eq!(fixture.store.save(request).unwrap().revision, 1);
    }

    #[test]
    fn owner_claim_failure_after_publication_leaves_valid_retryable_marker() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("owner-postpublish-failure");
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        request.snapshot.pads.clear();
        let expected_owner = request.snapshot.project_id.to_string();

        let error = fixture
            .store
            .save_with_hook(request.clone(), |point| {
                (point == AtomicWritePoint::OwnerAfterPublish).then_some(io::ErrorKind::Other)
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ProjectStoreError::AtomicWrite {
                point: AtomicWritePoint::OwnerAfterPublish,
                visibility: AtomicWriteVisibility::NewDestinationVisibleDurabilityUnconfirmed,
                ..
            }
        ));
        assert_eq!(
            fs::read_to_string(target.join(SAVE_AS_OWNER)).unwrap(),
            expected_owner
        );
        assert_eq!(fixture.store.save(request).unwrap().revision, 1);
    }

    #[test]
    fn corrupt_preexisting_owner_is_rejected_without_deleting_attacker_bytes() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("corrupt-owner");
        fs::create_dir(&target).unwrap();
        let attacker = b"partial attacker marker";
        fs::write(target.join(SAVE_AS_OWNER), attacker).unwrap();
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;

        assert!(matches!(
            fixture.store.save(request),
            Err(ProjectStoreError::SaveAsTargetNotEmpty { .. })
        ));
        assert_eq!(fs::read(target.join(SAVE_AS_OWNER)).unwrap(), attacker);
    }

    #[cfg(unix)]
    #[test]
    fn owner_publication_rejects_symlinked_temp_without_deleting_attacker_link() {
        use std::{cell::RefCell, os::unix::fs::symlink};

        let fixture = ProjectFixture::new();
        let target = fixture.root.join("owner-temp-symlink");
        let outside = fixture.root.join("outside-owner-temp");
        fs::write(&outside, b"attacker bytes").unwrap();
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        request.snapshot.pads.clear();
        let replaced = RefCell::new(None);

        let result = fixture.store.save_with_hook(request, |point| {
            if point == AtomicWritePoint::OwnerBeforePublish {
                let temp = fs::read_dir(&target)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .contains("save-as-owner.sampler-tui-tmp")
                    })
                    .unwrap();
                fs::remove_file(&temp).unwrap();
                symlink(&outside, &temp).unwrap();
                *replaced.borrow_mut() = Some(temp);
            }
            None
        });

        assert!(matches!(result, Err(ProjectStoreError::Filesystem { .. })));
        assert!(!target.join(SAVE_AS_OWNER).exists());
        let replacement = replaced.into_inner().unwrap();
        assert!(
            fs::symlink_metadata(replacement)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(outside).unwrap(), b"attacker bytes");
    }

    #[test]
    fn owner_postpublication_swap_never_reports_success() {
        let fixture = ProjectFixture::new();
        let target = fixture.root.join("owner-postpublication-swap");
        let mut request = fixture.request(1, SaveKind::Explicit);
        request.directory = target.clone();
        request.save_as = true;
        request.snapshot.pads.clear();
        let destination = target.join(SAVE_AS_OWNER);
        let foreign = b"foreign postpublication owner";

        let result = fixture.store.save_with_hook(request, |point| {
            if point == AtomicWritePoint::OwnerAfterPublish {
                fs::remove_file(&destination).unwrap();
                fs::write(&destination, foreign).unwrap();
            }
            None
        });

        assert!(matches!(
            result,
            Err(ProjectStoreError::AtomicWrite {
                visibility: AtomicWriteVisibility::NewDestinationVisibleDurabilityUnconfirmed,
                ..
            })
        ));
        assert_eq!(fs::read(destination).unwrap(), foreign);
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
