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

use std::{future::Future, time::Duration};

use futures::future::FutureExt;
use tokio::task::JoinHandle;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use zenoh_core::{ResolveFuture, Wait};
use zenoh_runtime::ZRuntime;

#[derive(Clone)]
pub struct TaskController {
    tracker: TaskTracker,
    token: CancellationToken,
}

impl Default for TaskController {
    fn default() -> Self {
        TaskController {
            tracker: TaskTracker::new(),
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
        #[cfg(feature = "tracing-instrument")]
        let future = tracing::Instrument::instrument(future, tracing::Span::current());

        self.tracker.spawn(self.into_abortable(future))
    }

    pub fn spawn_abortable_with_rt<F, T>(&self, rt: ZRuntime, future: F) -> JoinHandle<Option<T>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        #[cfg(feature = "tracing-instrument")]
        let future = tracing::Instrument::instrument(future, tracing::Span::current());

        self.tracker.spawn_on(self.into_abortable(future), &rt)
    }

    pub fn get_cancellation_token(&self) -> CancellationToken {
        self.token.child_token()
    }

    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        #[cfg(feature = "tracing-instrument")]
        let future = tracing::Instrument::instrument(future, tracing::Span::current());

        self.tracker.spawn(future)
    }

    pub fn spawn_with_rt<F, T>(&self, rt: ZRuntime, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        #[cfg(feature = "tracing-instrument")]
        let future = tracing::Instrument::instrument(future, tracing::Span::current());

        self.tracker.spawn_on(future, &rt)
    }

    pub fn terminate_all(&self, timeout: Duration) -> usize {
        ResolveFuture::new(async move {
            if tokio::time::timeout(timeout, self.terminate_all_async())
                .await
                .is_err()
            {
                tracing::error!("Failed to terminate {} tasks", self.tracker.len());
            }
            self.tracker.len()
        })
        .wait()
    }

    pub async fn terminate_all_async(&self) {
        self.tracker.close();
        self.token.cancel();
        self.tracker.wait().await
    }
}

pub struct TerminatableTask {
    handle: Option<JoinHandle<()>>,
    token: CancellationToken,
}

impl Drop for TerminatableTask {
    fn drop(&mut self) {
        self.terminate(std::time::Duration::from_secs(10));
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
            tokio::select! {
                _ = token2.cancelled() => {},
                _ = future => {}
            }
        };

        TerminatableTask {
            handle: Some(rt.spawn(task)),
            token,
        }
    }

    pub fn terminate(&mut self, timeout: Duration) -> bool {
        ResolveFuture::new(async move {
            if tokio::time::timeout(timeout, self.terminate_async())
                .await
                .is_err()
            {
                tracing::error!("Failed to terminate the task");
                return false;
            };
            true
        })
        .wait()
    }

    pub async fn terminate_async(&mut self) {
        self.token.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}
