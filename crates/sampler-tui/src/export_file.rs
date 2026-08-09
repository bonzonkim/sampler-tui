use std::fs::File;
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
use std::sync::{Arc, Mutex};

use hound::{SampleFormat, WavSpec, WavWriter};
use rustix::fs::FileType as RustixFileType;

use crate::export::{
    EXPORT_CHUNK_FRAMES, EXPORT_SAMPLE_RATE, ExportToken, OfflineExportError, OfflineExportReceipt,
    OfflineExportSnapshot, validate_wav_destination,
};
use crate::export_render::{OfflineFrameSink, OfflineRenderSummary};
use crate::project_store::{
    AnchoredTemp, AtomicWritePoint, NoReplacePublication, ProjectStoreError, create_anchored_temp,
    open_anchored_parent, revalidate_anchored_parent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublisherCheckpoint {
    BeforePublish,
    BeforeDirectorySync,
}

#[cfg(test)]
type MutationHook = Box<dyn FnMut(PublisherCheckpoint)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriterPhase {
    #[cfg(test)]
    Header,
    Samples,
    Finalize,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublisherFault {
    HeaderWrite,
    PartialSampleWrite,
    FinalizeWrite,
    FinalizeSeek,
    FileSync,
    Link,
    DirectorySync { failures: usize },
}

#[cfg(test)]
#[derive(Debug)]
struct PublisherFaultState {
    fault: PublisherFault,
    writer_phase: WriterPhase,
    writer_fault_consumed: bool,
    partial_write_started: bool,
    directory_sync_failures_remaining: usize,
}

struct PublisherFile {
    file: File,
    #[cfg(test)]
    faults: Option<Arc<Mutex<PublisherFaultState>>>,
}

impl Write for PublisherFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        #[cfg(test)]
        if let Some(faults) = &self.faults {
            let mut state = faults.lock().expect("publisher fault mutex poisoned");
            let fail_phase = match state.fault {
                PublisherFault::HeaderWrite => Some(WriterPhase::Header),
                PublisherFault::PartialSampleWrite => Some(WriterPhase::Samples),
                PublisherFault::FinalizeWrite => Some(WriterPhase::Finalize),
                _ => None,
            };
            if fail_phase == Some(state.writer_phase) && !state.writer_fault_consumed {
                if state.fault == PublisherFault::PartialSampleWrite && !state.partial_write_started
                {
                    let partial = bytes.len().min(2);
                    let written = self.file.write(&bytes[..partial])?;
                    state.partial_write_started = true;
                    return Ok(written);
                }
                state.writer_fault_consumed = true;
                return Err(io::Error::other("injected publisher writer failure"));
            }
        }
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for PublisherFile {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        #[cfg(test)]
        if let Some(faults) = &self.faults {
            let mut state = faults.lock().expect("publisher fault mutex poisoned");
            if state.fault == PublisherFault::FinalizeSeek
                && state.writer_phase == WriterPhase::Finalize
                && !state.writer_fault_consumed
            {
                state.writer_fault_consumed = true;
                return Err(io::Error::other("injected publisher seek failure"));
            }
        }
        self.file.seek(position)
    }
}

#[derive(Default)]
struct PublisherIo {
    #[cfg(test)]
    faults: Option<Arc<Mutex<PublisherFaultState>>>,
}

impl PublisherIo {
    #[cfg(test)]
    fn with_fault(fault: PublisherFault) -> Self {
        let directory_sync_failures_remaining = match fault {
            PublisherFault::DirectorySync { failures } => failures,
            _ => 0,
        };
        Self {
            faults: Some(Arc::new(Mutex::new(PublisherFaultState {
                fault,
                writer_phase: WriterPhase::Header,
                writer_fault_consumed: false,
                partial_write_started: false,
                directory_sync_failures_remaining,
            }))),
        }
    }

    fn wrap_file(&self, file: File) -> PublisherFile {
        PublisherFile {
            file,
            #[cfg(test)]
            faults: self.faults.clone(),
        }
    }

    fn set_writer_phase(&self, phase: WriterPhase) {
        #[cfg(not(test))]
        let _ = phase;
        #[cfg(test)]
        if let Some(faults) = &self.faults {
            faults
                .lock()
                .expect("publisher fault mutex poisoned")
                .writer_phase = phase;
        }
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        #[cfg(test)]
        if let Some(faults) = &self.faults {
            let mut state = faults.lock().expect("publisher fault mutex poisoned");
            if state.fault == PublisherFault::FileSync && !state.writer_fault_consumed {
                state.writer_fault_consumed = true;
                return Err(io::Error::other("injected file sync failure"));
            }
        }
        file.sync_all()
    }

    fn link_noreplace(
        &self,
        temporary: &AnchoredTemp,
        parent: &File,
        leaf: &Path,
    ) -> io::Result<NoReplacePublication> {
        #[cfg(test)]
        if let Some(faults) = &self.faults {
            let mut state = faults.lock().expect("publisher fault mutex poisoned");
            if state.fault == PublisherFault::Link && !state.writer_fault_consumed {
                state.writer_fault_consumed = true;
                return Err(io::Error::other("injected linkat failure"));
            }
        }
        temporary
            .link_noreplace(parent, leaf)
            .map_err(io::Error::from)
    }

    fn sync_directory(&self, directory: &File) -> io::Result<()> {
        #[cfg(test)]
        if let Some(faults) = &self.faults {
            let mut state = faults.lock().expect("publisher fault mutex poisoned");
            if state.directory_sync_failures_remaining != 0 {
                state.directory_sync_failures_remaining -= 1;
                return Err(io::Error::other("injected directory sync failure"));
            }
        }
        directory.sync_all()
    }
}

/// Streams bounded stereo frames into an identity-owned temporary WAV and atomically publishes it.
pub struct AtomicWavPublisher {
    writer: Option<WavWriter<PublisherFile>>,
    temporary: AnchoredTemp,
    parent: File,
    destination_leaf: std::ffi::OsString,
    destination: PathBuf,
    written_frames: u64,
    io: PublisherIo,
    #[cfg(test)]
    mutation_hook: MutationHook,
}

impl AtomicWavPublisher {
    pub fn prepare(destination: &Path) -> Result<Self, OfflineExportError> {
        Self::prepare_internal(
            destination,
            PublisherIo::default(),
            #[cfg(test)]
            Box::new(|_| {}),
        )
    }

    #[cfg(test)]
    fn prepare_with_mutation_hook<F>(
        destination: &Path,
        hook: F,
    ) -> Result<Self, OfflineExportError>
    where
        F: FnMut(PublisherCheckpoint) + 'static,
    {
        Self::prepare_internal(destination, PublisherIo::default(), Box::new(hook))
    }

    #[cfg(test)]
    fn prepare_with_fault(
        destination: &Path,
        fault: PublisherFault,
    ) -> Result<Self, OfflineExportError> {
        Self::prepare_internal(
            destination,
            PublisherIo::with_fault(fault),
            Box::new(|_| {}),
        )
    }

    fn prepare_internal(
        destination: &Path,
        io: PublisherIo,
        #[cfg(test)] mutation_hook: MutationHook,
    ) -> Result<Self, OfflineExportError> {
        validate_wav_destination(destination)?;
        let (parent, destination_leaf) = open_anchored_parent(destination, false)
            .map_err(|_| OfflineExportError::OutputParent(destination.to_path_buf()))?;
        let leaf_path = Path::new(&destination_leaf);
        match rustix::fs::statat(&parent, leaf_path, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat)
                if RustixFileType::from_raw_mode(stat.st_mode) == RustixFileType::Directory =>
            {
                return Err(OfflineExportError::OutputDirectory(
                    destination.to_path_buf(),
                ));
            }
            Ok(_) => {
                return Err(OfflineExportError::DestinationExists(
                    destination.to_path_buf(),
                ));
            }
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => {
                return Err(OfflineExportError::TemporaryFile {
                    path: destination.to_path_buf(),
                    kind: io::Error::from(error).kind(),
                });
            }
        }

        let (file, temporary) = create_anchored_temp(
            &parent,
            destination.parent().unwrap_or(Path::new(".")),
            destination,
        )
        .map_err(|error| temporary_error(destination, error))?;
        let writer = WavWriter::new(
            io.wrap_file(file),
            WavSpec {
                channels: 2,
                sample_rate: EXPORT_SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )
        .map_err(|_| OfflineExportError::Encode(destination.to_path_buf()))?;
        io.set_writer_phase(WriterPhase::Samples);
        let publisher = Self {
            writer: Some(writer),
            temporary,
            parent,
            destination_leaf,
            destination: destination.to_path_buf(),
            written_frames: 0,
            io,
            #[cfg(test)]
            mutation_hook,
        };
        Ok(publisher)
    }

    /// Removes this publisher's temporary inode if its sibling pathname still names that inode.
    pub fn abort(mut self) -> Result<(), OfflineExportError> {
        drop(self.writer.take());
        self.temporary
            .unlink_owned()
            .map_err(|error| cleanup_error(&self.destination, error))
    }

    pub fn publish(
        mut self,
        token: ExportToken,
        snapshot: &OfflineExportSnapshot,
        summary: OfflineRenderSummary,
        cancelled: &AtomicBool,
    ) -> Result<OfflineExportReceipt, OfflineExportError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(OfflineExportError::Cancelled);
        }
        if summary.frame_count != self.written_frames {
            return Err(OfflineExportError::Arithmetic);
        }

        self.io.set_writer_phase(WriterPhase::Finalize);
        let writer = self
            .writer
            .take()
            .ok_or_else(|| OfflineExportError::Encode(self.destination.clone()))?;
        writer
            .finalize()
            .map_err(|_| OfflineExportError::Encode(self.destination.clone()))?;

        self.io
            .sync_file(self.temporary.identity())
            .map_err(|error| OfflineExportError::Sync {
                path: self.destination.clone(),
                kind: error.kind(),
            })?;
        let file_bytes = self
            .temporary
            .identity()
            .metadata()
            .map_err(|error| OfflineExportError::Sync {
                path: self.destination.clone(),
                kind: error.kind(),
            })?
            .len();

        self.mutate(PublisherCheckpoint::BeforePublish);
        if cancelled.load(Ordering::Acquire) {
            return Err(OfflineExportError::Cancelled);
        }
        revalidate_anchored_parent(&self.destination, &self.parent)
            .map_err(|error| publish_error(&self.destination, error))?;
        self.temporary
            .verify_path_identity()
            .map_err(|error| publish_error(&self.destination, error))?;
        match self.io.link_noreplace(
            &self.temporary,
            &self.parent,
            Path::new(&self.destination_leaf),
        ) {
            Ok(NoReplacePublication::Published) => {}
            Ok(NoReplacePublication::DestinationExists) => {
                return Err(OfflineExportError::DestinationExists(
                    self.destination.clone(),
                ));
            }
            Err(error) => {
                return Err(OfflineExportError::Publish {
                    path: self.destination.clone(),
                    kind: error.kind(),
                });
            }
        }

        if let Err(error) = self.finish_publication() {
            return match self.rollback_publication() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(cleanup),
            };
        }

        Ok(OfflineExportReceipt {
            token,
            destination: self.destination.clone(),
            project_id: snapshot.project_id(),
            revision: snapshot.revision(),
            slot: snapshot.slot(),
            sample_rate: EXPORT_SAMPLE_RATE,
            rendered_frames: summary.frame_count,
            file_bytes,
        })
    }

    fn finish_publication(&mut self) -> Result<(), OfflineExportError> {
        self.mutate(PublisherCheckpoint::BeforeDirectorySync);
        self.temporary
            .verify_destination_identity(
                &self.parent,
                Path::new(&self.destination_leaf),
                &self.destination,
                AtomicWritePoint::BeforeDirectorySync,
            )
            .map_err(|error| publish_error(&self.destination, error))?;
        self.temporary
            .unlink_owned()
            .map_err(|error| cleanup_error(&self.destination, error))?;
        revalidate_anchored_parent(&self.destination, &self.parent)
            .map_err(|error| publish_error(&self.destination, error))?;
        self.io
            .sync_directory(&self.parent)
            .map_err(|error| OfflineExportError::Sync {
                path: self.destination.clone(),
                kind: error.kind(),
            })
    }

    fn rollback_publication(&mut self) -> Result<(), OfflineExportError> {
        self.temporary
            .verify_destination_identity(
                &self.parent,
                Path::new(&self.destination_leaf),
                &self.destination,
                AtomicWritePoint::BeforeDirectorySync,
            )
            .map_err(|error| cleanup_error(&self.destination, error))?;
        rustix::fs::unlinkat(
            &self.parent,
            Path::new(&self.destination_leaf),
            rustix::fs::AtFlags::empty(),
        )
        .map_err(|error| OfflineExportError::Cleanup {
            path: self.destination.clone(),
            kind: io::Error::from(error).kind(),
        })?;
        self.io
            .sync_directory(&self.parent)
            .map_err(|error| OfflineExportError::Cleanup {
                path: self.destination.clone(),
                kind: error.kind(),
            })
    }

    #[cfg(not(test))]
    fn mutate(&mut self, _point: PublisherCheckpoint) {}

    #[cfg(test)]
    fn mutate(&mut self, point: PublisherCheckpoint) {
        (self.mutation_hook)(point);
    }
}

