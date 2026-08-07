use std::array;
use std::sync::Arc;

use rtrb::PushError;
use sampler_core::{
    Frame, PadId, PadSettings, PlaybackMode, VoiceAllocator, VoiceId, VoiceRequest,
};

use crate::{
    AudioCommand, CriticalEvent, EngineError, EnginePorts, SAMPLE_SLOT_COUNT, SampleBuffer,
    SampleSlot,
};

const VOICE_COUNT: usize = 32;
const PENDING_COUNT: usize = 128;
const MAX_COMMANDS_PER_RENDER: usize = 64;
const PAD_COUNT: usize = 160;
const PADS_PER_BANK: usize = 16;
const ATTACK_FRAMES: u8 = 32;

struct SampleEntry {
    buffer: Option<Arc<SampleBuffer>>,
    pad_references: u16,
    retiring: bool,
}

#[derive(Clone, Copy)]
struct PadBinding {
    slot: Option<SampleSlot>,
    settings: PadSettings,
}

#[derive(Clone, Copy)]
struct Envelope {
    attack_frame: u8,
}

impl Envelope {
    const fn attack() -> Self {
        Self { attack_frame: 0 }
    }

    fn next_gain(&mut self) -> f32 {
        if self.attack_frame < ATTACK_FRAMES {
            self.attack_frame += 1;
        }
        f32::from(self.attack_frame) / f32::from(ATTACK_FRAMES)
    }
}

#[derive(Clone, Copy)]
struct AudioVoice {
    id: VoiceId,
    slot: SampleSlot,
    position: f64,
    advance: f64,
    pad: PadId,
    mode: PlaybackMode,
    left_gain: f32,
    right_gain: f32,
    envelope: Envelope,
}

#[derive(Clone, Copy)]
enum ScheduledAction {
    Trigger {
        pad: PadId,
        at_frame: Frame,
        velocity: f32,
    },
    Release {
        pad: PadId,
        at_frame: Frame,
    },
}

impl ScheduledAction {
    fn at_frame(self) -> Frame {
        match self {
            Self::Trigger { at_frame, .. } | Self::Release { at_frame, .. } => at_frame,
        }
    }
}

pub struct AudioEngine {
    sample_rate: u32,
    ports: EnginePorts,
    samples: [SampleEntry; SAMPLE_SLOT_COUNT],
    pads: [PadBinding; PAD_COUNT],
    allocator: VoiceAllocator<VOICE_COUNT>,
    voices: [Option<AudioVoice>; VOICE_COUNT],
    pending: [Option<ScheduledAction>; PENDING_COUNT],
    pending_len: usize,
    deferred_retirement: Option<CriticalEvent>,
    rendered_frame: Frame,
    late_commands: u64,
    invalid_commands: u64,
}

