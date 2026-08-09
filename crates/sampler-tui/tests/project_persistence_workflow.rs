//! Cross-layer persistence evidence using the real filesystem worker and audio engine. Physical
//! device I/O is the only substituted boundary.

#[path = "support/mixer_harness.rs"]
mod mixer_harness;

use std::fs;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use sampler_audio::ControlError;
use sampler_core::{
    BankId, ChokeGroup, DelaySettings, MasterMixSettings, PadId, PadMixSettings, PadSettings,
    PatternSlotId, PlaybackMode, ReverbSettings, SAMPLE_PHASE_SCALE, SampleEditRecipe,
};
use sampler_tui::{InputAction, ProjectOpenPhase, ProjectStore, RecoveryChoice, WorkerHandle};

use mixer_harness::{FixtureTree, Harness};

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

fn explicit_pad_mix() -> PadMixSettings {
    PadMixSettings::new(true, 0.25, 0.75).unwrap()
}

fn explicit_master_mix() -> MasterMixSettings {
    MasterMixSettings::new(
        -6.0,
        DelaySettings::new(true, 320, 0.5, -9.0).unwrap(),
        ReverbSettings::new(true, 0.75, 0.25, -8.0).unwrap(),
    )
    .unwrap()
}

fn recovery_pad_mix() -> PadMixSettings {
    PadMixSettings::new(false, 0.875, 0.125).unwrap()
}

fn recovery_master_mix() -> MasterMixSettings {
    MasterMixSettings::new(
        3.0,
        DelaySettings::new(true, 95, 0.75, -4.0).unwrap(),
        ReverbSettings::new(true, 0.9, 0.6, -3.0).unwrap(),
    )
    .unwrap()
}

fn render_live_hit_bits(harness: &mut Harness, pad: PadId) -> Vec<[u32; 2]> {
    harness
        .app
        .apply(InputAction::PadPress(pad.index() as usize));
    let mut frames = Vec::new();
    harness.engine.render_frames(65, |frame| {
        frames.push([frame[0].to_bits(), frame[1].to_bits()]);
    });
    frames
}

fn copy_project(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    fs::copy(
        source.join("project.toml"),
        destination.join("project.toml"),
    )
    .unwrap();
    fs::create_dir(destination.join("audio")).unwrap();
    for entry in fs::read_dir(source.join("audio")).unwrap() {
        let entry = entry.unwrap();
        fs::copy(
            entry.path(),
            destination.join("audio").join(entry.file_name()),
        )
        .unwrap();
    }
}

fn assert_failed_open_preserves(harness: &mut Harness, directory: &Path, now: Instant) {
    let before = harness.app.project_snapshot().unwrap();
    harness.app.request_open_project(directory).unwrap();
    assert_eq!(harness.dispatch_queued(), 1);
    for _ in 0..32 {
        if harness.app.project_open_stage().is_none() {
            break;
        }
        harness.app.maintain_project(now);
        harness.dispatch_queued();
    }
    assert!(harness.app.project_open_error().is_some());
    assert_eq!(harness.app.project_snapshot().unwrap(), before);
}

