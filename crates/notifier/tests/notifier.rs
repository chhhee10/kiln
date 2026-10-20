use async_trait::async_trait;
use kiln_events::{Bus, Event};
use kiln_notifier::*;
use kiln_store::{NewDevice, NewNotification, NewUser, Store};
use kiln_types::event::ServerEvent;
use kiln_types::memory::Visibility;
use kiln_types::{
    ClientKind, NotificationAction, NotificationChannel, NotificationKind, User, UserLimits,
    UserRole, UserSettings,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct FakeSender {
    pushes: Mutex<Vec<(String, PushPayload)>>,
    encrypted: Mutex<Vec<String>>,
}

impl FakeSender {
    fn pushes(&self) -> Vec<(String, PushPayload)> {
        self.pushes.lock().unwrap().clone()
    }
}

#[async_trait]
impl PushSender for FakeSender {
    async fn send(
        &self,
        endpoint: &str,
        keys: Option<&SubscriptionKeys>,
        payload: &PushPayload,
    ) -> Result<(), PushError> {
        if keys.is_some() {
            self.encrypted.lock().unwrap().push(endpoint.to_string());
        }
        self.pushes
            .lock()
            .unwrap()
            .push((endpoint.to_string(), payload.clone()));
        if endpoint.contains("gone") {
            return Err(PushError::Gone);
        }
        if endpoint.contains("broken") {
            return Err(PushError::Status(500));
        }
        Ok(())
    }
}

struct Env {
    store: Store,
    bus: Bus,
    sender: Arc<FakeSender>,
    notifier: Notifier,
    user: User,
}

async fn env() -> Env {
    let store = Store::open_in_memory().await.unwrap();
    let bus = Bus::new(64);
    let sender = Arc::new(FakeSender::default());
    let notifier = Notifier::with_sender(store.clone(), bus.clone(), sender.clone());
    let user = store
        .create_user(NewUser {
            email: "ada@example.com".into(),
            name: "Ada".into(),
            role: UserRole::Member,
            password_hash: "x".into(),
            limits: UserLimits::default(),
        })
        .await
        .unwrap();
    Env {
        store,
        bus,
        sender,
        notifier,
        user,
    }
}

async fn device(e: &Env, name: &str, endpoint: Option<&str>) -> kiln_types::Device {
    let d = e
        .store
        .upsert_device(NewDevice {
            id: None,
            user_id: e.user.id,
            kind: ClientKind::Android,
            name: name.into(),
        })
        .await
        .unwrap();
    if let Some(ep) = endpoint {
        e.store.set_push_endpoint(d.id, Some(ep)).await.unwrap();
    }
    d
}

fn failed(user: &User) -> NewNotification {
    let mut n = NewNotification::new(
        user.id,
        NotificationKind::TaskFailed,
        "QA failed",
        "filter resets on refresh",
    );
    n.deep_link = "task/FP-139".into();
    n.actions = vec![
        NotificationAction {
            id: "retry".into(),
            label: "Retry".into(),
        },
        NotificationAction {
            id: "open".into(),
            label: "Open".into(),
        },
    ];
    n
}

#[tokio::test]
async fn notify_persists_publishes_and_pushes() {
    let e = env().await;
    let mut sub = e.bus.subscribe();
    device(&e, "pixel", Some("https://ntfy.example.com/upAAA")).await;
    device(&e, "tablet", Some("https://ntfy.example.com/upBBB")).await;
    device(&e, "laptop-no-push", None).await;

    let n = e.notifier.notify(failed(&e.user)).await.unwrap();

    // Persisted.
    let stored = e.store.list_notifications(e.user.id, 10).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, n.id);

    // Published on the bus.
    let ev = sub.next().await.unwrap();
    match &*ev {
        Event::Server(ServerEvent::Notification { notification }) => {
            assert_eq!(notification.id, n.id)
        }
        _ => panic!("unexpected event"),
    }

    // Pushed to both devices with endpoints.
    let pushes = e.sender.pushes();
    assert_eq!(pushes.len(), 2);
    let mut endpoints: Vec<_> = pushes.iter().map(|(ep, _)| ep.clone()).collect();
    endpoints.sort();
    assert_eq!(
        endpoints,
        [
            "https://ntfy.example.com/upAAA",
            "https://ntfy.example.com/upBBB"
        ]
    );
    let p = &pushes[0].1;
    assert_eq!(p.id, n.id);
    assert_eq!(p.kind, NotificationKind::TaskFailed);
    assert_eq!(p.channel, NotificationChannel::Failures);
    assert_eq!(p.deep_link, "task/FP-139");
    assert_eq!(p.actions.len(), 2);
    assert_eq!(p.progress, None);

    // Wire format per ARCHITECTURE.md.
    let json = serde_json::to_value(p).unwrap();
    for key in [
        "id",
        "kind",
        "channel",
        "title",
        "body",
        "deep_link",
        "actions",
    ] {
        assert!(json.get(key).is_some(), "missing {key}");
    }
    assert_eq!(json["channel"], "failures");
    assert_eq!(json["kind"], "task_failed");
    assert!(json.get("progress").is_none());
}

