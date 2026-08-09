//! Cooperative interruption of running ECMAScript execution.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A handle that can be used to request interruption of a running [`Context`](crate::Context)
/// from another thread.
///
/// Every [`Context`](crate::Context) owns one, reachable via
/// [`Context::interrupt_handle`](crate::Context::interrupt_handle). Cloning an [`InterruptHandle`]
/// is cheap and shares the same underlying flag, so it can be stashed away (e.g. moved into a
/// watchdog thread or an async task) before starting execution.
///
/// Once [`InterruptHandle::interrupt`] is called, the next bytecode instruction dispatched by
/// the virtual machine throws an [`EngineError`](crate::error::EngineError) that unwinds all the
/// way back to the (Rust) caller of whatever entry point started execution (e.g.
/// [`Script::evaluate`](crate::Script::evaluate) or [`Module::evaluate`](crate::Module::evaluate)).
/// This error is **not catchable** from ECMAScript `try`/`catch`, so a hostile script cannot
/// suppress it and keep running.
///
/// Unlike dropping a `Future` returned by an `*_async` API, interrupting a [`Context`] this way
/// unwinds the virtual machine's call stack cleanly (the same code path used for any other
/// uncatchable engine error), so the [`Context`] is left in a valid state and can be reused for
/// further execution after calling [`InterruptHandle::reset`].
///
/// This mechanism only bounds CPU time spent executing ECMAScript bytecode. A single native
/// (Rust) builtin call - e.g. a pathological regular expression, or `Array.prototype.sort` on a
/// huge array - runs to completion without checking this flag, since the flag is only polled
/// between VM instructions. Pair this with an OS-level watchdog (a dedicated thread per
/// execution that is abandoned if it doesn't return in time) if native builtins need to be
/// bounded too.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle(Arc<AtomicBool>);

impl InterruptHandle {
    /// Requests interruption of the execution associated with this handle.
    ///
    /// Safe to call from any thread, at any time. Has no effect if the associated [`Context`]
    /// is not currently executing; the next execution will be interrupted immediately unless
    /// [`InterruptHandle::reset`] is called first.
    pub fn interrupt(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Returns `true` if [`InterruptHandle::interrupt`] was called and the flag hasn't been
    /// reset since.
    #[must_use]
    pub fn is_interrupted(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Clears a previously requested interruption.
    ///
    /// Call this before reusing a [`Context`] whose execution was interrupted.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}
