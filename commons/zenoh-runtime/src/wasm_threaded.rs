//
// Copyright (c) 2024 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

//! Multi-threaded WASM runtime using Web Workers + SharedArrayBuffer.
//!
//! Each [`ZRuntime`] variant gets its own Web Worker. Workers share linear memory
//! via SharedArrayBuffer, so `std::sync::Mutex`, `Condvar`, and atomics work across
//! workers. `block_in_place` genuinely blocks via `Condvar::wait` (compiles to
//! `memory.atomic.wait32`).
//!
//! Requirements:
//! - Nightly Rust with `-Ctarget-feature=+atomics,+bulk-memory,+mutable-globals`
//! - `-Zbuild-std=std,panic_abort`
//! - Server must send COOP/COEP headers for SharedArrayBuffer access

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::task::{Context, Poll, Wake};

use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

pub const ZENOH_RUNTIME_ENV: &str = "ZENOH_RUNTIME";

// ---------------------------------------------------------------------------
// ZRuntime enum
// ---------------------------------------------------------------------------

/// [`ZRuntime`] variants — each maps to a dedicated Web Worker.
#[derive(Hash, Eq, PartialEq, Clone, Copy, Debug, Deserialize)]
pub enum ZRuntime {
    #[serde(rename = "app")]
    Application,
    #[serde(rename = "acc")]
    Acceptor,
    #[serde(rename = "tx")]
    TX,
    #[serde(rename = "rx")]
    RX,
    #[serde(rename = "net")]
    Net,
}

impl std::fmt::Display for ZRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZRuntime::Application => write!(f, "app"),
            ZRuntime::Acceptor => write!(f, "acc"),
            ZRuntime::TX => write!(f, "tx"),
            ZRuntime::RX => write!(f, "rx"),
            ZRuntime::Net => write!(f, "net"),
        }
    }
}

const NUM_WORKERS: usize = 5;

impl ZRuntime {
    fn variant_id(self) -> usize {
        match self {
            ZRuntime::Application => 0,
            ZRuntime::Acceptor => 1,
            ZRuntime::TX => 2,
            ZRuntime::RX => 3,
            ZRuntime::Net => 4,
        }
    }

    #[allow(dead_code)]
    fn from_id(id: u32) -> Self {
        match id {
            0 => ZRuntime::Application,
            1 => ZRuntime::Acceptor,
            2 => ZRuntime::TX,
            3 => ZRuntime::RX,
            4 => ZRuntime::Net,
            _ => panic!("Invalid ZRuntime variant id: {id}"),
        }
    }
}

// ---------------------------------------------------------------------------
// JoinHandle
// ---------------------------------------------------------------------------

/// A handle to a spawned task, API-compatible with tokio's JoinHandle.
pub struct JoinHandle<T> {
    rx: flume::Receiver<T>,
}

impl<T> std::fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinHandle").finish()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.rx.try_recv() {
            Ok(val) => Poll::Ready(Ok(val)),
            Err(flume::TryRecvError::Empty) => {
                if THREADED_MODE.load(Ordering::Acquire) {
                    // In threaded mode, the result may arrive from another worker.
                    // JS wakers are per-thread (spawn_local uses microtasks), so a
                    // waker from worker A can't schedule a re-poll on worker B.
                    // Solution: use setTimeout(0) to re-poll on this thread's event loop.
                    let waker = cx.waker().clone();
                    schedule_waker_repoll(waker);
                    Poll::Pending
                } else {
                    // Single-threaded: use flume's async recv which works on one thread.
                    let rx = self.rx.clone();
                    let mut fut = Box::pin(async move { rx.recv_async().await });
                    match fut.as_mut().poll(cx) {
                        Poll::Ready(Ok(val)) => Poll::Ready(Ok(val)),
                        Poll::Ready(Err(_)) => Poll::Ready(Err(JoinError)),
                        Poll::Pending => Poll::Pending,
                    }
                }
            }
            Err(flume::TryRecvError::Disconnected) => Poll::Ready(Err(JoinError)),
        }
    }
}

