//! Run lifecycle (docs/SCHEDULER.md "`run(role)`").
//!
//! run row (Pending) → capacity → run folder (`config.json`, `meta.json`)
//! → token → socket up → container spec → create/start (Starting → Running)
//! → supervisor awaits `Complete` / container exit / timeout / stop
//! → `finish_run` → `RunFinished`.
//!
//! The run folder holds everything needed to re-attach after a daemon
//! restart: the `RunConfig` sent as `Welcome`, the admission footprint and
//! the Builder's pre-run HEAD.

use crate::Scheduler;
use crate::error::{Result, SchedulerError};
use crate::state::{Finished, LiveRun, Reservation, lock};
use crate::util::meta;
use kiln_events::Event;
use kiln_gitops::Worktree;
use kiln_sandbox::{
    ContainerSpec, LABEL_OWNER, LABEL_ROLE, LABEL_RUN_ID, LABEL_TASK, NetworkMode, SERVICE_CAPS,
    mounts_for, prepare_run_dir,
};
use kiln_types::protocol::{RunConfig, RunOutcome, paths};
use kiln_types::qa::QaService;
use kiln_types::{
    Project, ProjectId, ResourceProfile, Run, RunId, RunState, Task, UserId, WorkerRole,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

/// The daemon's own deadline is the runner's timeout plus this margin (the
/// runner enforces `timeout_secs` itself and reports a clean outcome).
const DEADLINE_MARGIN: Duration = Duration::from_secs(30);
/// After `Complete`, wait this long for the container to exit.
const EXIT_AFTER_COMPLETE: Duration = Duration::from_secs(15);
/// After the container exits, wait this long for a late `Complete`.
const COMPLETE_AFTER_EXIT: Duration = Duration::from_secs(3);
/// How long a QA service may take to pass its `healthcheck`.
const SERVICE_HEALTH_TIMEOUT: Duration = Duration::from_secs(120);
const SERVICE_HEALTH_POLL: Duration = Duration::from_secs(1);

/// What to launch for one run.
pub(crate) struct LaunchSpec {
    pub role: WorkerRole,
    pub owner: UserId,
    pub task: Option<Task>,
    pub project: Project,
    /// `run_id` is overwritten with the run's id.
    pub config: RunConfig,
    pub worktree: Option<Worktree>,
    pub image: String,
    pub resources: ResourceProfile,
    pub services: BTreeMap<String, QaService>,
    /// Builder: HEAD before the run (success needs a new commit).
    pub head_before: Option<String>,
}

/// Persisted next to `config.json` (`<run_dir>/meta.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct RunMeta {
    pub head_before: Option<String>,
    pub memory_mb: u64,
    pub image: String,
    pub timeout_secs: u64,
    pub project_id: Option<ProjectId>,
    pub network: Option<String>,
    #[serde(default)]
    pub sidecars: Vec<String>,
}

impl Scheduler {
    pub(crate) fn read_run_config(&self, run: RunId) -> Option<RunConfig> {
        let p = self.run_dir(run).join("config.json");
        serde_json::from_slice(&std::fs::read(p).ok()?).ok()
    }

    pub(crate) fn read_run_meta(&self, run: RunId) -> Option<RunMeta> {
        let p = self.run_dir(run).join("meta.json");
        serde_json::from_slice(&std::fs::read(p).ok()?).ok()
    }

    /// `<run_dir>/out/outcome.json`, written by the runner before it exits.
    pub(crate) fn read_outcome_file(&self, run: RunId) -> Option<RunOutcome> {
        let p = self.run_dir(run).join("out").join("outcome.json");
        serde_json::from_slice(&std::fs::read(p).ok()?).ok()
    }

    // ------------------------------------------------------------ capacity

