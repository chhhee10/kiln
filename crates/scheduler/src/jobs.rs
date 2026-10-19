//! Jobs loop (docs/SCHEDULER.md "jobs"): `distill` (L0 → L1 → L2 → L3,
//! optionally via a Distiller container with the owner's credentials),
//! `nightly` maintenance at 03:00 local, and PR merge polling.

use crate::Scheduler;
use crate::error::{Result, SchedulerError};
use crate::runs::LaunchSpec;
use crate::sql::Ext;
use crate::util::meta;
use chrono::{Local, Timelike, Utc};
use kiln_events::Event;
use kiln_gitops::PrState;
use kiln_store::{Job, NewRun, PullRequestState, TaskFilter};
use kiln_types::event::ServerEvent;
use kiln_types::protocol::RunConfig;
use kiln_types::{PlanStatus, ProjectId, RunId, RunState, TaskState, WorkerRole};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;

const JOB_KINDS: [&str; 2] = ["distill", "nightly"];
const LAST_NIGHTLY_KEY: &str = "scheduler.last_nightly";
/// Concurrent distill jobs (each may run a Distiller container).
const DISTILL_CONCURRENCY: usize = 2;

impl Scheduler {
    pub(crate) async fn jobs_loop(self: Arc<Self>) {
        let mut shutdown = self.st.shutdown.subscribe();
        let sem = Arc::new(Semaphore::new(DISTILL_CONCURRENCY));
        let mut last_poll = tokio::time::Instant::now();
        loop {
            if let Err(e) = self.maybe_schedule_nightly().await {
                tracing::warn!(error = %e, "nightly scheduling failed");
            }
            while let Ok(permit) = sem.clone().try_acquire_owned() {
                match self.store.claim_job(&JOB_KINDS, None).await {
                    Ok(Some(job)) => {
                        let s = self.clone();
                        self.spawn_bg(async move {
                            s.run_job(job).await;
                            drop(permit);
                        });
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "claiming a job failed");
                        break;
                    }
                }
            }
            if last_poll.elapsed() >= self.cfg.pr_poll_interval {
                last_poll = tokio::time::Instant::now();
                if let Err(e) = self.do_poll_prs().await {
                    tracing::warn!(error = %e, "PR polling failed");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(self.cfg.tick.min(self.cfg.pr_poll_interval)) => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    async fn run_job(self: &Arc<Self>, job: Job) {
        let r = match job.kind.as_str() {
            "distill" => self.job_distill(&job).await,
            "nightly" => self.do_nightly().await,
            other => Err(SchedulerError::Internal(format!(
                "unknown job kind {other}"
            ))),
        };
        match r {
            Ok(()) => {
                let _ = self.store.complete_job(job.id).await;
            }
            Err(SchedulerError::ShuttingDown) => {} // re-queued as stale on the next start
            Err(e) => {
                tracing::warn!(job = %job.id, kind = %job.kind, error = %e, "job failed");
                let _ = self
                    .store
                    .fail_job(job.id, &e.to_string(), Some(chrono::Duration::minutes(1)))
                    .await;
            }
        }
    }

    // ------------------------------------------------------------ distill

    async fn job_distill(self: &Arc<Self>, job: &Job) -> Result<()> {
        let id: RunId = job
            .payload
            .get("run_id")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| SchedulerError::Invalid("distill job without run_id".into()))?;
        let run = self.store.get_run(id).await?;
        let task = match run.task_id {
            Some(t) => self.store.get_task(t).await.ok(),
            None => None,
        };
        let episode = self.memory.distill(&run, task.as_ref()).await?;
        // The heuristic pass above runs for every run. The LLM Distiller (a
        // model session on the owner's key) runs once per attempt, after the
        // Reviewer, when it can see the diff, QA report and verdict, plus
        // when a task ended Escalated/Failed so failures are learned from.
        let task_failed = task
            .as_ref()
            .is_some_and(|t| matches!(t.state, TaskState::Escalated | TaskState::Failed));
        let llm = if self.cfg.llm_distill
            && (run.role == WorkerRole::Reviewer
                || (task_failed && matches!(run.role, WorkerRole::Builder | WorkerRole::Qa)))
        {
            match self.run_distiller(&run, task.as_ref(), &episode).await {
                Ok(o) => o,
                Err(SchedulerError::ShuttingDown) => return Err(SchedulerError::ShuttingDown),
                Err(e) => {
                    tracing::info!(run = %id, error = %e, "distiller container unavailable; heuristic extraction only");
                    None
                }
            }
        } else {
            None
        };
        let items = self.memory.extract(&episode, llm.as_deref()).await?;
        for item in &items {
            self.bus
                .server(ServerEvent::MemoryUpserted { item: item.clone() });
        }
        if let Some(p) = episode.project_id
            && let Err(e) = self.memory.rebuild_packs(run.owner_id, p).await
        {
            tracing::debug!(error = %e, "rebuild_packs failed");
        }
        self.bus.publish(Event::MemoryChanged {
            owner_id: run.owner_id,
            project_id: episode.project_id,
        });
        Ok(())
    }

    /// Run the Distiller container with the owner's credentials; returns its
    /// `memory.json` (or reply) for `memory.extract`.
    async fn run_distiller(
        self: &Arc<Self>,
        source: &kiln_types::Run,
        task: Option<&kiln_types::Task>,
        episode: &kiln_types::memory::Episode,
    ) -> Result<Option<String>> {
        let Some(task) = task else { return Ok(None) };
        let choice = self
            .worker_choice(None, source.owner_id, WorkerRole::Distiller)
            .await?;
        if self.harnesses.get(&choice.harness).is_none() {
            return Ok(None);
        }
        let project = self.store.get_project(task.project_id).await?;
        let events = self.memory.read_l0(source.id).await.unwrap_or_default();
        let skip = events.len().saturating_sub(kiln_planner::MAX_EVENTS);
        let prompt = kiln_planner::distiller_prompt(episode, &events[skip..]);
        let run = self
            .store
            .create_run(NewRun {
                task_id: Some(task.id),
                thread_id: None,
                owner_id: source.owner_id,
                role: WorkerRole::Distiller,
                harness: choice.harness.clone(),
                model: choice.model.clone(),
                attempt: source.attempt,
            })
            .await?;
        self.publish_run(&run);
        let config = RunConfig {
            run_id: run.id,
            task_id: Some(task.id),
            task_key: Some(task.key.clone()),
            role: WorkerRole::Distiller,
            harness: choice.harness.clone(),
            model: choice.model.clone(),
            prompt,
            system: kiln_planner::role_system(WorkerRole::Distiller),
            workdir: kiln_types::protocol::paths::RUN_DIR.into(),
            branch: None,
            base_branch: project.default_branch.clone(),
            attempt: source.attempt,
            timeout_secs: self
                .cfg
                .timeout_for(WorkerRole::Distiller, None)
                .as_secs()
                .max(1),
            qa_profile: None,
            options: self.options_for(&choice.harness),
            read_only: true,
        };
        let spec = LaunchSpec {
            role: WorkerRole::Distiller,
            owner: source.owner_id,
            task: Some(task.clone()),
            image: project
                .image
                .clone()
                .unwrap_or_else(|| self.cfg.worker_image.clone()),
            resources: kiln_types::ResourceProfile {
                cpus: 1.0,
                memory_mb: 1024,
            },
            project,
            config,
            worktree: None,
            services: Default::default(),
            head_before: None,
        };
        self.launch(&run, spec).await?;
        let f = self.await_run(run.id).await?;
        if f.state != RunState::Succeeded {
            return Ok(None);
        }
        let mem = self.run_dir(run.id).join("out").join("memory.json");
        Ok(match tokio::fs::read_to_string(mem).await {
            Ok(s) if !s.trim().is_empty() => Some(s),
            _ => f.outcome.reply.filter(|r| !r.trim().is_empty()),
        })
    }

    // ------------------------------------------------------------ PRs

    pub(crate) async fn do_poll_prs(&self) -> Result<u32> {
        let ready = self
            .store
            .list_tasks(TaskFilter {
                states: vec![TaskState::Ready],
                ..Default::default()
            })
            .await?;
        let mut merged = 0;
        for task in ready {
            let Some(number) = task.pr_number else {
                continue;
            };
            let project = self.store.get_project(task.project_id).await?;
            let state = match self.prs.pr_state(&project.repo_url, number).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(task = %task.key, error = %e, "pr_state failed");
                    continue;
                }
            };
            let row = self.store.get_pr_for_task(task.id).await?;
            match state {
                PrState::Merged => {
                    match self
                        .transition(
                            &task,
                            TaskState::Merged,
                            "PR merged",
                            None,
                            Ext {
                                meta: Some(meta::MERGED.into()),
                                ..Default::default()
                            },
                        )
                        .await
                    {
                        Ok(_) => merged += 1,
                        Err(e) if e.is_conflict() => continue,
                        Err(e) => return Err(e),
                    }
                    if let Some(r) = row {
                        let _ = self
                            .store
                            .set_pr_state(r.id, PullRequestState::Merged)
                            .await;
                    }
                    let tasks = self
                        .store
                        .list_tasks(TaskFilter {
                            plan: Some(task.plan_id),
                            ..Default::default()
                        })
                        .await?;
                    if tasks.iter().all(|t| t.state == TaskState::Merged) {
                        let p = self
                            .store
                            .set_plan_status(task.plan_id, PlanStatus::Merged)
                            .await?;
                        self.publish_plan(&p);
                        if let Ok(t) = self.store.get_thread(p.thread_id).await {
                            self.publish_thread(&t);
                        }
                    }
                    // Dependents stacked on this branch now use the default branch.
                    self.wake();
                }
                PrState::Closed => {
                    if let Some(r) = row
                        && r.state == PullRequestState::Open
                    {
                        let _ = self
                            .store
                            .set_pr_state(r.id, PullRequestState::Closed)
                            .await;
                        if let Ok(t) = self
                            .store
                            .update_task_progress(task.id, task.progress, "PR closed")
                            .await
                        {
                            self.publish_task(&t);
                        }
                    }
                }
                PrState::Open => {}
            }
        }
        Ok(merged)
    }

