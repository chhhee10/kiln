//! Shared application state handed to every handler.

use crate::config::Config;
use kiln_auth::Auth;
use kiln_events::Bus;
use kiln_gitops::GitOps;
use kiln_memory::Memory;
use kiln_notifier::Notifier;
use kiln_scheduler::Scheduler;
use kiln_store::Store;
use kiln_types::api::{HarnessCatalog, ServerInfo};
use kiln_types::memory::Visibility;
use kiln_types::plan::WorkerChoice;
use kiln_types::{UserSettings, WorkerRole};
use kiln_vault::Vault;
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Cheaply clonable handle to [`Inner`].
#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

/// Everything the HTTP layer needs.
pub struct Inner {
    pub cfg: Config,
    pub dev: bool,
    pub store: Store,
    pub auth: Arc<Auth>,
    pub vault: Arc<Vault>,
    pub bus: Bus,
    pub notifier: Arc<Notifier>,
    pub memory: Arc<Memory>,
    pub scheduler: Arc<Scheduler>,
    pub gitops: Arc<GitOps>,
    pub catalog: HarnessCatalog,
    /// Config defaults for every role (pipeline + planner + distiller).
    pub default_workers: BTreeMap<WorkerRole, WorkerChoice>,
    /// URL clients reach us at (pairing QR, PR links).
    pub public_url: String,
    pub hostname: String,
    pub tailnet_host: Option<String>,
    /// Directory with the web build's `index.html`, if any.
    pub web_dir: Option<PathBuf>,
    pub limiter: RateLimiter,
    /// Cancelled on shutdown: WebSocket loops exit.
    pub shutdown: CancellationToken,
    pub started: Instant,
}

impl Inner {
    /// Whether session cookies get the `Secure` attribute.
    pub fn secure_cookies(&self) -> bool {
        !self.dev || self.public_url.starts_with("https://")
    }

    /// `/info` payload.
    pub async fn server_info(&self) -> Result<ServerInfo, crate::error::AppError> {
        Ok(ServerInfo {
            product: kiln_types::PRODUCT_NAME.into(),
            version: kiln_types::VERSION.into(),
            setup_required: self.auth.setup_required().await?,
            allowed_domain: self.auth.allowed_domain().await?,
            hostname: self.hostname.clone(),
            tailnet_host: self.tailnet_host.clone(),
        })
    }

    /// Settings for a user who has not saved any.
    pub fn default_settings(&self) -> UserSettings {
        UserSettings {
            worker_defaults: self.default_workers.clone(),
            default_visibility: Visibility::Private,
            planner_reads_company: true,
            notification_channels: ["decisions", "failures", "ready_for_review", "live_workers"]
                .into_iter()
                .map(|c| (c.to_string(), true))
                .collect(),
        }
    }

    /// The user's settings, creating the defaults on first access so the
    /// scheduler always finds a row.
    pub async fn settings_for(
        &self,
        user: kiln_types::UserId,
    ) -> Result<UserSettings, crate::error::AppError> {
        if let Some(s) = self.store.get_user_settings(user).await? {
            return Ok(s);
        }
        let s = self.default_settings();
        self.store.set_user_settings(user, &s).await?;
        Ok(s)
    }
}

/// Fixed-window per-IP limiter for unauthenticated auth endpoints.
pub struct RateLimiter {
    per_minute: u32,
    hits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute: per_minute.max(1),
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Count one attempt from `ip`; false when over the limit.
    pub fn check(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut g = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() > 10_000 {
            g.retain(|_, (t, _)| now.duration_since(*t) < Duration::from_secs(60));
        }
        let e = g.entry(ip).or_insert((now, 0));
        if now.duration_since(e.0) >= Duration::from_secs(60) {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= self.per_minute
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_blocks_after_quota() {
        let l = RateLimiter::new(3);
        let ip: IpAddr = "100.64.0.9".parse().unwrap();
        assert!(l.check(ip) && l.check(ip) && l.check(ip));
        assert!(!l.check(ip));
        assert!(l.check("100.64.0.10".parse().unwrap()));
    }
}
