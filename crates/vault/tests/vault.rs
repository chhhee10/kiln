use async_trait::async_trait;
use kiln_store::{NewUser, Store};
use kiln_types::api::{CreateCredentialRequest, RotateCredentialRequest};
use kiln_types::{CredentialHealth, CredentialId, CredentialKind, UserId, UserLimits, UserRole};
use kiln_vault::*;
use std::sync::{Arc, Mutex};

/// Records calls; answers with a fixed outcome. Never touches the network.
#[derive(Default)]
struct FakeTester {
    calls: Mutex<Vec<(CredentialKind, String, Option<String>)>>,
}

#[async_trait]
impl CredentialTester for FakeTester {
    async fn test(
        &self,
        kind: CredentialKind,
        secret: &str,
        base_url: Option<&str>,
    ) -> TestOutcome {
        self.calls
            .lock()
            .unwrap()
            .push((kind, secret.to_string(), base_url.map(str::to_string)));
        if secret.contains("bad") {
            TestOutcome {
                ok: false,
                latency_ms: 5,
                // A misbehaving tester echoing the secret must still be redacted.
                message: format!("rejected {secret}"),
                health: Some(CredentialHealth::Invalid {
                    error: "rejected (HTTP 401)".into(),
                }),
            }
        } else {
            TestOutcome {
                ok: true,
                latency_ms: 7,
                message: "ok".into(),
                health: Some(CredentialHealth::Reachable { latency_ms: 7 }),
            }
        }
    }
}

async fn setup() -> (Vault, Store, Arc<FakeTester>, UserId, UserId) {
    let store = Store::open_in_memory().await.unwrap();
    let mut ids = Vec::new();
    for email in ["a@example.com", "b@example.com"] {
        let u = store
            .create_user(NewUser {
                email: email.into(),
                name: "U".into(),
                role: UserRole::Member,
                password_hash: "x".into(),
                limits: UserLimits::default(),
            })
            .await
            .unwrap();
        ids.push(u.id);
    }
    let tester = Arc::new(FakeTester::default());
    let vault = Vault::with_tester(MasterKey::generate(), store.clone(), tester.clone());
    (vault, store, tester, ids[0], ids[1])
}

fn req(kind: CredentialKind, name: &str, secret: &str) -> CreateCredentialRequest {
    CreateCredentialRequest {
        kind,
        name: name.into(),
        secret: secret.into(),
        base_url: None,
        env_var: None,
    }
}

const ANTHROPIC: &str = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz3f9a";
const OPENAI: &str = "sk-proj-zyxwvutsrqponmlkjihg0000";
const LITELLM: &str = "sk-litellm-master-key-1111";

#[tokio::test]
async fn add_list_never_returns_secrets() {
    let (vault, store, _, a, b) = setup().await;
    let s = vault
        .add(
            a,
            req(CredentialKind::AnthropicApiKey, "Anthropic", ANTHROPIC),
        )
        .await
        .unwrap();
    assert_eq!(s.masked, "sk-ant-…3f9a");
    assert_eq!(s.status, CredentialHealth::Untested);
    let list = vault.list(a).await.unwrap();
    assert_eq!(list.len(), 1);
    let json = serde_json::to_string(&list).unwrap();
    assert!(!json.contains(ANTHROPIC));
    assert!(!json.contains("abcdefghijklmnop"));
    assert!(vault.list(b).await.unwrap().is_empty());

    // Ciphertext at rest does not contain the plaintext.
    let ct = store
        .get_credential_secret(a, s.id)
        .await
        .unwrap()
        .unwrap()
        .ciphertext;
    assert!(
        !ct.windows(ANTHROPIC.len())
            .any(|w| w == ANTHROPIC.as_bytes())
    );
    assert_eq!(ct.len(), ANTHROPIC.len() + 16, "ciphertext + Poly1305 tag");
}

#[tokio::test]
async fn ciphertext_is_bound_to_user_and_credential() {
    let (vault, store, _, a, b) = setup().await;
    let s = vault
        .add(
            a,
            req(CredentialKind::AnthropicApiKey, "Anthropic", ANTHROPIC),
        )
        .await
        .unwrap();
    // Move the row to user B at the SQL level: decrypting as B must fail.
    kiln_store::sqlx::query("UPDATE credentials SET user_id = ? WHERE id = ?")
        .bind(b.to_string())
        .bind(s.id.to_string())
        .execute(store.pool())
        .await
        .unwrap();
    let env_b = vault.dispatch_env(b).await.unwrap();
    assert!(env_b.env.is_empty(), "B must not obtain A's secret");
    assert!(vault.test(b, s.id).await.is_err());
    // The failure is surfaced as health.
    let h = store.list_credentials(b).await.unwrap()[0].status.clone();
    assert!(matches!(h, CredentialHealth::Invalid { .. }));

    // Copying ciphertext into another credential id also fails.
    let key = MasterKey::generate();
    let (ct, nonce) = key.seal(a, s.id, b"x-secret").unwrap();
    assert!(key.open(a, CredentialId::new(), &ct, &nonce).is_err());
    assert!(key.open(b, s.id, &ct, &nonce).is_err());
    assert!(key.open(a, s.id, &ct, &nonce).is_ok());
}

