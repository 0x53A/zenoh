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

//! Runtime-dependent timers shared by native and browser transports.

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::{sleep, timeout};

#[cfg(target_arch = "wasm32")]
pub use crate::wasm_yield::sleep;

/// The operation did not complete within its deadline.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct Elapsed;

#[cfg(target_arch = "wasm32")]
impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

#[cfg(target_arch = "wasm32")]
impl std::error::Error for Elapsed {}

/// Apply a deadline without requiring Tokio's timer driver.
/// Dropping the timeout also drops the operation and its timer.
#[cfg(target_arch = "wasm32")]
pub async fn timeout<F: std::future::Future>(
    duration: std::time::Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    use futures::FutureExt;
    let future = future.fuse();
    let timer = sleep(duration).fuse();
    futures::pin_mut!(future, timer);
    futures::select_biased! {
        value = future => Ok(value),
        _ = timer => Err(Elapsed),
    }
}