/// Schedule a waker to fire after yielding to the JS event loop.
/// Uses `setTimeout(0)` (~4ms in browsers) to re-poll on the current thread.
/// This bridges the gap between cross-worker flume channels and per-thread
/// JS event loops.
fn schedule_waker_repoll(waker: std::task::Waker) {
    use wasm_bindgen::closure::Closure;
    let cb = Closure::once(move || {
        waker.wake();
    });
    set_timeout(&cb, 1);
    cb.forget(); // One-shot, leaked — acceptable for waker re-polls
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = "setTimeout")]
    fn set_timeout(closure: &Closure<dyn FnMut()>, millis: i32);
}


impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        // Cannot abort spawned tasks on WASM workers
    }

    pub fn is_finished(&self) -> bool {
        !self.rx.is_empty()
    }
}

/// Error returned when a spawned task fails.
#[derive(Debug)]
pub struct JoinError;

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "task failed or was cancelled")
    }
}

impl std::error::Error for JoinError {}

// ---------------------------------------------------------------------------
// Condvar-based waker for block_in_place
// ---------------------------------------------------------------------------

struct CondvarWaker {
    woken: Mutex<bool>,
    cvar: Condvar,
}

impl Wake for CondvarWaker {
    fn wake(self: Arc<Self>) {
        let mut woken = self.woken.lock().unwrap();
        *woken = true;
        self.cvar.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Worker pool
// ---------------------------------------------------------------------------

/// A task that can be sent to a worker for execution.
/// The closure captures the future and a result sender, then calls `spawn_local`.
type BoxedTask = Box<dyn FnOnce() + Send>;

struct WorkerHandle {
    task_tx: flume::Sender<BoxedTask>,
    _worker: SendWrapper<web_sys::Worker>,
}

/// Wrapper to make JsValue-based types Send+Sync on WASM.
/// SAFETY: Web Workers are thread-affine — the wrapped JS object is only
/// accessed from the creating thread. With shared memory, this pattern
/// remains valid as long as we don't call JS methods from other workers.
struct SendWrapper<T>(T);
unsafe impl<T> Send for SendWrapper<T> {}
unsafe impl<T> Sync for SendWrapper<T> {}

struct WorkerPool {
    workers: Vec<WorkerHandle>,
}

/// Shared atomic counter for workers to signal readiness.
static WORKERS_READY: AtomicU32 = AtomicU32::new(0);

/// Per-worker task receivers, indexed by variant_id.
/// Set once during pool initialization, read by workers after init.
static TASK_RECEIVERS: OnceLock<Vec<flume::Receiver<BoxedTask>>> = OnceLock::new();

/// Entry point called by each Web Worker after WASM module is initialized
/// with shared memory. The worker runs a `spawn_local` event loop that
/// drains its task channel.
#[wasm_bindgen]
pub fn __zenoh_worker_entry(variant_id: u32) {
    let receivers = TASK_RECEIVERS.get().expect("Task receivers not initialized");
    let rx = receivers[variant_id as usize].clone();

    // Signal that this worker is ready
    WORKERS_READY.fetch_add(1, Ordering::Release);

    // Start a periodic nudge (setInterval) that re-polls pending spawn_local futures.
    // This is necessary because cross-worker wakes (e.g., flume tx.send() from
    // worker A calling a waker registered on worker B) use queueMicrotask which
    // is thread-local. The nudge ensures pending futures on this worker get
    // re-polled even when their waker was called from another worker.
    start_poll_nudge();

    // Run the task drain loop on this worker's JS event loop.
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            match rx.recv_async().await {
                Ok(task) => task(),
                Err(_) => break, // Channel closed — pool shutting down
            }
        }
    });
}

