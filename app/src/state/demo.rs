//! DEMO data source: the prototype's mock data
//! as real `kiln_types` values, plus a timer-driven simulation that produces
//! the same `ServerEvent`s the daemon would. Enabled with `?demo=1` or the
//! "Try demo" button when no daemon is reachable.
//!
//! Everything here is pure: `seed` builds a `Data`, `DemoSim::step` and the
//! `act_*` functions return events that go through `Data::apply` exactly like
//! live WebSocket traffic.

// Seed tables mirror the prototype's literal arrays.
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use super::data::Data;
use super::derive::stage_index;
use chrono::{DateTime, Duration, Utc};
use kiln_types::api::*;
use kiln_types::event::{LogLevel, LogLine, ServerEvent};
use kiln_types::ids::*;
use kiln_types::memory::*;
use kiln_types::plan::*;
use kiln_types::qa::*;
use kiln_types::*;
use std::collections::BTreeMap;

pub const DOMAIN: &str = "company.dev";

pub const HARNESS_CLAUDE: &str = "claude_sdk";
pub const HARNESS_CODEX: &str = "codex";
pub const HARNESS_OPENCODE: &str = "opencode";

pub fn w0() -> BTreeMap<WorkerRole, WorkerChoice> {
    BTreeMap::from([
        (
            WorkerRole::Builder,
            WorkerChoice::new(HARNESS_CLAUDE, "claude-sonnet-4.5"),
        ),
        (WorkerRole::Qa, WorkerChoice::new(HARNESS_CODEX, "gpt-5")),
        (
            WorkerRole::Reviewer,
            WorkerChoice::new(HARNESS_CLAUDE, "claude-opus-4.1"),
        ),
    ])
}

fn plan_workers(m: &BTreeMap<WorkerRole, WorkerChoice>) -> PlanWorkers {
    PlanWorkers {
        builder: m.get(&WorkerRole::Builder).cloned(),
        qa: m.get(&WorkerRole::Qa).cloned(),
        reviewer: m.get(&WorkerRole::Reviewer).cloned(),
    }
}

pub fn catalog() -> HarnessCatalog {
    HarnessCatalog {
        harnesses: vec![
            HarnessInfo {
                id: HARNESS_CLAUDE.into(),
                label: "Claude Agent SDK".into(),
                models: vec!["claude-sonnet-4.5".into(), "claude-opus-4.1".into()],
                experimental: false,
            },
            HarnessInfo {
                id: HARNESS_CODEX.into(),
                label: "Codex".into(),
                models: vec!["gpt-5".into(), "gpt-5-codex".into(), "qwen3-coder".into()],
                experimental: false,
            },
            HarnessInfo {
                id: HARNESS_OPENCODE.into(),
                label: "OpenCode".into(),
                models: vec!["qwen3-coder".into(), "gpt-5".into()],
                experimental: true,
            },
        ],
        defaults: w0(),
    }
}

/// (initials, name, email, role, parallel limit, spend, budget)
const PEOPLE: [(&str, &str, &str, UserRole, u32, f64, f64); 4] = [
    (
        "AR",
        "Alex Rivera",
        "alex@company.dev",
        UserRole::Admin,
        4,
        212.0,
        400.0,
    ),
    (
        "JL",
        "Jordan Lee",
        "jordan@company.dev",
        UserRole::Member,
        3,
        148.0,
        300.0,
    ),
    (
        "MK",
        "Morgan Kim",
        "morgan@company.dev",
        UserRole::Member,
        3,
        261.0,
        300.0,
    ),
    (
        "SD",
        "Sam Diaz",
        "sam@company.dev",
        UserRole::Admin,
        2,
        74.0,
        300.0,
    ),
];

/// (key, short, name)
const PROJECTS: [(&str, &str, &str); 3] = [
    ("FP", "CLI/SDK", "Acme CLI/SDK"),
    ("FPC", "Cloud", "Acme Cloud"),
    ("WEB", "Website", "Marketing website"),
];

/// The prototype's T0: (key, title, project short, owner, column, status, progress, attempt).
const T0: [(&str, &str, &str, &str, &str, &str, u8, u32); 15] = [
    (
        "FP-151",
        "Config file schema validation",
        "CLI/SDK",
        "MK",
        "Idea",
        "queued",
        0,
        1,
    ),
    (
        "FP-153",
        "Dark mode for docs site",
        "Website",
        "JL",
        "Idea",
        "queued",
        0,
        1,
    ),
    (
        "FP-148",
        "Org-level audit log export",
        "Cloud",
        "AR",
        "Planned",
        "queued",
        0,
        1,
    ),
    (
        "FP-149",
        "Streaming responses in SDK",
        "CLI/SDK",
        "SD",
        "Planned",
        "queued",
        0,
        1,
    ),
    (
        "FP-145",
        "Usage chart on billing page",
        "Cloud",
        "JL",
        "Building",
        "running",
        22,
        1,
    ),
    (
        "FP-146",
        "Pricing page comparison table",
        "Website",
        "MK",
        "Building",
        "running",
        64,
        1,
    ),
    (
        "FP-142",
        "Add retry policy to SDK client",
        "CLI/SDK",
        "AR",
        "QA",
        "running",
        48,
        2,
    ),
    (
        "FP-139",
        "Webhook signature verification",
        "Cloud",
        "SD",
        "QA",
        "failed",
        100,
        2,
    ),
    (
        "FP-137",
        "Interactive prompts for `init`",
        "CLI/SDK",
        "MK",
        "Review",
        "running",
        30,
        1,
    ),
    (
        "FP-136",
        "SSO login screen polish",
        "Cloud",
        "AR",
        "Review",
        "human",
        100,
        3,
    ),
    (
        "FP-133",
        "Blog index pagination",
        "Website",
        "JL",
        "Ready",
        "passed",
        100,
        1,
    ),
    (
        "FP-131",
        "Rate-limit headers in SDK",
        "CLI/SDK",
        "SD",
        "Ready",
        "passed",
        100,
        2,
    ),
    (
        "FP-128",
        "Project switcher keyboard nav",
        "Cloud",
        "AR",
        "Done",
        "passed",
        100,
        1,
    ),
    (
        "FP-127",
        "Changelog page",
        "Website",
        "MK",
        "Done",
        "passed",
        100,
        1,
    ),
    (
        "FP-125",
        "Typed errors for SDK",
        "CLI/SDK",
        "SD",
        "Done",
        "passed",
        100,
        1,
    ),
];

pub const LOG_BUILDER: [(LogLevel, &str); 8] = [
    (
        LogLevel::Info,
        "cloning repo into /work (worktree feat/retry-policy)",
    ),
    (
        LogLevel::Step,
        "→ reading plan T1 + 3 memories from company brain",
    ),
    (LogLevel::Info, "editing src/client/retry.ts"),
    (LogLevel::Info, "editing src/client/backoff.ts"),
    (LogLevel::Step, "→ running typecheck"),
    (LogLevel::Pass, "tsc --noEmit  0 errors"),
    (
        LogLevel::Info,
        "committing “feat(sdk): retry policy with jitter”",
    ),
    (LogLevel::Step, "→ handing off to QA"),
];
pub const LOG_QA: [(LogLevel, &str); 13] = [
    (LogLevel::Info, "booting app container fp-cli:qa"),
    (LogLevel::Info, "playwright: chromium launched (headless)"),
    (LogLevel::Step, "→ run `fp init --yes` in tmp workspace"),
    (LogLevel::Pass, "client/retry.test.ts  12 passed  0.8s"),
    (LogLevel::Pass, "client/backoff.test.ts  9 passed  0.4s"),
    (
        LogLevel::Warn,
        "flaky: network/timeout.test.ts retried once",
    ),
    (LogLevel::Step, "→ simulate 503 from mock upstream ×3"),
    (LogLevel::Info, "observed backoff 200ms → 400ms → 800ms"),
    (LogLevel::Capt, "screenshot 02-retry-banner.png"),
    (
        LogLevel::Fail,
        "e2e/settings.spec.ts  expected toast “Saved”",
    ),
    (LogLevel::Info, "recording session.webm 00:48"),
    (LogLevel::Step, "→ drive: settings → toggle retries → save"),
    (LogLevel::Pass, "client/errors.test.ts  14 passed  0.6s"),
];
pub const LOG_REVIEWER: [(LogLevel, &str); 6] = [
    (
        LogLevel::Info,
        "loading diff (7 files, +212 −48) against plan",
    ),
    (LogLevel::Step, "→ checking acceptance criteria 1–3"),
    (LogLevel::Pass, "AC-1 retries 5xx + network errors"),
    (LogLevel::Warn, "AC-2 backoff cap hard-coded at 30s"),
    (LogLevel::Step, "→ reading QA report"),
    (LogLevel::Info, "drafting verdict"),
];

pub fn log_pool(role: WorkerRole) -> &'static [(LogLevel, &'static str)] {
    match role {
        WorkerRole::Builder => &LOG_BUILDER,
        WorkerRole::Reviewer => &LOG_REVIEWER,
        _ => &LOG_QA,
    }
}

