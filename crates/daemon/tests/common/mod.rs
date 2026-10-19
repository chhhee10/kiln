//! Shared harness for the daemon integration tests.
//!
//! Each test runs the full daemon in-process on an ephemeral 127.0.0.1 port
//! with a temp data dir, the process runtime, the mock harness, local PRs
//! and local git repos — no network, no Docker — and talks to it only
//! through `kiln_api_client::Client` (HTTP + native WebSocket).

#![allow(dead_code)]

use kiln_api_client::Client;
use kiln_api_client::kiln_types::api::*;
use kiln_api_client::kiln_types::event::ServerEvent;
use kiln_api_client::kiln_types::*;
use kiln_daemon::config::{Config, GithubMode, HarnessConfig, RuntimeKind, WorkerDefaults};
use kiln_daemon::{Daemon, ServeOptions};
use kiln_sandbox::{ProcessRuntime, Runtime};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub const ADMIN_EMAIL: &str = "admin@acme.dev";
pub const PASSWORD: &str = "correct horse battery";

/// `target/<profile>/examples/kiln-runner-dev`, built by `cargo test`.
pub fn runner_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    // target/<profile>/deps/<test-bin>
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target profile dir");
    let p = profile_dir.join("examples").join("kiln-runner-dev");
    static BUILD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    if !p.is_file() {
        // `cargo test --test api` doesn't build examples; build it once.
        let target = profile_dir.parent().expect("target dir");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = std::process::Command::new(cargo);
        cmd.args([
            "build",
            "-p",
            "kiln-daemon",
            "--example",
            "kiln-runner-dev",
            "--target-dir",
        ])
        .arg(target);
        if profile_dir.file_name().is_some_and(|n| n == "release") {
            cmd.arg("--release");
        }
        let st = cmd.status().expect("cargo build kiln-runner-dev");
        assert!(
            st.success() && p.is_file(),
            "could not build {}",
            p.display()
        );
    }
    p
}

/// Test environment: temp dir + shared process runtime (survives restarts).
pub struct Env {
    pub dir: tempfile::TempDir,
    pub runtime: Arc<dyn Runtime>,
    pub mock: BTreeMap<String, String>,
}

impl Env {
    pub fn new() -> Self {
        let _ = tracing_subscriber_init();
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            runtime: Arc::new(ProcessRuntime::new()),
            mock: BTreeMap::new(),
        }
    }

    pub fn with_mock(mut self, k: &str, v: &str) -> Self {
        self.mock.insert(k.to_string(), v.to_string());
        self
    }

    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    pub fn config(&self) -> Config {
        let mut c = Config {
            data_dir: self.data_dir(),
            ..Default::default()
        };
        c.server.bind = "127.0.0.1".into();
        c.server.auth_rate_per_minute = 10_000;
        c.runtime.kind = RuntimeKind::Process;
        c.runtime.runner_bin = Some(runner_bin());
        c.github.mode = GithubMode::Local;
        c.vault.keychain = false;
        c.defaults = WorkerDefaults::all_mock();
        c.capacity.ram_mb = 64 * 1024;
        c.capacity.reserve_mb = 1024;
        let mut opts = BTreeMap::from([("mock.sleep_ms".to_string(), "20".to_string())]);
        opts.extend(self.mock.clone());
        c.harnesses.insert(
            "mock".into(),
            HarnessConfig {
                options: opts,
                ..Default::default()
            },
        );
        c
    }

    /// Start a daemon on this env's data dir.
    pub async fn start(&self) -> Daemon {
        init_tracing();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let mut o = ServeOptions::new(self.config());
        o.dev = true;
        o.runtime = Some(self.runtime.clone());
        o.listener = Some(listener);
        o.password_cost = Some(kiln_auth::PasswordCost::INSECURE_FAST);
        o.print_banner = false;
        o.tweak_scheduler = Some(Box::new(|s| {
            s.tick = Duration::from_millis(200);
            s.sample_interval = Duration::from_millis(500);
            s.timeout_override = Some(Duration::from_secs(120));
            s.stop_grace = Duration::from_secs(2);
        }));
        Daemon::start(o).await.expect("daemon start")
    }
}

fn tracing_subscriber_init() -> Option<()> {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os("KILN_TEST_LOG").is_some() {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new(
                    std::env::var("KILN_TEST_LOG").unwrap_or_default(),
                ))
                .with_test_writer()
                .try_init();
        }
    });
    Some(())
}

pub fn client(d: &Daemon) -> Client {
    Client::new(d.url())
}

/// First-run setup: returns an admin client.
pub async fn setup_admin(d: &Daemon) -> (Client, User) {
    let c = client(d);
    let r = c
        .setup(&SetupRequest {
            admin_email: ADMIN_EMAIL.into(),
            admin_name: "Alex Rivera".into(),
            password: PASSWORD.into(),
            allowed_domain: "acme.dev".into(),
        })
        .await
        .expect("setup");
    (c, r.user)
}

