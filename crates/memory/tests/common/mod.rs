//! Shared fixtures: a real migrated SQLite file + a `Memory` over it, and
//! raw-SQL inserts for prerequisite rows (users, projects, tasks, runs).

#![allow(dead_code)]

use chrono::{SecondsFormat, Utc};
use kiln_memory::{Memory, MemoryConfig};
use kiln_types::event::{LogLevel, RunEvent, RunEventKind};
use kiln_types::memory::{MemoryProposal, MemoryType, Visibility};
use kiln_types::{
    PlanId, ProjectId, Run, RunId, RunState, Task, TaskId, TaskState, ThreadId, Timestamp, UserId,
    WorkerRole,
};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

pub struct Env {
    pub mem: Memory,
    pub pool: SqlitePool,
    pub data: PathBuf,
    pub _dir: tempfile::TempDir,
}

pub fn ts(t: Timestamp) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

pub async fn pool_at(path: &std::path::Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("../store/migrations")
        .run(&pool)
        .await
        .unwrap();
    pool
}

pub async fn env() -> Env {
    env_with(MemoryConfig::default()).await
}

pub async fn env_with(cfg: MemoryConfig) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool_at(&dir.path().join("kiln.db")).await;
    let data = dir.path().join("data");
    let mem = Memory::open(pool.clone(), &data, cfg).await.unwrap();
    Env {
        mem,
        pool,
        data,
        _dir: dir,
    }
}

impl Env {
    /// Drop and re-open `Memory` over the same pool and data dir (a daemon restart).
    pub async fn reopen(self, cfg: MemoryConfig) -> Env {
        let Env {
            mem,
            pool,
            data,
            _dir,
        } = self;
        drop(mem); // releases the Tantivy writer lock
        let mem = Memory::open(pool.clone(), &data, cfg).await.unwrap();
        Env {
            mem,
            pool,
            data,
            _dir,
        }
    }
}

pub async fn add_user(pool: &SqlitePool, name: &str) -> UserId {
    add_user_with_settings(pool, name, None).await
}

pub async fn add_user_with_settings(
    pool: &SqlitePool,
    name: &str,
    settings: Option<serde_json::Value>,
) -> UserId {
    let id = UserId::new();
    let now = ts(Utc::now());
    sqlx::query(
        "INSERT INTO users (id, email, name, role, status, password_hash, limits_json, settings_json, created_at, updated_at) \
         VALUES (?, ?, ?, 'member', 'active', 'x', '{}', ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(format!("{}@example.com", name.to_lowercase().replace(' ', ".")))
    .bind(name)
    .bind(settings.map(|s| s.to_string()))
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    id
}

pub async fn add_project(pool: &SqlitePool, key: &str) -> ProjectId {
    let id = ProjectId::new();
    let now = ts(Utc::now());
    sqlx::query(
        "INSERT INTO projects (id, key, name, repo_url, default_branch, qa_path, qa_status_json, resources_json, created_at, updated_at) \
         VALUES (?, ?, ?, 'https://example.com/r.git', 'main', 'qa.yaml', '{\"state\":\"unknown\"}', '{}', ?, ?)",
    )
    .bind(id.to_string())
    .bind(key)
    .bind(format!("{key} project"))
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    id
}

pub async fn add_task(
    pool: &SqlitePool,
    owner: UserId,
    project: ProjectId,
    key: &str,
    title: &str,
) -> Task {
    let now = Utc::now();
    let (thread, plan, task) = (ThreadId::new(), PlanId::new(), TaskId::new());
    sqlx::query("INSERT INTO threads (id, project_id, owner_id, title, status, created_at, updated_at) VALUES (?, ?, ?, ?, 'dispatched', ?, ?)")
        .bind(thread.to_string())
        .bind(project.to_string())
        .bind(owner.to_string())
        .bind(title)
        .bind(ts(now))
        .bind(ts(now))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO plans (id, project_id, owner_id, thread_id, status, version, spec_json, created_at, updated_at) VALUES (?, ?, ?, ?, 'dispatched', 1, '{}', ?, ?)")
        .bind(plan.to_string())
        .bind(project.to_string())
        .bind(owner.to_string())
        .bind(thread.to_string())
        .bind(ts(now))
        .bind(ts(now))
        .execute(pool)
        .await
        .unwrap();
    let number: i64 = key
        .rsplit('-')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1);
    sqlx::query(
        "INSERT INTO tasks (id, plan_id, project_id, owner_id, number, key, spec_id, title, acceptance_json, workers_json, state, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 't1', ?, '[]', '{}', 'building', ?, ?)",
    )
    .bind(task.to_string())
    .bind(plan.to_string())
    .bind(project.to_string())
    .bind(owner.to_string())
    .bind(number)
    .bind(key)
    .bind(title)
    .bind(ts(now))
    .bind(ts(now))
    .execute(pool)
    .await
    .unwrap();
    Task {
        id: task,
        plan_id: plan,
        project_id: project,
        owner_id: owner,
        number: number as u64,
        key: key.into(),
        spec_id: "t1".into(),
        title: title.into(),
        description: String::new(),
        acceptance: vec![],
        files_hint: vec![],
        depends_on: vec![],
        workers: BTreeMap::new(),
        state: TaskState::Building,
        last_active: None,
        loop_count: 0,
        loop_cap: 3,
        branch: None,
        pr_url: None,
        pr_number: None,
        progress: 0,
        meta: String::new(),
        created_at: now,
        updated_at: now,
    }
}

