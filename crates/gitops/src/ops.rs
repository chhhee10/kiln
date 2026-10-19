//! Mirrors, worktrees, diffs and guarded pushes.

use crate::branch::is_kiln_branch;
use crate::cmd::{Git, GitAuth};
use crate::error::{GitError, Result};
use crate::url::RepoUrl;
use kiln_types::{DiffFile, DiffSummary, Project};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Identity written into a new mirror's config so commits made inside worker
/// containers (and the runner's "commit leftovers" step) always have an author.
const DEFAULT_USER_NAME: &str = "Kiln";
const DEFAULT_USER_EMAIL: &str = "kiln@users.noreply.localhost";

/// A task worktree on the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// `<data>/worktrees/<TASK-KEY>` (absolute).
    pub path: PathBuf,
    /// The bare mirror (`<data>/repos/<KEY>.git`, absolute). The worktree's
    /// `.git` file points into it, so containers must mount it at the same
    /// absolute path (decision #3).
    pub git_common_dir: PathBuf,
    /// The checked-out branch, e.g. `kiln/fp-142-retry-backoff`.
    pub branch: String,
}

/// Git operations rooted at the Kiln data directory.
///
/// Cheap to clone; clones share the per-mirror locks that serialize
/// mutating operations (fetch, worktree add/remove, push) on one mirror.
#[derive(Clone, Debug)]
pub struct GitOps {
    data_dir: PathBuf,
    git: Git,
    locks: Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
}