#[tokio::test]
async fn channel_preferences_suppress_push_only() {
    let e = env().await;
    device(&e, "pixel", Some("https://ntfy.example.com/up1")).await;
    let mut channels = BTreeMap::new();
    channels.insert("failures".to_string(), false);
    channels.insert("ready_for_review".to_string(), true);
    e.store
        .set_user_settings(
            e.user.id,
            &UserSettings {
                worker_defaults: BTreeMap::new(),
                default_visibility: Visibility::Private,
                planner_reads_company: false,
                notification_channels: channels,
            },
        )
        .await
        .unwrap();
    let mut sub = e.bus.subscribe();

    e.notifier.notify(failed(&e.user)).await.unwrap();
    assert!(e.sender.pushes().is_empty(), "failures channel is off");
    // ...but it's still in-app.
    assert_eq!(
        e.store
            .list_notifications(e.user.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        &*sub.next().await.unwrap(),
        Event::Server(ServerEvent::Notification { .. })
    ));

    // Explicitly enabled channel and missing (= enabled) channel both push.
    e.notifier
        .notify(NewNotification::new(
            e.user.id,
            NotificationKind::TaskReady,
            "Ready",
            "PR #12",
        ))
        .await
        .unwrap();
    e.notifier
        .notify(NewNotification::new(
            e.user.id,
            NotificationKind::PlanReady,
            "Plan",
            "3 tasks",
        ))
        .await
        .unwrap();
    let kinds: Vec<_> = e.sender.pushes().iter().map(|(_, p)| p.kind).collect();
    assert_eq!(
        kinds,
        vec![NotificationKind::TaskReady, NotificationKind::PlanReady]
    );
}

#[tokio::test]
async fn gone_endpoints_are_unregistered_and_errors_do_not_fail_notify() {
    let e = env().await;
    let gone = device(&e, "old", Some("https://ntfy.example.com/gone")).await;
    let broken = device(&e, "broken", Some("https://ntfy.example.com/broken")).await;
    let bad_scheme = device(&e, "weird", Some("file:///etc/passwd")).await;
    e.notifier.notify(failed(&e.user)).await.unwrap();
    assert_eq!(
        e.sender.pushes().len(),
        2,
        "non-http endpoints are never contacted"
    );
    assert_eq!(
        e.store.get_device(gone.id).await.unwrap().push_endpoint,
        None
    );
    assert!(
        e.store
            .get_device(broken.id)
            .await
            .unwrap()
            .push_endpoint
            .is_some()
    );
    assert!(
        e.store
            .get_device(bad_scheme.id)
            .await
            .unwrap()
            .push_endpoint
            .is_some()
    );
}

