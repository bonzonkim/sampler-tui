use std::array;
use std::f32::consts::PI;
use std::sync::Arc;

use rtrb::PushError;
use sampler_core::{
    EventId, FIRST_LOOP_VALID_MASK_WORDS, Frame, PATTERN_SLOT_COUNT, PadId, PadSettings,
    PatternAction, PatternActionKind, PatternSlotId, PatternSnapshot, PlaybackMode, VoiceAllocator,
    VoiceId, VoiceRequest,
};

use crate::{
    AudioCommand, CriticalEvent, EngineError, EnginePorts, LiveAck, LiveAckKind, LiveCommandId,
    PatternRetirement, PatternSnapshotSlot, PatternSwitch, SAMPLE_SLOT_COUNT, SampleBuffer,
    SampleSlot, Telemetry, TransportStamp,
};

const VOICE_COUNT: usize = 32;
const PENDING_COUNT: usize = 128;
const NON_LIVE_PENDING_COUNT: usize = 64;
const MAX_COMMANDS_PER_RENDER: usize = 64;
const MAX_PATTERN_ACTIONS_PER_CALLBACK: usize = 64;
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
    sequence: u64,
    pattern_voice: Option<PatternVoiceId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PatternVoiceId {
    slot: PatternSlotId,
    generation: u64,
    event_id: EventId,
    occurrence_start: Frame,
}

