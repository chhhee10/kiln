//! Repository URL parsing.
//!
//! Kiln accepts three shapes of `Project::repo_url`:
//!
//! * HTTPS — `https://github.com/org/repo` or `https://github.com/org/repo.git`.
//!   Credentials are injected per command through git's environment config.
//! * SSH — `git@github.com:org/repo.git` or `ssh://git@github.com/org/repo.git`.
//!   No credential injection; the host's SSH agent/keys apply.
//! * Local — an absolute/relative filesystem path or a `file://` URL. Used by
//!   tests and by `LocalPrs`; never authenticated.

use crate::error::{GitError, Result};
use std::path::PathBuf;

/// How a repository is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoUrl {
    /// `http(s)://host/path`. `origin` is `scheme://host[:port]/` (used to
    /// scope injected auth headers to that host only).
    Http {
        /// The URL as given, with any embedded userinfo removed.
        url: String,
        /// `scheme://host[:port]/`.
        origin: String,
        /// Host name without port.
        host: String,
        /// Path after the host, without leading `/` or trailing `.git`.
        path: String,
    },
    /// scp-style `user@host:path` or `ssh://user@host/path`.
    Ssh {
        /// The URL as given.
        url: String,
        /// Host name without port.
        host: String,
        /// Path, without leading `/` or trailing `.git`.
        path: String,
    },
    /// Local path or `file://` URL.
    Local {
        /// The filesystem path.
        path: PathBuf,
    },
}

impl RepoUrl {
    /// Parse a repository URL.
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(GitError::InvalidUrl("empty".into()));
        }
        if let Some(rest) = s.strip_prefix("file://") {
            return Ok(RepoUrl::Local {
                path: PathBuf::from(rest),
            });
        }
        for scheme in ["https://", "http://"] {
            if let Some(rest) = s.strip_prefix(scheme) {
                let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
                let hostport = authority
                    .rsplit_once('@')
                    .map(|(_, h)| h)
                    .unwrap_or(authority);
                if hostport.is_empty() {
                    return Err(GitError::InvalidUrl(redact_url(s)));
                }
                let host = hostport
                    .split(':')
                    .next()
                    .unwrap_or(hostport)
                    .to_ascii_lowercase();
                return Ok(RepoUrl::Http {
                    url: format!("{scheme}{hostport}/{path}"),
                    origin: format!("{scheme}{hostport}/"),
                    host,
                    path: clean_path(path),
                });
            }
        }
        if let Some(rest) = s.strip_prefix("ssh://") {
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let hostport = authority
                .rsplit_once('@')
                .map(|(_, h)| h)
                .unwrap_or(authority);
            let host = hostport
                .split(':')
                .next()
                .unwrap_or(hostport)
                .to_ascii_lowercase();
            if host.is_empty() {
                return Err(GitError::InvalidUrl(s.to_string()));
            }
            return Ok(RepoUrl::Ssh {
                url: s.to_string(),
                host,
                path: clean_path(path),
            });
        }
        if s.contains("://") {
            return Err(GitError::InvalidUrl(redact_url(s)));
        }
        // scp-like syntax: `[user@]host:path`, where host contains no '/'.
        // Git treats it as a path if a '/' appears before the first ':'.
        if let Some((before, path)) = s.split_once(':') {
            let looks_scp = !before.contains('/') && !before.is_empty() && before.len() > 1; // "C:" is a drive
            if looks_scp {
                let host = before
                    .rsplit_once('@')
                    .map(|(_, h)| h)
                    .unwrap_or(before)
                    .to_ascii_lowercase();
                return Ok(RepoUrl::Ssh {
                    url: s.to_string(),
                    host,
                    path: clean_path(path),
                });
            }
        }
        Ok(RepoUrl::Local {
            path: PathBuf::from(s),
        })
    }

    /// The URL git should use for `origin` (userinfo stripped for HTTP).
    pub fn git_url(&self) -> String {
        match self {
            RepoUrl::Http { url, .. } | RepoUrl::Ssh { url, .. } => url.clone(),
            RepoUrl::Local { path } => path.display().to_string(),
        }
    }

    /// `(owner, repo)` when this is a github.com URL (HTTPS or SSH).
    pub fn github_owner_repo(&self) -> Option<(String, String)> {
        let (host, path) = match self {
            RepoUrl::Http { host, path, .. } | RepoUrl::Ssh { host, path, .. } => (host, path),
            RepoUrl::Local { .. } => return None,
        };
        if host != "github.com" && host != "www.github.com" {
            return None;
        }
        let mut parts = path.split('/').filter(|p| !p.is_empty());
        let owner = parts.next()?;
        let repo = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        Some((owner.to_string(), repo.to_string()))
    }
}

