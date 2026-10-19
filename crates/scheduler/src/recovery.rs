//! Crash recovery on [`Scheduler::start`](crate::Scheduler::start)
//! (docs/SCHEDULER.md "Crash recovery", PRD §7.6):
//!
//! 1. re-queue stale jobs;
//! 2. runs in Starting/Running: container alive → re-attach (new socket
//!    listener at the same path; the runner reconnects and resends from its
//!    spool); gone → `out/outcome.json` if written, else `Lost` (the driver
//!    retries the role once, else escalates);
//! 3. tasks in Preparing → Queued (preparation is idempotent);
//! 4. managed containers without a live run → removed once ingested;
//! 5. finished but un-ingested runs → ingest;
//! 6. planner threads still owed a reply → planner turn.
//!
//! Drivers for every in-flight task are then (re)started by the
//! dispatcher's watchdog, and each resumes from the database state.

use crate::Scheduler;
use crate::error::Result;
use crate::sql::Ext;
use crate::util::meta;
use kiln_store::TaskFilter;
use kiln_types::{RunState, TaskState};
use std::sync::Arc;

impl Scheduler {
    pub(crate) async fn recover(self: &Arc<Self>) -> Result<()> {
        // 1. Jobs whose worker died with the old daemon.
        let n = self
            .store
            .requeue_stale_jobs(chrono::Duration::zero())
            .await?;
        if n > 0 {
            tracing::info!(jobs = n, "re-queued stale jobs");
        }

        // 2. Live runs.
        let live = self
            .store
            .list_runs_in_states(&[RunState::Starting, RunState::Running])
            .await?;
        for run in &live {
            if let Err(e) = self.attach(run).await {
                tracing::warn!(run = %run.id, error = %e, "recovery: could not attach; marking lost");
                self.finalize(
                    run.id,
                    None,
                    RunState::Lost,
                    kiln_types::protocol::RunOutcome::failed(format!("recovery failed: {e}")),
                )
                .await;
            }
        }
        // Pending runs of tasks/threads that are no longer active: retire.
        for run in self.store.list_runs_in_states(&[RunState::Pending]).await? {
            let keep = match run.task_id {
                Some(t) => self
                    .store
                    .get_task(t)
                    .await
                    .map(|t| t.state.is_active())
                    .unwrap_or(false),
                None => run.thread_id.is_some(),
            };
            if !keep {
                let _ = self
                    .store
                    .finish_run(
                        run.id,
                        RunState::Cancelled,
                        &kiln_types::protocol::RunOutcome::failed("superseded"),
                    )
                    .await;
            }
        }

        // 3. Preparation is idempotent: send those tasks back to the queue.
        let preparing = self
            .store
            .list_tasks(TaskFilter {
                states: vec![TaskState::Preparing],
                ..Default::default()
            })
            .await?;
        for t in preparing {
            match self
                .transition(
                    &t,
                    TaskState::Queued,
                    "daemon restarted during preparation",
                    None,
                    Ext {
                        meta: Some(meta::QUEUED.into()),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => {}
                Err(e) if e.is_conflict() => {}
                Err(e) => tracing::warn!(task = %t.key, error = %e, "recovery: could not re-queue"),
            }
        }

        // 4. Orphaned containers (no live run row). Un-ingested runs keep
        // theirs until ingest removes it.
        match self.runtime.list_managed().await {
            Ok(list) => {
                for c in list {
                    let Some(rid) = c.run_id() else {
                        let _ = self.runtime.remove(&c.id).await;
                        continue;
                    };
                    let orphan = match self.store.get_run(rid).await {
                        Ok(r) => {
                            r.state.is_finished() && (r.ingested || c.role() == Some("service"))
                        }
                        Err(e) => e.is_not_found(),
                    };
                    if orphan {
                        tracing::info!(container = %c.id, run = %rid, "removing orphaned container");
                        let _ = self.runtime.remove(&c.id).await;
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "recovery: cannot list managed containers"),
        }

        // 5. Finished but not ingested.
        for r in self.store.list_uningested_runs().await? {
            self.queue_ingest(r.id);
        }

        // 6. Planner turns owed (the live planner run, if any, is re-attached
        // by the turn itself).
        for t in crate::sql::threads_awaiting_planner(&self.store).await? {
            self.enqueue_planner(t);
        }
        tracing::info!(reattached = live.len(), "crash recovery complete");
        Ok(())
    }

    pub(crate) fn spawn_loops(self: &Arc<Self>) {
        let s = self.clone();
        self.spawn_bg(async move { s.dispatch_loop().await });
        let s = self.clone();
        self.spawn_bg(async move { s.ingest_loop().await });
        let s = self.clone();
        self.spawn_bg(async move { s.jobs_loop().await });
        let s = self.clone();
        self.spawn_bg(async move { s.machine_loop().await });
    }
}