/// Self-register a member.
pub async fn member(d: &Daemon, name: &str, email: &str) -> (Client, User) {
    let c = client(d);
    let r = c
        .register(&RegisterRequest {
            email: email.into(),
            name: name.into(),
            password: PASSWORD.into(),
        })
        .await
        .expect("register");
    (c, r.user)
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@acme.dev",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A local bare repo with one commit on main and a valid qa.yaml.
pub fn make_repo(env: &Env, name: &str) -> PathBuf {
    let root = env.dir.path().join("remotes");
    std::fs::create_dir_all(&root).unwrap();
    let work = root.join(format!("{name}-src"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("README.md"), format!("# {name}\n")).unwrap();
    std::fs::write(
        work.join("qa.yaml"),
        "version: 1\nresources: { cpus: 1, memory: 512m }\nchecks:\n  unit: \"true\"\n",
    )
    .unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let bare = root.join(format!("{name}.git"));
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    bare
}

/// Register a project as admin.
pub async fn project(admin: &Client, env: &Env, key: &str) -> Project {
    let repo = make_repo(env, &key.to_lowercase());
    admin
        .create_project(&CreateProjectRequest {
            key: key.into(),
            name: format!("Project {key}"),
            repo_url: repo.to_string_lossy().into_owned(),
            default_branch: "main".into(),
            qa_path: "qa.yaml".into(),
        })
        .await
        .expect("create project")
}

/// Poll `f` until it returns `Some`, or panic after `timeout`.
pub async fn eventually<T, F, Fut>(what: &str, timeout: Duration, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f().await {
            return v;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Collect WS events until `pred` matches one (returns all seen).
pub async fn ws_until(
    rx: &mut mpsc::Receiver<ServerEvent>,
    timeout: Duration,
    mut pred: impl FnMut(&ServerEvent) -> bool,
) -> Vec<ServerEvent> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(ev)) => {
                let hit = pred(&ev);
                seen.push(ev);
                if hit {
                    return seen;
                }
            }
            Ok(None) => panic!("websocket closed; saw {} events", seen.len()),
            Err(_) => panic!(
                "timed out on websocket; saw {:?}",
                seen.iter().map(kind).collect::<Vec<_>>()
            ),
        }
    }
}

/// Drain whatever arrives within `d`.
pub async fn ws_drain(rx: &mut mpsc::Receiver<ServerEvent>, d: Duration) -> Vec<ServerEvent> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + d;
    while let Ok(Some(ev)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        rx.recv(),
    )
    .await
    {
        seen.push(ev);
    }
    seen
}

pub fn kind(e: &ServerEvent) -> &'static str {
    match e {
        ServerEvent::Hello { .. } => "hello",
        ServerEvent::TaskUpserted { .. } => "task",
        ServerEvent::TaskRemoved { .. } => "task_removed",
        ServerEvent::RunUpserted { .. } => "run",
        ServerEvent::Log { .. } => "log",
        ServerEvent::PlanUpserted { .. } => "plan",
        ServerEvent::ThreadUpserted { .. } => "thread",
        ServerEvent::Message { .. } => "message",
        ServerEvent::PlannerStatus { .. } => "planner_status",
        ServerEvent::Notification { .. } => "notification",
        ServerEvent::MemoryUpserted { .. } => "memory",
        ServerEvent::Machine { .. } => "machine",
        ServerEvent::Toast { .. } => "toast",
    }
}

/// Start a thread and wait for the planner's first plan draft (via HTTP).
pub async fn draft_plan(c: &Client, project: ProjectId, msg: &str) -> (ThreadDetail, Plan) {
    let t = c
        .create_thread(&CreateThreadRequest {
            project_id: project,
            message: msg.into(),
            voice: false,
        })
        .await
        .expect("create thread");
    let tid = t.thread.id;
    let plan = eventually("planner plan", Duration::from_secs(60), || async {
        c.thread(tid).await.ok().and_then(|d| d.plan)
    })
    .await;
    (t, plan)
}

/// Approve and wait until every task of the plan is Ready.
pub async fn approve_and_finish(c: &Client, plan: &Plan) -> Vec<Task> {
    let r = c
        .approve_plan(
            plan.id,
            &ApprovePlanRequest {
                version: Some(plan.version),
            },
        )
        .await
        .expect("approve");
    assert!(!r.tasks.is_empty());
    let ids: Vec<TaskId> = r.tasks.iter().map(|t| t.id).collect();
    eventually("all tasks ready", Duration::from_secs(180), || async {
        let mut out = Vec::new();
        for id in &ids {
            let d = c.task(*id).await.ok()?;
            if d.task.state != TaskState::Ready {
                if matches!(
                    d.task.state,
                    TaskState::Failed | TaskState::Escalated | TaskState::Cancelled
                ) {
                    panic!(
                        "task {} ended {:?}: {}",
                        d.task.key, d.task.state, d.task.meta
                    );
                }
                return None;
            }
            out.push(d.task);
        }
        Some(out)
    })
    .await
}

/// Opt-in test logging: set `RUST_LOG` (e.g. `kiln_memory=debug`) to see
/// daemon traces in test output.
fn init_tracing() {
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    }
}