fn clean_path(p: &str) -> String {
    let p = p.trim_matches('/');
    p.strip_suffix(".git")
        .unwrap_or(p)
        .trim_end_matches('/')
        .to_string()
}

/// Parse `(owner, repo)` from a GitHub HTTPS or SSH URL.
///
/// ```
/// use kiln_gitops::github_owner_repo;
/// assert_eq!(github_owner_repo("git@github.com:acme/web.git"), Some(("acme".into(), "web".into())));
/// assert_eq!(github_owner_repo("https://github.com/acme/web"), Some(("acme".into(), "web".into())));
/// assert_eq!(github_owner_repo("/tmp/repo"), None);
/// ```
pub fn github_owner_repo(url: &str) -> Option<(String, String)> {
    RepoUrl::parse(url).ok()?.github_owner_repo()
}

/// Remove `user:password@` from every URL in `s`, so error messages and logs
/// can never leak credentials embedded in a remote URL.
pub fn redact_url(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || c == '\'' || c == '"')
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https() {
        let u = RepoUrl::parse("https://github.com/Acme/Web.git").unwrap();
        assert_eq!(u.github_owner_repo(), Some(("Acme".into(), "Web".into())));
        let RepoUrl::Http { origin, host, .. } = &u else {
            panic!()
        };
        assert_eq!(origin, "https://github.com/");
        assert_eq!(host, "github.com");
        assert_eq!(
            github_owner_repo("https://github.com/acme/web/"),
            Some(("acme".into(), "web".into()))
        );
    }

    #[test]
    fn strips_userinfo() {
        let u = RepoUrl::parse("https://x-access-token:secret@github.com/a/b").unwrap();
        assert!(!u.git_url().contains("secret"));
        assert_eq!(u.github_owner_repo(), Some(("a".into(), "b".into())));
    }

    #[test]
    fn parses_ssh() {
        let u = RepoUrl::parse("git@github.com:acme/web.git").unwrap();
        assert!(matches!(u, RepoUrl::Ssh { .. }));
        assert_eq!(u.github_owner_repo(), Some(("acme".into(), "web".into())));
        let u = RepoUrl::parse("ssh://git@github.com/acme/web.git").unwrap();
        assert_eq!(u.github_owner_repo(), Some(("acme".into(), "web".into())));
    }

    #[test]
    fn parses_local() {
        assert!(matches!(
            RepoUrl::parse("/tmp/x/remote.git").unwrap(),
            RepoUrl::Local { .. }
        ));
        assert!(matches!(
            RepoUrl::parse("./rel/path").unwrap(),
            RepoUrl::Local { .. }
        ));
        let RepoUrl::Local { path } = RepoUrl::parse("file:///tmp/r").unwrap() else {
            panic!()
        };
        assert_eq!(path, PathBuf::from("/tmp/r"));
        assert_eq!(github_owner_repo("file:///tmp/r"), None);
    }

    #[test]
    fn non_github_host() {
        assert_eq!(github_owner_repo("https://gitlab.com/a/b"), None);
        assert!(RepoUrl::parse("ftp://x/y").is_err());
    }

    #[test]
    fn redacts() {
        assert_eq!(
            redact_url("fatal: https://u:p@github.com/a/b not found, also http://tok@h/x"),
            "fatal: https://***@github.com/a/b not found, also http://***@h/x"
        );
        assert_eq!(redact_url("no urls here"), "no urls here");
    }
}
