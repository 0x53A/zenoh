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

    /// High-resolution timestamp in milliseconds (from performance.now() or Date.now()).
    #[wasm_bindgen(js_namespace = Date, js_name = "now")]
    fn date_now() -> f64;
}

/// A WASM-compatible replacement for `std::time::Instant`.
/// Uses `Date.now()` (millisecond precision).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(u64); // milliseconds since epoch

impl Instant {
    pub fn now() -> Self {
        Instant(date_now() as u64)
    }

    pub fn elapsed(&self) -> std::time::Duration {
        let now = date_now() as u64;
        std::time::Duration::from_millis(now.saturating_sub(self.0))
    }

    pub fn duration_since(&self, earlier: Instant) -> std::time::Duration {
        std::time::Duration::from_millis(self.0.saturating_sub(earlier.0))
    }

    pub fn checked_add(&self, duration: std::time::Duration) -> Option<Self> {
        self.0.checked_add(duration.as_millis() as u64).map(Instant)
    }
}

impl std::ops::Add<std::time::Duration> for Instant {
    type Output = Instant;
    fn add(self, rhs: std::time::Duration) -> Instant {
        Instant(self.0 + rhs.as_millis() as u64)
    }
}

impl std::ops::AddAssign<std::time::Duration> for Instant {
    fn add_assign(&mut self, rhs: std::time::Duration) {
        self.0 += rhs.as_millis() as u64;
    }
}

impl std::ops::SubAssign<std::time::Duration> for Instant {
    fn sub_assign(&mut self, rhs: std::time::Duration) {
        self.0 = self.0.saturating_sub(rhs.as_millis() as u64);
    }
}

impl std::ops::Sub<Instant> for Instant {
    type Output = std::time::Duration;
    fn sub(self, rhs: Instant) -> std::time::Duration {
        std::time::Duration::from_millis(self.0.saturating_sub(rhs.0))
    }
}
