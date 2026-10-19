//! `/api/v1/ws`: live `ServerEvent`s, filtered per connection
//! (docs/SCHEDULER.md "WS fan-out").
//!
//! | Event | Delivered to |
//! |---|---|
//! | `TaskUpserted`, `TaskRemoved`, `RunUpserted` | everyone (cards are team-visible; content fields stripped for non-owners) |
//! | `Log` | the task/thread owner, and only for tasks/threads the socket follows (`FollowTask` / `FollowThread`) |
//! | `PlanUpserted`, `ThreadUpserted`, `Message`, `PlannerStatus` | the owner |
//! | `Notification` | its user |
//! | `MemoryUpserted` | users the item is `visible_to` |
//! | `Machine` | everyone |
//! | `Toast` | only toasts generated for this connection (e.g. the `"resync"` hint) |
//!
//! When the bus reports lag, the socket gets `Toast { message: "resync", ok:
//! true }`: clients treat it as "refetch over HTTP". The server pings every
//! 30 s, drops silent peers after 90 s and re-validates the session every
//! minute (a disabled user's sockets close).

use crate::error::AppError;
use crate::session::{authenticate, token_from_headers, token_from_query};
use crate::state::AppState;
use crate::views;
use axum::extract::ws::{CloseFrame, Message as WsMessage, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use kiln_events::{Event, Recv};
use kiln_types::event::{ClientMessage, ServerEvent};
use kiln_types::{TaskId, ThreadId, User, UserId};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// The resync hint sent after the bus dropped events for this socket.
pub const RESYNC: &str = "resync";
const PING_EVERY: Duration = Duration::from_secs(30);
const IDLE_LIMIT: Duration = Duration::from_secs(90);
const REAUTH_EVERY: Duration = Duration::from_secs(60);
const MAX_FOLLOWS: usize = 256;

pub async fn handler(State(st): State<AppState>, req: Request) -> Response {
    let (mut parts, _body) = req.into_parts();
    let token = token_from_headers(&parts.headers).or_else(|| token_from_query(parts.uri.query()));
    let me = match authenticate(&st, token).await {
        Ok(me) => me,
        Err(e) => return e.into_response(),
    };
    let ws =
        match <WebSocketUpgrade as axum::extract::FromRequestParts<AppState>>::from_request_parts(
            &mut parts, &st,
        )
        .await
        {
            Ok(ws) => ws,
            Err(_) => return AppError::invalid("expected a WebSocket upgrade").into_response(),
        };
    ws.max_message_size(64 * 1024)
        .on_upgrade(move |socket| async move { Conn::new(st, me.user, me.token).run(socket).await })
}

/// Per-connection filter state.
pub struct Filter {
    pub viewer: User,
    pub tasks: HashSet<TaskId>,
    pub threads: HashSet<ThreadId>,
    thread_owner: HashMap<ThreadId, UserId>,
}

impl Filter {
    pub fn new(viewer: User) -> Self {
        Self {
            viewer,
            tasks: HashSet::new(),
            threads: HashSet::new(),
            thread_owner: HashMap::new(),
        }
    }

    async fn owns_thread(&mut self, st: &AppState, id: ThreadId) -> bool {
        if let Some(o) = self.thread_owner.get(&id) {
            return *o == self.viewer.id;
        }
        match st.store.get_thread(id).await {
            Ok(t) => {
                if self.thread_owner.len() > 4096 {
                    self.thread_owner.clear();
                }
                self.thread_owner.insert(id, t.owner_id);
                t.owner_id == self.viewer.id
            }
            Err(_) => false,
        }
    }

    /// What this viewer may receive of `ev` (possibly redacted), if anything.
    pub async fn apply(&mut self, st: &AppState, ev: &ServerEvent) -> Option<ServerEvent> {
        let me = self.viewer.id;
        match ev {
            ServerEvent::TaskUpserted { task } => Some(ServerEvent::TaskUpserted {
                task: views::card_view(task, &self.viewer),
            }),
            ServerEvent::TaskRemoved { .. } | ServerEvent::Machine { .. } => Some(ev.clone()),
            ServerEvent::RunUpserted { run } => Some(ServerEvent::RunUpserted {
                run: views::run_view(run, &self.viewer),
            }),
            ServerEvent::Log {
                task_id, thread_id, ..
            } => {
                // Followed sets only ever contain the viewer's own tasks/threads.
                let ok = match (task_id, thread_id) {
                    (Some(t), _) => self.tasks.contains(t),
                    (None, Some(th)) => self.threads.contains(th),
                    (None, None) => false,
                };
                ok.then(|| ev.clone())
            }
            ServerEvent::PlanUpserted { plan } => (plan.owner_id == me).then(|| ev.clone()),
            ServerEvent::ThreadUpserted { thread } => {
                self.thread_owner.insert(thread.id, thread.owner_id);
                (thread.owner_id == me).then(|| ev.clone())
            }
            ServerEvent::Message { message } => self
                .owns_thread(st, message.thread_id)
                .await
                .then(|| ev.clone()),
            ServerEvent::PlannerStatus { thread_id, .. } => {
                self.owns_thread(st, *thread_id).await.then(|| ev.clone())
            }
            ServerEvent::Notification { notification } => {
                (notification.user_id == me).then(|| ev.clone())
            }
            ServerEvent::MemoryUpserted { item } => item.visible_to(me).then(|| ev.clone()),
            // Bus toasts carry no recipient; never broadcast them.
            ServerEvent::Toast { .. } | ServerEvent::Hello { .. } => None,
        }
    }

    /// Handle a client message; returns an optional reply.
    pub async fn on_client(&mut self, st: &AppState, m: ClientMessage) -> Option<ServerEvent> {
        match m {
            ClientMessage::FollowTask { task_id } => match st.store.get_task(task_id).await {
                Ok(t) if t.owner_id == self.viewer.id => {
                    if self.tasks.len() < MAX_FOLLOWS {
                        self.tasks.insert(task_id);
                    }
                    None
                }
                _ => Some(ServerEvent::Toast {
                    message: "live logs are visible to the task owner only".into(),
                    ok: false,
                }),
            },
            ClientMessage::UnfollowTask { task_id } => {
                self.tasks.remove(&task_id);
                None
            }
            ClientMessage::FollowThread { thread_id } => {
                if self.owns_thread(st, thread_id).await {
                    if self.threads.len() < MAX_FOLLOWS {
                        self.threads.insert(thread_id);
                    }
                    None
                } else {
                    Some(ServerEvent::Toast {
                        message: "thread not found".into(),
                        ok: false,
                    })
                }
            }
            ClientMessage::Ping => None,
        }
    }
}

struct Conn {
    st: AppState,
    token: String,
    filter: Filter,
}

fn encode(ev: &ServerEvent) -> Option<WsMessage> {
    serde_json::to_string(ev)
        .ok()
        .map(|s| WsMessage::Text(Utf8Bytes::from(s)))
}

impl Conn {
    fn new(st: AppState, user: User, token: String) -> Self {
        Self {
            st,
            token,
            filter: Filter::new(user),
        }
    }

    async fn run(mut self, socket: WebSocket) {
        // Subscribe before Hello so nothing published after the handshake is missed.
        let mut sub = self.st.bus.subscribe();
        let (mut tx, mut rx) = socket.split();
        let hello = ServerEvent::Hello {
            version: kiln_types::VERSION.into(),
            user: (&self.filter.viewer).into(),
        };
        if let Some(m) = encode(&hello)
            && tx.send(m).await.is_err()
        {
            return;
        }
        let user_id = self.filter.viewer.id;
        tracing::debug!(user = %user_id, "websocket connected");
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        let mut last_seen = Instant::now();
        let mut last_auth = Instant::now();
        let shutdown = self.st.shutdown.clone();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    let _ = tx.send(WsMessage::Close(Some(CloseFrame { code: 1012, reason: "server restarting".into() }))).await;
                    break;
                }
                r = sub.recv() => match r {
                    Recv::Event(ev) => {
                        let Event::Server(se) = &*ev else {
                            if matches!(&*ev, Event::Shutdown) {
                                let _ = tx.send(WsMessage::Close(Some(CloseFrame { code: 1012, reason: "server restarting".into() }))).await;
                                break;
                            }
                            continue;
                        };
                        if let Some(out) = self.filter.apply(&self.st, se).await
                            && let Some(m) = encode(&out)
                            && tx.send(m).await.is_err()
                        {
                            break;
                        }
                    }
                    Recv::Lagged(_) => {
                        if let Some(m) = encode(&ServerEvent::Toast { message: RESYNC.into(), ok: true })
                            && tx.send(m).await.is_err()
                        {
                            break;
                        }
                    }
                    Recv::Closed => break,
                },
                msg = rx.next() => match msg {
                    Some(Ok(WsMessage::Text(t))) => {
                        last_seen = Instant::now();
                        match serde_json::from_str::<ClientMessage>(t.as_str()) {
                            Ok(cm) => {
                                if let Some(reply) = self.filter.on_client(&self.st, cm).await
                                    && let Some(m) = encode(&reply)
                                    && tx.send(m).await.is_err()
                                {
                                    break;
                                }
                            }
                            Err(_) => tracing::debug!(user = %user_id, "ignoring malformed client message"),
                        }
                    }
                    Some(Ok(WsMessage::Ping(p))) => {
                        last_seen = Instant::now();
                        if tx.send(WsMessage::Pong(p)).await.is_err() { break; }
                    }
                    Some(Ok(WsMessage::Pong(_))) | Some(Ok(WsMessage::Binary(_))) => last_seen = Instant::now(),
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                },
                _ = ping.tick() => {
                    if last_seen.elapsed() > IDLE_LIMIT {
                        tracing::debug!(user = %user_id, "websocket idle; closing");
                        break;
                    }
                    if last_auth.elapsed() >= REAUTH_EVERY {
                        last_auth = Instant::now();
                        match self.st.auth.authenticate(&self.token).await {
                            Ok(u) => self.filter.viewer = u,
                            Err(_) => {
                                let _ = tx.send(WsMessage::Close(Some(CloseFrame { code: 4401, reason: "session ended".into() }))).await;
                                break;
                            }
                        }
                    }
                    if tx.send(WsMessage::Ping(Default::default())).await.is_err() { break; }
                }
            }
        }
        tracing::debug!(user = %user_id, "websocket closed");
    }
}
