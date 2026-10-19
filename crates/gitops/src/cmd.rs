//! Thin async wrapper around the `git` CLI plus credential injection.

use crate::error::{GitError, Result};
use crate::url::{RepoUrl, redact_url};
use base64::Engine as _;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Credentials for talking to a remote.
///
/// Secrets never appear on a command line: for HTTPS remotes the token is
/// passed as an `Authorization` header through git's environment-config
/// mechanism (`GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`),
/// scoped to the remote's `scheme://host/` so it is never sent elsewhere.
/// SSH and local remotes ignore it.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum GitAuth {
    /// No credentials (public HTTPS, SSH keys, local paths).
    #[default]
    None,
    /// A GitHub installation token or PAT, sent as HTTP Basic
    /// `x-access-token:<token>`.
    Token(String),
}

impl GitAuth {
    /// Construct token auth.
    pub fn token(token: impl Into<String>) -> Self {
        GitAuth::Token(token.into())
    }

    /// Environment variables that make `git` authenticate to `url`.
    pub(crate) fn env_for(&self, url: &RepoUrl) -> Vec<(String, String)> {
        let (GitAuth::Token(token), RepoUrl::Http { origin, .. }) = (self, url) else {
            return Vec::new();
        };
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        vec![
            ("GIT_CONFIG_COUNT".into(), "2".into()),
            (
                "GIT_CONFIG_KEY_0".into(),
                format!("http.{origin}.extraHeader"),
            ),
            (
                "GIT_CONFIG_VALUE_0".into(),
                format!("Authorization: Basic {basic}"),
            ),
            // Reset any host credential helper so it neither prompts nor stores.
            ("GIT_CONFIG_KEY_1".into(), "credential.helper".into()),
            ("GIT_CONFIG_VALUE_1".into(), String::new()),
        ]
    }
}

impl fmt::Debug for GitAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitAuth::None => f.write_str("GitAuth::None"),
            GitAuth::Token(_) => f.write_str("GitAuth::Token(***)"),
        }
    }
}

/// How to invoke git.
#[derive(Clone, Debug)]
pub(crate) struct Git {
    pub bin: PathBuf,
    /// Budget for local operations.
    pub local_timeout: Duration,
    /// Budget for operations that talk to a remote.
    pub net_timeout: Duration,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("git"),
            local_timeout: Duration::from_secs(120),
            net_timeout: Duration::from_secs(900),
        }
    }
}

/// Captured process output.
pub(crate) struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Git {
    /// Run git and return the captured output regardless of exit status.
    pub async fn output(
        &self,
        cwd: &Path,
        args: &[&str],
        env: &[(String, String)],
        net: bool,
    ) -> Result<Output> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // Never inherit a repository selection from our own environment.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env("SSH_ASKPASS", "true")
            .env("LC_ALL", "C");
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        if !env.iter().any(|(k, _)| k == "GIT_CONFIG_COUNT") {
            cmd.env_remove("GIT_CONFIG_COUNT");
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let budget = if net {
            self.net_timeout
        } else {
            self.local_timeout
        };
        let child = cmd.spawn()?;
        let out = match tokio::time::timeout(budget, child.wait_with_output()).await {
            Ok(r) => r?,
            Err(_) => {
                return Err(GitError::Timeout {
                    command: describe(args),
                    secs: budget.as_secs(),
                });
            }
        };
        Ok(Output {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Run git, failing on a non-zero exit; returns stdout.
    pub async fn run(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        self.run_env(cwd, args, &[], false).await
    }

    /// Like [`Git::run`] with extra environment and an explicit timeout class.
    pub async fn run_env(
        &self,
        cwd: &Path,
        args: &[&str],
        env: &[(String, String)],
        net: bool,
    ) -> Result<String> {
        let out = self.output(cwd, args, env, net).await?;
        if out.code == Some(0) {
            Ok(out.stdout)
        } else {
            Err(command_error(args, &out))
        }
    }

    /// Run git and map exit 0 → `true`, exit 1 → `false`, anything else → error.
    /// For predicates such as `merge-base --is-ancestor`.
    pub async fn check(&self, cwd: &Path, args: &[&str]) -> Result<bool> {
        let out = self.output(cwd, args, &[], false).await?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(command_error(args, &out)),
        }
    }

    /// Run git and return whether it exited 0 (any failure → `false`).
    pub async fn succeeds(&self, cwd: &Path, args: &[&str]) -> bool {
        matches!(self.output(cwd, args, &[], false).await, Ok(o) if o.code == Some(0))
    }
}

fn describe(args: &[&str]) -> String {
    redact_url(&args.join(" "))
}

fn command_error(args: &[&str], out: &Output) -> GitError {
    let mut stderr = redact_url(out.stderr.trim());
    if stderr.len() > 2000 {
        let mut cut = 2000;
        while !stderr.is_char_boundary(cut) {
            cut -= 1;
        }
        stderr.truncate(cut);
        stderr.push('…');
    }
    GitError::Command {
        command: describe(args),
        code: out.code,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_env_is_scoped_and_debug_redacted() {
        let auth = GitAuth::token("ghs_SECRET");
        assert_eq!(format!("{auth:?}"), "GitAuth::Token(***)");
        let env = auth.env_for(&RepoUrl::parse("https://github.com/a/b.git").unwrap());
        assert_eq!(env[1].1, "http.https://github.com/.extraHeader");
        assert!(env[2].1.starts_with("Authorization: Basic "));
        assert!(!env.iter().any(|(_, v)| v.contains("ghs_SECRET")));
        assert!(
            auth.env_for(&RepoUrl::parse("git@github.com:a/b.git").unwrap())
                .is_empty()
        );
        assert!(auth.env_for(&RepoUrl::parse("/tmp/x").unwrap()).is_empty());
    }
}
