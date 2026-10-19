//! Distill (L0→L1), extract (L1→L2), packs (L3), nightly maintenance and
//! the MCP helpers.

mod common;

use chrono::{Duration, Utc};
use common::*;
use kiln_memory::{MemoryConfig, NightlyOptions, ProposeAction, UseContext};
use kiln_types::api::{ApproveMemoryRequest, MemoryDecision};
use kiln_types::event::{LogLevel, RunEvent, RunEventKind};
use kiln_types::memory::{MemoryQuery, MemoryStatus, MemoryType, Visibility};
use kiln_types::{Run, Task, UserId, WorkerRole};

/// A builder run that hits a compile error, fixes it and passes.
fn builder_events(run: &Run) -> Vec<RunEvent> {
    let t0 = Utc::now() - Duration::minutes(5);
    let kinds = vec![
        RunEventKind::Step {
            title: "implement retry".into(),
        },
        cmd(
            "cargo test",
            101,
            "   Compiling fp v0.1.0\nerror[E0425]: cannot find value `backoff` in this scope\n",
        ),
        RunEventKind::FileChange {
            path: "src/upload/retry.rs".into(),
            change: "M".into(),
        },
        cmd("cargo test", 0, "test result: ok. 12 passed"),
        RunEventKind::Error {
            message: "lint step timed out".into(),
        },
        log(LogLevel::Pass, "lint ok on retry"),
        cmd("cargo clippy -- -D warnings", 0, ""),
        RunEventKind::Usage {
            tokens_in: 1000,
            tokens_out: 200,
            cost_usd: 0.11,
        },
        RunEventKind::Done {
            success: true,
            summary: "Retry with backoff implemented".into(),
        },
    ];
    kinds
        .into_iter()
        .enumerate()
        .map(|(i, k)| ev(run.id, i as u64 + 1, t0 + Duration::seconds(i as i64), k))
        .collect()
}

struct World {
    env: Env,
    a: UserId,
    b: UserId,
    project: kiln_types::ProjectId,
    task: Task,
    run: Run,
}

async fn world_with(cfg: MemoryConfig, a_settings: Option<serde_json::Value>) -> World {
    let env = env_with(cfg).await;
    let a = add_user_with_settings(&env.pool, "Ada Rao", a_settings).await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let project = add_project(&env.pool, "FP").await;
    let task = add_task(
        &env.pool,
        a,
        project,
        "FP-142",
        "Retry uploads with backoff",
    )
    .await;
    let run = add_run(&env.pool, a, Some(task.id), WorkerRole::Builder).await;
    env.mem
        .ingest_l0(&run, Some(project), &builder_events(&run), &no_redact)
        .await
        .unwrap();
    World {
        env,
        a,
        b,
        project,
        task,
        run,
    }
}

async fn world() -> World {
    world_with(MemoryConfig::default(), None).await
}

