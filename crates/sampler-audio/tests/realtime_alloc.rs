use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sampler_audio::{
    AudioController, AudioEngine, CaptureBuffer, CaptureOutcome, CaptureSource, CaptureState,
    LiveAck, MAX_CAPTURE_FRAMES, PadId, PadSettings, PatternSwitch, SampleBuffer, audio_channels,
    audio_channels_with_test_capacities, write_frames,
};
use sampler_core::{
    BankId, DelaySettings, EditablePattern, EventId, MasterMixSettings, Meter, PadMixSettings,
    PatternEvent, PatternSlotId, PlaybackMode, Resolution, ReverbSettings, Tempo, Transport,
};

struct CountingAllocator {
    enabled: AtomicBool,
    allocations: AtomicUsize,
    deallocations: AtomicUsize,
}

impl CountingAllocator {
    const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            allocations: AtomicUsize::new(0),
            deallocations: AtomicUsize::new(0),
        }
    }

    fn reset_and_enable(&self) {
        self.allocations.store(0, Ordering::Relaxed);
        self.deallocations.store(0, Ordering::Relaxed);
        self.enabled.store(true, Ordering::Release);
    }

    fn disable(&self) {
        self.enabled.store(false, Ordering::Release);
    }

    fn allocations(&self) -> usize {
        self.allocations.load(Ordering::Relaxed)
    }

    fn deallocations(&self) -> usize {
        self.deallocations.load(Ordering::Relaxed)
    }
}

// SAFETY: Every operation delegates to `System` with the original pointer and layout.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if self.enabled.load(Ordering::Acquire) {
            self.allocations.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: The caller upholds `GlobalAlloc::alloc`'s layout requirements.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if self.enabled.load(Ordering::Acquire) {
            self.deallocations.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: The caller provides the pointer and layout returned by this allocator.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static COUNTS: CountingAllocator = CountingAllocator::new();

fn looping_harness() -> (AudioController, AudioEngine) {
    let (mut controller, ports) = audio_channels();
    let engine = AudioEngine::new(48_000, ports).unwrap();
    let sample = Arc::new(SampleBuffer::new(48_000, vec![0.25; 16]).unwrap());
    let settings = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
    controller
        .install(PadId::first(), sample, settings, PadMixSettings::default())
        .unwrap();
    controller.trigger(PadId::first(), 0, 1.0).unwrap();
    (controller, engine)
}

fn pattern_snapshot(
    slot: u8,
    sample_rate: u32,
    trigger_frames: &[u64],
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
    for (index, frame) in trigger_frames.iter().copied().enumerate() {
        pattern
            .insert(
                PatternEvent::new(EventId(index as u64 + 1), PadId::first(), frame, 1.0, None)
                    .unwrap(),
            )
            .unwrap();
    }
    Arc::new(pattern.compile().unwrap())
}

fn duration_pattern_snapshot(
    slot: u8,
    sample_rate: u32,
    pad: PadId,
    trigger_frame: u64,
    duration: u64,
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
        EditablePattern::new(PatternSlotId::new(slot).unwrap(), "Duration", transport).unwrap();
    pattern
        .insert(PatternEvent::new(EventId(1), pad, trigger_frame, 1.0, Some(duration)).unwrap())
        .unwrap();
    Arc::new(pattern.compile().unwrap())
}

fn assert_zero_callback_activity(name: &str, callback: impl FnOnce()) {
    COUNTS.reset_and_enable();
    callback();
    COUNTS.disable();
    assert_eq!(COUNTS.allocations(), 0, "{name} allocated");
    assert_eq!(COUNTS.deallocations(), 0, "{name} deallocated");
}

fn measure_warmed_loop_render() {
    let (controller, mut engine) = looping_harness();
    let mut output = [0.0_f32; 256];
    engine.render_stereo(&mut output);
    assert_zero_callback_activity("warmed loop render", || {
        for _ in 0..1_000 {
            engine.render_stereo(&mut output);
        }
    });
    drop(controller);
}

fn measure_timed_and_immediate_command_ingestion() {
    let (mut controller, mut engine) = looping_harness();
    let settings = PadSettings::new(PlaybackMode::Loop, -3.0, -0.25, 1.0, None).unwrap();
    engine.render_frames(1, |_| {});
    controller.update_pad(PadId::first(), settings).unwrap();
    controller
        .trigger(PadId::first(), engine.rendered_frame() + 2, 0.75)
        .unwrap();
    controller
        .release(PadId::first(), engine.rendered_frame() + 3)
        .unwrap();
    controller.stop_pad(PadId::first()).unwrap();

    assert_zero_callback_activity("timed and immediate command ingestion", || {
        engine.render_frames(96, |_| {});
    });
    drop(controller);
}

