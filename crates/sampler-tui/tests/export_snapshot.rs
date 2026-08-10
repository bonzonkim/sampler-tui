use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use hound::{SampleFormat, WavSpec, WavWriter};
use sampler_core::{
    BankId, EventId, MasterMixSettings, Meter, PadId, PadMixSettings, PadSettings, PatternEvent,
    PatternSlotId, ProjectDocument, ProjectId, ProjectPad, ProjectPattern, ProjectPatternEvent,
    Resolution, SampleEditRecipe, Tempo, Transport,
};
use sampler_tui::export::stage_export_samples;
use sampler_tui::{
    EXPORT_SAMPLE_RATE, ExportPatternSlot, OfflineExportError, OfflineExportSnapshot,
    ProjectSavePad, ProjectSaveSnapshot, ProjectStore, SourceFingerprint,
};

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "sampler-tui-export-snapshot-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("audio")).unwrap();
        Self { directory }
    }

    fn add_wav(&self, seed: f32) -> (String, sampler_core::AssetDigest) {
        let source = self.directory.join(format!("source-{seed}.wav"));
        write_wav(&source, seed);
        let fingerprint = SourceFingerprint::from_path(&source).unwrap();
        let relative = format!("audio/{}.wav", fingerprint.digest);
        fs::rename(&source, self.directory.join(&relative)).unwrap();
        (relative, fingerprint.digest)
    }
}

fn write_wav(path: &std::path::Path, seed: f32) {
    let mut writer = WavWriter::create(
        path,
        WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        },
    )
    .unwrap();
    for frame in [[seed, -seed], [seed * 0.5, -seed * 0.5], [0.0, 0.0]] {
        writer.write_sample(frame[0]).unwrap();
        writer.write_sample(frame[1]).unwrap();
    }
    writer.finalize().unwrap();
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn pattern(slot: PatternSlotId, pads: &[PadId]) -> ProjectPattern {
    let tempo = Tempo::new(120.0).unwrap();
    let meter = Meter::new(4, 4).unwrap();
    let transport = Transport::new(44_100, tempo, meter, 1, Resolution::Sixteenth)
        .unwrap()
        .with_swing(0.5)
        .unwrap();
    ProjectPattern {
        slot,
        name: format!("Pattern {}", slot.get() + 1),
        sample_rate: 44_100,
        tempo,
        meter,
        bars: 1,
        resolution: Resolution::Sixteenth,
        swing: 0.5,
        quantize_strength: 0.0,
        events: pads
            .iter()
            .copied()
            .enumerate()
            .map(|(index, pad)| {
                let raw_frame = 22_050 + index as u64;
                ProjectPatternEvent {
                    event: PatternEvent::new(
                        EventId(index as u64 + 1),
                        pad,
                        raw_frame,
                        1.0,
                        Some(22_050),
                    )
                    .unwrap()
                    .quantized(&transport, 0.0),
                    raw_frame,
                }
            })
            .collect(),
    }
}

fn document(pads: Vec<ProjectPad>, selected: ProjectPattern) -> ProjectDocument {
    ProjectDocument::new_v4(
        ProjectId::from_bytes([7; 16]),
        "export snapshot",
        42,
        pads,
        vec![selected],
        MasterMixSettings::default(),
        sampler_core::MidiSettings::default(),
    )
    .unwrap()
}

fn project_pad(pad: PadId, asset: (String, sampler_core::AssetDigest)) -> ProjectPad {
    ProjectPad::new(
        pad,
        asset.0,
        asset.1,
        PadSettings::default(),
        PadMixSettings::default(),
        SampleEditRecipe::identity(),
    )
    .unwrap()
}

fn save_snapshot(source: PathBuf, fingerprint: SourceFingerprint) -> ProjectSaveSnapshot {
    let slot = PatternSlotId::new(0).unwrap();
    ProjectSaveSnapshot {
        project_id: ProjectId::from_bytes([0x42; 16]),
        name: "loose export snapshot".to_owned(),
        revision: 7,
        master_mix: MasterMixSettings::default(),
        midi: sampler_core::MidiSettings::default(),
        pads: vec![ProjectSavePad {
            pad: pad(1),
            source_path: source,
            source_generation: 3,
            fingerprint,
            settings: PadSettings::default(),
            mix: PadMixSettings::default(),
            recipe: SampleEditRecipe::identity(),
        }],
        patterns: vec![pattern(slot, &[pad(1)])],
    }
}

