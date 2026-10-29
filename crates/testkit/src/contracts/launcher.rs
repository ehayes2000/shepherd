//! The `HostLauncher` contract. Launchers are built through a [`LauncherFixture`]: the fake's counts launches, the
//! process adapter's starts a stand-in program that reports to the fixture when it runs. That the launched process
//! outlives the launcher and has no controlling terminal is tested next to the adapter.
//!
//! Run it with [`launcher_contract!`](crate::launcher_contract), which expands to one `#[test]` per case.

// Contract cases are tests that live outside `#[cfg(test)]` so other crates can run them.
#![allow(clippy::unwrap_used)]

/// Expands to one `#[test]` per `HostLauncher` contract case. `$fixture` is an expression building a fresh
/// [`LauncherFixture`] for each test, e.g. `shepherd_testkit::launcher_contract!(StandInFixture::new());`.
///
/// New cases go in the list below.
#[macro_export]
macro_rules! launcher_contract {
    ($fixture:expr) => {
        $crate::launcher_contract!(@cases $fixture;
            launch_starts_the_host,
            launch_returns_while_the_host_keeps_running,
            each_launch_starts_its_own_host,
            launch_that_cannot_start_the_host_is_an_error,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::launcher::$case($fixture);
            }
        )*
    };
}

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use shepherd_core::ports::{HostLauncher, LaunchError};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Builds launchers. Each test gets a fresh fixture, which stops whatever its launchers started when dropped.
pub trait LauncherFixture {
    /// `Send + Sync`, so a case can launch from another thread and notice if `launch` blocks.
    type Launcher: HostLauncher + Send + Sync + 'static;
    /// A launcher whose host starts and keeps running until the fixture is dropped.
    fn working(&mut self) -> Self::Launcher;
    /// A launcher whose host can't be started (for a process: its executable doesn't exist).
    fn broken(&mut self) -> Self::Launcher;
    /// Waits until `n` more hosts have started. Panics if they don't within `timeout`.
    fn wait_started(&mut self, n: usize, timeout: Duration);
}

/// Launches on another thread, so a `launch` that blocks fails the case instead of hanging it.
fn launch<L: HostLauncher + Send + Sync + 'static>(launcher: &Arc<L>) -> Result<(), LaunchError> {
    let (tx, rx) = mpsc::channel();
    let launcher = launcher.clone();
    thread::spawn(move || {
        let _ = tx.send(launcher.launch());
    });
    rx.recv_timeout(TIMEOUT)
        .unwrap_or_else(|_| panic!("launch blocked on the running host"))
}

pub fn launch_starts_the_host<F: LauncherFixture>(mut f: F) {
    let launcher = Arc::new(f.working());
    assert_eq!(launch(&launcher), Ok(()));
    f.wait_started(1, TIMEOUT);
}

/// The host runs until the fixture is dropped. `launch` must not wait for it, even with an earlier host running.
pub fn launch_returns_while_the_host_keeps_running<F: LauncherFixture>(mut f: F) {
    let launcher = Arc::new(f.working());
    assert_eq!(launch(&launcher), Ok(()));
    f.wait_started(1, TIMEOUT);
    assert_eq!(launch(&launcher), Ok(()));
    f.wait_started(1, TIMEOUT);
}

pub fn each_launch_starts_its_own_host<F: LauncherFixture>(mut f: F) {
    let launcher = Arc::new(f.working());
    assert_eq!(launch(&launcher), Ok(()));
    assert_eq!(launch(&launcher), Ok(()));
    f.wait_started(2, TIMEOUT);
}

pub fn launch_that_cannot_start_the_host_is_an_error<F: LauncherFixture>(mut f: F) {
    let launcher = Arc::new(f.broken());
    assert!(
        matches!(launch(&launcher), Err(LaunchError::SpawnFailed(_))),
        "a host that can't start must be an error"
    );
}