impl GitOps {
    /// Operate under `data_dir` (`$KILN_DATA_DIR`). Uses `git` from `PATH`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        let data_dir = std::path::absolute(&data_dir).unwrap_or(data_dir);
        Self {
            data_dir,
            git: Git::default(),
            locks: Arc::default(),
        }
    }

    /// Use a specific `git` binary.
    pub fn with_git_binary(mut self, bin: impl Into<PathBuf>) -> Self {
        self.git.bin = bin.into();
        self
    }

    /// Override the time budgets for local and network git commands
    /// (defaults: 2 min and 15 min).
    pub fn with_timeouts(mut self, local: Duration, network: Duration) -> Self {
        self.git.local_timeout = local;
        self.git.net_timeout = network;
        self
    }

    /// The data directory this instance operates in.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// `<data>/repos/<KEY>.git`.
    pub fn mirror_path(&self, project: &Project) -> PathBuf {
        self.data_dir
            .join("repos")
            .join(format!("{}.git", project.key))
    }

    /// `<data>/worktrees/<TASK-KEY>`.
    pub fn worktree_path(&self, task_key: &str) -> PathBuf {
        self.data_dir.join("worktrees").join(task_key)
    }

    async fn lock(&self, mirror: &Path) -> tokio::sync::OwnedMutexGuard<()> {
        let m = {
            let mut map = self.locks.lock().unwrap_or_else(|p| p.into_inner());
            map.entry(mirror.to_path_buf()).or_default().clone()
        };
        m.lock_owned().await
    }

    /// Create (bare `init` + `fetch`) or refresh (`fetch --prune`) the mirror
    /// of `project.repo_url`. Remote branches land in `refs/remotes/origin/*`;
    /// `refs/heads/*` holds only Kiln's task branches. Returns the mirror path.
    pub async fn ensure_mirror(&self, project: &Project, auth: &GitAuth) -> Result<PathBuf> {
        check_key("project key", &project.key)?;
        let url = absolutize(RepoUrl::parse(&project.repo_url)?);
        let env = auth.env_for(&url);
        let mirror = self.mirror_path(project);
        let _guard = self.lock(&mirror).await;

        if is_bare_repo(&mirror) {
            let current = self
                .git
                .run(&mirror, &["config", "--get", "remote.origin.url"])
                .await
                .unwrap_or_default();
            if current.trim() != url.git_url() {
                self.git
                    .run(&mirror, &["config", "remote.origin.url", &url.git_url()])
                    .await?;
            }
            self.git
                .run_env(
                    &mirror,
                    &["fetch", "--prune", "--quiet", "origin"],
                    &env,
                    true,
                )
                .await?;
            tracing::debug!(project = %project.key, "mirror refreshed");
            return Ok(mirror);
        }

        let parent = mirror.parent().expect("mirror path has a parent");
        tokio::fs::create_dir_all(parent).await?;
        if mirror.exists() {
            tracing::warn!(path = %mirror.display(), "removing broken mirror");
            tokio::fs::remove_dir_all(&mirror).await?;
        }
        // Build in a temp dir and rename, so a failed first fetch never
        // leaves a half-initialised mirror behind.
        let tmp = parent.join(format!(".{}.git.tmp-{}", project.key, std::process::id()));
        if tmp.exists() {
            tokio::fs::remove_dir_all(&tmp).await?;
        }
        let tmp_s = tmp.to_string_lossy().into_owned();
        let result = async {
            self.git
                .run(parent, &["init", "--bare", "--quiet", &tmp_s])
                .await?;
            self.git
                .run(&tmp, &["config", "remote.origin.url", &url.git_url()])
                .await?;
            self.git
                .run(
                    &tmp,
                    &[
                        "config",
                        "remote.origin.fetch",
                        "+refs/heads/*:refs/remotes/origin/*",
                    ],
                )
                .await?;
            self.git
                .run(&tmp, &["config", "user.name", DEFAULT_USER_NAME])
                .await?;
            self.git
                .run(&tmp, &["config", "user.email", DEFAULT_USER_EMAIL])
                .await?;
            self.git
                .run_env(&tmp, &["fetch", "--prune", "--quiet", "origin"], &env, true)
                .await?;
            Ok::<_, GitError>(())
        }
        .await;
        if let Err(e) = result {
            let _ = tokio::fs::remove_dir_all(&tmp).await;
            return Err(e);
        }
        tokio::fs::rename(&tmp, &mirror).await?;
        tracing::info!(project = %project.key, path = %mirror.display(), "mirror created");
        Ok(mirror)
    }

    /// Create the worktree for `task_key` on `branch`.
    ///
    /// * New branch: created from `origin/<base>` (falling back to `<base>`
    ///   as any revision) without upstream tracking.
    /// * Existing branch (a loop): checked out as-is, so prior attempts'
    ///   commits are kept.
    /// * Existing worktree: reused; uncommitted changes and untracked
    ///   (non-ignored) files are discarded so each run starts clean, and the
    ///   branch is switched to `branch` if needed.
    ///
    /// Requires [`GitOps::ensure_mirror`] to have run.
    pub async fn create_worktree(
        &self,
        project: &Project,
        task_key: &str,
        branch: &str,
        base: &str,
    ) -> Result<Worktree> {
        check_key("task key", task_key)?;
        if branch == project.default_branch {
            return Err(GitError::Refused(format!(
                "a task worktree may not check out the default branch `{branch}`"
            )));
        }
        let mirror = self.existing_mirror(project)?;
        self.check_branch_name(&mirror, branch).await?;
        let path = self.worktree_path(task_key);
        let _guard = self.lock(&mirror).await;
        self.git.run(&mirror, &["worktree", "prune"]).await?;
        let branch_ref = format!("refs/heads/{branch}");
        let branch_exists = self
            .git
            .succeeds(&mirror, &["show-ref", "--verify", "--quiet", &branch_ref])
            .await;

        if path.exists() {
            if self.is_worktree_of(&path, &mirror).await {
                self.git.run(&path, &["reset", "--hard", "--quiet"]).await?;
                self.git.run(&path, &["clean", "-fd", "--quiet"]).await?;
                let current = self
                    .git
                    .run(&path, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                    .await
                    .unwrap_or_default();
                if current.trim() != branch {
                    if branch_exists {
                        self.git
                            .run(&path, &["checkout", "--quiet", branch])
                            .await?;
                    } else {
                        let base_ref = self.resolve_base(&mirror, base).await?;
                        self.git
                            .run(
                                &path,
                                &["checkout", "--quiet", "--no-track", "-b", branch, &base_ref],
                            )
                            .await?;
                    }
                }
                tracing::debug!(task = task_key, "worktree reused");
                return Ok(Worktree {
                    path,
                    git_common_dir: mirror,
                    branch: branch.to_string(),
                });
            }
            tracing::warn!(path = %path.display(), "removing stray directory in place of worktree");
            tokio::fs::remove_dir_all(&path).await?;
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let path_s = path.to_string_lossy().into_owned();
        if branch_exists {
            self.git
                .run(&mirror, &["worktree", "add", "--quiet", &path_s, branch])
                .await?;
        } else {
            let base_ref = self.resolve_base(&mirror, base).await?;
            self.git
                .run(
                    &mirror,
                    &[
                        "worktree",
                        "add",
                        "--quiet",
                        "--no-track",
                        "-b",
                        branch,
                        &path_s,
                        &base_ref,
                    ],
                )
                .await?;
        }
        tracing::info!(task = task_key, branch, "worktree created");
        Ok(Worktree {
            path,
            git_common_dir: mirror,
            branch: branch.to_string(),
        })
    }

    /// Remove the worktree for `task_key` (idempotent). The branch is kept
    /// so the PR and later loops can still use it.
    pub async fn remove_worktree(&self, project: &Project, task_key: &str) -> Result<()> {
        check_key("task key", task_key)?;
        let path = self.worktree_path(task_key);
        let mirror = self.mirror_path(project);
        if !is_bare_repo(&mirror) {
            if path.exists() {
                tokio::fs::remove_dir_all(&path).await?;
            }
            return Ok(());
        }
        let _guard = self.lock(&mirror).await;
        if path.exists() {
            let path_s = path.to_string_lossy().into_owned();
            if let Err(e) = self
                .git
                .run(
                    &mirror,
                    &["worktree", "remove", "--force", "--force", &path_s],
                )
                .await
            {
                tracing::debug!(error = %e, "git worktree remove failed; deleting directory");
            }
            if path.exists() {
                tokio::fs::remove_dir_all(&path).await?;
            }
        }
        self.git.run(&mirror, &["worktree", "prune"]).await?;
        Ok(())
    }

    /// Contents of `path` at `rev` in the project's mirror, or `None` if the
    /// file does not exist at that revision. `rev` is tried as
    /// `origin/<rev>` first (so a branch name means the remote's state),
    /// then as any revision. Read with gitoxide.
    pub async fn read_file_at(
        &self,
        project: &Project,
        rev: &str,
        path: &str,
    ) -> Result<Option<String>> {
        let mirror = self.existing_mirror(project)?;
        let rev = rev.to_string();
        let rel = path
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<String>> {
            let repo = gix::open(&mirror).map_err(GitError::gix)?;
            let id = [format!("refs/remotes/origin/{rev}"), rev.clone()]
                .iter()
                .find_map(|spec| {
                    repo.rev_parse_single(spec.as_str())
                        .ok()
                        .map(|id| id.detach())
                })
                .ok_or_else(|| GitError::RevNotFound(rev.clone()))?;
            let commit = repo
                .find_object(id)
                .map_err(GitError::gix)?
                .peel_to_commit()
                .map_err(GitError::gix)?;
            let tree = commit.tree().map_err(GitError::gix)?;
            let Some(entry) = tree.lookup_entry_by_path(&rel).map_err(GitError::gix)? else {
                return Ok(None);
            };
            if !entry.mode().is_blob() {
                return Ok(None);
            }
            let obj = entry.object().map_err(GitError::gix)?;
            String::from_utf8(obj.data.clone())
                .map(Some)
                .map_err(|_| GitError::Gix(format!("{rel} at {rev} is not valid UTF-8")))
        })
        .await
        .map_err(|e| GitError::Gix(format!("blocking task failed: {e}")))?
    }

    /// Full hex id of `HEAD` in `worktree` (gitoxide).
    pub async fn head(&self, worktree: &Path) -> Result<String> {
        let wt = worktree.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let repo = gix::open(&wt).map_err(|_| GitError::NotAWorktree(wt.clone()))?;
            let id = repo.head_id().map_err(GitError::gix)?;
            Ok(id.to_string())
        })
        .await
        .map_err(|e| GitError::Gix(format!("blocking task failed: {e}")))?
    }

    /// Committed changes on `HEAD` relative to its merge-base with
    /// `origin/<base>` (or `<base>`), via `git diff --numstat`. Binary files
    /// count 0/0; renames are reported under their new path.
    pub async fn diff_summary(&self, worktree: &Path, base: &str) -> Result<DiffSummary> {
        let base_ref = self.resolve_base(worktree, base).await?;
        let mb = self
            .git
            .run(worktree, &["merge-base", "HEAD", &base_ref])
            .await?;
        let mb = mb.trim();
        let out = self
            .git
            .run(
                worktree,
                &[
                    "diff",
                    "--numstat",
                    "-z",
                    "-M",
                    "--no-ext-diff",
                    "--no-textconv",
                    mb,
                    "HEAD",
                    "--",
                ],
            )
            .await?;
        Ok(parse_numstat_z(&out))
    }

    /// Push `branch` from `worktree` to the project's remote.
    ///
    /// Refuses the project's default branch, any branch not under `kiln/`,
    /// and anything that would need a force push (the refspec has no `+`;
    /// a diverged remote is reported as [`GitError::Refused`]).
    pub async fn push_branch(
        &self,
        project: &Project,
        worktree: &Path,
        branch: &str,
        auth: &GitAuth,
    ) -> Result<()> {
        if branch == project.default_branch {
            return Err(GitError::Refused(format!(
                "`{branch}` is the project's default branch"
            )));
        }
        if !is_kiln_branch(branch) {
            return Err(GitError::Refused(format!(
                "`{branch}` is not a Kiln task branch (must start with `kiln/`)"
            )));
        }
        let mirror = self.existing_mirror(project)?;
        self.check_branch_name(&mirror, branch).await?;
        if !self.is_worktree_of(worktree, &mirror).await {
            return Err(GitError::NotAWorktree(worktree.to_path_buf()));
        }
        let branch_ref = format!("refs/heads/{branch}");
        if !self
            .git
            .succeeds(worktree, &["show-ref", "--verify", "--quiet", &branch_ref])
            .await
        {
            return Err(GitError::NotFound(format!("branch {branch}")));
        }
        let url = absolutize(RepoUrl::parse(&project.repo_url)?);
        let env = auth.env_for(&url);
        let refspec = format!("{branch_ref}:{branch_ref}");
        let _guard = self.lock(&mirror).await;
        let out = self
            .git
            .output(
                worktree,
                &["push", "--porcelain", "--no-verify", "origin", &refspec],
                &env,
                true,
            )
            .await?;
        if out.code == Some(0) {
            tracing::info!(branch, "branch pushed");
            return Ok(());
        }
        let text = format!("{}\n{}", out.stdout, out.stderr);
        if text.contains("non-fast-forward")
            || text.contains("[rejected]")
            || text.contains("fetch first")
        {
            return Err(GitError::Refused(format!(
                "remote `{branch}` has diverged; Kiln never force-pushes"
            )));
        }
        Err(GitError::Command {
            command: format!("push origin {refspec}"),
            code: out.code,
            stderr: crate::url::redact_url(out.stderr.trim()),
        })
    }

    /// Add `paths` (gitignore patterns) to the repository's `info/exclude`
    /// so generated files such as `AGENTS.md` / `CLAUDE.md` are never
    /// committed. For a linked worktree the effective file is in the common
    /// git dir (`<mirror>/info/exclude`); for a normal checkout it is
    /// `.git/info/exclude`. Idempotent.
    pub fn exclude_paths(&self, worktree: &Path, paths: &[&str]) -> Result<()> {
        let info = info_dir_for(worktree)?;
        std::fs::create_dir_all(&info)?;
        let file = info.join("exclude");
        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        let have: std::collections::HashSet<&str> = existing.lines().map(str::trim).collect();
        let missing: Vec<&str> = paths
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty() && !have.contains(p))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let mut out = existing.clone();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !have.contains("# kiln: generated files") {
            out.push_str("# kiln: generated files\n");
        }
        for p in missing {
            out.push_str(p);
            out.push('\n');
        }
        std::fs::write(&file, out)?;
        Ok(())
    }

    /// Delete a local `kiln/` branch from the mirror (after merge or
    /// cancellation). Refuses non-Kiln branches; a branch still checked out
    /// in a worktree is an error. Missing branches are fine.
    pub async fn delete_local_branch(&self, project: &Project, branch: &str) -> Result<()> {
        if !is_kiln_branch(branch) || branch == project.default_branch {
            return Err(GitError::Refused(format!(
                "`{branch}` is not a Kiln task branch"
            )));
        }
        let mirror = self.existing_mirror(project)?;
        let _guard = self.lock(&mirror).await;
        let branch_ref = format!("refs/heads/{branch}");
        if self
            .git
            .succeeds(&mirror, &["show-ref", "--verify", "--quiet", &branch_ref])
            .await
        {
            self.git.run(&mirror, &["branch", "-D", branch]).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ helpers

    fn existing_mirror(&self, project: &Project) -> Result<PathBuf> {
        check_key("project key", &project.key)?;
        let mirror = self.mirror_path(project);
        if is_bare_repo(&mirror) {
            Ok(mirror)
        } else {
            Err(GitError::NotFound(format!(
                "mirror for {} (call ensure_mirror first)",
                project.key
            )))
        }
    }

    async fn check_branch_name(&self, cwd: &Path, branch: &str) -> Result<()> {
        if branch.is_empty()
            || branch.starts_with('-')
            || branch.starts_with('+')
            || branch.contains(':')
        {
            return Err(GitError::Refused(format!("invalid branch name `{branch}`")));
        }
        if !self
            .git
            .succeeds(cwd, &["check-ref-format", "--branch", branch])
            .await
        {
            return Err(GitError::Refused(format!("invalid branch name `{branch}`")));
        }
        Ok(())
    }

    /// `origin/<base>` if the remote-tracking ref exists, else `<base>` if it
    /// resolves to a commit.
    async fn resolve_base(&self, cwd: &Path, base: &str) -> Result<String> {
        let remote = format!("refs/remotes/origin/{base}");
        if self
            .git
            .succeeds(
                cwd,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{remote}^{{commit}}"),
                ],
            )
            .await
        {
            return Ok(remote);
        }
        if !base.starts_with('-')
            && self
                .git
                .succeeds(
                    cwd,
                    &[
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("{base}^{{commit}}"),
                    ],
                )
                .await
        {
            return Ok(base.to_string());
        }
        Err(GitError::RevNotFound(base.to_string()))
    }

    /// True when `path` is the top level of a worktree whose common dir is `mirror`.
    async fn is_worktree_of(&self, path: &Path, mirror: &Path) -> bool {
        if !path.join(".git").exists() {
            return false;
        }
        let Ok(out) = self
            .git
            .run(
                path,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-common-dir",
                    "--show-toplevel",
                ],
            )
            .await
        else {
            return false;
        };
        let mut lines = out.lines();
        let (Some(common), Some(top)) = (lines.next(), lines.next()) else {
            return false;
        };
        same_path(Path::new(common), mirror) && same_path(Path::new(top), path)
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn is_bare_repo(p: &Path) -> bool {
    p.join("HEAD").is_file() && p.join("objects").is_dir() && p.join("refs").is_dir()
}

/// Keys become path components: allow only `[A-Za-z0-9_-]`, 1–64 chars.
fn check_key(what: &str, key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !key.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(GitError::Refused(format!("invalid {what} `{key}`")))
    }
}

