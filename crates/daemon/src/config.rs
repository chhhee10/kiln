//! `kiln.toml` loading (docs/SCHEDULER.md "Daemon (kilnd)").
//!
//! Resolution order for the file: `--config <path>` → `$KILN_CONFIG` →
//! `<data_dir>/kiln.toml` (where `<data_dir>` is `$KILN_DATA_DIR` or the
//! platform default). A missing file is fine: every field has a default.
//! After the file is parsed, `KILN_*` environment variables override
//! individual fields (see [`ENV_OVERRIDES`]). `~` at the start of any path is
//! expanded to `$HOME`.
//!
//! The example file `config/kiln.example.toml` documents every key.

use kiln_harness::Registry;
use kiln_types::WorkerRole;
use kiln_types::api::{HarnessCatalog, HarnessInfo};
use kiln_types::plan::WorkerChoice;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Default HTTP port.
pub const DEFAULT_PORT: u16 = 7420;
/// Default worker base image.
pub const DEFAULT_WORKER_IMAGE: &str =
    concat!("ghcr.io/chhhee10/kiln-worker:", env!("CARGO_PKG_VERSION"));
/// Longest run-socket path kilnd accepts. `sockaddr_un.sun_path` is 104
/// bytes on macOS (108 on Linux) including the NUL; 100 leaves headroom.
pub const MAX_SOCKET_PATH: usize = 100;
/// Config file name inside the data dir.
pub const CONFIG_FILE: &str = "kiln.toml";

/// Environment variables that override config fields, with the field they set.
pub const ENV_OVERRIDES: &[(&str, &str)] = &[
    ("KILN_CONFIG", "path of kiln.toml"),
    ("KILN_DATA_DIR", "data_dir"),
    ("KILN_WEB_DIR", "web_dir"),
    ("KILN_BIND", "server.bind"),
    ("KILN_PORT", "server.port"),
    ("KILN_PUBLIC_URL", "server.public_url"),
    ("KILN_RUNTIME", "runtime.kind"),
    ("KILN_WORKER_IMAGE", "runtime.worker_image"),
    ("KILN_RUNNER_BIN", "runtime.runner_bin"),
    ("KILN_GITHUB_MODE", "github.mode"),
    ("KILN_GITHUB_APP_ID", "github.app_id"),
    ("KILN_GITHUB_PRIVATE_KEY_PATH", "github.private_key_path"),
    ("KILN_NTFY_URL", "push.ntfy_url"),
    ("KILN_LLM_DISTILL", "memory.llm_distill"),
    ("KILN_LOG", "log.level"),
    ("KILN_LOG_FORMAT", "log.format"),
];

/// Errors while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid environment variable {var}: {message}")]
    Env { var: String, message: String },
    #[error("invalid config: {0}")]
    Invalid(String),
}

/// The whole `kiln.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Root of all persistent state (see `docs/ARCHITECTURE.md` "Data dir layout").
    pub data_dir: PathBuf,
    /// Built Leptos app served at `/app`. Default `<data_dir>/web`.
    pub web_dir: Option<PathBuf>,
    pub server: ServerConfig,
    pub runtime: RuntimeConfig,
    pub capacity: CapacityConfig,
    pub timeouts: TimeoutsConfig,
    pub github: GithubConfig,
    /// Harness catalog, keyed by adapter id. Entries here are merged over the
    /// built-in defaults (`claude_sdk`, `codex`, `opencode`, and `mock` in dev).
    pub harnesses: BTreeMap<String, HarnessConfig>,
    /// Default `harness:model` per worker role for users who haven't chosen.
    pub defaults: WorkerDefaults,
    pub memory: MemorySection,
    pub push: PushConfig,
    pub vault: VaultConfig,
    pub log: LogConfig,
    /// USD per million tokens, for harnesses that report tokens but not cost
    /// (Codex). Models routed through a LiteLLM credential are priced from
    /// the gateway's `/model/info` when not listed here.
    pub prices: BTreeMap<String, kiln_scheduler::pricing::ModelPrice>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            web_dir: None,
            server: ServerConfig::default(),
            runtime: RuntimeConfig::default(),
            capacity: CapacityConfig::default(),
            timeouts: TimeoutsConfig::default(),
            github: GithubConfig::default(),
            harnesses: BTreeMap::new(),
            defaults: WorkerDefaults::default(),
            memory: MemorySection::default(),
            push: PushConfig::default(),
            vault: VaultConfig::default(),
            log: LogConfig::default(),
            prices: BTreeMap::new(),
        }
    }
}

