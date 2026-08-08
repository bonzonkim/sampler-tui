use std::sync::Arc;

use sampler_audio::{
    AudioCommand, ControlError, PadId, PadSettings, SampleBuffer,
    audio_channels_with_test_capacities,
};

#[test]
fn public_ports_consume_both_lanes_and_release_shared_admission() {
    let (mut controller, mut ports) = audio_channels_with_test_capacities(3, 8, 8);
    let pad = PadId::first();
    let sample = Arc::new(SampleBuffer::new(48_000, vec![0.25, 0.25]).unwrap());

    controller
        .install(pad, sample, PadSettings::default())
        .unwrap();
    controller.trigger_live(pad, 1.0).unwrap();
    controller.trigger(pad, 10, 1.0).unwrap();
    assert_eq!(
        controller.stop_pad(pad),
        Err(ControlError::CommandQueueFull)
    );

    assert!(matches!(
        ports.immediate_commands.pop().unwrap(),
        AudioCommand::InstallSample { .. }
    ));
    assert!(matches!(
        ports.immediate_commands.pop().unwrap(),
        AudioCommand::TriggerLive { .. }
    ));
    assert!(matches!(
        ports.commands.pop().unwrap(),
        AudioCommand::Trigger { .. }
    ));

    controller.stop_pad(pad).unwrap();
    controller.release_live(pad).unwrap();
    controller.release(pad, 11).unwrap();
    assert_eq!(
        controller.stop_pad(pad),
        Err(ControlError::CommandQueueFull)
    );
}
