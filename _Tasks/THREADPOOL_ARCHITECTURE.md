# WASM Threadpool Architecture: Pure-Rust Workers

> **STATUS: IMPLEMENTED (2026-07-06).** All phases done; threaded tests pass
> 6/6 including session open and pub/sub roundtrip. Deviations from this plan:
> - The Acceptor's channel receive loops use setTimeout-based self-repolling
>   (`recv_async_anywhere` in zenoh-runtime) instead of a global waker
>   registry/poll nudge — same effect, no global state.
> - A second, independent bug was uncovered once the executor worked:
>   `WebSocket.send()` throws on SharedArrayBuffer-backed views; the write
>   loop swallowed the error. Fix: copy outgoing frames into a fresh
>   non-shared `Uint8Array` (unicast_wasm.rs).
> - `-Clink-arg=--export=__heap_base` is required in `.cargo/config.toml` —
>   newer wasm-ld stopped exporting it and wasm-bindgen's threading pass
>   needs it.

## Problem Statement

The current `wasm-threads` implementation uses `wasm_bindgen_futures::spawn_local`
on each Web Worker as the task executor. `spawn_local` is backed by the JS
microtask queue (`queueMicrotask`). This creates two fatal issues:

1. **`block_in_place` can't pump the executor.** When a worker calls
   `Condvar::wait` (Atomics.wait), its JS event loop freezes. No microtasks
   run, no pending futures make progress. Tokio's `block_in_place` works
   because it yields to the runtime's scheduler internally — our Condvar-based
   version can't do that because the scheduler IS the JS event loop.

2. **Cross-worker waking doesn't work.** When flume's `tx.send()` on worker A
   calls `waker.wake()` for a future on worker B, the wake schedules a
   microtask on worker A's queue (wrong thread). Worker B never sees it.

Both problems stem from the same root cause: **the executor depends on the JS
event loop, and we can't pump JS from Rust.**

## Solution: Pure-Rust Executor on Workers

### Core Principle

**Workers in the threadpool must not depend on JS at all.** No `spawn_local`,
no `setTimeout`, no JS Promises. The executor is a pure-Rust `LocalPool` that
we fully control and can pump from `block_in_place`.

JS is only used by:
- **Main thread** — UI, init, user-facing async (uses `spawn_local` as today)
- **I/O worker (Acceptor)** — WebSocket creation, callbacks, JS timers. Never
  blocks. Communicates with threadpool via shared-memory channels only.

### Architecture

```
Main Thread (JS event loop)
├── User code: zenoh::open().await
├── Uses spawn_local (JS microtask queue) — fine, never blocks
├── JoinHandle polls via setTimeout re-poll — fine for main thread
│
├── I/O Worker (Acceptor) — JS event loop
│   ├── Owns all WebSocket objects
│   ├── JS callbacks (onmessage, onerror, onopen, onclose)
│   ├── spawn_local for write loop, timeout tracking
│   ├── NEVER calls block_in_place
│   ├── Communicates with threadpool via flume channels (shared memory)
│   └── sleep/yield via setTimeout — fine, never blocks
│
└── Threadpool Workers (Application, Net, TX, RX) — NO JS
    ├── Pure-Rust LocalExecutor (VecDeque<Pin<Box<dyn Future>>>)
    ├── Pumped by:
    │   a) Worker's main loop (normal async operation)
    │   b) block_in_place (pumps between Condvar waits)
    ├── sleep/yield via Condvar::wait_timeout (pure Rust, no JS)
    ├── Wakers use Condvar::notify (pure Rust, works cross-worker)
    ├── No spawn_local, no setTimeout, no JS Promises
    └── All I/O goes through channels to/from I/O worker
```

### Why This Works

1. **`block_in_place` can pump the executor.** The `LocalExecutor` is a Rust
   struct we own. `block_in_place` polls the target future, then polls all
   other ready futures in the executor's queue, then waits on the Condvar.
   Other tasks on the same worker make progress between polls.

2. **Cross-worker waking works.** Wakers are backed by `Condvar::notify_one`
   which compiles to `memory.atomic.notify` — this works across any worker
   sharing the same memory. No JS microtask queue involved.

3. **I/O is decoupled.** The I/O worker has its own JS event loop for
   WebSocket callbacks and timers. It never blocks. Data flows to/from the
   threadpool via flume channels backed by shared memory atomics.

## Detailed Changes

### 1. New: `LocalExecutor` (in `wasm_threaded.rs`)

A minimal single-threaded async executor that can be pumped manually.

