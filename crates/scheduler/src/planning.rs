//! Planner sessions (docs/SCHEDULER.md "Planner sessions") and plan
//! approval.
//!
//! A user message is persisted, `PlannerStatus{pending}` is published and a
//! planner turn is queued for the thread (one turn at a time per thread; a
//! message that arrives mid-turn schedules another turn). The planner run
//! uses a read-only checkout of the default branch; its outcome becomes a
//! validated plan version plus a reply message, and the owner gets a
//! `PlanReady` notification. Planner runs take a container slot but do not
//! count toward `max_tasks`.

use crate::Scheduler;
use crate::error::{Result, SchedulerError};
use crate::runs::LaunchSpec;
use crate::state::lock;
use kiln_store::{NewMessage, NewNotification, NewPlan, NewRun, NewTask, NewThread, TaskFilter};
use kiln_types::api::{
    ApprovePlanRequest, ApprovePlanResponse, CreateThreadRequest, SendMessageRequest, ThreadDetail,
    UpdatePlanRequest,
};
use kiln_types::event::ServerEvent;
use kiln_types::memory::MemoryQuery;
use kiln_types::plan::PlanSpec;
use kiln_types::protocol::RunConfig;
use kiln_types::{
    Message, MessageAuthor, NotificationAction, NotificationKind, Plan, PlanId, PlanStatus,
    RunState, TaskState, ThreadId, User, WorkerRole,
};
use std::fmt::Write as _;
use std::sync::Arc;

const PLANNER_LABEL: &str = "reading company brain · drafting changes";

impl Scheduler {
    pub(crate) async fn do_create_thread(
        &self,
        user: &User,
        req: CreateThreadRequest,
    ) -> Result<ThreadDetail> {
        if self.st.is_shutdown() {
            return Err(SchedulerError::ShuttingDown);
        }
        if req.message.trim().is_empty() {
            return Err(SchedulerError::Invalid("the first message is empty".into()));
        }
        let project = self
            .store
            .get_project(req.project_id)
            .await
            .map_err(|_| SchedulerError::NotFound("project".into()))?;
        let thread = self
            .store
            .create_thread(NewThread {
                project_id: project.id,
                owner_id: user.id,
                title: crate::util::thread_title(&req.message),
            })
            .await?;
        self.publish_thread(&thread);
        let msg = self
            .store
            .add_message(NewMessage {
                thread_id: thread.id,
                author: MessageAuthor::User,
                body: req.message.trim().to_string(),
                voice: req.voice,
                plan_version: None,
            })
            .await?;
        self.bus.server(ServerEvent::Message {
            message: msg.clone(),
        });
        self.enqueue_planner(thread.id);
        let model = self
            .worker_choice(None, user.id, WorkerRole::Planner)
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        Ok(ThreadDetail {
            thread: self.store.get_thread(thread.id).await?,
            messages: vec![msg],
            plan: None,
            memories_in_context: 0,
            planner_model: model,
        })
    }

    pub(crate) async fn do_planner_message(
        &self,
        user: &User,
        thread: ThreadId,
        req: SendMessageRequest,
    ) -> Result<Message> {
        if self.st.is_shutdown() {
            return Err(SchedulerError::ShuttingDown);
        }
        let t = self
            .store
            .get_thread(thread)
            .await
            .map_err(|_| SchedulerError::NotFound("thread".into()))?;
        if t.owner_id != user.id {
            return Err(SchedulerError::Forbidden(
                "only the thread's owner may write to it".into(),
            ));
        }
        if req.body.trim().is_empty() {
            return Err(SchedulerError::Invalid("the message is empty".into()));
        }
        if t.status != PlanStatus::Drafting {
            return Err(SchedulerError::Conflict(
                "the plan was already approved; start a new thread".into(),
            ));
        }
        let msg = self
            .store
            .add_message(NewMessage {
                thread_id: thread,
                author: MessageAuthor::User,
                body: req.body.trim().to_string(),
                voice: req.voice,
                plan_version: None,
            })
            .await?;
        self.bus.server(ServerEvent::Message {
            message: msg.clone(),
        });
        if let Ok(t) = self.store.get_thread(thread).await {
            self.publish_thread(&t);
        }
        self.enqueue_planner(thread);
        Ok(msg)
    }

    /// Queue a planner turn for a thread (coalesced: one at a time).
    pub(crate) fn enqueue_planner(&self, thread: ThreadId) {
        {
            let mut g = lock(&self.st.planner_threads);
            if let Some(dirty) = g.get_mut(&thread) {
                *dirty = true;
                return;
            }
            g.insert(thread, false);
        }
        let Some(s) = self.arc() else { return };
        self.spawn_bg(async move { s.planner_worker(thread).await });
    }

