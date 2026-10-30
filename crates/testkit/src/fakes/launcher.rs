//! An in-memory `HostLauncher`. It counts launches and runs an optional hook for each, which a scenario uses to
//! start a fake host on a [`FakeNetwork`](super::link::FakeNetwork). Clones share their state, so a test can keep
//! one to inspect while the runtime owns another.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use shepherd_core::ports::{HostLauncher, LaunchError};

use crate::contracts::launcher::LauncherFixture;

type OnLaunch = Box<dyn FnMut() + Send>;

#[derive(Default)]
struct State {
    launches: usize,
    /// Every launch fails with this, until cleared.
    failing: Option<LaunchError>,
    on_launch: Option<OnLaunch>,
}

#[derive(Clone, Default)]
pub struct FakeHostLauncher(Arc<Mutex<State>>);

impl FakeHostLauncher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `hook` on every successful launch, e.g. to start a fake host.
    pub fn on_launch(self, hook: impl FnMut() + Send + 'static) -> Self {
        self.lock().on_launch = Some(Box::new(hook));
        self
    }

    /// Makes every launch fail with `error`, as when the host executable is missing.
    pub fn failing(self, error: LaunchError) -> Self {
        self.lock().failing = Some(error);
        self
    }

    /// How many launches succeeded.
    pub fn launches(&self) -> usize {
        self.lock().launches
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl HostLauncher for FakeHostLauncher {
    fn launch(&self) -> Result<(), LaunchError> {
        let mut hook = {
            let mut state = self.lock();
            if let Some(error) = &state.failing {
                return Err(error.clone());
            }
            state.launches += 1;
            state.on_launch.take()
        };
        // Run outside the lock: the hook may inspect this launcher.
        if let Some(run) = &mut hook {
            run();
        }
        if let Some(run) = hook {
            self.lock().on_launch.get_or_insert(run);
        }
        Ok(())
    }
}

/// The contract fixture: launchers sharing one count.
#[derive(Default)]
pub struct FakeLauncherFixture {
    launcher: FakeHostLauncher,
    /// Hosts already waited for.
    seen: usize,
}

impl LauncherFixture for FakeLauncherFixture {
    type Launcher = FakeHostLauncher;

    fn working(&mut self) -> FakeHostLauncher {
        self.launcher.clone()
    }

    fn broken(&mut self) -> FakeHostLauncher {
        FakeHostLauncher::new().failing(LaunchError::SpawnFailed("no such file".into()))
    }

    fn failing_host(&mut self, message: &str) -> FakeHostLauncher {
        FakeHostLauncher::new().failing(LaunchError::HostFailed(message.to_owned()))
    }

    fn wait_started(&mut self, n: usize, _timeout: Duration) {
        self.seen += n;
        assert_eq!(self.launcher.launches(), self.seen, "hosts started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    mod contract {
        use super::FakeLauncherFixture;

        crate::launcher_contract!(FakeLauncherFixture::default());
    }

    #[test]
    fn the_hook_runs_once_per_launch() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let launcher = FakeHostLauncher::new().on_launch(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        launcher.launch().unwrap();
        launcher.launch().unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failing_launcher_neither_counts_nor_runs_the_hook() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let launcher = FakeHostLauncher::new()
            .on_launch(move || {
                counted.fetch_add(1, Ordering::SeqCst);
            })
            .failing(LaunchError::SpawnFailed("gone".into()));
        assert!(launcher.launch().is_err());
        assert_eq!((launcher.launches(), runs.load(Ordering::SeqCst)), (0, 0));
    }
}
