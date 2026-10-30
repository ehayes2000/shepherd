//! Runs a synchronous port's calls on a thread of their own, so a slow one (git over the network) never stalls the
//! runtime loop. Decision 0011's pattern: the call runs in the background and its result comes back into the loop's
//! channel as an event ([`RuntimeEvent::Worktrees`]), which the loop hands to core like any other.
//!
//! One thread per port, taking calls in the order they were submitted, so results come back in that order too and
//! two git calls on one repo never race each other.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Sender};
use std::thread;

use shepherd_core::host::HostEvent;

use super::runtime::{RuntimeEvent, RuntimeTx};

/// A call to run on the worker: what it computes from the port, and what to report instead if it panics.
struct Job<W> {
    call: Box<dyn FnOnce(&W) -> HostEvent + Send>,
    panicked: HostEvent,
}

/// Owns a port on a worker thread. Dropping it lets the thread finish the call it is in and stop; it isn't joined,
/// since that call may be a git command waiting on its timeout.
pub struct Worker<W> {
    jobs: Sender<Job<W>>,
}

impl<W: Send + 'static> Worker<W> {
    /// Moves `port` to a new thread called `name`, which reports each call's result into `tx`.
    pub fn spawn(name: &str, port: W, tx: RuntimeTx) -> io::Result<Self> {
        let (jobs, calls) = mpsc::channel::<Job<W>>();
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                for job in calls {
                    let call = job.call;
                    let event =
                        catch_unwind(AssertUnwindSafe(|| call(&port))).unwrap_or_else(|_| {
                            tracing::error!("a worktrees call panicked");
                            job.panicked
                        });
                    tx.send(RuntimeEvent::Worktrees(event));
                }
            })?;
        Ok(Self { jobs })
    }

    /// Queues `call`. Never blocks. Its result arrives as [`RuntimeEvent::Worktrees`]; if it panics, `panicked` does
    /// instead, so every call is answered. Hands `panicked` back if the thread is gone and the call can't run.
    pub fn submit(
        &self,
        call: impl FnOnce(&W) -> HostEvent + Send + 'static,
        panicked: HostEvent,
    ) -> Result<(), Box<HostEvent>> {
        let job = Job {
            call: Box::new(call),
            panicked,
        };
        self.jobs.send(job).map_err(|e| Box::new(e.0.panicked))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use shepherd_core::host::WorkspaceAsk;

    use super::*;
    use crate::host::runtime_channel;

    const LIMIT: Duration = Duration::from_secs(10);

    fn probed(why: &str) -> HostEvent {
        HostEvent::WorkspaceProbed {
            ask: WorkspaceAsk::ClientCwd,
            repo: Err(why.to_owned()),
        }
    }

    #[test]
    fn a_call_that_panics_is_answered_with_its_fallback_and_later_calls_still_run() {
        let (tx, rx) = runtime_channel();
        let worker = Worker::spawn("f27-test-worker", (), tx).unwrap();
        worker
            .submit(|_| panic!("boom"), probed("fallback"))
            .unwrap();
        worker.submit(|_| probed("ok"), probed("unused")).unwrap();
        let answers = [rx.recv_timeout(LIMIT), rx.recv_timeout(LIMIT)];
        assert_eq!(
            answers,
            [
                Some(RuntimeEvent::Worktrees(probed("fallback"))),
                Some(RuntimeEvent::Worktrees(probed("ok")))
            ]
        );
    }
}
