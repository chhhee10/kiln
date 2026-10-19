//! Offline PR provider for tests, demos and repositories without GitHub.

use crate::cmd::{Git, GitAuth};
use crate::error::{GitError, Result};
use crate::pr::{NewPr, PrProvider, PrRef, PrState};
use crate::url::RepoUrl;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One locally recorded PR.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalPr {
    /// 1-based, unique per data dir.
    pub number: u64,
    /// `local://pr/<number>`.
    pub url: String,
    /// The repository URL as given in [`NewPr::repo_url`].
    pub repo_url: String,
    /// Target branch.
    pub base: String,
    /// Source branch.
    pub head: String,
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Draft flag.
    pub draft: bool,
    /// Last observed state.
    pub state: PrState,
    /// Remote tip of `head` when the PR was opened (or last seen).
    pub head_sha: String,
    /// When the PR was recorded.
    pub created_at: DateTime<Utc>,
    /// Last state change.
    pub updated_at: DateTime<Utc>,
}

/// Records PRs in `<data>/local_prs.json`. `pr_state` reports
/// [`PrState::Merged`] once the head branch tip is an ancestor of the
/// remote base branch, and [`PrState::Closed`] when the head branch was
/// deleted on the remote without being merged. Checks fetch into a private
/// scratch repository (`<data>/local_prs/check.git`).
#[derive(Debug)]
pub struct LocalPrs {
    file: PathBuf,
    scratch: PathBuf,
    git: Git,
    lock: tokio::sync::Mutex<()>,
}

impl LocalPrs {
    /// Store records under `data_dir`.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        let d = data_dir.as_ref();
        let d = std::path::absolute(d).unwrap_or_else(|_| d.to_path_buf());
        Self {
            file: d.join("local_prs.json"),
            scratch: d.join("local_prs").join("check.git"),
            git: Git::default(),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// All recorded PRs, oldest first.
    pub async fn list(&self) -> Result<Vec<LocalPr>> {
        let _g = self.lock.lock().await;
        self.load().await
    }

    /// One recorded PR.
    pub async fn get(&self, number: u64) -> Result<Option<LocalPr>> {
        Ok(self.list().await?.into_iter().find(|p| p.number == number))
    }

    /// Mark a PR closed (the local equivalent of closing it on GitHub).
    pub async fn close_pr(&self, number: u64) -> Result<()> {
        let _g = self.lock.lock().await;
        let mut prs = self.load().await?;
        let pr = prs
            .iter_mut()
            .find(|p| p.number == number)
            .ok_or_else(|| GitError::NotFound(format!("local PR #{number}")))?;
        if pr.state == PrState::Open {
            pr.state = PrState::Closed;
            pr.updated_at = Utc::now();
        }
        self.save(&prs).await
    }

    async fn load(&self) -> Result<Vec<LocalPr>> {
        match tokio::fs::read(&self.file).await {
            Ok(bytes) if bytes.is_empty() => Ok(Vec::new()),
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    async fn save(&self, prs: &[LocalPr]) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = self.file.with_extension("json.tmp");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(prs)?).await?;
        tokio::fs::rename(&tmp, &self.file).await?;
        Ok(())
    }

    async fn scratch_repo(&self) -> Result<&Path> {
        if !self.scratch.join("HEAD").is_file() {
            let parent = self.scratch.parent().expect("scratch has parent");
            tokio::fs::create_dir_all(parent).await?;
            let s = self.scratch.to_string_lossy().into_owned();
            self.git
                .run(parent, &["init", "--bare", "--quiet", &s])
                .await?;
        }
        Ok(&self.scratch)
    }

    /// Remote tip of `branch`, `None` if the branch does not exist.
    async fn remote_tip(&self, url: &str, branch: &str) -> Result<Option<String>> {
        let cwd = self.scratch_repo().await?;
        let out = self
            .git
            .run_env(
                cwd,
                &["ls-remote", "--heads", url, &format!("refs/heads/{branch}")],
                &[],
                true,
            )
            .await?;
        Ok(out.split_whitespace().next().map(str::to_string))
    }
}

fn git_url(repo_url: &str) -> Result<String> {
    Ok(match RepoUrl::parse(repo_url)? {
        RepoUrl::Local { path } => std::path::absolute(&path)
            .unwrap_or(path)
            .display()
            .to_string(),
        other => other.git_url(),
    })
}

#[async_trait]
impl PrProvider for LocalPrs {
    fn id(&self) -> &'static str {
        "local"
    }

