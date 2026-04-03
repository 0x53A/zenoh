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

//! WASM implementation of the Zenoh runtime.
//!
//! On WASM, there is no multi-threading and no tokio runtime.
//! We use `wasm-bindgen-futures::spawn_local` to spawn futures
//! and a channel-based `JoinHandle` for awaiting results.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use serde::Deserialize;

pub const ZENOH_RUNTIME_ENV: &str = "ZENOH_RUNTIME";

/// [`ZRuntime`] variants — on WASM these all map to the same single-threaded executor.
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
        // Cannot abort tasks on WASM — spawn_local tasks run to completion
    }

    pub fn is_finished(&self) -> bool {
        // If we can peek without consuming, the task is done
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

    /// Spawn a future on the WASM event loop.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (tx, rx) = flume::bounded(1);
        wasm_bindgen_futures::spawn_local(async move {
            let result = future.await;
            let _ = tx.send(result);
        });
        JoinHandle { rx }
    }

    /// On WASM, `block_in_place` cannot truly block.
    pub fn block_in_place<F, R>(&self, _f: F) -> R
    where
        F: Future<Output = R>,
    {
        panic!("block_in_place is not supported on WASM — use async/.await instead")
    }
}

// A runtime guard — no-op on WASM
pub struct ZRuntimePoolGuard;

impl Drop for ZRuntimePoolGuard {
    fn drop(&mut self) {}
}
