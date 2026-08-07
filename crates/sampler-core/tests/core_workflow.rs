use sampler_core::{
    BankId, EventId, Meter, PadId, Pattern, PatternEvent, Resolution, ScheduledEvent, Tempo,
    Transport, VoiceAllocator, VoiceRequest,
};

#[test]
fn trigger_record_schedule_and_allocate_voice() {
    let pad = PadId::new(BankId::new(0).unwrap(), 0).unwrap();
    let transport = Transport::new(
        48_000,
        Tempo::new(120.0).unwrap(),
        Meter::new(4, 4).unwrap(),
        1,
        Resolution::Sixteenth,
    )
    .unwrap();
    let event = PatternEvent::new(EventId(1), pad, 6_800, 0.9, None)
        .unwrap()
        .quantized(&transport, 1.0);
    let mut pattern = Pattern::new(transport.loop_frames());
    pattern.insert(event).unwrap();

    let mut scheduled = [ScheduledEvent::EMPTY; 4];
    assert_eq!(pattern.schedule_range(0, 12_000, &mut scheduled).written, 1);

    let mut voices = VoiceAllocator::<32>::new();
    let allocation = voices.trigger(VoiceRequest::new(
        scheduled[0].pad,
        scheduled[0].at,
        scheduled[0].velocity,
        None,
        false,
    ));
    assert_eq!(allocation.voice.started_at, 6_000);
    assert_eq!(voices.active_voices(), 1);
}
