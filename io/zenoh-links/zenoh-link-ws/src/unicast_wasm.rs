//
// Copyright (c) 2024 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

//! WASM WebSocket link implementation using web-sys::WebSocket.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use web_sys::{BinaryType, ErrorEvent, MessageEvent, WebSocket};
use zenoh_link_commons::{
    LinkAuthId, LinkManagerUnicastTrait, LinkUnicast, LinkUnicastTrait, NewLinkChannelSender,
};
use zenoh_protocol::{
    core::{EndPoint, Locator, Priority},
    transport::BatchSize,
};
use zenoh_result::{bail, zerror, ZResult};

use super::{WS_DEFAULT_MTU, WS_LOCATOR_PREFIX};

/// Wrapper to make JsValue-based types Send+Sync on WASM.
/// SAFETY: With shared-memory workers, the wrapped JS object is thread-affine —
/// only accessed from the worker that created it. The write_tx channel ensures
/// write requests are proxied to the correct worker.
struct SendWrapper<T>(T);
unsafe impl<T> Send for SendWrapper<T> {}
unsafe impl<T> Sync for SendWrapper<T> {}

impl<T> std::ops::Deref for SendWrapper<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Write request sent through the channel to the WebSocket-owning worker.
enum WriteCmd {
    Send(Vec<u8>),
    Close,
}

pub struct LinkUnicastWs {
    /// Channel for sending write requests to the I/O worker that owns the WebSocket.
    write_tx: flume::Sender<WriteCmd>,
    recv_rx: flume::Receiver<Vec<u8>>,
    src_locator: Locator,
    dst_locator: Locator,
    leftovers: tokio::sync::Mutex<Option<(Vec<u8>, usize, usize)>>,
    /// Dropping this signals the I/O worker to release WebSocket closures.
    _io_close_tx: flume::Sender<()>,
}

/// Result of synchronously setting up a WebSocket connection.
/// All !Send types are already wrapped in SendWrapper.
/// Closures are stored and returned so they can be dropped when the link closes.
struct WsSetup {
    write_tx: flume::Sender<WriteCmd>,
    recv_rx: flume::Receiver<Vec<u8>>,
    open_rx: flume::Receiver<Result<(), String>>,
    msg_closures: SendWrapper<Vec<Closure<dyn FnMut(MessageEvent)>>>,
    err_closures: SendWrapper<Vec<Closure<dyn FnMut(ErrorEvent)>>>,
}