impl OfflineFrameSink for AtomicWavPublisher {
    fn write_frames(&mut self, frames: &[[f32; 2]]) -> Result<(), OfflineExportError> {
        if frames.is_empty() || frames.len() > EXPORT_CHUNK_FRAMES {
            return Err(OfflineExportError::Encode(self.destination.clone()));
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| OfflineExportError::Encode(self.destination.clone()))?;
        for frame in frames {
            writer
                .write_sample(frame[0])
                .and_then(|()| writer.write_sample(frame[1]))
                .map_err(|_| OfflineExportError::Encode(self.destination.clone()))?;
        }
        self.written_frames = self
            .written_frames
            .checked_add(frames.len() as u64)
            .ok_or(OfflineExportError::Arithmetic)?;
        Ok(())
    }
}

fn error_kind(error: &ProjectStoreError) -> io::ErrorKind {
    match error {
        ProjectStoreError::SourceRead { kind, .. }
        | ProjectStoreError::AtomicWrite { kind, .. }
        | ProjectStoreError::Filesystem { kind, .. } => *kind,
        _ => io::ErrorKind::Other,
    }
}

fn temporary_error(destination: &Path, error: ProjectStoreError) -> OfflineExportError {
    OfflineExportError::TemporaryFile {
        path: destination.to_path_buf(),
        kind: error_kind(&error),
    }
}

