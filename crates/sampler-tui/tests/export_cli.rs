use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hound::{SampleFormat, WavSpec, WavWriter};
use sampler_audio::{AudioEngine, PatternSwitch, audio_channels};
use sampler_core::{
    EditablePattern, MasterMixSettings, Meter, MidiSettings, PadMixSettings, PadSettings,
    PatternSlotId, ProjectId, ProjectPattern, Resolution, SampleEditRecipe, Tempo, Transport,
};
use sampler_tui::cli::{
    CliOutcome, CliStartupFactories, TuiStartup, dispatch_args_os_with_startup, parse_args_os,
};
use sampler_tui::export::{StagedExportPad, stage_export_samples};
use sampler_tui::headless_export;
use sampler_tui::{
    AtomicWavPublisher, EXPORT_CHUNK_FRAMES, EXPORT_SAMPLE_RATE, ExportPatternSlot, ExportToken,
    OfflineFrameSink, OfflineRenderSummary, ProjectSavePad, ProjectSaveRequest,
    ProjectSaveSnapshot, ProjectStore, SaveKind, SourceFingerprint,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sampler-tui-export-cli-{name}-{}-{id}",
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

fn write_wav(path: &Path) {
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
    for index in 0..512 {
        let sample = ((index as f32 * 0.071).sin() * 0.7).clamp(-1.0, 1.0);
        writer.write_sample(sample).unwrap();
        writer.write_sample(-sample * 0.5).unwrap();
    }
    writer.finalize().unwrap();
}

fn pattern(slot: u8) -> ProjectPattern {
    let slot = PatternSlotId::new(slot).unwrap();
    let transport = Transport::new(
        44_100,
        Tempo::new(240.0).unwrap(),
        Meter::new(1, 4).unwrap(),
        1,
        Resolution::Sixteenth,
    )
    .unwrap();
    let mut editable =
        EditablePattern::new(slot, format!("Pattern {}", slot.get() + 1), transport).unwrap();
    editable
        .insert_new(sampler_core::PadId::first(), 341, 0.73, None)
        .unwrap();
    ProjectPattern::from_editable(&editable).unwrap()
}

fn save_project(
    fixture: &Fixture,
    directory: &Path,
    revision: u64,
    kind: SaveKind,
    slots: &[u8],
) -> sampler_tui::SaveReceipt {
    fs::create_dir_all(directory).unwrap();
    let source = fixture.path(&format!("source-{revision}-{}.wav", slots[0]));
    if !source.exists() {
        write_wav(&source);
    }
    let fingerprint = SourceFingerprint::from_path(&source).unwrap();
    ProjectStore
        .save(ProjectSaveRequest {
            directory: directory.to_path_buf(),
            save_as: false,
            kind,
            snapshot: ProjectSaveSnapshot {
                project_id: ProjectId::from_bytes([0x71; 16]),
                name: "headless export fixture".to_owned(),
                revision,
                master_mix: MasterMixSettings::default(),
                midi: MidiSettings::default(),
                pads: vec![ProjectSavePad {
                    pad: sampler_core::PadId::first(),
                    source_path: source,
                    source_generation: 1,
                    fingerprint,
                    settings: PadSettings::default(),
                    mix: PadMixSettings::default(),
                    recipe: SampleEditRecipe::identity(),
                }],
                patterns: slots.iter().copied().map(pattern).collect(),
            },
        })
        .unwrap()
}

fn render_with_independent_engine(project: &Path, slot: u8, destination: &Path) {
    let probe = ProjectStore.probe(project).unwrap();
    let document = probe.explicit.unwrap().unwrap();
    let snapshot = sampler_tui::OfflineExportSnapshot::from_document(
        &probe.directory,
        &document,
        ExportPatternSlot::try_from(slot).unwrap(),
    )
    .unwrap();
    let staged = stage_export_samples(&snapshot, &AtomicBool::new(false)).unwrap();
    let (_controller, mut engine) = independent_engine(&snapshot, &staged);
    let mut frames = Vec::with_capacity(snapshot.loop_frames().unwrap() as usize);
    engine.render_frames(snapshot.loop_frames().unwrap() as usize, |frame| {
        frames.push(frame)
    });
    let mut publisher = AtomicWavPublisher::prepare(destination).unwrap();
    for chunk in frames.chunks(EXPORT_CHUNK_FRAMES) {
        publisher.write_frames(chunk).unwrap();
    }
    publisher
        .publish(
            ExportToken::new(99),
            &snapshot,
            OfflineRenderSummary {
                frame_count: frames.len() as u64,
                peak: [0.0, 0.0],
            },
            &AtomicBool::new(false),
        )
        .unwrap();
}

fn independent_engine(
    snapshot: &sampler_tui::OfflineExportSnapshot,
    staged: &[StagedExportPad],
) -> (sampler_audio::AudioController, AudioEngine) {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(EXPORT_SAMPLE_RATE, ports).unwrap();
    for pad in staged {
        controller
            .install(pad.pad, Arc::clone(&pad.sample), pad.settings, pad.mix)
            .unwrap();
        engine.render_frames(0, |_| {});
    }
    controller.update_master_mix(snapshot.master_mix()).unwrap();
    engine.render_frames(0, |_| {});
    controller
        .install_pattern(Arc::new(
            snapshot.pattern().to_editable().unwrap().compile().unwrap(),
        ))
        .unwrap();
    controller
        .select_pattern(snapshot.slot(), PatternSwitch::Immediate)
        .unwrap();
    controller.play_pattern().unwrap();
    engine.render_frames(0, |_| {});
    (controller, engine)
}

#[derive(Default)]
struct StartupCalls {
    terminal: Rc<Cell<usize>>,
    keyboard: Rc<Cell<usize>>,
    midi: Rc<Cell<usize>>,
    audio_input: Rc<Cell<usize>>,
    audio_output: Rc<Cell<usize>>,
}

impl StartupCalls {
    fn assert_all(&self, expected: usize) {
        assert_eq!(self.terminal.get(), expected);
        assert_eq!(self.keyboard.get(), expected);
        assert_eq!(self.midi.get(), expected);
        assert_eq!(self.audio_input.get(), expected);
        assert_eq!(self.audio_output.get(), expected);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct StartupMarker(&'static str);

fn counted_factory(
    counter: Rc<Cell<usize>>,
    marker: &'static str,
) -> impl FnOnce() -> StartupMarker {
    move || {
        counter.set(counter.get() + 1);
        StartupMarker(marker)
    }
}

fn startup_factories(
    calls: &Rc<StartupCalls>,
) -> CliStartupFactories<
    impl FnOnce() -> StartupMarker,
    impl FnOnce() -> StartupMarker,
    impl FnOnce() -> StartupMarker,
    impl FnOnce() -> StartupMarker,
    impl FnOnce() -> StartupMarker,
> {
    CliStartupFactories::new(
        counted_factory(Rc::clone(&calls.terminal), "terminal"),
        counted_factory(Rc::clone(&calls.keyboard), "keyboard"),
        counted_factory(Rc::clone(&calls.midi), "midi"),
        counted_factory(Rc::clone(&calls.audio_input), "audio-input"),
        counted_factory(Rc::clone(&calls.audio_output), "audio-output"),
    )
}

#[test]
fn moved_real_project_exports_byte_equal_without_any_tui_or_device_startup() {
    let fixture = Fixture::new("moved-no-devices");
    let original = fixture.path("original-project");
    save_project(&fixture, &original, 11, SaveKind::Explicit, &[0, 15]);
    let moved = fixture.path("moved-project");
    fs::rename(&original, &moved).unwrap();
    let destination = fixture.path("headless.wav");
    let reference = fixture.path("reference.wav");
    let calls = Rc::new(StartupCalls::default());

    let outcome = dispatch_args_os_with_startup(
        [
            std::ffi::OsString::from("sampler-tui"),
            std::ffi::OsString::from("export"),
            moved.clone().into_os_string(),
            std::ffi::OsString::from("1"),
            destination.clone().into_os_string(),
        ]
        .into_iter(),
        startup_factories(&calls),
        |_, _: TuiStartup<_, _, _, _, _>| panic!("headless export entered the TUI startup path"),
        |_| panic!("headless export entered diagnostic playback"),
        headless_export::run,
    )
    .unwrap();

    let CliOutcome::Export(receipt) = outcome else {
        panic!("export command returned a non-export outcome")
    };
    calls.assert_all(0);
    assert_eq!(receipt.destination, destination);
    assert_eq!(receipt.revision, 11);
    render_with_independent_engine(&moved, 1, &reference);
    assert_eq!(
        fs::read(receipt.destination).unwrap(),
        fs::read(reference).unwrap()
    );
}

#[test]
fn tui_control_invokes_each_distinct_production_startup_factory_seam_once() {
    let calls = Rc::new(StartupCalls::default());
    let outcome = dispatch_args_os_with_startup(
        ["sampler-tui"].into_iter().map(std::ffi::OsString::from),
        startup_factories(&calls),
        |initial_project, startup| {
            assert_eq!(initial_project, None);
            assert_eq!(startup.terminal, StartupMarker("terminal"));
            assert_eq!(startup.keyboard, StartupMarker("keyboard"));
            assert_eq!(startup.midi, StartupMarker("midi"));
            assert_eq!(startup.audio_input, StartupMarker("audio-input"));
            assert_eq!(startup.audio_output, StartupMarker("audio-output"));
            Ok(())
        },
        |_| panic!("TUI command entered diagnostic playback"),
        |_, _, _| panic!("TUI command entered headless export"),
    )
    .unwrap();

    assert_eq!(outcome, CliOutcome::Silent);
    calls.assert_all(1);
}

fn run_export_binary(project: &Path, slot: &str, destination: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sampler-tui"))
        .arg("export")
        .arg(project)
        .arg(slot)
        .arg(destination)
        .output()
        .unwrap()
}

fn run_panicking_export_binary(project: &Path, destination: &Path, checkpoint: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sampler-tui"))
        .arg("export")
        .arg(project)
        .arg("1")
        .arg(destination)
        .env("SAMPLER_TUI_TEST_HEADLESS_PANIC", checkpoint)
        .output()
        .unwrap()
}

fn temporary_export_entries(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".sampler-tui-tmp-"))
        })
        .collect()
}

