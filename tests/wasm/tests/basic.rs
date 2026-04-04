use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

/// Verify zenoh config can be created and modified on WASM.
#[wasm_bindgen_test]
fn config_creation() {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("mode", r#""client""#)
        .expect("set mode");
    config
        .insert_json5("connect/endpoints", r#"["ws/127.0.0.1:7448"]"#)
        .expect("set endpoints");
}

/// Verify key expression operations work on WASM.
#[wasm_bindgen_test]
fn key_expression_ops() {
    use zenoh::key_expr::KeyExpr;

    let ke = KeyExpr::try_from("demo/example/test").unwrap();
    assert_eq!(ke.as_str(), "demo/example/test");

    let wild = KeyExpr::try_from("demo/example/**").unwrap();
    assert!(wild.intersects(&ke));

    let other = KeyExpr::try_from("other/path").unwrap();
    assert!(!wild.intersects(&other));
}

/// Verify ZBytes creation works on WASM.
#[wasm_bindgen_test]
fn zbytes_creation() {
    use zenoh::bytes::ZBytes;

    let zbytes = ZBytes::from("Hello from WASM test!");
    assert!(!zbytes.is_empty());

    let empty = ZBytes::new();
    assert!(empty.is_empty());
}

/// Verify ZenohId can be created and formatted on WASM.
#[wasm_bindgen_test]
fn zenoh_id_works() {
    let zid = zenoh::session::ZenohId::default();
    let formatted = format!("{}", zid);
    assert!(!formatted.is_empty());
}

/// Verify sample kind Display works (basic protocol types).
#[wasm_bindgen_test]
fn sample_kind_display() {
    use zenoh::sample::SampleKind;
    assert_eq!(format!("{}", SampleKind::Put), "PUT");
    assert_eq!(format!("{}", SampleKind::Delete), "DELETE");
}
