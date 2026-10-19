//! `threads`, `messages` and `plans`: the planner chat and its drafts.

use crate::row::{RowExt, enum_str, now, to_json};
use crate::{Result, Store, StoreError};
use kiln_types::plan::PlanSpec;
use kiln_types::{
    Message, MessageAuthor, MessageId, Plan, PlanId, PlanStatus, ProjectId, Thread, ThreadId,
    UserId,
};
use sqlx::sqlite::SqliteRow;

/// A planner thread to create.
#[derive(Clone, Debug)]
pub struct NewThread {
    pub project_id: ProjectId,
    pub owner_id: UserId,
    pub title: String,
}

/// A chat message to append.
#[derive(Clone, Debug)]
pub struct NewMessage {
    pub thread_id: ThreadId,
    pub author: MessageAuthor,
    pub body: String,
    pub voice: bool,
    pub plan_version: Option<u32>,
}

/// A plan draft to create (version 1, status `Drafting`).
#[derive(Clone, Debug)]
pub struct NewPlan {
    pub project_id: ProjectId,
    pub owner_id: UserId,
    pub thread_id: ThreadId,
    pub spec: PlanSpec,
}

fn thread_from_row(r: &SqliteRow) -> Result<Thread> {
    Ok(Thread {
        id: r.id("id")?,
        project_id: r.id("project_id")?,
        owner_id: r.id("owner_id")?,
        title: r.str("title")?,
        plan_id: r.opt_id("plan_id")?,
        status: r.enumv("status")?,
        created_at: r.time("created_at")?,
        updated_at: r.time("updated_at")?,
    })
}

fn message_from_row(r: &SqliteRow) -> Result<Message> {
    Ok(Message {
        id: r.id("id")?,
        thread_id: r.id("thread_id")?,
        author: r.enumv("author")?,
        body: r.str("body")?,
        voice: r.flag("voice")?,
        plan_version: r
            .opt_int("plan_version")?
            .map(|v| v.clamp(0, u32::MAX as i64) as u32),
        created_at: r.time("created_at")?,
    })
}

pub(crate) fn plan_from_row(r: &SqliteRow) -> Result<Plan> {
    Ok(Plan {
        id: r.id("id")?,
        project_id: r.id("project_id")?,
        owner_id: r.id("owner_id")?,
        thread_id: r.id("thread_id")?,
        status: r.enumv("status")?,
        version: r.u32v("version")?,
        spec: r.json("spec_json")?,
        created_at: r.time("created_at")?,
        updated_at: r.time("updated_at")?,
    })
}

impl Store {
    // ------------------------------------------------------------ threads

