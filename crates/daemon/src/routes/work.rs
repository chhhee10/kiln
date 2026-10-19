//! Board, tasks, logs, artifacts, projects, planner threads and plans.

use crate::error::{ApiResult, AppError};
use crate::extract::{Body, P, Q};
use crate::session::{AdminUser, CurrentUser};
use crate::state::AppState;
use crate::views;
use axum::Json;
use axum::body::Body as HttpBody;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use kiln_store::{LogTarget, NewProject, TaskFilter};
use kiln_types::api::*;
use kiln_types::event::LogLine;
use kiln_types::*;
use std::time::Duration;

// ---------------------------------------------------------------- board & tasks

pub async fn board(
    State(st): State<AppState>,
    me: CurrentUser,
    Q(q): Q<BoardQuery>,
) -> ApiResult<Json<BoardResponse>> {
    let mut owners = Vec::new();
    if let Some(list) = q.users.as_deref() {
        for s in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            owners.push(
                s.parse::<UserId>()
                    .map_err(|_| AppError::invalid(format!("bad user id {s:?}")))?,
            );
        }
    }
    let users = st.store.list_users().await?;
    let active: Vec<&User> = users
        .iter()
        .filter(|u| u.status == UserStatus::Active)
        .collect();
    let tasks = st
        .store
        .list_tasks(TaskFilter {
            project: q.project,
            plan: None,
            owners,
            states: q.state.into_iter().collect(),
            limit: Some(2000),
        })
        .await?
        .iter()
        .map(|t| views::card_view(t, &me.user))
        .collect();
    // Idea column: the caller's own draft plans (plans are owner-only).
    let ideas = st
        .store
        .list_plans(Some(me.user.id), Some(PlanStatus::Drafting))
        .await?
        .into_iter()
        .filter(|p| q.project.is_none_or(|id| p.project_id == id))
        .collect();
    Ok(Json(BoardResponse {
        tasks,
        ideas,
        projects: st.store.list_projects().await?,
        users: active.into_iter().map(UserSummary::from).collect(),
    }))
}

async fn load_task(st: &AppState, id: TaskId) -> ApiResult<Task> {
    st.store.get_task(id).await.map_err(|e| match e {
        kiln_store::StoreError::NotFound => AppError::not_found("task"),
        other => other.into(),
    })
}

pub async fn task_detail(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<TaskId>,
) -> ApiResult<Json<TaskDetail>> {
    let task = load_task(&st, id).await?;
    Ok(Json(views::task_detail(&st, &me.user, task).await?))
}

pub async fn task_action(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<TaskId>,
    Body(req): Body<TaskActionRequest>,
) -> ApiResult<Json<Task>> {
    let task = load_task(&st, id).await?;
    if task.owner_id != me.user.id && !me.user.is_admin() {
        return Err(AppError::forbidden(
            "only the task owner or an admin can act on this task",
        ));
    }
    if let TaskAction::Feedback { text, .. } = &req.action
        && (text.trim().is_empty() || text.len() > 20_000)
    {
        return Err(AppError::invalid(
            "feedback text must be 1–20000 characters",
        ));
    }
    let t = st.scheduler.task_action(&me.user, id, req.action).await?;
    Ok(Json(views::card_view(&t, &me.user)))
}

pub async fn task_logs(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<TaskId>,
    Q(q): Q<LogQuery>,
) -> ApiResult<Json<Vec<LogLine>>> {
    let task = load_task(&st, id).await?;
    if task.owner_id != me.user.id {
        return Err(AppError::forbidden(
            "task logs are visible to their owner only",
        ));
    }
    Ok(Json(
        st.store.list_log_lines(LogTarget::Task(id), &q).await?,
    ))
}

/// Content types that must never render on our origin.
fn safe_content_type(mime: &str, path: &str) -> String {
    let m = if mime.trim().is_empty() {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .essence_str()
            .to_string()
    } else {
        mime.trim().to_ascii_lowercase()
    };
    let active =
        m.contains("html") || m.contains("xml") || m.contains("javascript") || m.contains("svg");
    if active {
        "text/plain; charset=utf-8".into()
    } else {
        m
    }
}

