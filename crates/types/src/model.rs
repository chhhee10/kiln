//! Core domain records (mirror the SQLite tables in PRD §8.1).

use crate::ids::*;
use crate::plan::{PlanSpec, WorkerChoice};
use crate::qa::{QaReport, ReviewVerdict};
use crate::status::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type Timestamp = DateTime<Utc>;

// ---------------------------------------------------------------- users

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    Admin,
    Member,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserStatus {
    Active,
    Disabled,
}

/// Per-user limits set by an admin (PRD §2, §5.6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserLimits {
    /// Max tasks this user can have in flight at once (1–8).
    pub concurrency: u32,
    /// Share of machine RAM this user may occupy, 0.0–1.0.
    pub ram_share: f32,
    /// Monthly model-spend budget in USD (display + soft cap).
    pub monthly_budget_usd: f64,
}

impl Default for UserLimits {
    fn default() -> Self {
        Self { concurrency: 2, ram_share: 0.5, monthly_budget_usd: 200.0 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub name: String,
    pub role: UserRole,
    pub status: UserStatus,
    pub limits: UserLimits,
    /// Two-letter initials for avatars.
    pub initials: String,
    pub created_at: Timestamp,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == UserRole::Admin
    }
}

pub fn initials_for(name: &str) -> String {
    let mut parts = name.split_whitespace().filter_map(|p| p.chars().next());
    match (parts.next(), parts.last()) {
        (Some(a), Some(b)) => format!("{}{}", a, b).to_uppercase(),
        (Some(a), None) => a.to_uppercase().collect(),
        _ => "?".into(),
    }
}

/// Per-user preferences (PRD §4.1, settings "Worker defaults" / "Memory sharing").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserSettings {
    pub worker_defaults: BTreeMap<WorkerRole, WorkerChoice>,
    pub default_visibility: crate::memory::Visibility,
    pub planner_reads_company: bool,
    pub notification_channels: BTreeMap<String, bool>,
}

// ---------------------------------------------------------------- projects

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceProfile {
    pub cpus: f32,
    /// Memory limit in MiB.
    pub memory_mb: u64,
}

