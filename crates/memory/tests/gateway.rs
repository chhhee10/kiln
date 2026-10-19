//! Proposal rules, dedupe / supersede, approval flow, updates, feedback,
//! audit and index sync.

mod common;

use common::*;
use kiln_memory::{MemoryConfig, ProposeAction, UseContext};
use kiln_types::api::{ApproveMemoryRequest, MemoryDecision, UpdateMemoryRequest};
use kiln_types::memory::{MemoryQuery, MemoryStatus, MemoryType, Visibility};
use kiln_types::{MemoryId, WorkerRole};
use sqlx::SqlitePool;

/// Audit actions of an item, without the (frequent) `used` rows.
async fn audit_actions(pool: &SqlitePool, id: MemoryId) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT action FROM memory_audit WHERE item_id = ? AND action <> 'used' ORDER BY id",
    )
    .bind(id.to_string())
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn links(pool: &SqlitePool, kind: &str) -> Vec<(String, String)> {
    sqlx::query_as("SELECT from_id, to_id FROM memory_links WHERE kind = ? ORDER BY from_id")
        .bind(kind)
        .fetch_all(pool)
        .await
        .unwrap()
}

fn approve() -> ApproveMemoryRequest {
    ApproveMemoryRequest {
        decision: MemoryDecision::Approve,
        edit: None,
    }
}

#[tokio::test]
async fn private_proposals_are_active_immediately() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let item = env
        .mem
        .propose(
            a,
            None,
            prop(MemoryType::Fact, "api", "uses", "REST", Visibility::Private),
        )
        .await
        .unwrap();
    assert_eq!(item.status, MemoryStatus::Active);
    assert_eq!(item.visibility, Visibility::Private);
    assert_eq!(item.requested_visibility, None);
    assert_eq!(audit_actions(&env.pool, item.id).await, vec!["created"]);
}

#[tokio::test]
async fn shared_and_company_proposals_wait_for_the_owner() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let item = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Convention,
                "FP",
                "formats_with",
                "rustfmt",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    assert_eq!(item.status, MemoryStatus::Pending);
    assert_eq!(
        item.visibility,
        Visibility::Private,
        "stays private until approved"
    );
    assert_eq!(item.requested_visibility, Some(Visibility::Company));
    let q = MemoryQuery {
        q: "rustfmt".into(),
        ..Default::default()
    };
    assert!(env.mem.search(b, &q).await.unwrap().is_empty());
    assert_eq!(
        env.mem.search(a, &q).await.unwrap().len(),
        1,
        "owner sees own pending item"
    );
    let queue = env
        .mem
        .list(
            a,
            &MemoryQuery {
                pending_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(queue.items.len(), 1);
    assert_eq!(queue.pending_approvals, 1);

    let approved = env.mem.decide(a, item.id, approve()).await.unwrap();
    assert_eq!(approved.status, MemoryStatus::Active);
    assert_eq!(approved.visibility, Visibility::Company);
    assert!(
        approved.salience > item.salience,
        "approval is a human signal"
    );
    assert_eq!(env.mem.search(b, &q).await.unwrap().len(), 1);
    assert_eq!(
        audit_actions(&env.pool, item.id).await,
        vec!["created", "approved"]
    );

    // Shared needs people and only they see it after approval.
    let c = add_user(&env.pool, "Cy Moss").await;
    let mut p = prop(
        MemoryType::Fact,
        "staging",
        "lives_at",
        "stg.internal",
        Visibility::Shared,
    );
    assert!(
        env.mem
            .propose(a, None, p.clone())
            .await
            .unwrap_err()
            .is_invalid(),
        "shared with nobody"
    );
    p.shared_with = vec![b, a];
    let s = env.mem.propose(a, None, p).await.unwrap();
    assert_eq!(s.shared_with, vec![b], "owner removed from share list");
    env.mem.decide(a, s.id, approve()).await.unwrap();
    let q = MemoryQuery {
        q: "stg".into(),
        ..Default::default()
    };
    assert_eq!(env.mem.search(b, &q).await.unwrap().len(), 1);
    assert!(env.mem.search(c, &q).await.unwrap().is_empty());
}

#[tokio::test]
async fn preferences_are_always_private() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let item = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Preference,
                "Ada",
                "prefers",
                "tabs",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    assert_eq!(item.visibility, Visibility::Private);
    assert_eq!(
        item.status,
        MemoryStatus::Active,
        "forced private ⇒ active, no approval"
    );
    for v in [Visibility::Shared, Visibility::Company] {
        let req = UpdateMemoryRequest {
            visibility: Some(v),
            shared_with: Some(vec![b]),
            ..Default::default()
        };
        assert!(
            env.mem
                .update(a, item.id, req)
                .await
                .unwrap_err()
                .is_invalid()
        );
    }
    let q = MemoryQuery {
        q: "tabs".into(),
        ..Default::default()
    };
    assert!(env.mem.search(b, &q).await.unwrap().is_empty());
}

