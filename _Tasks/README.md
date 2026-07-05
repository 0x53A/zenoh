# Zenoh WASM Port

Port of Eclipse Zenoh to `wasm32-unknown-unknown` with WebSocket transport,
enabling browser-based zenoh clients via Web Workers.

## Status: Both modes working (single- and multi-threaded), zenoh 1.9.0

**Single-threaded (default):** Pub/sub works end-to-end: browser ↔ router ↔ CLI.
7 automated tests pass (5 basic + 2 session with router).

**Multi-threaded (`wasm-threads` feature):** SharedArrayBuffer Web Workers with
shared WASM memory — **6/6 tests pass including session open and a full pub/sub
roundtrip through the router.** Compute workers (app/tx/rx/net) run a pure-Rust
`LocalExecutor` (Condvar-backed wakers, pumped by `block_in_place`); only the
main thread and the Acceptor (I/O) worker keep a JS event loop. See
[THREADPOOL_ARCHITECTURE.md](THREADPOOL_ARCHITECTURE.md) — implemented 2026-07-06.

Two bugs had caused the historic "session open hangs":
1. Workers used `spawn_local` (JS microtask queue), unpumpable from
   `block_in_place` and unwakeable across workers → fixed by the LocalExecutor.
2. `WebSocket.send()` rejects SharedArrayBuffer-backed views with a TypeError
   that the write loop silently discarded (`let _ =`), so no zenoh frame ever
   hit the wire in threaded mode. Fixed by copying into a fresh non-shared
   buffer in `unicast_wasm.rs`. (Single-threaded builds have non-shared memory,
   which is why they always worked.)

**hiroz on threads (2026-07-06):** hiroz (ros-z) builds and runs against
`wasm-threads` — bidirectional browser ↔ ROS 2 Jazzy interop via rmw_zenoh
verified (see `ros-z-wasm/examples/wasm-demo-threaded/`).

**Next step:** benchmark threaded vs single-threaded throughput/latency.

## Documentation

- [IMPLEMENTATION.md](IMPLEMENTATION.md) — Detailed implementation strategy,
  phase by phase, with rationale for each design decision
- [KNOWN_ISSUES.md](KNOWN_ISSUES.md) — Known issues, weaknesses, and
  testing gaps with severity levels and fix suggestions
- [THREADPOOL_ARCHITECTURE.md](THREADPOOL_ARCHITECTURE.md) — Design +
  implementation status of the pure-Rust threadpool (SharedArrayBuffer
  workers with custom executor)

## Choosing a mode

- **Single-threaded (default):** stable toolchain, no COOP/COEP headers,
  works everywhere — but it is an audited async-only subset: anything that
  would truly block panics (`block_in_place` single-poll), some features are
  skipped (e.g. hiroz graph liveliness), and hidden-tab timer throttling can
  drop the session lease (KNOWN_ISSUES #14).
- **Multi-threaded (`wasm-threads` feature):** the full programming model —
  real blocking on workers, sync API without per-call-site audits, transport
  off the main thread, throttle-proof `Atomics.wait` timers. Requires
  nightly + build-std and a cross-origin-isolated page (COOP/COEP).

## Quick start (single-threaded)

```sh
# Build the router
cargo build --release -p zenohd

# Start router with WebSocket transport
./target/release/zenohd -l ws/0.0.0.0:7448

# Build the WASM example
cd examples/wasm-client
wasm-pack build --target no-modules --dev

# Serve the page
python -m http.server 8080

# Open http://localhost:8080 in a browser

# Send a message from CLI
cargo run --example z_put -- -e ws/127.0.0.1:7448 -k demo/example/test -p "Hello"
```

## Quick start (multi-threaded)

```sh
# Router as above, then:
cd examples/wasm-threaded
./build.sh              # nightly + build-std; flags in .cargo/config.toml
python3 serve.py 8082 & # COOP/COEP headers (SharedArrayBuffer requirement)
node run_headless.mjs   # automated 6/6, or open http://localhost:8082
```

hiroz (ros-z) demos incl. ROS 2 Jazzy interop:
`../ros-z-wasm/examples/wasm-demo/` (single-threaded, wasm-pack tests) and
`../ros-z-wasm/examples/wasm-demo-threaded/` (threaded, interactive page +
headless runner).

## Architecture (single-threaded example)

```
Browser Tab                     Server
+------------------+           +------------------+
| Main Thread (UI) |           |                  |
|   start_main()   |           |     zenohd       |
|   postMessage ◄──┼──────────┼── WebSocket ──────┤
|                  |           |                  |
| Web Worker       |           |  (optional)      |
|   start_worker() |           |  ROS 2 nodes via |
|   zenoh session  |           |  rmw_zenoh_cpp   |
|   transport      |           |  (proven e2e)    |
+------------------+           +------------------+
```

In the multi-threaded mode there is no postMessage protocol: the zenoh
session lives on SharedArrayBuffer workers inside the same tab, and the main
thread talks to it through shared-memory channels (see
`examples/wasm-threaded/` and THREADPOOL_ARCHITECTURE.md).

## Build commands

```sh
# WASM library only (no example)
cargo build --target wasm32-unknown-unknown -p zenoh --no-default-features --features transport_ws

# Native (unchanged, all features)
cargo build -p zenoh
```

## Commits (chronological)

1. getrandom fixes + zenoh-runtime native/wasm split
2. zenoh-task + zenoh-sync WASM support
3. zenoh-util + zenoh-config WASM support
4. zenoh-link-commons + zenoh-link I/O layer gating
5. zenoh-transport — tokio replaced/gated across 7+ files
6. zenoh main crate — orchestrator, scouting, timers, plugins gated
7. WebSocket transport — web-sys::WebSocket implementation
8. WASM example + Web Worker architecture
9. CancelledFuture busy-loop fix + block_in_place polling
10. Classic Workers for Firefox compat
11. Worker message buffering for reliable startup
12. WebSocket closure lifetime fixes (forget pattern)
13. std::time::Instant → WASM-compatible Instant (Date.now)
14. Async OpenBuilder + client mode on WASM
15. Keepalive timer for WASM (prevents connection timeout)
16. Automated WASM tests (basic + session/pub-sub integration)
17. WSS (TLS) support, closure memory leak fix, Session Drop fix
18. Instant audit, CancellationToken waker fix, reconnection support
19. Worker protocol: binary payloads, query/get/queryable support
20. Unused import cleanup across WASM-compiled crates
21. SharedArrayBuffer threadpool infrastructure (wasm-threads feature)
22. Channel-based WebSocket write for thread-safe I/O
23. Working cross-worker spawn + block_in_place with Condvar
24. Dedicated I/O worker (Acceptor) for WebSocket — no block_in_place
25. num_cpus fix, tokio::sync::Mutex for wasm-threads, poll nudge
26. Merge onto zenoh 1.9.0 + compilation fixes (4 commits)
27. LocalExecutor wired into compute workers; executor-based sleep/yield;
    poll-nudge/waker-registry hacks removed; SAB-safe WebSocket.send —
    threaded session open + pub/sub roundtrip pass (6/6)
