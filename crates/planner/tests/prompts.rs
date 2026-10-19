//! Every prompt carries the facts its role needs.

use chrono::Utc;
use kiln_planner::*;
use kiln_types::event::{LogLevel, RunEvent, RunEventKind};
use kiln_types::memory::{Episode, MemoryProposal, MemoryType, Visibility};
use kiln_types::plan::PlanSpec;
use kiln_types::protocol::paths;
use kiln_types::qa::*;
use kiln_types::*;
use std::collections::BTreeMap;

fn task() -> Task {
    Task {
        id: TaskId::new(),
        plan_id: PlanId::new(),
        project_id: ProjectId::new(),
        owner_id: UserId::new(),
        number: 142,
        key: "FP-142".into(),
        spec_id: "t1".into(),
        title: "Retry webhook deliveries with backoff".into(),
        description: "Wrap the sender in src/webhook/send.rs with exponential backoff.".into(),
        acceptance: vec![
            "Failed deliveries are retried 5 times with doubling delay".into(),
            "The delay never exceeds 60 seconds".into(),
            "After the last failure an ERROR log line names the endpoint".into(),
        ],
        files_hint: vec!["src/webhook/send.rs".into(), "src/webhook/retry.rs".into()],
        depends_on: vec![],
        workers: BTreeMap::new(),
        state: TaskState::Building,
        last_active: None,
        loop_count: 0,
        loop_cap: 3,
        branch: Some("kiln/fp-142-retry-webhook-deliveries-with-backoff".into()),
        pr_url: None,
        pr_number: None,
        progress: 0,
        meta: String::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn project() -> Project {
    Project {
        id: ProjectId::new(),
        key: "FP".into(),
        name: "Acme".into(),
        repo_url: "https://x-access-token:SECRET@github.com/acme/acme.git".into(),
        default_branch: "main".into(),
        qa_path: "qa.yaml".into(),
        qa_status: QaProfileStatus::Valid,
        image: None,
        resources: ResourceProfile::default(),
        loop_cap: 3,
        created_at: Utc::now(),
    }
}

fn msg(author: MessageAuthor, body: &str) -> Message {
    Message {
        id: MessageId::new(),
        thread_id: ThreadId::new(),
        author,
        body: body.into(),
        voice: false,
        plan_version: None,
        created_at: Utc::now(),
    }
}

fn failing_qa() -> QaReport {
    QaReport {
        criteria: vec![
            CriterionResult {
                criterion: "Failed deliveries are retried 5 times with doubling delay".into(),
                status: PassFail::Pass,
                evidence: vec!["artifacts/1-retries.log".into()],
                notes: "Observed 5 retries at 1,2,4,8,16s".into(),
            },
            CriterionResult {
                criterion: "The delay never exceeds 60 seconds".into(),
                status: PassFail::Fail,
                evidence: vec!["artifacts/2-delay.log".into()],
                notes: "Attempt 7 waited 64s (expected ≤ 60s)".into(),
            },
        ],
        tests: TestSummary {
            passed: 10,
            failed: 1,
            failing_assertion: Some("assert!(delay <= 60s)".into()),
            ..Default::default()
        },
        summary: "Cap missing.".into(),
        test_branch: None,
    }
}

fn verdict() -> ReviewVerdict {
    ReviewVerdict {
        decision: Decision::ChangesRequested,
        findings: vec![
            Finding {
                severity: Severity::Minor,
                file: None,
                line: None,
                message: "Rename `n` to `attempt`".into(),
            },
            Finding {
                severity: Severity::Major,
                file: Some("src/webhook/retry.rs".into()),
                line: Some(57),
                message: "Delay is not clamped".into(),
            },
        ],
        summary: "Needs a cap.".into(),
    }
}

fn assert_all(hay: &str, needles: &[&str]) {
    for n in needles {
        assert!(hay.contains(n), "missing {n:?} in:\n{hay}");
    }
}

#[test]
fn planner_system_has_rules_schema_and_outputs() {
    let s = planner_system();
    assert_all(
        &s,
        &[
            paths::PLAN,
            paths::WORKSPACE,
            "read-only",
            "search_memory",
            "acceptance criteria",
            "files_hint",
            "depends_on",
            "Reply to the user conversationally",
            "\"$schema\"",
            "\"acceptance_criteria\"",
            "Export runs as CSV",
        ],
    );
    assert!(s.contains(plan_json_schema()), "schema embedded verbatim");
    assert_eq!(role_system(WorkerRole::Planner), s);
}

#[test]
fn planner_prompt_includes_project_history_memory_and_draft() {
    let history = vec![
        msg(
            MessageAuthor::User,
            "Webhooks get lost when the receiver is down.",
        ),
        msg(MessageAuthor::Planner, "Should retries be capped?"),
        msg(MessageAuthor::User, "Yes, at 60 seconds."),
    ];
    let plan: PlanSpec = serde_json::from_str(EXAMPLE_PLAN).unwrap();
    let p = planner_prompt(
        &project(),
        &history,
        "- convention: use tracing",
        Some(&plan),
    );
    assert_all(
        &p,
        &[
            "Acme",
            "github.com/acme/acme.git",
            "`main`",
            "- convention: use tracing",
            "Webhooks get lost when the receiver is down.",
            "Yes, at 60 seconds.",
            "**Planner (you)**",
            "Add GET /api/runs/export.csv endpoint",
            "Keep the ids of unchanged tasks",
            paths::PLAN,
        ],
    );
    assert!(
        !p.contains("SECRET"),
        "credentials in repo URLs never reach the model"
    );
    // The latest message comes after the draft.
    assert!(p.find("Yes, at 60 seconds.").unwrap() > p.find("export.csv").unwrap());

    let none = planner_prompt(&project(), &[], "", None);
    assert_all(&none, &["No plan yet.", "No memory items yet"]);
}

#[test]
fn planner_prompt_caps_history() {
    let history: Vec<Message> = (0..MAX_HISTORY_MESSAGES + 5)
        .map(|i| msg(MessageAuthor::User, &format!("message number {i:03}")))
        .collect();
    let p = planner_prompt(&project(), &history, "", None);
    assert!(p.contains("5 earlier messages omitted"));
    assert!(!p.contains("message number 004"));
    assert!(p.contains("message number 005"));
    assert!(p.contains(&format!("message number {:03}", MAX_HISTORY_MESSAGES + 4)));
}

#[test]
fn builder_prompt_first_attempt() {
    let t = task();
    let p = builder_prompt(
        &t,
        "Reliable webhooks",
        1,
        3,
        &[],
        "- procedure: run cargo test -p webhook",
    );
    assert_all(
        &p,
        &[
            "FP-142: Retry webhook deliveries with backoff",
            "Attempt **1 of 3**",
            "kiln/fp-142-retry-webhook-deliveries-with-backoff",
            "Reliable webhooks",
            "src/webhook/send.rs",
            "- procedure: run cargo test -p webhook",
            paths::SUMMARY,
            "FP-142: <imperative summary>",
            "Keep the scope tight",
            "Do not push",
        ],
    );
    for c in &t.acceptance {
        assert!(p.contains(c.as_str()), "missing criterion {c}");
    }
    assert!(!p.contains("feedback from the previous attempt"));
    assert_eq!(role_system(WorkerRole::Builder), builder_system());
}

#[test]
fn builder_prompt_loop_shows_feedback_first() {
    let t = task();
    let fb = feedback_from(
        Some(&failing_qa()),
        Some(&verdict()),
        Some("Use the existing Backoff helper."),
    );
    let p = builder_prompt(&t, "Reliable webhooks", 3, 3, std::slice::from_ref(&fb), "");
    assert_all(
        &p,
        &[
            "Attempt **3 of 3**",
            "Fix this first",
            "Attempt 7 waited 64s",
            "Delay is not clamped",
            "Use the existing Backoff helper.",
            "last attempt",
            "earlier attempts are still on this branch",
        ],
    );
    let fb_at = p.find("Attempt 7 waited 64s").unwrap();
    assert!(
        fb_at < p.find("## What to build").unwrap(),
        "feedback precedes the task body"
    );
    assert!(fb_at < p.find("## Acceptance criteria").unwrap());

    // Several feedback items are numbered; blank ones are dropped.
    let p = builder_prompt(
        &t,
        "f",
        2,
        3,
        &["first note".into(), "  ".into(), "second note".into()],
        "",
    );
    assert_all(
        &p,
        &[
            "### Feedback 1",
            "first note",
            "### Feedback 2",
            "second note",
        ],
    );
    assert!(!p.contains("### Feedback 3"));
}

#[test]
fn qa_prompt_follows_profile_and_shape() {
    let t = task();
    let mut services = BTreeMap::new();
    services.insert(
        "db".to_string(),
        QaService {
            image: "postgres:16".into(),
            env: BTreeMap::new(),
            ports: vec![5432],
            command: None,
            healthcheck: None,
        },
    );
    let profile = QaProfile {
        version: 1,
        image: None,
        extras: Default::default(),
        resources: Default::default(),
        services,
        boot: vec!["docker compose up -d app".into()],
        seed: vec!["./scripts/seed.sh".into()],
        checks: QaChecks {
            unit: Some("cargo test --workspace".into()),
            e2e: Some("npx playwright test".into()),
            cli: Some("./qa/cli-scenarios.sh".into()),
            extra: vec!["lighthouse".into()],
        },
        artifacts: vec!["playwright-report/**".into()],
        loop_cap: Some(3),
        timeouts: BTreeMap::new(),
    };
    let p = qa_prompt(
        &t,
        Some(&profile),
        "Added retry.rs with Backoff; run `cargo test -p webhook`.",
    );
    assert_all(
        &p,
        &[
            "QA for FP-142",
            "docker compose up -d app",
            "./scripts/seed.sh",
            "cargo test --workspace",
            "npx playwright test",
            "./qa/cli-scenarios.sh",
            "`lighthouse`",
            "`db` — image `postgres:16`, ports 5432",
            "playwright-report/**",
            "Added retry.rs with Backoff",
            "Playwright",
            "real terminal session",
            "scratch branch",
            "kiln/fp-142-retry-webhook-deliveries-with-backoff-qa",
            paths::ARTIFACTS_DIR,
            paths::QA_REPORT,
            "\"failing_assertion\"",
        ],
    );
    for c in &t.acceptance {
        assert!(p.contains(c.as_str()), "missing criterion {c}");
    }
    assert!(p.contains(QA_REPORT_EXAMPLE));

    let bare = qa_prompt(&t, None, "");
    assert_all(
        &bare,
        &["no usable `qa.yaml`", "The Builder left no summary."],
    );
    assert_eq!(role_system(WorkerRole::Qa), qa_system());
}

#[test]
fn reviewer_prompt_has_diff_qa_conventions_and_output() {
    let t = task();
    let diff = DiffSummary {
        additions: 120,
        deletions: 8,
        files: vec![
            DiffFile {
                path: "src/webhook/retry.rs".into(),
                additions: 100,
                deletions: 0,
            },
            DiffFile {
                path: "src/webhook/send.rs".into(),
                additions: 20,
                deletions: 8,
            },
        ],
    };
    let p = reviewer_prompt(
        &t,
        "main",
        &diff,
        Some(&failing_qa()),
        "- convention: errors via thiserror",
    );
    assert_all(
        &p,
        &[
            "Review FP-142",
            "+120 −8 across 2 file(s)",
            "`src/webhook/retry.rs` (+100 −0)",
            "git diff origin/main...HEAD",
            "[FAIL] The delay never exceeds 60 seconds — Attempt 7 waited 64s",
            "- convention: errors via thiserror",
            "changes_requested",
            paths::VERDICT,
            "read-only",
        ],
    );
    for c in &t.acceptance {
        assert!(p.contains(c.as_str()), "missing criterion {c}");
    }
    assert!(p.contains(VERDICT_EXAMPLE));
    let no_qa = reviewer_prompt(&t, "develop", &DiffSummary::default(), None, "");
    assert_all(
        &no_qa,
        &[
            "QA produced no report",
            "origin/develop",
            "No recorded conventions",
        ],
    );
    assert_eq!(role_system(WorkerRole::Reviewer), reviewer_system());
}

#[test]
fn distiller_prompt_has_episode_events_and_rules() {
    let run = RunId::new();
    let project_id = ProjectId::new();
    let ep = Episode {
        id: EpisodeId::new(),
        run_id: run,
        task_id: None,
        owner_id: UserId::new(),
        project_id: Some(project_id),
        role: WorkerRole::Builder,
        goal: "FP-142 retry webhooks".into(),
        outcome: "passed after 2 loops".into(),
        errors: vec!["cargo test failed: missing DATABASE_URL".into()],
        fixes: vec!["export DATABASE_URL=postgres://db/test".into()],
        files: vec!["src/webhook/retry.rs".into()],
        duration_secs: 420,
        cost_usd: 1.25,
        visibility: Visibility::Company,
        created_at: Utc::now(),
    };
    let ev = |seq, kind| RunEvent {
        run_id: run,
        seq,
        ts: Utc::now(),
        kind,
    };
    let events = vec![
        ev(
            1,
            RunEventKind::Step {
                title: "run unit tests".into(),
            },
        ),
        ev(
            2,
            RunEventKind::Command {
                command: "cargo test".into(),
                exit_code: Some(101),
                output: String::new(),
            },
        ),
        ev(
            3,
            RunEventKind::Log {
                level: LogLevel::Info,
                message: "x".repeat(MAX_EVENT_CHARS * 2),
            },
        ),
    ];
    let p = distiller_prompt(&ep, &events);
    assert_all(
        &p,
        &[
            "FP-142 retry webhooks",
            "passed after 2 loops",
            "missing DATABASE_URL",
            "export DATABASE_URL=postgres://db/test",
            "[STEP] run unit tests",
            "$ cargo test → exit 101",
            "Always `\"visibility\": \"private\"`",
            "Transient problems",
            "Default visibility: `\"company\"`",
            &project_id.to_string(),
            MEMORY_OUT,
            "\"subject\"",
            "\"relation\"",
            "\"object\"",
        ],
    );
    assert!(
        !p.contains(&"x".repeat(MAX_EVENT_CHARS)),
        "long events are truncated"
    );
    assert!(p.contains(MEMORY_EXAMPLE));

    let shared = Episode {
        visibility: Visibility::Shared,
        project_id: None,
        ..ep
    };
    let p = distiller_prompt(&shared, &[]);
    assert_all(
        &p,
        &[
            "Default visibility: `\"private\"`",
            "`null` for knowledge",
            "No events sampled",
        ],
    );
}

#[test]
fn examples_deserialize_into_contract_types() {
    let r: QaReport = serde_json::from_str(QA_REPORT_EXAMPLE).unwrap();
    assert_eq!(r.criteria.len(), 2);
    assert!(!r.passed());
    let v: ReviewVerdict = serde_json::from_str(VERDICT_EXAMPLE).unwrap();
    assert!(!v.approved());
    let m: Vec<MemoryProposal> = serde_json::from_str(MEMORY_EXAMPLE).unwrap();
    assert_eq!(m.len(), 3);
    for p in &m {
        if p.kind == MemoryType::Preference {
            assert_eq!(p.visibility, Visibility::Private);
        }
    }
}

#[test]
fn feedback_formats_failures_findings_and_human() {
    let fb = feedback_from(
        Some(&failing_qa()),
        Some(&verdict()),
        Some("  Please keep the public API unchanged.  "),
    );
    assert_all(
        &fb,
        &[
            "## QA failed",
            "✕ **The delay never exceeds 60 seconds**",
            "Attempt 7 waited 64s",
            "`artifacts/2-delay.log`",
            "1 failed, 10 passed",
            "assert!(delay <= 60s)",
            "## Reviewer requested changes",
            "[major] `src/webhook/retry.rs:57` — Delay is not clamped",
            "[minor] — Rename",
            "## Note from a human",
            "Please keep the public API unchanged.",
        ],
    );
    assert!(
        !fb.contains("retried 5 times"),
        "passing criteria are not feedback"
    );
    assert!(
        fb.find("[major]").unwrap() < fb.find("[minor]").unwrap(),
        "sorted by severity"
    );

    // All green → nothing to say.
    let mut ok = failing_qa();
    ok.criteria[1].status = PassFail::Pass;
    ok.tests.failed = 0;
    let approve = ReviewVerdict {
        decision: Decision::Approve,
        findings: vec![Finding {
            severity: Severity::Minor,
            file: None,
            line: None,
            message: "nit".into(),
        }],
        summary: "LGTM".into(),
    };
    assert_eq!(feedback_from(Some(&ok), Some(&approve), None), "");
    assert_eq!(feedback_from(None, None, Some("   ")), "");
    assert_eq!(
        feedback_from(None, None, Some("redo it")),
        "## Note from a human\n\nredo it"
    );
}

#[test]
fn memory_file_has_generated_header_and_pack() {
    let t = task();
    let f = memory_file(
        "### Conventions\n- errors via thiserror",
        WorkerRole::Builder,
        Some(&t),
    );
    assert!(f.starts_with("<!-- Generated by Kiln for Builder on FP-142"));
    assert_all(
        &f,
        &[
            "Do not edit or commit",
            "# Kiln worker context",
            "- errors via thiserror",
            "search_memory",
            "propose_memory",
        ],
    );
    let empty = memory_file("", WorkerRole::Reviewer, None);
    assert_all(
        &empty,
        &["Generated by Kiln for Reviewer.", "No memory items yet"],
    );
}
