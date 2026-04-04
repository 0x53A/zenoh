mod protocol;
mod worker;

use wasm_bindgen::prelude::*;
use web_sys::Worker;

use protocol::{FromWorker, ToWorker};

/// Main thread entry point — called explicitly from the HTML page.
#[wasm_bindgen]
pub fn start_main() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {}", info);
        web_sys::console::error_1(&JsValue::from_str(&msg));
        log_to_page(&msg);
    }));

    log_to_page("Starting zenoh worker...");

    // Spawn the Web Worker (classic worker — works in all browsers)
    let worker = Worker::new("./worker_bootstrap.js")
        .expect("Failed to create Web Worker");

    // Handle messages from worker
    let worker_for_cmds = worker.clone();
    let onmessage = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
        let data = event.data();
        if let Some(json_str) = data.as_string() {
            match serde_json::from_str::<FromWorker>(&json_str) {
                Ok(FromWorker::Opened { zid }) => {
                    log_to_page(&format!("Session opened! ZID: {zid}"));
                    // Now subscribe
                    send_to_worker(&worker_for_cmds, &ToWorker::Subscribe {
                        id: 1,
                        key_expr: "demo/example/**".into(),
                    });
                    // And publish a test message
                    send_to_worker(&worker_for_cmds, &ToWorker::Put {
                        key_expr: "demo/example/wasm".into(),
                        payload: "Hello from WASM!".into(),
                    });
                }
                Ok(FromWorker::Sample { key_expr, payload, kind, .. }) => {
                    log_to_page(&format!("[{kind}] {key_expr} : {payload}"));
                }
                Ok(FromWorker::Error { message }) => {
                    log_to_page(&format!("ERROR: {message}"));
                }
                Ok(FromWorker::Log { message }) => {
                    log_to_page(&format!("worker: {message}"));
                }
                Err(e) => {
                    log_to_page(&format!("Failed to parse worker message: {e}"));
                }
            }
        }
    }) as Box<dyn FnMut(web_sys::MessageEvent)>);
    worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    // Handle worker errors
    let onerror = Closure::wrap(Box::new(move |event: web_sys::ErrorEvent| {
        log_to_page(&format!("Worker error: {}", event.message()));
    }) as Box<dyn FnMut(web_sys::ErrorEvent)>);
    worker.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    // Open session — subscribe and publish happen in the Opened callback above
    send_to_worker(&worker, &ToWorker::Open {
        endpoint: "ws/127.0.0.1:7448".into(),
    });

    log_to_page("Open command sent to worker...");
}

fn send_to_worker(worker: &Worker, msg: &ToWorker) {
    let json = serde_json::to_string(msg).unwrap();
    worker
        .post_message(&JsValue::from_str(&json))
        .expect("Failed to send message to worker");
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
