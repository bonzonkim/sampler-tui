#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{PadId, PadSettings, SampleBuffer};
    use sampler_core::{EditablePattern, Meter, PatternSlotId, Resolution, Tempo, Transport};

    fn snapshot(slot: u8) -> Arc<sampler_core::PatternSnapshot> {
        let transport = Transport::new(
            48_000,
            Tempo::new(120.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        Arc::new(
            EditablePattern::new(
                PatternSlotId::new(slot % sampler_core::PATTERN_SLOT_COUNT as u8).unwrap(),
                "Pattern",
                transport,
            )
            .unwrap()
            .compile()
            .unwrap(),
        )
    }

    fn ack(id: u64) -> LiveAck {
        LiveAck {
            id: LiveCommandId::new(id).unwrap(),
            pad: PadId::first(),
            kind: LiveAckKind::Release,
            frame: 0,
            transport: None,
        }
    }

    fn pop_pattern_install(
        ports: &mut EnginePorts,
    ) -> (PatternSnapshotSlot, Arc<sampler_core::PatternSnapshot>) {
        match ports.immediate_commands.pop().unwrap() {
            AudioCommand::InstallPattern {
                owner_slot,
                snapshot,
                ..
            } => (owner_slot, snapshot),
            command => panic!("expected pattern install, got {command:?}"),
        }
    }

    fn install_for_retirement(
        controller: &mut AudioController,
        ports: &mut EnginePorts,
        slot: u8,
    ) -> (PatternSnapshotSlot, Arc<sampler_core::PatternSnapshot>) {
        controller.install_pattern(snapshot(slot)).unwrap();
        pop_pattern_install(ports)
    }

    #[test]
    fn tracked_live_ids_advance_only_after_successful_admission() {
        let (mut controller, mut ports) = audio_channels_with_capacities(1, 1, 1);
        let pad = PadId::first();
        let first = controller.trigger_live_tracked(pad, 1.0).unwrap();
        assert_eq!(
            controller.release_live_tracked(pad),
            Err(ControlError::CommandQueueFull)
        );
        assert_eq!(
            ports.immediate_commands.pop().unwrap().live_id(),
            Some(first)
        );
        let second = controller.release_live_tracked(pad).unwrap();
        assert_eq!(second.get(), first.get() + 1);
    }

    #[test]
    fn live_command_ids_admit_max_once_then_report_exhaustion() {
        let (mut controller, mut ports) = audio_channels();
        controller.set_next_live_id_for_test(u64::MAX);

        let last = controller
            .trigger_live_tracked(PadId::first(), 1.0)
            .unwrap();
        assert_eq!(last.get(), u64::MAX);
        assert_eq!(
            controller.release_live_tracked(PadId::first()),
            Err(ControlError::LiveCommandIdExhausted)
        );
        assert_eq!(
            controller.release_live(PadId::first()),
            Err(ControlError::LiveCommandIdExhausted)
        );
        assert_eq!(
            controller.trigger_live(PadId::first(), 1.0),
            Err(ControlError::LiveCommandIdExhausted)
        );
        assert_eq!(
            ports.immediate_commands.pop().unwrap().live_id(),
            Some(last)
        );
        assert!(ports.immediate_commands.pop().is_err());
    }

    #[test]
    fn empty_ack_never_contains_the_zero_live_command_id() {
        assert_eq!(LiveCommandId::new(0), None);
        assert_ne!(LiveAck::EMPTY.id.get(), 0);
    }

    #[test]
    fn acknowledgements_and_pattern_retirements_have_independent_bounds() {
        let (mut controller, mut ports) = audio_channels();
        let (owner_slot, snapshot) = install_for_retirement(&mut controller, &mut ports, 0);
        ports.live_acks.push(ack(1)).unwrap();
        ports
            .pattern_retirements
            .push(PatternRetirement::new(owner_slot, snapshot))
            .unwrap();
        assert_eq!(controller.drain_live_acks(&mut [LiveAck::EMPTY; 1]), 1);
        assert_eq!(controller.reclaim_retired_pattern().unwrap().index(), 0);
    }

    #[test]
    fn auxiliary_lanes_accept_their_exact_fixed_capacities() {
        let (mut controller, mut ports) = audio_channels();
        for id in 1..=256 {
            ports.live_acks.push(ack(id)).unwrap();
        }
        assert!(ports.live_acks.push(ack(u64::MAX)).is_err());

        let mut overflow = None;
        for index in 0..32 {
            let (owner_slot, snapshot) =
                install_for_retirement(&mut controller, &mut ports, index as u8);
            if overflow.is_none() {
                overflow = Some(PatternRetirement::new(owner_slot, Arc::clone(&snapshot)));
            }
            ports
                .pattern_retirements
                .push(PatternRetirement::new(owner_slot, snapshot))
                .unwrap();
        }
        assert!(ports.pattern_retirements.push(overflow.unwrap()).is_err());
    }

    #[test]
    fn thirty_two_pattern_snapshots_are_bounded_and_reclaimed_by_slot() {
        let (mut controller, mut ports) = audio_channels();
        let slots = (0..32)
            .map(|generation| {
                controller
                    .install_pattern(snapshot(generation as u8))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            controller.install_pattern(snapshot(99)),
            Err(ControlError::NoFreePatternSlot)
        );
        let (retired_slot, retired_snapshot) = pop_pattern_install(&mut ports);
        assert_eq!(retired_slot, slots[0]);
        ports
            .pattern_retirements
            .push(PatternRetirement::new(retired_slot, retired_snapshot))
            .unwrap();
        assert_eq!(controller.reclaim_retired_pattern(), Some(slots[0]));
        let reused = controller.install_pattern(snapshot(100)).unwrap();
        assert_eq!(reused.index(), slots[0].index());
        assert_ne!(reused.generation(), slots[0].generation());
    }

    #[test]
    fn mismatched_pattern_arc_does_not_release_an_active_owner_slot() {
        let (mut controller, mut ports) = audio_channels();
        let (owner_slot, installed_snapshot) =
            install_for_retirement(&mut controller, &mut ports, 0);
        ports
            .pattern_retirements
            .push(PatternRetirement::new(owner_slot, snapshot(0)))
            .unwrap();

        assert_eq!(controller.reclaim_retired_pattern(), None);
        assert_eq!(controller.install_pattern(snapshot(1)).unwrap().index(), 1);

        ports
            .pattern_retirements
            .push(PatternRetirement::new(owner_slot, installed_snapshot))
            .unwrap();
        assert_eq!(controller.reclaim_retired_pattern(), Some(owner_slot));
    }

    #[test]
    fn stale_pattern_retirement_is_skipped_after_owner_slot_reuse() {
        let (mut controller, mut ports) = audio_channels();
        let (first_owner, first_snapshot) = install_for_retirement(&mut controller, &mut ports, 0);
        let stale_snapshot = Arc::clone(&first_snapshot);
        ports
            .pattern_retirements
            .push(PatternRetirement::new(first_owner, first_snapshot))
            .unwrap();
        assert_eq!(controller.reclaim_retired_pattern(), Some(first_owner));

        let (second_owner, second_snapshot) =
            install_for_retirement(&mut controller, &mut ports, 1);
        assert_eq!(first_owner.index(), second_owner.index());
        assert_ne!(first_owner.generation(), second_owner.generation());
        ports
            .pattern_retirements
            .push(PatternRetirement::new(first_owner, stale_snapshot))
            .unwrap();
        ports
            .pattern_retirements
            .push(PatternRetirement::new(second_owner, second_snapshot))
            .unwrap();

        assert_eq!(controller.reclaim_retired_pattern(), Some(second_owner));
    }

    #[test]
    fn rejected_pattern_install_returns_its_exact_owner_slot() {
        let (mut controller, mut ports) = audio_channels_with_capacities(1, 1, 1);
        controller.stop_pad(PadId::first()).unwrap();
        assert_eq!(
            controller.install_pattern(snapshot(0)),
            Err(ControlError::CommandQueueFull)
        );
        ports.immediate_commands.pop().unwrap();
        let owner_slot = controller.install_pattern(snapshot(0)).unwrap();
        assert_eq!(owner_slot.index(), 0);
        assert_eq!(owner_slot.generation(), 1);
        assert!(matches!(
            ports.immediate_commands.pop().unwrap(),
            AudioCommand::InstallPattern {
                owner_slot: command_owner,
                sequence: 1,
                ..
            } if command_owner == owner_slot
        ));
    }

    #[test]
    fn pattern_install_command_carries_the_returned_owner_and_snapshot() {
        let (mut controller, mut ports) = audio_channels();
        let snapshot = snapshot(4);
        let expected_snapshot = Arc::clone(&snapshot);
        let owner_slot = controller.install_pattern(snapshot).unwrap();

        match ports.immediate_commands.pop().unwrap() {
            AudioCommand::InstallPattern {
                owner_slot: command_owner,
                snapshot: command_snapshot,
                sequence,
            } => {
                assert_eq!(command_owner, owner_slot);
                assert!(Arc::ptr_eq(&command_snapshot, &expected_snapshot));
                assert_eq!(sequence, 1);
            }
            command => panic!("expected pattern install, got {command:?}"),
        }
        controller.stop_all().unwrap();
        assert_eq!(ports.take_stop_fence(), Some(1));
    }

    #[test]
    fn pattern_controls_use_shared_command_admission() {
        let (mut controller, mut ports) = audio_channels_with_capacities(4, 1, 1);
        let slot = PatternSlotId::new(3).unwrap();
        controller
            .select_pattern(slot, PatternSwitch::NextBoundary)
            .unwrap();
        controller.play_pattern().unwrap();
        controller.stop_pattern().unwrap();
        controller.set_record_capture(Some((slot, 7))).unwrap();
        assert_eq!(
            controller.set_record_capture(None),
            Err(ControlError::CommandQueueFull)
        );

        assert!(matches!(
            ports.immediate_commands.pop().unwrap(),
            AudioCommand::SelectPattern {
                slot: selected,
                switch_at: PatternSwitch::NextBoundary,
                sequence: 1,
            } if selected == slot
        ));
        assert!(matches!(
            ports.immediate_commands.pop().unwrap(),
            AudioCommand::PatternPlay { sequence: 2 }
        ));
        assert!(matches!(
            ports.immediate_commands.pop().unwrap(),
            AudioCommand::PatternStop { sequence: 3 }
        ));
        assert!(matches!(
            ports.immediate_commands.pop().unwrap(),
            AudioCommand::SetRecordCapture {
                capture: Some((captured, 7)),
                sequence: 4,
            } if captured == slot
        ));

        controller.set_record_capture(None).unwrap();
        assert!(matches!(
            ports.immediate_commands.pop().unwrap(),
            AudioCommand::SetRecordCapture {
                capture: None,
                sequence: 5,
            }
        ));
    }

    #[test]
    fn pattern_play_sequences_are_ordered_across_the_stop_fence() {
        let (mut controller, mut ports) = audio_channels();
        controller.play_pattern().unwrap();
        controller.stop_all().unwrap();
        controller.play_pattern().unwrap();
        let fence = ports.take_stop_fence().unwrap();

        let pre_fence = match ports.immediate_commands.pop().unwrap() {
            AudioCommand::PatternPlay { sequence } => sequence,
            command => panic!("expected pattern play, got {command:?}"),
        };
        let post_fence = match ports.immediate_commands.pop().unwrap() {
            AudioCommand::PatternPlay { sequence } => sequence,
            command => panic!("expected pattern play, got {command:?}"),
        };

        assert!(pre_fence <= fence);
        assert!(post_fence > fence);
    }

    #[test]
    fn controller_never_silently_drops_a_trigger() {
        let (mut controller, mut ports) = audio_channels_with_capacities(1, 256, 8);
        let pad = PadId::first();
        controller.trigger(pad, 10, 1.0).unwrap();
        assert_eq!(
            controller.trigger(pad, 11, 1.0),
            Err(ControlError::CommandQueueFull)
        );
        assert!(matches!(
            ports.commands.pop().unwrap(),
            AudioCommand::Trigger { at_frame: 10, .. }
        ));
    }

    #[test]
    fn retired_slots_return_to_the_free_pool_off_thread() {
        let (mut controller, mut ports) = audio_channels_with_capacities(8, 256, 8);
        let sample = Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap());
        let slot = controller
            .install(PadId::first(), sample, PadSettings::default())
            .unwrap();
        let installed = ports.immediate_commands.pop().unwrap();
        let buffer = installed.into_installed_buffer().unwrap();
        ports
            .retirements
            .push(CriticalEvent::RetiredSample { slot, buffer })
            .unwrap();
        assert_eq!(controller.reclaim_retired_slot(), Some(slot));
        assert_eq!(controller.reclaim_retired_slot(), None);
        assert_eq!(controller.available_slots(), 256);
    }

    #[test]
    fn controller_validates_velocity() {
        let (mut controller, _) = audio_channels_with_capacities(8, 256, 8);
        assert_eq!(
            controller.trigger(PadId::first(), 0, f32::NAN),
            Err(ControlError::InvalidVelocity)
        );
    }

    #[test]
    fn command_queue_accepts_its_exact_capacity() {
        let (mut controller, mut ports) = audio_channels_with_capacities(2, 256, 8);
        let pad = PadId::first();
        controller.trigger(pad, 0, 1.0).unwrap();
        controller.trigger(pad, 1, 1.0).unwrap();
        assert_eq!(
            controller.trigger(pad, 2, 1.0),
            Err(ControlError::CommandQueueFull)
        );
        assert_eq!(controller.command_overflows(), 1);
        assert!(ports.commands.pop().is_ok());
        assert!(ports.commands.pop().is_ok());
    }

    #[test]
    fn install_returns_slot_when_command_queue_is_full() {
        let (mut controller, _) = audio_channels_with_capacities(1, 256, 8);
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let sample = Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap());
        assert_eq!(
            controller.install(PadId::first(), sample, PadSettings::default()),
            Err(ControlError::CommandQueueFull)
        );
        assert_eq!(controller.available_slots(), 256);
        assert_eq!(controller.command_overflows(), 1);
    }

    #[test]
    fn recovery_admission_is_exact_and_does_not_reduce_ordinary_install_capacity() {
        let (mut controller, ports) = audio_channels_with_capacities(128, 256, 8);
        for _ in 0..RECOVERY_COMMAND_CAPACITY {
            controller
                .install_recovery(
                    PadId::first(),
                    Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
                    PadSettings::default(),
                )
                .unwrap();
        }
        assert_eq!(
            controller.install_recovery(
                PadId::first(),
                Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
                PadSettings::default(),
            ),
            Err(ControlError::CommandQueueFull)
        );
        controller
            .install(
                PadId::first(),
                Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
                PadSettings::default(),
            )
            .unwrap();

        let mut engine = crate::AudioEngine::new(48_000, ports).unwrap();
        engine.render_frames(0, |_| {});
        controller
            .install_recovery(
                PadId::first(),
                Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
                PadSettings::default(),
            )
            .unwrap();
    }

    #[test]
    fn failed_recovery_queue_push_rolls_back_its_admission_credit() {
        let (mut controller, mut ports) = audio_channels_with_capacities(1, 256, 8);
        controller.stop_pad(PadId::first()).unwrap();
        for _ in 0..=RECOVERY_COMMAND_CAPACITY {
            assert_eq!(
                controller.install_recovery(
                    PadId::first(),
                    Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
                    PadSettings::default(),
                ),
                Err(ControlError::CommandQueueFull)
            );
        }
        ports.immediate_commands.pop().unwrap();

        controller
            .install_recovery(
                PadId::first(),
                Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap()),
                PadSettings::default(),
            )
            .unwrap();
    }

    #[test]
    fn production_channels_use_the_command_capacity_constant() {
        let (mut controller, mut ports) = audio_channels();
        let pad = PadId::first();
        for frame in 0..COMMAND_CAPACITY {
            controller.trigger(pad, frame as u64, 1.0).unwrap();
        }
        assert_eq!(
            controller.trigger(pad, COMMAND_CAPACITY as u64, 1.0),
            Err(ControlError::CommandQueueFull)
        );
        for _ in 0..COMMAND_CAPACITY {
            assert!(ports.commands.pop().is_ok());
        }
    }

    #[test]
    fn telemetry_reports_controller_command_overflow_snapshot() {
        let (mut controller, ports) = audio_channels_with_capacities(1, 256, 8);
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        assert_eq!(
            controller.trigger(PadId::first(), 1, 1.0),
            Err(ControlError::CommandQueueFull)
        );
        let mut engine = crate::AudioEngine::new(48_000, ports).unwrap();

        engine.render_frames(1_600, |_| {});

        assert_eq!(controller.latest_telemetry().unwrap().command_overflows, 1);
    }

    #[test]
    fn active_pad_bits_cover_all_banks_without_aliasing() {
        let first = PadId::first();
        let last = PadId::new(sampler_core::BankId::new(9).unwrap(), 15).unwrap();
        let other = PadId::new(sampler_core::BankId::new(1).unwrap(), 0).unwrap();
        let telemetry = Telemetry {
            active_pads: [1, 0, 1 << 31],
            rendered_frame: 0,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 2,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
        };

        assert!(telemetry.is_pad_active(first));
        assert!(telemetry.is_pad_active(last));
        assert!(!telemetry.is_pad_active(other));
    }

    #[test]
    fn runtime_failure_closes_every_controller_operation() {
        let (mut controller, ports) = audio_channels_with_capacities(8, 256, 8);
        ports.shared.mark_failed();
        let sample = Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap());
        let pad = PadId::first();

        assert_eq!(
            controller.install(pad, sample, PadSettings::default()),
            Err(ControlError::ClosedSession)
        );
        assert_eq!(
            controller.trigger(pad, 0, f32::NAN),
            Err(ControlError::ClosedSession)
        );
        assert_eq!(controller.release(pad, 0), Err(ControlError::ClosedSession));
        assert_eq!(
            controller.update_pad(pad, PadSettings::default()),
            Err(ControlError::ClosedSession)
        );
        assert_eq!(controller.stop_pad(pad), Err(ControlError::ClosedSession));
        assert_eq!(controller.stop_all(), Err(ControlError::ClosedSession));
        assert_eq!(ports.commands.slots(), 0);
        assert_eq!(ports.immediate_commands.slots(), 0);
        assert_eq!(ports.queued_commands(), 0);
    }

    #[test]
    fn rejected_timed_command_does_not_advance_the_stop_fence() {
        let (mut controller, ports) = audio_channels_with_capacities(1, 256, 8);
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        assert_eq!(
            controller.trigger(PadId::first(), 1, 1.0),
            Err(ControlError::CommandQueueFull)
        );

        controller.stop_all().unwrap();

        assert_eq!(ports.take_stop_fence(), Some(1));
    }
}
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rtrb::{Consumer, PeekError, PopError, Producer, RingBuffer};
use sampler_core::{Frame, PadId, PadSettings, PatternSlotId, PatternSnapshot};