/// `[server]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// `"tailscale"` (default: the Tailscale IPv4 address), or an explicit
    /// `ip` / `ip:port`. `0.0.0.0` / `::` are always refused.
    pub bind: String,
    pub port: u16,
    /// URL clients use to reach the daemon (usually the `tailscale serve`
    /// HTTPS name). Encoded in pairing QR codes and PR artifact links.
    pub public_url: Option<String>,
    /// Maximum JSON request body, in KiB.
    pub max_body_kb: usize,
    /// Per-request timeout for API calls, in seconds (WebSocket and
    /// downloads stream past it).
    pub request_timeout_secs: u64,
    /// Login / register / pairing attempts allowed per client IP per minute.
    pub auth_rate_per_minute: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "tailscale".into(),
            port: DEFAULT_PORT,
            public_url: None,
            max_body_kb: 2048,
            request_timeout_secs: 30,
            auth_rate_per_minute: 20,
        }
    }
}

/// Container runtime kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    /// Docker-compatible API (Docker Engine, OrbStack, Podman).
    Docker,
    /// Host subprocesses, no isolation (development and tests only).
    Process,
}

/// `[runtime]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub kind: RuntimeKind,
    pub worker_image: String,
    /// Host path of `kiln-runner` for the process runtime. Default: next to
    /// the `kilnd` executable, else `kiln-runner` on `PATH`.
    pub runner_bin: Option<PathBuf>,
    /// Docker socket path override (else `DOCKER_HOST` / default sockets).
    pub docker_socket: Option<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            kind: RuntimeKind::Docker,
            worker_image: DEFAULT_WORKER_IMAGE.into(),
            runner_bin: None,
            docker_socket: None,
        }
    }
}

/// `[capacity]`: admission control inputs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapacityConfig {
    /// Machine RAM budget in MiB; 0 = detect host RAM.
    pub ram_mb: u64,
    /// RAM kept free for the OS and the daemon.
    pub reserve_mb: u64,
    pub max_containers: u32,
    pub max_tasks: u32,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            ram_mb: 0,
            reserve_mb: 6144,
            max_containers: 8,
            max_tasks: 4,
        }
    }
}

/// `[timeouts]`: per-role run timeouts in minutes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutsConfig {
    pub planner: u32,
    pub builder: u32,
    pub qa: u32,
    pub reviewer: u32,
    pub distiller: u32,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            planner: 15,
            builder: 60,
            qa: 45,
            reviewer: 20,
            distiller: 10,
        }
    }
}

/// How PRs are opened and how git authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubMode {
    /// GitHub App installation tokens.
    App,
    /// A personal access token (env var, or `gh auth token`).
    Token,
    /// No GitHub: PRs are recorded locally (dev, tests, air-gapped).
    Local,
}

/// `[github]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    pub mode: GithubMode,
    pub app_id: Option<u64>,
    pub private_key_path: Option<PathBuf>,
    /// Env var holding the token in `token` mode.
    pub token_env: String,
    /// In `token` mode, fall back to `gh auth token` when the env var is unset.
    pub use_gh_cli: bool,
    /// API base override (GitHub Enterprise).
    pub api_base: Option<String>,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            mode: GithubMode::App,
            app_id: None,
            private_key_path: None,
            token_env: "GITHUB_TOKEN".into(),
            use_gh_cli: true,
            api_base: None,
        }
    }
}

/// `[harnesses.<id>]`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarnessConfig {
    pub label: Option<String>,
    /// Model ids offered in pickers. Free-form: new models need no release.
    pub models: Option<Vec<String>>,
    pub experimental: Option<bool>,
    /// Set `false` to hide a built-in harness.
    pub enabled: Option<bool>,
    /// Passed to the runner as `RunConfig.options` for this harness.
    pub options: BTreeMap<String, String>,
}

/// `[defaults]`: `"harness:model"` per role.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerDefaults {
    pub planner: String,
    pub builder: String,
    pub qa: String,
    pub reviewer: String,
    pub distiller: String,
}

impl Default for WorkerDefaults {
    // Seed defaults (Settings → Worker defaults).
    fn default() -> Self {
        Self {
            planner: "claude_sdk:claude-opus-4.1".into(),
            builder: "claude_sdk:claude-sonnet-4.5".into(),
            qa: "codex:gpt-5".into(),
            reviewer: "claude_sdk:claude-opus-4.1".into(),
            distiller: "claude_sdk:claude-sonnet-4.5".into(),
        }
    }
}