#[tokio::test]
async fn dispatch_env_mapping() {
    let (vault, _, _, a, _) = setup().await;
    vault
        .add(
            a,
            req(
                CredentialKind::ClaudeOauth,
                "Claude",
                "sk-ant-oat01-oauthtokenvalue123",
            ),
        )
        .await
        .unwrap();
    vault
        .add(
            a,
            req(CredentialKind::AnthropicApiKey, "Anthropic", ANTHROPIC),
        )
        .await
        .unwrap();
    // Second key of the same kind: the oldest wins.
    vault
        .add(
            a,
            req(
                CredentialKind::AnthropicApiKey,
                "Anthropic 2",
                "sk-ant-api03-secondkeyzzzzzzzzzz",
            ),
        )
        .await
        .unwrap();
    vault
        .add(
            a,
            CreateCredentialRequest {
                kind: CredentialKind::Litellm,
                name: "LiteLLM".into(),
                secret: LITELLM.into(),
                base_url: Some("http://litellm:4000/".into()),
                env_var: None,
            },
        )
        .await
        .unwrap();
    vault
        .add(
            a,
            CreateCredentialRequest {
                kind: CredentialKind::Other,
                name: "Gemini".into(),
                secret: "gemini-secret-value-42".into(),
                base_url: None,
                env_var: Some("GEMINI_API_KEY".into()),
            },
        )
        .await
        .unwrap();

    let d = vault.dispatch_env(a).await.unwrap();
    assert_eq!(
        d.get(env::CLAUDE_CODE_OAUTH_TOKEN),
        Some("sk-ant-oat01-oauthtokenvalue123")
    );
    assert_eq!(d.get(env::ANTHROPIC_API_KEY), Some(ANTHROPIC));
    assert_eq!(d.get(env::LITELLM_BASE_URL), Some("http://litellm:4000"));
    assert_eq!(d.get(env::LITELLM_API_KEY), Some(LITELLM));
    // No OpenAI key → Codex gets LiteLLM as its OpenAI endpoint.
    assert_eq!(d.get(env::OPENAI_BASE_URL), Some("http://litellm:4000"));
    assert_eq!(d.get(env::OPENAI_API_KEY), Some(LITELLM));
    assert_eq!(d.get("GEMINI_API_KEY"), Some("gemini-secret-value-42"));
    assert_eq!(d.env.len(), 7);

    // The redactor knows every value.
    let line = format!(
        "token {ANTHROPIC} and {LITELLM} and gemini-secret-value-42 and sk-ant-api03-secondkeyzzzzzzzzzz"
    );
    let red = d.redactor.redact(&line);
    assert_eq!(
        red,
        "token [REDACTED] and [REDACTED] and [REDACTED] and [REDACTED]"
    );
    assert!(!format!("{d:?}").contains(ANTHROPIC));

    // With a real OpenAI key, it takes precedence over the LiteLLM fallback.
    vault
        .add(a, req(CredentialKind::OpenaiApiKey, "OpenAI", OPENAI))
        .await
        .unwrap();
    let d = vault.dispatch_env(a).await.unwrap();
    assert_eq!(d.get(env::OPENAI_API_KEY), Some(OPENAI));
    assert_eq!(d.get(env::OPENAI_BASE_URL), None);
}

#[tokio::test]
async fn add_validation() {
    let (vault, _, _, a, _) = setup().await;
    let bad = [
        req(CredentialKind::AnthropicApiKey, "", ANTHROPIC),
        req(CredentialKind::AnthropicApiKey, "x", "   "),
        req(CredentialKind::Litellm, "no base", LITELLM),
        CreateCredentialRequest {
            kind: CredentialKind::Litellm,
            name: "bad url".into(),
            secret: LITELLM.into(),
            base_url: Some("ftp://x".into()),
            env_var: None,
        },
        req(CredentialKind::Other, "no var", "value-123456"),
        CreateCredentialRequest {
            kind: CredentialKind::Other,
            name: "reserved".into(),
            secret: "value-123456".into(),
            base_url: None,
            env_var: Some("LD_PRELOAD".into()),
        },
        CreateCredentialRequest {
            kind: CredentialKind::Other,
            name: "lower".into(),
            secret: "value-123456".into(),
            base_url: None,
            env_var: Some("my_key".into()),
        },
    ];
    for r in bad {
        assert!(
            matches!(vault.add(a, r.clone()).await, Err(VaultError::Invalid(_))),
            "{}",
            r.name
        );
    }
    assert!(vault.list(a).await.unwrap().is_empty());
}

