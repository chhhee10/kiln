//! Live mode: talks to a `kilnd` daemon through `kiln-api-client` (HTTP for
//! loads and actions, the WebSocket for live events) and maps everything into
//! the shared view model.

use crate::app::PENDING_LABEL;
use crate::model::*;
use crate::session::Session;
use crate::source::{DataSource, EventTx};
use crate::theme::Tone;
use crate::util::{fmt_ago, fmt_size};
use chrono::Local;
use kiln_api_client::{Client, ClientError};
use kiln_types::api::*;
use kiln_types::event::{ClientMessage, LogLine, ServerEvent};
use kiln_types::model::*;
use kiln_types::plan::{PlanSpec, PlanWorkers, TaskSpec, WorkerChoice};
use kiln_types::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Default)]
struct State {
    user: Option<User>,
    tasks: HashMap<TaskId, Task>,
    projects: HashMap<ProjectId, Project>,
    plans: HashMap<PlanId, Plan>,
    ideas: Vec<Plan>,
    threads: Vec<Thread>,
    /// Plan id → thread id, for opening plans.
    plan_threads: HashMap<PlanId, ThreadId>,
    current_plan: Option<PlanId>,
    current_thread: Option<ThreadId>,
    memories: HashMap<ThreadId, u32>,
    catalog: Option<HarnessCatalog>,
    creds: Vec<CredentialSummary>,
    notifications: Vec<Notification>,
    followed: Option<TaskId>,
    cpu: Vec<f32>,
}

struct Inner {
    client: Client,
    tx: EventTx,
    session_path: PathBuf,
    state: Mutex<State>,
    follow: Mutex<Option<mpsc::Sender<ClientMessage>>>,
    ws: Mutex<Vec<JoinHandle<()>>>,
}

pub struct LiveSource {
    inner: Arc<Inner>,
}

fn err_text(e: &ClientError) -> String {
    match e {
        ClientError::Api { error, .. } => error.error.clone(),
        // reqwest's top-level message ("error sending request for url …") hides the cause.
        ClientError::Http(m) if m.contains("error sending request") => "connection failed".into(),
        ClientError::Http(m) => m.clone(),
        ClientError::Decode(m) => format!("unexpected response: {m}"),
        ClientError::Incompatible { .. } => e.to_string(),
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "terminal".into())
}

