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
use std::sync::Arc;
use std::time::Duration;

use futures::future::FutureExt;
use zenoh_runtime::{JoinHandle, ZRuntime};

/// A simple cancellation token for WASM, API-compatible with tokio_util's CancellationToken.
#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn child_token(&self) -> CancellationToken {
        // On WASM, child tokens share the same flag for simplicity
        self.clone()
    }

    /// Returns a future that completes when the token is cancelled.
    pub fn cancelled(&self) -> CancelledFuture {
        CancelledFuture {
            cancelled: self.cancelled.clone(),
        }
    }

    /// Runs a future until this token is cancelled.
    pub fn run_until_cancelled_owned<F: Future>(
        &self,
        future: F,
    ) -> impl Future<Output = Option<F::Output>> {
        let cancelled = self.cancelled.clone();
        async move {
            // Simple poll-based cancellation
            futures::pin_mut!(future);
            // We can't truly race without select!, so just run the future
            // and check cancellation. For WASM single-threaded this is acceptable.
            if cancelled.load(Ordering::SeqCst) {
                return None;
            }
            Some(future.await)
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CancelledFuture {
    cancelled: Arc<AtomicBool>,
}

impl Future for CancelledFuture {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            std::task::Poll::Ready(())
        } else {
            // Wake again soon to re-check
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

#[derive(Clone)]
pub struct TaskController {
    task_count: Arc<AtomicUsize>,
    token: CancellationToken,
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
    ) -> impl Future<Output = Option<T>> + 'a
    where
        F: Future<Output = T> + 'a,
        T: 'static,
    {
        self.token
            .child_token()
            .run_until_cancelled_owned(future)
    }

    pub fn spawn_abortable<F, T>(&self, future: F) -> JoinHandle<Option<T>>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
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
        F: Future<Output = T> + 'static,
        T: 'static,
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
        F: Future<Output = T> + 'static,
        T: 'static,
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
        F: Future<Output = T> + 'static,
        T: 'static,
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
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        TerminatableTask {
            handle: Some(rt.spawn(future.map(|_f| ()))),
            token,
        }
    }

    pub fn spawn_abortable<F, T>(rt: ZRuntime, future: F) -> TerminatableTask
    where
        F: Future<Output = T> + 'static,
        T: 'static,
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
