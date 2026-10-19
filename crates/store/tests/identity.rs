//! settings_kv, users, sessions, devices, pairing, credentials.

mod common;

use chrono::Duration;
use common::*;
use kiln_store::*;
use kiln_types::plan::WorkerChoice;
use kiln_types::*;
use std::collections::BTreeMap;

#[tokio::test]
async fn settings_roundtrip() {
    let s = store().await;
    assert_eq!(
        s.get_setting::<String>(setting_keys::ALLOWED_DOMAIN)
            .await
            .unwrap(),
        None
    );
    s.set_setting(setting_keys::ALLOWED_DOMAIN, "example.com")
        .await
        .unwrap();
    s.set_setting(setting_keys::SETUP_DONE, &true)
        .await
        .unwrap();
    assert_eq!(
        s.get_setting::<String>(setting_keys::ALLOWED_DOMAIN)
            .await
            .unwrap()
            .as_deref(),
        Some("example.com")
    );
    s.set_setting(setting_keys::ALLOWED_DOMAIN, "other.org")
        .await
        .unwrap();
    assert_eq!(
        s.get_setting::<String>(setting_keys::ALLOWED_DOMAIN)
            .await
            .unwrap()
            .as_deref(),
        Some("other.org")
    );
    assert_eq!(
        s.get_setting::<bool>(setting_keys::SETUP_DONE)
            .await
            .unwrap(),
        Some(true)
    );
    assert!(s.delete_setting(setting_keys::SETUP_DONE).await.unwrap());
    assert_eq!(
        s.get_setting::<bool>(setting_keys::SETUP_DONE)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn in_memory_stores_are_isolated() {
    let a = store().await;
    let b = store().await;
    user(&a, "x@example.com").await;
    assert_eq!(a.count_users().await.unwrap(), 1);
    assert_eq!(b.count_users().await.unwrap(), 0);
}

#[tokio::test]
async fn file_store_persists_and_uses_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub/kiln.db");
    {
        let s = Store::open(&path).await.unwrap();
        user(&s, "x@example.com").await;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(s.pool())
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(s.pool())
            .await
            .unwrap();
        assert_eq!(fk, 1);
        s.close().await;
    }
    let s = Store::open(&path).await.unwrap();
    assert_eq!(s.count_users().await.unwrap(), 1);
}

#[tokio::test]
async fn users_crud() {
    let s = store().await;
    let u = user(&s, "Ada@Example.com").await;
    assert_eq!(u.email, "ada@example.com");
    assert_eq!(u.initials, "AR");
    assert_eq!(u.status, UserStatus::Active);
    assert_eq!(s.get_user(u.id).await.unwrap(), u);
    assert_eq!(
        s.get_user_by_email("ADA@example.com")
            .await
            .unwrap()
            .unwrap()
            .id,
        u.id
    );
    assert!(
        s.get_user_by_email("nobody@example.com")
            .await
            .unwrap()
            .is_none()
    );
    assert!(s.get_user(UserId::new()).await.unwrap_err().is_not_found());

    // Duplicate email (case-insensitive) conflicts.
    let dup = s
        .create_user(NewUser {
            email: "ADA@example.com".into(),
            name: "Other".into(),
            role: UserRole::Member,
            password_hash: "h".into(),
            limits: UserLimits::default(),
        })
        .await
        .unwrap_err();
    assert!(dup.is_conflict(), "{dup:?}");

    let u2 = s
        .update_user(
            u.id,
            UserPatch {
                name: Some("Ada Lovelace".into()),
                role: Some(UserRole::Admin),
                limits: Some(UserLimits {
                    concurrency: 4,
                    ram_share: 0.25,
                    monthly_budget_usd: 50.0,
                }),
                password_hash: Some("newhash".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(u2.name, "Ada Lovelace");
    assert!(u2.is_admin());
    assert_eq!(u2.limits.concurrency, 4);
    assert_eq!(s.user_password_hash(u.id).await.unwrap(), "newhash");
    assert_eq!(s.list_users().await.unwrap().len(), 1);
    assert_eq!(s.count_users().await.unwrap(), 1);

    assert_eq!(s.get_user_settings(u.id).await.unwrap(), None);
    let mut defaults = BTreeMap::new();
    defaults.insert(WorkerRole::Builder, WorkerChoice::new("claude_sdk", "opus"));
    let mut channels = BTreeMap::new();
    channels.insert("live_workers".to_string(), false);
    let settings = UserSettings {
        worker_defaults: defaults,
        default_visibility: kiln_types::memory::Visibility::Private,
        planner_reads_company: true,
        notification_channels: channels,
    };
    s.set_user_settings(u.id, &settings).await.unwrap();
    assert_eq!(s.get_user_settings(u.id).await.unwrap(), Some(settings));
}

#[tokio::test]
async fn sessions_reject_expired_and_disabled() {
    let s = store().await;
    let u = user(&s, "ada@example.com").await;
    let sid = s
        .create_session(u.id, "hash-live", None, Duration::days(30))
        .await
        .unwrap();
    let (got, got_sid) = s.session_user("hash-live").await.unwrap().unwrap();
    assert_eq!(got.id, u.id);
    assert_eq!(got_sid, sid);
    assert!(s.session_user("unknown").await.unwrap().is_none());

    // Expired session.
    s.create_session(u.id, "hash-expired", None, Duration::seconds(-1))
        .await
        .unwrap();
    assert!(s.session_user("hash-expired").await.unwrap().is_none());

    // Duplicate token hash conflicts.
    assert!(
        s.create_session(u.id, "hash-live", None, Duration::days(1))
            .await
            .unwrap_err()
            .is_conflict()
    );

    // Extending works.
    s.extend_session(sid, Duration::days(30)).await.unwrap();
    assert!(s.session_user("hash-live").await.unwrap().is_some());

    // A disabled user is rejected even with a live session row.
    s.create_session(u.id, "hash-2", None, Duration::days(30))
        .await
        .unwrap();
    sqlx::query("UPDATE users SET status = 'disabled' WHERE id = ?")
        .bind(u.id.to_string())
        .execute(s.pool())
        .await
        .unwrap();
    assert!(s.session_user("hash-2").await.unwrap().is_none());

    // Disabling through update_user also deletes sessions.
    sqlx::query("UPDATE users SET status = 'active' WHERE id = ?")
        .bind(u.id.to_string())
        .execute(s.pool())
        .await
        .unwrap();
    assert!(s.session_user("hash-2").await.unwrap().is_some());
    s.update_user(
        u.id,
        UserPatch {
            status: Some(UserStatus::Disabled),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    s.update_user(
        u.id,
        UserPatch {
            status: Some(UserStatus::Active),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(s.session_user("hash-2").await.unwrap().is_none());

    s.create_session(u.id, "hash-3", None, Duration::days(30))
        .await
        .unwrap();
    assert!(s.delete_session("hash-3").await.unwrap());
    assert!(!s.delete_session("hash-3").await.unwrap());
    assert!(s.session_user("hash-3").await.unwrap().is_none());

    s.create_session(u.id, "hash-4", None, Duration::seconds(-5))
        .await
        .unwrap();
    assert!(s.prune_expired_sessions().await.unwrap() >= 1);
}

#[tokio::test]
async fn devices_and_push_endpoints() {
    let s = store().await;
    let u = user(&s, "ada@example.com").await;
    let d = s
        .upsert_device(NewDevice {
            id: None,
            user_id: u.id,
            kind: ClientKind::Android,
            name: "Pixel".into(),
        })
        .await
        .unwrap();
    assert_eq!(d.push_endpoint, None);
    // Same (user, kind, name) → same device.
    let d2 = s
        .upsert_device(NewDevice {
            id: None,
            user_id: u.id,
            kind: ClientKind::Android,
            name: "Pixel".into(),
        })
        .await
        .unwrap();
    assert_eq!(d.id, d2.id);
    // Known id → renamed in place.
    let d3 = s
        .upsert_device(NewDevice {
            id: Some(d.id),
            user_id: u.id,
            kind: ClientKind::Android,
            name: "Pixel 9".into(),
        })
        .await
        .unwrap();
    assert_eq!(d3.id, d.id);
    assert_eq!(d3.name, "Pixel 9");
    s.upsert_device(NewDevice {
        id: None,
        user_id: u.id,
        kind: ClientKind::Tui,
        name: "laptop".into(),
    })
    .await
    .unwrap();
    assert_eq!(s.list_devices(u.id).await.unwrap().len(), 2);

    s.set_push_endpoint(d.id, Some("https://ntfy.example.com/up123"))
        .await
        .unwrap();
    assert_eq!(
        s.get_device(d.id).await.unwrap().push_endpoint.as_deref(),
        Some("https://ntfy.example.com/up123")
    );
    s.set_push_endpoint(d.id, None).await.unwrap();
    assert_eq!(s.get_device(d.id).await.unwrap().push_endpoint, None);
    assert!(
        s.set_push_endpoint(DeviceId::new(), None)
            .await
            .unwrap_err()
            .is_not_found()
    );

    // Sessions linked to a device go away with it.
    s.create_session(u.id, "dev-session", Some(d.id), Duration::days(1))
        .await
        .unwrap();
    s.delete_device(u.id, d.id).await.unwrap();
    assert!(s.session_user("dev-session").await.unwrap().is_none());
    assert_eq!(s.list_devices(u.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn pairing_codes_are_single_use_and_expire() {
    let s = store().await;
    let u = user(&s, "ada@example.com").await;
    s.create_pairing_code("code-a", u.id, Duration::minutes(10))
        .await
        .unwrap();
    assert_eq!(s.consume_pairing_code("code-a").await.unwrap(), Some(u.id));
    assert_eq!(s.consume_pairing_code("code-a").await.unwrap(), None);
    assert_eq!(s.consume_pairing_code("never").await.unwrap(), None);
    s.create_pairing_code("code-b", u.id, Duration::seconds(-1))
        .await
        .unwrap();
    assert_eq!(s.consume_pairing_code("code-b").await.unwrap(), None);
}

fn new_cred(user: UserId, kind: CredentialKind, name: &str) -> NewCredential {
    NewCredential {
        id: CredentialId::new(),
        user_id: user,
        kind,
        name: name.into(),
        ciphertext: vec![1, 2, 3, 4],
        nonce: vec![9; 12],
        masked: "sk-ant-…3f9a".into(),
        base_url: None,
        env_var: None,
        expires_at: None,
    }
}

#[tokio::test]
async fn credentials_lifecycle_and_isolation() {
    let s = store().await;
    let a = user(&s, "a@example.com").await;
    let b = user(&s, "b@example.com").await;
    let c = s
        .insert_credential(new_cred(a.id, CredentialKind::AnthropicApiKey, "main"))
        .await
        .unwrap();
    assert_eq!(c.status, CredentialHealth::Untested);
    assert_eq!(c.masked, "sk-ant-…3f9a");

    let list = s.list_credentials(a.id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert!(s.list_credentials(b.id).await.unwrap().is_empty());

    let secret = s.get_credential_secret(a.id, c.id).await.unwrap().unwrap();
    assert_eq!(secret.ciphertext, vec![1, 2, 3, 4]);
    assert_eq!(secret.kind, CredentialKind::AnthropicApiKey);
    // Another user cannot fetch it.
    assert!(s.get_credential_secret(b.id, c.id).await.unwrap().is_none());
    assert!(
        s.revoke_credential(b.id, c.id)
            .await
            .unwrap_err()
            .is_not_found()
    );

    let rotated = s
        .rotate_credential(a.id, c.id, &[7, 7], &[8; 12], "sk-ant-…0000", None)
        .await
        .unwrap();
    assert_eq!(rotated.masked, "sk-ant-…0000");
    assert_eq!(
        s.get_credential_secret(a.id, c.id)
            .await
            .unwrap()
            .unwrap()
            .ciphertext,
        vec![7, 7]
    );

    s.set_credential_health(a.id, c.id, &CredentialHealth::Reachable { latency_ms: 42 })
        .await
        .unwrap();
    assert_eq!(
        s.get_credential(a.id, c.id).await.unwrap().status,
        CredentialHealth::Reachable { latency_ms: 42 }
    );
    // Rotation resets health.
    let r2 = s
        .rotate_credential(a.id, c.id, &[1], &[2; 12], "x", Some("http://litellm"))
        .await
        .unwrap();
    assert_eq!(r2.status, CredentialHealth::Untested);
    assert_eq!(r2.base_url.as_deref(), Some("http://litellm"));

    // Expired credentials are excluded from dispatch but still listed.
    let mut expired = new_cred(a.id, CredentialKind::OpenaiApiKey, "old");
    expired.expires_at = Some(chrono::Utc::now() - Duration::days(1));
    s.insert_credential(expired).await.unwrap();
    let dispatch = s.active_credentials_for_dispatch(a.id).await.unwrap();
    assert_eq!(dispatch.len(), 1);
    assert_eq!(dispatch[0].id, c.id);
    assert_eq!(s.list_credentials(a.id).await.unwrap().len(), 2);

    s.revoke_credential(a.id, c.id).await.unwrap();
    assert!(s.get_credential_secret(a.id, c.id).await.unwrap().is_none());
    assert!(
        s.active_credentials_for_dispatch(a.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(s.list_credentials(a.id).await.unwrap().len(), 1);
    assert!(
        s.revoke_credential(a.id, c.id)
            .await
            .unwrap_err()
            .is_not_found()
    );
    // Ciphertext is wiped on revoke.
    let blob: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM credentials WHERE id = ?")
        .bind(c.id.to_string())
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert!(blob.is_empty());
}

#[tokio::test]
async fn debug_output_hides_ciphertext() {
    let c = new_cred(UserId::new(), CredentialKind::Other, "x");
    let dbg = format!("{c:?}");
    assert!(!dbg.contains("ciphertext"));
}