    async fn open_pr(&self, pr: &NewPr) -> Result<PrRef> {
        let url = git_url(&pr.repo_url)?;
        let _g = self.lock.lock().await;
        let head_sha = self.remote_tip(&url, &pr.head).await?.ok_or_else(|| {
            GitError::NotFound(format!("branch {} on the remote (push it first)", pr.head))
        })?;
        if self.remote_tip(&url, &pr.base).await?.is_none() {
            return Err(GitError::NotFound(format!(
                "base branch {} on the remote",
                pr.base
            )));
        }
        let mut prs = self.load().await?;
        if let Some(existing) = prs
            .iter_mut()
            .find(|p| p.repo_url == pr.repo_url && p.head == pr.head && p.state == PrState::Open)
        {
            existing.head_sha = head_sha;
            existing.title = pr.title.clone();
            existing.body = pr.body.clone();
            existing.updated_at = Utc::now();
            let r = PrRef {
                number: existing.number,
                url: existing.url.clone(),
            };
            self.save(&prs).await?;
            return Ok(r);
        }
        let number = prs.iter().map(|p| p.number).max().unwrap_or(0) + 1;
        let now = Utc::now();
        let rec = LocalPr {
            number,
            url: format!("local://pr/{number}"),
            repo_url: pr.repo_url.clone(),
            base: pr.base.clone(),
            head: pr.head.clone(),
            title: pr.title.clone(),
            body: pr.body.clone(),
            draft: pr.draft,
            state: PrState::Open,
            head_sha,
            created_at: now,
            updated_at: now,
        };
        let r = PrRef {
            number,
            url: rec.url.clone(),
        };
        prs.push(rec);
        self.save(&prs).await?;
        Ok(r)
    }

    async fn pr_state(&self, repo_url: &str, number: u64) -> Result<PrState> {
        let _g = self.lock.lock().await;
        let mut prs = self.load().await?;
        let idx = prs
            .iter()
            .position(|p| p.number == number && p.repo_url == repo_url)
            .ok_or_else(|| {
                GitError::NotFound(format!(
                    "local PR #{number} for {}",
                    crate::url::redact_url(repo_url)
                ))
            })?;
        if prs[idx].state.is_final() {
            return Ok(prs[idx].state);
        }
        let url = git_url(repo_url)?;
        let (base, head) = (prs[idx].base.clone(), prs[idx].head.clone());
        let cwd = self.scratch_repo().await?.to_path_buf();
        let base_ref = format!("refs/kiln-check/{number}/base");
        let head_ref = format!("refs/kiln-check/{number}/head");
        self.git
            .run_env(
                &cwd,
                &[
                    "fetch",
                    "--no-tags",
                    "--quiet",
                    &url,
                    &format!("+refs/heads/{base}:{base_ref}"),
                ],
                &[],
                true,
            )
            .await?;
        let head_tip = self.remote_tip(&url, &head).await?;
        if head_tip.is_some() {
            self.git
                .run_env(
                    &cwd,
                    &[
                        "fetch",
                        "--no-tags",
                        "--quiet",
                        &url,
                        &format!("+refs/heads/{head}:{head_ref}"),
                    ],
                    &[],
                    true,
                )
                .await?;
        }
        // The live tip if the branch still exists, else the last tip we saw.
        let sha = head_tip
            .clone()
            .unwrap_or_else(|| prs[idx].head_sha.clone());
        let present = self
            .git
            .succeeds(&cwd, &["cat-file", "-e", &format!("{sha}^{{commit}}")])
            .await;
        let merged = present
            && self
                .git
                .check(&cwd, &["merge-base", "--is-ancestor", &sha, &base_ref])
                .await?;
        let state = if merged {
            PrState::Merged
        } else if head_tip.is_none() {
            PrState::Closed
        } else {
            PrState::Open
        };
        let pr = &mut prs[idx];
        if let Some(t) = head_tip {
            pr.head_sha = t;
        }
        if state != pr.state {
            pr.state = state;
            pr.updated_at = Utc::now();
        }
        self.save(&prs).await?;
        Ok(state)
    }

    async fn git_auth(&self, _repo_url: &str) -> Result<GitAuth> {
        Ok(GitAuth::None)
    }
}
