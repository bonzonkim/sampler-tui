//! Owned Mixer/FX/choke acceptance tests using isolated shared fixture/controller/App/worker
//! support. The dedicated workflow proves the continuous public surface without importing tests
//! from another integration target.

#[path = "support/mixer_harness.rs"]
mod mixer_harness;

use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sampler_audio::{
    AudioEngine, ControlError, Frame, SampleBuffer, SampleSlot, Telemetry, audio_channels,
    audio_channels_with_test_capacities,
};
use sampler_core::{
    BankId, ChokeGroup, DelaySettings, MasterMixSettings, PadId, PadMixSettings, PadSettings,
    PlaybackMode, ReverbSettings,
};
use sampler_tui::{
    App, AudioPort, CaptureSupport, InputAction, ProjectStore, RecoveryChoice, parse_palette,
};

use mixer_harness::{FixtureTree, Harness};

struct RejectingAudio(&'static str);

impl AudioPort for RejectingAudio {
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
    fn render_horizon(&self) -> Frame {
        0
    }
    fn install(
        &mut self,
        _pad: PadId,
        _sample: Arc<SampleBuffer>,
        _settings: PadSettings,
        _mix: PadMixSettings,
    ) -> Result<SampleSlot, String> {
        SampleSlot::new(0).map_err(|error| error.to_string())
    }
    fn trigger(&mut self, _pad: PadId, _at: Frame, _velocity: f32) -> Result<(), String> {
        Ok(())
    }
    fn release(&mut self, _pad: PadId, _at: Frame) -> Result<(), String> {
        Ok(())
    }
    fn stop_pad(&mut self, _pad: PadId) -> Result<(), String> {
        Ok(())
    }
    fn stop_all(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn update_pad(&mut self, _pad: PadId, _settings: PadSettings) -> Result<(), String> {
        Ok(())
    }
    fn update_pad_mix(&mut self, _pad: PadId, _settings: PadMixSettings) -> Result<(), String> {
        Ok(())
    }
    fn update_master_mix(&mut self, settings: MasterMixSettings) -> Result<(), String> {
        if settings == MasterMixSettings::default() {
            Ok(())
        } else {
            Err(self.0.to_owned())
        }
    }
    fn reclaim_retired(&mut self) -> usize {
        0
    }
    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        None
    }
    fn poll_runtime_error(&mut self) -> Option<String> {
        None
    }
    fn capture_support(&self) -> CaptureSupport {
        CaptureSupport::Unsupported
    }
}

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

fn constant_sample(value: f32) -> Arc<SampleBuffer> {
    Arc::new(SampleBuffer::new(1_000, vec![value; 512 * 2]).unwrap())
}

fn render_distinct_pad_mix(master: MasterMixSettings) -> Vec<[u32; 2]> {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(1_000, ports).unwrap();
    let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
    controller
        .install(
            pad(0),
            constant_sample(0.25),
            looping,
            PadMixSettings::new(false, 0.5, 0.25).unwrap(),
        )
        .unwrap();
    controller
        .install(
            pad(1),
            constant_sample(-0.125),
            looping,
            PadMixSettings::new(false, 0.25, 0.75).unwrap(),
        )
        .unwrap();
    controller.update_master_mix(master).unwrap();
    engine.render_frames(64, |_| {});
    controller.trigger(pad(0), 64, 1.0).unwrap();
    controller.trigger(pad(1), 64, 1.0).unwrap();
    let mut rendered = Vec::new();
    engine.render_frames(1_200, |frame| {
        rendered.push([frame[0].to_bits(), frame[1].to_bits()]);
    });
    rendered
}

#[test]
fn distinct_dry_and_wet_pad_mix_is_nonzero_and_bitwise_deterministic() {
    let dry = render_distinct_pad_mix(MasterMixSettings::default());
    let delay_only = MasterMixSettings::new(
        0.0,
        DelaySettings::new(true, 10, 0.4, -6.0).unwrap(),
        ReverbSettings::default(),
    )
    .unwrap();
    let reverb_only = MasterMixSettings::new(
        0.0,
        DelaySettings::default(),
        ReverbSettings::new(true, 0.8, 0.35, -9.0).unwrap(),
    )
    .unwrap();
    let both = MasterMixSettings::new(0.0, delay_only.delay, reverb_only.reverb).unwrap();
    let delayed = render_distinct_pad_mix(delay_only);
    let reverberated = render_distinct_pad_mix(reverb_only);
    let wet = render_distinct_pad_mix(both);
    assert!(wet.iter().any(|frame| *frame != [0, 0]));
    assert_ne!(delayed, dry, "the delay bus must contribute to the tail");
    assert_ne!(
        reverberated, dry,
        "the reverb bus must contribute to the tail"
    );
    assert_ne!(wet, delayed);
    assert_ne!(wet, reverberated);
    assert_eq!(wet, render_distinct_pad_mix(both));
}

#[test]
fn same_choke_group_releases_the_older_voice_and_keeps_the_newer_voice() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(1_000, ports).unwrap();
    let choked = PadSettings::new(
        PlaybackMode::Loop,
        0.0,
        0.0,
        0.0,
        Some(ChokeGroup::new(4).unwrap()),
    )
    .unwrap();
    for (index, value) in [(0, 0.25), (1, -0.5)] {
        controller
            .install(
                pad(index),
                constant_sample(value),
                choked,
                PadMixSettings::default(),
            )
            .unwrap();
    }
    controller.trigger(pad(0), 0, 1.0).unwrap();
    engine.render_frames(1, |_| {});
    assert_eq!(engine.active_voices(), 1);
    controller.trigger(pad(1), 1, 1.0).unwrap();
    engine.render_frames(1, |_| {});
    assert_eq!(
        engine.active_voices(),
        2,
        "the older voice enters its choke release"
    );
    engine.render_frames(63, |_| {});
    assert_eq!(
        engine.active_voices(),
        1,
        "only the newer choked voice remains"
    );
}