const AC: [&str; 3] = [
    "Retries 5xx and network errors up to maxRetries",
    "Backoff cap configurable via config (default 30s)",
    "Tests are deterministic with a seeded RNG",
];

const REPLIES: [&str; 3] = [
    "Updated. I tightened the acceptance criteria on T1 and added a check for Retry-After headers.",
    "Done. T2 now also covers --quiet mode. No change to the workers.",
    "Noted. I moved QA for T1 to gpt-5-codex, since it handles the Playwright flows better in past runs.",
];

pub const PENDING_LABEL: &str = "reading company brain · drafting changes";

/// Stage durations (passed) and elapsed (running) from the prototype.
const PASSED_SECS: [i64; 3] = [252, 400, 118];
const RUNNING_SECS: [i64; 3] = [185, 125, 48];

fn uid(n: u128) -> uuid::Uuid {
    // Stable ids keep tests readable; the high bits mark them as demo ids.
    uuid::Uuid::from_u128(0x6b11_0000_0000_7000_8000_0000_0000_0000 | n)
}

fn user_id(i: usize) -> UserId {
    UserId(uid(0x100 + i as u128))
}
fn project_id(i: usize) -> ProjectId {
    ProjectId(uid(0x200 + i as u128))
}

fn initials_to_user(d: &Data, i: &str) -> UserId {
    d.users
        .iter()
        .find(|u| u.initials == i)
        .map(|u| u.id)
        .unwrap_or_else(UserId::nil)
}

fn project_by_short(d: &Data, short: &str) -> ProjectId {
    let idx = PROJECTS.iter().position(|p| p.1 == short).unwrap_or(0);
    d.projects
        .get(idx)
        .map(|p| p.id)
        .unwrap_or_else(ProjectId::nil)
}

fn task_state(col: &str, status: &str) -> (TaskState, Option<TaskState>) {
    let active = match col {
        "Building" => TaskState::Building,
        "QA" => TaskState::Testing,
        "Review" => TaskState::Reviewing,
        "Ready" => TaskState::Ready,
        "Done" => TaskState::Merged,
        _ => TaskState::Queued,
    };
    match status {
        "failed" => (TaskState::Failed, Some(active)),
        "human" => (TaskState::Escalated, Some(active)),
        _ => (
            active,
            if active.is_active() {
                Some(active)
            } else {
                None
            },
        ),
    }
}

pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return format!("…{}", chars.iter().rev().take(2).rev().collect::<String>());
    }
    let head: String = chars.iter().take(7).collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn machine(d: &Data, cpu_history: &[f32]) -> MachineStats {
    let running = d.tasks.iter().filter(|t| t.state.is_active()).count() as u32;
    let ctr = (running + 1).min(8);
    let queued = d
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Queued)
        .count() as u32;
    let cpu = cpu_history.last().copied().unwrap_or(50.0);
    MachineStats {
        hostname: "mac-mini".into(),
        spec_line: "mac-mini · M4 Pro · 12 cores · 32 GB · uptime 18d".into(),
        cpu_percent: cpu,
        cpu_history: cpu_history.to_vec(),
        ram_used_gb: 8.6 + ctr as f32 * 2.1,
        ram_total_gb: 32.0,
        disk_used_gb: 412.0,
        disk_total_gb: 1000.0,
        containers_running: ctr,
        container_slots: 8,
        queue_depth: queued,
        est_wait_secs: 240,
        oldest_queued_secs: 120,
        uptime_secs: 18 * 86_400,
        runtime_ok: true,
        tailnet_name: Some("mac-mini.tailnet".into()),
        latency_ms: Some(18),
    }
}

fn log_line(run: RunId, role: WorkerRole, seq: u64, ts: DateTime<Utc>) -> LogLine {
    let pool = log_pool(role);
    let (level, msg) = pool[seq as usize % pool.len()];
    LogLine {
        run_id: run,
        seq,
        ts,
        role,
        level,
        message: msg.to_string(),
    }
}

fn make_run(
    task: &Task,
    role: WorkerRole,
    state: RunState,
    started: DateTime<Utc>,
    ended: Option<DateTime<Utc>>,
) -> Run {
    let w = task
        .workers
        .get(&role)
        .cloned()
        .unwrap_or_else(|| WorkerChoice::new(HARNESS_CLAUDE, "claude-sonnet-4.5"));
    Run {
        id: RunId::new(),
        task_id: Some(task.id),
        thread_id: None,
        owner_id: task.owner_id,
        role,
        harness: w.harness,
        model: w.model,
        attempt: task.attempt(),
        container_id: Some(format!(
            "kiln-{}-{}",
            task.key.to_lowercase(),
            role.as_str()
        )),
        state,
        progress: if state == RunState::Succeeded {
            100
        } else {
            task.progress
        },
        started_at: Some(started),
        ended_at: ended,
        ingested: ended.is_some(),
        cost_usd: 0.42,
        tokens_in: 48_000,
        tokens_out: 6_200,
        error: None,
        retries: 0,
    }
}

fn tests_summary(running: bool) -> TestSummary {
    TestSummary {
        passed: 41,
        failed: 1,
        skipped: 0,
        running: if running { 1 } else { 0 },
        suites: vec![
            TestSuite {
                name: "client/retry.test.ts".into(),
                passed: 12,
                failed: 0,
                duration_ms: 800,
            },
            TestSuite {
                name: "client/backoff.test.ts".into(),
                passed: 9,
                failed: 0,
                duration_ms: 400,
            },
            TestSuite {
                name: "client/errors.test.ts".into(),
                passed: 14,
                failed: 0,
                duration_ms: 600,
            },
            TestSuite {
                name: "network/timeout.test.ts".into(),
                passed: 6,
                failed: 0,
                duration_ms: 2100,
            },
            TestSuite {
                name: "e2e/settings.spec.ts".into(),
                passed: 0,
                failed: 1,
                duration_ms: 14_200,
            },
            TestSuite {
                name: "e2e/retry-banner.spec.ts".into(),
                passed: 0,
                failed: 0,
                duration_ms: 0,
            },
        ],
        failing_assertion: Some(
            "e2e/settings.spec.ts:42 — expected toast “Saved”, got none after 5000ms".into(),
        ),
    }
}

fn diff() -> DiffSummary {
    let f = |p: &str, a, d| DiffFile {
        path: p.into(),
        additions: a,
        deletions: d,
    };
    DiffSummary {
        additions: 212,
        deletions: 48,
        files: vec![
            f("src/client/retry.ts", 118, 6),
            f("src/client/backoff.ts", 46, 12),
            f("src/config/schema.ts", 21, 4),
            f("src/cli/output.ts", 0, 0),
            f("test/client/retry.test.ts", 27, 26),
            f("docs/sdk/retries.md", 0, 0),
            f("CHANGELOG.md", 0, 0),
        ],
    }
}

