//! `kiln-scheduler`: the heart of Kiln (docs/SCHEDULER.md).
//!
//! Turns approved plans into Builder → QA → Reviewer pipelines, enforces
//! capacity and fairness, survives restarts and feeds memory.
//!
//! # Moving parts
//!
//! | piece | module | what it does |
//! |---|---|---|
//! | dispatcher | `dispatch` | admission control + fair-share + dependency gating; spawns one [`TaskDriver`](driver) per admitted task; watchdog for orphaned in-flight tasks |
//! | task driver | `driver` | restartable state machine; every transition is persisted (CAS) before its side effect |
//! | run lifecycle | `runs` | run row → capacity → socket → container → outcome / exit / timeout → `finish_run` |
//! | run socket | `socket` | per-run Unix socket speaking the runner protocol (events, artifacts, memory MCP, completion) |
//! | planner | `planning` | threads, planner runs, plan versions, approval |
//! | human actions | `actions` | stop / retry / feedback / escalate |
//! | ingest | `ingest` | spool reconcile, transcripts, redaction, L0, container cleanup, distill enqueue |
//! | jobs | `jobs` | distill/extract/packs, nightly maintenance, PR merge polling |
//! | machine | `machine` | host sampler, `MachineStats`, health alerts |
//! | recovery | `recovery` | crash recovery on [`Scheduler::start`] |
//!
//! All loops are tokio tasks that stop on [`Scheduler::shutdown`]; no state
//! lives only in memory — everything needed to resume is in SQLite or the
//! run folder, so a daemon restart mid-run loses no task state, events or
//! transcripts.

mod actions;
mod config;
mod dispatch;
mod driver;
mod error;
mod ingest;
mod instance;
mod jobs;
mod machine;
mod planning;
mod qa_profile;
mod recovery;
mod runs;
mod socket;
mod sql;
mod util;
mod workspace;

pub use config::{Capacity, DEFAULT_WORKER_IMAGE, RoleTimeouts, SchedulerConfig};
pub use error::{Result, SchedulerError};
pub use qa_profile::parse_qa_yaml;

use kiln_events::{Bus, Event};
use kiln_gitops::{GitOps, PrProvider};
use kiln_memory::Memory;
use kiln_notifier::Notifier;
use kiln_sandbox::Runtime;
use kiln_store::Store;
use kiln_types::api::{
    ApprovePlanRequest, ApprovePlanResponse, CreateThreadRequest, SendMessageRequest, TaskAction,
    ThreadDetail, UpdatePlanRequest,
};
use kiln_types::{
    MachineStats, Message, Plan, PlanId, Project, ProjectId, QueueEntry, RunId, RunningContainer,
    Task, TaskId, ThreadId, User,
};
use kiln_vault::Vault;
use std::path::PathBuf;
use std::sync::Arc;

/// Everything the scheduler is wired to. Built once by the daemon.
pub struct SchedulerDeps {
    pub store: Store,
    pub bus: Bus,
    /// `DockerRuntime` in production, `ProcessRuntime` in dev/tests.
    pub runtime: Arc<dyn Runtime>,
    pub gitops: Arc<GitOps>,
    pub prs: Arc<dyn PrProvider>,
    pub vault: Arc<Vault>,
    pub memory: Arc<Memory>,
    pub notifier: Arc<Notifier>,
    pub harnesses: Arc<kiln_harness::Registry>,
    pub config: SchedulerConfig,
}

/// The scheduler. Create with [`Scheduler::new`], then call
/// [`Scheduler::start`] once (crash recovery, then the loops).
pub struct Scheduler {
    pub(crate) store: Store,
    pub(crate) bus: Bus,
    pub(crate) runtime: Arc<dyn Runtime>,
    pub(crate) gitops: Arc<GitOps>,
    pub(crate) prs: Arc<dyn PrProvider>,
    pub(crate) vault: Arc<Vault>,
    pub(crate) memory: Arc<Memory>,
    pub(crate) notifier: Arc<Notifier>,
    pub(crate) harnesses: Arc<kiln_harness::Registry>,
    pub(crate) cfg: SchedulerConfig,
    /// `cfg.capacity` with the host RAM resolved.
    pub(crate) capacity: Capacity,
    pub(crate) st: state::State,
}

pub mod pricing;
pub(crate) mod state;

