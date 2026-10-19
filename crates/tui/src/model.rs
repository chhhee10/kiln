//! View model shared by every data source (demo and live), plus the events a
//! source feeds into the app ([`DataEvent`]) and the effects the app asks a
//! source to perform ([`Cmd`]).

use crate::theme::Tone;
use chrono::{DateTime, Local};
use kiln_types::event::LogLevel;
use kiln_types::{BoardColumn, Status, WorkerRole};
use std::collections::HashMap;

pub const ROLES: [WorkerRole; 3] = WorkerRole::PIPELINE;

/// Harness + model for one pipeline role. `harness` is the config id
/// (`claude_sdk`, `codex`, `opencode`, …).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worker {
    pub harness: String,
    pub model: String,
}

impl Worker {
    pub fn new(h: &str, m: &str) -> Self {
        Self { harness: h.into(), model: m.into() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskView {
    /// Display key, e.g. "FP-142". Every command addresses tasks by key.
    pub key: String,
    pub title: String,
    /// Project name ("CLI/SDK").
    pub project: String,
    /// Short plan title ("Retry & backoff").
    pub plan: String,
    pub column: BoardColumn,
    pub status: Status,
    /// 0–100 progress of the current stage.
    pub progress: f32,
    /// 1-based attempt.
    pub attempt: u32,
    pub loop_cap: u32,
    pub workers: [Worker; 3],
    pub pr: Option<u64>,
}

impl TaskView {
    /// Index of the running pipeline stage, if the task is in one.
    pub fn stage(&self) -> Option<usize> {
        match self.column {
            BoardColumn::Building => Some(0),
            BoardColumn::Qa => Some(1),
            BoardColumn::Review => Some(2),
            _ => None,
        }
    }
    /// A worker container is live for this task.
    pub fn is_worker(&self) -> bool {
        self.status == Status::Running && self.stage().is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct MachineView {
    pub host: String,
    pub cpu: Vec<f32>,
    pub ram_used_gb: f32,
    pub ram_total_gb: f32,
    pub ctr_used: u32,
    pub ctr_slots: u32,
    pub queue: u32,
    pub est_wait: String,
    pub host_line: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InboxItem {
    pub glyph: String,
    pub tone: Tone,
    pub title: String,
    pub sub: String,
    /// Task key to open on ⏎ (if any).
    pub task: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityItem {
    pub at: DateTime<Local>,
    pub actor: String,
    pub glyph: String,
    pub tone: Tone,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanSummary {
    /// Opaque source key (plan id in live mode).
    pub key: String,
    pub glyph: String,
    pub tone: Tone,
    pub title: String,
    pub sub: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanTask {
    pub id: String,
    pub title: String,
    pub open: bool,
    pub ac: Vec<String>,
    pub workers: [Worker; 3],
    /// Fields of the source spec the TUI doesn't edit, kept for round-trips.
    pub description: String,
    pub files_hint: Vec<String>,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanDraft {
    pub key: String,
    pub title: String,
    pub version: u32,
    pub project: String,
    /// "drafted by Planner with Alex · 3 memories in context".
    pub byline: String,
    pub description: String,
    pub status: String,
    pub tasks: Vec<PlanTask>,
}

/// A row of the plan tree (`planItems()` in the prototype).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanItem {
    Task(usize),
    Criterion(usize, usize),
    Worker(usize, usize),
}

impl PlanDraft {
    pub fn items(&self) -> Vec<PlanItem> {
        let mut out = Vec::new();
        for (ti, t) in self.tasks.iter().enumerate() {
            out.push(PlanItem::Task(ti));
            if t.open {
                out.extend((0..t.ac.len()).map(|ai| PlanItem::Criterion(ti, ai)));
                out.extend((0..3).map(|wi| PlanItem::Worker(ti, wi)));
            }
        }
        out
    }
}

/// One harness the user may pick, with its models.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessOption {
    pub id: String,
    pub label: String,
    pub models: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadView {
    pub key: String,
    pub title: String,
    pub status: String,
    pub tone: Tone,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMsg {
    /// Source id, used to drop duplicates (WS echo of our own POST).
    pub id: Option<String>,
    pub you: bool,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Conversation {
    pub thread: Option<String>,
    pub title: String,
    pub model: String,
    pub memories: u32,
    pub msgs: Vec<ChatMsg>,
    /// Planner working label (spinner line).
    pub pending: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestRow {
    pub status: Status,
    pub name: String,
    pub meta: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactView {
    pub key: String,
    pub glyph: String,
    pub name: String,
    pub meta: String,
    /// Source handle: "run_id/artifact_id" in live mode, empty for synthetic rows.
    pub handle: String,
    /// URL to open instead of downloading (diff → PR).
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct RunDetail {
    pub task: String,
    pub tests: Vec<TestRow>,
    pub passed: u32,
    pub failed: u32,
    pub running: u32,
    pub total: u32,
    pub failing: Option<String>,
    pub artifacts: Vec<ArtifactView>,
    /// Finished stage durations in seconds.
    pub stage_secs: [Option<u64>; 3],
    /// When the running stage started.
    pub stage_started: [Option<DateTime<Local>>; 3],
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogLineView {
    /// Arrival index within the task's log (for follow/pause).
    pub idx: usize,
    pub ts: String,
    pub role: WorkerRole,
    pub level: LogLevel,
    pub msg: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CredField {
    pub label: String,
    /// Locally typed value (secrets are never fetched back from the daemon).
    pub value: String,
    pub secret: bool,
    /// Server-side masked display ("sk-ant-…3f9a") when `value` is empty.
    pub masked: Option<String>,
}

impl CredField {
    pub fn new(label: &str, value: &str, secret: bool) -> Self {
        Self { label: label.into(), value: value.into(), secret, masked: None }
    }
    pub fn display(&self, editing: bool) -> String {
        if self.secret {
            if self.value.is_empty() && !editing {
                return self.masked.clone().unwrap_or_default();
            }
            crate::util::mask(&self.value, editing)
        } else {
            self.value.clone()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CredView {
    pub key: String,
    pub name: String,
    pub kind_label: String,
    pub fields: Vec<CredField>,
    pub status_text: String,
    pub status: Status,
    /// Not yet saved to the vault.
    pub is_new: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChecklistItem {
    pub status: Status,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ServerView {
    /// Short host name ("mac-mini").
    pub host: String,
    /// Reachability host ("mac-mini.tailnet").
    pub tailnet: String,
    pub latency_ms: Option<u32>,
    pub error: Option<String>,
    pub allowed_domain: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PairView {
    pub uri: String,
    pub code: String,
    pub expires: String,
}

/// Everything the UI renders that comes from a data source.
#[derive(Clone, Debug, Default)]
pub struct Data {
    pub user_email: String,
    pub server: ServerView,
    pub tasks: Vec<TaskView>,
    pub machine: MachineView,
    pub inbox: Vec<InboxItem>,
    pub activity: Vec<ActivityItem>,
    pub plans: Vec<PlanSummary>,
    pub plan: Option<PlanDraft>,
    pub catalog: Vec<HarnessOption>,
    pub threads: Vec<ThreadView>,
    pub chat: Conversation,
    pub run: Option<RunDetail>,
    pub logs: HashMap<String, Vec<LogLineView>>,
    pub creds: Vec<CredView>,
    pub checklist: Vec<ChecklistItem>,
    pub last_sync: Option<DateTime<Local>>,
}

pub const LOG_CAP: usize = 5000;

impl Data {
    pub fn task(&self, key: &str) -> Option<&TaskView> {
        self.tasks.iter().find(|t| t.key == key)
    }
    pub fn workers(&self) -> Vec<&TaskView> {
        self.tasks.iter().filter(|t| t.is_worker()).collect()
    }
    pub fn push_log(&mut self, task: &str, mut line: LogLineView) {
        let v = self.logs.entry(task.to_string()).or_default();
        line.idx = v.last().map(|l| l.idx + 1).unwrap_or(0);
        v.push(line);
        if v.len() > LOG_CAP {
            let drop = v.len() - LOG_CAP;
            v.drain(..drop);
        }
    }
    pub fn upsert_task(&mut self, t: TaskView) {
        match self.tasks.iter_mut().find(|x| x.key == t.key) {
            Some(x) => *x = t,
            None => self.tasks.insert(0, t),
        }
    }
}

/// Flash/status-line message.
#[derive(Clone, Debug, PartialEq)]
pub struct FlashMsg {
    pub text: String,
    pub tone: Tone,
}

impl FlashMsg {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self { text: text.into(), tone }
    }
    pub fn ok(text: impl Into<String>) -> Self {
        Self::new(text, Tone::PASSED)
    }
    pub fn err(text: impl Into<String>) -> Self {
        Self::new(text, Tone::FAILED)
    }
}

/// Events from a data source to the app.
#[derive(Clone, Debug, PartialEq)]
pub enum DataEvent {
    Server(ServerView),
    SetupRequired {
        allowed_domain: Option<String>,
    },
    /// Signed in (login, setup or a saved session).
    SignedIn {
        email: String,
        flash: Option<FlashMsg>,
    },
    LoginError {
        message: String,
        password_field: bool,
    },
    /// Setup wizard: step done (0 = admin created, 1 = project registered).
    SetupStep {
        step: usize,
    },
    SetupError(String),
    SignedOut,
    Tasks(Vec<TaskView>),
    TaskUpsert(TaskView),
    TaskRemove(String),
    Machine(MachineView),
    Inbox(Vec<InboxItem>),
    Activity(ActivityItem),
    ActivityAll(Vec<ActivityItem>),
    Plans(Vec<PlanSummary>),
    Plan(PlanDraft),
    Catalog(Vec<HarnessOption>),
    Threads(Vec<ThreadView>),
    Conversation(Conversation),
    ChatMessage {
        thread: Option<String>,
        msg: ChatMsg,
    },
    PlannerPending {
        thread: Option<String>,
        label: Option<String>,
    },
    Run(RunDetail),
    LogBackfill {
        task: String,
        lines: Vec<LogLineView>,
    },
    Log {
        task: String,
        line: LogLineView,
    },
    Credentials(Vec<CredView>),
    Credential {
        key: String,
        cred: CredView,
    },
    CredentialTested {
        key: String,
        status: Status,
        text: String,
    },
    Checklist(Vec<ChecklistItem>),
    Pair(PairView),
    /// Plan dispatched: go to the dashboard.
    Dispatched,
    Flash(FlashMsg),
    Synced,
}

/// Effects the app asks the source to perform.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Login {
        email: String,
        password: String,
    },
    Setup {
        name: String,
        email: String,
        password: String,
        domain: String,
    },
    RegisterProject {
        key: String,
        name: String,
        repo_url: String,
        branch: String,
        qa_path: String,
    },
    Logout,
    Retry(String),
    Stop(String),
    Escalate(String),
    Feedback {
        task: String,
        text: String,
    },
    /// Start streaming logs + detail of a task (Run / Logs screens).
    Follow(String),
    Approve(PlanDraft),
    OpenPlan(String),
    OpenThread(String),
    NewThread,
    SendMessage {
        thread: Option<String>,
        text: String,
    },
    SaveCredential(CredView),
    TestCredential(String),
    OpenArtifact {
        task: String,
        artifact: ArtifactView,
    },
    PairStart,
    Quit,
}
