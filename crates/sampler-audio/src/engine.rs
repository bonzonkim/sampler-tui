use std::array;
use std::f32::consts::PI;
use std::sync::Arc;

use rtrb::PushError;
use sampler_core::{
    Frame, PadId, PadSettings, PlaybackMode, VoiceAllocator, VoiceId, VoiceRequest,
};

use crate::{
    AudioCommand, CriticalEvent, EngineError, EnginePorts, SAMPLE_SLOT_COUNT, SampleBuffer,
    SampleSlot, Telemetry,
};

const VOICE_COUNT: usize = 32;
const PENDING_COUNT: usize = 128;
const MAX_COMMANDS_PER_RENDER: usize = 64;
const PAD_COUNT: usize = 160;
const PADS_PER_BANK: usize = 16;
const ATTACK_FRAMES: u8 = 32;
const RELEASE_FRAMES: u8 = 64;

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
    release_frame: Option<u8>,
    release_start_gain: f32,
}

impl Envelope {
    const fn attack() -> Self {
        Self {
            attack_frame: 0,
            release_frame: None,
            release_start_gain: 0.0,
        }
    }

    fn begin_release(&mut self) {
        if self.release_frame.is_none() {
            self.release_start_gain = f32::from(self.attack_frame) / f32::from(ATTACK_FRAMES);
            self.release_frame = Some(0);
        }
    }

    fn next_gain(&mut self) -> (f32, bool) {
        if let Some(release_frame) = self.release_frame {
            let release_frame = release_frame.saturating_add(1).min(RELEASE_FRAMES);
            self.release_frame = Some(release_frame);
            return (
                self.release_start_gain * f32::from(RELEASE_FRAMES - release_frame)
                    / f32::from(RELEASE_FRAMES),
                release_frame == RELEASE_FRAMES,
            );
        }

        if self.attack_frame < ATTACK_FRAMES {
            self.attack_frame += 1;
        }
        (
            f32::from(self.attack_frame) / f32::from(ATTACK_FRAMES),
            false,
        )
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
    choke_group: Option<sampler_core::ChokeGroup>,
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
        sequence: u64,
    },
    Release {
        pad: PadId,
        at_frame: Frame,
        sequence: u64,
    },
}

#[derive(Clone, Copy)]
enum CommandLane {
    Immediate,
    Timed,
}

impl CommandLane {
    fn other(self) -> Self {
        match self {
            Self::Immediate => Self::Timed,
            Self::Timed => Self::Immediate,
        }
    }
}

impl ScheduledAction {
    fn at_frame(self) -> Frame {
        match self {
            Self::Trigger { at_frame, .. } | Self::Release { at_frame, .. } => at_frame,
        }
    }

