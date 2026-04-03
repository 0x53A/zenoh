# Zenoh WASM Port

## Goal
Port zenoh to compile for `wasm32-unknown-unknown` with WebSocket transport,
enabling a browser-based zenoh client that can connect to a zenoh router.

## Status

### Building for WASM (wasm32-unknown-unknown)
- [x] zenoh-result
- [x] zenoh-collections (needed getrandom 0.3 wasm_js feature)
- [x] zenoh-buffers
- [x] zenoh-keyexpr (has existing `js` feature)
- [x] zenoh-protocol
- [x] zenoh-codec
- [x] zenoh-crypto
- [x] zenoh-runtime (split into native.rs/wasm.rs, wasm uses spawn_local)
- [x] zenoh-core
- [ ] zenoh-sync — `event_listener::wait()` is blocking, doesn't exist on WASM
- [ ] zenoh-task — uses `tokio::task::JoinHandle`, `tokio_util::CancellationToken`, `tokio::select!`
- [ ] zenoh-util — `home`, `shellexpand`, `libloading`, tokio net
- [ ] zenoh-config — depends on zenoh-util
- [ ] zenoh-link-commons — `socket2`, tokio net/fs
- [ ] zenoh-link-ws — tokio-tungstenite → needs web-sys WebSocket for WASM
- [ ] zenoh-transport — heavy tokio usage, feature-gated transports
- [ ] zenoh (main crate)

### Approach
- `cfg(target_arch = "wasm32")` gates — keep native code untouched
- Re-export compatible types from zenoh-runtime (JoinHandle, etc.)
- On WASM: single-threaded, wasm-bindgen-futures, flume channels
- Only `transport_ws` needed for WASM target
- Replace tokio-tungstenite with web-sys WebSocket on WASM

### Key Decisions
- ZRuntime on WASM: all variants map to the same single-threaded executor
- JoinHandle on WASM: flume channel-based, compatible Future impl
- block_in_place: panics on WASM (callers must be gated)
- getrandom: v0.2 uses `js` feature, v0.3 uses `wasm_js` cfg + feature
