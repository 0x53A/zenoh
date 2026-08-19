//! Pure-Rust single-threaded async executor for WASM Web Workers.
//!
//! Designed for use with SharedArrayBuffer-backed threads. Each worker gets
//! its own `LocalExecutor` instance. Cross-worker waking uses `Condvar::notify`
//! which compiles to `memory.atomic.notify` — no JS event loop dependency.
//!
//! Key properties:
//! - `pump()` polls all ready tasks, driven by waker notifications
//! - `block_on()` polls a target future while pumping other tasks between waits
//! - `run()` is the blocking main loop for worker threads
//! - Wakers are `Send+Sync` (backed by `Arc<Mutex>` + `Condvar`)
//! - The executor itself is `!Send` (uses `RefCell` for task storage)
//! - Built-in timer queue for `sleep_ms` / timeout futures

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake};
use std::time::Duration;

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = Date, js_name = "now")]
    fn date_now() -> f64;
}

pub(crate) fn now_ms() -> u64 {
    date_now() as u64
}

thread_local! {
    static CURRENT_EXECUTOR: RefCell<Option<Rc<LocalExecutor>>> = const { RefCell::new(None) };
}

/// Get the current thread's executor. Returns `None` if not on a worker thread
/// (e.g., main thread or Acceptor worker using spawn_local).
pub(crate) fn try_current_executor() -> Option<Rc<LocalExecutor>> {
    CURRENT_EXECUTOR.with(|e| e.borrow().as_ref().cloned())
}

/// Returns true if the current thread has a LocalExecutor (i.e., is a compute worker).
pub fn has_local_executor() -> bool {
    CURRENT_EXECUTOR.with(|e| e.borrow().is_some())
}

/// Shared state between executor and wakers. Accessed from any thread via `Arc`.
struct WakeState {
    condvar: Condvar,
    inner: Mutex<WakeInner>,
}

struct WakeInner {
    /// Task IDs that have been woken and need re-polling.
    task_queue: Vec<usize>,
    /// Set by `BlockOnWaker` to signal the `block_on` caller.
    block_woken: bool,
    /// Pending timers: (deadline_ms, waker). Fired when `now_ms() >= deadline_ms`.
    timers: Vec<TimerEntry>,
}

struct TimerEntry {
    deadline_ms: u64,
    waker: std::task::Waker,
}

pub(crate) struct LocalExecutor {
    /// Task storage. `Some(future)` = live task, `None` = completed/empty slot.
    tasks: RefCell<Vec<Option<Pin<Box<dyn Future<Output = ()>>>>>>,
    /// Newly spawned futures, drained into `tasks` at the start of each pump cycle.
    /// Separate from `tasks` to avoid RefCell conflicts when a polled task spawns.
    spawn_queue: RefCell<Vec<Pin<Box<dyn Future<Output = ()>>>>>,
    /// Cross-thread wake notifications.
    wake_state: Arc<WakeState>,
    /// Monotonically increasing task ID counter.
    next_id: Cell<usize>,
}

impl LocalExecutor {
    pub fn new() -> Self {
        LocalExecutor {
            tasks: RefCell::new(Vec::new()),
            spawn_queue: RefCell::new(Vec::new()),
            wake_state: Arc::new(WakeState {
                condvar: Condvar::new(),
                inner: Mutex::new(WakeInner {
                    task_queue: Vec::new(),
                    block_woken: false,
                    timers: Vec::new(),
                }),
            }),
            next_id: Cell::new(0),
        }
    }

    /// Install this executor as the current thread's executor.
    pub fn install(self: &Rc<Self>) {
        CURRENT_EXECUTOR.with(|e| {
            *e.borrow_mut() = Some(self.clone());
        });
    }

    /// Spawn a future onto this executor. The future will be polled on the
    /// next `pump()` cycle.
    pub fn spawn(&self, f: impl Future<Output = ()> + 'static) {
        self.spawn_queue.borrow_mut().push(Box::pin(f));
        // Notify so run()/block_on() picks up the new task
        self.wake_state.condvar.notify_one();
    }

    /// Register a timer. When `now_ms() >= deadline_ms`, the waker will be called.
    /// Used by `CondvarSleep` futures for pure-Rust sleep on compute workers.
    pub fn register_timer(&self, deadline_ms: u64, waker: std::task::Waker) {
        let mut inner = self.wake_state.inner.lock().unwrap();
        inner.timers.push(TimerEntry { deadline_ms, waker });
        // If the timer is already expired, notify immediately
        if now_ms() >= deadline_ms {
            self.wake_state.condvar.notify_one();
        }
    }

    /// Fire expired timers. Collects wakers under the lock, then wakes them
    /// after releasing the lock to avoid deadlock (wakers re-acquire the lock).
    fn fire_expired_timers(&self) {
        let expired: Vec<std::task::Waker> = {
            let mut inner = self.wake_state.inner.lock().unwrap();
            let now = now_ms();
            let mut expired = Vec::new();
            inner.timers.retain(|t| {
                if t.deadline_ms <= now {
                    expired.push(t.waker.clone());
                    false
                } else {
                    true
                }
            });
            expired
        };
        for w in expired {
            w.wake();
        }
    }

