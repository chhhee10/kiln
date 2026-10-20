//! OCI sandbox for Kiln worker runs (ARCHITECTURE.md "sandbox").
//!
//! One container per run. The daemon builds a [`ContainerSpec`] (image,
//! command, env, [`mounts_for`] the role, resource limits, labels) and drives
//! it through a [`Runtime`]:
//!
//! * [`DockerRuntime`] talks to any Docker-compatible API (Docker Engine,
//!   Podman, OrbStack) through `bollard`. Containers are hardened:
//!   `no-new-privileges`, all capabilities dropped, pids/memory/cpu limits, an
//!   init process, and **no** auto-remove (the daemon removes a container only
//!   after its run has been ingested).
//! * [`ProcessRuntime`] runs the command as a plain host subprocess for
//!   development and tests. It provides no isolation; mount targets are mapped
//!   back to host paths through `KILN_RUN_DIR` / `KILN_WORKSPACE` and by
//!   rewriting env values that point inside a mount.

mod docker;
mod error;
mod extras;
mod mounts;
mod process;
mod spec;

pub use docker::DockerRuntime;
pub use error::{Result, SandboxError};
pub use extras::{EXTRAS_REPO, EXTRAS_TEMPLATE, extras_tag, render_extras};
pub use mounts::{TRANSCRIPT_DIRS, mounts_for, prepare_run_dir, transcript_mounts};
pub use process::ProcessRuntime;
pub use spec::*;

use async_trait::async_trait;
use std::time::Duration;

/// A container runtime. All methods are idempotent where it makes sense:
/// stopping a stopped container, removing a missing container and creating an
/// existing network succeed.
#[async_trait]
pub trait Runtime: Send + Sync {
    /// Check the runtime is reachable and describe it.
    async fn ping(&self) -> Result<RuntimeInfo>;
    /// Make sure `image` is available locally, pulling it if needed.
    async fn ensure_image(&self, image: &str) -> Result<()>;
    /// Create (but do not start) a container. Returns its id.
    async fn create(&self, spec: &ContainerSpec) -> Result<String>;
    async fn start(&self, id: &str) -> Result<()>;
    /// Wait for the container to exit and return its exit code.
    async fn wait(&self, id: &str) -> Result<i64>;
    /// SIGTERM, then SIGKILL after `grace`.
    async fn stop(&self, id: &str, grace: Duration) -> Result<()>;
    /// Remove the container (forcefully). Missing containers are not an error.
    async fn remove(&self, id: &str) -> Result<()>;
    async fn is_running(&self, id: &str) -> Result<bool>;
    /// Every container carrying the label `kiln.managed=1`, running or not.
    async fn list_managed(&self) -> Result<Vec<ManagedContainer>>;
    /// One-shot resource usage sample.
    async fn stats(&self, id: &str) -> Result<ContainerStats>;
    /// Create a user-defined bridge network (per-run QA sidecar network).
    async fn create_network(&self, name: &str) -> Result<()>;
    async fn remove_network(&self, name: &str) -> Result<()>;
    /// The last `lines` lines of combined stdout/stderr.
    async fn logs_tail(&self, id: &str, lines: usize) -> Result<String>;
    /// Run `cmd` inside a running container and wait for it (healthchecks).
    async fn exec(&self, id: &str, cmd: &[String]) -> Result<ExecOutput>;
    /// The local image's id, or `None` when it isn't present (no pull).
    async fn image_id(&self, image: &str) -> Result<Option<String>>;
    /// Build `tag` from a single Dockerfile with an empty context.
    async fn build_image(
        &self,
        tag: &str,
        dockerfile: &str,
        build_args: &[(String, String)],
    ) -> Result<()>;
}
