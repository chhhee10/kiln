//! `runs`: one execution of one worker role.

use crate::row::{RowExt, now, to_json, ts};
use crate::{Result, Store, StoreError};
use chrono::{Datelike, TimeZone, Utc};
use kiln_types::protocol::RunOutcome;
use kiln_types::{Run, RunId, RunState, TaskId, ThreadId, UserId, WorkerRole};
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Sqlite};

/// A run to create (state `Pending`).
#[derive(Clone, Debug)]
pub struct NewRun {
    pub task_id: Option<TaskId>,
    pub thread_id: Option<ThreadId>,
    pub owner_id: UserId,
    pub role: WorkerRole,
    pub harness: String,
    pub model: String,
    /// 1-based attempt of the task this run belongs to.
    pub attempt: u32,
}

pub(crate) fn run_from_row(r: &SqliteRow) -> Result<Run> {
    Ok(Run {
        id: r.id("id")?,
        task_id: r.opt_id("task_id")?,
        thread_id: r.opt_id("thread_id")?,
        owner_id: r.id("owner_id")?,
        role: r.enumv("role")?,
        harness: r.str("harness")?,
        model: r.str("model")?,
        attempt: r.u32v("attempt")?,
        container_id: r.opt_str("container_id")?,
        state: r.enumv("state")?,
        progress: r.int("progress")?.clamp(0, 100) as u8,
        started_at: r.opt_time("started_at")?,
        ended_at: r.opt_time("ended_at")?,
        ingested: r.flag("ingested")?,
        cost_usd: r.float("cost_usd")?,
        tokens_in: r.uint("tokens_in")?,
        tokens_out: r.uint("tokens_out")?,
        error: r.opt_str("error")?,
        retries: r.u32v("retries")?,
    })
}