/// Local remotes are given relative to the daemon's cwd; git runs elsewhere.
fn absolutize(url: RepoUrl) -> RepoUrl {
    match url {
        RepoUrl::Local { path } if path.is_relative() => RepoUrl::Local {
            path: std::path::absolute(&path).unwrap_or(path),
        },
        other => other,
    }
}

/// Locate the `info/` directory git consults for `info/exclude`.
fn info_dir_for(worktree: &Path) -> Result<PathBuf> {
    let dotgit = worktree.join(".git");
    if dotgit.is_dir() {
        return Ok(dotgit.join("info"));
    }
    if worktree.join("HEAD").is_file() && worktree.join("objects").is_dir() {
        return Ok(worktree.join("info")); // bare repo
    }
    let content = std::fs::read_to_string(&dotgit)
        .map_err(|_| GitError::NotAWorktree(worktree.to_path_buf()))?;
    let gitdir = content
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(|s| PathBuf::from(s.trim()))
        .ok_or_else(|| GitError::NotAWorktree(worktree.to_path_buf()))?;
    let gitdir = if gitdir.is_relative() {
        worktree.join(gitdir)
    } else {
        gitdir
    };
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) => {
            let c = PathBuf::from(c.trim());
            if c.is_relative() { gitdir.join(c) } else { c }
        }
        Err(_) => gitdir,
    };
    Ok(common.join("info"))
}

