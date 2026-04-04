# Zenoh WASM Port — Known Issues & Weaknesses

## Critical (will cause problems in production)

### 1. `Session::Drop` calls `.wait()` synchronously

**File:** `zenoh/src/api/session.rs:780`

When a `Session` is dropped, it calls `self.close().wait()` which goes through
`block_in_place`. If the close operation needs async I/O (sending a close frame
to the router), it will either:
- Succeed if the future resolves on first poll (likely for simple close)
- Panic if the future returns Pending

**Impact:** Session cleanup may be incomplete. The router will eventually notice
the connection is gone via lease expiry.

**Fix:** Override `Drop` on WASM to use `spawn_local` for async close, or
accept that the WebSocket close happens at the JS level when the worker terminates.

### 2. No WebSocket reconnection

If the WebSocket connection drops (network hiccup, router restart), the zenoh
session dies permanently. There is no automatic reconnection mechanism.

**Impact:** Long-running browser clients will eventually lose connectivity.

**Fix:** Implement reconnection logic in the Web Worker — detect connection
loss, re-open session, re-declare subscribers. Or implement at the WebSocket
link level with exponential backoff.

### 3. `Closure::forget()` memory leaks

Every WebSocket connection leaks 3 JS closures (`onmessage`, `onerror`,
`onopen`) via `Closure::forget()`. This prevents use-after-drop errors but
means the closures are never freed.

**Impact:** For single long-lived connections, negligible. For apps that
frequently reconnect, memory grows without bound.

**Fix:** Store closures in a side-table keyed by WebSocket identity, and
clean up on `close()`. Or use weak references if wasm-bindgen supports them.

### 4. No WSS (TLS) support

The WASM WebSocket link only creates `ws://` URLs. Browser `WebSocket` natively
supports `wss://` — just needs the URL scheme change.

**Impact:** No encryption in transit. Browsers may block `ws://` on HTTPS pages
due to mixed content policy.

**Fix:** Detect `wss/` locator prefix and generate `wss://` URL. The
`web_sys::WebSocket` handles TLS transparently.

## Moderate (will cause issues in specific scenarios)

### 5. `block_in_place` single-poll limitation

`ZRuntime::block_in_place` on WASM polls the future exactly once with a noop
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

### 6. `std::time::Instant` not universally replaced

We replaced `Instant` with `zenoh_runtime::wasm_yield::Instant` (backed by
`Date.now()`) in known hot paths:
- `pipeline.rs` (LOCAL_EPOCH, deadline tracking)
- `link.rs` (TimeoutTracker)
- `downsampling.rs` (message rate limiting)

But there may be other uses of `std::time::Instant` in less-common code paths
(error handling, diagnostics, tests) that would panic at runtime.

**Fix:** Global search for `std::time::Instant` and `Instant::now()` across
all compiled crates. Consider a crate-level type alias.

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

### 9. Worker protocol is JSON strings only

The `postMessage` protocol between main thread and worker serializes everything
as JSON strings, including binary payloads. This adds serialization overhead
and prevents zero-copy transfer.

**Fix:** Use `postMessage` with `Transferable` objects (`ArrayBuffer`) for
binary payloads. Define a binary protocol or use `postMessage` with structured
clone for mixed string/binary data.

### 10. No query/get support in worker protocol

The `ToWorker`/`FromWorker` protocol supports `open`, `subscribe`, `put` but
not `get` (queries) or `queryable` (query handlers).

**Fix:** Add `Get { id, selector }` and `Reply { query_id, ... }` message types.

### 11. Warnings from deprecated wasm-bindgen APIs

`WorkerOptions::type_()` is deprecated, `wasm_bindgen` init function uses
deprecated parameters. These are cosmetic but noisy.

### 12. Transport features degraded on WASM

Several transport features work differently or are disabled:
- **Timeout-based operations:** Use `setTimeout` instead of tokio timers;
  less precise, browser may throttle inactive tabs
- **Multicast transport:** Completely disabled
- **QoS priority queues:** Work but scheduling may differ from native
- **Compression:** Should work (pure Rust lz4_flex) but untested
- **Shared memory:** Disabled (no OS-level shared memory in WASM)

### 13. CancellationToken waker accumulation

Our WASM `CancellationToken` stores wakers in a `Vec<Waker>` that grows
each time `cancelled()` is polled. If a future repeatedly polls the
`CancelledFuture` without ever cancelling, the waker list grows unboundedly.

**Fix:** Deduplicate wakers or use a bounded structure. In practice this
is unlikely to be an issue because `cancelled()` futures are typically
polled once and then parked.

## Testing gaps

- No automated tests for the WASM build
- No integration test that verifies pub/sub through a router
- No stress testing of WebSocket link under load
- No testing of connection loss and recovery scenarios
- Only tested in Firefox and Chrome; Safari, Edge untested
- Only tested with the example router (v1.8.0); compatibility with other
  zenoh versions unknown
