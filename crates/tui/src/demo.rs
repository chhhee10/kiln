//! `kiln --demo`: an in-process simulated daemon replicating the prototype's
//! mock data and its timer-driven simulation (tasks advance, logs stream,
//! QA fails and loops back to the Builder, the planner replies, credential
//! tests finish).
//!
//! [`DemoWorld`] is synchronous and deterministic (seeded RNG) so it can be
//! unit tested; [`DemoSource`] drives it from tokio timers.

use crate::app::default_catalog;
use crate::model::*;
use crate::source::{DataSource, EventTx};
use crate::theme::Tone;
use chrono::{Duration as CDuration, Local};
use kiln_types::event::LogLevel;
use kiln_types::{BoardColumn, Status, WorkerRole};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One simulation step (8 ticks × 120 ms in the prototype).
pub const STEP: Duration = Duration::from_millis(960);

const DOMAIN: &str = "company.dev";

fn w0() -> [Worker; 3] {
    [
        Worker::new("claude_sdk", "claude-sonnet-4.5"),
        Worker::new("codex", "gpt-5"),
        Worker::new("claude_sdk", "claude-opus-4.1"),
    ]
}

fn col(s: &str) -> BoardColumn {
    match s {
        "Idea" => BoardColumn::Idea,
        "Planned" => BoardColumn::Planned,
        "Building" => BoardColumn::Building,
        "QA" => BoardColumn::Qa,
        "Review" => BoardColumn::Review,
        "Ready" => BoardColumn::Ready,
        _ => BoardColumn::Done,
    }
}

fn st(s: &str) -> Status {
    match s {
        "running" => Status::Running,
        "passed" => Status::Passed,
        "failed" => Status::Failed,
        "human" => Status::NeedsHuman,
        _ => Status::Queued,
    }
}

fn next_col(c: BoardColumn) -> BoardColumn {
    let i = BoardColumn::ALL.iter().position(|x| *x == c).unwrap_or(0);
    BoardColumn::ALL[(i + 1).min(6)]
}

const TASKS: [(&str, &str, &str, &str, &str, f32, u32); 16] = [
    ("FP-151", "Config file schema validation", "CLI/SDK", "Idea", "queued", 0.0, 1),
    ("FP-153", "Dark mode for docs site", "Website", "Idea", "queued", 0.0, 1),
    ("FP-148", "Org-level audit log export", "Cloud", "Planned", "queued", 0.0, 1),
    ("FP-149", "Streaming responses in SDK", "CLI/SDK", "Planned", "queued", 0.0, 1),
    ("FP-150", "Team invites via email", "Cloud", "Planned", "queued", 0.0, 1),
    ("FP-145", "Usage chart on billing page", "Cloud", "Building", "running", 22.0, 1),
    ("FP-146", "Pricing page comparison table", "Website", "Building", "running", 64.0, 1),
    ("FP-142", "Add retry policy to SDK client", "CLI/SDK", "QA", "running", 48.0, 2),
    ("FP-139", "Webhook signature verification", "Cloud", "QA", "failed", 100.0, 2),
    ("FP-137", "Interactive prompts for `init`", "CLI/SDK", "Review", "running", 30.0, 1),
    ("FP-136", "SSO login screen polish", "Cloud", "Review", "human", 100.0, 3),
    ("FP-133", "Blog index pagination", "Website", "Ready", "passed", 100.0, 1),
    ("FP-131", "Rate-limit headers in SDK", "CLI/SDK", "Ready", "passed", 100.0, 2),
    ("FP-128", "Project switcher keyboard nav", "Cloud", "Done", "passed", 100.0, 1),
    ("FP-127", "Changelog page", "Website", "Done", "passed", 100.0, 1),
    ("FP-125", "Typed errors for SDK", "CLI/SDK", "Done", "passed", 100.0, 1),
];

const LOG_BUILDER: [(&str, &str); 8] = [
    ("INFO", "cloning repo into /work (worktree feat/retry-policy)"),
    ("STEP", "→ reading plan T1 + 3 memories from company brain"),
    ("INFO", "editing src/client/retry.ts (+118 −6)"),
    ("INFO", "editing src/client/backoff.ts (+46 −12)"),
    ("STEP", "→ running typecheck"),
    ("PASS", "tsc --noEmit  0 errors"),
    ("INFO", "committing “feat(sdk): retry policy with jitter”"),
    ("STEP", "→ handing off to QA"),
];
const LOG_QA: [(&str, &str); 14] = [
    ("INFO", "booting app container fp-cli:qa"),
    ("INFO", "playwright: chromium launched (headless)"),
    ("STEP", "→ run `fp init --yes` in tmp workspace"),
    ("PASS", "client/retry.test.ts  12 passed  0.8s"),
    ("PASS", "client/backoff.test.ts  9 passed  0.4s"),
    ("WARN", "flaky: network/timeout.test.ts retried once"),
    ("STEP", "→ simulate 503 from mock upstream ×3"),
    ("INFO", "observed backoff 200ms → 400ms → 800ms"),
    ("CAPT", "screenshot 02-retry-banner.png"),
    ("FAIL", "e2e/settings.spec.ts  expected toast “Saved”, got none after 5000ms"),
    ("INFO", "recording session.webm 00:48"),
    ("STEP", "→ drive: settings → toggle retries → save"),
    ("PASS", "client/errors.test.ts  14 passed  0.6s"),
    ("INFO", "GET /v1/health 200 12ms"),
];
const LOG_REVIEWER: [(&str, &str); 6] = [
    ("INFO", "loading diff (7 files, +212 −48) against plan"),
    ("STEP", "→ checking acceptance criteria 1–3"),
    ("PASS", "AC-1 retries 5xx + network errors"),
    ("WARN", "AC-2 backoff cap hard-coded at 30s"),
    ("STEP", "→ reading QA report"),
    ("INFO", "drafting verdict"),
];

