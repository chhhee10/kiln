//! Test harness: a real scheduler over ProcessRuntime + the mock harness +
//! LocalPrs + a temp local git remote. No network, no Docker.

#![allow(dead_code)]

use kiln_events::{Bus, Event};
use kiln_gitops::{GitOps, LocalPrs};
use kiln_memory::{Memory, MemoryConfig};
use kiln_notifier::Notifier;
use kiln_sandbox::{ProcessRuntime, Runtime};
use kiln_scheduler::*;
use kiln_store::{NewPlan, NewProject, NewThread, NewUser, Store, TaskFilter};
use kiln_types::api::ApprovePlanRequest;
use kiln_types::event::ServerEvent;
use kiln_types::plan::{PlanSpec, TaskSpec, WorkerChoice};
use kiln_types::*;
use kiln_vault::{MasterKey, Vault};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

static ENV: Once = Once::new();

/// Hermetic git for every child process (mock harness, runner, gitops).
pub fn hermetic_env() {
    ENV.call_once(|| {
        // SAFETY: done once, before any test spawns threads that read env.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            std::env::set_var("GIT_AUTHOR_NAME", "Test");
            std::env::set_var("GIT_AUTHOR_EMAIL", "test@example.com");
            std::env::set_var("GIT_COMMITTER_NAME", "Test");
            std::env::set_var("GIT_COMMITTER_EMAIL", "test@example.com");
            std::env::remove_var("KILN_RUNNER_BIN");
        }
    });
}

/// `target/<profile>/examples/kiln-runner-dev` (built by `cargo test`).
pub fn runner_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().and_then(|d| d.parent()).unwrap().to_path_buf();
    let p = profile_dir.join("examples").join("kiln-runner-dev");
    assert!(
        p.is_file(),
        "{} missing: run `cargo test -p kiln-scheduler` (it builds the example) or `cargo build -p kiln-scheduler --example kiln-runner-dev`",
        p.display()
    );
    p
}

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub struct Opts {
    pub mock: BTreeMap<String, String>,
    pub capacity: Capacity,
    pub llm_distill: bool,
    pub timeout: Option<Duration>,
    /// Track `AGENTS.md` in the repo.
    pub tracked_agents_md: bool,
    pub reconnect_budget: Duration,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            mock: BTreeMap::from([
                ("mock.sleep_ms".into(), "10".into()),
                ("mock.steps".into(), "2".into()),
            ]),
            capacity: Capacity {
                ram_mb: 64 * 1024,
                reserve_mb: 0,
                max_containers: 4,
                max_tasks: 4,
            },
            llm_distill: false,
            timeout: Some(Duration::from_secs(60)),
            tracked_agents_md: false,
            reconnect_budget: Duration::from_secs(60),
        }
    }
}

impl Opts {
    pub fn mock(mut self, k: &str, v: &str) -> Self {
        self.mock.insert(k.into(), v.into());
        self
    }
}

pub struct Base {
    pub tmp: tempfile::TempDir,
    pub data: PathBuf,
    pub remote: PathBuf,
    pub store: Store,
    pub bus: Bus,
    pub runtime: Arc<ProcessRuntime>,
    pub memory: Arc<Memory>,
    pub vault: Arc<Vault>,
    pub notifier: Arc<Notifier>,
    pub prs: Arc<LocalPrs>,
    pub project: Project,
    pub events: Arc<Mutex<Vec<ServerEvent>>>,
    pub internal: Arc<Mutex<Vec<String>>>,
    pub opts_mock: BTreeMap<String, String>,
    pub capacity: Capacity,
    pub llm_distill: bool,
    pub timeout: Option<Duration>,
    pub reconnect_budget: Duration,
}

pub fn workers() -> BTreeMap<WorkerRole, WorkerChoice> {
    [
        WorkerRole::Planner,
        WorkerRole::Builder,
        WorkerRole::Qa,
        WorkerRole::Reviewer,
        WorkerRole::Distiller,
    ]
    .into_iter()
    .map(|r| (r, WorkerChoice::new("mock", "mock-1")))
    .collect()
}

