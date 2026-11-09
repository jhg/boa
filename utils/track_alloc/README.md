# track_alloc

A [`GlobalAlloc`][global-alloc] wrapper that tracks memory usage per thread, per async task, or
for the whole process - **without ever refusing an allocation**.

Most memory-tracking allocators (e.g. [`cap`][cap], [`stats_alloc`][stats-alloc]) track a single
global total. `track_alloc` adds two things on top of that:

- **Per-thread and per-scope isolation**, so you can measure "how much memory is *this* piece of
  work using" without noise from everything else running concurrently in the same process - the
  same way you might pin one long-lived, non-`Send` resource to its own thread.
- **`async`/`await`-awareness**: a [`track`] combinator attributes a future's allocations to one
  counter across every `poll`, even if a work-stealing executor (like `tokio`'s default runtime)
  moves it between OS threads between polls. This is runtime-agnostic - it only depends on
  [`Future`] from `core`, not on any particular executor.

It never enforces a limit by refusing allocations. Rust's standard collections treat a null
allocation as fatal and abort the whole process, so an allocator that refuses is unsafe to use
for a *per-scope* budget in a process that is also running unrelated work: one over-budget scope
would take the whole process down with it. Enforce your own limit by comparing the counters
against it at a point where it's safe to unwind - see the examples below.

## Install it

```rust
use std::alloc::System;
use track_alloc::TrackingAllocator;

#[global_allocator]
static ALLOCATOR: TrackingAllocator<System> = TrackingAllocator(System);
```

## Per-thread tracking

By default, every thread gets its own counter, read with [`allocated_bytes`]:

```rust
use std::alloc::System;
use track_alloc::{TrackingAllocator, allocated_bytes};

#[global_allocator]
static ALLOCATOR: TrackingAllocator<System> = TrackingAllocator(System);

let before = allocated_bytes();
let v: Vec<u8> = Vec::with_capacity(4096);
assert!(allocated_bytes().wrapping_sub(before) >= 4096);
```

This fits anything that pins a resource to one thread for its whole life - a `!Send` interpreter
context, an actor, a connection handler - and wants its own memory usage without needing to
subtract out what other threads are doing concurrently.

A freshly spawned thread's counter does *not* start at `0` - the standard library's own
thread-spawning machinery allocates and frees a little bookkeeping of its own across the
parent/child boundary. Always compare two readings with [`wrapping_sub`][wrapping-sub] as above,
never plain subtraction, and don't assume a fresh thread's counter is "small".

## Global, process-wide tracking

Set `TRACK_GLOBAL = true` on the installed allocator to also maintain a shared total, read with
[`global_allocated_bytes`]. It defaults to `false`, so installing this allocator costs nothing
extra unless you ask for it:

```rust
use std::alloc::System;
use track_alloc::TrackingAllocator;

#[global_allocator]
static ALLOCATOR: TrackingAllocator<System, true> = TrackingAllocator(System);
```

```rust
use track_alloc::global_allocated_bytes;

println!("Total memory allocated by this process: {}B", global_allocated_bytes());
```

## Per-scope and per-task tracking

A [`Scope`] redirects *this thread's* accounting to any counter you supply, for as long as it's
held - restoring whatever was current before once it's dropped. Scopes nest correctly:

```rust
use std::alloc::System;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use track_alloc::{Scope, TrackingAllocator};

#[global_allocator]
static ALLOCATOR: TrackingAllocator<System> = TrackingAllocator(System);

let budget = Arc::new(AtomicUsize::new(0));
{
    let _scope = Scope::enter(&budget);
    let v: Vec<u8> = Vec::with_capacity(1024);
    assert_eq!(budget.load(std::sync::atomic::Ordering::Relaxed), 1024);
}
```

Combine this with [`track`] to follow a logical `async` task's allocations across `poll` calls,
even when they land on different OS threads:

```rust
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use track_alloc::track;

async fn do_work() {
    let _buf: Vec<u8> = Vec::with_capacity(4096);
    // ... some `.await` point that a work-stealing executor could resume on another thread ...
}

let budget = Arc::new(AtomicUsize::new(0));
let tracked = track(budget.clone(), do_work());
// Run `tracked` on any executor; `budget` reflects its allocations regardless of which
// worker thread actually polls it.
```

[global-alloc]: https://doc.rust-lang.org/std/alloc/trait.GlobalAlloc.html
[cap]: https://crates.io/crates/cap
[stats-alloc]: https://crates.io/crates/stats_alloc
[`Future`]: https://doc.rust-lang.org/std/future/trait.Future.html
[wrapping-sub]: https://doc.rust-lang.org/std/primitive.usize.html#method.wrapping_sub