/// Create and configure WebSocket synchronously (no .await).
/// Returns only Send-safe types. JS closures are stored in the setup for later cleanup.
fn setup_ws(url: &str) -> ZResult<WsSetup> {
    let ws = WebSocket::new(url)
        .map_err(|e| zerror!("Failed to create WebSocket to {}: {:?}", url, e))?;
    ws.set_binary_type(BinaryType::Arraybuffer);

    let (recv_tx, recv_rx) = flume::unbounded::<Vec<u8>>();
    let (open_tx, open_rx) = flume::bounded::<Result<(), String>>(1);

    let msg_tx = recv_tx.clone();
    let on_message = Closure::wrap(Box::new(move |e: MessageEvent| {
        if let Ok(abuf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
            let array = js_sys::Uint8Array::new(&abuf);
            let _ = msg_tx.send(array.to_vec());
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

    let err_tx = open_tx.clone();
    let on_error = Closure::wrap(Box::new(move |e: ErrorEvent| {
        let msg = format!("WebSocket error: {}", e.message());
        tracing::error!("{}", msg);
        let _ = err_tx.send(Err(msg));
    }) as Box<dyn FnMut(ErrorEvent)>);
    ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    // Drop recv_tx on close so that read() returns an error promptly,
    // which triggers the transport layer's reconnection logic.
    let on_close = Closure::once(move |_: JsValue| {
        drop(recv_tx);
        tracing::warn!("WebSocket connection closed by remote");
    });
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget(); // one-shot, cleaned up when WS is GC'd

    let on_open = Closure::once(move |_: JsValue| {
        let _ = open_tx.send(Ok(()));
    });
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    // on_open is a one-shot — forget it since it's cleared after connection opens
    on_open.forget();

    // Channel-based write path: write requests are sent through write_tx and
    // dequeued by a spawn_local loop on this worker (which owns the WebSocket).
    // This ensures ws.send() is always called from the creating worker's JS context,
    // which is required for thread safety with SharedArrayBuffer workers.
    let (write_tx, write_rx) = flume::unbounded::<WriteCmd>();
    let ws_for_write = SendWrapper(ws.clone());
    wasm_bindgen_futures::spawn_local(async move {
        // Senders are on compute workers — recv_async_anywhere self-repolls
        // via setTimeout on this (Acceptor) thread instead of relying on
        // cross-thread microtask wakes.
        while let Ok(cmd) = zenoh_runtime::recv_async_anywhere(&write_rx).await {
            match cmd {
                WriteCmd::Send(data) => {
                    // With +atomics the WASM linear memory is a SharedArrayBuffer,
                    // and WebSocket.send() rejects SAB-backed views with a TypeError.
                    // Copy into a fresh (non-shared) buffer before sending.
                    let array = js_sys::Uint8Array::new_with_length(data.len() as u32);
                    array.copy_from(&data);
                    if let Err(e) = ws_for_write.send_with_array_buffer(&array.buffer()) {
                        tracing::error!("WebSocket send failed: {:?}", e);
                    }
                }
                WriteCmd::Close => {
                    ws_for_write.set_onmessage(None);
                    ws_for_write.set_onerror(None);
                    ws_for_write.set_onclose(None);
                    let _ = ws_for_write.close();
                    break;
                }
            }
        }
    });

    Ok(WsSetup {
        write_tx,
        recv_rx,
        open_rx,
        msg_closures: SendWrapper(vec![on_message]),
        err_closures: SendWrapper(vec![on_error]),
    })
}

impl LinkUnicastWs {
    async fn new(url: &str) -> ZResult<Self> {
        let url_owned = url.to_string();
        let src_locator = Locator::new(WS_LOCATOR_PREFIX, "wasm-client", "").unwrap();
        let dst_locator = Locator::new(WS_LOCATOR_PREFIX, url, "").unwrap();

        // Dispatch WebSocket creation to the Acceptor worker (dedicated I/O worker).
        // This ensures all JS WebSocket objects and their callbacks live on a worker
        // that never calls block_in_place, preventing event loop deadlocks.
        // The result channels (write_tx, recv_rx) cross back via shared memory.
        let (result_tx, result_rx) = flume::bounded::<
            Result<(flume::Sender<WriteCmd>, flume::Receiver<Vec<u8>>), String>,
        >(1);

        // Use a close_rx channel to keep the Acceptor task (and its closures)
        // alive until the link is closed.
        let (close_tx, close_rx) = flume::bounded::<()>(1);

        zenoh_runtime::ZRuntime::Acceptor.spawn(async move {
            let setup = match setup_ws(&url_owned) {
                Ok(s) => s,
                Err(e) => {
                    let _ = result_tx.send(Err(format!("WebSocket setup failed: {e}")));
                    return;
                }
            };

            // Wait for connection to open (on the Acceptor worker's event loop)
            match zenoh_runtime::recv_async_anywhere(&setup.open_rx).await {
                Ok(Ok(())) => {
                    let _ = result_tx.send(Ok((setup.write_tx, setup.recv_rx)));
                }
                Ok(Err(e)) => {
                    let _ = result_tx.send(Err(format!("WebSocket connection failed: {e}")));
                    return;
                }
                Err(_) => {
                    let _ =
                        result_tx.send(Err("WebSocket open channel closed unexpectedly".into()));
                    return;
                }
            }

            // Keep closures alive until the link is closed.
            // The close_rx channel blocks this task until close_tx is dropped.
            let _closures = setup.msg_closures;
            let _err_closures = setup.err_closures;
            // close_tx is dropped from another worker — needs the repoll-based recv.
            let _ = zenoh_runtime::recv_async_anywhere(&close_rx).await;
        });

        // Wait for the Acceptor worker to establish the connection.
        // recv_async_anywhere handles the cross-worker wake for every caller
        // thread (compute worker executor waker, or setTimeout repoll on JS threads).
        let (write_tx, recv_rx) = zenoh_runtime::recv_async_anywhere(&result_rx)
            .await
            .map_err(|e| zerror!("I/O worker channel closed: {}", e))?
            .map_err(|e| zerror!("{}", e))?;

        Ok(Self {
            write_tx,
            recv_rx,
            src_locator,
            dst_locator,
            leftovers: tokio::sync::Mutex::new(None),
            _io_close_tx: close_tx,
        })
    }
}

#[async_trait]
impl LinkUnicastTrait for LinkUnicastWs {
    async fn close(&self) -> ZResult<()> {
        tracing::trace!("Closing WebSocket link: {}", self);
        // Send close command through the write channel — the write loop on the
        // owning worker will clear handlers and close the WebSocket.
        let _ = self.write_tx.send(WriteCmd::Close);
        Ok(())
    }

    async fn write(&self, buffer: &[u8], _priority: Option<Priority>) -> ZResult<usize> {
        // Send through the channel — the write loop on the WebSocket-owning
        // worker will call ws.send_with_u8_array() from the correct JS context.
        self.write_tx
            .send(WriteCmd::Send(buffer.to_vec()))
            .map_err(|e| zerror!("Write error on WebSocket link {}: {}", self, e))?;
        Ok(buffer.len())
    }

    async fn write_all(&self, buffer: &[u8], priority: Option<Priority>) -> ZResult<()> {
        self.write(buffer, priority).await?;
        Ok(())
    }

    async fn read(&self, buffer: &mut [u8], _priority: Option<Priority>) -> ZResult<usize> {
        let mut leftovers_guard = self.leftovers.lock().await;
        // tokio::sync::MutexGuard is Send, so this works with async_trait

        let (slice, start, len) = match leftovers_guard.take() {
            Some(tuple) => tuple,
            None => {
                // Sender is the Acceptor's onmessage callback (another thread).
                let data = zenoh_runtime::recv_async_anywhere(&self.recv_rx)
                    .await
                    .map_err(|e| zerror!("Read error on WebSocket link {}: {}", self, e))?;
                let len = data.len();
                (data, 0usize, len)
            }
        };

        let len_min = (len - start).min(buffer.len());
        let end = start + len_min;
        buffer[0..len_min].copy_from_slice(&slice[start..end]);
        if end < len {
            *leftovers_guard = Some((slice, end, len));
        } else {
            *leftovers_guard = None;
        }
        Ok(len_min)
    }

    async fn read_exact(&self, buffer: &mut [u8], priority: Option<Priority>) -> ZResult<()> {
        let mut read: usize = 0;
        while read < buffer.len() {
            let n = self.read(&mut buffer[read..], priority).await?;
            read += n;
        }
        Ok(())
    }

    #[inline(always)]
    fn get_src(&self) -> &Locator {
        &self.src_locator
    }

    #[inline(always)]
    fn get_dst(&self) -> &Locator {
        &self.dst_locator
    }

    #[inline(always)]
    fn get_mtu(&self) -> BatchSize {
        *WS_DEFAULT_MTU
    }

    #[inline(always)]
    fn get_interface_names(&self) -> Vec<String> {
        vec![]
    }

    #[inline(always)]
    fn is_reliable(&self) -> bool {
        super::IS_RELIABLE
    }

    #[inline(always)]
    fn is_streamed(&self) -> bool {
        false
    }

    #[inline(always)]
    fn get_auth_id(&self) -> &LinkAuthId {
        &LinkAuthId::Ws
    }
}

impl fmt::Display for LinkUnicastWs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wasm-client => {}", self.dst_locator)
    }
}

impl fmt::Debug for LinkUnicastWs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsWasm")
            .field("dst", &self.dst_locator)
            .finish()
    }
}

