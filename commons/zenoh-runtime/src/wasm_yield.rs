//! WASM-safe async yield and sleep that produce `Send` futures.
//!
//! Unlike gloo-timers, these use flume channels to bridge the JS callback
//! to a `Send`-safe future, avoiding the `!Send` JsValue issue.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Yield to the browser event loop via `setTimeout(0)`.
/// Returns a `Send` future safe for use in `tokio::select!` and `async_trait`.
pub fn yield_now() -> impl std::future::Future<Output = ()> + Send {
    sleep_ms(0)
}

/// Sleep for `ms` milliseconds, yielding to the browser event loop.
/// Returns a `Send` future.
pub fn sleep_ms(ms: u32) -> impl std::future::Future<Output = ()> + Send {
    let (tx, rx) = flume::bounded::<()>(1);
    // Schedule a setTimeout callback that signals the channel
    let cb = Closure::once(move || {
        let _ = tx.send(());
    });
    set_timeout(&cb, ms as i32);
    cb.forget(); // prevent drop — JS holds the reference
    async move {
        let _ = rx.recv_async().await;
    }
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = "setTimeout")]
    fn set_timeout(closure: &Closure<dyn FnMut()>, millis: i32);
}
