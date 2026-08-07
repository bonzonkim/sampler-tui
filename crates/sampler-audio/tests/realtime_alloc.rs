use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sampler_audio::{
    AudioController, AudioEngine, PadId, PadSettings, SampleBuffer, audio_channels,
    audio_channels_with_test_capacities, write_frames,
};
use sampler_core::{BankId, PlaybackMode};

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
        .install(PadId::first(), sample, settings)
        .unwrap();
    controller.trigger(PadId::first(), 0, 1.0).unwrap();
    (controller, engine)
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
        .install(PadId::first(), first, PadSettings::default())
        .unwrap();
    engine.render_frames(1, |_| {});
    let second_pad = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
    let recovered = Arc::new(SampleBuffer::new(48_000, vec![0.5; 16]).unwrap());
    controller
        .install_recovery(second_pad, recovered, PadSettings::default())
        .unwrap();
    controller.trigger_live(PadId::first(), 0.75).unwrap();
    controller.release_live(PadId::first()).unwrap();

    assert_zero_callback_activity("live and recovery command ingestion", || {
        engine.render_frames(1, |_| {});
    });
    assert_eq!(engine.executed_triggers(), 1);

    let invalid = Arc::new(SampleBuffer::new(44_100, vec![0.25; 16]).unwrap());
    let keepalive = Arc::clone(&invalid);
    controller
        .install_recovery(second_pad, invalid, PadSettings::default())
        .unwrap();
    assert_zero_callback_activity("invalid recovery command ingestion", || {
        engine.render_frames(0, |_| {});
    });
    assert_eq!(controller.reclaim_retired(), 1);
    drop(keepalive);
}

fn measure_invalid_command_handling() {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(48_000, ports).unwrap();
    let invalid_rate = Arc::new(SampleBuffer::new(44_100, vec![0.25; 16]).unwrap());
    let keepalive = Arc::clone(&invalid_rate);
    controller
        .install(PadId::first(), invalid_rate, PadSettings::default())
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
        .install(PadId::first(), sample, PadSettings::default())
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
        .install(PadId::first(), first, PadSettings::default())
        .unwrap();
    engine.render_frames(0, |_| {});
    controller
        .install(
            PadId::first(),
            Arc::new(SampleBuffer::new(48_000, vec![0.5; 16]).unwrap()),
            PadSettings::default(),
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

#[test]
fn callback_scenarios_allocate_and_deallocate_nothing() {
    measure_warmed_loop_render();
    measure_timed_and_immediate_command_ingestion();
    measure_live_and_recovery_command_ingestion();
    measure_invalid_command_handling();
    measure_voice_completion_without_final_arc_drop();
    measure_sample_remap_retirement();
    measure_full_retirement_retry();
    measure_telemetry_full_handling();
    measure_pure_device_write_adapter();
    measure_render_horizon_publication();
}
