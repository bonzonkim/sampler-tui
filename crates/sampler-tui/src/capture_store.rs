use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rustix::fs::{Mode, OFlags};
use sampler_audio::SampleBuffer;
use sampler_core::SampleEditRecipe;

use crate::loader::{LoadedSample, MAX_DECODED_BYTES, MAX_PREPARED_FRAMES, build_preview};
use crate::project_store::{SourceFingerprint, SupportedAudioExtension};

const MAX_NAME_ATTEMPTS: usize = 16;
const CAPTURE_DIRECTORY_PREFIX: &str = "sampler-tui-capture-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureWritePoint {
    BeforeRootOpen,
    BeforeTempDirectoryClone,
    BeforeTempIdentityClone,
    BeforePublicationDirectoryClone,
    BeforePublicationIdentityClone,
    BeforePublishedVerification,
    AfterPublish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManagedCaptureId(u64);

impl ManagedCaptureId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManagedCapture {
    pub id: ManagedCaptureId,
    pub path: PathBuf,
    pub fingerprint: SourceFingerprint,
    pub sample: LoadedSample,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureStoreError {
    Entropy(String),
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        kind: io::ErrorKind,
    },
    Encode(String),
    Sample(String),
    Fingerprint(String),
    FrameLimitExceeded {
        frames: usize,
        max_frames: usize,
    },
    ByteLimitExceeded {
        bytes: usize,
        max_bytes: usize,
    },
    NameExhausted {
        path: PathBuf,
        attempts: usize,
    },
    IdExhausted,
    NotLive {
        id: ManagedCaptureId,
    },
    IdentityMismatch {
        id: ManagedCaptureId,
        path: PathBuf,
    },
}

impl fmt::Display for CaptureStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entropy(message) => write!(
                formatter,
                "could not obtain capture name entropy: {message}"
            ),
            Self::Filesystem {
                operation,
                path,
                kind,
            } => write!(formatter, "{operation} {}: {kind}", path.display()),
            Self::Encode(message) => write!(formatter, "could not encode captured WAV: {message}"),
            Self::Sample(message) => write!(formatter, "invalid captured samples: {message}"),
            Self::Fingerprint(message) => {
                write!(formatter, "could not fingerprint captured WAV: {message}")
            }
            Self::FrameLimitExceeded { frames, max_frames } => write!(
                formatter,
                "captured audio has {frames} prepared frames, exceeding the {max_frames}-frame limit"
            ),
            Self::ByteLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "captured audio has {bytes} decoded bytes, exceeding the {max_bytes}-byte limit"
            ),
            Self::NameExhausted { path, attempts } => write!(
                formatter,
                "could not allocate a unique capture name below {} after {attempts} attempts",
                path.display()
            ),
            Self::IdExhausted => formatter.write_str("managed capture identifiers are exhausted"),
            Self::NotLive { id } => write!(formatter, "managed capture {} is not live", id.get()),
            Self::IdentityMismatch { id, path } => write!(
                formatter,
                "managed capture {} no longer identifies {}",
                id.get(),
                path.display()
            ),
        }
    }
}

impl std::error::Error for CaptureStoreError {}

struct LiveCapture {
    leaf: std::ffi::OsString,
    path: PathBuf,
    identity: File,
}

pub(crate) struct CaptureStore {
    parent: File,
    root: File,
    root_leaf: std::ffi::OsString,
    root_path: PathBuf,
    next_id: u64,
    live: BTreeMap<ManagedCaptureId, LiveCapture>,
}

impl fmt::Debug for CaptureStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureStore")
            .field("root_path", &self.root_path)
            .field("next_id", &self.next_id)
            .field("live_count", &self.live.len())
            .finish()
    }
}

impl CaptureStore {
    pub(crate) fn new() -> Result<Self, CaptureStoreError> {
        Self::new_with_controls(os_entropy, |_, _| Ok(()))
    }

    #[cfg(test)]
    fn new_with_entropy<F>(mut fill_entropy: F) -> Result<Self, CaptureStoreError>
    where
        F: FnMut(&mut [u8; 32]) -> Result<(), CaptureStoreError>,
    {
        Self::new_with_controls(&mut fill_entropy, |_, _| Ok(()))
    }