use crate::{ControlError, SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot};

pub const COMMAND_CAPACITY: usize = 1024;
pub const RECOVERY_COMMAND_CAPACITY: usize = 32;
pub const RETIREMENT_CAPACITY: usize = 256;
pub const TELEMETRY_CAPACITY: usize = 64;
pub const LIVE_ACK_CAPACITY: usize = 256;
pub const PATTERN_SNAPSHOT_SLOT_COUNT: usize = 32;
pub const PATTERN_RETIREMENT_CAPACITY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LiveCommandId(NonZeroU64);

impl LiveCommandId {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    pub fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LiveAckKind {
    Trigger { velocity: f32 },
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportStamp {
    pub slot: PatternSlotId,
    pub generation: u64,
    pub origin: Frame,
    pub loop_frames: Frame,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveAck {
    pub id: LiveCommandId,
    pub pad: PadId,
    pub kind: LiveAckKind,
    pub frame: Frame,
    pub transport: Option<TransportStamp>,
}

impl LiveAck {
    pub const EMPTY: Self = Self {
        id: LiveCommandId::FIRST,
        pad: PadId::first(),
        kind: LiveAckKind::Release,
        frame: 0,
        transport: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PatternSnapshotSlot {
    index: u8,
    generation: NonZeroU64,
}

impl PatternSnapshotSlot {
    pub fn index(self) -> usize {
        usize::from(self.index)
    }

    pub fn generation(self) -> u64 {
        self.generation.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternSwitch {
    Immediate,
    NextBoundary,
}

#[derive(Debug)]
pub struct PatternRetirement {
    owner_slot: PatternSnapshotSlot,
    snapshot: Arc<PatternSnapshot>,
}

impl PatternRetirement {
    pub fn new(owner_slot: PatternSnapshotSlot, snapshot: Arc<PatternSnapshot>) -> Self {
        Self {
            owner_slot,
            snapshot,
        }
    }

    pub fn owner_slot(&self) -> PatternSnapshotSlot {
        self.owner_slot
    }
}

#[derive(Debug, Clone, Copy)]
struct PatternSnapshotOwner {
    token: PatternSnapshotSlot,
    snapshot_identity: usize,
}

#[derive(Debug)]
pub enum AudioCommand {
    InstallSample {
        pad: PadId,
        slot: SampleSlot,
        buffer: Arc<SampleBuffer>,
        settings: PadSettings,
        recovery: bool,
    },
    InstallPattern {
        owner_slot: PatternSnapshotSlot,
        snapshot: Arc<PatternSnapshot>,
        sequence: u64,
    },
    SelectPattern {
        slot: PatternSlotId,
        switch_at: PatternSwitch,
        sequence: u64,
    },
    PatternPlay {
        sequence: u64,
    },
    PatternStop {
        sequence: u64,
    },
    SetRecordCapture {
        capture: Option<(PatternSlotId, u64)>,
        sequence: u64,
    },
    Trigger {
        pad: PadId,
        at_frame: Frame,
        velocity: f32,
        sequence: u64,
    },
    TriggerLive {
        id: LiveCommandId,
        pad: PadId,
        velocity: f32,
        sequence: u64,
    },
    Release {
        pad: PadId,
        at_frame: Frame,
        sequence: u64,
    },
    ReleaseLive {
        id: LiveCommandId,
        pad: PadId,
        sequence: u64,
    },
    UpdatePad {
        pad: PadId,
        settings: PadSettings,
    },
    StopPad {
        pad: PadId,
    },
}

impl AudioCommand {
    pub fn into_installed_buffer(self) -> Option<Arc<SampleBuffer>> {
        match self {
            Self::InstallSample { buffer, .. } => Some(buffer),
            _ => None,
        }
    }

    pub fn live_id(&self) -> Option<LiveCommandId> {
        match self {
            Self::TriggerLive { id, .. } | Self::ReleaseLive { id, .. } => Some(*id),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum CriticalEvent {
    RetiredSample {
        slot: SampleSlot,
        buffer: Arc<SampleBuffer>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Telemetry {
    pub active_pads: [u64; 3],
    pub rendered_frame: Frame,
    pub last_triggered_frame: Option<Frame>,
    pub peak_left: f32,
    pub peak_right: f32,
    pub active_voices: usize,
    pub late_commands: u64,
    pub invalid_commands: u64,
    pub command_overflows: u64,
}

impl Telemetry {
    pub fn is_pad_active(self, pad: PadId) -> bool {
        let index = usize::from(u8::from(pad.bank())) * 16 + usize::from(pad.index());
        let word = index / u64::BITS as usize;
        let bit = index % u64::BITS as usize;
        self.active_pads
            .get(word)
            .is_some_and(|value| value & (1u64 << bit) != 0)
    }
}

pub struct AudioController {
    commands: Producer<AudioCommand>,
    immediate_commands: Producer<AudioCommand>,
    retirements: Consumer<CriticalEvent>,
    live_acks: Consumer<LiveAck>,
    pattern_retirements: Consumer<PatternRetirement>,
    telemetry: Consumer<Telemetry>,
    shared: Arc<SharedControlState>,
    free_slots: [bool; SAMPLE_SLOT_COUNT],
    next_timed_sequence: u64,
    next_live_id: Option<NonZeroU64>,
    pattern_owners: [Option<PatternSnapshotOwner>; PATTERN_SNAPSHOT_SLOT_COUNT],
    pattern_generations: [u64; PATTERN_SNAPSHOT_SLOT_COUNT],
}

/// A command lane that returns controller admission credit when a command is consumed.
pub struct CommandConsumer {
    inner: Consumer<AudioCommand>,
    shared: Arc<SharedControlState>,
}

impl CommandConsumer {
    fn new(inner: Consumer<AudioCommand>, shared: Arc<SharedControlState>) -> Self {
        Self { inner, shared }
    }

    pub fn pop(&mut self) -> Result<AudioCommand, PopError> {
        let command = self.inner.pop()?;
        self.shared.complete_command();
        if matches!(&command, AudioCommand::InstallSample { recovery: true, .. }) {
            self.shared.complete_recovery_command();
        }
        Ok(command)
    }

    pub fn peek(&self) -> Result<&AudioCommand, PeekError> {
        self.inner.peek()
    }

    pub fn slots(&self) -> usize {
        self.inner.slots()
    }
}

pub struct EnginePorts {
    pub commands: CommandConsumer,
    pub immediate_commands: CommandConsumer,
    pub retirements: Producer<CriticalEvent>,
    pub live_acks: Producer<LiveAck>,
    pub pattern_retirements: Producer<PatternRetirement>,
    pub telemetry: Producer<Telemetry>,
    pub(crate) shared: Arc<SharedControlState>,
}

impl EnginePorts {
    pub fn publish_render_horizon(&self, frame: Frame) {
        self.shared.publish_render_horizon(frame);
    }

    pub fn take_stop_fence(&self) -> Option<u64> {
        self.shared.take_stop_fence()
    }

    pub fn queued_commands(&self) -> usize {
        self.shared.queued_commands()
    }

    pub fn command_overflows(&self) -> u64 {
        self.shared.command_overflows()
    }
}

pub(crate) struct SharedControlState {
    command_capacity: usize,
    queued_commands: AtomicUsize,
    render_horizon: AtomicU64,
    fence_sequence: AtomicU64,
    stop_requested: AtomicBool,
    command_overflows: AtomicU64,
    failed: AtomicBool,
    queued_recovery_commands: AtomicUsize,
}

impl SharedControlState {
    fn new(command_capacity: usize) -> Self {
        Self {
            command_capacity,
            queued_commands: AtomicUsize::new(0),
            render_horizon: AtomicU64::new(0),
            fence_sequence: AtomicU64::new(0),
            stop_requested: AtomicBool::new(false),
            command_overflows: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            queued_recovery_commands: AtomicUsize::new(0),
        }
    }

    fn reserve_command(&self) -> bool {
        self.queued_commands
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < self.command_capacity).then_some(queued + 1)
            })
            .is_ok()
    }

    pub(crate) fn complete_command(&self) {
        let previous = self.queued_commands.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "command completion must match admission");
    }

    pub(crate) fn queued_commands(&self) -> usize {
        self.queued_commands.load(Ordering::Acquire)
    }

    pub(crate) fn publish_render_horizon(&self, frame: Frame) {
        self.render_horizon.store(frame, Ordering::Release);
    }

    fn render_horizon(&self) -> Frame {
        self.render_horizon.load(Ordering::Acquire)
    }

    fn request(&self, fence_sequence: u64) {
        self.fence_sequence.store(fence_sequence, Ordering::Relaxed);
        self.stop_requested.store(true, Ordering::Release);
    }

    pub(crate) fn take_stop_fence(&self) -> Option<u64> {
        self.stop_requested
            .swap(false, Ordering::AcqRel)
            .then(|| self.fence_sequence.load(Ordering::Acquire))
    }

    fn record_command_overflow(&self) {
        let _ =
            self.command_overflows
                .fetch_update(Ordering::Release, Ordering::Relaxed, |count| {
                    Some(count.saturating_add(1))
                });
    }

    pub(crate) fn command_overflows(&self) -> u64 {
        self.command_overflows.load(Ordering::Acquire)
    }

    pub(crate) fn mark_failed(&self) {
        self.failed.store(true, Ordering::Release);
    }

    fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    fn reserve_recovery_command(&self) -> bool {
        self.queued_recovery_commands
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < RECOVERY_COMMAND_CAPACITY).then_some(queued + 1)
            })
            .is_ok()
    }

    pub(crate) fn complete_recovery_command(&self) {
        let previous = self.queued_recovery_commands.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous > 0,
            "recovery command completion must match admission"
        );
    }
}

pub fn audio_channels() -> (AudioController, EnginePorts) {
    audio_channels_with_capacity_values(COMMAND_CAPACITY, RETIREMENT_CAPACITY, TELEMETRY_CAPACITY)
}

#[cfg(test)]
pub(crate) fn audio_channels_with_capacities(
    command_capacity: usize,
    retirement_capacity: usize,
    telemetry_capacity: usize,
) -> (AudioController, EnginePorts) {
    audio_channels_with_capacity_values(command_capacity, retirement_capacity, telemetry_capacity)
}

#[doc(hidden)]
pub fn audio_channels_with_test_capacities(
    command_capacity: usize,
    retirement_capacity: usize,
    telemetry_capacity: usize,
) -> (AudioController, EnginePorts) {
    audio_channels_with_capacity_values(command_capacity, retirement_capacity, telemetry_capacity)
}

fn audio_channels_with_capacity_values(
    command_capacity: usize,
    retirement_capacity: usize,
    telemetry_capacity: usize,
) -> (AudioController, EnginePorts) {
    let (command_producer, command_consumer) = RingBuffer::new(command_capacity);
    let (immediate_command_producer, immediate_command_consumer) =
        RingBuffer::new(command_capacity);
    let (retirement_producer, retirement_consumer) = RingBuffer::new(retirement_capacity);
    let (live_ack_producer, live_ack_consumer) = RingBuffer::new(LIVE_ACK_CAPACITY);
    let (pattern_retirement_producer, pattern_retirement_consumer) =
        RingBuffer::new(PATTERN_RETIREMENT_CAPACITY);
    let (telemetry_producer, telemetry_consumer) = RingBuffer::new(telemetry_capacity);
    let shared = Arc::new(SharedControlState::new(command_capacity));

    (
        AudioController {
            commands: command_producer,
            immediate_commands: immediate_command_producer,
            retirements: retirement_consumer,
            live_acks: live_ack_consumer,
            pattern_retirements: pattern_retirement_consumer,
            telemetry: telemetry_consumer,
            shared: Arc::clone(&shared),
            free_slots: [true; SAMPLE_SLOT_COUNT],
            next_timed_sequence: 1,
            next_live_id: Some(NonZeroU64::MIN),
            pattern_owners: [None; PATTERN_SNAPSHOT_SLOT_COUNT],
            pattern_generations: [0; PATTERN_SNAPSHOT_SLOT_COUNT],
        },
        EnginePorts {
            commands: CommandConsumer::new(command_consumer, Arc::clone(&shared)),
            immediate_commands: CommandConsumer::new(
                immediate_command_consumer,
                Arc::clone(&shared),
            ),
            retirements: retirement_producer,
            live_acks: live_ack_producer,
            pattern_retirements: pattern_retirement_producer,
            telemetry: telemetry_producer,
            shared,
        },
    )
}

impl AudioController {
    pub fn render_horizon(&self) -> Frame {
        self.shared.render_horizon()
    }

    pub fn install(
        &mut self,
        pad: PadId,
        buffer: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, ControlError> {
        self.install_inner(pad, buffer, settings, false)
    }

    pub fn install_recovery(
        &mut self,
        pad: PadId,
        buffer: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, ControlError> {
        self.install_inner(pad, buffer, settings, true)
    }

    pub fn install_pattern(
        &mut self,
        snapshot: Arc<PatternSnapshot>,
    ) -> Result<PatternSnapshotSlot, ControlError> {
        self.ensure_open()?;
        let Some((index, generation)) = (0..PATTERN_SNAPSHOT_SLOT_COUNT).find_map(|index| {
            if self.pattern_owners[index].is_some() {
                return None;
            }
            self.pattern_generations[index]
                .checked_add(1)
                .map(|generation| (index, generation))
        }) else {
            return Err(ControlError::NoFreePatternSlot);
        };
        let owner_slot = PatternSnapshotSlot {
            index: u8::try_from(index).expect("pattern owner-slot map fits in its slot identifier"),
            generation: NonZeroU64::new(generation)
                .expect("a checked increment from a generation starts above zero"),
        };
        let sequence = self.next_timed_sequence;
        self.pattern_owners[index] = Some(PatternSnapshotOwner {
            token: owner_slot,
            snapshot_identity: Arc::as_ptr(&snapshot) as usize,
        });
        if let Err(error) = self.push_immediate_command(AudioCommand::InstallPattern {
            owner_slot,
            snapshot,
            sequence,
        }) {
            self.pattern_owners[index] = None;
            return Err(error);
        }
        self.pattern_generations[index] = generation;
        self.advance_timed_sequence();
        Ok(owner_slot)
    }

    pub fn select_pattern(
        &mut self,
        slot: PatternSlotId,
        switch_at: PatternSwitch,
    ) -> Result<(), ControlError> {
        let sequence = self.next_timed_sequence;
        let result = self.push_immediate_command(AudioCommand::SelectPattern {
            slot,
            switch_at,
            sequence,
        });
        if result.is_ok() {
            self.advance_timed_sequence();
        }
        result
    }

    pub fn play_pattern(&mut self) -> Result<(), ControlError> {
        let sequence = self.next_timed_sequence;
        let result = self.push_immediate_command(AudioCommand::PatternPlay { sequence });
        if result.is_ok() {
            self.advance_timed_sequence();
        }
        result
    }

    pub fn stop_pattern(&mut self) -> Result<(), ControlError> {
        let sequence = self.next_timed_sequence;
        let result = self.push_immediate_command(AudioCommand::PatternStop { sequence });
        if result.is_ok() {
            self.advance_timed_sequence();
        }
        result
    }

    pub fn set_record_capture(
        &mut self,
        capture: Option<(PatternSlotId, u64)>,
    ) -> Result<(), ControlError> {
        let sequence = self.next_timed_sequence;
        let result =
            self.push_immediate_command(AudioCommand::SetRecordCapture { capture, sequence });
        if result.is_ok() {
            self.advance_timed_sequence();
        }
        result
    }

    fn install_inner(
        &mut self,
        pad: PadId,
        buffer: Arc<SampleBuffer>,
        settings: PadSettings,
        recovery: bool,
    ) -> Result<SampleSlot, ControlError> {
        self.ensure_open()?;
        let Some(index) = self.free_slots.iter().position(|is_free| *is_free) else {
            return Err(ControlError::NoFreeSampleSlot);
        };
        let slot = SampleSlot::new(index).expect("free-slot map matches sample-slot bounds");
        self.free_slots[index] = false;

        if recovery && !self.shared.reserve_recovery_command() {
            self.free_slots[index] = true;
            self.shared.record_command_overflow();
            return Err(ControlError::CommandQueueFull);
        }

        let command = AudioCommand::InstallSample {
            pad,
            slot,
            buffer,
            settings,
            recovery,
        };
        if let Err(error) = self.push_immediate_command(command) {
            if recovery {
                self.shared.complete_recovery_command();
            }
            self.free_slots[index] = true;
            return Err(error);
        }

        Ok(slot)
    }

    pub fn trigger(
        &mut self,
        pad: PadId,
        at_frame: Frame,
        velocity: f32,
    ) -> Result<(), ControlError> {
        self.ensure_open()?;
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            return Err(ControlError::InvalidVelocity);
        }
        let sequence = self.next_timed_sequence;
        let result = self.push_timed_command(AudioCommand::Trigger {
            pad,
            at_frame,
            velocity,
            sequence,
        });
        if result.is_ok() {
            self.advance_timed_sequence();
        }
        result
    }

    pub fn trigger_live(&mut self, pad: PadId, velocity: f32) -> Result<(), ControlError> {
        self.trigger_live_tracked(pad, velocity).map(|_| ())
    }

    pub fn trigger_live_tracked(
        &mut self,
        pad: PadId,
        velocity: f32,
    ) -> Result<LiveCommandId, ControlError> {
        self.ensure_open()?;
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            return Err(ControlError::InvalidVelocity);
        }
        let sequence = self.next_timed_sequence;
        let live_id = self
            .next_live_id
            .map(LiveCommandId)
            .ok_or(ControlError::LiveCommandIdExhausted)?;
        let result = self.push_immediate_command(AudioCommand::TriggerLive {
            id: live_id,
            pad,
            velocity,
            sequence,
        });
        if result.is_ok() {
            self.advance_timed_sequence();
            self.advance_live_id();
        }
        result.map(|()| live_id)
    }

    pub fn release(&mut self, pad: PadId, at_frame: Frame) -> Result<(), ControlError> {
        self.ensure_open()?;
        let sequence = self.next_timed_sequence;
        let result = self.push_timed_command(AudioCommand::Release {
            pad,
            at_frame,
            sequence,
        });
        if result.is_ok() {
            self.advance_timed_sequence();
        }
        result
    }

    pub fn release_live(&mut self, pad: PadId) -> Result<(), ControlError> {
        self.release_live_tracked(pad).map(|_| ())
    }

    pub fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, ControlError> {
        self.ensure_open()?;
        let sequence = self.next_timed_sequence;
        let live_id = self
            .next_live_id
            .map(LiveCommandId)
            .ok_or(ControlError::LiveCommandIdExhausted)?;
        let result = self.push_immediate_command(AudioCommand::ReleaseLive {
            id: live_id,
            pad,
            sequence,
        });
        if result.is_ok() {
            self.advance_timed_sequence();
            self.advance_live_id();
        }
        result.map(|()| live_id)
    }

    pub fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), ControlError> {
        self.ensure_open()?;
        self.push_immediate_command(AudioCommand::UpdatePad { pad, settings })
    }

    pub fn stop_pad(&mut self, pad: PadId) -> Result<(), ControlError> {
        self.ensure_open()?;
        self.push_immediate_command(AudioCommand::StopPad { pad })
    }

    pub fn stop_all(&mut self) -> Result<(), ControlError> {
        self.ensure_open()?;
        self.shared
            .request(self.next_timed_sequence.wrapping_sub(1));
        Ok(())
    }

    pub fn latest_telemetry(&mut self) -> Option<Telemetry> {
        let mut latest = None;
        while let Ok(telemetry) = self.telemetry.pop() {
            latest = Some(telemetry);
        }
        latest
    }

    pub fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
        let mut drained = 0;
        while let Some(slot) = output.get_mut(drained) {
            let Ok(ack) = self.live_acks.pop() else {
                break;
            };
            *slot = ack;
            drained += 1;
        }
        drained
    }

    pub fn reclaim_retired_pattern(&mut self) -> Option<PatternSnapshotSlot> {
        while let Ok(PatternRetirement {
            owner_slot,
            snapshot,
        }) = self.pattern_retirements.pop()
        {
            let snapshot_identity = Arc::as_ptr(&snapshot) as usize;
            let is_current_owner = self.pattern_owners[owner_slot.index()].is_some_and(|owner| {
                owner.token == owner_slot && owner.snapshot_identity == snapshot_identity
            });
            if !is_current_owner {
                continue;
            }
            self.pattern_owners[owner_slot.index()] = None;
            drop(snapshot);
            return Some(owner_slot);
        }
        None
    }

    pub fn reclaim_retired(&mut self) -> usize {
        let mut reclaimed = 0;
        while self.reclaim_retired_slot().is_some() {
            reclaimed += 1;
        }
        reclaimed
    }

    pub fn reclaim_retired_slot(&mut self) -> Option<SampleSlot> {
        let CriticalEvent::RetiredSample { slot, buffer } = self.retirements.pop().ok()?;
        drop(buffer);
        self.free_slots[slot.index()] = true;
        Some(slot)
    }

    pub fn available_slots(&self) -> usize {
        self.free_slots.iter().filter(|is_free| **is_free).count()
    }

    pub fn command_overflows(&self) -> u64 {
        self.shared.command_overflows()
    }

    fn push_timed_command(&mut self, command: AudioCommand) -> Result<(), ControlError> {
        self.ensure_open()?;
        if !self.shared.reserve_command() {
            self.shared.record_command_overflow();
            return Err(ControlError::CommandQueueFull);
        }
        if self.commands.push(command).is_ok() {
            return Ok(());
        }
        self.shared.complete_command();
        self.shared.record_command_overflow();
        Err(ControlError::CommandQueueFull)
    }

    fn push_immediate_command(&mut self, command: AudioCommand) -> Result<(), ControlError> {
        self.ensure_open()?;
        if !self.shared.reserve_command() {
            self.shared.record_command_overflow();
            return Err(ControlError::CommandQueueFull);
        }
        if self.immediate_commands.push(command).is_ok() {
            return Ok(());
        }
        self.shared.complete_command();
        self.shared.record_command_overflow();
        Err(ControlError::CommandQueueFull)
    }

    fn advance_timed_sequence(&mut self) {
        self.next_timed_sequence = self.next_timed_sequence.wrapping_add(1);
    }

    fn advance_live_id(&mut self) {
        self.next_live_id = self
            .next_live_id
            .and_then(|id| id.get().checked_add(1))
            .and_then(NonZeroU64::new);
    }

    #[cfg(test)]
    fn set_next_live_id_for_test(&mut self, value: u64) {
        self.next_live_id = NonZeroU64::new(value);
    }

    fn ensure_open(&self) -> Result<(), ControlError> {
        if self.shared.is_failed() {
            Err(ControlError::ClosedSession)
        } else {
            Ok(())
        }
    }
}