```rust
struct LocalExecutor {
    /// Ready queue: futures waiting to be polled.
    ready: RefCell<VecDeque<Task>>,
    /// Waker that enqueues a task back into ready.
    /// When a future returns Pending, its waker — when called —
    /// pushes the task back into `ready` and notifies the Condvar.
    condvar: Arc<Condvar>,
    wake_flag: Arc<Mutex<bool>>,
}

struct Task {
    future: Pin<Box<dyn Future<Output = ()>>>,
    // ... waker state
}

impl LocalExecutor {
    /// Pump: poll all ready tasks. Returns when ready queue is empty.
    fn pump(&self) { ... }

    /// Spawn a future onto this executor.
    fn spawn<F: Future<Output = ()> + 'static>(&self, f: F) { ... }

    /// Block until future completes, pumping other tasks between waits.
    fn block_on<F: Future>(&self, f: F) -> F::Output {
        pin!(f);
        loop {
            match f.poll(&mut cx) {
                Ready(v) => return v,
                Pending => {
                    // Pump other tasks — they may produce the value we need
                    self.pump();
                    // Wait for any waker to fire (with timeout)
                    let mut flag = self.wake_flag.lock().unwrap();
                    if !*flag {
                        flag = self.condvar.wait_timeout(flag, 1ms).unwrap().0;
                    }
                    *flag = false;
                }
            }
        }
    }
}
```

Key: wakers push tasks back into `ready` and call `Condvar::notify_one`.
This is all pure Rust — no JS involved.

### 2. Change: Worker Entry Point

Current:
```rust
// Uses spawn_local (JS microtask queue)
wasm_bindgen_futures::spawn_local(async move {
    loop {
        let task = rx.recv_async().await;
        task();  // task() calls spawn_local again
    }
});
```

New:
```rust
// Pure-Rust executor, no JS dependency
let executor = LocalExecutor::new();
executor.spawn(async move {
    loop {
        let task = rx.recv_async().await;
        task(&executor);  // task spawns onto our executor, not spawn_local
    }
});
// Run the executor — blocks the worker thread
executor.run();  // loops: pump() → Condvar::wait → pump() → ...
```

The worker thread is entirely owned by our executor. No JS event loop runs
on threadpool workers. The Condvar::wait is the idle state — it wakes when
any waker fires (new task arrived, channel data available, etc.).

### 3. Change: `ZRuntime::spawn()`

Current:
```rust
let task = Box::new(move || {
    wasm_bindgen_futures::spawn_local(async move { ... });
});
worker.task_tx.send(task);
```

New:
```rust
let task = Box::new(move |executor: &LocalExecutor| {
    executor.spawn(async move { ... });
});
worker.task_tx.send(task);
```

The spawned future runs on the worker's `LocalExecutor`, not `spawn_local`.

### 4. Change: `ZRuntime::block_in_place()`

Current:
```rust
loop {
    match f.poll(&mut cx) {
        Ready(v) => return v,
        Pending => Condvar::wait_timeout(5ms),  // nothing else runs
    }
}
```

New:
```rust
// Get this worker's executor
let executor = CURRENT_EXECUTOR.with(|e| e.clone());
executor.block_on(f)
// block_on internally: poll f → pump other tasks → Condvar::wait → repeat
```

This is the critical change. `block_in_place` now pumps ALL pending tasks
on the current worker between polls of the blocked future. This matches
tokio's behavior.

### 5. Change: `wasm_yield::sleep_ms` on Workers

Current: uses `setTimeout` (JS) + flume channel.

New (on threadpool workers): uses `Condvar::wait_timeout` (pure Rust).

```rust
pub fn sleep_ms(ms: u32) -> impl Future<Output = ()> + Send {
    #[cfg(feature = "wasm-threads")]
    {
        // Pure-Rust sleep: register a waker, wait_timeout, re-poll
        CondvarSleep::new(Duration::from_millis(ms as u64))
    }
    #[cfg(not(feature = "wasm-threads"))]
    {
        // JS-based sleep (existing implementation)
        // ...setTimeout...
    }
}
```

`CondvarSleep` is a future that:
1. Records `Instant::now() + duration` as the deadline
2. On poll: if deadline passed → Ready. Otherwise → registers waker, Pending.
3. The executor's pump loop checks timeouts and fires wakers when due.

### 6. Change: `Instant` (wasm_yield.rs)

`Date.now()` requires JS. But with `+atomics` and shared memory,
`std::time::SystemTime` may work (depends on WASI-like clock). If not,
we can use a shared monotonic counter incremented by the I/O worker's
`setInterval`:

