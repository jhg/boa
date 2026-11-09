//! `track_alloc` - a `GlobalAlloc` wrapper that tracks memory usage per thread, per task, or
//! for the whole process, without ever refusing an allocation.

#![doc = include_str!("../README.md")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod future;
pub use future::{Tracked, track};

thread_local! {
    // Every thread's own counter, used whenever no `Scope` has been entered on that thread.
    static PER_THREAD_DEFAULT: AtomicUsize = const { AtomicUsize::new(0) };

    // Points at whichever counter should receive this thread's accounting right now: either
    // null (meaning "not resolved yet - use `PER_THREAD_DEFAULT`", see `current_counter`), or a
    // `Scope`'s target while one is entered on this thread.
    //
    // This starts out null, rather than being eagerly set to `&PER_THREAD_DEFAULT` here, so
    // that initializing this thread-local never needs to call `.with()` on another one: both
    // initializers are then plain `const` expressions that need no lazy, heap-allocated storage
    // at all, which matters because that lazy initialization would itself be serviced by this
    // same allocator - calling back into code that is, at that very moment, still in the middle
    // of setting up the thread-local state it depends on.
    static CURRENT: Cell<*const AtomicUsize> = const { Cell::new(std::ptr::null()) };
}

// The process-wide counter, updated by every `TrackingAllocator<_, true>` allocation
// regardless of which thread performs it or which `Scope` is active there.
static GLOBAL: AtomicUsize = AtomicUsize::new(0);

/// Bytes currently live in whatever is "current" on the calling thread: a [`Scope`] that was
/// [`Scope::enter`]ed and not yet dropped, or else that thread's own default counter.
///
/// # A freshly created thread does not start at `0`
///
/// The standard library's own thread-spawning machinery allocates and frees some bookkeeping
/// of its own across the parent/child boundary, so a brand new thread's default counter can
/// already be a few dozen bytes away from `0` before any of your own code runs on it - in
/// either direction, including having already wrapped around `usize`'s range if a lot of
/// threads have been spawned.
///
/// Because of this, always compare two readings with [`usize::wrapping_sub`] (`after
/// .wrapping_sub(before)`), never plain subtraction: it computes the correct delta via modular
/// arithmetic no matter where in `usize`'s range either reading happens to land, including
/// right after a wraparound; plain subtraction can panic (in debug builds) or silently give a
/// nonsensical result otherwise. Don't rely on a fresh thread's counter being `0` or "small".
#[must_use]
pub fn allocated_bytes() -> usize {
    current_counter().load(Ordering::Relaxed)
}

/// Bytes currently allocated process-wide, across every thread.
///
/// Only accurate if a [`TrackingAllocator`] with `TRACK_GLOBAL = true` is installed as the
/// process's `#[global_allocator]`; otherwise this always reads `0`.
///
/// Unlike [`allocated_bytes`], this one genuinely starts at `0` and only grows or shrinks with
/// real (de)allocations - there's no per-thread bookkeeping to skew it. Comparing two readings
/// with [`usize::wrapping_sub`] rather than plain subtraction is still good practice, in the
/// unlikely case a long enough run wraps `usize`'s range.
#[must_use]
pub fn global_allocated_bytes() -> usize {
    GLOBAL.load(Ordering::Relaxed)
}

#[inline]
fn current_counter() -> &'static AtomicUsize {
    let ptr = CURRENT.with(Cell::get);
    let ptr = if ptr.is_null() {
        // First use of `CURRENT` on this thread: resolve and cache a pointer to this thread's
        // own `PER_THREAD_DEFAULT`. This happens here, in an ordinary function call, rather
        // than inside `CURRENT`'s own thread-local initializer - see the comment there.
        let default = PER_THREAD_DEFAULT.with(|counter| -> *const AtomicUsize { counter });
        CURRENT.with(|current| current.set(default));
        default
    } else {
        ptr
    };
    // SAFETY: `ptr` always points either at a thread's own `'static`-for-its-lifetime
    // `PER_THREAD_DEFAULT` storage, or at the `AtomicUsize` behind a `Scope`'s `Arc`, which the
    // `Scope` guard that installed it keeps alive (and restores the previous pointer before
    // dropping) for as long as it remains "current" on this thread. Either way the pointee
    // outlives every use made of it here.
    unsafe { &*ptr }
}

#[inline]
fn record_grow(bytes: usize, track_global: bool) {
    if track_global {
        GLOBAL.fetch_add(bytes, Ordering::Relaxed);
    }
    current_counter().fetch_add(bytes, Ordering::Relaxed);
}

#[inline]
fn record_shrink(bytes: usize, track_global: bool) {
    if track_global {
        GLOBAL.fetch_sub(bytes, Ordering::Relaxed);
    }
    current_counter().fetch_sub(bytes, Ordering::Relaxed);
}

