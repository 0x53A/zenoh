use wasm_bindgen::prelude::*;

/// Test entry point — called from the HTML page.
/// Initializes the threaded runtime and runs basic tests.
/// `endpoint` is the zenoh router endpoint for tests 5/6,
/// e.g. "ws/127.0.0.1:7448" or "wss/host:443".
#[wasm_bindgen]
pub async fn run_threaded_test(endpoint: String) {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {}", info);
        web_sys::console::error_1(&JsValue::from_str(&msg));
        log(&msg);
    }));

    // Route zenoh's tracing output to the browser console (workers included).
    // Raise to DEBUG/TRACE when diagnosing transport issues.
    tracing_wasm::set_as_global_default_with_config(
        tracing_wasm::WASMLayerConfigBuilder::new()
            .set_max_level(tracing::Level::INFO)
            .set_console_config(tracing_wasm::ConsoleConfig::ReportWithoutConsoleColor)
            .build(),
    );

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
        Ok(val) if val == 42 => log("  PASS: spawn returned correct value (42)"),
        Ok(val) => log(&format!("  FAIL: expected 42, got {val}")),
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

    // Test 4: block_in_place with a future that resolves via cross-worker wake.
    // The future uses a flume channel — the sender fires from another worker,
    // which wakes the Condvar in block_in_place via the channel's waker.
    log("Test 4: block_in_place with cross-worker resolution...");
    let h = zenoh_runtime::ZRuntime::Application.spawn(async {
        let (tx, rx) = flume::bounded::<u32>(1);

        // Spawn a task on Net worker that sends a value after async delay
        zenoh_runtime::ZRuntime::Net.spawn(async move {
            zenoh_runtime::wasm_yield::sleep_ms(100).await;
            let _ = tx.send(99);
        });

        // block_in_place: blocks the Application worker's thread via Condvar.
        // The flume channel's waker calls Condvar::notify when the Net worker
        // sends the value, unblocking this thread.
        zenoh_runtime::ZRuntime::Application.block_in_place(async {
            rx.recv_async().await.unwrap_or(0)
        })
    });
    match h.await {
        Ok(99) => log("  PASS: block_in_place resolved correctly (99)"),
        Ok(val) => log(&format!("  FAIL: expected 99, got {val}")),
        Err(e) => log(&format!("  FAIL: join error: {e}")),
    }

    // Test 5: zenoh session open (requires zenohd on the given endpoint)
    log(&format!("Test 5: zenoh session open on worker ({endpoint})..."));
    let ep = endpoint.clone();
    let h = zenoh_runtime::ZRuntime::Application.spawn(async move {
        web_sys::console::log_1(&JsValue::from_str("[test5] creating config..."));
        let mut config = zenoh::Config::default();
        config.insert_json5("mode", r#""client""#).unwrap();
        config.insert_json5("connect/endpoints", &format!(r#"["{ep}"]"#)).unwrap();
        config.insert_json5("scouting/multicast/enabled", "false").unwrap();
        web_sys::console::log_1(&JsValue::from_str("[test5] calling zenoh::open()..."));
        match zenoh::open(config).await {
            Ok(session) => {
                let zid = session.zid().to_string();
                let _ = session.close().await;
                Some(zid)
            }
            Err(e) => {
                web_sys::console::error_1(&JsValue::from_str(&format!("Open error: {e}")));
                None
            }
        }
    });
    match h.await {
        Ok(Some(zid)) => log(&format!("  PASS: session opened, ZID={zid}")),
        Ok(None) => log(&format!("  FAIL: session open returned error (is zenohd running on {endpoint}?)")),
        Err(e) => log(&format!("  FAIL: join error: {e}")),
    }

    // Test 6: pub/sub roundtrip through the router (requires zenohd)
    log("Test 6: pub/sub roundtrip on workers...");
    let ep = endpoint.clone();
    let h = zenoh_runtime::ZRuntime::Application.spawn(async move {
        let mut config = zenoh::Config::default();
        config.insert_json5("mode", r#""client""#).unwrap();
        config.insert_json5("connect/endpoints", &format!(r#"["{ep}"]"#)).unwrap();
        config.insert_json5("scouting/multicast/enabled", "false").unwrap();
        let session = match zenoh::open(config).await {
            Ok(s) => s,
            Err(e) => return Err(format!("open: {e}")),
        };
        let sub = match session.declare_subscriber("wasm/threaded/roundtrip").await {
            Ok(s) => s,
            Err(e) => return Err(format!("subscriber: {e}")),
        };
        // Give the router a moment to propagate the subscription
        zenoh_runtime::wasm_yield::sleep_ms(500).await;
        if let Err(e) = session.put("wasm/threaded/roundtrip", "ping-from-worker").await {
            return Err(format!("put: {e}"));
        }
        let sample = match sub.recv_async().await {
            Ok(s) => s,
            Err(e) => return Err(format!("recv: {e}")),
        };
        let payload = sample
            .payload()
            .try_to_string()
            .map(|s| s.into_owned())
            .unwrap_or_default();
        let _ = session.close().await;
        Ok(payload)
    });
    match h.await {
        Ok(Ok(p)) if p == "ping-from-worker" => log("  PASS: pub/sub roundtrip delivered payload"),
        Ok(Ok(p)) => log(&format!("  FAIL: wrong payload: {p}")),
        Ok(Err(e)) => log(&format!("  FAIL: {e}")),
        Err(e) => log(&format!("  FAIL: join error: {e}")),
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