#[test]
fn queue_and_device_failures_preserve_the_prior_app_mixer_tuple_and_revision() {
    let requested = MasterMixSettings::new(
        -3.0,
        DelaySettings::new(true, 250, 0.5, -6.0).unwrap(),
        ReverbSettings::new(true, 0.75, 0.25, -9.0).unwrap(),
    )
    .unwrap();
    for failure in ["audio command queue is full", "output device disconnected"] {
        let mut app = App::with_audio(Box::new(RejectingAudio(failure)));
        let local_pad_mix = PadMixSettings::new(true, 0.25, 0.75).unwrap();
        app.update_pad_mix(pad(0), local_pad_mix).unwrap();
        let before = (
            app.pad(pad(0)).settings,
            app.pad_mix(pad(0)),
            app.master_mix(),
            app.project_revision(),
        );
        assert_eq!(app.update_master_mix(requested), Err(failure.to_owned()));
        assert_eq!(
            (
                app.pad(pad(0)).settings,
                app.pad_mix(pad(0)),
                app.master_mix(),
                app.project_revision(),
            ),
            before
        );
    }
}

fn app_mixer_tuple(app: &App) -> (Vec<(PadSettings, PadMixSettings)>, MasterMixSettings, u64) {
    let pads = (0..16)
        .map(|index| (app.pad(pad(index)).settings, app.pad_mix(pad(index))))
        .collect();
    (pads, app.master_mix(), app.project_revision())
}

fn render_app_hit_bits(harness: &mut Harness, index: usize, frames: usize) -> Vec<[u32; 2]> {
    harness.app.apply(InputAction::PadPress(index));
    let mut rendered = Vec::with_capacity(frames);
    harness.engine.render_frames(frames, |frame| {
        rendered.push([frame[0].to_bits(), frame[1].to_bits()]);
    });
    rendered
}

fn render_independent_engine_bits(
    app: &App,
    index: u8,
    pad_mix_override: Option<PadMixSettings>,
    master_override: Option<MasterMixSettings>,
) -> Vec<[u32; 2]> {
    let (mut controller, ports) = audio_channels_with_test_capacities(32, 256, 8);
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    controller
        .update_master_mix(master_override.unwrap_or_else(|| app.master_mix()))
        .unwrap();
    for pad_index in 0..16 {
        let pad_id = pad(pad_index);
        let view = app.pad(pad_id);
        let Some(sample) = view.sample.as_ref() else {
            continue;
        };
        let mix = if pad_index == index {
            pad_mix_override.unwrap_or_else(|| app.pad_mix(pad_id))
        } else {
            app.pad_mix(pad_id)
        };
        controller
            .install(pad_id, Arc::clone(sample), view.settings, mix)
            .unwrap();
    }
    controller.trigger_live(pad(index), 1.0).unwrap();
    let mut rendered = Vec::with_capacity(1_100);
    engine.render_frames(1_100, |frame| {
        rendered.push([frame[0].to_bits(), frame[1].to_bits()]);
    });
    rendered
}