/// Build the detail (runs, artifacts, reports) for a task from its stage.
pub fn detail_for(d: &Data, t: &Task, now: DateTime<Utc>) -> TaskDetail {
    let idx = stage_index(t);
    let mut runs = Vec::new();
    if let Some(idx) = idx {
        let mut cursor = now;
        // Walk backwards: the current stage started RUNNING_SECS ago.
        let cur = idx.min(2);
        let mut starts = [now; 3];
        if idx < 3 {
            starts[cur] = now - Duration::seconds(RUNNING_SECS[cur]);
            cursor = starts[cur];
        }
        for i in (0..idx.min(3)).rev() {
            let s = cursor - Duration::seconds(PASSED_SECS[i]);
            starts[i] = s;
            cursor = s;
        }
        for (i, role) in WorkerRole::PIPELINE.into_iter().enumerate() {
            if i < idx {
                runs.push(make_run(
                    t,
                    role,
                    RunState::Succeeded,
                    starts[i],
                    Some(starts[i] + Duration::seconds(PASSED_SECS[i])),
                ));
            } else if i == idx {
                let state = match t.state {
                    TaskState::Failed => RunState::Failed,
                    TaskState::Cancelled => RunState::Cancelled,
                    TaskState::Escalated => RunState::Succeeded,
                    _ => RunState::Running,
                };
                let ended = (state != RunState::Running).then_some(now - Duration::seconds(20));
                let mut r = make_run(t, role, state, starts[i], ended);
                if state == RunState::Failed {
                    r.started_at = Some(now - Duration::seconds(271 + 20));
                }
                runs.push(r);
            }
        }
    }
    let reached_qa = idx.is_some_and(|i| i >= 1);
    let done = idx == Some(3);
    let qa_run = runs
        .iter()
        .find(|r| r.role == WorkerRole::Qa)
        .map(|r| r.id)
        .unwrap_or_else(RunId::nil);
    let artifacts = if reached_qa {
        vec![
            Artifact {
                id: ArtifactId(uid(0x900)),
                run_id: qa_run,
                kind: ArtifactKind::Screenshot,
                path: "out/artifacts/01-init-prompt.png".into(),
                size: 182_000,
                mime: "image/png".into(),
                caption: Some("01-init-prompt.png".into()),
                created_at: now,
            },
            Artifact {
                id: ArtifactId(uid(0x901)),
                run_id: qa_run,
                kind: ArtifactKind::Screenshot,
                path: "out/artifacts/02-retry-banner.png".into(),
                size: 201_000,
                mime: "image/png".into(),
                caption: Some("02-retry-banner.png".into()),
                created_at: now,
            },
            Artifact {
                id: ArtifactId(uid(0x902)),
                run_id: qa_run,
                kind: ArtifactKind::Video,
                path: "out/artifacts/session.webm".into(),
                size: 4_800_000,
                mime: "video/webm".into(),
                caption: Some("session.webm · 00:48 · QA drove settings → retries → save".into()),
                created_at: now,
            },
        ]
    } else {
        vec![]
    };
    let criteria: Vec<CriterionStatus> = AC
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let status = if idx.is_none() {
                Status::Queued
            } else if t.status() == Status::Passed {
                Status::Passed
            } else if i == 1 && t.attempt() > 1 {
                Status::Running
            } else if i == 1 {
                Status::Failed
            } else {
                Status::Passed
            };
            CriterionStatus {
                criterion: a.to_string(),
                status,
            }
        })
        .collect();
    let qa_report = reached_qa.then(|| QaReport {
        criteria: AC
            .iter()
            .enumerate()
            .map(|(i, a)| CriterionResult {
                criterion: a.to_string(),
                status: if i == 1 && !done {
                    PassFail::Fail
                } else {
                    PassFail::Pass
                },
                evidence: vec![],
                notes: String::new(),
            })
            .collect(),
        tests: tests_summary(t.state.is_active()),
        summary: "41 passed · 1 failed".into(),
        test_branch: None,
    });
    let verdict = reached_qa.then(|| {
        if done {
            ReviewVerdict {
                decision: Decision::Approve,
                findings: vec![],
                summary: "Everything matches the plan. Acceptance criteria are covered by tests and the QA evidence.".into(),
            }
        } else {
            ReviewVerdict {
                decision: Decision::ChangesRequested,
                findings: vec![Finding {
                    severity: Severity::Major,
                    file: Some("src/client/backoff.ts".into()),
                    line: Some(14),
                    message: "Backoff cap is hard-coded at 30s".into(),
                }],
                summary: "The backoff cap isn't configurable, but acceptance criterion 2 requires it. The jitter needs a seedable RNG so the tests are deterministic. Everything else matches the plan.".into(),
            }
        }
    });
    let project = d
        .project(t.project_id)
        .cloned()
        .unwrap_or_else(|| d.projects[0].clone());
    let owner = d
        .user(t.owner_id)
        .cloned()
        .unwrap_or_else(|| d.users[0].clone());
    TaskDetail {
        task: t.clone(),
        project,
        owner,
        runs,
        artifacts,
        qa_report,
        verdict,
        diff: reached_qa.then(diff),
        criteria,
    }
}

fn seed_logs(t: &Task, detail: &TaskDetail, now: DateTime<Utc>) -> Vec<LogLine> {
    let mut out = Vec::new();
    let Some(idx) = stage_index(t) else {
        return out;
    };
    for (i, role) in WorkerRole::PIPELINE.into_iter().enumerate() {
        if i > idx {
            break;
        }
        let run = detail
            .runs
            .iter()
            .find(|r| r.role == role)
            .map(|r| r.id)
            .unwrap_or_else(RunId::nil);
        let pool = log_pool(role);
        let live = i == idx && t.state.is_active();
        let count = if live { 14 } else { pool.len() };
        let stage_offset = (idx.min(3) - i) as i64 * 60;
        for k in 0..count {
            let seq = k as u64;
            let ts = now - Duration::seconds(stage_offset + (count - k) as i64 * 3);
            out.push(log_line(run, role, seq, ts));
        }
    }
    out.sort_by_key(|a| a.ts);
    out
}

fn mk_plan(
    id: PlanId,
    project: ProjectId,
    owner: UserId,
    thread: ThreadId,
    status: PlanStatus,
    version: u32,
    spec: PlanSpec,
    at: DateTime<Utc>,
) -> Plan {
    Plan {
        id,
        project_id: project,
        owner_id: owner,
        thread_id: thread,
        status,
        version,
        spec,
        created_at: at,
        updated_at: at,
    }
}

fn task_spec(id: &str, title: &str, ac: &[&str], workers: PlanWorkers) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        title: title.into(),
        description: String::new(),
        acceptance_criteria: ac.iter().map(|s| s.to_string()).collect(),
        files_hint: vec![],
        depends_on: vec![],
        workers,
    }
}

fn msg(thread: ThreadId, author: MessageAuthor, body: &str, at: DateTime<Utc>) -> Message {
    Message {
        id: MessageId::new(),
        thread_id: thread,
        author,
        body: body.into(),
        voice: false,
        plan_version: None,
        created_at: at,
    }
}