#[tokio::test]
async fn proposals_validate_inputs() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    assert!(
        env.mem
            .propose(
                a,
                None,
                prop(MemoryType::Fact, "", "is", "x", Visibility::Private)
            )
            .await
            .unwrap_err()
            .is_invalid()
    );
    assert!(
        env.mem
            .propose(
                a,
                None,
                prop(MemoryType::Episode, "s", "is", "x", Visibility::Private)
            )
            .await
            .unwrap_err()
            .is_invalid()
    );
    let ghost = kiln_types::UserId::new();
    assert!(
        env.mem
            .propose(
                ghost,
                None,
                prop(MemoryType::Fact, "s", "is", "x", Visibility::Private)
            )
            .await
            .unwrap_err()
            .is_invalid()
    );
    let mut p = prop(MemoryType::Fact, "s", "is", "x", Visibility::Shared);
    p.shared_with = vec![ghost];
    assert!(env.mem.propose(a, None, p).await.unwrap_err().is_invalid());
    // Someone else's run can't be the source.
    let run_b = add_run(&env.pool, b, None, WorkerRole::Builder).await;
    let e = env
        .mem
        .propose(
            a,
            Some(run_b.id),
            prop(MemoryType::Fact, "s", "is", "x", Visibility::Private),
        )
        .await
        .unwrap_err();
    assert!(e.is_forbidden());
    // Empty title is derived from the triple; relation defaults.
    let mut p = prop(MemoryType::Fact, "db", "", "sqlite", Visibility::Private);
    p.title = "  ".into();
    let item = env.mem.propose(a, None, p).await.unwrap();
    assert_eq!(item.relation, "is");
    assert_eq!(item.title, "db is sqlite");
}

