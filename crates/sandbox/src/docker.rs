//! Docker-compatible runtime (Docker Engine, Podman, OrbStack) via bollard.

use crate::Runtime;
use crate::error::{Result, SandboxError};
use crate::spec::*;
use async_trait::async_trait;
use bollard::errors::Error as BollardError;
use bollard::models::{
    ContainerCreateBody, ContainerStatsResponse, EndpointSettings, HostConfig,
    Mount as DockerMount, MountType, NetworkCreateRequest, NetworkingConfig,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, ListContainersOptions,
    LogsOptions, RemoveContainerOptions, StatsOptions, StopContainerOptions, WaitContainerOptions,
};
use bollard::{API_DEFAULT_VERSION, Docker};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Request timeout for the API client (seconds). Long enough for big pulls;
/// `wait` is a streaming call and not bounded by it in practice.
const CLIENT_TIMEOUT_SECS: u64 = 600;
/// Size of `/dev/shm` (Chromium needs more than Docker's 64 MiB default).
const SHM_BYTES: i64 = 512 * 1024 * 1024;

/// A runtime speaking the Docker Engine API.
#[derive(Clone)]
pub struct DockerRuntime {
    docker: Docker,
    endpoint: String,
}

impl std::fmt::Debug for DockerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerRuntime")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

/// Candidate sockets tried in order when `DOCKER_HOST` is unset.
pub(crate) fn candidate_sockets() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/var/run/docker.sock")];
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(h) = &home {
        v.push(h.join(".orbstack/run/docker.sock"));
    }
    if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
        v.push(PathBuf::from(x).join("podman/podman.sock"));
    }
    if let Some(h) = &home {
        v.push(h.join(".docker/run/docker.sock"));
    }
    v.push(PathBuf::from("/run/podman/podman.sock"));
    v
}

impl DockerRuntime {
    /// Connect to the runtime: `DOCKER_HOST` when set, otherwise the first
    /// reachable socket among the default Docker socket, OrbStack, rootless
    /// Podman and Docker Desktop.
    pub async fn connect() -> Result<Self> {
        if let Ok(host) = std::env::var("DOCKER_HOST")
            && !host.trim().is_empty()
        {
            let docker = Docker::connect_with_defaults()
                .map_err(|e| SandboxError::Unavailable(format!("DOCKER_HOST={host}: {e}")))?;
            let rt = Self {
                docker,
                endpoint: host.clone(),
            };
            rt.ping_quick()
                .await
                .map_err(|e| SandboxError::Unavailable(format!("DOCKER_HOST={host}: {e}")))?;
            return Ok(rt);
        }
        let mut tried = Vec::new();
        for sock in candidate_sockets() {
            if !sock.exists() {
                continue;
            }
            let path = sock.to_string_lossy().into_owned();
            match Self::with_socket(&path) {
                Ok(rt) => match rt.ping_quick().await {
                    Ok(()) => return Ok(rt),
                    Err(e) => tried.push(format!("{path}: {e}")),
                },
                Err(e) => tried.push(format!("{path}: {e}")),
            }
        }
        Err(SandboxError::Unavailable(if tried.is_empty() {
            "no container runtime socket found (set DOCKER_HOST)".into()
        } else {
            tried.join("; ")
        }))
    }

    /// Connect to a specific Unix socket without probing it.
    pub fn with_socket(path: &str) -> Result<Self> {
        let docker = Docker::connect_with_unix(path, CLIENT_TIMEOUT_SECS, API_DEFAULT_VERSION)?;
        Ok(Self {
            docker,
            endpoint: path.to_string(),
        })
    }

    /// Wrap an existing bollard client.
    pub fn from_docker(docker: Docker, endpoint: impl Into<String>) -> Self {
        Self {
            docker,
            endpoint: endpoint.into(),
        }
    }

    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn ping_quick(&self) -> Result<()> {
        match tokio::time::timeout(Duration::from_secs(5), self.docker.ping()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(SandboxError::Unavailable("ping timed out".into())),
        }
    }

