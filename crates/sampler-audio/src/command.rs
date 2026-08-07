#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{PadId, PadSettings, SampleBuffer};

    #[test]
    fn controller_never_silently_drops_a_trigger() {
        let (mut controller, mut ports) = audio_channels(1, 256, 8);
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
        let (mut controller, mut ports) = audio_channels(8, 256, 8);
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
        let (mut controller, _) = audio_channels(8, 256, 8);
        assert_eq!(
            controller.trigger(PadId::first(), 0, f32::NAN),
            Err(ControlError::InvalidVelocity)
        );
    }

    #[test]
    fn command_queue_accepts_its_exact_capacity() {
        let (mut controller, mut ports) = audio_channels(2, 256, 8);
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
        let (mut controller, _) = audio_channels(1, 256, 8);
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let sample = Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap());
        assert_eq!(
            controller.install(PadId::first(), sample, PadSettings::default()),
            Err(ControlError::CommandQueueFull)
        );
        assert_eq!(controller.available_slots(), 256);
        assert_eq!(controller.command_overflows(), 1);
    }
}
use std::sync::Arc;

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
    },
    Release {
        pad: PadId,
        at_frame: Frame,
    },
    UpdatePad {
        pad: PadId,
        settings: PadSettings,
    },
    StopPad {
        pad: PadId,
    },
    StopAll,
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
    pub peak_left: f32,
    pub peak_right: f32,
    pub active_voices: usize,
    pub late_commands: u64,
    pub invalid_commands: u64,
}

pub struct AudioController {
    commands: Producer<AudioCommand>,
    retirements: Consumer<CriticalEvent>,
    telemetry: Consumer<Telemetry>,
    free_slots: [bool; SAMPLE_SLOT_COUNT],
    command_overflows: u64,
}

pub struct EnginePorts {
    pub commands: Consumer<AudioCommand>,
    pub retirements: Producer<CriticalEvent>,
    pub telemetry: Producer<Telemetry>,
}

pub fn audio_channels(
    command_capacity: usize,
    retirement_capacity: usize,
    telemetry_capacity: usize,
) -> (AudioController, EnginePorts) {
    let (command_producer, command_consumer) = RingBuffer::new(command_capacity);
    let (retirement_producer, retirement_consumer) = RingBuffer::new(retirement_capacity);
    let (telemetry_producer, telemetry_consumer) = RingBuffer::new(telemetry_capacity);

    (
        AudioController {
            commands: command_producer,
            retirements: retirement_consumer,
            telemetry: telemetry_consumer,
            free_slots: [true; SAMPLE_SLOT_COUNT],
            command_overflows: 0,
        },
        EnginePorts {
            commands: command_consumer,
            retirements: retirement_producer,
            telemetry: telemetry_producer,
        },
    )
}

impl AudioController {
    pub fn install(
        &mut self,
        pad: PadId,
        buffer: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, ControlError> {
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
        if self.push_command(command).is_err() {
            self.free_slots[index] = true;
            return Err(ControlError::CommandQueueFull);
        }

        Ok(slot)
    }

    pub fn trigger(
        &mut self,
        pad: PadId,
        at_frame: Frame,
        velocity: f32,
    ) -> Result<(), ControlError> {
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            return Err(ControlError::InvalidVelocity);
        }
        self.push_command(AudioCommand::Trigger {
            pad,
            at_frame,
            velocity,
        })
    }

    pub fn release(&mut self, pad: PadId, at_frame: Frame) -> Result<(), ControlError> {
        self.push_command(AudioCommand::Release { pad, at_frame })
    }

    pub fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), ControlError> {
        self.push_command(AudioCommand::UpdatePad { pad, settings })
    }

    pub fn stop_pad(&mut self, pad: PadId) -> Result<(), ControlError> {
        self.push_command(AudioCommand::StopPad { pad })
    }

    pub fn stop_all(&mut self) -> Result<(), ControlError> {
        self.push_command(AudioCommand::StopAll)
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
        self.command_overflows
    }

    fn push_command(&mut self, command: AudioCommand) -> Result<(), ControlError> {
        if self.commands.push(command).is_err() {
            self.command_overflows += 1;
            Err(ControlError::CommandQueueFull)
        } else {
            Ok(())
        }
    }
}