fn level(s: &str) -> LogLevel {
    match s {
        "STEP" => LogLevel::Step,
        "PASS" => LogLevel::Pass,
        "WARN" => LogLevel::Warn,
        "FAIL" => LogLevel::Fail,
        "CAPT" => LogLevel::Capt,
        _ => LogLevel::Info,
    }
}

fn pool(role: WorkerRole) -> &'static [(&'static str, &'static str)] {
    match role {
        WorkerRole::Builder => &LOG_BUILDER,
        WorkerRole::Qa => &LOG_QA,
        _ => &LOG_REVIEWER,
    }
}

const TESTS: [(&str, &str, &str); 7] = [
    ("passed", "client/retry.test.ts", "12  0.8s"),
    ("passed", "client/backoff.test.ts", "9  0.4s"),
    ("passed", "client/errors.test.ts", "14  0.6s"),
    ("passed", "network/timeout.test.ts", "6  2.1s"),
    ("passed", "config/schema.test.ts", "8  0.2s"),
    ("failed", "e2e/settings.spec.ts", "1 14.2s"),
    ("running", "e2e/retry-banner.spec.ts", "—"),
];
const ARTS: [(&str, &str, &str); 5] = [
    ("▣", "01-init-prompt.png", "1440×900 png · 412 KB"),
    ("▣", "02-retry-banner.png", "1440×900 png · 388 KB"),
    ("▶", "session.webm", "00:48 webm · 6.1 MB"),
    ("≡", "qa.log", "2.1 MB · 3,214 lines"),
    ("±", "diff", "+212 −48 · 7 files"),
];

const ACT: [(&str, &str, &str, Tone); 10] = [
    ("AR", "◇", "plan “Retry & backoff” drafted v3 · 3 tasks", Tone::Accent),
    ("qa", "◐", "FP-142 QA started · codex / gpt-5", Tone::RUNNING),
    ("qa", "✕", "FP-139 QA failed · e2e/webhooks.spec expected 401", Tone::FAILED),
    ("JL", "✓", "PR #433 opened · Blog index pagination", Tone::PASSED),
    ("bld", "◐", "FP-145 builder committed 3 files", Tone::RUNNING),
    ("MK", "◑", "memory “E2E tests must seed randomness” → Company (pending)", Tone::Muted),
    ("sys", "!", "FP-136 escalated after 3 loops · Alex notified", Tone::HUMAN),
    ("bld", "✓", "FP-146 typecheck passed", Tone::PASSED),
    ("SD", "○", "FP-150 queued · waiting for a container slot", Tone::QUEUED),
    ("rev", "◐", "FP-137 reviewer reading diff", Tone::RUNNING),
];

const REPLIES: [&str; 3] = [
    "Updated to v4. I tightened the acceptance criteria on T1 and added a check for Retry-After headers.",
    "Done. T2 now also covers --quiet mode. No change to the workers.",
    "Noted. I moved QA for T1 to gpt-5-codex, since it handled the Playwright flows better in past runs.",
];

fn inbox() -> Vec<InboxItem> {
    let i = |g: &str, tone, t: &str, s: &str, task: Option<&str>| InboxItem {
        glyph: g.into(),
        tone,
        title: t.into(),
        sub: s.into(),
        task: task.map(str::to_string),
    };
    vec![
        i("!", Tone::HUMAN, "FP-136 SSO login polish — needs you", "escalated after 3 loops · 14m", Some("FP-136")),
        i(
            "✕",
            Tone::FAILED,
            "FP-139 Webhook signatures — QA failed",
            "attempt 2/3 · e2e/webhooks.spec · 3m",
            Some("FP-139"),
        ),
        i("◇", Tone::Accent, "Plan “Retry & backoff” awaits approval", "3 tasks · CLI/SDK · press 3", None),
        i("✓", Tone::PASSED, "PR #433 Blog index pagination ready", "+88 −12 · reviewer approved · 1h", Some("FP-133")),
        i("✓", Tone::PASSED, "PR #431 Rate-limit headers ready", "+64 −9 · reviewer approved · 2h", Some("FP-131")),
        i("◑", Tone::Muted, "2 memories await Company approval", "Morgan, Sam · open on desktop", None),
    ]
}

