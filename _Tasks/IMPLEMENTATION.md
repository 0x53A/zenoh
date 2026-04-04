# Zenoh WASM Port — Implementation Strategy

## Overview

This document describes the approach taken to port Eclipse Zenoh (v1.8.0) from
a native Rust library to compile and run on `wasm32-unknown-unknown`, targeting
browser environments via Web Workers and WebSocket transport.

The work was done bottom-up, starting from leaf crates with no platform
dependencies and working up through the dependency tree to the main `zenoh`
crate.

## Phase 1: Leaf Crates (Protocol Layer)

**Goal:** Get the core protocol, codec, and data structure crates compiling.

**Crates:** zenoh-result, zenoh-collections, zenoh-buffers, zenoh-keyexpr,
zenoh-protocol, zenoh-codec, zenoh-crypto

**Issues encountered:**
- `getrandom` v0.2 and v0.3 both need WASM-specific configuration
  - v0.2: enable `js` feature (for `rand` → `getrandom`)
  - v0.3: set `getrandom_backend="wasm_js"` rustc cfg AND enable `wasm_js`
    feature (pulled transitively by `ahash`)
- Solution: `.cargo/config.toml` for the cfg flag, workspace-level feature
  enablement for v0.2, target-specific dep for v0.3 in zenoh-collections

**Strategy:** These crates were already mostly platform-independent. Only the
random number generation needed WASM-specific configuration.

## Phase 2: Runtime Layer

**Goal:** Replace the tokio-based async runtime with WASM-compatible alternatives.

**Crates:** zenoh-runtime, zenoh-core, zenoh-sync, zenoh-task

### zenoh-runtime

The central challenge. The native runtime manages a pool of tokio multi-threaded
runtimes (`ZRuntime` enum with Application, Acceptor, TX, RX, Net variants).

**Strategy:** Split into `native.rs` and `wasm.rs` modules, selected by
`cfg(target_arch = "wasm32")`.

**WASM implementation:**
- `ZRuntime::spawn()` → `wasm_bindgen_futures::spawn_local()` with a
  flume channel-based `JoinHandle<T>` for awaiting results
- `ZRuntime::block_in_place()` → polls the future once with a noop waker;
  returns if Ready, panics if Pending. This works for synchronous operations
  but cannot drive truly async futures to completion.
- All `ZRuntime` variants map to the same single-threaded executor
- The `RegisterParam` / `GenericRuntimeParam` derive macros (which generate
  code referencing `ron`, `env`, tokio `Runtime`) are kept in `native.rs` only.
  The WASM `ZRuntime` is a plain enum without these macros.
- Re-exports `JoinHandle` from both platforms so consumers use
  `zenoh_runtime::JoinHandle`

### zenoh-task

**Strategy:** Split into `native.rs` and `wasm.rs`.

**WASM implementation:**
- `CancellationToken`: custom implementation using `AtomicBool` + waker
  registration (not busy-polling). The `cancel()` method wakes all registered
  futures. This replaces `tokio_util::sync::CancellationToken`.
- `TaskController` / `TerminatableTask`: simplified versions using
  `ZRuntime::spawn` and `AtomicUsize` task counting instead of
  `tokio_util::task::TaskTracker`.