    /// Compute the time until the nearest timer fires, capped at `max`.
    fn time_until_next_timer(&self, inner: &WakeInner, max: Duration) -> Duration {
        let now = now_ms();
        inner
            .timers
            .iter()
            .map(|t| Duration::from_millis(t.deadline_ms.saturating_sub(now)))
            .min()
            .map(|d| d.min(max))
            .unwrap_or(max)
    }

    /// Poll all ready tasks until no more are immediately available.
    ///
    /// Drains the spawn queue first, then repeatedly processes woken task IDs.
    /// Each task is taken out of its slot before polling (releasing the RefCell
    /// borrow) so that polled tasks can call `spawn()` without conflict.
    fn pump(&self) {
        loop {
            // Phase 1: move spawned futures into the task list and mark them ready
            {
                let mut sq = self.spawn_queue.borrow_mut();
                if !sq.is_empty() {
                    let mut tasks = self.tasks.borrow_mut();
                    let mut inner = self.wake_state.inner.lock().unwrap();
                    for fut in sq.drain(..) {
                        let id = self.next_id.get();
                        self.next_id.set(id + 1);
                        if id >= tasks.len() {
                            tasks.resize_with(id + 1, || None);
                        }
                        tasks[id] = Some(fut);
                        inner.task_queue.push(id);
                    }
                }
            }

            // Phase 2: take woken IDs
            let woken: Vec<usize> = {
                let mut inner = self.wake_state.inner.lock().unwrap();
                if inner.task_queue.is_empty() {
                    return;
                }
                std::mem::take(&mut inner.task_queue)
            };

            // Phase 3: poll each woken task
            for id in woken {
                // Take future out — releases borrow so poll can call spawn()
                let fut = self.tasks.borrow_mut().get_mut(id).and_then(|s| s.take());

                if let Some(mut fut) = fut {
                    let waker: std::task::Waker = Arc::new(TaskWaker {
                        task_id: id,
                        wake_state: self.wake_state.clone(),
                    })
                    .into();
                    let mut cx = Context::from_waker(&waker);

                    match fut.as_mut().poll(&mut cx) {
                        Poll::Ready(()) => {
                            // Done — slot stays None (could reclaim later)
                        }
                        Poll::Pending => {
                            // Put it back; waker will re-enqueue when ready
                            self.tasks.borrow_mut()[id] = Some(fut);
                        }
                    }
                }
            }
        }
    }

    /// Block the current thread until `f` completes, pumping other executor
    /// tasks between waits. This is the core of `block_in_place` for workers.
    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        let mut f = std::pin::pin!(f);

        let waker: std::task::Waker = Arc::new(BlockOnWaker(self.wake_state.clone())).into();
        let mut cx = Context::from_waker(&waker);

        loop {
            // Clear flag before poll so we detect wakes during/after poll
            self.wake_state.inner.lock().unwrap().block_woken = false;

            match f.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {
                    // Pump other tasks — they may produce the value we need
                    self.pump();

                    // Fire expired timers (may wake tasks)
                    self.fire_expired_timers();

                    // Check if anything needs attention before sleeping
                    let inner = self.wake_state.inner.lock().unwrap();
                    if inner.task_queue.is_empty()
                        && !inner.block_woken
                        && self.spawn_queue.borrow().is_empty()
                    {
                        // Sleep until woken, timer fires, or timeout
                        let timeout = self.time_until_next_timer(&inner, Duration::from_millis(1));
                        let _ = self.wake_state.condvar.wait_timeout(inner, timeout);
                    }
                }
            }
        }
    }

    /// Run the executor forever, pumping tasks and sleeping when idle.
    /// This is the entry point for compute worker threads.
    pub fn run(&self) -> ! {
        loop {
            self.pump();

            // Fire expired timers
            self.fire_expired_timers();

            let inner = self.wake_state.inner.lock().unwrap();
            if inner.task_queue.is_empty() && self.spawn_queue.borrow().is_empty() {
                let timeout = self.time_until_next_timer(&inner, Duration::from_millis(100));
                let _ = self.wake_state.condvar.wait_timeout(inner, timeout);
            }
        }
    }
}

/// Waker for executor tasks. Pushes the task ID back into the ready queue
/// and notifies the condvar. Safe to call from any thread.
struct TaskWaker {
    task_id: usize,
    wake_state: Arc<WakeState>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        let mut inner = self.wake_state.inner.lock().unwrap();
        inner.task_queue.push(self.task_id);
        drop(inner);
        self.wake_state.condvar.notify_one();
    }
}

/// Waker for `block_on`'s target future. Sets a flag and notifies the condvar.
struct BlockOnWaker(Arc<WakeState>);

impl Wake for BlockOnWaker {
    fn wake(self: Arc<Self>) {
        let mut inner = self.0.inner.lock().unwrap();
        inner.block_woken = true;
        drop(inner);
        self.0.condvar.notify_one();
    }
}