#[test]
fn mixer_save_move_and_fresh_open_preserve_the_portable_project_tuple() {
    let fixture = FixtureTree::new();
    let wav = fixture.write_wav("asymmetric.wav");
    let flac = fixture.write_flac("asymmetric.flac");
    let wav_before = fs::read(&wav).unwrap();
    let flac_before = fs::read(&flac).unwrap();
    let project = fixture.path("portable-project");
    let moved = fixture.path("renamed-project");
    let now = Instant::now();

    let mut source = Harness::new();
    source.load(pad(0), &wav);
    source.load(pad(7), &flac);
    let recipe_a = SampleEditRecipe::new(
        SAMPLE_PHASE_SCALE / 4,
        SAMPLE_PHASE_SCALE * 3 / 4,
        true,
        true,
    )
    .unwrap();
    let recipe_b = SampleEditRecipe::new(0, SAMPLE_PHASE_SCALE / 2, false, true).unwrap();
    source.edit(pad(0), recipe_a);
    source.edit(pad(7), recipe_b);
    let rendered_a = source.app.pad(pad(0)).sample.as_ref().unwrap();
    let rendered_b = source.app.pad(pad(7)).sample.as_ref().unwrap();
    assert_ne!(
        rendered_a.data(),
        source.app.base_sample(pad(0)).unwrap().data(),
        "the WAV evidence must be nonidentity rendered PCM"
    );
    assert_ne!(
        rendered_b.data(),
        source.app.base_sample(pad(7)).unwrap().data(),
        "the FLAC evidence must be nonidentity rendered PCM"
    );
    let rendered_a_data = rendered_a.data().to_vec();
    let rendered_b_data = rendered_b.data().to_vec();
    let rendered_a_endpoints = [
        [rendered_a_data[0], rendered_a_data[1]],
        [
            rendered_a_data[rendered_a_data.len() - 2],
            rendered_a_data[rendered_a_data.len() - 1],
        ],
    ];
    let rendered_b_endpoints = [
        [rendered_b_data[0], rendered_b_data[1]],
        [
            rendered_b_data[rendered_b_data.len() - 2],
            rendered_b_data[rendered_b_data.len() - 1],
        ],
    ];
    let preview_a = source.app.pad(pad(0)).preview;
    let preview_b = source.app.pad(pad(7)).preview;
    let settings_a = PadSettings::new(
        PlaybackMode::Gate,
        -3.0,
        -0.25,
        -7.0,
        Some(ChokeGroup::new(2).unwrap()),
    )
    .unwrap();
    let settings_b = PadSettings::new(
        PlaybackMode::Loop,
        -6.0,
        0.5,
        12.0,
        Some(ChokeGroup::new(7).unwrap()),
    )
    .unwrap();
    let mix_a = explicit_pad_mix();
    let mix_b = PadMixSettings::new(false, 0.625, 0.375).unwrap();
    let master_mix = explicit_master_mix();
    let mut settings_a_before_palette = settings_a;
    settings_a_before_palette.choke_group = None;
    source
        .app
        .update_pad_settings(pad(0), settings_a_before_palette)
        .unwrap();
    source.app.update_pad_settings(pad(7), settings_b).unwrap();
    source.palette("select 1");
    source.palette("pad-choke 2");
    source.palette("pad-mute on");
    source.palette("delay-send 0.25");
    source.palette("reverb-send 0.75");
    assert_eq!(source.app.pad_mix(pad(0)), mix_a);
    assert_eq!(
        source.app.pad(pad(0)).settings.choke_group,
        settings_a.choke_group
    );
    source.app.update_pad_mix(pad(7), mix_b).unwrap();
    source.palette("master-level -6");
    source.palette("delay-enable on");
    source.palette("delay-time 320");
    source.palette("delay-feedback 0.5");
    source.palette("delay-return -9");
    source.palette("reverb-enable on");
    source.palette("reverb-room 0.75");
    source.palette("reverb-damping 0.25");
    source.palette("reverb-return -8");
    assert_eq!(source.app.master_mix(), master_mix);
    source.palette("pattern 1");
    source.palette("tempo 137");
    source.palette("swing 63");
    source.palette("quantize 80");
    source.record_hit(0);
    source.palette("pattern 9");
    source.palette("tempo 91");
    source.palette("bars 2");
    source.palette("resolution 1/32");
    source.record_hit(7);
    let editable_before_save = source.app.project_snapshot().unwrap();
    let fingerprint_a = editable_before_save
        .pads
        .iter()
        .find(|saved_pad| saved_pad.pad == pad(0))
        .unwrap()
        .fingerprint;
    let fingerprint_b = editable_before_save
        .pads
        .iter()
        .find(|saved_pad| saved_pad.pad == pad(7))
        .unwrap()
        .fingerprint;

    source.save_as(&project, now);
    let saved_snapshot = source.app.project_snapshot().unwrap();
    let explicit_bytes = fs::read(project.join("project.toml")).unwrap();
    let explicit_text = std::str::from_utf8(&explicit_bytes).unwrap();
    assert!(explicit_text.contains("schema_version = 3"));
    let probe = ProjectStore.probe(&project).unwrap();
    let document = probe.explicit.unwrap().unwrap();
    assert_eq!(document.pads.len(), 2);
    assert_eq!(document.patterns.len(), 16);
    assert_eq!(document.master_mix, master_mix);
    assert_eq!(
        document
            .pads
            .iter()
            .find(|saved_pad| saved_pad.pad == pad(0))
            .unwrap()
            .mix,
        mix_a
    );
    assert_eq!(
        document
            .pads
            .iter()
            .find(|saved_pad| saved_pad.pad == pad(7))
            .unwrap()
            .mix,
        mix_b
    );
    for saved_pad in &document.pads {
        assert!(saved_pad.audio_path.starts_with("audio/"));
        assert!(
            saved_pad
                .audio_path
                .contains(&saved_pad.asset_digest.to_string())
        );
        assert!(project.join(&saved_pad.audio_path).is_file());
    }
    assert!(document.patterns[0].events.iter().all(|event| {
        event.raw_frame
            <= document.patterns[0]
                .to_editable()
                .unwrap()
                .transport()
                .loop_frames()
    }));
    assert!(document.patterns[8].events.iter().all(|event| {
        event.raw_frame
            <= document.patterns[8]
                .to_editable()
                .unwrap()
                .transport()
                .loop_frames()
    }));
    assert_eq!(fs::read(&wav).unwrap(), wav_before);
    assert_eq!(fs::read(&flac).unwrap(), flac_before);

    fs::rename(&project, &moved).unwrap();
    let mut reopened = Harness::new();
    reopened.open(&moved, None, now);
    let after_open = reopened.app.project_snapshot().unwrap();
    assert_eq!(after_open.project_id, saved_snapshot.project_id);
    assert_eq!(after_open.pads.len(), 2);
    assert_eq!(after_open.patterns, editable_before_save.patterns);
    assert_eq!(reopened.app.pad(pad(0)).settings, settings_a);
    assert_eq!(reopened.app.pad(pad(7)).settings, settings_b);
    assert_eq!(reopened.app.pad_mix(pad(0)), mix_a);
    assert_eq!(reopened.app.pad_mix(pad(7)), mix_b);
    assert_eq!(reopened.app.master_mix(), master_mix);
    assert_eq!(after_open.master_mix, master_mix);
    assert_eq!(reopened.app.committed_sample_recipe(pad(0)), Some(recipe_a));
    assert_eq!(reopened.app.committed_sample_recipe(pad(7)), Some(recipe_b));
    let reopened_a = reopened.app.pad(pad(0)).sample.as_ref().unwrap();
    let reopened_b = reopened.app.pad(pad(7)).sample.as_ref().unwrap();
    assert_eq!(reopened_a.data(), rendered_a_data);
    assert_eq!(reopened_b.data(), rendered_b_data);
    assert_eq!(
        [
            [reopened_a.data()[0], reopened_a.data()[1]],
            [
                reopened_a.data()[reopened_a.data().len() - 2],
                reopened_a.data()[reopened_a.data().len() - 1],
            ],
        ],
        rendered_a_endpoints
    );
    assert_eq!(
        [
            [reopened_b.data()[0], reopened_b.data()[1]],
            [
                reopened_b.data()[reopened_b.data().len() - 2],
                reopened_b.data()[reopened_b.data().len() - 1],
            ],
        ],
        rendered_b_endpoints
    );
    assert_eq!(reopened.app.pad(pad(0)).preview, preview_a);
    assert_eq!(reopened.app.pad(pad(7)).preview, preview_b);
    assert_eq!(
        after_open
            .pads
            .iter()
            .find(|saved_pad| saved_pad.pad == pad(0))
            .unwrap()
            .fingerprint,
        fingerprint_a
    );
    assert_eq!(
        after_open
            .pads
            .iter()
            .find(|saved_pad| saved_pad.pad == pad(7))
            .unwrap()
            .fingerprint,
        fingerprint_b
    );
    assert!(reopened.app.pad(pad(15)).sample.is_none());
    reopened.app.apply(InputAction::PadPress(15));
    reopened.engine.render_frames(65, |_| {});
    assert_eq!(reopened.engine.active_voices(), 0);
    let triggers = reopened.engine.executed_triggers();
    reopened.app.apply(InputAction::PadPress(0));
    let mut wav_output = [0.0; 2];
    reopened
        .engine
        .render_frames(65, |frame| wav_output = frame);
    assert!(reopened.engine.executed_triggers() > triggers);
    assert_eq!(wav_output, [0.0; 2], "persisted mute must silence pad zero");
    reopened.app.apply(InputAction::PadRelease(0));
    reopened.engine.render_frames(65, |_| {});
    let triggers = reopened.engine.executed_triggers();
    reopened.app.apply(InputAction::PadPress(7));
    let mut flac_peak = [0.0_f32; 2];
    reopened.engine.render_frames(512, |frame| {
        flac_peak[0] = flac_peak[0].max(frame[0].abs());
        flac_peak[1] = flac_peak[1].max(frame[1].abs());
    });
    assert!(reopened.engine.executed_triggers() > triggers);
    assert!(
        flac_peak[0] > 1.0e-4 && flac_peak[1] > flac_peak[0],
        "rendered FLAC trigger must be audible with rightward pan: {flac_peak:?}"
    );
    assert_eq!(fs::read(&wav).unwrap(), wav_before);
    assert_eq!(fs::read(&flac).unwrap(), flac_before);
}

