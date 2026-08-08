//! Pattern event model and scheduling.

use serde::{Deserialize, Serialize};

use crate::{Frame, ModelError, PadId, PatternCompileError, PatternEditError, Transport};

pub const PATTERN_SLOT_COUNT: usize = 16;
pub const MAX_PATTERN_EVENTS: usize = 1_024;
pub const MAX_PATTERN_ACTIONS: usize = 2_048;

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
        let strength = if strength.is_finite() {
            strength.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let target = (0..=transport.step_count())
            .map(|step| transport.step_frame(step))
            .min_by_key(|frame| frame.abs_diff(self.frame))
            .unwrap_or(0);
        let delta = target as i128 - self.frame as i128;
        let shifted = self.frame as i128 + (delta as f64 * f64::from(strength)).round() as i128;
        self.original_offset
            .get_or_insert(self.frame as i64 - target as i64);
        self.frame = (shifted.max(0) as Frame) % transport.loop_frames();
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

#[derive(Debug)]
struct PatternCheckpoint;

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
    #[allow(dead_code)]
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

    pub fn insert(&mut self, event: PatternEvent) -> Result<(), PatternEditError> {
        if self.events.events().len() >= MAX_PATTERN_EVENTS {
            return Err(PatternEditError::Full);
        }
        let raw_frame = event.frame;
        let event = quantize_event(event, raw_frame, self.transport, self.quantize_strength);
        self.events.insert(event)?;
        self.raw_frames.push((event.id, raw_frame));
        self.next_event_id = self.next_event_id.max(
            event
                .id
                .0
                .checked_add(1)
                .ok_or(PatternEditError::ArithmeticOverflow)?,
        );
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    pub fn set_quantize_strength(&mut self, strength: f32) -> Result<(), PatternEditError> {
        if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
            return Err(PatternEditError::InvalidQuantizeStrength);
        }
        let events = self.requantized_events(self.transport, &self.raw_frames, strength)?;
        self.quantize_strength = strength;
        self.events = events;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }

    pub fn rebuild_sample_rate(&mut self, sample_rate: u32) -> Result<(), PatternEditError> {
        let old_loop_frames = self.transport.loop_frames();
        let transport = Transport::new(
            sample_rate,
            self.transport.tempo(),
            self.transport.meter(),
            self.transport.bars(),
            self.transport.resolution(),
        )?
        .with_swing(self.transport.swing())?;
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
        self.transport = transport;
        self.raw_frames = raw_frames;
        self.events = events;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
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
                    event_id: event.id,
                    pad: event.pad,
                    kind: PatternActionKind::Trigger {
                        velocity: event.velocity,
                    },
                },
            )?;
            if let Some(duration) = event.duration {
                let release_frame = event
                    .frame
                    .checked_add(duration)
                    .ok_or(PatternCompileError::ArithmeticOverflow)?
                    % loop_frames;
                push_action(
                    &mut actions,
                    PatternAction {
                        frame: release_frame,
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
        Ok(PatternSnapshot {
            slot: self.slot,
            generation: self.generation,
            loop_frames,
            actions: actions.into_boxed_slice(),
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
            events.insert(quantize_event(*event, raw_frame, transport, strength))?;
        }
        Ok(events)
    }
}

fn scale_frame_phase(
    frame: Frame,
    old_loop_frames: Frame,
    new_loop_frames: Frame,
) -> Result<Frame, PatternEditError> {
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
}

fn validate_compilable_event(
    event: &PatternEvent,
    loop_frames: Frame,
) -> Result<(), PatternCompileError> {
    (event.id.0 != 0
        && event.frame < loop_frames
        && event.velocity.is_finite()
        && (0.0..=1.0).contains(&event.velocity)
        && event.duration != Some(0))
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

fn quantize_event(
    mut event: PatternEvent,
    raw_frame: Frame,
    transport: Transport,
    strength: f32,
) -> PatternEvent {
    event.frame = raw_frame;
    event.original_offset = None;
    event.quantized(&transport, strength)
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