/// The full prototype dataset, as of `now`.
pub fn seed(now: DateTime<Utc>) -> Data {
    let mut d = Data::default();
    let created = now - Duration::days(40);
    for (i, (ini, name, email, role, limit, _, budget)) in PEOPLE.iter().enumerate() {
        let u = User {
            id: user_id(i),
            email: email.to_string(),
            name: name.to_string(),
            role: *role,
            status: UserStatus::Active,
            limits: UserLimits {
                concurrency: *limit,
                ram_share: 0.5,
                monthly_budget_usd: *budget,
            },
            initials: ini.to_string(),
            created_at: created,
        };
        d.users.push(UserSummary::from(&u));
        if i == 0 {
            d.me = Some(u);
        }
    }
    for (i, (key, _, name)) in PROJECTS.iter().enumerate() {
        d.projects.push(Project {
            id: project_id(i),
            key: key.to_string(),
            name: name.to_string(),
            repo_url: format!("git@github.com:company/{}.git", key.to_lowercase()),
            default_branch: "main".into(),
            qa_path: "qa.yaml".into(),
            qa_status: QaProfileStatus::Valid,
            image: None,
            resources: ResourceProfile::default(),
            loop_cap: DEFAULT_LOOP_CAP,
            created_at: created,
        });
    }
    d.server = Some(ServerInfo {
        product: PRODUCT_NAME.into(),
        version: VERSION.into(),
        setup_required: false,
        allowed_domain: Some(DOMAIN.into()),
        hostname: "mac-mini".into(),
        tailnet_host: Some("mac-mini.tailnet".into()),
    });
    d.settings = Some(UserSettings {
        worker_defaults: w0(),
        default_visibility: Visibility::Shared,
        planner_reads_company: true,
        notification_channels: BTreeMap::new(),
    });
    d.harnesses = Some(catalog());
    let me = user_id(0);
    let cli = project_id(0);

    // ---- planner threads (own) + plans
    let threads = [
        (
            "Retry & backoff for SDK client",
            "CLI/SDK",
            PlanStatus::Drafting,
        ),
        (
            "Usage chart on billing page",
            "Cloud",
            PlanStatus::Dispatched,
        ),
        (
            "Pricing comparison table",
            "Website",
            PlanStatus::Dispatched,
        ),
        ("Org audit log export", "Cloud", PlanStatus::Queued),
        ("Typed errors for SDK", "CLI/SDK", PlanStatus::Merged),
    ];
    let mut thread_plan: Vec<PlanId> = Vec::new();
    for (i, (title, short, status)) in threads.iter().enumerate() {
        let tid = ThreadId(uid(0x300 + i as u128));
        let pid = PlanId(uid(0x400 + i as u128));
        let at = now - Duration::hours(1 + i as i64 * 20);
        let project = project_by_short(&d, short);
        let thread = Thread {
            id: tid,
            project_id: project,
            owner_id: me,
            title: title.to_string(),
            plan_id: Some(pid),
            status: *status,
            created_at: at,
            updated_at: at,
        };
        let (spec, version, messages) = if i == 0 {
            let spec = PlanSpec {
                feature: "Retry & backoff for SDK client".into(),
                description: "Retries for the SDK HTTP client: exponential backoff with jitter, configurable caps, and retry state shown in CLI output.".into(),
                tasks: vec![
                    task_spec("t1", "Retry policy core with backoff + jitter", &AC, plan_workers(&w0())),
                    task_spec("t2", "Show retry state in CLI output", &["“retrying (2/3) in 400ms” line with dim timestamp", "Silent in --json mode"], plan_workers(&w0())),
                    task_spec(
                        "t3",
                        "Docs page: configuring retries",
                        &["New page under /docs/sdk/retries", "Code sample matches the SDK API"],
                        PlanWorkers {
                            builder: Some(WorkerChoice::new(HARNESS_CLAUDE, "claude-sonnet-4.5")),
                            qa: Some(WorkerChoice::new(HARNESS_CLAUDE, "claude-sonnet-4.5")),
                            reviewer: Some(WorkerChoice::new(HARNESS_CODEX, "gpt-5")),
                        },
                    ),
                ],
            };
            let m = vec![
                msg(
                    tid,
                    MessageAuthor::User,
                    "SDK calls fail hard on any 503 from upstream. Add retries with backoff, and make it visible in the CLI.",
                    at - Duration::minutes(9),
                ),
                msg(
                    tid,
                    MessageAuthor::Planner,
                    "I checked the company brain. There's an approved convention for SDK retries: full jitter, base 200ms, respect Retry-After.\n\nTwo questions:\n1. Should the cap be configurable per client, or global only?\n2. Should retries be on by default?",
                    at - Duration::minutes(8),
                ),
                msg(
                    tid,
                    MessageAuthor::User,
                    "Configurable per client, default 30s. On by default with maxRetries 3.",
                    at - Duration::minutes(5),
                ),
                msg(
                    tid,
                    MessageAuthor::Planner,
                    "Got it. I drafted a 3-task plan on the right. T3 goes to the marketing site docs, so the Reviewer there runs on Codex for a second opinion. Edit anything, then approve.",
                    at - Duration::minutes(4),
                ),
            ];
            (spec, 3, m)
        } else {
            let spec = PlanSpec {
                feature: title.to_string(),
                description: String::new(),
                tasks: vec![task_spec(
                    "t1",
                    title,
                    &[
                        "Matches the acceptance notes in the thread",
                        "Covered by tests",
                    ],
                    plan_workers(&w0()),
                )],
            };
            let m = vec![
                msg(
                    tid,
                    MessageAuthor::User,
                    &format!("{title}. Keep it small and match the existing conventions."),
                    at - Duration::minutes(30),
                ),
                msg(
                    tid,
                    MessageAuthor::Planner,
                    "Drafted a 1-task plan with two acceptance criteria. It's on the right.",
                    at - Duration::minutes(29),
                ),
            ];
            (spec, 2, m)
        };
        let mut plan = mk_plan(pid, project, me, tid, *status, version, spec, at);
        if i == 0 {
            plan.updated_at = now - Duration::seconds(20);
        }
        d.plans.insert(pid, plan);
        d.thread_details.insert(
            tid,
            ThreadDetail {
                thread: thread.clone(),
                messages,
                plan: None,
                memories_in_context: 3,
                planner_model: "claude-opus-4.1".into(),
            },
        );
        d.threads.push(thread);
        thread_plan.push(pid);
    }

    // ---- board
    let hidden_plan = PlanId(uid(0x4ff));
    d.plans.insert(
        hidden_plan,
        mk_plan(
            hidden_plan,
            cli,
            me,
            ThreadId(uid(0x3ff)),
            PlanStatus::Dispatched,
            2,
            PlanSpec {
                feature: "Retry & backoff for SDK client".into(),
                description: String::new(),
                tasks: vec![],
            },
            now - Duration::days(1),
        ),
    );
    for (i, (key, title, short, owner, col, status, prog, att)) in T0.iter().enumerate() {
        let project = project_by_short(&d, short);
        let owner_id = initials_to_user(&d, owner);
        let at = now - Duration::hours(i as i64 + 1);
        if *col == "Idea" {
            let pid = PlanId(uid(0x500 + i as u128));
            let spec = PlanSpec {
                feature: title.to_string(),
                description: String::new(),
                tasks: vec![task_spec("t1", title, &["Works"], plan_workers(&w0()))],
            };
            d.plans.insert(
                pid,
                mk_plan(
                    pid,
                    project,
                    owner_id,
                    ThreadId(uid(0x600 + i as u128)),
                    PlanStatus::Drafting,
                    1,
                    spec,
                    at,
                ),
            );
            continue;
        }
        let (state, last_active) = task_state(col, status);
        let plan_id = match *key {
            "FP-145" => thread_plan[1],
            "FP-146" => thread_plan[2],
            "FP-148" => thread_plan[3],
            "FP-125" => thread_plan[4],
            _ => hidden_plan,
        };
        let pr = 400 + i as u64 * 2 + (i as u64 % 3);
        let t = Task {
            id: TaskId(uid(0x700 + i as u128)),
            plan_id,
            project_id: project,
            owner_id,
            number: key[3..].parse().unwrap_or(0),
            key: key.to_string(),
            spec_id: "t1".into(),
            title: title.to_string(),
            description: String::new(),
            acceptance: AC.iter().map(|s| s.to_string()).collect(),
            files_hint: vec![],
            depends_on: vec![],
            workers: w0(),
            state,
            last_active,
            loop_count: att - 1,
            loop_cap: DEFAULT_LOOP_CAP,
            branch: Some(format!("kiln/{}", key.to_lowercase())),
            pr_url: Some(format!(
                "https://github.com/company/{}/pull/{pr}",
                PROJECTS
                    .iter()
                    .find(|p| p.1 == *short)
                    .map(|p| p.0.to_lowercase())
                    .unwrap_or_default()
            )),
            pr_number: Some(pr),
            progress: *prog,
            meta: String::new(),
            created_at: at,
            updated_at: at,
        };
        d.tasks.push(t);
    }
    let tasks = d.tasks.clone();
    for t in &tasks {
        let det = detail_for(&d, t, now);
        let logs = seed_logs(t, &det, now);
        d.logs.insert(t.id, logs);
        d.details.insert(t.id, det);
    }

    // ---- memory
    let mem: [(
        &str,
        &str,
        Visibility,
        bool,
        &str,
        &str,
        MemoryType,
        u32,
        i64,
        &[&str],
    ); 7] = [
        (
            "Retry/backoff conventions for SDK clients",
            "All outbound HTTP in the SDK goes through withRetry(). Use exponential backoff with full jitter, base 200ms, cap from config (default 30s). Never retry 4xx except 408 and 429; respect Retry-After.",
            Visibility::Company,
            false,
            "AR",
            "FP-131 · Reviewer",
            MemoryType::Convention,
            14,
            2 * 24 * 60,
            &[],
        ),
        (
            "E2E tests must seed randomness",
            "Any test touching jitter, IDs or shuffles must call seedRng(42) in beforeEach. The reviewer rejects nondeterministic tests.",
            Visibility::Company,
            true,
            "MK",
            "FP-142 · Reviewer",
            MemoryType::Convention,
            3,
            60,
            &[],
        ),
        (
            "Cloud dashboard: use <DataTable> for lists over 20 rows",
            "Virtualized DataTable from ui/table handles sorting and keyboard nav. Hand-rolled tables were rejected twice in review for a11y.",
            Visibility::Company,
            false,
            "JL",
            "FP-128 · Builder",
            MemoryType::Convention,
            9,
            5 * 24 * 60,
            &[],
        ),
        (
            "Marketing site deploys need a Lighthouse ≥ 95 run",
            "QA for website tasks runs Lighthouse mobile and fails the task if performance drops below 95 or CLS exceeds 0.05.",
            Visibility::Shared,
            false,
            "MK",
            "FP-127 · QA",
            MemoryType::Procedure,
            5,
            7 * 24 * 60,
            &["MK", "JL"],
        ),
        (
            "My LiteLLM routing preferences",
            "Route Codex QA runs to gpt-5 via LiteLLM; fall back to qwen3-coder when the proxy reports rate limiting.",
            Visibility::Private,
            false,
            "AR",
            "manual",
            MemoryType::Preference,
            7,
            3 * 24 * 60,
            &[],
        ),
        (
            "Webhook signatures: HMAC-SHA256 over raw body",
            "Verify against the raw request bytes before JSON parsing. Constant-time compare. Tolerate 5 minutes of clock skew.",
            Visibility::Shared,
            true,
            "SD",
            "FP-139 · Builder",
            MemoryType::Fact,
            2,
            20,
            &["SD", "AR"],
        ),
        (
            "CLI output style",
            "Status lines start with a glyph (✓ ✕ ◐), dim timestamps, no emoji. Use the shared printer in src/cli/output.ts.",
            Visibility::Company,
            false,
            "SD",
            "FP-125 · Reviewer",
            MemoryType::Convention,
            11,
            14 * 24 * 60,
            &[],
        ),
    ];
    let subjects = [
        "sdk networking",
        "testing",
        "cloud ui",
        "website perf",
        "models",
        "cloud security",
        "cli",
    ];
    let projects_for = [Some(0usize), None, Some(1), Some(2), None, Some(1), Some(0)];
    for (i, (title, body, vis, pending, owner, src, kind, used, mins, shared)) in
        mem.iter().enumerate()
    {
        let at = now - Duration::minutes(*mins);
        let item = MemoryItem {
            id: MemoryId(uid(0x800 + i as u128)),
            owner_id: initials_to_user(&d, owner),
            kind: *kind,
            project_id: projects_for[i].map(project_id),
            visibility: *vis,
            shared_with: shared.iter().map(|s| initials_to_user(&d, s)).collect(),
            requested_visibility: pending.then_some(Visibility::Company),
            title: title.to_string(),
            subject: subjects[i].into(),
            relation: "convention".into(),
            object: String::new(),
            body: body.to_string(),
            status: if *pending {
                MemoryStatus::Pending
            } else {
                MemoryStatus::Active
            },
            salience: 0.6,
            source_run_id: None,
            source_label: Some(src.to_string()),
            use_count: *used,
            last_used_at: Some(now - Duration::minutes(12)),
            created_at: at,
            updated_at: at,
        };
        let owner_s = d
            .user(item.owner_id)
            .cloned()
            .unwrap_or_else(|| d.users[0].clone());
        let shared_s = item
            .shared_with
            .iter()
            .filter_map(|u| d.user(*u).cloned())
            .collect();
        d.memory_details.insert(
            item.id,
            MemoryDetail {
                item: item.clone(),
                recently_used: vec![
                    MemoryUse {
                        task_key: "FP-142".into(),
                        role: WorkerRole::Builder,
                        at: now - Duration::minutes(12),
                    },
                    MemoryUse {
                        task_key: "FP-131".into(),
                        role: WorkerRole::Reviewer,
                        at: now - Duration::hours(26),
                    },
                    MemoryUse {
                        task_key: "“Retry & backoff”".into(),
                        role: WorkerRole::Planner,
                        at: now - Duration::hours(1),
                    },
                ],
                owner: owner_s,
                shared_with: shared_s,
            },
        );
        d.memory.push(item);
    }
    d.memory_total = d.memory.len() as u32;
    d.memory_runs_week = 61;

    // ---- credentials
    let cred =
        |i: u128, kind, name: &str, masked: &str, base: Option<&str>, status| CredentialSummary {
            id: CredentialId(uid(0xa00 + i)),
            kind,
            name: name.into(),
            masked: masked.into(),
            base_url: base.map(str::to_string),
            env_var: None,
            status,
            created_at: now - Duration::days(20),
            expires_at: None,
        };
    d.credentials = vec![
        cred(
            0,
            CredentialKind::AnthropicApiKey,
            "Anthropic API key",
            "sk-ant-api03-…4f2a",
            None,
            CredentialHealth::Valid,
        ),
        cred(
            1,
            CredentialKind::OpenaiApiKey,
            "OpenAI API key",
            "sk-proj-…91cd",
            None,
            CredentialHealth::Valid,
        ),
        cred(
            2,
            CredentialKind::ClaudeOauth,
            "Claude subscription",
            "sk-ant-oat01-…7Qa9",
            None,
            CredentialHealth::ExpiresSoon { days: 6 },
        ),
        cred(
            3,
            CredentialKind::Litellm,
            "LiteLLM proxy",
            "sk-llm-…c0de",
            Some("https://litellm.tailnet:4000"),
            CredentialHealth::Reachable { latency_ms: 34 },
        ),
    ];

    // ---- notifications
    d.notifications = vec![Notification {
        id: NotificationId(uid(0xb00)),
        user_id: me,
        kind: NotificationKind::MemoryApproval,
        title: "2 memories await Company approval".into(),
        body: "E2E tests must seed randomness · Webhook signatures".into(),
        deep_link: "brain".into(),
        actions: vec![],
        task_id: None,
        plan_id: None,
        memory_id: None,
        read: false,
        resolved: None,
        created_at: now - Duration::minutes(20),
    }];

    // ---- machine + admin
    let hist: Vec<f32> = (0..32)
        .map(|i| 50.0 + ((i as f32 / 3.0).sin() * 14.0) + ((i * 37 % 8) as f32))
        .collect();
    d.machine = machine(&d, &hist);
    refresh_admin(&mut d, 0);
    d
}

