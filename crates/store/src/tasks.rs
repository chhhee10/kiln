//! `tasks` and `task_transitions`: the board cards and their state machine.

use crate::row::{RowExt, now, to_json};
use crate::{Result, Store, StoreError};
use kiln_types::plan::WorkerChoice;
use kiln_types::{PlanId, ProjectId, Task, TaskId, TaskState, Timestamp, UserId, WorkerRole};
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};
use std::collections::{BTreeMap, HashMap};

/// One task to create from an approved plan.
#[derive(Clone, Debug)]
pub struct NewTask {
    /// Id inside the plan spec ("t1").
    pub spec_id: String,
    pub title: String,
    pub description: String,
    pub acceptance: Vec<String>,
    pub files_hint: Vec<String>,
    /// Spec ids of tasks this one depends on (same batch, or already created
    /// for the same plan).
    pub depends_on: Vec<String>,
    /// Resolved harness/model per pipeline role.
    pub workers: BTreeMap<WorkerRole, WorkerChoice>,
}

/// Filter for [`Store::list_tasks`]. Empty vectors mean "any".
#[derive(Clone, Debug, Default)]
pub struct TaskFilter {
    pub project: Option<ProjectId>,
    pub plan: Option<PlanId>,
    pub owners: Vec<UserId>,
    pub states: Vec<TaskState>,
    pub limit: Option<u32>,
}

/// One persisted state transition.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskTransition {
    pub id: i64,
    pub task_id: TaskId,
    pub from: TaskState,
    pub to: TaskState,
    pub reason: String,
    /// The user who caused it, or `None` for the system.
    pub actor: Option<UserId>,
    pub at: Timestamp,
}

pub(crate) fn task_from_row(r: &SqliteRow) -> Result<Task> {
    Ok(Task {
        id: r.id("id")?,
        plan_id: r.id("plan_id")?,
        project_id: r.id("project_id")?,
        owner_id: r.id("owner_id")?,
        number: r.uint("number")?,
        key: r.str("key")?,
        spec_id: r.str("spec_id")?,
        title: r.str("title")?,
        description: r.str("description")?,
        acceptance: r.json("acceptance_json")?,
        files_hint: r.json("files_hint_json")?,
        depends_on: r.json("depends_on_json")?,
        workers: r.json("workers_json")?,
        state: r.enumv("state")?,
        last_active: r.opt_enumv("last_active")?,
        loop_count: r.u32v("loop_count")?,
        loop_cap: r.u32v("loop_cap")?,
        branch: r.opt_str("branch")?,
        pr_url: r.opt_str("pr_url")?,
        pr_number: r.opt_int("pr_number")?.map(|n| n.max(0) as u64),
        progress: r.int("progress")?.clamp(0, 100) as u8,
        meta: r.str("meta")?,
        created_at: r.time("created_at")?,
        updated_at: r.time("updated_at")?,
    })
}

async fn fetch_task(conn: &mut SqliteConnection, id: TaskId) -> Result<Task> {
    let row = sqlx::query("SELECT * FROM tasks WHERE id = ?")
        .bind(id.to_string())
        .fetch_optional(conn)
        .await?
        .ok_or(StoreError::NotFound)?;
    task_from_row(&row)
}

fn is_halted(s: TaskState) -> bool {
    matches!(
        s,
        TaskState::Escalated | TaskState::Failed | TaskState::Cancelled
    )
}

/// `last_active` after `from → to` (DECISIONS.md board mapping): moving into
/// a halted state remembers the stage it halted in; moving into an active
/// stage records that stage; anything else keeps the previous value.
fn next_last_active(prev: Option<TaskState>, from: TaskState, to: TaskState) -> Option<TaskState> {
    if is_halted(to) {
        if is_halted(from) { prev } else { Some(from) }
    } else if to.is_active() {
        Some(to)
    } else {
        prev
    }
}

