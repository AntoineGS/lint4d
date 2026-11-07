//! Allocation counting for the unit tests (l4d-10): measures the heap a
//! value keeps against its `retained_bytes()` estimate.
//!
//! Counts are per thread: a measurement allocates, keeps and frees on its
//! own thread, so other tests running at the same time do not disturb it.
//! Tree-sitter's C allocations are routed through counting wrappers around
//! the C allocator, so parse trees are counted too.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

fn add(bytes: isize) {
    let _ = LIVE.try_with(|live| live.set(live.get() + bytes));
}

struct CountingAllocator;

// SAFETY: every call forwards to `System` with the same arguments.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            add(layout.size() as isize);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            add(layout.size() as isize);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        add(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            add(new_size as isize - layout.size() as isize);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[cfg(target_os = "linux")]
mod c_side {
    use super::add;

    unsafe extern "C" {
        fn malloc_usable_size(pointer: *mut std::ffi::c_void) -> usize;
    }

    fn usable(pointer: *mut std::ffi::c_void) -> isize {
        if pointer.is_null() {
            0
        } else {
            // SAFETY: `pointer` came from the C allocator.
            unsafe { malloc_usable_size(pointer) as isize }
        }
    }

    pub(super) extern "C" fn malloc(size: usize) -> *mut std::ffi::c_void {
        // SAFETY: plain C allocation.
        let pointer = unsafe { libc::malloc(size) };
        add(usable(pointer));
        pointer
    }

    pub(super) extern "C" fn calloc(count: usize, size: usize) -> *mut std::ffi::c_void {
        // SAFETY: plain C allocation.
        let pointer = unsafe { libc::calloc(count, size) };
        add(usable(pointer));
        pointer
    }

    pub(super) extern "C" fn realloc(
        pointer: *mut std::ffi::c_void,
        size: usize,
    ) -> *mut std::ffi::c_void {
        let before = usable(pointer);
        // SAFETY: `pointer` came from the C allocator; the wrappers only
        // ever forward to it, so trees allocated before they were installed
        // are still freed by the allocator that made them.
        let moved = unsafe { libc::realloc(pointer, size) };
        if !moved.is_null() || size == 0 {
            add(usable(moved) - before);
        }
        moved
    }

    pub(super) fn install() {
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| {
            // SAFETY: the wrappers forward to the C allocator tree-sitter
            // uses by default, so memory allocated before and after the
            // switch is freed by the same allocator.
            unsafe {
                tree_sitter::set_allocator(Some(malloc), Some(calloc), Some(realloc), Some(free));
            }
        });
    }

    pub(super) extern "C" fn free(pointer: *mut std::ffi::c_void) {
        add(-usable(pointer));
        // SAFETY: as for `realloc`.
        unsafe { libc::free(pointer) };
    }
}

/// Bytes this thread allocated and has not freed.
pub(crate) fn live_bytes() -> isize {
    #[cfg(target_os = "linux")]
    c_side::install();
    LIVE.with(Cell::get)
}
