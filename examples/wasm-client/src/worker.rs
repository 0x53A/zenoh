//! Zenoh worker — runs in a Web Worker thread.
//!
//! The worker owns the zenoh session and processes commands from the main thread.

use std::collections::HashMap;

use wasm_bindgen::prelude::*;
use web_sys::DedicatedWorkerGlobalScope;

use crate::protocol::{FromWorker, ToWorker};

/// Send a message from worker to main thread.
fn post_to_main(scope: &DedicatedWorkerGlobalScope, msg: &FromWorker) {
    let json = serde_json::to_string(msg).unwrap();
    scope
        .post_message(&JsValue::from_str(&json))
        .unwrap_or_else(|e| {
            web_sys::console::error_1(&JsValue::from_str(&format!(
                "Worker: failed to post message: {:?}",
                e
            )));
        });
}

fn log_to_main(scope: &DedicatedWorkerGlobalScope, msg: &str) {
    post_to_main(scope, &FromWorker::Log { message: msg.to_string() });
}

/// Start the worker event loop. Called from the JS worker bootstrap.
#[wasm_bindgen]
pub async fn start_worker() {
    // Set up tracing
    tracing_subscriber::fmt()
        .with_writer(tracing_web::MakeConsoleWriter)
        .with_max_level(tracing::Level::INFO)
        .without_time()
        .init();

    std::panic::set_hook(Box::new(|info| {
        let msg = format!("WORKER PANIC: {}", info);
        web_sys::console::error_1(&JsValue::from_str(&msg));
    }));

    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    log_to_main(&scope, "Worker started, waiting for commands...");

    // Channel to receive messages from the onmessage handler
    let (cmd_tx, cmd_rx) = flume::unbounded::<ToWorker>();

    // Set up onmessage handler
    let onmessage = Closure::wrap(Box::new(move |event: web_sys::MessageEvent| {
        let data = event.data();
        if let Some(json_str) = data.as_string() {
            match serde_json::from_str::<ToWorker>(&json_str) {
                Ok(msg) => {
                    let _ = cmd_tx.send(msg);
                }
                Err(e) => {
                    web_sys::console::error_1(&JsValue::from_str(&format!(
                        "Worker: failed to parse message: {} (input: {})",
                        e, json_str
                    )));
                }
            }
        }
    }) as Box<dyn FnMut(web_sys::MessageEvent)>);
    scope.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget(); // prevent drop

    // Worker state
    let mut session: Option<zenoh::Session> = None;
    let mut subscribers: HashMap<u32, zenoh::pubsub::Subscriber<()>> = HashMap::new();

    // Command loop
    loop {
        let cmd = match cmd_rx.recv_async().await {
            Ok(cmd) => cmd,
            Err(_) => break,
        };

        match cmd {
            ToWorker::Open { endpoint } => {
                log_to_main(&scope, &format!("Opening session to {}...", endpoint));
                let mut config = zenoh::Config::default();
                let endpoints = format!(r#"["{}"]"#, endpoint);
                if let Err(e) = config.insert_json5("connect/endpoints", &endpoints) {
                    post_to_main(&scope, &FromWorker::Error {
                        message: format!("Config error: {e}"),
                    });
                    continue;
                }
                let _ = config.insert_json5("scouting/multicast/enabled", "false");

                match zenoh::open(config).await {
                    Ok(s) => {
                        let zid = s.zid().to_string();
                        session = Some(s);
                        post_to_main(&scope, &FromWorker::Opened { zid });
                    }
                    Err(e) => {
                        post_to_main(&scope, &FromWorker::Error {
                            message: format!("Open error: {e}"),
                        });
                    }
                }
            }

            ToWorker::Subscribe { id, key_expr } => {
                let Some(ref s) = session else {
                    post_to_main(&scope, &FromWorker::Error {
                        message: "No session open".into(),
                    });
                    continue;
                };
                log_to_main(&scope, &format!("Subscribing to '{key_expr}' (id={id})..."));

                let scope_clone = scope.clone();
                match s
                    .declare_subscriber(&key_expr)
                    .callback(move |sample| {
                        let payload = sample
                            .payload()
                            .try_to_string()
                            .unwrap_or_else(|e| e.to_string().into());
                        post_to_main(
                            &scope_clone,
                            &FromWorker::Sample {
                                sub_id: id,
                                key_expr: sample.key_expr().as_str().to_string(),
                                payload: payload.to_string(),
                                kind: format!("{}", sample.kind()),
                            },
                        );
                    })
                    .await
                {
                    Ok(sub) => {
                        subscribers.insert(id, sub);
                        log_to_main(&scope, &format!("Subscribed to '{key_expr}' (id={id})"));
                    }
                    Err(e) => {
                        post_to_main(&scope, &FromWorker::Error {
                            message: format!("Subscribe error: {e}"),
                        });
                    }
                }
            }

            ToWorker::Unsubscribe { id } => {
                if let Some(sub) = subscribers.remove(&id) {
                    drop(sub);
                    log_to_main(&scope, &format!("Unsubscribed (id={id})"));
                }
            }

            ToWorker::Put { key_expr, payload } => {
                let Some(ref s) = session else {
                    post_to_main(&scope, &FromWorker::Error {
                        message: "No session open".into(),
                    });
                    continue;
                };
                if let Err(e) = s.put(&key_expr, payload.as_bytes()).await {
                    post_to_main(&scope, &FromWorker::Error {
                        message: format!("Put error: {e}"),
                    });
                }
            }
        }
    }
}