/// Parse `git diff --numstat -z` output.
///
/// Records are `add\tdel\tpath\0`, or for renames/copies
/// `add\tdel\t\0old\0new\0`. Binary files show `-` counts.
pub(crate) fn parse_numstat_z(out: &str) -> DiffSummary {
    let mut summary = DiffSummary::default();
    let mut fields = out.split('\0');
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        let mut parts = rec.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = fields.next();
            fields.next().unwrap_or_default().to_string()
        } else {
            path.to_string()
        };
        let additions = a.parse().unwrap_or(0);
        let deletions = d.parse().unwrap_or(0);
        summary.additions += additions;
        summary.deletions += deletions;
        summary.files.push(DiffFile {
            path,
            additions,
            deletions,
        });
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_parsing() {
        let out = "3\t1\tsrc/a.rs\0-\t-\timg.png\x000\t0\t\0old/name.rs\0new/name.rs\x005\t0\tREADME.md\0";
        let s = parse_numstat_z(out);
        assert_eq!(s.additions, 8);
        assert_eq!(s.deletions, 1);
        let paths: Vec<_> = s.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/a.rs", "img.png", "new/name.rs", "README.md"]);
        assert_eq!(parse_numstat_z(""), DiffSummary::default());
    }

    #[test]
    fn keys() {
        assert!(check_key("k", "FP-142").is_ok());
        assert!(check_key("k", "../etc").is_err());
        assert!(check_key("k", "").is_err());
        assert!(check_key("k", "-x").is_err());
    }
}