impl Scheduler {
    /// Wire the scheduler. Does not touch containers or start any loop.
    pub async fn new(deps: SchedulerDeps) -> Result<Arc<Scheduler>> {
        let SchedulerDeps {
            store,
            bus,
            runtime,
            gitops,
            prs,
            vault,
            memory,
            notifier,
            harnesses,
            config,
        } = deps;
        let data_dir = std::path::absolute(&config.data_dir).unwrap_or(config.data_dir.clone());
        std::fs::create_dir_all(data_dir.join("runs"))?;
        std::fs::create_dir_all(data_dir.join("worktrees"))?;
        let cfg = SchedulerConfig { data_dir, ..config };
        let capacity = cfg.capacity.resolved();
        let s = Arc::new(Scheduler {
            store,
            bus,
            runtime,
            gitops,
            prs,
            vault,
            memory,
            notifier,
            harnesses,
            capacity,
            st: state::State::new(&cfg),
            cfg,
        });
        *state::lock(&s.st.self_ref) = Arc::downgrade(&s);
        Ok(s)
    }

    /// Crash recovery (docs/SCHEDULER.md "Crash recovery"), then spawn the
    /// dispatcher, ingest, jobs and machine-sampler loops. Call once.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        if self
            .st
            .started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(SchedulerError::Conflict("scheduler already started".into()));
        }
        self.recover().await?;
        self.spawn_loops();
        self.wake();
        Ok(())
    }

    /// Stop accepting work and stop every loop, driver and run socket.
    /// Containers keep running and runners keep spooling; a new scheduler
    /// on the same database re-attaches to them.
    pub async fn shutdown(&self) {
        self.st.shutdown.send_replace(true);
        self.bus.publish(Event::Shutdown);
        self.st.abort_all().await;
        tracing::info!("scheduler stopped; containers left running for re-attach");
    }

    // ------------------------------------------------------------ commands

    /// Start a planner thread with its first message and enqueue a planner run.
    pub async fn create_thread(
        &self,
        user: &User,
        req: CreateThreadRequest,
    ) -> Result<ThreadDetail> {
        self.do_create_thread(user, req).await
    }

    /// Add a user message to a thread and enqueue a planner run.
    pub async fn planner_message(
        &self,
        user: &User,
        thread: ThreadId,
        req: SendMessageRequest,
    ) -> Result<Message> {
        self.do_planner_message(user, thread, req).await
    }

    /// Edit a draft plan (optimistic concurrency on `req.version`).
    pub async fn update_plan(
        &self,
        user: &User,
        plan: PlanId,
        req: UpdatePlanRequest,
    ) -> Result<Plan> {
        self.do_update_plan(user, plan, req).await
    }

    /// Approve a draft plan: create its tasks (Queued) and dispatch.
    pub async fn approve_plan(
        &self,
        user: &User,
        plan: PlanId,
        req: ApprovePlanRequest,
    ) -> Result<ApprovePlanResponse> {
        self.do_approve_plan(user, plan, req).await
    }

    /// A human action on a task (owner or admin only).
    pub async fn task_action(&self, user: &User, task: TaskId, action: TaskAction) -> Result<Task> {
        self.do_task_action(user, task, action).await
    }

    /// Fetch the project's mirror and parse `qa.yaml` on the default branch
    /// into the project's `qa_status` (and last good profile).
    pub async fn refresh_project(&self, project: ProjectId) -> Result<Project> {
        self.do_refresh_project(project).await
    }

    // ------------------------------------------------------------ reads

    /// Latest machine sample (updated every `sample_interval`).
    pub fn machine(&self) -> MachineStats {
        self.st
            .machine
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Containers of live runs with a fresh resource sample.
    pub async fn running_containers(&self) -> Result<Vec<RunningContainer>> {
        self.do_running_containers().await
    }

    /// Queued tasks in the order the dispatcher would admit them.
    pub async fn queue(&self) -> Result<Vec<QueueEntry>> {
        self.do_queue().await
    }

    // ------------------------------------------------------------ extras

    /// The effective configuration.
    pub fn config(&self) -> &SchedulerConfig {
        &self.cfg
    }

    /// Host folder of a run (`<data>/runs/<run_id>`), mounted at `/run/kiln`.
    pub fn run_dir(&self, run: RunId) -> PathBuf {
        self.cfg.data_dir.join("runs").join(run.to_string())
    }

    /// Ask the dispatcher for an immediate pass.
    pub fn wake(&self) {
        self.st.wake.notify_one();
        self.bus.publish(Event::SchedulerWake);
    }

    /// Poll every Ready task's PR now (the jobs loop does this every
    /// `pr_poll_interval`). Returns how many tasks moved to Merged.
    pub async fn poll_prs(&self) -> Result<u32> {
        self.do_poll_prs().await
    }

    /// Run the nightly maintenance job now (normally 03:00 local).
    pub async fn run_nightly(&self) -> Result<()> {
        self.do_nightly().await
    }
}