    async fn planner_worker(self: Arc<Self>, thread: ThreadId) {
        loop {
            if let Err(e) = self.planner_turn(thread).await {
                if matches!(e, SchedulerError::ShuttingDown) {
                    return;
                }
                tracing::warn!(thread = %thread, error = %e, "planner turn failed");
                self.planner_reply_error(thread, &e.to_string()).await;
            }
            let owed = self.owes_reply(thread).await;
            let again = {
                let mut g = lock(&self.st.planner_threads);
                match g.get_mut(&thread) {
                    Some(d) if *d || owed => {
                        *d = false;
                        true
                    }
                    _ => {
                        g.remove(&thread);
                        false
                    }
                }
            };
            if !again {
                return;
            }
        }
    }

    fn planner_status(&self, thread: ThreadId, pending: bool, label: &str) {
        self.bus.server(ServerEvent::PlannerStatus {
            thread_id: thread,
            pending,
            label: label.into(),
        });
    }

    async fn planner_reply_error(&self, thread: ThreadId, err: &str) {
        let body = format!(
            "I couldn't finish that turn: {}",
            crate::util::first_line(err, 200)
        );
        if let Ok(m) = self
            .store
            .add_message(NewMessage {
                thread_id: thread,
                author: MessageAuthor::System,
                body,
                voice: false,
                plan_version: None,
            })
            .await
        {
            self.bus.server(ServerEvent::Message { message: m });
        }
        self.planner_status(thread, false, "");
    }

    /// One planner turn: (re-)attach to a live planner run of this thread,
    /// apply a finished-but-unapplied one (recovered after a restart), or
    /// start a new one; then apply its outcome.
    async fn planner_turn(self: &Arc<Self>, thread_id: ThreadId) -> Result<()> {
        let thread = self.store.get_thread(thread_id).await?;
        if thread.status != PlanStatus::Drafting {
            return Ok(());
        }
        self.planner_status(thread_id, true, PLANNER_LABEL);
        let runs = self.store.list_runs_for_thread(thread_id).await?;
        for r in runs.iter().filter(|r| r.state == RunState::Pending) {
            let _ = self
                .store
                .finish_run(
                    r.id,
                    RunState::Cancelled,
                    &kiln_types::protocol::RunOutcome::failed("superseded"),
                )
                .await;
        }
        let latest = runs
            .iter()
            .rev()
            .find(|r| r.state != RunState::Pending)
            .cloned();
        let run = match latest {
            Some(r) if !r.state.is_finished() => r,
            Some(r)
                if r.state != RunState::Cancelled && !self.read_marker(r.id, "planner_applied") =>
            {
                r
            }
            _ => self.start_planner_run(&thread).await?,
        };
        let f = self.await_run(run.id).await?;
        self.apply_planner_outcome(thread_id, &run, f.state, &f.outcome)
            .await
    }

    /// True when the thread's latest message is from the user (a planner
    /// turn is owed).
    async fn owes_reply(&self, thread: ThreadId) -> bool {
        match self.store.list_messages(thread).await {
            Ok(m) => m.last().is_some_and(|m| m.author == MessageAuthor::User),
            Err(_) => false,
        }
    }