    fn new_with_controls<F, H>(mut fill_entropy: F, mut hook: H) -> Result<Self, CaptureStoreError>
    where
        F: FnMut(&mut [u8; 32]) -> Result<(), CaptureStoreError>,
        H: FnMut(CaptureWritePoint, &Path) -> Result<(), CaptureStoreError>,
    {
        let parent_path = std::env::temp_dir();
        let parent_owned = rustix::fs::open(
            &parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| filesystem_error("open OS temporary directory", &parent_path, error))?;
        let parent = File::from(parent_owned);
        for _ in 0..MAX_NAME_ATTEMPTS {
            let leaf = random_leaf(CAPTURE_DIRECTORY_PREFIX, "", &mut fill_entropy)?;
            let root_path = parent_path.join(&leaf);
            let rollback_leaf = leaf.clone();
            let rollback_parent = parent.try_clone().map_err(|error| {
                io_error("clone OS temporary directory handle", &parent_path, error)
            })?;
            match rustix::fs::mkdirat(&parent, &leaf, Mode::from_raw_mode(0o700)) {
                Ok(()) => {
                    let identity = path_identity(&parent, Path::new(&leaf)).map_err(|error| {
                        io_error("inspect created capture directory", &root_path, error)
                    })?;
                    let mut creation = CreatedDirectory {
                        parent: rollback_parent,
                        leaf: rollback_leaf,
                        identity,
                        armed: true,
                    };
                    hook(CaptureWritePoint::BeforeRootOpen, &root_path)?;
                    let root_owned = rustix::fs::openat(
                        &parent,
                        &leaf,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|error| {
                        filesystem_error("open capture directory", &root_path, error)
                    })?;
                    let root = File::from(root_owned);
                    if file_identity(&root).map_err(|error| {
                        io_error("inspect opened capture directory", &root_path, error)
                    })? != identity
                    {
                        return Err(CaptureStoreError::Filesystem {
                            operation: "verify opened capture directory identity",
                            path: root_path,
                            kind: io::ErrorKind::Other,
                        });
                    }
                    creation.armed = false;
                    return Ok(Self {
                        parent,
                        root,
                        root_leaf: leaf,
                        root_path,
                        next_id: 1,
                        live: BTreeMap::new(),
                    });
                }
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => {
                    return Err(filesystem_error(
                        "create private capture directory",
                        &parent_path.join(&leaf),
                        error,
                    ));
                }
            }
        }
        Err(CaptureStoreError::NameExhausted {
            path: parent_path,
            attempts: MAX_NAME_ATTEMPTS,
        })
    }

    #[cfg(test)]
    pub(crate) fn root_path(&self) -> &Path {
        &self.root_path
    }

    #[cfg(test)]
    pub(crate) fn finalize(
        &mut self,
        stereo: Arc<[f32]>,
        sample_rate: u32,
    ) -> Result<ManagedCapture, CaptureStoreError> {
        let sample = SampleBuffer::new(sample_rate, stereo.as_ref().to_vec())
            .map_err(|error| CaptureStoreError::Sample(error.to_string()))?;
        self.finalize_sample(sample)
    }

    #[cfg(test)]
    fn finalize_with_hook<F>(
        &mut self,
        stereo: Arc<[f32]>,
        sample_rate: u32,
        hook: F,
    ) -> Result<ManagedCapture, CaptureStoreError>
    where
        F: FnMut(CaptureWritePoint) -> Result<(), CaptureStoreError>,
    {
        let mut hook = hook;
        self.finalize_with_controls(stereo, sample_rate, os_entropy, |point, _| hook(point))
    }

    #[cfg(test)]
    fn finalize_with_controls<F, H>(
        &mut self,
        stereo: Arc<[f32]>,
        sample_rate: u32,
        fill_entropy: F,
        hook: H,
    ) -> Result<ManagedCapture, CaptureStoreError>
    where
        F: FnMut(&mut [u8; 32]) -> Result<(), CaptureStoreError>,
        H: FnMut(CaptureWritePoint, &Path) -> Result<(), CaptureStoreError>,
    {
        let sample = SampleBuffer::new(sample_rate, stereo.as_ref().to_vec())
            .map_err(|error| CaptureStoreError::Sample(error.to_string()))?;
        self.finalize_sample_with_controls(sample, fill_entropy, hook)
    }

    pub(crate) fn finalize_sample(
        &mut self,
        sample: SampleBuffer,
    ) -> Result<ManagedCapture, CaptureStoreError> {
        self.finalize_sample_with_controls(sample, os_entropy, |_, _| Ok(()))
    }

