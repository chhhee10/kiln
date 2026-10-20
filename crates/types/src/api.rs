//! HTTP API contract (PRD §7.11). All routes live under `/api/v1`.
//! Auth: `Authorization: Bearer <token>` (TUI) or the `kiln_session`
//! HTTP-only cookie (desktop/Android/web). Errors are `ApiError` JSON with a
//! matching HTTP status.
//!
//! | Method | Path | Body → Response | Who |
//! |---|---|---|---|
//! | GET  | /info | → ServerInfo | public |
//! | POST | /setup | SetupRequest → LoginResponse | public, only while no users exist |
//! | POST | /auth/login | LoginRequest → LoginResponse | public |
//! | POST | /auth/register | RegisterRequest → LoginResponse | public, allowlisted domain |
//! | POST | /auth/logout | → () | user |
//! | GET  | /auth/me | → MeResponse | user |
//! | POST | /pair/start | → PairStartResponse | user (desktop/TUI shows the QR) |
//! | POST | /pair/complete | PairCompleteRequest → LoginResponse | public, one-time code |
//! | POST | /devices/push | RegisterPushRequest → Device | user |
//! | GET  | /board | BoardQuery (query string) → BoardResponse | user |
//! | GET  | /tasks/{id} | → TaskDetail | user (own or same project) |
//! | POST | /tasks/{id}/action | TaskActionRequest → Task | owner/admin |
//! | GET  | /tasks/{id}/logs | LogQuery → Vec<LogLine> | user |
//! | GET  | /runs/{id}/artifacts/{artifact_id} | → bytes | owner/admin |
//! | GET  | /projects | → Vec<Project> | user |
//! | POST | /projects | CreateProjectRequest → Project | admin |
//! | PATCH| /projects/{id} | UpdateProjectRequest → Project | admin |
//! | GET  | /threads | ThreadQuery → Vec<Thread> | user (own) |
//! | POST | /threads | CreateThreadRequest → ThreadDetail | user |
//! | GET  | /threads/{id} | → ThreadDetail | owner |
//! | POST | /threads/{id}/messages | SendMessageRequest → Message | owner (planner replies arrive over WS) |
//! | GET  | /plans | PlanListQuery (query string) → Vec<Plan> | user (own) |
//! | PUT  | /plans/{id} | UpdatePlanRequest → Plan | owner |
//! | POST | /plans/{id}/approve | ApprovePlanRequest → ApprovePlanResponse | owner |
//! | GET  | /memory | MemoryQuery → MemoryListResponse | user (visibility-filtered) |
//! | GET  | /memory/{id} | → MemoryDetail | visible |
//! | PATCH| /memory/{id} | UpdateMemoryRequest → MemoryItem | owner |
//! | POST | /memory/{id}/approve | ApproveMemoryRequest → MemoryItem | owner |
//! | GET  | /credentials | → Vec<CredentialSummary> | user (own) |
//! | POST | /credentials | CreateCredentialRequest → CredentialSummary | user |
//! | PUT  | /credentials/{id} | RotateCredentialRequest → CredentialSummary | owner |
//! | DELETE | /credentials/{id} | → () | owner |
//! | POST | /credentials/{id}/test | → CredentialTestResponse | owner |
//! | GET  | /settings | → UserSettings | user |
//! | PUT  | /settings | UserSettings → UserSettings | user |
//! | GET  | /harnesses | → HarnessCatalog | user |
//! | GET  | /notifications | → Vec<Notification> | user |
//! | POST | /notifications/{id}/read | → () | user |
//! | POST | /notifications/{id}/action | NotificationActionRequest → Notification | user (Android inline actions) |
//! | GET  | /machine | → MachineStats | user |
//! | GET  | /admin/overview | → AdminOverview | admin |
//! | GET  | /admin/users | → Vec<AdminUserRow> | admin |
//! | POST | /admin/users | CreateUserRequest → User | admin |
//! | PATCH| /admin/users/{id} | UpdateUserRequest → User | admin |
//! | PUT  | /admin/domain | SetDomainRequest → ServerInfo | admin |
//! | GET  | /ws | WebSocket: ServerEvent ↓ / ClientMessage ↑ | user |
//!
//! Non-API routes: `/app/*` (web build of the Leptos UI), `/download`
//! (client builds matching the daemon version), `/healthz`.

