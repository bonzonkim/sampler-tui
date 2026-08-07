//! Pattern event model and scheduling.

use serde::{Deserialize, Serialize};

use crate::{Frame, ModelError, PadId, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId(pub u64);

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