const PLANS: [(&str, &str, &str, &str, Tone); 5] = [
    ("◇", "Retry & backoff for SDK client", "CLI/SDK · 3 tasks", "needs approval", Tone::Accent),
    ("◐", "Usage chart on billing page", "Cloud · 2 tasks", "dispatched", Tone::RUNNING),
    ("◐", "Pricing comparison table", "Website · 1 task", "dispatched", Tone::RUNNING),
    ("○", "Org audit log export", "Cloud · 4 tasks", "draft", Tone::QUEUED),
    ("✓", "Typed errors for SDK", "CLI/SDK · 2 tasks", "merged", Tone::PASSED),
];

fn plan0() -> PlanDraft {
    let t = |id: &str, title: &str, open: bool, ac: &[&str], workers: [Worker; 3]| PlanTask {
        id: id.into(),
        title: title.into(),
        open,
        ac: ac.iter().map(|s| s.to_string()).collect(),
        workers,
        description: String::new(),
        files_hint: vec![],
        depends_on: vec![],
    };
    PlanDraft {
        key: "plan-0".into(),
        title: "Retry & backoff for SDK client".into(),
        version: 3,
        project: "CLI/SDK".into(),
        byline: "drafted by Planner with Alex · 3 memories in context".into(),
        description: "Retries for the SDK HTTP client: exponential backoff with jitter, configurable caps, retry state in CLI output.".into(),
        status: "needs approval".into(),
        tasks: vec![
            t(
                "T1",
                "Retry policy core with backoff + jitter",
                true,
                &[
                    "Retries 5xx and network errors up to maxRetries",
                    "Backoff cap configurable via config (default 30s)",
                    "Tests are deterministic with a seeded RNG",
                ],
                w0(),
            ),
            t(
                "T2",
                "Show retry state in CLI output",
                true,
                &["“retrying (2/3) in 400ms” line with dim timestamp", "Silent in --json mode"],
                w0(),
            ),
            t(
                "T3",
                "Docs page: configuring retries",
                false,
                &["New page under /docs/sdk/retries", "Code sample matches the SDK API"],
                [
                    Worker::new("claude_sdk", "claude-sonnet-4.5"),
                    Worker::new("claude_sdk", "claude-sonnet-4.5"),
                    Worker::new("codex", "gpt-5"),
                ],
            ),
        ],
    }
}

fn msgs0() -> Vec<ChatMsg> {
    let m = |i: usize, you: bool, t: &str| ChatMsg { id: Some(format!("m{i}")), you, text: t.into() };
    vec![
        m(
            0,
            true,
            "SDK calls fail hard on any 503 from upstream. Add retries with backoff, and make it visible in the CLI.",
        ),
        m(
            1,
            false,
            "I checked the company brain. There's an approved convention for SDK retries: full jitter, base 200ms, respect Retry-After.\nTwo questions: (1) should the cap be configurable per client, or global only? (2) Should retries be on by default?",
        ),
        m(2, true, "Configurable per client, default 30s. On by default with maxRetries 3."),
        m(
            3,
            false,
            "Got it. I drafted a 3-task plan; open it with 3. T3 goes to the marketing site docs, so its Reviewer runs on Codex for a second opinion. Edit anything there, then press a to approve and dispatch.",
        ),
    ]
}

fn creds0() -> Vec<CredView> {
    let c = |key: &str, n: &str, ty: &str, f: Vec<CredField>, stt: &str, s: Status| CredView {
        key: key.into(),
        name: n.into(),
        kind_label: ty.into(),
        fields: f,
        status_text: stt.into(),
        status: s,
        is_new: false,
    };
    vec![
        c(
            "anthropic",
            "Anthropic",
            "API key",
            vec![CredField::new("api key", "sk-ant-api03-Zq8mN4f2a", true)],
            "valid · used 2h ago",
            Status::Passed,
        ),
        c(
            "openai",
            "OpenAI",
            "API key",
            vec![CredField::new("api key", "sk-proj-Hk2Lx91cd", true)],
            "valid · used 12m ago",
            Status::Passed,
        ),
        c(
            "github",
            "GitHub",
            "OAuth token",
            vec![CredField::new("token", "ghu_7Qa93kdPz", true), CredField::new("scopes", "repo, workflow", false)],
            "expires in 6 days",
            Status::NeedsHuman,
        ),
        c(
            "litellm",
            "LiteLLM proxy",
            "URL + key",
            vec![
                CredField::new("base url", "https://litellm.tailnet:4000", false),
                CredField::new("api key", "sk-llm-77c0de", true),
            ],
            "reachable · 34ms",
            Status::Passed,
        ),
    ]
}

fn checklist0() -> Vec<ChecklistItem> {
    [
        (Status::Passed, "model provider key (Anthropic or OpenAI)"),
        (Status::Passed, "GitHub token with repo scope"),
        (Status::NeedsHuman, "GitHub token expires in 6 days · rotate soon"),
        (Status::Passed, "LiteLLM proxy reachable from containers"),
        (Status::Queued, "optional: test-account OAuth for QA login flows"),
    ]
    .into_iter()
    .map(|(status, t)| ChecklistItem { status, text: t.into() })
    .collect()
}

#[derive(Clone, Debug)]
struct DemoTask {
    view: TaskView,
    /// QA failures still to simulate before this task passes QA.
    qa_fail: u32,
    /// Steps left in the failed state before looping back to the Builder.
    fail_wait: u32,
}