    fn finalize_sample_with_controls<F, H>(
        &mut self,
        sample: SampleBuffer,
        mut fill_entropy: F,
        mut hook: H,
    ) -> Result<ManagedCapture, CaptureStoreError>
    where
        F: FnMut(&mut [u8; 32]) -> Result<(), CaptureStoreError>,
        H: FnMut(CaptureWritePoint, &Path) -> Result<(), CaptureStoreError>,
    {
        let frames = sample.frames();
        if frames > MAX_PREPARED_FRAMES {
            return Err(CaptureStoreError::FrameLimitExceeded {
                frames,
                max_frames: MAX_PREPARED_FRAMES,
            });
        }
        let decoded_bytes = sample
            .data()
            .len()
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or(CaptureStoreError::ByteLimitExceeded {
                bytes: usize::MAX,
                max_bytes: MAX_DECODED_BYTES,
            })?;
        if decoded_bytes > MAX_DECODED_BYTES {
            return Err(CaptureStoreError::ByteLimitExceeded {
                bytes: decoded_bytes,
                max_bytes: MAX_DECODED_BYTES,
            });
        }
        let id = ManagedCaptureId::new(self.next_id);
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or(CaptureStoreError::IdExhausted)?;
        let leaf = std::ffi::OsString::from(format!("capture-{:020}.wav", id.get()));
        let path = self.root_path.join(&leaf);
        let (file, mut temporary) =
            self.create_temp_with_controls(&path, &mut fill_entropy, &mut hook)?;
        let sync_handle = file
            .try_clone()
            .map_err(|error| io_error("clone capture temporary file", &path, error))?;
        encode_float_wav(file, &sample)?;
        sync_handle
            .sync_all()
            .map_err(|error| io_error("sync captured WAV", &path, error))?;
        temporary.verify_path_identity()?;
        hook(CaptureWritePoint::BeforePublicationDirectoryClone, &path)?;
        let publication_directory = self
            .root
            .try_clone()
            .map_err(|error| io_error("clone capture directory handle", &self.root_path, error))?;
        hook(CaptureWritePoint::BeforePublicationIdentityClone, &path)?;
        let publication_identity = temporary
            .identity
            .try_clone()
            .map_err(|error| io_error("retain published capture identity", &path, error))?;
        let mut publication = PublishedCapture::new_provisional(
            publication_directory,
            publication_identity,
            leaf.clone(),
            path.clone(),
        );
        rustix::fs::linkat(
            &self.root,
            &temporary.leaf,
            &self.root,
            &leaf,
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|error| {
            filesystem_error("publish captured WAV without replacement", &path, error)
        })?;
        publication.arm();
        hook(CaptureWritePoint::BeforePublishedVerification, &path)?;
        temporary.verify_published_identity(Path::new(&leaf), &path)?;
        hook(CaptureWritePoint::AfterPublish, &path)?;
        temporary.unlink_owned()?;
        self.root
            .sync_all()
            .map_err(|error| io_error("sync capture directory", &self.root_path, error))?;

        let encoded = read_exact_published_bytes(&mut temporary.identity, &path)?;
        let fingerprint = SourceFingerprint::from_encoded_bytes_with_extension(
            &path,
            &encoded,
            SupportedAudioExtension::Wav,
        )
        .map_err(|error| CaptureStoreError::Fingerprint(error.to_string()))?;
        let base = Arc::new(sample);
        let base_preview = build_preview(&base);
        let loaded = LoadedSample {
            fingerprint,
            source_rate: base.sample_rate(),
            source_frames: base.frames(),
            duration: Duration::from_secs_f64(base.frames() as f64 / base.sample_rate() as f64),
            rendered: Arc::clone(&base),
            rendered_preview: Arc::clone(&base_preview),
            base,
            base_preview,
            recipe: SampleEditRecipe::identity(),
        };
        let live = publication.promote_to_live()?;
        self.live.insert(id, live);
        self.next_id = next_id;
        Ok(ManagedCapture {
            id,
            path,
            fingerprint,
            sample: loaded,
        })
    }

    pub(crate) fn release(&mut self, id: ManagedCaptureId) -> Result<(), CaptureStoreError> {
        let entry = self
            .live
            .get(&id)
            .ok_or(CaptureStoreError::NotLive { id })?;
        if !path_matches_identity(&self.root, Path::new(&entry.leaf), &entry.identity) {
            return Err(CaptureStoreError::IdentityMismatch {
                id,
                path: entry.path.clone(),
            });
        }
        rustix::fs::unlinkat(&self.root, &entry.leaf, rustix::fs::AtFlags::empty())
            .map_err(|error| filesystem_error("release managed capture", &entry.path, error))?;
        self.live.remove(&id);
        self.root
            .sync_all()
            .map_err(|error| io_error("sync managed capture release", &self.root_path, error))
    }