- `futures::select!` instead of `tokio::select!` for cancellation races
  (though `tokio::select!` also works with tokio's `macros` feature on WASM)

**Critical bug found and fixed:** The initial `CancelledFuture` implementation
used `cx.waker().wake_by_ref()` on every poll when not cancelled, creating a
100% CPU busy-loop in every `tokio::select!` branch. Fixed by storing wakers
and only waking on actual cancellation.

### zenoh-sync

**Strategy:** Gate blocking methods behind `cfg(not(target_arch = "wasm32"))`.

- `Waiter::wait()`, `wait_deadline()`, `wait_timeout()` use
  `event_listener::EventListener::wait()` which blocks the thread — impossible
  on WASM. These methods are gated out.
- `Waiter::wait_async()` remains available on all platforms.

### zenoh-core

No changes needed — `ResolveFuture::IntoFuture` already correctly returns the
inner future for `.await`. The `Wait::wait()` implementation calls
`block_in_place` which works for sync operations on WASM.

## Phase 3: Configuration and Utility Layer

**Crates:** zenoh-util, zenoh-config

### zenoh-util

**Strategy:** Gate out platform-specific modules on WASM.

Gated out:
- `lib_loader` (uses `libloading` for dynamic plugin loading)
- `net` (uses `pnet_datalink`, `tokio::net` for network interface enumeration)
- `timer` (uses `tokio::time`, `tokio::runtime::Handle`)
- `zenoh_home()` (uses `home` crate for filesystem paths)

Kept:
- `lib_search_dirs` (type definitions needed by zenoh-config, but with empty
  default and no filesystem resolution on WASM)
- `log`, `ffi`, `time_range`, `concat_enabled_features` macro

### zenoh-config

Minimal changes — gated `LibLoader` usage and the `libloader()` method.

## Phase 4: I/O Layer

**Crates:** zenoh-link-commons, zenoh-link, zenoh-link-ws, zenoh-transport

### zenoh-link-commons

**Strategy:** Gate out socket-specific modules, keep trait definitions.

- `dscp.rs` (DSCP/QoS socket options) — gated out
- `listener.rs` (IP-based listener management) — gated out
- `tcp.rs` (TCP utilities) — gated out
- `multicast.rs` — kept (trait definitions are platform-independent)
- `unicast.rs` — kept (core `LinkUnicastTrait`, `LinkManagerUnicastTrait`)
- `socket2` and `tokio` with `net`/`fs` — moved to target-specific deps

The trait definitions (`LinkUnicastTrait`, `LinkManagerUnicastTrait`) use
`#[async_trait]` with `Send + Sync` bounds. On WASM, JS types are `!Send`,
so link implementations use `unsafe impl Send/Sync` (safe because WASM is
single-threaded).

### zenoh-link-ws (WebSocket transport)

**Strategy:** Split into `unicast_native.rs` (tokio-tungstenite) and
`unicast_wasm.rs` (web-sys::WebSocket).

**WASM WebSocket implementation:**
- Uses `web_sys::WebSocket` with `BinaryType::Arraybuffer`
- JS callbacks (`onmessage`, `onerror`, `onopen`) bridge to Rust via
  `flume` channels — callbacks send to channel, async Rust reads from channel
- `SendWrapper<T>` pattern wraps `!Send` JS types (`WebSocket`, `Closure`)
  with `unsafe impl Send/Sync`
- All JS `Closure`s are `.forget()`-ed to prevent use-after-drop errors
  (the closures live as long as the WebSocket)
- The `setup_ws()` function creates the WebSocket and all callbacks
  synchronously (no `.await`) to keep `!Send` types out of async futures
- `LinkManagerUnicastWs` on WASM: `new_link()` connects, no listener support

**Key challenge:** `#[async_trait]` generates `Pin<Box<dyn Future + Send>>`.
The future body captures `&self` references. If `self` contains `!Send` types
(even wrapped in SendWrapper), the future's captured references can fail Send
analysis. Solution: ensure all `!Send` types are wrapped in `SendWrapper`
BEFORE any `.await` point, and use `tokio::sync::Mutex` (Send-safe guard)
instead of `futures::lock::Mutex` for internal state.

### zenoh-transport

**Strategy:** Move `tokio` and `tokio-util` to target-specific deps. On WASM,
use `tokio` with only `macros` + `sync` features (no `net`/`fs`/`rt-multi-thread`).

**Changes across 7+ source files:**

1. **`tokio::time::*` replacement:**
   - Created `zenoh_runtime::wasm_yield` module with `yield_now()` and
     `sleep_ms()` using `setTimeout` + flume channels
   - These produce `Send` futures (unlike `gloo-timers` which captures
     `!Send` JsValue)
   - Used for retry delays, event loop yielding, and timeout tracking

2. **`tokio::task::block_in_place` / `spawn_blocking`:**
   - Gated with `cfg(not(wasm32))`, WASM alternatives either panic or
     call the operation directly (for `spawn_blocking` → direct call)

3. **`tokio::task::yield_now()`:**
   - Replaced with `zenoh_runtime::wasm_yield::yield_now()` which uses
     `setTimeout(0)` for a proper macrotask yield

4. **`std::time::Instant`:**
   - Not available on `wasm32-unknown-unknown` (panics at runtime)
   - Created `zenoh_runtime::wasm_yield::Instant` using `Date.now()`
   - Implements `Add`, `AddAssign`, `Sub`, `SubAssign` for `Duration`
   - Used in `pipeline.rs`, `link.rs`, `downsampling.rs`

5. **`tokio_util::sync::CancellationToken`:**
   - All imports changed to `zenoh_task::CancellationToken` which
     re-exports the correct type per platform

6. **`TaskTracker::spawn_on(&rt)`:**
   - On WASM, `ZRuntime` doesn't deref to `Handle`, so `spawn_on`
     replaced with plain `spawn` (everything runs on one thread anyway)

7. **Keepalive / lease timeout tracking:**
   - `TimeoutTracker` on native uses `tokio::spawn` + `tokio::time::sleep_until`
   - On WASM: `wasm_bindgen_futures::spawn_local` + `sleep_ms` loop
   - Critical for maintaining connections — without keepalive, the router
     closes the connection after the lease period (~10 seconds)

## Phase 5: Main zenoh Crate

**Strategy:** Gate out platform-specific code, provide WASM alternatives
for critical paths.

### Orchestrator (scouting/discovery)

- UDP scouting completely gated out on WASM (no UDP sockets)
- WASM `start_client()`: skips scouting, just `connect_peers()` to
  configured endpoints
- WASM `start_peer()` / `start_router()`: simplified or gated
- Retry delays use `sleep_ms` instead of `tokio::time::sleep`

### Session opening

- `OpenBuilder::IntoFuture` on native: `ready(self.wait())` — synchronous
- On WASM: truly async `Box::pin(async { ... runtime.build().await ... })`
- This is critical — session creation involves async WebSocket connection
  and zenoh protocol handshake that needs multiple event loop ticks

### Plugin system

- `zenoh-plugin-trait`: dynamic plugin loading (`libloading`) gated out
- `PluginsManager::dynamic()` gated out, `static_plugins_only()` available

### Other changes

- `socket2` moved to target-specific deps
- `tokio::time::*` usage in routing, interests, queries, downsampling
  replaced with cfg-gated WASM alternatives
- All `tokio_util::sync::CancellationToken` imports → `zenoh_task::CancellationToken`
- `tokio::task::JoinHandle` → `zenoh_runtime::JoinHandle`

## Phase 6: Web Worker Architecture

**Problem:** Even with all compilation fixes, running zenoh on the browser's
main thread causes deadlocks. The transport layer's concurrent RX/TX/keepalive
tasks cannot run properly when they share the single main thread with the UI.

**Solution:** Run the zenoh session in a dedicated Web Worker.

```
Main Thread (UI)                 Web Worker Thread
+-------------------+           +----------------------+
| Dioxus/JS app     |           | zenoh session         |
| sends commands    |<--------->| transport RX/TX       |
| displays results  | postMsg   | WebSocket I/O         |
+-------------------+           +----------------------+
```

**Implementation:**
- Single WASM build (`--target no-modules`) used by both main thread and worker
- Worker loads same WASM via `importScripts()` (works in all browsers)
- Separate entry points: `start_main()` (main thread), `start_worker()` (worker)
- No `#[wasm_bindgen(start)]` — explicit initialization to avoid running
  main-thread code in the worker context (no `window` in workers)
- JSON message protocol between threads (`ToWorker` / `FromWorker` enums)
- Worker buffers messages in JS during WASM loading, replays after init
- Multiple workers supported by design (each `Worker` instance is independent)

**Message buffering:** The main thread sends commands immediately after creating
the worker, but the worker needs time to load WASM and set up its `onmessage`
handler. Solution: the worker bootstrap JS sets up a temporary message buffer
before loading WASM, then replays buffered messages after `start_worker()`
has registered its handler.
