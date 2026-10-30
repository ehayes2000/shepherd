//! An in-memory `HostLauncher`. It counts launches and runs an optional hook for each, which a scenario uses to
//! start a fake host on a [`FakeNetwork`](super::link::FakeNetwork). Clones share their state, so a test can keep
//! one to inspect while the runtime owns another.
//!
//! It knows whether a host is running: a launch starts one, and so does [`FakeHostLauncher::running`] (a host started
//! before the test). Terminating a running host stops it and runs an optional hook, which a scenario uses to shut
//! the fake host down.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use shepherd_core::ports::{HostLauncher, LaunchError, TerminateError};

use crate::contracts::launcher::LauncherFixture;

type Hook = Box<dyn FnMut() + Send>;

#[derive(Default)]
struct State {
    launches: usize,
    terminations: usize,
    /// A host is running.
    running: bool,
    /// Every launch fails with this, until cleared.
    failing: Option<LaunchError>,
    on_launch: Option<Hook>,
    on_terminate: Option<Hook>,
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

    /// Runs `hook` on every termination of a running host, e.g. to shut a fake host down.
    pub fn on_terminate(self, hook: impl FnMut() + Send + 'static) -> Self {
        self.lock().on_terminate = Some(Box::new(hook));
        self
    }

    /// With a host already running, started by someone else.
    pub fn running(self) -> Self {
        self.lock().running = true;
        self
    }

    /// How many launches succeeded.
    pub fn launches(&self) -> usize {
        self.lock().launches
    }

    /// How many running hosts were terminated.
    pub fn terminations(&self) -> usize {
        self.lock().terminations
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs the hook `slot` picks, outside the lock: the hook may inspect this launcher.
    fn run_hook(&self, slot: fn(&mut State) -> &mut Option<Hook>) {
        let mut hook = slot(&mut self.lock()).take();
        if let Some(run) = &mut hook {
            run();
        }
        if let Some(run) = hook {
            slot(&mut self.lock()).get_or_insert(run);
        }
    }
}

impl HostLauncher for FakeHostLauncher {
    fn launch(&self) -> Result<(), LaunchError> {
        {
            let mut state = self.lock();
            if let Some(error) = &state.failing {
                return Err(error.clone());
            }
            state.launches += 1;
            state.running = true;
        }
        self.run_hook(|s| &mut s.on_launch);
        Ok(())
    }

    fn terminate(&self) -> Result<(), TerminateError> {
        {
            let mut state = self.lock();
            if !state.running {
                return Err(TerminateError::NotRunning);
            }
            state.running = false;
            state.terminations += 1;
        }
        self.run_hook(|s| &mut s.on_terminate);
        Ok(())
    }
}

/// The contract fixture: launchers sharing one count.
#[derive(Default)]
pub struct FakeLauncherFixture {
    launcher: FakeHostLauncher,
    /// Hosts already waited for.
    seen: usize,
    /// The launcher of the host [`LauncherFixture::running`] started.
    running: Option<FakeHostLauncher>,
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

    fn running(&mut self) -> FakeHostLauncher {
        self.running
            .get_or_insert_with(|| FakeHostLauncher::new().running())
            .clone()
    }

    fn idle(&mut self) -> FakeHostLauncher {
        FakeHostLauncher::new()
    }

    fn wait_terminated(&mut self, _timeout: Duration) {
        let terminations = self
            .running
            .as_ref()
            .map_or(0, FakeHostLauncher::terminations);
        assert_eq!(terminations, 1, "the running host was terminated");
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

    #[test]
    fn a_launched_host_can_be_terminated_once_and_the_hook_runs() {
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = runs.clone();
        let launcher = FakeHostLauncher::new().on_terminate(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
        launcher.launch().unwrap();
        assert_eq!(launcher.terminate(), Ok(()));
        assert_eq!(launcher.terminate(), Err(TerminateError::NotRunning));
        assert_eq!(
            (launcher.terminations(), runs.load(Ordering::SeqCst)),
            (1, 1)
        );
    }
}
