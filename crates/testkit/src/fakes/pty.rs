//! An in-memory `Pty` that runs scripted programs instead of processes. A test registers a program per
//! command name; spawning any other command fails with `SpawnFailed`.
//!
//! Events are sent synchronously, from inside the `Pty` call that caused them. Output follows the port's credit
//! rules: it goes out in chunks of at most [`MAX_OUTPUT_CHUNK`], and once a session's unacknowledged output reaches
//! [`OUTPUT_CREDIT`] the rest waits, like a child blocked on a full pty, until [`Pty::ack`] makes room. A program
//! that exits with output still waiting exits once it's all sent. A kill drops what was waiting.
//!
//! As a [`PtyHandoff`], a session's token is the running program itself: an export moves every live session out (a
//! copy stays behind for [`PtyHandoff::resume`]), and an adopting `FakePty` runs it on from where it was, credit
//! reset. [`FakePtyToken::unadoptable`] makes a token that adopting refuses, standing in for a session whose process
//! died during the handoff.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use shepherd_core::ids::SessionId;
use shepherd_core::ports::{
    MAX_OUTPUT_CHUNK, OUTPUT_CREDIT, Pty, PtyError, PtyEvent, PtyEventSink, PtyHandoff, PtySize,
    SpawnSpec,
};

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
    flood: Option<Vec<u8>>,
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

    /// After this step's output, prints `pattern` over and over until killed, as fast as credit allows (`yes`).
    pub fn flood(&mut self, pattern: impl AsRef<[u8]>) {
        self.flood = Some(pattern.as_ref().to_vec()).filter(|p| !p.is_empty());
    }

    /// Exits after this step's output is sent.
    pub fn exit(&mut self, code: i32) {
        self.exit = Some(code);
    }
}

pub type Program = Arc<dyn Fn(ProgramInput<'_>, &mut ScriptedProcess<'_>) + Send + Sync>;

#[derive(Clone)]
struct Running {
    spec: SpawnSpec,
    size: PtySize,
    program: Program,
    /// Printed and not sent yet, for want of credit.
    pending: VecDeque<u8>,
    /// Printed forever once `pending` is empty, and where in the pattern the next byte comes from.
    flood: Option<(Vec<u8>, usize)>,
    /// Output sent and not acknowledged.
    unacked: usize,
    /// The program has exited; its `Exited` goes out once `pending` is.
    exited: Option<i32>,
}

impl Running {
    /// The next chunk to send, if there is something to send and credit for it.
    fn next_chunk(&mut self) -> Option<Vec<u8>> {
        if self.unacked >= OUTPUT_CREDIT {
            return None;
        }
        let chunk: Vec<u8> = if !self.pending.is_empty() {
            let n = self.pending.len().min(MAX_OUTPUT_CHUNK);
            self.pending.drain(..n).collect()
        } else if let Some((pattern, at)) = &mut self.flood {
            (0..MAX_OUTPUT_CHUNK)
                .map(|_| {
                    let byte = pattern[*at];
                    *at = (*at + 1) % pattern.len();
                    byte
                })
                .collect()
        } else {
            return None;
        };
        self.unacked += chunk.len();
        Some(chunk)
    }
}

pub struct FakePty {
    sink: Arc<dyn PtyEventSink>,
    programs: HashMap<String, Program>,
    sessions: HashMap<SessionId, Running>,
    /// What the last export took out, for `resume`.
    exported: HashMap<SessionId, Running>,
    fail_next: Option<PtyError>,
}

/// One exported session of a [`FakePty`].
#[derive(Clone)]
pub struct FakePtyToken {
    running: Running,
    unadoptable: bool,
}

impl FakePtyToken {
    /// This token, refused by `adopt` as if its process had died during the handoff.
    pub fn unadoptable(self) -> Self {
        Self {
            unadoptable: true,
            ..self
        }
    }

    /// The size the session's terminal had when it was exported.
    pub fn size(&self) -> PtySize {
        self.running.size
    }
}

impl std::fmt::Debug for FakePtyToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakePtyToken")
            .field("command", &self.running.spec.command)
            .field("size", &self.running.size)
            .field("unadoptable", &self.unadoptable)
            .finish_non_exhaustive()
    }
}