use crate::event::LogLine;
use crate::ids::*;
use crate::memory::*;
use crate::model::*;
use crate::plan::{PlanSpec, WorkerChoice};
use crate::status::*;
use serde::{Deserialize, Serialize};

pub const API_PREFIX: &str = "/api/v1";
pub const SESSION_COOKIE: &str = "kiln_session";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {error}")]
pub struct ApiError {
    /// Machine-readable code: "unauthorized", "forbidden", "not_found",
    /// "invalid", "conflict", "not_allowlisted", "setup_required", "internal".
    pub code: String,
    /// Human-readable message, safe to show in the UI.
    pub error: String,
}

impl ApiError {
    pub fn new(code: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            error: error.into(),
        }
    }
}

// ---------------------------------------------------------------- info & auth

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub product: String,
    pub version: String,
    pub setup_required: bool,
    pub allowed_domain: Option<String>,
    pub hostname: String,
    /// e.g. "mac-mini.tailnet-abc.ts.net".
    pub tailnet_host: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetupRequest {
    pub admin_email: String,
    pub admin_name: String,
    pub password: String,
    pub allowed_domain: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub client: Option<ClientKind>,
    #[serde(default)]
    pub device_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub name: String,
    pub password: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoginResponse {
    /// Bearer token. Also set as an HTTP-only cookie for browser clients.
    pub token: String,
    pub user: User,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeResponse {
    pub user: User,
    pub settings: UserSettings,
    pub server: ServerInfo,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairStartResponse {
    pub code: String,
    pub server_url: String,
    /// `kiln://pair?url=<server_url>&code=<code>`, encoded in the QR.
    pub pair_uri: String,
    /// SVG of the QR code.
    pub qr_svg: String,
    pub expires_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PairCompleteRequest {
    pub code: String,
    pub client: ClientKind,
    pub device_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisterPushRequest {
    pub endpoint: String,
    pub device_name: String,
    /// Web Push keys from the UnifiedPush connector (base64url). With both
    /// set, payloads are encrypted end to end (RFC 8291).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p256dh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
}

// ---------------------------------------------------------------- board & tasks

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct BoardQuery {
    #[serde(default)]
    pub project: Option<ProjectId>,
    /// Comma-separated user ids in the query string.
    #[serde(default)]
    pub users: Option<String>,
    #[serde(default)]
    pub state: Option<TaskState>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardResponse {
    pub tasks: Vec<Task>,
    /// Draft plans shown in the Idea column.
    pub ideas: Vec<Plan>,
    pub projects: Vec<Project>,
    pub users: Vec<UserSummary>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TaskAction {
    Stop,
    Retry,
    Escalate {
        #[serde(default)]
        note: Option<String>,
    },
    Feedback {
        text: String,
        #[serde(default = "yes")]
        attach_qa: bool,
        #[serde(default = "yes")]
        attach_verdict: bool,
        #[serde(default)]
        save_memory: bool,
    },
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskActionRequest {
    #[serde(flatten)]
    pub action: TaskAction,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct LogQuery {
    #[serde(default)]
    pub role: Option<WorkerRole>,
    #[serde(default)]
    pub after_seq: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub filter: Option<String>,
}

pub type LogsResponse = Vec<LogLine>;

// ---------------------------------------------------------------- projects

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateProjectRequest {
    pub key: String,
    pub name: String,
    pub repo_url: String,
    #[serde(default = "main_branch")]
    pub default_branch: String,
    #[serde(default = "qa_path")]
    pub qa_path: String,
}

fn main_branch() -> String {
    "main".into()
}
fn qa_path() -> String {
    "qa.yaml".into()
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct UpdateProjectRequest {
    pub name: Option<String>,
    pub default_branch: Option<String>,
    pub qa_path: Option<String>,
    pub image: Option<String>,
    pub loop_cap: Option<u32>,
    pub resources: Option<ResourceProfile>,
}

// ---------------------------------------------------------------- planner

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct ThreadQuery {
    #[serde(default)]
    pub project: Option<ProjectId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateThreadRequest {
    pub project_id: ProjectId,
    /// First message; the thread title is derived from it.
    pub message: String,
    #[serde(default)]
    pub voice: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadDetail {
    pub thread: Thread,
    pub messages: Vec<Message>,
    pub plan: Option<Plan>,
    /// Memory items the planner had in context ("3 memories in context").
    pub memories_in_context: u32,
    pub planner_model: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub body: String,
    #[serde(default)]
    pub voice: bool,
}

/// `GET /plans`: the caller's plans, newest first (TUI plan list).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct PlanListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PlanStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpdatePlanRequest {
    pub spec: PlanSpec,
    /// Optimistic concurrency: the version the client edited.
    pub version: u32,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct ApprovePlanRequest {
    #[serde(default)]
    pub version: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovePlanResponse {
    pub plan: Plan,
    pub tasks: Vec<Task>,
    /// "3 tasks dispatched · 1 building, 2 queued".
    pub summary: String,
}

// ---------------------------------------------------------------- memory

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryListResponse {
    pub items: Vec<MemoryItem>,
    pub total: u32,
    pub pending_approvals: u32,
    pub runs_this_week: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryDetail {
    pub item: MemoryItem,
    pub recently_used: Vec<MemoryUse>,
    pub owner: UserSummary,
    pub shared_with: Vec<UserSummary>,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct UpdateMemoryRequest {
    pub title: Option<String>,
    pub body: Option<String>,
    /// Choosing Company (or Shared) on an item → status Pending + approval.
    pub visibility: Option<Visibility>,
    pub shared_with: Option<Vec<UserId>>,
    pub archived: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDecision {
    /// Approve the requested visibility.
    Approve,
    /// Keep it at Shared (decline Company).
    KeepShared,
    /// Keep it Private.
    KeepPrivate,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApproveMemoryRequest {
    pub decision: MemoryDecision,
    #[serde(default)]
    pub edit: Option<UpdateMemoryRequest>,
}

// ---------------------------------------------------------------- credentials & settings

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateCredentialRequest {
    pub kind: CredentialKind,
    pub name: String,
    pub secret: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub env_var: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RotateCredentialRequest {
    pub secret: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CredentialTestResponse {
    pub ok: bool,
    pub latency_ms: u32,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessInfo {
    /// Config id: "claude_sdk", "codex", "opencode", "mock".
    pub id: String,
    pub label: String,
    pub models: Vec<String>,
    pub experimental: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessCatalog {
    pub harnesses: Vec<HarnessInfo>,
    pub defaults: std::collections::BTreeMap<WorkerRole, WorkerChoice>,
}

// ---------------------------------------------------------------- notifications

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationActionRequest {
    pub action: String,
    #[serde(default)]
    pub text: Option<String>,
}

// ---------------------------------------------------------------- admin

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminUserRow {
    pub user: User,
    pub spend: UserSpend,
    pub running: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdminOverview {
    pub machine: MachineStats,
    pub users: Vec<AdminUserRow>,
    pub containers: Vec<RunningContainer>,
    pub queue: Vec<QueueEntry>,
    pub projects: Vec<Project>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub email: String,
    pub name: String,
    pub password: String,
    #[serde(default = "member")]
    pub role: UserRole,
}

fn member() -> UserRole {
    UserRole::Member
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct UpdateUserRequest {
    pub name: Option<String>,
    pub role: Option<UserRole>,
    pub status: Option<UserStatus>,
    pub limits: Option<UserLimits>,
    pub password: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetDomainRequest {
    pub allowed_domain: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_action_wire() {
        let a: TaskActionRequest =
            serde_json::from_str(r#"{"action":"feedback","text":"fix it"}"#).unwrap();
        assert_eq!(
            a.action,
            TaskAction::Feedback {
                text: "fix it".into(),
                attach_qa: true,
                attach_verdict: true,
                save_memory: false
            }
        );
        let a: TaskActionRequest = serde_json::from_str(r#"{"action":"retry"}"#).unwrap();
        assert_eq!(a.action, TaskAction::Retry);
    }
}
