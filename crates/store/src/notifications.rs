//! `notifications`: in-app notification cards.

use crate::row::{RowExt, enum_str, now, to_json};
use crate::{Result, Store, StoreError};
use kiln_types::{
    MemoryId, Notification, NotificationAction, NotificationId, NotificationKind, PlanId, TaskId,
    UserId,
};
use sqlx::sqlite::SqliteRow;

/// A notification to persist.
#[derive(Clone, Debug)]
pub struct NewNotification {
    pub user_id: UserId,
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
    /// In-app route, e.g. "task/FP-139".
    pub deep_link: String,
    pub actions: Vec<NotificationAction>,
    pub task_id: Option<TaskId>,
    pub plan_id: Option<PlanId>,
    pub memory_id: Option<MemoryId>,
}

impl NewNotification {
    /// A notification with no actions or links to domain objects.
    pub fn new(
        user_id: UserId,
        kind: NotificationKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            user_id,
            kind,
            title: title.into(),
            body: body.into(),
            deep_link: String::new(),
            actions: Vec::new(),
            task_id: None,
            plan_id: None,
            memory_id: None,
        }
    }
}

fn notification_from_row(r: &SqliteRow) -> Result<Notification> {
    Ok(Notification {
        id: r.id("id")?,
        user_id: r.id("user_id")?,
        kind: r.enumv("kind")?,
        title: r.str("title")?,
        body: r.str("body")?,
        deep_link: r.str("deep_link")?,
        actions: r.json("actions_json")?,
        task_id: r.opt_id("task_id")?,
        plan_id: r.opt_id("plan_id")?,
        memory_id: r.opt_id("memory_id")?,
        read: r.flag("read")?,
        resolved: r.opt_str("resolved")?,
        created_at: r.time("created_at")?,
    })
}

impl Store {
    /// Persist a notification (unread, unresolved).
    pub async fn insert_notification(&self, new: NewNotification) -> Result<Notification> {
        let row = sqlx::query(
            "INSERT INTO notifications (id, user_id, kind, title, body, deep_link, actions_json, task_id, plan_id,
                                        memory_id, read, resolved, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, NULL, ?) RETURNING *",
        )
        .bind(NotificationId::new().to_string())
        .bind(new.user_id.to_string())
        .bind(enum_str(&new.kind))
        .bind(&new.title)
        .bind(&new.body)
        .bind(&new.deep_link)
        .bind(to_json(&new.actions)?)
        .bind(new.task_id.map(|v| v.to_string()))
        .bind(new.plan_id.map(|v| v.to_string()))
        .bind(new.memory_id.map(|v| v.to_string()))
        .bind(now())
        .fetch_one(&self.pool)
        .await?;
        notification_from_row(&row)
    }

    /// Fetch a notification.
    pub async fn get_notification(&self, id: NotificationId) -> Result<Notification> {
        let row = sqlx::query("SELECT * FROM notifications WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        notification_from_row(&row)
    }

    /// The user's most recent notifications, newest first.
    pub async fn list_notifications(&self, user: UserId, limit: u32) -> Result<Vec<Notification>> {
        let rows = sqlx::query("SELECT * FROM notifications WHERE user_id = ? ORDER BY created_at DESC, id DESC LIMIT ?")
            .bind(user.to_string())
            .bind(i64::from(limit.clamp(1, 1000)))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(notification_from_row).collect()
    }

    /// Number of unread notifications.
    pub async fn count_unread_notifications(&self, user: UserId) -> Result<u64> {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE user_id = ? AND read = 0")
                .bind(user.to_string())
                .fetch_one(&self.pool)
                .await?;
        Ok(n.max(0) as u64)
    }

    /// Mark one of `user`'s notifications read. `NotFound` if it is not theirs.
    pub async fn mark_read(&self, user: UserId, id: NotificationId) -> Result<()> {
        let r = sqlx::query("UPDATE notifications SET read = 1 WHERE id = ? AND user_id = ?")
            .bind(id.to_string())
            .bind(user.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Mark every notification of `user` read. Returns rows changed.
    pub async fn mark_all_read(&self, user: UserId) -> Result<u64> {
        let r = sqlx::query("UPDATE notifications SET read = 1 WHERE user_id = ? AND read = 0")
            .bind(user.to_string())
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Record that a notification was acted on ("✓ Retried · attempt 3/3").
    /// Resolving also marks it read.
    pub async fn resolve_notification(
        &self,
        id: NotificationId,
        text: &str,
    ) -> Result<Notification> {
        let row =
            sqlx::query("UPDATE notifications SET resolved = ?, read = 1 WHERE id = ? RETURNING *")
                .bind(text)
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?
                .ok_or(StoreError::NotFound)?;
        notification_from_row(&row)
    }

    /// The newest unresolved notification of `kind` for `user` (used for the
    /// single ongoing "workers live" card).
    pub async fn find_open_notification(
        &self,
        user: UserId,
        kind: NotificationKind,
    ) -> Result<Option<Notification>> {
        let row = sqlx::query(
            "SELECT * FROM notifications WHERE user_id = ? AND kind = ? AND resolved IS NULL
             ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(user.to_string())
        .bind(enum_str(&kind))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(notification_from_row).transpose()
    }

    /// Replace a notification's title and body in place.
    pub async fn update_notification_content(
        &self,
        id: NotificationId,
        title: &str,
        body: &str,
    ) -> Result<Notification> {
        let row =
            sqlx::query("UPDATE notifications SET title = ?, body = ? WHERE id = ? RETURNING *")
                .bind(title)
                .bind(body)
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?
                .ok_or(StoreError::NotFound)?;
        notification_from_row(&row)
    }
}
