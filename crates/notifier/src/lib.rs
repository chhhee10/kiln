//! Kiln notifications (PRD §5.7, design/docs/android.md "Notifications").
//!
//! [`Notifier::notify`] persists a notification card, publishes
//! `ServerEvent::Notification` on the event bus (in-app delivery on every
//! client) and POSTs a UnifiedPush JSON payload to each of the user's devices
//! that registered a push endpoint, unless the user turned that channel off
//! in `UserSettings.notification_channels` (key = channel snake_case name;
//! a missing key means enabled).
//!
//! [`Notifier::workers_live`] maintains a single ongoing "workers live" card
//! per user, throttled to one update every 10 s.

use async_trait::async_trait;
use futures::future::join_all;
use kiln_events::Bus;
use kiln_store::{NewNotification, Store, StoreError};
use kiln_types::event::ServerEvent;
use kiln_types::{
    DeviceId, Notification, NotificationAction, NotificationChannel, NotificationId,
    NotificationKind, UserId,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Minimum interval between "workers live" updates for one user.
pub const WORKERS_LIVE_INTERVAL: Duration = Duration::from_secs(10);
/// Per-push HTTP timeout.
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(5);
/// Deep link of the workers-live card (Android "Runs" tab).
pub const RUNS_DEEP_LINK: &str = "runs";

/// Notifier errors.
#[derive(Debug, thiserror::Error)]
pub enum NotifierError {
    /// Store failure.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Result alias for notifier operations.
pub type Result<T, E = NotifierError> = std::result::Result<T, E>;

/// JSON body POSTed to a UnifiedPush endpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PushPayload {
    pub id: NotificationId,
    pub kind: NotificationKind,
    pub channel: NotificationChannel,
    pub title: String,
    pub body: String,
    pub deep_link: String,
    pub actions: Vec<NotificationAction>,
    /// 0–100 for ongoing progress notifications (workers live).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    /// Set when the card was acted on; the device should update/dismiss it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
}

impl PushPayload {
    /// Payload for a stored notification.
    pub fn from_notification(n: &Notification, progress: Option<u8>) -> Self {
        Self {
            id: n.id,
            kind: n.kind,
            channel: n.kind.channel(),
            title: n.title.clone(),
            body: n.body.clone(),
            deep_link: n.deep_link.clone(),
            actions: n.actions.clone(),
            progress,
            resolved: n.resolved.clone(),
        }
    }
}

/// Why a push failed.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// The endpoint no longer exists (HTTP 404/410): unregister it.
    #[error("push endpoint is gone")]
    Gone,
    /// The push server answered with another error status.
    #[error("push server returned HTTP {0}")]
    Status(u16),
    /// Network/transport failure.
    #[error("push transport error: {0}")]
    Transport(String),
}

/// Delivers push payloads. A trait so tests can capture pushes.
#[async_trait]
pub trait PushSender: Send + Sync {
    /// POST `payload` as JSON to `endpoint`.
    async fn send(
        &self,
        endpoint: &str,
        payload: &PushPayload,
    ) -> std::result::Result<(), PushError>;
}

/// UnifiedPush/ntfy delivery over HTTP.
pub struct HttpPushSender {
    client: reqwest::Client,
}

