use wasm_bindgen::prelude::*;

/// Entry point called from JavaScript.
#[wasm_bindgen(start)]
pub async fn main() {
    // Set up tracing to browser console
    tracing_subscriber::fmt()
        .with_writer(tracing_web::MakeConsoleWriter)
        .with_max_level(tracing::Level::INFO)
        .without_time()
        .init();

    tracing::info!("zenoh WASM client starting...");

    if let Err(e) = run().await {
        tracing::error!("Error: {}", e);
        log_to_page(&format!("ERROR: {}", e));
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Configure zenoh to connect to a router via WebSocket
    let mut config = zenoh::Config::default();
    // Connect to zenoh router on localhost:7447 via WebSocket
    // Change this to match your router's address
    config
        .insert_json5("connect/endpoints", r#"["ws/127.0.0.1:7447"]"#)
        .map_err(|e| format!("Config error: {e}"))?;
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .map_err(|e| format!("Config error: {e}"))?;

    log_to_page("Opening zenoh session (connecting to ws/127.0.0.1:7447)...");
    let session = zenoh::open(config).await.map_err(|e| format!("Open error: {e}"))?;
    log_to_page(&format!("Session opened! ZID: {}", session.zid()));

    // Subscribe to demo/example/**
    let key_expr = "demo/example/**";
    log_to_page(&format!("Subscribing to '{key_expr}'..."));
    let subscriber = session
        .declare_subscriber(key_expr)
        .await
        .map_err(|e| format!("Subscribe error: {e}"))?;
    log_to_page("Subscriber declared.");

    // Publish a test message
    let pub_key = "demo/example/wasm";
    log_to_page(&format!("Publishing to '{pub_key}'..."));
    session
        .put(pub_key, "Hello from WASM!")
        .await
        .map_err(|e| format!("Put error: {e}"))?;
    log_to_page("Published 'Hello from WASM!'");

    // Receive messages
    log_to_page("Waiting for messages...");
    loop {
        match subscriber.recv_async().await {
            Ok(sample) => {
                let payload = sample
                    .payload()
                    .try_to_string()
                    .unwrap_or_else(|e| e.to_string().into());
                let msg = format!(
                    "[{}] {} : {}",
                    sample.kind(),
                    sample.key_expr().as_str(),
                    payload
                );
                tracing::info!("{}", msg);
                log_to_page(&msg);
            }
            Err(e) => {
                log_to_page(&format!("Recv error: {e}"));
                break;
            }
        }
    }

    Ok(())
}

/// Append a message to the page's output div.
fn log_to_page(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        if let Some(output) = document.get_element_by_id("output") {
            let current = output.inner_html();
            output.set_inner_html(&format!("{current}<p>{msg}</p>"));
        }
    }
}