#[tokio::test]
async fn rotate_and_revoke() {
    let (vault, _, _, a, b) = setup().await;
    let s = vault
        .add(a, req(CredentialKind::OpenaiApiKey, "OpenAI", OPENAI))
        .await
        .unwrap();
    assert!(matches!(
        vault
            .rotate(
                b,
                s.id,
                RotateCredentialRequest {
                    secret: "sk-other-123456789012345".into(),
                    base_url: None
                }
            )
            .await,
        Err(VaultError::NotFound)
    ));
    let r = vault
        .rotate(
            a,
            s.id,
            RotateCredentialRequest {
                secret: "sk-proj-rotatedrotated9999".into(),
                base_url: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(r.masked, "sk-proj-…9999");
    assert_eq!(
        vault
            .dispatch_env(a)
            .await
            .unwrap()
            .get(env::OPENAI_API_KEY),
        Some("sk-proj-rotatedrotated9999")
    );

    assert!(matches!(
        vault.revoke(b, s.id).await,
        Err(VaultError::NotFound)
    ));
    vault.revoke(a, s.id).await.unwrap();
    assert!(vault.list(a).await.unwrap().is_empty());
    assert!(vault.dispatch_env(a).await.unwrap().env.is_empty());
    assert!(matches!(
        vault.revoke(a, s.id).await,
        Err(VaultError::NotFound)
    ));
}

#[tokio::test]
async fn test_connection_stores_health() {
    let (vault, store, tester, a, b) = setup().await;
    let good = vault
        .add(
            a,
            CreateCredentialRequest {
                kind: CredentialKind::Litellm,
                name: "LiteLLM".into(),
                secret: LITELLM.into(),
                base_url: Some("http://litellm:4000".into()),
                env_var: None,
            },
        )
        .await
        .unwrap();
    let r = vault.test(a, good.id).await.unwrap();
    assert!(r.ok);
    assert_eq!(r.latency_ms, 7);
    assert_eq!(
        store.get_credential(a, good.id).await.unwrap().status,
        CredentialHealth::Reachable { latency_ms: 7 }
    );
    {
        let calls = tester.calls.lock().unwrap();
        assert_eq!(
            calls[0],
            (
                CredentialKind::Litellm,
                LITELLM.to_string(),
                Some("http://litellm:4000".to_string())
            )
        );
    }

    let bad = vault
        .add(
            a,
            req(
                CredentialKind::AnthropicApiKey,
                "Bad",
                "sk-ant-bad-key-0000000000",
            ),
        )
        .await
        .unwrap();
    let r = vault.test(a, bad.id).await.unwrap();
    assert!(!r.ok);
    assert!(!r.message.contains("sk-ant-bad-key"), "{}", r.message);
    assert!(matches!(
        store.get_credential(a, bad.id).await.unwrap().status,
        CredentialHealth::Invalid { .. }
    ));

    // Another user can't test it.
    assert!(matches!(
        vault.test(b, good.id).await,
        Err(VaultError::NotFound)
    ));
}

#[tokio::test]
async fn verify_key_detects_a_changed_master_key() {
    let store = Store::open_in_memory().await.unwrap();
    let k = MasterKey::generate();
    let v1 = Vault::new(k.clone(), store.clone());
    v1.verify_key().await.unwrap(); // records
    v1.verify_key().await.unwrap(); // matches
    let v2 = Vault::new(MasterKey::generate(), store.clone());
    assert!(matches!(
        v2.verify_key().await,
        Err(VaultError::KeyMismatch)
    ));
}

#[tokio::test]
async fn file_key_survives_restart_and_decrypts() {
    let dir = tempfile::tempdir().unwrap();
    let opts = KeyOptions {
        env: false,
        keychain: false,
    };
    let store = Store::open(&dir.path().join("kiln.db")).await.unwrap();
    let u = store
        .create_user(NewUser {
            email: "a@example.com".into(),
            name: "A".into(),
            role: UserRole::Member,
            password_hash: "x".into(),
            limits: UserLimits::default(),
        })
        .await
        .unwrap();
    {
        let (key, src) = MasterKey::load_or_create_with(dir.path(), opts).unwrap();
        assert_eq!(src, KeySource::File);
        let v = Vault::new(key, store.clone());
        v.verify_key().await.unwrap();
        v.add(u.id, req(CredentialKind::AnthropicApiKey, "A", ANTHROPIC))
            .await
            .unwrap();
    }
    let (key, _) = MasterKey::load_or_create_with(dir.path(), opts).unwrap();
    let v = Vault::new(key, store.clone());
    v.verify_key().await.unwrap();
    assert_eq!(
        v.dispatch_env(u.id)
            .await
            .unwrap()
            .get(env::ANTHROPIC_API_KEY),
        Some(ANTHROPIC)
    );
}
