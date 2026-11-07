//! A counting allocator for the test binary's report-only memory
//! measurements (13d-2). Counting is off unless a measurement turns it on,
//! so other tests pay one relaxed load per allocation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    /// Bytes this thread freed while counting.
    static FREED: Cell<usize> = const { Cell::new(0) };
}

fn grow(bytes: usize) {
    if COUNTING.load(Ordering::Relaxed) {
        let live = LIVE.fetch_add(bytes as isize, Ordering::Relaxed) + bytes as isize;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

fn shrink(bytes: usize) {
    if COUNTING.load(Ordering::Relaxed) {
        LIVE.fetch_sub(bytes as isize, Ordering::Relaxed);
        let _ = FREED.try_with(|freed| freed.set(freed.get() + bytes));
    }
}

// SAFETY: every call is delegated to `System` with its original layout; the
// counters do not affect pointers or ownership.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grow(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        grow(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        shrink(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        grow(size);
        shrink(layout.size());
        unsafe { System.realloc(ptr, layout, size) }
    }
}

/// Starts counting (once per process: bytes freed before are not seen).
pub(crate) fn start() {
    COUNTING.store(true, Ordering::SeqCst);
}

/// Bytes allocated and not freed since `start`.
pub(crate) fn live() -> isize {
    LIVE.load(Ordering::SeqCst)
}

/// The highest `live` since the last `reset_peak`.
pub(crate) fn peak() -> isize {
    PEAK.load(Ordering::SeqCst)
}

/// Sets the peak to the current live bytes.
pub(crate) fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// Whether a measurement turned counting on.
pub(crate) fn counting() -> bool {
    COUNTING.load(Ordering::Relaxed)
}

/// The bytes this thread frees running `release`: what a value it drops
/// held alone, whatever other threads allocate meanwhile.
pub(crate) fn freed_during(release: impl FnOnce()) -> usize {
    let before = FREED.with(Cell::get);
    release();
    FREED.with(Cell::get) - before
}
