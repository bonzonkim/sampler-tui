use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sampler_audio::{
    CaptureBuffer, CaptureOutcome, CaptureSource, PadId, capture_channels, write_input_device,
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

fn buffer(token: u64, max_frames: usize) -> CaptureBuffer {
    CaptureBuffer::try_new(
        token,
        PadId::first(),
        CaptureSource::Input,
        48_000,
        max_frames,
    )
    .unwrap()
}

#[test]
fn input_callback_lifecycle_allocates_and_deallocates_nothing() {
    let (mut controller, mut core) = capture_channels(8, 1);
    let saturation = buffer(1, 1);
    let stopped = buffer(2, 16);
    let cancelled = buffer(3, 16);
    let saturation_allocation = saturation.stereo().as_ptr();
    let stopped_allocation = stopped.stereo().as_ptr();
    let cancelled_allocation = cancelled.stereo().as_ptr();
    let input = [0.25_f32, -0.5, 99.0, 98.0];
    COUNTS.reset_and_enable();

    controller.arm(saturation).unwrap();
    write_input_device(&mut core, 4, &input).unwrap();
    controller.start(1).unwrap();
    write_input_device(&mut core, 4, &input).unwrap();

    controller.arm(stopped).unwrap();
    write_input_device(&mut core, 4, &input).unwrap();
    controller.start(2).unwrap();
    write_input_device(&mut core, 4, &input).unwrap();
    controller.stop(2).unwrap();
    write_input_device(&mut core, 4, &input).unwrap();

    let saturation_outcome = controller.try_next_outcome();
    write_input_device(&mut core, 4, &input).unwrap();
    let stopped_outcome = controller.try_next_outcome();

    controller.arm(cancelled).unwrap();
    write_input_device(&mut core, 4, &input).unwrap();
    controller.start(3).unwrap();
    write_input_device(&mut core, 4, &input).unwrap();
    controller.cancel(3).unwrap();
    write_input_device(&mut core, 4, &input).unwrap();
    let cancelled_outcome = controller.try_next_outcome();

    COUNTS.disable();

    assert_eq!(COUNTS.allocations.load(Ordering::Relaxed), 0);
    assert_eq!(COUNTS.deallocations.load(Ordering::Relaxed), 0);

    let Some(CaptureOutcome::Completed(saturation)) = saturation_outcome else {
        panic!("hard-limit capture must fill the completion channel");
    };
    assert_eq!(saturation.stereo.as_ptr(), saturation_allocation);
    assert_eq!(saturation.stereo, [0.25, -0.5]);
    assert!(saturation.hard_limit);

    let Some(CaptureOutcome::Completed(stopped)) = stopped_outcome else {
        panic!("stopped capture must drain after completion backpressure");
    };
    assert_eq!(stopped.stereo.as_ptr(), stopped_allocation);
    assert_eq!(stopped.stereo, [0.25, -0.5]);
    assert!(!stopped.hard_limit);

    let Some(CaptureOutcome::Cancelled(cancelled)) = cancelled_outcome else {
        panic!("cancel must return its original buffer");
    };
    assert_eq!(cancelled.stereo().as_ptr(), cancelled_allocation);
    assert_eq!(cancelled.stereo(), [0.25, -0.5]);
}
