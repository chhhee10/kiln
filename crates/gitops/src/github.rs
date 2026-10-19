//! GitHub PR providers: [`GitHubApp`] (production) and [`GitHubToken`]
//! (PAT / `gh auth token`, for development). Both use the REST API directly
//! through `reqwest`; the API base URL is configurable so tests can point it
//! at a local mock server.

use crate::cmd::GitAuth;
use crate::error::{GitError, Result};
use crate::pr::{NewPr, PrProvider, PrRef, PrState};
use crate::url::github_owner_repo;
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

/// Public GitHub REST API base.
pub const GITHUB_API: &str = "https://api.github.com";

/// Installation tokens are refreshed this long before they expire.
pub const TOKEN_REFRESH_MARGIN_SECS: i64 = 5 * 60;

// ---------------------------------------------------------------- REST core

#[derive(Clone)]
struct Rest {
    http: reqwest::Client,
    base: String,
}

impl Rest {
    fn new(base: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kiln/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds with static config");
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// Send a request; returns status and parsed JSON body (`Null` if empty).
    async fn raw(
        &self,
        method: Method,
        path: &str,
        bearer: &str,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .bearer_auth(bearer);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                Value::String(String::from_utf8_lossy(&bytes).chars().take(300).collect())
            })
        };
        Ok((status, value))
    }

    async fn json(
        &self,
        method: Method,
        path: &str,
        bearer: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let m = method_name(&method);
        let (status, value) = self.raw(method, path, bearer, body).await?;
        if status.is_success() {
            Ok(value)
        } else {
            Err(http_error(m, path, status, &value))
        }
    }
}

fn method_name(m: &Method) -> &'static str {
    match *m {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PATCH => "PATCH",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        _ => "HTTP",
    }
}

fn http_error(method: &'static str, path: &str, status: StatusCode, body: &Value) -> GitError {
    let message = match body {
        Value::Object(o) => o
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let path = path.split('?').next().unwrap_or(path).to_string();
    GitError::Http {
        method,
        path,
        status: status.as_u16(),
        message,
    }
}

/// Percent-encode a query-string value (RFC 3986 unreserved kept).
fn qs(v: &str) -> String {
    let mut out = String::new();
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn owner_repo(url: &str) -> Result<(String, String)> {
    github_owner_repo(url).ok_or_else(|| {
        GitError::InvalidUrl(format!(
            "not a GitHub repository: {}",
            crate::url::redact_url(url)
        ))
    })
}

#[derive(Deserialize)]
struct PullJson {
    number: u64,
    html_url: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: Option<bool>,
    #[serde(default)]
    merged_at: Option<String>,
}

async fn open_pr_with(rest: &Rest, token: &str, pr: &NewPr) -> Result<PrRef> {
    let (owner, repo) = owner_repo(&pr.repo_url)?;
    let path = format!("/repos/{owner}/{repo}/pulls");
    let body = json!({ "title": pr.title, "head": pr.head, "base": pr.base, "body": pr.body, "draft": pr.draft });
    let (status, value) = rest.raw(Method::POST, &path, token, Some(&body)).await?;
    if status.is_success() {
        let p: PullJson = serde_json::from_value(value)?;
        return Ok(PrRef {
            number: p.number,
            url: p.html_url,
        });
    }
    if status == StatusCode::UNPROCESSABLE_ENTITY && value.to_string().contains("already exists") {
        let list_path = format!(
            "{path}?state=open&head={}",
            qs(&format!("{owner}:{}", pr.head))
        );
        let list = rest.json(Method::GET, &list_path, token, None).await?;
        let pulls: Vec<PullJson> = serde_json::from_value(list)?;
        if let Some(p) = pulls.into_iter().next() {
            tracing::info!(number = p.number, head = %pr.head, "PR already open; reusing");
            return Ok(PrRef {
                number: p.number,
                url: p.html_url,
            });
        }
    }
    Err(http_error("POST", &path, status, &value))
}

async fn pr_state_with(rest: &Rest, token: &str, repo_url: &str, number: u64) -> Result<PrState> {
    let (owner, repo) = owner_repo(repo_url)?;
    let path = format!("/repos/{owner}/{repo}/pulls/{number}");
    let (status, value) = rest.raw(Method::GET, &path, token, None).await?;
    if status == StatusCode::NOT_FOUND {
        return Err(GitError::NotFound(format!(
            "PR #{number} in {owner}/{repo}"
        )));
    }
    if !status.is_success() {
        return Err(http_error("GET", &path, status, &value));
    }
    let p: PullJson = serde_json::from_value(value)?;
    Ok(if p.merged.unwrap_or(false) || p.merged_at.is_some() {
        PrState::Merged
    } else if p.state == "closed" {
        PrState::Closed
    } else {
        PrState::Open
    })
}

// ---------------------------------------------------------------- GitHub App

#[derive(Serialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: String,
}

#[derive(Clone)]
struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

/// GitHub App provider (PRD default): signs an RS256 app JWT, finds the
/// installation covering the repository, and mints installation access
/// tokens scoped to that single repository. Tokens are cached until
/// [`TOKEN_REFRESH_MARGIN_SECS`] before expiry.
pub struct GitHubApp {
    app_id: u64,
    key: EncodingKey,
    rest: Rest,
    installations: tokio::sync::Mutex<HashMap<String, u64>>,
    tokens: tokio::sync::Mutex<HashMap<String, CachedToken>>,
}

impl fmt::Debug for GitHubApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubApp")
            .field("app_id", &self.app_id)
            .field("api_base", &self.rest.base)
            .finish_non_exhaustive()
    }
}

