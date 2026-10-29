//! An in-memory `Pty` that runs scripted programs instead of processes. A test registers a program per
//! command name; spawning any other command fails with `SpawnFailed`.
//!
//! Events are sent synchronously, from inside the `Pty` call that caused them.

use std::collections::HashMap;
use std::sync::Arc;

use shepherd_core::ids::SessionId;
use shepherd_core::ports::{Pty, PtyError, PtyEvent, PtyEventSink, PtySize, SpawnSpec};

/// What a scripted program is reacting to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramInput<'a> {
    Start,
    Stdin(&'a [u8]),
    Resized,
}

/// A running scripted program's view of itself: what it was spawned with, and how it answers.
pub struct ScriptedProcess<'a> {
    spec: &'a SpawnSpec,
    size: PtySize,
    output: Vec<u8>,
    exit: Option<i32>,
}

impl ScriptedProcess<'_> {
    pub fn spec(&self) -> &SpawnSpec {
        self.spec
    }

    pub fn size(&self) -> PtySize {
        self.size
    }

    pub fn print(&mut self, bytes: impl AsRef<[u8]>) {
        self.output.extend_from_slice(bytes.as_ref());
    }

    /// Exits after this step's output is sent.
    pub fn exit(&mut self, code: i32) {
        self.exit = Some(code);
    }
}

pub type Program = Arc<dyn Fn(ProgramInput<'_>, &mut ScriptedProcess<'_>) + Send + Sync>;

struct Running {
    spec: SpawnSpec,
    size: PtySize,
    program: Program,
}

pub struct FakePty {
    sink: Arc<dyn PtyEventSink>,
    programs: HashMap<String, Program>,
    sessions: HashMap<SessionId, Running>,
    fail_next: Option<PtyError>,
}

impl FakePty {
    pub fn new(sink: Arc<dyn PtyEventSink>) -> Self {
        Self {
            sink,
            programs: HashMap::new(),
            sessions: HashMap::new(),
            fail_next: None,
        }
    }

    /// Registers `program` to run whenever `command` is spawned.
    pub fn with_program(
        mut self,
        command: impl Into<String>,
        program: impl Fn(ProgramInput<'_>, &mut ScriptedProcess<'_>) + Send + Sync + 'static,
    ) -> Self {
        self.programs.insert(command.into(), Arc::new(program));
        self
    }

    /// The next `Pty` call returns `error` without doing anything.
    pub fn fail_next(&mut self, error: PtyError) {
        self.fail_next = Some(error);
    }

    pub fn is_running(&self, session: &SessionId) -> bool {
        self.sessions.contains_key(session)
    }

    pub fn size(&self, session: &SessionId) -> Option<PtySize> {
        self.sessions.get(session).map(|r| r.size)
    }

    fn injected(&mut self) -> Result<(), PtyError> {
        self.fail_next.take().map_or(Ok(()), Err)
    }

    fn running(&mut self, session: &SessionId) -> Result<&mut Running, PtyError> {
        self.sessions
            .get_mut(session)
            .ok_or(PtyError::UnknownSession)
    }

    /// Runs one step of `session`'s program and sends what it produced.
    fn step(&mut self, session: &SessionId, input: ProgramInput<'_>) {
        let Some(running) = self.sessions.get(session) else {
            return;
        };
        let mut process = ScriptedProcess {
            spec: &running.spec,
            size: running.size,
            output: Vec::new(),
            exit: None,
        };
        (running.program)(input, &mut process);
        let ScriptedProcess { output, exit, .. } = process;

        if !output.is_empty() {
            self.sink.send(PtyEvent::Output {
                session: session.clone(),
                bytes: output,
            });
        }
        if let Some(code) = exit {
            self.exit(session, Some(code));
        }
    }

    fn exit(&mut self, session: &SessionId, code: Option<i32>) {
        self.sessions.remove(session);
        self.sink.send(PtyEvent::Exited {
            session: session.clone(),
            code,
        });
    }
}

