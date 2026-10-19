//! `jobs`: the persistent work queue (PRD §7.6).
//!
//! A job is `queued` until a worker claims it (`running`, `locked_at` set,
//! `attempts` incremented), then ends `done`, `failed` or `cancelled`. A
//! failed attempt may be re-queued with a delay while attempts remain, and
//! jobs whose worker died (stale `locked_at`) are re-queued on recovery.

use crate::row::{RowExt, enum_str, now, to_json, ts};
use crate::{Result, Store, StoreError};
use chrono::{Duration, Utc};
use kiln_types::{JobId, Timestamp, UserId};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Sqlite};

/// Job lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting to be claimed (possibly delayed by `run_after`).
    Queued,
    /// Claimed by a worker (`locked_at` set).
    Running,
    /// Finished successfully.
    Done,
    /// Failed with no attempts left.
    Failed,
    /// Cancelled before completion.
    Cancelled,
}

/// A job to enqueue.
#[derive(Clone, Debug)]
pub struct NewJob {
    /// Job kind: "run_role", "distill", "extract", "rebuild_packs", "nightly", "poll_prs", ...
    pub kind: String,
    pub payload: serde_json::Value,
    /// Owner for fair-share scheduling.
    pub owner_id: Option<UserId>,
    /// Higher runs first.
    pub priority: i64,
    /// Total attempts allowed (≥ 1).
    pub max_attempts: u32,
    /// Do not run before this time (`None` = now).
    pub run_after: Option<Timestamp>,
}

impl NewJob {
    /// A job runnable now with priority 0 and two attempts.
    pub fn new(kind: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
            owner_id: None,
            priority: 0,
            max_attempts: 2,
            run_after: None,
        }
    }
    /// Set the owner.
    pub fn owner(mut self, owner: UserId) -> Self {
        self.owner_id = Some(owner);
        self
    }
}

/// A queued or claimed job.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: JobId,
    pub kind: String,
    pub payload: serde_json::Value,
    pub owner_id: Option<UserId>,
    pub state: JobState,
    pub priority: i64,
    pub attempts: u32,
    pub max_attempts: u32,
    pub run_after: Timestamp,
    pub locked_at: Option<Timestamp>,
    pub last_error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

fn job_from_row(r: &SqliteRow) -> Result<Job> {
    Ok(Job {
        id: r.id("id")?,
        kind: r.str("kind")?,
        payload: r.json("payload")?,
        owner_id: r.opt_id("owner_id")?,
        state: r.enumv("state")?,
        priority: r.int("priority")?,
        attempts: r.u32v("attempts")?,
        max_attempts: r.u32v("max_attempts")?,
        run_after: r.time("run_after")?,
        locked_at: r.opt_time("locked_at")?,
        last_error: r.opt_str("last_error")?,
        created_at: r.time("created_at")?,
        updated_at: r.time("updated_at")?,
    })
}

fn push_kinds(qb: &mut QueryBuilder<Sqlite>, kinds: &[&str]) {
    if kinds.is_empty() {
        return;
    }
    qb.push(" AND kind IN (");
    let mut sep = qb.separated(", ");
    for k in kinds {
        sep.push_bind(k.to_string());
    }
    qb.push(")");
}

impl Store {
    /// Enqueue a job.
    pub async fn enqueue_job(&self, new: NewJob) -> Result<JobId> {
        if new.kind.trim().is_empty() {
            return Err(StoreError::Invalid("job kind is empty".into()));
        }
        let id = JobId::new();
        let at = now();
        let run_after = new.run_after.as_ref().map(ts).unwrap_or_else(|| at.clone());
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, owner_id, state, priority, attempts, max_attempts, run_after,
                               locked_at, last_error, created_at, updated_at)
             VALUES (?, ?, ?, ?, 'queued', ?, 0, ?, ?, NULL, NULL, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&new.kind)
        .bind(to_json(&new.payload)?)
        .bind(new.owner_id.map(|o| o.to_string()))
        .bind(new.priority)
        .bind(i64::from(new.max_attempts.max(1)))
        .bind(run_after)
        .bind(&at)
        .bind(&at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Fetch a job.
    pub async fn get_job(&self, id: JobId) -> Result<Job> {
        let row = sqlx::query("SELECT * FROM jobs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        job_from_row(&row)
    }

    /// Atomically claim the next runnable job of one of `kinds` (any kind if
    /// empty), optionally restricted to one owner. Highest priority first,
    /// then oldest `run_after`. The claimed job is `running`, has `locked_at`
    /// set and `attempts` incremented. A single `UPDATE … RETURNING`
    /// statement, so two workers can never claim the same job.
    pub async fn claim_job(
        &self,
        kinds: &[&str],
        owner_filter: Option<UserId>,
    ) -> Result<Option<Job>> {
        let at = now();
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new("UPDATE jobs SET state = 'running', locked_at = ");
        qb.push_bind(at.clone());
        qb.push(", attempts = attempts + 1, updated_at = ")
            .push_bind(at.clone());
        qb.push(" WHERE state = 'queued' AND id = (SELECT id FROM jobs WHERE state = 'queued' AND run_after <= ");
        qb.push_bind(at);
        push_kinds(&mut qb, kinds);
        if let Some(o) = owner_filter {
            qb.push(" AND owner_id = ").push_bind(o.to_string());
        }
        qb.push(" ORDER BY priority DESC, run_after, created_at, id LIMIT 1) RETURNING *");
        let row = qb.build().fetch_optional(&self.pool).await?;
        row.as_ref().map(job_from_row).transpose()
    }

    /// Mark a running job done.
    pub async fn complete_job(&self, id: JobId) -> Result<()> {
        let r = sqlx::query(
            "UPDATE jobs SET state = 'done', locked_at = NULL, updated_at = ? WHERE id = ? AND state = 'running'",
        )
        .bind(now())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return self.job_state_mismatch(id, "running").await;
        }
        Ok(())
    }

    /// Record a failed attempt. With `retry_after` and attempts remaining the
    /// job is re-queued after that delay; otherwise it becomes `failed`.
    /// Returns the resulting state.
    pub async fn fail_job(
        &self,
        id: JobId,
        error: &str,
        retry_after: Option<Duration>,
    ) -> Result<JobState> {
        let at = Utc::now();
        let retry_at = retry_after.map(|d| ts(&(at + d)));
        let row = sqlx::query(
            "UPDATE jobs SET
                state = CASE WHEN ? IS NOT NULL AND attempts < max_attempts THEN 'queued' ELSE 'failed' END,
                run_after = CASE WHEN ? IS NOT NULL AND attempts < max_attempts THEN ? ELSE run_after END,
                locked_at = NULL, last_error = ?, updated_at = ?
             WHERE id = ? AND state = 'running'
             RETURNING state",
        )
        .bind(&retry_at)
        .bind(&retry_at)
        .bind(&retry_at)
        .bind(error)
        .bind(ts(&at))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => r.enumv("state"),
            None => self
                .job_state_mismatch(id, "running")
                .await
                .map(|_| JobState::Failed),
        }
    }