/*************************************/
/*          LINK MANAGER             */
/*************************************/

pub struct LinkManagerUnicastWs {
    _manager: NewLinkChannelSender,
}

// SAFETY: LinkManagerUnicastWs only holds a channel sender (Send-safe).
// The actual JS objects are accessed only from the creating worker.
unsafe impl Send for LinkManagerUnicastWs {}
unsafe impl Sync for LinkManagerUnicastWs {}

impl LinkManagerUnicastWs {
    pub fn new(manager: NewLinkChannelSender) -> Self {
        Self { _manager: manager }
    }
}

#[async_trait]
impl LinkManagerUnicastTrait for LinkManagerUnicastWs {
    async fn new_link(&self, endpoint: EndPoint) -> ZResult<LinkUnicast> {
        let address = endpoint.address();
        // Support both ws/ and wss/ endpoints.
        // On WASM, the browser's WebSocket handles TLS transparently for wss:// URLs.
        let proto = endpoint.protocol();
        let scheme = if proto.as_str().ends_with("ss") || proto.as_str().ends_with("tls") {
            "wss"
        } else {
            "ws"
        };
        let url = format!("{}://{}", scheme, address);
        tracing::debug!("Opening WASM WebSocket connection to {}", url);
        let link = Arc::new(LinkUnicastWs::new(&url).await?);
        Ok(LinkUnicast(zenoh_link_commons::NewLink::Single(link)))
    }

    async fn new_listener(&self, _endpoint: EndPoint) -> ZResult<Locator> {
        bail!("WebSocket listeners are not supported on WASM")
    }

    async fn del_listener(&self, _endpoint: &EndPoint) -> ZResult<()> {
        bail!("WebSocket listeners are not supported on WASM")
    }

    async fn get_listeners(&self) -> Vec<EndPoint> {
        vec![]
    }

    async fn get_locators(&self) -> Vec<Locator> {
        vec![]
    }

    async fn get_locators_noloopback(&self) -> Vec<Locator> {
        vec![]
    }
}
