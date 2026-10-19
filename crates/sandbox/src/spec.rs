//! Container specification and runtime-reported data.

use kiln_types::RunId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Label set on every container Kiln creates.
pub const LABEL_MANAGED: &str = "kiln.managed";
pub const LABEL_RUN_ID: &str = "kiln.run_id";
pub const LABEL_ROLE: &str = "kiln.role";
pub const LABEL_OWNER: &str = "kiln.owner";
pub const LABEL_TASK: &str = "kiln.task";

/// Default pids limit (fork-bomb protection; generous enough for Chromium + node).
pub const DEFAULT_PIDS_LIMIT: i64 = 4096;

/// One bind mount. `host` must exist (see [`crate::prepare_run_dir`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub host: PathBuf,
    pub container: String,
    pub read_only: bool,
}

impl Mount {
    pub fn rw(host: impl Into<PathBuf>, container: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            container: container.into(),
            read_only: false,
        }
    }
    pub fn ro(host: impl Into<PathBuf>, container: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            container: container.into(),
            read_only: true,
        }
    }
}

/// Container network.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "name")]
pub enum NetworkMode {
    /// No network at all.
    None,
    /// Default bridge: outbound access for model APIs.
    #[default]
    Bridge,
    /// A user-defined network (e.g. the per-run QA sidecar network).
    Named(String),
}

impl NetworkMode {
    pub fn as_docker(&self) -> String {
        match self {
            NetworkMode::None => "none".into(),
            NetworkMode::Bridge => "bridge".into(),
            NetworkMode::Named(n) => n.clone(),
        }
    }
}

/// Everything needed to create one worker (or sidecar) container.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContainerSpec {
    /// Container name (must be unique on the runtime).
    pub name: String,
    pub image: String,
    /// Command (overrides the image CMD; the image ENTRYPOINT is kept).
    /// Empty = the image default. `ProcessRuntime` requires a non-empty
    /// command and treats `cmd[0]` as the program.
    pub cmd: Vec<String>,
    /// Environment. May carry secrets: never log it.
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    /// CPU limit in cores (0 = unlimited).
    pub cpus: f32,
    /// Memory limit in MiB (0 = unlimited).
    pub memory_mb: u64,
    pub network: NetworkMode,
    /// Extra labels. `kiln.managed=1` is always added.
    pub labels: BTreeMap<String, String>,
    /// Working directory inside the container.
    pub workdir: Option<String>,
    /// `uid[:gid]` or user name. `None` = image default (`kiln`, uid 1000).
    pub user: Option<String>,
    /// DNS aliases on a `Named` network (sidecars: `db`, `redis`, …).
    pub aliases: Vec<String>,
    /// Pids limit; `None` = [`DEFAULT_PIDS_LIMIT`].
    #[serde(default)]
    pub pids_limit: Option<i64>,
}

impl ContainerSpec {
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            cmd: Vec::new(),
            env: Vec::new(),
            mounts: Vec::new(),
            cpus: 0.0,
            memory_mb: 0,
            network: NetworkMode::Bridge,
            labels: BTreeMap::new(),
            workdir: None,
            user: None,
            aliases: Vec::new(),
            pids_limit: None,
        }
    }

    /// Labels including the mandatory `kiln.managed=1`.
    pub fn effective_labels(&self) -> BTreeMap<String, String> {
        let mut l = self.labels.clone();
        l.insert(LABEL_MANAGED.into(), "1".into());
        l
    }

    /// Basic structural validation shared by every runtime.
    pub fn validate(&self) -> crate::Result<()> {
        use crate::SandboxError::InvalidSpec;
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            return Err(InvalidSpec(format!("bad container name {:?}", self.name)));
        }
        if self.image.trim().is_empty() {
            return Err(InvalidSpec("empty image".into()));
        }
        if !(self.cpus.is_finite() && self.cpus >= 0.0) {
            return Err(InvalidSpec(format!("bad cpu limit {}", self.cpus)));
        }
        for m in &self.mounts {
            if !m.container.starts_with('/') {
                return Err(InvalidSpec(format!(
                    "mount target {:?} is not absolute",
                    m.container
                )));
            }
            if !m.host.is_absolute() {
                return Err(InvalidSpec(format!(
                    "mount source {} is not absolute",
                    m.host.display()
                )));
            }
        }
        for (k, _) in &self.env {
            if k.is_empty() || k.contains('=') {
                return Err(InvalidSpec(format!("bad env var name {k:?}")));
            }
        }
        Ok(())
    }
}

/// What [`crate::Runtime::ping`] reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// "docker", "podman", "orbstack", "process", …
    pub name: String,
    pub version: String,
    pub api_version: Option<String>,
    pub os: String,
    pub arch: String,
    /// Where we connected (socket path / URL); "local" for ProcessRuntime.
    pub endpoint: String,
}

/// A container carrying `kiln.managed=1`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManagedContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    pub labels: BTreeMap<String, String>,
    pub running: bool,
    /// Runtime state string ("running", "exited", "created", …).
    pub state: String,
    /// Unix seconds.
    pub created: Option<i64>,
}

impl ManagedContainer {
    pub fn run_id(&self) -> Option<RunId> {
        self.labels.get(LABEL_RUN_ID).and_then(|s| s.parse().ok())
    }
    pub fn role(&self) -> Option<&str> {
        self.labels.get(LABEL_ROLE).map(String::as_str)
    }
}

/// One resource-usage sample.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct ContainerStats {
    /// Percent of one core (100 = one full core).
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    /// 0 when unlimited/unknown.
    pub memory_limit_bytes: u64,
    pub pids: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_spec() {
        let mut s = ContainerSpec::new("kiln-run-1", "alpine");
        s.validate().unwrap();
        s.name = "bad name".into();
        assert!(s.validate().is_err());
        s.name = "ok".into();
        s.mounts.push(Mount::rw("relative", "/x"));
        assert!(s.validate().is_err());
        s.mounts.clear();
        s.env.push(("A=B".into(), "c".into()));
        assert!(s.validate().is_err());
    }

    #[test]
    fn labels_always_managed() {
        let s = ContainerSpec::new("a", "b");
        assert_eq!(
            s.effective_labels().get(LABEL_MANAGED).map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn network_wire() {
        let v = serde_json::to_value(NetworkMode::Named("qa-1".into())).unwrap();
        assert_eq!(v, serde_json::json!({"mode": "named", "name": "qa-1"}));
        assert_eq!(NetworkMode::default().as_docker(), "bridge");
    }
}
