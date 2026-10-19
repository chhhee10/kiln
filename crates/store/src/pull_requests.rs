//! `pull_requests`: PRs opened for tasks (Local provider + merge polling).

use crate::row::{RowExt, enum_str, now};
use crate::{Result, Store, StoreError};
use kiln_types::{TaskId, Timestamp};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;

/// Lifecycle of a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    /// Open and awaiting review.
    Open,
    /// Merged into the base branch.
    Merged,
    /// Closed without merging.
    Closed,
}

/// A pull request to record.
#[derive(Clone, Debug)]
pub struct NewPr {
    pub task_id: TaskId,
    /// Provider id: "github_app", "github_token", "local".
    pub provider: String,
    pub number: u64,
    pub url: String,
    pub branch: String,
    pub title: String,
    pub body: String,
}

/// A recorded pull request.
#[derive(Clone, Debug, PartialEq)]
pub struct PullRequest {
    pub id: i64,
    pub task_id: TaskId,
    pub provider: String,
    pub number: u64,
    pub url: String,
    pub branch: String,
    pub title: String,
    pub body: String,
    pub state: PullRequestState,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

fn pr_from_row(r: &SqliteRow) -> Result<PullRequest> {
    Ok(PullRequest {
        id: r.int("id")?,
        task_id: r.id("task_id")?,
        provider: r.str("provider")?,
        number: r.uint("number")?,
        url: r.str("url")?,
        branch: r.str("branch")?,
        title: r.str("title")?,
        body: r.str("body")?,
        state: r.enumv("state")?,
        created_at: r.time("created_at")?,
        updated_at: r.time("updated_at")?,
    })
}

impl Store {
    /// Record an opened PR (state `Open`).
    pub async fn insert_pr(&self, new: NewPr) -> Result<PullRequest> {
        let at = now();
        let row = sqlx::query(
            "INSERT INTO pull_requests (task_id, provider, number, url, branch, title, body, state, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, 'open', ?, ?) RETURNING *",
        )
        .bind(new.task_id.to_string())
        .bind(&new.provider)
        .bind(i64::try_from(new.number).unwrap_or(i64::MAX))
        .bind(&new.url)
        .bind(&new.branch)
        .bind(&new.title)
        .bind(&new.body)
        .bind(&at)
        .bind(&at)
        .fetch_one(&self.pool)
        .await?;
        pr_from_row(&row)
    }

    /// Every open PR, oldest first (merge polling).
    pub async fn list_open_prs(&self) -> Result<Vec<PullRequest>> {
        let rows = sqlx::query("SELECT * FROM pull_requests WHERE state = 'open' ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(pr_from_row).collect()
    }

    /// The most recent PR of a task.
    pub async fn get_pr_for_task(&self, task: TaskId) -> Result<Option<PullRequest>> {
        let row =
            sqlx::query("SELECT * FROM pull_requests WHERE task_id = ? ORDER BY id DESC LIMIT 1")
                .bind(task.to_string())
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref().map(pr_from_row).transpose()
    }

    /// Update a PR's state.
    pub async fn set_pr_state(&self, id: i64, state: PullRequestState) -> Result<()> {
        let r = sqlx::query("UPDATE pull_requests SET state = ?, updated_at = ? WHERE id = ?")
            .bind(enum_str(&state))
            .bind(now())
            .bind(id)
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Next PR number for a provider (used by the Local provider).
    pub async fn next_pr_number(&self, provider: &str) -> Result<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(number), 0) + 1 FROM pull_requests WHERE provider = ?",
        )
        .bind(provider)
        .fetch_one(&self.pool)
        .await?;
        Ok(n.max(1) as u64)
    }
}