impl HttpPushSender {
    /// A sender with a [`PUSH_TIMEOUT`] request timeout.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(PUSH_TIMEOUT)
            .user_agent(concat!("kiln/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for HttpPushSender {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PushSender for HttpPushSender {
    async fn send(
        &self,
        endpoint: &str,
        payload: &PushPayload,
    ) -> std::result::Result<(), PushError> {
        let resp = self
            .client
            .post(endpoint)
            .json(payload)
            .send()
            .await
            .map_err(|e| {
                PushError::Transport(if e.is_timeout() {
                    "timed out".into()
                } else {
                    "request failed".into()
                })
            })?;
        match resp.status().as_u16() {
            200..=299 => Ok(()),
            404 | 410 => Err(PushError::Gone),
            code => Err(PushError::Status(code)),
        }
    }
}

#[derive(Clone, Copy)]
struct LiveState {
    sent_at: Instant,
    live: u32,
}

/// Notification service.
pub struct Notifier {
    store: Store,
    bus: Bus,
    http: Arc<dyn PushSender>,
    live_interval: Duration,
    live: Mutex<HashMap<UserId, LiveState>>,
}

impl Notifier {
    /// A notifier delivering pushes over HTTP.
    pub fn new(store: Store, bus: Bus) -> Self {
        Self::with_sender(store, bus, Arc::new(HttpPushSender::new()))
    }

    /// A notifier with a custom push sender (tests).
    pub fn with_sender(store: Store, bus: Bus, http: Arc<dyn PushSender>) -> Self {
        Self {
            store,
            bus,
            http,
            live_interval: WORKERS_LIVE_INTERVAL,
            live: Mutex::new(HashMap::new()),
        }
    }

    /// Override the workers-live throttle interval (tests).
    pub fn with_live_interval(mut self, interval: Duration) -> Self {
        self.live_interval = interval;
        self
    }

    /// Persist a notification, publish it on the bus and push it to the
    /// user's devices (respecting channel preferences). Push failures are
    /// logged, never returned.
    pub async fn notify(&self, new: NewNotification) -> Result<Notification> {
        let n = self.store.insert_notification(new).await?;
        self.publish(&n);
        self.push(&n, None).await;
        Ok(n)
    }

    /// Update the user's single ongoing "workers live" card: `live` running
    /// workers at `avg_progress`%. Updates arrive at most every 10 s per
    /// user; the transition to zero workers is always delivered (it resolves
    /// the card). Errors are logged.
    pub async fn workers_live(&self, user: UserId, live: u32, avg_progress: u8) {
        if !self.take_live_slot(user, live) {
            return;
        }
        if let Err(e) = self
            .update_live_card(user, live, avg_progress.min(100))
            .await
        {
            tracing::warn!(user = %user, error = %e, "workers-live notification failed");
        }
    }

    /// Record that a notification was acted on, and tell clients/devices.
    pub async fn resolve(&self, id: NotificationId, text: &str) -> Result<()> {
        let n = self.store.resolve_notification(id, text).await?;
        self.publish(&n);
        self.push(&n, None).await;
        Ok(())
    }

    /// Throttle gate. Returns true when this update should go out.
    fn take_live_slot(&self, user: UserId, live: u32) -> bool {
        let now = Instant::now();
        let mut map = match self.live.lock() {
            Ok(m) => m,
            Err(poisoned) => poisoned.into_inner(),
        };
        let allowed = match map.get(&user) {
            None => live > 0,
            Some(prev) => {
                (live == 0 && prev.live > 0)
                    || (live > 0 && now.duration_since(prev.sent_at) >= self.live_interval)
            }
        };
        if allowed {
            map.insert(user, LiveState { sent_at: now, live });
        }
        allowed
    }

    async fn update_live_card(&self, user: UserId, live: u32, avg: u8) -> Result<()> {
        let open = self
            .store
            .find_open_notification(user, NotificationKind::WorkersLive)
            .await?;
        if live == 0 {
            if let Some(n) = open {
                let n = self
                    .store
                    .resolve_notification(n.id, "All workers finished")
                    .await?;
                self.publish(&n);
                self.push(&n, Some(100)).await;
            }
            return Ok(());
        }
        let title = if live == 1 {
            "1 worker running".to_string()
        } else {
            format!("{live} workers running")
        };
        let body = format!("avg {avg}% · tap to open Runs");
        let n = match open {
            Some(n) => {
                self.store
                    .update_notification_content(n.id, &title, &body)
                    .await?
            }
            None => {
                let mut new =
                    NewNotification::new(user, NotificationKind::WorkersLive, title, body);
                new.deep_link = RUNS_DEEP_LINK.into();
                self.store.insert_notification(new).await?
            }
        };
        self.publish(&n);
        self.push(&n, Some(avg)).await;
        Ok(())
    }

    fn publish(&self, n: &Notification) {
        self.bus.server(ServerEvent::Notification {
            notification: n.clone(),
        });
    }

    /// Whether `user` wants pushes on `channel` (missing key = enabled).
    async fn channel_enabled(&self, user: UserId, channel: NotificationChannel) -> bool {
        let key = match serde_json::to_value(channel) {
            Ok(serde_json::Value::String(s)) => s,
            _ => return true,
        };
        match self.store.get_user_settings(user).await {
            Ok(Some(s)) => s.notification_channels.get(&key).copied().unwrap_or(true),
            Ok(None) => true,
            Err(e) => {
                tracing::warn!(user = %user, error = %e, "could not read notification settings; pushing anyway");
                true
            }
        }
    }

    async fn push(&self, n: &Notification, progress: Option<u8>) {
        if !self.channel_enabled(n.user_id, n.kind.channel()).await {
            return;
        }
        let devices = match self.store.list_devices(n.user_id).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(user = %n.user_id, error = %e, "could not list devices for push");
                return;
            }
        };
        let targets: Vec<(DeviceId, String)> = devices
            .into_iter()
            .filter_map(|d| d.push_endpoint.map(|e| (d.id, e)))
            .filter(|(_, e)| e.starts_with("https://") || e.starts_with("http://"))
            .collect();
        if targets.is_empty() {
            return;
        }
        let payload = PushPayload::from_notification(n, progress);
        let sends = targets.iter().map(|(id, endpoint)| {
            let payload = &payload;
            async move {
                let res =
                    match tokio::time::timeout(PUSH_TIMEOUT * 2, self.http.send(endpoint, payload))
                        .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(PushError::Transport("timed out".into())),
                    };
                (*id, res)
            }
        });
        for (device, res) in join_all(sends).await {
            match res {
                Ok(()) => {}
                Err(PushError::Gone) => {
                    tracing::info!(device = %device, "push endpoint gone; unregistering it");
                    if let Err(e) = self.store.set_push_endpoint(device, None).await {
                        tracing::warn!(device = %device, error = %e, "could not clear push endpoint");
                    }
                }
                Err(e) => tracing::warn!(device = %device, error = %e, "push delivery failed"),
            }
        }
    }
}