/// Work scheduled for later (planner replies, credential tests…).
#[derive(Clone, Debug, PartialEq)]
pub enum Deferred {
    CredTested(String),
    PlannerReply,
    Pair,
    SetupDone(usize),
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // short-lived, moved straight into the channel
pub enum Effect {
    Now(DataEvent),
    Later(Duration, Deferred),
}

pub struct DemoWorld {
    tasks: Vec<DemoTask>,
    /// Simulation step counter (prototype `n`).
    pub n: usize,
    act_n: usize,
    followed: String,
    stage_started: chrono::DateTime<Local>,
    cpu: Vec<f32>,
    rng: u64,
    creds: Vec<CredView>,
    plan: PlanDraft,
    msgs: usize,
    saved: usize,
    pub first_run: bool,
    email: String,
}

impl DemoWorld {
    pub fn new(first_run: bool) -> Self {
        let tasks = TASKS
            .iter()
            .map(|(id, t, p, c, s, prog, att)| DemoTask {
                view: TaskView {
                    key: id.to_string(),
                    title: t.to_string(),
                    project: p.to_string(),
                    plan: if *id == "FP-142" { "Retry & backoff".into() } else { t.to_string() },
                    column: col(c),
                    status: st(s),
                    progress: *prog,
                    attempt: *att,
                    loop_cap: 3,
                    workers: w0(),
                    pr: match *id {
                        "FP-142" => Some(418),
                        "FP-133" => Some(433),
                        "FP-131" => Some(431),
                        _ => None,
                    },
                },
                // QA on FP-142 fails once more, then the task loops to attempt 3/3.
                qa_fail: u32::from(*id == "FP-142"),
                fail_wait: 0,
            })
            .collect();
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let cpu = (0..40)
            .map(|i| {
                let r = next_rand(&mut rng) * 8.0;
                (50.0 + ((i as f32 / 3.0).sin() * 14.0 + r).round()).clamp(0.0, 100.0)
            })
            .collect();
        Self {
            tasks,
            n: 24,
            act_n: 24,
            followed: "FP-142".into(),
            stage_started: Local::now() - CDuration::seconds(125),
            cpu,
            rng,
            creds: creds0(),
            plan: plan0(),
            msgs: 4,
            saved: 0,
            first_run,
            email: "alex@company.dev".into(),
        }
    }

    fn views(&self) -> Vec<TaskView> {
        self.tasks.iter().map(|t| t.view.clone()).collect()
    }

    fn task_mut(&mut self, key: &str) -> Option<&mut DemoTask> {
        self.tasks.iter_mut().find(|t| t.view.key == key)
    }

    pub fn workers(&self) -> usize {
        self.tasks.iter().filter(|t| t.view.is_worker()).count()
    }

    fn machine(&self) -> MachineView {
        let ctr = (self.workers() as u32 + 1).min(8);
        MachineView {
            host: "mac-mini".into(),
            cpu: self.cpu.clone(),
            ram_used_gb: 8.6 + ctr as f32 * 2.1,
            ram_total_gb: 32.0,
            ctr_used: ctr,
            ctr_slots: 8,
            queue: self.tasks.iter().filter(|t| t.view.column == BoardColumn::Planned).count() as u32,
            est_wait: "~4m".into(),
            host_line: "up 18d · 100.84.12.7 · M4 Pro 12c · 32 GB".into(),
        }
    }

    fn activity(&self, i: usize, at: chrono::DateTime<Local>) -> ActivityItem {
        let (a, g, t, tone) = ACT[i % ACT.len()];
        ActivityItem { at, actor: a.into(), glyph: g.into(), tone, text: t.into() }
    }

    fn line(&self, role: WorkerRole, i: usize, at: chrono::DateTime<Local>) -> LogLineView {
        let p = pool(role);
        let (lvl, m) = p[i % p.len()];
        LogLineView { idx: 0, ts: at.format("%H:%M:%S").to_string(), role, level: level(lvl), msg: m.into() }
    }

    fn backfill(&self, key: &str) -> DataEvent {
        let now = Local::now();
        let start = self.n.saturating_sub(120);
        let mut lines = Vec::new();
        for i in start..self.n {
            let at = now - CDuration::seconds(((self.n - i) * 3) as i64);
            for role in ROLES {
                lines.push(self.line(role, i, at));
            }
        }
        DataEvent::LogBackfill { task: key.into(), lines }
    }

    fn run_detail(&self, key: &str) -> RunDetail {
        let Some(t) = self.tasks.iter().find(|t| t.view.key == key) else {
            return RunDetail::default();
        };
        let past_build =
            matches!(t.view.column, BoardColumn::Qa | BoardColumn::Review | BoardColumn::Ready | BoardColumn::Done);
        let mut d = RunDetail { task: key.into(), ..Default::default() };
        if past_build {
            d.tests = TESTS
                .iter()
                .map(|(s, n, m)| TestRow { status: st(s), name: n.to_string(), meta: m.to_string() })
                .collect();
            d.passed = 41;
            d.failed = 1;
            d.running = 1;
            d.total = 43;
            d.failing = Some("settings.spec:42 expected toast “Saved”".into());
            d.artifacts = ARTS
                .iter()
                .enumerate()
                .map(|(i, (g, n, m))| ArtifactView {
                    key: format!("a{i}"),
                    glyph: g.to_string(),
                    name: n.to_string(),
                    meta: m.to_string(),
                    handle: String::new(),
                    url: None,
                })
                .collect();
        }
        d.stage_secs = [Some(252), Some(400), Some(118)];
        if let Some(si) = t.view.stage() {
            d.stage_started[si] = Some(self.stage_started);
        }
        d
    }