    /// Translate a spec into the Docker API create body (pure; unit tested).
    pub fn create_body(spec: &ContainerSpec) -> ContainerCreateBody {
        let mounts = spec
            .mounts
            .iter()
            .map(|m| DockerMount {
                target: Some(m.container.clone()),
                source: Some(m.host.to_string_lossy().into_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(m.read_only),
                ..Default::default()
            })
            .collect();
        let memory =
            (spec.memory_mb > 0).then(|| (spec.memory_mb as i64).saturating_mul(1024 * 1024));
        let host_config = HostConfig {
            mounts: Some(mounts),
            memory,
            // No swap beyond the memory limit.
            memory_swap: memory,
            nano_cpus: (spec.cpus > 0.0).then(|| (spec.cpus as f64 * 1e9).round() as i64),
            pids_limit: Some(spec.pids_limit.unwrap_or(DEFAULT_PIDS_LIMIT)),
            security_opt: Some(vec!["no-new-privileges:true".into()]),
            cap_drop: Some(vec!["ALL".into()]),
            network_mode: Some(spec.network.as_docker()),
            // The daemon removes the container after the run is ingested.
            auto_remove: Some(false),
            // tini as PID 1 reaps the harness's orphans.
            init: Some(true),
            shm_size: Some(SHM_BYTES),
            privileged: Some(false),
            ..Default::default()
        };
        let networking_config = match &spec.network {
            NetworkMode::Named(net) if !spec.aliases.is_empty() => Some(NetworkingConfig {
                endpoints_config: Some(HashMap::from([(
                    net.clone(),
                    EndpointSettings {
                        aliases: Some(spec.aliases.clone()),
                        ..Default::default()
                    },
                )])),
            }),
            _ => None,
        };
        ContainerCreateBody {
            image: Some(spec.image.clone()),
            cmd: (!spec.cmd.is_empty()).then(|| spec.cmd.clone()),
            env: Some(spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect()),
            labels: Some(spec.effective_labels().into_iter().collect()),
            working_dir: spec.workdir.clone(),
            user: spec.user.clone(),
            attach_stdin: Some(false),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(false),
            open_stdin: Some(false),
            host_config: Some(host_config),
            networking_config,
            ..Default::default()
        }
    }
}

fn status_code(e: &BollardError) -> Option<u16> {
    match e {
        BollardError::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

fn not_found(id: &str, e: BollardError) -> SandboxError {
    if status_code(&e) == Some(404) {
        SandboxError::NotFound(id.to_string())
    } else {
        e.into()
    }
}

/// Split an image reference into (repo, tag-or-digest) for the pull API.
pub(crate) fn split_image_ref(image: &str) -> (String, String) {
    if let Some((repo, digest)) = image.split_once('@') {
        return (repo.to_string(), digest.to_string());
    }
    let last_slash = image.rfind('/').map(|i| i + 1).unwrap_or(0);
    match image[last_slash..].rfind(':') {
        Some(i) => (
            image[..last_slash + i].to_string(),
            image[last_slash + i + 1..].to_string(),
        ),
        None => (image.to_string(), "latest".to_string()),
    }
}

/// CPU percent (of one core) between the pre- and current sample.
fn cpu_percent(s: &ContainerStatsResponse) -> f32 {
    let (Some(cur), Some(pre)) = (&s.cpu_stats, &s.precpu_stats) else {
        return 0.0;
    };
    let total = |c: &bollard::models::ContainerCpuStats| {
        c.cpu_usage
            .as_ref()
            .and_then(|u| u.total_usage)
            .unwrap_or(0)
    };
    let cpu_delta = total(cur).saturating_sub(total(pre)) as f64;
    let sys_delta = cur
        .system_cpu_usage
        .unwrap_or(0)
        .saturating_sub(pre.system_cpu_usage.unwrap_or(0)) as f64;
    let cpus = cur.online_cpus.unwrap_or(1).max(1) as f64;
    if sys_delta <= 0.0 {
        0.0
    } else {
        (cpu_delta / sys_delta * cpus * 100.0) as f32
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    async fn ping(&self) -> Result<RuntimeInfo> {
        self.ping_quick().await?;
        let v = self.docker.version().await?;
        let podman = v.components.as_ref().is_some_and(|cs| {
            cs.iter()
                .any(|c| c.name.to_ascii_lowercase().contains("podman"))
        });
        let name = if podman {
            "podman"
        } else if self.endpoint.contains("orbstack") {
            "orbstack"
        } else {
            "docker"
        };
        Ok(RuntimeInfo {
            name: name.into(),
            version: v.version.unwrap_or_default(),
            api_version: v.api_version,
            os: v.os.unwrap_or_default(),
            arch: v.arch.unwrap_or_default(),
            endpoint: self.endpoint.clone(),
        })
    }

    async fn ensure_image(&self, image: &str) -> Result<()> {
        if self.docker.inspect_image(image).await.is_ok() {
            return Ok(());
        }
        let (repo, tag) = split_image_ref(image);
        tracing::info!(image, "pulling image");
        let opts = CreateImageOptions {
            from_image: Some(repo),
            tag: Some(tag),
            ..Default::default()
        };
        let mut stream = self.docker.create_image(Some(opts), None, None);
        while let Some(item) = stream.next().await {
            let info = item?;
            if let Some(err) = info.error_detail.and_then(|d| d.message) {
                return Err(SandboxError::Unavailable(format!("pull {image}: {err}")));
            }
        }
        Ok(())
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String> {
        spec.validate()?;
        if let Some(m) = spec.mounts.iter().find(|m| !m.host.exists()) {
            return Err(SandboxError::MissingMountSource(m.host.clone()));
        }
        let opts = CreateContainerOptions {
            name: Some(spec.name.clone()),
            platform: String::new(),
        };
        let resp = self
            .docker
            .create_container(Some(opts), Self::create_body(spec))
            .await?;
        for w in resp.warnings {
            tracing::warn!(container = %spec.name, warning = %w, "container create warning");
        }
        Ok(resp.id)
    }

    async fn start(&self, id: &str) -> Result<()> {
        match self.docker.start_container(id, None).await {
            Ok(()) => Ok(()),
            Err(e) if status_code(&e) == Some(304) => Ok(()),
            Err(e) => Err(not_found(id, e)),
        }
    }

    async fn wait(&self, id: &str) -> Result<i64> {
        let opts = WaitContainerOptions {
            condition: "not-running".into(),
        };
        let mut stream = self.docker.wait_container(id, Some(opts));
        match stream.next().await {
            Some(Ok(r)) => Ok(r.status_code),
            Some(Err(BollardError::DockerContainerWaitError { code, .. })) => Ok(code),
            Some(Err(e)) => Err(not_found(id, e)),
            None => {
                // Stream closed without a status: fall back to inspect.
                let c = self
                    .docker
                    .inspect_container(id, None::<InspectContainerOptions>)
                    .await
                    .map_err(|e| not_found(id, e))?;
                Ok(c.state.and_then(|s| s.exit_code).unwrap_or(-1))
            }
        }
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<()> {
        let t = i32::try_from(grace.as_secs().max(1)).unwrap_or(i32::MAX);
        match self
            .docker
            .stop_container(
                id,
                Some(StopContainerOptions {
                    signal: None,
                    t: Some(t),
                }),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if matches!(status_code(&e), Some(304) | Some(404)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let opts = RemoveContainerOptions {
            force: true,
            v: true,
            link: false,
        };
        match self.docker.remove_container(id, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if status_code(&e) == Some(404) => Ok(()),
            // 409: removal already in progress.
            Err(e) if status_code(&e) == Some(409) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn is_running(&self, id: &str) -> Result<bool> {
        match self
            .docker
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
        {
            Ok(c) => Ok(c.state.and_then(|s| s.running).unwrap_or(false)),
            Err(e) if status_code(&e) == Some(404) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn list_managed(&self) -> Result<Vec<ManagedContainer>> {
        let filters = HashMap::from([("label".to_string(), vec![format!("{LABEL_MANAGED}=1")])]);
        let opts = ListContainersOptions {
            all: true,
            limit: None,
            size: false,
            filters: Some(filters),
        };
        let list = self.docker.list_containers(Some(opts)).await?;
        Ok(list
            .into_iter()
            .map(|c| {
                let state = c.state.map(|s| s.to_string()).unwrap_or_default();
                ManagedContainer {
                    id: c.id.unwrap_or_default(),
                    name: c
                        .names
                        .and_then(|n| n.into_iter().next())
                        .map(|n| n.trim_start_matches('/').to_string())
                        .unwrap_or_default(),
                    image: c.image.unwrap_or_default(),
                    labels: c.labels.unwrap_or_default().into_iter().collect(),
                    running: state == "running",
                    state,
                    created: c.created,
                }
            })
            .collect())
    }

    async fn stats(&self, id: &str) -> Result<ContainerStats> {
        let mut stream = self.docker.stats(
            id,
            Some(StatsOptions {
                stream: false,
                one_shot: false,
            }),
        );
        let s = match stream.next().await {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(not_found(id, e)),
            None => return Err(SandboxError::NotFound(id.to_string())),
        };
        let mem = s.memory_stats.as_ref();
        let usage = mem.and_then(|m| m.usage).unwrap_or(0);
        // Match `docker stats`: subtract reclaimable page cache.
        let cache = mem
            .and_then(|m| m.stats.as_ref())
            .and_then(|st| {
                st.get("inactive_file")
                    .or_else(|| st.get("total_inactive_file"))
                    .copied()
            })
            .unwrap_or(0);
        Ok(ContainerStats {
            cpu_percent: cpu_percent(&s),
            memory_bytes: usage.saturating_sub(cache),
            memory_limit_bytes: mem.and_then(|m| m.limit).unwrap_or(0),
            pids: s.pids_stats.as_ref().and_then(|p| p.current).unwrap_or(0),
        })
    }

    async fn create_network(&self, name: &str) -> Result<()> {
        if self.docker.inspect_network(name, None).await.is_ok() {
            return Ok(());
        }
        let req = NetworkCreateRequest {
            name: name.to_string(),
            driver: Some("bridge".into()),
            labels: Some(HashMap::from([(
                LABEL_MANAGED.to_string(),
                "1".to_string(),
            )])),
            ..Default::default()
        };
        match self.docker.create_network(req).await {
            Ok(_) => Ok(()),
            Err(e) if status_code(&e) == Some(409) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn remove_network(&self, name: &str) -> Result<()> {
        match self.docker.remove_network(name).await {
            Ok(()) => Ok(()),
            Err(e) if status_code(&e) == Some(404) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn logs_tail(&self, id: &str, lines: usize) -> Result<String> {
        let opts = LogsOptions {
            stdout: true,
            stderr: true,
            tail: lines.to_string(),
            ..Default::default()
        };
        let mut stream = self.docker.logs(id, Some(opts));
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            let chunk = item.map_err(|e| not_found(id, e))?;
            out.push_str(&String::from_utf8_lossy(&chunk.into_bytes()));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn image_refs() {
        assert_eq!(
            split_image_ref("alpine"),
            ("alpine".into(), "latest".into())
        );
        assert_eq!(
            split_image_ref("alpine:3.20"),
            ("alpine".into(), "3.20".into())
        );
        assert_eq!(
            split_image_ref("localhost:5000/kiln/worker"),
            ("localhost:5000/kiln/worker".into(), "latest".into())
        );
        assert_eq!(
            split_image_ref("ghcr.io/a/b:v1"),
            ("ghcr.io/a/b".into(), "v1".into())
        );
        assert_eq!(
            split_image_ref("a/b@sha256:abc"),
            ("a/b".into(), "sha256:abc".into())
        );
    }

    #[test]
    fn create_body_hardening() {
        let mut spec = ContainerSpec::new("kiln-r1", "kiln/worker:dev");
        spec.cpus = 1.5;
        spec.memory_mb = 2048;
        spec.mounts = crate::mounts_for(
            kiln_types::WorkerRole::Qa,
            Some(Path::new("/w")),
            None,
            Path::new("/runs/r1"),
        );
        spec.network = NetworkMode::Named("kiln-qa-r1".into());
        spec.aliases = vec!["worker".into()];
        spec.labels.insert(LABEL_RUN_ID.into(), "r1".into());
        let b = DockerRuntime::create_body(&spec);
        let h = b.host_config.unwrap();
        assert_eq!(h.cap_drop, Some(vec!["ALL".to_string()]));
        assert_eq!(
            h.security_opt,
            Some(vec!["no-new-privileges:true".to_string()])
        );
        assert_eq!(h.auto_remove, Some(false));
        assert_eq!(h.nano_cpus, Some(1_500_000_000));
        assert_eq!(h.memory, Some(2048 * 1024 * 1024));
        assert_eq!(h.pids_limit, Some(DEFAULT_PIDS_LIMIT));
        assert_eq!(h.network_mode.as_deref(), Some("kiln-qa-r1"));
        let ws = h
            .mounts
            .unwrap()
            .into_iter()
            .find(|m| m.target.as_deref() == Some("/workspace"))
            .unwrap();
        assert_eq!(ws.read_only, Some(true));
        let labels = b.labels.unwrap();
        assert_eq!(labels.get(LABEL_MANAGED).map(String::as_str), Some("1"));
        assert_eq!(labels.get(LABEL_RUN_ID).map(String::as_str), Some("r1"));
        let eps = b.networking_config.unwrap().endpoints_config.unwrap();
        assert_eq!(eps["kiln-qa-r1"].aliases, Some(vec!["worker".to_string()]));
        assert!(b.cmd.is_none());
    }

    #[test]
    fn candidates_include_alternatives() {
        let c = candidate_sockets();
        assert_eq!(c[0], PathBuf::from("/var/run/docker.sock"));
        assert!(c.iter().any(|p| p.to_string_lossy().contains("podman")));
    }
}
