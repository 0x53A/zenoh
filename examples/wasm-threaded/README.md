# zenoh WASM — Multi-threaded Test Page

Exercises the `wasm-threads` runtime: SharedArrayBuffer Web Workers where the
compute workers (app/tx/rx/net) run a pure-Rust `LocalExecutor` and the
Acceptor worker owns all WebSocket I/O on a JS event loop.

## Tests (6)

1. Cross-worker spawn returns a value
2. Spawn on multiple runtime variants
3. `zenoh::Config` creation on a worker
4. `block_in_place` resolved by a cross-worker wake (executor pump + Condvar)
5. Session open against zenohd (the historic blocker)
6. Pub/sub roundtrip through the router

## Build & run

Requires nightly Rust with `rust-src` (build-std; all flags live in
`.cargo/config.toml`) and wasm-bindgen CLI 0.2.117 (matching Cargo.lock —
`build.sh` finds it in the wasm-pack cache).

```sh
./build.sh

# Router (from repo root)
../../target/release/zenohd -l ws/127.0.0.1:7448 &

# SharedArrayBuffer needs cross-origin isolation → COOP/COEP server
python3 serve.py 8082 &

# Headless (Chrome via puppeteer-core), or open http://localhost:8082
node run_headless.mjs
```

Tracing goes to the browser console (`tracing-wasm`, INFO by default —
raise the level in `src/lib.rs` when debugging transport issues).

## Files

- `src/lib.rs` — test sequence (`run_threaded_test`)
- `.cargo/config.toml` — atomics/shared-memory/build-std flags incl.
  `--export=__heap_base` (wasm-bindgen's threading pass needs it)
- `serve.py` — static server adding COOP/COEP headers
- `run_headless.mjs` — puppeteer-core runner, exits nonzero on FAIL
- `test_headless.html` / `index.html` — auto-run page
