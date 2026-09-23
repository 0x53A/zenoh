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
use futures::FutureExt;
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use web_sys::{BinaryType, MessageEvent, WebSocket};
use zenoh_link_commons::{
    LinkAuthId, LinkManagerUnicastTrait, LinkUnicast, LinkUnicastTrait, NewLinkChannelSender,
};
use zenoh_protocol::{
    core::{EndPoint, Locator, Priority},
    transport::BatchSize,
};
use zenoh_result::{bail, zerror, ZResult};

use super::{WS_DEFAULT_MTU, WS_LOCATOR_PREFIX};

/// Writes are acknowledged by the JS owner, preserving transport backpressure.
enum WriteCmd {
    Send(Vec<u8>, flume::Sender<Result<(), String>>),
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

/// This guard and all JS objects stay on the owning spawn_local task.
/// Detach callbacks before freeing them, including on failed/cancelled opens.
struct SocketOwner {
    ws: WebSocket,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _open: Closure<dyn FnMut(JsValue)>,
    _error: Closure<dyn FnMut(JsValue)>,
    _close: Closure<dyn FnMut(JsValue)>,
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        self.ws.set_onmessage(None);
        self.ws.set_onopen(None);
        self.ws.set_onerror(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();
    }
}

type Connected = (flume::Sender<WriteCmd>, flume::Receiver<Vec<u8>>);

async fn run_socket(
    url: String,
    result_tx: flume::Sender<Result<Connected, String>>,
    close_rx: flume::Receiver<()>,
) {
    let ws = match WebSocket::new(&url) {
        Ok(ws) => ws,
        Err(e) => {
            let _ = result_tx.send(Err(format!("WebSocket creation failed: {e:?}")));
            return;
        }
    };
    ws.set_binary_type(BinaryType::Arraybuffer);
    // Browsers do not expose receive-side WebSocket backpressure. Bound the
    // copied Rust backlog; an overloaded link must fail rather than grow forever.
    let (recv_tx, recv_rx) = flume::bounded(128);
    let (event_tx, event_rx) = flume::unbounded::<Result<(), String>>();
    let message_error_tx = event_tx.clone();
    let on_message = Closure::wrap(Box::new(move |e: MessageEvent| {
        if let Ok(abuf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
            if abuf.byte_length() == 0 {
                return;
            }
            if abuf.byte_length() > super::WS_MAX_MTU as u32 {
                let _ = message_error_tx.send(Err("WebSocket frame exceeds link MTU".into()));
                return;
            }
            match recv_tx.try_send(js_sys::Uint8Array::new(&abuf).to_vec()) {
                Ok(()) => {},
                Err(flume::TrySendError::Full(_)) => {
                    let _ = message_error_tx.send(Err("WebSocket receive backlog exceeded".into()));
                },
                Err(flume::TrySendError::Disconnected(_)) => {
                    let _ = message_error_tx.send(Err("WebSocket receive stream closed".into()));
                },
            }
        } else {
            // Zenoh WebSocket links carry binary batches. Match the native
            // transport's rejection instead of leaving readers waiting forever.
            let _ = message_error_tx.send(Err("WebSocket received a non-binary frame".into()));
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    let tx = event_tx.clone();
    let on_open = Closure::wrap(Box::new(move |_: JsValue| {
        let _ = tx.send(Ok(()));
    }) as Box<dyn FnMut(JsValue)>);
    let tx = event_tx.clone();
    // Browser WebSocket errors are Event, not ErrorEvent.
    let on_error = Closure::wrap(Box::new(move |_: JsValue| {
        let _ = tx.send(Err("WebSocket error".into()));
    }) as Box<dyn FnMut(JsValue)>);
    let on_close = Closure::wrap(Box::new(move |_: JsValue| {
        let _ = event_tx.send(Err("WebSocket closed".into()));
    }) as Box<dyn FnMut(JsValue)>);
    ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    ws.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    let owner = SocketOwner {
        ws,
        _message: on_message,
        _open: on_open,
        _error: on_error,
        _close: on_close,
    };
    let opened = futures::select_biased! {
        _ = zenoh_runtime::recv_async_anywhere(&close_rx).fuse() => return,
        event = event_rx.recv_async().fuse() => event,
    };
    match opened {
        Ok(Ok(())) => {}
        event => {
            let _ = result_tx.send(Err(format!("WebSocket failed to open: {event:?}")));
            return;
        }
    }
    let (write_tx, write_rx) = flume::unbounded();
    if result_tx.send(Ok((write_tx, recv_rx))).is_err() {
        return;
    }
    loop {
        let cmd = futures::select_biased! {
            _ = zenoh_runtime::recv_async_anywhere(&close_rx).fuse() => break,
            event = event_rx.recv_async().fuse() => {
                tracing::debug!("WebSocket owner stopped: {event:?}");
                break;
            },
            cmd = zenoh_runtime::recv_async_anywhere(&write_rx).fuse() => cmd,
        };
        let Ok(WriteCmd::Send(data, ack)) = cmd else {
            break;
        };
        // Bound the browser send backlog, remaining responsive to shutdown.
        while owner.ws.buffered_amount() > 1024 * 1024 {
            futures::select_biased! {
                _ = zenoh_runtime::recv_async_anywhere(&close_rx).fuse() => return,
                _ = event_rx.recv_async().fuse() => return,
                _ = zenoh_runtime::wasm_yield::sleep_ms(4).fuse() => {},
            }
        }
        if owner.ws.ready_state() != WebSocket::OPEN {
            let _ = ack.send(Err("WebSocket is not open".into()));
            break;
        }
        // WebSocket.send rejects SharedArrayBuffer-backed views.
        let array = js_sys::Uint8Array::new_with_length(data.len() as u32);
        array.copy_from(&data);
        let result = owner
            .ws
            .send_with_array_buffer(&array.buffer())
            .map_err(|e| format!("WebSocket send failed: {e:?}"));
        let failed = result.is_err();
        let _ = ack.send(result);
        if failed {
            break;
        }
    }
}

impl LinkUnicastWs {
    async fn new(url: &str, dst_locator: Locator) -> ZResult<Self> {
        let url_owned = url.to_string();
        let src_locator = Locator::new(WS_LOCATOR_PREFIX, "wasm-client", "").unwrap();

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
            wasm_bindgen_futures::spawn_local(run_socket(url_owned, result_tx, close_rx));
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
        // Wake the owner even if writes are blocked behind browser backpressure.
        let _ = self._io_close_tx.try_send(());
        Ok(())
    }

    async fn write(&self, buffer: &[u8], _priority: Option<Priority>) -> ZResult<usize> {
        // Send through the channel — the write loop on the WebSocket-owning
        // worker will call ws.send_with_u8_array() from the correct JS context.
        let started = zenoh_runtime::wasm_yield::Instant::now();
        let (ack_tx, ack_rx) = flume::bounded(1);
        self.write_tx
            .send(WriteCmd::Send(buffer.to_vec(), ack_tx))
            .map_err(|e| zerror!("Write error on WebSocket link {}: {}", self, e))?;
        zenoh_runtime::recv_async_anywhere(&ack_rx)
            .await
            .map_err(|e| zerror!("WebSocket writer stopped: {e}"))?
            .map_err(|e| zerror!("{e}"))?;
        if started.elapsed() > std::time::Duration::from_millis(250) {
            tracing::warn!("WebSocket write acknowledgement delayed {:?} on {}", started.elapsed(), self);
        }
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
        let link = Arc::new(LinkUnicastWs::new(&url, endpoint.to_locator()).await?);
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