/// An RAII guard that redirects the calling thread's accounting to `target` for as long as it
/// is held, restoring whatever was current before on drop.
///
/// Entering a [`Scope`] on top of another one nests correctly: the innermost active `Scope`
/// receives the accounting, and each one restores exactly what was current before it, in
/// reverse order, as they're dropped.
#[derive(Debug)]
#[must_use = "a `Scope` only has an effect while it is held; dropping it immediately undoes it"]
pub struct Scope {
    previous: *const AtomicUsize,
    // Keeps `target`'s allocation alive for as long as `previous` might point at it; dropped
    // only after `Drop::drop` below has already restored `previous` as current.
    _keep_alive: Arc<AtomicUsize>,
}

impl Scope {
    /// Redirects the calling thread's accounting to `target` until the returned [`Scope`] is
    /// dropped.
    pub fn enter(target: &Arc<AtomicUsize>) -> Self {
        let previous = CURRENT.with(Cell::get);
        CURRENT.with(|current| current.set(Arc::as_ptr(target)));
        Self {
            previous,
            _keep_alive: Arc::clone(target),
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.previous));
    }
}

/// A [`GlobalAlloc`] wrapper that tracks memory usage without ever refusing an allocation.
///
/// Install it as the process's `#[global_allocator]`:
///
/// ```
/// use std::alloc::System;
/// use track_alloc::TrackingAllocator;
///
/// #[global_allocator]
/// static ALLOCATOR: TrackingAllocator<System> = TrackingAllocator(System);
/// ```
///
/// Every allocation and deallocation updates whichever counter [`allocated_bytes`] would read
/// at that moment on that thread (see [`Scope`]). Set `TRACK_GLOBAL` to `true` to also maintain
/// a process-wide total, readable via [`global_allocated_bytes`]; it defaults to `false` so
/// that installing this allocator costs nothing extra unless you ask for it.
///
/// # Why never refuse?
///
/// Rust's standard collections treat a null allocation as fatal and abort the whole process, so
/// an allocator that refuses (as many "memory limit" allocators do) is unsafe to use for
/// cooperative, per-scope limits in a process that is also running unrelated work: one
/// over-budget scope would take the whole process down with it. This allocator always succeeds
/// when the wrapped allocator does; enforcing a budget is left to the caller, who should compare
/// [`allocated_bytes`]/[`global_allocated_bytes`] against their own limit at a point where it is
/// safe to unwind.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackingAllocator<A = System, const TRACK_GLOBAL: bool = false>(pub A);

