//! Starting the local host daemon in the background, for a client that found no host listening.
//!
//! [`HostLauncher::launch`] returns once the host process has been started, without waiting for it to be ready:
//! the client finds out by connecting (see `core::client`). The launched process belongs to nobody: it keeps
//! running after the client exits, has no controlling terminal, and never reads the client's terminal.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The host process could not be started, e.g. its executable is missing.
    #[error("could not start the host process: {0}")]
    SpawnFailed(String),
}

pub trait HostLauncher {
    fn launch(&self) -> Result<(), LaunchError>;
}