    /// Wait until the run fits (containers, RAM, user share), then hold a
    /// reservation for it (docs/SCHEDULER.md "re-checks admission before
    /// each role's container starts").
    pub(crate) async fn acquire_capacity(&self, run: RunId, res: Reservation) -> Result<()> {
        let mut waited = false;
        let mut shutdown = self.st.shutdown.subscribe();
        loop {
            if self.st.is_shutdown() {
                return Err(SchedulerError::ShuttingDown);
            }
            {
                let mut r = lock(&self.st.reservations);
                if r.contains_key(&run) || self.fits(&r, &res) {
                    r.insert(run, res);
                    return Ok(());
                }
            }
            if !waited {
                waited = true;
                if let Some(t) = res.task_id
                    && let Ok(task) = self.store.get_task(t).await
                    && let Ok(task) = self
                        .store
                        .update_task_progress(t, task.progress, meta::WAITING)
                        .await
                {
                    self.publish_task(&task);
                }
            }
            tokio::select! {
                _ = self.st.capacity_freed.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = shutdown.changed() => {}
            }
        }
    }

    fn fits(&self, r: &std::collections::HashMap<RunId, Reservation>, res: &Reservation) -> bool {
        let cap = &self.capacity;
        let budget = cap.budget_mb();
        let want = res.memory_mb.min(budget);
        if r.len() as u32 + 1 > cap.max_containers.max(1) {
            return false;
        }
        let used: u64 = r.values().map(|x| x.memory_mb).sum();
        if used + want > budget {
            return false;
        }
        let others = r.values().any(|x| x.owner != res.owner);
        if others {
            let share = self.user_share(res.owner);
            let mine: u64 = r
                .values()
                .filter(|x| x.owner == res.owner)
                .map(|x| x.memory_mb)
                .sum();
            let limit = (share.clamp(0.05, 1.0) as f64 * budget as f64) as u64;
            if mine + want > limit.max(want) && mine > 0 {
                return false;
            }
        }
        true
    }

    fn user_share(&self, user: UserId) -> f32 {
        lock(&self.st.user_shares)
            .get(&user)
            .copied()
            .unwrap_or(0.5)
    }

    pub(crate) fn release_capacity(&self, run: RunId) {
        if lock(&self.st.reservations).remove(&run).is_some() {
            self.st.capacity_freed.notify_waiters();
            self.st.capacity_freed.notify_one();
            self.wake();
        }
    }

    // ------------------------------------------------------------ launch

    /// Launch a Pending run: capacity, run folder, socket, container. On a
    /// start failure the run is finished as Failed (and `done` is set), so
    /// callers always just await the run.
    pub(crate) async fn launch(self: &Arc<Self>, run: &Run, mut spec: LaunchSpec) -> Result<()> {
        spec.config.run_id = run.id;
        let res = Reservation {
            owner: spec.owner,
            memory_mb: spec.resources.memory_mb,
            task_id: spec.task.as_ref().map(|t| t.id),
        };
        self.acquire_capacity(run.id, res).await?;
        match self.launch_inner(run, &spec).await {
            Ok(()) => Ok(()),
            Err(SchedulerError::ShuttingDown) => Err(SchedulerError::ShuttingDown),
            Err(e) => {
                tracing::warn!(run = %run.id, role = spec.role.as_str(), error = %e, "run failed to start");
                let msg = format!("could not start the {} container: {e}", spec.role.label());
                let live = lock(&self.st.live).get(&run.id).cloned();
                self.finalize(run.id, live, RunState::Failed, RunOutcome::failed(msg))
                    .await;
                Ok(())
            }
        }
    }

    /// Run a QA service's `healthcheck` (via `sh -c`) until it exits 0.
    async fn await_healthy(&self, name: &str, id: &str, check: &str) -> Result<()> {
        let cmd = vec!["sh".to_string(), "-c".to_string(), check.to_string()];
        let deadline = Instant::now() + SERVICE_HEALTH_TIMEOUT;
        loop {
            let last = match self.runtime.exec(id, &cmd).await {
                Ok(o) if o.exit_code == 0 => return Ok(()),
                Ok(o) => o.output,
                Err(e) => e.to_string(),
            };
            if !self.runtime.is_running(id).await.unwrap_or(false) {
                let logs = self.runtime.logs_tail(id, 20).await.unwrap_or_default();
                return Err(SchedulerError::Invalid(format!(
                    "QA service `{name}` exited before becoming healthy:\n{logs}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(SchedulerError::Invalid(format!(
                    "QA service `{name}` not healthy after {}s; last check said: {}",
                    SERVICE_HEALTH_TIMEOUT.as_secs(),
                    last.trim()
                )));
            }
            tokio::time::sleep(SERVICE_HEALTH_POLL).await;
        }
    }