/// Recompute the admin overview (containers + queue) from the board.
pub fn refresh_admin(d: &mut Data, tick: u64) {
    let users: Vec<AdminUserRow> = match d.admin.take() {
        Some(a) => a.users,
        None => PEOPLE
            .iter()
            .enumerate()
            .map(
                |(i, (ini, name, email, role, limit, spend, budget))| AdminUserRow {
                    user: User {
                        id: user_id(i),
                        email: email.to_string(),
                        name: name.to_string(),
                        role: *role,
                        status: UserStatus::Active,
                        limits: UserLimits {
                            concurrency: *limit,
                            ram_share: 0.5,
                            monthly_budget_usd: *budget,
                        },
                        initials: ini.to_string(),
                        created_at: Utc::now() - Duration::days(40),
                    },
                    spend: UserSpend {
                        user_id: user_id(i),
                        month_usd: *spend,
                        budget_usd: *budget,
                    },
                    running: 0,
                },
            )
            .collect(),
    };
    let running: Vec<&Task> = d.tasks.iter().filter(|t| t.state.is_active()).collect();
    let containers = running
        .iter()
        .enumerate()
        .map(|(i, t)| RunningContainer {
            run_id: RunId(uid(0xc00 + i as u128)),
            task_id: Some(t.id),
            task_key: Some(t.key.clone()),
            role: super::derive::current_role(t),
            owner: d
                .user(t.owner_id)
                .cloned()
                .unwrap_or_else(|| d.users[0].clone()),
            mem_mb: ((1.6 + ((i as f64 * 0.7) % 2.0)) * 1024.0) as u64,
            cpu_percent: (30 + ((i as u64 * 17 + tick * 3) % 60)) as f32,
        })
        .collect();
    let queue = d
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Queued)
        .enumerate()
        .map(|(i, t)| QueueEntry {
            position: i as u32 + 1,
            task_id: t.id,
            task_key: t.key.clone(),
            title: t.title.clone(),
            owner: d
                .user(t.owner_id)
                .cloned()
                .unwrap_or_else(|| d.users[0].clone()),
            est_wait_secs: (i as u32 + 1) * 240,
        })
        .collect();
    d.admin = Some(AdminOverview {
        machine: d.machine.clone(),
        users,
        containers,
        queue,
        projects: d.projects.clone(),
    });
}

// ---------------------------------------------------------------- simulation

/// Timer-driven simulation state (one `step` per second, like the prototype).
#[derive(Clone, Debug)]
pub struct DemoSim {
    pub tick: u64,
    rng: u64,
    /// Fractional progress per running task (the prototype advances 1.3%/s,
    /// 2.4%/s for the selected task).
    progress: BTreeMap<TaskId, f32>,
    seq: BTreeMap<TaskId, u64>,
    cpu: Vec<f32>,
    pub selected: Option<TaskId>,
    next_task_number: u64,
    reply_count: usize,
}

impl DemoSim {
    pub fn new(d: &Data) -> Self {
        Self {
            tick: 0,
            rng: 0x9e37_79b9_7f4a_7c15,
            progress: d.tasks.iter().map(|t| (t.id, t.progress as f32)).collect(),
            seq: d.tasks.iter().map(|t| (t.id, 14)).collect(),
            cpu: d.machine.cpu_history.clone(),
            selected: d.task_by_key("FP-142").map(|t| t.id),
            next_task_number: 160,
            reply_count: 0,
        }
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Advance one second. Returns the events to apply.
    pub fn step(&mut self, d: &Data, now: DateTime<Utc>) -> Vec<ServerEvent> {
        self.tick += 1;
        let mut out = Vec::new();
        for t in &d.tasks {
            if !matches!(
                t.state,
                TaskState::Building
                    | TaskState::Testing
                    | TaskState::Reviewing
                    | TaskState::Looping
            ) {
                continue;
            }
            let speed = if Some(t.id) == self.selected {
                2.4
            } else {
                1.3
            };
            let p = self.progress.entry(t.id).or_insert(t.progress as f32);
            *p += speed;
            let mut nt = t.clone();
            let role = super::derive::current_role(t);
            if *p >= 100.0 {
                *p = 0.0;
                let next = match t.state {
                    TaskState::Building | TaskState::Looping => TaskState::Testing,
                    TaskState::Testing => TaskState::Reviewing,
                    _ => TaskState::Ready,
                };
                nt.state = next;
                nt.last_active = if next.is_active() {
                    Some(next)
                } else {
                    t.last_active
                };
                nt.progress = if next == TaskState::Ready { 100 } else { 0 };
                if next == TaskState::Ready {
                    *p = 100.0;
                }
                // Close the finished run, open the next one.
                if let Some(det) = d.details.get(&t.id)
                    && let Some(run) = det
                        .runs
                        .iter()
                        .filter(|r| r.role == role)
                        .max_by_key(|r| r.started_at)
                {
                    let mut r = run.clone();
                    r.state = RunState::Succeeded;
                    r.ended_at = Some(now);
                    r.progress = 100;
                    out.push(ServerEvent::RunUpserted { run: r });
                }
                if let Some(next_role) = match next {
                    TaskState::Testing => Some(WorkerRole::Qa),
                    TaskState::Reviewing => Some(WorkerRole::Reviewer),
                    _ => None,
                } {
                    out.push(ServerEvent::RunUpserted {
                        run: make_run(&nt, next_role, RunState::Running, now, None),
                    });
                }
            } else {
                nt.progress = p.round().min(100.0) as u8;
            }
            nt.updated_at = now;
            // One log line per tick for the live role.
            let seq = self.seq.entry(t.id).or_insert(14);
            *seq += 1;
            let run = d
                .details
                .get(&t.id)
                .and_then(|det| {
                    det.runs
                        .iter()
                        .filter(|r| r.role == role)
                        .max_by_key(|r| r.started_at)
                })
                .map(|r| r.id)
                .unwrap_or_else(RunId::nil);
            out.push(ServerEvent::Log {
                task_id: Some(t.id),
                thread_id: None,
                line: log_line(run, role, *seq, now),
            });
            out.push(ServerEvent::TaskUpserted { task: nt });
        }
        // CPU history: random walk, clamped 20–96.
        let last = self.cpu.last().copied().unwrap_or(50.0);
        let delta = (self.rand() % 17) as f32 - 8.0;
        let next = (last + delta).clamp(20.0, 96.0).round();
        if self.cpu.len() >= 32 {
            self.cpu.remove(0);
        }
        self.cpu.push(next);
        out.push(ServerEvent::Machine {
            stats: machine(d, &self.cpu),
        });
        out
    }

    /// Keys for a batch of `n` new tasks: a random base (like the
    /// prototype's `160 + rand·20`), then consecutive numbers.
    pub fn next_keys(&mut self, n: usize) -> Vec<String> {
        let base = self.next_task_number + self.rand() % 20;
        self.next_task_number = base + n as u64;
        (0..n as u64).map(|i| format!("FP-{}", base + i)).collect()
    }

    pub fn set_progress(&mut self, id: TaskId, p: f32) {
        self.progress.insert(id, p);
    }

    pub fn next_reply(&mut self) -> &'static str {
        let r = REPLIES[self.reply_count % REPLIES.len()];
        self.reply_count += 1;
        r
    }
}

