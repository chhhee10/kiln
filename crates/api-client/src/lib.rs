//! Typed client for the Kiln daemon API (`/api/v1`, see `kiln_types::api`).
//!
//! The same [`Client`] works natively (TUI, using a bearer token) and in the
//! browser/WebView (desktop + Android app, using the HTTP-only session cookie
//! set at login; the token is also kept so Tauri can persist it).
//!
//! Live updates: see [`ws`] — `ws::connect` on native returns a stream of
//! [`ServerEvent`]s; on wasm it takes a callback.

use kiln_types::api::*;
use kiln_types::event::{ClientMessage, LogLine, ServerEvent};
use kiln_types::ids::*;
use kiln_types::memory::*;
use kiln_types::model::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::{Arc, RwLock};

pub mod ws;

pub use kiln_types;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The daemon answered with an error body.
    #[error("{}", .error.error)]
    Api { status: u16, error: ApiError },
    #[error("network error: {0}")]
    Http(String),
    #[error("unexpected response: {0}")]
    Decode(String),
    #[error("client {client} is not compatible with daemon {server}; download a matching build from /download")]
    Incompatible { client: String, server: String },
}

impl ClientError {
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, ClientError::Api { status: 401, .. })
    }
    pub fn code(&self) -> Option<&str> {
        match self {
            ClientError::Api { error, .. } => Some(error.code.as_str()),
            _ => None,
        }
    }
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_decode() { ClientError::Decode(e.to_string()) } else { ClientError::Http(e.to_string()) }
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Clone)]
pub struct Client {
    base: String,
    http: reqwest::Client,
    token: Arc<RwLock<Option<String>>>,
}

impl Client {
    /// `base` is the daemon origin, e.g. `https://mac-mini.tailnet.ts.net`
    /// (no trailing `/api/v1`). Empty string = same origin (browser).
    pub fn new(base: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        #[cfg(not(target_arch = "wasm32"))]
        let http = reqwest::Client::builder()
            .user_agent(format!("kiln-client/{}", kiln_types::VERSION))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        #[cfg(target_arch = "wasm32")]
        let http = reqwest::Client::new();
        Self { base, http, token: Arc::new(RwLock::new(None)) }
    }

    pub fn with_token(self, token: impl Into<String>) -> Self {
        self.set_token(Some(token.into()));
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> Option<String> {
        self.token.read().ok().and_then(|t| t.clone())
    }

    pub fn set_token(&self, token: Option<String>) {
        if let Ok(mut t) = self.token.write() {
            *t = token;
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}{}", self.base, API_PREFIX, path)
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, self.url(path));
        if let Some(t) = self.token() {
            rb = rb.bearer_auth(t);
        }
        #[cfg(target_arch = "wasm32")]
        {
            rb = rb.fetch_credentials_include();
        }
        rb
    }

