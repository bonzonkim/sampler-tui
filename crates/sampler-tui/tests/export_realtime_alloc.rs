use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sampler_audio::{AudioEngine, PatternSwitch, SampleBuffer, audio_channels};
use sampler_core::{
    DelaySettings, EditablePattern, MasterMixSettings, Meter, PadMixSettings, PadSettings,
    PatternEvent, PatternSlotId, PlaybackMode, Resolution, ReverbSettings, Tempo, Transport,
};
use sampler_tui::{EXPORT_CHUNK_FRAMES, EXPORT_SAMPLE_RATE};

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

    fn counts(&self) -> (usize, usize) {
        (
            self.allocations.load(Ordering::Relaxed),
            self.deallocations.load(Ordering::Relaxed),
        )
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

fn calibrate_counting_allocator() {
    COUNTS.reset_and_enable();
    let allocation = Box::new([0_u8; 64]);
    std::hint::black_box(allocation.as_ptr());
    COUNTS.disable();
    assert!(
        COUNTS.counts().0 > 0,
        "calibration allocation was not observed"
    );

    COUNTS.reset_and_enable();
    drop(allocation);
    COUNTS.disable();
    assert!(
        COUNTS.counts().1 > 0,
        "calibration deallocation was not observed"
    );
}

fn prepared_offline_engine() -> (sampler_audio::AudioController, AudioEngine) {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(EXPORT_SAMPLE_RATE, ports).unwrap();
    let mut stereo = Vec::with_capacity(8_192 * 2);
    for index in 0..8_192 {
        let sample = (index % 127) as f32 / 127.0;
        stereo.extend_from_slice(&[sample, -sample]);
    }
    controller
        .install(
            sampler_audio::PadId::first(),
            Arc::new(SampleBuffer::new(EXPORT_SAMPLE_RATE, stereo).unwrap()),
            PadSettings::new(PlaybackMode::Loop, -2.0, 0.25, -3.0, None).unwrap(),
            PadMixSettings::new(false, 0.6, 0.4).unwrap(),
        )
        .unwrap();
    engine.render_frames(0, |_| {});
    controller
        .update_master_mix(
            MasterMixSettings::new(
                -1.0,
                DelaySettings::new(true, 10, 0.5, -3.0).unwrap(),
                ReverbSettings::new(true, 0.75, 0.3, -4.0).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    engine.render_frames(0, |_| {});

    let transport = Transport::new(
        EXPORT_SAMPLE_RATE,
        Tempo::new(240.0).unwrap(),
        Meter::new(1, 4).unwrap(),
        1,
        Resolution::Sixteenth,
    )
    .unwrap();
    let mut pattern =
        EditablePattern::new(PatternSlotId::new(0).unwrap(), "allocation", transport).unwrap();
    pattern
        .insert(
            PatternEvent::new(
                sampler_core::EventId(1),
                sampler_audio::PadId::first(),
                0,
                0.75,
                Some(6_000),
            )
            .unwrap(),
        )
        .unwrap();
    controller
        .install_pattern(Arc::new(pattern.compile().unwrap()))
        .unwrap();
    controller
        .select_pattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate)
        .unwrap();
    controller.play_pattern().unwrap();
    engine.render_frames(0, |_| {});
    (controller, engine)
}

#[test]
fn offline_production_engine_render_loop_allocates_and_deallocates_nothing() {
    calibrate_counting_allocator();
    let (_controller, mut engine) = prepared_offline_engine();
    let mut sink = [[0.0_f32; 2]; EXPORT_CHUNK_FRAMES];
    let mut written = 0;

    COUNTS.reset_and_enable();
    engine.render_frames(EXPORT_CHUNK_FRAMES, |frame| {
        sink[written] = frame;
        written += 1;
    });
    COUNTS.disable();

    assert_eq!(written, EXPORT_CHUNK_FRAMES);
    assert!(sink.iter().any(|frame| *frame != [0.0, 0.0]));
    assert_eq!(COUNTS.counts(), (0, 0));
}
