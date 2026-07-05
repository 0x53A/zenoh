# Zenoh WASM Port — Known Issues & Weaknesses

## Critical (will cause problems in production)

### 1. ~~`Session::Drop` calls `.wait()` synchronously~~ FIXED

**Fixed:** On WASM, `Drop` now spawns the close future via
`ZRuntime::Application.spawn()` instead of blocking. The WebSocket close
frame is sent asynchronously; if the worker terminates first, the browser
cleans up the connection.

### 2. ~~No WebSocket reconnection~~ FIXED

**Fixed:** Zenoh's orchestrator already has reconnection with exponential
backoff (`peers_connector_retry`), which works on WASM via
`wasm_yield::sleep_ms`. Added an `onclose` handler to the WebSocket that
drops the recv channel sender, so the transport layer detects disconnection
immediately and triggers reconnection.

### 3. ~~`Closure::forget()` memory leaks~~ FIXED

**Fixed:** The `onmessage` and `onerror` closures are now stored in the
`LinkUnicastWs` struct and dropped when the link is closed/dropped, instead
of being leaked via `Closure::forget()`. One-shot closures (`onopen`,
`onclose`) are still forgotten since they fire once and are cleared.

### 4. ~~No WSS (TLS) support~~ FIXED

**Fixed:** The WASM WebSocket link now detects `wss/` endpoint prefixes
and generates `wss://` URLs. The browser's `WebSocket` handles TLS
transparently.

## Moderate (will cause issues in specific scenarios)

### 5. `block_in_place` single-poll limitation (single-threaded mode only)

With `wasm-threads`, `block_in_place` on compute workers now blocks properly:
it delegates to the worker's `LocalExecutor::block_on`, which pumps the
worker's other tasks between Condvar waits (matching tokio semantics).

In single-threaded mode (default), the old limitation remains:
`ZRuntime::block_in_place` polls the future exactly once with a noop
waker. If the future resolves immediately (synchronous operations), it works.
If the future needs multiple polls (async I/O), it panics.

**Currently affected:**
- `OpenBuilder` — FIXED (has async `IntoFuture` on WASM)
- Most other builders (subscriber, publisher, etc.) — work fine because
  their `wait()` implementations are synchronous

**Potentially affected:**
- Any new code added to zenoh that makes `.wait()` implementations async
- Custom plugins or extensions that use `block_in_place`

**Fix:** Audit all `block_in_place` call sites. For any that need async I/O,
provide WASM-specific async alternatives (like we did for `OpenBuilder`).

### 6. ~~`std::time::Instant` not universally replaced~~ FIXED

**Fixed:** Audited all `Instant` usage. Added `cfg` guards to
`multicast/link.rs` and added `checked_sub` to the WASM `Instant`
implementation. All compiled crates now use the WASM-compatible `Instant`
on `wasm32` targets.

### 7. Timer precision

Our WASM `Instant` uses `Date.now()` (millisecond precision). The native
`std::time::Instant` has nanosecond precision. Operations that depend on
sub-millisecond timing may behave differently.

Additionally, `setTimeout(0)` in browsers has a minimum delay of ~4ms due to
browser throttling. This affects our `yield_now()` and `sleep_ms(0)` calls.

**Impact:** Throughput may be lower than native due to coarser scheduling.
Pipeline batching behavior may differ.

### 8. No multicast / UDP scouting

Completely disabled on WASM. The client must be configured with explicit
`connect/endpoints` — it cannot discover routers via multicast.

**Impact:** No automatic router discovery. The application must know the
router address at build time or configuration time.

