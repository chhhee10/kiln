//! TaskDriver: one per in-flight task (docs/SCHEDULER.md "TaskDriver").
//!
//! The driver is a loop over the task's *database* state. Each step looks at
//! the state and the latest run of the current role and attempt, does one
//! thing, persists it (CAS transition before the side effect it unlocks)
//! and loops. Because every decision is derived from the database, a driver
//! can be killed at any point and a new one (after a restart) continues
//! where it left off; CAS conflicts (a human acted meanwhile) just cause a
//! re-read.
//!
//! ```text
//! Preparing → Building → Testing → Reviewing → Ready
//!                ↑          │          │
//!                └─ Looping ◄──────────┘  (attempt cap → Escalated)
//! run failure: retry the role once, then Escalated
//! ```

use crate::Scheduler;
use crate::error::{Result, SchedulerError};
use crate::runs::LaunchSpec;
use crate::sql::{Ext, LoopChange, transition_ext};
use crate::state::Finished;
use crate::util::meta;
use crate::workspace::Prepared;
use kiln_gitops::{ArtifactLink, NewPr, render_pr_body};
use kiln_store::{NewNotification, NewRun, OptionalExt};
use kiln_types::plan::WorkerChoice;
use kiln_types::protocol::{RunConfig, RunOutcome};
use kiln_types::qa::{QaReport, ReviewVerdict};
use kiln_types::{
    NotificationAction, NotificationKind, Project, Run, RunState, Task, TaskId, TaskState, UserId,
    WorkerRole,
};
use std::sync::Arc;
use std::time::Duration;

/// Consecutive transient step errors before a task is marked Failed.
const MAX_STEP_ERRORS: u32 = 5;
/// Attempts at pushing the branch / opening the PR before escalating.
const PUBLISH_ATTEMPTS: u32 = 3;

/// How a finished role run is judged.
enum Verdict {
    /// Advance to the next stage.
    Pass,
    /// QA failed / changes requested: loop with this feedback.
    Loop(String),
    /// The run itself failed (crash, timeout, missing report, no commit).
    RunFailed(String),
}

impl Scheduler {
    /// Make sure exactly one driver runs for an active task.
    pub(crate) async fn ensure_driver(self: &Arc<Self>, task: TaskId) {
        if self.st.is_shutdown() {
            return;
        }
        let mut d = self.st.drivers.lock().await;
        if d.get(&task).is_some_and(|h| !h.is_finished()) {
            return;
        }
        let s = self.clone();
        d.insert(task, tokio::spawn(async move { s.drive(task).await }));
    }