pub async fn artifact(
    State(st): State<AppState>,
    me: CurrentUser,
    P((run_id, artifact_id)): P<(RunId, ArtifactId)>,
) -> ApiResult<Response> {
    let run = st
        .store
        .get_run(run_id)
        .await
        .map_err(|_| AppError::not_found("artifact"))?;
    if run.owner_id != me.user.id {
        // Admins included: they see artifact metadata on the task, never content.
        return Err(AppError::forbidden(
            "artifacts are visible to their owner only",
        ));
    }
    let a = st
        .store
        .get_artifact(artifact_id)
        .await
        .map_err(|_| AppError::not_found("artifact"))?;
    if a.run_id != run_id {
        return Err(AppError::not_found("artifact"));
    }
    let path = crate::routes::artifact_path(&st, run_id, &a.path)?;
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| AppError::not_found("artifact"))?;
    let len = file.metadata().await.map(|m| m.len()).ok();
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Response::new(HttpBody::from_stream(stream));
    let h = resp.headers_mut();
    let ct = safe_content_type(&a.mime, &a.path);
    let inline =
        ct.starts_with("image/") || ct.starts_with("video/") || ct.starts_with("text/plain");
    if let Ok(v) = HeaderValue::from_str(&ct) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Some(len) = len {
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    let name: String = a
        .path
        .rsplit('/')
        .next()
        .unwrap_or("artifact")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    let disp = format!(
        "{}; filename=\"{}\"",
        if inline { "inline" } else { "attachment" },
        if name.is_empty() { "artifact" } else { &name }
    );
    if let Ok(v) = HeaderValue::from_str(&disp) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; img-src 'self'; media-src 'self'; sandbox"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    Ok(resp)
}

// ---------------------------------------------------------------- projects

pub async fn list_projects(
    State(st): State<AppState>,
    _me: CurrentUser,
) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(st.store.list_projects().await?))
}

/// Accept https / ssh / git@ remotes; local paths only without GitHub.
fn check_repo_url(st: &AppState, url: &str) -> ApiResult<()> {
    let u = url.trim();
    let remote = u.starts_with("https://")
        || u.starts_with("ssh://")
        || (u.starts_with("git@") && u.contains(':'));
    let local = u.starts_with("file://") || u.starts_with('/');
    if remote || (local && (st.dev || st.cfg.github.mode == crate::config::GithubMode::Local)) {
        return Ok(());
    }
    if local {
        return Err(AppError::invalid(
            "local repository paths are only allowed with github.mode = \"local\" or --dev",
        ));
    }
    Err(AppError::invalid(
        "repo_url must be an https://, ssh:// or git@host:path remote",
    ))
}

/// Fetch the mirror and parse qa.yaml, bounded so a slow remote can't hang the request.
async fn refresh(st: &AppState, project: Project) -> Project {
    match tokio::time::timeout(
        Duration::from_secs(20),
        st.scheduler.refresh_project(project.id),
    )
    .await
    {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::warn!(project = %project.key, error = %e, "project refresh failed");
            project
        }
        Err(_) => {
            tracing::warn!(project = %project.key, "project refresh timed out; continuing in the background");
            let st2 = st.clone();
            tokio::spawn(async move {
                let _ = st2.scheduler.refresh_project(project.id).await;
            });
            project
        }
    }
}

pub async fn create_project(
    State(st): State<AppState>,
    AdminUser(_me): AdminUser,
    Body(req): Body<CreateProjectRequest>,
) -> ApiResult<(StatusCode, Json<Project>)> {
    if req.name.trim().is_empty() {
        return Err(AppError::invalid("project name is required"));
    }
    check_repo_url(&st, &req.repo_url)?;
    let mut np = NewProject::new(req.key.trim(), req.name.trim(), req.repo_url.trim());
    np.default_branch = req.default_branch.trim().to_string();
    np.qa_path = req.qa_path.trim().to_string();
    let p = st.store.create_project(np).await?;
    tracing::info!(project = %p.key, "project registered");
    Ok((StatusCode::CREATED, Json(refresh(&st, p).await)))
}

pub async fn update_project(
    State(st): State<AppState>,
    AdminUser(_me): AdminUser,
    P(id): P<ProjectId>,
    Body(req): Body<UpdateProjectRequest>,
) -> ApiResult<Json<Project>> {
    let needs_refresh = req.default_branch.is_some() || req.qa_path.is_some();
    let p = st.store.update_project(id, req).await?;
    Ok(Json(if needs_refresh {
        refresh(&st, p).await
    } else {
        p
    }))
}

// ---------------------------------------------------------------- planner

pub async fn list_threads(
    State(st): State<AppState>,
    me: CurrentUser,
    Q(q): Q<ThreadQuery>,
) -> ApiResult<Json<Vec<Thread>>> {
    Ok(Json(st.store.list_threads(me.user.id, q.project).await?))
}