**Fix:** Not fixable (browsers don't have UDP). Could implement HTTP-based
discovery as an alternative.

## Minor (polish items)

### 9. ~~Worker protocol is JSON strings only~~ FIXED

**Fixed:** Added `PutBinary` message type with base64-encoded payloads.
`FromWorker::Sample` and `FromWorker::Reply` now include a `payload_binary`
field with raw bytes (base64-encoded in JSON).

### 10. ~~No query/get support in worker protocol~~ FIXED

**Fixed:** Added `Get`, `DeclareQueryable`, `UndeclareQueryable`, `Reply`
to `ToWorker`; added `Reply`, `Query` to `FromWorker`. Worker handles
get (collecting replies) and queryable declaration. Async query reply
storage is stubbed (needs query map for full support).

### 11. ~~Warnings from deprecated wasm-bindgen APIs~~ FIXED

**Fixed:** Cleaned up unused imports in `zenoh-link-ws/src/lib.rs` and
`zenoh-runtime/src/wasm_yield.rs`. The `WorkerOptions` deprecation warning
was not actually present in the current code.

### 12. Transport features degraded on WASM

Several transport features work differently or are disabled:
- **Timeout-based operations:** Use `setTimeout` instead of tokio timers;
  less precise, browser may throttle inactive tabs
- **Multicast transport:** Completely disabled
- **QoS priority queues:** Work but scheduling may differ from native
- **Compression:** Should work (pure Rust lz4_flex) but untested
- **Shared memory:** Disabled (no OS-level shared memory in WASM)

### 13. ~~CancellationToken waker accumulation~~ FIXED

**Fixed:** Each `CancelledFuture` now has a dedicated slot index in the
waker vec. Re-polling replaces the waker in-place instead of appending,
preventing unbounded growth.

### 14. Background-tab timer throttling can drop the session (single-threaded)

Single-threaded mode runs keepalives on `setTimeout`. Browsers throttle
timers in hidden tabs (Chrome's intensive throttling: chained timers fire
about once per minute after a few minutes in the background). With the
default 4s keep-alive / 10s lease, a backgrounded tab will likely miss its
lease and get disconnected; reconnection then only happens when the tab is
foregrounded again.

`wasm-threads` largely avoids this: dedicated workers are throttled far
less, and the compute workers' timers are `Atomics.wait`-based (executor
timer queue), which browsers do not throttle. **Untested so far** — verify
backgrounded-tab behavior explicitly in both modes (candidate for the
benchmark session).

### 15. SharedArrayBuffer-backed views rejected by some JS APIs (wasm-threads)

With `+atomics`, WASM linear memory is a SharedArrayBuffer. Several browser
APIs throw a TypeError when handed a view of it: `WebSocket.send()`,
`postMessage` (non-transfer), `fetch` bodies, `TextDecoder.decode`,
`crypto.getRandomValues`. wasm-bindgen's `&[u8]` marshalling passes such
views (`getUint8ArrayMemory0().subarray(...)`).

Fixed for `WebSocket.send()` in `unicast_wasm.rs` (copy into a fresh
non-shared `Uint8Array` first) — this silent TypeError was one of the two
root causes of the threaded session-open hang. **Watch for the same pattern
when adding new web-sys calls on the wasm-threads path**, and never discard
`Result`s from web-sys send/write APIs.

## Testing

**Single-threaded** (`tests/wasm/`, wasm-pack + Firefox):
- **`basic.rs`** (5 tests) — config, key expressions, ZBytes, ZenohId,
  SampleKind — no network needed
- **`session.rs`** (2 tests) — session open/close, pub/sub roundtrip —
  requires zenohd on `ws/127.0.0.1:7448`

```sh
cd tests/wasm
nix-shell -p geckodriver --run "wasm-pack test --headless --firefox"
```

**Multi-threaded** (`examples/wasm-threaded/`, headless Chrome — wasm-pack
can't serve the COOP/COEP headers SharedArrayBuffer needs):
- 6 tests: cross-worker spawn, multi-runtime spawn, config on worker,
  block_in_place with cross-worker wake, session open, pub/sub roundtrip

```sh
cd examples/wasm-threaded
./build.sh && python3 serve.py 8082 &
../../target/release/zenohd -l ws/127.0.0.1:7448 &
node run_headless.mjs
```

**hiroz / ROS 2 interop** (in `ros-z-wasm/examples/`):
- `wasm-demo/` — single-threaded, wasm-pack tests incl. e2e vs ROS 2 Jazzy
- `wasm-demo-threaded/` — threaded, headless Chrome, bidirectional /chatter
