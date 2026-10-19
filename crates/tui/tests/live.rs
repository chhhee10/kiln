//! Live mode against an in-process mock daemon: login, session file, board
//! mapping, task actions and credentials go through `kiln-api-client`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use kiln_api_client::Client;
use kiln_tui::live::LiveSource;
use kiln_tui::model::{Cmd, DataEvent};
use kiln_tui::session::Session;
use kiln_tui::source::DataSource;
use kiln_types::api::*;
use kiln_types::memory::Visibility;
use kiln_types::model::*;
use kiln_types::plan::WorkerChoice;
use kiln_types::*;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone)]
struct Mock {
    task: Arc<Mutex<Task>>,
    user: User,
    project: Project,
}

fn user() -> User {
    User {
        id: UserId::new(),
        email: "alex@company.dev".into(),
        name: "Alex Rao".into(),
        role: UserRole::Admin,
        status: UserStatus::Active,
        limits: UserLimits::default(),
        initials: "AR".into(),
        created_at: Utc::now(),
    }
}

fn project() -> Project {
    Project {
        id: ProjectId::new(),
        key: "FP".into(),
        name: "CLI/SDK".into(),
        repo_url: "git@github.com:acme/fp.git".into(),
        default_branch: "main".into(),
        qa_path: "qa.yaml".into(),
        qa_status: QaProfileStatus::Valid,
        image: None,
        resources: ResourceProfile::default(),
        loop_cap: 3,
        created_at: Utc::now(),
    }
}

fn task(owner: UserId, project: ProjectId) -> Task {
    let mut workers = BTreeMap::new();
    workers.insert(WorkerRole::Builder, WorkerChoice::new("claude_sdk", "claude-sonnet-4.5"));
    workers.insert(WorkerRole::Qa, WorkerChoice::new("codex", "gpt-5"));
    workers.insert(WorkerRole::Reviewer, WorkerChoice::new("claude_sdk", "claude-opus-4.1"));
    Task {
        id: TaskId::new(),
        plan_id: PlanId::new(),
        project_id: project,
        owner_id: owner,
        number: 142,
        key: "FP-142".into(),
        spec_id: "t1".into(),
        title: "Add retry policy to SDK client".into(),
        description: String::new(),
        acceptance: vec!["retries".into()],
        files_hint: vec![],
        depends_on: vec![],
        workers,
        state: TaskState::Testing,
        last_active: None,
        loop_count: 1,
        loop_cap: 3,
        branch: None,
        pr_url: None,
        pr_number: Some(418),
        progress: 48,
        meta: String::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn err(status: StatusCode, code: &str, msg: &str) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError::new(code, msg)))
}