fn sat_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Store {
    /// Create a run in state `Pending`.
    pub async fn create_run(&self, new: NewRun) -> Result<Run> {
        if new.task_id.is_none() && new.thread_id.is_none() && new.role != WorkerRole::Distiller {
            return Err(StoreError::Invalid("a run needs a task or a thread".into()));
        }
        let id = RunId::new();
        sqlx::query(
            "INSERT INTO runs (id, task_id, thread_id, owner_id, role, harness, model, attempt, state, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)",
        )
        .bind(id.to_string())
        .bind(new.task_id.map(|t| t.to_string()))
        .bind(new.thread_id.map(|t| t.to_string()))
        .bind(new.owner_id.to_string())
        .bind(new.role.as_str())
        .bind(&new.harness)
        .bind(&new.model)
        .bind(i64::from(new.attempt.max(1)))
        .bind(now())
        .execute(&self.pool)
        .await?;
        self.get_run(id).await
    }

    /// Fetch a run.
    pub async fn get_run(&self, id: RunId) -> Result<Run> {
        let row = sqlx::query("SELECT * FROM runs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        run_from_row(&row)
    }

    /// Runs of a task, oldest first.
    pub async fn list_runs_for_task(&self, task: TaskId) -> Result<Vec<Run>> {
        let rows = sqlx::query("SELECT * FROM runs WHERE task_id = ? ORDER BY created_at, id")
            .bind(task.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(run_from_row).collect()
    }

    /// Runs of a planner thread, oldest first.
    pub async fn list_runs_for_thread(&self, thread: ThreadId) -> Result<Vec<Run>> {
        let rows = sqlx::query("SELECT * FROM runs WHERE thread_id = ? ORDER BY created_at, id")
            .bind(thread.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(run_from_row).collect()
    }

    /// Runs in any of `states`, oldest first (crash recovery, capacity view).
    pub async fn list_runs_in_states(&self, states: &[RunState]) -> Result<Vec<Run>> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM runs WHERE state IN (");
        let mut sep = qb.separated(", ");
        for s in states {
            sep.push_bind(s.as_str());
        }
        qb.push(") ORDER BY created_at, id");
        let rows = qb.build().fetch_all(&self.pool).await?;
        rows.iter().map(run_from_row).collect()
    }

    /// Finished runs whose events are not yet fully flushed to L0.
    pub async fn list_uningested_runs(&self) -> Result<Vec<Run>> {
        let rows = sqlx::query(
            "SELECT * FROM runs WHERE ingested = 0 AND state NOT IN ('pending', 'starting', 'running')
             ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(run_from_row).collect()
    }

    /// Set a run's state (and error). Stamps `started_at` on the first move
    /// into `Starting`/`Running` and `ended_at` on a finished state.
    pub async fn update_run_state(
        &self,
        id: RunId,
        state: RunState,
        error: Option<&str>,
    ) -> Result<Run> {
        let at = now();
        let starting = matches!(state, RunState::Starting | RunState::Running);
        let r = sqlx::query(
            "UPDATE runs SET state = ?,
                error = COALESCE(?, error),
                started_at = CASE WHEN ? AND started_at IS NULL THEN ? ELSE started_at END,
                ended_at = CASE WHEN ? THEN COALESCE(ended_at, ?) ELSE ended_at END
             WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(error)
        .bind(starting)
        .bind(&at)
        .bind(state.is_finished())
        .bind(&at)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        self.get_run(id).await
    }

    /// Record the container and the sha256 of the per-run socket token.
    pub async fn set_run_container(
        &self,
        id: RunId,
        container_id: &str,
        token_hash: &str,
    ) -> Result<()> {
        let r = sqlx::query("UPDATE runs SET container_id = ?, token_hash = ? WHERE id = ?")
            .bind(container_id)
            .bind(token_hash)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// The per-run token hash (for runner handshake verification).
    pub async fn run_token_hash(&self, id: RunId) -> Result<Option<String>> {
        let v: Option<Option<String>> =
            sqlx::query_scalar("SELECT token_hash FROM runs WHERE id = ?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        v.ok_or(StoreError::NotFound)
    }

    /// Finish a run with its outcome. Usage in the outcome is authoritative
    /// when non-zero; otherwise the totals accumulated via
    /// [`Store::add_run_usage`] are kept. `state` must be a finished state.
    pub async fn finish_run(
        &self,
        id: RunId,
        state: RunState,
        outcome: &RunOutcome,
    ) -> Result<Run> {
        if !state.is_finished() {
            return Err(StoreError::Invalid(format!(
                "{} is not a finished run state",
                state.as_str()
            )));
        }
        let u = &outcome.usage;
        let has_usage = u.tokens_in > 0 || u.tokens_out > 0 || u.cost_usd > 0.0;
        let at = now();
        let r = sqlx::query(
            "UPDATE runs SET state = ?, ended_at = COALESCE(ended_at, ?),
                started_at = COALESCE(started_at, ?),
                outcome_json = ?, error = COALESCE(?, error),
                tokens_in = CASE WHEN ? THEN ? ELSE tokens_in END,
                tokens_out = CASE WHEN ? THEN ? ELSE tokens_out END,
                cost_usd = CASE WHEN ? THEN ? ELSE cost_usd END,
                last_seq = MAX(last_seq, ?),
                progress = CASE WHEN ? THEN 100 ELSE progress END
             WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(&at)
        .bind(&at)
        .bind(to_json(outcome)?)
        .bind(&outcome.error)
        .bind(has_usage)
        .bind(sat_i64(u.tokens_in))
        .bind(has_usage)
        .bind(sat_i64(u.tokens_out))
        .bind(has_usage)
        .bind(u.cost_usd)
        .bind(sat_i64(outcome.last_seq))
        .bind(state == RunState::Succeeded)
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        self.get_run(id).await
    }

    /// The stored outcome of a finished run.
    pub async fn get_run_outcome(&self, id: RunId) -> Result<Option<RunOutcome>> {
        let row = sqlx::query("SELECT outcome_json FROM runs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        row.opt_json("outcome_json")
    }

    /// Latest finished run of `role` for a task, if any.
    pub async fn latest_run_for_role(&self, task: TaskId, role: WorkerRole) -> Result<Option<Run>> {
        let row = sqlx::query("SELECT * FROM runs WHERE task_id = ? AND role = ? ORDER BY created_at DESC, id DESC LIMIT 1")
            .bind(task.to_string())
            .bind(role.as_str())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Mark a run's events as fully flushed to L0.
    pub async fn mark_ingested(&self, id: RunId) -> Result<()> {
        let r = sqlx::query("UPDATE runs SET ingested = 1 WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Add incremental usage (from streamed `Usage` events).
    pub async fn add_run_usage(
        &self,
        id: RunId,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd: f64,
    ) -> Result<()> {
        let r = sqlx::query(
            "UPDATE runs SET tokens_in = tokens_in + ?, tokens_out = tokens_out + ?, cost_usd = cost_usd + ? WHERE id = ?",
        )
        .bind(sat_i64(tokens_in))
        .bind(sat_i64(tokens_out))
        .bind(cost_usd.max(0.0))
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Set a run's progress (0–100).
    pub async fn set_run_progress(&self, id: RunId, progress: u8) -> Result<()> {
        let r = sqlx::query("UPDATE runs SET progress = ? WHERE id = ?")
            .bind(i64::from(progress.min(100)))
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Consume one retry of a run slot; returns the new retry count.
    pub async fn increment_run_retries(&self, id: RunId) -> Result<u32> {
        let v: Option<i64> = sqlx::query_scalar(
            "UPDATE runs SET retries = retries + 1 WHERE id = ? RETURNING retries",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        v.map(|n| n.clamp(0, u32::MAX as i64) as u32)
            .ok_or(StoreError::NotFound)
    }

    /// Model spend of `user` in the current calendar month (UTC), in USD.
    pub async fn month_spend(&self, user: UserId) -> Result<f64> {
        let n = Utc::now();
        let start = Utc
            .with_ymd_and_hms(n.year(), n.month(), 1, 0, 0, 0)
            .single()
            .ok_or_else(|| StoreError::Invalid("cannot compute month start".into()))?;
        let v: f64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM runs WHERE owner_id = ? AND created_at >= ?",
        )
        .bind(user.to_string())
        .bind(ts(&start))
        .fetch_one(&self.pool)
        .await?;
        Ok(v)
    }

    /// Number of runs of `user` currently pending/starting/running.
    pub async fn count_live_runs(&self, user: Option<UserId>) -> Result<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM runs WHERE state IN ('pending', 'starting', 'running') AND (? IS NULL OR owner_id = ?)",
        )
        .bind(user.map(|u| u.to_string()))
        .bind(user.map(|u| u.to_string()))
        .fetch_one(&self.pool)
        .await?;
        Ok(n.max(0) as u64)
    }
}