    /// Everything the UI needs at startup.
    pub fn initial(&self) -> Vec<DataEvent> {
        let now = Local::now();
        let mut ev = vec![
            DataEvent::Server(ServerView {
                host: "mac-mini".into(),
                tailnet: "mac-mini.tailnet".into(),
                latency_ms: Some(18),
                error: None,
                allowed_domain: Some(DOMAIN.into()),
            }),
            DataEvent::Tasks(self.views()),
            DataEvent::Machine(self.machine()),
            DataEvent::Inbox(inbox()),
            DataEvent::ActivityAll(
                (0..10)
                    .map(|i| {
                        self.activity(
                            (self.act_n + ACT.len() * 100 - i) % ACT.len(),
                            now - CDuration::seconds(i as i64 * 7),
                        )
                    })
                    .collect(),
            ),
            DataEvent::Plans(
                PLANS
                    .iter()
                    .enumerate()
                    .map(|(i, (g, t, s, stt, tone))| PlanSummary {
                        key: format!("plan-{i}"),
                        glyph: g.to_string(),
                        tone: *tone,
                        title: t.to_string(),
                        sub: s.to_string(),
                        status: stt.to_string(),
                    })
                    .collect(),
            ),
            DataEvent::Plan(self.plan.clone()),
            DataEvent::Catalog(default_catalog()),
            DataEvent::Threads(
                PLANS
                    .iter()
                    .enumerate()
                    .map(|(i, (_, t, _, stt, tone))| ThreadView {
                        key: format!("thread-{i}"),
                        title: t.to_string(),
                        status: stt.to_string(),
                        tone: *tone,
                    })
                    .collect(),
            ),
            DataEvent::Conversation(Conversation {
                thread: Some("thread-0".into()),
                title: self.plan.title.clone(),
                model: "opus-4.1".into(),
                memories: 3,
                msgs: msgs0(),
                pending: None,
            }),
            DataEvent::Credentials(self.creds.clone()),
            DataEvent::Checklist(checklist0()),
            DataEvent::Run(self.run_detail(&self.followed)),
            self.backfill(&self.followed),
            DataEvent::Synced,
        ];
        if self.first_run {
            ev.insert(1, DataEvent::SetupRequired { allowed_domain: None });
        }
        ev
    }

    /// Advance the simulation by one step.
    pub fn step(&mut self) -> Vec<DataEvent> {
        let now = Local::now();
        self.n += 1;
        let mut ev = Vec::new();
        let followed = self.followed.clone();
        let mut stage_changed = false;
        let mut acts = Vec::new();
        for t in &mut self.tasks {
            let v = &mut t.view;
            if v.status == Status::Failed && t.fail_wait > 0 {
                t.fail_wait -= 1;
                if t.fail_wait == 0 {
                    if v.attempt >= v.loop_cap {
                        v.status = Status::NeedsHuman;
                        acts.push((
                            "sys",
                            "!",
                            format!("{} escalated after {} loops · team notified", v.key, v.loop_cap),
                            Tone::HUMAN,
                        ));
                    } else {
                        v.attempt += 1;
                        v.column = BoardColumn::Building;
                        v.status = Status::Running;
                        v.progress = 0.0;
                        acts.push((
                            "sys",
                            "◐",
                            format!("{} looping to builder · attempt {}/{}", v.key, v.attempt, v.loop_cap),
                            Tone::RUNNING,
                        ));
                    }
                    stage_changed |= v.key == followed;
                }
                continue;
            }
            if !v.is_worker() {
                continue;
            }
            v.progress += if v.key == followed { 1.6 } else { 0.9 };
            if v.progress >= 100.0 {
                if v.column == BoardColumn::Qa && t.qa_fail > 0 {
                    t.qa_fail -= 1;
                    t.fail_wait = 4;
                    v.status = Status::Failed;
                    v.progress = 100.0;
                    acts.push((
                        "qa",
                        "✕",
                        format!("{} QA failed · e2e/settings.spec expected toast “Saved”", v.key),
                        Tone::FAILED,
                    ));
                } else {
                    v.progress = 0.0;
                    v.column = next_col(v.column);
                    if v.column == BoardColumn::Ready {
                        v.status = Status::Passed;
                        v.progress = 100.0;
                        acts.push(("rev", "✓", format!("{} reviewer approved · PR ready", v.key), Tone::PASSED));
                    }
                }
                stage_changed |= v.key == followed;
            }
        }
        for (a, g, t, tone) in acts {
            ev.push(DataEvent::Activity(ActivityItem { at: now, actor: a.into(), glyph: g.into(), tone, text: t }));
        }
        if stage_changed {
            self.stage_started = now;
        }
        let last = self.cpu.last().copied().unwrap_or(50.0);
        let delta = (next_rand(&mut self.rng) * 16.0 - 8.0).round();
        self.cpu.remove(0);
        self.cpu.push((last + delta).clamp(20.0, 96.0));
        ev.push(DataEvent::Tasks(self.views()));
        ev.push(DataEvent::Machine(self.machine()));
        for role in ROLES {
            ev.push(DataEvent::Log { task: followed.clone(), line: self.line(role, self.n - 1, now) });
        }
        if stage_changed {
            ev.push(DataEvent::Run(self.run_detail(&followed)));
        }
        if self.n.is_multiple_of(7) {
            self.act_n += 1;
            ev.push(DataEvent::Activity(self.activity(self.act_n % ACT.len(), now)));
        }
        ev.push(DataEvent::Synced);
        ev
    }