#[test]
fn snapshot_owns_only_referenced_committed_pads_and_revision() {
    let fixture = Fixture::new("referenced");
    let first = project_pad(pad(1), fixture.add_wav(0.25));
    let seventh = project_pad(pad(7), fixture.add_wav(0.75));
    let unused = project_pad(pad(3), fixture.add_wav(0.5));
    let document = document(
        vec![first, seventh, unused],
        pattern(PatternSlotId::new(2).unwrap(), &[pad(1), pad(7)]),
    );

    let snapshot = OfflineExportSnapshot::from_document(
        &fixture.directory,
        &document,
        ExportPatternSlot::try_from(3).unwrap(),
    )
    .unwrap();

    assert_eq!(snapshot.project_id(), document.project_id);
    assert_eq!(snapshot.revision(), document.revision);
    assert_eq!(snapshot.sample_rate(), EXPORT_SAMPLE_RATE);
    assert_eq!(
        snapshot
            .pads()
            .iter()
            .map(|pad| pad.pad)
            .collect::<Vec<_>>(),
        vec![pad(1), pad(7)]
    );
    assert_eq!(snapshot.pattern().sample_rate, EXPORT_SAMPLE_RATE);
    assert_eq!(snapshot.loop_frames().unwrap(), 96_000);
    assert_eq!(snapshot.pattern().events[0].raw_frame, 24_000);
    assert_eq!(snapshot.pattern().events[0].event.frame, 24_000);
    assert_eq!(snapshot.pattern().events[0].event.duration, Some(24_000));
}

#[test]
fn snapshot_rejects_empty_or_unresolved_patterns() {
    let fixture = Fixture::new("rejected");
    let selected = PatternSlotId::new(0).unwrap();
    let empty = document(Vec::new(), pattern(selected, &[]));
    assert_eq!(
        OfflineExportSnapshot::from_document(
            &fixture.directory,
            &empty,
            ExportPatternSlot::try_from(1).unwrap(),
        ),
        Err(OfflineExportError::EmptyPattern)
    );

    let unresolved = document(Vec::new(), pattern(selected, &[pad(1)]));
    assert!(matches!(
        OfflineExportSnapshot::from_document(
            &fixture.directory,
            &unresolved,
            ExportPatternSlot::try_from(1).unwrap(),
        ),
        Err(OfflineExportError::MissingPadSource { .. })
    ));
}

#[test]
fn staging_resamples_referenced_pads_at_the_canonical_rate() {
    let fixture = Fixture::new("staged");
    let committed = project_pad(pad(1), fixture.add_wav(0.25));
    let document = document(
        vec![committed],
        pattern(PatternSlotId::new(0).unwrap(), &[pad(1)]),
    );
    let snapshot = OfflineExportSnapshot::from_document(
        &fixture.directory,
        &document,
        ExportPatternSlot::try_from(1).unwrap(),
    )
    .unwrap();

    let staged = stage_export_samples(&snapshot, &AtomicBool::new(false)).unwrap();

    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].pad, pad(1));
    assert_eq!(staged[0].sample.sample_rate(), EXPORT_SAMPLE_RATE);
}

#[test]
fn staging_rejects_mutated_assets_and_pre_decode_cancellation() {
    let fixture = Fixture::new("mutation");
    let committed = project_pad(pad(1), fixture.add_wav(0.25));
    let asset = fixture.directory.join(&committed.audio_path);
    let document = document(
        vec![committed],
        pattern(PatternSlotId::new(0).unwrap(), &[pad(1)]),
    );
    let snapshot = OfflineExportSnapshot::from_document(
        &fixture.directory,
        &document,
        ExportPatternSlot::try_from(1).unwrap(),
    )
    .unwrap();

    write_wav(&asset, 0.9);
    assert!(matches!(
        stage_export_samples(&snapshot, &AtomicBool::new(false)),
        Err(OfflineExportError::ProjectStore(
            sampler_tui::ProjectStoreError::AssetIntegrity { .. }
        ))
    ));
    assert!(matches!(
        stage_export_samples(&snapshot, &AtomicBool::new(true)),
        Err(OfflineExportError::Cancelled)
    ));
}

#[cfg(unix)]
#[test]
fn staging_rejects_a_symlink_substituted_after_snapshot() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new("symlink");
    let committed = project_pad(pad(1), fixture.add_wav(0.25));
    let asset = fixture.directory.join(&committed.audio_path);
    let document = document(
        vec![committed],
        pattern(PatternSlotId::new(0).unwrap(), &[pad(1)]),
    );
    let snapshot = OfflineExportSnapshot::from_document(
        &fixture.directory,
        &document,
        ExportPatternSlot::try_from(1).unwrap(),
    )
    .unwrap();
    let replacement = fixture.directory.join("replacement.wav");
    fs::copy(&asset, &replacement).unwrap();
    fs::remove_file(&asset).unwrap();
    symlink(&replacement, &asset).unwrap();
    assert!(matches!(
        stage_export_samples(&snapshot, &AtomicBool::new(false)),
        Err(OfflineExportError::ProjectStore(
            sampler_tui::ProjectStoreError::SymlinkRejected { .. }
        ))
    ));
}