// ---------------------------------------------------------------- actions

/// Result of a demo action: events to apply and the toast to show.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub events: Vec<ServerEvent>,
    pub toast: Option<String>,
}

impl Outcome {
    fn new(events: Vec<ServerEvent>, toast: impl Into<String>) -> Self {
        Self {
            events,
            toast: Some(toast.into()),
        }
    }
}

fn cap_attempt(t: &Task) -> u32 {
    // loop_count is 0-based; the attempt shown is loop_count + 1 ≤ cap.
    (t.loop_count + 1).min(t.loop_cap.saturating_sub(1))
}

pub fn act_task(
    sim: &mut DemoSim,
    d: &Data,
    id: TaskId,
    action: &TaskAction,
    now: DateTime<Utc>,
) -> Outcome {
    let Some(t) = d.task(id) else {
        return Outcome::default();
    };
    let mut nt = t.clone();
    nt.updated_at = now;
    let active_stage = |t: &Task| -> TaskState {
        let s = if t.state.is_active() {
            Some(t.state)
        } else {
            t.last_active
        };
        match s {
            Some(s @ (TaskState::Building | TaskState::Testing | TaskState::Reviewing)) => s,
            Some(TaskState::Looping) => TaskState::Building,
            _ => TaskState::Building,
        }
    };
    let mut events = Vec::new();
    let toast = match action {
        TaskAction::Retry => {
            nt.state = active_stage(t);
            nt.last_active = Some(nt.state);
            nt.progress = 0;
            nt.loop_count = cap_attempt(t);
            sim.set_progress(id, 0.0);
            let role = super::derive::current_role(&nt);
            events.push(ServerEvent::RunUpserted {
                run: make_run(&nt, role, RunState::Running, now, None),
            });
            format!(
                "{} restarted · attempt {}/{}",
                t.key,
                nt.attempt(),
                nt.loop_cap
            )
        }
        TaskAction::Stop => {
            nt.last_active = if t.state.is_active() {
                Some(t.state)
            } else {
                t.last_active.or(Some(t.state))
            };
            nt.state = TaskState::Cancelled;
            format!("{} stopped · container released", t.key)
        }
        TaskAction::Escalate { .. } => {
            nt.last_active = if t.state.is_active() {
                Some(t.state)
            } else {
                t.last_active.or(Some(t.state))
            };
            nt.state = TaskState::Escalated;
            format!("{} escalated · team notified on Android", t.key)
        }
        TaskAction::Feedback { .. } => {
            nt.state = TaskState::Building;
            nt.last_active = Some(TaskState::Building);
            nt.progress = 0;
            nt.loop_count = cap_attempt(t);
            sim.set_progress(id, 0.0);
            events.push(ServerEvent::RunUpserted {
                run: make_run(&nt, WorkerRole::Builder, RunState::Running, now, None),
            });
            format!("Feedback sent · Builder restarted on {}", t.key)
        }
    };
    events.push(ServerEvent::TaskUpserted { task: nt });
    Outcome::new(events, toast)
}

/// A user message. Returns the immediate events (thread, message, pending);
/// the planner's reply comes from `planner_reply` ~1.6s later.
pub fn act_send(
    d: &Data,
    thread: Option<ThreadId>,
    project: ProjectId,
    body: &str,
    now: DateTime<Utc>,
) -> (ThreadId, Vec<ServerEvent>) {
    let mut events = Vec::new();
    let me = d.me.as_ref().map(|u| u.id).unwrap_or_else(UserId::nil);
    let tid = match thread.filter(|t| d.thread(*t).is_some()) {
        Some(t) => t,
        None => {
            let tid = ThreadId::new();
            let title: String = body
                .split(['.', '\n'])
                .next()
                .unwrap_or(body)
                .chars()
                .take(48)
                .collect();
            let th = Thread {
                id: tid,
                project_id: project,
                owner_id: me,
                title: title.trim().to_string(),
                plan_id: None,
                status: PlanStatus::Drafting,
                created_at: now,
                updated_at: now,
            };
            events.push(ServerEvent::ThreadUpserted { thread: th });
            tid
        }
    };
    events.push(ServerEvent::Message {
        message: msg(tid, MessageAuthor::User, body, now),
    });
    events.push(ServerEvent::PlannerStatus {
        thread_id: tid,
        pending: true,
        label: PENDING_LABEL.into(),
    });
    (tid, events)
}

pub fn planner_reply(
    sim: &mut DemoSim,
    d: &Data,
    thread: ThreadId,
    now: DateTime<Utc>,
) -> Vec<ServerEvent> {
    let mut events = vec![ServerEvent::PlannerStatus {
        thread_id: thread,
        pending: false,
        label: String::new(),
    }];
    let Some(th) = d.thread(thread) else {
        return events;
    };
    match d.plan_for_thread(thread) {
        Some(p) => {
            let mut p = p.clone();
            p.version += 1;
            p.updated_at = now;
            let mut m = msg(thread, MessageAuthor::Planner, sim.next_reply(), now);
            m.plan_version = Some(p.version);
            events.push(ServerEvent::Message { message: m });
            events.push(ServerEvent::PlanUpserted { plan: p });
        }
        None => {
            let first = d
                .thread_details
                .get(&thread)
                .and_then(|t| t.messages.iter().find(|m| m.author == MessageAuthor::User))
                .map(|m| m.body.clone())
                .unwrap_or_else(|| th.title.clone());
            let title: String = th.title.chars().take(60).collect();
            let defaults = d
                .settings
                .as_ref()
                .map(|s| s.worker_defaults.clone())
                .unwrap_or_else(w0);
            let spec = PlanSpec {
                feature: title.clone(),
                description: first,
                tasks: vec![
                    task_spec(
                        "t1",
                        &title,
                        &[
                            "Behaves as described in the thread",
                            "Covered by unit tests",
                        ],
                        plan_workers(&defaults),
                    ),
                    task_spec(
                        "t2",
                        "Document the change",
                        &["Docs page updated with an example"],
                        plan_workers(&defaults),
                    ),
                ],
            };
            let pid = PlanId::new();
            let plan = mk_plan(
                pid,
                th.project_id,
                th.owner_id,
                thread,
                PlanStatus::Drafting,
                1,
                spec,
                now,
            );
            let mut m = msg(
                thread,
                MessageAuthor::Planner,
                "I checked the company brain and drafted a 2-task plan on the right. Edit anything, then approve.",
                now,
            );
            m.plan_version = Some(1);
            events.push(ServerEvent::Message { message: m });
            events.push(ServerEvent::PlanUpserted { plan });
        }
    }
    events
}

pub fn act_update_plan(
    d: &Data,
    plan: PlanId,
    spec: PlanSpec,
    now: DateTime<Utc>,
) -> Vec<ServerEvent> {
    let Some(p) = d.plans.get(&plan) else {
        return vec![];
    };
    let mut p = p.clone();
    p.spec = spec;
    p.updated_at = now;
    vec![ServerEvent::PlanUpserted { plan: p }]
}

