# Zenoh WASM Port

## Goal
Port zenoh to compile for `wasm32-unknown-unknown` with WebSocket transport,
enabling a browser-based zenoh client that can connect to a zenoh router.

## Status

### Building for WASM (wasm32-unknown-unknown)

#### Commons (all done)
- [x] zenoh-result
- [x] zenoh-collections (needed getrandom 0.3 wasm_js feature)
- [x] zenoh-buffers
- [x] zenoh-keyexpr (has existing `js` feature)
- [x] zenoh-protocol
- [x] zenoh-codec
- [x] zenoh-crypto
- [x] zenoh-runtime (split into native.rs/wasm.rs, wasm uses spawn_local)
- [x] zenoh-core
- [x] zenoh-sync (blocking wait methods gated out)
- [x] zenoh-task (split into native/wasm, custom CancellationToken for wasm)
- [x] zenoh-util (platform-specific modules gated out)
- [x] zenoh-config (LibLoader gated out, LibSearchDirs kept with empty default)

#### I/O Layer (next)
- [ ] zenoh-link-commons — needs heavy refactoring:
  - socket2, tokio net/fs are core dependencies
  - Trait definitions (LinkUnicast, etc.) needed by transport layer
  - Strategy: gate out socket-specific code, keep trait abstractions
- [ ] zenoh-link-ws — the target transport for WASM
  - Native: tokio-tungstenite (keep as-is)
  - WASM: web-sys::WebSocket (new implementation)
- [ ] zenoh-link (aggregator) — compile with only transport_ws on WASM
- [ ] zenoh-transport — heavy tokio, needs selective gating

#### Top Level
- [ ] zenoh (main crate)

### Architecture Notes
- ZRuntime on WASM: all variants map to single-threaded wasm-bindgen-futures executor
- JoinHandle on WASM: flume channel-based, compatible Future impl
- CancellationToken on WASM: AtomicBool-based (from zenoh-task wasm module)
- block_in_place: panics on WASM (callers must be gated)
- getrandom: v0.2 uses `js` feature, v0.3 uses `wasm_js` cfg + feature

### Next Steps for I/O Layer
The I/O layer is the hardest part. Two approaches:

**Option A: Refactor zenoh-link-commons**
Gate out socket2/tokio-net deps, keep trait definitions.
Pro: minimal code duplication. Con: lots of cfg gates in existing code.

**Option B: Thin WASM transport crate**
Create a new `zenoh-link-ws-wasm` that implements link traits directly
using web-sys WebSocket, bypassing zenoh-link-commons entirely.
Pro: clean separation. Con: duplicates some trait definitions.

Recommendation: Option A — the traits are stable and most impls are
already feature-gated (quic, tls). Adding a wasm gate is consistent.
