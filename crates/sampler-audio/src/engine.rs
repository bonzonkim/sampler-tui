use std::array;
use std::f32::consts::PI;
use std::sync::Arc;

use rtrb::PushError;
use sampler_core::{
    EventId, FIRST_LOOP_VALID_MASK_WORDS, Frame, MasterMixSettings, PATTERN_SLOT_COUNT, PadId,
    PadMixSettings, PadSettings, PatternAction, PatternActionKind, PatternSlotId, PatternSnapshot,
    PlaybackMode, VoiceAllocator, VoiceId, VoiceRequest,
};

use crate::fx::FxRack;
use crate::{
    AudioCommand, CaptureState, CriticalEvent, EngineError, EnginePorts, LiveAck, LiveAckKind,
    LiveCommandId, PatternRetirement, PatternSnapshotSlot, PatternSwitch, SAMPLE_SLOT_COUNT,
    SampleBuffer, SampleSlot, Telemetry, TransportStamp,
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
const ROUTE_RAMP_FRAMES: u32 = 64;

struct SampleEntry {
    buffer: Option<Arc<SampleBuffer>>,
    pad_references: u16,
    retiring: bool,
}

#[derive(Clone, Copy)]
struct PadBinding {
    slot: Option<SampleSlot>,
    settings: PadSettings,
    mix: PadMixSettings,
    route: PadRoute,
}

#[derive(Clone, Copy)]
struct RouteRamp {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl RouteRamp {
    const fn new(value: f32) -> Self {
        Self {
            current: value,
            target: value,
            step: 0.0,
            remaining: 0,
        }
    }

    fn set_target(&mut self, target: f32) {
        if self.target.to_bits() == target.to_bits() {
            return;
        }
        self.target = target;
        self.step = (target - self.current) / ROUTE_RAMP_FRAMES as f32;
        self.remaining = ROUTE_RAMP_FRAMES;
    }

    fn advance(&mut self, frames: Frame) {
        let steps =
            u32::try_from(frames.min(Frame::from(self.remaining))).unwrap_or(self.remaining);
        if steps == 0 {
            return;
        }
        self.current += self.step * steps as f32;
        self.remaining -= steps;
        if self.remaining == 0 {
            self.current = self.target;
        }
    }
}

#[derive(Clone, Copy)]
struct PadRoute {
    level: RouteRamp,
    pan: RouteRamp,
    audible: RouteRamp,
    delay_send: RouteRamp,
    reverb_send: RouteRamp,
    last_frame: Option<Frame>,
}

#[derive(Clone, Copy)]
struct PadRouteValues {
    level: f32,
    pan: f32,
    audible: f32,
    delay_send: f32,
    reverb_send: f32,
}

impl PadRoute {
    fn new(settings: PadSettings, mix: PadMixSettings, at_frame: Frame) -> Self {
        Self {
            level: RouteRamp::new(db_to_gain(settings.gain_db)),
            pan: RouteRamp::new(settings.pan),
            audible: RouteRamp::new(if mix.muted { 0.0 } else { 1.0 }),
            delay_send: RouteRamp::new(mix.delay_send),
            reverb_send: RouteRamp::new(mix.reverb_send),
            last_frame: at_frame.checked_sub(1),
        }
    }

    fn set_pad_settings(&mut self, settings: PadSettings, at_frame: Frame) {
        self.advance_before(at_frame);
        self.level.set_target(db_to_gain(settings.gain_db));
        self.pan.set_target(settings.pan);
    }

    fn set_mix_settings(&mut self, settings: PadMixSettings, at_frame: Frame) {
        self.advance_before(at_frame);
        self.audible
            .set_target(if settings.muted { 0.0 } else { 1.0 });
        self.delay_send.set_target(settings.delay_send);
        self.reverb_send.set_target(settings.reverb_send);
    }

    fn values_for_frame(&mut self, frame: Frame) -> PadRouteValues {
        self.advance_through(frame);
        PadRouteValues {
            level: self.level.current,
            pan: self.pan.current,
            audible: self.audible.current,
            delay_send: self.delay_send.current,
            reverb_send: self.reverb_send.current,
        }
    }

    fn advance_before(&mut self, frame: Frame) {
        if let Some(previous) = frame.checked_sub(1) {
            self.advance_through(previous);
        }
    }

    fn advance_through(&mut self, frame: Frame) {
        if self.last_frame.is_some_and(|last| last >= frame) {
            return;
        }
        let elapsed = self.last_frame.map_or_else(
            || frame.saturating_add(1),
            |last| frame.saturating_sub(last),
        );
        self.level.advance(elapsed);
        self.pan.advance(elapsed);
        self.audible.advance(elapsed);
        self.delay_send.advance(elapsed);
        self.reverb_send.advance(elapsed);
        self.last_frame = Some(frame);
    }
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
    velocity: f32,
    envelope: Envelope,
    sequence: u64,
    pattern_voice: Option<PatternVoiceId>,
    live_trigger: Option<LiveCommandId>,
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
        target_live_trigger: Option<LiveCommandId>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    ) -> Option<PatternTransition> {
        let incoming = self.installed(slot).map(|pattern| PatternGenerationId {
            slot: pattern.snapshot.slot(),
            generation: pattern.snapshot.generation(),
        })?;
        if !self.playing || switch_at == PatternSwitch::Immediate {
            let outgoing = self.current_generation_id();
            self.selected_slot = Some(slot);
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
            self.pending_switch = None;
            self.origin = now;
            self.loop_count = 0;
            return outgoing.map(|outgoing| PatternTransition { outgoing, incoming });
        } else {
            self.pending_switch = Some(PendingPatternSwitch {
                slot,
                at_frame: boundary,
            });
        }
        None
    }

    fn play(&mut self, now: Frame, sequence: u64) -> Option<PatternTransition> {
        let transition = if !self.playing {
            self.pending_switch.take().and_then(|pending| {
                let outgoing = self.current_generation_id()?;
                let incoming = self
                    .installed(pending.slot)
                    .map(|pattern| PatternGenerationId {
                        slot: pattern.snapshot.slot(),
                        generation: pattern.snapshot.generation(),
                    })?;
                self.selected_slot = Some(pending.slot);
                Some(PatternTransition { outgoing, incoming })
            })
        } else {
            self.pending_switch = None;
            None
        };
        self.playing = true;
        self.play_sequence = sequence;
        self.origin = now;
        self.loop_count = 0;
        transition
    }

    fn stop(&mut self) {
        self.playing = false;
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

    fn pending_generation_id(&self) -> Option<PatternGenerationId> {
        let pending = self.pending_switch?;
        self.installed(pending.slot)
            .map(|pattern| PatternGenerationId {
                slot: pattern.snapshot.slot(),
                generation: pattern.snapshot.generation(),
            })
    }

    fn apply_fence(&mut self, fence: u64) {
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

#[derive(Clone, Copy)]
struct PatternIntervalPlan {
    interval: PatternInterval,
    loop_frames: Frame,
    action_len: usize,
    first_loop: u128,
    end_loop: u128,
    first_index: usize,
    end_index: usize,
    first_loop_valid: usize,
    total: u128,
}

impl PatternIntervalPlan {
    fn new(snapshot: &PatternSnapshot, interval: PatternInterval) -> Option<Self> {
        let start = interval.start.max(interval.origin);
        let loop_frames = snapshot.loop_frames();
        let action_len = snapshot.actions().len();
        if loop_frames == 0 || action_len == 0 || start >= interval.end {
            return None;
        }
        let relative_start = start.checked_sub(interval.origin)?;
        let relative_end = interval.end.checked_sub(interval.origin)?;
        let first_loop = u128::from(relative_start / loop_frames);
        let end_loop = u128::from(relative_end / loop_frames);
        let first_index = snapshot.action_index_at_or_after(relative_start % loop_frames);
        let end_index = snapshot.action_index_at_or_after(relative_end % loop_frames);
        let first_segment_end = if first_loop == end_loop {
            end_index
        } else {
            action_len
        };
        let first_loop_valid = if first_loop == 0 {
            snapshot.first_loop_valid_count(first_index, first_segment_end)
        } else {
            first_segment_end.saturating_sub(first_index)
        };
        let total = if first_loop == end_loop {
            first_loop_valid as u128
        } else {
            let middle_loops = end_loop.saturating_sub(first_loop).saturating_sub(1);
            let middle_count = middle_loops.saturating_mul(action_len as u128);
            (first_loop_valid as u128)
                .saturating_add(middle_count)
                .saturating_add(end_index as u128)
        };
        Some(Self {
            interval,
            loop_frames,
            action_len,
            first_loop,
            end_loop,
            first_index,
            end_index,
            first_loop_valid,
            total,
        })
    }
}

struct PatternWorkBudget {
    mask_words_remaining: usize,
    action_reads_remaining: usize,
    #[cfg(test)]
    mask_word_loads: usize,
    #[cfg(test)]
    action_reads: usize,
}

impl PatternWorkBudget {
    fn new() -> Self {
        Self {
            mask_words_remaining: FIRST_LOOP_VALID_MASK_WORDS,
            action_reads_remaining: MAX_PATTERN_ACTIONS_PER_CALLBACK,
            #[cfg(test)]
            mask_word_loads: 0,
            #[cfg(test)]
            action_reads: 0,
        }
    }

    fn load_mask_word(&mut self, snapshot: &PatternSnapshot, word_index: usize) -> Option<u64> {
        if self.mask_words_remaining == 0 {
            return None;
        }
        self.mask_words_remaining -= 1;
        #[cfg(test)]
        {
            self.mask_word_loads += 1;
        }
        Some(snapshot.first_loop_valid_word(word_index))
    }

    fn read_action(
        &mut self,
        snapshot: &PatternSnapshot,
        action_index: usize,
    ) -> Option<PatternAction> {
        if self.action_reads_remaining == 0 {
            return None;
        }
        self.action_reads_remaining -= 1;
        #[cfg(test)]
        {
            self.action_reads += 1;
        }
        snapshot.actions().get(action_index).copied()
    }
}

enum PatternCursorStep {
    Action(PatternAction, Frame, u128),
    Finished,
    BudgetExhausted,
}

struct PatternIntervalCursor {
    plan: PatternIntervalPlan,
    loop_index: u128,
    action_index: usize,
    first_loop_valid_remaining: usize,
    cached_mask_word: Option<(usize, u64)>,
    remaining: u128,
}

impl PatternIntervalCursor {
    fn new(plan: PatternIntervalPlan) -> Self {
        Self {
            plan,
            loop_index: plan.first_loop,
            action_index: plan.first_index,
            first_loop_valid_remaining: plan.first_loop_valid,
            cached_mask_word: None,
            remaining: plan.total,
        }
    }

    fn next(
        &mut self,
        snapshot: &PatternSnapshot,
        budget: &mut PatternWorkBudget,
    ) -> PatternCursorStep {
        if self.remaining == 0 {
            return PatternCursorStep::Finished;
        }
        loop {
            if self.loop_index == 0 {
                if self.first_loop_valid_remaining == 0 {
                    self.loop_index = 1;
                    self.action_index = 0;
                    self.cached_mask_word = None;
                    continue;
                }
                let first_loop_end = if self.plan.end_loop == 0 {
                    self.plan.end_index
                } else {
                    self.plan.action_len
                };
                let word_index = self.action_index / u64::BITS as usize;
                let mut bits = match self.cached_mask_word {
                    Some((cached_index, cached_bits)) if cached_index == word_index => cached_bits,
                    _ => {
                        let Some(word) = budget.load_mask_word(snapshot, word_index) else {
                            return PatternCursorStep::BudgetExhausted;
                        };
                        word
                    }
                };
                bits &= u64::MAX << (self.action_index % u64::BITS as usize);
                let word_start = word_index * u64::BITS as usize;
                if first_loop_end < word_start + u64::BITS as usize {
                    let retained = first_loop_end.saturating_sub(word_start);
                    bits &= if retained == u64::BITS as usize {
                        u64::MAX
                    } else {
                        (1_u64 << retained) - 1
                    };
                }
                if bits == 0 {
                    self.action_index = (word_start + u64::BITS as usize).min(first_loop_end);
                    self.cached_mask_word = None;
                    continue;
                }
                let bit = bits.trailing_zeros() as usize;
                let selected = word_start + bit;
                bits &= bits - 1;
                self.cached_mask_word = Some((word_index, bits));
                self.action_index = selected + 1;
                self.first_loop_valid_remaining -= 1;
                let Some(action) = budget.read_action(snapshot, selected) else {
                    return PatternCursorStep::BudgetExhausted;
                };
                return self.action_step(action);
            }

            if self.loop_index > self.plan.end_loop
                || (self.loop_index == self.plan.end_loop
                    && self.action_index >= self.plan.end_index)
            {
                return PatternCursorStep::Finished;
            }
            if self.action_index == self.plan.action_len {
                let Some(next_loop) = self.loop_index.checked_add(1) else {
                    return PatternCursorStep::Finished;
                };
                self.loop_index = next_loop;
                self.action_index = 0;
                continue;
            }
            let selected = self.action_index;
            self.action_index += 1;
            let Some(action) = budget.read_action(snapshot, selected) else {
                return PatternCursorStep::BudgetExhausted;
            };
            return self.action_step(action);
        }
    }

    fn action_step(&mut self, action: PatternAction) -> PatternCursorStep {
        let Some(loop_offset) = self
            .loop_index
            .checked_mul(u128::from(self.plan.loop_frames))
        else {
            return PatternCursorStep::Finished;
        };
        let Some(absolute) = u128::from(self.plan.interval.origin)
            .checked_add(loop_offset)
            .and_then(|base| base.checked_add(u128::from(action.frame)))
        else {
            return PatternCursorStep::Finished;
        };
        let Ok(at_frame) = Frame::try_from(absolute) else {
            return PatternCursorStep::Finished;
        };
        self.remaining -= 1;
        PatternCursorStep::Action(action, at_frame, self.loop_index)
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
    fx: FxRack,
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
        Self::new_with_master_mix(sample_rate, ports, MasterMixSettings::default())
    }

    /// Constructs fresh engine state with persisted master/FX values active at frame zero.
    ///
    /// Live updates continue to use [`crate::AudioController::update_master_mix`] and its
    /// click-suppressing ramps. This setup-only path revalidates the supplied public settings
    /// before initializing the otherwise-zeroed effect rack directly.
    pub fn new_with_master_mix(
        sample_rate: u32,
        ports: EnginePorts,
        master_mix: MasterMixSettings,
    ) -> Result<Self, EngineError> {
        if sample_rate == 0 {
            return Err(EngineError::ZeroSampleRate);
        }
        master_mix
            .validate()
            .map_err(|_| EngineError::InvalidSettings)?;

        let telemetry_interval = telemetry_interval(sample_rate);
        Ok(Self {
            sample_rate,
            ports,
            fx: FxRack::new(sample_rate, master_mix)?,
            samples: array::from_fn(|_| SampleEntry {
                buffer: None,
                pad_references: 0,
                retiring: false,
            }),
            pads: array::from_fn(|_| PadBinding {
                slot: None,
                settings: PadSettings::default(),
                mix: PadMixSettings::default(),
                route: PadRoute::new(PadSettings::default(), PadMixSettings::default(), 0),
            }),
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

    pub fn render_frames(&mut self, frame_count: usize, write_frame: impl FnMut([f32; 2])) {
        self.render_frames_inner(frame_count, || {}, || {}, write_frame);
    }

    #[cfg(test)]
    fn render_frames_with_after_initial_fence_poll_hook(
        &mut self,
        frame_count: usize,
        after_initial_fence_poll: impl FnOnce(),
        write_frame: impl FnMut([f32; 2]),
    ) {
        self.render_frames_inner(frame_count, after_initial_fence_poll, || {}, write_frame);
    }

    #[cfg(test)]
    fn render_frames_with_capture_progress_fence_hook(
        &mut self,
        frame_count: usize,
        capture_progress_fence: impl FnMut(),
        write_frame: impl FnMut([f32; 2]),
    ) {
        self.render_frames_inner(frame_count, || {}, capture_progress_fence, write_frame);
    }

    fn render_frames_inner(
        &mut self,
        frame_count: usize,
        after_initial_fence_poll: impl FnOnce(),
        mut capture_progress_fence: impl FnMut(),
        mut write_frame: impl FnMut([f32; 2]),
    ) {
        let frame_count_as_frame = Frame::try_from(frame_count).unwrap_or(Frame::MAX);
        let horizon = self.rendered_frame.saturating_add(frame_count_as_frame);
        self.ports.publish_render_horizon(horizon);

        self.flush_deferred_retirement();
        self.flush_pattern_retirement();
        self.advance_pattern_to(self.rendered_frame);
        self.apply_stop_fence();
        after_initial_fence_poll();
        self.drain_commands(horizon);
        self.schedule_pattern_actions(horizon);
        self.ports.capture.poll_commands();

        for _ in 0..frame_count {
            self.advance_pattern_to(self.rendered_frame);
            self.execute_due_actions();
            let frame = self.render_frame();
            let capture_was_recording = self.ports.capture.state() == CaptureState::Recording;
            if capture_was_recording {
                self.ports.capture_progress.record_frame(frame);
            }
            capture_progress_fence();
            self.ports.capture.push_frame(frame);
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
        let PatternTransition { outgoing, incoming } = transition;
        if self
            .pattern_player
            .record_capture
            .is_some_and(|capture| capture.slot != incoming.slot)
        {
            self.pattern_player.record_capture = None;
        }
        self.release_sustained_pattern_generation(outgoing.slot, outgoing.generation);
        self.cancel_pattern_generation(outgoing.slot, outgoing.generation);
        debug_assert_eq!(self.pattern_player.current_generation_id(), Some(incoming));
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
        let resolved_live_frame = self.rendered_frame;
        // The controller's SPSC producer and this consumer are FIFO. Equal-frame insertion below
        // is stable, so a tracked trigger is executed and acknowledged before its later release.
        // Recording correlation intentionally relies on that engine order.
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
                target_live_trigger: None,
            }),
            Ok(AudioCommand::ReleaseOwnedLive {
                id,
                target_trigger_id,
                pad,
                sequence,
            }) => Some(ScheduledAction::Release {
                pad: *pad,
                at_frame: resolved_live_frame,
                sequence: *sequence,
                source: ActionSource::Live(*id),
                target_live_trigger: Some(*target_trigger_id),
            }),
            Ok(AudioCommand::Install {
                slot,
                buffer,
                settings,
                mix,
                ..
            }) if self.install_is_invalid(*slot, buffer, *settings, *mix)
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
            Ok(AudioCommand::RemoveSample { pad })
                if self.remove_needs_retirement_capacity(*pad)
                    && self.ports.retirements.slots() == 0 =>
            {
                return false;
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
                target_live_trigger: None,
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
            AudioCommand::Install {
                pad,
                slot,
                buffer,
                settings,
                mix,
                ..
            } => self.install_sample(pad, slot, buffer, settings, mix),
            AudioCommand::UpdatePad { pad, settings } => {
                if settings_are_valid(settings) && self.pad_binding(pad).slot.is_some() {
                    let frame = self.rendered_frame;
                    let binding = self.pad_binding_mut(pad);
                    binding.route.set_pad_settings(settings, frame);
                    binding.settings = settings;
                } else {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                }
            }
            AudioCommand::UpdatePadMix { pad, settings } => {
                if mix_settings_are_valid(settings) && self.pad_binding(pad).slot.is_some() {
                    let frame = self.rendered_frame;
                    let binding = self.pad_binding_mut(pad);
                    binding.route.set_mix_settings(settings, frame);
                    binding.mix = settings;
                } else {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                }
            }
            AudioCommand::UpdateMasterMix { settings } => {
                if master_settings_are_valid(settings) {
                    self.fx.set_settings(settings);
                } else {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                }
            }
            AudioCommand::RemoveSample { pad } => self.remove_sample(pad),
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
                sequence: _,
            } => {
                if self.pattern_player.installed(slot).is_none() {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                    return;
                }
                let transition = self
                    .pattern_player
                    .select(slot, switch_at, self.rendered_frame);
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
                let transition = self.pattern_player.play(self.rendered_frame, sequence);
                if let Some(transition) = transition {
                    self.apply_pattern_transition(transition);
                }
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
                let Some((slot, generation)) = capture else {
                    self.pattern_player.record_capture = None;
                    return;
                };
                let current = self.pattern_player.transport_stamp();
                let pending = self.pattern_player.pending_generation_id();
                if !self.sequence_is_stopped(sequence)
                    && (current
                        .is_some_and(|stamp| (stamp.slot, stamp.generation) == (slot, generation))
                        || pending.is_some_and(|target| {
                            (target.slot, target.generation) == (slot, generation)
                        }))
                {
                    self.pattern_player.record_capture = Some(PatternRecordCapture {
                        slot,
                        generation,
                        sequence,
                    });
                } else {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                }
            }
            AudioCommand::Trigger { .. }
            | AudioCommand::TriggerLive { .. }
            | AudioCommand::Release { .. }
            | AudioCommand::ReleaseLive { .. }
            | AudioCommand::ReleaseOwnedLive { .. } => {
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
        mix: PadMixSettings,
    ) {
        if self.install_is_invalid(slot, &buffer, settings, mix) {
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
            mix,
            route: PadRoute::new(settings, mix, self.rendered_frame),
        };
    }

    fn install_is_invalid(
        &self,
        slot: SampleSlot,
        buffer: &SampleBuffer,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> bool {
        let entry = &self.samples[slot.index()];
        entry.buffer.is_some()
            || entry.retiring
            || buffer.sample_rate() != self.sample_rate
            || !settings_are_valid(settings)
            || !mix_settings_are_valid(mix)
    }

    fn remove_needs_retirement_capacity(&self, pad: PadId) -> bool {
        let Some(slot) = self.pad_binding(pad).slot else {
            return false;
        };
        let entry = &self.samples[slot.index()];
        entry.buffer.is_some()
            && entry.pad_references == 1
            && !self.sample_has_active_voice(slot.index())
    }

    fn remove_sample(&mut self, pad: PadId) {
        let pad_index = pad_index(pad);
        let Some(slot) = self.pads[pad_index].slot.take() else {
            return;
        };

        let entry = &mut self.samples[slot.index()];
        entry.pad_references = entry.pad_references.saturating_sub(1);
        entry.retiring = true;
        self.release_sustained_live_voices(pad);
        self.retire_sample_if_unused(slot.index());
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
        #[cfg(test)]
        {
            self.pattern_action_reads = 0;
            self.pattern_mask_word_reads = 0;
        }
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

        let mut plans = [None; 2];
        let mut total = 0_u128;
        for (index, interval) in intervals.iter().take(interval_len).flatten().enumerate() {
            let Some(pattern) = self.pattern_player.installed(interval.slot) else {
                continue;
            };
            if let Some(plan) = PatternIntervalPlan::new(&pattern.snapshot, *interval) {
                total = total.saturating_add(plan.total);
                plans[index] = Some(plan);
            }
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
        let mut budget = PatternWorkBudget::new();
        'plans: for plan in plans.into_iter().take(interval_len).flatten() {
            if admitted_len == admitted_limit {
                break;
            }
            let Some((slot, generation, loop_frames)) = self
                .pattern_player
                .installed(plan.interval.slot)
                .map(|pattern| {
                    (
                        pattern.snapshot.slot(),
                        pattern.snapshot.generation(),
                        pattern.snapshot.loop_frames(),
                    )
                })
            else {
                continue;
            };
            let mut cursor = PatternIntervalCursor::new(plan);
            while admitted_len < admitted_limit {
                let step = self
                    .pattern_player
                    .installed(plan.interval.slot)
                    .map_or(PatternCursorStep::Finished, |pattern| {
                        cursor.next(&pattern.snapshot, &mut budget)
                    });
                let (action, at_frame, loop_index) = match step {
                    PatternCursorStep::Action(action, at_frame, loop_index) => {
                        (action, at_frame, loop_index)
                    }
                    PatternCursorStep::Finished => break,
                    PatternCursorStep::BudgetExhausted => break 'plans,
                };
                let Some(id) = pattern_voice_id(
                    slot,
                    generation,
                    loop_frames,
                    plan.interval.origin,
                    action,
                    loop_index,
                ) else {
                    continue;
                };
                self.insert_pending(scheduled_pattern_action(action, at_frame, sequence, id));
                admitted_len += 1;
            }
        }

        #[cfg(test)]
        {
            self.pattern_action_reads = budget.action_reads;
            self.pattern_mask_word_reads = budget.mask_word_loads;
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
                let live_trigger = match source {
                    ActionSource::Live(id) => Some(id),
                    _ => None,
                };
                let triggered = self.trigger(pad, velocity, sequence, pattern_voice, live_trigger);
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
                target_live_trigger,
                ..
            } => {
                let valid = self.pad_binding(pad).slot.is_some();
                if !valid {
                    self.invalid_commands = self.invalid_commands.saturating_add(1);
                } else if let ActionSource::Pattern(id) = source {
                    self.release_pattern_voice(id);
                } else if let Some(target_trigger_id) = target_live_trigger {
                    self.release_owned_live_voice(pad, target_trigger_id);
                } else {
                    self.release_sustained_live_voices(pad);
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
        live_trigger: Option<LiveCommandId>,
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
            velocity,
            envelope: Envelope::attack(),
            sequence,
            pattern_voice,
            live_trigger,
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

    fn release_sustained_live_voices(&mut self, pad: PadId) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.pad == pad && matches!(voice.mode, PlaybackMode::Gate | PlaybackMode::Loop) {
                voice.envelope.begin_release();
            }
        }
    }

    fn release_owned_live_voice(&mut self, pad: PadId, trigger_id: LiveCommandId) {
        for voice in self.voices.iter_mut().flatten() {
            if voice.pad == pad
                && voice.live_trigger == Some(trigger_id)
                && matches!(voice.mode, PlaybackMode::Gate | PlaybackMode::Loop)
            {
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

    fn release_sustained_pattern_generation(&mut self, slot: PatternSlotId, generation: u64) {
        for voice in self.voices.iter_mut().flatten() {
            if voice
                .pattern_voice
                .is_some_and(|id| id.slot == slot && id.generation == generation)
                && matches!(voice.mode, PlaybackMode::Gate | PlaybackMode::Loop)
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
        let mut dry = [0.0, 0.0];
        let mut delay_input = [0.0, 0.0];
        let mut reverb_input = [0.0, 0.0];
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
            let rendered_frame = self.rendered_frame;
            let route = self
                .pad_binding_mut(voice.pad)
                .route
                .values_for_frame(rendered_frame);
            let pan_angle = (route.pan + 1.0) * PI / 4.0;
            let routed_gain = route.level * route.audible * voice.velocity * envelope_gain;
            let left = finite_or_zero(
                sample[0] * routed_gain * pan_angle.cos(),
                &mut self.invalid_commands,
            );
            let right = finite_or_zero(
                sample[1] * routed_gain * pan_angle.sin(),
                &mut self.invalid_commands,
            );
            dry[0] = finite_or_zero(dry[0] + left, &mut self.invalid_commands);
            dry[1] = finite_or_zero(dry[1] + right, &mut self.invalid_commands);
            delay_input[0] = finite_or_zero(
                delay_input[0] + left * route.delay_send,
                &mut self.invalid_commands,
            );
            delay_input[1] = finite_or_zero(
                delay_input[1] + right * route.delay_send,
                &mut self.invalid_commands,
            );
            reverb_input[0] = finite_or_zero(
                reverb_input[0] + left * route.reverb_send,
                &mut self.invalid_commands,
            );
            reverb_input[1] = finite_or_zero(
                reverb_input[1] + right * route.reverb_send,
                &mut self.invalid_commands,
            );
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
        let master = self
            .fx
            .process(dry, delay_input, reverb_input, &mut self.invalid_commands);
        [
            soft_limit(master[0], &mut self.invalid_commands),
            soft_limit(master[1], &mut self.invalid_commands),
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

    fn sample_has_active_voice(&self, index: usize) -> bool {
        self.voices
            .iter()
            .flatten()
            .any(|voice| voice.slot.index() == index)
    }

    fn retire_sample_if_unused(&mut self, index: usize) {
        let entry = &self.samples[index];
        if !entry.retiring
            || entry.pad_references != 0
            || self.sample_has_active_voice(index)
            || self.ports.retirements.slots() == 0
        {
            return;
        }

        let Some(buffer) = self.samples[index].buffer.take() else {
            self.samples[index].retiring = false;
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return;
        };
        let Ok(slot) = SampleSlot::new(index) else {
            self.samples[index].buffer = Some(buffer);
            self.invalid_commands = self.invalid_commands.saturating_add(1);
            return;
        };
        let event = CriticalEvent::RetiredSample { slot, buffer };
        match self.ports.retirements.push(event) {
            Ok(()) => self.samples[index].retiring = false,
            Err(PushError::Full(CriticalEvent::RetiredSample { buffer, .. })) => {
                self.samples[index].buffer = Some(buffer);
            }
        }
    }

    fn retire_unused_samples(&mut self) {
        for index in 0..SAMPLE_SLOT_COUNT {
            self.retire_sample_if_unused(index);
        }
    }

    fn pad_binding(&self, pad: PadId) -> &PadBinding {
        &self.pads[pad_index(pad)]
    }

    fn pad_binding_mut(&mut self, pad: PadId) -> &mut PadBinding {
        &mut self.pads[pad_index(pad)]
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
            target_live_trigger: None,
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
    settings.validate().is_ok()
}

fn mix_settings_are_valid(settings: PadMixSettings) -> bool {
    settings.validate().is_ok()
}

fn master_settings_are_valid(settings: MasterMixSettings) -> bool {
    settings.validate().is_ok()
}

fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Weak};

    use super::*;
    use crate::{
        AudioController, CaptureBuffer, CaptureCommand, CaptureOutcome, CaptureSource,
        CaptureState, ControlError, PadId, PadSettings, PatternSwitch, SampleBuffer,
        audio_channels,
        command::{RECOVERY_COMMAND_CAPACITY, audio_channels_with_capacities},
    };
    use sampler_core::{
        BankId, ChokeGroup, DelaySettings, EditablePattern, EventId, MasterMixSettings, Meter,
        PadMixSettings, PatternEvent, PatternSlotId, Resolution, ReverbSettings, Tempo, Transport,
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

    fn sparse_first_loop_snapshot(slot: u8) -> Arc<sampler_core::PatternSnapshot> {
        let transport = Transport::new(
            100,
            Tempo::new(300.0).unwrap(),
            Meter::new(1, 8).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        let mut pattern =
            EditablePattern::new(PatternSlotId::new(slot).unwrap(), "Sparse", transport).unwrap();
        for id in 1..sampler_core::MAX_PATTERN_EVENTS as u64 {
            pattern
                .insert(PatternEvent::new(EventId(id), PadId::first(), 9, 1.0, Some(3)).unwrap())
                .unwrap();
        }
        pattern
            .insert(
                PatternEvent::new(
                    EventId(sampler_core::MAX_PATTERN_EVENTS as u64),
                    PadId::first(),
                    5,
                    1.0,
                    Some(10),
                )
                .unwrap(),
            )
            .unwrap();
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
            .install(
                pad,
                constant_sample_at(sample_rate, frames, 0.5),
                settings,
                PadMixSettings::default(),
            )
            .unwrap();
        engine.render_frames(0, |_| {});
    }

    fn resample_buffer(
        token: u64,
        target: PadId,
        sample_rate: u32,
        max_frames: usize,
    ) -> CaptureBuffer {
        CaptureBuffer::try_new(
            token,
            target,
            CaptureSource::Resample,
            sample_rate,
            max_frames,
        )
        .unwrap()
    }

    fn flatten_frames(frames: &[[f32; 2]]) -> Vec<f32> {
        frames.iter().flatten().copied().collect()
    }

    #[test]
    fn resample_capture_matches_mixed_live_and_pattern_master_between_start_and_stop() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pattern_pad = PadId::first();
        let live_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 100, pattern_pad, looping, 128);
        controller
            .install(
                live_pad,
                constant_sample_at(100, 128, -0.25),
                looping,
                PadMixSettings::default(),
            )
            .unwrap();
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(2, pattern_pad)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        controller.trigger_live(live_pad, 1.0).unwrap();
        engine.render_frames(1, |_| {});

        controller
            .arm_capture(resample_buffer(101, live_pad, 100, 128))
            .unwrap();
        engine.render_frames(0, |_| {});
        let mut before_start = Vec::new();
        engine.render_frames(3, |frame| before_start.push(frame));

        controller.start_capture(101).unwrap();
        let mut expected = Vec::new();
        engine.render_frames(70, |frame| expected.push(frame));

        controller.stop_capture(101).unwrap();
        let mut stop_frame = None;
        engine.render_frames(1, |frame| stop_frame = Some(frame));

        let CaptureOutcome::Completed(completion) = controller
            .try_capture_completion()
            .expect("capture completion")
        else {
            panic!("stop must complete the resample take");
        };
        assert!(before_start.iter().any(|frame| *frame != [0.0, 0.0]));
        assert_ne!(stop_frame, Some([0.0, 0.0]));
        assert_eq!(completion.stereo, flatten_frames(&expected));
        assert_eq!(completion.stereo.len(), expected.len() * 2);
        assert_eq!(completion.token, 101);
        assert_eq!(completion.target, live_pad);
        assert_eq!(completion.source, CaptureSource::Resample);
        assert!(!completion.hard_limit);
    }

    #[test]
    fn post_master_fx_capture_matches_the_final_output_bitwise() {
        fn render_fixture(
            master: MasterMixSettings,
            capture_token: Option<u64>,
        ) -> (Vec<[f32; 2]>, Option<CaptureOutcome>) {
            let (mut controller, ports) = audio_channels();
            let mut engine = AudioEngine::new(1_000, ports).unwrap();
            let pad = PadId::first();
            let mut impulse = vec![0.0; 512];
            impulse[0] = 1.0;
            controller
                .install(
                    pad,
                    Arc::new(SampleBuffer::new(1_000, impulse).unwrap()),
                    PadSettings::new(PlaybackMode::OneShot, 0.0, -1.0, 0.0, None).unwrap(),
                    PadMixSettings::new(false, 1.0, 1.0).unwrap(),
                )
                .unwrap();
            controller.update_master_mix(master).unwrap();
            engine.render_frames(128, |_| {});

            if let Some(token) = capture_token {
                controller
                    .arm_capture(resample_buffer(token, pad, 1_000, 512))
                    .unwrap();
                engine.render_frames(0, |_| {});
                controller.start_capture(token).unwrap();
                engine.render_frames(0, |_| {});
            }

            controller
                .trigger(pad, engine.rendered_frame(), 1.0)
                .unwrap();
            let mut rendered = Vec::with_capacity(256);
            engine.render_frames(256, |frame| rendered.push(frame));

            let completion = capture_token.map(|token| {
                controller.stop_capture(token).unwrap();
                engine.render_frames(0, |_| {});
                let CaptureOutcome::Completed(completion) = controller
                    .try_capture_completion()
                    .expect("post-master capture completion")
                else {
                    panic!("post-master capture must complete")
                };
                completion
            });
            (rendered, completion.map(CaptureOutcome::Completed))
        }

        let dry = render_fixture(MasterMixSettings::default(), None).0;
        let effects = MasterMixSettings::new(
            0.0,
            DelaySettings::new(true, 10, 0.5, 0.0).unwrap(),
            ReverbSettings::new(true, 0.8, 0.4, 0.0).unwrap(),
        )
        .unwrap();
        let (rendered, completion) = render_fixture(effects, Some(201));
        let Some(CaptureOutcome::Completed(completion)) = completion else {
            panic!("post-master capture fixture must return a completion")
        };

        assert_eq!(completion.stereo.len(), rendered.len() * 2);
        for (captured, output) in completion.stereo.chunks_exact(2).zip(&rendered) {
            assert_eq!(
                [captured[0].to_bits(), captured[1].to_bits()],
                [output[0].to_bits(), output[1].to_bits()],
                "capture must contain the exact final post-master output"
            );
        }
        assert!(
            completion
                .stereo
                .chunks_exact(2)
                .zip(&dry)
                .any(|(captured, dry)| {
                    captured[0].to_bits() != dry[0].to_bits()
                        || captured[1].to_bits() != dry[1].to_bits()
                }),
            "enabled effects must differ from the default dry render"
        );
    }

    #[test]
    fn resample_capture_status_is_copy_only_and_hard_limit_stops_exactly() {
        fn assert_copy<T: Copy>(_: T) {}

        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let target = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            100,
            target,
            PadSettings::default(),
            16,
        );
        controller
            .trigger(target, engine.rendered_frame(), 1.0)
            .unwrap();
        controller
            .arm_capture(resample_buffer(102, target, 100, 3))
            .unwrap();
        engine.render_frames(0, |_| {});

        let armed = controller.capture_status().expect("armed status");
        assert_copy(armed);
        assert_eq!(armed.token, 102);
        assert_eq!(armed.source, CaptureSource::Resample);
        assert_eq!(armed.target, target);
        assert_eq!(armed.state, CaptureState::Armed);
        assert_eq!(armed.frames, 0);
        assert_eq!(armed.max_frames, 3);
        assert_eq!(armed.peak, 0.0);
        assert!(!armed.hard_limit);

        controller.start_capture(102).unwrap();
        let mut rendered = Vec::new();
        engine.render_frames(2, |frame| rendered.push(frame));
        let recording = controller.capture_status().expect("recording status");
        assert_eq!(recording.state, CaptureState::Recording);
        assert_eq!(recording.frames, 2);
        assert!(recording.peak > 0.0);
        assert!(!recording.hard_limit);

        engine.render_frames(2, |frame| rendered.push(frame));
        let limited = controller.capture_status().expect("limited status");
        assert_eq!(limited.frames, 3);
        assert_eq!(limited.max_frames, 3);
        assert!(limited.hard_limit);

        let CaptureOutcome::Completed(completion) = controller
            .try_capture_completion()
            .expect("hard-limit completion")
        else {
            panic!("hard limit must complete the capture");
        };
        assert_eq!(completion.stereo, flatten_frames(&rendered[..3]));
        assert!(completion.hard_limit);
        assert_eq!(completion.peak, limited.peak);
        assert_eq!(controller.capture_status(), None);
    }

    #[test]
    fn resample_capture_hard_limit_progress_is_published_before_rearm() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let target = PadId::first();
        controller
            .arm_capture(resample_buffer(106, target, 100, 1))
            .unwrap();
        engine.render_frames(0, |_| {});
        controller.start_capture(106).unwrap();

        let next = resample_buffer(107, target, 100, 2);
        let next_allocation = next.stereo().as_ptr();
        let progress_fence = Arc::new(Barrier::new(2));
        let arm_finished = Arc::new(Barrier::new(2));
        let thread_progress_fence = Arc::clone(&progress_fence);
        let thread_arm_finished = Arc::clone(&arm_finished);
        let (mut controller, arm_result) = std::thread::scope(|scope| {
            let arm = scope.spawn(move || {
                thread_progress_fence.wait();
                let result = controller.arm_capture(next);
                thread_arm_finished.wait();
                (controller, result)
            });
            engine.render_frames_with_capture_progress_fence_hook(
                1,
                || {
                    progress_fence.wait();
                    arm_finished.wait();
                },
                |_| {},
            );
            arm.join().unwrap()
        });

        let failure = arm_result.expect_err("rearm cannot enter before final progress publishes");
        assert_eq!(failure.error(), crate::CaptureError::InvalidState);
        let CaptureCommand::Arm(returned) = failure.into_command() else {
            panic!("rearm rejection must return the exact buffer");
        };
        assert_eq!(returned.stereo().as_ptr(), next_allocation);

        assert!(matches!(
            controller.try_capture_completion(),
            Some(CaptureOutcome::Completed(completion)) if completion.token == 106
        ));
        controller.arm_capture(returned).unwrap();
        let fresh = controller.capture_status().expect("fresh arm status");
        assert_eq!(fresh.token, 107);
        assert_eq!(fresh.frames, 0);
        assert_eq!(fresh.peak, 0.0);
        assert!(!fresh.hard_limit);
        engine.render_frames(0, |_| {});
        controller.cancel_capture(107).unwrap();
        engine.render_frames(0, |_| {});
        assert!(matches!(
            controller.try_capture_completion(),
            Some(CaptureOutcome::Cancelled(buffer)) if buffer.stereo().as_ptr() == next_allocation
        ));
    }

    #[test]
    fn resample_capture_completion_backpressure_keeps_rendering_and_buffer_ownership() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let target = PadId::first();

        controller
            .arm_capture(resample_buffer(103, target, 100, 1))
            .unwrap();
        engine.render_frames(0, |_| {});
        controller.start_capture(103).unwrap();
        engine.render_frames(1, |_| {});

        let second = resample_buffer(104, target, 100, 1);
        let second_allocation = second.stereo().as_ptr();
        controller.arm_capture(second).unwrap();
        engine.render_frames(0, |_| {});
        controller.start_capture(104).unwrap();
        engine.render_frames(1, |_| {});
        assert_eq!(
            controller.capture_status().expect("pending status").state,
            CaptureState::CompletionPending
        );

        let before = engine.rendered_frame();
        let mut rendered = 0;
        engine.render_frames(32, |_| rendered += 1);
        assert_eq!(rendered, 32);
        assert_eq!(engine.rendered_frame(), before + 32);

        assert!(matches!(
            controller.try_capture_completion(),
            Some(CaptureOutcome::Completed(completion)) if completion.token == 103
        ));
        engine.render_frames(0, |_| {});
        let CaptureOutcome::Completed(second) = controller
            .try_capture_completion()
            .expect("pending capture must flush after one later poll")
        else {
            panic!("second capture must complete");
        };
        assert_eq!(second.token, 104);
        assert_eq!(second.stereo.as_ptr(), second_allocation);
        assert_eq!(second.stereo.len(), 2);
    }

    #[test]
    fn resample_capture_lane_is_bounded_typed_and_independent_of_normal_saturation() {
        let (mut controller, ports) = audio_channels_with_capacities(1, 1, 1);
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let target = PadId::first();
        controller.stop_pad(target).unwrap();
        assert_eq!(
            controller.stop_pad(target),
            Err(ControlError::CommandQueueFull)
        );

        let buffer = resample_buffer(105, target, 100, 8);
        let allocation = buffer.stereo().as_ptr();
        controller.arm_capture(buffer).unwrap();
        for _ in 1..crate::command::CAPTURE_COMMAND_CAPACITY {
            controller.start_capture(105).unwrap();
        }
        let failure = controller.start_capture(105).unwrap_err();
        assert_eq!(failure.error(), crate::CaptureError::CommandFull);
        assert!(matches!(
            failure.into_command(),
            CaptureCommand::Start { token: 105 }
        ));

        engine.render_frames(0, |_| {});
        assert_eq!(
            controller
                .capture_status()
                .expect("arm is the sole capture poll")
                .state,
            CaptureState::Armed
        );
        engine.render_frames(0, |_| {});
        assert_eq!(
            controller
                .capture_status()
                .expect("start is the next capture poll")
                .state,
            CaptureState::Recording
        );
        for _ in 2..crate::command::CAPTURE_COMMAND_CAPACITY {
            engine.render_frames(0, |_| {});
        }
        controller.cancel_capture(105).unwrap();
        engine.render_frames(0, |_| {});
        let CaptureOutcome::Cancelled(returned) = controller
            .try_capture_completion()
            .expect("cancelled ownership must drain through the audio controller")
        else {
            panic!("cancel must return the original capture buffer");
        };
        assert_eq!(returned.stereo().as_ptr(), allocation);
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
    fn live_release_releases_a_loop_voice_without_leaving_it_stuck() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 128);

        controller.trigger_live(pad, 1.0).unwrap();
        engine.render_frames(65, |_| {});
        controller.release_live(pad).unwrap();
        engine.render_frames(65, |_| {});

        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .filter(|voice| voice.pad == pad)
                .all(|voice| voice.envelope.release_frame.is_some())
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
        assert_eq!(engine.pattern_action_reads, 64);
        assert_eq!(engine.pattern_mask_word_reads, 17);
        assert_eq!(engine.pattern_overflows(), 960);
    }

    #[test]
    fn boundary_intervals_share_one_actual_pattern_work_budget() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        controller
            .install_pattern(sparse_first_loop_snapshot(0))
            .unwrap();
        controller
            .install_pattern(sparse_first_loop_snapshot(1))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(0, |_| {});
        engine.pattern_player.pending_switch = Some(PendingPatternSwitch {
            slot: PatternSlotId::new(1).unwrap(),
            at_frame: 6,
        });

        engine.schedule_pattern_actions(12);

        let scheduled_slots = engine
            .pending
            .iter()
            .take(engine.pending_len)
            .flatten()
            .filter_map(|action| action.pattern_voice_id().map(|id| id.slot))
            .collect::<Vec<_>>();
        assert_eq!(scheduled_slots, vec![PatternSlotId::new(0).unwrap()]);
        assert_eq!(engine.pattern_action_reads, 1);
        assert_eq!(engine.pattern_mask_word_reads, 32);
        assert_eq!(engine.pattern_overflows(), 1);
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
        engine.render_frames(3, |_| {});
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
    fn immediate_switch_preserves_outgoing_one_shot_tail_but_cancels_its_future_actions() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let outgoing_pad = PadId::first();
        let incoming_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let one_shot = PadSettings::new(PlaybackMode::OneShot, 0.0, 0.0, 0.0, None).unwrap();
        for pad in [outgoing_pad, incoming_pad] {
            install_ready_sample(&mut controller, &mut engine, 100, pad, one_shot, 128);
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
            .select_pattern(PatternSlotId::new(1).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        engine.render_frames(0, |_| {});

        assert!(
            engine.voices.iter().flatten().any(|voice| {
                voice.pad == outgoing_pad && voice.envelope.release_frame.is_none()
            })
        );
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
            assert!(engine.voices_for_pad(preserved_pad) >= 2);
        }
    }

    #[test]
    fn boundary_switch_preserves_outgoing_one_shot_tail_but_cancels_its_future_actions() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let outgoing_pad = PadId::first();
        let incoming_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let one_shot = PadSettings::new(PlaybackMode::OneShot, 0.0, 0.0, 0.0, None).unwrap();
        for pad in [outgoing_pad, incoming_pad] {
            install_ready_sample(&mut controller, &mut engine, 100, pad, one_shot, 128);
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
        engine.render_frames(10, |_| {});

        assert!(
            engine.voices.iter().flatten().any(|voice| {
                voice.pad == outgoing_pad && voice.envelope.release_frame.is_none()
            })
        );
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
    fn slot_switch_clears_ghost_capture_only_when_the_switch_executes() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let first = pattern_snapshot_with_triggers(0, 100, &[]);
        let first_generation = first.generation();
        let second = pattern_snapshot_with_triggers(1, 100, &[]);
        let first_slot = PatternSlotId::new(0).unwrap();
        let second_slot = PatternSlotId::new(1).unwrap();
        controller.install_pattern(first).unwrap();
        controller.install_pattern(second).unwrap();
        controller
            .select_pattern(first_slot, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        controller
            .set_record_capture(Some((first_slot, first_generation)))
            .unwrap();
        engine.render_frames(1, |_| {});

        controller
            .select_pattern(second_slot, PatternSwitch::NextBoundary)
            .unwrap();
        engine.render_frames(0, |_| {});
        assert!(engine.pattern_player.record_capture.is_some());
        engine.render_frames(99, |_| {});
        assert!(engine.pattern_player.record_capture.is_none());

        controller
            .select_pattern(first_slot, PatternSwitch::Immediate)
            .unwrap();
        engine.render_frames(0, |_| {});
        assert!(engine.pattern_player.record_capture.is_none());

        controller
            .set_record_capture(Some((first_slot, first_generation)))
            .unwrap();
        engine.render_frames(0, |_| {});
        controller
            .select_pattern(second_slot, PatternSwitch::Immediate)
            .unwrap();
        engine.render_frames(0, |_| {});
        assert!(engine.pattern_player.record_capture.is_none());
    }

    #[test]
    fn capture_admission_rejects_stale_or_wrong_slot_but_accepts_exact_current_identity() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let first = pattern_snapshot_with_triggers(0, 100, &[]);
        let first_generation = first.generation();
        let second = pattern_snapshot_with_triggers(1, 100, &[]);
        let first_slot = PatternSlotId::new(0).unwrap();
        let second_slot = PatternSlotId::new(1).unwrap();
        controller.install_pattern(first).unwrap();
        controller.install_pattern(second).unwrap();
        controller
            .select_pattern(first_slot, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        controller
            .set_record_capture(Some((first_slot, first_generation)))
            .unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(
            engine
                .pattern_player
                .record_capture
                .map(|capture| (capture.slot, capture.generation)),
            Some((first_slot, first_generation))
        );

        controller
            .set_record_capture(Some((first_slot, first_generation + 1)))
            .unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(
            engine
                .pattern_player
                .record_capture
                .map(|capture| (capture.slot, capture.generation)),
            Some((first_slot, first_generation))
        );

        controller
            .select_pattern(second_slot, PatternSwitch::Immediate)
            .unwrap();
        controller
            .set_record_capture(Some((first_slot, first_generation)))
            .unwrap();
        engine.render_frames(0, |_| {});
        assert!(engine.pattern_player.record_capture.is_none());
        assert!(engine.invalid_commands() >= 2);

        controller.set_record_capture(None).unwrap();
        engine.render_frames(0, |_| {});
        assert!(engine.pattern_player.record_capture.is_none());
    }

    #[test]
    fn capture_admission_accepts_only_the_exact_pending_boundary_target() {
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
        let first = pattern_snapshot_with_triggers(0, 100, &[]);
        let second = pattern_snapshot_with_triggers(1, 100, &[]);
        let first_slot = PatternSlotId::new(0).unwrap();
        let second_slot = PatternSlotId::new(1).unwrap();
        let second_generation = second.generation();
        controller.install_pattern(first).unwrap();
        controller.install_pattern(second).unwrap();
        controller
            .select_pattern(first_slot, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(1, |_| {});

        controller
            .select_pattern(second_slot, PatternSwitch::NextBoundary)
            .unwrap();
        controller
            .set_record_capture(Some((second_slot, second_generation)))
            .unwrap();
        engine.render_frames(3, |_| {});
        assert_eq!(
            engine
                .pattern_player
                .record_capture
                .map(|capture| (capture.slot, capture.generation)),
            Some((second_slot, second_generation))
        );
        assert!(!controller.latest_telemetry().unwrap().pattern_recording);

        engine.render_frames(96, |_| {});
        let telemetry = controller.latest_telemetry().unwrap();
        assert_eq!(telemetry.pattern_slot, Some(second_slot));
        assert!(telemetry.pattern_recording);
        let id = controller.trigger_live_tracked(pad, 1.0).unwrap();
        engine.render_frames(65, |_| {});
        let mut acks = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut acks), 1);
        assert_eq!(acks[0].id, id);
        assert_eq!(acks[0].transport.map(|stamp| stamp.slot), Some(second_slot));

        let invalid = engine.invalid_commands();
        controller
            .set_record_capture(Some((first_slot, 0)))
            .unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(engine.invalid_commands(), invalid + 1);
        controller
            .select_pattern(first_slot, PatternSwitch::Immediate)
            .unwrap();
        engine.render_frames(0, |_| {});
        assert!(engine.pattern_player.record_capture.is_none());
    }

    #[test]
    fn superseding_a_pending_target_clears_its_capture_at_the_new_boundary() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let first = pattern_snapshot_with_triggers(0, 100, &[]);
        let second = pattern_snapshot_with_triggers(1, 100, &[]);
        let third = pattern_snapshot_with_triggers(2, 100, &[]);
        let first_slot = PatternSlotId::new(0).unwrap();
        let second_slot = PatternSlotId::new(1).unwrap();
        let third_slot = PatternSlotId::new(2).unwrap();
        let second_generation = second.generation();
        controller.install_pattern(first).unwrap();
        controller.install_pattern(second).unwrap();
        controller.install_pattern(third).unwrap();
        controller
            .select_pattern(first_slot, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(1, |_| {});
        controller
            .select_pattern(second_slot, PatternSwitch::NextBoundary)
            .unwrap();
        controller
            .set_record_capture(Some((second_slot, second_generation)))
            .unwrap();
        engine.render_frames(0, |_| {});
        assert!(engine.pattern_player.record_capture.is_some());

        controller
            .select_pattern(third_slot, PatternSwitch::NextBoundary)
            .unwrap();
        engine.render_frames(99, |_| {});
        assert_eq!(engine.pattern_player.selected_slot, Some(third_slot));
        assert!(engine.pattern_player.record_capture.is_none());
        let invalid = engine.invalid_commands();
        controller
            .set_record_capture(Some((second_slot, second_generation)))
            .unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(engine.invalid_commands(), invalid + 1);
    }

    #[test]
    fn tracked_live_ack_and_sound_share_the_first_frame_of_the_next_callback() {
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
        assert_eq!(onset, Some(observed_at));
        assert_eq!((acks[0].id, acks[0].frame), (id, observed_at));
    }

    #[test]
    fn live_ack_transport_uses_the_pattern_active_at_immediate_execution() {
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
        let mut callback_frame = 36;
        let mut onset = None;

        engine.render_frames(1, |frame| {
            if onset.is_none() && frame != [0.0, 0.0] {
                onset = Some(callback_frame);
            }
            callback_frame += 1;
        });

        let mut acks = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut acks), 1);
        assert_eq!((onset, acks[0].id, acks[0].frame), (Some(36), id, 36));
        assert_eq!(
            acks[0].transport,
            Some(TransportStamp {
                slot: slot_zero,
                generation: 0,
                origin: 0,
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
    fn live_frame_is_the_first_frame_of_a_short_callback() {
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

        let mut onset = None;
        let mut callback_frame = 0;
        engine.render_frames(32, |frame| {
            if onset.is_none() && frame != [0.0, 0.0] {
                onset = Some(callback_frame);
            }
            callback_frame += 1;
        });
        assert_eq!(engine.queued_commands(), 0);
        assert_eq!(engine.pending_actions(), 0);

        let mut acks = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut acks), 1);
        assert_eq!(onset, Some(0));
        assert_eq!((acks[0].id, acks[0].frame), (id, 0));
    }

    #[test]
    fn live_trigger_starts_on_the_first_frame_of_the_next_audio_callback() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        let pad = PadId::first();
        install_ready_sample(
            &mut controller,
            &mut engine,
            48_000,
            pad,
            PadSettings::default(),
            128,
        );
        controller.trigger_live(pad, 1.0).unwrap();
        let mut first_frame = [0.0, 0.0];

        engine.render_frames(1, |frame| first_frame = frame);

        assert_ne!(first_frame, [0.0, 0.0]);
        assert_eq!(engine.executed_triggers(), 1);
    }

    #[test]
    fn stop_pad_starts_release_on_the_next_callback_and_is_silent_within_sixty_four_frames() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        install_ready_sample(&mut controller, &mut engine, 48_000, pad, looping, 128);
        controller.trigger_live(pad, 1.0).unwrap();
        let mut before_stop = [0.0, 0.0];
        engine.render_frames(32, |frame| before_stop = frame);
        controller.stop_pad(pad).unwrap();
        let mut first_release_frame = [0.0, 0.0];

        engine.render_frames(1, |frame| first_release_frame = frame);

        assert!(first_release_frame[0].abs() < before_stop[0].abs());
        let mut final_release_frame = [1.0, 1.0];
        engine.render_frames(63, |frame| final_release_frame = frame);
        assert_eq!(final_release_frame, [0.0, 0.0]);
        assert_eq!(engine.active_voices(), 0);
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
        assert_eq!(onset, Some(observed_at));
        assert_eq!(acks[0].frame, observed_at);
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);
    }

    #[test]
    fn command_between_short_callbacks_starts_on_the_next_callback_first_frame() {
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
        engine.render_frames(64, |frame| assert_eq!(frame, [0.0, 0.0]));
        let id = controller.trigger_live_tracked(pad, 1.0).unwrap();
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
        engine.render_frames(1, |_| {});
        let mut trigger_ack = [crate::LiveAck::EMPTY; 1];
        assert_eq!(controller.drain_live_acks(&mut trigger_ack), 1);
        let observed_at = engine.rendered_frame();
        let release_id = controller.release_live_tracked(pad).unwrap();

        engine.render_frames(1, |_| {});

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
        assert_eq!(release_ack[0].frame, observed_at);
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
        assert_eq!(engine.last_triggered_frame, Some(0));
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
    fn pre_fence_select_is_reused_by_post_fence_play() {
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

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(
            engine.pattern_player.selected_slot,
            Some(PatternSlotId::new(0).unwrap())
        );
        assert!(engine.pattern_player.playing);
    }

    #[test]
    fn pending_boundary_selection_is_promoted_when_post_fence_play_starts_new_transport() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let slot_zero = PatternSlotId::new(0).unwrap();
        let slot_one = PatternSlotId::new(1).unwrap();
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[]))
            .unwrap();
        controller
            .install_pattern(pattern_snapshot_with_triggers(1, 100, &[]))
            .unwrap();
        controller
            .select_pattern(slot_zero, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(5, |_| {});
        controller
            .select_pattern(slot_one, PatternSwitch::NextBoundary)
            .unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(
            engine
                .pattern_player
                .pending_switch
                .map(|pending| pending.slot),
            Some(slot_one)
        );

        controller.stop_all().unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(0, |_| {});

        assert_eq!(engine.pattern_player.selected_slot, Some(slot_one));
        assert!(engine.pattern_player.pending_switch.is_none());
        assert_eq!(engine.pattern_player.origin, 5);
        assert!(engine.pattern_player.playing);
    }

    #[test]
    fn stop_all_interleaving_preserves_only_post_fence_runtime_and_all_configuration() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let outgoing_pad = PadId::first();
        let incoming_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        for pad in [outgoing_pad, incoming_pad] {
            install_ready_sample(&mut controller, &mut engine, 100, pad, looping, 128);
        }
        let slot_zero = PatternSlotId::new(0).unwrap();
        let slot_one = PatternSlotId::new(1).unwrap();
        let first = pattern_snapshot_with_triggers(0, 100, &[(0, outgoing_pad)]);
        let second = pattern_snapshot_with_triggers(1, 100, &[(0, incoming_pad)]);
        let second_generation = second.generation();
        controller.install_pattern(first).unwrap();
        controller.install_pattern(second).unwrap();
        controller
            .select_pattern(slot_zero, PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        controller.set_record_capture(Some((slot_zero, 0))).unwrap();
        engine.render_frames(1, |_| {});
        controller.trigger(outgoing_pad, 100, 1.0).unwrap();

        engine.render_frames_with_after_initial_fence_poll_hook(
            1,
            || {
                controller.stop_all().unwrap();
                controller.play_pattern().unwrap();
                controller
                    .select_pattern(slot_one, PatternSwitch::Immediate)
                    .unwrap();
                controller
                    .set_record_capture(Some((slot_one, second_generation)))
                    .unwrap();
            },
            |_| {},
        );

        assert!(engine.pattern_player.playing);
        assert_eq!(engine.pattern_player.selected_slot, Some(slot_one));
        assert_eq!(
            engine
                .pattern_player
                .record_capture
                .map(|capture| (capture.slot, capture.generation)),
            Some((slot_one, second_generation))
        );
        assert!(
            engine.voices.iter().flatten().any(|voice| {
                voice.pad == outgoing_pad && voice.envelope.release_frame.is_some()
            })
        );
        assert!(
            engine.voices.iter().flatten().any(|voice| {
                voice.pad == incoming_pad && voice.envelope.release_frame.is_none()
            })
        );
        assert!(
            !engine
                .pending
                .iter()
                .take(engine.pending_len)
                .flatten()
                .any(|action| matches!(
                    action,
                    ScheduledAction::Trigger {
                        pad,
                        source: ActionSource::Command,
                        ..
                    } if *pad == outgoing_pad
                ))
        );
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
                PadMixSettings::default(),
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
        assert_eq!(engine.last_triggered_frame, Some(513));
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
                PadMixSettings::default(),
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
        assert_eq!(engine.last_triggered_frame, Some(1));
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);
    }

    #[test]
    fn live_input_bypasses_a_blocked_timed_command_without_overtaking_setup() {
        let (mut controller, mut engine) = harness();
        let first = PadId::first();
        let second = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        controller
            .install(
                first,
                constant_sample(1_024, 0.25),
                PadSettings::default(),
                PadMixSettings::default(),
            )
            .unwrap();
        engine.render_frames(64, |_| {});
        for _ in 0..NON_LIVE_PENDING_COUNT {
            controller.trigger(first, 10_000, 1.0).unwrap();
        }
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pending_actions(), NON_LIVE_PENDING_COUNT);

        controller.trigger(first, 20_000, 1.0).unwrap();
        controller
            .install(
                second,
                constant_sample(1_024, 0.5),
                PadSettings::default(),
                PadMixSettings::default(),
            )
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
        assert_eq!(engine.last_triggered_frame, Some(64));
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
            .install(
                pad,
                constant_sample(1_024, 0.25),
                PadSettings::default(),
                PadMixSettings::default(),
            )
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
            .install(
                pad,
                constant_sample(1_024, 0.25),
                PadSettings::default(),
                PadMixSettings::default(),
            )
            .unwrap();
        engine.render_frames(1, |_| {});
        for _ in 0..MAX_COMMANDS_PER_RENDER {
            controller.trigger(pad, 10_000, 1.0).unwrap();
        }
        controller.trigger_live(pad, 1.0).unwrap();

        engine.render_frames(65, |_| {});

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
            .install(
                pad,
                constant_sample(1_024, 0.25),
                settings,
                PadMixSettings::default(),
            )
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
        assert_eq!(engine.last_triggered_frame, Some(129));
        assert_eq!(engine.pending_actions(), 0);
        assert_eq!(engine.queued_commands(), 0);
    }

    #[test]
    fn post_fence_actions_observed_after_the_initial_poll_survive_stop_all() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(
                pad,
                constant_sample(1_024, 0.25),
                settings,
                PadMixSettings::default(),
            )
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
                .install_recovery(
                    pad,
                    constant_sample(8, 0.25),
                    PadSettings::default(),
                    PadMixSettings::default(),
                )
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
            .install(
                PadId::first(),
                sample,
                PadSettings::default(),
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                sample,
                PadSettings::default(),
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                constant_sample(8, 0.5),
                settings,
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                constant_sample(256, 1.0),
                settings,
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                constant_sample(256, 1.0),
                settings,
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                constant_sample(2, 0.5),
                settings,
                PadMixSettings::default(),
            )
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
        controller
            .install(PadId::first(), ramp, settings, PadMixSettings::default())
            .unwrap();
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
        controller
            .install(PadId::first(), ramp, settings, PadMixSettings::default())
            .unwrap();
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
            .install(
                PadId::first(),
                constant_sample(8, 1.0),
                settings,
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                constant_sample(256, 1.0),
                settings,
                PadMixSettings::default(),
            )
            .unwrap();
        controller
            .install(
                second,
                constant_sample(256, 0.5),
                settings,
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                constant_sample(256, 1.0),
                settings,
                PadMixSettings::default(),
            )
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
                .install(
                    pad,
                    constant_sample(8, 0.25),
                    settings,
                    PadMixSettings::default(),
                )
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
                PadMixSettings::default(),
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
                PadMixSettings::default(),
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
                .install(
                    PadId::first(),
                    constant_sample(64, 0.1),
                    settings,
                    PadMixSettings::default(),
                )
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
                PadMixSettings::default(),
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
            .install(
                PadId::first(),
                constant_sample(256, 1.0),
                settings,
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                constant_sample(8, 1.0),
                settings,
                PadMixSettings::default(),
            )
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
                PadMixSettings::default(),
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
            .install(
                pad,
                constant_sample(4_000, 1.0),
                PadSettings::default(),
                PadMixSettings::default(),
            )
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
            .install(PadId::first(), sample, settings, PadMixSettings::default())
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
            .install(
                PadId::first(),
                constant_sample(8, 0.25),
                settings,
                PadMixSettings::default(),
            )
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
            .install(
                PadId::first(),
                constant_sample(8, f32::MAX),
                settings,
                PadMixSettings::default(),
            )
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
                .install(
                    PadId::first(),
                    sample,
                    PadSettings::default(),
                    PadMixSettings::default(),
                )
                .unwrap();
            engine.render_stereo(&mut []);
        }
        controller
            .install(
                PadId::first(),
                constant_sample(8, 0.75),
                PadSettings::default(),
                PadMixSettings::default(),
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

    #[test]
    fn remove_sample_preserves_one_shot_tail_and_silences_new_triggers() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        let sample = constant_sample(128, 0.5);
        let weak = Arc::downgrade(&sample);
        controller
            .install(
                pad,
                Arc::clone(&sample),
                PadSettings::default(),
                PadMixSettings::default(),
            )
            .unwrap();
        drop(sample);
        controller.trigger(pad, 0, 1.0).unwrap();
        engine.render_frames(1, |_| {});

        controller.remove_sample(pad).unwrap();
        engine.render_frames(0, |_| {});
        assert!(engine.pad_binding(pad).slot.is_none());
        let voice = engine
            .voices
            .iter()
            .flatten()
            .find(|voice| voice.pad == pad)
            .unwrap();
        assert_eq!(voice.mode, PlaybackMode::OneShot);
        assert_eq!(voice.envelope.release_frame, None);

        controller
            .trigger(pad, engine.rendered_frame(), 1.0)
            .unwrap();
        engine.render_frames(1, |_| {});
        assert_eq!(engine.voices_for_pad(pad), 1);
        assert!(weak.upgrade().is_some());
        engine.render_frames(128, |_| {});
        assert!(weak.upgrade().is_some());
        assert_eq!(controller.reclaim_retired(), 1);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn remove_sample_releases_only_sustained_voices_for_the_removed_pad() {
        for mode in [PlaybackMode::Gate, PlaybackMode::Loop] {
            let (mut controller, mut engine) = harness();
            let removed = PadId::first();
            let preserved = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
            let settings = PadSettings::new(mode, 0.0, 0.0, 0.0, None).unwrap();
            for pad in [removed, preserved] {
                controller
                    .install(
                        pad,
                        constant_sample(256, 0.5),
                        settings,
                        PadMixSettings::default(),
                    )
                    .unwrap();
                controller.trigger(pad, 0, 1.0).unwrap();
            }
            engine.render_frames(1, |_| {});

            controller.remove_sample(removed).unwrap();
            engine.render_frames(0, |_| {});

            assert!(
                engine.voices.iter().flatten().any(|voice| {
                    voice.pad == removed && voice.envelope.release_frame.is_some()
                })
            );
            assert!(
                engine.voices.iter().flatten().any(|voice| {
                    voice.pad == preserved && voice.envelope.release_frame.is_none()
                })
            );
        }
    }

    #[test]
    fn remove_sample_waits_at_command_head_for_immediate_retirement_capacity() {
        let (mut controller, ports) = audio_channels_with_capacities(8, 1, 1);
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        let pad = PadId::first();
        controller
            .install(
                pad,
                constant_sample(8, 0.5),
                PadSettings::default(),
                PadMixSettings::default(),
            )
            .unwrap();
        engine.render_frames(0, |_| {});
        engine
            .ports
            .retirements
            .push(CriticalEvent::RetiredSample {
                slot: SampleSlot::new(SAMPLE_SLOT_COUNT - 1).unwrap(),
                buffer: constant_sample(1, 0.0),
            })
            .unwrap();

        controller.remove_sample(pad).unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(engine.queued_commands(), 1);
        assert!(engine.pad_binding(pad).slot.is_some());

        assert_eq!(controller.reclaim_retired(), 1);
        engine.render_frames(0, |_| {});
        assert_eq!(engine.queued_commands(), 0);
        assert!(engine.pad_binding(pad).slot.is_none());
        assert_eq!(controller.reclaim_retired(), 1);
    }

    #[test]
    fn remove_sample_of_absent_pad_is_deterministic_with_full_retirement_queue() {
        let (mut controller, ports) = audio_channels_with_capacities(8, 1, 1);
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        engine
            .ports
            .retirements
            .push(CriticalEvent::RetiredSample {
                slot: SampleSlot::new(SAMPLE_SLOT_COUNT - 1).unwrap(),
                buffer: constant_sample(1, 0.0),
            })
            .unwrap();
        let invalid_before = engine.invalid_commands();

        controller.remove_sample(PadId::first()).unwrap();
        engine.render_frames(0, |_| {});

        assert_eq!(engine.queued_commands(), 0);
        assert_eq!(engine.invalid_commands(), invalid_before);
        assert_eq!(controller.reclaim_retired(), 1);
    }

    #[test]
    fn default_mixer_is_bit_exact_with_the_pre_fx_dry_fixture() {
        let (mut controller, mut engine) = harness();
        let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(
                PadId::first(),
                constant_sample(16, 1.0),
                settings,
                PadMixSettings::default(),
            )
            .unwrap();
        controller.trigger(PadId::first(), 0, 1.0).unwrap();

        let mut frames = Vec::new();
        engine.render_frames(4, |frame| frames.push(frame));
        assert_eq!(
            frames,
            vec![
                [f32::from_bits(0x3cb1_1b16); 2],
                [f32::from_bits(0x3d2d_5ba0); 2],
                [f32::from_bits(0x3d7e_a5e7); 2],
                [f32::from_bits(0x3da6_5196); 2],
            ]
        );
    }

    #[test]
    fn playing_loop_ramps_current_gain_pan_and_mute_without_stopping() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(
                pad,
                constant_sample(256, 1.0),
                looping,
                PadMixSettings::default(),
            )
            .unwrap();
        controller.trigger(pad, 0, 1.0).unwrap();
        engine.render_frames(32, |_| {});

        controller
            .update_pad(
                pad,
                PadSettings::new(PlaybackMode::Loop, -6.0, -1.0, 0.0, None).unwrap(),
            )
            .unwrap();
        let mut routed = [0.0; 2];
        engine.render_frames(64, |frame| routed = frame);
        assert!((routed[0] - 0.333_860_58).abs() < 1.0e-7);
        assert_eq!(routed[1], 0.0);

        controller
            .update_pad_mix(pad, PadMixSettings::new(true, 0.0, 0.0).unwrap())
            .unwrap();
        let mut muted = [1.0; 2];
        engine.render_frames(64, |frame| muted = frame);
        assert_eq!(muted, [0.0; 2]);
        assert_eq!(engine.voices_for_pad(pad), 1);
    }

    #[test]
    fn two_pads_feed_distinct_amounts_into_the_same_delay_bus() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(1_000, ports).unwrap();
        let first = PadId::first();
        let second = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let one_shot = PadSettings::new(PlaybackMode::OneShot, 0.0, -1.0, 0.0, None).unwrap();
        controller
            .install(
                first,
                constant_sample_at(1_000, 1, 1.0),
                one_shot,
                PadMixSettings::new(false, 0.25, 0.0).unwrap(),
            )
            .unwrap();
        controller
            .install(
                second,
                constant_sample_at(1_000, 1, 1.0),
                one_shot,
                PadMixSettings::new(false, 0.75, 0.0).unwrap(),
            )
            .unwrap();
        controller
            .update_master_mix(
                MasterMixSettings::new(
                    0.0,
                    DelaySettings::new(true, 10, 0.0, 0.0).unwrap(),
                    ReverbSettings::default(),
                )
                .unwrap(),
            )
            .unwrap();
        engine.render_frames(128, |_| {});
        controller.trigger(first, 128, 1.0).unwrap();
        controller.trigger(second, 129, 1.0).unwrap();

        let mut frames = Vec::new();
        engine.render_frames(12, |frame| frames.push(frame));
        assert!(
            (frames[10][0] - 0.007_751_938).abs() < 1.0e-7,
            "unexpected first delay tap: {:?}",
            frames[10]
        );
        assert!(
            (frames[11][0] - 0.022_900_764).abs() < 1.0e-7,
            "unexpected second delay tap: {:?}",
            frames[11]
        );
        assert_eq!(frames[10][1], 0.0);
        assert_eq!(frames[11][1], 0.0);
    }

    #[test]
    fn master_level_is_applied_before_the_existing_limiter() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(1_000, ports).unwrap();
        let pad = PadId::first();
        controller
            .install(
                pad,
                constant_sample_at(1_000, 1, 1.0),
                PadSettings::new(PlaybackMode::OneShot, 0.0, -1.0, 0.0, None).unwrap(),
                PadMixSettings::default(),
            )
            .unwrap();
        controller
            .update_master_mix(
                MasterMixSettings::new(
                    -6.020_600_3,
                    DelaySettings::default(),
                    ReverbSettings::default(),
                )
                .unwrap(),
            )
            .unwrap();
        engine.render_frames(64, |_| {});
        controller.trigger(pad, 64, 1.0).unwrap();

        let mut rendered = [0.0; 2];
        engine.render_frames(1, |frame| rendered = frame);
        assert!((rendered[0] - 0.015_384_615).abs() < 1.0e-7);
        assert_eq!(rendered[1], 0.0);
    }

    #[test]
    fn active_same_pad_polyphony_advances_one_shared_route_ramp_per_frame() {
        let (mut doubled_controller, mut doubled_engine) = harness();
        let (mut reference_controller, mut reference_engine) = harness();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, -1.0, 0.0, None).unwrap();
        doubled_controller
            .install(
                pad,
                constant_sample(256, 0.5),
                looping,
                PadMixSettings::default(),
            )
            .unwrap();
        reference_controller
            .install(
                pad,
                constant_sample(256, 1.0),
                looping,
                PadMixSettings::default(),
            )
            .unwrap();
        doubled_controller.trigger(pad, 0, 1.0).unwrap();
        doubled_controller.trigger(pad, 0, 1.0).unwrap();
        reference_controller.trigger(pad, 0, 1.0).unwrap();
        let mut doubled_before = [0.0; 2];
        let mut reference_before = [0.0; 2];
        doubled_engine.render_frames(1, |frame| doubled_before = frame);
        reference_engine.render_frames(1, |frame| reference_before = frame);
        assert_eq!(doubled_before, reference_before);
        assert_eq!(doubled_engine.voices_for_pad(pad), 2);

        let quiet = PadSettings::new(PlaybackMode::Loop, -60.0, -1.0, 0.0, None).unwrap();
        doubled_controller.update_pad(pad, quiet).unwrap();
        reference_controller.update_pad(pad, quiet).unwrap();

        let mut doubled = Vec::new();
        let mut reference = Vec::new();
        doubled_engine.render_frames(64, |frame| doubled.push(frame));
        reference_engine.render_frames(64, |frame| reference.push(frame));
        for frame in [0, 15, 31, 62, 63] {
            assert_eq!(
                doubled[frame], reference[frame],
                "two active voices advanced the route more than once at ramp frame {frame}"
            );
        }
        assert_eq!(doubled_engine.voices_for_pad(pad), 2);
    }

    #[test]
    fn live_same_pad_and_pattern_cross_pad_triggers_use_trigger_time_choke_membership() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(100, ports).unwrap();
        let first = PadId::first();
        let second = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
        let group = Some(ChokeGroup::new(1).unwrap());
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, group).unwrap();
        for pad in [first, second] {
            controller
                .install(
                    pad,
                    constant_sample_at(100, 256, 0.5),
                    looping,
                    PadMixSettings::default(),
                )
                .unwrap();
        }

        controller.trigger_live(first, 1.0).unwrap();
        controller.trigger_live(first, 1.0).unwrap();
        engine.render_frames(129, |_| {});
        assert_eq!(engine.voices_for_pad(first), 1);

        controller
            .update_pad(
                first,
                PadSettings::new(
                    PlaybackMode::Loop,
                    0.0,
                    0.0,
                    0.0,
                    Some(ChokeGroup::new(2).unwrap()),
                )
                .unwrap(),
            )
            .unwrap();
        controller
            .install_pattern(pattern_snapshot_with_triggers(0, 100, &[(0, second)]))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();
        engine.render_frames(1, |_| {});
        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .any(|voice| voice.pad == first && voice.envelope.release_frame.is_some())
        );
        assert_eq!(engine.voices_for_pad(second), 1);
    }

    #[test]
    fn mute_advances_loop_silently_and_stop_all_leaves_delay_tail_running() {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(1_000, ports).unwrap();
        let pad = PadId::first();
        let looping = PadSettings::new(PlaybackMode::Loop, 0.0, -1.0, 0.0, None).unwrap();
        controller
            .install(
                pad,
                constant_sample_at(1_000, 257, 0.5),
                looping,
                PadMixSettings::new(false, 1.0, 0.0).unwrap(),
            )
            .unwrap();
        controller.trigger(pad, 0, 1.0).unwrap();
        engine.render_frames(96, |_| {});

        controller
            .update_pad_mix(pad, PadMixSettings::new(true, 1.0, 0.0).unwrap())
            .unwrap();
        engine.render_frames(64, |_| {});
        let position = engine
            .voices
            .iter()
            .flatten()
            .find(|voice| voice.pad == pad)
            .unwrap()
            .position;
        let mut muted = Vec::new();
        engine.render_frames(16, |frame| muted.push(frame));
        assert!(muted.iter().all(|frame| *frame == [0.0; 2]));
        assert!(
            engine
                .voices
                .iter()
                .flatten()
                .find(|voice| voice.pad == pad)
                .unwrap()
                .position
                > position
        );

        controller
            .update_pad_mix(pad, PadMixSettings::new(false, 1.0, 0.0).unwrap())
            .unwrap();
        controller
            .update_master_mix(
                MasterMixSettings::new(
                    0.0,
                    DelaySettings::new(true, 10, 0.5, 0.0).unwrap(),
                    ReverbSettings::default(),
                )
                .unwrap(),
            )
            .unwrap();
        engine.render_frames(64, |_| {});
        controller.stop_all().unwrap();
        engine.render_frames(65, |_| {});
        assert_eq!(engine.voices_for_pad(pad), 0);
        let mut tail = Vec::new();
        engine.render_frames(20, |frame| tail.push(frame));
        assert!(tail.iter().any(|frame| frame[0] != 0.0 || frame[1] != 0.0));
    }

    #[test]
    fn engine_rejects_malformed_pad_mix_without_changing_the_binding() {
        let (mut controller, mut engine) = harness();
        let pad = PadId::first();
        controller
            .install(
                pad,
                constant_sample(16, 0.5),
                PadSettings::default(),
                PadMixSettings::default(),
            )
            .unwrap();
        engine.render_frames(0, |_| {});
        let invalid_before = engine.invalid_commands();
        engine.execute_immediate(AudioCommand::UpdatePadMix {
            pad,
            settings: PadMixSettings {
                muted: false,
                delay_send: f32::NAN,
                reverb_send: 0.0,
            },
        });
        assert_eq!(engine.pad_binding(pad).mix, PadMixSettings::default());
        assert_eq!(engine.invalid_commands(), invalid_before + 1);
    }

    #[test]
    fn full_queue_rejects_pad_mix_update_without_changing_the_binding() {
        let (mut controller, ports) = audio_channels_with_capacities(1, 256, 8);
        let mut engine = AudioEngine::new(48_000, ports).unwrap();
        let pad = PadId::first();
        controller
            .install(
                pad,
                constant_sample(16, 0.5),
                PadSettings::default(),
                PadMixSettings::default(),
            )
            .unwrap();
        engine.render_frames(0, |_| {});

        controller.stop_pad(pad).unwrap();
        assert_eq!(
            controller.update_pad_mix(pad, PadMixSettings::new(true, 0.5, 0.5).unwrap()),
            Err(crate::ControlError::CommandQueueFull)
        );
        engine.render_frames(0, |_| {});
        assert_eq!(engine.pad_binding(pad).mix, PadMixSettings::default());
    }
}
