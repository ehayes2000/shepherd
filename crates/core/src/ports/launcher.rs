//! Starting the local host daemon in the background, for a client that found no host listening.
//!
//! [`HostLauncher::launch`] returns once the host process has been started and has either reported that it is
//! serving or why it can't start ([`LaunchError::HostFailed`]), or a liveness bound has passed. Either way the
//! client finds out it is ready by connecting (see `core::client`). The launched process belongs to nobody: it keeps
//! running after the client exits, has no controlling terminal, and never reads the client's terminal.
//!
//! [`HostLauncher::terminate`] stops the serving host, so that the client's reconnect launches its own build in its
//! place (T36a). It runs only when the user asks.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// The host process could not be started, e.g. its executable is missing.
    #[error("could not start the host process: {0}")]
    SpawnFailed(String),
    /// The host process started but reported that it couldn't start serving. The message is the host's own, for
    /// the user.
    #[error("{0}")]
    HostFailed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TerminateError {
    /// No host is serving: there is nothing to stop.
    #[error("no shepherd host is running")]
    NotRunning,
    /// A host is serving, but it couldn't be told to stop. The message says why.
    #[error("{0}")]
    Failed(String),
}

pub trait HostLauncher {
    fn launch(&self) -> Result<(), LaunchError>;

    /// Tells the host that is serving to stop, whoever started it and whatever build it is: it needs nothing from the
    /// host but that it is serving (a host older than this client has no request for it). Returns once the host has
    /// been told; it then shuts down on its own, and its clients see their links drop.
    fn terminate(&self) -> Result<(), TerminateError>;
}