// SAFETY: `TrackingAllocator` forwards every operation to the wrapped allocator `A`, which must
// itself be a valid `GlobalAlloc` implementation. The bookkeeping done around each call only
// inspects the `Layout`/return value and never affects allocator behavior.
unsafe impl<A: GlobalAlloc, const TRACK_GLOBAL: bool> GlobalAlloc
    for TrackingAllocator<A, TRACK_GLOBAL>
{
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { self.0.alloc(layout) };
        if !ptr.is_null() {
            record_grow(layout.size(), TRACK_GLOBAL);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) };
        record_shrink(layout.size(), TRACK_GLOBAL);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            if new_size >= layout.size() {
                record_grow(new_size - layout.size(), TRACK_GLOBAL);
            } else {
                record_shrink(layout.size() - new_size, TRACK_GLOBAL);
            }
        }
        new_ptr
    }
}

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: TrackingAllocator<System, true> = TrackingAllocator(System);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_allocations_on_this_thread() {
        // `wrapping_sub` rather than plain `-`: this is the very first thing this test's own
        // (freshly spawned by the test harness) thread does, and a freshly spawned thread's
        // counter is not guaranteed to be a "small" number - see the comment on
        // `per_thread_default_is_isolated_across_real_threads`. `wrapping_sub` computes the
        // correct delta via modular arithmetic regardless of where `baseline` happens to sit in
        // `usize`'s range, including right after it wrapped around.
        let baseline = allocated_bytes();
        let v: Vec<u8> = Vec::with_capacity(4096);
        assert!(allocated_bytes().wrapping_sub(baseline) >= 4096);
        drop(v);
        // Not necessarily back to exactly `baseline` (other allocations may have happened
        // concurrently on this thread, e.g. inside the test harness), but it must have shrunk
        // back down by roughly what we freed.
        assert!(allocated_bytes().wrapping_sub(baseline) < 4096);
    }

    #[test]
    fn global_counter_aggregates_across_real_threads() {
        // Two barriers make the sequence deterministic: by the time the main thread's `ready`
        // wait returns, both workers have already allocated (every party, including this
        // thread, must arrive at a `Barrier` before any of them proceeds past it); and neither
        // worker can drop its allocation until the main thread also reaches `release`, which it
        // only does after reading the counter. So the read below is guaranteed to observe both
        // allocations alive at once, not a sequential, possibly-already-freed snapshot.
        let ready = Arc::new(std::sync::Barrier::new(3));
        let release = Arc::new(std::sync::Barrier::new(3));
        let global_before = global_allocated_bytes();

        let spawn_holding =
            |size: usize, ready: Arc<std::sync::Barrier>, release: Arc<std::sync::Barrier>| {
                std::thread::spawn(move || {
                    let v: Vec<u8> = Vec::with_capacity(size);
                    ready.wait();
                    release.wait();
                    std::hint::black_box(&v);
                })
            };

        let t1 = spawn_holding(10_000, ready.clone(), release.clone());
        let t2 = spawn_holding(20_000, ready.clone(), release.clone());

        ready.wait();
        let peak = global_allocated_bytes();
        release.wait();

        t1.join().unwrap();
        t2.join().unwrap();

        assert!(peak >= global_before + 30_000);
    }

    #[test]
    fn per_thread_default_is_isolated_across_real_threads() {
        // Note: a freshly spawned thread's own default counter is *not* guaranteed to read
        // exactly `0` - `std::thread::spawn` itself allocates and frees some bookkeeping across
        // the parent/child boundary (confirmed by tracing `GlobalAlloc` calls directly: even an
        // empty closure leaves a small, non-zero skew on the child). This is a property of the
        // standard library's own thread-spawning machinery, not of this crate, and it's why
        // `allocated_bytes`/`global_allocated_bytes` are documented as "track a delta on the
        // same thread", not "starts at zero". Each thread below takes its own baseline first,
        // and `wrapping_sub` (not plain `-`) computes the delta correctly by modular arithmetic
        // even if that baseline already sits right after a wraparound.
        let t1 = std::thread::spawn(|| {
            let baseline = allocated_bytes();
            let v: Vec<u8> = Vec::with_capacity(1000);
            let during = allocated_bytes();
            drop(v);
            during.wrapping_sub(baseline)
        });
        let t2 = std::thread::spawn(|| {
            let baseline = allocated_bytes();
            let v: Vec<u8> = Vec::with_capacity(2000);
            let during = allocated_bytes();
            drop(v);
            during.wrapping_sub(baseline)
        });

        let grew_by_1 = t1.join().unwrap();
        let grew_by_2 = t2.join().unwrap();
        assert!(grew_by_1 >= 1000);
        assert!(grew_by_2 >= 2000);
    }

    #[test]
    fn scope_nests_and_restores_in_order() {
        let outer = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(AtomicUsize::new(0));
        let baseline_default = allocated_bytes();

        {
            let _outer_scope = Scope::enter(&outer);
            let v1: Vec<u8> = Vec::with_capacity(100);
            assert_eq!(outer.load(Ordering::Relaxed), 100);
            {
                let _inner_scope = Scope::enter(&inner);
                let v2: Vec<u8> = Vec::with_capacity(50);
                assert_eq!(inner.load(Ordering::Relaxed), 50);
                assert_eq!(
                    outer.load(Ordering::Relaxed),
                    100,
                    "outer untouched while nested"
                );
                drop(v2);
                assert_eq!(inner.load(Ordering::Relaxed), 0);
            }
            // Back to `outer` now that the inner scope dropped.
            let v3: Vec<u8> = Vec::with_capacity(25);
            assert_eq!(outer.load(Ordering::Relaxed), 125);
            drop(v1);
            drop(v3);
        }
        // Back to this thread's own default now that both scopes dropped.
        assert_eq!(allocated_bytes(), baseline_default);
    }

    #[test]
    fn realloc_grow_and_shrink_update_the_current_counter() {
        // Exercise `GlobalAlloc` directly with explicit `Layout`s, rather than through `Vec`:
        // `Vec::reserve`/`reserve_exact` reserve capacity relative to `len()`, not the current
        // capacity, which makes the resulting byte counts easy to get wrong by hand - direct
        // calls give full, unambiguous control over exactly what's allocated.
        let scope_target = Arc::new(AtomicUsize::new(0));
        let _scope = Scope::enter(&scope_target);

        let allocator: TrackingAllocator = TrackingAllocator(System);
        let small = Layout::array::<u8>(16).unwrap();

        // SAFETY: standard, correctly-paired `GlobalAlloc` usage: each layout matches the one
        // used to allocate, `realloc`'s `ptr` is the live pointer from the previous call, and
        // nothing is used after being freed.
        unsafe {
            let ptr = allocator.alloc(small);
            assert_eq!(scope_target.load(Ordering::Relaxed), 16);

            let ptr = allocator.realloc(ptr, small, 256);
            assert_eq!(scope_target.load(Ordering::Relaxed), 256);

            let big = Layout::array::<u8>(256).unwrap();
            let ptr = allocator.realloc(ptr, big, 16);
            assert_eq!(scope_target.load(Ordering::Relaxed), 16);

            allocator.dealloc(ptr, small);
            assert_eq!(scope_target.load(Ordering::Relaxed), 0);
        }
    }
}