#[test]
fn mixer_autosave_restore_discard_and_failed_open_preserve_explicit_or_running_truth() {
    let fixture = FixtureTree::new();
    let wav = fixture.write_wav("source.wav");
    let source_bytes = fs::read(&wav).unwrap();
    let project = fixture.path("recovery-project");
    let now = Instant::now();
    let mut source = Harness::new();
    source.load(pad(0), &wav);
    source
        .app
        .update_pad_settings(
            pad(0),
            PadSettings::new(
                PlaybackMode::OneShot,
                -2.0,
                -0.25,
                0.0,
                Some(ChokeGroup::new(3).unwrap()),
            )
            .unwrap(),
        )
        .unwrap();
    source
        .app
        .update_pad_mix(pad(0), explicit_pad_mix())
        .unwrap();
    source.app.update_master_mix(explicit_master_mix()).unwrap();
    source.save_as(&project, now);
    let explicit_snapshot = source.app.project_snapshot().unwrap();
    let explicit_bytes = fs::read(project.join("project.toml")).unwrap();

    source
        .app
        .update_pad_settings(
            pad(0),
            PadSettings::new(
                PlaybackMode::Loop,
                -5.0,
                0.5,
                4.0,
                Some(ChokeGroup::new(9).unwrap()),
            )
            .unwrap(),
        )
        .unwrap();
    source
        .app
        .update_pad_mix(pad(0), recovery_pad_mix())
        .unwrap();
    source.app.update_master_mix(recovery_master_mix()).unwrap();
    source.palette("tempo 139");
    source.palette("tempo 144");
    source.palette("tempo 149");
    let recovery_snapshot = source.app.project_snapshot().unwrap();
    source.autosave(now);
    assert_eq!(
        fs::read(project.join("project.toml")).unwrap(),
        explicit_bytes
    );
    assert!(project.join(".sampler-tui-recovery.toml").is_file());

    let mut restored = Harness::new();
    restored.open(&project, Some(RecoveryChoice::Restore), now);
    assert_eq!(
        restored.app.project_snapshot().unwrap().patterns,
        recovery_snapshot.patterns
    );
    assert_eq!(restored.app.pad_mix(pad(0)), recovery_pad_mix());
    assert_eq!(restored.app.master_mix(), recovery_master_mix());
    assert_eq!(
        restored.app.project_snapshot().unwrap().revision,
        recovery_snapshot.revision
    );
    assert!(restored.app.project_header().contains("MODIFIED"));
    drop(restored);

    let mut discarded = Harness::new();
    discarded.open(&project, Some(RecoveryChoice::Discard), now);
    assert!(!project.join(".sampler-tui-recovery.toml").exists());
    assert_eq!(
        discarded.app.project_snapshot().unwrap().patterns,
        ProjectStore
            .probe(&project)
            .unwrap()
            .explicit
            .unwrap()
            .unwrap()
            .patterns
    );
    assert_eq!(discarded.app.pad_mix(pad(0)), explicit_pad_mix());
    assert_eq!(discarded.app.master_mix(), explicit_master_mix());
    assert_eq!(
        discarded.app.project_snapshot().unwrap().revision,
        explicit_snapshot.revision
    );

    let saved_document = ProjectStore
        .probe(&project)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();
    let asset = &saved_document.pads[0].audio_path;

    let changed = fixture.path("changed-project");
    copy_project(&project, &changed);
    fs::write(changed.join(asset), b"changed asset").unwrap();
    assert_failed_open_preserves(&mut discarded, &changed, now);

    let missing = fixture.path("missing-project");
    copy_project(&project, &missing);
    fs::remove_file(missing.join(asset)).unwrap();
    assert_failed_open_preserves(&mut discarded, &missing, now);

    let corrupt = fixture.path("corrupt-project");
    copy_project(&project, &corrupt);
    fs::write(corrupt.join("project.toml"), b"not = [valid").unwrap();
    assert_failed_open_preserves(&mut discarded, &corrupt, now);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let linked = fixture.path("linked-project");
        copy_project(&project, &linked);
        fs::remove_file(linked.join(asset)).unwrap();
        symlink(&wav, linked.join(asset)).unwrap();
        assert_failed_open_preserves(&mut discarded, &linked, now);
    }

    assert_eq!(fs::read(&wav).unwrap(), source_bytes);
    assert_eq!(
        fs::read(project.join("project.toml")).unwrap(),
        explicit_bytes
    );
}

