//! The `/api/v1` router: every route in the `kiln_types::api` table.

mod account;
mod admin;
mod brain;
mod notifications;
mod work;

use crate::error::{ApiResult, AppError};
use crate::state::AppState;
use axum::Router;
use axum::routing::{get, patch, post, put};
use kiln_types::RunId;
use std::path::{Component, Path, PathBuf};

/// All `/api/v1` routes (mounted under [`kiln_types::api::API_PREFIX`]).
pub fn api_router() -> Router<AppState> {
    Router::new()
        // info & auth
        .route("/info", get(account::info))
        .route("/setup", post(account::setup))
        .route("/auth/login", post(account::login))
        .route("/auth/register", post(account::register))
        .route("/auth/logout", post(account::logout))
        .route("/auth/me", get(account::me))
        .route("/pair/start", post(account::pair_start))
        .route("/pair/complete", post(account::pair_complete))
        .route("/devices/push", post(account::register_push))
        // board & tasks
        .route("/board", get(work::board))
        .route("/tasks/{id}", get(work::task_detail))
        .route("/tasks/{id}/action", post(work::task_action))
        .route("/tasks/{id}/logs", get(work::task_logs))
        .route("/runs/{id}/artifacts/{artifact_id}", get(work::artifact))
        // projects
        .route(
            "/projects",
            get(work::list_projects).post(work::create_project),
        )
        .route("/projects/{id}", patch(work::update_project))
        // planner
        .route(
            "/threads",
            get(work::list_threads).post(work::create_thread),
        )
        .route("/threads/{id}", get(work::get_thread))
        .route("/threads/{id}/messages", post(work::send_message))
        .route("/plans", get(work::list_plans))
        .route("/plans/{id}", put(work::update_plan))
        .route("/plans/{id}/approve", post(work::approve_plan))
        // memory
        .route("/memory", get(brain::list_memory))
        .route(
            "/memory/{id}",
            get(brain::get_memory).patch(brain::update_memory),
        )
        .route("/memory/{id}/approve", post(brain::decide_memory))
        // credentials & settings
        .route(
            "/credentials",
            get(brain::list_credentials).post(brain::add_credential),
        )
        .route(
            "/credentials/{id}",
            put(brain::rotate_credential).delete(brain::revoke_credential),
        )
        .route("/credentials/{id}/test", post(brain::test_credential))
        .route(
            "/settings",
            get(account::get_settings).put(account::put_settings),
        )
        .route("/harnesses", get(account::harnesses))
        // notifications & machine
        .route("/notifications", get(notifications::list))
        .route("/notifications/{id}/read", post(notifications::read))
        .route("/notifications/{id}/action", post(notifications::action))
        .route("/machine", get(account::machine))
        // admin
        .route("/admin/overview", get(admin::overview))
        .route("/admin/users", get(admin::users).post(admin::create_user))
        .route("/admin/users/{id}", patch(admin::update_user))
        .route("/admin/domain", put(admin::set_domain))
        // live updates
        .route("/ws", get(crate::ws::handler))
        .fallback(|| async { AppError::not_found("route") })
}

/// Host path of a file inside a run folder, refusing anything that would
/// leave `<data>/runs/<run_id>/` (absolute paths, `..`, symlinks pointing
/// outside). The file must exist.
pub fn artifact_path(state: &AppState, run: RunId, rel: &str) -> ApiResult<PathBuf> {
    let run_dir = state.cfg.data_dir.join("runs").join(run.to_string());
    resolve_inside(&run_dir, rel).ok_or_else(|| AppError::not_found("artifact"))
}

/// Join `rel` under `root` and canonicalize, returning `None` if the result
/// escapes `root` or does not exist as a regular file.
pub fn resolve_inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel.is_empty() || rel_path.is_absolute() || rel.contains('\0') {
        return None;
    }
    if rel_path
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let full = std::fs::canonicalize(root.join(rel_path)).ok()?;
    (full.starts_with(&root) && full.is_file()).then_some(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run");
        std::fs::create_dir_all(run.join("out/artifacts")).unwrap();
        std::fs::write(run.join("out/artifacts/a.png"), b"x").unwrap();
        std::fs::write(d.path().join("secret.txt"), b"s").unwrap();
        assert!(resolve_inside(&run, "out/artifacts/a.png").is_some());
        assert!(resolve_inside(&run, "./out/artifacts/a.png").is_some());
        assert!(resolve_inside(&run, "../secret.txt").is_none());
        assert!(resolve_inside(&run, "out/../../secret.txt").is_none());
        assert!(resolve_inside(&run, "/etc/passwd").is_none());
        assert!(resolve_inside(&run, "out/artifacts").is_none());
        assert!(resolve_inside(&run, "missing").is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.path().join("secret.txt"), run.join("out/link")).unwrap();
            assert!(resolve_inside(&run, "out/link").is_none());
        }
    }
}
