//! HTTP error mapping: every failure becomes an `ApiError` JSON body with a
//! matching status code. Internal details are logged, never sent.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use kiln_auth::AuthError;
use kiln_memory::MemoryError;
use kiln_store::StoreError;
use kiln_types::api::ApiError;
use kiln_vault::VaultError;

/// An API failure.
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub body: ApiError,
}

/// Handler result.
pub type ApiResult<T> = Result<T, AppError>;

impl AppError {
    pub fn new(status: StatusCode, code: &str, msg: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiError::new(code, msg),
        }
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "not signed in")
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", msg)
    }
    pub fn not_found(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} not found"),
        )
    }
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid", msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", msg)
    }
    pub fn too_many() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many attempts; try again in a minute",
        )
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", msg)
    }
    /// Log `err` and return a generic 500.
    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
    /// Status for a wire code (used when a crate already produced an ApiError).
    pub fn from_code(code: &str, msg: String) -> Self {
        let status = match code {
            "unauthorized" => StatusCode::UNAUTHORIZED,
            "forbidden" => StatusCode::FORBIDDEN,
            "not_found" => StatusCode::NOT_FOUND,
            "invalid" => StatusCode::BAD_REQUEST,
            "conflict" => StatusCode::CONFLICT,
            "not_allowlisted" => StatusCode::FORBIDDEN,
            "setup_required" => StatusCode::CONFLICT,
            "rate_limited" => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self::new(status, code, msg)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => AppError::new(StatusCode::NOT_FOUND, "not_found", "not found"),
            StoreError::Conflict(m) => AppError::conflict(m),
            StoreError::Invalid(m) => AppError::invalid(m),
            other => AppError::internal(other),
        }
    }
}

impl From<AuthError> for AppError {
    fn from(e: AuthError) -> Self {
        if let AuthError::Store(s) = &e
            && !matches!(s, StoreError::NotFound | StoreError::Conflict(_))
        {
            return AppError::internal(s);
        }
        let api = e.to_api_error();
        AppError::from_code(&api.code, api.error)
    }
}

impl From<VaultError> for AppError {
    fn from(e: VaultError) -> Self {
        match e {
            VaultError::NotFound => AppError::not_found("credential"),
            VaultError::Invalid(m) => AppError::invalid(m),
            VaultError::Store(s) => s.into(),
            // Crypto / key problems are internal and must not leak details.
            other => AppError::internal(other),
        }
    }
}

impl From<MemoryError> for AppError {
    fn from(e: MemoryError) -> Self {
        match e {
            MemoryError::NotFound => AppError::not_found("memory item"),
            MemoryError::Forbidden(m) => AppError::forbidden(m),
            MemoryError::Invalid(m) => AppError::invalid(m),
            MemoryError::Conflict(m) => AppError::conflict(m),
            other => AppError::internal(other),
        }
    }
}

impl From<kiln_notifier::NotifierError> for AppError {
    fn from(e: kiln_notifier::NotifierError) -> Self {
        match e {
            kiln_notifier::NotifierError::Store(s) => s.into(),
        }
    }
}

impl From<kiln_scheduler::SchedulerError> for AppError {
    fn from(e: kiln_scheduler::SchedulerError) -> Self {
        crate::sched_err::map(e)
    }
}
