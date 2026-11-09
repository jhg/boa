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
    // Every thread's own counters, used whenever no `Scope` has been entered on that thread.
    static PER_THREAD_DEFAULT: Counters = const { Counters::new() };

    // Points at whichever counters should receive this thread's accounting right now: either
    // null (meaning "not resolved yet - use `PER_THREAD_DEFAULT`", see `current_counters`), or
    // a `Scope`'s target while one is entered on this thread.
    //
    // This starts out null, rather than being eagerly set to `&PER_THREAD_DEFAULT` here, so
    // that initializing this thread-local never needs to call `.with()` on another one: both
    // initializers are then plain `const` expressions that need no lazy, heap-allocated storage
    // at all, which matters because that lazy initialization would itself be serviced by this
    // same allocator - calling back into code that is, at that very moment, still in the middle
    // of setting up the thread-local state it depends on.
    static CURRENT: Cell<*const Counters> = const { Cell::new(std::ptr::null()) };
}

// The process-wide counters, updated by every `TrackingAllocator<_, true>` allocation
// regardless of which thread performs it or which `Scope` is active there.
static GLOBAL: Counters = Counters::new();

/// A snapshot of memory-usage statistics for one set of [`Counters`] - a [`Scope`]'s target,
/// the process-wide global, or the calling thread's own default - taken with [`stats`] or
/// [`global_stats`].
///
/// Every field is a plain value read at one point in time, not something that updates on its
/// own; take a fresh snapshot to see current numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// Bytes currently live: allocated and not yet freed.
    ///
    /// This is the same number [`allocated_bytes`]/[`global_allocated_bytes`] read.
    pub live: usize,
    /// The highest `live` has been at any point so far.
    pub peak: usize,
    /// Total bytes ever allocated or grown into, including ones already freed since. Only
    /// increases.
    pub total_allocated: usize,
    /// Number of `alloc` calls.
    pub allocations: usize,
    /// Number of `dealloc` calls.
    pub deallocations: usize,
    /// Number of `realloc` calls, growing or shrinking.
    pub reallocations: usize,
}

/// The atomics backing one [`Stats`] snapshot: a [`Scope`]'s target, the process-wide global,
/// or a thread's own default counters.
#[derive(Debug, Default)]
pub struct Counters {
    live: AtomicUsize,
    peak: AtomicUsize,
    total_allocated: AtomicUsize,
    allocations: AtomicUsize,
    deallocations: AtomicUsize,
    reallocations: AtomicUsize,
}

impl Counters {
    /// Creates a fresh set of counters, all starting at `0`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            total_allocated: AtomicUsize::new(0),
            allocations: AtomicUsize::new(0),
            deallocations: AtomicUsize::new(0),
            reallocations: AtomicUsize::new(0),
        }
    }

    /// Takes a snapshot of these counters.
    #[must_use]
    pub fn snapshot(&self) -> Stats {
        Stats {
            live: self.live(),
            peak: self.peak.load(Ordering::Relaxed),
            total_allocated: self.total_allocated.load(Ordering::Relaxed),
            allocations: self.allocations.load(Ordering::Relaxed),
            deallocations: self.deallocations.load(Ordering::Relaxed),
            reallocations: self.reallocations.load(Ordering::Relaxed),
        }
    }

    #[inline]
    fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    fn record_alloc(&self, bytes: usize) {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        self.grow(bytes);
    }

    fn record_dealloc(&self, bytes: usize) {
        self.deallocations.fetch_add(1, Ordering::Relaxed);
        self.shrink(bytes);
    }

    fn record_realloc_grow(&self, additional: usize) {
        self.reallocations.fetch_add(1, Ordering::Relaxed);
        self.grow(additional);
    }

    fn record_realloc_shrink(&self, removed: usize) {
        self.reallocations.fetch_add(1, Ordering::Relaxed);
        self.shrink(removed);
    }

    fn grow(&self, bytes: usize) {
        self.total_allocated.fetch_add(bytes, Ordering::Relaxed);
        let previous = self.live.fetch_add(bytes, Ordering::Relaxed);
        self.peak
            .fetch_max(previous.wrapping_add(bytes), Ordering::Relaxed);
    }

    fn shrink(&self, bytes: usize) {
        self.live.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// Bytes currently live in whatever is "current" on the calling thread: a [`Scope`] that was
/// [`Scope::enter`]ed and not yet dropped, or else that thread's own default counters.
///
/// Equivalent to `stats().live`, without the cost of reading the other fields too.
///
/// # A freshly created thread does not start at `0`
///
/// The standard library's own thread-spawning machinery allocates and frees some bookkeeping
/// of its own across the parent/child boundary, so a brand new thread's default counters can
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
    current_counters().live()
}

