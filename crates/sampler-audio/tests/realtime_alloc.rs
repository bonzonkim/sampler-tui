use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sampler_audio::{
    AudioController, AudioEngine, PadId, PadSettings, SampleBuffer, audio_channels,
};
use sampler_core::PlaybackMode;

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

#[test]
fn warmed_render_path_allocates_and_deallocates_nothing() {
    let (controller, mut engine) = looping_harness();
    let mut output = [0.0_f32; 256];
    engine.render_stereo(&mut output);
    COUNTS.reset_and_enable();
    for _ in 0..1_000 {
        engine.render_stereo(&mut output);
    }
    COUNTS.disable();
    assert_eq!(COUNTS.allocations(), 0);
    assert_eq!(COUNTS.deallocations(), 0);
    drop(controller);
}
