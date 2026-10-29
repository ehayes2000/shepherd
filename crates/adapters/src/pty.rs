//! `Pty` on top of portable-pty. Each session gets a reader thread that forwards output until EOF, then
//! reaps the child, forgets the session, and sends `Exited`, in that order.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, native_pty_system};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec};

struct Live {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

type Sessions = Arc<Mutex<HashMap<SessionId, Live>>>;

pub struct PortablePty {
    sink: Arc<dyn PtyEventSink>,
    sessions: Sessions,
}

impl PortablePty {
    pub fn new(sink: Arc<dyn PtyEventSink>) -> Self {
        Self {
            sink,
            sessions: Arc::default(),
        }
    }

    fn with_live<T>(
        &self,
        session: &SessionId,
        f: impl FnOnce(&mut Live) -> std::io::Result<T>,
    ) -> Result<T, PtyError> {
        let mut sessions = lock(&self.sessions);
        let live = sessions.get_mut(session).ok_or(PtyError::UnknownSession)?;
        f(live).map_err(io_error)
    }
}

impl Pty for PortablePty {
    fn spawn(&mut self, session: SessionId, spec: SpawnSpec) -> Result<(), PtyError> {
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&session) {
            return Err(PtyError::AlreadyExists);
        }

        let pair = native_pty_system()
            .openpty(pty_size(spec.size))
            .map_err(io_error)?;
        let mut cmd = CommandBuilder::new(&spec.command);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::SpawnFailed(e.to_string()))?;
        // Only the child should hold the slave, so the reader sees EOF when it exits.
        drop(pair.slave);

        let master = pair.master;
        let handles = master
            .try_clone_reader()
            .and_then(|reader| Ok((reader, master.take_writer()?)));
        let (reader, writer) = match handles {
            Ok(handles) => handles,
            Err(e) => {
                let _ = child.kill();
                return Err(io_error(e));
            }
        };
        let mut killer = child.clone_killer();

        // The thread can't forget the session before it's inserted: we hold the lock until then.
        let pump = {
            let (session, sessions, sink) =
                (session.clone(), self.sessions.clone(), self.sink.clone());
            move || pump(session, reader, child, sessions, sink)
        };
        if let Err(e) = thread::Builder::new()
            .name(format!("pty-{session}"))
            .spawn(pump)
        {
            let _ = killer.kill();
            return Err(io_error(e));
        }

        sessions.insert(
            session,
            Live {
                master,
                writer,
                killer,
            },
        );
        Ok(())
    }

    fn write(&mut self, session: &SessionId, bytes: &[u8]) -> Result<(), PtyError> {
        self.with_live(session, |live| {
            live.writer.write_all(bytes)?;
            live.writer.flush()
        })
    }

    fn resize(&mut self, session: &SessionId, size: PtySize) -> Result<(), PtyError> {
        self.with_live(session, |live| {
            live.master
                .resize(pty_size(size))
                .map_err(std::io::Error::other)
        })
    }

    fn kill(&mut self, session: &SessionId) -> Result<(), PtyError> {
        self.with_live(session, |live| live.killer.kill())
    }
}

impl Drop for PortablePty {
    /// Don't leave processes behind when the port goes away.
    fn drop(&mut self) {
        for live in lock(&self.sessions).values_mut() {
            let _ = live.killer.kill();
        }
    }
}

/// Runs on the session's reader thread until the child exits.
fn pump(
    session: SessionId,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    sessions: Sessions,
    sink: Arc<dyn PtyEventSink>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => sink.send(PtyEvent::Output {
                session: session.clone(),
                bytes: buf[..n].to_vec(),
            }),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            // EIO once the slave side is closed.
            Err(_) => break,
        }
    }
    let code = match child.wait() {
        Ok(status) if status.signal().is_none() => i32::try_from(status.exit_code()).ok(),
        _ => None,
    };
    lock(&sessions).remove(&session);
    sink.send(PtyEvent::Exited { session, code });
}

fn lock(sessions: &Sessions) -> MutexGuard<'_, HashMap<SessionId, Live>> {
    sessions.lock().unwrap_or_else(PoisonError::into_inner)
}

fn pty_size(size: PtySize) -> portable_pty::PtySize {
    portable_pty::PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn io_error(e: impl std::fmt::Display) -> PtyError {
    PtyError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepherd_testkit::contracts;

    #[test]
    fn portable_pty_satisfies_contract() {
        contracts::pty::suite(PortablePty::new);
    }
}
