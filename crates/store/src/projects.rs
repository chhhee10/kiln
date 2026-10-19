//! `projects`: registered repositories.

use crate::row::{RowExt, now, to_json};
use crate::{Result, Store, StoreError, conflict_on_unique};
use kiln_types::api::UpdateProjectRequest;
use kiln_types::qa::QaProfile;
use kiln_types::{DEFAULT_LOOP_CAP, Project, ProjectId, QaProfileStatus, ResourceProfile};
use sqlx::sqlite::SqliteRow;

/// A project to register.
#[derive(Clone, Debug)]
pub struct NewProject {
    /// Short key used in task ids ("FP"). Uppercased on insert.
    pub key: String,
    pub name: String,
    pub repo_url: String,
    pub default_branch: String,
    pub qa_path: String,
    pub image: Option<String>,
    pub resources: ResourceProfile,
    pub loop_cap: u32,
}

impl NewProject {
    /// A project with default image, resources and loop cap.
    pub fn new(
        key: impl Into<String>,
        name: impl Into<String>,
        repo_url: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            name: name.into(),
            repo_url: repo_url.into(),
            default_branch: "main".into(),
            qa_path: "qa.yaml".into(),
            image: None,
            resources: ResourceProfile::default(),
            loop_cap: DEFAULT_LOOP_CAP,
        }
    }
}

pub(crate) fn project_from_row(r: &SqliteRow) -> Result<Project> {
    Ok(Project {
        id: r.id("id")?,
        key: r.str("key")?,
        name: r.str("name")?,
        repo_url: r.str("repo_url")?,
        default_branch: r.str("default_branch")?,
        qa_path: r.str("qa_path")?,
        qa_status: r.json("qa_status_json")?,
        image: r.opt_str("image")?,
        resources: r.json("resources_json")?,
        loop_cap: r.u32v("loop_cap")?,
        created_at: r.time("created_at")?,
    })
}

/// Validate and normalize a project key: 1–10 chars of `[A-Z0-9]`, starting
/// with a letter.
fn normalize_key(key: &str) -> Result<String> {
    let k = key.trim().to_ascii_uppercase();
    let ok = !k.is_empty()
        && k.len() <= 10
        && k.starts_with(|c: char| c.is_ascii_uppercase())
        && k.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !ok {
        return Err(StoreError::Invalid(
            "project key must be 1-10 letters/digits, starting with a letter".into(),
        ));
    }
    Ok(k)
}

fn check_loop_cap(cap: u32) -> Result<()> {
    if !(1..=20).contains(&cap) {
        return Err(StoreError::Invalid(
            "loop cap must be between 1 and 20".into(),
        ));
    }
    Ok(())
}

impl Store {
    /// Register a project. `Conflict` if the key is taken.
    pub async fn create_project(&self, new: NewProject) -> Result<Project> {
        let key = normalize_key(&new.key)?;
        if new.name.trim().is_empty() || new.repo_url.trim().is_empty() {
            return Err(StoreError::Invalid(
                "project name and repo url are required".into(),
            ));
        }
        check_loop_cap(new.loop_cap)?;
        let id = ProjectId::new();
        let at = now();
        sqlx::query(
            "INSERT INTO projects (id, key, name, repo_url, default_branch, qa_path, qa_status_json, qa_profile_json,
                                   image, resources_json, loop_cap, next_task_number, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, 1, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&key)
        .bind(new.name.trim())
        .bind(new.repo_url.trim())
        .bind(new.default_branch.trim())
        .bind(new.qa_path.trim())
        .bind(to_json(&QaProfileStatus::Unknown)?)
        .bind(&new.image)
        .bind(to_json(&new.resources)?)
        .bind(new.loop_cap as i64)
        .bind(&at)
        .bind(&at)
        .execute(&self.pool)
        .await
        .map_err(|e| conflict_on_unique(e, "project with this key"))?;
        self.get_project(id).await
    }

    /// Fetch a project by id.
    pub async fn get_project(&self, id: ProjectId) -> Result<Project> {
        let row = sqlx::query("SELECT * FROM projects WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        project_from_row(&row)
    }

    /// Fetch a project by key (case-insensitive).
    pub async fn get_project_by_key(&self, key: &str) -> Result<Project> {
        let row = sqlx::query("SELECT * FROM projects WHERE key = ?")
            .bind(key.trim().to_ascii_uppercase())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        project_from_row(&row)
    }

    /// All projects ordered by key.
    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let rows = sqlx::query("SELECT * FROM projects ORDER BY key")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(project_from_row).collect()
    }

    /// Apply a partial update.
    pub async fn update_project(
        &self,
        id: ProjectId,
        patch: UpdateProjectRequest,
    ) -> Result<Project> {
        if let Some(cap) = patch.loop_cap {
            check_loop_cap(cap)?;
        }
        if patch.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
            return Err(StoreError::Invalid("project name is empty".into()));
        }
        let resources = patch.resources.as_ref().map(to_json).transpose()?;
        let r = sqlx::query(
            "UPDATE projects SET
                name = COALESCE(?, name),
                default_branch = COALESCE(?, default_branch),
                qa_path = COALESCE(?, qa_path),
                image = CASE WHEN ? THEN ? ELSE image END,
                loop_cap = COALESCE(?, loop_cap),
                resources_json = COALESCE(?, resources_json),
                updated_at = ?
             WHERE id = ?",
        )
        .bind(patch.name.as_deref().map(str::trim))
        .bind(patch.default_branch.as_deref().map(str::trim))
        .bind(patch.qa_path.as_deref().map(str::trim))
        .bind(patch.image.is_some())
        // An empty image string clears the override.
        .bind(
            patch
                .image
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
        .bind(patch.loop_cap.map(i64::from))
        .bind(resources)
        .bind(now())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        self.get_project(id).await
    }

    /// Record the latest `qa.yaml` parse result.
    pub async fn set_project_qa(
        &self,
        id: ProjectId,
        status: &QaProfileStatus,
        profile: Option<&QaProfile>,
    ) -> Result<()> {
        let profile = profile.map(to_json).transpose()?;
        let r = sqlx::query("UPDATE projects SET qa_status_json = ?, qa_profile_json = ?, updated_at = ? WHERE id = ?")
            .bind(to_json(status)?)
            .bind(profile)
            .bind(now())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// The last successfully parsed `qa.yaml` profile, if any.
    pub async fn get_project_qa_profile(&self, id: ProjectId) -> Result<Option<QaProfile>> {
        let row = sqlx::query("SELECT qa_profile_json FROM projects WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        row.opt_json("qa_profile_json")
    }
}