fn check_body(body: &str) -> ApiResult<()> {
    if body.trim().is_empty() || body.len() > 50_000 {
        return Err(AppError::invalid("message must be 1–50000 characters"));
    }
    Ok(())
}

pub async fn create_thread(
    State(st): State<AppState>,
    me: CurrentUser,
    Body(req): Body<CreateThreadRequest>,
) -> ApiResult<(StatusCode, Json<ThreadDetail>)> {
    check_body(&req.message)?;
    st.store
        .get_project(req.project_id)
        .await
        .map_err(|_| AppError::not_found("project"))?;
    let d = st.scheduler.create_thread(&me.user, req).await?;
    Ok((StatusCode::CREATED, Json(d)))
}

/// A thread the caller owns (others get 404: threads are private).
async fn own_thread(st: &AppState, me: &User, id: ThreadId) -> ApiResult<Thread> {
    match st.store.get_thread(id).await {
        Ok(t) if t.owner_id == me.id => Ok(t),
        Ok(_) | Err(kiln_store::StoreError::NotFound) => Err(AppError::not_found("thread")),
        Err(e) => Err(e.into()),
    }
}

/// Assemble a thread view (same shape the scheduler returns on create).
async fn thread_detail(st: &AppState, me: &User, thread: Thread) -> ApiResult<ThreadDetail> {
    let messages = st.store.list_messages(thread.id).await?;
    let plan = st.store.get_plan_for_thread(thread.id).await?;
    let settings = st.settings_for(me.id).await?;
    let planner_model = settings
        .worker_defaults
        .get(&WorkerRole::Planner)
        .or_else(|| st.default_workers.get(&WorkerRole::Planner))
        .map(|c| c.model.clone())
        .unwrap_or_default();
    // The planner's L3 pack is what it always has in context (cached; no use is recorded).
    let memories_in_context = match st
        .memory
        .pack_with_items(me.id, thread.project_id, WorkerRole::Planner)
        .await
    {
        Ok((_, ids)) => ids.len() as u32,
        Err(e) => {
            tracing::debug!(error = %e, "planner pack unavailable");
            0
        }
    };
    Ok(ThreadDetail {
        thread,
        messages,
        plan,
        memories_in_context,
        planner_model,
    })
}

pub async fn get_thread(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<ThreadId>,
) -> ApiResult<Json<ThreadDetail>> {
    let thread = own_thread(&st, &me.user, id).await?;
    Ok(Json(thread_detail(&st, &me.user, thread).await?))
}

pub async fn send_message(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<ThreadId>,
    Body(req): Body<SendMessageRequest>,
) -> ApiResult<Json<Message>> {
    check_body(&req.body)?;
    own_thread(&st, &me.user, id).await?;
    Ok(Json(st.scheduler.planner_message(&me.user, id, req).await?))
}

pub async fn list_plans(
    State(st): State<AppState>,
    me: CurrentUser,
    Q(q): Q<PlanListQuery>,
) -> ApiResult<Json<Vec<Plan>>> {
    let plans = st
        .store
        .list_plans(Some(me.user.id), q.status)
        .await?
        .into_iter()
        .filter(|p| q.project.is_none_or(|id| p.project_id == id))
        .collect();
    Ok(Json(plans))
}

async fn own_plan(st: &AppState, me: &User, id: PlanId) -> ApiResult<Plan> {
    match st.store.get_plan(id).await {
        Ok(p) if p.owner_id == me.id => Ok(p),
        Ok(_) | Err(kiln_store::StoreError::NotFound) => Err(AppError::not_found("plan")),
        Err(e) => Err(e.into()),
    }
}

pub async fn update_plan(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<PlanId>,
    Body(req): Body<UpdatePlanRequest>,
) -> ApiResult<Json<Plan>> {
    own_plan(&st, &me.user, id).await?;
    req.spec
        .validate()
        .map_err(|e| AppError::invalid(e.to_string()))?;
    Ok(Json(st.scheduler.update_plan(&me.user, id, req).await?))
}

pub async fn approve_plan(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<PlanId>,
    Body(req): Body<ApprovePlanRequest>,
) -> ApiResult<Json<ApprovePlanResponse>> {
    own_plan(&st, &me.user, id).await?;
    Ok(Json(st.scheduler.approve_plan(&me.user, id, req).await?))
}
