//! Allocation counting for the unit tests (l4d-10): measures the heap a
//! value keeps against its `retained_bytes()` estimate.
//!
//! Rust allocations are counted by `heap_count`'s allocator. Counts are per
//! thread: a measurement allocates, keeps and frees on its
//! own thread, so other tests running at the same time do not disturb it.
//! Tree-sitter's C allocations are routed through counting wrappers around
//! the C allocator, so parse trees are counted too.

#[cfg(target_os = "linux")]
mod c_side {
    use crate::heap_count::add_thread_live as add;

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
    crate::heap_count::thread_live()
}
