use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sampler_tui::{MIDI_INGRESS_CAPACITY, midi_ingress};

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

fn calibrate_counting_allocator() {
    COUNTS.reset_and_enable();
    let allocation = Box::new([0_u8; 64]);
    std::hint::black_box(allocation.as_ptr());
    COUNTS.disable();
    assert!(
        COUNTS.allocations.load(Ordering::Relaxed) > 0,
        "calibration allocation must be observable"
    );

    COUNTS.reset_and_enable();
    drop(allocation);
    COUNTS.disable();
    assert!(
        COUNTS.deallocations.load(Ordering::Relaxed) > 0,
        "calibration deallocation must be observable"
    );
}

#[test]
fn callback_parse_push_and_full_ring_overflow_allocate_and_deallocate_nothing() {
    calibrate_counting_allocator();
    let (mut producer, consumer) = midi_ingress();
    let note_on = [0x90, 60, 100];
    let unsupported = [0xb0, 1, 127];

    COUNTS.reset_and_enable();
    producer.try_push_message(&unsupported);
    for _ in 0..MIDI_INGRESS_CAPACITY {
        producer.try_push_message(&note_on);
    }
    producer.try_push_message(&note_on);
    COUNTS.disable();

    assert_eq!(COUNTS.allocations.load(Ordering::Relaxed), 0);
    assert_eq!(COUNTS.deallocations.load(Ordering::Relaxed), 0);
    assert_eq!(consumer.lost_count(), 1);
}