impl FakePty {
    pub fn new(sink: Arc<dyn PtyEventSink>) -> Self {
        Self {
            sink,
            programs: HashMap::new(),
            sessions: HashMap::new(),
            exported: HashMap::new(),
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

    /// The next `Pty` call returns `error` without doing anything. `ack` doesn't count: the runtime acks after
    /// every output, so it would take a failure meant for another call.
    pub fn fail_next(&mut self, error: PtyError) {
        self.fail_next = Some(error);
    }

    pub fn is_running(&self, session: &SessionId) -> bool {
        self.sessions.contains_key(session)
    }

    pub fn size(&self, session: &SessionId) -> Option<PtySize> {
        self.sessions.get(session).map(|r| r.size)
    }

    /// How much of `session`'s output has been sent and not acknowledged.
    pub fn unacked(&self, session: &SessionId) -> Option<usize> {
        self.sessions.get(session).map(|r| r.unacked)
    }

    fn injected(&mut self) -> Result<(), PtyError> {
        self.fail_next.take().map_or(Ok(()), Err)
    }

    fn running(&mut self, session: &SessionId) -> Result<&mut Running, PtyError> {
        self.sessions
            .get_mut(session)
            .ok_or(PtyError::UnknownSession)
    }

    /// Runs one step of `session`'s program and sends what it produced, as far as credit allows. A program that
    /// has exited, its last output still waiting, takes no more steps.
    fn step(&mut self, session: &SessionId, input: ProgramInput<'_>) {
        let Some(running) = self.sessions.get_mut(session) else {
            return;
        };
        if running.exited.is_some() {
            return;
        }
        let mut process = ScriptedProcess {
            spec: &running.spec,
            size: running.size,
            output: Vec::new(),
            flood: None,
            exit: None,
        };
        (running.program)(input, &mut process);
        let ScriptedProcess {
            output,
            flood,
            exit,
            ..
        } = process;
        running.pending.extend(output);
        if let Some(pattern) = flood {
            running.flood = Some((pattern, 0));
        }
        running.exited = exit;
        self.deliver(session);
    }

    /// Sends `session`'s waiting output while it has credit, then its exit if it has exited and nothing waits.
    fn deliver(&mut self, session: &SessionId) {
        let Some(running) = self.sessions.get_mut(session) else {
            return;
        };
        while let Some(bytes) = running.next_chunk() {
            self.sink.send(PtyEvent::Output {
                session: session.clone(),
                bytes,
            });
        }
        if running.pending.is_empty()
            && let Some(code) = running.exited
        {
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
                pending: VecDeque::new(),
                flood: None,
                unacked: 0,
                exited: None,
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

    fn ack(&mut self, session: &SessionId, bytes: usize) -> Result<(), PtyError> {
        let running = self.running(session)?;
        running.unacked = running.unacked.saturating_sub(bytes);
        self.deliver(session);
        Ok(())
    }
}

impl PtyHandoff for FakePty {
    type Token = FakePtyToken;

    /// Exports every session whose program hasn't exited. One that has, its last output waiting for credit, stays
    /// to finish.
    fn export(&mut self) -> Result<Vec<(SessionId, FakePtyToken)>, PtyError> {
        self.injected()?;
        let live: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, r)| r.exited.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        let mut tokens = Vec::new();
        for id in live {
            if let Some(running) = self.sessions.remove(&id) {
                self.exported.insert(id.clone(), running.clone());
                let token = FakePtyToken {
                    running,
                    unadoptable: false,
                };
                tokens.push((id, token));
            }
        }
        tokens.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(tokens)
    }

    fn resume(&mut self) {
        let exported: Vec<(SessionId, Running)> = self.exported.drain().collect();
        for (id, running) in exported {
            self.sessions.insert(id.clone(), running);
            self.deliver(&id);
        }
    }

    fn adopt(&mut self, session: SessionId, token: FakePtyToken) -> Result<(), PtyError> {
        self.injected()?;
        if self.sessions.contains_key(&session) {
            return Err(PtyError::AlreadyExists);
        }
        if token.unadoptable {
            return Err(PtyError::Io("the session's process is gone".into()));
        }
        let mut running = token.running;
        running.unacked = 0;
        self.sessions.insert(session.clone(), running);
        self.deliver(&session);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
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
            .with_program("cat", cat)
            .with_program("sh", |input, p| {
                let script = p.spec().args.get(1).cloned().unwrap_or_default();
                match script.as_str() {
                    "read _; stty size" => {
                        if let ProgramInput::Stdin(bytes) = input
                            && bytes.contains(&b'\n')
                        {
                            let size = p.size();
                            p.print(format!("{} {}\r\n", size.rows, size.cols));
                            p.exit(0);
                        }
                    }
                    // There is no echo or SIGHUP here, so both reduce to "say ready, then cat".
                    "stty -echo; echo ready; cat" | "trap '' HUP; echo ready; cat" => {
                        if input == ProgramInput::Start {
                            p.print("ready\r\n");
                        } else {
                            cat(input, p);
                        }
                    }
                    // No SIGHUP here either.
                    "trap '' HUP; exec yes" => {
                        if input == ProgramInput::Start {
                            p.flood("y\r\n");
                        }
                    }
                    "read _; exit 7" => {
                        if let ProgramInput::Stdin(bytes) = input
                            && bytes.contains(&b'\n')
                        {
                            p.exit(7);
                        }
                    }
                    "stty -opost; exec seq 1 400000" => {
                        if input == ProgramInput::Start {
                            let text: String = (1..=400_000).map(|i| format!("{i}\n")).collect();
                            p.print(text);
                            p.exit(0);
                        }
                    }
                    _ => p.exit(127),
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
            .with_program("yes", |input, p| {
                if input == ProgramInput::Start {
                    p.flood("y\r\n");
                }
            })
    }

    /// Echoes stdin until ^D, then exits 0.
    fn cat(input: ProgramInput<'_>, p: &mut ScriptedProcess<'_>) {
        if let ProgramInput::Stdin(bytes) = input {
            match bytes.iter().position(|&b| b == 0x04) {
                Some(eof) => {
                    p.print(&bytes[..eof]);
                    p.exit(0);
                }
                None => p.print(bytes),
            }
        }
    }

    mod contract {
        use super::contract_fake;

        crate::pty_contract!(contract_fake);
    }

    mod handoff_contract {
        use super::contract_fake;

        crate::pty_handoff_contract!(contract_fake);
    }

    #[test]
    fn an_unadoptable_token_is_refused_and_the_session_stays_unknown() {
        let (tx, _rx) = mpsc::channel();
        let mut old = contract_fake(Arc::new(ChannelSink(tx.clone())));
        let s = SessionId::new("s");
        old.spawn(s.clone(), spec("cat", &[])).unwrap();
        let (id, token) = old.export().unwrap().remove(0);

        let mut new = contract_fake(Arc::new(ChannelSink(tx)));
        assert!(matches!(
            new.adopt(id, token.unadoptable()),
            Err(PtyError::Io(_))
        ));
        assert!(!new.is_running(&s));
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
