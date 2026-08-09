use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hound::{SampleFormat, WavReader};
use sampler_audio::SampleBuffer;
use sampler_core::{
    AssetDigest, BankId, EditablePattern, MasterMixSettings, Meter, PadId, PadMixSettings,
    PadSettings, PatternSlotId, ProjectId, ProjectPattern, Resolution, SampleEditRecipe, Tempo,
    Transport,
};
use sampler_tui::export::StagedExportPad;
use sampler_tui::export_file::AtomicWavPublisher;
use sampler_tui::{
    EXPORT_CHUNK_FRAMES, EXPORT_SAMPLE_RATE, ExportToken, OfflineExportError, OfflineExportReceipt,
    OfflineExportSnapshot, OfflineFrameSink, OfflineRenderSummary, ProjectSavePad,
    SourceFingerprint, SupportedAudioExtension, render_offline,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-export-file-{name}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
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
    let transport = Transport::new(
        EXPORT_SAMPLE_RATE,
        Tempo::new(120.0).unwrap(),
        Meter::new(4, 4).unwrap(),
        1,
        Resolution::Sixteenth,
    )
    .unwrap();
    let mut editable = EditablePattern::new(slot, "atomic wav", transport).unwrap();
    editable.insert_new(pad(), 0, 1.0, None).unwrap();
    let pattern = ProjectPattern::from_editable(&editable).unwrap();
    OfflineExportSnapshot::new(
        ProjectId::from_bytes([0x44; 16]),
        9,
        slot,
        pattern,
        vec![ProjectSavePad {
            pad: pad(),
            source_path: PathBuf::from("audio/source.wav"),
            source_generation: 1,
            fingerprint: SourceFingerprint {
                digest: AssetDigest::from_bytes([0x55; 32]),
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

fn export_frames(
    destination: &Path,
    frames: &[[f32; 2]],
) -> Result<OfflineExportReceipt, OfflineExportError> {
    let snapshot = snapshot();
    let cancelled = AtomicBool::new(false);
    let mut publisher = AtomicWavPublisher::prepare(destination)?;
    publisher.write_frames(frames)?;
    publisher.publish(
        ExportToken::new(7),
        &snapshot,
        OfflineRenderSummary {
            frame_count: frames.len() as u64,
            peak: [0.5, 0.75],
        },
        &cancelled,
    )
}

#[test]
fn publisher_writes_float_stereo_48k_and_never_overwrites() {
    let fixture = Fixture::new("format-collision");
    let destination = fixture.path("mix.wav");
    let frames = [[0.25, -0.5], [0.5, -0.75]];

    let receipt = export_frames(&destination, &frames).unwrap();

    assert_eq!(
        receipt,
        OfflineExportReceipt {
            token: ExportToken::new(7),
            destination: destination.clone(),
            project_id: ProjectId::from_bytes([0x44; 16]),
            revision: 9,
            slot: PatternSlotId::new(0).unwrap(),
            sample_rate: 48_000,
            rendered_frames: 2,
            file_bytes: fs::metadata(&destination).unwrap().len(),
        }
    );

    let mut reader = WavReader::open(&destination).unwrap();
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate, 48_000);
    assert_eq!(reader.spec().bits_per_sample, 32);
    assert_eq!(reader.spec().sample_format, SampleFormat::Float);
    let decoded = reader
        .samples::<f32>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        decoded
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        frames
            .iter()
            .flat_map(|frame| frame.iter().map(|sample| sample.to_bits()))
            .collect::<Vec<_>>()
    );

    let original = fs::read(&destination).unwrap();
    assert_eq!(
        export_frames(&destination, &frames),
        Err(OfflineExportError::DestinationExists(destination.clone()))
    );
    assert_eq!(fs::read(&destination).unwrap(), original);
}

#[test]
fn publisher_rejects_empty_or_oversized_frame_writes() {
    let fixture = Fixture::new("write-bounds");
    let destination = fixture.path("mix.wav");
    let mut publisher = AtomicWavPublisher::prepare(&destination).unwrap();

    assert_eq!(
        publisher.write_frames(&[]),
        Err(OfflineExportError::Encode(destination.clone()))
    );
    assert_eq!(
        publisher.write_frames(&vec![[0.0, 0.0]; EXPORT_CHUNK_FRAMES + 1]),
        Err(OfflineExportError::Encode(destination.clone()))
    );
    publisher.abort().unwrap();
    assert!(!destination.exists());
}

#[test]
fn identical_frame_streams_produce_byte_identical_wavs() {
    let fixture = Fixture::new("deterministic");
    let first = fixture.path("first.wav");
    let second = fixture.path("second.wav");
    let frames = (0..8_193)
        .map(|index| {
            let left = (index as f32 * 0.03125).sin() * 0.5;
            [left, -left]
        })
        .collect::<Vec<_>>();

    for destination in [&first, &second] {
        let snapshot = snapshot();
        let cancelled = AtomicBool::new(false);
        let mut publisher = AtomicWavPublisher::prepare(destination).unwrap();
        for chunk in frames.chunks(EXPORT_CHUNK_FRAMES) {
            publisher.write_frames(chunk).unwrap();
        }
        publisher
            .publish(
                ExportToken::new(8),
                &snapshot,
                OfflineRenderSummary {
                    frame_count: frames.len() as u64,
                    peak: [0.5, 0.5],
                },
                &cancelled,
            )
            .unwrap();
    }

    assert_eq!(fs::read(first).unwrap(), fs::read(second).unwrap());
}

struct ObservingPublisher {
    publisher: AtomicWavPublisher,
    writes: usize,
    max_resident_frames: usize,
    every_write_was_bounded: bool,
}

impl OfflineFrameSink for ObservingPublisher {
    fn write_frames(&mut self, frames: &[[f32; 2]]) -> Result<(), OfflineExportError> {
        self.writes += 1;
        self.max_resident_frames = self.max_resident_frames.max(frames.len());
        self.every_write_was_bounded &= (1..=EXPORT_CHUNK_FRAMES).contains(&frames.len());
        self.publisher.write_frames(frames)
    }
}

#[test]
fn renderer_to_publisher_stream_uses_only_bounded_nonempty_chunks() {
    let fixture = Fixture::new("stream-bounds");
    let destination = fixture.path("mix.wav");
    let snapshot = snapshot();
    let staged = vec![StagedExportPad {
        pad: pad(),
        sample: Arc::new(SampleBuffer::new(EXPORT_SAMPLE_RATE, vec![0.5, -0.5]).unwrap()),
        settings: PadSettings::default(),
        mix: PadMixSettings::default(),
    }];
    let cancelled = AtomicBool::new(false);
    let mut sink = ObservingPublisher {
        publisher: AtomicWavPublisher::prepare(&destination).unwrap(),
        writes: 0,
        max_resident_frames: 0,
        every_write_was_bounded: true,
    };
    let summary = render_offline(&snapshot, &staged, &mut sink, &cancelled).unwrap();

    assert!(sink.writes > 1);
    assert!(sink.every_write_was_bounded);
    assert_eq!(sink.max_resident_frames, EXPORT_CHUNK_FRAMES);
    sink.publisher
        .publish(ExportToken::new(9), &snapshot, summary, &cancelled)
        .unwrap();
}

#[test]
fn abort_rejects_a_replaced_temp_without_deleting_the_foreign_inode() {
    let fixture = Fixture::new("abort-identity");
    let destination = fixture.path("mix.wav");
    let publisher = AtomicWavPublisher::prepare(&destination).unwrap();
    let temporary = fs::read_dir(&fixture.root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".mix.wav.sampler-tui-tmp-")
        })
        .unwrap();
    fs::remove_file(&temporary).unwrap();
    fs::write(&temporary, b"foreign replacement").unwrap();

    assert!(matches!(
        publisher.abort(),
        Err(OfflineExportError::Cleanup { .. })
    ));
    assert_eq!(fs::read(temporary).unwrap(), b"foreign replacement");
    assert!(!destination.exists());
}
