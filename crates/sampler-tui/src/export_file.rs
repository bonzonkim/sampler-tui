use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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
    AfterPrepare,
    BeforeWrite,
    BeforeFinalize,
    BeforeFileSync,
    BeforePublish,
    BeforeDirectorySync,
}

#[cfg(test)]
type FailureHook = Box<dyn FnMut(PublisherCheckpoint) -> Option<io::ErrorKind>>;

/// Streams bounded stereo frames into an identity-owned temporary WAV and atomically publishes it.
pub struct AtomicWavPublisher {
    writer: Option<WavWriter<File>>,
    temporary: AnchoredTemp,
    parent: File,
    destination_leaf: std::ffi::OsString,
    destination: PathBuf,
    written_frames: u64,
    #[cfg(test)]
    hook: FailureHook,
}

impl AtomicWavPublisher {
    pub fn prepare(destination: &Path) -> Result<Self, OfflineExportError> {
        Self::prepare_internal(
            destination,
            #[cfg(test)]
            Box::new(|_| None),
        )
    }

    #[cfg(test)]
    fn prepare_with_hook<F>(destination: &Path, hook: F) -> Result<Self, OfflineExportError>
    where
        F: FnMut(PublisherCheckpoint) -> Option<io::ErrorKind> + 'static,
    {
        Self::prepare_internal(destination, Box::new(hook))
    }

    fn prepare_internal(
        destination: &Path,
        #[cfg(test)] hook: FailureHook,
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
            file,
            WavSpec {
                channels: 2,
                sample_rate: EXPORT_SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: SampleFormat::Float,
            },
        )
        .map_err(|_| OfflineExportError::Encode(destination.to_path_buf()))?;
        let mut publisher = Self {
            writer: Some(writer),
            temporary,
            parent,
            destination_leaf,
            destination: destination.to_path_buf(),
            written_frames: 0,
            #[cfg(test)]
            hook,
        };
        publisher.checkpoint(PublisherCheckpoint::AfterPrepare)?;
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

        self.checkpoint(PublisherCheckpoint::BeforeFinalize)?;
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| OfflineExportError::Encode(self.destination.clone()))?;
        writer
            .flush()
            .and_then(|()| writer.finalize())
            .map_err(|_| OfflineExportError::Encode(self.destination.clone()))?;

        self.checkpoint(PublisherCheckpoint::BeforeFileSync)?;
        self.temporary
            .identity()
            .sync_all()
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

        self.checkpoint(PublisherCheckpoint::BeforePublish)?;
        if cancelled.load(Ordering::Acquire) {
            return Err(OfflineExportError::Cancelled);
        }
        revalidate_anchored_parent(&self.destination, &self.parent)
            .map_err(|error| publish_error(&self.destination, error))?;
        self.temporary
            .verify_path_identity()
            .map_err(|error| publish_error(&self.destination, error))?;
        match self
            .temporary
            .link_noreplace(&self.parent, Path::new(&self.destination_leaf))
        {
            Ok(NoReplacePublication::Published) => {}
            Ok(NoReplacePublication::DestinationExists) => {
                return Err(OfflineExportError::DestinationExists(
                    self.destination.clone(),
                ));
            }
            Err(error) => {
                return Err(OfflineExportError::Publish {
                    path: self.destination.clone(),
                    kind: io::Error::from(error).kind(),
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
        self.checkpoint(PublisherCheckpoint::BeforeDirectorySync)?;
        revalidate_anchored_parent(&self.destination, &self.parent)
            .map_err(|error| publish_error(&self.destination, error))?;
        self.parent
            .sync_all()
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
        self.parent
            .sync_all()
            .map_err(|error| OfflineExportError::Cleanup {
                path: self.destination.clone(),
                kind: error.kind(),
            })
    }

    #[cfg(not(test))]
    fn checkpoint(&mut self, _point: PublisherCheckpoint) -> Result<(), OfflineExportError> {
        Ok(())
    }

    #[cfg(test)]
    fn checkpoint(&mut self, point: PublisherCheckpoint) -> Result<(), OfflineExportError> {
        let Some(kind) = (self.hook)(point) else {
            return Ok(());
        };
        Err(match point {
            PublisherCheckpoint::AfterPrepare => OfflineExportError::TemporaryFile {
                path: self.destination.clone(),
                kind,
            },
            PublisherCheckpoint::BeforeWrite | PublisherCheckpoint::BeforeFinalize => {
                OfflineExportError::Encode(self.destination.clone())
            }
            PublisherCheckpoint::BeforeFileSync | PublisherCheckpoint::BeforeDirectorySync => {
                OfflineExportError::Sync {
                    path: self.destination.clone(),
                    kind,
                }
            }
            PublisherCheckpoint::BeforePublish => OfflineExportError::Publish {
                path: self.destination.clone(),
                kind,
            },
        })
    }
}

impl OfflineFrameSink for AtomicWavPublisher {
    fn write_frames(&mut self, frames: &[[f32; 2]]) -> Result<(), OfflineExportError> {
        if frames.is_empty() || frames.len() > EXPORT_CHUNK_FRAMES {
            return Err(OfflineExportError::Encode(self.destination.clone()));
        }
        self.checkpoint(PublisherCheckpoint::BeforeWrite)?;
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
    use std::path::{Path, PathBuf};
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

    use super::{AtomicWavPublisher, PublisherCheckpoint};

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

    fn failure_result(
        destination: &Path,
        point: PublisherCheckpoint,
    ) -> Result<(), OfflineExportError> {
        let hook = move |candidate| (candidate == point).then_some(io::ErrorKind::Other);
        let mut publisher = AtomicWavPublisher::prepare_with_hook(destination, hook)?;
        publisher.write_frames(&[[0.25, -0.5]])?;
        publish(publisher, &AtomicBool::new(false))
    }

    #[test]
    fn every_injected_failure_fence_preserves_an_absent_destination_and_cleans_owned_temp() {
        for point in [
            PublisherCheckpoint::AfterPrepare,
            PublisherCheckpoint::BeforeWrite,
            PublisherCheckpoint::BeforeFinalize,
            PublisherCheckpoint::BeforeFileSync,
            PublisherCheckpoint::BeforePublish,
            PublisherCheckpoint::BeforeDirectorySync,
        ] {
            let fixture = Fixture::new("failure-matrix");
            let destination = fixture.destination();

            assert!(failure_result(&destination, point).is_err(), "{point:?}");
            assert!(!destination.exists(), "{point:?}");
            assert!(fixture.temp_entries().is_empty(), "{point:?}");
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
            None
        };
        let mut publisher = AtomicWavPublisher::prepare_with_hook(&destination, hook).unwrap();
        publisher.write_frames(&[[0.25, -0.5]]).unwrap();

        assert_eq!(
            publish(publisher, &AtomicBool::new(false)),
            Err(OfflineExportError::DestinationExists(destination.clone()))
        );
        assert_eq!(fs::read(&destination).unwrap(), b"foreign destination");
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
                None
            };
            let mut publisher = AtomicWavPublisher::prepare_with_hook(&destination, hook).unwrap();
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
            None
        };
        let mut publisher = AtomicWavPublisher::prepare_with_hook(&destination, hook).unwrap();
        publisher.write_frames(&[[0.25, -0.5]]).unwrap();

        assert!(matches!(
            publish(publisher, &AtomicBool::new(false)),
            Err(OfflineExportError::Publish { .. })
        ));
        assert!(!destination.exists());
        assert!(fs::read_dir(&held).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('.')
        }));
        fs::remove_dir_all(held).unwrap();
    }
}