impl GitHubApp {
    /// Create from the App id and its PEM private key (PKCS#1 or PKCS#8,
    /// as downloaded from GitHub).
    pub fn new(app_id: u64, private_key_pem: &[u8]) -> Result<Self> {
        if app_id == 0 {
            return Err(GitError::Config("GitHub App id must be non-zero".into()));
        }
        let key = EncodingKey::from_rsa_pem(private_key_pem)?;
        Ok(Self {
            app_id,
            key,
            rest: Rest::new(GITHUB_API),
            installations: Default::default(),
            tokens: Default::default(),
        })
    }

    /// Point at a different API base (GitHub Enterprise, or a test mock).
    pub fn with_api_base(mut self, base: &str) -> Self {
        self.rest = Rest::new(base);
        self
    }

    /// The App id.
    pub fn app_id(&self) -> u64 {
        self.app_id
    }

    /// A fresh App JWT: `iat` 60 s in the past (clock skew), `exp` 9 min ahead.
    pub fn app_jwt(&self) -> Result<String> {
        let now = Utc::now().timestamp();
        let claims = AppClaims {
            iat: now - 60,
            exp: now + 9 * 60,
            iss: self.app_id.to_string(),
        };
        Ok(jsonwebtoken::encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &self.key,
        )?)
    }

    /// Installation id for `owner/repo`, cached. Uses
    /// `GET /repos/{owner}/{repo}/installation`, falling back to scanning
    /// `GET /app/installations` for an account matching `owner`.
    pub async fn installation_id(&self, owner: &str, repo: &str) -> Result<u64> {
        let cache_key = format!("{owner}/{repo}").to_ascii_lowercase();
        let mut cache = self.installations.lock().await;
        if let Some(id) = cache.get(&cache_key) {
            return Ok(*id);
        }
        let jwt = self.app_jwt()?;
        let path = format!("/repos/{owner}/{repo}/installation");
        let (status, value) = self.rest.raw(Method::GET, &path, &jwt, None).await?;
        let id = if status.is_success() {
            value.get("id").and_then(Value::as_u64)
        } else if status == StatusCode::NOT_FOUND {
            self.scan_installations(&jwt, owner).await?
        } else {
            return Err(http_error("GET", &path, status, &value));
        };
        let id = id.ok_or_else(|| {
            GitError::NotFound(format!("GitHub App installation for {owner}/{repo}"))
        })?;
        cache.insert(cache_key, id);
        Ok(id)
    }

    async fn scan_installations(&self, jwt: &str, owner: &str) -> Result<Option<u64>> {
        for page in 1..=20 {
            let list = self
                .rest
                .json(
                    Method::GET,
                    &format!("/app/installations?per_page=100&page={page}"),
                    jwt,
                    None,
                )
                .await?;
            let Some(items) = list.as_array() else { break };
            if items.is_empty() {
                break;
            }
            let hit = items.iter().find(|i| {
                i.pointer("/account/login")
                    .and_then(Value::as_str)
                    .is_some_and(|l| l.eq_ignore_ascii_case(owner))
            });
            if let Some(i) = hit {
                return Ok(i.get("id").and_then(Value::as_u64));
            }
            if items.len() < 100 {
                break;
            }
        }
        Ok(None)
    }

    /// An installation access token for the repository at `repo_url`,
    /// restricted to that repository. Served from cache while it has more
    /// than [`TOKEN_REFRESH_MARGIN_SECS`] left.
    pub async fn installation_token(&self, repo_url: &str) -> Result<String> {
        let (owner, repo) = owner_repo(repo_url)?;
        let cache_key = format!("{owner}/{repo}").to_ascii_lowercase();
        // Holding the lock across the refresh dedupes concurrent requests.
        let mut tokens = self.tokens.lock().await;
        if let Some(t) = tokens.get(&cache_key)
            && t.expires_at - Utc::now() > ChronoDuration::seconds(TOKEN_REFRESH_MARGIN_SECS)
        {
            return Ok(t.token.clone());
        }
        let installation = self.installation_id(&owner, &repo).await?;
        let jwt = self.app_jwt()?;
        let path = format!("/app/installations/{installation}/access_tokens");
        let (status, value) = self
            .rest
            .raw(
                Method::POST,
                &path,
                &jwt,
                Some(&json!({ "repositories": [repo] })),
            )
            .await?;
        if status == StatusCode::NOT_FOUND {
            // The installation went away; forget it so the next call re-resolves.
            self.installations.lock().await.remove(&cache_key);
        }
        if !status.is_success() {
            return Err(http_error("POST", &path, status, &value));
        }
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| GitError::Config("access_tokens response has no token".into()))?
            .to_string();
        let expires_at = value
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(55));
        tokens.insert(
            cache_key,
            CachedToken {
                token: token.clone(),
                expires_at,
            },
        );
        tracing::debug!(%owner, %repo, %expires_at, "minted installation token");
        Ok(token)
    }
}