impl AudioEngine {
    pub fn new(sample_rate: u32, ports: EnginePorts) -> Result<Self, EngineError> {
        if sample_rate == 0 {
            return Err(EngineError::ZeroSampleRate);
        }

        Ok(Self {
            sample_rate,
            ports,
            samples: array::from_fn(|_| SampleEntry {
                buffer: None,
                pad_references: 0,
                retiring: false,
            }),
            pads: [PadBinding {
                slot: None,
                settings: PadSettings::default(),
            }; PAD_COUNT],
            allocator: VoiceAllocator::new(),
            voices: [None; VOICE_COUNT],
            pending: [None; PENDING_COUNT],
            pending_len: 0,
            deferred_retirement: None,
            rendered_frame: 0,
            late_commands: 0,
            invalid_commands: 0,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn render_stereo(&mut self, output: &mut [f32]) {
        if !output.len().is_multiple_of(2) {
            output.fill(0.0);
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return;
        }

        let mut output_index = 0;
        self.render_frames(output.len() / 2, |frame| {
            output[output_index] = frame[0];
            output[output_index + 1] = frame[1];
            output_index += 2;
        });
    }

    pub fn render_frames(&mut self, frame_count: usize, mut write_frame: impl FnMut([f32; 2])) {
        self.flush_deferred_retirement();
        self.drain_commands();

        for _ in 0..frame_count {
            self.execute_due_actions();
            write_frame(self.render_frame());
            self.rendered_frame = self.rendered_frame.saturating_add(1);
        }
    }

    pub fn rendered_frame(&self) -> Frame {
        self.rendered_frame
    }

    pub fn active_voices(&self) -> usize {
        self.allocator.active_voices()
    }

    pub fn late_commands(&self) -> u64 {
        self.late_commands
    }

    pub fn invalid_commands(&self) -> u64 {
        self.invalid_commands
    }

    pub fn pending_actions(&self) -> usize {
        self.pending_len
    }

    pub fn queued_commands(&self) -> usize {
        self.ports.commands.slots()
    }

    fn drain_commands(&mut self) {
        let mut processed = 0;
        while processed < MAX_COMMANDS_PER_RENDER {
            let timed_action = match self.ports.commands.peek() {
                Ok(AudioCommand::Trigger {
                    pad,
                    at_frame,
                    velocity,
                }) => Some(ScheduledAction::Trigger {
                    pad: *pad,
                    at_frame: *at_frame,
                    velocity: *velocity,
                }),
                Ok(AudioCommand::Release { pad, at_frame }) => Some(ScheduledAction::Release {
                    pad: *pad,
                    at_frame: *at_frame,
                }),
                Ok(AudioCommand::InstallSample {
                    slot,
                    buffer,
                    settings,
                    ..
                }) if self.install_is_invalid(*slot, buffer, *settings)
                    && self.deferred_retirement.is_some() =>
                {
                    break;
                }
                Ok(_) => None,
                Err(_) => break,
            };

            if let Some(action) = timed_action {
                if self.pending_len == PENDING_COUNT {
                    break;
                }
                if self.ports.commands.pop().is_err() {
                    break;
                }
                self.insert_pending(action);
            } else {
                let Ok(command) = self.ports.commands.pop() else {
                    break;
                };
                self.execute_immediate(command);
            }
            processed += 1;
        }
    }

    fn execute_immediate(&mut self, command: AudioCommand) {
        match command {
            AudioCommand::InstallSample {
                pad,
                slot,
                buffer,
                settings,
            } => self.install_sample(pad, slot, buffer, settings),
            AudioCommand::UpdatePad { pad, settings } => {
                if settings_are_valid(settings) && self.pad_binding(pad).slot.is_some() {
                    self.pad_binding_mut(pad).settings = settings;
                } else {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                }
            }
            AudioCommand::StopPad { pad } => self.stop_pad(pad),
            AudioCommand::StopAll => self.stop_all(),
            AudioCommand::Trigger { .. } | AudioCommand::Release { .. } => {
                self.invalid_commands = self.invalid_commands.saturating_add(1);
            }
        }
    }

    fn install_sample(
        &mut self,
        pad: PadId,
        slot: SampleSlot,
        buffer: Arc<SampleBuffer>,
        settings: PadSettings,
    ) {
        if self.install_is_invalid(slot, &buffer, settings) {
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            self.return_rejected_buffer(slot, buffer);
            return;
        }

        let pad_index = pad_index(pad);
        if let Some(previous_slot) = self.pads[pad_index].slot {
            let previous = &mut self.samples[previous_slot.index()];
            previous.pad_references = previous.pad_references.saturating_sub(1);
            previous.retiring = true;
        }

        let entry = &mut self.samples[slot.index()];
        entry.buffer = Some(buffer);
        entry.pad_references = 1;
        entry.retiring = false;
        self.pads[pad_index] = PadBinding {
            slot: Some(slot),
            settings,
        };
    }

    fn install_is_invalid(
        &self,
        slot: SampleSlot,
        buffer: &SampleBuffer,
        settings: PadSettings,
    ) -> bool {
        let entry = &self.samples[slot.index()];
        entry.buffer.is_some()
            || entry.retiring
            || buffer.sample_rate() != self.sample_rate
            || !settings_are_valid(settings)
    }

    fn return_rejected_buffer(&mut self, slot: SampleSlot, buffer: Arc<SampleBuffer>) {
        let event = CriticalEvent::RetiredSample { slot, buffer };
        match self.ports.retirements.push(event) {
            Ok(()) => {}
            Err(PushError::Full(event)) => self.deferred_retirement = Some(event),
        }
    }

    fn flush_deferred_retirement(&mut self) {
        let Some(event) = self.deferred_retirement.take() else {
            return;
        };
        if let Err(PushError::Full(event)) = self.ports.retirements.push(event) {
            self.deferred_retirement = Some(event);
        }
    }

    fn insert_pending(&mut self, action: ScheduledAction) {
        let mut insert_at = self.pending_len;
        while insert_at > 0
            && self.pending[insert_at - 1]
                .is_some_and(|pending| pending.at_frame() > action.at_frame())
        {
            self.pending[insert_at] = self.pending[insert_at - 1];
            insert_at -= 1;
        }
        self.pending[insert_at] = Some(action);
        self.pending_len += 1;
    }

    fn execute_due_actions(&mut self) {
        while self.pending[0].is_some_and(|action| action.at_frame() <= self.rendered_frame) {
            let Some(action) = self.remove_first_pending() else {
                break;
            };
            if action.at_frame() < self.rendered_frame {
                self.late_commands = self.late_commands.saturating_add(1);
            }
            self.execute_action(action);
        }
    }

    fn remove_first_pending(&mut self) -> Option<ScheduledAction> {
        let action = self.pending[0]?;
        for index in 1..self.pending_len {
            self.pending[index - 1] = self.pending[index];
        }
        self.pending_len -= 1;
        self.pending[self.pending_len] = None;
        Some(action)
    }

    fn execute_action(&mut self, action: ScheduledAction) {
        match action {
            ScheduledAction::Trigger { pad, velocity, .. } => self.trigger(pad, velocity),
            ScheduledAction::Release { pad, .. } => {
                if self.pad_binding(pad).slot.is_none() {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                }
            }
        }
    }

    fn trigger(&mut self, pad: PadId, velocity: f32) {
        let binding = *self.pad_binding(pad);
        let Some(slot) = binding.slot else {
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return;
        };
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return;
        }
        if self.samples[slot.index()].buffer.is_none() {
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return;
        }

        let allocation = self.allocator.trigger(VoiceRequest::new(
            pad,
            self.rendered_frame,
            velocity,
            binding.settings.choke_group,
            false,
        ));
        self.voices[allocation.slot] = Some(AudioVoice {
            id: allocation.voice.id,
            slot,
            position: 0.0,
            advance: 1.0,
            pad,
            mode: binding.settings.mode,
            left_gain: velocity,
            right_gain: velocity,
            envelope: Envelope::attack(),
        });
    }

    fn stop_pad(&mut self, pad: PadId) {
        for slot in 0..VOICE_COUNT {
            if self.voices[slot].is_some_and(|voice| voice.pad == pad) {
                self.stop_voice(slot);
            }
        }
    }

    fn stop_all(&mut self) {
        for slot in 0..VOICE_COUNT {
            if self.voices[slot].is_some() {
                self.stop_voice(slot);
            }
        }
    }

    fn stop_voice(&mut self, slot: usize) {
        if let Some(voice) = self.voices[slot].take() {
            let _ = self.allocator.stop_slot(slot, voice.id);
        }
    }

    fn render_frame(&mut self) -> [f32; 2] {
        let mut output = [0.0, 0.0];
        for slot in 0..VOICE_COUNT {
            let Some(mut voice) = self.voices[slot] else {
                continue;
            };

            let sample = self.samples[voice.slot.index()]
                .buffer
                .as_ref()
                .and_then(|buffer| buffer.frame_linear(voice.position));
            let Some(sample) = sample else {
                self.invalid_commands = self.invalid_commands.saturating_add(1);
                self.stop_voice(slot);
                continue;
            };

            let envelope_gain = voice.envelope.next_gain();
            output[0] += sample[0] * voice.left_gain * envelope_gain;
            output[1] += sample[1] * voice.right_gain * envelope_gain;
            voice.position += voice.advance;

            let sample_frames = self.samples[voice.slot.index()]
                .buffer
                .as_ref()
                .map_or(0, |buffer| buffer.frames());
            let finished = matches!(
                voice.mode,
                PlaybackMode::OneShot | PlaybackMode::Gate | PlaybackMode::Loop
            ) && voice.position >= sample_frames as f64;
            if finished {
                self.stop_voice(slot);
            } else {
                self.voices[slot] = Some(voice);
            }
        }
        output
    }

    fn pad_binding(&self, pad: PadId) -> &PadBinding {
        &self.pads[pad_index(pad)]
    }

    fn pad_binding_mut(&mut self, pad: PadId) -> &mut PadBinding {
        &mut self.pads[pad_index(pad)]
    }
}

fn pad_index(pad: PadId) -> usize {
    usize::from(u8::from(pad.bank())) * PADS_PER_BANK + usize::from(pad.index())
}

fn settings_are_valid(settings: PadSettings) -> bool {
    settings.gain_db.is_finite()
        && (-60.0..=6.0).contains(&settings.gain_db)
        && settings.pan.is_finite()
        && (-1.0..=1.0).contains(&settings.pan)
        && settings.pitch_semitones.is_finite()
        && (-24.0..=24.0).contains(&settings.pitch_semitones)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{AudioController, PadId, PadSettings, SampleBuffer, audio_channels};

    fn harness() -> (AudioController, AudioEngine) {
        let (controller, ports) = audio_channels();
        (controller, AudioEngine::new(48_000, ports).unwrap())
    }

    fn constant_sample(frames: usize, value: f32) -> Arc<SampleBuffer> {
        Arc::new(SampleBuffer::new(48_000, vec![value; frames * 2]).unwrap())
    }

    #[test]
    fn trigger_starts_at_the_exact_frame_inside_a_block() {
        let (mut controller, mut engine) = harness();
        let sample = constant_sample(8, 1.0);
        controller
            .install(PadId::first(), sample, PadSettings::default())
            .unwrap();
        controller.trigger(PadId::first(), 3, 1.0).unwrap();
        let mut output = [0.0; 16];
        engine.render_stereo(&mut output);
        assert_eq!(&output[..6], &[0.0; 6]);
        assert!(output[6] > 0.0 && output[7] > 0.0);
    }

    #[test]
    fn one_shot_finishes_and_late_trigger_is_counted() {
        let (mut controller, mut engine) = harness();
        let sample = constant_sample(2, 1.0);
        controller
            .install(PadId::first(), sample, PadSettings::default())
            .unwrap();
        let mut warmup = [0.0; 8];
        engine.render_stereo(&mut warmup);
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        engine.render_stereo(&mut warmup);
        assert_eq!(engine.active_voices(), 0);
        assert_eq!(engine.late_commands(), 1);
    }

    #[test]
    fn full_pending_array_leaves_additional_actions_in_the_command_queue() {
        let (mut controller, mut engine) = harness();
        for frame in 1000..1130 {
            controller.trigger(PadId::first(), frame, 1.0).unwrap();
        }
        let mut output = [0.0; 2];
        for _ in 0..3 {
            engine.render_stereo(&mut output);
        }
        assert_eq!(engine.pending_actions(), 128);
        assert_eq!(engine.queued_commands(), 2);
    }
}