impl WorkerDefaults {
    /// Every role mapped to `harness:model`.
    pub fn all_mock() -> Self {
        let m = "mock:mock-1".to_string();
        Self {
            planner: m.clone(),
            builder: m.clone(),
            qa: m.clone(),
            reviewer: m.clone(),
            distiller: m,
        }
    }

    fn entries(&self) -> [(WorkerRole, &str); 5] {
        [
            (WorkerRole::Planner, self.planner.as_str()),
            (WorkerRole::Builder, self.builder.as_str()),
            (WorkerRole::Qa, self.qa.as_str()),
            (WorkerRole::Reviewer, self.reviewer.as_str()),
            (WorkerRole::Distiller, self.distiller.as_str()),
        ]
    }

    /// Parsed choices. Errors name the offending role.
    pub fn resolve(&self) -> Result<BTreeMap<WorkerRole, WorkerChoice>, ConfigError> {
        self.entries()
            .into_iter()
            .map(|(role, s)| {
                parse_choice(s).map(|c| (role, c)).ok_or_else(|| {
                    ConfigError::Invalid(format!(
                        "defaults.{}: expected \"harness:model\", got {s:?}",
                        role.as_str()
                    ))
                })
            })
            .collect()
    }
}

/// Parse `"harness:model"`.
pub fn parse_choice(s: &str) -> Option<WorkerChoice> {
    let (h, m) = s.split_once(':')?;
    let (h, m) = (h.trim(), m.trim());
    (!h.is_empty() && !m.is_empty()).then(|| WorkerChoice::new(h, m))
}

/// `[memory]`: daemon-level switches plus every `kiln_memory::MemoryConfig` tunable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MemorySection {
    /// Run the Distiller container with the run owner's credentials (decision #1).
    pub llm_distill: bool,
    /// L0 raw-event retention.
    pub retention_days: u32,
    #[serde(flatten)]
    pub tuning: kiln_memory::MemoryConfig,
}

impl Default for MemorySection {
    fn default() -> Self {
        Self {
            llm_distill: true,
            retention_days: 75,
            tuning: kiln_memory::MemoryConfig::default(),
        }
    }
}

/// `[push]`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PushConfig {
    /// Self-hosted ntfy server advertised to Android clients (optional).
    pub ntfy_url: Option<String>,
}

/// `[vault]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VaultConfig {
    /// Store the master key in the OS keychain (falls back to `<data>/master.key`).
    pub keychain: bool,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self { keychain: true }
    }
}

/// `[log]`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// `tracing` env-filter directive, e.g. `"info"` or `"kiln_daemon=debug,info"`.
    pub level: String,
    /// `"pretty"` or `"json"`.
    pub format: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "pretty".into(),
        }
    }
}

// ---------------------------------------------------------------- paths

/// Expand a leading `~` to `$HOME`.
pub fn expand_tilde(p: &Path) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p.to_path_buf();
    };
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if s == "~" {
        return home().unwrap_or_else(|| p.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(h) = home()
    {
        return h.join(rest);
    }
    p.to_path_buf()
}

/// Platform default data dir: `~/Library/Application Support/kiln` on macOS,
/// `$XDG_DATA_HOME/kiln` or `~/.local/share/kiln` elsewhere.
pub fn default_data_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        return expand_tilde(Path::new("~/Library/Application Support/kiln"));
    }
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(x).join("kiln");
    }
    expand_tilde(Path::new("~/.local/share/kiln"))
}

// ---------------------------------------------------------------- loading

impl Config {
    /// Locate, parse and env-override the config. `explicit` is `--config`.
    /// Returns the config and the file it came from (if one existed).
    pub fn load(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>), ConfigError> {
        Self::load_with_env(explicit, &|k| std::env::var(k).ok())
    }