/// Open a file or URL with the platform opener (`open` / `xdg-open`).
pub fn open_external(target: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

fn short_host(h: &str) -> String {
    h.split('.').next().unwrap_or(h).to_string()
}

impl LiveSource {
    /// Start: check the server, resume a saved session (unless `force_login`).
    pub fn start(client: Client, tx: EventTx, session_path: PathBuf, force_login: bool) -> Self {
        let inner = Arc::new(Inner {
            client,
            tx,
            session_path,
            state: Mutex::new(State::default()),
            follow: Mutex::new(None),
            ws: Mutex::new(Vec::new()),
        });
        let i = inner.clone();
        tokio::spawn(async move {
            // Keep checking until the daemon answers (it may start after us).
            let info = loop {
                if let Some(info) = i.check_server().await {
                    break info;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                if i.tx.is_closed() {
                    return;
                }
            };
            if info.setup_required {
                i.send(DataEvent::SetupRequired { allowed_domain: info.allowed_domain });
                return;
            }
            if force_login || i.client.token().is_none() {
                return;
            }
            match i.client.me().await {
                Ok(me) => {
                    i.send(DataEvent::SignedIn { email: me.user.email.clone(), flash: None });
                    i.state.lock().unwrap().user = Some(me.user);
                    i.clone().bootstrap().await;
                }
                Err(e) if e.is_unauthorized() => {
                    i.send(DataEvent::Flash(FlashMsg::new("! session expired · sign in again", Tone::HUMAN)));
                }
                Err(e) => i.send(DataEvent::Flash(FlashMsg::err(format!("✕ {}", err_text(&e))))),
            }
        });
        Self { inner }
    }
}

impl DataSource for LiveSource {
    fn handle(&mut self, cmd: Cmd) {
        let i = self.inner.clone();
        tokio::spawn(async move { i.handle(cmd).await });
    }
}

impl Inner {
    fn send(&self, e: DataEvent) {
        let _ = self.tx.send(e);
    }

    fn flash_err(&self, prefix: &str, e: &ClientError) {
        self.send(DataEvent::Flash(FlashMsg::err(format!("✕ {prefix} · {}", err_text(e)))));
    }

    async fn check_server(&self) -> Option<ServerInfo> {
        let t0 = Instant::now();
        match self.client.check_compatible().await {
            Ok(info) => {
                let ms = t0.elapsed().as_millis() as u32;
                self.send(DataEvent::Server(ServerView {
                    host: short_host(&info.hostname),
                    tailnet: info.tailnet_host.clone().unwrap_or_else(|| info.hostname.clone()),
                    latency_ms: Some(ms.max(1)),
                    error: None,
                    allowed_domain: info.allowed_domain.clone(),
                }));
                Some(info)
            }
            Err(e) => {
                let host = self.client.base().trim_start_matches("https://").trim_start_matches("http://").to_string();
                self.send(DataEvent::Server(ServerView {
                    host: short_host(&host),
                    tailnet: host,
                    latency_ms: None,
                    error: Some(match &e {
                        ClientError::Incompatible { .. } => {
                            "version mismatch · download a matching build from /download".into()
                        }
                        other => err_text(other),
                    }),
                    allowed_domain: None,
                }));
                None
            }
        }
    }

    fn save_session(&self, email: &str) {
        let mut s = Session::load(&self.session_path);
        s.server = Some(self.client.base().to_string());
        s.token = self.client.token();
        s.email = Some(email.to_string());
        if let Err(e) = s.save(&self.session_path) {
            self.send(DataEvent::Flash(FlashMsg::err(format!("✕ could not save session · {e}"))));
        }
    }

    // ------------------------------------------------------------ loading

    async fn bootstrap(self: Arc<Self>) {
        if let Ok(cat) = self.client.harnesses().await {
            self.send(DataEvent::Catalog(
                cat.harnesses
                    .iter()
                    .map(|h| HarnessOption { id: h.id.clone(), label: h.label.clone(), models: h.models.clone() })
                    .collect(),
            ));
            self.state.lock().unwrap().catalog = Some(cat);
        }
        self.load_board().await;
        self.load_machine().await;
        self.load_notifications().await;
        self.load_creds().await;
        self.load_threads(true).await;
        self.start_ws();
        let me = self.clone();
        let poll = tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(5));
            let mut n = 0u64;
            loop {
                iv.tick().await;
                n += 1;
                me.load_machine().await;
                if n.is_multiple_of(6) {
                    me.load_board().await;
                }
            }
        });
        self.ws.lock().unwrap().push(poll);
    }

    fn task_view(&self, st: &State, t: &Task) -> TaskView {
        let project = st.projects.get(&t.project_id).map(|p| p.name.clone()).unwrap_or_default();
        let plan = st.plans.get(&t.plan_id).map(|p| p.spec.feature.clone()).unwrap_or_default();
        let defaults = st.catalog.as_ref().map(|c| c.defaults.clone()).unwrap_or_default();
        let worker = |r: WorkerRole| {
            t.workers
                .get(&r)
                .or_else(|| defaults.get(&r))
                .map(|w| Worker::new(&w.harness, &w.model))
                .unwrap_or_else(|| Worker::new("—", "—"))
        };
        TaskView {
            key: t.key.clone(),
            title: t.title.clone(),
            project,
            plan,
            column: t.column(),
            status: t.status(),
            progress: f32::from(t.progress),
            attempt: t.attempt(),
            loop_cap: t.loop_cap.max(1),
            workers: [worker(WorkerRole::Builder), worker(WorkerRole::Qa), worker(WorkerRole::Reviewer)],
            pr: t.pr_number,
        }
    }

    fn idea_view(&self, st: &State, p: &Plan) -> TaskView {
        TaskView {
            key: crate::util::trunc(&p.spec.feature, 10),
            title: p.spec.feature.clone(),
            project: st.projects.get(&p.project_id).map(|p| p.name.clone()).unwrap_or_default(),
            plan: p.spec.feature.clone(),
            column: BoardColumn::Idea,
            status: Status::Queued,
            progress: 0.0,
            attempt: 1,
            loop_cap: DEFAULT_LOOP_CAP,
            workers: [Worker::new("—", "—"), Worker::new("—", "—"), Worker::new("—", "—")],
            pr: None,
        }
    }

    fn emit_tasks(&self) {
        let views = {
            let st = self.state.lock().unwrap();
            let mut tasks: Vec<&Task> = st.tasks.values().collect();
            tasks.sort_by_key(|t| std::cmp::Reverse(t.created_at));
            let mut v: Vec<TaskView> = st.ideas.iter().map(|p| self.idea_view(&st, p)).collect();
            v.extend(tasks.into_iter().map(|t| self.task_view(&st, t)));
            v
        };
        self.send(DataEvent::Tasks(views));
    }

    async fn load_board(&self) {
        match self.client.board(&BoardQuery::default()).await {
            Ok(b) => {
                {
                    let mut st = self.state.lock().unwrap();
                    st.projects = b.projects.into_iter().map(|p| (p.id, p)).collect();
                    st.tasks = b.tasks.into_iter().map(|t| (t.id, t)).collect();
                    for p in &b.ideas {
                        st.plans.insert(p.id, p.clone());
                    }
                    st.ideas = b.ideas;
                }
                self.emit_tasks();
                self.send(DataEvent::Synced);
            }
            Err(e) if e.is_unauthorized() => self.send(DataEvent::SignedOut),
            Err(e) => self.flash_err("board", &e),
        }
    }

    fn machine_view(&self, m: &MachineStats) -> MachineView {
        let mut st = self.state.lock().unwrap();
        if !m.cpu_history.is_empty() {
            st.cpu = m.cpu_history.clone();
        } else {
            st.cpu.push(m.cpu_percent);
        }
        let len = st.cpu.len();
        if len > 40 {
            st.cpu.drain(..len - 40);
        }
        MachineView {
            host: short_host(&m.hostname),
            cpu: st.cpu.clone(),
            ram_used_gb: m.ram_used_gb,
            ram_total_gb: m.ram_total_gb,
            ctr_used: m.containers_running,
            ctr_slots: m.container_slots,
            queue: m.queue_depth,
            est_wait: format!("~{}", fmt_ago(i64::from(m.est_wait_secs))),
            host_line: m.spec_line.clone(),
        }
    }

    async fn load_machine(&self) {
        if let Ok(m) = self.client.machine().await {
            let v = self.machine_view(&m);
            self.send(DataEvent::Machine(v));
        }
    }

    fn inbox_item(n: &Notification) -> InboxItem {
        let (glyph, tone) = match n.kind {
            NotificationKind::TaskEscalated => ("!", Tone::HUMAN),
            NotificationKind::TaskFailed => ("✕", Tone::FAILED),
            NotificationKind::PlanReady => ("◇", Tone::Accent),
            NotificationKind::TaskReady => ("✓", Tone::PASSED),
            NotificationKind::MemoryApproval => ("◑", Tone::Muted),
            NotificationKind::SystemAlert => ("!", Tone::FAILED),
            NotificationKind::WorkersLive => ("◐", Tone::RUNNING),
        };
        let ago = fmt_ago((chrono::Utc::now() - n.created_at).num_seconds());
        let sub = if n.body.is_empty() { ago } else { format!("{} · {ago}", n.body) };
        InboxItem {
            glyph: glyph.into(),
            tone,
            title: n.title.clone(),
            sub,
            task: n.deep_link.strip_prefix("task/").map(str::to_string),
        }
    }

    fn activity_item(n: &Notification) -> ActivityItem {
        let i = Self::inbox_item(n);
        ActivityItem {
            at: n.created_at.with_timezone(&Local),
            actor: "sys".into(),
            glyph: i.glyph,
            tone: i.tone,
            text: n.title.clone(),
        }
    }

    fn emit_inbox(&self) {
        let items: Vec<InboxItem> = {
            let st = self.state.lock().unwrap();
            st.notifications
                .iter()
                .filter(|n| n.resolved.is_none() && n.kind != NotificationKind::WorkersLive)
                .take(6)
                .map(Self::inbox_item)
                .collect()
        };
        self.send(DataEvent::Inbox(items));
    }

    async fn load_notifications(&self) {
        if let Ok(mut ns) = self.client.notifications().await {
            ns.sort_by_key(|n| std::cmp::Reverse(n.created_at));
            let acts = ns.iter().take(20).map(Self::activity_item).collect();
            self.state.lock().unwrap().notifications = ns;
            self.emit_inbox();
            self.send(DataEvent::ActivityAll(acts));
        }
    }

    fn cred_view(c: &CredentialSummary) -> CredView {
        let secret_label = if c.kind == CredentialKind::ClaudeOauth { "token" } else { "api key" };
        let mut secret = CredField::new(secret_label, "", true);
        secret.masked = Some(c.masked.clone());
        let mut fields = Vec::new();
        if c.kind == CredentialKind::Litellm {
            fields.push(CredField::new("base url", c.base_url.as_deref().unwrap_or(""), false));
        }
        if c.kind == CredentialKind::Other {
            fields.push(CredField::new("env var", c.env_var.as_deref().unwrap_or(""), false));
        }
        fields.push(secret);
        let (status, text) = health(&c.status);
        CredView {
            key: c.id.to_string(),
            name: c.name.clone(),
            kind_label: c.kind.label().into(),
            fields,
            status_text: text,
            status,
            is_new: false,
        }
    }

    fn checklist(creds: &[CredentialSummary]) -> Vec<ChecklistItem> {
        let mut out = Vec::new();
        let model = creds.iter().any(|c| {
            matches!(
                c.kind,
                CredentialKind::AnthropicApiKey
                    | CredentialKind::OpenaiApiKey
                    | CredentialKind::ClaudeOauth
                    | CredentialKind::Litellm
            ) && !matches!(c.status, CredentialHealth::Invalid { .. })
        });
        out.push(ChecklistItem {
            status: if model { Status::Passed } else { Status::NeedsHuman },
            text: "model provider key (Anthropic or OpenAI)".into(),
        });
        for c in creds {
            match &c.status {
                CredentialHealth::ExpiresSoon { days } => out.push(ChecklistItem {
                    status: Status::NeedsHuman,
                    text: format!("{} expires in {days} days · rotate soon", c.name),
                }),
                CredentialHealth::Invalid { error } => {
                    out.push(ChecklistItem { status: Status::Failed, text: format!("{} invalid · {error}", c.name) })
                }
                _ => {}
            }
        }
        if let Some(l) = creds.iter().find(|c| c.kind == CredentialKind::Litellm) {
            let ok = matches!(l.status, CredentialHealth::Reachable { .. } | CredentialHealth::Valid);
            out.push(ChecklistItem {
                status: if ok { Status::Passed } else { Status::Queued },
                text: "LiteLLM proxy reachable from containers".into(),
            });
        }
        out.push(ChecklistItem {
            status: Status::Queued,
            text: "optional: test-account OAuth for QA login flows".into(),
        });
        out
    }

    async fn load_creds(&self) {
        match self.client.credentials().await {
            Ok(cs) => {
                self.send(DataEvent::Credentials(cs.iter().map(Self::cred_view).collect()));
                self.send(DataEvent::Checklist(Self::checklist(&cs)));
                self.state.lock().unwrap().creds = cs;
            }
            Err(e) => self.flash_err("credentials", &e),
        }
    }

    fn status_of(s: PlanStatus) -> (&'static str, Tone, &'static str) {
        match s {
            PlanStatus::Drafting => ("◇", Tone::Accent, "needs approval"),
            PlanStatus::Dispatched => ("◐", Tone::RUNNING, "dispatched"),
            PlanStatus::Queued => ("○", Tone::QUEUED, "queued"),
            PlanStatus::Merged => ("✓", Tone::PASSED, "merged"),
            PlanStatus::Archived => ("○", Tone::Dim, "archived"),
        }
    }

    fn emit_threads(&self) {
        let (threads, plans) = {
            let st = self.state.lock().unwrap();
            let threads: Vec<ThreadView> = st
                .threads
                .iter()
                .map(|t| {
                    let (_, tone, label) = Self::status_of(t.status);
                    ThreadView { key: t.id.to_string(), title: t.title.clone(), status: label.into(), tone }
                })
                .collect();
            let plans: Vec<PlanSummary> = st
                .threads
                .iter()
                .filter_map(|t| t.plan_id.and_then(|id| st.plans.get(&id)))
                .map(|p| {
                    let (g, tone, label) = Self::status_of(p.status);
                    let project = st.projects.get(&p.project_id).map(|x| x.name.clone()).unwrap_or_default();
                    let n = p.spec.tasks.len();
                    PlanSummary {
                        key: p.id.to_string(),
                        glyph: g.into(),
                        tone,
                        title: p.spec.feature.clone(),
                        sub: format!("{project} · {n} task{}", if n == 1 { "" } else { "s" }),
                        status: label.into(),
                    }
                })
                .collect();
            (threads, plans)
        };
        self.send(DataEvent::Threads(threads));
        self.send(DataEvent::Plans(plans));
    }

    async fn load_threads(&self, open_first: bool) {
        let threads = match self.client.threads(&ThreadQuery::default()).await {
            Ok(t) => t,
            Err(e) => return self.flash_err("threads", &e),
        };
        let mut details = Vec::new();
        for t in threads.iter().take(12) {
            if let Ok(d) = self.client.thread(t.id).await {
                details.push(d);
            }
        }
        {
            let mut st = self.state.lock().unwrap();
            st.threads = threads;
            for d in &details {
                st.memories.insert(d.thread.id, d.memories_in_context);
                if let Some(p) = &d.plan {
                    st.plan_threads.insert(p.id, d.thread.id);
                    st.plans.insert(p.id, p.clone());
                }
            }
        }
        self.emit_threads();
        if open_first {
            let first_plan = details.iter().find_map(|d| d.plan.as_ref().map(|p| p.id));
            if let Some(d) = details.first() {
                self.show_thread(d);
            }
            if let Some(pid) = first_plan {
                self.show_plan(pid);
            }
        }
    }

    fn short_model(m: &str) -> String {
        m.strip_prefix("claude-").unwrap_or(m).to_string()
    }

    fn show_thread(&self, d: &ThreadDetail) {
        {
            let mut st = self.state.lock().unwrap();
            st.current_thread = Some(d.thread.id);
            st.memories.insert(d.thread.id, d.memories_in_context);
        }
        let title = d.plan.as_ref().map(|p| p.spec.feature.clone()).unwrap_or_else(|| d.thread.title.clone());
        self.send(DataEvent::Conversation(Conversation {
            thread: Some(d.thread.id.to_string()),
            title,
            model: Self::short_model(&d.planner_model),
            memories: d.memories_in_context,
            msgs: d.messages.iter().map(chat_msg).collect(),
            pending: None,
        }));
        self.follow_ws(ClientMessage::FollowThread { thread_id: d.thread.id });
    }

    fn plan_draft(&self, st: &State, p: &Plan) -> PlanDraft {
        let defaults = st.catalog.as_ref().map(|c| c.defaults.clone()).unwrap_or_default();
        let project = st.projects.get(&p.project_id).map(|x| x.name.clone()).unwrap_or_default();
        let memories = st.plan_threads.get(&p.id).and_then(|t| st.memories.get(t)).copied().unwrap_or(0);
        let who = st
            .user
            .as_ref()
            .map(|u| u.name.split_whitespace().next().unwrap_or("you").to_string())
            .unwrap_or("you".into());
        let (_, _, label) = Self::status_of(p.status);
        PlanDraft {
            key: p.id.to_string(),
            title: p.spec.feature.clone(),
            version: p.version,
            project,
            byline: format!("drafted by Planner with {who} · {memories} memories in context"),
            description: p.spec.description.clone(),
            status: label.into(),
            tasks: p
                .spec
                .tasks
                .iter()
                .map(|t| {
                    let resolved = t.workers.resolve(&defaults);
                    let w = |r: WorkerRole| {
                        resolved
                            .get(&r)
                            .map(|c| Worker::new(&c.harness, &c.model))
                            .unwrap_or_else(|| Worker::new("claude_sdk", "—"))
                    };
                    PlanTask {
                        id: t.id.clone(),
                        title: t.title.clone(),
                        open: true,
                        ac: t.acceptance_criteria.clone(),
                        workers: [w(WorkerRole::Builder), w(WorkerRole::Qa), w(WorkerRole::Reviewer)],
                        description: t.description.clone(),
                        files_hint: t.files_hint.clone(),
                        depends_on: t.depends_on.clone(),
                    }
                })
                .collect(),
        }
    }

    fn show_plan(&self, id: PlanId) {
        let draft = {
            let mut st = self.state.lock().unwrap();
            st.current_plan = Some(id);
            st.plans.get(&id).map(|p| self.plan_draft(&st, p))
        };
        if let Some(d) = draft {
            self.send(DataEvent::Plan(d));
        }
    }

    fn spec_from_draft(d: &PlanDraft) -> PlanSpec {
        PlanSpec {
            feature: d.title.clone(),
            description: d.description.clone(),
            tasks: d
                .tasks
                .iter()
                .map(|t| {
                    let c = |w: &Worker| Some(WorkerChoice::new(w.harness.clone(), w.model.clone()));
                    TaskSpec {
                        id: t.id.clone(),
                        title: t.title.clone(),
                        description: t.description.clone(),
                        acceptance_criteria: t.ac.clone(),
                        files_hint: t.files_hint.clone(),
                        depends_on: t.depends_on.clone(),
                        workers: PlanWorkers {
                            builder: c(&t.workers[0]),
                            qa: c(&t.workers[1]),
                            reviewer: c(&t.workers[2]),
                        },
                    }
                })
                .collect(),
        }
    }

    fn run_detail(&self, d: &TaskDetail) -> RunDetail {
        let mut r = RunDetail { task: d.task.key.clone(), ..Default::default() };
        if let Some(qa) = &d.qa_report {
            let t = &qa.tests;
            r.passed = t.passed;
            r.failed = t.failed;
            r.running = t.running;
            r.total = t.total();
            r.failing = t.failing_assertion.clone();
            r.tests = t
                .suites
                .iter()
                .map(|s| TestRow {
                    status: if s.failed > 0 { Status::Failed } else { Status::Passed },
                    name: s.name.clone(),
                    meta: format!("{}  {:.1}s", s.passed + s.failed, s.duration_ms as f64 / 1000.0),
                })
                .collect();
            if t.running > 0 {
                r.tests.push(TestRow {
                    status: Status::Running,
                    name: format!("{} running", t.running),
                    meta: "—".into(),
                });
            }
        }
        let mut has_diff = false;
        for a in &d.artifacts {
            if matches!(a.kind, ArtifactKind::Transcript) {
                continue;
            }
            let name = a.path.rsplit('/').next().unwrap_or(&a.path).to_string();
            let ext = a.mime.rsplit('/').next().unwrap_or("").to_string();
            let glyph = match a.kind {
                ArtifactKind::Screenshot => "▣",
                ArtifactKind::Video => "▶",
                ArtifactKind::Diff => {
                    has_diff = true;
                    "±"
                }
                _ => "≡",
            };
            r.artifacts.push(ArtifactView {
                key: a.id.to_string(),
                glyph: glyph.into(),
                name,
                meta: if ext.is_empty() { fmt_size(a.size) } else { format!("{ext} · {}", fmt_size(a.size)) },
                handle: format!("{}/{}", a.run_id, a.id),
                url: None,
            });
        }
        if let (Some(diff), false) = (&d.diff, has_diff) {
            r.artifacts.push(ArtifactView {
                key: "diff".into(),
                glyph: "±".into(),
                name: "diff".into(),
                meta: format!("+{} −{} · {} files", diff.additions, diff.deletions, diff.files.len()),
                handle: String::new(),
                url: d.task.pr_url.clone(),
            });
        }
        for (i, role) in ROLES.iter().enumerate() {
            let latest = d.runs.iter().filter(|x| x.role == *role).max_by_key(|x| (x.attempt, x.started_at));
            if let Some(run) = latest {
                if run.state.is_finished() {
                    r.stage_secs[i] = run.duration_secs().map(|s| s.max(0) as u64);
                } else {
                    r.stage_started[i] = run.started_at.map(|t| t.with_timezone(&Local));
                }
            }
        }
        r
    }

    fn key_to_id(&self, key: &str) -> Option<TaskId> {
        self.state.lock().unwrap().tasks.values().find(|t| t.key == key).map(|t| t.id)
    }

    fn id_to_key(&self, id: TaskId) -> Option<String> {
        self.state.lock().unwrap().tasks.get(&id).map(|t| t.key.clone())
    }

    async fn load_detail(&self, id: TaskId) {
        match self.client.task(id).await {
            Ok(d) => {
                let r = self.run_detail(&d);
                self.state.lock().unwrap().tasks.insert(d.task.id, d.task.clone());
                self.send(DataEvent::Run(r));
            }
            Err(e) => self.flash_err("task detail", &e),
        }
    }

    async fn load_logs(&self, id: TaskId, key: &str) {
        match self.client.task_logs(id, &LogQuery { limit: Some(1000), ..Default::default() }).await {
            Ok(lines) => {
                self.send(DataEvent::LogBackfill { task: key.into(), lines: lines.iter().map(log_view).collect() })
            }
            Err(e) => self.flash_err("logs", &e),
        }
    }

    // ------------------------------------------------------------ websocket

    fn follow_ws(&self, m: ClientMessage) {
        if let Some(tx) = self.follow.lock().unwrap().as_ref() {
            let _ = tx.try_send(m);
        }
    }

    fn start_ws(self: &Arc<Self>) {
        if self.follow.lock().unwrap().is_some() {
            return;
        }
        let (ev_tx, mut ev_rx) = mpsc::channel::<ServerEvent>(1024);
        let (follow_tx, follow_rx) = mpsc::channel::<ClientMessage>(64);
        let (resync_tx, mut resync_rx) = mpsc::unbounded_channel::<()>();
        *self.follow.lock().unwrap() = Some(follow_tx);
        let client = self.client.clone();
        let ws = tokio::spawn(kiln_api_client::ws::connect_forever(client, ev_tx, follow_rx, move || {
            let _ = resync_tx.send(());
        }));
        let me = self.clone();
        let events = tokio::spawn(async move {
            let mut first = true;
            loop {
                tokio::select! {
                    ev = ev_rx.recv() => match ev {
                        Some(ev) => me.on_ws(ev).await,
                        None => return,
                    },
                    r = resync_rx.recv() => {
                        if r.is_none() { return; }
                        if !first { me.load_board().await; }
                        first = false;
                    }
                }
            }
        });
        let mut h = self.ws.lock().unwrap();
        h.push(ws);
        h.push(events);
    }

    fn stop_ws(&self) {
        for h in self.ws.lock().unwrap().drain(..) {
            h.abort();
        }
        *self.follow.lock().unwrap() = None;
    }

    async fn on_ws(&self, ev: ServerEvent) {
        match ev {
            ServerEvent::Hello { .. } => self.send(DataEvent::Synced),
            ServerEvent::TaskUpserted { task } => {
                let (view, prev, followed) = {
                    let mut st = self.state.lock().unwrap();
                    let prev = st.tasks.insert(task.id, task.clone()).map(|t| t.state);
                    (self.task_view(&st, &task), prev, st.followed == Some(task.id))
                };
                if prev != Some(task.state) {
                    let (glyph, tone) = (view.status.glyph(), Tone::Status(view.status));
                    self.send(DataEvent::Activity(ActivityItem {
                        at: Local::now(),
                        actor: "sys".into(),
                        glyph: glyph.into(),
                        tone,
                        text: format!("{} {} · {}", task.key, task.state.as_str(), task.title),
                    }));
                    if followed {
                        self.load_detail(task.id).await;
                    }
                }
                self.send(DataEvent::TaskUpsert(view));
            }
            ServerEvent::TaskRemoved { task_id } => {
                let key = self.state.lock().unwrap().tasks.remove(&task_id).map(|t| t.key);
                if let Some(k) = key {
                    self.send(DataEvent::TaskRemove(k));
                }
            }
            ServerEvent::RunUpserted { run } => {
                let followed = self.state.lock().unwrap().followed;
                if let Some(id) = run.task_id
                    && Some(id) == followed
                {
                    self.load_detail(id).await;
                }
            }
            ServerEvent::Log { task_id: Some(id), line, .. } => {
                if let Some(key) = self.id_to_key(id) {
                    self.send(DataEvent::Log { task: key, line: log_view(&line) });
                }
            }
            ServerEvent::Log { .. } => {}
            ServerEvent::PlanUpserted { plan } => {
                let current = {
                    let mut st = self.state.lock().unwrap();
                    st.plans.insert(plan.id, plan.clone());
                    st.plan_threads.insert(plan.id, plan.thread_id);
                    st.current_plan.is_none() || st.current_plan == Some(plan.id)
                };
                self.emit_threads();
                if current {
                    self.show_plan(plan.id);
                }
            }
            ServerEvent::ThreadUpserted { thread } => {
                {
                    let mut st = self.state.lock().unwrap();
                    match st.threads.iter_mut().find(|t| t.id == thread.id) {
                        Some(t) => *t = thread,
                        None => st.threads.insert(0, thread),
                    }
                }
                self.emit_threads();
            }
            ServerEvent::Message { message } => {
                self.send(DataEvent::ChatMessage {
                    thread: Some(message.thread_id.to_string()),
                    msg: chat_msg(&message),
                });
            }
            ServerEvent::PlannerStatus { thread_id, pending, label } => {
                let label = if label.is_empty() { PENDING_LABEL.to_string() } else { label };
                self.send(DataEvent::PlannerPending {
                    thread: Some(thread_id.to_string()),
                    label: pending.then_some(label),
                });
            }
            ServerEvent::Notification { notification } => {
                self.send(DataEvent::Activity(Self::activity_item(&notification)));
                self.state.lock().unwrap().notifications.insert(0, notification);
                self.emit_inbox();
            }
            ServerEvent::MemoryUpserted { .. } => {}
            ServerEvent::Machine { stats } => {
                let v = self.machine_view(&stats);
                self.send(DataEvent::Machine(v));
            }
            ServerEvent::Toast { message, ok } => {
                self.send(DataEvent::Flash(if ok { FlashMsg::ok(message) } else { FlashMsg::err(message) }));
            }
        }
    }

    // ------------------------------------------------------------ commands

    async fn after_sign_in(self: &Arc<Self>, user: User, flash: Option<FlashMsg>) {
        let email = user.email.clone();
        self.save_session(&email);
        self.state.lock().unwrap().user = Some(user);
        self.send(DataEvent::SignedIn { email, flash });
        self.clone().bootstrap().await;
    }

    async fn task_action(&self, key: &str, action: TaskAction, ok: impl Fn(&Task) -> FlashMsg, what: &str) {
        let Some(id) = self.key_to_id(key) else {
            return self.send(DataEvent::Flash(FlashMsg::err(format!("✕ unknown task {key}"))));
        };
        match self.client.task_action(id, action).await {
            Ok(t) => {
                let view = {
                    let mut st = self.state.lock().unwrap();
                    st.tasks.insert(t.id, t.clone());
                    self.task_view(&st, &t)
                };
                self.send(DataEvent::TaskUpsert(view));
                self.send(DataEvent::Flash(ok(&t)));
            }
            Err(e) => self.flash_err(&format!("{what} {key}"), &e),
        }
    }

    async fn handle(self: Arc<Self>, cmd: Cmd) {
        match cmd {
            Cmd::Login { email, password } => {
                if password.is_empty() {
                    return self
                        .send(DataEvent::LoginError { message: "✕ password required".into(), password_field: true });
                }
                let req = LoginRequest {
                    email: email.clone(),
                    password,
                    client: Some(ClientKind::Tui),
                    device_name: Some(hostname()),
                };
                match self.client.login(&req).await {
                    Ok(r) => {
                        let flash = FlashMsg::ok(format!("✓ signed in as {} · session saved", r.user.email));
                        self.after_sign_in(r.user, Some(flash)).await;
                    }
                    Err(e) => {
                        let message = match (&e, e.code()) {
                            (_, Some("not_allowlisted")) => {
                                "✕ not on the allowlist · ask an admin to create your account".into()
                            }
                            (_, Some("unauthorized")) => "✕ wrong email or password".into(),
                            (ClientError::Http(m), _) => {
                                self.check_server().await;
                                format!("✕ cannot reach the server · {m}")
                            }
                            _ => format!("✕ {}", err_text(&e)),
                        };
                        self.send(DataEvent::LoginError { message, password_field: false });
                    }
                }
            }
            Cmd::Setup { name, email, password, domain } => {
                let req = SetupRequest { admin_email: email, admin_name: name, password, allowed_domain: domain };
                match self.client.setup(&req).await {
                    Ok(r) => {
                        let email = r.user.email.clone();
                        self.save_session(&email);
                        self.state.lock().unwrap().user = Some(r.user);
                        self.send(DataEvent::SignedIn { email, flash: None });
                        self.check_server().await;
                        self.send(DataEvent::SetupStep { step: 0 });
                    }
                    Err(e) => self.send(DataEvent::SetupError(format!("✕ {}", err_text(&e)))),
                }
            }
            Cmd::RegisterProject { key, name, repo_url, branch, qa_path } => {
                let req = CreateProjectRequest { key: key.clone(), name, repo_url, default_branch: branch, qa_path };
                match self.client.create_project(&req).await {
                    Ok(_) => {
                        self.send(DataEvent::SetupStep { step: 1 });
                        self.send(DataEvent::Flash(FlashMsg::ok(format!(
                            "✓ setup complete · project {key} registered"
                        ))));
                        self.clone().bootstrap().await;
                    }
                    Err(e) => self.send(DataEvent::SetupError(format!("✕ {}", err_text(&e)))),
                }
            }
            Cmd::Logout => {
                let _ = self.client.logout().await;
                self.stop_ws();
                let mut s = Session::load(&self.session_path);
                s.token = None;
                let _ = s.save(&self.session_path);
                *self.state.lock().unwrap() = State::default();
                self.send(DataEvent::SignedOut);
            }
            Cmd::Retry(key) => {
                self.task_action(
                    &key,
                    TaskAction::Retry,
                    |t| {
                        FlashMsg::new(
                            format!("◐ {} restarted · attempt {}/{}", t.key, t.attempt(), t.loop_cap),
                            Tone::RUNNING,
                        )
                    },
                    "retry",
                )
                .await
            }
            Cmd::Stop(key) => {
                self.task_action(
                    &key,
                    TaskAction::Stop,
                    |t| FlashMsg::new(format!("○ {} stopped · container released", t.key), Tone::QUEUED),
                    "stop",
                )
                .await
            }
            Cmd::Escalate(key) => {
                self.task_action(
                    &key,
                    TaskAction::Escalate { note: None },
                    |t| FlashMsg::new(format!("! {} escalated · team notified on Android", t.key), Tone::HUMAN),
                    "escalate",
                )
                .await
            }
            Cmd::Feedback { task, text } => {
                self.task_action(
                    &task,
                    TaskAction::Feedback { text, attach_qa: true, attach_verdict: true, save_memory: false },
                    |t| FlashMsg::ok(format!("✓ feedback sent · builder restarted on {}", t.key)),
                    "feedback",
                )
                .await
            }
            Cmd::Follow(key) => {
                let Some(id) = self.key_to_id(&key) else {
                    return;
                };
                let prev = self.state.lock().unwrap().followed.replace(id);
                if let Some(p) = prev.filter(|p| *p != id) {
                    self.follow_ws(ClientMessage::UnfollowTask { task_id: p });
                }
                self.follow_ws(ClientMessage::FollowTask { task_id: id });
                self.load_detail(id).await;
                self.load_logs(id, &key).await;
            }
            Cmd::Approve(draft) => {
                let Ok(id) = draft.key.parse::<PlanId>() else {
                    return;
                };
                let server = self.state.lock().unwrap().plans.get(&id).cloned();
                let spec = Self::spec_from_draft(&draft);
                let mut version = draft.version;
                if server.as_ref().is_some_and(|p| p.spec != spec) {
                    match self.client.update_plan(id, &UpdatePlanRequest { spec, version }).await {
                        Ok(p) => {
                            version = p.version;
                            self.state.lock().unwrap().plans.insert(p.id, p);
                        }
                        Err(e) => {
                            if e.code() == Some("conflict") {
                                self.send(DataEvent::Flash(FlashMsg::err(
                                    "✕ the plan changed on the server · review the new draft and approve again",
                                )));
                                self.load_threads(false).await;
                                self.show_plan(id);
                            } else {
                                self.flash_err("save plan", &e);
                            }
                            return;
                        }
                    }
                }
                match self.client.approve_plan(id, &ApprovePlanRequest { version: Some(version) }).await {
                    Ok(r) => {
                        let building = r.tasks.iter().filter(|t| t.state.is_active()).count();
                        let n = r.tasks.len();
                        {
                            let mut st = self.state.lock().unwrap();
                            st.plans.insert(r.plan.id, r.plan.clone());
                            for t in r.tasks {
                                st.tasks.insert(t.id, t);
                            }
                        }
                        self.emit_tasks();
                        self.emit_threads();
                        self.show_plan(id);
                        self.send(DataEvent::Dispatched);
                        self.send(DataEvent::Flash(FlashMsg::ok(format!(
                            "✓ dispatched {n} tasks · {building} building, {} queued",
                            n - building
                        ))));
                    }
                    Err(e) => self.flash_err("dispatch", &e),
                }
            }
            Cmd::OpenPlan(key) => {
                let Ok(id) = key.parse::<PlanId>() else {
                    return;
                };
                self.show_plan(id);
            }
            Cmd::OpenThread(key) => {
                let Ok(id) = key.parse::<ThreadId>() else {
                    return;
                };
                match self.client.thread(id).await {
                    Ok(d) => {
                        if let Some(p) = &d.plan {
                            let mut st = self.state.lock().unwrap();
                            st.plans.insert(p.id, p.clone());
                            st.plan_threads.insert(p.id, d.thread.id);
                        }
                        self.show_thread(&d);
                        if let Some(p) = &d.plan {
                            self.show_plan(p.id);
                        }
                    }
                    Err(e) => self.flash_err("thread", &e),
                }
            }
            Cmd::NewThread => self.state.lock().unwrap().current_thread = None,
            Cmd::SendMessage { thread, text } => {
                let thread_id = thread.and_then(|t| t.parse::<ThreadId>().ok());
                match thread_id {
                    Some(tid) => {
                        match self.client.send_message(tid, &SendMessageRequest { body: text, voice: false }).await {
                            Ok(m) => {
                                self.send(DataEvent::ChatMessage { thread: Some(tid.to_string()), msg: chat_msg(&m) })
                            }
                            Err(e) => {
                                self.send(DataEvent::PlannerPending { thread: None, label: None });
                                self.flash_err("send", &e);
                            }
                        }
                    }
                    None => {
                        let project = {
                            let st = self.state.lock().unwrap();
                            let mut ps: Vec<&Project> = st.projects.values().collect();
                            ps.sort_by_key(|p| p.created_at);
                            ps.first().map(|p| p.id)
                        };
                        let Some(project_id) = project else {
                            self.send(DataEvent::PlannerPending { thread: None, label: None });
                            return self.send(DataEvent::Flash(FlashMsg::err(
                                "✕ no projects yet · ask an admin to register one",
                            )));
                        };
                        match self
                            .client
                            .create_thread(&CreateThreadRequest { project_id, message: text, voice: false })
                            .await
                        {
                            Ok(d) => {
                                self.show_thread(&d);
                                self.send(DataEvent::PlannerPending {
                                    thread: None,
                                    label: Some(PENDING_LABEL.into()),
                                });
                                self.load_threads(false).await;
                            }
                            Err(e) => {
                                self.send(DataEvent::PlannerPending { thread: None, label: None });
                                self.flash_err("new thread", &e);
                            }
                        }
                    }
                }
            }
            Cmd::SaveCredential(c) => self.save_credential(c).await,
            Cmd::TestCredential(key) => {
                let Ok(id) = key.parse::<CredentialId>() else {
                    return;
                };
                let name = self
                    .state
                    .lock()
                    .unwrap()
                    .creds
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                match self.client.test_credential(id).await {
                    Ok(r) => {
                        let text = if r.ok {
                            format!("valid · tested just now · {}ms", r.latency_ms)
                        } else {
                            format!("failed · {}", r.message)
                        };
                        self.send(DataEvent::CredentialTested {
                            key: key.clone(),
                            status: if r.ok { Status::Passed } else { Status::Failed },
                            text,
                        });
                        self.send(DataEvent::Flash(if r.ok {
                            FlashMsg::ok(format!("✓ {name} · connection ok · {}ms", r.latency_ms))
                        } else {
                            FlashMsg::err(format!("✕ {name} · {}", r.message))
                        }));
                    }
                    Err(e) => self.flash_err(&format!("test {name}"), &e),
                }
            }
            Cmd::OpenArtifact { artifact, .. } => self.open_artifact(artifact).await,
            Cmd::PairStart => match self.client.pair_start().await {
                Ok(p) => self.send(DataEvent::Pair(PairView {
                    uri: p.pair_uri,
                    code: p.code,
                    expires: p.expires_at.with_timezone(&Local).format("%H:%M").to_string(),
                })),
                Err(e) => self.flash_err("pairing", &e),
            },
            Cmd::Quit => self.stop_ws(),
        }
    }

    async fn save_credential(&self, c: CredView) {
        let field = |label: &str| {
            c.fields.iter().find(|f| f.label == label).map(|f| f.value.trim().to_string()).unwrap_or_default()
        };
        let secret = c.fields.iter().find(|f| f.secret).map(|f| f.value.trim().to_string()).unwrap_or_default();
        let base_url = Some(field("base url")).filter(|s| !s.is_empty());
        let result = if c.is_new {
            let provider = field("provider");
            if provider.is_empty() || secret.is_empty() {
                return self.send(DataEvent::Flash(FlashMsg::err("✕ provider and key are both required")));
            }
            let p = provider.to_lowercase();
            let kind = if p.contains("litellm") {
                CredentialKind::Litellm
            } else if p.contains("oauth") || (p.contains("claude") && secret.starts_with("sk-ant-oat")) {
                CredentialKind::ClaudeOauth
            } else if p.contains("anthropic") || p.contains("claude") {
                CredentialKind::AnthropicApiKey
            } else if p.contains("openai") || p.contains("codex") {
                CredentialKind::OpenaiApiKey
            } else {
                CredentialKind::Other
            };
            if kind == CredentialKind::Litellm && base_url.is_none() {
                return self.send(DataEvent::Flash(FlashMsg::err("✕ LiteLLM needs a base url")));
            }
            let env_var = (kind == CredentialKind::Other).then(|| {
                format!("{}_API_KEY", provider.to_uppercase().replace(|ch: char| !ch.is_ascii_alphanumeric(), "_"))
            });
            self.client
                .add_credential(&CreateCredentialRequest { kind, name: provider, secret, base_url, env_var })
                .await
        } else {
            let Ok(id) = c.key.parse::<CredentialId>() else {
                return;
            };
            if secret.is_empty() {
                let msg = if base_url.is_some() {
                    FlashMsg::new("! enter the key again to change the base url", Tone::HUMAN)
                } else {
                    FlashMsg::new(format!("○ {} unchanged", c.name), Tone::QUEUED)
                };
                return self.send(DataEvent::Flash(msg));
            }
            self.client.rotate_credential(id, &RotateCredentialRequest { secret, base_url }).await
        };
        match result {
            Ok(s) => {
                let view = Self::cred_view(&s);
                self.send(DataEvent::Credential { key: c.key.clone(), cred: view });
                self.send(DataEvent::Flash(FlashMsg::ok(format!("✓ {} saved · encrypted at rest", s.name))));
                if let Ok(cs) = self.client.credentials().await {
                    self.send(DataEvent::Checklist(Self::checklist(&cs)));
                    self.state.lock().unwrap().creds = cs;
                }
            }
            Err(e) => self.flash_err(&format!("save {}", c.name), &e),
        }
    }

    async fn open_artifact(&self, a: ArtifactView) {
        let opened = format!("↗ opened {} externally (Preview / mpv)", a.name);
        if let Some(url) = &a.url {
            return match open_external(url) {
                Ok(()) => self.send(DataEvent::Flash(FlashMsg::new(opened, Tone::Muted))),
                Err(e) => self.send(DataEvent::Flash(FlashMsg::err(format!("✕ could not open {} · {e}", a.name)))),
            };
        }
        let mut parts = a.handle.split('/');
        let (Some(run), Some(art)) = (
            parts.next().and_then(|s| s.parse::<RunId>().ok()),
            parts.next().and_then(|s| s.parse::<ArtifactId>().ok()),
        ) else {
            return self
                .send(DataEvent::Flash(FlashMsg::new(format!("○ {} has nothing to open", a.name), Tone::QUEUED)));
        };
        self.send(DataEvent::Flash(FlashMsg::new(format!("↓ downloading {}…", a.name), Tone::Muted)));
        match self.client.artifact_bytes(run, art).await {
            Ok(bytes) => {
                let dir = std::env::temp_dir().join("kiln-artifacts").join(run.to_string());
                let safe: String = a.name.chars().map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect();
                let path = dir.join(if safe.is_empty() { "artifact".into() } else { safe });
                let res = tokio::fs::create_dir_all(&dir).await.and(tokio::fs::write(&path, &bytes).await);
                match res.and_then(|_| open_external(&path.to_string_lossy())) {
                    Ok(()) => self.send(DataEvent::Flash(FlashMsg::new(opened, Tone::Muted))),
                    Err(e) => self.send(DataEvent::Flash(FlashMsg::err(format!("✕ could not open {} · {e}", a.name)))),
                }
            }
            Err(e) => self.flash_err(&format!("download {}", a.name), &e),
        }
    }
}