/// Global waker registry for cross-worker wake bridging.
/// Each worker registers wakers of pending futures here. The poll nudge
/// wakes ALL registered wakers, forcing the wasm_bindgen_futures executor
/// to re-poll them.
static WAKER_REGISTRY: Mutex<Vec<std::task::Waker>> = Mutex::new(Vec::new());

/// Register a waker in the global registry for cross-worker nudging.
pub fn register_cross_worker_waker(waker: &std::task::Waker) {
    if let Ok(mut registry) = WAKER_REGISTRY.lock() {
        // Replace existing waker for this thread or add new one.
        // We keep a bounded set to avoid unbounded growth.
        if registry.len() >= 64 {
            // Compact: remove stale entries by waking and re-adding
            let old = std::mem::take(&mut *registry);
            for w in old {
                w.wake();
            }
        }
        registry.push(waker.clone());
    }
}

/// Start a setInterval that periodically wakes ALL registered futures.
/// This bridges cross-worker async channels where the waker was called
/// from the wrong thread's microtask queue.
fn start_poll_nudge() {
    use wasm_bindgen::closure::Closure;

    let cb = Closure::wrap(Box::new(|| {
        // Wake all registered wakers — this forces the executor to re-poll
        // pending futures that may have been woken from other workers.
        if let Ok(mut registry) = WAKER_REGISTRY.lock() {
            for waker in registry.drain(..) {
                waker.wake();
            }
        }
    }) as Box<dyn FnMut()>);

    // 5ms interval — frequent enough for responsiveness, light enough for perf.
    set_interval(&cb, 5);
    cb.forget(); // Runs for the lifetime of the worker
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = "setInterval")]
    fn set_interval(closure: &Closure<dyn FnMut()>, millis: i32);
}

/// URL of the wasm-bindgen JS shim. Set during init, read by workers.
static SHIM_URL: OnceLock<String> = OnceLock::new();

impl WorkerPool {
    fn new(shim_url: &str) -> Self {
        // Create task channels for each worker
        let mut senders = Vec::with_capacity(NUM_WORKERS);
        let mut receivers = Vec::with_capacity(NUM_WORKERS);
        for _ in 0..NUM_WORKERS {
            let (tx, rx) = flume::unbounded();
            senders.push(tx);
            receivers.push(rx);
        }

        // Store receivers globally so workers can access them after WASM init
        TASK_RECEIVERS
            .set(receivers)
            .expect("Task receivers already initialized");

        // Spawn Web Workers
        let mut workers = Vec::with_capacity(NUM_WORKERS);
        for (variant_id, tx) in senders.into_iter().enumerate() {
            let worker = Self::spawn_worker(variant_id as u32, shim_url);
            workers.push(WorkerHandle {
                task_tx: tx,
                _worker: SendWrapper(worker),
            });
        }

        WorkerPool { workers }
    }

