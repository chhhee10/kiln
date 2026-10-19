//! `artifacts`: files produced by runs, indexed by path inside the run dir.

use crate::row::{RowExt, enum_str, now};
use crate::{Result, Store, StoreError};
use kiln_types::{Artifact, ArtifactId, ArtifactKind, RunId, TaskId};
use sqlx::sqlite::SqliteRow;

/// An artifact to index.
#[derive(Clone, Debug)]
pub struct NewArtifact {
    pub run_id: RunId,
    pub kind: ArtifactKind,
    /// Path relative to the run folder; must not escape it.
    pub path: String,
    pub size: u64,
    pub mime: String,
    pub caption: Option<String>,
}

fn artifact_from_row(r: &SqliteRow) -> Result<Artifact> {
    Ok(Artifact {
        id: r.id("id")?,
        run_id: r.id("run_id")?,
        kind: r.enumv("kind")?,
        path: r.str("path")?,
        size: r.uint("size")?,
        mime: r.str("mime")?,
        caption: r.opt_str("caption")?,
        created_at: r.time("created_at")?,
    })
}

/// Reject absolute paths and `..` components.
fn check_rel_path(p: &str) -> Result<()> {
    let bad = p.is_empty()
        || p.starts_with('/')
        || p.starts_with('\\')
        || p.contains('\0')
        || p.split(['/', '\\']).any(|c| c == "..");
    if bad {
        return Err(StoreError::Invalid(format!(
            "artifact path {p:?} must be relative to the run dir"
        )));
    }
    Ok(())
}

impl Store {
    /// Index an artifact. Idempotent on `(run_id, path)`: a repeat updates
    /// kind/size/mime/caption and returns the existing id.
    pub async fn insert_artifact(&self, new: NewArtifact) -> Result<Artifact> {
        check_rel_path(&new.path)?;
        let row = sqlx::query(
            "INSERT INTO artifacts (id, run_id, kind, path, size, mime, caption, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(run_id, path) DO UPDATE SET
                kind = excluded.kind, size = excluded.size, mime = excluded.mime,
                caption = COALESCE(excluded.caption, artifacts.caption)
             RETURNING *",
        )
        .bind(ArtifactId::new().to_string())
        .bind(new.run_id.to_string())
        .bind(enum_str(&new.kind))
        .bind(&new.path)
        .bind(i64::try_from(new.size).unwrap_or(i64::MAX))
        .bind(&new.mime)
        .bind(&new.caption)
        .bind(now())
        .fetch_one(&self.pool)
        .await?;
        artifact_from_row(&row)
    }

    /// Fetch an artifact.
    pub async fn get_artifact(&self, id: ArtifactId) -> Result<Artifact> {
        let row = sqlx::query("SELECT * FROM artifacts WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        artifact_from_row(&row)
    }

    /// Artifacts of every run of a task, oldest first.
    pub async fn list_artifacts_for_task(&self, task: TaskId) -> Result<Vec<Artifact>> {
        let rows = sqlx::query(
            "SELECT a.* FROM artifacts a JOIN runs r ON r.id = a.run_id WHERE r.task_id = ?
             ORDER BY a.created_at, a.id",
        )
        .bind(task.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(artifact_from_row).collect()
    }

    /// Artifacts of one run, oldest first.
    pub async fn list_artifacts_for_run(&self, run: RunId) -> Result<Vec<Artifact>> {
        let rows = sqlx::query("SELECT * FROM artifacts WHERE run_id = ? ORDER BY created_at, id")
            .bind(run.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(artifact_from_row).collect()
    }
}
