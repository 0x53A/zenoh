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
pub use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zenoh_core::{ResolveFuture, Wait};
use zenoh_runtime::ZRuntime;

#[derive(Clone)]
pub struct TaskController {
    tracker: TaskTracker,
    token: CancellationToken,
}

impl std::fmt::Debug for TaskController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskController")
            .field("is_closed", &self.tracker.is_closed())
            .field("is_cancelled", &self.token.is_cancelled())
            .finish_non_exhaustive()
    }
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

impl std::fmt::Debug for TerminatableTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminatableTask")
            .field(
                "is_finished",
                &self.handle.as_ref().map_or(true, |h| h.is_finished()),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for TerminatableTask {
    fn drop(&mut self) {
        // The final owner can be released by the task itself (for example a
        // routing worker dropping its last tables reference). It cannot join
        // itself; let the current poll unwind normally after cancellation.
        if self.handle.as_ref().is_some_and(|handle| Some(handle.id()) == tokio::task::try_id()) {
            self.token.cancel();
            return;
        }
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
        if let Some(handle) = self.handle.as_mut() {
            let _ = handle.await;
        }
        // Preserve ownership if this wait is canceled or times out.
        self.handle = None;
    }
}

#[cfg(test)]
mod termination_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_termination_keeps_the_join_handle() {
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let mut task = TerminatableTask::spawn(
            ZRuntime::Application,
            async move { let _ = released.await; },
            CancellationToken::new(),
        );
        assert!(tokio::time::timeout(Duration::from_millis(10), task.terminate_async()).await.is_err());
        let retained = task.handle.is_some();
        release.send(()).unwrap();
        task.terminate_async().await;
        assert!(retained, "a canceled wait must not detach the task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_its_own_handle_does_not_wait_for_itself() {
        let (send_handle, receive_handle) = tokio::sync::oneshot::channel::<TerminatableTask>();
        let (done, finished) = tokio::sync::oneshot::channel();
        let task = TerminatableTask::spawn(
            ZRuntime::Application,
            async move {
                drop(receive_handle.await.unwrap());
                let _ = done.send(());
            },
            CancellationToken::new(),
        );
        send_handle.send(task).unwrap();
        tokio::time::timeout(Duration::from_secs(1), finished).await
            .expect("dropping the current task's owner must not self-join").unwrap();
    }
}