    async fn send<T: DeserializeOwned>(&self, rb: reqwest::RequestBuilder) -> Result<T> {
        let resp = rb.send().await?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            if status == 204 {
                return serde_json::from_str("null").map_err(|e| ClientError::Decode(e.to_string()));
            }
            let bytes = resp.bytes().await?;
            if bytes.is_empty() {
                return serde_json::from_str("null").map_err(|e| ClientError::Decode(e.to_string()));
            }
            serde_json::from_slice(&bytes).map_err(|e| ClientError::Decode(e.to_string()))
        } else {
            let text = resp.text().await.unwrap_or_default();
            let error = serde_json::from_str::<ApiError>(&text)
                .unwrap_or_else(|_| ApiError::new("http", if text.is_empty() { format!("HTTP {status}") } else { text }));
            Err(ClientError::Api { status, error })
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.req(reqwest::Method::GET, path)).await
    }
    async fn get_q<T: DeserializeOwned, Q: Serialize + ?Sized>(&self, path: &str, q: &Q) -> Result<T> {
        self.send(self.req(reqwest::Method::GET, path).query(q)).await
    }
    async fn post<T: DeserializeOwned, B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<T> {
        self.send(self.req(reqwest::Method::POST, path).json(body)).await
    }
    async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.req(reqwest::Method::POST, path)).await
    }
    async fn put<T: DeserializeOwned, B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<T> {
        self.send(self.req(reqwest::Method::PUT, path).json(body)).await
    }
    async fn patch<T: DeserializeOwned, B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<T> {
        self.send(self.req(reqwest::Method::PATCH, path).json(body)).await
    }
    async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.req(reqwest::Method::DELETE, path)).await
    }

    // ------------------------------------------------------------ info & auth

    pub async fn info(&self) -> Result<ServerInfo> {
        self.get("/info").await
    }

    /// `info()` plus the version-match check (PRD §7.13).
    pub async fn check_compatible(&self) -> Result<ServerInfo> {
        let info = self.info().await?;
        if !kiln_types::versions_compatible(kiln_types::VERSION, &info.version) {
            return Err(ClientError::Incompatible { client: kiln_types::VERSION.into(), server: info.version });
        }
        Ok(info)
    }

    pub async fn setup(&self, req: &SetupRequest) -> Result<LoginResponse> {
        let r: LoginResponse = self.post("/setup", req).await?;
        self.set_token(Some(r.token.clone()));
        Ok(r)
    }

    pub async fn login(&self, req: &LoginRequest) -> Result<LoginResponse> {
        let r: LoginResponse = self.post("/auth/login", req).await?;
        self.set_token(Some(r.token.clone()));
        Ok(r)
    }

    pub async fn register(&self, req: &RegisterRequest) -> Result<LoginResponse> {
        let r: LoginResponse = self.post("/auth/register", req).await?;
        self.set_token(Some(r.token.clone()));
        Ok(r)
    }

    pub async fn logout(&self) -> Result<()> {
        let r = self.post_empty("/auth/logout").await;
        self.set_token(None);
        r
    }

    pub async fn me(&self) -> Result<MeResponse> {
        self.get("/auth/me").await
    }

    pub async fn pair_start(&self) -> Result<PairStartResponse> {
        self.post_empty("/pair/start").await
    }

    pub async fn pair_complete(&self, req: &PairCompleteRequest) -> Result<LoginResponse> {
        let r: LoginResponse = self.post("/pair/complete", req).await?;
        self.set_token(Some(r.token.clone()));
        Ok(r)
    }

    pub async fn register_push(&self, req: &RegisterPushRequest) -> Result<Device> {
        self.post("/devices/push", req).await
    }

    // ------------------------------------------------------------ board & tasks

    pub async fn board(&self, q: &BoardQuery) -> Result<BoardResponse> {
        self.get_q("/board", q).await
    }

    pub async fn task(&self, id: TaskId) -> Result<TaskDetail> {
        self.get(&format!("/tasks/{id}")).await
    }

    pub async fn task_action(&self, id: TaskId, action: TaskAction) -> Result<Task> {
        self.post(&format!("/tasks/{id}/action"), &TaskActionRequest { action }).await
    }

    pub async fn task_logs(&self, id: TaskId, q: &LogQuery) -> Result<Vec<LogLine>> {
        self.get_q(&format!("/tasks/{id}/logs"), q).await
    }

    /// Absolute URL of an artifact (for `<img src>` or opening externally).
    pub fn artifact_url(&self, run: RunId, artifact: ArtifactId) -> String {
        self.url(&format!("/runs/{run}/artifacts/{artifact}"))
    }

    pub async fn artifact_bytes(&self, run: RunId, artifact: ArtifactId) -> Result<Vec<u8>> {
        let resp = self.req(reqwest::Method::GET, &format!("/runs/{run}/artifacts/{artifact}")).send().await?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            let error = serde_json::from_str(&text).unwrap_or_else(|_| ApiError::new("http", format!("HTTP {status}")));
            return Err(ClientError::Api { status, error });
        }
        Ok(resp.bytes().await?.to_vec())
    }

    // ------------------------------------------------------------ projects

    pub async fn projects(&self) -> Result<Vec<Project>> {
        self.get("/projects").await
    }
    pub async fn create_project(&self, req: &CreateProjectRequest) -> Result<Project> {
        self.post("/projects", req).await
    }
    pub async fn update_project(&self, id: ProjectId, req: &UpdateProjectRequest) -> Result<Project> {
        self.patch(&format!("/projects/{id}"), req).await
    }

    // ------------------------------------------------------------ planner

    pub async fn threads(&self, q: &ThreadQuery) -> Result<Vec<Thread>> {
        self.get_q("/threads", q).await
    }
    pub async fn create_thread(&self, req: &CreateThreadRequest) -> Result<ThreadDetail> {
        self.post("/threads", req).await
    }
    pub async fn thread(&self, id: ThreadId) -> Result<ThreadDetail> {
        self.get(&format!("/threads/{id}")).await
    }
    pub async fn send_message(&self, id: ThreadId, req: &SendMessageRequest) -> Result<Message> {
        self.post(&format!("/threads/{id}/messages"), req).await
    }
    pub async fn update_plan(&self, id: PlanId, req: &UpdatePlanRequest) -> Result<Plan> {
        self.put(&format!("/plans/{id}"), req).await
    }
    pub async fn approve_plan(&self, id: PlanId, req: &ApprovePlanRequest) -> Result<ApprovePlanResponse> {
        self.post(&format!("/plans/{id}/approve"), req).await
    }

    // ------------------------------------------------------------ memory

    pub async fn memory(&self, q: &MemoryQuery) -> Result<MemoryListResponse> {
        self.get_q("/memory", q).await
    }
    pub async fn memory_item(&self, id: MemoryId) -> Result<MemoryDetail> {
        self.get(&format!("/memory/{id}")).await
    }
    pub async fn update_memory(&self, id: MemoryId, req: &UpdateMemoryRequest) -> Result<MemoryItem> {
        self.patch(&format!("/memory/{id}"), req).await
    }
    pub async fn decide_memory(&self, id: MemoryId, req: &ApproveMemoryRequest) -> Result<MemoryItem> {
        self.post(&format!("/memory/{id}/approve"), req).await
    }

    // ------------------------------------------------------------ credentials & settings

    pub async fn credentials(&self) -> Result<Vec<CredentialSummary>> {
        self.get("/credentials").await
    }
    pub async fn add_credential(&self, req: &CreateCredentialRequest) -> Result<CredentialSummary> {
        self.post("/credentials", req).await
    }
    pub async fn rotate_credential(&self, id: CredentialId, req: &RotateCredentialRequest) -> Result<CredentialSummary> {
        self.put(&format!("/credentials/{id}"), req).await
    }
    pub async fn revoke_credential(&self, id: CredentialId) -> Result<()> {
        self.delete(&format!("/credentials/{id}")).await
    }
    pub async fn test_credential(&self, id: CredentialId) -> Result<CredentialTestResponse> {
        self.post_empty(&format!("/credentials/{id}/test")).await
    }
    pub async fn settings(&self) -> Result<UserSettings> {
        self.get("/settings").await
    }
    pub async fn update_settings(&self, s: &UserSettings) -> Result<UserSettings> {
        self.put("/settings", s).await
    }
    pub async fn harnesses(&self) -> Result<HarnessCatalog> {
        self.get("/harnesses").await
    }

    // ------------------------------------------------------------ notifications & machine

    pub async fn notifications(&self) -> Result<Vec<Notification>> {
        self.get("/notifications").await
    }
    pub async fn mark_notification_read(&self, id: NotificationId) -> Result<()> {
        self.post_empty(&format!("/notifications/{id}/read")).await
    }
    pub async fn notification_action(&self, id: NotificationId, req: &NotificationActionRequest) -> Result<Notification> {
        self.post(&format!("/notifications/{id}/action"), req).await
    }
    pub async fn machine(&self) -> Result<MachineStats> {
        self.get("/machine").await
    }

    // ------------------------------------------------------------ admin

    pub async fn admin_overview(&self) -> Result<AdminOverview> {
        self.get("/admin/overview").await
    }
    pub async fn admin_users(&self) -> Result<Vec<AdminUserRow>> {
        self.get("/admin/users").await
    }
    pub async fn create_user(&self, req: &CreateUserRequest) -> Result<User> {
        self.post("/admin/users", req).await
    }
    pub async fn update_user(&self, id: UserId, req: &UpdateUserRequest) -> Result<User> {
        self.patch(&format!("/admin/users/{id}"), req).await
    }
    pub async fn set_domain(&self, req: &SetDomainRequest) -> Result<ServerInfo> {
        self.put("/admin/domain", req).await
    }

    /// WebSocket URL for live events (`ws://` / `wss://`).
    pub fn ws_url(&self) -> String {
        let base = if self.base.is_empty() { ws::same_origin() } else { self.base.clone() };
        let base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        match self.token() {
            // Browsers can't set headers on WebSocket; the cookie covers them.
            // Native clients pass the token as a query param over TLS.
            Some(t) if cfg!(not(target_arch = "wasm32")) => format!("{base}{API_PREFIX}/ws?token={t}"),
            _ => format!("{base}{API_PREFIX}/ws"),
        }
    }
}

/// Encode a [`ClientMessage`] for the WebSocket.
pub fn encode_client_message(m: &ClientMessage) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "{\"type\":\"ping\"}".into())
}

/// Decode a WebSocket text frame.
pub fn decode_server_event(s: &str) -> Option<ServerEvent> {
    serde_json::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_from_base() {
        let c = Client::new("https://mini.ts.net/").with_token("abc");
        assert_eq!(c.ws_url(), "wss://mini.ts.net/api/v1/ws?token=abc");
        let c = Client::new("http://127.0.0.1:7420");
        assert_eq!(c.ws_url(), "ws://127.0.0.1:7420/api/v1/ws");
        assert_eq!(c.url("/info"), "http://127.0.0.1:7420/api/v1/info");
    }
}
