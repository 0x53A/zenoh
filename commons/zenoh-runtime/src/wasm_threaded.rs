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
use std::sync::OnceLock;
use std::task::{Context, Poll};

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
    abort: futures::future::AbortHandle,
    /// Persistent recv future so the flume waker registration survives
    /// across polls (recreating it each poll would deregister the waker on drop).
    fut: Option<Pin<Box<dyn Future<Output = Result<T, flume::RecvError>> + Send + Sync>>>,
    repoll: Option<RepollTimer>,
}

impl<T> std::fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinHandle").finish()
    }
}

impl<T: Send + 'static> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.rx.try_recv() {
            Ok(val) => { this.repoll = None; return Poll::Ready(Ok(val)); },
            Err(flume::TryRecvError::Disconnected) => { this.repoll = None; return Poll::Ready(Err(JoinError)); },
            Err(flume::TryRecvError::Empty) => {}
        }
        if THREADED_MODE.load(Ordering::Acquire) && !has_local_executor() {
            // JS thread (main or Acceptor) in threaded mode: the result arrives
            // from another worker, which may depend on browser cross-worker wake support.
            // Keep the existing timer-based compatibility path bounded and owned.
            let timer = this.repoll.get_or_insert_with(RepollTimer::new);
            if Pin::new(timer).poll(cx).is_ready() {
                this.repoll = None;
                cx.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
        // Compute worker (Condvar-backed waker, cross-thread safe) or
        // single-threaded fallback (same-thread wakes work).
        let rx = this.rx.clone();
        let fut = this
            .fut
            .get_or_insert_with(|| Box::pin(rx.into_recv_async()));
        match fut.as_mut().poll(cx) {
            Poll::Ready(Ok(val)) => Poll::Ready(Ok(val)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(JoinError)),
            Poll::Pending => Poll::Pending,
        }
    }
}

// Repoll timers belong to the waiting future. Shared-select wakers may poll
// several branches repeatedly before a timer fires; rescheduling on every poll
// creates an unbounded backlog instead of a wakeup bridge.
static REPOLL_PENDING: AtomicU32 = AtomicU32::new(0);
static REPOLL_PEAK: AtomicU32 = AtomicU32::new(0);
#[wasm_bindgen]
#[doc(hidden)]
pub fn __zenoh_repoll_counts() -> js_sys::Array {
    js_sys::Array::of2(&JsValue::from(REPOLL_PENDING.load(Ordering::Relaxed)), &JsValue::from(REPOLL_PEAK.load(Ordering::Relaxed)))
}
struct RepollTimer(crate::wasm_yield::Sleep);
impl RepollTimer {
    fn new() -> Self {
        let pending=REPOLL_PENDING.fetch_add(1,Ordering::Relaxed)+1;
        REPOLL_PEAK.fetch_max(pending,Ordering::Relaxed);
        Self(crate::wasm_yield::sleep_ms(1))
    }
}
impl Future for RepollTimer {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.0).poll(cx)
    }
}
impl Drop for RepollTimer {
    fn drop(&mut self) { REPOLL_PENDING.fetch_sub(1,Ordering::Relaxed); }
}

impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.abort.abort();
    }

    pub fn is_finished(&self) -> bool {
        !self.rx.is_empty() || self.rx.is_disconnected()
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
// Worker pool
// ---------------------------------------------------------------------------

/// A task that can be sent to a worker for execution.
/// The closure captures the future and a result sender, then calls `spawn_local`.
type BoxedTask = Box<dyn FnOnce() + Send>;

struct WorkerHandle {
    task_tx: flume::Sender<BoxedTask>,
}

// JS handles and callbacks stay on the thread that created the workers.
struct WorkerOwner {
    _worker: web_sys::Worker,
    _error: Closure<dyn FnMut(JsValue)>,
    _message: Closure<dyn FnMut(JsValue)>,
}
thread_local! {
    static WORKER_OWNERS: std::cell::RefCell<Vec<WorkerOwner>> = const { std::cell::RefCell::new(Vec::new()) };
}

// Startup errors are terminal: shared-memory workers cannot safely be killed
// while holding a Rust lock. Reload the page to get a fresh WASM instance.
static STARTUP_STARTED: AtomicBool = AtomicBool::new(false);
static WORKER_FAILURE: OnceLock<String> = OnceLock::new();
fn worker_failed(message: String) {
    let _ = WORKER_FAILURE.set(message);
}

struct WorkerPool {
    workers: Vec<WorkerHandle>,
}

/// Shared atomic counter for workers to signal readiness.
static WORKERS_READY: AtomicU32 = AtomicU32::new(0);

/// Per-worker task receivers, indexed by variant_id.
/// Set once during pool initialization, read by workers after init.
static TASK_RECEIVERS: OnceLock<Vec<flume::Receiver<BoxedTask>>> = OnceLock::new();

/// Entry point called by each Web Worker after WASM module is initialized
/// with shared memory.
///
/// Compute workers (Application, TX, RX, Net) run a pure-Rust [`LocalExecutor`]
/// that blocks the worker thread forever. Their futures are woken via
/// `Condvar::notify` (`memory.atomic.notify`), which works from any thread.
/// No JS is used on these workers after entry — `setTimeout`/`spawn_local`
/// would never fire since the event loop is permanently blocked.
///
/// The Acceptor (I/O worker) keeps its JS event loop alive for WebSocket
/// callbacks. Its task drain keeps the timer-based cross-worker compatibility path.
/// Current atomic wasm-bindgen wakers also support cross-thread notifications.
#[wasm_bindgen]
pub fn __zenoh_worker_entry(variant_id: u32) {
    WORKER_RUNTIME.with(|current| current.set(Some(ZRuntime::from_id(variant_id))));
    let receivers = TASK_RECEIVERS
        .get()
        .expect("Task receivers not initialized");
    let rx = receivers[variant_id as usize].clone();

    if variant_id == ZRuntime::Acceptor.variant_id() as u32 {
        // I/O worker: JS event loop stays alive for WebSocket callbacks.
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                match recv_async_anywhere(&rx).await {
                    Ok(task) => task(),
                    Err(_) => break, // Channel closed — pool shutting down
                }
            }
        });
        WORKERS_READY.fetch_add(1, Ordering::Release);
    } else {
        // Compute worker: pure-Rust executor owns this thread from here on.
        let executor = std::rc::Rc::new(crate::executor::LocalExecutor::new());
        executor.install();
        executor.spawn(async move {
            loop {
                // flume's waker is our executor's Condvar-backed waker here,
                // so sends from any thread wake this loop.
                match rx.recv_async().await {
                    Ok(task) => task(),
                    Err(_) => break,
                }
            }
        });
        WORKERS_READY.fetch_add(1, Ordering::Release);
        executor.run();
    }
}

thread_local! {
    static WORKER_RUNTIME: std::cell::Cell<Option<ZRuntime>> = const { std::cell::Cell::new(None) };
}

/// Dedicated worker executing this code; None on the browser main thread.
pub fn current_runtime() -> Option<ZRuntime> {
    WORKER_RUNTIME.with(|current| current.get())
}

pub use crate::executor::has_local_executor;

/// Spawn a `!Send` future on the current thread: onto the thread's
/// [`LocalExecutor`] on compute workers, or the JS microtask queue on the
/// main thread / Acceptor.
pub fn spawn_on_current<F: Future<Output = ()> + 'static>(f: F) {
    if let Some(exec) = crate::executor::try_current_executor() {
        exec.spawn(f);
    } else {
        wasm_bindgen_futures::spawn_local(f);
    }
}

/// Receive from a flume channel, working correctly regardless of which
/// thread runs the future and which thread the sender fires from.
///
/// - Compute workers: plain `recv_async` — the executor's wakers are
///   Condvar-backed and cross-thread safe.
/// - Main thread / Acceptor in threaded mode: retain a bounded `setTimeout(1)`
///   compatibility bridge, owned by the waiting future.
/// - Threaded mode inactive (single-threaded fallback): plain `recv_async` —
///   everything is on one thread.
pub async fn recv_async_anywhere<T: Send + 'static>(
    rx: &flume::Receiver<T>,
) -> Result<T, flume::RecvError> {
    let needs_repoll = THREADED_MODE.load(Ordering::Acquire) && !has_local_executor();
    if !needs_repoll {
        return rx.recv_async().await;
    }
    loop {
        match rx.try_recv() {
            Ok(value) => return Ok(value),
            Err(flume::TryRecvError::Disconnected) => return Err(flume::RecvError::Disconnected),
            Err(flume::TryRecvError::Empty) => RepollTimer::new().await,
        }
    }
}