    fn flash(text: impl Into<String>, tone: Tone) -> Effect {
        Effect::Now(DataEvent::Flash(FlashMsg::new(text, tone)))
    }

    fn upsert(&self, key: &str) -> Vec<Effect> {
        self.tasks
            .iter()
            .find(|t| t.view.key == key)
            .map(|t| vec![Effect::Now(DataEvent::TaskUpsert(t.view.clone()))])
            .unwrap_or_default()
    }

    /// Apply a command from the UI.
    pub fn handle(&mut self, cmd: Cmd) -> Vec<Effect> {
        use Effect::*;
        match cmd {
            Cmd::Login { email, password } => {
                if !email.ends_with(&format!("@{DOMAIN}")) {
                    vec![Now(DataEvent::LoginError {
                        message: "✕ not on the allowlist · ask an admin to create your account".into(),
                        password_field: false,
                    })]
                } else if password.is_empty() {
                    vec![Now(DataEvent::LoginError { message: "✕ password required".into(), password_field: true })]
                } else {
                    self.email = email.clone();
                    vec![Now(DataEvent::SignedIn {
                        email: email.clone(),
                        flash: Some(FlashMsg::ok(format!("✓ signed in as {email} · session saved"))),
                    })]
                }
            }
            Cmd::Setup { email, .. } => {
                self.email = email;
                vec![Later(Duration::from_millis(600), Deferred::SetupDone(0))]
            }
            Cmd::RegisterProject { .. } => {
                vec![Later(Duration::from_millis(600), Deferred::SetupDone(1))]
            }
            Cmd::Logout => vec![Now(DataEvent::SignedOut)],
            Cmd::Retry(key) => {
                let Some(t) = self.task_mut(&key) else {
                    return vec![];
                };
                let v = &mut t.view;
                v.status = Status::Running;
                v.progress = 0.0;
                v.attempt = (v.attempt + 1).min(v.loop_cap);
                if v.stage().is_none() {
                    v.column = BoardColumn::Building;
                }
                t.fail_wait = 0;
                let msg = format!("◐ {} restarted · attempt {}/{}", v.key, v.attempt, v.loop_cap);
                let mut e = self.upsert(&key);
                e.push(Self::flash(msg, Tone::RUNNING));
                e
            }
            Cmd::Stop(key) => {
                let Some(t) = self.task_mut(&key) else {
                    return vec![];
                };
                t.view.status = Status::Queued;
                t.fail_wait = 0;
                let mut e = self.upsert(&key);
                e.push(Self::flash(format!("○ {key} stopped · container released"), Tone::QUEUED));
                e
            }
            Cmd::Escalate(key) => {
                let Some(t) = self.task_mut(&key) else {
                    return vec![];
                };
                t.view.status = Status::NeedsHuman;
                t.fail_wait = 0;
                let mut e = self.upsert(&key);
                e.push(Self::flash(format!("! {key} escalated · team notified on Android"), Tone::HUMAN));
                e
            }
            Cmd::Feedback { task, .. } => {
                let Some(t) = self.task_mut(&task) else {
                    return vec![];
                };
                let v = &mut t.view;
                v.column = BoardColumn::Building;
                v.status = Status::Running;
                v.progress = 0.0;
                v.attempt = (v.attempt + 1).min(v.loop_cap);
                t.fail_wait = 0;
                let mut e = self.upsert(&task);
                e.push(Self::flash(format!("✓ feedback sent · builder restarted on {task}"), Tone::PASSED));
                e
            }
            Cmd::Follow(key) => {
                if self.followed != key {
                    self.followed = key.clone();
                    self.stage_started = Local::now() - CDuration::seconds(65);
                }
                vec![Now(DataEvent::Run(self.run_detail(&key))), Now(self.backfill(&key))]
            }
            Cmd::Approve(draft) => {
                let n = draft.tasks.len();
                let new: Vec<DemoTask> = draft
                    .tasks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| DemoTask {
                        view: TaskView {
                            key: format!("FP-{}", 160 + i),
                            title: t.title.clone(),
                            project: if i == 2 { "Website".into() } else { "CLI/SDK".into() },
                            plan: "Retry & backoff".into(),
                            column: if i == 0 { BoardColumn::Building } else { BoardColumn::Planned },
                            status: if i == 0 { Status::Running } else { Status::Queued },
                            progress: 0.0,
                            attempt: 1,
                            loop_cap: 3,
                            workers: t.workers.clone(),
                            pr: None,
                        },
                        qa_fail: 0,
                        fail_wait: 0,
                    })
                    .collect();
                self.tasks.retain(|t| !t.view.key.starts_with("FP-16"));
                let mut tasks = new;
                tasks.append(&mut self.tasks);
                self.tasks = tasks;
                vec![
                    Now(DataEvent::Tasks(self.views())),
                    Now(DataEvent::Dispatched),
                    Self::flash(
                        format!("✓ dispatched {n} tasks · 1 building, {} queued", n.saturating_sub(1)),
                        Tone::PASSED,
                    ),
                ]
            }
            Cmd::OpenPlan(key) => {
                if key == self.plan.key {
                    vec![Now(DataEvent::Plan(self.plan.clone()))]
                } else {
                    vec![Self::flash("○ demo · only the “Retry & backoff” plan is editable", Tone::QUEUED)]
                }
            }
            Cmd::OpenThread(key) => {
                if key == "thread-0" {
                    vec![]
                } else {
                    vec![Self::flash("○ demo · only the “Retry & backoff” thread has messages", Tone::QUEUED)]
                }
            }
            Cmd::NewThread => vec![],
            Cmd::SendMessage { thread, text } => {
                self.msgs += 1;
                vec![
                    Now(DataEvent::ChatMessage {
                        thread,
                        msg: ChatMsg { id: Some(format!("m{}", self.msgs)), you: true, text },
                    }),
                    Later(Duration::from_millis(1700), Deferred::PlannerReply),
                ]
            }
            Cmd::SaveCredential(mut c) => {
                let old = c.key.clone();
                if c.is_new {
                    self.saved += 1;
                    c.key = format!("cred-{}", self.saved);
                    c.is_new = false;
                    c.status = Status::Queued;
                    c.status_text = "not tested".into();
                }
                match self.creds.iter_mut().find(|x| x.key == c.key) {
                    Some(x) => *x = c.clone(),
                    None => self.creds.push(c.clone()),
                }
                let name = c.name.clone();
                vec![
                    Now(DataEvent::Credential { key: old, cred: c }),
                    Self::flash(format!("✓ {name} saved · encrypted at rest"), Tone::PASSED),
                ]
            }
            Cmd::TestCredential(key) => vec![Later(Duration::from_millis(1200), Deferred::CredTested(key))],
            Cmd::OpenArtifact { artifact, .. } => {
                vec![Self::flash(format!("↗ opened {} externally (Preview / mpv)", artifact.name), Tone::Muted)]
            }
            Cmd::PairStart => vec![Later(Duration::from_millis(400), Deferred::Pair)],
            Cmd::Quit => vec![],
        }
    }