```rust
// I/O worker increments this every 1ms via setInterval
static MONOTONIC_MS: AtomicU64 = AtomicU64::new(0);

impl Instant {
    pub fn now() -> Self {
        Instant(MONOTONIC_MS.load(Ordering::Relaxed))
    }
}
```

Alternatively, keep `Date.now()` — it's a simple FFI call that doesn't
depend on the JS event loop. It's synchronous and safe to call from workers.
**Decision: keep Date.now() — it's a leaf call, not event-loop-dependent.**

### 7. I/O Worker (Acceptor) — No Changes Needed

The Acceptor worker already:
- Owns WebSocket objects
- Runs `spawn_local` for callback processing
- Uses the JS event loop for timers
- Never calls `block_in_place`
- Communicates via flume channels

The only change: it stays on `spawn_local` (JS-based) while other workers
move to `LocalExecutor`. Its entry point is different from threadpool workers.

### 8. WebSocket Link (`unicast_wasm.rs`)

Already correct architecture:
- WebSocket creation dispatched to Acceptor (I/O worker)
- Read path: flume channel `recv_rx` (shared memory) — works from any worker
- Write path: flume channel `write_tx` (shared memory) — works from any worker
- Callbacks run on Acceptor's JS event loop

No changes needed. The `cross_worker_recv` wrapper can be removed since
the pure-Rust executor's wakers work natively across workers.

### 9. Timeout Tracking (`link.rs`)

Current: uses `spawn_local` + `sleep_ms` (JS-dependent).

New: use `LocalExecutor::spawn` + `CondvarSleep` on the worker.
The keep-alive timer runs as a task on the RX worker's executor.

### 10. Pipeline yield (`pipeline.rs`)

Current: `wasm_yield::yield_now()` (setTimeout).

New: `executor.yield_now()` — yields by returning Pending and immediately
re-queueing the task. Pure Rust, no JS.

## Implementation Order

### Phase 1: LocalExecutor

File: `commons/zenoh-runtime/src/executor.rs` (new)

Implement the core executor:
- `LocalExecutor::new()`
- `LocalExecutor::spawn()`
- `LocalExecutor::pump()`
- `LocalExecutor::block_on()`
- `LocalExecutor::run()` (blocking main loop for workers)
- Task/Waker types that use Condvar::notify

Test: standalone Rust test (not WASM) to verify executor correctness.

### Phase 2: Wire into Workers

File: `commons/zenoh-runtime/src/wasm_threaded.rs`

- Store `LocalExecutor` as thread-local in each worker
- Change `__zenoh_worker_entry` to create executor and call `run()`
- Change `spawn()` to send tasks that use executor instead of `spawn_local`
- Change `block_in_place()` to call `executor.block_on()`
- Remove `spawn_local`, `setTimeout`, `setInterval` from worker paths

### Phase 3: Pure-Rust Sleep/Yield

File: `commons/zenoh-runtime/src/wasm_yield.rs`

- Add `CondvarSleep` future for threadpool workers
- Keep JS-based `sleep_ms` for I/O worker and main thread
- Gate with `cfg(feature = "wasm-threads")`

### Phase 4: I/O Worker Separation

File: `commons/zenoh-runtime/src/wasm_threaded.rs`

- Acceptor worker keeps `spawn_local` (JS-based) entry point
- Other 4 workers use `LocalExecutor` entry point
- Factor init into `init_io_worker()` vs `init_compute_worker()`

### Phase 5: Remove Cross-Worker Hacks

- Remove `WAKER_REGISTRY`, `start_poll_nudge`, `schedule_waker_repoll`
- Remove `cross_worker_recv` wrapper
- Remove setTimeout-based JoinHandle re-poll
- JoinHandle uses pure executor-based waking

### Phase 6: Test and Verify

- Tests 1-4 should pass immediately
- Test 5 (session open) should pass with pure-Rust executor
- All 7 standard tests pass (non-threaded path unchanged)
- Native build unaffected

## What Does NOT Change

- **Main thread**: continues to use `spawn_local` (JS event loop). User code
  that calls `zenoh::open().await` from the main thread works as before.
- **I/O Worker (Acceptor)**: continues to use `spawn_local` for JS callbacks.
- **Non-threaded WASM mode**: entirely unchanged (feature-gated).
- **Native mode**: entirely unchanged.
- **WebSocket link architecture**: already correct (channels + I/O dispatch).
- **`Date.now()` for Instant**: kept — it's a synchronous FFI call.

## Current File State (post-implementation, 2026-07-06)

