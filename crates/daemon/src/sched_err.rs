//! Scheduler error → HTTP mapping.

use crate::error::AppError;
use axum::http::StatusCode;
use kiln_scheduler::SchedulerError;

pub fn map(e: SchedulerError) -> AppError {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if status.is_server_error() {
        if matches!(e, SchedulerError::ShuttingDown) {
            return AppError::unavailable("the daemon is restarting; try again shortly");
        }
        return AppError::internal(e);
    }
    let body = e.to_api_error();
    AppError { status, body }
}