    /// Finish deferred work.
    pub fn deferred(&mut self, d: Deferred) -> Vec<DataEvent> {
        match d {
            Deferred::CredTested(key) => {
                let Some(c) = self.creds.iter_mut().find(|c| c.key == key) else {
                    return vec![];
                };
                let github = c.name == "GitHub";
                let (status, text) = if github {
                    (Status::NeedsHuman, "valid · expires in 6 days".to_string())
                } else {
                    (Status::Passed, "valid · tested just now · 212ms".to_string())
                };
                c.status = status;
                c.status_text = text.clone();
                vec![
                    DataEvent::CredentialTested { key, status, text },
                    DataEvent::Flash(FlashMsg::ok(format!("✓ {} · connection ok · 212ms", c.name))),
                ]
            }
            Deferred::PlannerReply => {
                self.plan.version += 1;
                self.msgs += 1;
                let reply = REPLIES[(self.msgs / 2) % 3];
                vec![
                    DataEvent::PlannerPending { thread: None, label: None },
                    DataEvent::ChatMessage {
                        thread: None,
                        msg: ChatMsg { id: Some(format!("m{}", self.msgs)), you: false, text: reply.into() },
                    },
                    DataEvent::Plan(self.plan.clone()),
                ]
            }
            Deferred::Pair => vec![DataEvent::Pair(PairView {
                uri: "kiln://pair?url=https%3A%2F%2Fmac-mini.tailnet.ts.net&code=K7Q29XMA".into(),
                code: "K7Q2-9XMA".into(),
                expires: (Local::now() + CDuration::minutes(10)).format("%H:%M").to_string(),
            })],
            Deferred::SetupDone(0) => vec![
                DataEvent::SignedIn { email: self.email.clone(), flash: None },
                DataEvent::Server(ServerView {
                    host: "mac-mini".into(),
                    tailnet: "mac-mini.tailnet".into(),
                    latency_ms: Some(18),
                    error: None,
                    allowed_domain: Some(DOMAIN.into()),
                }),
                DataEvent::SetupStep { step: 0 },
            ],
            Deferred::SetupDone(_) => vec![
                DataEvent::SetupStep { step: 1 },
                DataEvent::Flash(FlashMsg::ok("✓ setup complete · project registered · qa.yaml found")),
            ],
        }
    }
}

