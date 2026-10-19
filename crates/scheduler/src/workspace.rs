//! Task workspaces: mirror + worktree + branch (stacked on a Ready
//! dependency), `qa.yaml` at the task branch, image choice, generated
//! `AGENTS.md` / `CLAUDE.md`, and the read-only planner checkout.

use crate::Scheduler;
use crate::error::{Result, SchedulerError};
use crate::qa_profile::parse_qa_yaml;
use kiln_gitops::{GENERATED_FILES, Worktree, branch_name};
use kiln_types::qa::QaProfile;
use kiln_types::{Project, RunId, Task, TaskState, UserId, WorkerRole};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// A prepared task workspace.
#[derive(Clone, Debug)]
pub(crate) struct Prepared {
    pub worktree: Worktree,
    /// The branch this task's branch was cut from and its PR targets.
    pub base_branch: String,
    pub profile: Option<QaProfile>,
    pub image: String,
}

/// Run `git -C dir args…` (no prompts). Returns stdout or a redacted error.
pub(crate) async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true)
        .output()
        .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(SchedulerError::Internal(format!(
            "git {}: {}",
            args.first().copied().unwrap_or(""),
            kiln_gitops::redact_url(String::from_utf8_lossy(&out.stderr).trim())
        )))
    }
}

impl Scheduler {
    /// The branch a task builds on: the branch of its last dependency that
    /// is Ready (PR open, unmerged) — stacked PRs — else the default branch.
    pub(crate) async fn base_branch_for(&self, task: &Task, project: &Project) -> Result<String> {
        let mut base = project.default_branch.clone();
        for dep in &task.depends_on {
            let d = self.store.get_task(*dep).await?;
            if d.state == TaskState::Ready
                && let Some(b) = d.branch.clone()
            {
                base = b;
            }
        }
        Ok(base)
    }

    /// Prepare (or re-open) the task's workspace. `refresh` fetches the
    /// mirror and (re)creates the worktree; otherwise an existing worktree is
    /// reused as-is. Idempotent.
    pub(crate) async fn prepare_workspace(
        &self,
        task: &Task,
        project: &Project,
        refresh: bool,
    ) -> Result<Prepared> {
        let branch = match &task.branch {
            Some(b) => b.clone(),
            None => {
                let b = branch_name(&task.key, &task.title);
                let t = self.store.set_task_branch(task.id, &b).await?;
                self.publish_task(&t);
                b
            }
        };
        let base_branch = self.base_branch_for(task, project).await?;
        let path = self.gitops.worktree_path(&task.key);
        let mirror = self.gitops.mirror_path(project);
        let worktree = if refresh || !path.join(".git").exists() || !mirror.join("HEAD").is_file() {
            let auth = self.prs.git_auth(&project.repo_url).await?;
            self.gitops.ensure_mirror(project, &auth).await?;
            self.gitops
                .create_worktree(project, &task.key, &branch, &base_branch)
                .await?
        } else {
            Worktree {
                path,
                git_common_dir: mirror,
                branch: branch.clone(),
            }
        };
        let profile = match self
            .gitops
            .read_file_at(project, &branch, &project.qa_path)
            .await
        {
            Ok(Some(text)) => match parse_qa_yaml(&text) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(task = %task.key, error = %e, "qa.yaml at the task branch is invalid; using the project's last good profile");
                    self.store.get_project_qa_profile(project.id).await?
                }
            },
            _ => self.store.get_project_qa_profile(project.id).await?,
        };
        let image = match profile.as_ref().and_then(|p| p.image.clone()) {
            Some(i) => i,
            None => {
                if profile.as_ref().is_some_and(|p| !p.extras.is_empty()) {
                    // Building `kiln-extras:<hash>` needs an image-build API the
                    // runtime trait doesn't have yet; fall back to the base image.
                    tracing::warn!(project = %project.key, "qa.yaml extras are not built yet; using the base image");
                }
                project
                    .image
                    .clone()
                    .unwrap_or_else(|| self.cfg.worker_image.clone())
            }
        };
        Ok(Prepared {
            worktree,
            base_branch,
            profile,
            image,
        })
    }

    /// Write `AGENTS.md` and `CLAUDE.md` from the L3 pack for `role` into
    /// `dir`, keeping them out of commits, and record the pack's items as
    /// used by `run`. Returns the number of memory items in context.
    pub(crate) async fn write_memory_files(
        &self,
        task: Option<&Task>,
        owner: UserId,
        project: &Project,
        role: WorkerRole,
        dir: &Path,
        run: RunId,
    ) -> Result<u32> {
        let (pack, ids) = match self.memory.pack_with_items(owner, project.id, role).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "memory pack unavailable; writing an empty one");
                (String::new(), Vec::new())
            }
        };
        let ours = kiln_planner::memory_file(&pack, role, task);
        let mut untracked: Vec<&str> = Vec::new();
        for name in GENERATED_FILES {
            let target = dir.join(name);
            let tracked = git(dir, &["ls-files", "--error-unmatch", "--", name])
                .await
                .is_ok();
            let content = if tracked {
                // `info/exclude` can't hide a tracked file: mark it
                // skip-worktree so the Builder's `git add -A` ignores our
                // version, and keep the repo's own content on top.
                let _ = git(dir, &["update-index", "--skip-worktree", "--", name]).await;
                let original = git(dir, &["show", &format!("HEAD:{name}")])
                    .await
                    .unwrap_or_default();
                format!("{}\n\n{ours}", original.trim_end())
            } else {
                untracked.push(name);
                ours.clone()
            };
            tokio::fs::write(&target, content).await?;
        }
        if !untracked.is_empty() {
            self.gitops.exclude_paths(dir, &untracked)?;
        }
        if !ids.is_empty() {
            let ctx = kiln_memory::UseContext {
                run: Some(run),
                task: task.map(|t| t.id),
                role: Some(role),
            };
            if let Err(e) = self.memory.record_use(owner, &ids, &ctx).await {
                tracing::debug!(error = %e, "record_use failed");
            }
        }
        Ok(ids.len() as u32)
    }

    /// Read-only checkout of the default branch for planner runs at
    /// `<data>/worktrees/_planner/<KEY>` (detached; refreshed per run).
    pub(crate) async fn planner_checkout(&self, project: &Project) -> Result<Worktree> {
        let _g = self.st.planner_checkout.lock().await;
        let auth = self.prs.git_auth(&project.repo_url).await?;
        let mirror = self.gitops.ensure_mirror(project, &auth).await?;
        let path: PathBuf = self
            .cfg
            .data_dir
            .join("worktrees")
            .join("_planner")
            .join(&project.key);
        let remote = format!("refs/remotes/origin/{}", project.default_branch);
        let rev = if git(
            &mirror,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{remote}^{{commit}}"),
            ],
        )
        .await
        .is_ok()
        {
            remote
        } else {
            project.default_branch.clone()
        };
        let _ = git(&mirror, &["worktree", "prune"]).await;
        if path.join(".git").exists() {
            git(&path, &["checkout", "--quiet", "--force", "--detach", &rev]).await?;
            git(&path, &["clean", "-fdq"]).await?;
        } else {
            if path.exists() {
                tokio::fs::remove_dir_all(&path).await?;
            }
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let p = path.to_string_lossy().into_owned();
            git(
                &mirror,
                &["worktree", "add", "--quiet", "--detach", &p, &rev],
            )
            .await?;
        }
        Ok(Worktree {
            path,
            git_common_dir: mirror,
            branch: project.default_branch.clone(),
        })
    }
}