    fn sequence(self) -> u64 {
        match self {
            Self::Trigger { sequence, .. } | Self::Release { sequence, .. } => sequence,
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
    next_command_lane: CommandLane,
    active_stop_fence: Option<u64>,
    deferred_retirement: Option<CriticalEvent>,
    rendered_frame: Frame,
    last_triggered_frame: Option<Frame>,
    late_commands: u64,
    invalid_commands: u64,
    executed_triggers: u64,
    telemetry_peak_left: f32,
    telemetry_peak_right: f32,
    next_telemetry_frame: Frame,
}

impl AudioEngine {
    pub fn new(sample_rate: u32, ports: EnginePorts) -> Result<Self, EngineError> {
        if sample_rate == 0 {
            return Err(EngineError::ZeroSampleRate);
        }

        let telemetry_interval = telemetry_interval(sample_rate);
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
            next_command_lane: CommandLane::Immediate,
            active_stop_fence: None,
            deferred_retirement: None,
            rendered_frame: 0,
            last_triggered_frame: None,
            late_commands: 0,
            invalid_commands: 0,
            executed_triggers: 0,
            telemetry_peak_left: 0.0,
            telemetry_peak_right: 0.0,
            next_telemetry_frame: telemetry_interval,
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
        let frame_count_as_frame = Frame::try_from(frame_count).unwrap_or(Frame::MAX);
        let horizon = self.rendered_frame.saturating_add(frame_count_as_frame);
        self.ports.publish_render_horizon(horizon);

        self.flush_deferred_retirement();
        self.apply_stop_fence();
        self.drain_commands();

        for _ in 0..frame_count {
            self.execute_due_actions();
            let frame = self.render_frame();
            self.telemetry_peak_left = self.telemetry_peak_left.max(frame[0].abs());
            self.telemetry_peak_right = self.telemetry_peak_right.max(frame[1].abs());
            write_frame(frame);
            self.rendered_frame = self.rendered_frame.saturating_add(1);
            self.emit_telemetry_if_due();
        }

        self.retire_unused_samples();
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

    pub fn executed_triggers(&self) -> u64 {
        self.executed_triggers
    }

    pub fn pending_actions(&self) -> usize {
        self.pending_len
    }

    pub fn queued_commands(&self) -> usize {
        self.ports.queued_commands()
    }

    #[cfg(test)]
    fn set_rendered_frame_for_test(&mut self, frame: Frame) {
        self.rendered_frame = frame;
    }

    #[cfg(test)]
    fn voices_for_pad(&self, pad: PadId) -> usize {
        self.voices
            .iter()
            .flatten()
            .filter(|voice| voice.pad == pad)
            .count()
    }

    fn drain_commands(&mut self) {
        for _ in 0..MAX_COMMANDS_PER_RENDER {
            let preferred = self.next_command_lane;
            if self.drain_one_command(preferred) {
                self.next_command_lane = preferred.other();
                continue;
            }
            let fallback = preferred.other();
            if self.drain_one_command(fallback) {
                self.next_command_lane = preferred;
                continue;
            }
            break;
        }
    }

    fn drain_one_command(&mut self, lane: CommandLane) -> bool {
        match lane {
            CommandLane::Immediate => self.drain_one_immediate_command(),
            CommandLane::Timed => self.drain_one_timed_command(),
        }
    }

    fn drain_one_immediate_command(&mut self) -> bool {
        let live_action = match self.ports.immediate_commands.peek() {
            Ok(AudioCommand::TriggerLive {
                pad,
                velocity,
                sequence,
            }) => Some(ScheduledAction::Trigger {
                pad: *pad,
                at_frame: self.rendered_frame,
                velocity: *velocity,
                sequence: *sequence,
            }),
            Ok(AudioCommand::ReleaseLive { pad, sequence }) => Some(ScheduledAction::Release {
                pad: *pad,
                at_frame: self.rendered_frame,
                sequence: *sequence,
            }),
            Ok(AudioCommand::InstallSample {
                slot,
                buffer,
                settings,
                ..
            }) if self.install_is_invalid(*slot, buffer, *settings)
                && self.deferred_retirement.is_some() =>
            {
                return false;
            }
            Ok(_) => None,
            Err(_) => return false,
        };
        if live_action.is_some() {
            self.apply_stop_fence();
        }

        let Ok(command) = self.ports.immediate_commands.pop() else {
            return false;
        };
        if let Some(action) = live_action {
            if !self.action_is_stopped(action) {
                self.execute_action(action);
            }
        } else {
            self.execute_immediate(command);
        }
        true
    }

    fn drain_one_timed_command(&mut self) -> bool {
        let timed_action = match self.ports.commands.peek() {
            Ok(AudioCommand::Trigger {
                pad,
                at_frame,
                velocity,
                sequence,
            }) => Some(ScheduledAction::Trigger {
                pad: *pad,
                at_frame: *at_frame,
                velocity: *velocity,
                sequence: *sequence,
            }),
            Ok(AudioCommand::Release {
                pad,
                at_frame,
                sequence,
            }) => Some(ScheduledAction::Release {
                pad: *pad,
                at_frame: *at_frame,
                sequence: *sequence,
            }),
            Ok(_) => None,
            Err(_) => return false,
        };
        if timed_action.is_some() {
            self.apply_stop_fence();
        }

        if let Some(action) = timed_action {
            if !self.action_is_stopped(action) && self.pending_len == PENDING_COUNT {
                return false;
            }
            if self.ports.commands.pop().is_err() {
                return false;
            }
            if !self.action_is_stopped(action) {
                self.insert_pending(action);
            }
        } else {
            let Ok(command) = self.ports.commands.pop() else {
                return false;
            };
            self.execute_immediate(command);
        }
        true
    }

    fn execute_immediate(&mut self, command: AudioCommand) {
        match command {
            AudioCommand::InstallSample {
                pad,
                slot,
                buffer,
                settings,
                ..
            } => self.install_sample(pad, slot, buffer, settings),
            AudioCommand::UpdatePad { pad, settings } => {
                if settings_are_valid(settings) && self.pad_binding(pad).slot.is_some() {
                    self.pad_binding_mut(pad).settings = settings;
                } else {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                }
            }
            AudioCommand::StopPad { pad } => self.stop_pad(pad),
            AudioCommand::Trigger { .. }
            | AudioCommand::TriggerLive { .. }
            | AudioCommand::Release { .. }
            | AudioCommand::ReleaseLive { .. } => {
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

    fn apply_stop_fence(&mut self) {
        let Some(fence_sequence) = self.ports.take_stop_fence() else {
            return;
        };

        self.active_stop_fence = Some(fence_sequence);
        self.stop_all();

        let old_len = self.pending_len;
        let mut retained = 0;
        for index in 0..old_len {
            let Some(action) = self.pending[index] else {
                continue;
            };
            if !self.action_is_stopped(action) {
                self.pending[retained] = Some(action);
                retained += 1;
            }
        }
        for index in retained..old_len {
            self.pending[index] = None;
        }
        self.pending_len = retained;
    }

    fn action_is_stopped(&self, action: ScheduledAction) -> bool {
        self.active_stop_fence
            .is_some_and(|fence| sequence_is_at_or_before(action.sequence(), fence))
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
            ScheduledAction::Trigger { pad, velocity, .. } => {
                if self.trigger(pad, velocity) {
                    self.executed_triggers = self.executed_triggers.saturating_add(1);
                    self.last_triggered_frame = Some(self.rendered_frame);
                }
            }
            ScheduledAction::Release { pad, .. } => {
                if self.pad_binding(pad).slot.is_none() {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                } else {
                    self.release_gate_voices(pad);
                }
            }
        }
    }

    fn trigger(&mut self, pad: PadId, velocity: f32) -> bool {
        let binding = *self.pad_binding(pad);
        let Some(slot) = binding.slot else {
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return false;
        };
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return false;
        }
        if self.samples[slot.index()].buffer.is_none() {
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return false;
        }

        if let Some(group) = binding.settings.choke_group {
            self.release_choke_group(group);
        }

        let gain = 10.0_f32.powf(binding.settings.gain_db / 20.0) * velocity;
        let pan_angle = (binding.settings.pan + 1.0) * PI / 4.0;

        let advance = 2.0_f64.powf(f64::from(binding.settings.pitch_semitones) / 12.0_f64);
        let allocation = self.allocator.trigger(VoiceRequest::new(
            pad,
            self.rendered_frame,
            gain,
            binding.settings.choke_group,
            false,
        ));
        self.voices[allocation.slot] = Some(AudioVoice {
            id: allocation.voice.id,
            slot,
            position: 0.0,
            advance,
            pad,
            mode: binding.settings.mode,
            choke_group: binding.settings.choke_group,
            left_gain: gain * pan_angle.cos(),
            right_gain: gain * pan_angle.sin(),
            envelope: Envelope::attack(),
        });
        true
    }

    fn stop_pad(&mut self, pad: PadId) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.pad == pad {
                voice.envelope.begin_release();
            }
        }
    }

    fn stop_all(&mut self) {
        for voice in self.voices.iter_mut().flatten() {
            voice.envelope.begin_release();
        }
    }

    fn release_gate_voices(&mut self, pad: PadId) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.pad == pad && voice.mode == PlaybackMode::Gate {
                voice.envelope.begin_release();
            }
        }
    }