#[test]
fn owned_schema_v2_fixture_defaults_bitwise_to_an_explicit_dry_v3_mix() {
    let fixture = FixtureTree::new();
    let source_path = fixture.write_wav("schema-v2-source.wav");
    let project = fixture.path("schema-v2-project");
    let now = Instant::now();

    let mut explicit_v3 = Harness::new();
    explicit_v3.load(pad(0), &source_path);
    let dry_v3_bits = render_app_hit_bits(&mut explicit_v3, 0, 65);
    explicit_v3.save_as(&project, now);
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
name = "owned schema v2"
revision = 29
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
    drop(explicit_v3);

    let mut migrated = Harness::new();
    migrated.open(&project, None, now);
    assert_eq!(migrated.app.pad_mix(pad(0)), PadMixSettings::default());
    assert_eq!(migrated.app.master_mix(), MasterMixSettings::default());
    assert_eq!(migrated.app.project_revision(), 29);
    assert_eq!(render_app_hit_bits(&mut migrated, 0, 65), dry_v3_bits);
    assert_eq!(
        fs::read_to_string(project.join("project.toml")).unwrap(),
        literal_v2
    );
}

#[test]
fn dedicated_public_mixer_fx_workflow_survives_capture_persistence_recovery_and_device_retry() {
    let fixture = FixtureTree::new();
    let first_wav = fixture.write_wav("first.wav");
    let second_wav = fixture.write_wav("second.wav");
    let project = fixture.path("mixer-project");
    let moved = fixture.path("moved-mixer-project");
    let corrupt = fixture.path("corrupt-project");
    let now = Instant::now();

    let mut source = Harness::new();
    source.load(pad(0), &first_wav);
    source.load(pad(1), &second_wav);
    source.palette("select 1");
    source.palette("pad-choke 4");
    source.palette("delay-send 0.75");
    source.palette("reverb-send 0.25");
    source.palette("select 2");
    source.palette("pad-choke 4");
    source.palette("delay-send 0.25");
    source.palette("reverb-send 0.75");
    source.palette("master-level 0");
    source.palette("delay-enable on");
    source.palette("delay-time 10");
    source.palette("delay-feedback 0.4");
    source.palette("delay-return -6");
    source.palette("reverb-enable on");
    source.palette("reverb-room 0.8");
    source.palette("reverb-damping 0.35");
    source.palette("reverb-return -9");
    for index in [0, 1] {
        let settings = source.app.pad(pad(index)).settings;
        source
            .app
            .update_pad_settings(
                pad(index),
                PadSettings::new(
                    PlaybackMode::Loop,
                    settings.gain_db,
                    settings.pan,
                    settings.pitch_semitones,
                    settings.choke_group,
                )
                .unwrap(),
            )
            .unwrap();
    }
    source.engine.render_frames(64, |_| {});

    source.app.apply(InputAction::PadPress(0));
    source.engine.render_frames(65, |_| {});
    assert_eq!(source.engine.active_voices(), 1);
    source.app.apply(InputAction::PadPress(1));
    source.engine.render_frames(1, |_| {});
    assert_eq!(source.engine.active_voices(), 2);
    source.engine.render_frames(63, |_| {});
    assert_eq!(source.engine.active_voices(), 1);

    source.palette("select 3");
    source.app.apply(InputAction::PadPress(0));
    let captured_pcm = source.resample_and_install(1_024);
    assert!(captured_pcm.iter().any(|sample| *sample != 0.0));
    assert_eq!(
        source.app.pad(pad(2)).sample.as_ref().unwrap().data(),
        captured_pcm
    );
    let explicit_tuple = app_mixer_tuple(&source.app);
    source.save_as(&project, now);
    drop(source);

    fs::rename(&project, &moved).unwrap();
    let mut reopened = Harness::new();
    reopened.open(&moved, None, now);
    assert_eq!(app_mixer_tuple(&reopened.app), explicit_tuple);
    assert_eq!(
        reopened.app.pad(pad(2)).sample.as_ref().unwrap().data(),
        captured_pcm
    );

    reopened.palette("select 1");
    reopened.palette("pad-choke 9");
    reopened.palette("pad-mute on");
    reopened.palette("delay-send 0.9");
    reopened.palette("reverb-send 0.1");
    reopened.palette("master-level -4");
    reopened.palette("delay-feedback 0.7");
    reopened.palette("reverb-room 0.95");
    let recovered_settings = PadSettings::new(
        PlaybackMode::Gate,
        -5.0,
        0.5,
        3.0,
        Some(ChokeGroup::new(9).unwrap()),
    )
    .unwrap();
    reopened
        .app
        .update_pad_settings(pad(0), recovered_settings)
        .unwrap();
    reopened.engine.render_frames(0, |_| {});
    let recovery_tuple = app_mixer_tuple(&reopened.app);
    let recovery_pcm = reopened
        .app
        .pad(pad(2))
        .sample
        .as_ref()
        .unwrap()
        .data()
        .to_vec();
    reopened.autosave(now + Duration::from_secs(10));
    drop(reopened);

    let mut restored = Harness::new();
    restored.open(
        &moved,
        Some(RecoveryChoice::Restore),
        now + Duration::from_secs(10),
    );
    assert_eq!(restored.app.pad(pad(0)).settings, recovered_settings);
    assert_eq!(app_mixer_tuple(&restored.app), recovery_tuple);
    assert_eq!(
        restored.app.pad(pad(2)).sample.as_ref().unwrap().data(),
        recovery_pcm
    );
    let reference_tuple = app_mixer_tuple(&restored.app);
    let reference_bits = render_independent_engine_bits(&restored.app, 1, None, None);
    assert!(reference_bits.iter().any(|frame| *frame != [0, 0]));
    let wrong_default_bits = render_independent_engine_bits(
        &restored.app,
        1,
        Some(PadMixSettings::default()),
        Some(MasterMixSettings::default()),
    );
    assert_ne!(wrong_default_bits, reference_bits);

    fs::create_dir(&corrupt).unwrap();
    fs::write(corrupt.join("project.toml"), "not = [valid").unwrap();
    let before_failure = app_mixer_tuple(&restored.app);
    restored.app.request_open_project(&corrupt).unwrap();
    restored.dispatch_queued();
    for _ in 0..16 {
        if restored.app.project_open_stage().is_none() {
            break;
        }
        restored.app.maintain_project(now);
        restored.dispatch_queued();
    }
    assert!(restored.app.project_open_error().is_some());
    assert_eq!(app_mixer_tuple(&restored.app), before_failure);

    loop {
        match restored.controller.borrow_mut().stop_pad(pad(15)) {
            Ok(()) => {}
            Err(ControlError::CommandQueueFull) => break,
            Err(error) => panic!("unexpected saturation error: {error}"),
        }
    }
    let mut queue_edit = restored.app.master_mix();
    queue_edit.gain_db = -3.0;
    assert!(restored.app.update_master_mix(queue_edit).is_err());
    assert_eq!(app_mixer_tuple(&restored.app), before_failure);
    restored.engine.render_frames(0, |_| {});

    restored.fail_runtime("output device disconnected");
    assert_eq!(restored.app.audio_format(), None);
    assert_eq!(app_mixer_tuple(&restored.app), before_failure);
    assert!(
        restored
            .app
            .retry_with(Box::new(RejectingAudio("replacement device failed")))
    );
    assert_eq!(restored.app.audio_format(), None);
    assert_eq!(restored.app.status(), "replacement device failed");
    assert_eq!(app_mixer_tuple(&restored.app), before_failure);
    assert!(restored.retry_fresh_audio());
    assert_eq!(app_mixer_tuple(&restored.app), before_failure);
    assert_eq!(app_mixer_tuple(&restored.app), reference_tuple);
    assert_eq!(restored.app.master_mix(), recovery_tuple.1);
    for index in 0..16 {
        assert_eq!(
            restored.app.pad(pad(index as u8)).settings,
            recovery_tuple.0[index].0
        );
        assert_eq!(
            restored.app.pad_mix(pad(index as u8)),
            recovery_tuple.0[index].1
        );
    }
    assert_eq!(
        restored.app.pad(pad(2)).sample.as_ref().unwrap().data(),
        recovery_pcm
    );
    let replacement_bits = render_app_hit_bits(&mut restored, 1, 1_100);
    assert_eq!(replacement_bits, reference_bits);
}

#[test]
fn strict_nonnegative_mixer_palette_commands_reject_negative_underflow() {
    for command in [
        "delay-send",
        "reverb-send",
        "delay-feedback",
        "reverb-room",
        "reverb-damping",
    ] {
        assert!(
            parse_palette(&format!("{command} -1e-9999")).is_err(),
            "{command} must reject a lexically negative value even when it underflows to -0.0"
        );
    }
}
