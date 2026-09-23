//! WASM-safe async yield and sleep that produce `Send` futures.
//!
//! Unlike gloo-timers, these use flume channels to bridge the JS callback
//! to a `Send`-safe future, avoiding the `!Send` JsValue issue.

use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
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
            registration: None,
        });
    }
    js_sleep(ms)
}

// JS handles never enter the Send sleep future. Their owner remains on the
// creating thread, and observes receiver disconnection even if the future is
// dropped on a different worker. One short cleanup timer serves all live timers
// on this thread; background throttling can delay cleanup until the event loop
// runs again, but cancelled long sleeps do not retain callbacks until expiry.
struct JsTimer {
    id: i32,
    sender: flume::Sender<()>,
    fired: Rc<Cell<bool>>,
    _callback: Closure<dyn FnMut()>,
}

impl Drop for JsTimer {
    fn drop(&mut self) {
        if !self.fired.get() {
            clear_timeout(self.id);
        }
        // The callback is dropped afterwards, on its owning JS thread.
    }
}

thread_local! {
    static JS_TIMERS: RefCell<Vec<JsTimer>> = const { RefCell::new(Vec::new()) };
}

fn schedule_timer_cleanup() {
    let cleanup = Closure::once_into_js(|| {
        let more = JS_TIMERS.with(|timers| {
            let mut timers = timers.borrow_mut();
            // Cleanup runs in a separate event-loop callback, so it cannot
            // destroy a timer callback while that callback is executing.
            timers.retain(|timer| !timer.fired.get() && !timer.sender.is_disconnected());
            if timers.is_empty() {
                timers.shrink_to_fit();
                false
            } else {
                true
            }
        });
        if more {
            schedule_timer_cleanup();
        }
    });
    set_timeout(cleanup.unchecked_ref(), 16);
}

/// Number of JS timer callbacks owned by the current thread, for diagnostics.
/// Completed/cancelled callbacks are reclaimed on the next cleanup tick.
pub fn active_js_timers() -> usize {
    JS_TIMERS.with(|timers| timers.borrow().len())
}

fn js_sleep(ms: u32) -> Sleep {
    let (tx, rx) = flume::bounded::<()>(1);
    let fired = Rc::new(Cell::new(false));
    let callback_fired = fired.clone();
    let callback_tx = tx.clone();
    let callback = Closure::wrap(Box::new(move || {
        callback_fired.set(true);
        let _ = callback_tx.try_send(());
    }) as Box<dyn FnMut()>);
    let id = set_timeout(
        callback.as_ref().unchecked_ref(),
        ms.min(i32::MAX as u32) as i32,
    );
    let start_cleanup = JS_TIMERS.with(|timers| {
        let mut timers = timers.borrow_mut();
        let was_empty = timers.is_empty();
        timers.push(JsTimer {
            id,
            sender: tx,
            fired,
            _callback: callback,
        });
        was_empty
    });
    if start_cleanup {
        schedule_timer_cleanup();
    }
    Sleep(SleepInner::Js {
        deadline_ms: now_ms() + ms as u64,
        future: rx.into_recv_async(),
    })
}

/// Future returned by [`sleep_ms`] / [`yield_now`]. `Send`, awaitable anywhere.
pub struct Sleep(SleepInner);