    fn release_choke_group(&mut self, group: sampler_core::ChokeGroup) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.choke_group == Some(group) {
                voice.envelope.begin_release();
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
                .and_then(|buffer| voice_sample(buffer, voice.position, voice.mode));
            let Some(sample) = sample else {
                self.invalid_commands = self.invalid_commands.saturating_add(1);
                self.stop_voice(slot);
                continue;
            };

            let (envelope_gain, release_finished) = voice.envelope.next_gain();
            let left = finite_or_zero(
                sample[0] * voice.left_gain * envelope_gain,
                &mut self.invalid_commands,
            );
            let right = finite_or_zero(
                sample[1] * voice.right_gain * envelope_gain,
                &mut self.invalid_commands,
            );
            output[0] = finite_or_zero(output[0] + left, &mut self.invalid_commands);
            output[1] = finite_or_zero(output[1] + right, &mut self.invalid_commands);
            voice.position += voice.advance;

            let sample_frames = self.samples[voice.slot.index()]
                .buffer
                .as_ref()
                .map_or(0, |buffer| buffer.frames());
            if voice.mode == PlaybackMode::Loop && voice.position >= sample_frames as f64 {
                voice.position %= sample_frames as f64;
            }
            let finished = release_finished
                || (voice.mode != PlaybackMode::Loop && voice.position >= sample_frames as f64);
            if finished {
                self.stop_voice(slot);
            } else {
                self.voices[slot] = Some(voice);
            }
        }
        [
            soft_limit(output[0], &mut self.invalid_commands),
            soft_limit(output[1], &mut self.invalid_commands),
        ]
    }

    fn emit_telemetry_if_due(&mut self) {
        if self.rendered_frame < self.next_telemetry_frame {
            return;
        }

        let telemetry = Telemetry {
            active_pads: self.active_pad_bits(),
            rendered_frame: self.rendered_frame,
            last_triggered_frame: self.last_triggered_frame,
            peak_left: self.telemetry_peak_left,
            peak_right: self.telemetry_peak_right,
            active_voices: self.allocator.active_voices(),
            late_commands: self.late_commands,
            invalid_commands: self.invalid_commands,
            command_overflows: self.ports.command_overflows(),
        };
        let _ = self.ports.telemetry.push(telemetry);
        self.telemetry_peak_left = 0.0;
        self.telemetry_peak_right = 0.0;
        self.next_telemetry_frame = self
            .next_telemetry_frame
            .saturating_add(telemetry_interval(self.sample_rate));
    }

    fn active_pad_bits(&self) -> [u64; 3] {
        let mut bits = [0; 3];
        for voice in self.voices.iter().flatten() {
            let index = usize::from(u8::from(voice.pad.bank())) * PADS_PER_BANK
                + usize::from(voice.pad.index());
            bits[index / u64::BITS as usize] |= 1u64 << (index % u64::BITS as usize);
        }
        bits
    }

    fn retire_unused_samples(&mut self) {
        for index in 0..SAMPLE_SLOT_COUNT {
            let entry = &self.samples[index];
            if !entry.retiring
                || entry.pad_references != 0
                || self
                    .voices
                    .iter()
                    .flatten()
                    .any(|voice| voice.slot.index() == index)
                || self.ports.retirements.slots() == 0
            {
                continue;
            }

            let Some(buffer) = self.samples[index].buffer.take() else {
                self.samples[index].retiring = false;
                self.invalid_commands = self.invalid_commands.saturating_add(1);
                continue;
            };
            let Ok(slot) = SampleSlot::new(index) else {
                self.samples[index].buffer = Some(buffer);
                self.invalid_commands = self.invalid_commands.saturating_add(1);
                continue;
            };
            let event = CriticalEvent::RetiredSample { slot, buffer };
            match self.ports.retirements.push(event) {
                Ok(()) => self.samples[index].retiring = false,
                Err(PushError::Full(CriticalEvent::RetiredSample { buffer, .. })) => {
                    self.samples[index].buffer = Some(buffer);
                }
            }
        }
    }

    fn pad_binding(&self, pad: PadId) -> &PadBinding {
        &self.pads[pad_index(pad)]
    }

    fn pad_binding_mut(&mut self, pad: PadId) -> &mut PadBinding {
        &mut self.pads[pad_index(pad)]
    }
}