impl WorkerPool {
    fn new(shim_url: &str) -> Result<Self, JsValue> {
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
            let owner = Self::spawn_worker(variant_id as u32, shim_url)?;
            WORKER_OWNERS.with(|owners| owners.borrow_mut().push(owner));
            workers.push(WorkerHandle { task_tx: tx });
        }

        Ok(WorkerPool { workers })
    }

    /// Resolve a potentially relative URL to an absolute one using the page's location.
    fn resolve_url(url: &str) -> Result<String, JsValue> {
        let location = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("location"))?;
        let base = js_sys::Reflect::get(&location, &JsValue::from_str("href"))?
            .as_string()
            .ok_or_else(|| JsValue::from_str("location.href is unavailable"))?;
        Ok(web_sys::Url::new_with_base(url, &base)?.href())
    }

    /// Spawn a Web Worker that shares our WASM module and linear memory.
    ///
    /// The worker:
    /// 1. Loads the wasm-bindgen JS shim via importScripts
    /// 2. Receives [module, memory, variant_id] via onmessage
    /// 3. Calls wasm_bindgen(module, memory) to init with shared memory
    /// 4. Calls __zenoh_worker_entry(variant_id) to start its event loop
    fn spawn_worker(variant_id: u32, shim_url: &str) -> Result<WorkerOwner, JsValue> {
        // Resolve to absolute URL — relative URLs don't work in Blob URL workers.
        let abs_shim_url = Self::resolve_url(shim_url)?;

        // Worker JS: load the wasm-bindgen shim, then init with shared memory.
        // With +atomics, wasm_bindgen(module, memory) accepts two args —
        // the Module and the shared Memory (backed by SharedArrayBuffer).
        let shim_literal = js_sys::JSON::stringify(&JsValue::from_str(&abs_shim_url))
            .expect("a URL string can be serialized");
        let js_code = format!(
            r#"self.onmessage = async function(e) {{
    const [module, memory, variant_id] = e.data;
    try {{
        importScripts({});
        await wasm_bindgen(module, memory);
        wasm_bindgen.__zenoh_worker_entry(variant_id);
    }} catch(err) {{
        self.postMessage({{ zenohWorkerError: String(err) }});
    }}
}};"#,
            shim_literal
                .as_string()
                .expect("JSON.stringify returns a string")
        );

        // Create a Blob URL for the worker script
        let blob = web_sys::Blob::new_with_str_sequence_and_options(
            &js_sys::Array::of1(&JsValue::from_str(&js_code)),
            web_sys::BlobPropertyBag::new().type_("application/javascript"),
        )?;

        let url = web_sys::Url::create_object_url_with_blob(&blob)?;
        let result = web_sys::Worker::new(&url);

        // Clean up the blob URL (worker already has the script)
        let _ = web_sys::Url::revoke_object_url(&url);
        let worker = result?;
        let error = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            let detail = js_sys::Reflect::get(&event, &JsValue::from_str("message"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_else(|| "worker script failed".into());
            worker_failed(format!("worker {variant_id}: {detail}"));
        });
        let message = Closure::<dyn FnMut(JsValue)>::new(move |event: JsValue| {
            if let Ok(data) = js_sys::Reflect::get(&event, &JsValue::from_str("data")) {
                if let Some(detail) =
                    js_sys::Reflect::get(&data, &JsValue::from_str("zenohWorkerError"))
                        .ok()
                        .and_then(|v| v.as_string())
                {
                    worker_failed(format!("worker {variant_id}: {detail}"));
                }
            }
        });
        worker.set_onerror(Some(error.as_ref().unchecked_ref()));
        worker.set_onmessage(Some(message.as_ref().unchecked_ref()));

        // Post the WASM module + shared memory + variant_id to the worker.
        // wasm_bindgen::module() returns the WebAssembly.Module.
        // wasm_bindgen::memory() returns the WebAssembly.Memory (shared with SAB).
        let init_data = js_sys::Array::new();
        init_data.push(&wasm_bindgen::module());
        init_data.push(&wasm_bindgen::memory());
        init_data.push(&JsValue::from(variant_id));

        if let Err(error_value) = worker.post_message(&init_data) {
            worker.set_onerror(None);
            worker.set_onmessage(None);
            worker.terminate(); // No init message was sent: no shared Rust code runs here.
            return Err(error_value);
        }
        Ok(WorkerOwner {
            _worker: worker,
            _error: error,
            _message: message,
        })
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
/// This legacy API reports dispatch only, not readiness. Prefer
/// [`__zenoh_init_threaded_runtime_async`] and await it before spawning tasks.
/// `true` if workers were dispatched, `false` if startup failed or falling back to single-threaded
/// (e.g., SharedArrayBuffer not available due to missing COOP/COEP headers).
#[wasm_bindgen]
pub fn __zenoh_init_threaded_runtime(shim_url: &str) -> bool {
    // Check if SharedArrayBuffer is available
    let sab_available =
        js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("SharedArrayBuffer"))
            .map(|v| v.is_function())
            .unwrap_or(false);

    if !sab_available {
        tracing::warn!(
            "SharedArrayBuffer not available (missing COOP/COEP headers?). \
             Falling back to single-threaded WASM runtime."
        );
        return false;
    }

    if WORKER_FAILURE.get().is_some() {
        return false;
    }
    if STARTUP_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        match WorkerPool::new(shim_url) {
            Ok(pool) => {
                let _ = WORKER_POOL.set(pool);
                THREADED_MODE.store(true, Ordering::Release);
            }
            Err(error) => {
                worker_failed(format!("worker initialization failed: {error:?}"));
                return false;
            }
        }
    }
    true
}