    /// [`Config::load`] with an injectable environment (tests).
    pub fn load_with_env(
        explicit: Option<&Path>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(Config, Option<PathBuf>), ConfigError> {
        let env_data_dir = env("KILN_DATA_DIR")
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from);
        let path = match (
            explicit,
            env("KILN_CONFIG").filter(|v| !v.trim().is_empty()),
        ) {
            (Some(p), _) => Some(expand_tilde(p)),
            (None, Some(p)) => Some(expand_tilde(Path::new(&p))),
            (None, None) => {
                let dd = env_data_dir
                    .clone()
                    .map(|d| expand_tilde(&d))
                    .unwrap_or_else(default_data_dir);
                let p = dd.join(CONFIG_FILE);
                p.exists().then_some(p)
            }
        };
        let mut cfg = match &path {
            Some(p) => {
                let text = std::fs::read_to_string(p).map_err(|source| ConfigError::Read {
                    path: p.clone(),
                    source,
                })?;
                Self::parse(&text).map_err(|message| ConfigError::Parse {
                    path: p.clone(),
                    message,
                })?
            }
            None => Config::default(),
        };
        cfg.apply_env(env)?;
        cfg.normalize();
        Ok((cfg, path))
    }

    /// Parse TOML text (no env overrides).
    pub fn parse(text: &str) -> Result<Config, String> {
        toml::from_str::<Config>(text).map_err(|e| e.to_string())
    }