/// A full [`Stats`] snapshot of whatever is "current" on the calling thread. See
/// [`allocated_bytes`] for what "current" means and the caveats of a freshly created thread.
#[must_use]
pub fn stats() -> Stats {
    current_counters().snapshot()
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
    GLOBAL.live()
}

/// A full [`Stats`] snapshot of the process-wide global counters. See
/// [`global_allocated_bytes`] for the requirements and caveats.
#[must_use]
pub fn global_stats() -> Stats {
    GLOBAL.snapshot()
}

#[inline]
fn current_counters() -> &'static Counters {
    let ptr = CURRENT.with(Cell::get);
    let ptr = if ptr.is_null() {
        // First use of `CURRENT` on this thread: resolve and cache a pointer to this thread's
        // own `PER_THREAD_DEFAULT`. This happens here, in an ordinary function call, rather
        // than inside `CURRENT`'s own thread-local initializer - see the comment there.
        let default = PER_THREAD_DEFAULT.with(|counters| -> *const Counters { counters });
        CURRENT.with(|current| current.set(default));
        default
    } else {
        ptr
    };
    // SAFETY: `ptr` always points either at a thread's own `'static`-for-its-lifetime
    // `PER_THREAD_DEFAULT` storage, or at the `Counters` behind a `Scope`'s `Arc`, which the
    // `Scope` guard that installed it keeps alive (and restores the previous pointer before
    // dropping) for as long as it remains "current" on this thread. Either way the pointee
    // outlives every use made of it here.
    unsafe { &*ptr }
}

#[inline]
fn on_alloc(bytes: usize, track_global: bool) {
    if track_global {
        GLOBAL.record_alloc(bytes);
    }
    current_counters().record_alloc(bytes);
}

#[inline]
fn on_dealloc(bytes: usize, track_global: bool) {
    if track_global {
        GLOBAL.record_dealloc(bytes);
    }
    current_counters().record_dealloc(bytes);
}

#[inline]
fn on_realloc_grow(additional: usize, track_global: bool) {
    if track_global {
        GLOBAL.record_realloc_grow(additional);
    }
    current_counters().record_realloc_grow(additional);
}

#[inline]
fn on_realloc_shrink(removed: usize, track_global: bool) {
    if track_global {
        GLOBAL.record_realloc_shrink(removed);
    }
    current_counters().record_realloc_shrink(removed);
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
    previous: *const Counters,
    // Keeps `target`'s allocation alive for as long as `previous` might point at it; dropped
    // only after `Drop::drop` below has already restored `previous` as current.
    _keep_alive: Arc<Counters>,
}

impl Scope {
    /// Redirects the calling thread's accounting to `target` until the returned [`Scope`] is
    /// dropped.
    pub fn enter(target: &Arc<Counters>) -> Self {
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
/// Every allocation and deallocation updates whichever counters [`allocated_bytes`]/[`stats`]
/// would read at that moment on that thread (see [`Scope`]). Set `TRACK_GLOBAL` to `true` to
/// also maintain a process-wide total, readable via [`global_allocated_bytes`]/
/// [`global_stats`]; it defaults to `false` so that installing this allocator costs nothing
/// extra unless you ask for it.
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
            on_alloc(layout.size(), TRACK_GLOBAL);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(ptr, layout) };
        on_dealloc(layout.size(), TRACK_GLOBAL);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            if new_size >= layout.size() {
                on_realloc_grow(new_size - layout.size(), TRACK_GLOBAL);
            } else {
                on_realloc_shrink(layout.size() - new_size, TRACK_GLOBAL);
            }
        }
        new_ptr
    }
}

#[cfg(test)]
#[global_allocator]
static TEST_ALLOCATOR: TrackingAllocator<System, true> = TrackingAllocator(System);

