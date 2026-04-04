# Zenoh WASM Port

Port of Eclipse Zenoh to `wasm32-unknown-unknown` with WebSocket transport,
enabling browser-based zenoh clients via Web Workers.

## Status: Working prototype

Pub/sub works end-to-end: browser ↔ router ↔ CLI / other browsers.
See [KNOWN_ISSUES.md](KNOWN_ISSUES.md) for limitations.

## Documentation

- [IMPLEMENTATION.md](IMPLEMENTATION.md) — Detailed implementation strategy,
  phase by phase, with rationale for each design decision
- [KNOWN_ISSUES.md](KNOWN_ISSUES.md) — Known issues, weaknesses, and
  testing gaps with severity levels and fix suggestions

## Quick start

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

## Architecture

```
Browser Tab                     Server
+------------------+           +------------------+
| Main Thread (UI) |           |                  |
|   start_main()   |           |     zenohd       |
|   postMessage ◄──┼──────────┼── WebSocket ──────┤
|                  |           |                  |
| Web Worker       |           |  (optional)      |
|   start_worker() |           | zenoh-plugin-    |
|   zenoh session  |           |   ros2dds        |
|   transport      |           |     ↕ DDS        |
+------------------+           +------------------+
```

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