fn finite_or_zero(value: f32, invalid_commands: &mut u64) -> f32 {
    if value.is_finite() {
        value
    } else {
        *invalid_commands = invalid_commands.saturating_add(1);
        0.0
    }
}

fn soft_limit(value: f32, invalid_commands: &mut u64) -> f32 {
    let value = finite_or_zero(value, invalid_commands);
    finite_or_zero(value / (1.0 + value.abs()), invalid_commands)
}

fn voice_sample(buffer: &SampleBuffer, position: f64, mode: PlaybackMode) -> Option<[f32; 2]> {
    if mode != PlaybackMode::Loop {
        return buffer.frame_linear(position);
    }
    if !position.is_finite() || position.is_sign_negative() || position >= buffer.frames() as f64 {
        return None;
    }

    let frame = position as usize;
    let next_frame = (frame + 1) % buffer.frames();
    let fraction = position - frame as f64;
    let data = buffer.data();
    let current = [data[frame * 2], data[frame * 2 + 1]];
    let next = [data[next_frame * 2], data[next_frame * 2 + 1]];
    Some([
        (f64::from(current[0]) * (1.0 - fraction) + f64::from(next[0]) * fraction) as f32,
        (f64::from(current[1]) * (1.0 - fraction) + f64::from(next[1]) * fraction) as f32,
    ])
}

fn telemetry_interval(sample_rate: u32) -> Frame {
    Frame::from(sample_rate.div_ceil(30).max(1))
}