fn next_rand(state: &mut u64) -> f32 {
    // xorshift64*
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    ((x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 40) as f32) / (1u64 << 24) as f32
}

/// Tokio driver for [`DemoWorld`].
pub struct DemoSource {
    world: Arc<Mutex<DemoWorld>>,
    tx: EventTx,
}

impl DemoSource {
    /// Emit the initial state and start the simulation timer. Must be called
    /// inside a tokio runtime.
    pub fn start(tx: EventTx, first_run: bool) -> Self {
        let world = Arc::new(Mutex::new(DemoWorld::new(first_run)));
        if let Ok(w) = world.lock() {
            for e in w.initial() {
                let _ = tx.send(e);
            }
        }
        let (w2, tx2) = (world.clone(), tx.clone());
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(STEP);
            iv.tick().await;
            loop {
                iv.tick().await;
                let evs = match w2.lock() {
                    Ok(mut w) => w.step(),
                    Err(_) => return,
                };
                for e in evs {
                    if tx2.send(e).is_err() {
                        return;
                    }
                }
            }
        });
        Self { world, tx }
    }
}

impl DataSource for DemoSource {
    fn handle(&mut self, cmd: Cmd) {
        let effects = match self.world.lock() {
            Ok(mut w) => w.handle(cmd),
            Err(_) => return,
        };
        for e in effects {
            match e {
                Effect::Now(ev) => {
                    let _ = self.tx.send(ev);
                }
                Effect::Later(after, d) => {
                    let (w, tx) = (self.world.clone(), self.tx.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        let evs = match w.lock() {
                            Ok(mut w) => w.deferred(d),
                            Err(_) => return,
                        };
                        for ev in evs {
                            let _ = tx.send(ev);
                        }
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tasks_of(evs: &[DataEvent]) -> Vec<TaskView> {
        evs.iter().rev().find_map(|e| if let DataEvent::Tasks(t) = e { Some(t.clone()) } else { None }).unwrap()
    }

    #[test]
    fn tasks_advance_and_logs_stream() {
        let mut w = DemoWorld::new(false);
        let evs = w.step();
        let t = tasks_of(&evs);
        let fp142 = t.iter().find(|t| t.key == "FP-142").unwrap();
        assert!((fp142.progress - 49.6).abs() < 0.01);
        let fp145 = t.iter().find(|t| t.key == "FP-145").unwrap();
        assert!((fp145.progress - 22.9).abs() < 0.01);
        assert_eq!(evs.iter().filter(|e| matches!(e, DataEvent::Log { .. })).count(), 3);
    }

    #[test]
    fn qa_fails_then_loops_back_to_builder() {
        let mut w = DemoWorld::new(false);
        let mut saw_fail = false;
        let mut looped = false;
        for _ in 0..60 {
            let evs = w.step();
            let t = tasks_of(&evs);
            let fp = t.iter().find(|t| t.key == "FP-142").unwrap().clone();
            if fp.status == Status::Failed {
                saw_fail = true;
                assert_eq!(fp.column, BoardColumn::Qa);
            }
            if saw_fail && fp.column == BoardColumn::Building {
                assert_eq!(fp.attempt, 3);
                assert_eq!(fp.status, Status::Running);
                looped = true;
                break;
            }
        }
        assert!(saw_fail && looped);
    }

    #[test]
    fn login_rules() {
        let mut w = DemoWorld::new(false);
        let e = w.handle(Cmd::Login { email: "x@evil.com".into(), password: "p".into() });
        assert!(matches!(&e[0], Effect::Now(DataEvent::LoginError { message, .. }) if message.contains("allowlist")));
        let e = w.handle(Cmd::Login { email: "alex@company.dev".into(), password: "".into() });
        assert!(matches!(&e[0], Effect::Now(DataEvent::LoginError { password_field: true, .. })));
        let e = w.handle(Cmd::Login { email: "alex@company.dev".into(), password: "pw".into() });
        assert!(matches!(&e[0], Effect::Now(DataEvent::SignedIn { .. })));
    }

    #[test]
    fn dispatch_creates_tasks() {
        let mut w = DemoWorld::new(false);
        let e = w.handle(Cmd::Approve(plan0()));
        let Effect::Now(DataEvent::Tasks(t)) = &e[0] else { panic!() };
        assert_eq!(t[0].key, "FP-160");
        assert_eq!(t[0].status, Status::Running);
        assert_eq!(t[1].status, Status::Queued);
        assert_eq!(t[2].project, "Website");
        assert!(
            matches!(&e[2], Effect::Now(DataEvent::Flash(f)) if f.text == "✓ dispatched 3 tasks · 1 building, 2 queued")
        );
    }

    #[test]
    fn planner_reply_bumps_version() {
        let mut w = DemoWorld::new(false);
        let e = w.handle(Cmd::SendMessage { thread: None, text: "hi".into() });
        assert!(matches!(e[1], Effect::Later(_, Deferred::PlannerReply)));
        let ev = w.deferred(Deferred::PlannerReply);
        assert!(matches!(&ev[2], DataEvent::Plan(p) if p.version == 4));
    }
}
