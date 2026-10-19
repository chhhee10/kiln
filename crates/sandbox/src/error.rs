use std::path::PathBuf;

pub type Result<T, E = SandboxError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// No container runtime could be reached.
    #[error("container runtime unavailable: {0}")]
    Unavailable(String),
    #[error("container {0} not found")]
    NotFound(String),
    #[error("invalid container spec: {0}")]
    InvalidSpec(String),
    /// A bind-mount source does not exist on the host.
    #[error("mount source {0} does not exist")]
    MissingMountSource(PathBuf),
    #[error("docker API error: {0}")]
    Docker(#[from] bollard::errors::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl SandboxError {
    /// True when the error means "no such container/network".
    pub fn is_not_found(&self) -> bool {
        match self {
            SandboxError::NotFound(_) => true,
            SandboxError::Docker(bollard::errors::Error::DockerResponseServerError {
                status_code,
                ..
            }) => *status_code == 404,
            _ => false,
        }
    }
}