    async fn drive(self: Arc<Self>, id: TaskId) {
        let mut errors = 0u32;
        loop {
            if self.st.is_shutdown() {
                return;
            }
            let task = match self.store.get_task(id).await {
                Ok(t) => t,
                Err(e) if e.is_not_found() => break,
                Err(e) => {
                    tracing::warn!(task = %id, error = %e, "driver: cannot read task");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            if !task.state.is_active() {
                // Exit protocol: re-check under the drivers lock so a task
                // re-activated concurrently is never left without a driver.
                let mut d = self.st.drivers.lock().await;
                match self.store.get_task(id).await {
                    Ok(t) if t.state.is_active() => continue,
                    _ => {}
                }
                d.remove(&id);
                drop(d);
                self.wake();
                return;
            }
            match self.step(&task).await {
                Ok(()) => errors = 0,
                Err(SchedulerError::ShuttingDown) => return,
                Err(SchedulerError::Conflict(m)) => {
                    tracing::debug!(task = %task.key, "driver: state moved ({m}); re-reading");
                }
                Err(e) => {
                    errors += 1;
                    let fatal =
                        matches!(e, SchedulerError::Invalid(_) | SchedulerError::Forbidden(_));
                    tracing::warn!(task = %task.key, state = task.state.as_str(), error = %e, errors, "driver step failed");
                    if fatal || errors >= MAX_STEP_ERRORS {
                        if let Err(e2) = self.fail_task(&task, &e.to_string()).await {
                            tracing::debug!(task = %task.key, error = %e2, "could not mark the task failed");
                        }
                        errors = 0;
                        continue;
                    }
                    tokio::time::sleep(Duration::from_secs(1 << errors.min(5))).await;
                }
            }
        }
        self.st.drivers.lock().await.remove(&id);
    }

    async fn step(self: &Arc<Self>, task: &Task) -> Result<()> {
        match task.state {
            TaskState::Preparing => self.step_prepare(task).await,
            TaskState::Building => self.step_role(task, WorkerRole::Builder).await,
            TaskState::Testing => self.step_role(task, WorkerRole::Qa).await,
            TaskState::Reviewing => self.step_role(task, WorkerRole::Reviewer).await,
            TaskState::Looping => self.step_loop(task).await,
            _ => Ok(()),
        }
    }

    // ------------------------------------------------------------ transitions

    /// CAS transition + publish. `Conflict` bubbles up (driver re-reads).
    pub(crate) async fn transition(
        &self,
        task: &Task,
        to: TaskState,
        reason: &str,
        actor: Option<UserId>,
        ext: Ext,
    ) -> Result<Task> {
        let t = transition_ext(&self.store, task.id, task.state, to, reason, actor, &ext).await?;
        self.published_transition(&t, task.state);
        Ok(t)
    }

    async fn set_meta(&self, task: &Task, meta: &str) -> Result<Task> {
        let t = self
            .store
            .update_task_progress(task.id, task.progress, meta)
            .await?;
        self.publish_task(&t);
        Ok(t)
    }

    pub(crate) async fn notify_task(
        &self,
        task: &Task,
        kind: NotificationKind,
        title: String,
        body: String,
        actions: &[(&str, &str)],
    ) {
        let mut n = NewNotification::new(task.owner_id, kind, title, body);
        n.deep_link = format!("task/{}", task.key);
        n.task_id = Some(task.id);
        n.plan_id = Some(task.plan_id);
        n.actions = actions
            .iter()
            .map(|(id, label)| NotificationAction {
                id: (*id).into(),
                label: (*label).into(),
            })
            .collect();
        if let Err(e) = self.notifier.notify(n).await {
            tracing::warn!(task = %task.key, error = %e, "notification failed");
        }
    }

    /// → Escalated ("Needs you") with a notification.
    pub(crate) async fn escalate(
        &self,
        task: &Task,
        kind: NotificationKind,
        reason: &str,
    ) -> Result<Task> {
        let t = self
            .transition(
                task,
                TaskState::Escalated,
                reason,
                None,
                Ext {
                    meta: Some(meta::NEEDS_YOU.into()),
                    ..Default::default()
                },
            )
            .await?;
        self.stop_task_runs(task.id, RunState::Cancelled, "task escalated");
        let title = match kind {
            NotificationKind::TaskFailed => format!("Failed: {}", task.title),
            _ => format!("Needs your call: {}", task.title),
        };
        self.notify_task(
            &t,
            kind,
            title,
            format!("{} · {reason}", t.key),
            &[("retry", "Retry"), ("feedback", "Send back to Builder")],
        )
        .await;
        Ok(t)
    }

    /// → Failed ("Failed · reason") with a notification.
    async fn fail_task(&self, task: &Task, reason: &str) -> Result<()> {
        let reason = crate::util::first_line(reason, 200);
        let t = self
            .transition(
                task,
                TaskState::Failed,
                &reason,
                None,
                Ext {
                    meta: Some(meta::failed(&reason)),
                    ..Default::default()
                },
            )
            .await?;
        self.stop_task_runs(task.id, RunState::Cancelled, "task failed");
        self.notify_task(
            &t,
            NotificationKind::TaskFailed,
            format!("Failed: {}", t.title),
            format!("{} · {reason}", t.key),
            &[("retry", "Retry")],
        )
        .await;
        Ok(())
    }

    // ------------------------------------------------------------ steps

    async fn step_prepare(self: &Arc<Self>, task: &Task) -> Result<()> {
        let task = if task.meta != meta::PREPARING {
            self.set_meta(task, meta::PREPARING).await?
        } else {
            task.clone()
        };
        let project = self.store.get_project(task.project_id).await?;
        self.prepare_workspace(&task, &project, true).await?;
        let task = self.store.get_task(task.id).await?;
        if task.state != TaskState::Preparing {
            return Ok(());
        }
        let m = meta::building(task.attempt(), task.loop_cap);
        self.transition(
            &task,
            TaskState::Building,
            "workspace ready",
            None,
            Ext {
                meta: Some(m),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    async fn step_loop(self: &Arc<Self>, task: &Task) -> Result<()> {
        let cap = task.loop_cap.max(1);
        if task.loop_count + 1 >= cap {
            self.escalate(
                task,
                NotificationKind::TaskEscalated,
                &format!("loop cap reached ({cap} attempts) — needs your call"),
            )
            .await?;
            return Ok(());
        }
        let m = meta::building(task.attempt() + 1, cap);
        self.transition(
            task,
            TaskState::Building,
            &format!("attempt {} of {cap}", task.attempt() + 1),
            None,
            Ext {
                loop_change: Some(LoopChange::Increment),
                meta: Some(m),
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    /// Latest run of `role` for the task's current attempt.
    async fn current_run(&self, task: &Task, role: WorkerRole) -> Result<Option<Run>> {
        Ok(self
            .store
            .latest_run_for_role(task.id, role)
            .await?
            .filter(|r| r.attempt == task.attempt()))
    }

    async fn step_role(self: &Arc<Self>, task: &Task, role: WorkerRole) -> Result<()> {
        let run = match self.current_run(task, role).await? {
            None => self.start_role_run(task, role, 0, None).await?,
            Some(r) if r.state == RunState::Pending => {
                // Created but never started (daemon died in between): retire
                // it without consuming the retry, and start a fresh one.
                let o = RunOutcome::failed("superseded: never started");
                let _ = self.store.finish_run(r.id, RunState::Cancelled, &o).await;
                self.start_role_run(task, role, r.retries, Some(&r)).await?
            }
            Some(r) => r,
        };
        let finished = self.await_run(run.id).await?;
        // A human may have acted while we waited.
        let task = self.store.get_task(task.id).await?;
        if task.state != role.task_state().unwrap_or(task.state)
            || self.current_run(&task, role).await?.map(|r| r.id) != Some(run.id)
        {
            return Ok(());
        }
        let run = self.store.get_run(run.id).await?;
        match self.judge(&task, role, &run, &finished).await {
            Verdict::Pass => self.advance(&task, role).await,
            Verdict::Loop(feedback) => {
                self.memory_feedback(&task, false).await;
                let m = meta::looping(task.attempt(), task.loop_cap);
                let why = if role == WorkerRole::Qa {
                    "QA failed"
                } else {
                    "changes requested"
                };
                self.transition(
                    &task,
                    TaskState::Looping,
                    why,
                    None,
                    Ext {
                        feedback: vec![feedback],
                        meta: Some(m),
                        ..Default::default()
                    },
                )
                .await?;
                Ok(())
            }
            Verdict::RunFailed(reason) => {
                if run.retries == 0 {
                    tracing::info!(task = %task.key, role = role.as_str(), %reason, "run failed; retrying once");
                    self.start_role_run(&task, role, 1, Some(&run)).await?;
                    Ok(())
                } else {
                    self.memory_feedback(&task, false).await;
                    self.escalate(
                        &task,
                        NotificationKind::TaskFailed,
                        &format!("{} failed twice: {reason}", role.label()),
                    )
                    .await?;
                    Ok(())
                }
            }
        }
    }

    async fn judge(&self, task: &Task, role: WorkerRole, run: &Run, f: &Finished) -> Verdict {
        let o = &f.outcome;
        if f.state != RunState::Succeeded || !o.success {
            let why = o.error.clone().unwrap_or_else(|| o.summary.clone());
            let why = if why.trim().is_empty() {
                f.state.as_str().to_string()
            } else {
                why
            };
            return Verdict::RunFailed(match f.state {
                RunState::TimedOut => format!("timed out ({})", crate::util::first_line(&why, 120)),
                RunState::Lost => "the run was lost".into(),
                _ => crate::util::first_line(&why, 160),
            });
        }
        match role {
            WorkerRole::Builder => {
                let before = self.read_run_meta(run.id).and_then(|m| m.head_before);
                match (&o.commit, before) {
                    (None, _) => Verdict::RunFailed("the Builder left no commit".into()),
                    (Some(c), Some(b)) if *c == b => {
                        Verdict::RunFailed("the Builder made no new commit".into())
                    }
                    _ => Verdict::Pass,
                }
            }
            WorkerRole::Qa => match &o.qa_report {
                None => Verdict::RunFailed("QA produced no report".into()),
                Some(r) if r.passed() => Verdict::Pass,
                Some(r) => {
                    let fb = kiln_planner::feedback_from(Some(r), None, None);
                    Verdict::Loop(if fb.is_empty() {
                        "QA failed without details.".into()
                    } else {
                        fb
                    })
                }
            },
            WorkerRole::Reviewer => match &o.verdict {
                None => Verdict::RunFailed("the Reviewer produced no verdict".into()),
                Some(v) if v.approved() => Verdict::Pass,
                Some(v) => {
                    let fb = kiln_planner::feedback_from(None, Some(v), None);
                    Verdict::Loop(if fb.is_empty() {
                        "The Reviewer requested changes.".into()
                    } else {
                        fb
                    })
                }
            },
            _ => {
                let _ = task;
                Verdict::Pass
            }
        }
    }

    async fn advance(self: &Arc<Self>, task: &Task, role: WorkerRole) -> Result<()> {
        match role {
            WorkerRole::Builder => {
                let m = meta::testing(task.attempt(), task.loop_cap);
                self.transition(
                    task,
                    TaskState::Testing,
                    "build committed",
                    None,
                    Ext {
                        meta: Some(m),
                        ..Default::default()
                    },
                )
                .await?;
            }
            WorkerRole::Qa => {
                self.transition(
                    task,
                    TaskState::Reviewing,
                    "QA passed",
                    None,
                    Ext {
                        meta: Some(meta::REVIEWING.into()),
                        ..Default::default()
                    },
                )
                .await?;
            }
            WorkerRole::Reviewer => self.publish_ready(task).await?,
            _ => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------ Ready

    /// Push the branch, open the PR (idempotent), → Ready, notify, clean up.
    async fn publish_ready(self: &Arc<Self>, task: &Task) -> Result<()> {
        let project = self.store.get_project(task.project_id).await?;
        let prepared = self.prepare_workspace(task, &project, false).await?;
        let qa = self
            .latest_outcome(task, WorkerRole::Qa)
            .await?
            .and_then(|o| o.qa_report);
        let verdict = self
            .latest_outcome(task, WorkerRole::Reviewer)
            .await?
            .and_then(|o| o.verdict);
        let mut last_err = None;
        let mut pr = None;
        for i in 0..PUBLISH_ATTEMPTS {
            match self
                .open_pr(task, &project, &prepared, qa.as_ref(), verdict.as_ref())
                .await
            {
                Ok(p) => {
                    pr = Some(p);
                    break;
                }
                Err(e) => {
                    tracing::warn!(task = %task.key, error = %e, attempt = i + 1, "publishing the branch/PR failed");
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(500 * (1 << i))).await;
                }
            }
        }
        let Some((number, url)) = pr else {
            let e = last_err.map(|e| e.to_string()).unwrap_or_default();
            self.escalate(
                task,
                NotificationKind::TaskFailed,
                &format!(
                    "could not open the PR: {}",
                    crate::util::first_line(&e, 160)
                ),
            )
            .await?;
            return Ok(());
        };
        let task = self.store.set_task_pr(task.id, &url, number).await?;
        let t = self
            .transition(
                &task,
                TaskState::Ready,
                "review approved; PR open",
                None,
                Ext {
                    meta: Some(meta::ready(Some(number))),
                    ..Default::default()
                },
            )
            .await?;
        self.memory_feedback(&t, true).await;
        self.notify_task(
            &t,
            NotificationKind::TaskReady,
            format!("Ready for review: {}", t.title),
            format!("{} · PR #{number} is open", t.key),
            &[("open_pr", "Open PR")],
        )
        .await;
        if let Err(e) = self.gitops.remove_worktree(&project, &t.key).await {
            tracing::warn!(task = %t.key, error = %e, "could not remove the worktree");
        }
        // Dependents may now start (stacked on this branch).
        self.wake();
        Ok(())
    }

    async fn open_pr(
        &self,
        task: &Task,
        project: &Project,
        prepared: &Prepared,
        qa: Option<&QaReport>,
        verdict: Option<&ReviewVerdict>,
    ) -> Result<(u64, String)> {
        let auth = self.prs.git_auth(&project.repo_url).await?;
        let branch = prepared.worktree.branch.clone();
        self.gitops
            .push_branch(project, &prepared.worktree.path, &branch, &auth)
            .await?;
        let plan = self.store.get_plan(task.plan_id).await?;
        let base = self
            .cfg
            .public_url
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        // Evidence from the attempt being shipped only: earlier attempts and
        // failed or retried runs would repeat or contradict it.
        let runs = self.store.list_runs_for_task(task.id).await?;
        let latest = runs.iter().map(|r| r.attempt).max().unwrap_or(0);
        let shipped: std::collections::HashSet<_> = runs
            .iter()
            .filter(|r| r.attempt == latest && r.state == RunState::Succeeded)
            .map(|r| r.id)
            .collect();
        let links: Vec<ArtifactLink> =
            self.store
                .list_artifacts_for_task(task.id)
                .await?
                .into_iter()
                .filter(|a| shipped.contains(&a.run_id))
                .map(|a| ArtifactLink {
                    key: a.path.strip_prefix("out/").unwrap_or(&a.path).to_string(),
                    label: a.caption.clone().unwrap_or_else(|| {
                        a.path.rsplit('/').next().unwrap_or(&a.path).to_string()
                    }),
                    url: format!("{base}/app/task/{}?artifact={}", task.key, a.id),
                })
                .collect();
        let body = render_pr_body(&plan.spec, task, qa, verdict, &links);
        let title = format!("{}: {}", task.key, task.title);
        let new = NewPr {
            repo_url: project.repo_url.clone(),
            base: prepared.base_branch.clone(),
            head: branch.clone(),
            title: title.clone(),
            body: body.clone(),
            draft: false,
        };
        let pr = self.prs.open_pr(&new).await?;
        let known = self.store.get_pr_for_task(task.id).await?;
        if known
            .as_ref()
            .is_none_or(|k| k.number != pr.number || k.provider != self.prs.id())
        {
            self.store
                .insert_pr(kiln_store::NewPr {
                    task_id: task.id,
                    provider: self.prs.id().into(),
                    number: pr.number,
                    url: pr.url.clone(),
                    branch,
                    title,
                    body,
                })
                .await?;
        }
        Ok((pr.number, pr.url))
    }

    async fn latest_outcome(&self, task: &Task, role: WorkerRole) -> Result<Option<RunOutcome>> {
        match self.current_run(task, role).await? {
            Some(r) => Ok(self.store.get_run_outcome(r.id).await.optional()?.flatten()),
            None => Ok(None),
        }
    }

    /// Credit/flag the memory items the current attempt's runs used.
    async fn memory_feedback(&self, task: &Task, passed: bool) {
        let Ok(runs) = self.store.list_runs_for_task(task.id).await else {
            return;
        };
        for r in runs
            .iter()
            .filter(|r| r.attempt == task.attempt() && r.role != WorkerRole::Distiller)
        {
            if let Err(e) = self.memory.feedback(r.id, passed).await {
                tracing::debug!(run = %r.id, error = %e, "memory feedback failed");
            }
        }
    }

    // ------------------------------------------------------------ starting runs

    /// Resolve the harness/model for a role: plan/task → user settings →
    /// daemon defaults.
    pub(crate) async fn worker_choice(
        &self,
        task: Option<&Task>,
        owner: UserId,
        role: WorkerRole,
    ) -> Result<WorkerChoice> {
        if let Some(c) = task.and_then(|t| t.workers.get(&role)) {
            return Ok(c.clone());
        }
        let settings = self.store.get_user_settings(owner).await?;
        let from_settings = settings.as_ref().and_then(|s| {
            s.worker_defaults.get(&role).or_else(|| match role {
                WorkerRole::Planner | WorkerRole::Distiller => {
                    s.worker_defaults.get(&WorkerRole::Builder)
                }
                _ => None,
            })
        });
        if let Some(c) = from_settings {
            return Ok(c.clone());
        }
        let d = &self.cfg.default_workers;
        d.get(&role)
            .or_else(|| match role {
                WorkerRole::Planner | WorkerRole::Distiller => d.get(&WorkerRole::Builder),
                _ => None,
            })
            .cloned()
            .ok_or_else(|| {
                SchedulerError::Invalid(format!(
                    "no harness configured for the {} role",
                    role.label()
                ))
            })
    }

    pub(crate) fn options_for(&self, harness: &str) -> std::collections::BTreeMap<String, String> {
        self.cfg
            .harness_options
            .get(harness)
            .cloned()
            .unwrap_or_default()
    }

    /// Create and launch a run of `role` for the task's current attempt.
    /// `prev` (a failed/superseded run of the same slot) lends its prompt to
    /// a retry so the retry sees exactly the same feedback.
    async fn start_role_run(
        self: &Arc<Self>,
        task: &Task,
        role: WorkerRole,
        retries: u32,
        prev: Option<&Run>,
    ) -> Result<Run> {
        let project = self.store.get_project(task.project_id).await?;
        let choice = self.worker_choice(Some(task), task.owner_id, role).await?;
        if self.harnesses.get(&choice.harness).is_none() {
            return Err(SchedulerError::Invalid(format!(
                "unknown harness {:?} for the {} role",
                choice.harness,
                role.label()
            )));
        }
        let prepared = self.prepare_workspace(task, &project, false).await?;
        let attempt = task.attempt();
        let cap = task.loop_cap.max(1);

        let run = self
            .store
            .create_run(NewRun {
                task_id: Some(task.id),
                thread_id: None,
                owner_id: task.owner_id,
                role,
                harness: choice.harness.clone(),
                model: choice.model.clone(),
                attempt,
            })
            .await?;
        if retries > 0 {
            crate::sql::set_run_retries(&self.store, run.id, retries).await?;
        }
        let run = self.store.get_run(run.id).await?;
        self.publish_run(&run);

        let in_context = self
            .write_memory_files(
                Some(task),
                task.owner_id,
                &project,
                role,
                &prepared.worktree.path,
                run.id,
            )
            .await
            .unwrap_or(0);
        let pack = self
            .memory
            .pack(task.owner_id, project.id, role)
            .await
            .unwrap_or_default();
        let _ = in_context;
        let prev_prompt = prev
            .and_then(|p| self.read_run_config(p.id))
            .map(|c| c.prompt);
        let mut take_feedback = false;
        let prompt = match (prev_prompt, role) {
            (Some(p), _) => p,
            (None, WorkerRole::Builder) => {
                let plan = self.store.get_plan(task.plan_id).await?;
                let feedback = self.store.peek_task_feedback(task.id).await?;
                take_feedback = true;
                kiln_planner::builder_prompt(
                    task,
                    &plan.spec.feature,
                    attempt,
                    cap,
                    &feedback,
                    &pack,
                )
            }
            (None, WorkerRole::Qa) => {
                let summary = self.builder_summary(task).await;
                kiln_planner::qa_prompt(task, prepared.profile.as_ref(), &summary)
            }
            (None, WorkerRole::Reviewer) => {
                let diff = self
                    .gitops
                    .diff_summary(&prepared.worktree.path, &prepared.base_branch)
                    .await
                    .unwrap_or_default();
                self.persist_diff(run.id, &diff).await;
                let qa = self
                    .latest_outcome(task, WorkerRole::Qa)
                    .await?
                    .and_then(|o| o.qa_report);
                kiln_planner::reviewer_prompt(
                    task,
                    &prepared.base_branch,
                    &diff,
                    qa.as_ref(),
                    &pack,
                )
            }
            (None, _) => String::new(),
        };
        let head_before = if role == WorkerRole::Builder {
            self.gitops.head(&prepared.worktree.path).await.ok()
        } else {
            None
        };
        let profile_minutes = prepared
            .profile
            .as_ref()
            .and_then(|p| p.timeouts.get(role.as_str()).copied());
        let timeout = self.cfg.timeout_for(role, profile_minutes);
        let config = RunConfig {
            run_id: run.id,
            task_id: Some(task.id),
            task_key: Some(task.key.clone()),
            role,
            harness: choice.harness.clone(),
            model: choice.model.clone(),
            prompt,
            system: kiln_planner::role_system(role),
            workdir: kiln_types::protocol::paths::WORKSPACE.into(),
            branch: Some(prepared.worktree.branch.clone()),
            base_branch: prepared.base_branch.clone(),
            attempt,
            timeout_secs: timeout.as_secs().max(1),
            qa_profile: if role == WorkerRole::Qa {
                prepared.profile.clone()
            } else {
                None
            },
            options: self.options_for(&choice.harness),
            read_only: !role.writes_code(),
        };
        let resources = crate::qa_profile::resources_for(
            &project,
            prepared.profile.as_ref(),
            role == WorkerRole::Qa,
        );
        let services = if role == WorkerRole::Qa {
            prepared
                .profile
                .as_ref()
                .map(|p| p.services.clone())
                .unwrap_or_default()
        } else {
            Default::default()
        };
        let m = match role {
            WorkerRole::Builder => meta::building(attempt, cap),
            WorkerRole::Qa => meta::testing(attempt, cap),
            _ => meta::REVIEWING.to_string(),
        };
        let t = self.store.get_task(task.id).await?;
        if t.meta != m {
            self.set_meta(&t, &m).await?;
        }
        let spec = LaunchSpec {
            role,
            owner: task.owner_id,
            task: Some(task.clone()),
            project,
            config,
            worktree: Some(prepared.worktree.clone()),
            image: prepared.image.clone(),
            resources,
            services,
            head_before,
        };
        self.launch(&run, spec).await?;
        if take_feedback {
            // The prompt (with the feedback) is persisted in config.json.
            let _ = self.store.take_task_feedback(task.id).await;
        }
        Ok(self.store.get_run(run.id).await?)
    }

    /// What the Builder said it did (summary.md, else the outcome summary).
    async fn builder_summary(&self, task: &Task) -> String {
        if let Ok(Some(r)) = self.current_run(task, WorkerRole::Builder).await {
            let md = self.run_dir(r.id).join("out").join("summary.md");
            if let Ok(s) = tokio::fs::read_to_string(md).await
                && !s.trim().is_empty()
            {
                return s;
            }
            if let Ok(Some(o)) = self.store.get_run_outcome(r.id).await {
                return o.summary;
            }
        }
        String::new()
    }
}

impl Scheduler {
    /// Keep the diff after the worktree is gone (it's removed at Ready): the
    /// daemon's task view falls back to a `Diff` artifact holding this JSON.
    async fn persist_diff(&self, run: kiln_types::RunId, diff: &kiln_types::DiffSummary) {
        let rel = "out/artifacts/diff.json";
        let path = self.run_dir(run).join(rel);
        let Ok(json) = serde_json::to_vec(diff) else {
            return;
        };
        if let Some(dir) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(dir).await
        {
            tracing::debug!(error = %e, "cannot create artifact dir for diff");
            return;
        }
        if let Err(e) = tokio::fs::write(&path, &json).await {
            tracing::debug!(error = %e, "cannot write diff artifact");
            return;
        }
        let _ = self
            .store
            .insert_artifact(kiln_store::NewArtifact {
                run_id: run,
                kind: kiln_types::ArtifactKind::Diff,
                path: rel.into(),
                size: json.len() as u64,
                mime: "application/json".into(),
                caption: Some(format!(
                    "+{} −{} · {} files",
                    diff.additions,
                    diff.deletions,
                    diff.files.len()
                )),
            })
            .await;
    }
}
