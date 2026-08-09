//! This example shows how to bound the wall-clock execution time of a script (or module) run
//! with Boa, using [`Context::interrupt_handle`] together with a watchdog thread, backed up by
//! [`RuntimeLimits`] as a deterministic safety net.
//!
//! `Context::interrupt_handle()` returns a cheap, `Send + Sync` [`InterruptHandle`] that can be
//! handed to another thread (here, a plain `std::thread` watchdog - no async runtime required).
//! Calling `handle.interrupt()` makes the *next* bytecode instruction the VM dispatches throw an
//! uncatchable engine error, unwinding all the way back to the Rust caller of `evaluate`/
//! `evaluate` (module). Because this goes through the engine's normal (uncatchable-error) stack
//! unwinding, the `Context` is left in a valid state afterwards and CAN be reused, unlike
//! abandoning a `Future` mid-poll.
//!
//! This bounds time spent executing ECMAScript *bytecode* - loops, arithmetic, user function
//! calls. It does NOT preempt a single native (Rust) builtin call that is already running (e.g.
//! a pathological regex, or `JSON.stringify` on a huge structure): the interrupt flag is only
//! checked between VM instructions, and a builtin call is one instruction from the VM's point of
//! view. `RuntimeLimits::set_loop_iteration_limit`/`set_recursion_limit` are layered in as a
//! deterministic, thread-independent backstop for tight loops that (for whatever reason) never
//! get interrupted in time.
//!
//! Both `Script` and `Module` execution go through the same VM instruction dispatch loop, so
//! this works identically for both - unlike the `budget`-based cooperative-yield APIs
//! (`Script::evaluate_async_with_budget`), which only exist for `Script`.

use boa_engine::vm::InterruptHandle;
use boa_engine::{Context, JsResult, Module, Script, Source};
use std::time::Duration;

fn main() -> JsResult<()> {
    run_script_with_timeout(
        "Script: infinite loop",
        "let i = 0; while (true) { i++; }",
        Duration::from_millis(500),
    );

    run_script_with_timeout(
        "Script: fast script",
        "let sum = 0; for (let i = 0; i < 1000; i++) { sum += i; } sum;",
        Duration::from_millis(500),
    );

    run_module_with_timeout(
        "Module: infinite loop",
        "let i = 0; while (true) { i++; }",
        Duration::from_millis(500),
    );

    Ok(())
}

/// Spawns a watchdog thread that interrupts `handle` after `timeout`, unless `done` fires first.
///
/// Returns a guard: drop it (or call `.cancel()`) as soon as execution finishes normally, so the
/// watchdog doesn't fire on a `Context` that has already moved on to unrelated work.
fn watchdog(handle: InterruptHandle, timeout: Duration) -> std::sync::mpsc::Sender<()> {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        // If `done` fires before the timeout, the script finished on its own; do nothing.
        if done_rx.recv_timeout(timeout).is_err() {
            handle.interrupt();
        }
    });
    done_tx
}

fn run_script_with_timeout(label: &str, src: &str, timeout: Duration) {
    println!("\n=== {label} ===");

    let mut context = Context::default();
    // Deterministic backstop, independent of the watchdog thread.
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(50_000_000);

    let done = watchdog(context.interrupt_handle(), timeout);

    let script = match Script::parse(Source::from_bytes(src), None, &mut context) {
        Ok(script) => script,
        Err(err) => {
            println!("parse error: {err}");
            return;
        }
    };

    match script.evaluate(&mut context) {
        Ok(value) => println!("finished: {}", value.display()),
        Err(err) => println!("finished with error: {err}"),
    }

    // Execution is over; tell the watchdog to stand down so it doesn't fire later.
    let _ = done.send(());
}

fn run_module_with_timeout(label: &str, src: &str, timeout: Duration) {
    println!("\n=== {label} ===");

    let mut context = Context::default();
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(50_000_000);

    let done = watchdog(context.interrupt_handle(), timeout);

    let module = match Module::parse(Source::from_bytes(src), None, &mut context) {
        Ok(module) => module,
        Err(err) => {
            println!("parse error: {err}");
            return;
        }
    };

    if let Err(err) = module.link(&mut context) {
        println!("link error: {err}");
        return;
    }

    // This module has no imports and no top-level await, so `evaluate` runs its whole body
    // synchronously before returning, going through the same VM loop `Script::evaluate` uses -
    // which is exactly the loop the interrupt handle is checked in.
    let promise = module.evaluate(&mut context);

    let _ = done.send(());

    match promise {
        Ok(promise) => println!(
            "evaluate() returned promise in state: {:?}",
            promise.state()
        ),
        Err(err) => println!("finished with error: {err}"),
    }
}