#[derive(Clone, Copy)]
enum ScheduledAction {
    Trigger {
        pad: PadId,
        at_frame: Frame,
        velocity: f32,
        sequence: u64,
        source: ActionSource,
    },
    Release {
        pad: PadId,
        at_frame: Frame,
        sequence: u64,
        source: ActionSource,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionSource {
    Command,
    Pattern(PatternVoiceId),
    Live(LiveCommandId),
}

struct InstalledPattern {
    owner_slot: PatternSnapshotSlot,
    snapshot: Arc<PatternSnapshot>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PatternGenerationId {
    slot: PatternSlotId,
    generation: u64,
}

#[derive(Clone, Copy)]
struct PatternTransition {
    outgoing: PatternGenerationId,
    incoming: PatternGenerationId,
}

#[derive(Clone, Copy)]
struct PendingPatternSwitch {
    slot: PatternSlotId,
    at_frame: Frame,
    sequence: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PatternRecordCapture {
    slot: PatternSlotId,
    generation: u64,
    sequence: u64,
}

struct PatternPlayer {
    patterns: [Option<InstalledPattern>; PATTERN_SLOT_COUNT],
    selected_slot: Option<PatternSlotId>,
    selected_sequence: u64,
    pending_switch: Option<PendingPatternSwitch>,
    origin: Frame,
    loop_count: u64,
    playing: bool,
    play_sequence: u64,
    record_capture: Option<PatternRecordCapture>,
    overflow_count: u64,
    live_ack_overflows: u64,
    pending_retirement: Option<PatternRetirement>,
}

impl PatternPlayer {
    fn new() -> Self {
        Self {
            patterns: array::from_fn(|_| None),
            selected_slot: None,
            selected_sequence: 0,
            pending_switch: None,
            origin: 0,
            loop_count: 0,
            playing: false,
            play_sequence: 0,
            record_capture: None,
            overflow_count: 0,
            live_ack_overflows: 0,
            pending_retirement: None,
        }
    }

    fn installed(&self, slot: PatternSlotId) -> Option<&InstalledPattern> {
        self.patterns[usize::from(slot.get())].as_ref()
    }

    fn current(&self) -> Option<&InstalledPattern> {
        self.selected_slot.and_then(|slot| self.installed(slot))
    }

    fn current_generation_id(&self) -> Option<PatternGenerationId> {
        self.current().map(|pattern| PatternGenerationId {
            slot: pattern.snapshot.slot(),
            generation: pattern.snapshot.generation(),
        })
    }

    fn replace(
        &mut self,
        owner_slot: PatternSnapshotSlot,
        snapshot: Arc<PatternSnapshot>,
    ) -> Option<InstalledPattern> {
        let index = usize::from(snapshot.slot().get());
        self.patterns[index].replace(InstalledPattern {
            owner_slot,
            snapshot,
        })
    }

    fn select(
        &mut self,
        slot: PatternSlotId,
        switch_at: PatternSwitch,
        now: Frame,
        sequence: u64,
    ) -> Option<PatternTransition> {
        let incoming = self.installed(slot).map(|pattern| PatternGenerationId {
            slot: pattern.snapshot.slot(),
            generation: pattern.snapshot.generation(),
        })?;
        if !self.playing || switch_at == PatternSwitch::Immediate {
            let outgoing = self.current_generation_id();
            self.selected_slot = Some(slot);
            self.selected_sequence = sequence;
            self.pending_switch = None;
            if self.playing {
                self.origin = now;
                self.loop_count = 0;
            }
            return outgoing.map(|outgoing| PatternTransition { outgoing, incoming });
        }

        if self.selected_slot == Some(slot) {
            self.pending_switch = None;
            return None;
        }
        let boundary = self.next_boundary(now);
        if boundary <= now {
            let outgoing = self.current_generation_id();
            self.selected_slot = Some(slot);
            self.selected_sequence = sequence;
            self.pending_switch = None;
            self.origin = now;
            self.loop_count = 0;
            return outgoing.map(|outgoing| PatternTransition { outgoing, incoming });
        } else {
            self.pending_switch = Some(PendingPatternSwitch {
                slot,
                at_frame: boundary,
                sequence,
            });
        }
        None
    }

    fn play(&mut self, now: Frame, sequence: u64) {
        self.playing = true;
        self.play_sequence = sequence;
        self.origin = now;
        self.loop_count = 0;
        self.pending_switch = None;
    }

    fn stop(&mut self) {
        self.playing = false;
        self.pending_switch = None;
    }

    fn advance_to(&mut self, frame: Frame) -> Option<PatternTransition> {
        if !self.playing {
            return None;
        }
        let mut transition = None;
        if let Some(pending) = self.pending_switch
            && pending.at_frame <= frame
        {
            let outgoing = self.current_generation_id();
            let incoming = self
                .installed(pending.slot)
                .map(|pattern| PatternGenerationId {
                    slot: pattern.snapshot.slot(),
                    generation: pattern.snapshot.generation(),
                });
            self.selected_slot = Some(pending.slot);
            self.selected_sequence = pending.sequence;
            self.origin = pending.at_frame;
            self.loop_count = 0;
            self.pending_switch = None;
            transition = outgoing
                .zip(incoming)
                .map(|(outgoing, incoming)| PatternTransition { outgoing, incoming });
        }
        let Some(loop_frames) = self.current().map(|pattern| pattern.snapshot.loop_frames()) else {
            return transition;
        };
        self.loop_count = frame.saturating_sub(self.origin) / loop_frames;
        transition
    }

    fn next_boundary(&self, now: Frame) -> Frame {
        let Some(loop_frames) = self.current().map(|pattern| pattern.snapshot.loop_frames()) else {
            return now;
        };
        let elapsed = now.saturating_sub(self.origin);
        let completed = elapsed / loop_frames;
        let loops = completed.saturating_add(u64::from(!elapsed.is_multiple_of(loop_frames)));
        self.origin
            .saturating_add(loops.saturating_mul(loop_frames))
    }

    fn transport_stamp(&self) -> Option<TransportStamp> {
        let pattern = self.playing.then(|| self.current()).flatten()?;
        Some(TransportStamp {
            slot: pattern.snapshot.slot(),
            generation: pattern.snapshot.generation(),
            origin: self.origin,
            loop_frames: pattern.snapshot.loop_frames(),
        })
    }

    fn apply_fence(&mut self, fence: u64) {
        if self
            .selected_slot
            .is_some_and(|_| sequence_is_at_or_before(self.selected_sequence, fence))
        {
            self.selected_slot = None;
        }
        if self
            .pending_switch
            .is_some_and(|pending| sequence_is_at_or_before(pending.sequence, fence))
        {
            self.pending_switch = None;
        }
        if self.playing && sequence_is_at_or_before(self.play_sequence, fence) {
            self.playing = false;
        }
        if self
            .record_capture
            .is_some_and(|capture| sequence_is_at_or_before(capture.sequence, fence))
        {
            self.record_capture = None;
        }
    }
}

#[derive(Clone, Copy)]
struct PatternInterval {
    slot: PatternSlotId,
    origin: Frame,
    start: Frame,
    end: Frame,
}

struct PatternIntervalCursor {
    origin: Frame,
    start: Frame,
    end: Frame,
    loop_frames: Frame,
    loop_index: u128,
    action_index: usize,
    first_loop_word: usize,
    first_loop_bits: u64,
}

impl PatternIntervalCursor {
    fn new(snapshot: &PatternSnapshot, interval: PatternInterval) -> Option<Self> {
        let start = interval.start.max(interval.origin);
        let loop_frames = snapshot.loop_frames();
        if loop_frames == 0 || start >= interval.end || snapshot.actions().is_empty() {
            return None;
        }
        let relative_start = start - interval.origin;
        let loop_index = u128::from(relative_start / loop_frames);
        let start_phase = relative_start % loop_frames;
        let action_index = snapshot
            .actions()
            .partition_point(|action| action.frame < start_phase);
        let first_loop_word = action_index / u64::BITS as usize;
        let first_loop_bits = if loop_index == 0 && first_loop_word < FIRST_LOOP_VALID_MASK_WORDS {
            snapshot.first_loop_valid_word(first_loop_word)
                & (u64::MAX << (action_index % u64::BITS as usize))
        } else {
            0
        };
        Some(Self {
            origin: interval.origin,
            start,
            end: interval.end,
            loop_frames,
            loop_index,
            action_index,
            first_loop_word,
            first_loop_bits,
        })
    }

    fn next(&mut self, snapshot: &PatternSnapshot) -> Option<(PatternAction, Frame, u128)> {
        let actions = snapshot.actions();
        loop {
            if self.loop_index == 0 {
                while self.first_loop_bits == 0 {
                    self.first_loop_word += 1;
                    if self.first_loop_word >= FIRST_LOOP_VALID_MASK_WORDS
                        || self.first_loop_word * u64::BITS as usize >= actions.len()
                    {
                        self.loop_index = 1;
                        self.action_index = 0;
                        break;
                    }
                    self.first_loop_bits = snapshot.first_loop_valid_word(self.first_loop_word);
                }
                if self.loop_index == 0 {
                    let bit = self.first_loop_bits.trailing_zeros() as usize;
                    self.first_loop_bits &= self.first_loop_bits - 1;
                    self.action_index = self.first_loop_word * u64::BITS as usize + bit;
                }
            } else if self.action_index == actions.len() {
                self.loop_index = self.loop_index.checked_add(1)?;
                self.action_index = 0;
            }

            let action = *actions.get(self.action_index)?;
            self.action_index += 1;
            let absolute = u128::from(self.origin)
                .checked_add(self.loop_index.checked_mul(u128::from(self.loop_frames))?)?
                .checked_add(u128::from(action.frame))?;
            if absolute >= u128::from(self.end) {
                return None;
            }
            if absolute < u128::from(self.start) {
                continue;
            }
            return Some((action, Frame::try_from(absolute).ok()?, self.loop_index));
        }
    }
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

    fn is_pattern(self) -> bool {
        match self {
            Self::Trigger { source, .. } | Self::Release { source, .. } => {
                matches!(source, ActionSource::Pattern(_))
            }
        }
    }

    fn pattern_voice_id(self) -> Option<PatternVoiceId> {
        match self {
            Self::Trigger {
                source: ActionSource::Pattern(id),
                ..
            }
            | Self::Release {
                source: ActionSource::Pattern(id),
                ..
            } => Some(id),
            _ => None,
        }
    }

    fn is_live(self) -> bool {
        match self {
            Self::Trigger { source, .. } | Self::Release { source, .. } => {
                matches!(source, ActionSource::Live(_))
            }
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
    non_live_pending: usize,
    next_command_lane: CommandLane,
    active_stop_fence: Option<u64>,
    deferred_retirement: Option<CriticalEvent>,
    pattern_player: PatternPlayer,
    rendered_frame: Frame,
    last_triggered_frame: Option<Frame>,
    late_commands: u64,
    invalid_commands: u64,
    executed_triggers: u64,
    telemetry_peak_left: f32,
    telemetry_peak_right: f32,
    next_telemetry_frame: Frame,
    #[cfg(test)]
    pattern_action_reads: usize,
    #[cfg(test)]
    pattern_mask_word_reads: usize,
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
            non_live_pending: 0,
            next_command_lane: CommandLane::Immediate,
            active_stop_fence: None,
            deferred_retirement: None,
            pattern_player: PatternPlayer::new(),
            rendered_frame: 0,
            last_triggered_frame: None,
            late_commands: 0,
            invalid_commands: 0,
            executed_triggers: 0,
            telemetry_peak_left: 0.0,
            telemetry_peak_right: 0.0,
            next_telemetry_frame: telemetry_interval,
            #[cfg(test)]
            pattern_action_reads: 0,
            #[cfg(test)]
            pattern_mask_word_reads: 0,
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
        self.flush_pattern_retirement();
        self.advance_pattern_to(self.rendered_frame);
        self.apply_stop_fence();
        self.drain_commands(horizon);
        self.schedule_pattern_actions(horizon);

        for _ in 0..frame_count {
            self.advance_pattern_to(self.rendered_frame);
            self.execute_due_actions();
            let frame = self.render_frame();
            self.telemetry_peak_left = self.telemetry_peak_left.max(frame[0].abs());
            self.telemetry_peak_right = self.telemetry_peak_right.max(frame[1].abs());
            write_frame(frame);
            self.rendered_frame = self.rendered_frame.saturating_add(1);
            self.advance_pattern_to(self.rendered_frame);
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

    pub fn pattern_overflows(&self) -> u64 {
        self.pattern_player.overflow_count
    }

    pub fn queued_commands(&self) -> usize {
        self.ports.queued_commands()
    }

    fn advance_pattern_to(&mut self, frame: Frame) {
        if let Some(transition) = self.pattern_player.advance_to(frame) {
            self.apply_pattern_transition(transition);
        }
    }

    fn apply_pattern_transition(&mut self, transition: PatternTransition) {
        let PatternTransition {
            outgoing,
            incoming: _,
        } = transition;
        self.release_pattern_generation(outgoing.slot, outgoing.generation);
        self.cancel_pattern_generation(outgoing.slot, outgoing.generation);
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

    fn drain_commands(&mut self, horizon: Frame) {
        for _ in 0..MAX_COMMANDS_PER_RENDER {
            let preferred = self.next_command_lane;
            if self.drain_one_command(preferred, horizon) {
                self.next_command_lane = preferred.other();
                continue;
            }
            let fallback = preferred.other();
            if self.drain_one_command(fallback, horizon) {
                self.next_command_lane = preferred;
                continue;
            }
            break;
        }
    }

    fn drain_one_command(&mut self, lane: CommandLane, horizon: Frame) -> bool {
        match lane {
            CommandLane::Immediate => self.drain_one_immediate_command(horizon),
            CommandLane::Timed => self.drain_one_timed_command(),
        }
    }

    fn drain_one_immediate_command(&mut self, _horizon: Frame) -> bool {
        self.apply_stop_fence();
        let resolved_live_frame = self
            .rendered_frame
            .saturating_add(Frame::from(RELEASE_FRAMES));
        let live_action = match self.ports.immediate_commands.peek() {
            Ok(AudioCommand::TriggerLive {
                id,
                pad,
                velocity,
                sequence,
            }) => Some(ScheduledAction::Trigger {
                pad: *pad,
                at_frame: resolved_live_frame,
                velocity: *velocity,
                sequence: *sequence,
                source: ActionSource::Live(*id),
            }),
            Ok(AudioCommand::ReleaseLive { id, pad, sequence }) => Some(ScheduledAction::Release {
                pad: *pad,
                at_frame: resolved_live_frame,
                sequence: *sequence,
                source: ActionSource::Live(*id),
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
            Ok(AudioCommand::InstallPattern { snapshot, .. }) => {
                let needs_retirement = self.pattern_player.installed(snapshot.slot()).is_some();
                if needs_retirement && !self.pattern_retirement_available() {
                    return false;
                }
                None
            }
            Ok(_) => None,
            Err(_) => return false,
        };

        if let Some(action) = live_action
            && !self.action_is_stopped(action)
            && !self.can_admit(action)
        {
            return false;
        }

        let Ok(command) = self.ports.immediate_commands.pop() else {
            return false;
        };
        if let Some(action) = live_action {
            if !self.action_is_stopped(action) {
                self.insert_pending(action);
            }
        } else {
            if is_sequenced_pattern_command(&command) {
                self.apply_stop_fence();
            }
            self.execute_immediate(command);
        }
        true
    }

    fn drain_one_timed_command(&mut self) -> bool {
        self.apply_stop_fence();
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
                source: ActionSource::Command,
            }),
            Ok(AudioCommand::Release {
                pad,
                at_frame,
                sequence,
            }) => Some(ScheduledAction::Release {
                pad: *pad,
                at_frame: *at_frame,
                sequence: *sequence,
                source: ActionSource::Command,
            }),
            Ok(_) => None,
            Err(_) => return false,
        };
        if let Some(action) = timed_action {
            if !self.action_is_stopped(action) && !self.can_admit(action) {
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
            AudioCommand::InstallPattern {
                owner_slot,
                snapshot,
                sequence: _,
            } => {
                if let Some((slot, generation)) = self
                    .pattern_player
                    .installed(snapshot.slot())
                    .map(|installed| (installed.snapshot.slot(), installed.snapshot.generation()))
                {
                    self.release_pattern_generation(slot, generation);
                    self.cancel_pattern_generation(slot, generation);
                }
                let retired = self.pattern_player.replace(owner_slot, snapshot);
                if let Some(retired) = retired {
                    self.retire_pattern(retired);
                }
            }
            AudioCommand::SelectPattern {
                slot,
                switch_at,
                sequence,
            } => {
                if self.sequence_is_stopped(sequence) {
                    return;
                }
                if self.pattern_player.installed(slot).is_none() {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                    return;
                }
                let transition =
                    self.pattern_player
                        .select(slot, switch_at, self.rendered_frame, sequence);
                if let Some(transition) = transition {
                    self.apply_pattern_transition(transition);
                }
            }
            AudioCommand::PatternPlay { sequence } => {
                if self.sequence_is_stopped(sequence) {
                    return;
                }
                if self.pattern_player.current().is_none() {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                    return;
                }
                if self.pattern_player.playing {
                    let previous_play = self.pattern_player.play_sequence;
                    self.release_pattern_play(previous_play);
                    self.cancel_pattern_play(previous_play);
                }
                self.cancel_pattern_actions();
                self.pattern_player.play(self.rendered_frame, sequence);
            }
            AudioCommand::PatternStop { sequence } => {
                if self.sequence_is_stopped(sequence) {
                    return;
                }
                let stopped_play = self
                    .pattern_player
                    .playing
                    .then_some(self.pattern_player.play_sequence);
                self.pattern_player.stop();
                self.pattern_player.record_capture = None;
                if let Some(play_sequence) = stopped_play {
                    self.release_pattern_play(play_sequence);
                    self.cancel_pattern_play(play_sequence);
                }
            }
            AudioCommand::SetRecordCapture { capture, sequence } => {
                if !self.sequence_is_stopped(sequence) {
                    self.pattern_player.record_capture =
                        capture.map(|(slot, generation)| PatternRecordCapture {
                            slot,
                            generation,
                            sequence,
                        });
                }
            }
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

    fn pattern_retirement_available(&self) -> bool {
        self.pattern_player.pending_retirement.is_none()
            && self.ports.pattern_retirements.slots() > 0
    }

    fn retire_pattern(&mut self, pattern: InstalledPattern) {
        let retirement = PatternRetirement::new(pattern.owner_slot, pattern.snapshot);
        match self.ports.pattern_retirements.push(retirement) {
            Ok(()) => {}
            Err(PushError::Full(retirement)) => {
                debug_assert!(self.pattern_player.pending_retirement.is_none());
                self.pattern_player.pending_retirement = Some(retirement);
            }
        }
    }

    fn flush_pattern_retirement(&mut self) {
        let Some(retirement) = self.pattern_player.pending_retirement.take() else {
            return;
        };
        if let Err(PushError::Full(retirement)) = self.ports.pattern_retirements.push(retirement) {
            self.pattern_player.pending_retirement = Some(retirement);
        }
    }

    fn apply_stop_fence(&mut self) {
        let Some(fence_sequence) = self.ports.take_stop_fence() else {
            return;
        };

        self.apply_stop_fence_sequence(fence_sequence);
    }

    fn apply_stop_fence_sequence(&mut self, fence_sequence: u64) {
        self.active_stop_fence = Some(fence_sequence);
        self.stop_all_at_or_before(fence_sequence);
        self.pattern_player.apply_fence(fence_sequence);

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
        self.refresh_non_live_pending();
    }

    fn cancel_pattern_actions(&mut self) {
        let old_len = self.pending_len;
        let mut retained = 0;
        for index in 0..old_len {
            let Some(action) = self.pending[index] else {
                continue;
            };
            if !action.is_pattern() {
                self.pending[retained] = Some(action);
                retained += 1;
            }
        }
        for index in retained..old_len {
            self.pending[index] = None;
        }
        self.pending_len = retained;
        self.refresh_non_live_pending();
    }

    fn cancel_pattern_play(&mut self, play_sequence: u64) {
        self.retain_pending_actions(|action| {
            !(action.is_pattern() && action.sequence() == play_sequence)
        });
    }

    fn cancel_pattern_generation(&mut self, slot: PatternSlotId, generation: u64) {
        self.retain_pending_actions(|action| {
            !action
                .pattern_voice_id()
                .is_some_and(|id| id.slot == slot && id.generation == generation)
        });
    }

    fn retain_pending_actions(&mut self, mut retain: impl FnMut(ScheduledAction) -> bool) {
        let old_len = self.pending_len;
        let mut retained = 0;
        for index in 0..old_len {
            let Some(action) = self.pending[index] else {
                continue;
            };
            if retain(action) {
                self.pending[retained] = Some(action);
                retained += 1;
            }
        }
        for index in retained..old_len {
            self.pending[index] = None;
        }
        self.pending_len = retained;
        self.refresh_non_live_pending();
    }

    fn schedule_pattern_actions(&mut self, horizon: Frame) {
        let start = self.rendered_frame;
        if !self.pattern_player.playing || start >= horizon {
            return;
        }
        self.advance_pattern_to(start);
        let Some(selected_slot) = self.pattern_player.selected_slot else {
            return;
        };

        let mut intervals = [None; 2];
        let mut interval_len = 1;
        let mut current_end = horizon;
        if let Some(pending) = self.pattern_player.pending_switch
            && pending.at_frame > start
            && pending.at_frame < horizon
        {
            current_end = pending.at_frame;
            intervals[1] = Some(PatternInterval {
                slot: pending.slot,
                origin: pending.at_frame,
                start: pending.at_frame,
                end: horizon,
            });
            interval_len = 2;
        }
        intervals[0] = Some(PatternInterval {
            slot: selected_slot,
            origin: self.pattern_player.origin,
            start,
            end: current_end,
        });

        let mut total = 0_u128;
        for interval in intervals.iter().take(interval_len).flatten() {
            let Some(pattern) = self.pattern_player.installed(interval.slot) else {
                continue;
            };
            #[cfg(test)]
            {
                self.pattern_mask_word_reads = self.pattern_mask_word_reads.saturating_add(
                    pattern_interval_first_loop_mask_words(&pattern.snapshot, *interval),
                );
            }
            total =
                total.saturating_add(pattern_interval_action_count(&pattern.snapshot, *interval));
        }

        let free_entries = PENDING_COUNT
            .saturating_sub(self.pending_len)
            .min(NON_LIVE_PENDING_COUNT.saturating_sub(self.non_live_pending));
        let admitted_limit = usize::try_from(total)
            .unwrap_or(usize::MAX)
            .min(MAX_PATTERN_ACTIONS_PER_CALLBACK)
            .min(free_entries);
        let mut admitted_len = 0;
        let sequence = self.pattern_player.play_sequence;
        for interval in intervals.iter().take(interval_len).flatten() {
            if admitted_len == admitted_limit {
                break;
            }
            let Some((slot, generation, loop_frames, mut cursor)) = self
                .pattern_player
                .installed(interval.slot)
                .and_then(|pattern| {
                    Some((
                        pattern.snapshot.slot(),
                        pattern.snapshot.generation(),
                        pattern.snapshot.loop_frames(),
                        PatternIntervalCursor::new(&pattern.snapshot, *interval)?,
                    ))
                })
            else {
                continue;
            };
            while admitted_len < admitted_limit {
                let next = self
                    .pattern_player
                    .installed(interval.slot)
                    .and_then(|pattern| cursor.next(&pattern.snapshot));
                let Some((action, at_frame, loop_index)) = next else {
                    break;
                };
                #[cfg(test)]
                {
                    self.pattern_action_reads = self.pattern_action_reads.saturating_add(1);
                }
                let Some(id) = pattern_voice_id(
                    slot,
                    generation,
                    loop_frames,
                    interval.origin,
                    action,
                    loop_index,
                ) else {
                    continue;
                };
                self.insert_pending(scheduled_pattern_action(action, at_frame, sequence, id));
                admitted_len += 1;
            }
        }

        let dropped = total.saturating_sub(admitted_len as u128);
        self.pattern_player.overflow_count = self
            .pattern_player
            .overflow_count
            .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
    }

    fn action_is_stopped(&self, action: ScheduledAction) -> bool {
        self.sequence_is_stopped(action.sequence())
    }

    fn sequence_is_stopped(&self, sequence: u64) -> bool {
        self.active_stop_fence
            .is_some_and(|fence| sequence_is_at_or_before(sequence, fence))
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
        if !action.is_live() {
            self.non_live_pending += 1;
        }
    }

    fn can_admit(&self, action: ScheduledAction) -> bool {
        self.pending_len < PENDING_COUNT
            && (action.is_live() || self.non_live_pending < NON_LIVE_PENDING_COUNT)
    }

    fn refresh_non_live_pending(&mut self) {
        self.non_live_pending = self
            .pending
            .iter()
            .take(self.pending_len)
            .flatten()
            .filter(|action| !action.is_live())
            .count();
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
        if !action.is_live() {
            self.non_live_pending -= 1;
        }
        Some(action)
    }

    fn execute_action(&mut self, action: ScheduledAction) {
        match action {
            ScheduledAction::Trigger {
                pad,
                at_frame: _,
                velocity,
                sequence,
                source,
            } => {
                let pattern_voice = match source {
                    ActionSource::Pattern(id) => Some(id),
                    _ => None,
                };
                let triggered = self.trigger(pad, velocity, sequence, pattern_voice);
                if triggered {
                    self.executed_triggers = self.executed_triggers.saturating_add(1);
                    self.last_triggered_frame = Some(self.rendered_frame);
                    if let ActionSource::Live(id) = source {
                        self.emit_live_ack(id, pad, LiveAckKind::Trigger { velocity });
                    }
                }
            }
            ScheduledAction::Release {
                pad,
                at_frame: _,
                source,
                ..
            } => {
                let valid = self.pad_binding(pad).slot.is_some();
                if !valid {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                } else if let ActionSource::Pattern(id) = source {
                    self.release_pattern_voice(id);
                } else {
                    self.release_gate_voices(pad);
                }
                if valid && let ActionSource::Live(id) = source {
                    self.emit_live_ack(id, pad, LiveAckKind::Release);
                }
            }
        }
    }

    fn emit_live_ack(&mut self, id: LiveCommandId, pad: PadId, kind: LiveAckKind) {
        let Some(capture) = self.pattern_player.record_capture else {
            return;
        };
        let Some(transport) = self.pattern_player.transport_stamp() else {
            return;
        };
        if (capture.slot, capture.generation) != (transport.slot, transport.generation) {
            return;
        }
        let ack = LiveAck {
            id,
            pad,
            kind,
            frame: self.rendered_frame,
            transport: Some(transport),
        };
        if self.ports.live_acks.push(ack).is_err() {
            self.pattern_player.live_ack_overflows =
                self.pattern_player.live_ack_overflows.saturating_add(1);
        }
    }

    fn trigger(
        &mut self,
        pad: PadId,
        velocity: f32,
        sequence: u64,
        pattern_voice: Option<PatternVoiceId>,
    ) -> bool {
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
            sequence,
            pattern_voice,
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

    fn stop_all_at_or_before(&mut self, fence: u64) {
        for voice in self.voices.iter_mut().flatten() {
            if sequence_is_at_or_before(voice.sequence, fence) {
                voice.envelope.begin_release();
            }
        }
    }

    fn release_gate_voices(&mut self, pad: PadId) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.pad == pad && voice.mode == PlaybackMode::Gate {
                voice.envelope.begin_release();
            }
        }
    }

    fn release_pattern_voice(&mut self, id: PatternVoiceId) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.pattern_voice == Some(id) {
                voice.envelope.begin_release();
            }
        }
    }

    fn release_pattern_play(&mut self, play_sequence: u64) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.pattern_voice.is_some() && voice.sequence == play_sequence {
                voice.envelope.begin_release();
            }
        }
    }

    fn release_pattern_generation(&mut self, slot: PatternSlotId, generation: u64) {
        for voice in self.voices.iter_mut().flatten() {
            if voice
                .pattern_voice
                .is_some_and(|id| id.slot == slot && id.generation == generation)
            {
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

        let pattern = self.pattern_player.current();
        let pattern_slot = pattern.map(|installed| installed.snapshot.slot());
        let pattern_generation = pattern.map(|installed| installed.snapshot.generation());
        let pattern_loop_frames = pattern.map(|installed| installed.snapshot.loop_frames());
        let pattern_recording = self.pattern_player.playing
            && pattern_slot
                .zip(pattern_generation)
                .is_some_and(|(slot, generation)| {
                    self.pattern_player.record_capture.is_some_and(|capture| {
                        (capture.slot, capture.generation) == (slot, generation)
                    })
                });
        let pattern_playhead = if self.pattern_player.playing {
            pattern_loop_frames.map_or(0, |loop_frames| {
                self.rendered_frame
                    .saturating_sub(self.pattern_player.origin)
                    % loop_frames
            })
        } else {
            0
        };
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
            pattern_slot,
            pattern_generation,
            pattern_playing: self.pattern_player.playing,
            pattern_recording,
            pattern_origin: self
                .pattern_player
                .playing
                .then_some(self.pattern_player.origin),
            pattern_playhead,
            pattern_loop_count: self.pattern_player.loop_count,
            pattern_overflows: self.pattern_player.overflow_count,
            live_ack_overflows: self.pattern_player.live_ack_overflows,
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

fn pattern_interval_action_count(snapshot: &PatternSnapshot, interval: PatternInterval) -> u128 {
    let start = interval.start.max(interval.origin);
    if start >= interval.end {
        return 0;
    }
    let actions = snapshot.actions();
    if actions.is_empty() {
        return 0;
    }

    let loop_frames = snapshot.loop_frames();
    if loop_frames == 0 {
        return 0;
    }
    let relative_start = start - interval.origin;
    let relative_end = interval.end - interval.origin;
    let first_loop = relative_start / loop_frames;
    let end_loop = relative_end / loop_frames;
    let start_phase = relative_start % loop_frames;
    let end_phase = relative_end % loop_frames;
    let first_index = actions.partition_point(|action| action.frame < start_phase);

    if first_loop == end_loop {
        let end_index = actions.partition_point(|action| action.frame < end_phase);
        return if first_loop == 0 {
            snapshot.first_loop_valid_count(first_index, end_index) as u128
        } else {
            end_index.saturating_sub(first_index) as u128
        };
    }

    let first_count = if first_loop == 0 {
        snapshot.first_loop_valid_count(first_index, actions.len()) as u128
    } else {
        actions.len().saturating_sub(first_index) as u128
    };
    let middle_loops = end_loop.saturating_sub(first_loop).saturating_sub(1);
    let middle_count = u128::from(middle_loops).saturating_mul(actions.len() as u128);
    let last_count = actions.partition_point(|action| action.frame < end_phase) as u128;
    first_count
        .saturating_add(middle_count)
        .saturating_add(last_count)
}

#[cfg(test)]
fn pattern_interval_first_loop_mask_words(
    snapshot: &PatternSnapshot,
    interval: PatternInterval,
) -> usize {
    let start = interval.start.max(interval.origin);
    let actions = snapshot.actions();
    let loop_frames = snapshot.loop_frames();
    if actions.is_empty() || loop_frames == 0 || start >= interval.end {
        return 0;
    }
    let relative_start = start - interval.origin;
    if relative_start / loop_frames != 0 {
        return 0;
    }
    let relative_end = interval.end - interval.origin;
    let start_phase = relative_start % loop_frames;
    let end_phase = relative_end % loop_frames;
    let first = actions.partition_point(|action| action.frame < start_phase);
    let last = if relative_end / loop_frames == 0 {
        actions.partition_point(|action| action.frame < end_phase)
    } else {
        actions.len()
    };
    if first >= last {
        0
    } else {
        let first_word = first / u64::BITS as usize;
        let last_word = (last - 1) / u64::BITS as usize;
        last_word - first_word + 1
    }
}

fn scheduled_pattern_action(
    action: PatternAction,
    at_frame: Frame,
    sequence: u64,
    id: PatternVoiceId,
) -> ScheduledAction {
    match action.kind {
        PatternActionKind::Trigger { velocity } => ScheduledAction::Trigger {
            pad: action.pad,
            at_frame,
            velocity,
            sequence,
            source: ActionSource::Pattern(id),
        },
        PatternActionKind::Release => ScheduledAction::Release {
            pad: action.pad,
            at_frame,
            sequence,
            source: ActionSource::Pattern(id),
        },
    }
}

fn pattern_voice_id(
    slot: PatternSlotId,
    generation: u64,
    loop_frames: Frame,
    origin: Frame,
    action: PatternAction,
    loop_index: u128,
) -> Option<PatternVoiceId> {
    let occurrence_loop = loop_index.checked_sub(u128::from(action.trigger_loop_delta))?;
    let occurrence_start = u128::from(origin)
        .checked_add(occurrence_loop.checked_mul(u128::from(loop_frames))?)?
        .checked_add(u128::from(action.trigger_frame))?;
    Some(PatternVoiceId {
        slot,
        generation,
        event_id: action.event_id,
        occurrence_start: Frame::try_from(occurrence_start).ok()?,
    })
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

fn is_sequenced_pattern_command(command: &AudioCommand) -> bool {
    matches!(
        command,
        AudioCommand::InstallPattern { .. }
            | AudioCommand::SelectPattern { .. }
            | AudioCommand::PatternPlay { .. }
            | AudioCommand::PatternStop { .. }
            | AudioCommand::SetRecordCapture { .. }
    )
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
        AudioController, PadId, PadSettings, PatternSwitch, SampleBuffer, audio_channels,
        command::{RECOVERY_COMMAND_CAPACITY, audio_channels_with_capacities},
    };
    use sampler_core::{
        BankId, ChokeGroup, EditablePattern, EventId, Meter, PatternEvent, PatternSlotId,
        Resolution, Tempo, Transport,
    };

    fn harness() -> (AudioController, AudioEngine) {
        let (controller, ports) = audio_channels();
        (controller, AudioEngine::new(48_000, ports).unwrap())
    }

    fn constant_sample(frames: usize, value: f32) -> Arc<SampleBuffer> {
        Arc::new(SampleBuffer::new(48_000, vec![value; frames * 2]).unwrap())
    }

    fn constant_sample_at(sample_rate: u32, frames: usize, value: f32) -> Arc<SampleBuffer> {
        Arc::new(SampleBuffer::new(sample_rate, vec![value; frames * 2]).unwrap())
    }

    fn pattern_snapshot_with_triggers(
        slot: u8,
        sample_rate: u32,
        events: &[(Frame, PadId)],
    ) -> Arc<sampler_core::PatternSnapshot> {
        let transport = Transport::new(
            sample_rate,
            Tempo::new(300.0).unwrap(),
            Meter::new(1, 8).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        let mut pattern =
            EditablePattern::new(PatternSlotId::new(slot).unwrap(), "Pattern", transport).unwrap();
        for (index, (frame, pad)) in events.iter().copied().enumerate() {
            pattern
                .insert(
                    PatternEvent::new(EventId(index as u64 + 1), pad, frame, 1.0, None).unwrap(),
                )
                .unwrap();
        }
        Arc::new(pattern.compile().unwrap())
    }

    fn pattern_snapshot_with_durations(
        slot: u8,
        sample_rate: u32,
        events: &[(Frame, PadId, Frame)],
    ) -> Arc<sampler_core::PatternSnapshot> {
        let transport = Transport::new(
            sample_rate,
            Tempo::new(300.0).unwrap(),
            Meter::new(1, 8).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        let mut pattern =
            EditablePattern::new(PatternSlotId::new(slot).unwrap(), "Pattern", transport).unwrap();
        for (index, (frame, pad, duration)) in events.iter().copied().enumerate() {
            pattern
                .insert(
                    PatternEvent::new(EventId(index as u64 + 1), pad, frame, 1.0, Some(duration))
                        .unwrap(),
                )
                .unwrap();
        }
        Arc::new(pattern.compile().unwrap())
    }

    fn install_ready_sample(
        controller: &mut AudioController,
        engine: &mut AudioEngine,
        sample_rate: u32,
        pad: PadId,
        settings: PadSettings,
        frames: usize,
    ) {
        controller
            .install(pad, constant_sample_at(sample_rate, frames, 0.5), settings)
            .unwrap();
        engine.render_frames(0, |_| {});
    }

    #[test]
    fn pattern_actions_are_placed_from_callback_origin_and_wrap_exactly() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        controller
            .install_pattern(pattern_snapshot_with_triggers(
                0,
                100,
                &[(2, pad), (8, pad)],
            ))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        let mut callback_frame = 0;
        let mut active_frames = Vec::new();
        engine.render_frames(22, |frame| {
            if frame != [0.0, 0.0] {
                active_frames.push(callback_frame);
            }
            callback_frame += 1;
        });

        assert_eq!(active_frames, [2, 8, 12, 18]);
    }

    #[test]
    fn overlapping_gate_releases_only_its_exact_pattern_occurrence() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let gate = PadSettings::new(PlaybackMode::Gate, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 100, pad, gate, 128);
        controller
            .install_pattern(pattern_snapshot_with_durations(
                0,
                100,
                &[(0, pad, 4), (2, pad, 6)],
            ))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        engine.render_frames(5, |_| {});

        let releasing = engine
            .voices
            .iter()
            .flatten()
            .filter(|voice| voice.envelope.release_frame.is_some())
            .count();
        let sustained = engine
            .voices
            .iter()
            .flatten()
            .filter(|voice| voice.envelope.release_frame.is_none())
            .count();
        assert_eq!((releasing, sustained), (1, 1));
    }

    #[test]
    fn pattern_duration_releases_loop_voice() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 16);
        controller
            .install_pattern(pattern_snapshot_with_durations(0, 100, &[(0, pad, 2)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        engine.render_frames(3, |_| {});

        assert_eq!(
            engine
                .voices
                .iter()
                .flatten()
                .find(|voice| voice.pad == pad)
                .unwrap()
                .envelope
                .release_frame,
            Some(1)
        );
    }

    #[test]
    fn wrapped_pattern_release_skips_loop_zero_and_never_releases_live_same_pad() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(1_000, ports).unwrap();
        let pad = PadId::first();
        let gate = PadSettings::new(PlaybackMode::Gate, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 1_000, pad, gate, 512);
        controller
            .install_pattern(pattern_snapshot_with_durations(0, 1_000, &[(90, pad, 74)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        controller.trigger_live(pad, 1.0).unwrap();

        engine.render_frames(65, |_| {});

        assert_eq!(engine.voices_for_pad(pad), 1);
        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .all(|voice| voice.envelope.release_frame.is_none())
        );

        engine.render_frames(100, |_| {});

        let releasing = engine
            .voices
            .iter()
            .flatten()
            .filter(|voice| voice.envelope.release_frame.is_some())
            .count();
        let sustained = engine
            .voices
            .iter()
            .flatten()
            .filter(|voice| voice.envelope.release_frame.is_none())
            .count();
        assert_eq!((releasing, sustained), (1, 1));
    }

    #[test]
    fn duration_equal_to_loop_releases_the_prior_occurrence_despite_equal_phases() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 64);
        let snapshot = pattern_snapshot_with_durations(0, 100, &[(2, pad, 10)]);
        assert_eq!(snapshot.loop_frames(), 10);
        controller.install_pattern(snapshot).unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        engine.render_frames(3, |_| {});
        assert_eq!(engine.voices_for_pad(pad), 1);
        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .all(|voice| voice.envelope.release_frame.is_none())
        );

        engine.render_frames(10, |_| {});
        let releasing = engine
            .voices
            .iter()
            .flatten()
            .filter(|voice| voice.envelope.release_frame.is_some())
            .count();
        let sustained = engine
            .voices
            .iter()
            .flatten()
            .filter(|voice| voice.envelope.release_frame.is_none())
            .count();
        assert_eq!((releasing, sustained), (1, 1));
    }

    #[test]
    fn first_loop_enumeration_bounds_reads_before_the_earliest_valid_triggers() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let events = vec![(9, pad, 1); sampler_core::MAX_PATTERN_EVENTS];
        controller
            .install_pattern(pattern_snapshot_with_durations(0, 100, &events))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(0, |_| {});

        engine.schedule_pattern_actions(10);

        let event_ids = engine
            .pending
            .iter()
            .take(engine.pending_len)
            .flatten()
            .filter_map(|action| action.pattern_voice_id().map(|id| id.event_id.0))
            .collect::<Vec<_>>();
        assert_eq!(event_ids, (1..=64).collect::<Vec<_>>());
        assert!(engine.pattern_action_reads <= MAX_PATTERN_ACTIONS_PER_CALLBACK);
        assert!(engine.pattern_mask_word_reads <= sampler_core::FIRST_LOOP_VALID_MASK_WORDS);
    }

    #[test]
    fn pattern_stop_releases_active_loop_voice_from_the_play_sequence() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 16);
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(0, pad)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(1, |_| {});
        controller.stop_pattern().unwrap();

        engine.render_frames(0, |_| {});

        assert_eq!(
            engine
                .voices
                .iter()
                .flatten()
                .find(|voice| voice.pad == pad)
                .unwrap()
                .envelope
                .release_frame,
            Some(0)
        );
    }

    #[test]
    fn pattern_replacement_releases_only_replaced_generation_voice() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pattern_pad = PadId::first();
        let timed_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        for pad in [pattern_pad, timed_pad] {
            install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 16);
        }
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(0, pattern_pad)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        controller.trigger(timed_pad, 0, 1.0).unwrap();
        engine.render_frames(1, |_| {});
        controller
            .install_pattern(pattern_snapshot_with_triggers(
                0,
                100,
                &[(5, pattern_pad), (7, pattern_pad)],
            ))
            .unwrap();

        engine.render_frames(0, |_| {});

        assert_eq!(
            engine
                .voices
                .iter()
                .flatten()
                .find(|voice| voice.pad == pattern_pad)
                .unwrap()
                .envelope
                .release_frame,
            Some(0)
        );
        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .any(|voice| voice.pad == timed_pad && voice.envelope.release_frame.is_none())
        );
    }

    #[test]
    fn slot_switch_while_playing_occurs_only_at_the_next_loop_origin() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad_a = PadId::first();
        let pad_b = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        for pad in [pad_a, pad_b] {
            install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 64);
        }
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(8, pad_a)]))
            .unwrap();
        controller
            .install_pattern(pattern_snapshot_with_triggers(1, 100, &[(2, pad_b)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        engine.render_frames(5, |_| {});
        controller
            .select_pattern(PatternSlotId::new(1).unwrap(), PatternSwitch::NextBoundary)
            .unwrap();
        engine.render_frames(5, |_| {});
        assert_eq!(engine.voices_for_pad(pad_a), 1);
        assert_eq!(engine.voices_for_pad(pad_b), 0);

        engine.render_frames(10, |_| {});
        assert_eq!(engine.voices_for_pad(pad_b), 1);
    }

    #[test]
    fn immediate_switch_releases_gate_and_loop_outgoing_voices_only() {
        for mode in [PlaybackMode::Gate, PlaybackMode::Loop] {
            let (mut controller, ports) = audio_channels();
            let mut engine = AudioEngine::new(100, ports).unwrap();
            let outgoing_pad = PadId::first();
            let incoming_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
            let preserved_pad = PadId::new(BankId::new(0).unwrap(), 2).unwrap();
            let switched = PadSettings::new(mode, 0.0, 0.0, 0.0, None).unwrap();
            let preserved = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
            install_ready_sample(
                &mut controller,
                &mut engine,
                100,
                outgoing_pad,
                switched,
                128,
            );
            install_ready_sample(
                &mut controller,
                &mut engine,
                100,
                incoming_pad,
                switched,
                128,
            );
            install_ready_sample(
                &mut controller,
                &mut engine,
                100,
                preserved_pad,
                preserved,
                128,
            );
            controller
                .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(0, outgoing_pad)]))
                .unwrap();
            controller
                .install_pattern(pattern_snapshot_with_triggers(1, 100, &[(0, incoming_pad)]))
                .unwrap();
            controller
                .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
                .unwrap();
            controller.play_pattern().unwrap();
            engine.render_frames(1, |_| {});

            controller
                .select_pattern(PatternSlotId::new(1).unwrap(), PatternSwitch::Immediate)
                .unwrap();
            controller.trigger(preserved_pad, 50, 1.0).unwrap();
            controller.trigger_live(preserved_pad, 1.0).unwrap();
            engine.render_frames(1, |_| {});

            assert!(engine.voices.iter().flatten().any(|voice| {
                voice.pad == outgoing_pad && voice.envelope.release_frame.is_some()
            }));
            assert!(engine.voices.iter().flatten().any(|voice| {
                voice.pad == incoming_pad && voice.envelope.release_frame.is_none()
            }));
            assert!(
                engine
                    .pending
                    .iter()
                    .take(engine.pending_len)
                    .flatten()
                    .any(|action| matches!(
                        action,
                        ScheduledAction::Trigger {
                            pad,
                            source: ActionSource::Command | ActionSource::Live(_),
                            ..
                        } if *pad == preserved_pad
                    ))
            );
        }
    }

    #[test]
    fn boundary_switch_cleans_exact_outgoing_actions_and_gate_or_loop_voices() {
        for mode in [PlaybackMode::Gate, PlaybackMode::Loop] {
            let (mut controller, ports) = audio_channels();
            let mut engine = AudioEngine::new(100, ports).unwrap();
            let outgoing_pad = PadId::first();
            let incoming_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
            let preserved_pad = PadId::new(BankId::new(0).unwrap(), 2).unwrap();
            let switched = PadSettings::new(mode, 0.0, 0.0, 0.0, None).unwrap();
            let preserved = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
            for (pad, settings) in [
                (outgoing_pad, switched),
                (incoming_pad, switched),
                (preserved_pad, preserved),
            ] {
                install_ready_sample(&mut controller, &mut engine, 100, pad, settings, 128);
            }
            let outgoing = pattern_snapshot_with_triggers(0, 100, &[(0, outgoing_pad)]);
            let outgoing_generation = outgoing.generation();
            controller.install_pattern(outgoing).unwrap();
            controller
                .install_pattern(pattern_snapshot_with_triggers(1, 100, &[(0, incoming_pad)]))
                .unwrap();
            controller
                .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
                .unwrap();
            controller.play_pattern().unwrap();
            engine.render_frames(1, |_| {});
            engine.schedule_pattern_actions(30);

            controller
                .select_pattern(PatternSlotId::new(1).unwrap(), PatternSwitch::NextBoundary)
                .unwrap();
            controller.trigger(preserved_pad, 10, 1.0).unwrap();
            controller.trigger_live(preserved_pad, 1.0).unwrap();
            engine.render_frames(10, |_| {});

            assert!(engine.voices.iter().flatten().any(|voice| {
                voice.pad == outgoing_pad && voice.envelope.release_frame.is_some()
            }));
            assert!(engine.voices.iter().flatten().any(|voice| {
                voice.pad == incoming_pad && voice.envelope.release_frame.is_none()
            }));
            assert!(engine.voices.iter().flatten().any(|voice| {
                voice.pad == preserved_pad
                    && voice.pattern_voice.is_none()
                    && voice.envelope.release_frame.is_none()
            }));
            assert!(
                !engine
                    .pending
                    .iter()
                    .take(engine.pending_len)
                    .flatten()
                    .any(|action| action.pattern_voice_id().is_some_and(|id| {
                        id.slot == PatternSlotId::new(0).unwrap()
                            && id.generation == outgoing_generation
                    }))
            );
            assert!(
                engine
                    .pending
                    .iter()
                    .take(engine.pending_len)
                    .flatten()
                    .any(|action| matches!(
                        action,
                        ScheduledAction::Trigger {
                            pad,
                            source: ActionSource::Live(_),
                            ..
                        } if *pad == preserved_pad
                    ))
            );
        }
    }

    #[test]
    fn one_callback_crossing_a_switch_boundary_schedules_both_slots() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad_a = PadId::first();
        let pad_b = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        for pad in [pad_a, pad_b] {
            install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 64);
        }
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(8, pad_a)]))
            .unwrap();
        controller
            .install_pattern(pattern_snapshot_with_triggers(
                1,
                100,
                &[(0, pad_b), (2, pad_b)],
            ))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(5, |_| {});
        controller
            .select_pattern(PatternSlotId::new(1).unwrap(), PatternSwitch::NextBoundary)
            .unwrap();

        engine.render_frames(15, |_| {});

        assert_eq!(engine.voices_for_pad(pad_a), 1);
        assert_eq!(engine.voices_for_pad(pad_b), 2);
    }

    #[test]
    fn a_callback_spanning_five_loops_emits_the_earliest_sixty_four_and_counts_the_rest() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        let events = (0..16).map(|index| (index % 10, pad)).collect::<Vec<_>>();
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &events))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        engine.render_frames(50, |_| {});

        assert_eq!(engine.executed_triggers(), 64);
        assert_eq!(engine.last_triggered_frame, Some(39));
        assert_eq!(engine.pattern_overflows(), 16);
    }

    #[test]
    fn pattern_capacity_preserves_the_earliest_actions_in_remaining_future_slots() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        for _ in 0..36 {
            controller.trigger(pad, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pending_actions(), 36);
        let events = (0..16).map(|index| (index % 10, pad)).collect::<Vec<_>>();
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &events))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        engine.render_frames(50, |_| {});

        assert_eq!(engine.executed_triggers(), 28);
        assert_eq!(engine.last_triggered_frame, Some(15));
        assert_eq!(engine.pending_actions(), 36);
        assert_eq!(engine.pattern_overflows(), 52);
    }

    fn arm_recording_capture(
        controller: &mut AudioController,
        engine: &mut AudioEngine,
        sample_rate: u32,
    ) {
        let slot = PatternSlotId::new(0).unwrap();
        let snapshot = pattern_snapshot_with_triggers(0, sample_rate, &[]);
        let generation = snapshot.generation();
        controller.install_pattern(snapshot).unwrap();
        controller
            .select_pattern(slot, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        controller
            .set_record_capture(Some((slot, generation)))
            .unwrap();
        engine.render_frames(0, |_| {});
    }

    #[test]
    fn tracked_live_ack_and_sound_share_observed_callback_plus_sixty_four() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        arm_recording_capture(&mut controller, &mut engine, 100);
        let observed_at = engine.rendered_frame();
        let id = controller.trigger_live_tracked(pad, 0.75).unwrap();
        let mut callback_frame = observed_at;
        let mut onset = None;

        engine.render_frames(128, |frame| {
            if onset.is_none() && frame != [0.0, 0.0] {
                onset = Some(callback_frame);
            }
            callback_frame += 1;
        });

        let mut acks = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut acks), 1);
        assert_eq!(onset, Some(observed_at + 64));
        assert_eq!((acks[0].id, acks[0].frame), (id, observed_at + 64));
    }

    #[test]
    fn live_ack_transport_uses_the_pattern_entered_at_its_execution_boundary() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(1_000, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            1_000,
            pad,
            PadSettings::default(),
            256,
        );
        let first = pattern_snapshot_with_triggers(0, 1_000, &[]);
        let second = pattern_snapshot_with_triggers(1, 1_000, &[(50, pad)]);
        let second_generation = second.generation();
        let slot_zero = PatternSlotId::new(0).unwrap();
        let slot_one = PatternSlotId::new(1).unwrap();
        controller.install_pattern(first).unwrap();
        controller.install_pattern(second).unwrap();
        controller
            .select_pattern(slot_zero, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(36, |_| {});
        controller
            .select_pattern(slot_one, PatternSwitch::NextBoundary)
            .unwrap();
        controller.set_record_capture(Some((slot_zero, 0))).unwrap();
        let id = controller.trigger_live_tracked(pad, 1.0).unwrap();
        engine.render_frames(0, |_| {});
        controller
            .set_record_capture(Some((slot_one, second_generation)))
            .unwrap();
        engine.render_frames(0, |_| {});
        let mut callback_frame = 36;
        let mut onset = None;

        engine.render_frames(65, |frame| {
            if onset.is_none() && frame != [0.0, 0.0] {
                onset = Some(callback_frame);
            }
            callback_frame += 1;
        });

        let mut acks = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut acks), 1);
        assert_eq!((onset, acks[0].id, acks[0].frame), (Some(100), id, 100));
        assert_eq!(
            acks[0].transport,
            Some(TransportStamp {
                slot: slot_one,
                generation: second_generation,
                origin: 100,
                loop_frames: 100,
            })
        );
    }

    #[test]
    fn invalid_or_fenced_live_trigger_never_emits_ack() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        arm_recording_capture(&mut controller, &mut engine, 100);
        let missing_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        controller.trigger_live_tracked(missing_pad, 1.0).unwrap();
        engine.render_frames(65, |_| {});
        controller
            .trigger_live_tracked(PadId::first(), 1.0)
            .unwrap();
        controller.stop_all().unwrap();
        engine.render_frames(65, |_| {});

        assert_eq!(
            controller.drain_live_acks(&mut [crate::LiveAck::EMPTY; 2]),
            0
        );
    }

    #[test]
    fn live_frame_is_fixed_across_repeated_short_callbacks() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            128,
        );
        arm_recording_capture(&mut controller, &mut engine, 100);
        let id = controller.trigger_live_tracked(pad, 1.0).unwrap();

        engine.render_frames(32, |frame| assert_eq!(frame, [0.0, 0.0]));
        assert_eq!(engine.queued_commands(), 0);
        assert_eq!(engine.pending_actions(), 1);
        engine.render_frames(32, |frame| assert_eq!(frame, [0.0, 0.0]));
        let mut onset = None;
        engine.render_frames(1, |frame| {
            if frame != [0.0, 0.0] {
                onset = Some(64);
            }
        });

        let mut acks = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut acks), 1);
        assert_eq!(onset, Some(64));
        assert_eq!((acks[0].id, acks[0].frame), (id, 64));
    }

    #[test]
    fn sixty_four_non_live_and_sixty_four_live_share_exactly_128_slots() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        for _ in 0..64 {
            controller.trigger(pad, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        for _ in 0..64 {
            controller.trigger_live(pad, 1.0).unwrap();
        }

        engine.render_frames(0, |_| {});

        assert_eq!(engine.pending_actions(), 128);
        assert_eq!(
            engine
                .pending
                .iter()
                .flatten()
                .filter(|action| {
                    !matches!(
                        action,
                        ScheduledAction::Trigger {
                            source: ActionSource::Live(_),
                            ..
                        } | ScheduledAction::Release {
                            source: ActionSource::Live(_),
                            ..
                        }
                    )
                })
                .count(),
            64
        );
        controller.trigger_live(pad, 1.0).unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pending_actions(), 128);
        assert_eq!(engine.queued_commands(), 1);
    }

    #[test]
    fn non_live_quota_blocks_only_the_non_live_lane_head() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        for _ in 0..65 {
            controller.trigger(pad, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        controller.trigger_live(pad, 1.0).unwrap();

        engine.render_frames(0, |_| {});

        assert_eq!(engine.pending_actions(), 65);
        assert_eq!(engine.queued_commands(), 1);
        assert!(engine.pending.iter().flatten().any(|action| {
            matches!(
                action,
                ScheduledAction::Trigger {
                    source: ActionSource::Live(_),
                    ..
                } | ScheduledAction::Release {
                    source: ActionSource::Live(_),
                    ..
                }
            )
        }));
    }

    #[test]
    fn full_non_live_quota_does_not_delay_live_onset_or_ack_in_a_large_block() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        arm_recording_capture(&mut controller, &mut engine, 100);
        for _ in 0..NON_LIVE_PENDING_COUNT {
            controller.trigger(pad, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);
        let observed_at = engine.rendered_frame();
        controller.trigger_live_tracked(pad, 1.0).unwrap();
        let mut callback_frame = observed_at;
        let mut onset = None;

        engine.render_frames(128, |frame| {
            if onset.is_none() && frame != [0.0, 0.0] {
                onset = Some(callback_frame);
            }
            callback_frame += 1;
        });

        let mut acks = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut acks), 1);
        assert_eq!(onset, Some(observed_at + 64));
        assert_eq!(acks[0].frame, observed_at + 64);
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);
    }

    #[test]
    fn short_callbacks_preserve_the_initial_resolved_live_frame() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        arm_recording_capture(&mut controller, &mut engine, 100);
        let id = controller.trigger_live_tracked(pad, 1.0).unwrap();

        engine.render_frames(64, |frame| assert_eq!(frame, [0.0, 0.0]));
        let mut onset = None;
        let mut callback_frame = 64;
        engine.render_frames(32, |frame| {
            if onset.is_none() && frame != [0.0, 0.0] {
                onset = Some(callback_frame);
            }
            callback_frame += 1;
        });

        let mut acks = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut acks), 1);
        assert_eq!(engine.queued_commands(), 0);
        assert_eq!(onset, Some(64));
        assert_eq!((acks[0].id, acks[0].frame), (id, 64));
    }

    #[test]
    fn tracked_live_release_executes_on_its_acknowledged_frame() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let gate = PadSettings::new(PlaybackMode::Gate, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 100, pad, gate, 256);
        arm_recording_capture(&mut controller, &mut engine, 100);
        controller.trigger_live_tracked(pad, 1.0).unwrap();
        engine.render_frames(65, |_| {});
        let mut trigger_ack = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut trigger_ack), 1);
        let observed_at = engine.rendered_frame();
        let release_id = controller.release_live_tracked(pad).unwrap();

        engine.render_frames(65, |_| {});

        let voice = engine
            .voices
            .iter()
            .flatten()
            .find(|voice| voice.pad == pad)
            .unwrap();
        let mut release_ack = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut release_ack), 1);
        assert_eq!(voice.envelope.release_frame, Some(1));
        assert_eq!(release_ack[0].id, release_id);
        assert_eq!(release_ack[0].kind, LiveAckKind::Release);
        assert_eq!(release_ack[0].frame, observed_at + 64);
    }

    #[test]
    fn pattern_stop_cancels_pattern_actions_but_not_post_stop_live_input() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad_a = PadId::first();
        let pad_b = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        for pad in [pad_a, pad_b] {
            install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 64);
        }
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(8, pad_a)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(0, |_| {});

        controller.stop_pattern().unwrap();
        controller.trigger_live_tracked(pad_b, 1.0).unwrap();
        engine.render_frames(128, |_| {});

        assert_eq!(engine.voices_for_pad(pad_a), 0);
        assert_eq!(engine.voices_for_pad(pad_b), 1);
        assert_eq!(engine.last_triggered_frame, Some(64));
    }

    #[test]
    fn pre_fence_pattern_install_is_available_to_post_fence_select_and_play() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(2, pad)]))
            .unwrap();
        controller.stop_all().unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        engine.render_frames(10, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(2));
        assert_eq!(engine.invalid_commands(), 0);
    }

    #[test]
    fn pre_fence_select_is_not_reused_by_post_fence_play() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(2, pad)]))
            .unwrap();
        engine.render_frames(0, |_| {});
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.stop_all().unwrap();
        controller.play_pattern().unwrap();

        engine.render_frames(10, |_| {});

        assert_eq!(engine.executed_triggers(), 0);
        assert_eq!(engine.pattern_player.selected_slot, None);
    }

    #[test]
    fn late_observed_older_fence_preserves_newer_pattern_state() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let slot_zero = PatternSlotId::new(0).unwrap();
        let slot_one = PatternSlotId::new(1).unwrap();
        let first = pattern_snapshot_with_triggers(0, 100, &[]);
        let first_generation = first.generation();
        controller.install_pattern(first).unwrap();
        controller
            .install_pattern(pattern_snapshot_with_triggers(1, 100, &[]))
            .unwrap();
        controller
            .select_pattern(slot_zero, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        controller
            .set_record_capture(Some((slot_zero, first_generation)))
            .unwrap();
        engine.render_frames(5, |_| {});
        controller
            .select_pattern(slot_one, PatternSwitch::NextBoundary)
            .unwrap();
        engine.render_frames(0, |_| {});

        engine.apply_stop_fence_sequence(0);

        assert!(engine.pattern_player.playing);
        assert_eq!(engine.pattern_player.selected_slot, Some(slot_zero));
        assert_eq!(
            engine
                .pattern_player
                .pending_switch
                .map(|pending| pending.slot),
            Some(slot_one)
        );
        assert_eq!(
            engine
                .pattern_player
                .record_capture
                .map(|capture| (capture.slot, capture.generation)),
            Some((slot_zero, first_generation)),
        );
    }

    #[test]
    fn late_observed_older_fence_preserves_newer_pattern_voice() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 32);
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(0, pad)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(1, |_| {});
        let voice_sequence = engine
            .voices
            .iter()
            .flatten()
            .find(|voice| voice.pad == pad)
            .unwrap()
            .sequence;

        engine.apply_stop_fence_sequence(voice_sequence.wrapping_sub(1));

        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .any(|voice| voice.pad == pad && voice.envelope.release_frame.is_none())
        );
    }

    #[test]
    fn pattern_replacement_waits_for_exact_retirement_capacity() {
        let (mut controller, mut ports) = audio_channels();
        let filler = pattern_snapshot_with_triggers(15, 100, &[]);
        let filler_owner = controller.install_pattern(Arc::clone(&filler)).unwrap();
        let filler_command_snapshot = match ports.immediate_commands.pop().unwrap() {
            AudioCommand::InstallPattern { snapshot, .. } => snapshot,
            command => panic!("expected pattern install, got {command:?}"),
        };
        for _ in 0..crate::PATTERN_RETIREMENT_CAPACITY {
            ports
                .pattern_retirements
                .push(crate::PatternRetirement::new(
                    filler_owner,
                    Arc::clone(&filler_command_snapshot),
                ))
                .unwrap();
        }

        let mut engine = AudioEngine::new(100, ports).unwrap();
        let first = pattern_snapshot_with_triggers(0, 100, &[]);
        let first_weak = Arc::downgrade(&first);
        let first_owner = controller.install_pattern(first).unwrap();
        engine.render_frames(0, |_| {});
        controller
            .install_pattern(pattern_snapshot_with_triggers(
                0,
                100,
                &[(2, PadId::first())],
            ))
            .unwrap();
        controller.trigger(PadId::first(), 1, 1.0).unwrap();
        controller.trigger_live(PadId::first(), 1.0).unwrap();

        engine.render_frames(128, |_| {});

        assert_eq!(engine.queued_commands(), 2);
        assert_eq!(engine.executed_triggers(), 0);
        assert!(first_weak.upgrade().is_some());
        assert_eq!(controller.reclaim_retired_pattern(), Some(filler_owner));

        engine.render_frames(128, |_| {});

        assert_eq!(engine.queued_commands(), 0);
        assert!(first_weak.upgrade().is_some());
        assert_eq!(controller.reclaim_retired_pattern(), Some(first_owner));
        assert!(first_weak.upgrade().is_none());
        drop(filler_command_snapshot);
        drop(filler);
    }

    #[test]
    fn huge_pattern_interval_saturates_overflow_without_scanning_dropped_loops() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let events = (0..16).map(|index| (index % 10, pad)).collect::<Vec<_>>();
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &events))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(0, |_| {});
        engine.pattern_player.overflow_count = u64::MAX - 1;

        engine.schedule_pattern_actions(u64::MAX);

        assert_eq!(engine.pending_actions(), MAX_PATTERN_ACTIONS_PER_CALLBACK);
        assert_eq!(engine.pattern_overflows(), u64::MAX);
    }

    #[test]
    fn pattern_transport_telemetry_uses_the_callback_clock() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        arm_recording_capture(&mut controller, &mut engine, 100);

        engine.render_frames(12, |_| {});

        let telemetry = controller.latest_telemetry().unwrap();
        assert_eq!(telemetry.pattern_slot, Some(PatternSlotId::new(0).unwrap()));
        assert_eq!(telemetry.pattern_generation, Some(0));
        assert!(telemetry.pattern_playing);
        assert!(telemetry.pattern_recording);
        assert_eq!(telemetry.pattern_origin, Some(0));
        assert_eq!(telemetry.pattern_playhead, 2);
        assert_eq!(telemetry.pattern_loop_count, 1);
        assert_eq!(telemetry.pattern_overflows, 0);
        assert_eq!(telemetry.live_ack_overflows, 0);
    }

    #[test]
    fn acknowledgement_overflow_is_typed_telemetry_not_callback_work_growth() {
        let (mut controller, mut ports) = audio_channels();
        for _ in 0..crate::LIVE_ACK_CAPACITY {
            ports.live_acks.push(crate::LiveAck::EMPTY).unwrap();
        }
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            pad,
            PadSettings::default(),
            1,
        );
        arm_recording_capture(&mut controller, &mut engine, 100);
        controller.trigger_live_tracked(pad, 1.0).unwrap();

        engine.render_frames(128, |_| {});

        assert_eq!(controller.latest_telemetry().unwrap().live_ack_overflows, 1);
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
    fn live_trigger_enqueued_during_a_callback_resolves_in_the_next_large_callback() {
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

        engine.render_frames(65, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(577));
        assert_eq!(engine.late_commands(), 0);
    }

    #[test]
    fn live_trigger_runs_when_the_non_live_quota_is_full() {
        let (mut controller, mut engine) = harness();
        controller
            .install(
                PadId::first(),
                constant_sample(1_024, 0.25),
                PadSettings::default(),
            )
            .unwrap();
        engine.render_frames(1, |_| {});
        for _ in 0..NON_LIVE_PENDING_COUNT {
            controller.trigger(PadId::first(), 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);

        controller.trigger_live(PadId::first(), 1.0).unwrap();
        engine.render_frames(65, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(65));
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);
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
        for _ in 0..NON_LIVE_PENDING_COUNT {
            controller.trigger(first, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);

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

        engine.render_frames(129, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(128));
        assert_eq!(engine.voices_for_pad(second), 0);
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);
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

        engine.render_frames(65, |_| {});

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.last_triggered_frame, Some(65));
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
        for _ in 0..NON_LIVE_PENDING_COUNT {
            controller.trigger(pad, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});

        controller.trigger(pad, 20_000, 1.0).unwrap();
        controller.trigger_live(pad, 1.0).unwrap();
        engine.render_frames(65, |_| {});
        assert_eq!(engine.executed_triggers(), 1);

        controller.stop_all().unwrap();
        controller.trigger_live(pad, 1.0).unwrap();
        engine.render_frames(65, |_| {});

        assert_eq!(engine.executed_triggers(), 2);
        assert_eq!(engine.last_triggered_frame, Some(193));
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
        engine.render_frames(65, |_| {});

        engine.apply_stop_fence();
        controller.stop_all().unwrap();
        controller.trigger_live(pad, 1.0).unwrap();
        controller
            .trigger(pad, engine.rendered_frame(), 1.0)
            .unwrap();
        engine.render_frames(65, |_| {});
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

        engine.render_frames(65, |_| {});

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
    fn full_non_live_quota_leaves_additional_timed_actions_in_the_command_queue() {
        let (mut controller, mut engine) = harness();
        for frame in 1000..1130 {
            controller.trigger(PadId::first(), frame, 1.0).unwrap();
        }
        let mut output = [0.0; 2];
        for _ in 0..3 {
            engine.render_stereo(&mut output);
        }
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);
        assert_eq!(engine.queued_commands(), 66);
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
            for frame in 10_000..10_066 {
                controller.trigger(PadId::first(), frame, 1.0).unwrap();
            }
            engine.render_frames(0, |_| {});
            assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);
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