#[tokio::test]
async fn resolve_updates_store_and_clients() {
    let e = env().await;
    device(&e, "pixel", Some("https://ntfy.example.com/up1")).await;
    let n = e.notifier.notify(failed(&e.user)).await.unwrap();
    let mut sub = e.bus.subscribe();
    e.notifier
        .resolve(n.id, "✓ Retried · attempt 3/3 running")
        .await
        .unwrap();
    let stored = e.store.get_notification(n.id).await.unwrap();
    assert_eq!(
        stored.resolved.as_deref(),
        Some("✓ Retried · attempt 3/3 running")
    );
    match &*sub.next().await.unwrap() {
        Event::Server(ServerEvent::Notification { notification }) => {
            assert_eq!(
                notification.resolved.as_deref(),
                Some("✓ Retried · attempt 3/3 running")
            )
        }
        _ => panic!("unexpected event"),
    }
    let last = e.sender.pushes().pop().unwrap().1;
    assert_eq!(
        last.resolved.as_deref(),
        Some("✓ Retried · attempt 3/3 running")
    );
    assert!(
        e.notifier
            .resolve(kiln_types::NotificationId::new(), "x")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn workers_live_is_throttled_and_single_card() {
    let e = env().await;
    device(&e, "pixel", Some("https://ntfy.example.com/up1")).await;

    // Zero workers with no card: nothing happens.
    e.notifier.workers_live(e.user.id, 0, 0).await;
    assert!(e.sender.pushes().is_empty());

    e.notifier.workers_live(e.user.id, 2, 30).await;
    e.notifier.workers_live(e.user.id, 3, 40).await; // throttled (< 10 s)
    e.notifier.workers_live(e.user.id, 3, 45).await; // throttled
    let pushes = e.sender.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].1.kind, NotificationKind::WorkersLive);
    assert_eq!(pushes[0].1.channel, NotificationChannel::LiveWorkers);
    assert_eq!(pushes[0].1.progress, Some(30));
    assert_eq!(pushes[0].1.title, "2 workers running");

    // Going to zero is always delivered and resolves the single card.
    e.notifier.workers_live(e.user.id, 0, 0).await;
    let all = e.store.list_notifications(e.user.id, 10).await.unwrap();
    assert_eq!(all.len(), 1, "one ongoing card, not one per update");
    assert!(all[0].resolved.is_some());
    assert_eq!(e.sender.pushes().len(), 2);
    // Repeated zero is a no-op.
    e.notifier.workers_live(e.user.id, 0, 0).await;
    assert_eq!(e.sender.pushes().len(), 2);
}