pub async fn add_run(
    pool: &SqlitePool,
    owner: UserId,
    task: Option<TaskId>,
    role: WorkerRole,
) -> Run {
    let id = RunId::new();
    let now = Utc::now();
    sqlx::query("INSERT INTO runs (id, task_id, owner_id, role, harness, model, state, started_at, ended_at, cost_usd, created_at) VALUES (?, ?, ?, ?, 'mock', 'mock-1', 'succeeded', ?, ?, 0.25, ?)")
        .bind(id.to_string())
        .bind(task.map(|t| t.to_string()))
        .bind(owner.to_string())
        .bind(role.as_str())
        .bind(ts(now - chrono::Duration::seconds(90)))
        .bind(ts(now))
        .bind(ts(now))
        .execute(pool)
        .await
        .unwrap();
    Run {
        id,
        task_id: task,
        thread_id: None,
        owner_id: owner,
        role,
        harness: "mock".into(),
        model: "mock-1".into(),
        attempt: 1,
        container_id: None,
        state: RunState::Succeeded,
        progress: 100,
        started_at: Some(now - chrono::Duration::seconds(90)),
        ended_at: Some(now),
        ingested: false,
        cost_usd: 0.25,
        tokens_in: 0,
        tokens_out: 0,
        error: None,
        retries: 0,
    }
}

pub fn prop(
    kind: MemoryType,
    subject: &str,
    relation: &str,
    object: &str,
    vis: Visibility,
) -> MemoryProposal {
    MemoryProposal {
        kind,
        title: format!("{subject} {relation} {object}"),
        subject: subject.into(),
        relation: relation.into(),
        object: object.into(),
        body: String::new(),
        visibility: vis,
        shared_with: vec![],
        project_id: None,
    }
}

pub fn ev(run: RunId, seq: u64, at: Timestamp, kind: RunEventKind) -> RunEvent {
    RunEvent {
        run_id: run,
        seq,
        ts: at,
        kind,
    }
}

pub fn log(level: LogLevel, msg: &str) -> RunEventKind {
    RunEventKind::Log {
        level,
        message: msg.into(),
    }
}

pub fn cmd(command: &str, exit: i32, output: &str) -> RunEventKind {
    RunEventKind::Command {
        command: command.into(),
        exit_code: Some(exit),
        output: output.into(),
    }
}

pub fn no_redact(s: &str) -> String {
    s.to_string()
}