/// Wait until all five workers have installed their task executors.
/// Returns false only when SharedArrayBuffer is unavailable (single-threaded
/// fallback). Script load, initialization and timeout failures reject with an
/// explanatory error. Failure is terminal; reload the page before retrying.
/// The timeout starts when this future is polled and includes worker dispatch.
#[wasm_bindgen]
pub async fn __zenoh_init_threaded_runtime_async(
    shim_url: &str,
    timeout_ms: u32,
) -> Result<bool, JsValue> {
    let started = crate::wasm_yield::Instant::now();
    let dispatched = __zenoh_init_threaded_runtime(shim_url);
    loop {
        if let Some(error) = WORKER_FAILURE.get() {
            return Err(JsValue::from_str(&format!(
                "{error}; reload the page to retry"
            )));
        }
        if !dispatched {
            return Ok(false);
        }
        if WORKERS_READY.load(Ordering::Acquire) == NUM_WORKERS as u32 {
            return Ok(true);
        }
        if started.elapsed() >= std::time::Duration::from_millis(timeout_ms.into()) {
            worker_failed(format!(
                "worker startup timed out after {timeout_ms} ms ({}/{NUM_WORKERS} ready)",
                WORKERS_READY.load(Ordering::Acquire)
            ));
            continue;
        }
        crate::wasm_yield::sleep_ms(1).await;
    }
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
        let (abort, registration) = futures::future::AbortHandle::new_pair();
        let future = futures::future::Abortable::new(future, registration);

        if WORKER_FAILURE.get().is_some() {
            drop(future);
            drop(tx);
        } else if THREADED_MODE.load(Ordering::Acquire) {
            // Dispatch to the worker for this runtime variant. The closure runs
            // ON the target worker: compute workers spawn onto their
            // LocalExecutor, the Acceptor onto its JS microtask queue.
            let pool = WORKER_POOL.get().expect("Worker pool not initialized");
            let worker = pool.get(self);
            let task: BoxedTask = Box::new(move || {
                spawn_on_current(async move {
                    if let Ok(result) = future.await {
                        let _ = tx.send(result);
                    }
                });
            });
            worker
                .task_tx
                .send(task)
                .expect("Worker task channel closed");
        } else {
            // Fallback: single-threaded mode (same as old wasm.rs)
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(result) = future.await {
                    let _ = tx.send(result);
                }
            });
        }

        JoinHandle {
            rx,
            abort,
            fut: None,
            repoll: None,
        }
    }

    /// Drive a future synchronously on a compute worker, pumping its executor.
    ///
    /// On the main thread or the Acceptor worker, only immediately ready futures
    /// are supported: waiting would prevent JS events from making progress.
    #[track_caller]
    pub fn block_in_place<F, R>(&self, f: F) -> R
    where
        F: Future<Output = R>,
    {
        if let Some(exec) = crate::executor::try_current_executor() {
            return exec.block_on(f);
        }

        let mut f = std::pin::pin!(f);
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!(
                "block_in_place: cannot wait on a WASM JS event-loop thread; \
                 await the future or run it on a compute worker"
            ),
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
