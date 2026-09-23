use wasm_bindgen_test::*;
use zenoh_runtime::{wasm_yield, ZRuntime};
wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test(async)]
async fn joining_a_pending_task_keeps_its_waker() {
    let handle = ZRuntime::Application.spawn(async {
        wasm_yield::sleep_ms(20).await;
        42
    });
    assert_eq!(handle.await.unwrap(), 42);
}

#[wasm_bindgen_test(async)]
async fn abort_drops_a_pending_task_and_completes_join() {
    let handle = ZRuntime::Application.spawn(std::future::pending::<()>());
    wasm_yield::sleep_ms(10).await;
    handle.abort();
    assert!(handle.await.is_err());
}

#[wasm_bindgen_test(async)]
async fn cancellation_after_poll_and_child_isolation() {
    let parent = zenoh_task::CancellationToken::new();
    let child = parent.child_token();
    child.cancel();
    assert!(!parent.is_cancelled());
    let token = parent.clone();
    let handle = ZRuntime::Application.spawn(async move {
        token
            .run_until_cancelled(std::future::pending::<()>())
            .await
    });
    wasm_yield::sleep_ms(10).await;
    parent.cancel();
    assert!(handle.await.unwrap().is_none());
}

#[wasm_bindgen_test(async)]
async fn huge_checked_duration_does_not_truncate() {
    assert!(wasm_yield::Instant::now()
        .checked_add(std::time::Duration::MAX)
        .is_none());
}

#[wasm_bindgen_test(async)]
async fn abort_before_first_poll_releases_task_controller_count() {
    let controller = zenoh_task::TaskController::default();
    let handle = controller.spawn_abortable(std::future::pending::<()>());
    handle.abort();
    assert!(handle.await.is_err());
    controller.terminate_all_async().await;
    assert_eq!(controller.terminate_all(std::time::Duration::ZERO), 0);
}

#[wasm_bindgen_test(async)]
async fn dropping_join_handle_detaches_task() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let completed = Arc::new(AtomicBool::new(false));
    let completed_in_task = completed.clone();
    drop(ZRuntime::Application.spawn(async move {
        wasm_yield::sleep_ms(5).await;
        completed_in_task.store(true, Ordering::SeqCst);
    }));
    wasm_yield::sleep_ms(25).await;
    assert!(completed.load(Ordering::SeqCst));
}

#[wasm_bindgen_test(async)]
async fn sleep_above_signed_timer_limit_does_not_complete_early() {
    let mut sleep = Box::pin(wasm_yield::sleep_ms(u32::MAX));
    assert!(futures_lite::future::poll_once(&mut sleep).await.is_none());
    wasm_yield::sleep_ms(20).await;
    assert!(futures_lite::future::poll_once(&mut sleep).await.is_none());
}

#[wasm_bindgen_test(async)]
async fn sleep_respects_monotonic_deadline() {
    let start = wasm_yield::Instant::now();
    wasm_yield::sleep(std::time::Duration::from_millis(15)).await;
    assert!(start.elapsed() >= std::time::Duration::from_millis(15));
}

#[wasm_bindgen_test]
fn block_in_place_accepts_an_immediately_ready_future() {
    assert_eq!(ZRuntime::Application.block_in_place(async { 42 }), 42);
}

#[wasm_bindgen_test(async)]
async fn cancelled_shutdown_retains_the_join_handle() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let mut task = zenoh_task::TerminatableTask::spawn(
        ZRuntime::Application,
        async move {
            while !released.load(Ordering::SeqCst) {
                wasm_yield::sleep_ms(1).await;
            }
        },
        zenoh_task::CancellationToken::new(),
    );
    assert!(!task.terminate(Duration::ZERO));
    {
        let mut shutdown = Box::pin(task.terminate_async());
        assert!(futures_lite::future::poll_once(&mut shutdown)
            .await
            .is_none());
    }
    assert!(!task.terminate_async_timeout(Duration::from_millis(5)).await);
    assert!(!task.terminate(Duration::ZERO));
    release.store(true, Ordering::SeqCst);
    assert!(task.terminate_async_timeout(Duration::from_secs(1)).await);
    assert!(task.terminate(Duration::ZERO));
    // Repeated termination is idempotent, including a zero-duration deadline.
    assert!(task.terminate_async_timeout(Duration::ZERO).await);
}

#[wasm_bindgen_test(async)]
async fn dropped_long_js_sleeps_release_callbacks() {
    let baseline = wasm_yield::active_js_timers();
    let sleeps: Vec<_> = (0..128).map(|_| wasm_yield::sleep_ms(u32::MAX)).collect();
    assert_eq!(wasm_yield::active_js_timers(), baseline + 128);
    drop(sleeps);
    // Include an ordinary sleep to verify its callback is reclaimed too.
    wasm_yield::sleep_ms(1).await;
    wasm_yield::sleep_ms(80).await;
    // Only our most recent sleep may await its next cleanup tick.
    assert!(wasm_yield::active_js_timers() <= baseline + 1);
}
