//! Raw queries the store crate does not (yet) provide. Each one is written
//! against `store.pool()` with the store's storage conventions (RFC 3339
//! microsecond UTC timestamps, snake_case enums) and is a candidate to move
//! into `kiln-store`.

use crate::error::{Result, SchedulerError};
use chrono::{SecondsFormat, Utc};
use kiln_store::Store;
use kiln_types::{RunId, Task, TaskId, TaskState, ThreadId, UserId};
use std::collections::HashSet;

/// Current time in the store's canonical format.
pub(crate) fn now_ts() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// How a transition changes the loop counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopChange {
    Increment,
    Set(u32),
}

/// Extra changes applied atomically with a transition.
#[derive(Clone, Debug, Default)]
pub(crate) struct Ext {
    pub loop_change: Option<LoopChange>,
    /// Appended to the task's pending feedback.
    pub feedback: Vec<String>,
    pub meta: Option<String>,
}

fn is_halted(s: TaskState) -> bool {
    matches!(
        s,
        TaskState::Escalated | TaskState::Failed | TaskState::Cancelled
    )
}

/// Same rule as the store's `transition_task`.
fn next_last_active(prev: Option<TaskState>, from: TaskState, to: TaskState) -> Option<TaskState> {
    if is_halted(to) {
        if is_halted(from) { prev } else { Some(from) }
    } else if to.is_active() {
        Some(to)
    } else {
        prev
    }
}