    fn apply_env(&mut self, env: &dyn Fn(&str) -> Option<String>) -> Result<(), ConfigError> {
        let get = |k: &str| {
            env(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let bad = |var: &str, message: String| ConfigError::Env {
            var: var.into(),
            message,
        };
        if let Some(v) = get("KILN_DATA_DIR") {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = get("KILN_WEB_DIR") {
            self.web_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = get("KILN_BIND") {
            self.server.bind = v;
        }
        if let Some(v) = get("KILN_PORT") {
            self.server.port = v
                .parse()
                .map_err(|_| bad("KILN_PORT", format!("not a port: {v}")))?;
        }
        if let Some(v) = get("KILN_PUBLIC_URL") {
            self.server.public_url = Some(v);
        }
        if let Some(v) = get("KILN_RUNTIME") {
            self.runtime.kind = match v.as_str() {
                "docker" => RuntimeKind::Docker,
                "process" => RuntimeKind::Process,
                _ => return Err(bad("KILN_RUNTIME", "expected docker or process".into())),
            };
        }
        if let Some(v) = get("KILN_WORKER_IMAGE") {
            self.runtime.worker_image = v;
        }
        if let Some(v) = get("KILN_RUNNER_BIN") {
            self.runtime.runner_bin = Some(PathBuf::from(v));
        }
        if let Some(v) = get("KILN_GITHUB_MODE") {
            self.github.mode = match v.as_str() {
                "app" => GithubMode::App,
                "token" => GithubMode::Token,
                "local" => GithubMode::Local,
                _ => {
                    return Err(bad(
                        "KILN_GITHUB_MODE",
                        "expected app, token or local".into(),
                    ));
                }
            };
        }
        if let Some(v) = get("KILN_GITHUB_APP_ID") {
            self.github.app_id = Some(
                v.parse()
                    .map_err(|_| bad("KILN_GITHUB_APP_ID", "not a number".into()))?,
            );
        }
        if let Some(v) = get("KILN_GITHUB_PRIVATE_KEY_PATH") {
            self.github.private_key_path = Some(PathBuf::from(v));
        }
        if let Some(v) = get("KILN_NTFY_URL") {
            self.push.ntfy_url = Some(v);
        }
        if let Some(v) = get("KILN_LLM_DISTILL") {
            self.memory.llm_distill = matches!(v.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(v) = get("KILN_LOG") {
            self.log.level = v;
        }
        if let Some(v) = get("KILN_LOG_FORMAT") {
            self.log.format = v;
        }
        Ok(())
    }

    fn normalize(&mut self) {
        self.data_dir = expand_tilde(&self.data_dir);
        self.web_dir = self.web_dir.as_deref().map(expand_tilde);
        self.runtime.runner_bin = self.runtime.runner_bin.as_deref().map(expand_tilde);
        self.github.private_key_path = self.github.private_key_path.as_deref().map(expand_tilde);
        if let Some(u) = &mut self.server.public_url {
            *u = u.trim_end_matches('/').to_string();
        }
    }

    /// Check cross-field rules. `dev` enables the mock harness.
    pub fn validate(&self, dev: bool) -> Result<(), ConfigError> {
        if self.data_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid("data_dir is empty".into()));
        }
        if self.server.max_body_kb == 0 || self.server.request_timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "server.max_body_kb and server.request_timeout_secs must be > 0".into(),
            ));
        }
        if let Some(u) = &self.server.public_url
            && !(u.starts_with("https://") || u.starts_with("http://"))
        {
            return Err(ConfigError::Invalid(
                "server.public_url must start with http:// or https://".into(),
            ));
        }
        if self.github.mode == GithubMode::App
            && !dev
            && (self.github.app_id.is_none() || self.github.private_key_path.is_none())
        {
            return Err(ConfigError::Invalid(
                "github.mode = \"app\" needs github.app_id and github.private_key_path (or use mode = \"token\" / \"local\")".into(),
            ));
        }
        let catalog = self.harness_catalog(dev)?;
        for (role, choice) in self.defaults.resolve()? {
            if !catalog.harnesses.iter().any(|h| h.id == choice.harness) {
                return Err(ConfigError::Invalid(format!(
                    "defaults.{}: harness {:?} is not enabled{}",
                    role.as_str(),
                    choice.harness,
                    if choice.harness == "mock" {
                        " (mock is only available with --dev)"
                    } else {
                        ""
                    }
                )));
            }
        }
        Ok(())
    }

    /// Harness catalog served at `/harnesses`: built-in defaults merged with
    /// `[harnesses.*]`, restricted to adapters that exist. `mock` only in dev.
    pub fn harness_catalog(&self, dev: bool) -> Result<HarnessCatalog, ConfigError> {
        let registry: Registry = kiln_harness::registry();
        let mut merged: BTreeMap<String, HarnessInfo> = builtin_harnesses()
            .into_iter()
            .map(|h| (h.id.clone(), h))
            .collect();
        for (id, hc) in &self.harnesses {
            if registry.get(id).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "harnesses.{id}: no such adapter (known: {})",
                    registry.ids().join(", ")
                )));
            }
            let entry = merged.entry(id.clone()).or_insert_with(|| HarnessInfo {
                id: id.clone(),
                label: id.clone(),
                models: vec![],
                experimental: false,
            });
            if let Some(l) = &hc.label {
                entry.label = l.clone();
            }
            if let Some(m) = &hc.models {
                entry.models = m
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            if let Some(e) = hc.experimental {
                entry.experimental = e;
            }
            if hc.enabled == Some(false) {
                merged.remove(id);
            }
        }
        if !dev {
            merged.remove("mock");
        }
        // Stable, design-matching order.
        let order = ["claude_sdk", "codex", "opencode", "mock"];
        let mut harnesses: Vec<HarnessInfo> = merged.into_values().collect();
        harnesses.sort_by_key(|h| order.iter().position(|o| *o == h.id).unwrap_or(order.len()));
        let defaults = self
            .defaults
            .resolve()?
            .into_iter()
            .filter(|(r, _)| WorkerRole::PIPELINE.contains(r) || *r == WorkerRole::Planner)
            .collect();
        Ok(HarnessCatalog {
            harnesses,
            defaults,
        })
    }

    /// Per-harness options for `RunConfig.options`.
    pub fn harness_options(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        self.harnesses
            .iter()
            .filter(|(_, h)| !h.options.is_empty())
            .map(|(id, h)| (id.clone(), h.options.clone()))
            .collect()
    }

    /// Length of the longest per-run socket path,
    /// `<data_dir>/runs/<36-char uuid>/kiln.sock` (absolute).
    pub fn run_socket_path_len(&self) -> usize {
        let abs = std::path::absolute(&self.data_dir).unwrap_or_else(|_| self.data_dir.clone());
        abs.join("runs")
            .join("0".repeat(36))
            .join("kiln.sock")
            .as_os_str()
            .len()
    }

    /// Fail when run sockets would exceed [`MAX_SOCKET_PATH`].
    pub fn check_socket_path(&self) -> Result<(), ConfigError> {
        let n = self.run_socket_path_len();
        if n > MAX_SOCKET_PATH {
            return Err(ConfigError::Invalid(format!(
                "data_dir {} is too long: run sockets (<data_dir>/runs/<uuid>/kiln.sock) would be {n} bytes, \
                 over the {MAX_SOCKET_PATH}-byte Unix socket limit. Use a shorter data_dir, e.g. \
                 data_dir = \"~/.kiln\" or KILN_DATA_DIR=/usr/local/var/kiln",
                self.data_dir.display()
            )));
        }
        Ok(())
    }

    /// `<data>/web` unless `web_dir` is set.
    pub fn effective_web_dir(&self) -> PathBuf {
        self.web_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("web"))
    }
}

