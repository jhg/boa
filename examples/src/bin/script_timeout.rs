//! This example shows how to run a (possibly long-running or infinite-looping) script
//! with a wall-clock timeout, using `Script::evaluate_async_with_budget` combined with
//! `tokio::time::timeout`.
//!
//! `evaluate_async_with_budget` runs the script cooperatively: every `budget` "clock cycles"
//! (an implementation-defined cost unit per VM instruction) it yields back to the async
//! executor via `yield_now().await`. That periodic yield point is what allows an external
//! `tokio::select!`/`timeout` to reclaim control: once the timeout future wins the race, we
//! simply stop polling the script's future and drop it. The script does not receive any
//! signal and does not get to run any more JS after that point - execution is abandoned,
//! not "told" to stop.
//!
//! `RuntimeLimits` is layered on top as a defense against tight CPU-bound loops that never
//! reach an `await`/yield point in JS (e.g. `while (true) {}` with no async operations
//! inside): the VM itself will throw a catchable `RuntimeLimitError` once the loop iteration
//! limit is exceeded, regardless of the tokio timeout.

use boa_engine::{Context, JsResult, Script, Source};
use std::time::Duration;

#[tokio::main]
async fn main() -> JsResult<()> {
    // Script that loops forever and never yields on its own (no `await`, no I/O).
    run_with_timeout(
        "infinite loop, bounded by RuntimeLimits",
        "let i = 0; while (true) { i++; }",
        Duration::from_secs(2),
    )
    .await;

    // A script that finishes well within the timeout.
    run_with_timeout(
        "fast script",
        "let sum = 0; for (let i = 0; i < 1000; i++) { sum += i; } sum;",
        Duration::from_secs(2),
    )
    .await;

    Ok(())
}

async fn run_with_timeout(label: &str, src: &str, timeout: Duration) {
    println!("\n=== {label} ===");

    let mut context = Context::default();

    // Safety net for tight loops that never yield to the async budget check on their own.
    // Without this, a `while (true) {}` with a huge/steppy budget could in principle still
    // take a long time to reach a yield point, since the budget is measured in "cost units",
    // not wall-clock time.
    context
        .runtime_limits_mut()
        .set_loop_iteration_limit(50_000_000);

    let script = match Script::parse(Source::from_bytes(src), None, &mut context) {
        Ok(script) => script,
        Err(err) => {
            println!("parse error: {err}");
            return;
        }
    };

    // A small budget means the VM yields to the executor more often, which lowers the
    // worst-case latency between the timeout firing and us actually stopping polling the
    // script's future, at the cost of a bit more overhead. Tune this per application.
    let eval = script.evaluate_async_with_budget(&mut context, 256);

    match tokio::time::timeout(timeout, eval).await {
        Ok(Ok(value)) => println!("finished: {}", value.display()),
        Ok(Err(err)) => println!("finished with JS error: {err}"),
        Err(_) => {
            // The `eval` future is dropped here without being polled again. The script's
            // execution is abandoned mid-instruction; the VM state inside `context` is left
            // in whatever partial state it was in. Don't reuse `context` afterwards - drop it
            // (as we do, by letting it go out of scope) and create a fresh one if you need to
            // run more scripts.
            println!("timed out after {timeout:?}, execution abandoned");
        }
    }
}