    /// Resolve a potentially relative URL to an absolute one using the page's location.
    fn resolve_url(url: &str) -> String {
        js_sys::eval(&format!(
            "new URL('{}', self.location.href).href",
            url.replace('\'', "\\'")
        ))
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| url.to_string())
    }

    /// Spawn a Web Worker that shares our WASM module and linear memory.
    ///
    /// The worker:
    /// 1. Loads the wasm-bindgen JS shim via importScripts
    /// 2. Receives [module, memory, variant_id] via onmessage
    /// 3. Calls wasm_bindgen(module, memory) to init with shared memory
    /// 4. Calls __zenoh_worker_entry(variant_id) to start its event loop
    fn spawn_worker(variant_id: u32, shim_url: &str) -> web_sys::Worker {
        // Resolve to absolute URL — relative URLs don't work in Blob URL workers.
        let abs_shim_url = Self::resolve_url(shim_url);

        // Worker JS: load the wasm-bindgen shim, then init with shared memory.
        // With +atomics, wasm_bindgen(module, memory) accepts two args —
        // the Module and the shared Memory (backed by SharedArrayBuffer).
        let js_code = format!(
            r#"importScripts('{}');
self.onmessage = async function(e) {{
    const [module, memory, variant_id] = e.data;
    try {{
        await wasm_bindgen(module, memory);
        wasm_bindgen.__zenoh_worker_entry(variant_id);
    }} catch(err) {{
        console.error('[zenoh-worker:' + variant_id + '] error:', err);
    }}
}};"#,
            abs_shim_url
        );

        // Create a Blob URL for the worker script
        let blob = web_sys::Blob::new_with_str_sequence_and_options(
            &js_sys::Array::of1(&JsValue::from_str(&js_code)),
            web_sys::BlobPropertyBag::new().type_("application/javascript"),
        )
        .expect("Failed to create worker blob");

        let url = web_sys::Url::create_object_url_with_blob(&blob)
            .expect("Failed to create worker blob URL");

        let worker = web_sys::Worker::new(&url).expect("Failed to create Web Worker");

        // Clean up the blob URL (worker already has the script)
        let _ = web_sys::Url::revoke_object_url(&url);

        // Post the WASM module + shared memory + variant_id to the worker.
        // wasm_bindgen::module() returns the WebAssembly.Module.
        // wasm_bindgen::memory() returns the WebAssembly.Memory (shared with SAB).
        let init_data = js_sys::Array::new();
        init_data.push(&wasm_bindgen::module());
        init_data.push(&wasm_bindgen::memory());
        init_data.push(&JsValue::from(variant_id));

        worker
            .post_message(&init_data)
            .expect("Failed to send init data to worker");

        worker
    }

    fn get(&self, rt: &ZRuntime) -> &WorkerHandle {
        &self.workers[rt.variant_id()]
    }
}

/// Global worker pool — lazily initialized on first use.
static WORKER_POOL: OnceLock<WorkerPool> = OnceLock::new();

/// Whether we're running in threaded mode (workers spawned) or fallback single-threaded.
static THREADED_MODE: AtomicBool = AtomicBool::new(false);

/// Initialize the threaded WASM runtime.
///
/// Creates Web Workers for each ZRuntime variant, sharing the WASM module and
/// linear memory via SharedArrayBuffer.
///
/// # Arguments
/// * `shim_url` — URL to the wasm-bindgen JS shim file (e.g., `"./pkg/my_crate.js"`).
///   Workers will load this via `importScripts()`.
///
/// # Returns
/// `true` if threaded mode was activated, `false` if falling back to single-threaded
/// (e.g., SharedArrayBuffer not available due to missing COOP/COEP headers).
#[wasm_bindgen]
pub fn __zenoh_init_threaded_runtime(shim_url: &str) -> bool {
    // Check if SharedArrayBuffer is available
    let sab_available = js_sys::eval("typeof SharedArrayBuffer !== 'undefined'")
        .map(|v| v.as_bool().unwrap_or(false))
        .unwrap_or(false);

    if !sab_available {
        tracing::warn!(
            "SharedArrayBuffer not available (missing COOP/COEP headers?). \
             Falling back to single-threaded WASM runtime."
        );
        return false;
    }

    let _ = SHIM_URL.set(shim_url.to_string());
    WORKER_POOL.get_or_init(|| WorkerPool::new(shim_url));
    THREADED_MODE.store(true, Ordering::Release);
    tracing::info!("Zenoh threaded WASM runtime initialized with {} workers", NUM_WORKERS);
    true
}

// ---------------------------------------------------------------------------
// ZRuntime methods
// ---------------------------------------------------------------------------

impl ZRuntime {
    /// Create an iterator over all runtime variants.
    pub fn iter() -> impl Iterator<Item = ZRuntime> {
        [
            ZRuntime::Application,
            ZRuntime::Acceptor,
            ZRuntime::TX,
            ZRuntime::RX,
            ZRuntime::Net,
        ]
        .into_iter()
    }

    /// Spawn a future on this runtime's worker.
    ///
    /// With `wasm-threads`, this dispatches the future to the dedicated Web Worker
    /// for this runtime variant. The future must be `Send` since it crosses a
    /// thread boundary.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (tx, rx) = flume::bounded(1);

