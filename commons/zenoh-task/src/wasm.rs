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
//! Uses the browser/worker runtime and runtime-independent cancellation tokens.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::FutureExt;
use zenoh_runtime::{JoinHandle, ZRuntime};

// CancellationToken uses synchronization primitives only; it needs no Tokio
// runtime. Keep native cancellation, child-token and waiter-drop semantics.
pub use tokio_util::sync::CancellationToken;

struct TaskCountGuard(Arc<AtomicUsize>);
impl Drop for TaskCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
    pub fn into_abortable<'a, F, T>(&self, future: F) -> impl Future<Output = Option<T>> + Send + 'a
    where
        F: Future<Output = T> + Send + 'a,
        T: Send + 'static,
    {
        self.token.child_token().run_until_cancelled_owned(future)
    }

    pub fn spawn_abortable<F, T>(&self, future: F) -> JoinHandle<Option<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let count = self.task_count.clone();
        count.fetch_add(1, Ordering::SeqCst);
        let abortable = self.into_abortable(future);
        let completed = TaskCountGuard(count);
        ZRuntime::Application.spawn(async move {
            let _completed = completed;
            abortable.await
        })
    }

    pub fn spawn_abortable_with_rt<F, T>(&self, rt: ZRuntime, future: F) -> JoinHandle<Option<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let count = self.task_count.clone();
        count.fetch_add(1, Ordering::SeqCst);
        let abortable = self.into_abortable(future);
        let completed = TaskCountGuard(count);
        rt.spawn(async move {
            let _completed = completed;
            abortable.await
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
        let completed = TaskCountGuard(count);
        ZRuntime::Application.spawn(async move {
            let _completed = completed;
            future.await
        })
    }

    pub fn spawn_with_rt<F, T>(&self, rt: ZRuntime, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let count = self.task_count.clone();
        count.fetch_add(1, Ordering::SeqCst);
        let completed = TaskCountGuard(count);
        rt.spawn(async move {
            let _completed = completed;
            future.await
        })
    }

    /// Cancel all tasks and return the number still running.
    /// Compute workers wait up to `timeout`; JS event-loop threads cannot block
    /// and return immediately. Use async termination to wait on those threads.
    pub fn terminate_all(&self, timeout: Duration) -> usize {
        self.token.cancel();
        if zenoh_runtime::has_local_executor() {
            ZRuntime::Application.block_in_place(async {
                wait_with_timeout(self.terminate_all_async(), timeout).await;
            });
        }
        self.task_count.load(Ordering::SeqCst)
    }

    pub async fn terminate_all_async(&self) {
        self.token.cancel();
        while self.task_count.load(Ordering::SeqCst) != 0 {
            zenoh_runtime::wasm_yield::sleep_ms(1).await;
        }
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

    /// Cancel and report whether the task has actually finished.
    /// Compute workers wait up to `timeout`; JS event-loop threads return the
    /// current status immediately. A false result keeps the handle for retry.
    pub fn terminate(&mut self, timeout: Duration) -> bool {
        self.token.cancel();
        if zenoh_runtime::has_local_executor() {
            ZRuntime::Application.block_in_place(self.terminate_async_timeout(timeout))
        } else {
            self.handle.as_ref().is_none_or(JoinHandle::is_finished)
        }
    }

    /// Cancel and wait for completion, retaining the handle if the wait times out.
    pub async fn terminate_async_timeout(&mut self, timeout: Duration) -> bool {
        wait_with_timeout(self.terminate_async(), timeout).await
    }

    pub async fn terminate_async(&mut self) {
        self.token.cancel();
        if let Some(handle) = self.handle.as_mut() {
            let _ = handle.await;
        }
        // Do not take the handle until the join completes: dropping this wait
        // (including a timeout) must leave a subsequent join possible.
        self.handle = None;
    }
}

async fn wait_with_timeout(future: impl Future<Output = ()>, timeout: Duration) -> bool {
    let completion = future.fuse();
    let deadline = zenoh_runtime::wasm_yield::sleep(timeout).fuse();
    futures::pin_mut!(completion, deadline);
    futures::select_biased! {
        _ = completion => true,
        _ = deadline => false,
    }
}