fn publish_error(destination: &Path, error: ProjectStoreError) -> OfflineExportError {
    OfflineExportError::Publish {
        path: destination.to_path_buf(),
        kind: error_kind(&error),
    }
}

fn cleanup_error(destination: &Path, error: ProjectStoreError) -> OfflineExportError {
    OfflineExportError::Cleanup {
        path: destination.to_path_buf(),
        kind: error_kind(&error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use sampler_core::{
        AssetDigest, BankId, EventId, MasterMixSettings, Meter, PadId, PadMixSettings, PadSettings,
        PatternEvent, PatternSlotId, ProjectId, ProjectPattern, ProjectPatternEvent, Resolution,
        SampleEditRecipe, Tempo,
    };

    use crate::{
        EXPORT_SAMPLE_RATE, ExportToken, OfflineExportError, OfflineExportSnapshot,
        OfflineFrameSink, OfflineRenderSummary, ProjectSavePad, SourceFingerprint,
        SupportedAudioExtension,
    };

    use super::{AtomicWavPublisher, PublisherCheckpoint, PublisherFault};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "sampler-tui-export-file-unit-{name}-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir(&root).unwrap();
            Self { root }
        }

        fn destination(&self) -> PathBuf {
            self.root.join("mix.wav")
        }

        fn temp_entries(&self) -> Vec<PathBuf> {
            fs::read_dir(&self.root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".mix.wav.sampler-tui-tmp-")
                })
                .collect()
        }

        fn entry_names(&self) -> std::collections::BTreeSet<std::ffi::OsString> {
            fs::read_dir(&self.root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn pad() -> PadId {
        PadId::new(BankId::new(0).unwrap(), 0).unwrap()
    }

    fn snapshot() -> OfflineExportSnapshot {
        let slot = PatternSlotId::new(0).unwrap();
        OfflineExportSnapshot::new(
            ProjectId::from_bytes([0x66; 16]),
            12,
            slot,
            ProjectPattern {
                slot,
                name: "failure fences".to_owned(),
                sample_rate: EXPORT_SAMPLE_RATE,
                tempo: Tempo::new(120.0).unwrap(),
                meter: Meter::new(4, 4).unwrap(),
                bars: 1,
                resolution: Resolution::Sixteenth,
                swing: 0.5,
                quantize_strength: 0.0,
                events: vec![ProjectPatternEvent {
                    event: PatternEvent::new(EventId(1), pad(), 0, 1.0, None).unwrap(),
                    raw_frame: 0,
                }],
            },
            vec![ProjectSavePad {
                pad: pad(),
                source_path: PathBuf::from("audio/source.wav"),
                source_generation: 1,
                fingerprint: SourceFingerprint {
                    digest: AssetDigest::from_bytes([0x77; 32]),
                    encoded_bytes: 8,
                    extension: SupportedAudioExtension::Wav,
                },
                settings: PadSettings::default(),
                mix: PadMixSettings::default(),
                recipe: SampleEditRecipe::identity(),
            }],
            MasterMixSettings::default(),
            EXPORT_SAMPLE_RATE,
        )
        .unwrap()
    }

    fn publish(
        publisher: AtomicWavPublisher,
        cancelled: &AtomicBool,
    ) -> Result<(), OfflineExportError> {
        publisher
            .publish(
                ExportToken::new(21),
                &snapshot(),
                OfflineRenderSummary {
                    frame_count: 1,
                    peak: [0.25, 0.5],
                },
                cancelled,
            )
            .map(|_| ())
    }

    #[test]
    fn writer_faults_are_observed_from_header_partial_sample_and_finalize_io() {
        for fault in [
            PublisherFault::HeaderWrite,
            PublisherFault::PartialSampleWrite,
            PublisherFault::FinalizeWrite,
        ] {
            let fixture = Fixture::new("writer-io-fault");
            fs::write(fixture.root.join("foreign-entry"), b"foreign").unwrap();
            let destination = fixture.destination();
            let before = fixture.entry_names();

            let result = match AtomicWavPublisher::prepare_with_fault(&destination, fault) {
                Err(error) => Err(error),
                Ok(mut publisher) => match publisher.write_frames(&[[0.25, -0.5]]) {
                    Err(error) => Err(error),
                    Ok(()) => publish(publisher, &AtomicBool::new(false)),
                },
            };

            assert_eq!(result, Err(OfflineExportError::Encode(destination)));
            assert_eq!(fixture.entry_names(), before, "{fault:?}");
            assert_eq!(
                fs::read(fixture.root.join("foreign-entry")).unwrap(),
                b"foreign"
            );
        }
    }

    #[test]
    fn finalize_seek_fault_executes_seek_and_cleans_only_the_owned_temp() {
        let fixture = Fixture::new("finalize-seek-fault");
        fs::write(fixture.root.join("foreign-entry"), b"foreign").unwrap();
        let destination = fixture.destination();
        let before = fixture.entry_names();
        let mut publisher =
            AtomicWavPublisher::prepare_with_fault(&destination, PublisherFault::FinalizeSeek)
                .unwrap();
        publisher.write_frames(&[[0.25, -0.5]]).unwrap();

        assert_eq!(
            publish(publisher, &AtomicBool::new(false)),
            Err(OfflineExportError::Encode(destination))
        );
        assert_eq!(fixture.entry_names(), before);
        assert_eq!(
            fs::read(fixture.root.join("foreign-entry")).unwrap(),
            b"foreign"
        );
    }

    #[test]
    fn sync_link_and_repeated_rollback_sync_faults_return_exact_errors_and_no_destination() {
        for (fault, expected) in [
            (
                PublisherFault::FileSync,
                OfflineExportError::Sync {
                    path: PathBuf::new(),
                    kind: io::ErrorKind::Other,
                },
            ),
            (
                PublisherFault::Link,
                OfflineExportError::Publish {
                    path: PathBuf::new(),
                    kind: io::ErrorKind::Other,
                },
            ),
            (
                PublisherFault::DirectorySync { failures: 1 },
                OfflineExportError::Sync {
                    path: PathBuf::new(),
                    kind: io::ErrorKind::Other,
                },
            ),
            (
                PublisherFault::DirectorySync { failures: 2 },
                OfflineExportError::Cleanup {
                    path: PathBuf::new(),
                    kind: io::ErrorKind::Other,
                },
            ),
        ] {
            let fixture = Fixture::new("publication-io-fault");
            fs::write(fixture.root.join("foreign-entry"), b"foreign").unwrap();
            let destination = fixture.destination();
            let before = fixture.entry_names();
            let mut publisher =
                AtomicWavPublisher::prepare_with_fault(&destination, fault).unwrap();
            publisher.write_frames(&[[0.25, -0.5]]).unwrap();

            let mut expected = expected;
            match &mut expected {
                OfflineExportError::Sync { path, .. }
                | OfflineExportError::Publish { path, .. }
                | OfflineExportError::Cleanup { path, .. } => *path = destination.clone(),
                _ => unreachable!(),
            }
            assert_eq!(publish(publisher, &AtomicBool::new(false)), Err(expected));
            assert_eq!(fixture.entry_names(), before, "{fault:?}");
            assert_eq!(
                fs::read(fixture.root.join("foreign-entry")).unwrap(),
                b"foreign"
            );
        }
    }

    #[test]
    fn destination_substitution_is_preserved_and_only_owned_temp_is_removed() {
        let fixture = Fixture::new("destination-substitution");
        let destination = fixture.destination();
        let hook_destination = destination.clone();
        let substituted = Arc::new(AtomicBool::new(false));
        let hook_substituted = Arc::clone(&substituted);
        let hook = move |point| {
            if point == PublisherCheckpoint::BeforePublish
                && !hook_substituted.swap(true, Ordering::AcqRel)
            {
                fs::write(&hook_destination, b"foreign destination").unwrap();
            }
        };
        let mut publisher =
            AtomicWavPublisher::prepare_with_mutation_hook(&destination, hook).unwrap();
        publisher.write_frames(&[[0.25, -0.5]]).unwrap();

        assert_eq!(
            publish(publisher, &AtomicBool::new(false)),
            Err(OfflineExportError::DestinationExists(destination.clone()))
        );
        assert_eq!(fs::read(&destination).unwrap(), b"foreign destination");
        assert!(fixture.temp_entries().is_empty());
    }

    #[test]
    fn post_link_destination_substitution_never_returns_a_receipt_or_deletes_foreign_bytes() {
        let fixture = Fixture::new("post-link-destination-substitution");
        let destination = fixture.destination();
        let hook_destination = destination.clone();
        let substituted = Arc::new(AtomicBool::new(false));
        let hook_substituted = Arc::clone(&substituted);
        let hook = move |point| {
            if point == PublisherCheckpoint::BeforeDirectorySync
                && !hook_substituted.swap(true, Ordering::AcqRel)
            {
                fs::remove_file(&hook_destination).unwrap();
                fs::write(&hook_destination, b"foreign post-link destination").unwrap();
            }
        };
        let mut publisher =
            AtomicWavPublisher::prepare_with_mutation_hook(&destination, hook).unwrap();
        publisher.write_frames(&[[0.25, -0.5]]).unwrap();

        assert_eq!(
            publisher.publish(
                ExportToken::new(21),
                &snapshot(),
                OfflineRenderSummary {
                    frame_count: 1,
                    peak: [0.25, 0.5],
                },
                &AtomicBool::new(false),
            ),
            Err(OfflineExportError::Cleanup {
                path: destination.clone(),
                kind: io::ErrorKind::Other,
            })
        );
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"foreign post-link destination"
        );
        assert!(fixture.temp_entries().is_empty());
    }

    #[test]
    fn cancellation_before_and_at_publish_leaves_no_destination_or_owned_temp() {
        for cancel_at_fence in [false, true] {
            let fixture = Fixture::new("cancel-publish");
            let destination = fixture.destination();
            let cancelled = Arc::new(AtomicBool::new(!cancel_at_fence));
            let hook_cancelled = Arc::clone(&cancelled);
            let hook = move |point| {
                if cancel_at_fence && point == PublisherCheckpoint::BeforePublish {
                    hook_cancelled.store(true, Ordering::Release);
                }
            };
            let mut publisher =
                AtomicWavPublisher::prepare_with_mutation_hook(&destination, hook).unwrap();
            publisher.write_frames(&[[0.25, -0.5]]).unwrap();

            assert_eq!(
                publish(publisher, &cancelled),
                Err(OfflineExportError::Cancelled)
            );
            assert!(!destination.exists());
            assert!(fixture.temp_entries().is_empty());
        }
    }

    #[test]
    fn parent_substitution_before_publish_fails_closed_and_cleans_the_anchored_temp() {
        let fixture = Fixture::new("parent-substitution");
        let destination = fixture.destination();
        let held = fixture.root.with_extension("held");
        let hook_root = fixture.root.clone();
        let hook_held = held.clone();
        let substituted = Arc::new(AtomicBool::new(false));
        let hook_substituted = Arc::clone(&substituted);
        let hook = move |point| {
            if point == PublisherCheckpoint::BeforePublish
                && !hook_substituted.swap(true, Ordering::AcqRel)
            {
                fs::rename(&hook_root, &hook_held).unwrap();
                fs::create_dir(&hook_root).unwrap();
            }
        };
        let mut publisher =
            AtomicWavPublisher::prepare_with_mutation_hook(&destination, hook).unwrap();
        publisher.write_frames(&[[0.25, -0.5]]).unwrap();

        assert_eq!(
            publish(publisher, &AtomicBool::new(false)),
            Err(OfflineExportError::Publish {
                path: destination.clone(),
                kind: io::ErrorKind::Other,
            })
        );
        assert!(!destination.exists());
        assert!(!held.join("mix.wav").exists());
        assert_eq!(fs::read_dir(&held).unwrap().count(), 0);
        fs::remove_dir_all(held).unwrap();
    }
}