/// Compare-and-swap transition plus loop-counter / feedback / meta changes
/// in ONE transaction (store candidate: `transition_task_ext`). Semantics of
/// `from`/`to` match `Store::transition_task`: `Invalid` for a disallowed
/// edge, `Conflict` when the task is no longer in `from`.
pub(crate) async fn transition_ext(
    store: &Store,
    id: TaskId,
    from: TaskState,
    to: TaskState,
    reason: &str,
    actor: Option<UserId>,
    ext: &Ext,
) -> Result<Task> {
    if !from.can_transition(to) {
        return Err(SchedulerError::Invalid(format!(
            "transition {} → {} is not allowed",
            from.as_str(),
            to.as_str()
        )));
    }
    let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, Option<String>, i64, i64, String)> = sqlx::query_as(
        "SELECT state, last_active, progress, loop_count, feedback_json FROM tasks WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((state, last_active, progress, loop_count, feedback_json)) = row else {
        return Err(SchedulerError::NotFound("task".into()));
    };
    if state != from.as_str() {
        return Err(SchedulerError::Conflict(format!(
            "task is {state}, expected {}",
            from.as_str()
        )));
    }
    let prev = last_active.as_deref().and_then(TaskState::parse);
    let last_active = next_last_active(prev, from, to);
    let progress = if to.is_active() {
        0
    } else if matches!(to, TaskState::Ready | TaskState::Merged) {
        100
    } else {
        progress
    };
    let loop_count = match ext.loop_change {
        None => loop_count,
        Some(LoopChange::Increment) => loop_count + 1,
        Some(LoopChange::Set(n)) => i64::from(n),
    };
    let mut fb: Vec<String> = serde_json::from_str(&feedback_json).unwrap_or_default();
    fb.extend(
        ext.feedback
            .iter()
            .filter(|f| !f.trim().is_empty())
            .cloned(),
    );
    let at = now_ts();
    let r = sqlx::query(
        "UPDATE tasks SET state = ?, last_active = ?, progress = ?, loop_count = ?, feedback_json = ?,
                meta = COALESCE(?, meta), updated_at = ?
         WHERE id = ? AND state = ?",
    )
    .bind(to.as_str())
    .bind(last_active.map(|s| s.as_str()))
    .bind(progress)
    .bind(loop_count)
    .bind(serde_json::to_string(&fb)?)
    .bind(ext.meta.as_deref())
    .bind(&at)
    .bind(id.to_string())
    .bind(from.as_str())
    .execute(&mut *tx)
    .await?;
    if r.rows_affected() == 0 {
        return Err(SchedulerError::Conflict(
            "task changed state concurrently".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO task_transitions (task_id, from_state, to_state, reason, actor, at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(reason)
    .bind(actor.map(|a| a.to_string()))
    .bind(&at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(store.get_task(id).await?)
}

/// Push feedback and change the loop counter without a state change
/// (feedback to a live Builder). Store candidate.
pub(crate) async fn bump_loop_with_feedback(
    store: &Store,
    id: TaskId,
    expected: TaskState,
    feedback: &str,
) -> Result<Task> {
    let mut tx = store.pool().begin_with("BEGIN IMMEDIATE").await?;
    let fb: Option<(String, String)> =
        sqlx::query_as("SELECT state, feedback_json FROM tasks WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
    let Some((state, fb)) = fb else {
        return Err(SchedulerError::NotFound("task".into()));
    };
    if state != expected.as_str() {
        return Err(SchedulerError::Conflict(format!("task is {state}")));
    }
    let mut v: Vec<String> = serde_json::from_str(&fb).unwrap_or_default();
    v.push(feedback.to_string());
    sqlx::query("UPDATE tasks SET loop_count = loop_count + 1, feedback_json = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&v)?)
        .bind(now_ts())
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(store.get_task(id).await?)
}

/// Record the per-run token hash before the container exists (the store's
/// `set_run_container` sets both at once). Store candidate.
pub(crate) async fn set_run_token_hash(store: &Store, run: RunId, hash: &str) -> Result<()> {
    sqlx::query("UPDATE runs SET token_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(run.to_string())
        .execute(store.pool())
        .await?;
    Ok(())
}

/// Set a run's retry counter (a retry run inherits `previous + 1`). Store candidate.
pub(crate) async fn set_run_retries(store: &Store, run: RunId, retries: u32) -> Result<()> {
    sqlx::query("UPDATE runs SET retries = ? WHERE id = ?")
        .bind(i64::from(retries))
        .bind(run.to_string())
        .execute(store.pool())
        .await?;
    Ok(())
}

/// Sequence numbers already stored in `run_events` for a run. Store candidate.
pub(crate) async fn stored_seqs(store: &Store, run: RunId) -> Result<HashSet<u64>> {
    let v: Vec<i64> = sqlx::query_scalar("SELECT seq FROM run_events WHERE run_id = ?")
        .bind(run.to_string())
        .fetch_all(store.pool())
        .await?;
    Ok(v.into_iter().map(|s| s.max(0) as u64).collect())
}

/// Drafting threads whose latest message is from the user (a planner turn
/// is owed). Store candidate.
pub(crate) async fn threads_awaiting_planner(store: &Store) -> Result<Vec<ThreadId>> {
    let v: Vec<String> = sqlx::query_scalar(
        "SELECT t.id FROM threads t
         WHERE t.status = 'drafting'
           AND (SELECT m.author FROM messages m WHERE m.thread_id = t.id
                ORDER BY m.created_at DESC, m.rowid DESC LIMIT 1) = 'user'",
    )
    .fetch_all(store.pool())
    .await?;
    Ok(v.iter().filter_map(|s| s.parse().ok()).collect())
}

/// Average wall-clock seconds of recent finished task runs (queue ETA).
/// Store candidate.
pub(crate) async fn avg_run_secs(store: &Store) -> Result<Option<f64>> {
    let v: Option<f64> = sqlx::query_scalar(
        "SELECT AVG((julianday(ended_at) - julianday(started_at)) * 86400.0) FROM
           (SELECT started_at, ended_at FROM runs
            WHERE task_id IS NOT NULL AND started_at IS NOT NULL AND ended_at IS NOT NULL
            ORDER BY created_at DESC LIMIT 50)",
    )
    .fetch_one(store.pool())
    .await?;
    Ok(v)
}
