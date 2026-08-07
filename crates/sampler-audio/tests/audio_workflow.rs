use std::sync::Arc;

use sampler_audio::{AudioEngine, PadId, PadSettings, SampleBuffer, audio_channels};
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
