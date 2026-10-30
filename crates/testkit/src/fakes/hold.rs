//! Making a fake's next call block until the test releases it, to stand in for a slow call (a fetch over the
//! network, a long hook) without timing anything. Shared by the fakes that offer `hold_next`.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How long a held call waits for its release before panicking: a liveness bound, so a test that never releases
/// (or code that blocks the thread meant to release it) fails instead of hanging.
const HOLD_LIMIT: Duration = Duration::from_secs(10);

/// Where a held call waits. Released by the test, or cancelled by the code under test (e.g. `cancel_create`).
#[derive(Debug, Default)]
pub(crate) struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct GateState {
    entered: bool,
    released: bool,
    cancelled: bool,
}

impl Gate {
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until the gate is released or cancelled, and says whether it was cancelled. `fake` names the fake in the
    /// panic of a call never released.
    pub(crate) fn wait(&self, fake: &str) -> bool {
        let mut state = self.lock();
        state.entered = true;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, HOLD_LIMIT, |s| !s.released && !s.cancelled)
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            state.released || state.cancelled,
            "a held {fake} call was never released (is the thread that should release it blocked on it?)"
        );
        state.cancelled
    }

    /// Lets the call waiting at the gate (or about to) go on, as cancelled.
    pub(crate) fn cancel(&self) {
        self.lock().cancelled = true;
        self.changed.notify_all();
    }
}

/// The test's handle on a held call, from a fake's `hold_next`. Releasing it, or dropping it, lets the call go on.
#[derive(Debug)]
pub struct Hold(Arc<Gate>);

impl Hold {
    /// Blocks until the held call has started and is waiting, up to `limit` (a liveness bound). Whether it did.
    pub fn wait_entered(&self, limit: Duration) -> bool {
        let state = self.0.lock();
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, limit, |s| !s.entered)
            .unwrap_or_else(PoisonError::into_inner);
        state.entered
    }

    pub fn release(self) {}
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.lock().released = true;
        self.0.changed.notify_all();
    }
}

/// Where a fake keeps the gate its next call waits at. Clones share it. Kept apart from the fake's state, so a held
/// call holds no lock the test needs.
#[derive(Debug, Clone, Default)]
pub(crate) struct HoldSlot(Arc<Mutex<Option<Arc<Gate>>>>);

impl HoldSlot {
    /// Arms the gate for the next call.
    pub(crate) fn arm(&self) -> Hold {
        let gate = Arc::new(Gate::default());
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(gate.clone());
        Hold(gate)
    }

    /// Disarms the armed gate, if any, for the caller to wait at.
    pub(crate) fn take(&self) -> Option<Arc<Gate>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Waits at the armed gate, if any, disarming it. `fake` names the fake in the panic of a call never released.
    pub(crate) fn wait_if_held(&self, fake: &str) {
        if let Some(gate) = self.take() {
            gate.wait(fake);
        }
    }
}