impl Pty for FakePty {
    fn spawn(&mut self, session: SessionId, spec: SpawnSpec) -> Result<(), PtyError> {
        self.injected()?;
        if self.sessions.contains_key(&session) {
            return Err(PtyError::AlreadyExists);
        }
        let program = self.programs.get(&spec.command).cloned().ok_or_else(|| {
            PtyError::SpawnFailed(format!("no scripted program for {:?}", spec.command))
        })?;
        let size = spec.size;
        self.sessions.insert(
            session.clone(),
            Running {
                spec,
                size,
                program,
            },
        );
        self.step(&session, ProgramInput::Start);
        Ok(())
    }

    fn write(&mut self, session: &SessionId, bytes: &[u8]) -> Result<(), PtyError> {
        self.injected()?;
        self.running(session)?;
        self.step(session, ProgramInput::Stdin(bytes));
        Ok(())
    }

    fn resize(&mut self, session: &SessionId, size: PtySize) -> Result<(), PtyError> {
        self.injected()?;
        self.running(session)?.size = size;
        self.step(session, ProgramInput::Resized);
        Ok(())
    }

    fn kill(&mut self, session: &SessionId) -> Result<(), PtyError> {
        self.injected()?;
        self.running(session)?;
        self.exit(session, None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::contracts;
    use crate::contracts::pty::{ChannelSink, spec};

    /// Scripted stand-ins for the commands the contract runs, doing just what the contract asks of them.
    fn contract_fake(sink: Arc<dyn PtyEventSink>) -> FakePty {
        FakePty::new(sink)
            .with_program("printf", |input, p| {
                if input == ProgramInput::Start {
                    let text = p.spec().args.first().cloned().unwrap_or_default();
                    p.print(text);
                    p.exit(0);
                }
            })
            .with_program("cat", |input, p| {
                if let ProgramInput::Stdin(bytes) = input {
                    match bytes.iter().position(|&b| b == 0x04) {
                        Some(eof) => {
                            p.print(&bytes[..eof]);
                            p.exit(0);
                        }
                        None => p.print(bytes),
                    }
                }
            })
            // `sh -c 'read _; stty size'`
            .with_program("sh", |input, p| {
                if let ProgramInput::Stdin(bytes) = input
                    && bytes.contains(&b'\n')
                {
                    let size = p.size();
                    p.print(format!("{} {}\r\n", size.rows, size.cols));
                    p.exit(0);
                }
            })
            .with_program("printenv", |input, p| {
                if input == ProgramInput::Start {
                    let name = p.spec().args.first().cloned().unwrap_or_default();
                    let value = p
                        .spec()
                        .env
                        .iter()
                        .find(|(k, _)| *k == name)
                        .map(|(_, v)| v.clone());
                    match value {
                        Some(value) => {
                            p.print(format!("{value}\r\n"));
                            p.exit(0);
                        }
                        None => p.exit(1),
                    }
                }
            })
            .with_program("pwd", |input, p| {
                if input == ProgramInput::Start {
                    let cwd = p.spec().cwd.display().to_string();
                    p.print(format!("{cwd}\r\n"));
                    p.exit(0);
                }
            })
    }

    #[test]
    fn fake_satisfies_contract() {
        contracts::pty::suite(contract_fake);
    }

    #[test]
    fn fail_next_fails_only_the_next_call() {
        let (tx, _rx) = mpsc::channel();
        let mut pty = contract_fake(Arc::new(ChannelSink(tx)));
        let s = SessionId::new("s");
        pty.fail_next(PtyError::Io("disk on fire".into()));

        assert_eq!(
            pty.spawn(s.clone(), spec("cat", &[])),
            Err(PtyError::Io("disk on fire".into()))
        );
        assert!(!pty.is_running(&s));
        assert_eq!(pty.spawn(s.clone(), spec("cat", &[])), Ok(()));
        assert!(pty.is_running(&s));
    }
}
