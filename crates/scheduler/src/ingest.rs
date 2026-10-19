//! Ingest (docs/SCHEDULER.md "ingest", PRD §7.9 "spool & flush").
//!
//! For every finished run: reconcile `<run_dir>/spool.jsonl` against the
//! stored `run_events` seqs (anything the socket missed is inserted now),
//! flush the spool to L0 (idempotent), parse the harness's native
//! transcript into L0, redact everything, `mark_ingested`, publish
//! `RunIngested`, remove the container and enqueue the `distill` job. The
//! run folder itself is never deleted before the run is ingested (the
//! nightly job removes old ingested folders).

use crate::Scheduler;
use crate::error::Result;
use crate::state::lock;
use crate::util::redact_event;
use chrono::Utc;
use kiln_events::Event;
use kiln_store::NewJob;
use kiln_types::event::RunEvent;
use kiln_types::{ProjectId, Run, RunId, WorkerRole};
use std::sync::Arc;
use std::time::Duration;

/// Transcript events get seqs above this so they never collide with the
/// runner's spool seqs in L0's `(run_id, seq)` dedupe.
const TRANSCRIPT_SEQ_BASE: u64 = 1 << 40;
/// Sweep for un-ingested runs this often (covers anything missed).
const SWEEP_EVERY: Duration = Duration::from_secs(30);

impl Scheduler {
    /// Queue a finished run for ingest (deduplicated).
    pub(crate) fn queue_ingest(&self, run: RunId) {
        if !lock(&self.st.ingesting).insert(run) {
            return;
        }
        let Some(s) = self.arc() else { return };
        self.spawn_bg(async move {
            // Ingest serially-ish: a small delay lets the runner's last
            // spool writes and the container exit settle.
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Err(e) = s.ingest_run(run).await {
                tracing::warn!(run = %run, error = %e, "ingest failed; will retry on the next sweep");
            }
            lock(&s.st.ingesting).remove(&run);
        });
    }

    pub(crate) async fn ingest_loop(self: Arc<Self>) {
        let mut shutdown = self.st.shutdown.subscribe();
        loop {
            match self.store.list_uningested_runs().await {
                Ok(runs) => {
                    for r in runs {
                        if !lock(&self.st.live).contains_key(&r.id) {
                            self.queue_ingest(r.id);
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "ingest sweep failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(SWEEP_EVERY) => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    async fn project_for(&self, run: &Run) -> Option<ProjectId> {
        if let Some(m) = self.read_run_meta(run.id)
            && m.project_id.is_some()
        {
            return m.project_id;
        }
        if let Some(t) = run.task_id {
            return self.store.get_task(t).await.ok().map(|t| t.project_id);
        }
        if let Some(t) = run.thread_id {
            return self.store.get_thread(t).await.ok().map(|t| t.project_id);
        }
        None
    }

    /// Read the run's spool (torn/garbage lines are skipped).
    pub(crate) fn read_spool(&self, run: RunId) -> Vec<RunEvent> {
        let p = self.run_dir(run).join("spool.jsonl");
        let Ok(text) = std::fs::read_to_string(p) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str::<RunEvent>(l.trim()).ok())
            .filter(|e| e.run_id == run)
            .collect()
    }

    /// Remove every managed container labelled with this run (worker and
    /// sidecars). Matching on the label, not the stored id, so a recycled
    /// id can never take down another run's container.
    pub(crate) async fn remove_run_containers(&self, run: RunId) {
        match self.runtime.list_managed().await {
            Ok(list) => {
                for c in list.into_iter().filter(|c| c.run_id() == Some(run)) {
                    if let Err(e) = self.runtime.remove(&c.id).await {
                        tracing::debug!(run = %run, error = %e, "container remove failed");
                    }
                }
            }
            Err(e) => tracing::debug!(error = %e, "cannot list containers"),
        }
    }

    pub(crate) async fn ingest_run(self: &Arc<Self>, id: RunId) -> Result<()> {
        let run = self.store.get_run(id).await?;
        if run.ingested || !run.state.is_finished() || lock(&self.st.live).contains_key(&id) {
            return Ok(());
        }
        let redactor = match self.vault.dispatch_env(run.owner_id).await {
            Ok(s) => s.redactor,
            Err(_) => kiln_vault::Redactor::default(),
        };
        let project = self.project_for(&run).await;

        // 1. Spool ↔ run_events reconciliation.
        let spool: Vec<RunEvent> = self
            .read_spool(id)
            .iter()
            .map(|e| redact_event(e, &redactor))
            .collect();
        let stored = crate::sql::stored_seqs(&self.store, id).await?;
        let missing: Vec<(RunEvent, WorkerRole)> = spool
            .iter()
            .filter(|e| !stored.contains(&e.seq))
            .map(|e| (e.clone(), run.role))
            .collect();
        if !missing.is_empty() {
            let n = self.store.insert_run_events(&missing).await?;
            tracing::info!(run = %id, recovered = n, "reconciled events from the spool");
        }

        // 2. Spool → L0 (idempotent on seq).
        let r = redactor.clone();
        let redact = move |s: &str| r.scrub(s);
        if !spool.is_empty() {
            self.memory
                .ingest_l0(&run, project, &spool, &redact)
                .await?;
        }

        // 3. Native transcript → L0.
        if let Some(h) = self.harnesses.get(&run.harness)
            && let Some(key) = h.transcript_key()
        {
            let dir = self.run_dir(id).join("transcripts").join(key);
            if dir.exists() {
                let kinds = tokio::task::spawn_blocking(move || h.parse_transcript(&dir))
                    .await
                    .unwrap_or_default();
                let ts = run.ended_at.unwrap_or_else(Utc::now);
                let events: Vec<RunEvent> = kinds
                    .into_iter()
                    .enumerate()
                    .map(|(i, kind)| {
                        redact_event(
                            &RunEvent {
                                run_id: id,
                                seq: TRANSCRIPT_SEQ_BASE + i as u64,
                                ts,
                                kind,
                            },
                            &redactor,
                        )
                    })
                    .collect();
                if !events.is_empty() {
                    self.memory
                        .ingest_l0(&run, project, &events, &redact)
                        .await?;
                }
            }
        }

        // 4. Done: mark, publish, clean the container, distill.
        self.store.mark_ingested(id).await?;
        self.bus.publish(Event::RunIngested {
            run_id: id,
            owner_id: run.owner_id,
        });
        if let Ok(r) = self.store.get_run(id).await {
            self.publish_run(&r);
        }
        self.remove_run_containers(id).await;
        if !matches!(run.role, WorkerRole::Distiller)
            && run.state != kiln_types::RunState::Cancelled
        {
            let job = NewJob::new("distill", serde_json::json!({ "run_id": id.to_string() }))
                .owner(run.owner_id);
            self.store.enqueue_job(job).await?;
        }
        tracing::debug!(run = %id, "run ingested");
        Ok(())
    }
}
