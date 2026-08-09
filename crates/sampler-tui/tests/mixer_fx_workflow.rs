//! Mixer/FX/choke acceptance target. The two established cross-layer harnesses are compiled into
//! this target so the final slice proves one continuous public surface: engine/controller capture,
//! App/worker persistence, project-store migration, and command-palette validation.

#[path = "capture_workflow.rs"]
mod capture_workflow;
#[path = "project_persistence_workflow.rs"]
mod project_persistence_workflow;

use std::sync::Arc;

use sampler_audio::{AudioEngine, Frame, SampleBuffer, SampleSlot, Telemetry, audio_channels};
use sampler_core::{
    BankId, ChokeGroup, DelaySettings, MasterMixSettings, PadId, PadMixSettings, PadSettings,
    PlaybackMode, ReverbSettings,
};
use sampler_tui::{App, AudioPort, CaptureSupport, parse_palette};

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
    engine.render_frames(96, |frame| {
        rendered.push([frame[0].to_bits(), frame[1].to_bits()]);
    });
    rendered
}

#[test]
fn distinct_dry_and_wet_pad_mix_is_nonzero_and_bitwise_deterministic() {
    let dry = render_distinct_pad_mix(MasterMixSettings::default());
    let wet_settings = MasterMixSettings::new(
        -3.0,
        DelaySettings::new(true, 10, 0.4, -6.0).unwrap(),
        ReverbSettings::new(true, 0.8, 0.35, -9.0).unwrap(),
    )
    .unwrap();
    let wet = render_distinct_pad_mix(wet_settings);
    assert!(wet.iter().any(|frame| *frame != [0, 0]));
    assert_ne!(wet, dry);
    assert_eq!(wet, render_distinct_pad_mix(wet_settings));
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