async fn serve() -> String {
    let u = user();
    let p = project();
    let m = Mock { task: Arc::new(Mutex::new(task(u.id, p.id))), user: u, project: p };
    let app = Router::new()
        .route(
            "/api/v1/info",
            get(|| async {
                Json(ServerInfo {
                    product: "Kiln".into(),
                    version: VERSION.into(),
                    setup_required: false,
                    allowed_domain: Some("company.dev".into()),
                    hostname: "mac-mini.local".into(),
                    tailnet_host: Some("mac-mini.tailnet.ts.net".into()),
                })
            }),
        )
        .route(
            "/api/v1/auth/login",
            post(|State(m): State<Mock>, Json(r): Json<LoginRequest>| async move {
                if !r.email.ends_with("@company.dev") {
                    return Err(err(StatusCode::FORBIDDEN, "not_allowlisted", "not on the allowlist"));
                }
                if r.password != "pw" {
                    return Err(err(StatusCode::UNAUTHORIZED, "unauthorized", "bad credentials"));
                }
                Ok(Json(LoginResponse { token: "tok-123".into(), user: m.user.clone() }))
            }),
        )
        .route(
            "/api/v1/board",
            get(|State(m): State<Mock>| async move {
                Json(BoardResponse {
                    tasks: vec![m.task.lock().unwrap().clone()],
                    ideas: vec![],
                    projects: vec![m.project.clone()],
                    users: vec![],
                })
            }),
        )
        .route(
            "/api/v1/machine",
            get(|| async {
                Json(MachineStats {
                    hostname: "mac-mini".into(),
                    spec_line: "M4 Pro · 12 cores · 32 GB".into(),
                    cpu_percent: 40.0,
                    cpu_history: vec![10.0, 50.0, 90.0],
                    ram_used_gb: 12.0,
                    ram_total_gb: 32.0,
                    containers_running: 2,
                    container_slots: 8,
                    queue_depth: 1,
                    est_wait_secs: 240,
                    ..Default::default()
                })
            }),
        )
        .route("/api/v1/notifications", get(|| async { Json(Vec::<Notification>::new()) }))
        .route("/api/v1/threads", get(|| async { Json(Vec::<Thread>::new()) }))
        .route(
            "/api/v1/harnesses",
            get(|| async {
                Json(HarnessCatalog {
                    harnesses: vec![HarnessInfo {
                        id: "codex".into(),
                        label: "Codex".into(),
                        models: vec!["gpt-5".into()],
                        experimental: false,
                    }],
                    defaults: BTreeMap::new(),
                })
            }),
        )
        .route(
            "/api/v1/credentials",
            get(|| async {
                Json(vec![CredentialSummary {
                    id: CredentialId::new(),
                    kind: CredentialKind::Litellm,
                    name: "LiteLLM proxy".into(),
                    masked: "sk-llm-…c0de".into(),
                    base_url: Some("https://litellm.tailnet:4000".into()),
                    env_var: None,
                    status: CredentialHealth::Reachable { latency_ms: 34 },
                    created_at: Utc::now(),
                    expires_at: None,
                }])
            }),
        )
        .route(
            "/api/v1/tasks/{id}/action",
            post(|State(m): State<Mock>, Path(_id): Path<String>, Json(r): Json<TaskActionRequest>| async move {
                let mut t = m.task.lock().unwrap();
                if r.action == TaskAction::Retry {
                    t.loop_count += 1;
                    t.state = TaskState::Building;
                }
                Json(t.clone())
            }),
        )
        .with_state(m);
    let _ = Visibility::Private;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn wait_for(rx: &mut mpsc::UnboundedReceiver<DataEvent>, f: impl Fn(&DataEvent) -> bool) -> DataEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(e)) if f(&e) => return e,
            Ok(Some(_)) => continue,
            other => panic!("timed out waiting for event: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_board_actions_and_credentials() {
    let base = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kiln").join("session.toml");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut src = LiveSource::start(Client::new(base.clone()), tx, path.clone(), false);

    let DataEvent::Server(s) = wait_for(&mut rx, |e| matches!(e, DataEvent::Server(_))).await else { unreachable!() };
    assert_eq!(s.host, "mac-mini");
    assert_eq!(s.tailnet, "mac-mini.tailnet.ts.net");
    assert!(s.latency_ms.is_some());
    assert_eq!(s.allowed_domain.as_deref(), Some("company.dev"));

    src.handle(Cmd::Login { email: "alex@company.dev".into(), password: "".into() });
    let e = wait_for(&mut rx, |e| matches!(e, DataEvent::LoginError { .. })).await;
    assert!(matches!(e, DataEvent::LoginError { password_field: true, .. }));

    src.handle(Cmd::Login { email: "x@evil.com".into(), password: "pw".into() });
    let DataEvent::LoginError { message, .. } = wait_for(&mut rx, |e| matches!(e, DataEvent::LoginError { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(message, "✕ not on the allowlist · ask an admin to create your account");

    src.handle(Cmd::Login { email: "alex@company.dev".into(), password: "nope".into() });
    let DataEvent::LoginError { message, .. } = wait_for(&mut rx, |e| matches!(e, DataEvent::LoginError { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(message, "✕ wrong email or password");

    src.handle(Cmd::Login { email: "alex@company.dev".into(), password: "pw".into() });
    let DataEvent::SignedIn { email, flash } = wait_for(&mut rx, |e| matches!(e, DataEvent::SignedIn { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(email, "alex@company.dev");
    assert_eq!(flash.unwrap().text, "✓ signed in as alex@company.dev · session saved");
    let saved = Session::load(&path);
    assert_eq!(saved.token.as_deref(), Some("tok-123"));
    assert_eq!(saved.server.as_deref(), Some(base.as_str()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    let DataEvent::Catalog(cat) = wait_for(&mut rx, |e| matches!(e, DataEvent::Catalog(_))).await else {
        unreachable!()
    };
    assert_eq!(cat[0].label, "Codex");
    let DataEvent::Tasks(tasks) = wait_for(&mut rx, |e| matches!(e, DataEvent::Tasks(_))).await else { unreachable!() };
    let t = &tasks[0];
    assert_eq!(t.key, "FP-142");
    assert_eq!(t.project, "CLI/SDK");
    assert_eq!(t.column, BoardColumn::Qa);
    assert_eq!(t.status, Status::Running);
    assert_eq!(t.attempt, 2);
    assert_eq!(t.progress, 48.0);
    assert_eq!(t.workers[1].harness, "codex");
    assert_eq!(t.pr, Some(418));
    let DataEvent::Machine(m) = wait_for(&mut rx, |e| matches!(e, DataEvent::Machine(_))).await else { unreachable!() };
    assert_eq!((m.ctr_used, m.ctr_slots, m.queue), (2, 8, 1));
    assert_eq!(m.est_wait, "~4m");
    assert_eq!(m.cpu, vec![10.0, 50.0, 90.0]);
    let DataEvent::Credentials(creds) = wait_for(&mut rx, |e| matches!(e, DataEvent::Credentials(_))).await else {
        unreachable!()
    };
    let c = &creds[0];
    assert_eq!(c.kind_label, "URL + key");
    assert_eq!(c.fields[0].value, "https://litellm.tailnet:4000");
    assert_eq!(c.fields[1].display(false), "sk-llm-…c0de");
    assert_eq!(c.status_text, "reachable · 34ms");

    src.handle(Cmd::Retry("FP-142".into()));
    let DataEvent::TaskUpsert(t) = wait_for(&mut rx, |e| matches!(e, DataEvent::TaskUpsert(_))).await else {
        unreachable!()
    };
    assert_eq!(t.column, BoardColumn::Building);
    let DataEvent::Flash(f) =
        wait_for(&mut rx, |e| matches!(e, DataEvent::Flash(f) if f.text.contains("restarted"))).await
    else {
        unreachable!()
    };
    assert_eq!(f.text, "◐ FP-142 restarted · attempt 3/3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_server_reports_error() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let dir = tempfile::tempdir().unwrap();
    let _src = LiveSource::start(Client::new("http://127.0.0.1:9"), tx, dir.path().join("s.toml"), false);
    let DataEvent::Server(s) = wait_for(&mut rx, |e| matches!(e, DataEvent::Server(_))).await else { unreachable!() };
    assert!(s.error.is_some());
    assert!(s.latency_ms.is_none());
}