        if THREADED_MODE.load(Ordering::Acquire) {
            // Dispatch to the worker for this runtime variant
            let pool = WORKER_POOL.get().expect("Worker pool not initialized");
            let worker = pool.get(self);
            let task: BoxedTask = Box::new(move || {
                wasm_bindgen_futures::spawn_local(async move {
                    let result = future.await;
                    let _ = tx.send(result);
                });
            });
            worker
                .task_tx
                .send(task)
                .expect("Worker task channel closed");
        } else {
            // Fallback: single-threaded mode (same as old wasm.rs)
            wasm_bindgen_futures::spawn_local(async move {
                let result = future.await;
                let _ = tx.send(result);
            });
        }

        JoinHandle { rx }
    }

    /// Block the current worker until the future completes.
    ///
    /// In threaded mode, uses `Condvar::wait` which compiles to `memory.atomic.wait32`
    /// — real OS-level blocking. The waker is called from whichever worker completes
    /// the async work.
    ///
    /// **Must not be called from the main browser thread** — `Atomics.wait()` is
    /// disallowed there. All zenoh usage should run from worker contexts.
    ///
    /// In single-threaded fallback mode, polls once and panics if Pending.
    #[track_caller]
    pub fn block_in_place<F, R>(&self, f: F) -> R
    where
        F: Future<Output = R>,
    {
        if !THREADED_MODE.load(Ordering::Acquire) {
            // Fallback: single-poll mode (same as old wasm.rs)
            let mut f = std::pin::pin!(f);
            let cv = Arc::new(CondvarWaker {
                woken: Mutex::new(false),
                cvar: Condvar::new(),
            });
            let waker: std::task::Waker = cv.into();
            let mut cx = Context::from_waker(&waker);
            match f.as_mut().poll(&mut cx) {
                Poll::Ready(result) => return result,
                Poll::Pending => {
                    let caller = std::panic::Location::caller();
                    panic!(
                        "block_in_place: future returned Pending on WASM (single-threaded fallback) \
                         — cannot block (called from {}:{}:{}). \
                         Enable wasm-threads with SharedArrayBuffer for real blocking.",
                        caller.file(),
                        caller.line(),
                        caller.column()
                    )
                }
            }
        }

        // Threaded mode: real blocking via Condvar with periodic timeout.
        // The timeout ensures we re-poll even if a waker notification was missed
        // (e.g., waker called between poll returning Pending and entering wait).
        let mut f = std::pin::pin!(f);
        let cv_waker = Arc::new(CondvarWaker {
            woken: Mutex::new(false),
            cvar: Condvar::new(),
        });
        let waker: std::task::Waker = cv_waker.clone().into();
        let mut cx = Context::from_waker(&waker);

        loop {
            match f.as_mut().poll(&mut cx) {
                Poll::Ready(val) => return val,
                Poll::Pending => {
                    // Wait for the waker with a timeout. The timeout (5ms) ensures
                    // we re-poll periodically, which handles cases where:
                    // - The waker was called before we entered wait
                    // - Cross-worker wake notifications were delayed
                    // - The poll nudge on other workers triggered state changes
                    let mut woken = cv_waker.woken.lock().unwrap();
                    if !*woken {
                        let (_guard, _timeout) = cv_waker
                            .cvar
                            .wait_timeout(woken, std::time::Duration::from_millis(5))
                            .unwrap();
                        woken = _guard;
                    }
                    *woken = false;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ZRuntimePoolGuard
// ---------------------------------------------------------------------------

pub struct ZRuntimePoolGuard;

impl Drop for ZRuntimePoolGuard {
    fn drop(&mut self) {
        // Drop the pool to close task channels, which causes worker loops to exit
        // Note: OnceLock doesn't support taking/dropping, so workers will run
        // until the page unloads. This matches browser lifecycle semantics.
    }
}
