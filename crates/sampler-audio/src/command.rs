#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{PadId, PadSettings, SampleBuffer};

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
        let installed = ports.commands.pop().unwrap();
        let buffer = installed.into_installed_buffer().unwrap();
        ports
            .retirements
            .push(CriticalEvent::RetiredSample { slot, buffer })
            .unwrap();
        assert_eq!(controller.reclaim_retired(), 1);
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

        assert_eq!(ports.shared.take_stop_fence(), Some(1));
    }
}
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};
use sampler_core::{Frame, PadId, PadSettings};

use crate::{ControlError, SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot};

pub const COMMAND_CAPACITY: usize = 1024;
pub const RETIREMENT_CAPACITY: usize = 256;
pub const TELEMETRY_CAPACITY: usize = 64;

#[derive(Debug)]
pub enum AudioCommand {
    InstallSample {
        pad: PadId,
        slot: SampleSlot,
        buffer: Arc<SampleBuffer>,
        settings: PadSettings,
    },
    Trigger {
        pad: PadId,
        at_frame: Frame,
        velocity: f32,
        sequence: u64,
    },
    Release {
        pad: PadId,
        at_frame: Frame,
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
    pub rendered_frame: Frame,
    pub last_triggered_frame: Option<Frame>,
    pub peak_left: f32,
    pub peak_right: f32,
    pub active_voices: usize,
    pub late_commands: u64,
    pub invalid_commands: u64,
    pub command_overflows: u64,
}

pub struct AudioController {
    commands: Producer<AudioCommand>,
    retirements: Consumer<CriticalEvent>,
    telemetry: Consumer<Telemetry>,
    shared: Arc<SharedControlState>,
    free_slots: [bool; SAMPLE_SLOT_COUNT],
    next_timed_sequence: u64,
}

pub struct EnginePorts {
    pub commands: Consumer<AudioCommand>,
    pub retirements: Producer<CriticalEvent>,
    pub telemetry: Producer<Telemetry>,
    pub(crate) shared: Arc<SharedControlState>,
}

pub(crate) struct SharedControlState {
    render_horizon: AtomicU64,
    fence_sequence: AtomicU64,
    stop_requested: AtomicBool,
    command_overflows: AtomicU64,
    failed: AtomicBool,
}

impl SharedControlState {
    fn new() -> Self {
        Self {
            render_horizon: AtomicU64::new(0),
            fence_sequence: AtomicU64::new(0),
            stop_requested: AtomicBool::new(false),
            command_overflows: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        }
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
    let (retirement_producer, retirement_consumer) = RingBuffer::new(retirement_capacity);
    let (telemetry_producer, telemetry_consumer) = RingBuffer::new(telemetry_capacity);
    let shared = Arc::new(SharedControlState::new());

    (
        AudioController {
            commands: command_producer,
            retirements: retirement_consumer,
            telemetry: telemetry_consumer,
            shared: Arc::clone(&shared),
            free_slots: [true; SAMPLE_SLOT_COUNT],
            next_timed_sequence: 1,
        },
        EnginePorts {
            commands: command_consumer,
            retirements: retirement_producer,
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
        self.ensure_open()?;
        let Some(index) = self.free_slots.iter().position(|is_free| *is_free) else {
            return Err(ControlError::NoFreeSampleSlot);
        };
        let slot = SampleSlot::new(index).expect("free-slot map matches sample-slot bounds");
        self.free_slots[index] = false;

        let command = AudioCommand::InstallSample {
            pad,
            slot,
            buffer,
            settings,
        };
        if let Err(error) = self.push_command(command) {
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
        let result = self.push_command(AudioCommand::Trigger {
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

    pub fn release(&mut self, pad: PadId, at_frame: Frame) -> Result<(), ControlError> {
        self.ensure_open()?;
        let sequence = self.next_timed_sequence;
        let result = self.push_command(AudioCommand::Release {
            pad,
            at_frame,
            sequence,
        });
        if result.is_ok() {
            self.advance_timed_sequence();
        }
        result
    }

    pub fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), ControlError> {
        self.ensure_open()?;
        self.push_command(AudioCommand::UpdatePad { pad, settings })
    }

    pub fn stop_pad(&mut self, pad: PadId) -> Result<(), ControlError> {
        self.ensure_open()?;
        self.push_command(AudioCommand::StopPad { pad })
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

    pub fn reclaim_retired(&mut self) -> usize {
        let mut reclaimed = 0;
        while let Ok(CriticalEvent::RetiredSample { slot, buffer }) = self.retirements.pop() {
            drop(buffer);
            self.free_slots[slot.index()] = true;
            reclaimed += 1;
        }
        reclaimed
    }

    pub fn available_slots(&self) -> usize {
        self.free_slots.iter().filter(|is_free| **is_free).count()
    }

    pub fn command_overflows(&self) -> u64 {
        self.shared.command_overflows()
    }

    fn push_command(&mut self, command: AudioCommand) -> Result<(), ControlError> {
        self.ensure_open()?;
        if self.commands.push(command).is_err() {
            self.shared.record_command_overflow();
            Err(ControlError::CommandQueueFull)
        } else {
            Ok(())
        }
    }

    fn advance_timed_sequence(&mut self) {
        self.next_timed_sequence = self.next_timed_sequence.wrapping_add(1);
    }

    fn ensure_open(&self) -> Result<(), ControlError> {
        if self.shared.is_failed() {
            Err(ControlError::ClosedSession)
        } else {
            Ok(())
        }
    }
}