#[cfg(unix)]
#[test]
fn document_snapshot_rejects_same_and_different_byte_project_ancestor_symlink_substitution() {
    use std::os::unix::fs::symlink;

    for same_bytes in [true, false] {
        let fixture = Fixture::new(if same_bytes {
            "document-ancestor-same"
        } else {
            "document-ancestor-different"
        });
        let committed = project_pad(pad(1), fixture.add_wav(0.25));
        let relative = committed.audio_path.clone();
        let document = document(
            vec![committed],
            pattern(PatternSlotId::new(0).unwrap(), &[pad(1)]),
        );
        let snapshot = OfflineExportSnapshot::from_document(
            &fixture.directory,
            &document,
            ExportPatternSlot::try_from(1).unwrap(),
        )
        .unwrap();
        let original = fixture.directory.with_extension("original");
        let attacker = fixture.directory.with_extension("attacker");
        fs::rename(&fixture.directory, &original).unwrap();
        fs::create_dir_all(attacker.join("audio")).unwrap();
        if same_bytes {
            fs::copy(original.join(&relative), attacker.join(&relative)).unwrap();
        } else {
            write_wav(&attacker.join(&relative), 0.9);
        }
        symlink(&attacker, &fixture.directory).unwrap();

        let result = stage_export_samples(&snapshot, &AtomicBool::new(false));

        fs::remove_file(&fixture.directory).unwrap();
        fs::rename(&original, &fixture.directory).unwrap();
        fs::remove_dir_all(&attacker).unwrap();
        assert!(
            matches!(
                result,
                Err(OfflineExportError::ProjectStore(
                    sampler_tui::ProjectStoreError::Filesystem { .. }
                        | sampler_tui::ProjectStoreError::SymlinkRejected { .. }
                ))
            ),
            "same_bytes={same_bytes}: {result:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn save_snapshot_rejects_same_and_different_byte_loose_ancestor_symlink_substitution() {
    use std::os::unix::fs::symlink;

    for same_bytes in [true, false] {
        let fixture = Fixture::new(if same_bytes {
            "save-ancestor-same"
        } else {
            "save-ancestor-different"
        });
        let source_parent = fixture.directory.join("loose");
        fs::create_dir(&source_parent).unwrap();
        let source = source_parent.join("source.wav");
        write_wav(&source, 0.5);
        let fingerprint = SourceFingerprint::from_path(&source).unwrap();
        let project = save_snapshot(source.clone(), fingerprint);
        let snapshot = OfflineExportSnapshot::from_save_snapshot(
            &fixture.directory,
            &project,
            ExportPatternSlot::try_from(1).unwrap(),
        )
        .unwrap();
        let original = fixture.directory.join("loose-original");
        let attacker = fixture.directory.join("loose-attacker");
        fs::rename(&source_parent, &original).unwrap();
        fs::create_dir(&attacker).unwrap();
        if same_bytes {
            fs::copy(original.join("source.wav"), attacker.join("source.wav")).unwrap();
        } else {
            write_wav(&attacker.join("source.wav"), 0.9);
        }
        symlink(&attacker, &source_parent).unwrap();

        let result = stage_export_samples(&snapshot, &AtomicBool::new(false));

        fs::remove_file(&source_parent).unwrap();
        fs::rename(&original, &source_parent).unwrap();
        fs::remove_dir_all(&attacker).unwrap();
        assert!(
            matches!(
                result,
                Err(OfflineExportError::ProjectStore(
                    sampler_tui::ProjectStoreError::Filesystem { .. }
                ))
            ),
            "same_bytes={same_bytes}: {result:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn save_snapshot_with_relative_project_directory_rejects_project_audio_symlink() {
    use std::os::unix::fs::symlink;

    let name = format!(
        ".sampler-tui-export-relative-project-{}",
        std::process::id()
    );
    let relative_directory = PathBuf::from(&name);
    let directory = std::env::current_dir().unwrap().join(&relative_directory);
    let attacker = std::env::temp_dir().join(format!("{name}-attacker"));
    let _ = fs::remove_dir_all(&directory);
    let _ = fs::remove_dir_all(&attacker);
    fs::create_dir(&directory).unwrap();
    fs::create_dir(&attacker).unwrap();
    let source = attacker.join("source.wav");
    write_wav(&source, 0.5);
    symlink(&attacker, directory.join("audio")).unwrap();
    let project_source = directory.join("audio/source.wav");
    let fingerprint = SourceFingerprint::from_path(&project_source).unwrap();
    let project = save_snapshot(project_source, fingerprint);

    let result = OfflineExportSnapshot::from_save_snapshot(
        &relative_directory,
        &project,
        ExportPatternSlot::try_from(1).unwrap(),
    );

    fs::remove_file(directory.join("audio")).unwrap();
    fs::remove_dir(&directory).unwrap();
    fs::remove_dir_all(&attacker).unwrap();
    assert!(
        matches!(
            result,
            Err(OfflineExportError::ProjectStore(
                sampler_tui::ProjectStoreError::SymlinkRejected { .. }
                    | sampler_tui::ProjectStoreError::Filesystem { .. }
            ))
        ),
        "{result:?}"
    );
}

#[test]
fn migrated_v1_v2_and_v3_patterns_snapshot_at_48khz() {
    let fixture = Fixture::new("legacy-rates");
    let (asset_path, digest) = fixture.add_wav(0.25);
    let asset = fixture.directory.join(&asset_path);
    let digest = digest.to_string();
    let legacy = fixture.directory.join("audio/legacy.wav");
    fs::copy(&asset, &legacy).unwrap();
    fs::write(
        fixture.directory.join("project.toml"),
        r#"schema_version = 1
name = "legacy"

[[pads]]
audio_path = "audio/legacy.wav"

[pads.pad]
bank = 0
index = 1

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0

[[patterns]]
name = "legacy pattern"
sample_rate = 44100
tempo = 120.0
bars = 1
resolution = "sixteenth"
swing = 0.5

[patterns.meter]
numerator = 4
denominator = 4

[[patterns.events]]
id = 1
frame = 22050
velocity = 1.0
duration = 22050

[patterns.events.pad]
bank = 0
index = 1
"#,
    )
    .unwrap();
    let v1 = ProjectStore
        .probe(&fixture.directory)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();

    let v2 = current_document_from_legacy_toml(2, &asset_path, &digest);
    let v3 = current_document_from_legacy_toml(3, &asset_path, &digest);
    for document in [v1, v2, v3] {
        let snapshot = OfflineExportSnapshot::from_document(
            &fixture.directory,
            &document,
            ExportPatternSlot::try_from(1).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot.sample_rate(), EXPORT_SAMPLE_RATE);
        assert_eq!(snapshot.pattern().sample_rate, EXPORT_SAMPLE_RATE);
        assert_eq!(snapshot.loop_frames().unwrap(), 96_000);
        assert_eq!(snapshot.pattern().events[0].raw_frame, 24_000);
        assert_eq!(snapshot.pattern().events[0].event.frame, 24_000);
        assert_eq!(snapshot.pattern().events[0].event.duration, Some(24_000));
    }
}

fn current_document_from_legacy_toml(
    schema: u8,
    asset_path: &str,
    digest: &str,
) -> ProjectDocument {
    let mix = (schema == 3).then_some(
        r#"
[master_mix]
gain_db = 0.0

[master_mix.delay]
enabled = false
time_ms = 320
feedback = 0.0
return_db = 0.0

[master_mix.reverb]
enabled = false
room_size = 0.5
damping = 0.5
return_db = 0.0
"#,
    );
    let pad_mix = (schema == 3).then_some(
        r#"
[pads.mix]
muted = false
delay_send = 0.0
reverb_send = 0.0
"#,
    );
    let source = format!(
        r#"schema_version = {schema}
project_id = "07070707070707070707070707070707"
name = "legacy current"
revision = 4
{mix}
[[pads]]
audio_path = "{asset_path}"
asset_digest = "{digest}"

[pads.pad]
bank = 0
index = 1

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0
{pad_mix}
[pads.recipe]
start_phase = 0
end_phase = 4294967296
reversed = false
normalize = false

[[patterns]]
slot = 0
name = "legacy current pattern"
sample_rate = 44100
tempo = 120.0
bars = 1
resolution = "sixteenth"
swing = 0.5
quantize_strength = 0.0

[patterns.meter]
numerator = 4
denominator = 4

[[patterns.events]]
id = 1
frame = 22050
raw_frame = 22050
velocity = 1.0
duration = 22050
original_offset = 0

[patterns.events.pad]
bank = 0
index = 1
"#,
        mix = mix.unwrap_or_default(),
        pad_mix = pad_mix.unwrap_or_default(),
    );
    let sampler_core::ParsedProjectDocument::Current(document) =
        ProjectDocument::from_toml(&source).unwrap()
    else {
        panic!("schema v{schema} must migrate to a current document");
    };
    document
}