// `GLOBAL` is one counter shared by every allocation anywhere in this test binary - including
// ones from tests that don't otherwise care about it, since `TEST_ALLOCATOR` above always
// tracks it. The tests here are fast (all of them together run in well under a second), so
// every `#[test]` in this crate acquires this lock for its whole body: full serialization is
// cheap for a suite this size, and it rules out *test bodies* racing each other over `GLOBAL`.
// It does not rule out the test harness's own per-test thread setup, which runs before a test's
// body (and this lock) ever does and - like any thread spawn - nudges `GLOBAL` by some small
// amount; tests that measure `GLOBAL` precisely also need a margin generous enough to not care
// about that, on top of holding this lock.
#[cfg(test)]
static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquires [`TEST_SERIAL`], recovering it if some other test already panicked while holding
/// it: a panic there never leaves `GLOBAL`/`PER_THREAD_DEFAULT`/any `Counters` in an invalid
/// state (they're just atomics), so there's nothing for poisoning to usefully protect here.
#[cfg(test)]
fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_allocations_on_this_thread() {
        let _lock = test_lock();

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
        // Deliberately large (not a realistic workload): `TEST_SERIAL` only serializes the
        // *bodies* of the tests here, not the test harness's own per-test thread
        // setup/teardown, which can nudge `GLOBAL` by some small, unpredictable amount outside
        // that window - see the longer comment on
        // `global_track_global_false_leaves_the_global_counters_untouched` about the resulting
        // (rare) flake and why `--test-threads=1` avoids it entirely.
        const SIZE_1: usize = 8 * 1024 * 1024;
        const SIZE_2: usize = 16 * 1024 * 1024;

        let _lock = test_lock();

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

        let t1 = spawn_holding(SIZE_1, ready.clone(), release.clone());
        let t2 = spawn_holding(SIZE_2, ready.clone(), release.clone());

        ready.wait();
        let peak = global_allocated_bytes();
        release.wait();

        t1.join().unwrap();
        t2.join().unwrap();

        assert!(peak.wrapping_sub(global_before) >= SIZE_1 + SIZE_2 - 2 * 1024 * 1024);
    }

    #[test]
    fn per_thread_default_is_isolated_across_real_threads() {
        let _lock = test_lock();

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
        let _lock = test_lock();
        let outer = Arc::new(Counters::new());
        let inner = Arc::new(Counters::new());
        let baseline_default = allocated_bytes();

        {
            let _outer_scope = Scope::enter(&outer);
            let v1: Vec<u8> = Vec::with_capacity(100);
            assert_eq!(outer.live(), 100);
            {
                let _inner_scope = Scope::enter(&inner);
                let v2: Vec<u8> = Vec::with_capacity(50);
                assert_eq!(inner.live(), 50);
                assert_eq!(outer.live(), 100, "outer untouched while nested");
                drop(v2);
                assert_eq!(inner.live(), 0);
            }
            // Back to `outer` now that the inner scope dropped.
            let v3: Vec<u8> = Vec::with_capacity(25);
            assert_eq!(outer.live(), 125);
            drop(v1);
            drop(v3);
        }
        // Back to this thread's own default now that both scopes dropped.
        assert_eq!(allocated_bytes(), baseline_default);
    }

    #[test]
    fn realloc_grow_and_shrink_update_the_current_counter() {
        let _lock = test_lock();

        // Exercise `GlobalAlloc` directly with explicit `Layout`s, rather than through `Vec`:
        // `Vec::reserve`/`reserve_exact` reserve capacity relative to `len()`, not the current
        // capacity, which makes the resulting byte counts easy to get wrong by hand - direct
        // calls give full, unambiguous control over exactly what's allocated.
        let scope_target = Arc::new(Counters::new());
        let _scope = Scope::enter(&scope_target);

        let allocator: TrackingAllocator = TrackingAllocator(System);
        let small = Layout::array::<u8>(16).unwrap();

        // SAFETY: standard, correctly-paired `GlobalAlloc` usage: each layout matches the one
        // used to allocate, `realloc`'s `ptr` is the live pointer from the previous call, and
        // nothing is used after being freed.
        unsafe {
            let ptr = allocator.alloc(small);
            assert_eq!(scope_target.live(), 16);

            let ptr = allocator.realloc(ptr, small, 256);
            assert_eq!(scope_target.live(), 256);

            let big = Layout::array::<u8>(256).unwrap();
            let ptr = allocator.realloc(ptr, big, 16);
            assert_eq!(scope_target.live(), 16);

            allocator.dealloc(ptr, small);
            assert_eq!(scope_target.live(), 0);
        }
    }

    #[test]
    fn stats_track_peak_total_and_operation_counts() {
        let _lock = test_lock();
        let target = Arc::new(Counters::new());
        let _scope = Scope::enter(&target);

        let allocator: TrackingAllocator = TrackingAllocator(System);
        let layout16 = Layout::array::<u8>(16).unwrap();
        let layout256 = Layout::array::<u8>(256).unwrap();

        // SAFETY: same reasoning as `realloc_grow_and_shrink_update_the_current_counter`.
        unsafe {
            let ptr = allocator.alloc(layout16);
            let ptr = allocator.realloc(ptr, layout16, 256); // grow to 256
            let ptr = allocator.realloc(ptr, layout256, 16); // shrink back to 16
            allocator.dealloc(ptr, Layout::array::<u8>(16).unwrap());
        }

        let snapshot = target.snapshot();
        assert_eq!(snapshot.live, 0, "everything was freed");
        assert_eq!(
            snapshot.peak, 256,
            "the high point was reached mid-sequence"
        );
        assert_eq!(
            snapshot.total_allocated,
            16 + 240,
            "initial 16 + the 240 the first realloc grew by; shrinking never counts here"
        );
        assert_eq!(snapshot.allocations, 1);
        assert_eq!(snapshot.deallocations, 1);
        assert_eq!(snapshot.reallocations, 2, "one grow, one shrink");
    }

    #[test]
    fn global_track_global_false_leaves_the_global_counters_untouched() {
        // Large and with a generous margin, not a realistic workload: `TEST_SERIAL` only covers
        // the *bodies* of the tests in this crate, not the test harness's own per-test thread
        // setup/teardown. In rare cases (observed at well under 1% across repeated runs while
        // deliberately hammering this suite in a tight loop) a sibling test's thread can still
        // be in the middle of such OS-level teardown - with its own, unrelated TLS destructors
        // potentially touching the heap - just after it released the lock and just as this test
        // takes its own reading, which can make this flake. That's a property of testing
        // genuinely process-wide state under a parallel test harness, not a bug in the tracking
        // logic itself (which 100+ consecutive runs under `--test-threads=1`, where this can't
        // happen at all, confirm independently). Use `--test-threads=1` for a fully
        // deterministic run of this crate's tests.
        const SIZE: usize = 1024 * 1024;

        // These two allocators are never installed as `#[global_allocator]` - that can only
        // happen once per binary, and this crate's test binary already installs
        // `TrackingAllocator<System, true>` above. Calling their `GlobalAlloc` methods directly
        // still exercises the exact same code path a real installation would use; it's just the
        // *caller*, not the process's actual allocator, routing these particular bytes through
        // them.
        let _lock = test_lock();

        let tracks_global: TrackingAllocator<System, true> = TrackingAllocator(System);
        let ignores_global: TrackingAllocator<System, false> = TrackingAllocator(System);
        let layout = Layout::array::<u8>(SIZE).unwrap();

        let before = global_allocated_bytes();

        // SAFETY: each layout matches the allocation it frees.
        unsafe {
            let ptr = ignores_global.alloc(layout);
            assert!(
                global_allocated_bytes().wrapping_sub(before) < SIZE / 2,
                "TRACK_GLOBAL = false must not meaningfully move the global counter"
            );
            ignores_global.dealloc(ptr, layout);
        }

        // SAFETY: as above.
        unsafe {
            let ptr = tracks_global.alloc(layout);
            assert!(
                global_allocated_bytes().wrapping_sub(before) >= SIZE,
                "TRACK_GLOBAL = true must move the global counter by at least what we allocated"
            );
            tracks_global.dealloc(ptr, layout);
        }
    }

    #[test]
    fn a_shared_scope_aggregates_correctly_under_real_concurrent_access() {
        let _lock = test_lock();

        // Two real threads holding a `Scope` over the *same* `Counters`, allocating at the same
        // time, to exercise `Counters`' atomics under genuine contention rather than just
        // sequential access from one thread.
        let shared = Arc::new(Counters::new());
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let spawn_worker =
            |shared: Arc<Counters>, barrier: Arc<std::sync::Barrier>, size: usize| {
                std::thread::spawn(move || {
                    let _scope = Scope::enter(&shared);
                    barrier.wait();
                    for _ in 0..100 {
                        let v: Vec<u8> = Vec::with_capacity(size);
                        std::hint::black_box(&v);
                    }
                })
            };

        let t1 = spawn_worker(shared.clone(), barrier.clone(), 16);
        let t2 = spawn_worker(shared.clone(), barrier.clone(), 32);

        t1.join().unwrap();
        t2.join().unwrap();

        let snapshot = shared.snapshot();
        assert_eq!(
            snapshot.live, 0,
            "everything was freed by the time both threads finished"
        );
        assert_eq!(snapshot.allocations, 200);
        assert_eq!(snapshot.deallocations, 200);
        assert_eq!(snapshot.total_allocated, 100 * 16 + 100 * 32);
    }
}