/// Built-in catalog. These model lists are **editable seed defaults**; override them with `[harnesses.<id>] models = [...]`.
pub fn builtin_harnesses() -> Vec<HarnessInfo> {
    let s = |v: &[&str]| v.iter().map(|m| m.to_string()).collect::<Vec<_>>();
    vec![
        HarnessInfo {
            id: "claude_sdk".into(),
            label: "Claude Agent SDK".into(),
            models: s(&["claude-sonnet-4.5", "claude-opus-4.1"]),
            experimental: false,
        },
        HarnessInfo {
            id: "codex".into(),
            label: "Codex".into(),
            models: s(&["gpt-5", "gpt-5-codex", "qwen3-coder"]),
            experimental: false,
        },
        HarnessInfo {
            id: "opencode".into(),
            label: "OpenCode".into(),
            models: s(&["qwen3-coder", "gpt-5", "claude-sonnet-4.5"]),
            experimental: true,
        },
        HarnessInfo {
            id: "mock".into(),
            label: "Mock (deterministic, dev only)".into(),
            models: s(&["mock-1"]),
            experimental: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(a, _)| *a == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn defaults_parse_and_validate_in_dev() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.server.port, DEFAULT_PORT);
        assert_eq!(c.server.bind, "tailscale");
        c.validate(true).unwrap();
        // Production needs GitHub App settings in app mode.
        assert!(c.validate(false).is_err());
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../config/kiln.example.toml");
        let c = Config::parse(text).unwrap();
        c.validate(true).unwrap();
        assert_eq!(c.memory.retention_days, 75);
    }

    #[test]
    fn env_overrides_and_tilde() {
        let env = env_of(&[
            ("KILN_DATA_DIR", "~/kiln-test"),
            ("KILN_PORT", "9000"),
            ("KILN_RUNTIME", "process"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k.toml");
        std::fs::write(&p, "[server]\nport = 1234\nbind = \"127.0.0.1\"\n").unwrap();
        let (c, path) = Config::load_with_env(Some(&p), &env).unwrap();
        assert_eq!(path.as_deref(), Some(p.as_path()));
        assert_eq!(c.server.port, 9000);
        assert_eq!(c.server.bind, "127.0.0.1");
        assert_eq!(c.runtime.kind, RuntimeKind::Process);
        if let Some(h) = std::env::var_os("HOME") {
            assert_eq!(c.data_dir, PathBuf::from(h).join("kiln-test"));
        }
        let bad = env_of(&[("KILN_PORT", "x")]);
        assert!(Config::load_with_env(Some(&p), &bad).is_err());
    }

    #[test]
    fn catalog_merges_and_hides_mock() {
        let c = Config::parse(
            "[harnesses.codex]\nmodels = [\"gpt-6\"]\n[harnesses.opencode]\nenabled = false\n",
        )
        .unwrap();
        let prod = c.harness_catalog(false).unwrap();
        let ids: Vec<_> = prod.harnesses.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["claude_sdk", "codex"]);
        assert_eq!(prod.harnesses[1].models, ["gpt-6"]);
        let dev = c.harness_catalog(true).unwrap();
        assert!(dev.harnesses.iter().any(|h| h.id == "mock"));
        assert!(
            Config::parse("[harnesses.nope]\n")
                .unwrap()
                .harness_catalog(true)
                .is_err()
        );
        assert!(Config::parse("bogus = 1").is_err());
    }

    #[test]
    fn socket_path_limit() {
        let mut c = Config {
            data_dir: PathBuf::from("/srv/kiln"),
            ..Default::default()
        };
        assert_eq!(
            c.run_socket_path_len(),
            "/srv/kiln/runs/".len() + 36 + "/kiln.sock".len()
        );
        c.check_socket_path().unwrap();
        c.data_dir = PathBuf::from(format!("/{}", "x".repeat(60)));
        let e = c.check_socket_path().unwrap_err().to_string();
        assert!(e.contains("shorter data_dir"), "{e}");
    }

    #[test]
    fn mock_defaults_need_dev() {
        let mut c = Config::default();
        c.github.mode = GithubMode::Local;
        c.defaults = WorkerDefaults::all_mock();
        assert!(c.validate(false).is_err());
        c.validate(true).unwrap();
        assert_eq!(
            parse_choice("codex:gpt-5"),
            Some(WorkerChoice::new("codex", "gpt-5"))
        );
        assert_eq!(parse_choice("codex"), None);
    }
}