    /// Cancel one queued or running job. Returns whether it changed.
    pub async fn cancel_job(&self, id: JobId) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE jobs SET state = 'cancelled', locked_at = NULL, updated_at = ?
             WHERE id = ? AND state IN ('queued', 'running')",
        )
        .bind(now())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Cancel every *queued* job (optionally of `kind`) whose JSON payload has
    /// `payload_key == value` (e.g. `("task_id", "<uuid>")`). Returns the
    /// number cancelled.
    pub async fn cancel_jobs_for(
        &self,
        kind: Option<&str>,
        payload_key: &str,
        value: &str,
    ) -> Result<u64> {
        if payload_key.is_empty()
            || !payload_key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(StoreError::Invalid(
                "payload key must be [A-Za-z0-9_]+".into(),
            ));
        }
        let r = sqlx::query(
            "UPDATE jobs SET state = 'cancelled', updated_at = ?
             WHERE state = 'queued' AND (? IS NULL OR kind = ?) AND json_extract(payload, ?) = ?",
        )
        .bind(now())
        .bind(kind)
        .bind(kind)
        .bind(format!("$.{payload_key}"))
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Re-queue running jobs locked longer than `older_than` ago (their worker
    /// died). Jobs out of attempts become `failed`. Returns rows changed.
    pub async fn requeue_stale_jobs(&self, older_than: Duration) -> Result<u64> {
        let at = Utc::now();
        let r = sqlx::query(
            "UPDATE jobs SET
                state = CASE WHEN attempts < max_attempts THEN 'queued' ELSE 'failed' END,
                last_error = COALESCE(last_error, 'worker lost'),
                locked_at = NULL, updated_at = ?
             WHERE state = 'running' AND locked_at < ?",
        )
        .bind(ts(&at))
        .bind(ts(&(at - older_than)))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Queued jobs (optionally of one kind) in claim order.
    pub async fn queued_jobs(&self, kind: Option<&str>) -> Result<Vec<Job>> {
        let rows = sqlx::query(
            "SELECT * FROM jobs WHERE state = 'queued' AND (? IS NULL OR kind = ?)
             ORDER BY priority DESC, run_after, created_at, id",
        )
        .bind(kind)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(job_from_row).collect()
    }

    /// Jobs in `state` (optionally of one kind), oldest first.
    pub async fn jobs_in_state(&self, state: JobState, kind: Option<&str>) -> Result<Vec<Job>> {
        let rows = sqlx::query("SELECT * FROM jobs WHERE state = ? AND (? IS NULL OR kind = ?) ORDER BY created_at, id")
            .bind(enum_str(&state))
            .bind(kind)
            .bind(kind)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(job_from_row).collect()
    }

    /// Delete finished jobs last updated before `older_than`.
    pub async fn prune_finished_jobs(&self, older_than: Timestamp) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM jobs WHERE state IN ('done', 'failed', 'cancelled') AND updated_at < ?",
        )
        .bind(ts(&older_than))
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    async fn job_state_mismatch(&self, id: JobId, expected: &str) -> Result<()> {
        let job = self.get_job(id).await?;
        Err(StoreError::Conflict(format!(
            "job is {}, expected {expected}",
            enum_str(&job.state)
        )))
    }
}
