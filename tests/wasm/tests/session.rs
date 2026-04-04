use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

/// Verify we can open a zenoh session via WebSocket and get a valid ZenohId.
/// Requires a zenohd router running with: zenohd -l ws/127.0.0.1:7448
#[wasm_bindgen_test]
async fn session_open_close() {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("mode", r#""client""#)
        .expect("set mode");
    config
        .insert_json5("connect/endpoints", r#"["ws/127.0.0.1:7448"]"#)
        .expect("set endpoints");

    let session = zenoh::open(config).await.expect("Failed to open session");
    let zid = session.zid();
    web_sys::console::log_1(&format!("Session opened with ZID: {zid}").into());

    // ZID should not be all zeros (that would mean it wasn't assigned)
    let zid_str = format!("{zid}");
    assert!(!zid_str.chars().all(|c| c == '0'), "ZID should not be all zeros");

    session.close().await.expect("Failed to close session");
}

/// Verify pub/sub round-trip through the router.
/// Requires a zenohd router running with: zenohd -l ws/127.0.0.1:7448
#[wasm_bindgen_test]
async fn pubsub_roundtrip() {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("mode", r#""client""#)
        .expect("set mode");
    config
        .insert_json5("connect/endpoints", r#"["ws/127.0.0.1:7448"]"#)
        .expect("set endpoints");

    let session = zenoh::open(config).await.expect("Failed to open session");

    // Create subscriber
    let subscriber = session
        .declare_subscriber("test/wasm/roundtrip")
        .await
        .expect("Failed to declare subscriber");

    // Small delay for subscriber to propagate
    wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 200)
            .unwrap();
    }))
    .await
    .unwrap();

    // Publish a message
    session
        .put("test/wasm/roundtrip", "hello from wasm test")
        .await
        .expect("Failed to put");

    // Wait for the message to come back via the router
    let recv = subscriber.recv_async();

    // Use a timeout to avoid hanging forever
    let timeout = wasm_bindgen_futures::JsFuture::from(js_sys::Promise::new(
        &mut |resolve, _| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 5000)
                .unwrap();
        },
    ));

    // Race: either we get a sample or we time out
    let result = futures_lite::future::or(
        async {
            let sample = recv.await.expect("Subscriber channel closed");
            Some(sample)
        },
        async {
            timeout.await.ok();
            None
        },
    )
    .await;

    let sample = result.expect("Timed out waiting for pub/sub roundtrip");
    let payload = sample.payload().to_bytes();
    let payload_str = std::str::from_utf8(&payload).expect("payload should be UTF-8");
    assert_eq!(payload_str, "hello from wasm test");

    session.close().await.expect("Failed to close session");
}