fn measure_live_and_recovery_command_ingestion() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let first = Arc::new(SampleBuffer::new(48_000, vec![0.25; 16]).unwrap());
    controller
        .install(
            PadId::first(),
            first,
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();
    engine.render_frames(1, |_| {});
    let second_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
    let recovered = Arc::new(SampleBuffer::new(48_000, vec![0.5; 16]).unwrap());
    controller
        .install_recovery(
            second_pad,
            recovered,
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();
    let trigger = controller
        .trigger_live_tracked(PadId::first(), 0.75)
        .unwrap();
    controller
        .release_owned_live_tracked(PadId::first(), trigger)
        .unwrap();

    assert_zero_callback_activity(
        "tracked owned-release and recovery command ingestion",
        || {
            engine.render_frames(32, |_| {});
            engine.render_frames(32, |_| {});
            engine.render_frames(1, |_| {});
        },
    );
    assert_eq!(engine.executed_triggers(), 1);

    let invalid = Arc::new(SampleBuffer::new(44_100, vec![0.25; 16]).unwrap());
    let keepalive = Arc::clone(&invalid);
    controller
        .install_recovery(
            second_pad,
            invalid,
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();
    assert_zero_callback_activity("invalid recovery command ingestion", || {
        engine.render_frames(0, |_| {});
    });
    assert_eq!(controller.reclaim_retired(), 1);
    drop(keepalive);
}

fn measure_single_action_store_source_quotas() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let pad = PadId::first();
    controller
        .install(
            pad,
            Arc::new(SampleBuffer::new(48_000, vec![0.25; 2_048]).unwrap()),
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();
    engine.render_frames(1, |_| {});
    for _ in 0..64 {
        controller.trigger(pad, 10_000, 1.0).unwrap();
    }
    engine.render_frames(0, |_| {});
    for _ in 0..64 {
        controller.trigger_live(pad, 1.0).unwrap();
    }

    assert_zero_callback_activity("64 non-live plus 64 live admissions", || {
        engine.render_frames(0, |_| {});
    });
    assert_eq!(engine.pending_actions(), 128);
    assert_zero_callback_activity("single 128-action execution", || {
        engine.render_frames(65, |_| {});
    });
    assert_eq!(engine.executed_triggers(), 64);
    assert_eq!(engine.pending_actions(), 64);
}

fn measure_invalid_command_handling() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let invalid_rate = Arc::new(SampleBuffer::new(44_100, vec![0.25; 16]).unwrap());
    let keepalive = Arc::clone(&invalid_rate);
    controller
        .install(
            PadId::first(),
            invalid_rate,
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();

    assert_zero_callback_activity("invalid command handling", || {
        engine.render_frames(0, |_| {});
    });
    assert_eq!(engine.invalid_commands(), 1);
    assert_eq!(controller.reclaim_retired(), 1);
    drop(keepalive);
}

fn measure_voice_completion_without_final_arc_drop() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let sample = Arc::new(SampleBuffer::new(48_000, vec![0.25; 2]).unwrap());
    let keepalive = Arc::clone(&sample);
    controller
        .install(
            PadId::first(),
            sample,
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();
    controller.trigger(PadId::first(), 0, 1.0).unwrap();

    assert_zero_callback_activity("voice completion", || {
        engine.render_frames(2, |_| {});
    });
    assert_eq!(engine.active_voices(), 0);
    drop(controller);
    drop(keepalive);
}

fn measure_sample_remap_retirement() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let first = Arc::new(SampleBuffer::new(48_000, vec![0.25; 16]).unwrap());
    let first_keepalive = Arc::clone(&first);
    controller
        .install(
            PadId::first(),
            first,
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();
    engine.render_frames(0, |_| {});
    controller
        .install(
            PadId::first(),
            Arc::new(SampleBuffer::new(48_000, vec![0.5; 16]).unwrap()),
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();

    assert_zero_callback_activity("sample remap retirement", || {
        engine.render_frames(0, |_| {});
    });
    assert_eq!(controller.reclaim_retired(), 1);
    drop(first_keepalive);
}

fn measure_full_retirement_retry() {
    let (mut controller, ports) = audio_channels_with_test_capacities(16, 1, 1);
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    for value in [0.25, 0.5, 0.75] {
        controller
            .install(
                PadId::first(),
                Arc::new(SampleBuffer::new(48_000, vec![value; 16]).unwrap()),
                PadSettings::default(),
                PadMixSettings::default(),
            )
            .unwrap();
        engine.render_frames(0, |_| {});
    }

    assert_zero_callback_activity("full retirement retention", || {
        engine.render_frames(0, |_| {});
    });
    assert_eq!(controller.reclaim_retired(), 1);
    assert_zero_callback_activity("retirement retry", || {
        engine.render_frames(0, |_| {});
    });
    assert_eq!(controller.reclaim_retired(), 1);
}

fn measure_telemetry_full_handling() {
    let (_controller, ports) = audio_channels_with_test_capacities(8, 8, 1);
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    engine.render_frames(1_600, |_| {});

    assert_zero_callback_activity("telemetry full handling", || {
        engine.render_frames(1_600, |_| {});
    });
}

fn measure_pure_device_write_adapter() {
    let frames = [[0.25, -0.25]; 64];
    let mut output = [0_i16; 128];
    write_frames(&frames, 2, &mut output).unwrap();

    assert_zero_callback_activity("pure device write adapter", || {
        for _ in 0..1_000 {
            write_frames(&frames, 2, &mut output).unwrap();
        }
    });
}

fn measure_render_horizon_publication() {
    let (controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    assert_eq!(controller.render_horizon(), 0);

    COUNTS.reset_and_enable();
    engine.render_frames(128, |_| {});
    COUNTS.disable();

    assert_eq!(controller.render_horizon(), 128);
    assert_eq!((COUNTS.allocations(), COUNTS.deallocations()), (0, 0));
}

fn measure_pattern_playback_acknowledgement_and_retirement() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(100, ports).unwrap();
    controller
        .install(
            PadId::first(),
            Arc::new(SampleBuffer::new(100, vec![0.5; 128]).unwrap()),
            PadSettings::default(),
            PadMixSettings::default(),
        )
        .unwrap();
    engine.render_frames(0, |_| {});

    let slot_zero = PatternSlotId::new(0).unwrap();
    let slot_one = PatternSlotId::new(1).unwrap();
    let initial = pattern_snapshot(0, 100, &[2, 8]);
    let replacement = pattern_snapshot(0, 100, &[1, 7, 9]);
    let boundary_target = pattern_snapshot(1, 100, &[0, 6]);
    let initial_generation = initial.generation();
    let boundary_generation = boundary_target.generation();
    let initial_owner = controller.install_pattern(initial).unwrap();
    controller.install_pattern(boundary_target).unwrap();
    controller
        .select_pattern(slot_zero, PatternSwitch::Immediate)
        .unwrap();
    controller.play_pattern().unwrap();
    controller
        .set_record_capture(Some((slot_zero, initial_generation)))
        .unwrap();
    controller
        .trigger_live_tracked(PadId::first(), 0.75)
        .unwrap();

    assert_zero_callback_activity(
        "pattern playback, ack, switch, stop, and retirement",
        || {
            engine.render_frames(75, |_| {});
            controller.install_pattern(replacement).unwrap();
            controller
                .select_pattern(slot_one, PatternSwitch::NextBoundary)
                .unwrap();
            controller.release_live_tracked(PadId::first()).unwrap();
            engine.render_frames(25, |_| {});
            controller
                .set_record_capture(Some((slot_one, boundary_generation)))
                .unwrap();
            engine.render_frames(103, |_| {});
            controller.stop_pattern().unwrap();
            engine.render_frames(128, |_| {});
        },
    );

    let mut acks = [LiveAck::EMPTY; 2];
    // The release now executes immediately while replacement invalidates the old recording
    // capture; only the earlier trigger belongs to a valid capture generation.
    assert_eq!(controller.drain_live_acks(&mut acks), 1);
    assert_eq!(controller.reclaim_retired_pattern(), Some(initial_owner));
}

fn measure_exact_duration_pattern_releases() {
    for mode in [PlaybackMode::Gate, PlaybackMode::Loop] {
        let (mut controller, ports) = audio_channels();
        let mut engine = AudioEngine::new(1_000, ports).unwrap();
        let pad = PadId::first();
        let sample = Arc::new(SampleBuffer::new(1_000, vec![0.5; 256]).unwrap());
        let settings = PadSettings::new(mode, 0.0, 0.0, 0.0, None).unwrap();
        controller
            .install(pad, sample, settings, PadMixSettings::default())
            .unwrap();
        engine.render_frames(0, |_| {});
        controller
            .install_pattern(duration_pattern_snapshot(0, 1_000, pad, 2, 3))
            .unwrap();
        controller
            .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
            .unwrap();
        controller.play_pattern().unwrap();

        assert_zero_callback_activity("exact duration pattern release", || {
            engine.render_frames(69, |_| {});
        });

        assert_eq!(engine.executed_triggers(), 1);
        assert_eq!(engine.active_voices(), 0);
    }
}

fn measure_bounded_capture_ownership() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(100, ports).unwrap();
    let pattern_pad = PadId::first();
    let live_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
    let looping = PadSettings::new(PlaybackMode::Loop, 0.0, 0.0, 0.0, None).unwrap();
    controller
        .install(
            pattern_pad,
            Arc::new(SampleBuffer::new(100, vec![0.5; 256]).unwrap()),
            looping,
            PadMixSettings::default(),
        )
        .unwrap();
    controller
        .install(
            live_pad,
            Arc::new(SampleBuffer::new(100, vec![0.25; 256]).unwrap()),
            looping,
            PadMixSettings::default(),
        )
        .unwrap();
    engine.render_frames(0, |_| {});
    controller
        .install_pattern(pattern_snapshot(0, 100, &[2]))
        .unwrap();
    controller
        .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
        .unwrap();
    controller.play_pattern().unwrap();
    controller.trigger_live(live_pad, 1.0).unwrap();
    engine.render_frames(75, |_| {});
    assert!(
        engine.active_voices() >= 2,
        "active={}, triggers={}",
        engine.active_voices(),
        engine.executed_triggers()
    );

    let saturation =
        CaptureBuffer::try_new(700, live_pad, CaptureSource::Resample, 100, 1).unwrap();
    let saturation_allocation = saturation.stereo().as_ptr();
    let first = CaptureBuffer::try_new(
        701,
        live_pad,
        CaptureSource::Resample,
        100,
        MAX_CAPTURE_FRAMES,
    )
    .unwrap();
    let first_allocation = first.stereo().as_ptr();
    let second = CaptureBuffer::try_new(
        702,
        live_pad,
        CaptureSource::Resample,
        100,
        MAX_CAPTURE_FRAMES,
    )
    .unwrap();
    let second_allocation = second.stereo().as_ptr();
    controller.arm_capture(saturation).unwrap();
    engine.render_frames(0, |_| {});
    controller.start_capture(700).unwrap();
    engine.render_frames(0, |_| {});

    let mut saturated_outcome = None;
    let mut stopped_outcome = None;
    let mut cancelled_outcome = None;
    let mut rendered = [[0.0_f32; 2]; 16];
    let mut rendered_len = 0;
    assert_zero_callback_activity("bounded capture ownership", || {
        engine.render_frames(1, |_| {});

        controller.arm_capture(first).unwrap();
        engine.render_frames(0, |_| {});
        controller.start_capture(701).unwrap();
        engine.render_frames(16, |frame| {
            rendered[rendered_len] = frame;
            rendered_len += 1;
        });
        controller.stop_capture(701).unwrap();
        engine.render_frames(0, |_| {});
        assert_eq!(
            controller.capture_status().unwrap().state,
            CaptureState::CompletionPending
        );

        saturated_outcome = controller.try_capture_completion();
        engine.render_frames(0, |_| {});
        stopped_outcome = controller.try_capture_completion();

        controller.arm_capture(second).unwrap();
        engine.render_frames(0, |_| {});
        controller.start_capture(702).unwrap();
        engine.render_frames(1, |_| {});
        controller.cancel_capture(702).unwrap();
        engine.render_frames(1, |_| {});
        cancelled_outcome = controller.try_capture_completion();
    });

    let Some(CaptureOutcome::Completed(saturated)) = saturated_outcome else {
        panic!("hard-limit outcome must saturate the completion ring");
    };
    assert_eq!(saturated.token, 700);
    assert_eq!(saturated.stereo.as_ptr(), saturation_allocation);
    assert_eq!(saturated.stereo.len(), 2);
    assert!(saturated.hard_limit);

    let Some(CaptureOutcome::Completed(stopped)) = stopped_outcome else {
        panic!("stopped capture must be reclaimed after backpressure");
    };
    assert_eq!(stopped.token, 701);
    assert_eq!(stopped.stereo.as_ptr(), first_allocation);
    assert_eq!(stopped.stereo.len(), 32);
    assert!(!stopped.hard_limit);
    assert_eq!(rendered_len, rendered.len());
    for (captured, expected) in stopped.stereo.chunks_exact(2).zip(rendered) {
        assert_eq!(captured, expected);
    }

    let Some(CaptureOutcome::Cancelled(cancelled)) = cancelled_outcome else {
        panic!("cancelled capture must return its original buffer");
    };
    assert_eq!(cancelled.token(), 702);
    assert_eq!(cancelled.stereo().as_ptr(), second_allocation);
    assert_eq!(cancelled.stereo().len(), 2);
}

fn measure_fx_commands_telemetry_and_capture_completion() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(100, ports).unwrap();
    let pad = PadId::first();
    let looping = PadSettings::new(PlaybackMode::Loop, 0.0, -1.0, 0.0, None).unwrap();
    controller
        .install(
            pad,
            Arc::new(SampleBuffer::new(100, vec![0.5; 512]).unwrap()),
            looping,
            PadMixSettings::new(false, 0.5, 0.5).unwrap(),
        )
        .unwrap();
    controller
        .update_master_mix(
            MasterMixSettings::new(
                0.0,
                DelaySettings::new(true, 20, 0.25, -6.0).unwrap(),
                ReverbSettings::new(true, 0.5, 0.25, -6.0).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    engine.render_frames(16, |_| {});

    let capture = CaptureBuffer::try_new(800, pad, CaptureSource::Resample, 100, 32).unwrap();
    let capture_allocation = capture.stereo().as_ptr();
    controller.arm_capture(capture).unwrap();
    engine.render_frames(0, |_| {});
    controller.start_capture(800).unwrap();
    controller
        .update_pad(
            pad,
            PadSettings::new(PlaybackMode::Loop, -3.0, -0.5, 0.0, None).unwrap(),
        )
        .unwrap();
    controller
        .update_pad_mix(pad, PadMixSettings::new(false, 1.0, 0.75).unwrap())
        .unwrap();
    controller
        .update_master_mix(
            MasterMixSettings::new(
                -1.0,
                DelaySettings::new(true, 10, 0.5, -3.0).unwrap(),
                ReverbSettings::new(true, 0.8, 0.4, -3.0).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    controller
        .trigger(pad, engine.rendered_frame(), 1.0)
        .unwrap();
    assert!(controller.latest_telemetry().is_some());

    assert_zero_callback_activity("fx commands, telemetry, and capture completion", || {
        engine.render_frames(64, |_| {});
    });

    assert!(engine.active_voices() > 0);
    assert!(controller.latest_telemetry().is_some());
    let status = controller
        .capture_status()
        .expect("measured callback must publish capture progress");
    assert_eq!(status.state, CaptureState::Idle);
    assert_eq!(status.frames, 32);
    assert!(status.peak > 0.0);
    assert!(status.hard_limit);
    let Some(CaptureOutcome::Completed(completion)) = controller.try_capture_completion() else {
        panic!("measured callback must publish capture completion")
    };
    assert_eq!(completion.stereo.as_ptr(), capture_allocation);
    assert_eq!(completion.stereo.len(), 64);
    assert!(completion.hard_limit);
}

#[test]
fn callback_scenarios_allocate_and_deallocate_nothing() {
    measure_warmed_loop_render();
    measure_timed_and_immediate_command_ingestion();
    measure_live_and_recovery_command_ingestion();
    measure_single_action_store_source_quotas();
    measure_invalid_command_handling();
    measure_voice_completion_without_final_arc_drop();
    measure_sample_remap_retirement();
    measure_full_retirement_retry();
    measure_telemetry_full_handling();
    measure_pure_device_write_adapter();
    measure_render_horizon_publication();
    measure_pattern_playback_acknowledgement_and_retirement();
    measure_exact_duration_pattern_releases();
    measure_bounded_capture_ownership();
    measure_fx_commands_telemetry_and_capture_completion();
}
