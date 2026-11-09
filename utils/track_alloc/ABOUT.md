# About `track_alloc`

`track_alloc` is a small, dependency-free utility crate that provides a [`GlobalAlloc`][global-alloc]
wrapper tracking the number of bytes currently allocated **on the calling thread**.

It never refuses an allocation; it only counts. This makes it safe to install as a process's
`#[global_allocator]` without risking an allocation-failure abort, while still giving accurate,
real-time memory usage - including allocations that a tracing garbage collector like `boa_gc`
cannot see on its own (e.g. string buffers, or a `Vec` growing in place).

[global-alloc]: https://doc.rust-lang.org/std/alloc/trait.GlobalAlloc.html
