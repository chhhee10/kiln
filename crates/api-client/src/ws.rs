//! Live event stream over `/api/v1/ws`.
//!
//! Native: [`connect`] returns a [`Connection`] whose `events` receiver yields
//! [`ServerEvent`]s and whose `send` pushes [`ClientMessage`]s. Reconnection
//! is the caller's job (see [`connect_forever`] for the standard loop).
//!
//! Wasm: [`connect`] takes an `on_event` callback and returns a [`Connection`]
//! that closes the socket on drop.

use kiln_types::event::{ClientMessage, ServerEvent};

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
#[cfg(target_arch = "wasm32")]
pub use wasm::*;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use crate::{Client, ClientError, Result};
    use futures::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    pub(crate) fn same_origin_impl() -> String {
        String::new()
    }

    pub struct Connection {
        pub events: mpsc::Receiver<ServerEvent>,
        pub send: mpsc::Sender<ClientMessage>,
    }

    /// Open one WebSocket connection. The receiver closes when the socket does.
    pub async fn connect(client: &Client) -> Result<Connection> {
        let url = client.ws_url();
        let (stream, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|e| ClientError::Http(format!("websocket: {e}")))?;
        let (mut sink, mut source) = stream.split();
        let (ev_tx, ev_rx) = mpsc::channel(1024);
        let (out_tx, mut out_rx) = mpsc::channel::<ClientMessage>(64);

        tokio::spawn(async move {
            let mut ping = tokio::time::interval(Duration::from_secs(25));
            loop {
                tokio::select! {
                    msg = source.next() => match msg {
                        Some(Ok(WsMessage::Text(t))) => {
                            if let Some(ev) = crate::decode_server_event(t.as_str())
                                && ev_tx.send(ev).await.is_err()
                            {
                                break;
                            }
                        }
                        Some(Ok(WsMessage::Ping(p))) => { let _ = sink.send(WsMessage::Pong(p)).await; }
                        Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                        _ => {}
                    },
                    out = out_rx.recv() => match out {
                        Some(m) => {
                            if sink.send(WsMessage::Text(crate::encode_client_message(&m).into())).await.is_err() { break; }
                        }
                        None => break,
                    },
                    _ = ping.tick() => {
                        if sink.send(WsMessage::Text(crate::encode_client_message(&ClientMessage::Ping).into())).await.is_err() { break; }
                    }
                }
            }
            tracing::debug!("websocket closed");
        });

        Ok(Connection { events: ev_rx, send: out_tx })
    }

    /// Keep a connection alive forever, forwarding events to `tx`. Reconnects
    /// with capped exponential backoff. `on_reconnect` is invoked after each
    /// successful (re)connect so the caller can resync state from HTTP.
    /// Returns when `tx` is closed.
    pub async fn connect_forever(
        client: Client,
        tx: mpsc::Sender<ServerEvent>,
        follow: mpsc::Receiver<ClientMessage>,
        mut on_reconnect: impl FnMut() + Send + 'static,
    ) {
        let mut follow = follow;
        let mut followed: Vec<ClientMessage> = Vec::new();
        let mut backoff = Duration::from_millis(500);
        loop {
            match connect(&client).await {
                Ok(mut conn) => {
                    backoff = Duration::from_millis(500);
                    on_reconnect();
                    for m in &followed {
                        let _ = conn.send.send(m.clone()).await;
                    }
                    loop {
                        tokio::select! {
                            ev = conn.events.recv() => match ev {
                                Some(ev) => if tx.send(ev).await.is_err() { return; },
                                None => break,
                            },
                            m = follow.recv() => match m {
                                Some(m) => {
                                    remember(&mut followed, &m);
                                    let _ = conn.send.send(m).await;
                                }
                                None => return,
                            }
                        }
                    }
                }
                Err(e) => tracing::debug!("websocket connect failed: {e}"),
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(15));
        }
    }

    fn remember(list: &mut Vec<ClientMessage>, m: &ClientMessage) {
        match m {
            ClientMessage::UnfollowTask { task_id } => {
                list.retain(|x| !matches!(x, ClientMessage::FollowTask { task_id: t } if t == task_id))
            }
            ClientMessage::FollowTask { .. } | ClientMessage::FollowThread { .. } => {
                if !list.contains(m) {
                    list.push(m.clone())
                }
            }
            ClientMessage::Ping => {}
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use crate::{Client, ClientError, Result};
    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::Closure;
    use web_sys::{CloseEvent, MessageEvent, WebSocket};

    pub(crate) fn same_origin_impl() -> String {
        web_sys::window().and_then(|w| w.location().origin().ok()).unwrap_or_default()
    }

    pub struct Connection {
        socket: WebSocket,
        _on_message: Closure<dyn FnMut(MessageEvent)>,
        _on_close: Closure<dyn FnMut(CloseEvent)>,
    }

    impl Connection {
        pub fn send(&self, m: &ClientMessage) {
            let _ = self.socket.send_with_str(&crate::encode_client_message(m));
        }
        pub fn is_open(&self) -> bool {
            self.socket.ready_state() == WebSocket::OPEN
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            self.socket.set_onmessage(None);
            self.socket.set_onclose(None);
            let _ = self.socket.close();
        }
    }

    /// Open a socket. `on_event` gets each event; `on_close` fires once when
    /// the socket closes (reconnect from there).
    pub fn connect(
        client: &Client,
        mut on_event: impl FnMut(ServerEvent) + 'static,
        mut on_close: impl FnMut() + 'static,
    ) -> Result<Connection> {
        let socket = WebSocket::new(&client.ws_url()).map_err(|e| ClientError::Http(format!("{e:?}")))?;
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Some(text) = e.data().as_string() {
                if let Some(ev) = crate::decode_server_event(&text) {
                    on_event(ev);
                }
            }
        });
        let on_close_cb = Closure::<dyn FnMut(CloseEvent)>::new(move |_e: CloseEvent| on_close());
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(on_close_cb.as_ref().unchecked_ref()));
        Ok(Connection { socket, _on_message: on_message, _on_close: on_close_cb })
    }
}

pub(crate) fn same_origin() -> String {
    same_origin_impl()
}
