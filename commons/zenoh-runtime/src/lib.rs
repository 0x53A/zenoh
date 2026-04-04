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

//! ⚠️ WARNING ⚠️
//!
//! This crate is intended for Zenoh's internal use.
//!
//! [Click here for Zenoh's documentation](https://docs.rs/zenoh/latest/zenoh)

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
mod wasm_threaded;
#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
pub use wasm_threaded::*;

#[cfg(all(target_arch = "wasm32", not(feature = "wasm-threads")))]
mod wasm;
#[cfg(all(target_arch = "wasm32", not(feature = "wasm-threads")))]
pub use wasm::*;

pub mod compat;

/// WASM-safe async yield/sleep utilities (Send-safe, works in Web Workers).
#[cfg(target_arch = "wasm32")]
pub mod wasm_yield;