    /// Create a thread in status `Drafting`.
    pub async fn create_thread(&self, new: NewThread) -> Result<Thread> {
        let id = ThreadId::new();
        let at = now();
        sqlx::query(
            "INSERT INTO threads (id, project_id, owner_id, title, plan_id, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, NULL, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(new.project_id.to_string())
        .bind(new.owner_id.to_string())
        .bind(new.title.trim())
        .bind(enum_str(&PlanStatus::Drafting))
        .bind(&at)
        .bind(&at)
        .execute(&self.pool)
        .await?;
        self.get_thread(id).await
    }

    /// Fetch a thread.
    pub async fn get_thread(&self, id: ThreadId) -> Result<Thread> {
        let row = sqlx::query("SELECT * FROM threads WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        thread_from_row(&row)
    }

    /// The owner's threads (optionally in one project), most recent first.
    pub async fn list_threads(
        &self,
        owner: UserId,
        project: Option<ProjectId>,
    ) -> Result<Vec<Thread>> {
        let rows = sqlx::query(
            "SELECT * FROM threads WHERE owner_id = ? AND (? IS NULL OR project_id = ?)
             ORDER BY updated_at DESC, id DESC",
        )
        .bind(owner.to_string())
        .bind(project.map(|p| p.to_string()))
        .bind(project.map(|p| p.to_string()))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(thread_from_row).collect()
    }

    /// Set a thread's status (mirrors its plan's status).
    pub async fn update_thread_status(&self, id: ThreadId, status: PlanStatus) -> Result<Thread> {
        let r = sqlx::query("UPDATE threads SET status = ?, updated_at = ? WHERE id = ?")
            .bind(enum_str(&status))
            .bind(now())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        self.get_thread(id).await
    }

    /// Rename a thread.
    pub async fn set_thread_title(&self, id: ThreadId, title: &str) -> Result<()> {
        let r = sqlx::query("UPDATE threads SET title = ?, updated_at = ? WHERE id = ?")
            .bind(title.trim())
            .bind(now())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    // ------------------------------------------------------------ messages

    /// Append a message and bump the thread's `updated_at`.
    pub async fn add_message(&self, new: NewMessage) -> Result<Message> {
        let id = MessageId::new();
        let at = now();
        let mut tx = self.begin_write().await?;
        let r = sqlx::query("UPDATE threads SET updated_at = ? WHERE id = ?")
            .bind(&at)
            .bind(new.thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        sqlx::query(
            "INSERT INTO messages (id, thread_id, author, body, voice, plan_version, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(new.thread_id.to_string())
        .bind(enum_str(&new.author))
        .bind(&new.body)
        .bind(new.voice as i64)
        .bind(new.plan_version.map(i64::from))
        .bind(&at)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query("SELECT * FROM messages WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        message_from_row(&row)
    }

    /// Every message of a thread in order.
    pub async fn list_messages(&self, thread: ThreadId) -> Result<Vec<Message>> {
        let rows =
            sqlx::query("SELECT * FROM messages WHERE thread_id = ? ORDER BY created_at, rowid")
                .bind(thread.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(message_from_row).collect()
    }

    // ------------------------------------------------------------ plans

    /// Create the plan for a thread and link it (`threads.plan_id`).
    /// `Conflict` if the thread already has a plan.
    pub async fn create_plan(&self, new: NewPlan) -> Result<Plan> {
        let id = PlanId::new();
        let at = now();
        let mut tx = self.begin_write().await?;
        let existing: Option<Option<String>> =
            sqlx::query_scalar("SELECT plan_id FROM threads WHERE id = ?")
                .bind(new.thread_id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        match existing {
            None => return Err(StoreError::NotFound),
            Some(Some(_)) => return Err(StoreError::Conflict("thread already has a plan".into())),
            Some(None) => {}
        }
        sqlx::query(
            "INSERT INTO plans (id, project_id, owner_id, thread_id, status, version, spec_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(new.project_id.to_string())
        .bind(new.owner_id.to_string())
        .bind(new.thread_id.to_string())
        .bind(enum_str(&PlanStatus::Drafting))
        .bind(to_json(&new.spec)?)
        .bind(&at)
        .bind(&at)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE threads SET plan_id = ?, updated_at = ? WHERE id = ?")
            .bind(id.to_string())
            .bind(&at)
            .bind(new.thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT * FROM plans WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        plan_from_row(&row)
    }

    /// Fetch a plan.
    pub async fn get_plan(&self, id: PlanId) -> Result<Plan> {
        let row = sqlx::query("SELECT * FROM plans WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        plan_from_row(&row)
    }

    /// The plan attached to a thread, if any.
    pub async fn get_plan_for_thread(&self, thread: ThreadId) -> Result<Option<Plan>> {
        let row =
            sqlx::query("SELECT * FROM plans WHERE thread_id = ? ORDER BY created_at DESC LIMIT 1")
                .bind(thread.to_string())
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref().map(plan_from_row).transpose()
    }

    /// Replace a plan's spec with optimistic concurrency: succeeds only when
    /// the stored version equals `expected_version`, and bumps it by one.
    /// Only `Drafting` plans can be edited.
    pub async fn update_plan_spec(
        &self,
        id: PlanId,
        spec: &PlanSpec,
        expected_version: u32,
    ) -> Result<Plan> {
        let mut tx = self.begin_write().await?;
        let r = sqlx::query(
            "UPDATE plans SET spec_json = ?, version = version + 1, updated_at = ?
             WHERE id = ? AND version = ? AND status = 'drafting'",
        )
        .bind(to_json(spec)?)
        .bind(now())
        .bind(id.to_string())
        .bind(i64::from(expected_version))
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query("SELECT * FROM plans WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let plan = plan_from_row(&row)?;
        if r.rows_affected() == 0 {
            return Err(if plan.status != PlanStatus::Drafting {
                StoreError::Conflict(format!(
                    "plan is {} and can no longer be edited",
                    enum_str(&plan.status)
                ))
            } else {
                StoreError::Conflict(format!(
                    "plan was edited concurrently (expected v{expected_version}, now v{})",
                    plan.version
                ))
            });
        }
        tx.commit().await?;
        Ok(plan)
    }

    /// Set a plan's status; the owning thread's status follows.
    pub async fn set_plan_status(&self, id: PlanId, status: PlanStatus) -> Result<Plan> {
        let at = now();
        let mut tx = self.begin_write().await?;
        let r = sqlx::query("UPDATE plans SET status = ?, updated_at = ? WHERE id = ?")
            .bind(enum_str(&status))
            .bind(&at)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        sqlx::query("UPDATE threads SET status = ?, updated_at = ? WHERE plan_id = ?")
            .bind(enum_str(&status))
            .bind(&at)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT * FROM plans WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        plan_from_row(&row)
    }

    /// Plans filtered by owner and/or status, most recently updated first.
    pub async fn list_plans(
        &self,
        owner: Option<UserId>,
        status: Option<PlanStatus>,
    ) -> Result<Vec<Plan>> {
        let owner = owner.map(|o| o.to_string());
        let status = status.map(|s| enum_str(&s));
        let rows = sqlx::query(
            "SELECT * FROM plans WHERE (? IS NULL OR owner_id = ?) AND (? IS NULL OR status = ?)
             ORDER BY updated_at DESC, id DESC",
        )
        .bind(&owner)
        .bind(&owner)
        .bind(&status)
        .bind(&status)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(plan_from_row).collect()
    }
}
