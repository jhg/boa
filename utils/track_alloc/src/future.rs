use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::{Counters, Scope};

/// Wraps `fut` so that every byte it allocates or frees, on every `poll`, is attributed to
/// `scope` - regardless of which OS thread actually executes that particular `poll` call.
///
/// This is what makes memory tracking meaningful for a logical *task* on a work-stealing,
/// multi-threaded `async` executor (such as `tokio`'s default runtime): an ordinary
/// thread-local counter would instead attribute each `poll` to whichever worker thread
/// happened to run it, smearing one task's usage across however many threads it was scheduled
/// on over its lifetime.
///
/// This works with any executor: it only depends on [`Future`]/[`Context`]/[`Poll`] from
/// `core`, not on any particular async runtime.
#[must_use]
pub fn track<F: Future>(scope: Arc<Counters>, fut: F) -> Tracked<F> {
    Tracked { inner: fut, scope }
}

/// A [`Future`] that attributes every byte its inner future allocates or frees, on every
/// `poll`, to a [`Scope`] over the given counters. See [`track`].
#[derive(Debug)]
pub struct Tracked<F> {
    inner: F,
    scope: Arc<Counters>,
}

impl<F: Future> Future for Tracked<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // `scope` is `Arc<Counters>`, which is always `Unpin`, so reading it through `Pin`'s
        // unconditional `Deref` is sound regardless of whether `F` (and therefore `Self`) is.
        // The borrow ends once `Scope::enter` returns (it clones what it needs), so it doesn't
        // conflict with projecting into `self` right below.
        let _guard = Scope::enter(&self.scope);

        // SAFETY: this is the standard single-field pin projection: `inner` is never moved out
        // of `Self` anywhere else, and `Self` is not `repr(packed)`, so its address is stable
        // for as long as `Self` is pinned.
        let inner = unsafe { self.map_unchecked_mut(|tracked| &mut tracked.inner) };
        inner.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{RawWaker, RawWakerVTable, Waker};

    fn dummy_waker() -> Waker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        // SAFETY: every function in `VTABLE` is a correct no-op for a waker that is never
        // actually used to wake anything (this test drives polling manually).
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    // A real `async fn` future: it holds local state (`extra`) across an `.await` point, so
    // its compiler-generated state machine is genuinely `!Unpin` - the case that matters, since
    // requiring `Unpin` would make this combinator useless for ordinary `async fn`/`async {}`
    // code.
    //
    // It allocates across *three* polls rather than two, each gated separately, so there's a
    // point - after the second allocation, before the function returns - where both `first` and
    // `second` are still genuinely live (an `async fn`'s locals are dropped as part of it
    // completing, within that same final `poll`, so checking right as it returns would measure
    // memory that's already been freed rather than what was live mid-flight).
    fn wait_for(gate: Arc<std::sync::atomic::AtomicBool>) -> impl Future<Output = ()> {
        std::future::poll_fn(move |cx| {
            if gate.load(std::sync::atomic::Ordering::Relaxed) {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
    }

    async fn allocates_twice(
        gate1: Arc<std::sync::atomic::AtomicBool>,
        gate2: Arc<std::sync::atomic::AtomicBool>,
    ) -> usize {
        let first: Vec<u8> = Vec::with_capacity(10);
        wait_for(gate1).await;
        let second: Vec<u8> = Vec::with_capacity(20);
        wait_for(gate2).await;
        first.capacity() + second.capacity()
    }

    #[test]
    fn attributes_allocations_across_polls_on_different_threads() {
        // See `TEST_SERIAL`'s doc comment: every test in this crate holds it, so that tests
        // measuring the process-wide global counter elsewhere aren't disturbed by this one's
        // allocations running concurrently.
        let _lock = crate::test_lock();

        let gate1 = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate2 = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scope = Arc::new(Counters::new());
        let mut tracked = Box::pin(track(
            scope.clone(),
            allocates_twice(gate1.clone(), gate2.clone()),
        ));

        let waker = dummy_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll, on this thread: allocates `first` and returns Pending.
        assert!(tracked.as_mut().poll(&mut cx).is_pending());
        assert!(scope.snapshot().live >= 10);

        // Second poll, on a *different* OS thread - simulating a work-stealing executor moving
        // the task between polls. The `Tracked` wrapper re-enters its `Scope` on every poll
        // regardless of which thread calls it, so this must still land in `scope`, not in
        // whatever default counter this new thread would otherwise use. `first` is still alive
        // here (held across the `.await`), and this poll allocates `second` too, so both are
        // genuinely live together once this returns.
        gate1.store(true, std::sync::atomic::Ordering::Relaxed);
        let scope_for_thread = scope.clone();
        tracked = std::thread::spawn(move || {
            let waker = dummy_waker();
            let mut cx = Context::from_waker(&waker);
            assert!(tracked.as_mut().poll(&mut cx).is_pending());
            assert_eq!(
                scope_for_thread.snapshot().live,
                30,
                "first + second, both still live"
            );
            tracked
        })
        .join()
        .unwrap();

        // Third poll, back on this thread: completes and returns, dropping both locals as part
        // of that same call.
        gate2.store(true, std::sync::atomic::Ordering::Relaxed);
        let result = match tracked.as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("gate2 was set; must be ready"),
        };

        assert_eq!(result, 30);
        let snapshot = scope.snapshot();
        assert_eq!(snapshot.live, 0, "both freed once the task completed");
        assert_eq!(snapshot.allocations, 2);
        assert_eq!(snapshot.deallocations, 2);
        assert_eq!(snapshot.total_allocated, 30);
    }
}
