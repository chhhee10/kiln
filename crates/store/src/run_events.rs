//! `run_events`: the bounded hot cache of run events behind log panes.

use crate::row::{RowExt, like_escape, ts};
use crate::{Result, Store};
use kiln_types::api::LogQuery;
use kiln_types::event::{LogLine, RunEvent};
use kiln_types::{RunId, TaskId, Timestamp, WorkerRole};
use sqlx::{QueryBuilder, Sqlite};

/// Default and maximum number of log lines per query.
const DEFAULT_LOG_LIMIT: u32 = 500;
const MAX_LOG_LIMIT: u32 = 5000;

/// What a log query is scoped to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogTarget {
    /// Every run of a task (all roles, all attempts).
    Task(TaskId),
    /// A single run.
    Run(RunId),
}

/// The serde `type` tag of an event kind ("log", "tool_call", ...).
fn kind_tag(e: &RunEvent) -> String {
    serde_json::to_value(&e.kind)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

impl Store {
    /// Insert run events idempotently (`INSERT OR IGNORE` on `(run_id, seq)`)
    /// and advance each run's `last_seq`. Returns the number of new rows.
    pub async fn insert_run_events(&self, events: &[(RunEvent, WorkerRole)]) -> Result<u64> {
        if events.is_empty() {
            return Ok(0);
        }
        let mut tx = self.begin_write().await?;
        let mut inserted = 0;
        for (e, role) in events {
            let (level, message) = e.kind.log_line();
            let seq = i64::try_from(e.seq).unwrap_or(i64::MAX);
            let r = sqlx::query(
                "INSERT OR IGNORE INTO run_events (run_id, seq, ts, role, level, message, kind)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(e.run_id.to_string())
            .bind(seq)
            .bind(ts(&e.ts))
            .bind(role.as_str())
            .bind(level.as_str())
            .bind(message)
            .bind(kind_tag(e))
            .execute(&mut *tx)
            .await?;
            inserted += r.rows_affected();
            sqlx::query("UPDATE runs SET last_seq = MAX(last_seq, ?) WHERE id = ?")
                .bind(seq)
                .bind(e.run_id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// Log lines for a task or run.
    ///
    /// With `after_seq` the first `limit` lines after it are returned (live
    /// tailing; `seq` is per run, so this is most useful with
    /// [`LogTarget::Run`]). Without it, the *last* `limit` lines are returned.
    /// Results are always in chronological order. `filter` is a
    /// case-insensitive substring match on the message.
    pub async fn list_log_lines(&self, target: LogTarget, q: &LogQuery) -> Result<Vec<LogLine>> {
        let limit = i64::from(q.limit.unwrap_or(DEFAULT_LOG_LIMIT).clamp(1, MAX_LOG_LIMIT));
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(
            "SELECT e.run_id, e.seq, e.ts, e.role, e.level, e.message FROM run_events e ",
        );
        match target {
            LogTarget::Task(t) => {
                qb.push("JOIN runs r ON r.id = e.run_id WHERE r.task_id = ")
                    .push_bind(t.to_string());
            }
            LogTarget::Run(r) => {
                qb.push("WHERE e.run_id = ").push_bind(r.to_string());
            }
        }
        if let Some(role) = q.role {
            qb.push(" AND e.role = ").push_bind(role.as_str());
        }
        if let Some(f) = q.filter.as_deref().filter(|f| !f.is_empty()) {
            qb.push(" AND e.message LIKE ")
                .push_bind(format!("%{}%", like_escape(f)))
                .push(" ESCAPE '\\'");
        }
        let tail = q.after_seq.is_none();
        if let Some(after) = q.after_seq {
            qb.push(" AND e.seq > ")
                .push_bind(i64::try_from(after).unwrap_or(i64::MAX));
            qb.push(" ORDER BY e.ts, e.run_id, e.seq LIMIT ")
                .push_bind(limit);
        } else {
            qb.push(" ORDER BY e.ts DESC, e.run_id DESC, e.seq DESC LIMIT ")
                .push_bind(limit);
        }
        let rows = qb.build().fetch_all(&self.pool).await?;
        let mut out = rows
            .iter()
            .map(|r| {
                Ok(LogLine {
                    run_id: r.id("run_id")?,
                    seq: r.uint("seq")?,
                    ts: r.time("ts")?,
                    role: r.enumv("role")?,
                    level: r.enumv("level")?,
                    message: r.str("message")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if tail {
            out.reverse();
        }
        Ok(out)
    }

    /// Delete cached events older than `older_than`. Returns rows deleted.
    pub async fn prune_run_events(&self, older_than: Timestamp) -> Result<u64> {
        let r = sqlx::query("DELETE FROM run_events WHERE ts < ?")
            .bind(ts(&older_than))
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }
}
