# Browser WASM branch

This branch runs the Rust Zenoh client in browsers over WS/WSS. It is a working
port and a source of reusable changes, not a proposal to merge the entire branch.
The native implementation remains available. Upstream main was merged through
`9fcd9cb5d364192c3e8a27e66de76f4bc750d1d5` (Zenoh 1.10.1).

The [integration repository](https://github.com/0x53A/hiroz-web) contains the
threaded browser harness, ROS 2 fixtures, demos and CI. Clone it recursively to
get matching Zenoh, hiroz and example revisions. See its
[validation notes](https://github.com/0x53A/hiroz-web/blob/main/docs/reviews/2026-09-23-upstream-cleanup.md)
for the checks actually run after this merge.
The subsequent [iterative review](https://github.com/0x53A/hiroz-web/blob/main/docs/reviews/2026-09-23-iterative-review.md)
covers timer ownership, remaining browser deadlines, native task shutdown and
the final pass that found no new actionable issues.

## Where to look

- `commons/zenoh-runtime`: native/runtime separation, single-thread browser
  execution, shared-memory worker execution, cancellation, monotonic timers and
  a shared timeout adapter. The `wasm-threads` feature selects the worker pool.
- `io/zenoh-links/zenoh-link-ws/src/unicast_wasm.rs`: browser-owned WebSockets,
  cross-worker reads/writes, backpressure and socket/callback cleanup.
- `io/zenoh-transport` and `zenoh/src/net/runtime`: runtime-independent waits,
  connection deadlines and retries, browser transport selection, and bounded
  publishing behavior on threads that cannot block.
- `tests/wasm`: standalone browser tests. Run
  `wasm-pack test --headless --firefox -- --test runtime` from that directory.

Threaded tests live in `examples/wasm-threaded` in the integration repository.
They require a build with atomics, shared memory and `build-std`, plus browser
cross-origin isolation. Its build script and `.cargo/config.toml` carry the
complete flags; enabling the Cargo feature alone is insufficient.

## Limits and deliberate compromises

Browser networking is WS/WSS only: no UDP scouting, listeners, serial, shared
memory transport, or dynamic native plugins. WASM low-latency transport is
explicitly rejected; the default universal transport is the supported path.
Peer/router modes do not make a browser into a listening network router.

The worker pool lives until page unload. Startup failures require a reload;
workers cannot safely be killed while holding shared Rust locks. JS-thread
cross-worker receives retain a timer-based repoll bridge. Short shared critical
sections use non-parking acquisition where browsers prohibit `Atomics.wait`.
These choices have CPU/scheduling costs and provide no hard latency guarantee.

Synchronous calls may block compute workers, but cannot wait on the page or I/O
event loop. Publishing there can fail under contention. Incoming WebSocket
backlogs are bounded; overload closes the link because the browser API exposes
no receive-side backpressure. A send acknowledgement means browser acceptance,
not remote application receipt. Background tabs can be throttled or suspended.

Native builds still use Tokio. Browser timeouts use the monotonic runtime timer;
connection, acceptance and flush deadlines are not intentionally disabled.
The old fixed WS accept-loop report is archived in the integration repository,
outside this library's upstream diff.