    async fn start_planner_run(
        self: &Arc<Self>,
        thread: &kiln_types::Thread,
    ) -> Result<kiln_types::Run> {
        let owner = thread.owner_id;
        let project = self.store.get_project(thread.project_id).await?;
        let choice = self.worker_choice(None, owner, WorkerRole::Planner).await?;
        if self.harnesses.get(&choice.harness).is_none() {
            return Err(SchedulerError::Invalid(format!(
                "unknown planner harness {:?}",
                choice.harness
            )));
        }
        let checkout = self.planner_checkout(&project).await?;
        let run = self
            .store
            .create_run(NewRun {
                task_id: None,
                thread_id: Some(thread.id),
                owner_id: owner,
                role: WorkerRole::Planner,
                harness: choice.harness.clone(),
                model: choice.model.clone(),
                attempt: 1,
            })
            .await?;
        self.publish_run(&run);
        let history = self.store.list_messages(thread.id).await?;
        let current = self.store.get_plan_for_thread(thread.id).await?;
        let pack = self
            .memory
            .pack(owner, project.id, WorkerRole::Planner)
            .await
            .unwrap_or_default();
        let query = history
            .iter()
            .rev()
            .find(|m| m.author == MessageAuthor::User)
            .map(|m| m.body.clone())
            .unwrap_or_default();
        let hits = self
            .memory
            .search(
                owner,
                &MemoryQuery {
                    q: query,
                    project_id: Some(project.id),
                    limit: Some(5),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_default();
        let mut memory = pack;
        if !hits.is_empty() {
            memory.push_str("\n\n## Search hits for the latest message\n\n");
            for h in &hits {
                let i = &h.item;
                let _ = writeln!(
                    memory,
                    "- **{}** — {} {} {}",
                    i.title,
                    i.subject,
                    i.relation.replace('_', " "),
                    i.object
                );
            }
            let ids: Vec<_> = hits.iter().map(|h| h.item.id).collect();
            let ctx = kiln_memory::UseContext {
                run: Some(run.id),
                task: None,
                role: Some(WorkerRole::Planner),
            };
            let _ = self.memory.record_use(owner, &ids, &ctx).await;
        }
        let prompt = kiln_planner::planner_prompt(
            &project,
            &history,
            &memory,
            current.as_ref().map(|p| &p.spec),
        );
        let config = RunConfig {
            run_id: run.id,
            task_id: None,
            task_key: None,
            role: WorkerRole::Planner,
            harness: choice.harness.clone(),
            model: choice.model.clone(),
            prompt,
            system: kiln_planner::role_system(WorkerRole::Planner),
            workdir: kiln_types::protocol::paths::WORKSPACE.into(),
            branch: None,
            base_branch: project.default_branch.clone(),
            attempt: 1,
            timeout_secs: self
                .cfg
                .timeout_for(WorkerRole::Planner, None)
                .as_secs()
                .max(1),
            qa_profile: None,
            options: self.options_for(&choice.harness),
            read_only: true,
        };
        let spec = LaunchSpec {
            role: WorkerRole::Planner,
            owner,
            task: None,
            image: project
                .image
                .clone()
                .unwrap_or_else(|| self.cfg.worker_image.clone()),
            resources: project.resources.clone(),
            project,
            config,
            worktree: Some(checkout),
            services: Default::default(),
            head_before: None,
        };
        self.launch(&run, spec).await?;
        Ok(self.store.get_run(run.id).await?)
    }

    /// Validate the plan, store a new version, add the reply, notify.
    pub(crate) async fn apply_planner_outcome(
        &self,
        thread_id: ThreadId,
        run: &kiln_types::Run,
        state: RunState,
        outcome: &kiln_types::protocol::RunOutcome,
    ) -> Result<()> {
        // Idempotent: a reply for this run was already recorded.
        if self.read_marker(run.id, "planner_applied") {
            self.planner_status(thread_id, false, "");
            return Ok(());
        }
        let thread = self.store.get_thread(thread_id).await?;
        let mut version = None;
        let mut body = outcome
            .reply
            .clone()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| outcome.summary.clone());
        if state != RunState::Succeeded && outcome.plan.is_none() {
            let why = outcome
                .error
                .clone()
                .unwrap_or_else(|| state.as_str().into());
            body = format!(
                "The planner run failed: {}",
                crate::util::first_line(&why, 200)
            );
        }
        let mut plan_out: Option<Plan> = None;
        if let Some(spec) = &outcome.plan {
            match spec.validate() {
                Ok(()) => match self.save_plan_version(&thread, spec).await {
                    Ok(p) => {
                        version = Some(p.version);
                        plan_out = Some(p);
                    }
                    Err(e) => {
                        let _ = write!(body, "\n\n_(The new draft could not be saved: {e})_");
                    }
                },
                Err(e) => {
                    let _ = write!(
                        body,
                        "\n\n_The proposed plan is invalid and was not saved: {e}. Tell me how to fix it, or ask me to try again._"
                    );
                }
            }
        }
        if body.trim().is_empty() {
            body = "Done.".into();
        }
        let msg = self
            .store
            .add_message(NewMessage {
                thread_id,
                author: MessageAuthor::Planner,
                body,
                voice: false,
                plan_version: version,
            })
            .await?;
        self.mark(run.id, "planner_applied");
        self.bus.server(ServerEvent::Message { message: msg });
        let thread = self
            .store
            .update_thread_status(thread_id, PlanStatus::Drafting)
            .await?;
        self.publish_thread(&thread);
        self.planner_status(thread_id, false, "");
        if let Some(p) = plan_out {
            self.publish_plan(&p);
            let mut n = NewNotification::new(
                p.owner_id,
                NotificationKind::PlanReady,
                format!(
                    "Approve plan: {}",
                    crate::util::truncate(&p.spec.feature, 80)
                ),
                format!("Draft v{} · {} task(s)", p.version, p.spec.tasks.len()),
            );
            n.deep_link = format!("plan/{}", p.id);
            n.plan_id = Some(p.id);
            n.actions = vec![
                NotificationAction {
                    id: "approve".into(),
                    label: "Approve".into(),
                },
                NotificationAction {
                    id: "review".into(),
                    label: "Review".into(),
                },
            ];
            if let Err(e) = self.notifier.notify(n).await {
                tracing::warn!(error = %e, "PlanReady notification failed");
            }
        }
        Ok(())
    }

    async fn save_plan_version(
        &self,
        thread: &kiln_types::Thread,
        spec: &PlanSpec,
    ) -> Result<Plan> {
        match self.store.get_plan_for_thread(thread.id).await? {
            None => Ok(self
                .store
                .create_plan(NewPlan {
                    project_id: thread.project_id,
                    owner_id: thread.owner_id,
                    thread_id: thread.id,
                    spec: spec.clone(),
                })
                .await?),
            Some(p) => Ok(self.store.update_plan_spec(p.id, spec, p.version).await?),
        }
    }

    /// Small per-run idempotency markers in the run folder.
    pub(crate) fn mark(&self, run: kiln_types::RunId, name: &str) {
        let _ = std::fs::write(self.run_dir(run).join(format!(".{name}")), b"1");
    }
    pub(crate) fn read_marker(&self, run: kiln_types::RunId, name: &str) -> bool {
        self.run_dir(run).join(format!(".{name}")).exists()
    }

    pub(crate) async fn do_update_plan(
        &self,
        user: &User,
        id: PlanId,
        req: UpdatePlanRequest,
    ) -> Result<Plan> {
        let plan = self
            .store
            .get_plan(id)
            .await
            .map_err(|_| SchedulerError::NotFound("plan".into()))?;
        if plan.owner_id != user.id {
            return Err(SchedulerError::Forbidden(
                "only the plan's owner may edit it".into(),
            ));
        }
        req.spec
            .validate()
            .map_err(|e| SchedulerError::Invalid(e.to_string()))?;
        let p = self
            .store
            .update_plan_spec(id, &req.spec, req.version)
            .await?;
        self.publish_plan(&p);
        Ok(p)
    }

    pub(crate) async fn do_approve_plan(
        &self,
        user: &User,
        id: PlanId,
        req: ApprovePlanRequest,
    ) -> Result<ApprovePlanResponse> {
        if self.st.is_shutdown() {
            return Err(SchedulerError::ShuttingDown);
        }
        let plan = self
            .store
            .get_plan(id)
            .await
            .map_err(|_| SchedulerError::NotFound("plan".into()))?;
        if plan.owner_id != user.id {
            return Err(SchedulerError::Forbidden(
                "only the plan's owner may approve it".into(),
            ));
        }
        if plan.status != PlanStatus::Drafting {
            return Err(SchedulerError::Conflict(
                "the plan was already approved".into(),
            ));
        }
        if let Some(v) = req.version
            && v != plan.version
        {
            return Err(SchedulerError::Conflict(format!(
                "you approved v{v} but the plan is now v{}",
                plan.version
            )));
        }
        plan.spec
            .validate()
            .map_err(|e| SchedulerError::Invalid(e.to_string()))?;
        let mut defaults = self.cfg.default_workers.clone();
        if let Some(s) = self.store.get_user_settings(user.id).await? {
            defaults.extend(s.worker_defaults);
        }
        let order = plan
            .spec
            .topo_order()
            .map_err(|e| SchedulerError::Invalid(e.to_string()))?;
        let new: Vec<NewTask> = order
            .iter()
            .filter_map(|id| plan.spec.tasks.iter().find(|t| &t.id == id))
            .map(|t| NewTask {
                spec_id: t.id.clone(),
                title: t.title.clone(),
                description: t.description.clone(),
                acceptance: t.acceptance_criteria.clone(),
                files_hint: t.files_hint.clone(),
                depends_on: t.depends_on.clone(),
                workers: t.workers.resolve(&defaults),
            })
            .collect();
        // Status first (CAS-like guard against a double approve), then tasks.
        let fresh = self.store.get_plan(id).await?;
        if fresh.status != PlanStatus::Drafting || fresh.version != plan.version {
            return Err(SchedulerError::Conflict(
                "the plan changed while approving".into(),
            ));
        }
        let tasks = self.store.create_tasks_for_plan(id, new).await?;
        let plan = self
            .store
            .set_plan_status(id, PlanStatus::Dispatched)
            .await?;
        self.publish_plan(&plan);
        if let Ok(t) = self.store.get_thread(plan.thread_id).await {
            self.publish_thread(&t);
        }
        for t in &tasks {
            self.publish_task(t);
        }
        // Admit what fits right now so the summary is accurate.
        if let Some(s) = self.arc() {
            let _ = s.dispatch_pass().await;
        }
        self.wake();
        let tasks = self
            .store
            .list_tasks(TaskFilter {
                plan: Some(id),
                ..Default::default()
            })
            .await?;
        let building = tasks.iter().filter(|t| t.state.is_active()).count();
        let queued = tasks
            .iter()
            .filter(|t| t.state == TaskState::Queued)
            .count();
        let summary = format!(
            "{} task{} dispatched · {building} building, {queued} queued",
            tasks.len(),
            if tasks.len() == 1 { "" } else { "s" }
        );
        Ok(ApprovePlanResponse {
            plan,
            tasks,
            summary,
        })
    }
}