    // ------------------------------------------------------------ nightly

    async fn maybe_schedule_nightly(&self) -> Result<()> {
        let now = Local::now();
        if now.hour() < self.cfg.nightly_hour.min(23) {
            return Ok(());
        }
        let today = now.format("%Y-%m-%d").to_string();
        let last: Option<String> = self.store.get_setting(LAST_NIGHTLY_KEY).await?;
        if last.as_deref() == Some(today.as_str()) {
            return Ok(());
        }
        self.store.set_setting(LAST_NIGHTLY_KEY, &today).await?;
        if self.store.queued_jobs(Some("nightly")).await?.is_empty() {
            self.store
                .enqueue_job(kiln_store::NewJob::new(
                    "nightly",
                    serde_json::json!({ "date": today }),
                ))
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn do_nightly(&self) -> Result<()> {
        tracing::info!("nightly maintenance starting");
        // Memory: L0 compaction/retention, dedupe, stale flags.
        let projects = self.store.list_projects().await?;
        let mirrors: HashMap<ProjectId, (PathBuf, String)> = projects
            .iter()
            .map(|p| (p.id, (self.gitops.mirror_path(p), p.default_branch.clone())))
            .collect();
        let file_exists = move |project: ProjectId, path: &str| -> bool {
            let Some((mirror, branch)) = mirrors.get(&project) else {
                return true;
            };
            if !mirror.join("HEAD").is_file() {
                return true;
            }
            let spec = format!(
                "refs/remotes/origin/{branch}:{}",
                path.trim_start_matches("./")
            );
            std::process::Command::new("git")
                .arg("-C")
                .arg(mirror)
                .args(["cat-file", "-e", &spec])
                .status()
                .map(|s| s.success())
                .unwrap_or(true)
        };
        let opts = kiln_memory::NightlyOptions {
            retention_days: self.cfg.l0_retention_days,
            ..Default::default()
        };
        match self.memory.nightly(&opts, &file_exists).await {
            Ok(r) => tracing::info!(?r, "memory nightly done"),
            Err(e) => tracing::warn!(error = %e, "memory nightly failed"),
        }
        let now = Utc::now();
        let pruned = self
            .store
            .prune_run_events(
                now - chrono::Duration::days(i64::from(self.cfg.run_events_retention_days)),
            )
            .await?;
        let _ = self
            .store
            .prune_finished_jobs(now - chrono::Duration::days(30))
            .await;

        // Dead containers whose run is ingested (or unknown).
        if let Ok(list) = self.runtime.list_managed().await {
            for c in list.into_iter().filter(|c| !c.running && self.owns(c)) {
                let remove = match c.run_id() {
                    Some(r) => match self.store.get_run(r).await {
                        Ok(run) => run.state.is_finished() && run.ingested,
                        Err(e) => e.is_not_found(),
                    },
                    None => true,
                };
                if remove {
                    let _ = self.runtime.remove(&c.id).await;
                }
            }
        }
        // Worktrees of finished tasks.
        let done = self
            .store
            .list_tasks(TaskFilter {
                states: vec![TaskState::Merged, TaskState::Cancelled, TaskState::Failed],
                ..Default::default()
            })
            .await?;
        for t in done {
            if self.gitops.worktree_path(&t.key).exists()
                && let Some(p) = projects.iter().find(|p| p.id == t.project_id)
            {
                let _ = self.gitops.remove_worktree(p, &t.key).await;
            }
        }
        // Run folders past retention — ingested runs only (PRD §7.9).
        let cutoff = now - chrono::Duration::days(i64::from(self.cfg.run_retention_days));
        let mut removed = 0;
        if let Ok(rd) = std::fs::read_dir(self.cfg.data_dir.join("runs")) {
            for e in rd.flatten() {
                let Some(id) = e.file_name().to_str().and_then(|s| s.parse::<RunId>().ok()) else {
                    continue;
                };
                let Ok(run) = self.store.get_run(id).await else {
                    continue;
                };
                if run.ingested
                    && run.ended_at.is_some_and(|t| t < cutoff)
                    && std::fs::remove_dir_all(e.path()).is_ok()
                {
                    removed += 1;
                }
            }
        }
        tracing::info!(
            pruned_events = pruned,
            removed_run_dirs = removed,
            "nightly maintenance done"
        );
        Ok(())
    }
}