#[async_trait]
impl PrProvider for GitHubApp {
    fn id(&self) -> &'static str {
        "github_app"
    }
    async fn open_pr(&self, pr: &NewPr) -> Result<PrRef> {
        let token = self.installation_token(&pr.repo_url).await?;
        open_pr_with(&self.rest, &token, pr).await
    }
    async fn pr_state(&self, repo_url: &str, number: u64) -> Result<PrState> {
        let token = self.installation_token(repo_url).await?;
        pr_state_with(&self.rest, &token, repo_url, number).await
    }
    async fn git_auth(&self, repo_url: &str) -> Result<GitAuth> {
        Ok(GitAuth::Token(self.installation_token(repo_url).await?))
    }
}

// ---------------------------------------------------------------- token

/// PAT / `gh auth token` provider for development setups.
pub struct GitHubToken {
    token: String,
    rest: Rest,
}

impl fmt::Debug for GitHubToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubToken")
            .field("token", &"***")
            .field("api_base", &self.rest.base)
            .finish()
    }
}

impl GitHubToken {
    /// Use an explicit token.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            rest: Rest::new(GITHUB_API),
        }
    }

    /// `GH_TOKEN` or `GITHUB_TOKEN` from the environment.
    pub fn from_env() -> Option<Self> {
        ["GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
            .map(|t| Self::new(t.trim()))
    }

    /// Ask the GitHub CLI (`gh auth token`).
    pub async fn from_gh_cli() -> Result<Self> {
        let out = tokio::process::Command::new("gh")
            .args(["auth", "token"])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| GitError::Config(format!("could not run `gh auth token`: {e}")))?;
        let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() || token.is_empty() {
            return Err(GitError::Config(
                "`gh auth token` returned no token (run `gh auth login`)".into(),
            ));
        }
        Ok(Self::new(token))
    }

    /// Point at a different API base (GitHub Enterprise, or a test mock).
    pub fn with_api_base(mut self, base: &str) -> Self {
        self.rest = Rest::new(base);
        self
    }
}

#[async_trait]
impl PrProvider for GitHubToken {
    fn id(&self) -> &'static str {
        "github_token"
    }
    async fn open_pr(&self, pr: &NewPr) -> Result<PrRef> {
        open_pr_with(&self.rest, &self.token, pr).await
    }
    async fn pr_state(&self, repo_url: &str, number: u64) -> Result<PrState> {
        pr_state_with(&self.rest, &self.token, repo_url, number).await
    }
    async fn git_auth(&self, _repo_url: &str) -> Result<GitAuth> {
        Ok(GitAuth::Token(self.token.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encoding() {
        assert_eq!(qs("acme:kiln/fp-1-x"), "acme%3Akiln/fp-1-x");
    }

    #[test]
    fn debug_hides_token() {
        let t = GitHubToken::new("ghp_SECRET");
        assert!(!format!("{t:?}").contains("SECRET"));
    }
}