#[tokio::test]
async fn duplicates_merge_instead_of_inserting() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let run = add_run(&env.pool, a, None, WorkerRole::Builder).await;
    let first = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Convention,
                "FP",
                "runs_tests_with",
                "`cargo test`",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let out = env
        .mem
        .propose_detailed(
            a,
            Some(run.id),
            prop(
                MemoryType::Convention,
                "fp ",
                "Runs_Tests_With",
                "cargo test",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    assert_eq!(out.action, ProposeAction::Merged);
    assert_eq!(out.item.id, first.id);
    assert!(out.item.salience > first.salience);
    assert_eq!(out.item.use_count, first.use_count + 1);
    let all = env.mem.list(a, &MemoryQuery::default()).await.unwrap();
    assert_eq!(all.total, 1, "no second row");
    assert_eq!(
        links(&env.pool, "duplicate_of").await,
        vec![(run.id.to_string(), first.id.to_string())]
    );
    assert!(
        audit_actions(&env.pool, first.id)
            .await
            .contains(&"merged".to_string())
    );

    // High cosine with the same subject+relation also merges (≥ 0.92).
    let mut long = prop(
        MemoryType::Procedure,
        "flaky upload",
        "fixed_by",
        "retry with backoff",
        Visibility::Private,
    );
    long.body = "When the upload to the artifact store times out, retry it with exponential backoff and jitter, \
                 up to five attempts, logging each failure with the request id."
        .into();
    let x = env.mem.propose(a, None, long.clone()).await.unwrap();
    long.object = "retry with backoff and jitter".into();
    let out = env.mem.propose_detailed(a, None, long).await.unwrap();
    assert_eq!(
        out.action,
        ProposeAction::Merged,
        "near-identical text merges"
    );
    assert_eq!(out.item.id, x.id);
}

#[tokio::test]
async fn duplicate_of_a_company_item_bumps_it_without_copying() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let item = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Decision,
                "auth",
                "uses",
                "argon2",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    env.mem.decide(a, item.id, approve()).await.unwrap();
    let out = env
        .mem
        .propose_detailed(
            b,
            None,
            prop(
                MemoryType::Decision,
                "auth",
                "uses",
                "argon2",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    assert_eq!(out.action, ProposeAction::Merged);
    assert_eq!(out.item.id, item.id);
    assert_eq!(out.item.owner_id, a, "ownership never changes");
    assert_eq!(out.item.visibility, Visibility::Company);
    // A private item of A is NOT a dedupe target for B (not visible).
    let secret = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "key",
                "lives_in",
                "vault",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let mine = env
        .mem
        .propose_detailed(
            b,
            None,
            prop(
                MemoryType::Fact,
                "key",
                "lives_in",
                "vault",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    assert_eq!(mine.action, ProposeAction::Created);
    assert_ne!(mine.item.id, secret.id);
}

#[tokio::test]
async fn wider_duplicate_request_turns_owner_item_pending() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let item = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "ci",
                "runs_on",
                "buildkite",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let out = env
        .mem
        .propose_detailed(
            a,
            None,
            prop(
                MemoryType::Fact,
                "ci",
                "runs_on",
                "buildkite",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    assert_eq!(out.action, ProposeAction::Merged);
    assert_eq!(out.item.status, MemoryStatus::Pending);
    assert_eq!(out.item.requested_visibility, Some(Visibility::Company));
    assert_eq!(out.item.visibility, Visibility::Private);
    assert_eq!(out.item.id, item.id);
}

#[tokio::test]
async fn contradictions_supersede_the_older_item() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let old = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Convention,
                "FP",
                "runs_tests_with",
                "`cargo test`",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let out = env
        .mem
        .propose_detailed(
            a,
            None,
            prop(
                MemoryType::Convention,
                "FP",
                "runs_tests_with",
                "`cargo nextest run`",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    assert_eq!(out.action, ProposeAction::Created);
    assert_eq!(out.superseded, vec![old.id]);
    let old_now = env.mem.get(a, old.id).await.unwrap().item;
    assert_eq!(old_now.status, MemoryStatus::Superseded);
    assert_eq!(
        links(&env.pool, "supersedes").await,
        vec![(out.item.id.to_string(), old.id.to_string())]
    );
    let superseded_by: Option<String> =
        sqlx::query_scalar("SELECT superseded_by FROM memory_items WHERE id = ?")
            .bind(old.id.to_string())
            .fetch_one(&env.pool)
            .await
            .unwrap();
    assert_eq!(superseded_by, Some(out.item.id.to_string()));
    let hits = env
        .mem
        .search(
            a,
            &MemoryQuery {
                q: "FP tests".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|h| h.item.id).collect::<Vec<_>>(),
        vec![out.item.id],
        "superseded items are not retrieved"
    );

    // Multi-valued relations accumulate instead of contradicting.
    let tokio = env
        .mem
        .propose(
            a,
            None,
            prop(MemoryType::Fact, "FP", "uses", "tokio", Visibility::Private),
        )
        .await
        .unwrap();
    let serde = env
        .mem
        .propose_detailed(
            a,
            None,
            prop(MemoryType::Fact, "FP", "uses", "serde", Visibility::Private),
        )
        .await
        .unwrap();
    assert!(serde.superseded.is_empty());
    assert_eq!(
        env.mem.get(a, tokio.id).await.unwrap().item.status,
        MemoryStatus::Active
    );

    // Another user's contradicting item is linked, never superseded.
    let theirs = env
        .mem
        .propose(
            b,
            None,
            prop(
                MemoryType::Decision,
                "db",
                "is",
                "postgres",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    env.mem.decide(b, theirs.id, approve()).await.unwrap();
    let mine = env
        .mem
        .propose_detailed(
            a,
            None,
            prop(
                MemoryType::Decision,
                "db",
                "is",
                "sqlite",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    assert!(mine.superseded.is_empty());
    assert_eq!(
        env.mem.get(b, theirs.id).await.unwrap().item.status,
        MemoryStatus::Active
    );
    assert!(
        links(&env.pool, "related")
            .await
            .contains(&(mine.item.id.to_string(), theirs.id.to_string()))
    );
}

#[tokio::test]
async fn pending_contradiction_supersedes_on_approval_only() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let old = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Decision,
                "queue",
                "is",
                "redis",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    env.mem.decide(a, old.id, approve()).await.unwrap();
    let new = env
        .mem
        .propose_detailed(
            a,
            None,
            prop(
                MemoryType::Decision,
                "queue",
                "is",
                "sqlite jobs table",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    assert_eq!(new.item.status, MemoryStatus::Pending);
    assert_eq!(
        env.mem.get(a, old.id).await.unwrap().item.status,
        MemoryStatus::Active,
        "old stays live while new is pending"
    );
    env.mem.decide(a, new.item.id, approve()).await.unwrap();
    assert_eq!(
        env.mem.get(a, old.id).await.unwrap().item.status,
        MemoryStatus::Superseded
    );

    // A private replacement never hides a company item from others.
    let old2 = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Decision,
                "cache",
                "is",
                "memcached",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    env.mem.decide(a, old2.id, approve()).await.unwrap();
    let new2 = env
        .mem
        .propose_detailed(
            a,
            None,
            prop(
                MemoryType::Decision,
                "cache",
                "is",
                "in-process lru",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    assert!(new2.superseded.is_empty());
    assert_eq!(
        env.mem.get(a, old2.id).await.unwrap().item.status,
        MemoryStatus::Active
    );
}

#[tokio::test]
async fn decisions_cover_every_outcome() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let mk = |o: &str| {
        let mut p = prop(
            MemoryType::Fact,
            "svc",
            &format!("rel_{o}"),
            o,
            Visibility::Company,
        );
        p.shared_with = vec![b];
        p
    };
    // KeepShared: company request downgraded to the share list.
    let i = env.mem.propose(a, None, mk("one")).await.unwrap();
    assert!(
        i.shared_with.is_empty(),
        "company proposals carry no share list"
    );
    let keep_shared = ApproveMemoryRequest {
        decision: MemoryDecision::KeepShared,
        edit: Some(UpdateMemoryRequest {
            shared_with: Some(vec![b]),
            title: Some("Edited".into()),
            ..Default::default()
        }),
    };
    let r = env.mem.decide(a, i.id, keep_shared).await.unwrap();
    assert_eq!(
        (
            r.status,
            r.visibility,
            r.shared_with.clone(),
            r.title.as_str()
        ),
        (MemoryStatus::Active, Visibility::Shared, vec![b], "Edited")
    );
    // KeepPrivate
    let i = env.mem.propose(a, None, mk("two")).await.unwrap();
    let r = env
        .mem
        .decide(
            a,
            i.id,
            ApproveMemoryRequest {
                decision: MemoryDecision::KeepPrivate,
                edit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.visibility),
        (MemoryStatus::Active, Visibility::Private)
    );
    // Reject
    let i = env.mem.propose(a, None, mk("three")).await.unwrap();
    let r = env
        .mem
        .decide(
            a,
            i.id,
            ApproveMemoryRequest {
                decision: MemoryDecision::Reject,
                edit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.visibility, r.requested_visibility),
        (MemoryStatus::Rejected, Visibility::Private, None)
    );
    assert!(
        audit_actions(&env.pool, i.id)
            .await
            .contains(&"rejected".to_string())
    );
    // Not pending any more ⇒ conflict.
    assert!(
        env.mem
            .decide(a, i.id, approve())
            .await
            .unwrap_err()
            .is_conflict()
    );
    // KeepShared without people is invalid.
    let i = env.mem.propose(a, None, mk("four")).await.unwrap();
    let e = env
        .mem
        .decide(
            a,
            i.id,
            ApproveMemoryRequest {
                decision: MemoryDecision::KeepShared,
                edit: None,
            },
        )
        .await
        .unwrap_err();
    assert!(e.is_invalid());
}

#[tokio::test]
async fn update_visibility_rules() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let c = add_user(&env.pool, "Cy Moss").await;
    let item = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "deploy",
                "uses",
                "fly.io",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let vis = |v, s: Option<Vec<_>>| UpdateMemoryRequest {
        visibility: Some(v),
        shared_with: s,
        ..Default::default()
    };

    // widening → pending
    let r = env
        .mem
        .update(a, item.id, vis(Visibility::Company, None))
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.visibility, r.requested_visibility),
        (
            MemoryStatus::Pending,
            Visibility::Private,
            Some(Visibility::Company)
        )
    );
    // back to private → immediate
    let r = env
        .mem
        .update(a, item.id, vis(Visibility::Private, None))
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.visibility, r.requested_visibility),
        (MemoryStatus::Active, Visibility::Private, None)
    );
    // shared needs people
    assert!(
        env.mem
            .update(a, item.id, vis(Visibility::Shared, None))
            .await
            .unwrap_err()
            .is_invalid()
    );
    let r = env
        .mem
        .update(a, item.id, vis(Visibility::Shared, Some(vec![b, c])))
        .await
        .unwrap();
    assert_eq!(r.status, MemoryStatus::Pending);
    let r = env.mem.decide(a, item.id, approve()).await.unwrap();
    assert_eq!((r.visibility, r.shared_with.len()), (Visibility::Shared, 2));
    // narrowing the share list → immediate
    let r = env
        .mem
        .update(
            a,
            item.id,
            UpdateMemoryRequest {
                shared_with: Some(vec![b]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.shared_with.clone()),
        (MemoryStatus::Active, vec![b])
    );
    let q = MemoryQuery {
        q: "fly".into(),
        ..Default::default()
    };
    assert!(
        env.mem.search(c, &q).await.unwrap().is_empty(),
        "c removed immediately"
    );
    // adding someone → pending (and hidden from everyone else meanwhile)
    let r = env
        .mem
        .update(
            a,
            item.id,
            UpdateMemoryRequest {
                shared_with: Some(vec![b, c]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.status, MemoryStatus::Pending);
    assert!(env.mem.search(c, &q).await.unwrap().is_empty());
    env.mem.decide(a, item.id, approve()).await.unwrap();
    assert_eq!(env.mem.search(c, &q).await.unwrap().len(), 1);
    // company → shared narrowing is immediate
    env.mem
        .update(a, item.id, vis(Visibility::Company, None))
        .await
        .unwrap();
    env.mem.decide(a, item.id, approve()).await.unwrap();
    let r = env
        .mem
        .update(a, item.id, vis(Visibility::Shared, Some(vec![b])))
        .await
        .unwrap();
    assert_eq!(
        (r.status, r.visibility),
        (MemoryStatus::Active, Visibility::Shared)
    );
    // edits re-embed and re-index
    let r = env
        .mem
        .update(
            a,
            item.id,
            UpdateMemoryRequest {
                title: Some("Deploys use kubernetes".into()),
                body: Some("helm charts".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.title, "Deploys use kubernetes");
    let hits = env
        .mem
        .search(
            b,
            &MemoryQuery {
                q: "kubernetes helm".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(hits.first().map(|h| h.item.id), Some(item.id));
    assert!(
        env.mem
            .update(
                a,
                item.id,
                UpdateMemoryRequest {
                    title: Some(" ".into()),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
            .is_invalid()
    );
    // archive / restore
    let r = env
        .mem
        .update(
            a,
            item.id,
            UpdateMemoryRequest {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.status, MemoryStatus::Archived);
    assert!(
        env.mem.search(b, &q).await.unwrap().is_empty(),
        "archived items are not retrieved"
    );
    assert!(
        env.mem
            .update(a, item.id, vis(Visibility::Company, None))
            .await
            .unwrap_err()
            .is_conflict()
    );
    let r = env
        .mem
        .update(
            a,
            item.id,
            UpdateMemoryRequest {
                archived: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.status, MemoryStatus::Active);
    let actions = audit_actions(&env.pool, item.id).await;
    for a in ["created", "visibility", "approved", "edited", "archived"] {
        assert!(
            actions.contains(&a.to_string()),
            "missing audit {a}: {actions:?}"
        );
    }
}

#[tokio::test]
async fn feedback_boosts_and_flags() {
    let env = env_with(MemoryConfig {
        flag_after_failures: 2,
        ..Default::default()
    })
    .await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let p = add_project(&env.pool, "FP").await;
    let task = add_task(&env.pool, a, p, "FP-7", "retry uploads").await;
    let good = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Procedure,
                "uploads",
                "retry_with",
                "backoff",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let bad = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Procedure,
                "uploads",
                "skip",
                "checksums",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();

    let r1 = add_run(&env.pool, a, Some(task.id), WorkerRole::Builder).await;
    let ctx = UseContext {
        run: Some(r1.id),
        task: Some(task.id),
        role: Some(WorkerRole::Builder),
    };
    env.mem
        .search_ctx(
            a,
            &MemoryQuery {
                q: "uploads backoff".into(),
                limit: Some(1),
                ..Default::default()
            },
            &ctx,
        )
        .await
        .unwrap();
    let rep = env.mem.feedback(r1.id, true).await.unwrap();
    assert_eq!((rep.boosted, rep.penalized), (1, 0));
    let g = env.mem.get(a, good.id).await.unwrap();
    assert!(g.item.salience > good.salience);
    assert_eq!(g.recently_used.len(), 1);
    assert_eq!(g.recently_used[0].task_key, "FP-7");
    assert_eq!(g.recently_used[0].role, WorkerRole::Builder);
    // idempotent per run
    assert_eq!(env.mem.feedback(r1.id, true).await.unwrap().boosted, 0);

    for _ in 0..2 {
        let r = add_run(&env.pool, a, Some(task.id), WorkerRole::Qa).await;
        let ctx = UseContext {
            run: Some(r.id),
            task: Some(task.id),
            role: Some(WorkerRole::Qa),
        };
        env.mem.record_use(a, &[bad.id], &ctx).await.unwrap();
        env.mem.feedback(r.id, false).await.unwrap();
    }
    let b_now = env.mem.get(a, bad.id).await.unwrap().item;
    assert_eq!(b_now.status, MemoryStatus::Flagged, "failures dominate");
    let fails: i64 = sqlx::query_scalar("SELECT failure_count FROM memory_items WHERE id = ?")
        .bind(bad.id.to_string())
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(fails, 2);
    let hits = env
        .mem
        .search(
            a,
            &MemoryQuery {
                q: "checksums".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        hits.iter().all(|h| h.item.id != bad.id),
        "flagged items are not retrieved"
    );
    assert!(
        audit_actions(&env.pool, bad.id)
            .await
            .contains(&"flagged".to_string())
    );
}

#[tokio::test]
async fn list_counts_and_detail() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let mut p = prop(
        MemoryType::Fact,
        "on-call",
        "rotates",
        "weekly",
        Visibility::Shared,
    );
    p.shared_with = vec![b];
    let item = env.mem.propose(a, None, p).await.unwrap();
    env.mem.decide(a, item.id, approve()).await.unwrap();
    let run = add_run(&env.pool, b, None, WorkerRole::Reviewer).await;
    env.mem
        .record_use(
            b,
            &[item.id],
            &UseContext {
                run: Some(run.id),
                task: None,
                role: Some(WorkerRole::Reviewer),
            },
        )
        .await
        .unwrap();
    let r = env.mem.list(b, &MemoryQuery::default()).await.unwrap();
    assert_eq!((r.total, r.runs_this_week, r.pending_approvals), (1, 1, 0));
    let d = env.mem.get(b, item.id).await.unwrap();
    assert_eq!(d.owner.name, "Ada Rao");
    assert_eq!(d.owner.initials, "AR");
    assert_eq!(
        d.shared_with.iter().map(|u| u.id).collect::<Vec<_>>(),
        vec![b]
    );
    assert_eq!(d.recently_used.len(), 1);
    assert!(d.recently_used[0].task_key.starts_with("run "));
}

#[tokio::test]
async fn indexes_survive_restart_and_rebuild_when_wiped() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    env.mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "billing",
                "uses",
                "stripe",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let env = env.reopen(MemoryConfig::default()).await;
    let q = MemoryQuery {
        q: "stripe".into(),
        ..Default::default()
    };
    assert_eq!(env.mem.search(a, &q).await.unwrap().len(), 1);
    // Wipe the Tantivy directory: the next open rebuilds it from SQLite.
    let data = env.data.clone();
    let Env {
        mem,
        pool,
        data: d,
        _dir,
    } = env;
    drop(mem);
    std::fs::remove_dir_all(data.join("index").join("tantivy")).unwrap();
    let env = Env {
        mem: kiln_memory::Memory::open(pool.clone(), &d, MemoryConfig::default())
            .await
            .unwrap(),
        pool,
        data: d,
        _dir,
    };
    assert_eq!(env.mem.text_index().count().await.unwrap(), 1);
    assert_eq!(env.mem.search(a, &q).await.unwrap().len(), 1);
}
