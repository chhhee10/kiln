//! Scheduler errors and their API mapping.

use kiln_types::api::ApiError;

/// Errors returned by [`crate::Scheduler`] commands.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// The object does not exist (or is not visible to the caller).
    #[error("{0} not found")]
    NotFound(String),
    /// The caller may not act on this object (HTTP 403).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The request is malformed or breaks a rule (HTTP 400).
    #[error("{0}")]
    Invalid(String),
    /// The object is not in a state that allows the action (HTTP 409).
    #[error("conflict: {0}")]
    Conflict(String),
    /// The scheduler is shutting down.
    #[error("the scheduler is shutting down")]
    ShuttingDown,
    #[error(transparent)]
    Store(kiln_store::StoreError),
    #[error("git: {0}")]
    Git(#[from] kiln_gitops::GitError),
    #[error("container runtime: {0}")]
    Sandbox(#[from] kiln_sandbox::SandboxError),
    #[error("memory: {0}")]
    Memory(#[from] kiln_memory::MemoryError),
    #[error("vault: {0}")]
    Vault(#[from] kiln_vault::VaultError),
    #[error("notifier: {0}")]
    Notifier(#[from] kiln_notifier::NotifierError),
    #[error("harness: {0}")]
    Harness(#[from] kiln_harness::HarnessError),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Anything else (a bug or an unexpected state).
    #[error("{0}")]
    Internal(String),
}

impl From<kiln_store::StoreError> for SchedulerError {
    fn from(e: kiln_store::StoreError) -> Self {
        match e {
            kiln_store::StoreError::NotFound => SchedulerError::NotFound("object".into()),
            kiln_store::StoreError::Conflict(m) => SchedulerError::Conflict(m),
            kiln_store::StoreError::Invalid(m) => SchedulerError::Invalid(m),
            other => SchedulerError::Store(other),
        }
    }
}

impl SchedulerError {
    /// Machine-readable code (matches `ApiError::code`).
    pub fn code(&self) -> &'static str {
        match self {
            SchedulerError::NotFound(_) => "not_found",
            SchedulerError::Forbidden(_) => "forbidden",
            SchedulerError::Invalid(_) => "invalid",
            SchedulerError::Conflict(_) => "conflict",
            SchedulerError::Memory(m) if m.is_not_found() => "not_found",
            SchedulerError::Memory(m) if m.is_forbidden() => "forbidden",
            SchedulerError::Memory(m) if m.is_invalid() => "invalid",
            SchedulerError::Memory(m) if m.is_conflict() => "conflict",
            _ => "internal",
        }
    }

    /// HTTP status for the daemon.
    pub fn status(&self) -> u16 {
        match self.code() {
            "not_found" => 404,
            "forbidden" => 403,
            "invalid" => 400,
            "conflict" => 409,
            _ if matches!(self, SchedulerError::ShuttingDown) => 503,
            _ => 500,
        }
    }

    /// API error body. Internal errors get a generic message (details are
    /// logged, never sent to clients).
    pub fn to_api_error(&self) -> ApiError {
        match self.code() {
            "internal" => ApiError::new("internal", "internal error"),
            code => ApiError::new(code, self.to_string()),
        }
    }

    /// True for [`SchedulerError::Conflict`].
    pub fn is_conflict(&self) -> bool {
        matches!(self, SchedulerError::Conflict(_))
    }
}

/// Result alias for scheduler operations.
pub type Result<T, E = SchedulerError> = std::result::Result<T, E>;