    fn create_temp_with_controls<F, H>(
        &self,
        destination: &Path,
        fill_entropy: &mut F,
        hook: &mut H,
    ) -> Result<(File, CaptureTemp), CaptureStoreError>
    where
        F: FnMut(&mut [u8; 32]) -> Result<(), CaptureStoreError>,
        H: FnMut(CaptureWritePoint, &Path) -> Result<(), CaptureStoreError>,
    {
        let base = destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("capture.wav");
        for _ in 0..MAX_NAME_ATTEMPTS {
            let leaf = random_leaf(&format!(".{base}.sampler-tui-tmp-"), "", &mut *fill_entropy)?;
            let temporary_path = destination.to_path_buf();
            hook(CaptureWritePoint::BeforeTempDirectoryClone, destination)?;
            let directory = self.root.try_clone().map_err(|error| {
                io_error("clone capture directory handle", &self.root_path, error)
            })?;
            match rustix::fs::openat(
                &self.root,
                &leaf,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(owned) => {
                    let temporary = CaptureTemp {
                        directory,
                        identity: File::from(owned),
                        leaf,
                        path: temporary_path,
                        armed: true,
                    };
                    hook(CaptureWritePoint::BeforeTempIdentityClone, destination)?;
                    let file = temporary.identity.try_clone().map_err(|error| {
                        io_error("retain capture temporary identity", destination, error)
                    })?;
                    return Ok((file, temporary));
                }
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => {
                    return Err(filesystem_error(
                        "create anchored capture temporary file",
                        destination,
                        error,
                    ));
                }
            }
        }
        Err(CaptureStoreError::NameExhausted {
            path: destination.to_path_buf(),
            attempts: MAX_NAME_ATTEMPTS,
        })
    }
}

struct PublishedCapture {
    directory: File,
    identity: File,
    leaf: std::ffi::OsString,
    path: PathBuf,
    armed: bool,
}