#[test]
fn real_worker_opens_sparse_v1_pattern_as_exact_slot_zero_plus_defaults() {
    let fixture = FixtureTree::new();
    let source = fixture.write_wav("legacy-source.wav");
    let project = fixture.path("legacy-project");
    fs::create_dir_all(project.join("audio")).unwrap();
    fs::copy(source, project.join("audio/kick.wav")).unwrap();
    fs::write(
        project.join("project.toml"),
        r#"
schema_version = 1
name = "Legacy Sparse"

[[pads]]
audio_path = "audio/kick.wav"

[pads.pad]
bank = 0
index = 0

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0

[[patterns]]
name = "Legacy Beat"
sample_rate = 48000
tempo = 133.0
bars = 2
resolution = "eighth"
swing = 0.61

[patterns.meter]
numerator = 4
denominator = 4

[[patterns.events]]
id = 9
frame = 6800
velocity = 0.75

[patterns.events.pad]
bank = 0
index = 0
"#,
    )
    .unwrap();

    let mut opened = Harness::new();
    opened.open(&project, None, Instant::now());
    let patterns = opened.app.patterns().export_project_patterns().unwrap();
    assert_eq!(patterns.len(), sampler_core::PATTERN_SLOT_COUNT);
    assert_eq!(patterns[0].slot, PatternSlotId::new(0).unwrap());
    assert_eq!(patterns[0].name, "Legacy Beat");
    assert_eq!(patterns[0].tempo.bpm(), 133.0);
    assert_eq!(patterns[0].bars, 2);
    assert_eq!(patterns[0].events.len(), 1);
    assert_eq!(patterns[0].events[0].event.frame, 6_800);
    assert_eq!(patterns[0].events[0].raw_frame, 6_800);
    for (index, pattern) in patterns.iter().enumerate().skip(1) {
        assert_eq!(pattern.slot, PatternSlotId::new(index as u8).unwrap());
        assert_eq!(pattern.name, format!("Pattern {:02}", index + 1));
        assert!(pattern.events.is_empty());
        assert_eq!(pattern.sample_rate, 48_000);
        assert_eq!(pattern.tempo.bpm(), 120.0);
    }
}