enum SleepInner {
    /// JS setTimeout signals a flume channel (created on a JS thread).
    Js {
        deadline_ms: u64,
        future: flume::r#async::RecvFut<'static, ()>,
    },
    /// Deadline on the current worker's LocalExecutor timer queue.
    #[cfg(feature = "wasm-threads")]
    Timer {
        deadline_ms: u64,
        registration: Option<crate::executor::TimerRegistration>,
    },
    /// Single re-queue on the LocalExecutor (yield semantics).
    #[cfg(feature = "wasm-threads")]
    Yield { yielded: bool },
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        #[cfg(feature = "wasm-threads")]
        if crate::has_local_executor() {
            if let SleepInner::Js { deadline_ms, .. } = &this.0 {
                // A Send sleep can leave its creating JS thread. Compute workers
                // never service JS callbacks, including a long sleep's next
                // chunk. Keep the original deadline, but use their timer queue.
                // Dropping the receiver also cancels the original JS timer on
                // its owning thread's cleanup tick.
                this.0 = SleepInner::Timer {
                    deadline_ms: *deadline_ms,
                    registration: None,
                };
            }
        }
        match &mut this.0 {
            SleepInner::Js {
                deadline_ms,
                future,
            } => match Pin::new(future).poll(cx) {
                Poll::Ready(_) => {
                    let remaining = deadline_ms.saturating_sub(now_ms());
                    if remaining == 0 {
                        return Poll::Ready(());
                    }
                    this.0 = js_sleep(remaining.min(u32::MAX as u64) as u32).0;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Poll::Pending => Poll::Pending,
            },
            #[cfg(feature = "wasm-threads")]
            SleepInner::Timer {
                deadline_ms,
                registration,
            } => {
                if crate::executor::now_ms() >= *deadline_ms {
                    return Poll::Ready(());
                }
                if let Some(exec) = crate::executor::try_current_executor() {
                    // A sleep moved between executors can race expiry on
                    // the old executor. If it already removed the timer, its
                    // wake targeted the previous task: register our new waker
                    // even when the deadline has just passed.
                    let updated = registration
                        .as_ref()
                        .is_some_and(|timer| timer.update_waker(&exec, cx.waker()));
                    if !updated {
                        *registration = Some(exec.register_timer(*deadline_ms, cx.waker().clone()));
                    }
                    Poll::Pending
                } else {
                    // A Send sleep may be moved to a JS event-loop thread.
                    // Use a real timer there instead of starving the event loop
                    // with a microtask busy loop.
                    let remaining = deadline_ms.saturating_sub(now_ms());
                    this.0 = js_sleep(remaining.min(u32::MAX as u64) as u32).0;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            #[cfg(feature = "wasm-threads")]
            SleepInner::Yield { yielded } => {
                if !crate::has_local_executor() {
                    this.0 = js_sleep(0).0;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
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
    fn set_timeout(f: &js_sys::Function, millis: i32) -> i32;

    #[wasm_bindgen(js_name = "clearTimeout")]
    fn clear_timeout(id: i32);

}

thread_local! {
    static PERFORMANCE: web_sys::Performance = js_sys::Reflect::get(
        &js_sys::global(), &JsValue::from_str("performance")
    ).expect("performance is available in browsers and workers").unchecked_into();
}

// timeOrigin aligns the monotonic clocks of the page and worker contexts.
pub(crate) fn now_ms() -> u64 {
    PERFORMANCE.with(|p| (p.time_origin() + p.now()) as u64)
}

/// Sleep without overflowing the signed 32-bit browser setTimeout argument.
pub async fn sleep(duration: std::time::Duration) {
    let mut remaining = duration.as_millis() + u128::from(duration.subsec_nanos() % 1_000_000 != 0);
    while remaining > 0 {
        let chunk = remaining.min(i32::MAX as u128) as u32;
        sleep_ms(chunk).await;
        remaining -= chunk as u128;
    }
}

/// A WASM-compatible replacement for `std::time::Instant`.
/// Uses the browser monotonic performance clock (millisecond precision).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(u64); // milliseconds since epoch

impl Instant {
    pub fn now() -> Self {
        Instant(now_ms())
    }

    pub fn elapsed(&self) -> std::time::Duration {
        let now = now_ms();
        std::time::Duration::from_millis(now.saturating_sub(self.0))
    }

    pub fn duration_since(&self, earlier: Instant) -> std::time::Duration {
        std::time::Duration::from_millis(self.0.saturating_sub(earlier.0))
    }

    pub fn saturating_duration_since(&self, earlier: Instant) -> std::time::Duration {
        self.duration_since(earlier)
    }

    pub fn checked_add(&self, duration: std::time::Duration) -> Option<Self> {
        self.0
            .checked_add(u64::try_from(duration.as_millis()).ok()?)
            .map(Instant)
    }

    pub fn checked_sub(&self, duration: std::time::Duration) -> Option<Self> {
        self.0
            .checked_sub(u64::try_from(duration.as_millis()).ok()?)
            .map(Instant)
    }
}

impl std::ops::Add<std::time::Duration> for Instant {
    type Output = Instant;
    fn add(self, rhs: std::time::Duration) -> Instant {
        self.checked_add(rhs)
            .expect("overflow when adding duration to instant")
    }
}

impl std::ops::AddAssign<std::time::Duration> for Instant {
    fn add_assign(&mut self, rhs: std::time::Duration) {
        *self = *self + rhs;
    }
}

impl std::ops::SubAssign<std::time::Duration> for Instant {
    fn sub_assign(&mut self, rhs: std::time::Duration) {
        *self = self
            .checked_sub(rhs)
            .expect("overflow when subtracting duration from instant");
    }
}

impl std::ops::Sub<Instant> for Instant {
    type Output = std::time::Duration;
    fn sub(self, rhs: Instant) -> std::time::Duration {
        std::time::Duration::from_millis(self.0.saturating_sub(rhs.0))
    }
}
