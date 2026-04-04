//! Message protocol between main thread and zenoh worker.
//!
//! All messages are serialized as JSON via postMessage.

use serde::{Deserialize, Serialize};

/// Messages sent from the main thread to the worker.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ToWorker {
    /// Open a zenoh session.
    #[serde(rename = "open")]
    Open { endpoint: String },

    /// Declare a subscriber.
    #[serde(rename = "subscribe")]
    Subscribe { id: u32, key_expr: String },

    /// Undeclare a subscriber.
    #[serde(rename = "unsubscribe")]
    Unsubscribe { id: u32 },

    /// Publish a value.
    #[serde(rename = "put")]
    Put {
        key_expr: String,
        payload: String,
    },
}

/// Messages sent from the worker to the main thread.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum FromWorker {
    /// Session opened successfully.
    #[serde(rename = "opened")]
    Opened { zid: String },

    /// A sample was received on a subscription.
    #[serde(rename = "sample")]
    Sample {
        sub_id: u32,
        key_expr: String,
        payload: String,
        kind: String,
    },

    /// An error occurred.
    #[serde(rename = "error")]
    Error { message: String },

    /// Log message for display.
    #[serde(rename = "log")]
    Log { message: String },
}