#[test]
fn literal_schema_v2_opens_with_exact_dry_mixer_defaults_and_unchanged_render() {
    let fixture = FixtureTree::new();
    let source_path = fixture.write_wav("schema-v2-source.wav");
    let project = fixture.path("schema-v2-project");
    let now = Instant::now();

    let mut source = Harness::new();
    source.load(pad(0), &source_path);
    let dry_before = render_live_hit_bits(&mut source, pad(0));
    source.save_as(&project, now);
    let saved = ProjectStore
        .probe(&project)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();
    let saved_pad = &saved.pads[0];
    let literal_v2 = format!(
        r#"schema_version = 2
project_id = "{}"
name = "literal schema v2"
revision = 17
patterns = []

[[pads]]
audio_path = "{}"
asset_digest = "{}"

[pads.pad]
bank = 0
index = 0

[pads.settings]
mode = "OneShot"
gain_db = 0.0
pan = 0.0
pitch_semitones = 0.0

[pads.recipe]
start_phase = 0
end_phase = 4294967296
reversed = false
normalize = false
"#,
        saved.project_id, saved_pad.audio_path, saved_pad.asset_digest
    );
    fs::write(project.join("project.toml"), &literal_v2).unwrap();

    let mut opened = Harness::new();
    opened.open(&project, None, now);
    assert_eq!(opened.app.pad_mix(pad(0)), PadMixSettings::default());
    assert_eq!(opened.app.master_mix(), MasterMixSettings::default());
    let snapshot = opened.app.project_snapshot().unwrap();
    assert_eq!(snapshot.pads[0].mix, PadMixSettings::default());
    assert_eq!(snapshot.master_mix, MasterMixSettings::default());
    assert_eq!(snapshot.revision, 17);
    assert_eq!(render_live_hit_bits(&mut opened, pad(0)), dry_before);
    assert_eq!(
        fs::read_to_string(project.join("project.toml")).unwrap(),
        literal_v2
    );
}

