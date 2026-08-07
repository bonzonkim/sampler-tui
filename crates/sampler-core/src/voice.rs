//! Fixed-capacity voice allocation.

use crate::{ChokeGroup, Frame, PadId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VoiceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceRequest {
    pub pad: PadId,
    pub started_at: Frame,
    pub gain: f32,
    pub choke_group: Option<ChokeGroup>,
    pub protected: bool,
}

impl VoiceRequest {
    pub fn new(
        pad: PadId,
        started_at: Frame,
        gain: f32,
        choke_group: Option<ChokeGroup>,
        protected: bool,
    ) -> Self {
        Self {
            pad,
            started_at,
            gain,
            choke_group,
            protected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Voice {
    pub id: VoiceId,
    pub pad: PadId,
    pub started_at: Frame,
    pub gain: f32,
    pub choke_group: Option<ChokeGroup>,
    pub protected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Allocation {
    pub slot: usize,
    pub voice: Voice,
    pub stolen: Option<Voice>,
}

#[derive(Debug)]
pub struct VoiceAllocator<const N: usize> {
    voices: [Option<Voice>; N],
    next_id: u64,
}

impl<const N: usize> VoiceAllocator<N> {
    pub fn new() -> Self {
        assert!(N > 0, "voice allocator capacity must be non-zero");
        Self {
            voices: [None; N],
            next_id: 1,
        }
    }

    pub fn trigger(&mut self, request: VoiceRequest) -> Allocation {
        let slot = self
            .voices
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| self.steal_slot());
        let stolen = self.voices[slot];
        let voice = Voice {
            id: VoiceId(self.next_id),
            pad: request.pad,
            started_at: request.started_at,
            gain: request.gain,
            choke_group: request.choke_group,
            protected: request.protected,
        };
        self.next_id = self.next_id.wrapping_add(1);
        self.voices[slot] = Some(voice);
        Allocation {
            slot,
            voice,
            stolen,
        }
    }

    pub fn release_pad(&mut self, pad: PadId) -> usize {
        self.stop_where(|voice| voice.pad == pad)
    }

    pub fn stop_choke_group(&mut self, group: ChokeGroup) -> usize {
        self.stop_where(|voice| voice.choke_group == Some(group))
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().flatten().count()
    }

    fn stop_where(&mut self, predicate: impl Fn(Voice) -> bool) -> usize {
        let mut stopped = 0;
        for slot in &mut self.voices {
            if slot.is_some_and(&predicate) {
                *slot = None;
                stopped += 1;
            }
        }
        stopped
    }

    fn steal_slot(&self) -> usize {
        self.voices
            .iter()
            .enumerate()
            .filter_map(|(slot, voice)| voice.map(|voice| (slot, voice)))
            .filter(|(_, voice)| !voice.protected)
            .min_by(|(_, left), (_, right)| {
                left.gain
                    .total_cmp(&right.gain)
                    .then_with(|| left.started_at.cmp(&right.started_at))
            })
            .or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, voice)| voice.map(|voice| (slot, voice)))
                    .min_by_key(|(_, voice)| voice.started_at)
            })
            .map(|(slot, _)| slot)
            .expect("non-zero full allocator has a voice")
    }
}

impl<const N: usize> Default for VoiceAllocator<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BankId, ChokeGroup, PadId};

    fn pad(index: u8) -> PadId {
        PadId::new(BankId::new(0).unwrap(), index).unwrap()
    }

    #[test]
    fn uses_empty_slots_before_stealing() {
        let mut voices = VoiceAllocator::<2>::new();
        let first = voices.trigger(VoiceRequest::new(pad(0), 10, 1.0, None, false));
        let second = voices.trigger(VoiceRequest::new(pad(1), 11, 0.8, None, false));
        assert_eq!((first.slot, second.slot), (0, 1));
        assert_eq!(voices.active_voices(), 2);
    }

    #[test]
    fn steals_quietest_then_oldest_unprotected_voice() {
        let mut voices = VoiceAllocator::<3>::new();
        voices.trigger(VoiceRequest::new(pad(0), 10, 0.5, None, true));
        voices.trigger(VoiceRequest::new(pad(1), 11, 0.2, None, false));
        voices.trigger(VoiceRequest::new(pad(2), 12, 0.2, None, false));
        let allocation = voices.trigger(VoiceRequest::new(pad(3), 20, 1.0, None, false));
        assert_eq!(allocation.slot, 1);
        assert_eq!(allocation.stolen.unwrap().pad, pad(1));
    }

    #[test]
    fn all_protected_voices_fall_back_to_the_oldest() {
        let mut voices = VoiceAllocator::<2>::new();
        voices.trigger(VoiceRequest::new(pad(0), 10, 0.5, None, true));
        voices.trigger(VoiceRequest::new(pad(1), 11, 0.1, None, true));
        let allocation = voices.trigger(VoiceRequest::new(pad(2), 12, 1.0, None, false));
        assert_eq!(allocation.slot, 0);
        assert_eq!(allocation.stolen.unwrap().pad, pad(0));
    }

    #[test]
    fn choke_stops_matching_group() {
        let mut voices = VoiceAllocator::<4>::new();
        let group = ChokeGroup::new(1).unwrap();
        voices.trigger(VoiceRequest::new(pad(0), 10, 1.0, Some(group), false));
        voices.trigger(VoiceRequest::new(pad(1), 11, 1.0, Some(group), false));
        voices.trigger(VoiceRequest::new(pad(2), 12, 1.0, None, false));
        assert_eq!(voices.stop_choke_group(group), 2);
        assert_eq!(voices.active_voices(), 1);
    }

    #[test]
    fn release_stops_every_voice_for_the_pad() {
        let mut voices = VoiceAllocator::<4>::new();
        voices.trigger(VoiceRequest::new(pad(0), 10, 1.0, None, false));
        voices.trigger(VoiceRequest::new(pad(0), 11, 1.0, None, false));
        voices.trigger(VoiceRequest::new(pad(1), 12, 1.0, None, false));
        assert_eq!(voices.release_pad(pad(0)), 2);
        assert_eq!(voices.active_voices(), 1);
    }
}