pub fn act_approve(sim: &mut DemoSim, d: &Data, plan: PlanId, now: DateTime<Utc>) -> Outcome {
    let Some(p) = d.plans.get(&plan) else {
        return Outcome::default();
    };
    let mut events = Vec::new();
    let n = p.spec.tasks.len();
    let keys = sim.next_keys(n);
    for (i, ts) in p.spec.tasks.iter().enumerate() {
        let key = keys[i].clone();
        let mut workers = d
            .settings
            .as_ref()
            .map(|s| s.worker_defaults.clone())
            .unwrap_or_else(w0);
        for role in WorkerRole::PIPELINE {
            if let Some(w) = ts.workers.get(role) {
                workers.insert(role, w.clone());
            }
        }
        let state = if i == 0 {
            TaskState::Building
        } else {
            TaskState::Queued
        };
        let t = Task {
            id: TaskId::new(),
            plan_id: p.id,
            project_id: p.project_id,
            owner_id: p.owner_id,
            number: key[3..].parse().unwrap_or(0),
            key: key.clone(),
            spec_id: ts.id.clone(),
            title: ts.title.clone(),
            description: ts.description.clone(),
            acceptance: ts.acceptance_criteria.clone(),
            files_hint: ts.files_hint.clone(),
            depends_on: vec![],
            workers,
            state,
            last_active: (i == 0).then_some(TaskState::Building),
            loop_count: 0,
            loop_cap: d.loop_cap_for(p.project_id),
            branch: Some(format!("kiln/{}", key.to_lowercase())),
            pr_url: None,
            pr_number: Some(500 + i as u64),
            progress: 0,
            meta: String::new(),
            created_at: now + Duration::milliseconds((n - i) as i64),
            updated_at: now,
        };
        sim.set_progress(t.id, 0.0);
        if i == 0 {
            events.push(ServerEvent::RunUpserted {
                run: make_run(&t, WorkerRole::Builder, RunState::Running, now, None),
            });
        }
        events.insert(0, ServerEvent::TaskUpserted { task: t });
    }
    let mut p2 = p.clone();
    p2.status = PlanStatus::Dispatched;
    p2.updated_at = now;
    events.push(ServerEvent::PlanUpserted { plan: p2 });
    let queued = n.saturating_sub(1);
    Outcome::new(
        events,
        format!(
            "{n} task{} dispatched · 1 building, {queued} queued",
            if n == 1 { "" } else { "s" }
        ),
    )
}

pub fn act_update_memory(
    d: &Data,
    id: MemoryId,
    req: &UpdateMemoryRequest,
    now: DateTime<Utc>,
) -> Outcome {
    let Some(m) = d.memory_item(id) else {
        return Outcome::default();
    };
    let mut m = m.clone();
    let mut toast = None;
    if let Some(t) = &req.title {
        m.title = t.clone();
    }
    if let Some(b) = &req.body {
        m.body = b.clone();
    }
    if let Some(v) = req.visibility {
        if m.kind == MemoryType::Preference && v != Visibility::Private {
            return Outcome {
                events: vec![],
                toast: Some("Personal preferences are always Private".into()),
            };
        }
        if v == Visibility::Company && m.visibility != Visibility::Company {
            m.visibility = Visibility::Company;
            m.status = MemoryStatus::Pending;
            m.requested_visibility = Some(Visibility::Company);
            toast = Some("Submitted for Company approval".to_string());
        } else if v != Visibility::Company {
            m.visibility = v;
            m.status = MemoryStatus::Active;
            m.requested_visibility = None;
            if v == Visibility::Shared && m.shared_with.is_empty() {
                m.shared_with = d.me.iter().map(|u| u.id).collect();
            }
        }
    }
    if let Some(s) = &req.shared_with {
        m.shared_with = s.clone();
    }
    if req.archived == Some(true) {
        m.status = MemoryStatus::Archived;
        toast = Some("Archived".into());
    }
    m.updated_at = now;
    Outcome {
        events: vec![ServerEvent::MemoryUpserted { item: m }],
        toast,
    }
}

pub fn act_decide_memory(
    d: &Data,
    id: MemoryId,
    decision: MemoryDecision,
    now: DateTime<Utc>,
) -> Outcome {
    let Some(m) = d.memory_item(id) else {
        return Outcome::default();
    };
    let mut m = m.clone();
    m.updated_at = now;
    m.requested_visibility = None;
    let toast = match decision {
        MemoryDecision::Approve => {
            m.visibility = Visibility::Company;
            m.status = MemoryStatus::Active;
            "Approved · now visible to the whole company"
        }
        MemoryDecision::KeepShared => {
            m.visibility = Visibility::Shared;
            m.status = MemoryStatus::Active;
            "Kept as shared"
        }
        MemoryDecision::KeepPrivate => {
            m.visibility = Visibility::Private;
            m.status = MemoryStatus::Active;
            "Kept private"
        }
        MemoryDecision::Reject => {
            m.status = MemoryStatus::Rejected;
            "Rejected"
        }
    };
    Outcome::new(vec![ServerEvent::MemoryUpserted { item: m }], toast)
}

