//! Error type for every fallible operation in this crate.

use std::path::PathBuf;

/// Result alias used throughout `kiln-gitops`.
pub type Result<T, E = GitError> = std::result::Result<T, E>;

/// Errors from git plumbing and PR providers.
///
/// Messages never contain secrets: URLs are redacted before they are put in
/// an error and credentials are never passed on a command line.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// Filesystem or process-spawn failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A `git` subprocess exited non-zero.
    #[error("`git {command}` failed (exit {code:?}): {stderr}")]
    Command {
        /// The sub-command and its (non-secret) arguments.
        command: String,
        /// Exit code, `None` if killed by a signal.
        code: Option<i32>,
        /// Trimmed, redacted standard error.
        stderr: String,
    },

    /// A `git` subprocess exceeded its time budget and was killed.
    #[error("`git {command}` timed out after {secs}s")]
    Timeout {
        /// The sub-command that timed out.
        command: String,
        /// The budget that was exceeded.
        secs: u64,
    },

    /// A guarded operation was refused (protected branch, force push, …).
    #[error("refused: {0}")]
    Refused(String),

    /// The repository URL could not be understood.
    #[error("unsupported repository url: {0}")]
    InvalidUrl(String),

    /// A revision did not resolve in the mirror.
    #[error("revision not found: {0}")]
    RevNotFound(String),

    /// The path is not a git worktree managed by Kiln.
    #[error("not a git worktree: {}", .0.display())]
    NotAWorktree(PathBuf),

    /// Read-only repository access through gitoxide failed.
    #[error("gix: {0}")]
    Gix(String),

    /// The GitHub API answered with an unexpected status.
    #[error("GitHub API {method} {path} → {status}: {message}")]
    Http {
        /// HTTP method.
        method: &'static str,
        /// API path (no query string, no host).
        path: String,
        /// HTTP status code.
        status: u16,
        /// GitHub's `message` field, or a truncated body.
        message: String,
    },

    /// Transport-level HTTP failure.
    #[error("http transport: {0}")]
    Transport(#[from] reqwest::Error),

    /// JWT signing failed (bad private key, …).
    #[error("github app jwt: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),

    /// JSON (de)serialization failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// Something the provider needs is missing (no installation, no PR, …).
    #[error("not found: {0}")]
    NotFound(String),

    /// Provider configuration is unusable (no token, bad app id, …).
    #[error("configuration: {0}")]
    Config(String),
}

impl GitError {
    /// Shorthand for a `Gix` error from anything displayable.
    pub(crate) fn gix(e: impl std::fmt::Display) -> Self {
        GitError::Gix(e.to_string())
    }
}
