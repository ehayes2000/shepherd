//! Runs a synchronous port's calls on a thread of their own, so a slow one (git over the network, a user's hook) never
//! stalls the runtime loop. Decision 0011's pattern: the call runs in the background and its result comes back into the
//! loop's channel as an event (e.g. [`RuntimeEvent::Worktrees`]), which the loop hands to core like any other.
//!
//! One thread per port, taking calls in the order they were submitted, so results come back in that order too. A call
//! that may run for long and doesn't need that order (creating a worktree) can instead run on a thread of its own
//! ([`Worker::run_alone`]), so it never holds up the others: herdr's model, one thread per operation.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;

use shepherd_core::host::HostEvent;

use super::runtime::{RuntimeEvent, RuntimeTx};

/// A call to run on the worker: what it computes from the port, and what to report instead if it panics.
struct Job<W> {
    call: Box<dyn FnOnce(&W) -> HostEvent + Send>,
    panicked: HostEvent,
}

impl<W> Job<W> {
    /// Runs the call, reporting `panicked` if it panics.
    fn run(self, port: &W, thread: &str) -> HostEvent {
        let call = self.call;
        catch_unwind(AssertUnwindSafe(|| call(port))).unwrap_or_else(|_| {
            tracing::error!(%thread, "a port call panicked");
            self.panicked
        })
    }
}

/// Owns a port, shared by a worker thread and the threads of [`Worker::run_alone`]. Dropping it lets the worker
/// thread finish the call it is in and stop; no thread is joined, since a call may be a git command that runs for long.
pub struct Worker<W> {
    jobs: Sender<Job<W>>,
    port: Arc<W>,
    tx: RuntimeTx,
    wrap: fn(HostEvent) -> RuntimeEvent,
}

impl<W: Send + Sync + 'static> Worker<W> {
    /// Moves `port` to a new thread called `name`, which reports each call's result into `tx`, wrapped by `wrap`.
    pub fn spawn(
        name: &str,
        port: W,
        tx: RuntimeTx,
        wrap: fn(HostEvent) -> RuntimeEvent,
    ) -> io::Result<Self> {
        let (jobs, calls) = mpsc::channel::<Job<W>>();
        let port = Arc::new(port);
        let thread = name.to_owned();
        let (shared, results) = (port.clone(), tx.clone());
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                for job in calls {
                    results.send(wrap(job.run(&shared, &thread)));
                }
            })?;
        Ok(Self {
            jobs,
            port,
            tx,
            wrap,
        })
    }

    /// The port, for a call that never blocks (e.g. `Worktrees::cancel_create`), made on the caller's thread.
    pub fn port(&self) -> &W {
        &self.port
    }

    /// Runs `call` on a new thread called `name`, alongside the worker thread and any other such call. Its result
    /// arrives as [`Worker::submit`]'s does; if it panics, `panicked` does instead. Hands `panicked` back if the thread
    /// can't be started.
    pub fn run_alone(
        &self,
        name: &str,
        call: impl FnOnce(&W) -> HostEvent + Send + 'static,
        panicked: HostEvent,
    ) -> Result<(), Box<HostEvent>> {
        let unanswered = panicked.clone();
        let job = Job {
            call: Box::new(call),
            panicked,
        };
        let (port, tx, wrap) = (self.port.clone(), self.tx.clone(), self.wrap);
        let thread = name.to_owned();
        let spawned = thread::Builder::new()
            .name(thread.clone())
            .spawn(move || tx.send(wrap(job.run(&port, &thread))));
        spawned.map(drop).map_err(|error| {
            tracing::error!(%error, name, "can't start a thread for a port call");
            Box::new(unanswered)
        })
    }

    /// Queues `call`. Never blocks. Its result arrives wrapped as [`Worker::spawn`] was told; if it panics,
    /// `panicked` does instead. Hands `panicked` back if the thread is already gone and the call can't run. Not
    /// guaranteed: if the thread dies after accepting a call (outside the call's own panic, which is caught), that call
    /// and any queued behind it are dropped unanswered.
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
        let worker = Worker::spawn("f27-test-worker", (), tx, RuntimeEvent::Worktrees).unwrap();
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

    #[test]
    fn a_call_run_alone_that_panics_is_answered_with_its_fallback() {
        let (tx, rx) = runtime_channel();
        let worker = Worker::spawn("t37-test-worker", (), tx, RuntimeEvent::Worktrees).unwrap();
        worker
            .run_alone("t37-test-alone", |_| panic!("boom"), probed("fallback"))
            .unwrap();
        assert_eq!(
            rx.recv_timeout(LIMIT),
            Some(RuntimeEvent::Worktrees(probed("fallback")))
        );
    }
}
