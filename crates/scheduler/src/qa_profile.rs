//! `qa.yaml` parsing and project refresh.

use crate::Scheduler;
use crate::error::Result;
use kiln_types::qa::QaProfile;
use kiln_types::{Project, ProjectId, QaProfileStatus, ResourceProfile};

/// Parse and validate a `qa.yaml` document. Errors are human-readable and
/// end up in `Project.qa_status = Invalid { error }`.
pub fn parse_qa_yaml(text: &str) -> std::result::Result<QaProfile, String> {
    if text.trim().is_empty() {
        return Err("qa.yaml is empty".into());
    }
    let p: QaProfile = serde_norway::from_str(text).map_err(|e| format!("qa.yaml: {e}"))?;
    if p.version != 1 {
        return Err(format!(
            "qa.yaml: unsupported version {} (expected 1)",
            p.version
        ));
    }
    if p.resources.memory_mb().is_none() {
        return Err(format!(
            "qa.yaml: bad resources.memory {:?}",
            p.resources.memory
        ));
    }
    if !(p.resources.cpus.is_finite() && p.resources.cpus > 0.0) {
        return Err("qa.yaml: resources.cpus must be positive".into());
    }
    if let Some(c) = p.loop_cap
        && !(1..=20).contains(&c)
    {
        return Err("qa.yaml: loop_cap must be between 1 and 20".into());
    }
    for (name, s) in &p.services {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("qa.yaml: bad service name {name:?}"));
        }
        if s.image.trim().is_empty() {
            return Err(format!("qa.yaml: service {name} has no image"));
        }
    }
    Ok(p)
}

/// Resources for a role: QA uses the profile's `resources`, everyone else
/// the project's resource profile.
pub(crate) fn resources_for(
    project: &Project,
    profile: Option<&QaProfile>,
    qa: bool,
) -> ResourceProfile {
    match profile {
        Some(p) if qa => ResourceProfile {
            cpus: p.resources.cpus,
            memory_mb: p
                .resources
                .memory_mb()
                .unwrap_or(project.resources.memory_mb),
        },
        _ => project.resources.clone(),
    }
}

impl Scheduler {
    pub(crate) async fn do_refresh_project(&self, id: ProjectId) -> Result<Project> {
        let project = self.store.get_project(id).await?;
        let auth = self.prs.git_auth(&project.repo_url).await?;
        self.gitops.ensure_mirror(&project, &auth).await?;
        let (status, profile) = match self
            .gitops
            .read_file_at(&project, &project.default_branch, &project.qa_path)
            .await
        {
            Ok(None) => (QaProfileStatus::Missing, None),
            Ok(Some(text)) => match parse_qa_yaml(&text) {
                Ok(p) => (QaProfileStatus::Valid, Some(p)),
                Err(error) => (QaProfileStatus::Invalid { error }, None),
            },
            Err(e) => (
                QaProfileStatus::Invalid {
                    error: format!("cannot read {}: {e}", project.qa_path),
                },
                None,
            ),
        };
        // Keep the last good profile when the new one is invalid/missing.
        let keep = match &profile {
            Some(p) => Some(p.clone()),
            None => self.store.get_project_qa_profile(id).await?,
        };
        self.store
            .set_project_qa(id, &status, keep.as_ref())
            .await?;
        Ok(self.store.get_project(id).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prd_example() {
        let y = r#"
version: 1
image: ghcr.io/acme/web-worker:latest
resources: { cpus: 2, memory: 6g }
boot:
  - docker compose -f qa/compose.yaml up -d
seed:
  - ./scripts/seed.sh
checks:
  unit:  "cargo test --workspace"
  e2e:   "npx playwright test"
  extra: ["lighthouse", "linkcheck"]
artifacts: ["playwright-report/**", "qa/out/**"]
loop_cap: 3
services:
  db:
    image: postgres:16
    env: { POSTGRES_PASSWORD: dev }
    ports: [5432]
timeouts: { qa: 30 }
"#;
        let p = parse_qa_yaml(y).unwrap();
        assert_eq!(p.resources.memory_mb(), Some(6144));
        assert_eq!(p.checks.unit.as_deref(), Some("cargo test --workspace"));
        assert_eq!(p.services["db"].image, "postgres:16");
        assert_eq!(p.timeouts["qa"], 30);
        assert_eq!(p.loop_cap, Some(3));
    }

    #[test]
    fn rejects_bad_profiles() {
        assert!(parse_qa_yaml("").is_err());
        assert!(parse_qa_yaml("version: 2").unwrap_err().contains("version"));
        assert!(parse_qa_yaml("version: 1\nresources: { memory: lots }").is_err());
        assert!(parse_qa_yaml("version: 1\nloop_cap: 0").is_err());
        assert!(parse_qa_yaml(": : :").is_err());
        assert!(parse_qa_yaml("version: 1").is_ok());
    }
}
