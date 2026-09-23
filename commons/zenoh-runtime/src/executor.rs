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
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::task::{Context, Poll, Wake};
use std::time::Duration;

pub(crate) use crate::wasm_yield::now_ms;

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

impl WakeState {
    fn lock(&self) -> MutexGuard<'_, WakeInner> {
        if has_local_executor() {
            return self.inner.lock().unwrap();
        }
        // JS callbacks can wake compute tasks or drop migrated timers. A
        // contended std mutex would execute Atomics.wait on the browser main
        // thread, where it throws. These critical sections never await or poll
        // user futures; use non-sleeping acquisition on event-loop threads.
        loop {
            match self.inner.try_lock() {
                Ok(guard) => return guard,
                Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
                Err(TryLockError::Poisoned(_)) => panic!("executor wake state poisoned"),
            }
        }
    }
}

struct WakeInner {
    /// Task IDs that have been woken and need re-polling.
    task_queue: VecDeque<usize>,
    queued: HashSet<usize>,
    /// Set by `BlockOnWaker` to signal the `block_on` caller.
    block_woken: bool,
    /// Pending timers: (deadline_ms, waker). Fired when `now_ms() >= deadline_ms`.
    timers: HashMap<usize, TimerEntry>,
    next_timer: usize,
}

struct TimerEntry {
    deadline_ms: u64,
    waker: std::task::Waker,
}

pub(crate) struct LocalExecutor {
    /// Live tasks; completed futures are removed instead of retaining empty slots.
    tasks: RefCell<HashMap<usize, Pin<Box<dyn Future<Output = ()>>>>>,
    /// Newly spawned futures, drained into `tasks` at the start of each pump cycle.
    /// Separate from `tasks` to avoid RefCell conflicts when a polled task spawns.
    spawn_queue: RefCell<Vec<Pin<Box<dyn Future<Output = ()>>>>>,
    active: RefCell<HashSet<usize>>,
    /// Cross-thread wake notifications.
    wake_state: Arc<WakeState>,
    /// Monotonically increasing task ID counter.
    next_id: Cell<usize>,
}

impl LocalExecutor {
    pub fn new() -> Self {
        LocalExecutor {
            tasks: RefCell::new(HashMap::new()),
            spawn_queue: RefCell::new(Vec::new()),
            active: RefCell::new(HashSet::new()),
            wake_state: Arc::new(WakeState {
                condvar: Condvar::new(),
                inner: Mutex::new(WakeInner {
                    task_queue: VecDeque::new(),
                    queued: HashSet::new(),
                    block_woken: false,
                    timers: HashMap::new(),
                    next_timer: 0,
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
    pub fn register_timer(&self, deadline_ms: u64, waker: std::task::Waker) -> TimerRegistration {
        let mut inner = self.wake_state.lock();
        let id = inner.next_timer;
        inner.next_timer += 1;
        inner.timers.insert(id, TimerEntry { deadline_ms, waker });
        TimerRegistration {
            id,
            state: self.wake_state.clone(),
        }
    }

    /// Fire expired timers. Collects wakers under the lock, then wakes them
    /// after releasing the lock to avoid deadlock (wakers re-acquire the lock).
    fn fire_expired_timers(&self) {
        let expired: Vec<std::task::Waker> = {
            let mut inner = self.wake_state.lock();
            let now = now_ms();
            let mut expired = Vec::new();
            inner.timers.retain(|_, t| {
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
            .values()
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
        // Bound each turn: a task that repeatedly yields must not starve timers
        // or the future being driven by block_on. Pop one ID at a time so a
        // nested block_on can still access all other ready tasks.
        for _ in 0..64 {
            {
                let mut sq = self.spawn_queue.borrow_mut();
                let mut tasks = self.tasks.borrow_mut();
                let mut inner = self.wake_state.lock();
                for fut in sq.drain(..) {
                    let id = self.next_id.get();
                    self.next_id
                        .set(id.checked_add(1).expect("task ID overflow"));
                    tasks.insert(id, fut);
                    inner.queued.insert(id);
                    inner.task_queue.push_back(id);
                }
            }
            let id = {
                let mut inner = self.wake_state.lock();
                let Some(id) = inner.task_queue.pop_front() else {
                    return;
                };
                inner.queued.remove(&id);
                id
            };
            let fut = self.tasks.borrow_mut().remove(&id);
            if let Some(mut fut) = fut {
                self.active.borrow_mut().insert(id);
                let waker = Arc::new(TaskWaker {
                    task_id: id,
                    wake_state: self.wake_state.clone(),
                })
                .into();
                let mut cx = Context::from_waker(&waker);
                let pending = fut.as_mut().poll(&mut cx).is_pending();
                self.active.borrow_mut().remove(&id);
                if pending {
                    self.tasks.borrow_mut().insert(id, fut);
                }
            } else if self.active.borrow().contains(&id) {
                // A nested block_on cannot poll its suspended caller, but it
                // must preserve that caller's wake for when the poll returns.
                let mut inner = self.wake_state.lock();
                if inner.queued.insert(id) {
                    inner.task_queue.push_back(id);
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
            self.wake_state.lock().block_woken = false;

            match f.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {
                    // Pump other tasks — they may produce the value we need
                    self.pump();

                    // Fire expired timers (may wake tasks)
                    self.fire_expired_timers();

                    // Check if anything needs attention before sleeping
                    let inner = self.wake_state.lock();
                    // Wakes for callers suspended inside a nested block_on
                    // must stay queued, but cannot make progress until their
                    // current poll returns. Do not busy-spin on those IDs.
                    let has_runnable = {
                        let active = self.active.borrow();
                        inner.task_queue.iter().any(|id| !active.contains(id))
                    };
                    if !has_runnable && !inner.block_woken && self.spawn_queue.borrow().is_empty() {
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

            let inner = self.wake_state.lock();
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
        let mut inner = self.wake_state.lock();
        if inner.queued.insert(self.task_id) {
            inner.task_queue.push_back(self.task_id);
        }
        drop(inner);
        self.wake_state.condvar.notify_one();
    }
}

/// Waker for `block_on`'s target future. Sets a flag and notifies the condvar.
struct BlockOnWaker(Arc<WakeState>);

impl Wake for BlockOnWaker {
    fn wake(self: Arc<Self>) {
        let mut inner = self.0.lock();
        inner.block_woken = true;
        drop(inner);
        self.0.condvar.notify_one();
    }
}

/// Removes a sleep's timer when the future completes or is cancelled.
pub(crate) struct TimerRegistration {
    id: usize,
    state: Arc<WakeState>,
}

impl TimerRegistration {
    pub fn update_waker(&self, executor: &LocalExecutor, waker: &std::task::Waker) -> bool {
        // Migrated sleeps must not depend on the old worker making progress.
        if !Arc::ptr_eq(&self.state, &executor.wake_state) {
            return false;
        }
        if let Some(timer) = self.state.lock().timers.get_mut(&self.id) {
            timer.waker.clone_from(waker);
            true
        } else {
            false
        }
    }
}

impl Drop for TimerRegistration {
    fn drop(&mut self) {
        self.state.lock().timers.remove(&self.id);
    }
}