#[test]
fn production_binary_contains_hostile_panics_across_the_complete_headless_pipeline() {
    let fixture = Fixture::new("contained-panics");
    let project = fixture.path("project");
    save_project(&fixture, &project, 31, SaveKind::Explicit, &[0]);

    for checkpoint in ["before-probe", "after-prepare", "after-link"] {
        let destination = fixture.path(&format!("panic-{checkpoint}.wav"));
        let output = run_panicking_export_binary(&project, &destination, checkpoint);
        assert_eq!(output.status.code(), Some(1), "checkpoint={checkpoint}");
        assert!(output.stdout.is_empty(), "checkpoint={checkpoint}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "sampler-tui: offline pattern export failed\n  caused by: offline export request panicked\n",
            "checkpoint={checkpoint}"
        );
        assert!(!destination.exists(), "checkpoint={checkpoint}");
        assert!(
            temporary_export_entries(&fixture.root).is_empty(),
            "checkpoint={checkpoint}"
        );
    }
}

#[test]
fn cli_exports_schema_v1_v2_v3_and_v4_projects() {
    let fixture = Fixture::new("schema-matrix");
    for schema in 1..=4 {
        let project = fixture.path(&format!("schema-v{schema}"));
        if schema == 4 {
            save_project(&fixture, &project, 4, SaveKind::Explicit, &[0]);
        } else {
            write_legacy_project(&project, schema);
        }
        let destination = fixture.path(&format!("schema-v{schema}.wav"));
        let output = run_export_binary(&project, "1", &destination);
        assert!(
            output.status.success(),
            "schema v{schema}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(destination.is_file());
        let reader = hound::WavReader::open(destination).unwrap();
        assert_eq!(reader.spec().sample_rate, EXPORT_SAMPLE_RATE);
        assert_eq!(reader.spec().sample_format, SampleFormat::Float);
    }
}

#[test]
fn cli_accepts_boundary_slots_and_prints_the_exact_success_receipt() {
    let fixture = Fixture::new("slot-boundaries");
    let project = fixture.path("project");
    save_project(&fixture, &project, 27, SaveKind::Explicit, &[0, 15]);

    for slot in [1_u8, 16] {
        let destination = fixture.path(&format!("slot-{slot}.wav"));
        let output = run_export_binary(&project, &slot.to_string(), &destination);
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "exported {} pattern={slot} rate=48000 frames=12000 revision=27\n",
                destination.display()
            )
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn cli_rejects_slots_zero_and_seventeen_as_usage_errors() {
    let fixture = Fixture::new("invalid-slots");
    let project = fixture.path("project");
    save_project(&fixture, &project, 1, SaveKind::Explicit, &[0]);
    for slot in ["0", "17"] {
        let destination = fixture.path(&format!("invalid-{slot}.wav"));
        let output = run_export_binary(&project, slot, &destination);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("Usage:\n"));
        assert!(!destination.exists());
    }
}

#[test]
fn missing_assets_and_destination_collisions_fail_without_partial_publication() {
    let fixture = Fixture::new("ordinary-errors");
    let collision_project = fixture.path("collision-project");
    save_project(&fixture, &collision_project, 2, SaveKind::Explicit, &[0]);
    let collision = fixture.path("collision.wav");
    fs::write(&collision, b"existing destination").unwrap();
    let output = run_export_binary(&collision_project, "1", &collision);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read(&collision).unwrap(), b"existing destination");
    assert!(String::from_utf8_lossy(&output.stderr).contains("destination already exists"));

    let missing_project = fixture.path("missing-project");
    save_project(&fixture, &missing_project, 3, SaveKind::Explicit, &[0]);
    let document = ProjectStore
        .probe(&missing_project)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();
    fs::remove_file(missing_project.join(&document.pads[0].audio_path)).unwrap();
    let missing_destination = fixture.path("missing.wav");
    let output = run_export_binary(&missing_project, "1", &missing_destination);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("sampler-tui: could not load explicit project document"));
    assert!(
        stderr.contains("\n  caused by: could not read source"),
        "{stderr}"
    );
    assert!(!missing_destination.exists());
}