impl Env {
    pub async fn new(opts: Opts) -> Env {
        hermetic_env();
        // Short path: Unix socket paths are limited to ~104 bytes.
        let tmp = tempfile::Builder::new()
            .prefix("ks")
            .tempdir_in("/tmp")
            .unwrap();
        let root = tmp.path().to_path_buf();
        let data = root.join("d");
        std::fs::create_dir_all(&data).unwrap();
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        std::fs::write(seed.join("README.md"), "# demo\n").unwrap();
        std::fs::write(
            seed.join("qa.yaml"),
            "version: 1\nresources: { cpus: 1, memory: 1g }\nchecks:\n  unit: cargo test\n",
        )
        .unwrap();
        if opts.tracked_agents_md {
            std::fs::write(seed.join("AGENTS.md"), "# Repo's own agent guide\n").unwrap();
        }
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "initial"]);
        let remote = root.join("remote.git");
        git(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );

        let store = Store::open(&data.join("kiln.db")).await.unwrap();
        let bus = Bus::new(65536);
        let runtime = Arc::new(ProcessRuntime::new());
        let memory = Arc::new(
            Memory::open(store.pool().clone(), &data, MemoryConfig::default())
                .await
                .unwrap(),
        );
        let vault = Arc::new(Vault::new(MasterKey::from_bytes([7u8; 32]), store.clone()));
        let notifier = Arc::new(Notifier::new(store.clone(), bus.clone()));
        let prs = Arc::new(LocalPrs::new(&data));
        let mut p = NewProject::new("FP", "Acme", remote.to_string_lossy());
        p.resources = ResourceProfile {
            cpus: 1.0,
            memory_mb: 1024,
        };
        let project = store.create_project(p).await.unwrap();

        let events: Arc<Mutex<Vec<ServerEvent>>> = Default::default();
        let internal: Arc<Mutex<Vec<String>>> = Default::default();
        {
            let mut sub = bus.subscribe();
            let (ev, int) = (events.clone(), internal.clone());
            tokio::spawn(async move {
                while let Some(e) = sub.next().await {
                    match &*e {
                        Event::Server(s) => ev.lock().unwrap().push(s.clone()),
                        other => int.lock().unwrap().push(format!("{other:?}")),
                    }
                }
            });
        }
        let base = Base {
            tmp,
            data,
            remote,
            store,
            bus,
            runtime,
            memory,
            vault,
            notifier,
            prs,
            project,
            events,
            internal,
            opts_mock: opts.mock,
            capacity: opts.capacity,
            llm_distill: opts.llm_distill,
            timeout: opts.timeout,
            reconnect_budget: opts.reconnect_budget,
        };
        let sched = base.make_scheduler(base.runtime.clone()).await;
        let mut env = Env { base, sched };
        env.sched.start().await.unwrap();
        env.sched.refresh_project(env.project.id).await.unwrap();
        env.project = env.store.get_project(env.project.id).await.unwrap();
        env
    }
}

/// The test environment: shared parts + the current scheduler.
pub struct Env {
    pub base: Base,
    pub sched: Arc<Scheduler>,
}

impl std::ops::Deref for Env {
    type Target = Base;
    fn deref(&self) -> &Base {
        &self.base
    }
}

impl std::ops::DerefMut for Env {
    fn deref_mut(&mut self) -> &mut Base {
        &mut self.base
    }
}

impl Base {
    pub fn config(&self) -> SchedulerConfig {
        SchedulerConfig {
            worker_image: "kiln-worker:test".into(),
            runner_bin: Some(runner_bin()),
            capacity: self.capacity.clone(),
            tick: Duration::from_millis(200),
            llm_distill: self.llm_distill,
            harness_options: BTreeMap::from([("mock".to_string(), self.opts_mock.clone())]),
            default_workers: workers(),
            pr_poll_interval: Duration::from_secs(3600),
            stop_grace: Duration::from_secs(2),
            sample_interval: Duration::from_millis(500),
            runner_reconnect_budget: self.reconnect_budget,
            timeout_override: self.timeout,
            ..SchedulerConfig::new(&self.data)
        }
    }

    /// A scheduler over the same DB / data dir (crash-recovery tests pass a
    /// fresh or the same runtime).
    pub async fn make_scheduler(&self, runtime: Arc<dyn Runtime>) -> Arc<Scheduler> {
        Scheduler::new(SchedulerDeps {
            store: self.store.clone(),
            bus: self.bus.clone(),
            runtime,
            gitops: Arc::new(GitOps::new(&self.data)),
            prs: self.prs.clone(),
            vault: self.vault.clone(),
            memory: self.memory.clone(),
            notifier: self.notifier.clone(),
            harnesses: Arc::new(kiln_harness::registry()),
            config: self.config(),
        })
        .await
        .unwrap()
    }
}