#[tokio::test]
async fn distill_builds_an_episode() {
    let w = world().await;
    let ep = w.env.mem.distill(&w.run, Some(&w.task)).await.unwrap();
    assert_eq!(ep.goal, "FP-142: Retry uploads with backoff");
    assert_eq!(ep.outcome, "succeeded: Retry with backoff implemented");
    assert_eq!(ep.owner_id, w.a);
    assert_eq!(ep.project_id, Some(w.project));
    assert_eq!(ep.role, WorkerRole::Builder);
    assert!(
        ep.errors.iter().any(|e| e.contains("error[E0425]")),
        "{:?}",
        ep.errors
    );
    assert!(ep.errors.iter().any(|e| e == "lint step timed out"));
    assert!(
        ep.fixes
            .iter()
            .any(|f| f.contains("changed src/upload/retry.rs") && f.contains("cargo test")),
        "{:?}",
        ep.fixes
    );
    assert!(
        ep.fixes
            .iter()
            .any(|f| f.starts_with("lint step timed out → resolved"))
    );
    assert_eq!(ep.files, vec!["src/upload/retry.rs"]);
    assert_eq!(ep.duration_secs, 90);
    assert!(
        (ep.cost_usd - 0.25).abs() < 1e-9,
        "run cost wins over summed usage"
    );

    // idempotent per run: same id, one row
    let again = w.env.mem.distill(&w.run, Some(&w.task)).await.unwrap();
    assert_eq!(again.id, ep.id);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM episodes")
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);

    // L1 is searchable by its owner only
    let q = MemoryQuery {
        q: "retry uploads backoff".into(),
        kind: Some(MemoryType::Episode),
        ..Default::default()
    };
    let hits = w.env.mem.search(w.a, &q).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item.kind, MemoryType::Episode);
    assert_eq!(hits[0].item.id.0, ep.id.0);
    assert!(w.env.mem.get(w.a, hits[0].item.id).await.is_ok());
    assert!(w.env.mem.search(w.b, &q).await.unwrap().is_empty());
    assert!(
        w.env
            .mem
            .get(w.b, hits[0].item.id)
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert_eq!(w.env.mem.episodes(w.a, 10).await.unwrap().len(), 1);
    assert!(w.env.mem.episodes(w.b, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn distill_without_task_or_events_uses_fallbacks() {
    let w = world().await;
    let bare = add_run(&w.env.pool, w.a, None, WorkerRole::Qa).await;
    let ep = w.env.mem.distill(&bare, None).await.unwrap();
    assert_eq!(ep.goal, "QA run");
    assert_eq!(ep.outcome, "succeeded");
    assert!(ep.errors.is_empty() && ep.fixes.is_empty() && ep.files.is_empty());
}

#[tokio::test]
async fn heuristic_extract_promotes_procedures_and_conventions() {
    let w = world().await;
    let ep = w.env.mem.distill(&w.run, Some(&w.task)).await.unwrap();
    let items = w.env.mem.extract(&ep, None).await.unwrap();
    let kinds: Vec<(MemoryType, &str)> = items
        .iter()
        .map(|i| (i.kind, i.relation.as_str()))
        .collect();
    assert!(
        kinds.contains(&(MemoryType::Procedure, "fixed_by")),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&(MemoryType::Convention, "runs_tests_with")),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&(MemoryType::Convention, "lints_with")),
        "{kinds:?}"
    );
    let tests = items
        .iter()
        .find(|i| i.relation == "runs_tests_with")
        .unwrap();
    assert_eq!(tests.title, "FP runs tests with `cargo test`");
    assert_eq!(tests.object, "`cargo test`");
    for i in &items {
        assert_eq!(i.owner_id, w.a);
        assert_eq!(
            i.visibility,
            Visibility::Private,
            "owner default is private"
        );
        assert_eq!(i.status, MemoryStatus::Active);
        assert_eq!(i.project_id, Some(w.project));
        assert_eq!(i.source_run_id, Some(w.run.id));
        assert_eq!(i.source_label.as_deref(), Some("FP-142 · Builder"));
        assert!(i.salience >= 0.35, "promoted items pass the threshold");
    }
    let fix = items
        .iter()
        .find(|i| i.kind == MemoryType::Procedure)
        .unwrap();
    assert!(fix.subject.contains("error[E0425]"));
    assert!(fix.object.contains("src/upload/retry.rs"));

    // Re-extracting the same episode merges, it doesn't duplicate.
    let before = w
        .env
        .mem
        .list(w.a, &MemoryQuery::default())
        .await
        .unwrap()
        .total;
    w.env.mem.extract(&ep, None).await.unwrap();
    let after = w
        .env
        .mem
        .list(w.a, &MemoryQuery::default())
        .await
        .unwrap()
        .total;
    assert_eq!(before, after);

    // Packs were rebuilt with the new knowledge.
    let pack = w
        .env
        .mem
        .pack(w.a, w.project, WorkerRole::Builder)
        .await
        .unwrap();
    assert!(pack.contains("FP runs tests with `cargo test`"), "{pack}");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_packs WHERE owner_id = ?")
        .bind(w.a.to_string())
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
    assert_eq!(stored, 4, "planner, builder, qa, reviewer");
    // Ben sees none of Ada's private knowledge.
    let ben = w
        .env
        .mem
        .pack(w.b, w.project, WorkerRole::Builder)
        .await
        .unwrap();
    assert!(!ben.contains("cargo test"));
}

#[tokio::test]
async fn extract_uses_the_owners_default_visibility() {
    let w = world_with(
        MemoryConfig::default(),
        Some(serde_json::json!({"default_visibility": "company"})),
    )
    .await;
    let ep = w.env.mem.distill(&w.run, Some(&w.task)).await.unwrap();
    let items = w.env.mem.extract(&ep, None).await.unwrap();
    assert!(!items.is_empty());
    for i in &items {
        assert_eq!(i.status, MemoryStatus::Pending);
        assert_eq!(i.visibility, Visibility::Private);
        assert_eq!(i.requested_visibility, Some(Visibility::Company));
    }
    let queue = w
        .env
        .mem
        .list(
            w.a,
            &MemoryQuery {
                pending_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(queue.items.len(), items.len());

    // "Shared" default means the project's other task owners.
    let w2 = world_with(
        MemoryConfig::default(),
        Some(serde_json::json!({"default_visibility": "shared"})),
    )
    .await;
    add_task(&w2.env.pool, w2.b, w2.project, "FP-143", "other work").await;
    let ep = w2.env.mem.distill(&w2.run, Some(&w2.task)).await.unwrap();
    let items = w2.env.mem.extract(&ep, None).await.unwrap();
    assert!(
        items
            .iter()
            .all(|i| i.requested_visibility == Some(Visibility::Shared)
                && i.shared_with == vec![w2.b])
    );
}

#[tokio::test]
async fn llm_extraction_is_tolerant_and_salience_gated() {
    let w = world().await;
    let ep = w.env.mem.distill(&w.run, Some(&w.task)).await.unwrap();
    let llm = r#"Sure! Here are the memories:
```json
[
  {"kind": "decision", "title": "Uploads retry 5 times", "subject": "upload client", "relation": "retries", "object": "5 times with jitter", "body": "Chosen in FP-142.", "visibility": "company"},
  {"kind": "preference", "title": "Ada likes small PRs", "subject": "Ada", "relation": "prefers", "object": "small PRs", "visibility": "company"},
  {"kind": "fact", "subject": "", "relation": "is", "object": "broken entry"},
  {"nonsense": true}
]
```"#;
    let items = w.env.mem.extract(&ep, Some(llm)).await.unwrap();
    assert_eq!(items.len(), 2, "invalid entries are skipped: {items:?}");
    let pref = items
        .iter()
        .find(|i| i.kind == MemoryType::Preference)
        .unwrap();
    assert_eq!(
        (pref.visibility, pref.status),
        (Visibility::Private, MemoryStatus::Active)
    );
    let dec = items
        .iter()
        .find(|i| i.kind == MemoryType::Decision)
        .unwrap();
    assert_eq!(
        dec.visibility,
        Visibility::Private,
        "capped at the owner's (private) default"
    );

    // A failed run re-stating known knowledge scores low and is not promoted.
    let failed = add_run(&w.env.pool, w.a, Some(w.task.id), WorkerRole::Qa).await;
    w.env
        .mem
        .ingest_l0(
            &failed,
            Some(w.project),
            &[ev(
                failed.id,
                1,
                Utc::now(),
                RunEventKind::Done {
                    success: false,
                    summary: "QA failed".into(),
                },
            )],
            &no_redact,
        )
        .await
        .unwrap();
    let ep2 = w.env.mem.distill(&failed, Some(&w.task)).await.unwrap();
    assert!(ep2.outcome.starts_with("failed"));
    let restated = r#"[{"kind": "decision", "title": "Uploads retry 5 times", "subject": "upload client", "relation": "retries", "object": "5 times with jitter", "body": "Chosen in FP-142.", "visibility": "private"}]"#;
    let promoted = w.env.mem.extract(&ep2, Some(restated)).await.unwrap();
    assert!(promoted.is_empty(), "salience below threshold stays in L1");
}

#[tokio::test]
async fn packs_are_role_specific_and_budgeted() {
    let env = env_with(MemoryConfig {
        pack_token_budget: 300,
        ..Default::default()
    })
    .await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let p = add_project(&env.pool, "FP").await;
    let mk = |kind, s: &str, r: &str, o: &str| {
        let mut pr = prop(kind, s, r, o, Visibility::Private);
        pr.project_id = Some(p);
        pr
    };
    env.mem
        .propose(
            a,
            None,
            mk(MemoryType::Convention, "FP", "formats_with", "rustfmt"),
        )
        .await
        .unwrap();
    env.mem
        .propose(
            a,
            None,
            mk(
                MemoryType::Procedure,
                "flaky e2e",
                "fixed_by",
                "reseeding the db",
            ),
        )
        .await
        .unwrap();
    env.mem
        .propose(
            a,
            None,
            mk(MemoryType::Decision, "FP", "chose", "sqlite over postgres"),
        )
        .await
        .unwrap();
    env.mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "company",
                "ships",
                "on fridays",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let builder = env.mem.pack(a, p, WorkerRole::Builder).await.unwrap();
    let qa = env.mem.pack(a, p, WorkerRole::Qa).await.unwrap();
    let reviewer = env.mem.pack(a, p, WorkerRole::Reviewer).await.unwrap();
    let pos = |s: &str, h: &str| s.find(h).unwrap_or(usize::MAX);
    assert!(builder.starts_with("# Company brain · FP · Builder"));
    assert!(pos(&builder, "## Conventions") < pos(&builder, "## Procedures"));
    assert!(pos(&qa, "## Procedures") < pos(&qa, "## Conventions"));
    assert!(pos(&reviewer, "## Conventions") < pos(&reviewer, "## Decisions"));
    assert!(pos(&reviewer, "## Decisions") < pos(&reviewer, "## Procedures"));
    assert!(
        builder.contains("on fridays"),
        "company-wide (project-less) items are included"
    );

    // Budget: many items, pack stays within ≈ budget × 4 chars.
    for i in 0..60 {
        env.mem
            .propose(
                a,
                None,
                mk(
                    MemoryType::Fact,
                    &format!("module{i}"),
                    "owned_by",
                    &format!("team number {i} with a long name"),
                ),
            )
            .await
            .unwrap();
    }
    let (text, ids) = env
        .mem
        .pack_with_items(a, p, WorkerRole::Builder)
        .await
        .unwrap();
    assert!(
        text.chars().count() <= 300 * 4 + 64,
        "pack is {} chars",
        text.chars().count()
    );
    assert!(ids.len() < 64);
    // A new visible item invalidates the cached pack.
    env.mem
        .propose(
            a,
            None,
            mk(
                MemoryType::Convention,
                "FP",
                "names_branches",
                "kiln/<key>-<slug>",
            ),
        )
        .await
        .unwrap();
    let (text2, _) = env
        .mem
        .pack_with_items(a, p, WorkerRole::Builder)
        .await
        .unwrap();
    assert!(text2.contains("names branches"), "cache was refreshed");
}

#[tokio::test]
async fn nightly_merges_duplicates_and_flags_stale_items() {
    let env = env().await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let p = add_project(&env.pool, "FP").await;
    // Two identical items (as if proposed concurrently): bypass dedupe by
    // inserting the second under a different relation, then renaming it.
    let mut pr = prop(
        MemoryType::Convention,
        "FP",
        "formats_with",
        "rustfmt",
        Visibility::Private,
    );
    pr.project_id = Some(p);
    let keep = env.mem.propose(a, None, pr.clone()).await.unwrap();
    let mut pr2 = pr.clone();
    pr2.relation = "tmp".into();
    let dup = env.mem.propose(a, None, pr2).await.unwrap();
    sqlx::query("UPDATE memory_items SET relation = 'formats_with', salience = 0.3 WHERE id = ?")
        .bind(dup.id.to_string())
        .execute(&env.pool)
        .await
        .unwrap();
    let mut gone = prop(
        MemoryType::Procedure,
        "uploads",
        "fixed_by",
        "editing src/upload/old_retry.rs",
        Visibility::Private,
    );
    gone.project_id = Some(p);
    let gone = env.mem.propose(a, None, gone).await.unwrap();
    let mut here = prop(
        MemoryType::Procedure,
        "uploads",
        "configured_in",
        "src/upload/config.rs",
        Visibility::Private,
    );
    here.project_id = Some(p);
    let here = env.mem.propose(a, None, here).await.unwrap();

    let exists = |_: kiln_types::ProjectId, path: &str| path != "src/upload/old_retry.rs";
    let rep = env
        .mem
        .nightly(&NightlyOptions::default(), &exists)
        .await
        .unwrap();
    assert_eq!(rep.duplicates_merged, 1);
    assert_eq!(rep.flagged_missing_files, 1);
    assert_eq!(rep.flagged_unused, 0);
    let st = |id| {
        let env = &env;
        async move { env.mem.get(a, id).await.unwrap().item.status }
    };
    assert_eq!(st(keep.id).await, MemoryStatus::Active);
    assert_eq!(st(dup.id).await, MemoryStatus::Superseded);
    assert_eq!(st(gone.id).await, MemoryStatus::Flagged);
    assert_eq!(st(here.id).await, MemoryStatus::Active);
    let link: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memory_links WHERE from_id = ? AND to_id = ? AND kind = 'duplicate_of'")
        .bind(dup.id.to_string())
        .bind(keep.id.to_string())
        .fetch_one(&env.pool)
        .await
        .unwrap();
    assert_eq!(link, 1);

    // 61 days later nothing was used: stale.
    let later = NightlyOptions {
        now: Some(Utc::now() + Duration::days(61)),
        ..Default::default()
    };
    let rep = env.mem.nightly(&later, &|_, _| true).await.unwrap();
    assert_eq!(rep.flagged_unused, 2, "keep + here");
    assert_eq!(st(keep.id).await, MemoryStatus::Flagged);
}

#[tokio::test]
async fn nightly_expires_low_salience_old_episodes() {
    let w = world().await;
    let ep = w.env.mem.distill(&w.run, Some(&w.task)).await.unwrap();
    let old = kiln_types::Timestamp::from(Utc::now() - Duration::days(100));
    sqlx::query("UPDATE episodes SET salience = 0.1, created_at = ? WHERE id = ?")
        .bind(ts(old))
        .bind(ep.id.to_string())
        .execute(&w.env.pool)
        .await
        .unwrap();
    let rep = w
        .env
        .mem
        .nightly(&NightlyOptions::default(), &|_, _| true)
        .await
        .unwrap();
    assert_eq!(rep.episodes_expired, 1);
    let q = MemoryQuery {
        q: "retry uploads".into(),
        kind: Some(MemoryType::Episode),
        ..Default::default()
    };
    assert!(w.env.mem.search(w.a, &q).await.unwrap().is_empty());
    assert_eq!(w.env.mem.text_index().count().await.unwrap(), 0);
}

#[tokio::test]
async fn mcp_helpers_return_compact_json() {
    let w = world().await;
    let mut p = prop(
        MemoryType::Convention,
        "FP",
        "runs_e2e_with",
        "`pnpm playwright test`",
        Visibility::Company,
    );
    p.body = "Needs the stack booted first.".into();
    let v = w
        .env
        .mem
        .mcp_propose(w.a, Some(w.run.id), p.clone())
        .await
        .unwrap();
    assert_eq!(v["action"], "created");
    assert_eq!(v["status"], "pending");
    assert_eq!(v["visibility"], "private");
    assert_eq!(v["requested_visibility"], "company");
    assert!(v["message"].as_str().unwrap().contains("approval"));
    let id: kiln_types::MemoryId = v["id"].as_str().unwrap().parse().unwrap();
    let item = w.env.mem.get(w.a, id).await.unwrap().item;
    assert_eq!(
        item.project_id,
        Some(w.project),
        "project taken from the run's task"
    );
    let again = w.env.mem.mcp_propose(w.a, Some(w.run.id), p).await.unwrap();
    assert_eq!(again["action"], "merged");
    assert_eq!(again["id"], v["id"]);

    // Ben's run can't attribute a proposal to Ada's run.
    let e = w
        .env
        .mem
        .mcp_propose(
            w.b,
            Some(w.run.id),
            prop(MemoryType::Fact, "x", "is", "y", Visibility::Private),
        )
        .await
        .unwrap_err();
    assert!(e.is_forbidden());

    w.env
        .mem
        .decide(
            w.a,
            id,
            ApproveMemoryRequest {
                decision: MemoryDecision::Approve,
                edit: None,
            },
        )
        .await
        .unwrap();
    let ben_run = add_run(&w.env.pool, w.b, None, WorkerRole::Qa).await;
    let ctx = UseContext {
        run: Some(ben_run.id),
        task: None,
        role: Some(WorkerRole::Qa),
    };
    let res = w
        .env
        .mem
        .mcp_search(w.b, Some(w.project), "e2e playwright", Some(3), &ctx)
        .await
        .unwrap();
    assert_eq!(res["count"], 1);
    let r = &res["results"][0];
    assert_eq!(r["fact"], "FP runs e2e with `pnpm playwright test`");
    assert_eq!(r["kind"], "convention");
    assert_eq!(r["visibility"], "company");
    assert!(r["score"].as_f64().unwrap() > 0.0);
    // the use is attributed to Ben's run, so feedback credits it
    let rep = w.env.mem.feedback(ben_run.id, true).await.unwrap();
    assert_eq!(rep.boosted, 1);
    let out = w
        .env
        .mem
        .propose_detailed(
            w.b,
            None,
            prop(
                MemoryType::Convention,
                "FP",
                "runs_e2e_with",
                "`pnpm playwright test`",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        out.action,
        ProposeAction::Merged,
        "Ben's duplicate bumps the company item"
    );
}