impl Store {
    /// Create every task of an approved plan in one transaction.
    ///
    /// Allocates consecutive per-project numbers, builds keys like `FP-142`,
    /// resolves `depends_on` spec ids to task ids, and sets every task to
    /// `Queued` with the project's loop cap.
    pub async fn create_tasks_for_plan(
        &self,
        plan: PlanId,
        tasks: Vec<NewTask>,
    ) -> Result<Vec<Task>> {
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.begin_write().await?;
        let prow = sqlx::query(
            "SELECT p.project_id, p.owner_id, pr.key, pr.loop_cap, pr.next_task_number
             FROM plans p JOIN projects pr ON pr.id = p.project_id WHERE p.id = ?",
        )
        .bind(plan.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        let project_id: ProjectId = prow.id("project_id")?;
        let owner_id: UserId = prow.id("owner_id")?;
        let key = prow.str("key")?;
        let loop_cap = prow.int("loop_cap")?;
        let first_number = prow.int("next_task_number")?;

        // spec id → task id, including tasks already created for this plan.
        let mut ids: HashMap<String, TaskId> = HashMap::new();
        let existing = sqlx::query("SELECT id, spec_id FROM tasks WHERE plan_id = ?")
            .bind(plan.to_string())
            .fetch_all(&mut *tx)
            .await?;
        for r in &existing {
            ids.insert(r.str("spec_id")?, r.id("id")?);
        }
        let mut new_ids = Vec::with_capacity(tasks.len());
        for t in &tasks {
            if t.spec_id.trim().is_empty() || t.title.trim().is_empty() {
                return Err(StoreError::Invalid(
                    "task spec id and title are required".into(),
                ));
            }
            if ids.contains_key(&t.spec_id) {
                return Err(StoreError::Conflict(format!(
                    "task {} already exists for this plan",
                    t.spec_id
                )));
            }
            let id = TaskId::new();
            ids.insert(t.spec_id.clone(), id);
            new_ids.push(id);
        }

        let at = now();
        for (i, (t, id)) in tasks.iter().zip(&new_ids).enumerate() {
            let deps = t
                .depends_on
                .iter()
                .map(|d| {
                    ids.get(d).copied().ok_or_else(|| {
                        StoreError::Invalid(format!(
                            "task {} depends on unknown task {d}",
                            t.spec_id
                        ))
                    })
                })
                .collect::<Result<Vec<TaskId>>>()?;
            let number = first_number + i as i64;
            sqlx::query(
                "INSERT INTO tasks (id, plan_id, project_id, owner_id, number, key, spec_id, title, description,
                                    acceptance_json, files_hint_json, depends_on_json, workers_json, state,
                                    last_active, loop_count, loop_cap, progress, meta, feedback_json,
                                    created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', NULL, 0, ?, 0, '', '[]', ?, ?)",
            )
            .bind(id.to_string())
            .bind(plan.to_string())
            .bind(project_id.to_string())
            .bind(owner_id.to_string())
            .bind(number)
            .bind(format!("{key}-{number}"))
            .bind(&t.spec_id)
            .bind(t.title.trim())
            .bind(&t.description)
            .bind(to_json(&t.acceptance)?)
            .bind(to_json(&t.files_hint)?)
            .bind(to_json(&deps)?)
            .bind(to_json(&t.workers)?)
            .bind(loop_cap)
            .bind(&at)
            .bind(&at)
            .execute(&mut *tx)
            .await
            .map_err(|e| crate::conflict_on_unique(e, "task number"))?;
        }
        sqlx::query("UPDATE projects SET next_task_number = ?, updated_at = ? WHERE id = ?")
            .bind(first_number + tasks.len() as i64)
            .bind(&at)
            .bind(project_id.to_string())
            .execute(&mut *tx)
            .await?;
        let mut out = Vec::with_capacity(new_ids.len());
        for id in new_ids {
            out.push(fetch_task(&mut tx, id).await?);
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Fetch a task.
    pub async fn get_task(&self, id: TaskId) -> Result<Task> {
        let mut conn = self.pool.acquire().await?;
        fetch_task(&mut conn, id).await
    }

    /// Fetch a task by display key ("FP-142", case-insensitive).
    pub async fn get_task_by_key(&self, key: &str) -> Result<Task> {
        let row = sqlx::query("SELECT * FROM tasks WHERE key = ?")
            .bind(key.trim().to_ascii_uppercase())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        task_from_row(&row)
    }

    /// Tasks matching `filter`, oldest first (by project number).
    pub async fn list_tasks(&self, filter: TaskFilter) -> Result<Vec<Task>> {
        let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new("SELECT * FROM tasks WHERE 1 = 1");
        if let Some(p) = filter.project {
            qb.push(" AND project_id = ").push_bind(p.to_string());
        }
        if let Some(p) = filter.plan {
            qb.push(" AND plan_id = ").push_bind(p.to_string());
        }
        if !filter.owners.is_empty() {
            qb.push(" AND owner_id IN (");
            let mut sep = qb.separated(", ");
            for o in &filter.owners {
                sep.push_bind(o.to_string());
            }
            qb.push(")");
        }
        if !filter.states.is_empty() {
            qb.push(" AND state IN (");
            let mut sep = qb.separated(", ");
            for s in &filter.states {
                sep.push_bind(s.as_str());
            }
            qb.push(")");
        }
        qb.push(" ORDER BY created_at, number");
        if let Some(l) = filter.limit {
            qb.push(" LIMIT ").push_bind(i64::from(l));
        }
        let rows = qb.build().fetch_all(&self.pool).await?;
        rows.iter().map(task_from_row).collect()
    }

    /// Compare-and-swap state transition.
    ///
    /// Fails with `Invalid` if `from → to` is not allowed by
    /// [`TaskState::can_transition`], `NotFound` if the task is missing, and
    /// `Conflict` if the task is no longer in `from`. On success the
    /// `task_transitions` row is written in the same transaction and
    /// `last_active` / `progress` are maintained.
    pub async fn transition_task(
        &self,
        id: TaskId,
        from: TaskState,
        to: TaskState,
        reason: &str,
        actor: Option<UserId>,
    ) -> Result<Task> {
        if !from.can_transition(to) {
            return Err(StoreError::Invalid(format!(
                "transition {} → {} is not allowed",
                from.as_str(),
                to.as_str()
            )));
        }
        let mut tx = self.begin_write().await?;
        let cur = fetch_task(&mut tx, id).await?;
        if cur.state != from {
            return Err(StoreError::Conflict(format!(
                "task {} is {}, expected {}",
                cur.key,
                cur.state.as_str(),
                from.as_str()
            )));
        }
        let last_active = next_last_active(cur.last_active, from, to);
        let progress: i64 = if to.is_active() {
            0
        } else if matches!(to, TaskState::Ready | TaskState::Merged) {
            100
        } else {
            i64::from(cur.progress)
        };
        let at = now();
        let r = sqlx::query(
            "UPDATE tasks SET state = ?, last_active = ?, progress = ?, updated_at = ? WHERE id = ? AND state = ?",
        )
        .bind(to.as_str())
        .bind(last_active.map(|s| s.as_str()))
        .bind(progress)
        .bind(&at)
        .bind(id.to_string())
        .bind(from.as_str())
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::Conflict(format!(
                "task {} changed state concurrently",
                cur.key
            )));
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
        let task = fetch_task(&mut tx, id).await?;
        tx.commit().await?;
        Ok(task)
    }

    /// Every transition of a task, oldest first.
    pub async fn list_transitions(&self, task: TaskId) -> Result<Vec<TaskTransition>> {
        let rows = sqlx::query("SELECT * FROM task_transitions WHERE task_id = ? ORDER BY id")
            .bind(task.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| {
                Ok(TaskTransition {
                    id: r.int("id")?,
                    task_id: r.id("task_id")?,
                    from: r.enumv("from_state")?,
                    to: r.enumv("to_state")?,
                    reason: r.str("reason")?,
                    actor: r.opt_id("actor")?,
                    at: r.time("at")?,
                })
            })
            .collect()
    }

    /// Run an `UPDATE tasks ... WHERE id = ?` and return the updated task.
    async fn finish_task_update(&self, id: TaskId, rows: u64) -> Result<Task> {
        if rows == 0 {
            return Err(StoreError::NotFound);
        }
        self.get_task(id).await
    }

    /// Set the current stage's progress (clamped to 0–100) and meta line.
    pub async fn update_task_progress(&self, id: TaskId, progress: u8, meta: &str) -> Result<Task> {
        let r = sqlx::query("UPDATE tasks SET progress = ?, meta = ?, updated_at = ? WHERE id = ?")
            .bind(i64::from(progress.min(100)))
            .bind(meta)
            .bind(now())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        self.finish_task_update(id, r.rows_affected()).await
    }

    /// Record the task branch.
    pub async fn set_task_branch(&self, id: TaskId, branch: &str) -> Result<Task> {
        let r = sqlx::query("UPDATE tasks SET branch = ?, updated_at = ? WHERE id = ?")
            .bind(branch)
            .bind(now())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        self.finish_task_update(id, r.rows_affected()).await
    }

    /// Record the opened pull request.
    pub async fn set_task_pr(&self, id: TaskId, url: &str, number: u64) -> Result<Task> {
        let r =
            sqlx::query("UPDATE tasks SET pr_url = ?, pr_number = ?, updated_at = ? WHERE id = ?")
                .bind(url)
                .bind(i64::try_from(number).unwrap_or(i64::MAX))
                .bind(now())
                .bind(id.to_string())
                .execute(&self.pool)
                .await?;
        self.finish_task_update(id, r.rows_affected()).await
    }

    /// Increment the Builder loop counter and return the task.
    pub async fn increment_loop(&self, id: TaskId) -> Result<Task> {
        let r = sqlx::query(
            "UPDATE tasks SET loop_count = loop_count + 1, updated_at = ? WHERE id = ?",
        )
        .bind(now())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        self.finish_task_update(id, r.rows_affected()).await
    }

    /// Reset the loop counter (human retry after escalation).
    pub async fn reset_loops(&self, id: TaskId) -> Result<Task> {
        let r = sqlx::query("UPDATE tasks SET loop_count = 0, updated_at = ? WHERE id = ?")
            .bind(now())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        self.finish_task_update(id, r.rows_affected()).await
    }

    /// Queue feedback text for the next Builder run.
    pub async fn push_task_feedback(&self, id: TaskId, text: &str) -> Result<()> {
        let r = sqlx::query(
            "UPDATE tasks SET feedback_json = json_insert(feedback_json, '$[#]', ?), updated_at = ? WHERE id = ?",
        )
        .bind(text)
        .bind(now())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Atomically take (and clear) the queued feedback.
    pub async fn take_task_feedback(&self, id: TaskId) -> Result<Vec<String>> {
        let mut tx = self.begin_write().await?;
        let fb: String = sqlx::query_scalar("SELECT feedback_json FROM tasks WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        sqlx::query("UPDATE tasks SET feedback_json = '[]' WHERE id = ?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(serde_json::from_str(&fb)?)
    }

    /// Peek at the queued feedback without clearing it.
    pub async fn peek_task_feedback(&self, id: TaskId) -> Result<Vec<String>> {
        let fb: String = sqlx::query_scalar("SELECT feedback_json FROM tasks WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        Ok(serde_json::from_str(&fb)?)
    }

    /// Count tasks per state for one owner (scheduler concurrency caps).
    pub async fn count_tasks_by_state(
        &self,
        owner: Option<UserId>,
    ) -> Result<BTreeMap<String, u64>> {
        let rows = sqlx::query("SELECT state, COUNT(*) AS n FROM tasks WHERE (? IS NULL OR owner_id = ?) GROUP BY state")
            .bind(owner.map(|o| o.to_string()))
            .bind(owner.map(|o| o.to_string()))
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| Ok((r.str("state")?, r.uint("n")?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskState::*;

    #[test]
    fn last_active_rules() {
        assert_eq!(next_last_active(None, Queued, Preparing), Some(Preparing));
        assert_eq!(
            next_last_active(Some(Testing), Testing, Escalated),
            Some(Testing)
        );
        assert_eq!(
            next_last_active(Some(Testing), Escalated, Failed),
            Some(Testing)
        );
        assert_eq!(
            next_last_active(Some(Reviewing), Escalated, Queued),
            Some(Reviewing)
        );
        assert_eq!(
            next_last_active(Some(Reviewing), Reviewing, Ready),
            Some(Reviewing)
        );
    }
}
