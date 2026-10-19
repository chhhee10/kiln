//! Pull-request provider abstraction.

use crate::cmd::GitAuth;
use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A pull request to open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPr {
    /// `Project::repo_url`.
    pub repo_url: String,
    /// Target branch (the project's default branch).
    pub base: String,
    /// Source branch (`kiln/…`), already pushed.
    pub head: String,
    /// PR title, e.g. `FP-142: Retry backoff`.
    pub title: String,
    /// Markdown body (see [`crate::render_pr_body`]).
    pub body: String,
    /// Open as a draft.
    #[serde(default)]
    pub draft: bool,
}

/// Reference to an opened PR.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    /// Provider-scoped PR number.
    pub number: u64,
    /// Browser URL.
    pub url: String,
}

/// Lifecycle state of a PR, as stored in `pull_requests.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    /// Open and not merged.
    Open,
    /// Merged into the base branch.
    Merged,
    /// Closed without merging.
    Closed,
}

impl PrState {
    /// `open` | `merged` | `closed`.
    pub fn as_str(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }
    }

    /// Inverse of [`PrState::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(PrState::Open),
            "merged" => Some(PrState::Merged),
            "closed" => Some(PrState::Closed),
            _ => None,
        }
    }

    /// True once the PR can no longer change (merged or closed).
    pub fn is_final(self) -> bool {
        !matches!(self, PrState::Open)
    }
}

/// Opens PRs, reports their state (merge polling) and supplies push
/// credentials for a repository. Object-safe: the daemon holds a
/// `Box<dyn PrProvider>` chosen by configuration.
#[async_trait]
pub trait PrProvider: Send + Sync {
    /// Stable provider id stored in `pull_requests.provider`
    /// (`github_app` | `github_token` | `local`).
    fn id(&self) -> &'static str;

    /// Open a PR. Idempotent: if an open PR already exists for
    /// `pr.head`, that PR is returned instead of failing.
    async fn open_pr(&self, pr: &NewPr) -> Result<PrRef>;

    /// Current state of PR `number` in `repo_url`.
    async fn pr_state(&self, repo_url: &str, number: u64) -> Result<PrState>;

    /// Credentials for fetching from / pushing to `repo_url`.
    async fn git_auth(&self, repo_url: &str) -> Result<GitAuth>;
}
