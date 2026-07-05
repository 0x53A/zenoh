//! WASM-safe async yield and sleep that produce `Send` futures.
//!
//! Unlike gloo-timers, these use flume channels to bridge the JS callback
//! to a `Send`-safe future, avoiding the `!Send` JsValue issue.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Yield the current task so other tasks can run.
///
/// On a compute worker (wasm-threads), re-queues the task on its LocalExecutor.
/// On JS threads, `setTimeout(0)` yields to the browser event loop (macrotask,
/// so callbacks and rendering get a chance — a microtask re-queue would not).
pub fn yield_now() -> Sleep {
    #[cfg(feature = "wasm-threads")]
    if crate::has_local_executor() {
        return Sleep(SleepInner::Yield { yielded: false });
    }
    js_sleep(0)
}

/// Sleep for `ms` milliseconds.
///
/// On a compute worker (wasm-threads), uses the LocalExecutor's timer queue —
/// pure Rust, no JS. Elsewhere, schedules a `setTimeout` on the current
/// thread's event loop; the resulting future may be awaited on any thread
/// (the flume send crosses threads).
pub fn sleep_ms(ms: u32) -> Sleep {
    #[cfg(feature = "wasm-threads")]
    if crate::has_local_executor() {
        if ms == 0 {
            return Sleep(SleepInner::Yield { yielded: false });
        }
        return Sleep(SleepInner::Timer {
            deadline_ms: crate::executor::now_ms() + ms as u64,
        });
    }
    js_sleep(ms)
}

fn js_sleep(ms: u32) -> Sleep {
    let (tx, rx) = flume::bounded::<()>(1);
    // Schedule a setTimeout callback that signals the channel.
    // once_into_js hands ownership to JS — freed after the call fires.
    let cb = Closure::once_into_js(move || {
        let _ = tx.send(());
    });
    set_timeout(cb.unchecked_ref(), ms as i32);
    Sleep(SleepInner::Js(rx.into_recv_async()))
}

/// Future returned by [`sleep_ms`] / [`yield_now`]. `Send`, awaitable anywhere.
pub struct Sleep(SleepInner);

enum SleepInner {
    /// JS setTimeout signals a flume channel (created on a JS thread).
    Js(flume::r#async::RecvFut<'static, ()>),
    /// Deadline on the current worker's LocalExecutor timer queue.
    #[cfg(feature = "wasm-threads")]
    Timer { deadline_ms: u64 },
    /// Single re-queue on the LocalExecutor (yield semantics).
    #[cfg(feature = "wasm-threads")]
    Yield { yielded: bool },
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match &mut self.get_mut().0 {
            SleepInner::Js(fut) => match Pin::new(fut).poll(cx) {
                Poll::Ready(_) => Poll::Ready(()),
                Poll::Pending => Poll::Pending,
            },
            #[cfg(feature = "wasm-threads")]
            SleepInner::Timer { deadline_ms } => {
                if crate::executor::now_ms() >= *deadline_ms {
                    return Poll::Ready(());
                }
                if let Some(exec) = crate::executor::try_current_executor() {
                    exec.register_timer(*deadline_ms, cx.waker().clone());
                    Poll::Pending
                } else {
                    // Created on a compute worker but polled elsewhere (task
                    // migrated): no timer queue here — degrade to busy re-poll.
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            #[cfg(feature = "wasm-threads")]
            SleepInner::Yield { yielded } => {
                if *yielded {
                    Poll::Ready(())
                } else {
                    *yielded = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }
    }
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = "setTimeout")]
    fn set_timeout(f: &js_sys::Function, millis: i32);

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

    pub fn checked_sub(&self, duration: std::time::Duration) -> Option<Self> {
        self.0.checked_sub(duration.as_millis() as u64).map(Instant)
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
