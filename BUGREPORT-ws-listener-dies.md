# zenohd WebSocket listener permanently dies on a single malformed handshake

## Summary

A single TCP connection to a `ws/` listener that fails the WebSocket
handshake (any non-WebSocket bytes, or a client that connects and closes)
**permanently terminates the entire accept loop for that listener**. From
that point on zenohd accepts no further WebSocket connections on that
endpoint until the process is restarted. Other transports (e.g. `tcp/`) are
unaffected.

This is a denial-of-service: one stray connection — a health-check probe, a
port scanner, a browser that gives up mid-upgrade, a `curl http://host:7448`
— kills browser/WASM connectivity for everyone.

## Affected version

- Observed on `eclipse/zenoh:1.8.0` (Docker image) and against a local
  build of zenoh 1.9.0.
- The relevant code (`accept_task` in the ws link) is **stock upstream** —
  it is unchanged from upstream zenoh in our fork; our fork only renamed
  `unicast.rs` → `unicast_native.rs`. So this reproduces on unmodified
  upstream zenoh with the `transport_ws` feature.

## Reproduction

Start a router with a WebSocket listener:

```
zenohd --no-multicast-scouting --listen tcp/0.0.0.0:7447 --listen ws/0.0.0.0:7448
```

Confirm WebSocket connectivity works (any WS client handshakes fine), then
send one malformed connection to the ws port:

```js
// node
const net = require('net');
const s = net.connect(7448, '127.0.0.1', () => {
  s.write('THIS IS NOT A WEBSOCKET HANDSHAKE\r\n\r\n');
  setTimeout(() => s.destroy(), 500);
});
```

(Equivalently: `curl http://127.0.0.1:7448/`, or any client that opens a TCP
connection and closes it without completing the WS upgrade.)

After this, **every** subsequent WebSocket handshake to `ws/…:7448` fails,
permanently. `tcp/…:7447` keeps working.

### Confirmation that the listener is gone (not just failing)

Inside the container after triggering the bug, only the TCP listener remains:

```
$ netstat -tln    # inside container
tcp   0   0 0.0.0.0:7447   0.0.0.0:*   LISTEN
# 7448 is absent — the ws accept task has exited
```

Note: when run under Docker port-forwarding, the *host* port 7448 still shows
`LISTEN` (that is `docker-proxy`, not zenohd) and accepts-then-immediately-
closes each connection, because docker-proxy can no longer reach the dead
in-container listener. That masks the failure as a generic "connection reset /
socket hang up" on the client side, which is how we originally saw it
(Firefox: "can't establish a connection to the server at ws://…").

## Root cause

`io/zenoh-links/zenoh-link-ws/src/unicast.rs` (upstream) /
`unicast_native.rs` (our fork), function `accept_task`:

The accept loop performs the WebSocket handshake **inline**, and propagates
its error with `?`:

```rust
loop {
    let (stream, dst_addr) = tokio::select! {
        res = accept(&socket) => { /* on TCP accept error: throttle + continue */ }
        _ = token.cancelled() => break,
    };
    ...
    let stream = accept_async(MaybeTlsStream::Plain(stream))
        .await
        .map_err(|e| { ... e })?;   // <-- FATAL: kills the whole accept loop
    ...
}
```

When `accept_async` (the tungstenite handshake) returns `Err` for a bad
client, the `?` propagates out of `accept_task`. The task returns, its
`socket: TcpListener` is dropped (listener closes), and the caller removes it
from the listeners map:

```rust
let res = accept_task(socket, token, manager).await;
zasyncwrite!(listeners).remove(&addr);   // listener gone, never respawned
res
```

There is no respawn/retry, so the endpoint is dead for the life of the
process.

Notably, the TCP-accept branch a few lines above already does the *right*
thing — it logs, throttles, and `continue`s instead of dying. The handshake
path should behave the same way.

## Two distinct defects

1. **Fatal error propagation (the DoS).** A per-connection handshake failure
   must not terminate the listener. `accept_async` errors should be logged
   and the connection dropped, then `continue` the loop — mirroring the
   existing TCP-accept error handling.

2. **Head-of-line blocking (latent, even after fixing #1).** The handshake is
   `await`ed inline in the accept loop, so a single slow or stalled client
   (connects, never sends the HTTP upgrade) blocks *all* other incoming
   connections for the duration of its handshake/timeout. The handshake
   should be moved into its own spawned task so the accept loop returns
   immediately to `accept()`.

## Suggested fix (sketch)

```rust
res = accept(&socket) => match res {
    Ok((stream, dst_addr)) => {
        let manager = manager.clone();
        // don't block the accept loop; don't let one bad client kill it
        zenoh_runtime::ZRuntime::Acceptor.spawn(async move {
            let src_addr = match stream.local_addr() { Ok(sa) => sa, Err(_) => return };
            let ws = match accept_async(MaybeTlsStream::Plain(stream)).await {
                Ok(ws) => ws,
                Err(e) => { tracing::debug!("WS handshake failed from {dst_addr}: {e}"); return; }
            };
            let link: Arc<dyn LinkUnicastTrait> =
                Arc::new(LinkUnicastWs::new(ws, src_addr, dst_addr));
            if let Err(e) = manager.send_async(LinkUnicast::from(link)).await {
                tracing::error!("{}-{}: {}", file!(), line!(), e);
            }
        });
    }
    Err(e) => { tracing::warn!("{e}"); tokio::time::sleep(...).await; }
},
```

(A handshake timeout on the spawned task is also advisable so a client that
connects and never speaks can't leak accept tasks.)

## Impact on this project

For the browser/WASM demos this is the difference between "works" and
"mysteriously stops accepting browsers after some time." Our router had been
up ~25h and had received at least one malformed probe, leaving the ws
listener dead while ROS 2 traffic on `tcp/7447` kept flowing — which is
exactly why it looked like a browser-side/mixed-content problem at first.

Mitigation until upstream is fixed: run the ws listener behind a reverse
proxy that only forwards completed WebSocket upgrades, or supervise/restart
zenohd. Neither is a real fix.
