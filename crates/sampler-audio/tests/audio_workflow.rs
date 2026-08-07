use std::sync::Arc;

use sampler_audio::{
    AudioEngine, COMMAND_CAPACITY, ControlError, PadId, PadSettings, SAMPLE_SLOT_COUNT,
    SampleBuffer, audio_channels,
};
use sampler_core::{BankId, PlaybackMode};

fn constant_sample(frames: usize, value: f32) -> Arc<SampleBuffer> {
    Arc::new(SampleBuffer::new(48_000, vec![value; frames * 2]).unwrap())
}

#[test]
fn rendered_replacement_retires_and_reuses_the_original_slot() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let first_pad = PadId::first();
    let second_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
    let one_shot = PadSettings::new(PlaybackMode::OneShot, 0.0, 0.0, 0.0, None).unwrap();

    let original_slot = controller
        .install(first_pad, constant_sample(4, 0.75), one_shot)
        .unwrap();
    controller.trigger(first_pad, 0, 1.0).unwrap();
    let mut rendered = [0.0; 16];
    engine.render_stereo(&mut rendered);
    assert!(rendered.iter().any(|sample| *sample != 0.0));
    assert_eq!(engine.active_voices(), 0);

    let replacement_slot = controller
        .install(first_pad, constant_sample(8, 0.5), one_shot)
        .unwrap();
    assert_ne!(replacement_slot, original_slot);
    engine.render_stereo(&mut []);
    assert_eq!(controller.reclaim_retired(), 1);

    let reused_slot = controller
        .install(second_pad, constant_sample(8, 0.25), one_shot)
        .unwrap();
    assert_eq!(reused_slot, original_slot);
}

#[test]
fn rapid_pressure_reports_backpressure_and_reaches_bounded_quiescence_without_trigger_loss() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let pad = PadId::first();
    let looped = PadSettings::new(PlaybackMode::Loop, -6.0, 0.0, 0.0, None).unwrap();
    controller
        .install(pad, constant_sample(32, 0.25), looped)
        .unwrap();
    engine.render_frames(0, |_| {});

    let mut accepted_triggers = 0_u64;
    let mut typed_overflows = 0_u64;
    for submission in 0..COMMAND_CAPACITY * 2 {
        let (result, is_trigger) = match submission % 16 {
            0 => (
                controller
                    .install(
                        pad,
                        constant_sample(32, 0.25 + (submission % 4) as f32 * 0.05),
                        looped,
                    )
                    .map(|_| ()),
                false,
            ),
            1 => (controller.update_pad(pad, looped), false),
            2 => (controller.stop_pad(pad), false),
            _ => (controller.trigger(pad, 0, 0.75), true),
        };

        match result {
            Ok(()) if is_trigger => accepted_triggers += 1,
            Ok(()) => {}
            Err(ControlError::CommandQueueFull) => typed_overflows += 1,
            Err(error) => panic!("unexpected controller error: {error}"),
        }
    }

    assert!(typed_overflows > 0);
    assert_eq!(controller.command_overflows(), typed_overflows);

    let mut drain_callbacks = 0;
    while engine.queued_commands() > 0 || engine.pending_actions() > 0 {
        assert!(drain_callbacks < COMMAND_CAPACITY.div_ceil(64) + 1);
        engine.render_frames(64, |_| {});
        controller.reclaim_retired();
        drain_callbacks += 1;
    }

    assert_eq!(engine.executed_triggers(), accepted_triggers);
    assert_eq!(engine.invalid_commands(), 0);
    assert!(engine.late_commands() > 0);
    assert!(engine.late_commands() <= accepted_triggers);

    controller.stop_all().unwrap();
    let mut quiescence_callbacks = 0;
    while engine.active_voices() > 0 || controller.available_slots() < SAMPLE_SLOT_COUNT - 1 {
        assert!(quiescence_callbacks < 4);
        engine.render_frames(64, |_| {});
        controller.reclaim_retired();
        quiescence_callbacks += 1;
    }

    engine.render_frames(1_600, |_| {});
    let telemetry = controller.latest_telemetry().unwrap();
    assert_eq!(telemetry.active_voices, 0);
    assert_eq!(telemetry.invalid_commands, 0);
    assert_eq!(telemetry.command_overflows, typed_overflows);
    assert_eq!(controller.available_slots(), SAMPLE_SLOT_COUNT - 1);
}