/// A fake (non-scannable) QR pattern for the demo pairing dialog.
pub fn demo_pairing(now: DateTime<Utc>) -> PairStartResponse {
    let code = "K7Q2-9XMD".to_string();
    let n = 25;
    let mut cells = String::new();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for y in 0..n {
        for x in 0..n {
            let finder = |cx: i32, cy: i32| {
                let (dx, dy) = ((x - cx).abs(), (y - cy).abs());
                let r = dx.max(dy);
                r <= 3 && r != 2
            };
            let in_finder_zone = |cx: i32, cy: i32| (x - cx).abs() <= 4 && (y - cy).abs() <= 4;
            h ^= (x * 31 + y * 17) as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
            let on = if in_finder_zone(3, 3) || in_finder_zone(21, 3) || in_finder_zone(3, 21) {
                finder(3, 3) || finder(21, 3) || finder(3, 21)
            } else {
                h >> 61 & 1 == 1
            };
            if on {
                cells.push_str(&format!(
                    "<rect x=\"{x}\" y=\"{y}\" width=\"1\" height=\"1\"/>"
                ));
            }
        }
    }
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"-2 -2 29 29\" shape-rendering=\"crispEdges\"><rect x=\"-2\" y=\"-2\" width=\"29\" height=\"29\" fill=\"#fff\"/><g fill=\"#0f1115\">{cells}</g></svg>"
    );
    PairStartResponse {
        code: code.clone(),
        server_url: "https://mac-mini.tailnet".into(),
        pair_uri: format!("kiln://pair?url=https://mac-mini.tailnet&code={code}"),
        qr_svg: svg,
        expires_at: now + Duration::minutes(10),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::derive::*;

    #[test]
    fn seed_matches_prototype() {
        let d = seed(Utc::now());
        assert_eq!(d.tasks.len(), 13);
        assert_eq!(d.users.len(), 4);
        assert_eq!(d.projects.len(), 3);
        assert_eq!(d.memory.len(), 7);
        assert_eq!(d.credentials.len(), 4);
        assert_eq!(d.threads.len(), 5);
        let fp142 = d.task_by_key("FP-142").unwrap();
        assert_eq!(fp142.state, TaskState::Testing);
        assert_eq!(fp142.attempt(), 2);
        assert_eq!(
            d.plans
                .values()
                .filter(|p| p.status == PlanStatus::Drafting)
                .count(),
            3
        );
        assert!(d.admin.is_some());
        assert_eq!(d.machine.cpu_history.len(), 32);
    }

    #[test]
    fn sim_advances_and_moves_columns() {
        let mut d = seed(Utc::now());
        let mut sim = DemoSim::new(&d);
        let id = d.task_by_key("FP-146").unwrap().id; // Building at 64%
        let mut moved = false;
        for _ in 0..40 {
            let evs = sim.step(&d, Utc::now());
            for e in &evs {
                d.apply(e);
            }
            if d.task(id).unwrap().state == TaskState::Testing {
                moved = true;
                break;
            }
        }
        assert!(moved, "64% + 1.3%/s reaches QA within 40 steps");
        assert_eq!(d.task(id).unwrap().progress, 0);
        assert!(
            d.details[&id]
                .runs
                .iter()
                .any(|r| r.role == WorkerRole::Qa && r.state == RunState::Running)
        );
    }

    #[test]
    fn sim_review_to_ready_becomes_passed() {
        let mut d = seed(Utc::now());
        let mut sim = DemoSim::new(&d);
        let id = d.task_by_key("FP-137").unwrap().id; // Review at 30%
        for _ in 0..80 {
            let evs = sim.step(&d, Utc::now());
            for e in &evs {
                d.apply(e);
            }
        }
        let t = d.task(id).unwrap();
        assert_eq!(t.state, TaskState::Ready);
        assert_eq!(t.status(), Status::Passed);
        assert_eq!(t.progress, 100);
    }

    #[test]
    fn sim_emits_logs_and_machine() {
        let d = seed(Utc::now());
        let mut sim = DemoSim::new(&d);
        let evs = sim.step(&d, Utc::now());
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, ServerEvent::Log { .. }))
                .count(),
            4
        );
        assert!(evs.iter().any(|e| matches!(e, ServerEvent::Machine { .. })));
        if let Some(ServerEvent::Machine { stats }) = evs.last() {
            assert_eq!(stats.cpu_history.len(), 32);
            assert!((20.0..=96.0).contains(&stats.cpu_percent));
        }
    }

    #[test]
    fn task_actions_follow_prototype() {
        let mut d = seed(Utc::now());
        let mut sim = DemoSim::new(&d);
        let id = d.task_by_key("FP-142").unwrap().id;

        let o = act_task(&mut sim, &d, id, &TaskAction::Stop, Utc::now());
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        let t = d.task(id).unwrap();
        assert_eq!(t.status(), Status::Queued);
        assert_eq!(card_meta(t), "Stopped");
        assert_eq!(
            t.column(),
            BoardColumn::Qa,
            "stopped tasks stay in their column"
        );
        assert_eq!(
            o.toast.as_deref(),
            Some("FP-142 stopped · container released")
        );

        let o = act_task(&mut sim, &d, id, &TaskAction::Retry, Utc::now());
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        let t = d.task(id).unwrap();
        assert_eq!(t.state, TaskState::Testing);
        assert_eq!(t.attempt(), 3);
        assert_eq!(t.progress, 0);
        assert_eq!(o.toast.as_deref(), Some("FP-142 restarted · attempt 3/3"));

        // Attempts never exceed the cap.
        let o = act_task(&mut sim, &d, id, &TaskAction::Retry, Utc::now());
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        assert_eq!(d.task(id).unwrap().attempt(), 3);

        let o = act_task(
            &mut sim,
            &d,
            id,
            &TaskAction::Escalate { note: None },
            Utc::now(),
        );
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        assert_eq!(d.task(id).unwrap().status(), Status::NeedsHuman);
        assert_eq!(
            o.toast.as_deref(),
            Some("FP-142 escalated · team notified on Android")
        );

        let fb = TaskAction::Feedback {
            text: "fix".into(),
            attach_qa: true,
            attach_verdict: true,
            save_memory: false,
        };
        let o = act_task(&mut sim, &d, id, &fb, Utc::now());
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        let t = d.task(id).unwrap();
        assert_eq!(t.state, TaskState::Building);
        assert_eq!(t.column(), BoardColumn::Building);
    }

    #[test]
    fn approve_dispatches_one_building_rest_queued() {
        let mut d = seed(Utc::now());
        let mut sim = DemoSim::new(&d);
        let plan = d
            .plans
            .values()
            .find(|p| p.status == PlanStatus::Drafting && p.spec.tasks.len() == 3)
            .unwrap()
            .id;
        let before = d.tasks.len();
        let o = act_approve(&mut sim, &d, plan, Utc::now());
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        assert_eq!(
            o.toast.as_deref(),
            Some("3 tasks dispatched · 1 building, 2 queued")
        );
        assert_eq!(d.tasks.len(), before + 3);
        assert_eq!(
            d.tasks[0].state,
            TaskState::Building,
            "first task first on the board"
        );
        assert_eq!(
            d.tasks
                .iter()
                .take(3)
                .filter(|t| t.state == TaskState::Queued)
                .count(),
            2
        );
        assert_eq!(d.plans[&plan].status, PlanStatus::Dispatched);
    }

    #[test]
    fn planner_round_trip_bumps_version() {
        let mut d = seed(Utc::now());
        let mut sim = DemoSim::new(&d);
        let th = d.threads[0].id;
        let v = d.plan_for_thread(th).unwrap().version;
        let (tid, evs) = act_send(
            &d,
            Some(th),
            d.threads[0].project_id,
            "Also cover --quiet",
            Utc::now(),
        );
        assert_eq!(tid, th);
        evs.iter().for_each(|e| {
            d.apply(e);
        });
        assert!(d.planner_status.contains_key(&th));
        let evs = planner_reply(&mut sim, &d, th, Utc::now());
        evs.iter().for_each(|e| {
            d.apply(e);
        });
        assert!(!d.planner_status.contains_key(&th));
        assert_eq!(d.plan_for_thread(th).unwrap().version, v + 1);
    }

    #[test]
    fn new_thread_gets_a_plan() {
        let mut d = seed(Utc::now());
        let mut sim = DemoSim::new(&d);
        let p = d.projects[1].id;
        let (tid, evs) = act_send(
            &d,
            None,
            p,
            "Add CSV export to the audit log. Admins only.",
            Utc::now(),
        );
        evs.iter().for_each(|e| {
            d.apply(e);
        });
        assert_eq!(d.threads[0].id, tid);
        assert_eq!(d.threads[0].title, "Add CSV export to the audit log");
        planner_reply(&mut sim, &d, tid, Utc::now())
            .iter()
            .for_each(|e| {
                d.apply(e);
            });
        let plan = d.plan_for_thread(tid).unwrap();
        assert_eq!(plan.spec.tasks.len(), 2);
        assert!(plan.spec.validate().is_ok());
    }

    #[test]
    fn memory_visibility_rules() {
        let mut d = seed(Utc::now());
        let m = d
            .memory
            .iter()
            .find(|m| m.visibility == Visibility::Private)
            .unwrap()
            .clone();
        // m5 is a personal preference: can't leave Private.
        let o = act_update_memory(
            &d,
            m.id,
            &UpdateMemoryRequest {
                visibility: Some(Visibility::Company),
                ..Default::default()
            },
            Utc::now(),
        );
        assert!(o.events.is_empty());
        let m = d.memory[2].clone(); // company, active
        let o = act_update_memory(
            &d,
            m.id,
            &UpdateMemoryRequest {
                visibility: Some(Visibility::Shared),
                ..Default::default()
            },
            Utc::now(),
        );
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        let m2 = d.memory_item(m.id).unwrap().clone();
        assert_eq!(m2.visibility, Visibility::Shared);
        assert!(!m2.shared_with.is_empty(), "sharing defaults to yourself");
        let o = act_update_memory(
            &d,
            m.id,
            &UpdateMemoryRequest {
                visibility: Some(Visibility::Company),
                ..Default::default()
            },
            Utc::now(),
        );
        assert_eq!(o.toast.as_deref(), Some("Submitted for Company approval"));
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        assert_eq!(d.memory_item(m.id).unwrap().status, MemoryStatus::Pending);
        let o = act_decide_memory(&d, m.id, MemoryDecision::Approve, Utc::now());
        o.events.iter().for_each(|e| {
            d.apply(e);
        });
        let m3 = d.memory_item(m.id).unwrap();
        assert_eq!(
            (m3.visibility, m3.status),
            (Visibility::Company, MemoryStatus::Active)
        );
    }

    #[test]
    fn masking() {
        assert_eq!(mask("sk-ant-api03-ABCDEFGH1234"), "sk-ant-…1234");
        assert!(!mask("sk-ant-api03-ABCDEFGH1234").contains("ABCD"));
    }
}

/// Writes the demo dataset as daemon-shaped JSON responses so a stub HTTP
/// server can exercise the live API path:
/// `KILN_FIXTURES=/tmp/fx cargo test -p kiln-app dump_fixtures -- --ignored`
#[cfg(test)]
mod fixtures {
    use super::*;

    #[test]
    #[ignore]
    fn dump_fixtures() {
        let Ok(dir) = std::env::var("KILN_FIXTURES") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = seed(Utc::now());
        let w = |name: &str, v: serde_json::Value| {
            std::fs::write(dir.join(name), serde_json::to_vec_pretty(&v).unwrap()).unwrap()
        };
        let mut server = d.server.clone().unwrap();
        w("info.json", serde_json::to_value(&server).unwrap());
        server.setup_required = false;
        let me = MeResponse {
            user: d.me.clone().unwrap(),
            settings: d.settings.clone().unwrap(),
            server,
        };
        w("me.json", serde_json::to_value(&me).unwrap());
        w(
            "login.json",
            serde_json::to_value(LoginResponse {
                token: "tok".into(),
                user: d.me.clone().unwrap(),
            })
            .unwrap(),
        );
        let ideas: Vec<Plan> = d
            .plans
            .values()
            .filter(|p| p.status == PlanStatus::Drafting)
            .cloned()
            .collect();
        w(
            "board.json",
            serde_json::to_value(BoardResponse {
                tasks: d.tasks.clone(),
                ideas,
                projects: d.projects.clone(),
                users: d.users.clone(),
            })
            .unwrap(),
        );
        w("threads.json", serde_json::to_value(&d.threads).unwrap());
        for (id, t) in &d.thread_details {
            let mut t = t.clone();
            t.plan = d.plan_for_thread(*id).cloned();
            w(
                &format!("thread-{id}.json"),
                serde_json::to_value(&t).unwrap(),
            );
        }
        for (id, t) in &d.details {
            w(&format!("task-{id}.json"), serde_json::to_value(t).unwrap());
            w(
                &format!("logs-{id}.json"),
                serde_json::to_value(d.logs.get(id).cloned().unwrap_or_default()).unwrap(),
            );
        }
        w(
            "memory.json",
            serde_json::to_value(MemoryListResponse {
                items: d.memory.clone(),
                total: d.memory_total,
                pending_approvals: 2,
                runs_this_week: 61,
            })
            .unwrap(),
        );
        for (id, m) in &d.memory_details {
            w(
                &format!("memory-{id}.json"),
                serde_json::to_value(m).unwrap(),
            );
        }
        w(
            "credentials.json",
            serde_json::to_value(&d.credentials).unwrap(),
        );
        w(
            "harnesses.json",
            serde_json::to_value(d.harnesses.clone().unwrap()).unwrap(),
        );
        w(
            "notifications.json",
            serde_json::to_value(&d.notifications).unwrap(),
        );
        w("machine.json", serde_json::to_value(&d.machine).unwrap());
        w(
            "admin.json",
            serde_json::to_value(d.admin.clone().unwrap()).unwrap(),
        );
    }
}