#[test]
fn mixer_project_open_pad_install_backpressure_rolls_back_before_clean_retry() {
    let fixture = FixtureTree::new();
    let wav = fixture.write_wav("source.wav");
    let project = fixture.path("pad-backpressure-project");
    let now = Instant::now();
    let mut source = Harness::new();
    source.load(pad(0), &wav);
    source.save_as(&project, now);

    let mut target = Harness::new();
    target
        .app
        .update_pad_mix(pad(0), recovery_pad_mix())
        .unwrap();
    target.app.update_master_mix(recovery_master_mix()).unwrap();
    target.engine.render_frames(0, |_| {});
    let old = target.app.project_snapshot().unwrap();
    target.app.request_open_project(&project).unwrap();
    assert_eq!(target.dispatch_queued(), 1);
    while target
        .app
        .project_open_stage()
        .is_some_and(|stage| stage.phase != ProjectOpenPhase::Admitting)
    {
        target.app.maintain_project(now);
        target.dispatch_queued();
    }
    assert!(target.app.maintain_project(now));
    assert_eq!(
        target.app.project_open_stage().unwrap().admitted_actions,
        2,
        "StopAll and master must admit before targeting the first pad install"
    );
    target.engine.render_frames(0, |_| {});
    let blocked_progress = target.app.project_open_stage().unwrap().clone();
    let mut saturated_commands = 0;
    loop {
        match target.controller.borrow_mut().stop_pad(pad(15)) {
            Ok(()) => saturated_commands += 1,
            Err(ControlError::CommandQueueFull) => break,
            Err(error) => panic!("unexpected controller saturation error: {error}"),
        }
    }
    assert_eq!(saturated_commands, 8);
    target.app.maintain_project(now);
    assert!(target.app.status().contains("audio rollback failed"));
    assert!(target.app.status().contains("queue is full"));
    assert_eq!(target.app.project_open_stage().unwrap(), &blocked_progress);
    assert_eq!(target.app.project_snapshot().unwrap(), old);
    assert_eq!(target.app.pad_mix(pad(0)), recovery_pad_mix());
    assert_eq!(target.app.master_mix(), recovery_master_mix());
    assert!(target.app.pad(pad(0)).sample.is_none());

    target.engine.render_frames(0, |_| {});
    target.app.maintain_audio();
    assert!(target.app.maintain_project(now));
    assert!(target.app.maintain_project(now));
    assert_eq!(
        target.app.project_open_stage().unwrap().admitted_actions,
        0,
        "rollback completes before candidate admission restarts"
    );
    assert!(target.app.maintain_project(now));
    assert!(target.app.maintain_project(now));
    assert!(target.app.maintain_project(now));
    assert_eq!(
        target.app.project_open_stage().unwrap().admitted_actions,
        3,
        "clean retry replays StopAll and master before the first pad install"
    );
    target.finish_open(now);
    assert!(target.app.pad(pad(0)).sample.is_some());
    assert_ne!(
        target.app.project_snapshot().unwrap().project_id,
        old.project_id
    );
}

