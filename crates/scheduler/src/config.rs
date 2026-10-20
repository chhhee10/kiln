//! Scheduler configuration (`SchedulerConfig`, `Capacity`, `RoleTimeouts`).

use kiln_types::WorkerRole;
use kiln_types::plan::WorkerChoice;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Default base worker image.
pub const DEFAULT_WORKER_IMAGE: &str = "ghcr.io/chhhee10/kiln-worker:0.1.0";

/// Machine capacity used by admission control. RAM-aware: the
/// defaults are derived from the host's RAM when `ram_mb` is 0.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capacity {
    /// Total RAM budget in MiB. `0` = detect the host's RAM.
    pub ram_mb: u64,
    /// RAM kept free for the OS, the daemon and the container runtime (MiB).
    pub reserve_mb: u64,
    /// Max concurrently running containers (≈8 on 32 GB).
    pub max_containers: u32,
    /// Max tasks in flight (Preparing … Looping), ≈4 on 32 GB.
    pub max_tasks: u32,
}

impl Default for Capacity {
    fn default() -> Self {
        Self {
            ram_mb: 0,
            reserve_mb: 6144,
            max_containers: 8,
            max_tasks: 4,
        }
    }
}

impl Capacity {
    /// Host RAM in MiB (via `sysinfo`), or 32 GiB if it cannot be read.
    pub fn host_ram_mb() -> u64 {
        let mut sys = sysinfo::System::new();
        sys.refresh_memory();
        let mb = sys.total_memory() / (1024 * 1024);
        if mb == 0 { 32 * 1024 } else { mb }
    }

    /// `ram_mb` with `0` resolved to the host's RAM.
    pub fn effective_ram_mb(&self) -> u64 {
        if self.ram_mb == 0 {
            Self::host_ram_mb()
        } else {
            self.ram_mb
        }
    }

    /// RAM available to containers: `ram_mb − reserve_mb` (at least 512 MiB).
    pub fn budget_mb(&self) -> u64 {
        self.effective_ram_mb()
            .saturating_sub(self.reserve_mb)
            .max(512)
    }

    /// Resolve `ram_mb = 0` once, so later checks don't re-read the host.
    pub fn resolved(&self) -> Capacity {
        Capacity {
            ram_mb: self.effective_ram_mb(),
            ..self.clone()
        }
    }
}

/// Per-role run timeouts, in minutes. A `qa.yaml` `timeouts:` map
/// overrides them per project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleTimeouts {
    pub planner: u32,
    pub builder: u32,
    pub qa: u32,
    pub reviewer: u32,
    pub distiller: u32,
}

impl Default for RoleTimeouts {
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

impl RoleTimeouts {
    /// Minutes for `role`.
    pub fn minutes(&self, role: WorkerRole) -> u32 {
        match role {
            WorkerRole::Planner => self.planner,
            WorkerRole::Builder => self.builder,
            WorkerRole::Qa => self.qa,
            WorkerRole::Reviewer => self.reviewer,
            WorkerRole::Distiller => self.distiller,
        }
        .max(1)
    }
}

/// Everything the scheduler needs besides its dependencies.
///
/// Construct with [`SchedulerConfig::new`] (or `Default`) and override
/// fields with struct-update syntax, so new fields never break callers:
/// `SchedulerConfig { capacity, ..SchedulerConfig::new(data_dir) }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    /// `<data>`: run folders, worktrees and mirrors live under it.
    pub data_dir: PathBuf,
    /// Base worker image, e.g. `ghcr.io/chhhee10/kiln-worker:0.1.0`.
    pub worker_image: String,
    /// `ProcessRuntime` only: host path of `kiln-runner`. When set, the
    /// container command is `<runner_bin> run` and `HOME` is not overridden;
    /// otherwise the image's `kiln-runner` is used.
    pub runner_bin: Option<PathBuf>,
    pub capacity: Capacity,
    pub timeouts: RoleTimeouts,
    /// Dispatcher fallback tick (the dispatcher also wakes on events).
    pub tick: Duration,
    /// Run the Distiller container with the run owner's credentials
    /// (decision #1). When false, distillation is heuristic only.
    pub llm_distill: bool,
    /// Per-harness options passed through as `RunConfig.options`
    /// (`harness id → key → value`).
    pub harness_options: BTreeMap<String, BTreeMap<String, String>>,
    /// Model prices for harnesses that report tokens but not cost (`[prices]`).
    pub prices: BTreeMap<String, crate::pricing::ModelPrice>,
    /// Fallback harness/model per role when neither the plan nor the user's
    /// settings choose one (the daemon's `[defaults]`).
    pub default_workers: BTreeMap<WorkerRole, WorkerChoice>,
    /// Public base URL of the daemon (`https://mac-mini.tailnet.ts.net`),
    /// used for artifact links in PR bodies. Relative links when `None`.
    pub public_url: Option<String>,
    /// Ingested run folders older than this are deleted by the nightly job.
    pub run_retention_days: u32,
    /// L0 raw-event retention passed to `memory.nightly` (60–90).
    pub l0_retention_days: u32,
    /// `run_events` hot-cache retention.
    pub run_events_retention_days: u32,
    /// How often Ready tasks' PRs are polled for merges (5 min).
    pub pr_poll_interval: Duration,
    /// Local hour (0–23) at which the nightly job runs.
    pub nightly_hour: u32,
    /// Grace period between `Stop` and a forced container stop.
    pub stop_grace: Duration,
    /// Machine sampler period.
    pub sample_interval: Duration,
    /// How long a runner keeps reconnecting to a restarting daemon before it
    /// gives up and writes `out/outcome.json` (passed as
    /// `KILN_RECONNECT_BUDGET_MS`).
    pub runner_reconnect_budget: Duration,
    /// Dev/tests: replaces every role timeout (minutes are too coarse for
    /// tests). `None` in production.
    pub timeout_override: Option<Duration>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("."),
            worker_image: DEFAULT_WORKER_IMAGE.into(),
            runner_bin: None,
            capacity: Capacity::default(),
            timeouts: RoleTimeouts::default(),
            tick: Duration::from_secs(2),
            llm_distill: true,
            harness_options: BTreeMap::new(),
            prices: BTreeMap::new(),
            default_workers: BTreeMap::new(),
            public_url: None,
            run_retention_days: 30,
            l0_retention_days: 75,
            run_events_retention_days: 14,
            pr_poll_interval: Duration::from_secs(300),
            nightly_hour: 3,
            stop_grace: Duration::from_secs(10),
            sample_interval: Duration::from_secs(2),
            runner_reconnect_budget: Duration::from_secs(600),
            timeout_override: None,
        }
    }
}

impl SchedulerConfig {
    /// Defaults rooted at `data_dir`.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            ..Self::default()
        }
    }

    /// Effective timeout of `role`, honoring a `qa.yaml` override (minutes).
    pub fn timeout_for(&self, role: WorkerRole, profile_minutes: Option<u32>) -> Duration {
        if let Some(d) = self.timeout_override {
            return d;
        }
        let m = profile_minutes
            .filter(|m| *m > 0)
            .unwrap_or_else(|| self.timeouts.minutes(role));
        Duration::from_secs(u64::from(m) * 60)
    }
}
