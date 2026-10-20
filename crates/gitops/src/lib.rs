//! `kiln-gitops`: git plumbing and pull requests for Kiln.
//!
//! * **Mirrors** — one bare repository per project at
//!   `<data>/repos/<KEY>.git`. Remote branches are fetched into
//!   `refs/remotes/origin/*`; `refs/heads/*` only ever holds Kiln task
//!   branches, so fetching can never clobber a branch a worker is using.
//! * **Worktrees** — one linked worktree per task at
//!   `<data>/worktrees/<TASK-KEY>` on branch `kiln/<key>-<slug>`, sharing the
//!   mirror's object store. [`Worktree::git_common_dir`] is the mirror, which
//!   containers mount at the same absolute path (decision #3).
//! * **Pushes** — only `kiln/…` branches, never the default branch, never
//!   forced.
//! * **PR providers** — [`PrProvider`] with [`GitHubApp`], [`GitHubToken`]
//!   and the offline [`LocalPrs`].
//!
//! # Why the git CLI (and where gitoxide is used)
//!
//! Mutating operations (`init`, `fetch`, `worktree add/remove/prune`,
//! `checkout`, `reset`, `push`) shell out to the `git` binary through
//! `tokio::process`: gitoxide's worktree creation and push support are not
//! complete, and the CLI gives identical behaviour to what users run by hand.
//! Read-only lookups that are simple object-database reads use `gix`
//! ([`GitOps::head`], [`GitOps::read_file_at`]). Diff statistics use
//! `git diff --numstat` so rename detection matches git exactly.
//!
//! # Credentials
//!
//! Secrets never appear on a command line (where `ps` would show them).
//! [`GitAuth::Token`] is passed to git as an `http.<origin>.extraHeader`
//! through the `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`
//! environment, scoped to the remote's host. Every git process runs with
//! `GIT_TERMINAL_PROMPT=0` and a no-op askpass so nothing can block on a
//! prompt, and URLs are redacted before they reach errors or logs.

#![warn(missing_docs)]

mod body;
mod branch;
mod cmd;
mod error;
mod github;
mod local;
mod ops;
mod pr;
mod url;

pub use body::{ArtifactLink, render_pr_body};
pub use branch::{BRANCH_PREFIX, MAX_BRANCH_TAIL, branch_name, is_kiln_branch, slugify};
pub use cmd::GitAuth;
pub use error::{GitError, Result};
pub use github::{GITHUB_API, GitHubApp, GitHubToken, TOKEN_REFRESH_MARGIN_SECS};
pub use local::{LocalPr, LocalPrs};
pub use ops::{GitOps, Worktree};
pub use pr::{NewPr, PrProvider, PrRef, PrState};
pub use url::{RepoUrl, github_owner_repo, redact_url};

/// Files Kiln generates into every worktree and keeps out of commits.
pub const GENERATED_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
