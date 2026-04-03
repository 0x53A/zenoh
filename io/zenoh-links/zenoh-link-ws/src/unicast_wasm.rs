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

/// Wrapper to make JsValue-based types Send+Sync on WASM (single-threaded).
struct SendWrapper<T>(T);
// SAFETY: wasm32 is single-threaded
unsafe impl<T> Send for SendWrapper<T> {}
unsafe impl<T> Sync for SendWrapper<T> {}

impl<T> std::ops::Deref for SendWrapper<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

pub struct LinkUnicastWs {
    ws: SendWrapper<WebSocket>,
    recv_rx: flume::Receiver<Vec<u8>>,
    // We hold closures to prevent them from being dropped
    _on_message: SendWrapper<Closure<dyn FnMut(MessageEvent)>>,
    _on_error: SendWrapper<Closure<dyn FnMut(ErrorEvent)>>,
    src_locator: Locator,
    dst_locator: Locator,
    leftovers: tokio::sync::Mutex<Option<(Vec<u8>, usize, usize)>>,
}


/// Result of synchronously setting up a WebSocket connection.
/// All !Send types are already wrapped in SendWrapper.
struct WsSetup {
    ws: SendWrapper<WebSocket>,
    recv_rx: flume::Receiver<Vec<u8>>,
    open_rx: flume::Receiver<Result<(), String>>,
    on_message: SendWrapper<Closure<dyn FnMut(MessageEvent)>>,
    on_error: SendWrapper<Closure<dyn FnMut(ErrorEvent)>>,
}

/// Create and configure WebSocket synchronously (no .await).
/// Returns only Send-safe types.
fn setup_ws(url: &str) -> ZResult<WsSetup> {
    let ws = WebSocket::new(url)
        .map_err(|e| zerror!("Failed to create WebSocket to {}: {:?}", url, e))?;
    ws.set_binary_type(BinaryType::Arraybuffer);

    let (recv_tx, recv_rx) = flume::unbounded::<Vec<u8>>();
    let (open_tx, open_rx) = flume::bounded::<Result<(), String>>(1);

    let tx = recv_tx.clone();
    let on_message = Closure::wrap(Box::new(move |e: MessageEvent| {
        if let Ok(abuf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
            let array = js_sys::Uint8Array::new(&abuf);
            let _ = tx.send(array.to_vec());
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

    let on_open = Closure::once(move |_: JsValue| {
        let _ = open_tx.send(Ok(()));
    });
    ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    // on_open is a one-shot closure; it will be dropped but that's fine
    // since JS already has the reference.

    Ok(WsSetup {
        ws: SendWrapper(ws),
        recv_rx,
        open_rx,
        on_message: SendWrapper(on_message),
        on_error: SendWrapper(on_error),
    })
}

impl LinkUnicastWs {
    async fn new(url: &str) -> ZResult<Self> {
        // All !Send types are created and wrapped synchronously in setup_ws.
        // Only Send types cross the .await boundary.
        let setup = setup_ws(url)?;

        // Wait for connection to open
        match setup.open_rx.recv_async().await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => bail!("WebSocket connection failed: {}", e),
            Err(_) => bail!("WebSocket open channel closed unexpectedly"),
        }

        // Clear onopen handler (one-shot)
        setup.ws.set_onopen(None);

        let src_locator =
            Locator::new(WS_LOCATOR_PREFIX, "wasm-client", "").unwrap();
        let dst_locator = Locator::new(WS_LOCATOR_PREFIX, url, "").unwrap();

        Ok(Self {
            ws: setup.ws,
            recv_rx: setup.recv_rx,
            _on_message: setup.on_message,
            _on_error: setup.on_error,
            src_locator,
            dst_locator,
            leftovers: tokio::sync::Mutex::new(None),
        })
    }
}

#[async_trait]
impl LinkUnicastTrait for LinkUnicastWs {
    async fn close(&self) -> ZResult<()> {
        tracing::trace!("Closing WebSocket link: {}", self);
        self.ws.close().map_err(|e| {
            zerror!("Failed to close WebSocket link {}: {:?}", self, e).into()
        })
    }

    async fn write(&self, buffer: &[u8], _priority: Option<Priority>) -> ZResult<usize> {
        self.ws
            .send_with_u8_array(buffer)
            .map_err(|e| zerror!("Write error on WebSocket link {}: {:?}", self, e))?;
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
                let data = self
                    .recv_rx
                    .recv_async()
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

// SAFETY: Single-threaded WASM context
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
        let url = format!("ws://{}", address);
        tracing::debug!("Opening WASM WebSocket connection to {}", url);
        let link = Arc::new(LinkUnicastWs::new(&url).await?);
        Ok(LinkUnicast(link))
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
}