#[test]
fn newer_or_ambiguous_recovery_is_never_chosen_silently() {
    let fixture = Fixture::new("recovery-choice");

    let newer = fixture.path("newer");
    save_project(&fixture, &newer, 7, SaveKind::Explicit, &[0]);
    save_project(&fixture, &newer, 8, SaveKind::Recovery, &[0]);
    let newer_destination = fixture.path("newer.wav");
    let output = run_export_binary(&newer, "1", &newer_destination);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("newer recovery revision 8"));
    assert!(!newer_destination.exists());

    let recovery_only = fixture.path("recovery-only");
    save_project(&fixture, &recovery_only, 3, SaveKind::Explicit, &[0]);
    save_project(&fixture, &recovery_only, 4, SaveKind::Recovery, &[0]);
    fs::remove_file(recovery_only.join("project.toml")).unwrap();
    let recovery_only_destination = fixture.path("recovery-only.wav");
    let output = run_export_binary(&recovery_only, "1", &recovery_only_destination);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no explicit saved document"));
    assert!(!recovery_only_destination.exists());

    let invalid_recovery = fixture.path("invalid-recovery");
    save_project(&fixture, &invalid_recovery, 5, SaveKind::Explicit, &[0]);
    fs::write(
        invalid_recovery.join(".sampler-tui-recovery.toml"),
        "not valid toml",
    )
    .unwrap();
    let invalid_destination = fixture.path("invalid-recovery.wav");
    let output = run_export_binary(&invalid_recovery, "1", &invalid_destination);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not load recovery document"));
    assert!(!invalid_destination.exists());

    let invalid_explicit = fixture.path("invalid-explicit");
    save_project(&fixture, &invalid_explicit, 5, SaveKind::Explicit, &[0]);
    save_project(&fixture, &invalid_explicit, 6, SaveKind::Recovery, &[0]);
    fs::write(invalid_explicit.join("project.toml"), "not valid toml").unwrap();
    let invalid_explicit_destination = fixture.path("invalid-explicit.wav");
    let output = run_export_binary(&invalid_explicit, "1", &invalid_explicit_destination);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("could not load explicit project document")
    );
    assert!(!invalid_explicit_destination.exists());

    let mismatched = fixture.path("mismatched-recovery");
    save_project(&fixture, &mismatched, 9, SaveKind::Explicit, &[0]);
    save_project(&fixture, &mismatched, 10, SaveKind::Recovery, &[0]);
    let recovery_path = mismatched.join(".sampler-tui-recovery.toml");
    let foreign = fs::read_to_string(&recovery_path).unwrap().replace(
        "71717171717171717171717171717171",
        "72727272727272727272727272727272",
    );
    fs::write(&recovery_path, foreign).unwrap();
    let mismatched_destination = fixture.path("mismatched-recovery.wav");
    let output = run_export_binary(&mismatched, "1", &mismatched_destination);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("recovery document belongs to a different project")
    );
    assert!(!mismatched_destination.exists());
}

