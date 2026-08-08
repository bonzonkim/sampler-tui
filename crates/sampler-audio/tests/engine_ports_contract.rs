use std::sync::Arc;

use sampler_audio::{
    AudioCommand, ControlError, PadId, PadSettings, PatternRetirement, SampleBuffer,
    audio_channels_with_test_capacities,
};
use sampler_core::{EditablePattern, Meter, PatternSlotId, Resolution, Tempo, Transport};

fn pattern_snapshot() -> Arc<sampler_core::PatternSnapshot> {
    let transport = Transport::new(
        48_000,
        Tempo::new(120.0).unwrap(),
        Meter::new(4, 4).unwrap(),
        1,
        Resolution::Sixteenth,
    )
    .unwrap();
    Arc::new(
        EditablePattern::new(PatternSlotId::new(0).unwrap(), "Pattern", transport)
            .unwrap()
            .compile()
            .unwrap(),
    )
}

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

#[test]
fn public_pattern_retirement_constructor_round_trips_the_owner() {
    let (mut controller, mut ports) = audio_channels_with_test_capacities(8, 8, 8);
    let owner_slot = controller.install_pattern(pattern_snapshot()).unwrap();
    let installed_snapshot = match ports.immediate_commands.pop().unwrap() {
        AudioCommand::InstallPattern {
            owner_slot: command_owner,
            snapshot,
            ..
        } => {
            assert_eq!(command_owner, owner_slot);
            snapshot
        }
        command => panic!("expected pattern install, got {command:?}"),
    };

    ports
        .pattern_retirements
        .push(PatternRetirement::new(owner_slot, installed_snapshot))
        .unwrap();

    assert_eq!(controller.reclaim_retired_pattern(), Some(owner_slot));
    assert_ne!(owner_slot.generation(), 0);
}
