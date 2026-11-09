//! Utility library that provides a `GlobalAlloc` wrapper tracking bytes allocated on the
//! calling thread

#![doc = include_str!("../ABOUT.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/boa-dev/boa/main/assets/logo_black.svg",
    html_favicon_url = "https://raw.githubusercontent.com/boa-dev/boa/main/assets/logo_black.svg"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}

/// Bytes currently allocated on the calling thread through a [`TrackingAllocator`].
///
/// Returns `0` if no [`TrackingAllocator`] was ever installed as the process's
/// `#[global_allocator]`, or if nothing has been allocated on the calling thread yet.
#[must_use]
pub fn allocated_bytes() -> usize {
    ALLOCATED.with(Cell::get)
}

/// A [`GlobalAlloc`] wrapper that tracks the number of bytes currently allocated on the calling
/// thread, without ever refusing an allocation.
///
/// Install it as the process's `#[global_allocator]` to make [`allocated_bytes`] report
/// accurate, real-time, per-thread memory usage:
///
/// ```
/// use std::alloc::System;
/// use track_alloc::TrackingAllocator;
///
/// #[global_allocator]
/// static ALLOCATOR: TrackingAllocator<System> = TrackingAllocator(System);
/// ```
///
/// # Why per-thread, and why never refuse?
///
/// Every allocation and deallocation updates a counter local to the thread that performs it, so
/// the count for one thread is unaffected by what other threads are doing concurrently. This
/// fits an embedder that pins one unit of work (e.g. a [`boa_engine::Context`]) to a single OS
/// thread for its whole lifetime, which is already a requirement for anything built on
/// [`boa_gc`], since `Context` is not [`Send`].
///
/// Unlike allocators that enforce a hard limit (e.g. by returning a null pointer once a budget
/// is exceeded), this allocator always delegates to the wrapped allocator and always succeeds
/// when the wrapped allocator does. Rust's standard collections treat a null allocation as fatal
/// and abort the whole process, so an allocator that refuses is unsafe to use for cooperative,
/// per-execution limits in a process that is also running unrelated work. Enforcing a budget is
/// left to the caller, who should compare [`allocated_bytes`] against their own limit at a point
/// where it is safe to unwind (such as between bytecode instructions).
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackingAllocator<A = System>(pub A);

// SAFETY: `TrackingAllocator` forwards every operation to the wrapped allocator `A`, which
// must itself be a valid `GlobalAlloc` implementation. The bookkeeping done around each call
// only inspects the `Layout`/return value and never affects allocator behavior.
unsafe impl<A: GlobalAlloc> GlobalAlloc for TrackingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.0.alloc(layout) };
        if !ptr.is_null() {
            ALLOCATED.with(|allocated| allocated.set(allocated.get() + layout.size()));
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) };
        ALLOCATED.with(|allocated| allocated.set(allocated.get().saturating_sub(layout.size())));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            ALLOCATED.with(|allocated| {
                let current = allocated.get().saturating_sub(layout.size());
                allocated.set(current + new_size);
            });
        }
        new_ptr
    }
}