#[test]
fn equal_and_older_same_project_recovery_leave_bytes_and_mtime_untouched_and_export_explicit() {
    let fixture = Fixture::new("non-newer-recovery");
    for (name, explicit_revision, recovery_revision) in
        [("equal", 41_u64, 41_u64), ("older", 43_u64, 42_u64)]
    {
        let project = fixture.path(&format!("{name}-project"));
        save_project(
            &fixture,
            &project,
            explicit_revision,
            SaveKind::Explicit,
            &[0],
        );
        save_project(
            &fixture,
            &project,
            recovery_revision,
            SaveKind::Recovery,
            &[0],
        );
        let recovery_path = project.join(".sampler-tui-recovery.toml");
        let distinct_recovery = fs::read_to_string(&recovery_path)
            .unwrap()
            .replace("velocity = 0.73", "velocity = 0.25");
        fs::write(&recovery_path, distinct_recovery).unwrap();
        let recovery_before = fs::read(&recovery_path).unwrap();
        let modified_before = fs::metadata(&recovery_path).unwrap().modified().unwrap();
        let destination = fixture.path(&format!("{name}.wav"));
        let reference = fixture.path(&format!("{name}-explicit-reference.wav"));

        let output = run_export_binary(&project, "1", &destination);

        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains(&format!("revision={explicit_revision}"))
        );
        render_with_independent_engine(&project, 1, &reference);
        assert_eq!(
            fs::read(&destination).unwrap(),
            fs::read(reference).unwrap()
        );
        assert_eq!(fs::read(&recovery_path).unwrap(), recovery_before);
        assert_eq!(
            fs::metadata(&recovery_path).unwrap().modified().unwrap(),
            modified_before
        );
    }
}