impl PublishedCapture {
    fn new_provisional(
        directory: File,
        identity: File,
        leaf: std::ffi::OsString,
        path: PathBuf,
    ) -> Self {
        Self {
            directory,
            identity,
            leaf,
            path,
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn promote_to_live(&mut self) -> Result<LiveCapture, CaptureStoreError> {
        let identity = self
            .identity
            .try_clone()
            .map_err(|error| io_error("retain managed capture identity", &self.path, error))?;
        let leaf = self.leaf.clone();
        let path = self.path.clone();
        self.armed = false;
        Ok(LiveCapture {
            leaf,
            path,
            identity,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

struct CreatedDirectory {
    parent: File,
    leaf: std::ffi::OsString,
    identity: FileIdentity,
    armed: bool,
}

impl Drop for CreatedDirectory {
    fn drop(&mut self) {
        if self.armed && path_has_identity(&self.parent, Path::new(&self.leaf), self.identity) {
            let _ = rustix::fs::unlinkat(&self.parent, &self.leaf, rustix::fs::AtFlags::REMOVEDIR);
            let _ = self.parent.sync_all();
        }
    }
}

impl Drop for PublishedCapture {
    fn drop(&mut self) {
        if self.armed
            && path_matches_identity(&self.directory, Path::new(&self.leaf), &self.identity)
        {
            let _ = rustix::fs::unlinkat(&self.directory, &self.leaf, rustix::fs::AtFlags::empty());
            let _ = self.directory.sync_all();
        }
    }
}

impl Drop for CaptureStore {
    fn drop(&mut self) {
        let ids = self.live.keys().copied().collect::<Vec<_>>();
        for id in ids {
            let _ = self.release(id);
        }
        if self.live.is_empty()
            && path_matches_identity(&self.parent, Path::new(&self.root_leaf), &self.root)
        {
            let _ = rustix::fs::unlinkat(
                &self.parent,
                &self.root_leaf,
                rustix::fs::AtFlags::REMOVEDIR,
            );
            let _ = self.parent.sync_all();
        }
    }
}

struct CaptureTemp {
    directory: File,
    identity: File,
    leaf: std::ffi::OsString,
    path: PathBuf,
    armed: bool,
}

impl CaptureTemp {
    fn verify_path_identity(&self) -> Result<(), CaptureStoreError> {
        if path_matches_identity(&self.directory, Path::new(&self.leaf), &self.identity) {
            Ok(())
        } else {
            Err(CaptureStoreError::Filesystem {
                operation: "verify capture temporary identity",
                path: self.path.clone(),
                kind: io::ErrorKind::Other,
            })
        }
    }

    fn verify_published_identity(&self, leaf: &Path, path: &Path) -> Result<(), CaptureStoreError> {
        if path_matches_identity(&self.directory, leaf, &self.identity) {
            Ok(())
        } else {
            Err(CaptureStoreError::Filesystem {
                operation: "verify published capture identity",
                path: path.to_path_buf(),
                kind: io::ErrorKind::Other,
            })
        }
    }

    fn unlink_owned(&mut self) -> Result<(), CaptureStoreError> {
        self.verify_path_identity()?;
        rustix::fs::unlinkat(&self.directory, &self.leaf, rustix::fs::AtFlags::empty()).map_err(
            |error| filesystem_error("remove capture temporary file", &self.path, error),
        )?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for CaptureTemp {
    fn drop(&mut self) {
        if self.armed && self.verify_path_identity().is_ok() {
            let _ = rustix::fs::unlinkat(&self.directory, &self.leaf, rustix::fs::AtFlags::empty());
        }
    }
}

fn random_leaf<F>(
    prefix: &str,
    suffix: &str,
    mut fill_entropy: F,
) -> Result<std::ffi::OsString, CaptureStoreError>
where
    F: FnMut(&mut [u8; 32]) -> Result<(), CaptureStoreError>,
{
    let mut entropy = [0_u8; 32];
    fill_entropy(&mut entropy)?;
    let mut nonce = String::with_capacity(64);
    for byte in entropy {
        use std::fmt::Write as _;
        write!(&mut nonce, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(std::ffi::OsString::from(format!("{prefix}{nonce}{suffix}")))
}

fn encode_float_wav(file: File, sample: &SampleBuffer) -> Result<(), CaptureStoreError> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sample.sample_rate(),
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::new(file, spec)
        .map_err(|error| CaptureStoreError::Encode(error.to_string()))?;
    for value in sample.data() {
        writer
            .write_sample(*value)
            .map_err(|error| CaptureStoreError::Encode(error.to_string()))?;
    }
    writer
        .flush()
        .and_then(|()| writer.finalize())
        .map_err(|error| CaptureStoreError::Encode(error.to_string()))
}

fn read_exact_published_bytes(file: &mut File, path: &Path) -> Result<Vec<u8>, CaptureStoreError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error("seek published captured WAV", path, error))?;
    let mut encoded = Vec::new();
    file.read_to_end(&mut encoded)
        .map_err(|error| io_error("read published captured WAV", path, error))?;
    Ok(encoded)
}

fn path_matches_identity(directory: &File, leaf: &Path, expected: &File) -> bool {
    let Ok(current) = rustix::fs::openat(
        directory,
        leaf,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) else {
        return false;
    };
    let (Ok(expected), Ok(actual)) = (rustix::fs::fstat(expected), rustix::fs::fstat(&current))
    else {
        return false;
    };
    expected.st_dev == actual.st_dev && expected.st_ino == actual.st_ino
}

fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let stat = rustix::fs::fstat(file).map_err(io::Error::from)?;
    Ok(FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}

fn path_identity(directory: &File, leaf: &Path) -> io::Result<FileIdentity> {
    let stat = rustix::fs::statat(directory, leaf, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)?;
    Ok(FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}

fn path_has_identity(directory: &File, leaf: &Path, expected: FileIdentity) -> bool {
    path_identity(directory, leaf).is_ok_and(|actual| actual == expected)
}

fn os_entropy(bytes: &mut [u8; 32]) -> Result<(), CaptureStoreError> {
    getrandom::fill(bytes).map_err(|error| CaptureStoreError::Entropy(error.to_string()))
}

fn filesystem_error(
    operation: &'static str,
    path: &Path,
    error: rustix::io::Errno,
) -> CaptureStoreError {
    io_error(operation, path, io::Error::from(error))
}

fn io_error(operation: &'static str, path: &Path, error: io::Error) -> CaptureStoreError {
    CaptureStoreError::Filesystem {
        operation,
        path: path.to_path_buf(),
        kind: error.kind(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;

    use sampler_core::SampleEditRecipe;
    use sha2::{Digest, Sha256};

    use super::{CaptureStore, CaptureStoreError, CaptureWritePoint, ManagedCaptureId};
    use crate::{MAX_PREPARED_FRAMES, SupportedAudioExtension};

    #[test]
    fn runtime_directory_is_private_and_removed_only_when_store_drops() {
        let path = {
            let store = CaptureStore::new().unwrap();
            let path = store.root_path().to_owned();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
            path
        };

        assert!(!path.exists());
    }

    #[test]
    fn finalization_publishes_deterministic_float_wav_and_identity_loaded_sample() {
        let mut first_store = CaptureStore::new().unwrap();
        let first = first_store
            .finalize(Arc::from([0.0_f32, -0.0, 0.25, -0.25]), 48_000)
            .unwrap();
        let first_bytes = fs::read(&first.path).unwrap();
        let mut second_store = CaptureStore::new().unwrap();
        let second = second_store
            .finalize(Arc::from([0.0_f32, -0.0, 0.25, -0.25]), 48_000)
            .unwrap();
        let second_bytes = fs::read(&second.path).unwrap();

        assert_eq!(first_bytes, second_bytes);
        assert_eq!(first.path.extension().unwrap(), "wav");
        assert_eq!(first.fingerprint.extension, SupportedAudioExtension::Wav);
        assert_eq!(first.fingerprint.encoded_bytes, first_bytes.len() as u64);
        assert_eq!(
            first.fingerprint.digest.as_bytes(),
            Sha256::digest(&first_bytes).as_slice()
        );
        assert_eq!(first.sample.recipe, SampleEditRecipe::identity());
        assert!(Arc::ptr_eq(&first.sample.base, &first.sample.rendered));
        assert!(Arc::ptr_eq(
            &first.sample.base_preview,
            &first.sample.rendered_preview
        ));
        assert_eq!(first.sample.base.data(), [0.0, -0.0, 0.25, -0.25]);
        assert_eq!(first.sample.source_rate, 48_000);
        assert_eq!(first.sample.source_frames, 2);

        let decoded = sampler_audio::decode_path(&first.path).unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, vec![vec![0.0, 0.25], vec![-0.0, -0.25]]);
        let reader = hound::WavReader::open(&first.path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().bits_per_sample, 32);
        assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
    }

    #[test]
    fn post_publication_failure_removes_only_the_owned_file_and_allows_retry() {
        let mut store = CaptureStore::new().unwrap();

        let error = store
            .finalize_with_hook(Arc::from([0.25_f32, -0.25]), 48_000, |point| {
                if point == CaptureWritePoint::AfterPublish {
                    Err(CaptureStoreError::Filesystem {
                        operation: "injected post-publication failure",
                        path: PathBuf::from("injected"),
                        kind: std::io::ErrorKind::Other,
                    })
                } else {
                    Ok(())
                }
            })
            .unwrap_err();

        assert!(matches!(error, CaptureStoreError::Filesystem { .. }));
        assert!(fs::read_dir(store.root_path()).unwrap().next().is_none());
        let retry = store
            .finalize(Arc::from([0.25_f32, -0.25]), 48_000)
            .unwrap();
        assert_eq!(retry.id, ManagedCaptureId::new(1));
    }

    #[test]
    fn runtime_leaf_uses_256_bit_lowercase_hex_entropy() {
        let store = CaptureStore::new().unwrap();
        let leaf = store.root_path().file_name().unwrap().to_string_lossy();
        let nonce = leaf.strip_prefix("sampler-tui-capture-").unwrap();

        assert_eq!(nonce.len(), 64);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(nonce, nonce.to_ascii_lowercase());
    }

    #[test]
    fn runtime_name_collisions_retry_and_exhaust_after_sixteen_attempts() {
        let attempts = Cell::new(0_u8);
        let first = CaptureStore::new_with_entropy(|bytes| {
            bytes.fill(0x31);
            Ok(())
        })
        .unwrap();
        let second = CaptureStore::new_with_entropy(|bytes| {
            let attempt = attempts.get();
            attempts.set(attempt + 1);
            bytes.fill(if attempt == 0 { 0x31 } else { 0x32 });
            Ok(())
        })
        .unwrap();
        assert_eq!(attempts.get(), 2);
        drop(second);

        let exhausted_attempts = Cell::new(0_usize);
        let error = CaptureStore::new_with_entropy(|bytes| {
            exhausted_attempts.set(exhausted_attempts.get() + 1);
            bytes.fill(0x31);
            Ok(())
        })
        .unwrap_err();
        assert_eq!(exhausted_attempts.get(), 16);
        assert!(matches!(
            error,
            CaptureStoreError::NameExhausted { attempts: 16, .. }
        ));
        drop(first);
    }

    #[test]
    fn root_open_failure_rolls_back_the_exact_created_directory_and_retry_succeeds() {
        let entropy = [0x57_u8; 32];
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-capture-{}",
            entropy
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));

        let error = CaptureStore::new_with_controls(
            |bytes| {
                *bytes = entropy;
                Ok(())
            },
            |point, _| injected_failure(point, CaptureWritePoint::BeforeRootOpen),
        )
        .unwrap_err();

        assert!(matches!(error, CaptureStoreError::Filesystem { .. }));
        assert!(!root.exists());
        let retry = CaptureStore::new_with_entropy(|bytes| {
            *bytes = entropy;
            Ok(())
        })
        .unwrap();
        assert_eq!(retry.root_path(), root);
    }

    #[test]
    fn root_open_failure_preserves_a_foreign_replacement() {
        let entropy = [0x58_u8; 32];
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-capture-{}",
            entropy
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));

        let error = CaptureStore::new_with_controls(
            |bytes| {
                *bytes = entropy;
                Ok(())
            },
            |point, path| {
                if point == CaptureWritePoint::BeforeRootOpen {
                    fs::remove_dir(path).unwrap();
                    fs::create_dir(path).unwrap();
                    fs::write(path.join("foreign"), b"foreign root").unwrap();
                    return Err(injected_error(point));
                }
                Ok(())
            },
        )
        .unwrap_err();

        assert!(matches!(error, CaptureStoreError::Filesystem { .. }));
        assert_eq!(fs::read(root.join("foreign")).unwrap(), b"foreign root");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn temp_identity_clone_failure_removes_owned_temp_and_retry_reuses_id() {
        assert_finalize_boundary_cleanup(CaptureWritePoint::BeforeTempIdentityClone);
    }

    #[test]
    fn temp_directory_clone_failure_leaves_no_temp_and_retry_reuses_id() {
        assert_finalize_boundary_cleanup(CaptureWritePoint::BeforeTempDirectoryClone);
    }

    #[test]
    fn publication_directory_clone_failure_leaves_no_final_and_retry_reuses_id() {
        assert_finalize_boundary_cleanup(CaptureWritePoint::BeforePublicationDirectoryClone);
    }

    #[test]
    fn publication_identity_clone_failure_leaves_no_final_and_retry_reuses_id() {
        assert_finalize_boundary_cleanup(CaptureWritePoint::BeforePublicationIdentityClone);
    }

    #[test]
    fn post_link_verification_failure_removes_owned_final_and_retry_reuses_id() {
        assert_finalize_boundary_cleanup(CaptureWritePoint::BeforePublishedVerification);
    }

    #[test]
    fn post_link_verification_failure_preserves_a_foreign_final() {
        let mut store = CaptureStore::new().unwrap();
        let final_path = store.root_path().join("capture-00000000000000000001.wav");

        let error = store
            .finalize_with_controls(
                Arc::from([0.25_f32, -0.25]),
                48_000,
                |bytes| {
                    bytes.fill(0x61);
                    Ok(())
                },
                |point, path| {
                    if point == CaptureWritePoint::BeforePublishedVerification {
                        fs::remove_file(path).unwrap();
                        fs::write(path, b"foreign final").unwrap();
                        return Err(injected_error(point));
                    }
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(matches!(error, CaptureStoreError::Filesystem { .. }));
        assert_eq!(fs::read(&final_path).unwrap(), b"foreign final");
        assert_eq!(fs::read_dir(store.root_path()).unwrap().count(), 1);
        fs::remove_file(&final_path).unwrap();
        let retry = store
            .finalize(Arc::from([0.25_f32, -0.25]), 48_000)
            .unwrap();
        assert_eq!(retry.id, ManagedCaptureId::new(1));
    }

    #[test]
    fn temporary_name_collisions_retry_then_exhaust_without_removing_foreign_entry() {
        let mut store = CaptureStore::new().unwrap();
        let collision_leaf = format!(
            ".capture-00000000000000000001.wav.sampler-tui-tmp-{}",
            "41".repeat(32)
        );
        let collision = store.root_path().join(collision_leaf);
        fs::write(&collision, b"foreign temp").unwrap();
        let attempts = Cell::new(0_u8);

        let capture = store
            .finalize_with_controls(
                Arc::from([0.25_f32, -0.25]),
                48_000,
                |bytes| {
                    let attempt = attempts.get();
                    attempts.set(attempt + 1);
                    bytes.fill(if attempt == 0 { 0x41 } else { 0x42 });
                    Ok(())
                },
                |_, _| Ok(()),
            )
            .unwrap();
        assert_eq!(attempts.get(), 2);
        assert_eq!(fs::read(&collision).unwrap(), b"foreign temp");
        store.release(capture.id).unwrap();
        fs::remove_file(&collision).unwrap();

        let exhausted_collision_leaf = format!(
            ".capture-00000000000000000002.wav.sampler-tui-tmp-{}",
            "41".repeat(32)
        );
        let exhausted_collision = store.root_path().join(exhausted_collision_leaf);
        fs::write(&exhausted_collision, b"foreign exhausted temp").unwrap();

        let exhausted_attempts = Cell::new(0_usize);
        let error = store
            .finalize_with_controls(
                Arc::from([0.25_f32, -0.25]),
                48_000,
                |bytes| {
                    exhausted_attempts.set(exhausted_attempts.get() + 1);
                    bytes.fill(0x41);
                    Ok(())
                },
                |_, _| Ok(()),
            )
            .unwrap_err();
        assert_eq!(exhausted_attempts.get(), 16);
        assert!(matches!(
            error,
            CaptureStoreError::NameExhausted { attempts: 16, .. }
        ));
        assert_eq!(
            fs::read(&exhausted_collision).unwrap(),
            b"foreign exhausted temp"
        );
        fs::remove_file(exhausted_collision).unwrap();
        let retry = store
            .finalize(Arc::from([0.25_f32, -0.25]), 48_000)
            .unwrap();
        assert_eq!(retry.id, ManagedCaptureId::new(2));
    }

    #[test]
    fn release_requires_the_exact_live_id_and_preserves_mismatches() {
        let mut store = CaptureStore::new().unwrap();
        let capture = store.finalize(Arc::from([0.5_f32, -0.5]), 48_000).unwrap();
        let path = capture.path.clone();

        let error = store
            .release(ManagedCaptureId::new(capture.id.get() + 1))
            .unwrap_err();

        assert!(matches!(error, CaptureStoreError::NotLive { .. }));
        assert!(path.is_file());
        store.release(capture.id).unwrap();
        assert!(!path.exists());
        assert!(matches!(
            store.release(capture.id),
            Err(CaptureStoreError::NotLive { .. })
        ));
    }

    #[test]
    fn release_fails_closed_when_the_published_leaf_was_replaced() {
        let mut store = CaptureStore::new().unwrap();
        let capture = store.finalize(Arc::from([0.5_f32, -0.5]), 48_000).unwrap();
        let retained_owned_link = store.root_path().join("retained-owned-link");
        fs::hard_link(&capture.path, &retained_owned_link).unwrap();
        fs::remove_file(&capture.path).unwrap();
        fs::write(&capture.path, b"foreign replacement").unwrap();

        assert!(matches!(
            store.release(capture.id),
            Err(CaptureStoreError::IdentityMismatch { .. })
        ));
        assert_eq!(fs::read(&capture.path).unwrap(), b"foreign replacement");
        fs::remove_file(&capture.path).unwrap();
        fs::rename(retained_owned_link, &capture.path).unwrap();
    }

    #[test]
    fn finalization_rejects_payloads_over_the_prepared_limit() {
        let mut store = CaptureStore::new().unwrap();
        let oversized = vec![0.0_f32; MAX_PREPARED_FRAMES.saturating_mul(2).saturating_add(2)];

        let error = store.finalize(Arc::from(oversized), 48_000).unwrap_err();

        assert!(matches!(
            error,
            CaptureStoreError::FrameLimitExceeded { .. }
        ));
    }

    fn assert_finalize_boundary_cleanup(boundary: CaptureWritePoint) {
        let mut store = CaptureStore::new().unwrap();

        let error = store
            .finalize_with_controls(
                Arc::from([0.25_f32, -0.25]),
                48_000,
                |bytes| {
                    bytes.fill(0x62);
                    Ok(())
                },
                |point, _| injected_failure(point, boundary),
            )
            .unwrap_err();

        assert!(matches!(error, CaptureStoreError::Filesystem { .. }));
        assert!(fs::read_dir(store.root_path()).unwrap().next().is_none());
        let retry = store
            .finalize(Arc::from([0.25_f32, -0.25]), 48_000)
            .unwrap();
        assert_eq!(retry.id, ManagedCaptureId::new(1));
    }

    fn injected_failure(
        point: CaptureWritePoint,
        boundary: CaptureWritePoint,
    ) -> Result<(), CaptureStoreError> {
        if point == boundary {
            Err(injected_error(point))
        } else {
            Ok(())
        }
    }

    fn injected_error(_point: CaptureWritePoint) -> CaptureStoreError {
        CaptureStoreError::Filesystem {
            operation: "injected capture-store boundary failure",
            path: PathBuf::from("injected"),
            kind: std::io::ErrorKind::Other,
        }
    }
}