    async fn launch_inner(self: &Arc<Self>, run: &Run, spec: &LaunchSpec) -> Result<()> {
        let run_dir = self.run_dir(run.id);
        prepare_run_dir(&run_dir)?;
        let timeout_secs = spec.config.timeout_secs;
        let mut meta = RunMeta {
            head_before: spec.head_before.clone(),
            memory_mb: spec.resources.memory_mb,
            image: spec.image.clone(),
            timeout_secs,
            project_id: Some(spec.project.id),
            network: None,
            sidecars: vec![],
        };
        write_json(&run_dir.join("config.json"), &spec.config)?;
        write_json(&run_dir.join("meta.json"), &meta)?;

        let token = kiln_auth::new_token();
        let token_hash = kiln_auth::token_hash(&token);
        crate::sql::set_run_token_hash(&self.store, run.id, &token_hash).await?;

        let secrets = match self.vault.dispatch_env(spec.owner).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(user = %spec.owner, error = %e, "no credentials for dispatch; running without");
                kiln_vault::DispatchSecrets::default()
            }
        };
        let mut redactor = secrets.redactor.clone();
        redactor.extend([token.as_str()]);

        let live = self.register_live(
            run,
            spec.project.id,
            spec.config.clone(),
            token_hash.clone(),
            redactor,
        );
        self.serve_run(live.clone())?;

        // QA sidecars on a per-run network (decision #4).
        let mut network = NetworkMode::Bridge;
        if spec.role == WorkerRole::Qa && !spec.services.is_empty() {
            let net = format!("kiln-run-{}", run.id);
            self.runtime.create_network(&net).await?;
            meta.network = Some(net.clone());
            for (name, svc) in &spec.services {
                let mut s = ContainerSpec::new(format!("kiln-svc-{}-{name}", run.id), &svc.image);
                s.cmd = svc.command.clone().unwrap_or_default();
                s.env = svc
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                s.network = NetworkMode::Named(net.clone());
                s.aliases = vec![name.clone()];
                s.cap_add = SERVICE_CAPS.iter().map(|c| c.to_string()).collect();
                s.labels.insert(LABEL_RUN_ID.into(), run.id.to_string());
                s.labels.insert(LABEL_ROLE.into(), "service".into());
                s.labels.insert(LABEL_OWNER.into(), spec.owner.to_string());
                self.tag_instance(&mut s.labels);
                self.runtime.ensure_image(&svc.image).await?;
                let id = self.runtime.create(&s).await?;
                meta.sidecars.push(id.clone());
                write_json(&run_dir.join("meta.json"), &meta)?;
                self.runtime.start(&id).await?;
            }
            let ids = meta.sidecars.clone();
            for ((name, svc), id) in spec.services.iter().zip(&ids) {
                match &svc.healthcheck {
                    Some(check) => self.await_healthy(name, id, check).await?,
                    // No check declared: give it a moment; QA's `boot` steps
                    // can still wait on the port.
                    None => tokio::time::sleep(Duration::from_secs(2)).await,
                }
            }
            network = NetworkMode::Named(net);
        }

        let mut c = ContainerSpec::new(format!("kiln-run-{}", run.id), &spec.image);
        let process_mode = self.cfg.runner_bin.is_some();
        c.cmd = match &self.cfg.runner_bin {
            Some(bin) => vec![bin.to_string_lossy().into_owned(), "run".into()],
            None => vec!["kiln-runner".into(), "run".into()],
        };
        c.env = secrets.env.clone();
        c.env.push((paths::ENV_TOKEN.into(), token));
        c.env.push((paths::ENV_RUN_ID.into(), run.id.to_string()));
        c.env.push((paths::ENV_SOCKET.into(), paths::SOCKET.into()));
        c.env.push((
            "KILN_RECONNECT_BUDGET_MS".into(),
            self.cfg.runner_reconnect_budget.as_millis().to_string(),
        ));
        c.env.push((
            "KILN_STOP_GRACE_MS".into(),
            self.cfg.stop_grace.as_millis().to_string(),
        ));
        if let Some(bin) = &self.cfg.runner_bin {
            c.env
                .push(("KILN_RUNNER_BIN".into(), bin.to_string_lossy().into_owned()));
        } else {
            c.env.push(("HOME".into(), "/home/kiln".into()));
        }
        c.mounts = mounts_for(
            spec.role,
            spec.worktree.as_ref().map(|w| w.path.as_path()),
            spec.worktree.as_ref().map(|w| w.git_common_dir.as_path()),
            &run_dir,
        );
        c.cpus = spec.resources.cpus.max(0.0);
        c.memory_mb = spec.resources.memory_mb;
        c.network = network;
        c.labels.insert(LABEL_RUN_ID.into(), run.id.to_string());
        c.labels
            .insert(LABEL_ROLE.into(), spec.role.as_str().into());
        c.labels.insert(LABEL_OWNER.into(), spec.owner.to_string());
        self.tag_instance(&mut c.labels);
        if let Some(t) = &spec.task {
            c.labels.insert(LABEL_TASK.into(), t.key.clone());
        }
        c.workdir = Some(if spec.worktree.is_some() {
            paths::WORKSPACE.into()
        } else {
            paths::RUN_DIR.into()
        });
        if !process_mode {
            c.user = host_user();
        }

        self.runtime.ensure_image(&c.image).await?;
        let cid = self.runtime.create(&c).await?;
        self.store
            .set_run_container(run.id, &cid, &token_hash)
            .await?;
        let r = self
            .store
            .update_run_state(run.id, RunState::Starting, None)
            .await?;
        self.publish_run(&r);
        self.runtime.start(&cid).await?;
        let r = self
            .store
            .update_run_state(run.id, RunState::Running, None)
            .await?;
        self.publish_run(&r);
        tracing::info!(run = %run.id, role = spec.role.as_str(), container = %cid, "run started");

        let s = self.clone();
        let timeout = Duration::from_secs(timeout_secs.max(1)) + DEADLINE_MARGIN;
        self.spawn_bg(async move { s.supervise(live, cid, Instant::now() + timeout).await });
        Ok(())
    }

    /// Register a live run (socket state) in the registry.
    pub(crate) fn register_live(
        &self,
        run: &Run,
        project_id: ProjectId,
        config: RunConfig,
        token_hash: String,
        redactor: kiln_vault::Redactor,
    ) -> Arc<LiveRun> {
        let live = Arc::new(LiveRun {
            run_id: run.id,
            owner: run.owner_id,
            role: run.role,
            task_id: run.task_id,
            thread_id: run.thread_id,
            project_id: Some(project_id),
            config,
            token_hash,
            redactor,
            outcome: watch::channel(None).0,
            control: broadcast::channel(16).0,
            done: watch::channel(None).0,
            stop_reason: Default::default(),
            stop_notify: Default::default(),
            tasks: Default::default(),
            l0_buf: Default::default(),
            l0_lock: Default::default(),
        });
        if let Some(old) = lock(&self.st.live).insert(run.id, live.clone()) {
            old.abort();
        }
        live
    }

    /// Await `Complete`, the container's exit, the deadline or a stop
    /// request, then finish the run.
    pub(crate) async fn supervise(
        self: Arc<Self>,
        live: Arc<LiveRun>,
        cid: String,
        deadline: Instant,
    ) {
        let mut outcome_rx = live.outcome.subscribe();
        let rt = self.runtime.clone();
        let wait_id = cid.clone();
        let mut exit = Box::pin(async move { rt.wait(&wait_id).await });
        let mut exited: Option<std::result::Result<i64, String>> = None;
        let mut timed_out = false;
        let mut force_at: Option<Instant> = None;
        let mut forced = false;
        let grace = self.cfg.stop_grace;

        loop {
            if outcome_rx.borrow().is_some() || exited.is_some() {
                break;
            }
            tokio::select! {
                r = &mut exit => {
                    exited = Some(r.map_err(|e| e.to_string()));
                }
                c = outcome_rx.changed() => {
                    if c.is_err() {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(deadline), if !timed_out => {
                    timed_out = true;
                    let secs = live.config.timeout_secs;
                    live.request_stop(RunState::TimedOut, &format!("timed out after {secs}s"));
                }
                _ = live.stop_notify.notified() => {
                    if force_at.is_none() {
                        force_at = Some(Instant::now() + grace);
                    }
                }
                _ = tokio::time::sleep_until(force_at.unwrap_or_else(Instant::now)), if force_at.is_some() && !forced => {
                    forced = true;
                    tracing::info!(run = %live.run_id, "runner did not stop in time; stopping the container");
                    if let Err(e) = self.runtime.stop(&cid, Duration::from_secs(2)).await {
                        tracing::warn!(run = %live.run_id, error = %e, "container stop failed");
                    }
                }
            }
        }
        if exited.is_none() {
            // Complete arrived: give the container a moment to exit.
            if let Ok(r) = tokio::time::timeout(EXIT_AFTER_COMPLETE, &mut exit).await {
                exited = Some(r.map_err(|e| e.to_string()));
            }
        } else if outcome_rx.borrow().is_none() {
            let _ = tokio::time::timeout(COMPLETE_AFTER_EXIT, outcome_rx.wait_for(Option::is_some))
                .await;
        }
        let delivered = outcome_rx.borrow().clone();
        let stop = lock(&live.stop_reason).clone();
        let silent = delivered.is_none() && stop.is_none();
        let (state, mut outcome) = self.classify_end(live.run_id, delivered, exited, stop);
        if silent
            && state == RunState::Failed
            && outcome
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("runner exited"))
        {
            // No outcome at all: the runner likely never started. The
            // container is removed after ingest, so keep its last words.
            if let Ok(tail) = self.runtime.logs_tail(&cid, 20).await
                && !tail.trim().is_empty()
            {
                let tail = live.redactor.scrub(&tail);
                if let Some(e) = outcome.error.as_mut() {
                    e.push_str(":\n");
                    e.push_str(tail.trim_end());
                }
            }
        }
        self.finalize(live.run_id, Some(live), state, outcome).await;
    }

    /// Decide the final run state from what we observed.
    fn classify_end(
        &self,
        run: RunId,
        delivered: Option<RunOutcome>,
        exited: Option<std::result::Result<i64, String>>,
        stop: Option<(RunState, String)>,
    ) -> (RunState, RunOutcome) {
        let outcome = delivered.or_else(|| self.read_outcome_file(run));
        match (outcome, stop) {
            (Some(o), Some((state, _))) if !o.success => (state, o),
            (Some(o), Some((state, reason))) if state == RunState::Cancelled => {
                let mut o = o;
                o.success = false;
                o.error.get_or_insert(reason);
                (state, o)
            }
            (Some(o), _) => {
                let timed_out = o
                    .error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("timed out"));
                let state = if o.success {
                    RunState::Succeeded
                } else if timed_out {
                    RunState::TimedOut
                } else {
                    RunState::Failed
                };
                (state, o)
            }
            (None, Some((state, reason))) => (state, RunOutcome::failed(reason)),
            (None, None) => {
                let msg = match &exited {
                    Some(Ok(code)) => {
                        format!("runner exited with code {code} without reporting an outcome")
                    }
                    Some(Err(e)) => format!("lost the container: {e}"),
                    None => "the run ended without an outcome".into(),
                };
                let mut o = RunOutcome::failed(msg);
                if let Some(Ok(code)) = exited {
                    o.exit_code = i32::try_from(code).ok();
                }
                (RunState::Failed, o)
            }
        }
    }

    /// Persist the end of a run and notify everyone waiting on it.
    pub(crate) async fn finalize(
        &self,
        run: RunId,
        live: Option<Arc<LiveRun>>,
        state: RunState,
        outcome: RunOutcome,
    ) {
        if let Some(l) = &live {
            self.flush_l0(l).await;
        }
        let mut outcome = outcome;
        if outcome.last_seq == 0
            && let Ok(r) = self.store.get_run(run).await
            && let Some(o) = self.read_outcome_file(r.id)
        {
            outcome.last_seq = o.last_seq;
        }
        let finished = match self.store.finish_run(run, state, &outcome).await {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::error!(run = %run, error = %e, "could not persist run outcome");
                None
            }
        };
        // QA sidecars and their network go as soon as QA is done.
        if let Some(m) = self.read_run_meta(run) {
            for id in &m.sidecars {
                let _ = self.runtime.remove(id).await;
            }
            if let Some(net) = &m.network {
                let _ = self.runtime.remove_network(net).await;
            }
        }
        if let Some(r) = &finished {
            self.publish_run(r);
            self.bus.publish(Event::RunFinished {
                run_id: r.id,
                task_id: r.task_id,
                owner_id: r.owner_id,
                role: r.role,
                success: state == RunState::Succeeded,
            });
            tracing::info!(run = %r.id, role = r.role.as_str(), state = state.as_str(), "run finished");
        }
        self.release_capacity(run);
        let live = live.or_else(|| lock(&self.st.live).get(&run).cloned());
        if let Some(l) = live {
            {
                let mut map = lock(&self.st.live);
                if map.get(&run).is_some_and(|x| Arc::ptr_eq(x, &l)) {
                    map.remove(&run);
                }
            }
            l.done.send_replace(Some(Finished { state, outcome }));
            l.abort();
        }
        let _ = std::fs::remove_file(self.run_dir(run).join("kiln.sock"));
        self.queue_ingest(run);
    }

    /// Wait for a run to finish (live, re-attached, or already finished).
    pub(crate) async fn await_run(self: &Arc<Self>, run: RunId) -> Result<Finished> {
        loop {
            let live = lock(&self.st.live).get(&run).cloned();
            if let Some(l) = live {
                let mut rx = l.done.subscribe();
                let mut shutdown = self.st.shutdown.subscribe();
                tokio::select! {
                    r = rx.wait_for(Option::is_some) => {
                        if let Ok(v) = r
                            && let Some(f) = v.clone()
                        {
                            return Ok(f);
                        }
                    }
                    _ = shutdown.changed() => return Err(SchedulerError::ShuttingDown),
                }
                continue;
            }
            let r = self.store.get_run(run).await?;
            if r.state.is_finished() {
                let outcome = self.store.get_run_outcome(run).await?.unwrap_or_else(|| {
                    RunOutcome::failed(r.error.clone().unwrap_or_else(|| r.state.as_str().into()))
                });
                return Ok(Finished {
                    state: r.state,
                    outcome,
                });
            }
            // Live in the database but nobody supervises it (e.g. recovery
            // could not attach yet): attach now.
            self.attach(&r).await?;
            if lock(&self.st.live).get(&run).is_none()
                && !self.store.get_run(run).await?.state.is_finished()
            {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }

    /// Re-attach to a run that was live when the daemon stopped (crash
    /// recovery). Container alive → new socket listener + supervisor; gone →
    /// `out/outcome.json` if the runner wrote one, else `Lost`.
    pub(crate) async fn attach(self: &Arc<Self>, run: &Run) -> Result<()> {
        if lock(&self.st.live).contains_key(&run.id) {
            return Ok(());
        }
        let alive = match &run.container_id {
            Some(c) if !c.is_empty() => self.runtime.is_running(c).await.unwrap_or(false),
            _ => false,
        };
        let config = self.read_run_config(run.id);
        let token_hash = self.store.run_token_hash(run.id).await?;
        let meta = self.read_run_meta(run.id).unwrap_or_default();
        match (alive, config, token_hash) {
            (true, Some(config), Some(hash)) => {
                let redactor = match self.vault.dispatch_env(run.owner_id).await {
                    Ok(s) => s.redactor,
                    Err(_) => kiln_vault::Redactor::default(),
                };
                let project = match meta.project_id {
                    Some(p) => p,
                    None => self.project_of_run(run).await?,
                };
                lock(&self.st.reservations).insert(
                    run.id,
                    Reservation {
                        owner: run.owner_id,
                        memory_mb: meta.memory_mb,
                        task_id: run.task_id,
                    },
                );
                let live = self.register_live(run, project, config.clone(), hash, redactor);
                if let Err(e) = self.serve_run(live.clone()) {
                    tracing::error!(run = %run.id, error = %e, "re-attach: cannot bind the run socket");
                    let _ = self
                        .runtime
                        .stop(
                            run.container_id.as_deref().unwrap_or_default(),
                            Duration::from_secs(2),
                        )
                        .await;
                    self.finalize(
                        run.id,
                        Some(live),
                        RunState::Lost,
                        RunOutcome::failed(format!("re-attach failed: {e}")),
                    )
                    .await;
                    return Ok(());
                }
                let elapsed = run
                    .started_at
                    .map(|s| (chrono::Utc::now() - s).to_std().unwrap_or_default())
                    .unwrap_or_default();
                let total = Duration::from_secs(config.timeout_secs.max(1)) + DEADLINE_MARGIN;
                let deadline =
                    Instant::now() + total.saturating_sub(elapsed).max(Duration::from_secs(5));
                let cid = run.container_id.clone().unwrap_or_default();
                tracing::info!(run = %run.id, role = run.role.as_str(), "re-attached to a live run");
                let s = self.clone();
                self.spawn_bg(async move { s.supervise(live, cid, deadline).await });
            }
            _ => {
                if alive && let Some(c) = &run.container_id {
                    // Alive but we can't talk to it (no config/token): stop it.
                    let _ = self.runtime.stop(c, Duration::from_secs(2)).await;
                }
                match self.read_outcome_file(run.id) {
                    Some(o) => {
                        let state = if o.success {
                            RunState::Succeeded
                        } else if o
                            .error
                            .as_deref()
                            .is_some_and(|e| e.starts_with("timed out"))
                        {
                            RunState::TimedOut
                        } else {
                            RunState::Failed
                        };
                        tracing::info!(run = %run.id, "recovered the outcome from out/outcome.json");
                        self.finalize(run.id, None, state, o).await;
                    }
                    None => {
                        tracing::warn!(run = %run.id, "container gone without an outcome; marking the run lost");
                        self.finalize(
                            run.id,
                            None,
                            RunState::Lost,
                            RunOutcome::failed("the run was lost while the daemon was down"),
                        )
                        .await;
                    }
                }
            }
        }
        Ok(())
    }

    async fn project_of_run(&self, run: &Run) -> Result<ProjectId> {
        if let Some(t) = run.task_id {
            return Ok(self.store.get_task(t).await?.project_id);
        }
        if let Some(t) = run.thread_id {
            return Ok(self.store.get_thread(t).await?.project_id);
        }
        Err(SchedulerError::NotFound("project of run".into()))
    }

    /// Stop every live run of a task.
    pub(crate) fn stop_task_runs(
        &self,
        task: kiln_types::TaskId,
        state: RunState,
        reason: &str,
    ) -> u32 {
        let lives: Vec<Arc<LiveRun>> = lock(&self.st.live)
            .values()
            .filter(|l| l.task_id == Some(task))
            .cloned()
            .collect();
        for l in &lives {
            l.request_stop(state, reason);
        }
        lives.len() as u32
    }
}

pub(crate) fn write_json<T: Serialize>(path: &Path, v: &T) -> std::io::Result<()> {
    let tmp: PathBuf = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path)
}

/// `uid:gid` of the daemon on Linux (bind-mounted files stay ours); `None`
/// elsewhere (Docker Desktop/OrbStack map ownership themselves).
fn host_user() -> Option<String> {
    if cfg!(target_os = "linux") {
        // SAFETY: getuid/getgid cannot fail.
        let (u, g) = unsafe { (libc::getuid(), libc::getgid()) };
        Some(format!("{u}:{g}"))
    } else {
        None
    }
}