fn health(h: &CredentialHealth) -> (Status, String) {
    match h {
        CredentialHealth::Untested => (Status::Queued, "not tested".into()),
        CredentialHealth::Valid => (Status::Passed, "valid".into()),
        CredentialHealth::Reachable { latency_ms } => (Status::Passed, format!("reachable · {latency_ms}ms")),
        CredentialHealth::ExpiresSoon { days } => (Status::NeedsHuman, format!("expires in {days} days")),
        CredentialHealth::Invalid { error } => (Status::Failed, format!("invalid · {error}")),
    }
}

fn chat_msg(m: &Message) -> ChatMsg {
    ChatMsg { id: Some(m.id.to_string()), you: m.author == MessageAuthor::User, text: m.body.clone() }
}

fn log_view(l: &LogLine) -> LogLineView {
    LogLineView {
        idx: 0,
        ts: l.ts.with_timezone(&Local).format("%H:%M:%S").to_string(),
        role: l.role,
        level: l.level,
        msg: l.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_mapping() {
        assert_eq!(
            health(&CredentialHealth::ExpiresSoon { days: 6 }),
            (Status::NeedsHuman, "expires in 6 days".into())
        );
        assert_eq!(health(&CredentialHealth::Reachable { latency_ms: 34 }).1, "reachable · 34ms");
    }

    #[test]
    fn checklist_flags_missing_model_key() {
        let c = Inner::checklist(&[]);
        assert_eq!(c[0].status, Status::NeedsHuman);
    }
}