Key files and their roles — read these first when picking up:

| File | Role | State |
|------|------|-------|
| `commons/zenoh-runtime/src/lib.rs` | Feature-gates `wasm.rs` vs `executor.rs`+`wasm_threaded.rs` | Done |
| `commons/zenoh-runtime/src/wasm.rs` | Single-threaded WASM runtime (default, no feature) | Stable, working |
| `commons/zenoh-runtime/src/executor.rs` | LocalExecutor: Condvar wakers, timer queue, block_on/run | Done |
| `commons/zenoh-runtime/src/wasm_threaded.rs` | Threaded runtime: executor on compute workers, Acceptor on JS loop, `spawn_on_current`, `recv_async_anywhere`, setTimeout self-repoll for JS threads | Done |
| `commons/zenoh-runtime/src/wasm_yield.rs` | sleep/yield: executor timer queue on compute workers, setTimeout on JS threads; Instant (Date.now) | Done |
| `commons/zenoh-runtime/src/compat.rs` | AsyncMutex/RwLock platform aliases | Done (tokio::sync for wasm-threads) |
| `commons/zenoh-task/src/wasm.rs` | CancellationToken + TaskController (routes via ZRuntime::spawn) | Done |
| `io/zenoh-links/zenoh-link-ws/src/unicast_wasm.rs` | WebSocket link: I/O on Acceptor, `recv_async_anywhere` everywhere, SAB-safe send (copies to non-shared buffer) | Done |
| `io/zenoh-transport/src/unicast/universal/link.rs` | Keep-alive timeout tracker via `spawn_on_current` + sleep_ms | Done |
| `io/zenoh-transport/src/common/pipeline.rs` | TX pipeline yield (executor-aware yield_now) | Done |
| `examples/wasm-threaded/` | Standalone test page with COEP server + headless Chrome runner | 6/6 pass (incl. session open + pub/sub roundtrip) |
| `examples/wasm-client/` | Original single-threaded example | Working |
| `tests/wasm/` | Automated wasm-pack tests (non-threaded) | 7/7 pass |
| `ros-z-wasm/examples/wasm-demo-threaded/` | hiroz on wasm-threads ↔ ROS 2 Jazzy | 3/3 pass, bidirectional |

### Build configurations

```sh
# Non-threaded WASM (default, all tests pass)
cargo build --target wasm32-unknown-unknown -p zenoh --no-default-features --features transport_ws

# Threaded WASM (needs atomics + build-std)
# Use examples/wasm-threaded/.cargo/config.toml which has the flags
cd examples/wasm-threaded
cargo build --target wasm32-unknown-unknown --release

# Native (unaffected by all changes)
cargo build -p zenoh

# Run standard WASM tests
cd tests/wasm && nix-shell -p geckodriver --run "wasm-pack test --headless --firefox"

# Run threaded tests headless (needs COEP server + zenohd)
cd examples/wasm-threaded
./build.sh                                    # cargo build + wasm-bindgen 0.2.117
python3 serve.py 8082 &                       # COOP/COEP headers
../../target/release/zenohd -l ws/127.0.0.1:7448 &
node run_headless.mjs                         # 6/6 expected
```

### Git log (implementation milestones, most recent first)

```
d18c6aee8 docs: hiroz verified on wasm-threads (bidirectional ROS 2 interop)
23a0e7611 wasm32: pure-Rust LocalExecutor on compute workers + SAB-safe WebSocket send
62e430397..2863fcaf1 Merge WASM support onto zenoh 1.9.0 (+fixes)
6510fbd docs: pure-Rust threadpool architecture for WASM workers
2a37d16 wasm32: cross-worker waker registry + I/O dispatch improvements
6220889 wasm32: dispatch WebSocket I/O to dedicated Acceptor worker
474d0074..a51a7f0 SharedArrayBuffer threadpool infrastructure
```

## Risk Assessment

| Risk | Severity | Mitigation |
|------|----------|------------|
| LocalExecutor bugs (starvation, deadlock) | High | Extensive unit tests, match tokio's LocalSet semantics |
| Send bounds on executor tasks | Medium | LocalExecutor is !Send (thread-local), tasks don't need Send for local spawning. Cross-worker dispatch still uses Send. |
| Performance of pump loop | Low | Only polls ready tasks (waker-driven), not all tasks |
| Condvar timeout precision | Low | 1ms timeout is fine for throughput; latency-sensitive paths use immediate wake |
| Compatibility with flume | Low | flume's wakers are standard Rust wakers — work with any executor |