#[cfg(unix)]
#[test]
fn probe_then_symlink_replacement_fails_secure_stage_without_replacing_old_tuple() {
    use std::os::unix::fs::symlink;

    let fixture = FixtureTree::new();
    let source_path = fixture.write_wav("source.wav");
    let project = fixture.path("stage-race-project");
    let now = Instant::now();
    let mut source = Harness::new();
    source.load(pad(0), &source_path);
    source.save_as(&project, now);
    let document = ProjectStore
        .probe(&project)
        .unwrap()
        .explicit
        .unwrap()
        .unwrap();
    let asset = project.join(&document.pads[0].audio_path);

    let mut target = Harness::new();
    let old = target.app.project_snapshot().unwrap();
    target.app.request_open_project(&project).unwrap();
    assert_eq!(target.dispatch_queued(), 1, "probe completes first");
    fs::remove_file(&asset).unwrap();
    symlink(&source_path, &asset).unwrap();
    assert!(target.app.maintain_project(now));
    assert_eq!(target.dispatch_queued(), 1, "secure stage reads next");

    assert!(target.app.project_open_error().is_some());
    assert_eq!(target.app.project_snapshot().unwrap(), old);
}

#[cfg(unix)]
#[test]
fn stage_fails_if_project_directory_path_changes_after_asset_fd_open() {
    use std::os::unix::fs::symlink;

    let fixture = FixtureTree::new();
    let source_path = fixture.write_wav("directory-race-source.wav");
    let project = fixture.path("directory-race-project");
    let moved = fixture.path("opened-directory-race-project");
    let now = Instant::now();
    let mut source = Harness::new();
    source.load(pad(0), &source_path);
    source.save_as(&project, now);

    let opened = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let worker_opened = Arc::clone(&opened);
    let worker_resume = Arc::clone(&resume);
    let worker = WorkerHandle::spawn_with_project_asset_open_hook(move || {
        worker_opened.wait();
        worker_resume.wait();
    });
    let mut target = Harness::new_with_worker(worker);
    let old = target.app.project_snapshot().unwrap();
    let old_header = target.app.project_header();
    target.app.request_open_project(&project).unwrap();
    assert_eq!(target.dispatch_queued(), 1, "probe completes first");
    assert!(target.app.maintain_project(now));
    let [request] = target.app.take_worker_requests().try_into().unwrap();
    target.worker.try_send(request).unwrap();

    opened.wait();
    fs::rename(&project, &moved).unwrap();
    symlink(&moved, &project).unwrap();
    resume.wait();
    let result = target.worker.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(target.app.apply_worker_result(result));

    assert!(matches!(
        target.app.project_open_error(),
        Some(sampler_tui::ProjectOpenError::Stage {
            error: sampler_tui::ProjectStageError::Load(
                sampler_tui::LoadSampleError::ProjectAsset(
                    sampler_tui::ProjectStoreError::Filesystem {
                        operation: "verify project directory identity",
                        ..
                    }
                )
            ),
            ..
        })
    ));
    assert_eq!(target.app.project_snapshot().unwrap(), old);
    assert_eq!(target.app.project_header(), old_header);
}
