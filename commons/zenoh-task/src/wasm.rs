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

//! WASM implementation of task management.
//!
//! On WASM there are no threads and no tokio runtime.
//! We provide API-compatible types that use spawn_local and simple cancellation.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::FutureExt;
use zenoh_runtime::{JoinHandle, ZRuntime};

/// A simple cancellation token for WASM, API-compatible with tokio_util's CancellationToken.
#[derive(Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationTokenInner>,
}

struct CancellationTokenInner {
    cancelled: AtomicBool,
    /// Stores one waker per CancelledFuture instance (keyed by index).
    /// Replaces wakers on re-poll instead of accumulating duplicates.
    wakers: Mutex<Vec<Option<std::task::Waker>>>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancellationTokenInner {
                cancelled: AtomicBool::new(false),
                wakers: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        // Wake all waiting futures
        if let Ok(mut wakers) = self.inner.wakers.lock() {
            for waker in wakers.iter_mut().filter_map(|w| w.take()) {
                waker.wake();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub fn child_token(&self) -> CancellationToken {
        self.clone()
    }

    /// Returns a future that completes when the token is cancelled.
    pub fn cancelled(&self) -> CancelledFuture {
        // Allocate a slot in the waker vec for this future
        let slot = if let Ok(mut wakers) = self.inner.wakers.lock() {
            let idx = wakers.len();
            wakers.push(None);
            idx
        } else {
            0
        };
        CancelledFuture {
            inner: self.inner.clone(),
            slot,
        }
    }

    /// Runs a future until this token is cancelled (owned version).
    pub fn run_until_cancelled_owned<F: Future + Send>(
        &self,
        future: F,
    ) -> impl Future<Output = Option<F::Output>> + Send {
        let inner = self.inner.clone();
        async move {
            if inner.cancelled.load(Ordering::SeqCst) {
                return None;
            }
            Some(future.await)
        }
    }

    /// Runs a future until this token is cancelled (reference version).
    pub async fn run_until_cancelled<F: Future>(&self, future: F) -> Option<F::Output> {
        if self.inner.cancelled.load(Ordering::SeqCst) {
            return None;
        }
        Some(future.await)
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CancelledFuture {
    inner: Arc<CancellationTokenInner>,
    slot: usize,
}

impl Future for CancelledFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.inner.cancelled.load(Ordering::SeqCst) {
            std::task::Poll::Ready(())
        } else {
            // Replace the waker in our dedicated slot (no accumulation)
            if let Ok(mut wakers) = self.inner.wakers.lock() {
                if let Some(entry) = wakers.get_mut(self.slot) {
                    *entry = Some(cx.waker().clone());
                }
            }
            std::task::Poll::Pending
        }
    }
}

#[derive(Clone)]
pub struct TaskController {
    task_count: Arc<AtomicUsize>,
    token: CancellationToken,
}

impl std::fmt::Debug for TaskController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskController")
            .field("is_cancelled", &self.token.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Default for TaskController {
    fn default() -> Self {
        TaskController {
            task_count: Arc::new(AtomicUsize::new(0)),
            token: CancellationToken::new(),
        }
    }
}

impl TaskController {
    pub fn into_abortable<'a, F, T>(
        &self,
        future: F,
    ) -> impl Future<Output = Option<T>> + Send + 'a
    where
        F: Future<Output = T> + Send + 'a,
        T: Send + 'static,
    {
        self.token
            .child_token()
            .run_until_cancelled_owned(future)
    }

    pub fn spawn_abortable<F, T>(&self, future: F) -> JoinHandle<Option<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let count = self.task_count.clone();
        count.fetch_add(1, Ordering::SeqCst);
        let abortable = self.into_abortable(future);
        let count2 = count.clone();
        ZRuntime::Application.spawn(async move {
            let result = abortable.await;
            count2.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }

    pub fn spawn_abortable_with_rt<F, T>(
        &self,
        rt: ZRuntime,
        future: F,
    ) -> JoinHandle<Option<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let count = self.task_count.clone();
        count.fetch_add(1, Ordering::SeqCst);
        let abortable = self.into_abortable(future);
        let count2 = count.clone();
        rt.spawn(async move {
            let result = abortable.await;
            count2.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }

    pub fn get_cancellation_token(&self) -> CancellationToken {
        self.token.child_token()
    }

    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let count = self.task_count.clone();
        count.fetch_add(1, Ordering::SeqCst);
        let count2 = count.clone();
        ZRuntime::Application.spawn(async move {
            let result = future.await;
            count2.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }

    pub fn spawn_with_rt<F, T>(&self, rt: ZRuntime, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let count = self.task_count.clone();
        count.fetch_add(1, Ordering::SeqCst);
        let count2 = count.clone();
        rt.spawn(async move {
            let result = future.await;
            count2.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }

    pub fn terminate_all(&self, _timeout: Duration) -> usize {
        self.token.cancel();
        self.task_count.load(Ordering::SeqCst)
    }

    pub async fn terminate_all_async(&self) {
        self.token.cancel();
        // On WASM we can't truly wait for tasks to complete
        // since there's no join mechanism for spawn_local tasks
    }
}

pub struct TerminatableTask {
    handle: Option<JoinHandle<()>>,
    token: CancellationToken,
}

impl std::fmt::Debug for TerminatableTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminatableTask")
            .field("is_cancelled", &self.token.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl Drop for TerminatableTask {
    fn drop(&mut self) {
        // On WASM, we just cancel — can't block waiting
        self.token.cancel();
    }
}

impl TerminatableTask {
    pub fn create_cancellation_token() -> CancellationToken {
        CancellationToken::new()
    }

    pub fn spawn<F, T>(rt: ZRuntime, future: F, token: CancellationToken) -> TerminatableTask
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        TerminatableTask {
            handle: Some(rt.spawn(future.map(|_f| ()))),
            token,
        }
    }

    pub fn spawn_abortable<F, T>(rt: ZRuntime, future: F) -> TerminatableTask
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let token = CancellationToken::new();
        let token2 = token.clone();
        let task = async move {
            futures::pin_mut!(future);
            futures::select! {
                _ = token2.cancelled().fuse() => {},
                _ = future.fuse() => {}
            }
        };

        TerminatableTask {
            handle: Some(rt.spawn(task)),
            token,
        }
    }

    pub fn terminate(&mut self, _timeout: Duration) -> bool {
        self.token.cancel();
        true
    }

    pub async fn terminate_async(&mut self) {
        self.token.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}