#[tokio::test]
async fn workers_live_updates_in_place_after_interval() {
    let e = env().await;
    let notifier = Notifier::with_sender(e.store.clone(), e.bus.clone(), e.sender.clone())
        .with_live_interval(Duration::from_millis(30));
    device(&e, "pixel", Some("https://ntfy.example.com/up1")).await;
    notifier.workers_live(e.user.id, 1, 10).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    notifier.workers_live(e.user.id, 4, 70).await;
    let pushes = e.sender.pushes();
    assert_eq!(pushes.len(), 2);
    assert_eq!(pushes[0].1.id, pushes[1].1.id, "same card updated in place");
    assert_eq!(pushes[0].1.title, "1 worker running");
    assert_eq!(pushes[1].1.title, "4 workers running");
    assert_eq!(pushes[1].1.progress, Some(70));
    let stored = e.store.list_notifications(e.user.id, 10).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].body, "avg 70% · tap to open Runs");

    // Throttling is per user.
    let other = e
        .store
        .create_user(NewUser {
            email: "jin@example.com".into(),
            name: "Jin".into(),
            role: UserRole::Member,
            password_hash: "x".into(),
            limits: UserLimits::default(),
        })
        .await
        .unwrap();
    notifier.workers_live(other.id, 1, 5).await;
    assert_eq!(
        e.store
            .list_notifications(other.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn live_workers_channel_can_be_disabled() {
    let e = env().await;
    device(&e, "pixel", Some("https://ntfy.example.com/up1")).await;
    let mut channels = BTreeMap::new();
    channels.insert("live_workers".to_string(), false);
    e.store
        .set_user_settings(
            e.user.id,
            &UserSettings {
                worker_defaults: BTreeMap::new(),
                default_visibility: Visibility::Private,
                planner_reads_company: false,
                notification_channels: channels,
            },
        )
        .await
        .unwrap();
    e.notifier.workers_live(e.user.id, 2, 50).await;
    assert!(e.sender.pushes().is_empty());
    assert_eq!(
        e.store
            .list_notifications(e.user.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// One-shot local HTTP server answering `status`; returns the request text.
async fn serve_once(status: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = sock.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(idx) = text.find("\r\n\r\n") {
                let len = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if buf.len() >= idx + 4 + len || n == 0 {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let resp = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        sock.write_all(resp.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buf).to_string()
    });
    (format!("http://{addr}/up/abc"), handle)
}

#[tokio::test]
async fn http_sender_posts_json_and_maps_status() {
    let payload = PushPayload {
        id: kiln_types::NotificationId::new(),
        kind: NotificationKind::TaskReady,
        channel: NotificationChannel::ReadyForReview,
        title: "Ready".into(),
        body: "PR #12".into(),
        deep_link: "task/FP-1".into(),
        actions: vec![],
        progress: None,
        resolved: None,
    };
    let sender = HttpPushSender::new();

    let (url, h) = serve_once("200 OK").await;
    sender.send(&url, None, &payload).await.unwrap();
    let req = h.await.unwrap();
    assert!(req.starts_with("POST /up/abc"), "{req}");
    assert!(
        req.to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert!(req.contains("\"deep_link\":\"task/FP-1\""));

    let (url, h) = serve_once("410 Gone").await;
    assert!(matches!(
        sender.send(&url, None, &payload).await,
        Err(PushError::Gone)
    ));
    h.await.unwrap();

    let (url, h) = serve_once("503 Service Unavailable").await;
    assert!(matches!(
        sender.send(&url, None, &payload).await,
        Err(PushError::Status(503))
    ));
    h.await.unwrap();
}

/// RFC 8291 Appendix A's user-agent keys: a valid subscription.
fn rfc_keys() -> SubscriptionKeys {
    SubscriptionKeys {
        p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
        auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
    }
}

#[tokio::test]
async fn http_sender_encrypts_when_keys_are_registered() {
    let payload = PushPayload {
        id: kiln_types::NotificationId::new(),
        kind: NotificationKind::TaskReady,
        channel: NotificationChannel::ReadyForReview,
        title: "Ready".into(),
        body: "secret-looking body".into(),
        deep_link: "task/FP-1".into(),
        actions: vec![],
        progress: None,
        resolved: None,
    };
    let (url, h) = serve_once("201 Created").await;
    HttpPushSender::new()
        .send(&url, Some(&rfc_keys()), &payload)
        .await
        .unwrap();
    let req = h.await.unwrap();
    let lower = req.to_ascii_lowercase();
    assert!(lower.contains("content-encoding: aes128gcm"), "{req}");
    assert!(lower.contains("ttl: 86400"), "{req}");
    assert!(
        !req.contains("secret-looking body"),
        "plaintext leaked: {req}"
    );
    assert!(!req.contains("task/FP-1"), "plaintext leaked: {req}");
}

#[tokio::test]
async fn devices_with_keys_get_encrypted_pushes() {
    let e = env().await;
    let plain = device(&e, "Old phone", Some("https://ntfy.example.com/up-plain")).await;
    let enc = device(&e, "Pixel", None).await;
    let k = rfc_keys();
    e.store
        .set_push_subscription(
            enc.id,
            Some("https://ntfy.example.com/up-enc"),
            Some((&k.p256dh, &k.auth)),
        )
        .await
        .unwrap();
    assert!(e.store.get_device(enc.id).await.unwrap().push_encrypted);
    assert!(!e.store.get_device(plain.id).await.unwrap().push_encrypted);
    e.notifier.notify(failed(&e.user)).await.unwrap();
    assert_eq!(e.sender.pushes().len(), 2);
    assert_eq!(
        *e.sender.encrypted.lock().unwrap(),
        vec!["https://ntfy.example.com/up-enc".to_string()]
    );
    // Clearing the endpoint drops the keys with it.
    e.store.set_push_endpoint(enc.id, None).await.unwrap();
    assert!(!e.store.get_device(enc.id).await.unwrap().push_encrypted);
}