#[test]
fn cli_preserves_the_complete_nested_error_chain_and_nonzero_exit() {
    let fixture = Fixture::new("error-chain");
    let missing_project = fixture.path("does-not-exist");
    let destination = fixture.path("never.wav");
    let output = run_export_binary(&missing_project, "1", &destination);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let lines = stderr.lines().collect::<Vec<_>>();
    assert!(lines[0].starts_with("sampler-tui: could not probe project directory"));
    assert!(lines[1].starts_with("  caused by: filesystem operation open no-follow path failed"));
    assert!(!destination.exists());
}

#[test]
fn parser_rejects_non_numeric_and_out_of_range_slots_before_any_runtime_call() {
    for slot in ["0", "17", "one"] {
        let parsed = parse_args_os(
            ["sampler-tui", "export", "project", slot, "mix.wav"]
                .into_iter()
                .map(std::ffi::OsString::from),
        );
        assert!(parsed.is_err());
    }
}

fn write_legacy_project(directory: &Path, schema: u8) {
    fs::create_dir_all(directory.join("audio")).unwrap();
    let asset = directory.join("audio/legacy.wav");
    write_wav(&asset);
    if schema == 1 {
        fs::write(
            directory.join("project.toml"),
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
tempo = 240.0
bars = 1
resolution = "sixteenth"
swing = 0.5

[patterns.meter]
numerator = 1
denominator = 4

[[patterns.events]]
id = 1
frame = 341
velocity = 0.73

[patterns.events.pad]
bank = 0
index = 1
"#,
        )
        .unwrap();
        return;
    }

    let fingerprint = SourceFingerprint::from_path(&asset).unwrap();
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
    let digest = fingerprint.digest.to_string();
    assert_eq!(digest.len(), 64);
    let asset_path = format!("audio/{digest}.wav");
    fs::rename(&asset, directory.join(&asset_path)).unwrap();
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
    fs::write(directory.join("project.toml"), source).unwrap();
}
