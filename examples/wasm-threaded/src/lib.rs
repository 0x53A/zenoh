use wasm_bindgen::prelude::*;

/// Test entry point — called from the HTML page.
/// Initializes the threaded runtime and runs basic tests.
#[wasm_bindgen]
pub async fn run_threaded_test() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {}", info);
        web_sys::console::error_1(&JsValue::from_str(&msg));
        log(&msg);
    }));

    log("=== Zenoh WASM Threaded Runtime Test ===");

    // Initialize the threaded runtime with the shim URL
    log("Initializing threaded runtime...");
    let ok = zenoh_runtime::__zenoh_init_threaded_runtime("./pkg/zenoh_wasm_threaded_test.js");
    if !ok {
        log("FAIL: SharedArrayBuffer not available. Are COOP/COEP headers set?");
        return;
    }
    log("Threaded runtime initialized!");

    // Give workers a moment to start up
    sleep_ms(500).await;

    // Test 1: spawn a future on a specific runtime and get the result
    log("Test 1: cross-worker spawn...");
    let handle = zenoh_runtime::ZRuntime::Net.spawn(async {
        42u32
    });
    match handle.await {
        Ok(val) => {
            if val == 42 {
                log("  PASS: spawn returned correct value (42)");
            } else {
                log(&format!("  FAIL: expected 42, got {val}"));
            }
        }
        Err(e) => log(&format!("  FAIL: join error: {e}")),
    }

    // Test 2: spawn on multiple runtimes
    log("Test 2: multi-runtime spawn...");
    let h1 = zenoh_runtime::ZRuntime::Application.spawn(async { "app" });
    let h2 = zenoh_runtime::ZRuntime::TX.spawn(async { "tx" });
    let h3 = zenoh_runtime::ZRuntime::RX.spawn(async { "rx" });
    let r1 = h1.await.unwrap_or("err");
    let r2 = h2.await.unwrap_or("err");
    let r3 = h3.await.unwrap_or("err");
    if r1 == "app" && r2 == "tx" && r3 == "rx" {
        log("  PASS: all runtimes returned correct values");
    } else {
        log(&format!("  FAIL: got {r1}, {r2}, {r3}"));
    }

    // Test 3: zenoh config creation (basic sanity check on a worker)
    log("Test 3: zenoh config on worker...");
    let h = zenoh_runtime::ZRuntime::Application.spawn(async {
        let mut config = zenoh::Config::default();
        config.insert_json5("mode", r#""client""#).is_ok()
    });
    match h.await {
        Ok(true) => log("  PASS: config created on worker"),
        Ok(false) => log("  FAIL: config insert failed"),
        Err(e) => log(&format!("  FAIL: {e}")),
    }

    log("=== Tests complete ===");
}

fn log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        if let Some(output) = document.get_element_by_id("output") {
            let current = output.inner_html();
            output.set_inner_html(&format!("{current}<p>{msg}</p>"));
        }
    }
}

async fn sleep_ms(ms: u32) {
    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms as i32)
            .unwrap();
    }))
    .await
    .unwrap();
}
