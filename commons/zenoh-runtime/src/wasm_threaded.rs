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
                let rx = self.rx.clone();
                let mut fut = Box::pin(async move { rx.recv_async().await });
                match fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(val)) => Poll::Ready(Ok(val)),
                    Poll::Ready(Err(_)) => Poll::Ready(Err(JoinError)),
                    Poll::Pending => Poll::Pending,
                }
            }
            Err(flume::TryRecvError::Disconnected) => Poll::Ready(Err(JoinError)),
        }
    }
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

    // Run the task drain loop on this worker's JS event loop.
    // Each received task is a closure that calls spawn_local internally,
    // so it integrates with this worker's microtask queue.
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            match rx.recv_async().await {
                Ok(task) => task(),
                Err(_) => break, // Channel closed — pool shutting down
            }
        }
    });
}

impl WorkerPool {
    fn new() -> Self {
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
            let worker = Self::spawn_worker(variant_id as u32);
            workers.push(WorkerHandle {
                task_tx: tx,
                _worker: SendWrapper(worker),
            });
        }

        WorkerPool { workers }
    }

    /// Spawn a Web Worker that shares our WASM module and linear memory.
    ///
    /// The worker receives the WebAssembly.Module and Memory via postMessage,
    /// re-instantiates the module with the shared memory, then calls
    /// `__zenoh_worker_entry(variant_id)` to start its event loop.
    fn spawn_worker(variant_id: u32) -> web_sys::Worker {
        // The JS code for the worker:
        // - Receives [module, memory, variant_id] via onmessage
        // - Calls wasm_bindgen(module, memory) to init with shared memory
        // - Calls the exported __zenoh_worker_entry(variant_id)
        //
        // For --target no-modules: uses importScripts + wasm_bindgen global
        // For --target web: uses import() + init() pattern
        //
        // We use the no-modules approach since the existing wasm-client example
        // uses importScripts (classic workers for browser compat).
        let js_code = r#"
            self.onmessage = async function(e) {
                const [module, memory, variant_id] = e.data;
                // wasm_bindgen is available as a global after the WASM module
                // is instantiated. We re-init with the shared memory.
                const instance = await WebAssembly.instantiate(module, {
                    './zenoh_wasm_bg.js': self.__wbg_star0,
                    env: { memory },
                    __wbindgen_thread_xform__: { __wbindgen_thread_id: () => variant_id },
                });
                // Initialize wasm-bindgen glue with the shared instance
                // The generated __wbg_init function or wasm_bindgen() handles this.
                if (typeof wasm_bindgen !== 'undefined') {
                    await wasm_bindgen(module, memory);
                    wasm_bindgen.__zenoh_worker_entry(variant_id);
                } else {
                    // Fallback: direct call via instance exports
                    instance.exports.__zenoh_worker_entry(variant_id);
                }
            };
        "#;

        // Create a Blob URL for the worker script
        let blob = web_sys::Blob::new_with_str_sequence_and_options(
            &js_sys::Array::of1(&JsValue::from_str(js_code)),
            web_sys::BlobPropertyBag::new().type_("application/javascript"),
        )
        .expect("Failed to create worker blob");

        let url = web_sys::Url::create_object_url_with_blob(&blob)
            .expect("Failed to create worker blob URL");

        let worker = web_sys::Worker::new(&url).expect("Failed to create Web Worker");

        // Clean up the blob URL
        let _ = web_sys::Url::revoke_object_url(&url);

        // Send the WASM module, shared memory, and variant_id to the worker
        let module = wasm_bindgen::module();
        let memory = wasm_bindgen::memory();
        let init_data = js_sys::Array::new();
        init_data.push(&module);
        init_data.push(&memory);
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
/// Creates Web Workers for each ZRuntime variant. Must be called from a context
/// with access to the JS global scope (main thread or an existing worker).
///
/// If SharedArrayBuffer is not available (missing COOP/COEP headers), this
/// will fail and the runtime falls back to single-threaded mode.
#[wasm_bindgen]
pub fn __zenoh_init_threaded_runtime() -> bool {
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

    WORKER_POOL.get_or_init(WorkerPool::new);
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

        // Threaded mode: real blocking via Condvar
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
                    // Block until the waker is called (from another worker).
                    // Condvar::wait compiles to memory.atomic.wait32 — real blocking.
                    let mut woken = cv_waker.woken.lock().unwrap();
                    while !*woken {
                        woken = cv_waker.cvar.wait(woken).unwrap();
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
