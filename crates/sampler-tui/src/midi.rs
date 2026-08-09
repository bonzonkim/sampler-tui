use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};
use sampler_core::{MidiChannel, MidiNote};

pub const MIDI_INGRESS_CAPACITY: usize = 512;
pub const MAX_MIDI_DRAIN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiEvent {
    NoteOn {
        channel: MidiChannel,
        note: MidiNote,
        velocity: u8,
    },
    NoteOff {
        channel: MidiChannel,
        note: MidiNote,
    },
}

pub fn parse_midi_message(message: &[u8]) -> Option<MidiEvent> {
    let &[status, raw_note, velocity] = message else {
        return None;
    };
    if raw_note > 127 || velocity > 127 {
        return None;
    }

    let channel = MidiChannel::new((status & 0x0f) + 1).ok()?;
    let note = MidiNote::new(raw_note).ok()?;
    match status & 0xf0 {
        0x80 => Some(MidiEvent::NoteOff { channel, note }),
        0x90 if velocity == 0 => Some(MidiEvent::NoteOff { channel, note }),
        0x90 => Some(MidiEvent::NoteOn {
            channel,
            note,
            velocity,
        }),
        _ => None,
    }
}

pub struct MidiIngressProducer {
    producer: Producer<MidiEvent>,
    lost: Arc<AtomicUsize>,
}

impl MidiIngressProducer {
    pub fn try_push_message(&mut self, message: &[u8]) {
        let Some(event) = parse_midi_message(message) else {
            return;
        };
        if self.producer.push(event).is_err() {
            increment_lost(&self.lost);
        }
    }
}

pub struct MidiIngressConsumer {
    consumer: Consumer<MidiEvent>,
    lost: Arc<AtomicUsize>,
}

impl MidiIngressConsumer {
    pub fn drain_into(&mut self, output: &mut [MidiEvent]) -> usize {
        let mut drained = 0;
        for slot in output.iter_mut().take(MAX_MIDI_DRAIN) {
            let Ok(event) = self.consumer.pop() else {
                break;
            };
            *slot = event;
            drained += 1;
        }
        drained
    }

    pub fn lost_count(&self) -> usize {
        self.lost.load(Ordering::Relaxed)
    }
}

pub fn midi_ingress() -> (MidiIngressProducer, MidiIngressConsumer) {
    let (producer, consumer) = RingBuffer::new(MIDI_INGRESS_CAPACITY);
    let lost = Arc::new(AtomicUsize::new(0));
    (
        MidiIngressProducer {
            producer,
            lost: Arc::clone(&lost),
        },
        MidiIngressConsumer { consumer, lost },
    )
}

fn increment_lost(lost: &AtomicUsize) {
    let _ = lost.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
        Some(count.saturating_add(1))
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sampler_core::{MidiChannel, MidiNote};

    use super::{
        MAX_MIDI_DRAIN, MIDI_INGRESS_CAPACITY, MidiEvent, increment_lost, midi_ingress,
        parse_midi_message,
    };

    fn note(value: u8) -> MidiNote {
        MidiNote::new(value).unwrap()
    }

    fn channel(value: u8) -> MidiChannel {
        MidiChannel::new(value).unwrap()
    }

    #[test]
    fn parses_note_on_and_off_status_for_all_sixteen_channels() {
        for raw_channel in 0_u8..16 {
            let numbered = channel(raw_channel + 1);
            assert_eq!(
                parse_midi_message(&[0x90 | raw_channel, 0, 127]),
                Some(MidiEvent::NoteOn {
                    channel: numbered,
                    note: note(0),
                    velocity: 127,
                }),
                "Note On channel {}",
                raw_channel + 1
            );
            assert_eq!(
                parse_midi_message(&[0x80 | raw_channel, 127, 64]),
                Some(MidiEvent::NoteOff {
                    channel: numbered,
                    note: note(127),
                }),
                "Note Off channel {}",
                raw_channel + 1
            );
        }
    }

    #[test]
    fn note_on_velocity_zero_is_normalized_to_note_off_and_boundaries_are_exact() {
        assert_eq!(
            parse_midi_message(&[0x90, 0, 1]),
            Some(MidiEvent::NoteOn {
                channel: channel(1),
                note: note(0),
                velocity: 1,
            })
        );
        assert_eq!(
            parse_midi_message(&[0x9f, 127, 127]),
            Some(MidiEvent::NoteOn {
                channel: channel(16),
                note: note(127),
                velocity: 127,
            })
        );
        assert_eq!(
            parse_midi_message(&[0x95, 42, 0]),
            Some(MidiEvent::NoteOff {
                channel: channel(6),
                note: note(42),
            })
        );
    }

    #[test]
    fn rejects_malformed_running_status_system_and_non_note_messages() {
        let rejected: &[&[u8]] = &[
            &[],
            &[0x90],
            &[0x90, 60],
            &[0x90, 60, 100, 0],
            &[60, 100],
            &[0x90, 128, 1],
            &[0x90, 60, 128],
            &[0x80, 60, 128],
            &[0xa0, 60, 100],
            &[0xb0, 1, 127],
            &[0xc0, 1],
            &[0xd0, 1],
            &[0xe0, 0, 64],
            &[0xf0, 1, 0xf7],
            &[0xf1, 0, 0],
            &[0xf8],
            &[0xfe],
            &[0xff],
        ];

        for message in rejected {
            assert_eq!(parse_midi_message(message), None, "message {message:?}");
        }
    }

    #[test]
    fn ingress_is_fifo_with_exact_capacity_and_counts_overflow() {
        let (mut producer, mut consumer) = midi_ingress();
        for index in 0..MIDI_INGRESS_CAPACITY {
            producer.try_push_message(&[0x90, (index % 128) as u8, 100]);
        }
        assert_eq!(consumer.lost_count(), 0);

        producer.try_push_message(&[0x90, 99, 100]);
        assert_eq!(consumer.lost_count(), 1);

        let sentinel = MidiEvent::NoteOff {
            channel: channel(16),
            note: note(127),
        };
        let mut output = [sentinel; MAX_MIDI_DRAIN];
        let drained = consumer.drain_into(&mut output);
        assert_eq!(drained, MAX_MIDI_DRAIN);
        for (index, event) in output.into_iter().enumerate() {
            assert_eq!(
                event,
                MidiEvent::NoteOn {
                    channel: channel(1),
                    note: note(index as u8),
                    velocity: 100,
                }
            );
        }
    }

    #[test]
    fn drain_never_exceeds_one_hundred_twenty_eight_events_per_call() {
        let (mut producer, mut consumer) = midi_ingress();
        for index in 0..200_u8 {
            producer.try_push_message(&[0x90, index % 128, 1]);
        }

        let sentinel = MidiEvent::NoteOff {
            channel: channel(16),
            note: note(127),
        };
        let mut output = [sentinel; 256];
        assert_eq!(consumer.drain_into(&mut output), 128);
        assert_eq!(consumer.drain_into(&mut output), 72);
        assert_eq!(consumer.drain_into(&mut output), 0);
    }

    #[test]
    fn lost_counter_saturates_instead_of_wrapping() {
        let lost = AtomicUsize::new(usize::MAX);
        increment_lost(&lost);
        assert_eq!(lost.load(Ordering::Relaxed), usize::MAX);
    }
}