impl Env {
    pub async fn user(&self, email: &str, role: UserRole, limits: UserLimits) -> User {
        let name = email.split('@').next().unwrap().to_string();
        self.store
            .create_user(NewUser {
                email: email.into(),
                name,
                role,
                password_hash: "$argon2id$fake".into(),
                limits,
            })
            .await
            .unwrap()
    }

    pub async fn member(&self, email: &str) -> User {
        self.user(
            email,
            UserRole::Member,
            UserLimits {
                concurrency: 4,
                ram_share: 1.0,
                monthly_budget_usd: 100.0,
            },
        )
        .await
    }

    /// Create a thread + draft plan directly (no planner run) and approve it.
    pub async fn dispatch(&self, user: &User, spec: PlanSpec) -> (Plan, Vec<Task>) {
        let thread = self
            .store
            .create_thread(NewThread {
                project_id: self.project.id,
                owner_id: user.id,
                title: spec.feature.clone(),
            })
            .await
            .unwrap();
        let plan = self
            .store
            .create_plan(NewPlan {
                project_id: self.project.id,
                owner_id: user.id,
                thread_id: thread.id,
                spec,
            })
            .await
            .unwrap();
        let r = self
            .sched
            .approve_plan(
                user,
                plan.id,
                ApprovePlanRequest {
                    version: Some(plan.version),
                },
            )
            .await
            .unwrap();
        (r.plan, r.tasks)
    }

    pub async fn task(&self, id: TaskId) -> Task {
        self.store.get_task(id).await.unwrap()
    }

    pub async fn wait_state(&self, id: TaskId, want: TaskState, timeout: Duration) -> Task {
        let start = Instant::now();
        loop {
            let t = self.task(id).await;
            if t.state == want {
                return t;
            }
            if start.elapsed() > timeout {
                let tr = self.store.list_transitions(id).await.unwrap();
                let runs = self.store.list_runs_for_task(id).await.unwrap();
                panic!(
                    "task {} is {:?} (meta {:?}), wanted {want:?}\ntransitions: {:#?}\nruns: {:#?}",
                    t.key,
                    t.state,
                    t.meta,
                    tr.iter()
                        .map(|t| format!("{:?}→{:?} {}", t.from, t.to, t.reason))
                        .collect::<Vec<_>>(),
                    runs.iter()
                        .map(|r| format!(
                            "{:?} a{} {:?} r{} {:?}",
                            r.role, r.attempt, r.state, r.retries, r.error
                        ))
                        .collect::<Vec<_>>()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn wait_until<F, Fut>(&self, what: &str, timeout: Duration, mut f: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let start = Instant::now();
        while !f().await {
            assert!(start.elapsed() < timeout, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn transitions(&self, id: TaskId) -> Vec<(TaskState, TaskState)> {
        self.store
            .list_transitions(id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.from, t.to))
            .collect()
    }

    pub fn run_config(&self, run: RunId) -> kiln_types::protocol::RunConfig {
        let p = self
            .data
            .join("runs")
            .join(run.to_string())
            .join("config.json");
        serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
    }

    pub async fn runs(&self, task: TaskId) -> Vec<Run> {
        self.store.list_runs_for_task(task).await.unwrap()
    }

    pub async fn notifications(&self, user: UserId) -> Vec<Notification> {
        self.store.list_notifications(user, 100).await.unwrap()
    }

    pub async fn wait_all_ingested(&self, timeout: Duration) {
        self.wait_until("all runs ingested", timeout, || async {
            self.store.list_uningested_runs().await.unwrap().is_empty()
                && self.store.count_live_runs(None).await.unwrap() == 0
        })
        .await;
    }

    pub async fn tasks_of(&self, plan: PlanId) -> Vec<Task> {
        self.store
            .list_tasks(TaskFilter {
                plan: Some(plan),
                ..Default::default()
            })
            .await
            .unwrap()
    }
}

pub fn spec(tasks: &[(&str, &[&str])]) -> PlanSpec {
    PlanSpec {
        feature: "Retry backoff".into(),
        description: "Retries with jittered backoff.".into(),
        tasks: tasks
            .iter()
            .map(|(id, deps)| TaskSpec {
                id: id.to_string(),
                title: format!("Task {id}"),
                description: format!("Do {id}."),
                acceptance_criteria: vec![format!("{id} works")],
                files_hint: vec![],
                depends_on: deps.iter().map(|d| d.to_string()).collect(),
                workers: Default::default(),
            })
            .collect(),
    }
}