fn sequence_is_at_or_before(sequence: u64, fence: u64) -> bool {
    // Sequence numbers use serial-number arithmetic, so a fence remains causal
    // across u64 wrap. Outstanding commands are many orders of magnitude below
    // the half-range where serial-number ordering becomes ambiguous.
    fence.wrapping_sub(sequence) < (1_u64 << 63)
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
    use std::sync::{Arc, Weak};

    use super::*;
    use crate::{
        AudioController, PadId, PadSettings, SampleBuffer, audio_channels,
        command::{RECOVERY_COMMAND_CAPACITY, audio_channels_with_capacities},
    };
    use sampler_core::{BankId, ChokeGroup};

    fn harness() -> (AudioController, AudioEngine) {
        let (controller, ports) = audio_channels();
        (controller, AudioEngine::new(48_000, ports).unwrap())
    }

    fn constant_sample(frames: usize, value: f32) -> Arc<SampleBuffer> {
        Arc::new(SampleBuffer::new(48_000, vec![value; frames * 2]).unwrap())
    }

    #[test]
    fn render_horizon_is_visible_before_the_first_frame_is_written() {
        let (controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        let mut seen = Vec::new();

        engine.render_frames(257, |_| seen.push(controller.render_horizon()));

        assert_eq!(seen, vec![257; 257]);
        assert_eq!(controller.render_horizon(), 257);
    }

    #[test]
    fn render_horizon_saturates_instead_of_panicking() {
        let (controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        engine.set_rendered_frame_for_test(u64::MAX - 2);

        engine.render_frames(8, |_| {});

        assert_eq!(controller.render_horizon(), u64::MAX);
    }

    #[test]
    fn live_trigger_enqueued_after_a_large_callback_drain_runs_at_the_next_callback_start() {
        let (mut controller, mut engine) = harness();
        controller
            .install(
                PadId::first(),
                constant_sample(1_024, 0.25),
                PadSettings::default(),
            )
            .unwrap();
        engine.render_frames(1, |_| {});
        let mut queued = false;

        engine.render_frames(512, |_| {
            if !queued {
                controller.trigger_live(PadId::first(), 1.0).unwrap();
                queued = true;
            }
        });
        assert_eq!(engine.executed_triggers(), 0);

        engine.render_frames(1, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(513));
        assert_eq!(engine.late_commands(), 0);
    }

    #[test]
    fn live_trigger_runs_when_the_future_action_array_is_full() {
        let (mut controller, mut engine) = harness();
        controller
            .install(
                PadId::first(),
                constant_sample(1_024, 0.25),
                PadSettings::default(),
            )
            .unwrap();
        engine.render_frames(1, |_| {});
        for _ in 0..PENDING_COUNT {
            controller.trigger(PadId::first(), 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pending_actions(), PENDING_COUNT);

        controller.trigger_live(PadId::first(), 1.0).unwrap();
        engine.render_frames(1, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(1));
        assert_eq!(engine.pending_actions(), PENDING_COUNT);
    }

    #[test]
    fn live_input_bypasses_a_blocked_timed_command_without_overtaking_setup() {
        let (mut controller, mut engine) = harness();
        let first = PadId::first();
        let second = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        controller
            .install(first, constant_sample(1_024, 0.25), PadSettings::default())
            .unwrap();
        engine.render_frames(64, |_| {});
        for _ in 0..PENDING_COUNT {
            controller.trigger(first, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pending_actions(), PENDING_COUNT);

        controller.trigger(first, 20_000, 1.0).unwrap();
        controller
            .install(second, constant_sample(1_024, 0.5), PadSettings::default())
            .unwrap();
        let gate = PadSettings::new(PlaybackMode::Gate, 0.0, 0.0, 0.0, None).unwrap();
        controller.update_pad(second, gate).unwrap();
        controller.trigger_live(second, 1.0).unwrap();
        controller.release_live(second).unwrap();
        for _ in 0..61 {
            controller.stop_pad(first).unwrap();
        }

        engine.render_frames(64, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(64));
        assert_eq!(engine.voices_for_pad(second), 0);
        assert_eq!(engine.pending_actions(), PENDING_COUNT);
        assert_eq!(engine.queued_commands(), 2);
        assert_eq!(engine.late_commands(), 0);
    }

    #[test]
    fn sustained_immediate_traffic_does_not_starve_absolute_timed_action() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        controller
            .install(pad, constant_sample(1_024, 0.25), PadSettings::default())
            .unwrap();
        engine.render_frames(1, |_| {});
        let at_frame = engine.rendered_frame() + 1;
        controller.trigger(pad, at_frame, 1.0).unwrap();

        for _ in 0..3 {
            for _ in 0..MAX_COMMANDS_PER_RENDER {
                controller.update_pad(pad, PadSettings::default()).unwrap();
            }
            engine.render_frames(1, |_| {});
        }

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(at_frame));
        assert_eq!(engine.late_commands(), 0);
    }

    #[test]
    fn sustained_timed_traffic_does_not_starve_live_input() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        controller
            .install(pad, constant_sample(1_024, 0.25), PadSettings::default())
            .unwrap();
        engine.render_frames(1, |_| {});
        for _ in 0..MAX_COMMANDS_PER_RENDER {
            controller.trigger(pad, 10_000, 1.0).unwrap();
        }
        controller.trigger_live(pad, 1.0).unwrap();

        engine.render_frames(1, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(1));
        assert_eq!(engine.queued_commands(), 1);
        assert_eq!(engine.late_commands(), 0);
    }

    #[test]
    fn stop_all_cancels_blocked_timed_actions_but_not_newer_live_input() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(pad, constant_sample(1_024, 0.25), settings)
            .unwrap();
        engine.render_frames(64, |_| {});
        for _ in 0..PENDING_COUNT {
            controller.trigger(pad, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        engine.render_frames(0, |_| {});

        controller.trigger(pad, 20_000, 1.0).unwrap();
        controller.trigger_live(pad, 1.0).unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(engine.executed_triggers(), 1);

        controller.stop_all().unwrap();
        controller.trigger_live(pad, 1.0).unwrap();
        engine.render_frames(1, |_| {});

        assert_eq!(engine.executed_triggers(), 2);
        assert_eq!(engine.last_triggered_frame, Some(64));
        assert_eq!(engine.pending_actions(), 0);
        assert_eq!(engine.queued_commands(), 0);
    }

    #[test]
    fn post_fence_actions_observed_after_the_initial_poll_survive_stop_all() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(pad, constant_sample(1_024, 0.25), settings)
            .unwrap();
        engine.render_frames(1, |_| {});

        engine.apply_stop_fence();
        controller.stop_all().unwrap();
        controller.trigger_live(pad, 1.0).unwrap();
        controller
            .trigger(pad, engine.rendered_frame(), 1.0)
            .unwrap();
        engine.drain_commands();
        engine.execute_due_actions();
        assert_eq!(engine.executed_triggers(), 2);
        assert_eq!(engine.voices_for_pad(pad), 2);

        engine.render_frames(usize::from(RELEASE_FRAMES), |_| {});

        assert_eq!(engine.voices_for_pad(pad), 2);
    }

    #[test]
    fn recovery_installs_leave_room_for_live_input_in_one_drain_budget() {
        let (mut controller, mut engine) = harness();
        let mut accepted_installs = 0;
        for index in 0..PAD_COUNT {
            let bank = BankId::new((index / PADS_PER_BANK) as u8).unwrap();
            let pad = PadId::new(bank, (index % PADS_PER_BANK) as u8).unwrap();
            if controller
                .install_recovery(pad, constant_sample(8, 0.25), PadSettings::default())
                .is_ok()
            {
                accepted_installs += 1;
            }
        }
        controller.trigger_live(PadId::first(), 1.0).unwrap();

        engine.render_frames(1, |_| {});

        assert_eq!(accepted_installs, RECOVERY_COMMAND_CAPACITY);
        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.late_commands(), 0);
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

    fn stop_all_outcome(saturate_pending: bool) -> (usize, usize, usize, usize) {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(8, 0.5), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        engine.render_frames(32, |_| {});
        assert_eq!(engine.active_voices(), 1);

        if saturate_pending {
            for frame in 10_000..10_130 {
                controller.trigger(PadId::first(), frame, 1.0).unwrap();
            }
            engine.render_frames(0, |_| {});
            engine.render_frames(0, |_| {});
            engine.render_frames(0, |_| {});
            assert_eq!(engine.pending_actions(), PENDING_COUNT);
            assert_eq!(engine.queued_commands(), 2);
        }

        controller.stop_all().unwrap();
        let post_fence_frame = engine.rendered_frame() + Frame::from(RELEASE_FRAMES);
        controller
            .trigger(PadId::first(), post_fence_frame, 1.0)
            .unwrap();

        engine.render_frames(usize::from(RELEASE_FRAMES), |_| {});
        let stopped = (
            engine.active_voices(),
            engine.pending_actions(),
            engine.queued_commands(),
        );
        engine.render_frames(1, |_| {});
        (stopped.0, stopped.1, stopped.2, engine.active_voices())
    }

    #[test]
    fn stop_all_is_the_same_causal_fence_with_empty_or_full_pending_storage() {
        let empty = stop_all_outcome(false);
        let full = stop_all_outcome(true);

        assert_eq!(empty, (0, 1, 0, 1));
        assert_eq!(full, empty);
    }

    #[test]
    fn stop_fence_sequence_order_wraps_without_canceling_newer_commands() {
        assert!(sequence_is_at_or_before(u64::MAX, 0));
        assert!(sequence_is_at_or_before(0, 0));
        assert!(!sequence_is_at_or_before(1, 0));
    }

    #[test]
    fn gate_release_reaches_silence_after_64_frames() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Gate, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(256, 1.0), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        controller.release(PadId::first(), 32).unwrap();
        let mut output = vec![0.0; 2 * 128];
        engine.render_stereo(&mut output);
        assert_eq!(&output[2 * 96..], &[0.0; 64]);
        assert_eq!(engine.active_voices(), 0);
    }

    #[test]
    fn release_during_attack_does_not_jump_above_the_current_level() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Gate, 0.0, -1.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(256, 1.0), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        controller.release(PadId::first(), 8).unwrap();
        let mut output = [0.0; 2 * 9];
        engine.render_stereo(&mut output);
        assert!(output[16] <= output[14]);
    }

    #[test]
    fn loop_wraps_without_finishing_until_stopped() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(2, 0.5), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let mut output = [0.0; 20];
        engine.render_stereo(&mut output);
        assert_eq!(engine.active_voices(), 1);
        assert!(output.iter().skip(4).any(|value| *value != 0.0));
    }

    #[test]
    fn fractional_loop_pitch_interpolates_from_the_last_frame_to_the_first() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, -1.0, -12.0, None).unwrap();
        let ramp = Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0, 1.0, 1.0]).unwrap());
        controller.install(PadId::first(), ramp, settings).unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let mut output = [0.0; 2 * 36];
        engine.render_stereo(&mut output);
        assert!((output[2 * 33] - output[2 * 35]).abs() < 1.0e-6);
    }

    #[test]
    fn octave_up_advances_two_source_frames() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::OneShot, 0.0, -1.0, 12.0, None).unwrap();
        let ramp = Arc::new(
            SampleBuffer::new(
                48_000,
                vec![0.0, 0.0, 0.2, 0.2, 0.4, 0.4, 0.6, 0.6, 0.8, 0.8, 1.0, 1.0],
            )
            .unwrap(),
        );
        controller.install(PadId::first(), ramp, settings).unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let mut output = [0.0; 6];
        engine.render_stereo(&mut output);
        assert!(output[2] > output[0]);
        assert!(output[4] > output[2]);
    }

    #[test]
    fn hard_left_equal_power_pan_silences_right_channel() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, -1.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(8, 1.0), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let mut output = [0.0; 16];
        engine.render_stereo(&mut output);
        assert!(output.chunks_exact(2).all(|frame| frame[1] == 0.0));
    }

    #[test]
    fn triggering_a_choke_group_releases_the_prior_pad() {
        let (mut controller, mut engine) = harness();
        let group = Some(ChokeGroup::new(1).unwrap());
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, group).unwrap();
        let second = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        controller
            .install(PadId::first(), constant_sample(256, 1.0), settings)
            .unwrap();
        controller
            .install(second, constant_sample(256, 0.5), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        controller.trigger(second, 32, 1.0).unwrap();
        let mut output = [0.0; 2 * 128];
        engine.render_stereo(&mut output);
        assert_eq!(engine.voices_for_pad(PadId::first()), 0);
        assert_eq!(engine.voices_for_pad(second), 1);
    }

    #[test]
    fn thirty_three_triggers_leave_exactly_thirty_two_voices() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(256, 1.0), settings)
            .unwrap();
        for frame in 0..33 {
            controller
                .trigger(PadId::first(), frame, 0.5 + frame as f32 / 100.0)
                .unwrap();
        }
        let mut output = [0.0; 2 * 64];
        engine.render_stereo(&mut output);
        assert_eq!(engine.active_voices(), 32);
    }

    #[test]
    fn stealing_uses_velocity_times_pad_gain_as_the_audible_level() {
        let (mut controller, mut engine) = harness();
        let bank = BankId::new(0).unwrap();
        let quiet_by_gain = PadId::new(bank, 0).unwrap();
        let lower_velocity = PadId::new(bank, 1).unwrap();
        let filler = PadId::new(bank, 2).unwrap();
        let newcomer = PadId::new(bank, 3).unwrap();
        let quiet_settings = PadSettings::new(PlaybackMode::Loop, -60.0, 0.0, 0.0, None).unwrap();
        let normal_settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();

        for (pad, settings) in [
            (quiet_by_gain, quiet_settings),
            (lower_velocity, normal_settings),
            (filler, normal_settings),
            (newcomer, normal_settings),
        ] {
            controller
                .install(pad, constant_sample(8, 0.25), settings)
                .unwrap();
        }
        controller.trigger(quiet_by_gain, 0, 1.0).unwrap();
        controller.trigger(lower_velocity, 0, 0.2).unwrap();
        for _ in 0..30 {
            controller.trigger(filler, 0, 1.0).unwrap();
        }
        controller.trigger(newcomer, 0, 1.0).unwrap();
        engine.render_frames(1, |_| {});

        assert_eq!(engine.voices_for_pad(quiet_by_gain), 0);
        assert_eq!(engine.voices_for_pad(lower_velocity), 1);
        assert_eq!(engine.voices_for_pad(newcomer), 1);
    }

    #[test]
    fn retired_buffer_is_moved_only_after_last_voice_finishes() {
        let (mut controller, mut engine) = harness();
        controller
            .install(
                PadId::first(),
                constant_sample(128, 1.0),
                PadSettings::default(),
            )
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let mut started = [0.0; 64];
        engine.render_stereo(&mut started);
        controller
            .install(
                PadId::first(),
                constant_sample(2, 0.5),
                PadSettings::default(),
            )
            .unwrap();
        let mut first = [0.0; 64];
        engine.render_stereo(&mut first);
        assert_eq!(controller.reclaim_retired(), 0);
        let mut rest = [0.0; 256];
        engine.render_stereo(&mut rest);
        assert_eq!(controller.reclaim_retired(), 1);
    }

    #[test]
    fn negative_pad_gain_reduces_the_rendered_level() {
        let render_peak = |gain_db| {
            let (mut controller, mut engine) = harness();
            let settings = PadSettings::new(PlaybackMode::Loop, gain_db, -1.0, 0.0, None).unwrap();
            controller
                .install(PadId::first(), constant_sample(64, 0.1), settings)
                .unwrap();
            controller.trigger(PadId::first(), 0, 1.0).unwrap();
            let mut output = [0.0; 64];
            engine.render_stereo(&mut output);
            output[62]
        };

        assert!(render_peak(-6.0) < render_peak(0.0));
    }

    #[test]
    fn one_shot_ignores_a_matching_release() {
        let (mut controller, mut engine) = harness();
        controller
            .install(
                PadId::first(),
                constant_sample(128, 1.0),
                PadSettings::default(),
            )
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        controller.release(PadId::first(), 0).unwrap();
        let mut output = [0.0; 4];
        engine.render_stereo(&mut output);
        assert_eq!(engine.active_voices(), 1);
        assert!(output.iter().any(|sample| *sample != 0.0));
    }

    #[test]
    fn stop_pad_releases_instead_of_cutting_the_voice() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(256, 1.0), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let mut attack = [0.0; 64];
        engine.render_stereo(&mut attack);
        controller.stop_pad(PadId::first()).unwrap();
        let mut release = [0.0; 2 * 63];
        engine.render_stereo(&mut release);
        assert_eq!(engine.active_voices(), 1);
        let mut final_frame = [1.0; 2];
        engine.render_stereo(&mut final_frame);
        assert_eq!(final_frame, [0.0; 2]);
        assert_eq!(engine.active_voices(), 0);
    }

    #[test]
    fn telemetry_is_emitted_at_thirty_hertz_with_limited_peaks() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(8, 1.0), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        engine.render_frames(1_599, |_| {});
        assert_eq!(controller.latest_telemetry(), None);
        engine.render_frames(1, |_| {});
        let telemetry = controller.latest_telemetry().unwrap();
        assert_eq!(telemetry.rendered_frame, 1_600);
        assert!(telemetry.peak_left > 0.0 && telemetry.peak_left < 1.0);
        assert_eq!(telemetry.peak_left, telemetry.peak_right);
        assert_eq!(telemetry.active_voices, 1);
        assert!(telemetry.is_pad_active(PadId::first()));
        assert!(!telemetry.is_pad_active(PadId::new(BankId::new(0).unwrap(), 1).unwrap()));
        engine.render_frames(1_599, |_| {});
        assert_eq!(controller.latest_telemetry(), None);
    }

    #[test]
    fn telemetry_preserves_the_actual_late_trigger_frame_after_a_short_one_shot_ends() {
        let (mut controller, mut engine) = harness();
        controller
            .install(
                PadId::first(),
                constant_sample(1, 1.0),
                PadSettings::default(),
            )
            .unwrap();

        engine.render_frames(1_600, |_| {});
        assert_eq!(
            controller.latest_telemetry().unwrap().last_triggered_frame,
            None
        );
        controller.trigger(PadId::first(), 100, 1.0).unwrap();
        engine.render_frames(1_600, |_| {});

        let telemetry = controller.latest_telemetry().unwrap();
        assert_eq!(telemetry.rendered_frame, 3_200);
        assert_eq!(telemetry.last_triggered_frame, Some(1_600));
        assert_eq!(telemetry.active_voices, 0);
    }

    #[test]
    fn telemetry_keeps_a_released_one_shot_pad_active_until_its_voice_finishes() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        controller
            .install(pad, constant_sample(4_000, 1.0), PadSettings::default())
            .unwrap();
        controller.trigger(pad, 0, 1.0).unwrap();
        controller.release(pad, 1).unwrap();

        engine.render_frames(1_600, |_| {});

        let telemetry = controller.latest_telemetry().unwrap();
        assert_eq!(telemetry.active_voices, 1);
        assert!(telemetry.is_pad_active(pad));
    }

    #[test]
    fn telemetry_never_exceeds_thirty_events_per_second() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(44_101, ports).unwrap();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        let sample = Arc::new(SampleBuffer::new(44_101, vec![0.25; 16]).unwrap());
        controller
            .install(PadId::first(), sample, settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();

        engine.render_frames(1_470, |_| {});
        assert_eq!(controller.latest_telemetry(), None);
        engine.render_frames(1, |_| {});
        assert_eq!(controller.latest_telemetry().unwrap().rendered_frame, 1_471);
    }

    #[test]
    fn full_telemetry_queue_drops_the_new_event_without_retaining_it() {
        let (mut controller, ports) = audio_channels_with_capacities(8, 1, 1);
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(8, 0.25), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();

        engine.render_frames(1_600, |_| {});
        engine.render_frames(1_600, |_| {});
        assert_eq!(controller.latest_telemetry().unwrap().rendered_frame, 1_600);
        engine.render_frames(1_600, |_| {});
        assert_eq!(controller.latest_telemetry().unwrap().rendered_frame, 4_800);
    }

    #[test]
    fn non_finite_mix_intermediates_are_silenced_and_counted() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 6.0, 0.0, 0.0, None).unwrap();
        controller
            .install(PadId::first(), constant_sample(8, f32::MAX), settings)
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();
        let mut output = [0.0; 2 * 64];
        engine.render_stereo(&mut output);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(engine.invalid_commands() > 0);
    }

    #[test]
    fn full_retirement_queue_keeps_the_buffer_for_a_later_callback() {
        let (mut controller, ports) = audio_channels_with_capacities(16, 1, 1);
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        let mut retired_weak: [Option<Weak<SampleBuffer>>; 2] = [None, None];

        for (index, weak) in retired_weak.iter_mut().enumerate() {
            let sample = constant_sample(8, 0.25 + index as f32 * 0.25);
            *weak = Some(Arc::downgrade(&sample));
            controller
                .install(PadId::first(), sample, PadSettings::default())
                .unwrap();
            engine.render_stereo(&mut []);
        }
        controller
            .install(
                PadId::first(),
                constant_sample(8, 0.75),
                PadSettings::default(),
            )
            .unwrap();
        engine.render_stereo(&mut []);

        assert!(
            retired_weak
                .iter()
                .all(|weak| weak.as_ref().unwrap().upgrade().is_some())
        );
        assert_eq!(controller.reclaim_retired(), 1);
        assert!(retired_weak[0].as_ref().unwrap().upgrade().is_none());
        assert!(retired_weak[1].as_ref().unwrap().upgrade().is_some());

        engine.render_stereo(&mut []);
        assert_eq!(controller.reclaim_retired(), 1);
        assert!(retired_weak[1].as_ref().unwrap().upgrade().is_none());
    }
}
