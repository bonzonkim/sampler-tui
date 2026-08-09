//! Pattern event model and scheduling.

use serde::{Deserialize, Serialize};

use crate::{Frame, ModelError, PadId, PatternCompileError, PatternEditError, Transport};

pub const PATTERN_SLOT_COUNT: usize = 16;
pub const MAX_PATTERN_EVENTS: usize = 1_024;
pub const MAX_PATTERN_ACTIONS: usize = 2_048;
pub const FIRST_LOOP_VALID_MASK_WORDS: usize = MAX_PATTERN_ACTIONS / u64::BITS as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PatternSlotId(u8);

impl PatternSlotId {
    pub fn new(value: u8) -> Result<Self, PatternEditError> {
        (usize::from(value) < PATTERN_SLOT_COUNT)
            .then_some(Self(value))
            .ok_or(PatternEditError::InvalidSlot)
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PatternEvent {
    pub id: EventId,
    pub pad: PadId,
    pub frame: Frame,
    pub velocity: f32,
    pub duration: Option<Frame>,
    pub original_offset: Option<i64>,
}

impl PatternEvent {
    pub fn new(
        id: EventId,
        pad: PadId,
        frame: Frame,
        velocity: f32,
        duration: Option<Frame>,
    ) -> Result<Self, ModelError> {
        if id.0 == 0
            || !velocity.is_finite()
            || !(0.0..=1.0).contains(&velocity)
            || duration == Some(0)
        {
            return Err(ModelError::InvalidEvent);
        }
        Ok(Self {
            id,
            pad,
            frame,
            velocity,
            duration,
            original_offset: None,
        })
    }

    pub fn quantized(mut self, transport: &Transport, strength: f32) -> Self {
        if self.frame >= transport.loop_frames() {
            return self;
        }
        let strength = if strength.is_finite() {
            strength.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let target = (0..=transport.step_count())
            .map(|step| transport.step_frame(step))
            .min_by_key(|frame| frame.abs_diff(self.frame))
            .unwrap_or(0);
        let delta = i128::from(target) - i128::from(self.frame);
        let shifted = i128::from(self.frame) + (delta as f64 * f64::from(strength)).round() as i128;
        if self.original_offset.is_none() {
            self.original_offset = i64::try_from(self.frame)
                .ok()
                .zip(i64::try_from(target).ok())
                .and_then(|(frame, target)| frame.checked_sub(target));
        }
        if let Ok(shifted) = Frame::try_from(shifted.max(0)) {
            self.frame = shifted % transport.loop_frames();
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScheduledEvent {
    pub event_id: EventId,
    pub pad: PadId,
    pub at: Frame,
    pub velocity: f32,
    pub duration: Option<Frame>,
}

impl ScheduledEvent {
    pub const EMPTY: Self = Self {
        event_id: EventId(0),
        pad: PadId::first(),
        at: 0,
        velocity: 0.0,
        duration: None,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleResult {
    pub written: usize,
    pub dropped: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    length_frames: Frame,
    events: Vec<PatternEvent>,
}

impl Pattern {
    pub fn new(length_frames: Frame) -> Self {
        assert!(length_frames > 0, "pattern length must be non-zero");
        Self {
            length_frames,
            events: Vec::new(),
        }
    }

    pub fn insert(&mut self, event: PatternEvent) -> Result<(), ModelError> {
        if event.frame >= self.length_frames {
            return Err(ModelError::InvalidEvent);
        }
        if self.events.iter().any(|existing| existing.id == event.id) {
            return Err(ModelError::DuplicateEvent);
        }
        let index = self
            .events
            .binary_search_by_key(&(event.frame, event.id), |existing| {
                (existing.frame, existing.id)
            })
            .unwrap_or_else(|index| index);
        self.events.insert(index, event);
        Ok(())
    }

    pub fn remove(&mut self, id: EventId) -> Option<PatternEvent> {
        let index = self.events.iter().position(|event| event.id == id)?;
        Some(self.events.remove(index))
    }

    pub fn schedule_range(
        &self,
        start: Frame,
        end: Frame,
        output: &mut [ScheduledEvent],
    ) -> ScheduleResult {
        if start >= end {
            return ScheduleResult {
                written: 0,
                dropped: 0,
            };
        }
        let first = self.events.partition_point(|event| event.frame < start);
        let last = self.events.partition_point(|event| event.frame < end);
        let matching = &self.events[first..last];
        let written = matching.len().min(output.len());
        for (slot, event) in output.iter_mut().zip(matching.iter()).take(written) {
            *slot = ScheduledEvent {
                event_id: event.id,
                pad: event.pad,
                at: event.frame,
                velocity: event.velocity,
                duration: event.duration,
            };
        }
        ScheduleResult {
            written,
            dropped: matching.len() - written,
        }
    }

    pub fn length_frames(&self) -> Frame {
        self.length_frames
    }

    pub fn events(&self) -> &[PatternEvent] {
        &self.events
    }
}

#[derive(Debug, Clone)]
struct PatternCheckpoint {
    transport: Transport,
    events: Pattern,
    raw_frames: Vec<(EventId, Frame)>,
    quantize_strength: f32,
    next_event_id: u64,
}

#[derive(Debug)]
pub struct EditablePattern {
    slot: PatternSlotId,
    name: String,
    transport: Transport,
    events: Pattern,
    raw_frames: Vec<(EventId, Frame)>,
    quantize_strength: f32,
    next_event_id: u64,
    generation: u64,
    checkpoint: Option<Box<PatternCheckpoint>>,
}

impl EditablePattern {
    pub fn new(
        slot: PatternSlotId,
        name: impl Into<String>,
        transport: Transport,
    ) -> Result<Self, PatternEditError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(PatternEditError::InvalidName);
        }
        Ok(Self {
            slot,
            name,
            transport,
            events: Pattern::new(transport.loop_frames()),
            raw_frames: Vec::new(),
            quantize_strength: 0.0,
            next_event_id: 1,
            generation: 0,
            checkpoint: None,
        })
    }

    pub fn from_persisted(
        slot: PatternSlotId,
        name: impl Into<String>,
        transport: Transport,
        events: Vec<(PatternEvent, Frame)>,
        quantize_strength: f32,
    ) -> Result<Self, PatternEditError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(PatternEditError::InvalidName);
        }
        if !quantize_strength.is_finite() || !(0.0..=1.0).contains(&quantize_strength) {
            return Err(PatternEditError::InvalidQuantizeStrength);
        }
        if events.len() > MAX_PATTERN_EVENTS {
            return Err(PatternEditError::Full);
        }

        let next_event_id = events
            .iter()
            .map(|(event, _)| event.id.0)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(PatternEditError::ArithmeticOverflow)?;
        let mut restored_events = Pattern::new(transport.loop_frames());
        let mut raw_frames = Vec::with_capacity(events.len());
        for (event, raw_frame) in events {
            validate_editable_event(&event, transport.loop_frames())?;
            if raw_frame >= transport.loop_frames() {
                return Err(PatternEditError::Model(ModelError::InvalidEvent));
            }
            let id = event.id;
            let restored = quantize_event(event, raw_frame, transport, quantize_strength)?;
            if restored != event {
                return Err(PatternEditError::Model(ModelError::InvalidEvent));
            }
            restored_events.insert(restored)?;
            raw_frames.push((id, raw_frame));
        }

        Ok(Self {
            slot,
            name,
            transport,
            events: restored_events,
            raw_frames,
            quantize_strength,
            next_event_id,
            generation: 0,
            checkpoint: None,
        })
    }

    pub fn slot(&self) -> PatternSlotId {
        self.slot
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn transport(&self) -> Transport {
        self.transport
    }

    pub fn events(&self) -> &[PatternEvent] {
        self.events.events()
    }

    pub fn event(&self, id: EventId) -> Option<&PatternEvent> {
        self.events.events().iter().find(|event| event.id == id)
    }

    pub fn quantize_strength(&self) -> f32 {
        self.quantize_strength
    }

    pub fn next_event_id(&self) -> EventId {
        EventId(self.next_event_id)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn persisted_events(&self) -> Result<Vec<(PatternEvent, Frame)>, PatternEditError> {
        self.events
            .events()
            .iter()
            .map(|event| {
                let raw_frame = self
                    .raw_frames
                    .iter()
                    .find_map(|(id, raw_frame)| (*id == event.id).then_some(*raw_frame))
                    .ok_or(PatternEditError::MissingRawFrame)?;
                Ok((*event, raw_frame))
            })
            .collect()
    }

    pub fn insert(&mut self, event: PatternEvent) -> Result<(), PatternEditError> {
        if self.events.events().len() >= MAX_PATTERN_EVENTS {
            return Err(PatternEditError::Full);
        }
        validate_editable_event(&event, self.transport.loop_frames())?;
        let next_event_id = event
            .id
            .0
            .checked_add(1)
            .ok_or(PatternEditError::ArithmeticOverflow)?;
        let generation = self.next_generation()?;
        let raw_frame = event.frame;
        let event = quantize_event(event, raw_frame, self.transport, self.quantize_strength)?;
        let mut events = self.events.clone();
        events.insert(event)?;
        let mut raw_frames = self.raw_frames.clone();
        raw_frames.push((event.id, raw_frame));
        self.events = events;
        self.raw_frames = raw_frames;
        self.next_event_id = self.next_event_id.max(next_event_id);
        self.generation = generation;
        Ok(())
    }

    pub fn insert_new(
        &mut self,
        pad: PadId,
        frame: Frame,
        velocity: f32,
        duration: Option<Frame>,
    ) -> Result<EventId, PatternEditError> {
        self.next_event_id
            .checked_add(1)
            .ok_or(PatternEditError::ArithmeticOverflow)?;
        let id = EventId(self.next_event_id);
        let event = PatternEvent::new(id, pad, frame, velocity, duration)?;
        self.insert(event)?;
        Ok(id)
    }

    pub fn remove(&mut self, id: EventId) -> Result<PatternEvent, PatternEditError> {
        let event = *self.event(id).ok_or(PatternEditError::EventNotFound(id))?;
        let raw_index = self
            .raw_frames
            .iter()
            .position(|(event_id, _)| *event_id == id)
            .ok_or(PatternEditError::MissingRawFrame)?;
        let generation = self.next_generation()?;
        let mut events = self.events.clone();
        let removed = events
            .remove(id)
            .ok_or(PatternEditError::EventNotFound(id))?;
        let mut raw_frames = self.raw_frames.clone();
        raw_frames.remove(raw_index);
        self.events = events;
        self.raw_frames = raw_frames;
        self.generation = generation;
        debug_assert_eq!(removed, event);
        Ok(removed)
    }

    pub fn set_velocity(&mut self, id: EventId, velocity: f32) -> Result<(), PatternEditError> {
        if !velocity.is_finite() || !(0.0..=1.0).contains(&velocity) {
            return Err(PatternEditError::InvalidVelocity);
        }
        let index = self
            .events
            .events
            .iter()
            .position(|event| event.id == id)
            .ok_or(PatternEditError::EventNotFound(id))?;
        let generation = self.next_generation()?;
        let mut events = self.events.clone();
        events.events[index].velocity = velocity;
        self.events = events;
        self.generation = generation;
        Ok(())
    }

    /// Changes an event's held duration without reconstructing its position from a quantized
    /// frame. This preserves the raw-frame ledger used by reversible quantization.
    pub fn set_duration(
        &mut self,
        id: EventId,
        duration: Option<Frame>,
    ) -> Result<(), PatternEditError> {
        if duration == Some(0)
            || duration.is_some_and(|duration| duration > self.transport.loop_frames())
        {
            return Err(PatternEditError::Model(ModelError::InvalidEvent));
        }
        let index = self
            .events
            .events
            .iter()
            .position(|event| event.id == id)
            .ok_or(PatternEditError::EventNotFound(id))?;
        let generation = self.next_generation()?;
        let mut events = self.events.clone();
        events.events[index].duration = duration;
        self.events = events;
        self.generation = generation;
        Ok(())
    }

    pub fn toggle_at(
        &mut self,
        pad: PadId,
        raw_frame: Frame,
        velocity: f32,
    ) -> Result<Option<EventId>, PatternEditError> {
        let existing = self.events.events().iter().find_map(|event| {
            (event.pad == pad)
                .then(|| {
                    self.raw_frames.iter().find_map(|(id, frame)| {
                        (*id == event.id && *frame == raw_frame).then_some(event.id)
                    })
                })
                .flatten()
        });
        if let Some(id) = existing {
            self.remove(id)?;
            return Ok(None);
        }
        self.insert_new(pad, raw_frame, velocity, None).map(Some)
    }

    pub fn set_quantize_strength(&mut self, strength: f32) -> Result<(), PatternEditError> {
        if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
            return Err(PatternEditError::InvalidQuantizeStrength);
        }
        let generation = self.next_generation()?;
        let events = self.requantized_events(self.transport, &self.raw_frames, strength)?;
        self.quantize_strength = strength;
        self.events = events;
        self.generation = generation;
        Ok(())
    }

    pub fn rebuild_sample_rate(&mut self, sample_rate: u32) -> Result<(), PatternEditError> {
        let transport = Transport::new(
            sample_rate,
            self.transport.tempo(),
            self.transport.meter(),
            self.transport.bars(),
            self.transport.resolution(),
        )?
        .with_swing(self.transport.swing())?;
        self.replace_transport(transport)
    }

    pub fn set_tempo(&mut self, tempo: crate::Tempo) -> Result<(), PatternEditError> {
        self.replace_transport(self.transport_with(
            self.transport.sample_rate(),
            tempo,
            self.transport.meter(),
            self.transport.bars(),
            self.transport.resolution(),
            self.transport.swing(),
        )?)
    }

    pub fn set_meter(&mut self, meter: crate::Meter) -> Result<(), PatternEditError> {
        self.replace_transport(self.transport_with(
            self.transport.sample_rate(),
            self.transport.tempo(),
            meter,
            self.transport.bars(),
            self.transport.resolution(),
            self.transport.swing(),
        )?)
    }

    pub fn set_bars(&mut self, bars: u16) -> Result<(), PatternEditError> {
        self.replace_transport(self.transport_with(
            self.transport.sample_rate(),
            self.transport.tempo(),
            self.transport.meter(),
            bars,
            self.transport.resolution(),
            self.transport.swing(),
        )?)
    }

    pub fn set_resolution(
        &mut self,
        resolution: crate::Resolution,
    ) -> Result<(), PatternEditError> {
        self.replace_transport(self.transport_with(
            self.transport.sample_rate(),
            self.transport.tempo(),
            self.transport.meter(),
            self.transport.bars(),
            resolution,
            self.transport.swing(),
        )?)
    }

    pub fn set_swing(&mut self, swing: f64) -> Result<(), PatternEditError> {
        self.replace_transport(self.transport_with(
            self.transport.sample_rate(),
            self.transport.tempo(),
            self.transport.meter(),
            self.transport.bars(),
            self.transport.resolution(),
            swing,
        )?)
    }

    pub fn clear(&mut self) -> Result<(), PatternEditError> {
        let generation = self.next_generation()?;
        let checkpoint = PatternCheckpoint {
            transport: self.transport,
            events: self.events.clone(),
            raw_frames: self.raw_frames.clone(),
            quantize_strength: self.quantize_strength,
            next_event_id: self.next_event_id,
        };
        self.events = Pattern::new(self.transport.loop_frames());
        self.raw_frames = Vec::new();
        self.checkpoint = Some(Box::new(checkpoint));
        self.generation = generation;
        Ok(())
    }

    pub fn undo_clear(&mut self) -> Result<(), PatternEditError> {
        let generation = self.next_generation()?;
        let checkpoint = self
            .checkpoint
            .take()
            .ok_or(PatternEditError::NothingToUndo)?;
        self.transport = checkpoint.transport;
        self.events = checkpoint.events;
        self.raw_frames = checkpoint.raw_frames;
        self.quantize_strength = checkpoint.quantize_strength;
        self.next_event_id = checkpoint.next_event_id;
        self.generation = generation;
        Ok(())
    }

    #[cfg(test)]
    fn set_generation_for_test(&mut self, generation: u64) {
        self.generation = generation;
    }

    fn replace_transport(&mut self, transport: Transport) -> Result<(), PatternEditError> {
        let generation = self.next_generation()?;
        let (events, raw_frames) = self.rebuilt_events(transport)?;
        self.transport = transport;
        self.raw_frames = raw_frames;
        self.events = events;
        self.generation = generation;
        Ok(())
    }

    fn transport_with(
        &self,
        sample_rate: u32,
        tempo: crate::Tempo,
        meter: crate::Meter,
        bars: u16,
        resolution: crate::Resolution,
        swing: f64,
    ) -> Result<Transport, PatternEditError> {
        Ok(Transport::new(sample_rate, tempo, meter, bars, resolution)?.with_swing(swing)?)
    }

    fn rebuilt_events(
        &self,
        transport: Transport,
    ) -> Result<(Pattern, Vec<(EventId, Frame)>), PatternEditError> {
        let old_loop_frames = self.transport.loop_frames();
        let new_loop_frames = transport.loop_frames();
        let raw_frames = self
            .raw_frames
            .iter()
            .map(|(id, frame)| {
                let scaled = scale_frame_phase(*frame, old_loop_frames, new_loop_frames)?
                    .min(new_loop_frames - 1);
                Ok((*id, scaled))
            })
            .collect::<Result<Vec<_>, PatternEditError>>()?;
        let mut events = self.requantized_events(transport, &raw_frames, self.quantize_strength)?;
        for event in &mut events.events {
            if let Some(duration) = event.duration {
                event.duration =
                    Some(scale_frame_phase(duration, old_loop_frames, new_loop_frames)?.max(1));
            }
        }
        Ok((events, raw_frames))
    }

    fn next_generation(&self) -> Result<u64, PatternEditError> {
        self.generation
            .checked_add(1)
            .ok_or(PatternEditError::GenerationOverflow)
    }

    pub fn compile(&self) -> Result<PatternSnapshot, PatternCompileError> {
        let loop_frames = self.transport.loop_frames();
        let mut actions = Vec::with_capacity(self.events.events().len().saturating_mul(2));
        for event in self.events.events() {
            validate_compilable_event(event, loop_frames)?;
            push_action(
                &mut actions,
                PatternAction {
                    frame: event.frame,
                    trigger_frame: event.frame,
                    trigger_loop_delta: 0,
                    event_id: event.id,
                    pad: event.pad,
                    kind: PatternActionKind::Trigger {
                        velocity: event.velocity,
                    },
                },
            )?;
            if let Some(duration) = event.duration {
                let release_offset = event
                    .frame
                    .checked_add(duration)
                    .ok_or(PatternCompileError::ArithmeticOverflow)?;
                let release_frame = release_offset % loop_frames;
                push_action(
                    &mut actions,
                    PatternAction {
                        frame: release_frame,
                        trigger_frame: event.frame,
                        trigger_loop_delta: u8::from(release_offset >= loop_frames),
                        event_id: event.id,
                        pad: event.pad,
                        kind: PatternActionKind::Release,
                    },
                )?;
            }
        }
        actions.sort_by(|left, right| {
            left.frame
                .cmp(&right.frame)
                .then(left.event_id.cmp(&right.event_id))
                .then(action_kind_order(left.kind).cmp(&action_kind_order(right.kind)))
        });
        let mut first_loop_valid = [0_u64; FIRST_LOOP_VALID_MASK_WORDS];
        let mut first_loop_valid_prefix = Vec::with_capacity(actions.len() + 1);
        first_loop_valid_prefix.push(0_u16);
        for (index, action) in actions.iter().enumerate() {
            if action.trigger_loop_delta == 0 {
                first_loop_valid[index / u64::BITS as usize] |=
                    1_u64 << (index % u64::BITS as usize);
            }
            let next = first_loop_valid_prefix[index]
                .saturating_add(u16::from(action.trigger_loop_delta == 0));
            first_loop_valid_prefix.push(next);
        }
        let action_frames = actions
            .iter()
            .map(|action| action.frame)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(PatternSnapshot {
            slot: self.slot,
            generation: self.generation,
            loop_frames,
            actions: actions.into_boxed_slice(),
            first_loop_valid,
            first_loop_valid_prefix: first_loop_valid_prefix.into_boxed_slice(),
            action_frames,
        })
    }

    fn requantized_events(
        &self,
        transport: Transport,
        raw_frames: &[(EventId, Frame)],
        strength: f32,
    ) -> Result<Pattern, PatternEditError> {
        let mut events = Pattern::new(transport.loop_frames());
        for event in self.events.events() {
            let raw_frame = raw_frames
                .iter()
                .find_map(|(id, frame)| (*id == event.id).then_some(*frame))
                .ok_or(PatternEditError::MissingRawFrame)?;
            events.insert(quantize_event(*event, raw_frame, transport, strength)?)?;
        }
        Ok(events)
    }
}

/// Scales a frame by loop phase using the same nearest-frame rounding as transport replacement.
pub fn scale_frame_phase(
    frame: Frame,
    old_loop_frames: Frame,
    new_loop_frames: Frame,
) -> Result<Frame, PatternEditError> {
    if old_loop_frames == 0 || new_loop_frames == 0 {
        return Err(PatternEditError::InvalidLoopFrames);
    }
    let scaled = u128::from(frame)
        .checked_mul(u128::from(new_loop_frames))
        .and_then(|value| value.checked_add(u128::from(old_loop_frames / 2)))
        .ok_or(PatternEditError::ArithmeticOverflow)?
        / u128::from(old_loop_frames);
    Frame::try_from(scaled).map_err(|_| PatternEditError::ArithmeticOverflow)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PatternActionKind {
    Trigger { velocity: f32 },
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PatternAction {
    pub frame: Frame,
    pub trigger_frame: Frame,
    pub trigger_loop_delta: u8,
    pub event_id: EventId,
    pub pad: PadId,
    pub kind: PatternActionKind,
}

#[derive(Debug)]
pub struct PatternSnapshot {
    slot: PatternSlotId,
    generation: u64,
    loop_frames: Frame,
    actions: Box<[PatternAction]>,
    first_loop_valid: [u64; FIRST_LOOP_VALID_MASK_WORDS],
    first_loop_valid_prefix: Box<[u16]>,
    action_frames: Box<[Frame]>,
}

impl PatternSnapshot {
    pub fn slot(&self) -> PatternSlotId {
        self.slot
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn loop_frames(&self) -> Frame {
        self.loop_frames
    }

    pub fn actions(&self) -> &[PatternAction] {
        &self.actions
    }

    pub fn first_loop_valid_word(&self, word_index: usize) -> u64 {
        self.first_loop_valid.get(word_index).copied().unwrap_or(0)
    }

    pub fn first_loop_valid_count(&self, start: usize, end: usize) -> usize {
        let start = start.min(self.actions.len());
        let end = end.min(self.actions.len());
        if start >= end {
            return 0;
        }
        usize::from(self.first_loop_valid_prefix[end] - self.first_loop_valid_prefix[start])
    }

    pub fn action_index_at_or_after(&self, frame: Frame) -> usize {
        self.action_frames
            .partition_point(|action_frame| *action_frame < frame)
    }
}

fn validate_compilable_event(
    event: &PatternEvent,
    loop_frames: Frame,
) -> Result<(), PatternCompileError> {
    (event.id.0 != 0
        && event.frame < loop_frames
        && event.velocity.is_finite()
        && (0.0..=1.0).contains(&event.velocity)
        && event.duration != Some(0)
        && event
            .duration
            .is_none_or(|duration| duration <= loop_frames))
    .then_some(())
    .ok_or(PatternCompileError::InvalidEvent)
}

fn push_action(
    actions: &mut Vec<PatternAction>,
    action: PatternAction,
) -> Result<(), PatternCompileError> {
    if actions.len() >= MAX_PATTERN_ACTIONS {
        return Err(PatternCompileError::TooManyActions);
    }
    actions.push(action);
    Ok(())
}

fn action_kind_order(kind: PatternActionKind) -> u8 {
    match kind {
        PatternActionKind::Release => 0,
        PatternActionKind::Trigger { .. } => 1,
    }
}

fn validate_editable_event(
    event: &PatternEvent,
    loop_frames: Frame,
) -> Result<(), PatternEditError> {
    (event.id.0 != 0
        && event.frame < loop_frames
        && event.velocity.is_finite()
        && (0.0..=1.0).contains(&event.velocity)
        && event.duration != Some(0)
        && event
            .duration
            .is_none_or(|duration| duration <= loop_frames))
    .then_some(())
    .ok_or(PatternEditError::Model(ModelError::InvalidEvent))
}

fn quantize_event(
    mut event: PatternEvent,
    raw_frame: Frame,
    transport: Transport,
    strength: f32,
) -> Result<PatternEvent, PatternEditError> {
    if raw_frame >= transport.loop_frames() {
        return Err(PatternEditError::Model(ModelError::InvalidEvent));
    }
    let target = (0..=transport.step_count())
        .map(|step| transport.step_frame(step))
        .min_by_key(|frame| frame.abs_diff(raw_frame))
        .unwrap_or(0);
    let raw = i64::try_from(raw_frame).map_err(|_| PatternEditError::ArithmeticOverflow)?;
    let target_offset = i64::try_from(target).map_err(|_| PatternEditError::ArithmeticOverflow)?;
    let original_offset = raw
        .checked_sub(target_offset)
        .ok_or(PatternEditError::ArithmeticOverflow)?;
    let delta = i128::from(target) - i128::from(raw_frame);
    let shifted = i128::from(raw_frame) + (delta as f64 * f64::from(strength)).round() as i128;
    event.frame = raw_frame;
    event.original_offset = Some(original_offset);
    event.frame = Frame::try_from(shifted.max(0))
        .map_err(|_| PatternEditError::ArithmeticOverflow)?
        % transport.loop_frames();
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BankId, Meter, ModelError, PadId, Resolution, Tempo, Transport};

    fn transport() -> Transport {
        Transport::new(
            48_000,
            Tempo::new(120.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap()
    }

    fn pad() -> PadId {
        PadId::new(BankId::new(0).unwrap(), 0).unwrap()
    }

    fn editable(sample_rate: u32, bars: u16) -> EditablePattern {
        EditablePattern::new(
            PatternSlotId::new(0).unwrap(),
            "Pattern",
            Transport::new(
                sample_rate,
                Tempo::new(120.0).unwrap(),
                Meter::new(4, 4).unwrap(),
                bars,
                Resolution::Sixteenth,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn raw_event(id: u64, frame: Frame) -> PatternEvent {
        PatternEvent::new(EventId(id), pad(), frame, 1.0, None).unwrap()
    }

    fn event_with_duration(id: u64, frame: Frame, duration: Frame) -> PatternEvent {
        PatternEvent::new(EventId(id), pad(), frame, 1.0, Some(duration)).unwrap()
    }

    fn editable_with_durations(count: usize) -> EditablePattern {
        let mut pattern = editable(48_000, 4);
        for id in 1..=count as u64 {
            pattern.insert(event_with_duration(id, id - 1, 1)).unwrap();
        }
        pattern
    }

    #[test]
    fn pattern_slot_and_event_capacity_are_exact() {
        assert!(PatternSlotId::new(15).is_ok());
        assert!(PatternSlotId::new(16).is_err());
        let mut pattern = editable(48_000, 4);
        for id in 1..=MAX_PATTERN_EVENTS as u64 {
            pattern.insert(raw_event(id, id - 1)).unwrap();
        }
        assert_eq!(
            pattern.insert(raw_event(2_000, 10)),
            Err(PatternEditError::Full)
        );
    }

    #[test]
    fn changing_quantization_is_reversible_from_the_raw_frame() {
        let mut pattern = editable(48_000, 1);
        pattern.insert(raw_event(1, 6_800)).unwrap();
        pattern.set_quantize_strength(1.0).unwrap();
        assert_eq!(pattern.event(EventId(1)).unwrap().frame, 6_000);
        assert_eq!(
            pattern.event(EventId(1)).unwrap().original_offset,
            Some(800)
        );
        pattern.set_quantize_strength(0.0).unwrap();
        assert_eq!(pattern.event(EventId(1)).unwrap().frame, 6_800);
    }

    #[test]
    fn snapshot_expands_wrapped_duration_and_sorts_release_after_trigger() {
        let mut pattern = editable(100, 1);
        let loop_frames = pattern.transport().loop_frames();
        pattern
            .insert(event_with_duration(1, loop_frames - 5, 10))
            .unwrap();
        let snapshot = pattern.compile().unwrap();
        assert_eq!(snapshot.actions()[0].frame, 5);
        assert_eq!(snapshot.actions()[0].kind, PatternActionKind::Release);
        assert_eq!(snapshot.actions()[1].frame, loop_frames - 5);
        assert!(matches!(
            snapshot.actions()[1].kind,
            PatternActionKind::Trigger { .. }
        ));
    }

    #[test]
    fn compiled_release_retains_the_trigger_relative_frame() {
        let mut pattern = editable(100, 1);
        let loop_frames = pattern.transport().loop_frames();
        pattern
            .insert(event_with_duration(7, loop_frames - 5, 10))
            .unwrap();

        let snapshot = pattern.compile().unwrap();
        let release = snapshot
            .actions()
            .iter()
            .find(|action| action.kind == PatternActionKind::Release)
            .unwrap();

        assert_eq!(
            (
                release.event_id,
                release.frame,
                release.trigger_frame,
                release.trigger_loop_delta,
            ),
            (EventId(7), 5, loop_frames - 5, 1)
        );
    }

    #[test]
    fn release_loop_delta_distinguishes_non_wrapped_wrapped_and_exact_loop_durations() {
        let mut pattern = editable(100, 1);
        let loop_frames = pattern.transport().loop_frames();
        pattern.insert(event_with_duration(1, 10, 5)).unwrap();
        pattern
            .insert(event_with_duration(2, loop_frames - 2, 4))
            .unwrap();
        pattern
            .insert(event_with_duration(3, 20, loop_frames))
            .unwrap();

        let snapshot = pattern.compile().unwrap();
        let delta = |id| {
            snapshot
                .actions()
                .iter()
                .find(|action| {
                    action.event_id == EventId(id) && action.kind == PatternActionKind::Release
                })
                .unwrap()
                .trigger_loop_delta
        };

        assert_eq!((delta(1), delta(2), delta(3)), (0, 1, 1));
    }

    #[test]
    fn duration_longer_than_the_loop_is_rejected_without_mutating_editable_state() {
        let mut pattern = editable(100, 1);
        let loop_frames = pattern.transport().loop_frames();
        let before_generation = pattern.generation();
        let before_next_event_id = pattern.next_event_id();
        let before_raw_frames = pattern.raw_frames.clone();

        assert_eq!(
            pattern.insert_new(pad(), 10, 1.0, Some(loop_frames + 1)),
            Err(PatternEditError::Model(ModelError::InvalidEvent))
        );
        assert!(pattern.events().is_empty());
        assert_eq!(pattern.raw_frames, before_raw_frames);
        assert_eq!(pattern.generation(), before_generation);
        assert_eq!(pattern.next_event_id(), before_next_event_id);
    }

    #[test]
    fn compile_rejects_a_corrupted_duration_longer_than_the_loop() {
        let mut pattern = editable(100, 1);
        let loop_frames = pattern.transport().loop_frames();
        pattern.insert(event_with_duration(1, 10, 5)).unwrap();
        pattern.events.events[0].duration = Some(loop_frames + 1);

        assert!(matches!(
            pattern.compile(),
            Err(PatternCompileError::InvalidEvent)
        ));
    }

    #[test]
    fn first_loop_validity_mask_skips_a_thousand_twenty_four_wrapped_releases() {
        let mut pattern = editable(100, 1);
        let loop_frames = pattern.transport().loop_frames();
        for id in 1..=MAX_PATTERN_EVENTS as u64 {
            pattern
                .insert(event_with_duration(id, loop_frames - 1, 1))
                .unwrap();
        }

        let snapshot = pattern.compile().unwrap();
        let valid = (0..FIRST_LOOP_VALID_MASK_WORDS)
            .map(|word| snapshot.first_loop_valid_word(word).count_ones() as usize)
            .sum::<usize>();

        assert_eq!(valid, MAX_PATTERN_EVENTS);
        assert!((0..16).all(|word| snapshot.first_loop_valid_word(word) == 0));
        assert!((16..32).all(|word| snapshot.first_loop_valid_word(word) == u64::MAX));
        assert_eq!(
            snapshot.first_loop_valid_word(FIRST_LOOP_VALID_MASK_WORDS),
            0
        );
        assert_eq!(snapshot.action_index_at_or_after(0), 0);
        assert_eq!(snapshot.action_index_at_or_after(1), MAX_PATTERN_EVENTS);
        assert_eq!(
            snapshot.action_index_at_or_after(loop_frames - 1),
            MAX_PATTERN_EVENTS
        );
        assert_eq!(
            snapshot.action_index_at_or_after(loop_frames),
            MAX_PATTERN_ACTIONS
        );
        assert_eq!(snapshot.first_loop_valid_count(0, MAX_PATTERN_EVENTS), 0);
        assert_eq!(
            snapshot.first_loop_valid_count(0, MAX_PATTERN_ACTIONS),
            MAX_PATTERN_EVENTS
        );
    }

    #[test]
    fn snapshot_accepts_the_exact_two_thousand_forty_eight_action_bound() {
        let pattern = editable_with_durations(MAX_PATTERN_EVENTS);
        assert_eq!(
            pattern.compile().unwrap().actions().len(),
            MAX_PATTERN_ACTIONS
        );
    }

    #[test]
    fn rebuilding_sample_rate_preserves_raw_frame_phase() {
        let mut pattern = editable(100, 1);
        pattern.insert(raw_event(1, 100)).unwrap();
        pattern.rebuild_sample_rate(200).unwrap();
        assert_eq!(pattern.transport().sample_rate(), 200);
        assert_eq!(pattern.event(EventId(1)).unwrap().frame, 200);
    }

    #[test]
    fn rebuilding_sample_rate_preserves_duration_phase() {
        let mut pattern = editable(100, 1);
        pattern.insert(event_with_duration(1, 100, 10)).unwrap();
        pattern.rebuild_sample_rate(200).unwrap();
        assert_eq!(pattern.event(EventId(1)).unwrap().duration, Some(20));
    }

    #[test]
    fn phase_scaling_rejects_zero_loop_sizes_with_the_exact_typed_error() {
        assert_eq!(
            scale_frame_phase(1, 0, 4),
            Err(PatternEditError::InvalidLoopFrames)
        );
        assert_eq!(
            scale_frame_phase(1, 4, 0),
            Err(PatternEditError::InvalidLoopFrames)
        );
    }

    #[test]
    fn phase_scaling_preserves_nearest_frame_boundary_rounding() {
        assert_eq!(scale_frame_phase(1, 2, 3), Ok(2));
        assert_eq!(scale_frame_phase(2, 3, 4), Ok(3));
        assert_eq!(scale_frame_phase(1, 2, 4), Ok(2));
        assert_eq!(
            scale_frame_phase(u64::MAX, u64::MAX, u64::MAX),
            Ok(u64::MAX)
        );
    }

    #[test]
    fn failed_maximum_event_id_insert_leaves_the_pattern_retryable() {
        let mut pattern = editable(48_000, 1);
        let before_generation = pattern.generation();
        let event = raw_event(u64::MAX, 100);

        assert_eq!(
            pattern.insert(event),
            Err(PatternEditError::ArithmeticOverflow)
        );
        assert!(pattern.events().is_empty());
        assert_eq!(pattern.next_event_id(), EventId(1));
        assert_eq!(pattern.generation(), before_generation);
        assert_eq!(
            pattern.insert(event),
            Err(PatternEditError::ArithmeticOverflow)
        );
        pattern.insert(raw_event(1, 100)).unwrap();
        assert_eq!(pattern.event(EventId(1)).unwrap().frame, 100);
    }

    #[test]
    fn persisted_restore_rebuilds_raw_ledger_and_resets_transient_state() {
        let transport = transport();
        let mut event = raw_event(17, 6_000);
        event.original_offset = Some(800);
        let mut restored = EditablePattern::from_persisted(
            PatternSlotId::new(4).unwrap(),
            "restored",
            transport,
            vec![(event, 6_800)],
            1.0,
        )
        .unwrap();

        assert_eq!(restored.event(EventId(17)).unwrap().frame, 6_000);
        assert_eq!(
            restored.event(EventId(17)).unwrap().original_offset,
            Some(800)
        );
        assert_eq!(restored.quantize_strength(), 1.0);
        assert_eq!(restored.next_event_id(), EventId(18));
        assert_eq!(restored.generation(), 0);
        assert_eq!(restored.undo_clear(), Err(PatternEditError::NothingToUndo));
    }

    #[test]
    fn persisted_restore_is_failure_atomic_for_invalid_raw_frames_and_max_ids() {
        let before_loop = raw_event(1, 0);
        assert!(matches!(
            EditablePattern::from_persisted(
                PatternSlotId::new(0).unwrap(),
                "invalid",
                transport(),
                vec![(before_loop, u64::MAX)],
                1.0,
            ),
            Err(PatternEditError::Model(ModelError::InvalidEvent))
        ));

        let max = raw_event(u64::MAX, 0);
        assert!(matches!(
            EditablePattern::from_persisted(
                PatternSlotId::new(0).unwrap(),
                "invalid",
                transport(),
                vec![(max, 0)],
                0.0,
            ),
            Err(PatternEditError::ArithmeticOverflow)
        ));

        let mut inconsistent = raw_event(2, 6_001);
        inconsistent.original_offset = Some(799);
        assert!(matches!(
            EditablePattern::from_persisted(
                PatternSlotId::new(0).unwrap(),
                "invalid",
                transport(),
                vec![(inconsistent, 6_800)],
                1.0,
            ),
            Err(PatternEditError::Model(ModelError::InvalidEvent))
        ));
    }

    #[test]
    fn insert_rejects_out_of_loop_raw_frames_without_wrapping_them() {
        let mut pattern = editable(48_000, 1);
        let loop_frames = pattern.transport().loop_frames();
        for frame in [loop_frames, u64::MAX] {
            assert_eq!(
                pattern.insert(raw_event(1, frame)),
                Err(PatternEditError::Model(ModelError::InvalidEvent))
            );
            assert!(pattern.events().is_empty());
            assert_eq!(pattern.next_event_id(), EventId(1));
        }
    }

    #[test]
    fn failed_zero_loop_rate_rebuild_preserves_every_editable_field() {
        let mut pattern = editable(48_000, 1);
        pattern.insert(raw_event(1, 6_800)).unwrap();
        pattern.set_tempo(Tempo::new(300.0).unwrap()).unwrap();
        pattern.set_meter(Meter::new(1, 16).unwrap()).unwrap();
        let before_transport = pattern.transport();
        let before_event = *pattern.event(EventId(1)).unwrap();
        let before_generation = pattern.generation();

        assert_eq!(
            pattern.rebuild_sample_rate(1),
            Err(PatternEditError::Model(ModelError::InvalidTransport))
        );
        assert_eq!(pattern.transport(), before_transport);
        assert_eq!(pattern.event(EventId(1)), Some(&before_event));
        assert_eq!(pattern.generation(), before_generation);
    }

    #[test]
    fn editing_operations_keep_raw_events_and_one_clear_checkpoint_aligned() {
        let mut pattern = editable(48_000, 1);
        let first = pattern.insert_new(pad(), 6_800, 1.0, None).unwrap();
        let second = pattern.insert_new(pad(), 12_100, 0.8, None).unwrap();
        pattern.set_quantize_strength(1.0).unwrap();

        let removed = pattern.remove(first).unwrap();
        assert_eq!(removed.id, first);
        assert_eq!(pattern.event(first), None);
        pattern.set_velocity(second, 0.35).unwrap();
        assert_eq!(pattern.event(second).unwrap().velocity, 0.35);
        pattern.set_swing(0.60).unwrap();
        assert_eq!(pattern.event(second).unwrap().frame, 12_000);

        pattern.clear().unwrap();
        assert!(pattern.events().is_empty());
        pattern.undo_clear().unwrap();
        assert_eq!(pattern.event(second).unwrap().frame, 12_000);
        assert_eq!(pattern.event(second).unwrap().velocity, 0.35);
        assert_eq!(pattern.undo_clear(), Err(PatternEditError::NothingToUndo));
    }

    #[test]
    fn removing_an_event_removes_its_raw_position() {
        let mut pattern = editable(48_000, 1);
        let id = pattern.insert_new(pad(), 6_800, 1.0, None).unwrap();
        pattern.remove(id).unwrap();
        pattern.set_quantize_strength(1.0).unwrap();
        assert!(pattern.events().is_empty());
    }

    #[test]
    fn velocity_updates_are_visible_without_moving_the_event() {
        let mut pattern = editable(48_000, 1);
        let id = pattern.insert_new(pad(), 6_800, 1.0, None).unwrap();
        pattern.set_velocity(id, 0.35).unwrap();
        assert_eq!(
            (
                pattern.event(id).unwrap().frame,
                pattern.event(id).unwrap().velocity
            ),
            (6_800, 0.35)
        );
    }

    #[test]
    fn duration_update_preserves_raw_frames_and_is_failure_atomic() {
        let mut pattern = editable(48_000, 1);
        let id = pattern.insert_new(pad(), 6_800, 1.0, None).unwrap();
        pattern.set_swing(0.60).unwrap();
        pattern.set_quantize_strength(1.0).unwrap();
        assert_eq!(pattern.event(id).unwrap().frame, 7_200);

        pattern.set_duration(id, Some(4_000)).unwrap();
        pattern.set_quantize_strength(0.0).unwrap();
        assert_eq!(
            (
                pattern.event(id).unwrap().frame,
                pattern.event(id).unwrap().duration
            ),
            (6_800, Some(4_000))
        );

        let before_event = *pattern.event(id).unwrap();
        let before_generation = pattern.generation();
        assert_eq!(
            pattern.set_duration(id, Some(pattern.transport().loop_frames() + 1)),
            Err(PatternEditError::Model(ModelError::InvalidEvent))
        );
        assert_eq!(pattern.event(id), Some(&before_event));
        assert_eq!(pattern.generation(), before_generation);
    }

    #[test]
    fn changing_swing_requantizes_from_the_retained_raw_frame() {
        let mut pattern = editable(48_000, 1);
        let id = pattern.insert_new(pad(), 6_800, 1.0, None).unwrap();
        pattern.set_quantize_strength(1.0).unwrap();
        assert_eq!(pattern.event(id).unwrap().frame, 6_000);
        pattern.set_swing(0.60).unwrap();
        assert_eq!(pattern.event(id).unwrap().frame, 7_200);
        pattern.set_quantize_strength(0.0).unwrap();
        assert_eq!(pattern.event(id).unwrap().frame, 6_800);
    }

    #[test]
    fn clear_and_undo_restore_a_single_complete_checkpoint() {
        let mut pattern = editable(48_000, 1);
        let id = pattern.insert_new(pad(), 6_800, 0.4, None).unwrap();
        pattern.set_quantize_strength(1.0).unwrap();
        pattern.set_swing(0.60).unwrap();
        pattern.clear().unwrap();
        assert!(pattern.events().is_empty());
        pattern.undo_clear().unwrap();
        assert_eq!(pattern.event(id).unwrap().frame, 7_200);
        assert_eq!(pattern.event(id).unwrap().velocity, 0.4);
        assert_eq!(pattern.undo_clear(), Err(PatternEditError::NothingToUndo));
    }

    #[test]
    fn toggling_a_raw_step_inserts_then_removes_the_same_event() {
        let mut pattern = editable(48_000, 1);
        let inserted = pattern.toggle_at(pad(), 6_000, 0.9).unwrap();
        assert_eq!(inserted, Some(EventId(1)));
        assert_eq!(pattern.events().len(), 1);
        assert_eq!(pattern.toggle_at(pad(), 6_000, 0.9).unwrap(), None);
        assert!(pattern.events().is_empty());
    }

    #[test]
    fn tempo_bars_and_resolution_changes_preserve_phase_or_requantize_raw_timing() {
        let mut pattern = editable(100, 1);
        pattern.insert_new(pad(), 100, 1.0, None).unwrap();
        pattern.set_tempo(Tempo::new(60.0).unwrap()).unwrap();
        assert_eq!(pattern.event(EventId(1)).unwrap().frame, 200);
        pattern.set_bars(2).unwrap();
        assert_eq!(pattern.event(EventId(1)).unwrap().frame, 400);
        pattern.set_quantize_strength(1.0).unwrap();
        pattern.set_resolution(Resolution::Quarter).unwrap();
        assert_eq!(pattern.event(EventId(1)).unwrap().frame, 400);
    }

    #[test]
    fn meter_change_preserves_event_loop_phase() {
        let mut pattern = editable(100, 1);
        pattern.insert_new(pad(), 100, 1.0, None).unwrap();
        pattern.set_meter(Meter::new(2, 4).unwrap()).unwrap();
        assert_eq!(pattern.event(EventId(1)).unwrap().frame, 50);
    }

    #[test]
    fn invalid_transport_change_leaves_the_editable_pattern_unchanged() {
        let mut pattern = editable(48_000, 1);
        pattern.insert_new(pad(), 6_800, 1.0, None).unwrap();
        let before_transport = pattern.transport();
        let before_event = *pattern.event(EventId(1)).unwrap();
        let before_generation = pattern.generation();
        assert_eq!(
            pattern.set_bars(0),
            Err(PatternEditError::Model(ModelError::InvalidTransport))
        );
        assert_eq!(pattern.transport(), before_transport);
        assert_eq!(pattern.event(EventId(1)), Some(&before_event));
        assert_eq!(pattern.generation(), before_generation);
    }

    #[test]
    fn generation_overflow_rejects_an_edit_without_mutating_state() {
        let mut pattern = editable(48_000, 1);
        pattern.set_generation_for_test(u64::MAX);
        assert_eq!(
            pattern.insert(raw_event(1, 100)),
            Err(PatternEditError::GenerationOverflow)
        );
        assert!(pattern.events().is_empty());
    }

    #[test]
    fn quantization_preserves_original_offset() {
        let event = PatternEvent::new(EventId(1), pad(), 6_800, 1.0, None).unwrap();
        let quantized = event.quantized(&transport(), 1.0);
        assert_eq!(quantized.frame, 6_000);
        assert_eq!(quantized.original_offset, Some(800));
    }

    #[test]
    fn partial_quantization_moves_toward_the_grid() {
        let event = PatternEvent::new(EventId(1), pad(), 6_800, 1.0, None).unwrap();
        assert_eq!(event.quantized(&transport(), 0.5).frame, 6_400);
        assert_eq!(event.quantized(&transport(), -1.0).frame, 6_800);
    }

    #[test]
    fn direct_quantization_does_not_wrap_an_out_of_loop_frame() {
        let event =
            PatternEvent::new(EventId(1), pad(), transport().loop_frames(), 1.0, None).unwrap();
        assert_eq!(
            event.quantized(&transport(), 1.0).frame,
            transport().loop_frames()
        );
    }

    #[test]
    fn scheduling_writes_into_caller_buffer_and_reports_overflow() {
        let mut pattern = Pattern::new(96_000);
        pattern
            .insert(PatternEvent::new(EventId(1), pad(), 100, 0.8, None).unwrap())
            .unwrap();
        pattern
            .insert(PatternEvent::new(EventId(2), pad(), 200, 1.0, Some(50)).unwrap())
            .unwrap();
        let mut output = [ScheduledEvent::EMPTY; 1];
        let result = pattern.schedule_range(0, 300, &mut output);
        assert_eq!(result.written, 1);
        assert_eq!(result.dropped, 1);
        assert_eq!(output[0].at, 100);
        assert_eq!(output[0].pad, pad());
    }

    #[test]
    fn range_is_half_open_and_events_are_sorted() {
        let mut pattern = Pattern::new(1_000);
        pattern
            .insert(PatternEvent::new(EventId(2), pad(), 200, 1.0, None).unwrap())
            .unwrap();
        pattern
            .insert(PatternEvent::new(EventId(1), pad(), 100, 1.0, None).unwrap())
            .unwrap();
        let mut output = [ScheduledEvent::EMPTY; 2];
        let result = pattern.schedule_range(100, 200, &mut output);
        assert_eq!(result.written, 1);
        assert_eq!(output[0].event_id, EventId(1));
    }

    #[test]
    fn empty_or_reversed_range_schedules_nothing() {
        let mut pattern = Pattern::new(1_000);
        pattern
            .insert(PatternEvent::new(EventId(1), pad(), 100, 1.0, None).unwrap())
            .unwrap();
        let mut output = [ScheduledEvent::EMPTY; 1];
        assert_eq!(
            pattern.schedule_range(200, 100, &mut output),
            ScheduleResult {
                written: 0,
                dropped: 0
            }
        );
        assert_eq!(
            pattern.schedule_range(100, 100, &mut output),
            ScheduleResult {
                written: 0,
                dropped: 0
            }
        );
    }

    #[test]
    fn duplicate_ids_and_out_of_bounds_events_are_rejected() {
        let mut pattern = Pattern::new(1_000);
        let event = PatternEvent::new(EventId(1), pad(), 100, 1.0, None).unwrap();
        pattern.insert(event).unwrap();
        assert_eq!(pattern.insert(event), Err(ModelError::DuplicateEvent));
        assert_eq!(
            pattern.insert(PatternEvent::new(EventId(2), pad(), 1_000, 1.0, None).unwrap()),
            Err(ModelError::InvalidEvent)
        );
        assert_eq!(pattern.remove(EventId(1)), Some(event));
    }

    #[test]
    fn events_validate_identity_velocity_and_duration() {
        assert_eq!(
            PatternEvent::new(EventId(0), pad(), 0, 1.0, None),
            Err(ModelError::InvalidEvent)
        );
        assert_eq!(
            PatternEvent::new(EventId(1), pad(), 0, f32::NAN, None),
            Err(ModelError::InvalidEvent)
        );
        assert_eq!(
            PatternEvent::new(EventId(1), pad(), 0, 1.0, Some(0)),
            Err(ModelError::InvalidEvent)
        );
    }
}
