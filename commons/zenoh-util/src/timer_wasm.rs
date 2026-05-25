use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Weak,
};
use std::time::Duration;

use async_trait::async_trait;

#[async_trait]
pub trait Timed {
    async fn run(&mut self);
}

#[derive(Clone)]
pub struct TimedHandle(Weak<AtomicBool>);

impl std::fmt::Debug for TimedHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimedHandle")
            .field("is_live", &self.0.strong_count().gt(&0))
            .finish()
    }
}

impl TimedHandle {
    pub fn defuse(self) {
        if let Some(arc) = self.0.upgrade() {
            arc.store(false, Ordering::Release);
        }
    }
}

#[derive(Clone)]
pub struct TimedEvent {
    fused: Arc<AtomicBool>,
}

impl std::fmt::Debug for TimedEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimedEvent").finish_non_exhaustive()
    }
}

impl TimedEvent {
    pub fn once(_when: std::time::Instant, _event: impl Timed + Send + Sync + 'static) -> Self {
        TimedEvent {
            fused: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn periodic(
        _interval: Duration,
        _event: impl Timed + Send + Sync + 'static,
    ) -> Self {
        TimedEvent {
            fused: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn is_fused(&self) -> bool {
        self.fused.load(Ordering::Acquire)
    }

    pub fn get_handle(&self) -> TimedHandle {
        TimedHandle(Arc::downgrade(&self.fused))
    }
}

pub struct Timer;

impl Timer {
    pub fn new(_spawn_blocking: bool) -> Self {
        Timer
    }

    pub fn add(&self, _event: TimedEvent) {}

    pub async fn add_async(&self, _event: TimedEvent) {}
}
