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

//! Platform compatibility layer.
//!
//! Re-exports async primitives that work on both native (tokio) and WASM targets.
//! Transport code should use these instead of importing tokio directly.

// --- Async Mutex / RwLock ---

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::sync::{
    Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard, RwLock as AsyncRwLock,
};

// With wasm-threads (SharedArrayBuffer), use tokio::sync::Mutex which
// works correctly with cross-worker waking and block_in_place.
// Without wasm-threads, futures::lock::Mutex is fine for single-threaded.
#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
pub use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

#[cfg(all(target_arch = "wasm32", not(feature = "wasm-threads")))]
pub use futures::lock::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

// futures doesn't have RwLock — on WASM, use std's.
// With wasm-threads (SharedArrayBuffer), std::sync::RwLock uses real atomics.
// This is safe because AsyncRwLock is only used in native-only link crates
// (serial, vsock, unixsock, native ws) which don't compile on WASM.
#[cfg(target_arch = "wasm32")]
pub use std::sync::RwLock as AsyncRwLock;