impl Default for ResourceProfile {
    fn default() -> Self {
        Self { cpus: 2.0, memory_mb: 4096 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    /// Short uppercase key used for task ids, e.g. "FP" → FP-142.
    pub key: String,
    pub name: String,
    pub repo_url: String,
    pub default_branch: String,
    /// Path of `qa.yaml` inside the repo.
    pub qa_path: String,
    /// Parsed status of the qa profile.
    pub qa_status: QaProfileStatus,
    pub image: Option<String>,
    pub resources: ResourceProfile,
    pub loop_cap: u32,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum QaProfileStatus {
    Unknown,
    Valid,
    Missing,
    Invalid { error: String },
}

// ---------------------------------------------------------------- planner

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Being drafted in chat (board column Idea; thread color `human`).
    Drafting,
    /// Approved; tasks exist.
    Dispatched,
    /// Approved but every task is still queued.
    Queued,
    /// All tasks merged.
    Merged,
    Archived,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: PlanId,
    pub project_id: ProjectId,
    pub owner_id: UserId,
    pub thread_id: ThreadId,
    pub status: PlanStatus,
    /// Monotonic draft number ("Plan · draft v3").
    pub version: u32,
    pub spec: PlanSpec,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A planner chat thread (desktop "Threads" column).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub owner_id: UserId,
    pub title: String,
    pub plan_id: Option<PlanId>,
    pub status: PlanStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAuthor {
    User,
    Planner,
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub author: MessageAuthor,
    pub body: String,
    /// True when the user dictated the message (Android "◉ voice").
    pub voice: bool,
    /// The plan draft this message produced, if any.
    pub plan_version: Option<u32>,
    pub created_at: Timestamp,
}

// ---------------------------------------------------------------- tasks & runs

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub plan_id: PlanId,
    pub project_id: ProjectId,
    pub owner_id: UserId,
    /// Per-project sequence, displayed as `{project.key}-{number}`.
    pub number: u64,
    /// Display id, e.g. "FP-142".
    pub key: String,
    /// Id inside the plan spec ("t1").
    pub spec_id: String,
    pub title: String,
    pub description: String,
    pub acceptance: Vec<String>,
    pub files_hint: Vec<String>,
    pub depends_on: Vec<TaskId>,
    pub workers: BTreeMap<WorkerRole, WorkerChoice>,
    pub state: TaskState,
    /// Last active state, used to place Escalated/Failed cards on the board.
    pub last_active: Option<TaskState>,
    /// Number of Builder re-runs so far (0 on the first attempt).
    pub loop_count: u32,
    pub loop_cap: u32,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
    pub pr_number: Option<u64>,
    /// 0–100 progress of the current stage, for progress bars.
    pub progress: u8,
    /// Short human-readable meta line, e.g. "QA testing · 2/3".
    pub meta: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Task {
    /// "Attempt n of cap": the first attempt is 1.
    pub fn attempt(&self) -> u32 {
        self.loop_count + 1
    }
    pub fn column(&self) -> BoardColumn {
        self.state.column(self.last_active)
    }
    pub fn status(&self) -> Status {
        self.state.status()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: RunId,
    pub task_id: Option<TaskId>,
    /// Planner runs belong to a thread instead of a task.
    pub thread_id: Option<ThreadId>,
    pub owner_id: UserId,
    pub role: WorkerRole,
    pub harness: String,
    pub model: String,
    /// Which attempt of the task this run belongs to (1-based).
    pub attempt: u32,
    pub container_id: Option<String>,
    pub state: RunState,
    pub progress: u8,
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub ingested: bool,
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub error: Option<String>,
    /// Retries already consumed for this run slot (PRD §7.6: retried once).
    pub retries: u32,
}

impl Run {
    pub fn duration_secs(&self) -> Option<i64> {
        let start = self.started_at?;
        let end = self.ended_at.unwrap_or_else(Utc::now);
        Some((end - start).num_seconds())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Transcript,
    Log,
    Diff,
    TestReport,
    QaReport,
    Verdict,
    Screenshot,
    Video,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: ArtifactId,
    pub run_id: RunId,
    pub kind: ArtifactKind,
    /// Path relative to the run folder.
    pub path: String,
    pub size: u64,
    pub mime: String,
    pub caption: Option<String>,
    pub created_at: Timestamp,
}

/// Everything a task-detail view needs in one call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub project: Project,
    pub owner: UserSummary,
    pub runs: Vec<Run>,
    pub artifacts: Vec<Artifact>,
    pub qa_report: Option<QaReport>,
    pub verdict: Option<ReviewVerdict>,
    pub diff: Option<DiffSummary>,
    pub criteria: Vec<CriterionStatus>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CriterionStatus {
    pub criterion: String,
    pub status: Status,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct DiffSummary {
    pub additions: u64,
    pub deletions: u64,
    pub files: Vec<DiffFile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserSummary {
    pub id: UserId,
    pub name: String,
    pub email: String,
    pub initials: String,
}

impl From<&User> for UserSummary {
    fn from(u: &User) -> Self {
        Self { id: u.id, name: u.name.clone(), email: u.email.clone(), initials: u.initials.clone() }
    }
}

// ---------------------------------------------------------------- credentials

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// Claude subscription OAuth token (`CLAUDE_CODE_OAUTH_TOKEN`).
    ClaudeOauth,
    /// Anthropic API key (`ANTHROPIC_API_KEY`).
    AnthropicApiKey,
    /// OpenAI API key (`OPENAI_API_KEY`) for Codex.
    OpenaiApiKey,
    /// LiteLLM proxy: base URL + key.
    Litellm,
    /// Any other harness key, injected under `env_var`.
    Other,
}

impl CredentialKind {
    pub fn label(self) -> &'static str {
        match self {
            CredentialKind::ClaudeOauth => "OAuth",
            CredentialKind::AnthropicApiKey | CredentialKind::OpenaiApiKey | CredentialKind::Other => "API key",
            CredentialKind::Litellm => "URL + key",
        }
    }
}

/// What the vault shows after a secret is saved: never the secret itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CredentialSummary {
    pub id: CredentialId,
    pub kind: CredentialKind,
    pub name: String,
    /// e.g. "sk-ant-…3f9a" — last 4 characters only.
    pub masked: String,
    /// Non-secret companion value (LiteLLM base URL, env var name).
    pub base_url: Option<String>,
    pub env_var: Option<String>,
    pub status: CredentialHealth,
    pub created_at: Timestamp,
    pub expires_at: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CredentialHealth {
    Untested,
    Valid,
    Reachable { latency_ms: u32 },
    ExpiresSoon { days: u32 },
    Invalid { error: String },
}

// ---------------------------------------------------------------- devices & notifications

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Tui,
    Desktop,
    Android,
    Web,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    pub user_id: UserId,
    pub kind: ClientKind,
    pub name: String,
    /// UnifiedPush endpoint URL (Android).
    pub push_endpoint: Option<String>,
    pub last_seen: Timestamp,
}

/// Android channels (design/docs/android.md "Notifications").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationChannel {
    Decisions,
    Failures,
    ReadyForReview,
    LiveWorkers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    PlanReady,
    TaskFailed,
    TaskEscalated,
    TaskReady,
    MemoryApproval,
    WorkersLive,
    SystemAlert,
}

impl NotificationKind {
    pub fn channel(self) -> NotificationChannel {
        match self {
            NotificationKind::PlanReady | NotificationKind::TaskEscalated | NotificationKind::MemoryApproval => {
                NotificationChannel::Decisions
            }
            NotificationKind::TaskFailed | NotificationKind::SystemAlert => NotificationChannel::Failures,
            NotificationKind::TaskReady => NotificationChannel::ReadyForReview,
            NotificationKind::WorkersLive => NotificationChannel::LiveWorkers,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationAction {
    /// Machine action id, e.g. "retry", "approve", "open".
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub user_id: UserId,
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
    /// In-app route, e.g. "task/FP-139", "plan/<id>", "memory/<id>".
    pub deep_link: String,
    pub actions: Vec<NotificationAction>,
    pub task_id: Option<TaskId>,
    pub plan_id: Option<PlanId>,
    pub memory_id: Option<MemoryId>,
    pub read: bool,
    /// The card was acted on ("✓ Retried · attempt 3/3 running").
    pub resolved: Option<String>,
    pub created_at: Timestamp,
}

// ---------------------------------------------------------------- machine

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct MachineStats {
    pub hostname: String,
    /// e.g. "mac-mini · M4 Pro · 12 cores · 32 GB · uptime 18d".
    pub spec_line: String,
    pub cpu_percent: f32,
    /// Last 32 samples, oldest first.
    pub cpu_history: Vec<f32>,
    pub ram_used_gb: f32,
    pub ram_total_gb: f32,
    pub disk_used_gb: f32,
    pub disk_total_gb: f32,
    pub containers_running: u32,
    pub container_slots: u32,
    pub queue_depth: u32,
    pub est_wait_secs: u32,
    pub oldest_queued_secs: u32,
    pub uptime_secs: u64,
    pub runtime_ok: bool,
    pub tailnet_name: Option<String>,
    pub latency_ms: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunningContainer {
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
    pub task_key: Option<String>,
    pub role: WorkerRole,
    pub owner: UserSummary,
    pub mem_mb: u64,
    pub cpu_percent: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub position: u32,
    pub task_id: TaskId,
    pub task_key: String,
    pub title: String,
    pub owner: UserSummary,
    pub est_wait_secs: u32,
}

/// Per-user monthly spend for the admin table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserSpend {
    pub user_id: UserId,
    pub month_usd: f64,
    pub budget_usd: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials() {
        assert_eq!(initials_for("Ada Rao"), "AR");
        assert_eq!(initials_for("jin"), "J");
        assert_eq!(initials_for("Jin Lee Park"), "JP");
        assert_eq!(initials_for(""), "?");
    }
}
