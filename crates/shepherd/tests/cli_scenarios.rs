//! `shepherd sessions`, `shepherd new` (T46) and `shepherd ui` (T50) across machines, end to end on fakes: the commands drive a real
//! client runtime on machine A's host, which trusts B ("laptop") and reaches it through a tunnel, with both hosts'
//! runtimes on all-fake ports. The built binary's own tests are in `sessions_cli.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use shepherd::cli::prompt::Prompt;
use shepherd::cli::sessions::{New, new, sessions};
use shepherd::cli::ui::{Screenshot, Ui, prepare, screenshot, ui};
use shepherd::cli::{CliError, Drive, WaitError, target};
use shepherd::client::{ClientError, ClientRuntime, Connector};
use shepherd_core::acpp::{
    Dir, Envelope, Frame, FrameDraft, PeerTrusted, Seq, ShepherdFrame, Source, SourceKind,
    Timestamp,
};
use shepherd_core::client::automation::SessionStatus;
use shepherd_core::client::{AskId, Client, MachineStatus};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::layout::{Content, Pane, PaneId, SplitDir};
use shepherd_core::ports::{EventLog, Link, LinkEvent, LinkEventSink, LogId, Reply};
use shepherd_core::wire::{
    ClientMessage, ClientRequest, CloseWorktree, Hello, HookFrame, HostResponse, PROTOCOL_VERSION,
    SessionClose, SessionPrompt, SessionRename, WireError, decode_client_message, encode_response,
};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::launcher::FakeHostLauncher;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use shepherd_testkit::fakes::pty::{ProgramInput, ScriptedProcess};
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

const KEY_A: [u8; 32] = [0xaa; 32];
const KEY_B: [u8; 32] = [0xbb; 32];
/// A liveness bound, never a measurement.
const LIVENESS: Duration = Duration::from_secs(60);
/// The command of the built-in `claude` profile, which runs the fake agent on A.
const CLAUDE: &str = "claude";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The host log of a machine that trusts `peer`, called `name`.
fn trusting(peer: &MachineId, name: &str) -> Vec<Envelope> {
    let mut driver = HostDriver::new();
    let frame = ShepherdFrame::PeerTrusted(PeerTrusted {
        node: peer.clone(),
        name: name.into(),
    });
    driver.append_draft(FrameDraft::new(
        frame.to_frame(),
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        None,
    ));
    driver.log().to_vec()
}

type Runtime = ClientRuntime<FakeLink, Connector<FakeLink>, FakeHostLauncher>;

/// How the fake agent answers a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    /// Submits it, writes `Done: <prompt>` to its transcript and ends its turn.
    Reply,
    /// Submits it, then asks permission to use this tool.
    Ask(&'static str),
    /// Submits it and works on it forever.
    Silent,
    /// Submits it, works for a while (this many rounds of the world in which something else happened), then replies.
    Slow(u32),
    /// Submits it and ends its turn without writing anything.
    Quiet,
    /// Submits it, then the session is closed (by someone else) before the turn ends.
    Closed,
    /// Submits it, then the agent exits before the turn ends.
    Exits,
}

/// What the fake agent saw: a session's `claude` started, or was typed into.
enum Seen {
    Started(String),
    Typed(String, Vec<u8>),
}

/// A fake Claude Code, run as the `claude` profile's command on A. It sees what its PTY is typed; the world answers as
/// Claude Code would through its hooks ([`World::react`]), on a hook conn of its own, and writes its replies to A's fake
/// transcripts.
#[derive(Clone, Default)]
struct FakeAgent {
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Where each session's shell was started, by session.
    shells: Arc<Mutex<Vec<(String, PathBuf)>>>,
}

/// The session a fake program runs in.
fn session_of(p: &ScriptedProcess<'_>) -> String {
    p.spec()
        .env
        .iter()
        .find(|(k, _)| k == shepherd::hook::SESSION_VAR)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

impl FakeAgent {
    /// A shell that echoes, as the harness's does, and notes where it was started.
    fn shell(&self) -> Program {
        let shells = self.shells.clone();
        let echo = echo_shell();
        Arc::new(move |input, p| {
            if let ProgramInput::Start = input {
                lock(&shells).push((session_of(p), p.spec().cwd.clone()));
            }
            echo(input, p);
        })
    }

    /// Where session `session`'s shell was started.
    fn shell_cwd(&self, session: &SessionId) -> Option<PathBuf> {
        lock(&self.shells)
            .iter()
            .find(|(s, _)| s == session.as_str())
            .map(|(_, cwd)| cwd.clone())
    }

    fn program(&self) -> Program {
        let seen = self.seen.clone();
        Arc::new(move |input, p| {
            let session = session_of(p);
            match input {
                ProgramInput::Start => lock(&seen).push(Seen::Started(session)),
                ProgramInput::Stdin(bytes) => {
                    lock(&seen).push(Seen::Typed(session, bytes.to_vec()))
                }
                ProgramInput::Resized => {}
            }
        })
    }

    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *lock(&self.seen))
    }
}

/// The transcript the fake agent of session `session` writes.
fn transcript_of(session: &str) -> PathBuf {
    PathBuf::from(CLAUDE_HOME).join(format!("projects/-app/agent-{session}.jsonl"))
}

/// Machine A's host trusting B ("laptop"), in workspace `app` (a repo the fake worktrees know), running a fake agent as
/// its `claude`; B's host, letting A in unless told not to; and a command-line client on A's host.
struct World {
    _tmp: tempfile::TempDir,
    a: HostHarness,
    b: HostHarness,
    client: Runtime,
    /// B's host is running: a stopped one isn't stepped.
    b_running: bool,
    /// What the client's clock says.
    now: Arc<Mutex<jiff::Timestamp>>,
    agent: FakeAgent,
    /// The fake agent's hook conn to A.
    hooks: FakeClient,
    /// How the fake agent answers each next prompt; [`Answer::Reply`] once these run out.
    answers: VecDeque<Answer>,
    /// What each session's agent has been typed and not yet submitted.
    typing: BTreeMap<String, Vec<u8>>,
    /// A slow answer under way: its session, its prompt, and the rounds left.
    working: Option<(String, String, u32)>,
    /// Every prompt the fake agent was submitted, as its input box got it.
    submitted: Vec<String>,
}

impl World {
    fn new() -> Self {
        Self::with(true)
    }

    /// `b_lets_a_in`: B trusts A, so A's tunnel to it is accepted.
    fn with(b_lets_a_in: bool) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let app = base.join("app");
        std::fs::create_dir(&app).unwrap();
        let worktrees = FakeWorktrees::new(base.join("worktrees"));
        worktrees.add_repo(&app, "main");
        let machines = FakeNetwork::new();
        let env = TestEnv::new();
        let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
        let a_log = FakeEventLog::new().with_envelopes(LogId::Host, trusting(&b_id, "laptop"));
        let agent = FakeAgent::default();
        let mut a = HostHarness::builder()
            .machines(machines.clone())
            .machine_key(KEY_A)
            .env(env.clone())
            .log(a_log)
            .worktrees(worktrees)
            .program(CLAUDE, agent.program())
            .program(SHELL, painter("on testbox", agent.shell()))
            .start();
        let b = HostHarness::builder()
            .machines(machines.clone())
            .machine_key(KEY_B)
            .env(env)
            .program(SHELL, painter("on laptop", echo_shell()))
            .start();
        if b_lets_a_in {
            machines.trust(&b.machine(), &a_id);
        }
        select_workspace(&mut a, &app);
        let now = Arc::new(Mutex::new(
            "2026-10-29T12:00:00Z".parse::<jiff::Timestamp>().unwrap(),
        ));
        let read = now.clone();
        let clock = move || Timestamp::parse(lock(&read).to_string()).unwrap();
        let net = a.network().clone();
        let connect: Connector<FakeLink> = Box::new(move |sink: Arc<dyn LinkEventSink>| {
            net.connect(sink).map_err(|e| e.to_string())
        });
        // Set up as `shepherd`'s own commands set up theirs.
        let mut client = ClientRuntime::new(
            MachineId::new("m_cli"),
            connect,
            FakeHostLauncher::new(),
            clock,
        );
        // As `shepherd ui screenshot` makes it.
        prepare(&mut client);
        let client = shepherd::cli::started(client);
        let hooks = a.connect();
        Self {
            _tmp: tmp,
            a,
            b,
            client,
            b_running: true,
            now,
            agent,
            hooks,
            answers: VecDeque::new(),
            typing: BTreeMap::new(),
            working: None,
            submitted: Vec::new(),
        }
    }

    /// Sends a Claude Code hook payload from session `session`'s agent, as `shepherd hook` would.
    fn hook(&mut self, session: &str, payload: Value) {
        self.hooks
            .request(ClientRequest::HookFrame(Box::new(HookFrame {
                session: SessionId::new(session),
                agent: AgentKind::ClaudeCode,
                payload,
            })));
    }

    /// The fake agent's reply to `prompt`: written to its transcript, then its turn ends.
    fn finish(&mut self, session: &str, prompt: &str) {
        // Written just after the prompt was recorded, as an agent's clock would have it.
        let at = self
            .prompted_at(session)
            .checked_add(jiff::SignedDuration::from_millis(1))
            .unwrap();
        let reply = Reply {
            text: format!("Done: {prompt}"),
            at: Some(at),
        };
        self.a
            .transcripts()
            .add_reply(transcript_of(session), AgentKind::ClaudeCode, reply);
        self.hook(session, json!({ "hook_event_name": "Stop" }));
    }

    /// When A recorded the last prompt it typed into session `session`.
    fn prompted_at(&self, session: &str) -> jiff::Timestamp {
        self.a
            .log()
            .read(LogId::Session(SessionId::new(session)), Seq::ZERO)
            .unwrap()
            .into_iter()
            .rev()
            .find(|e| {
                matches!(e.frame(), Frame::Shepherd(m)
                    if matches!(m.body(), ShepherdFrame::SessionPrompted(_)))
            })
            .map(|e| e.ts().instant())
            .expect("a prompt was recorded")
    }

    /// What the fake agent does about what it saw since the last time: reports its start, and answers each prompt
    /// submitted to it (a whole bracketed paste, then Enter). Returns whether it did anything.
    fn react(&mut self) -> bool {
        let mut acted = false;
        if let Some((session, prompt, left)) = self.working.take() {
            acted = true;
            match left {
                0 => self.finish(&session, &prompt),
                _ => self.working = Some((session, prompt, left - 1)),
            }
        }
        for seen in self.agent.take() {
            acted = true;
            match seen {
                Seen::Started(session) => {
                    let payload = json!({
                        "hook_event_name": "SessionStart",
                        "source": "startup",
                        "session_id": format!("agent-{session}"),
                        "transcript_path": transcript_of(&session),
                    });
                    self.hook(&session, payload);
                }
                Seen::Typed(session, bytes) => {
                    let typed = self.typing.entry(session.clone()).or_default();
                    typed.extend_from_slice(&bytes);
                    let Some(end) = typed.windows(7).position(|w| w == b"\x1b[201~\r") else {
                        continue;
                    };
                    let whole = typed.drain(..end + 7).collect::<Vec<u8>>();
                    let prompt = String::from_utf8(whole[6..whole.len() - 7].to_vec()).unwrap();
                    assert!(whole.starts_with(b"\x1b[200~"), "{whole:?}");
                    self.submitted.push(prompt.clone());
                    self.hook(
                        &session,
                        json!({ "hook_event_name": "UserPromptSubmit", "prompt": prompt }),
                    );
                    match self.answers.pop_front().unwrap_or(Answer::Reply) {
                        Answer::Reply => self.finish(&session, &prompt),
                        Answer::Ask(tool) => self.hook(
                            &session,
                            json!({ "hook_event_name": "PermissionRequest", "tool_name": tool }),
                        ),
                        Answer::Silent => {}
                        Answer::Slow(rounds) => self.working = Some((session, prompt, rounds)),
                        Answer::Quiet => {
                            self.hook(&session, json!({ "hook_event_name": "Stop" }));
                        }
                        Answer::Closed => {
                            self.hooks
                                .request(ClientRequest::SessionClose(SessionClose {
                                    session: SessionId::new(&session),
                                    worktree: CloseWorktree::Keep,
                                }));
                        }
                        Answer::Exits => self.hook(
                            &session,
                            json!({ "hook_event_name": "SessionEnd", "reason": "other" }),
                        ),
                    }
                }
            }
        }
        acted
    }

    /// Makes a session of the fake agent with `shepherd new`, and waits for it to be at its prompt. Returns its id.
    fn agent_session(&mut self, name: &str) -> String {
        let made = self
            .new_json(New {
                profile: Some("claude".into()),
                cwd: Some("app".into()),
                name: Some(name.into()),
                ..New::default()
            })
            .unwrap();
        let id = made["session"]["id"].as_str().unwrap().to_owned();
        let session = SessionId::new(&id);
        self.wait(
            &mut |c| {
                c.sessions()
                    .iter()
                    .find(|s| s.id == session)
                    .is_some_and(|s| SessionStatus::in_client(c, s) == SessionStatus::Idle)
            },
            LIVENESS,
        )
        .unwrap();
        id
    }

    /// What `shepherd prompt --json` prints, or why it failed.
    fn prompt_json(&mut self, prompt: Prompt) -> Result<Value, CliError> {
        let mut out = Vec::new();
        shepherd::cli::prompt::prompt(self, &prompt, true, &mut out)?;
        Ok(serde_json::from_slice(&out).unwrap())
    }

    /// The `session/prompted` entries in A's logs: what was typed, and by whom.
    fn prompted(&self) -> Vec<(String, String)> {
        let log = self.a.log();
        log.logs()
            .unwrap()
            .into_iter()
            .flat_map(|id| log.read(id, Seq::ZERO).unwrap())
            .filter_map(|e| match e.frame() {
                Frame::Shepherd(m) => match m.body() {
                    ShepherdFrame::SessionPrompted(p) => Some((p.text.clone(), p.by.to_string())),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn here(&self) -> PathBuf {
        self._tmp.path().to_path_buf()
    }

    /// What `shepherd sessions --json` prints.
    fn sessions_json(&mut self) -> Value {
        let mut out = Vec::new();
        sessions(self, true, &mut out).unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    /// What `shepherd new --json` prints, or why it failed.
    fn new_json(&mut self, args: New) -> Result<Value, String> {
        let mut out = Vec::new();
        let here = self.here();
        new(self, &args, &here, true, &mut out)?;
        Ok(serde_json::from_slice(&out).unwrap())
    }
}

impl Drive for World {
    fn client(&self) -> &Client {
        self.client.client()
    }

    fn screens(&self) -> &shepherd_tui::Screens {
        self.client.screens()
    }

    fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId {
        self.client.ask(machine, request)
    }

    /// Runs both hosts, the fake agent and the client until `done`. Once none of them has anything left to do, nothing
    /// ever will: whatever the limit, it passes ([`WaitError::TimedOut`]).
    fn wait(
        &mut self,
        done: &mut dyn FnMut(&Client) -> bool,
        _: Duration,
    ) -> Result<(), WaitError> {
        let mut quiet = 0;
        while quiet < 3 {
            if done(self.client.client()) {
                return Ok(());
            }
            self.a.step();
            if self.b_running {
                self.b.step();
            }
            let acted = self.react();
            quiet = match (self.client.pump(), acted) {
                (0, false) => quiet + 1,
                _ => 0,
            };
        }
        Err(WaitError::TimedOut)
    }
}

/// `inner`, which also prints `<label> first line`, a right-to-left override and `!`, and a line too long for a narrow
/// pane when it starts. Resized, it redraws its whole terminal: `<label> row 01` on each row but the last, and there
/// `wide xxx…x edge` across it, the cursor after it.
fn painter(label: &'static str, inner: Program) -> Program {
    Arc::new(move |input, p| {
        if input == ProgramInput::Resized {
            let size = p.size();
            let mut screen = "\x1b[H\x1b[2J".to_owned();
            for row in 1..size.rows {
                screen.push_str(&format!("{label} row {row:02}\r\n"));
            }
            let xs = "x".repeat(usize::from(size.cols).saturating_sub(10));
            screen.push_str(&format!("wide {xs}edge"));
            p.print(screen);
            return;
        }
        let start = input == ProgramInput::Start;
        inner(input, p);
        if start {
            p.print(format!(
                "{label} first line\u{202e}!\r\nthis line is long enough to run past the edge of a narrow pane without stopping\r\n"
            ));
        }
    })
}

/// Selects `app` as `host`'s workspace, as a client started there does with its hello.
fn select_workspace(host: &mut HostHarness, app: &Path) {
    let mut client = host.connect();
    client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("m_setup"),
        cwd: Some(app.to_str().unwrap().to_owned()),
        build: None,
    }));
    host.step();
    assert_eq!(host.host().workspace(), Some(app));
    client.close();
    host.step();
}

/// Makes a session named `name` on `host` with a client of its own. Returns its id.
fn session_on(host: &mut HostHarness, name: &str) -> SessionId {
    let mut client = host.connect();
    let new = client.new_session();
    host.step();
    let session = new_session_id(&client.take(), &new);
    client.request(ClientRequest::SessionRename(SessionRename {
        session: session.clone(),
        name: name.into(),
    }));
    host.step();
    client.close();
    host.step();
    session
}

fn field<'a>(sessions: &'a Value, key: &str) -> Vec<&'a str> {
    sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s[key].as_str().unwrap())
        .collect()
}

#[test]
fn sessions_lists_every_machines_sessions_newest_first_with_their_machines() {
    let mut w = World::new();
    let old = session_on(&mut w.a, "old");
    let theirs = session_on(&mut w.b, "theirs");
    let newest = session_on(&mut w.a, "newest");

    let listed = w.sessions_json();
    assert_eq!(
        field(&listed, "id"),
        [newest.as_str(), theirs.as_str(), old.as_str()]
    );
    assert_eq!(field(&listed, "machine"), ["testbox", "laptop", "testbox"]);
    assert_eq!(
        field(&listed, "machine_id")[1],
        w.b.machine().as_str(),
        "{listed}"
    );
    let machines: Vec<(&str, &str)> = listed["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m["status"].as_str().unwrap()))
        .collect();
    assert_eq!(
        machines,
        [("testbox", "connected"), ("laptop", "connected")]
    );
}

#[test]
fn sessions_lists_this_machines_sessions_and_says_which_peer_it_cannot_reach() {
    let mut w = World::with(false);
    let mine = session_on(&mut w.a, "mine");
    session_on(&mut w.b, "theirs");

    let listed = w.sessions_json();
    assert_eq!(field(&listed, "id"), [mine.as_str()]);
    let laptop = &listed["machines"][1];
    assert_eq!(laptop["status"], "unreachable", "{listed}");
    assert!(laptop["why"].is_string(), "{listed}");

    let mut out = Vec::new();
    sessions(&mut w, false, &mut out).unwrap();
    let human = String::from_utf8(out).unwrap();
    assert!(human.contains("(laptop is unreachable: "), "{human}");
}

#[test]
fn new_on_a_peer_starts_the_session_on_its_host() {
    let mut w = World::new();
    let made = w
        .new_json(New {
            machine: Some("laptop".into()),
            name: Some("remote".into()),
            ..New::default()
        })
        .unwrap();
    let session = &made["session"];
    assert_eq!(session["machine"], "laptop");
    assert_eq!(session["name"], "remote");
    let on_b: Vec<String> =
        w.b.host()
            .sessions()
            .into_iter()
            .map(|s| s.id.to_string())
            .collect();
    assert_eq!(on_b, [session["id"].as_str().unwrap()]);
    assert!(w.a.host().sessions().is_empty());
}

#[test]
fn new_without_a_cwd_makes_a_worktree_named_after_the_session_in_the_workspace() {
    let mut w = World::new();
    let made = w
        .new_json(New {
            name: Some("api".into()),
            ..New::default()
        })
        .unwrap();
    let session = &made["session"];
    assert_eq!(session["branch"], "api", "{made}");
    assert!(
        session["workspace"].as_str().unwrap().ends_with("/app"),
        "{made}"
    );
}

#[test]
fn new_on_a_peer_that_cannot_be_reached_fails_saying_why_and_makes_nothing() {
    let mut w = World::with(false);
    let err = w
        .new_json(New {
            machine: Some("laptop".into()),
            ..New::default()
        })
        .unwrap_err();
    assert!(err.contains("laptop can't be reached"), "{err}");
    assert!(w.b.host().sessions().is_empty());
}

#[test]
fn a_target_names_a_session_on_any_machine_and_an_ambiguous_one_lists_each_machine() {
    let mut w = World::new();
    let mine = session_on(&mut w.a, "mine");
    let theirs = session_on(&mut w.b, "theirs");
    session_on(&mut w.a, "twin");
    session_on(&mut w.b, "twin");
    w.sessions_json();

    assert_eq!(target(&w, "theirs").map(|s| s.id), Ok(theirs));
    assert_eq!(target(&w, mine.as_str()).map(|s| s.id), Ok(mine));
    let err = target(&w, "twin").unwrap_err();
    assert!(
        err.contains("more than one session is called `twin`")
            && err.contains("on testbox")
            && err.contains("on laptop"),
        "{err}"
    );
}

#[test]
fn sessions_leaves_out_the_last_known_sessions_of_a_peer_gone_since() {
    let mut w = World::new();
    let mine = session_on(&mut w.a, "mine");
    session_on(&mut w.b, "theirs");
    assert_eq!(field(&w.sessions_json(), "id").len(), 2);

    w.b.stop();
    w.b_running = false;
    // The client hears B is gone; its sessions stay in the sidebar, offline.
    w.wait(
        &mut |c| c.machines()[1].status != MachineStatus::Connected,
        LIVENESS,
    )
    .unwrap();
    assert_eq!(w.client.client().sessions().len(), 2);
    let listed = w.sessions_json();
    assert_eq!(field(&listed, "id"), [mine.as_str()]);
    assert_eq!(listed["machines"][1]["status"], "unreachable", "{listed}");
}

#[test]
fn new_on_a_peer_refuses_a_relative_cwd_and_makes_nothing() {
    let mut w = World::new();
    let err = w
        .new_json(New {
            machine: Some("laptop".into()),
            cwd: Some("sub".into()),
            ..New::default()
        })
        .unwrap_err();
    assert!(err.contains("relative"), "{err}");
    assert!(w.b.host().sessions().is_empty());
    // An absolute one is the peer's own path, passed as it is.
    let dir = w.here().join("there");
    std::fs::create_dir(&dir).unwrap();
    let made = w.new_json(New {
        machine: Some("laptop".into()),
        cwd: Some(dir.to_str().unwrap().to_owned()),
        ..New::default()
    });
    assert!(made.is_ok(), "{made:?}");
}

/// Forwards a stand-in host's link events to a channel.
struct ChannelSink(Mutex<std::sync::mpsc::Sender<LinkEvent>>);

impl LinkEventSink for ChannelSink {
    fn send(&self, event: LinkEvent) {
        let _ = lock(&self.0).send(event);
    }
}

#[test]
fn a_command_waiting_on_a_host_speaking_another_protocol_stops_at_once_saying_so() {
    let net = FakeNetwork::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut host = net.listen(Arc::new(ChannelSink(Mutex::new(tx)))).unwrap();
    let connect: Connector<FakeLink> =
        Box::new(move |sink: Arc<dyn LinkEventSink>| net.connect(sink).map_err(|e| e.to_string()));
    let clock = || Timestamp::parse("2026-10-29T12:00:00Z").unwrap();
    let mut client: Runtime = ClientRuntime::new(
        MachineId::new("m_cli"),
        connect,
        FakeHostLauncher::new(),
        clock,
    );
    client.start();
    // The stand-in answers the hello as a host of another protocol version.
    let (conn, hello) = loop {
        match rx.recv_timeout(LIVENESS).expect("the hello") {
            LinkEvent::Line { conn, line } => break (conn, line),
            _ => continue,
        }
    };
    let ClientMessage::Request(hello) = decode_client_message(&hello).unwrap() else {
        panic!("not a request");
    };
    let refusal = encode_response(&HostResponse {
        id: hello.id,
        result: Err(WireError::VersionMismatch {
            host: PROTOCOL_VERSION + 1,
            client: PROTOCOL_VERSION,
        }),
    });
    host.send(conn, refusal.strip_suffix(b"\n").unwrap_or(&refusal))
        .unwrap();
    // A liveness bound only: it must stop on the answer, long before this.
    let err = client.run_until(Client::settled, LIVENESS).unwrap_err();
    assert!(
        matches!(&err, ClientError::Fatal(why) if why.contains("another protocol version")),
        "{err:?}"
    );
}

#[test]
fn a_peer_that_does_not_send_its_sessions_in_time_is_left_out_by_the_commands_client() {
    let mut w = World::new();
    session_on(&mut w.b, "theirs");
    // Step until the client has B's hello answer, then stop B before it sends its sessions.
    let mut rounds = 0;
    let laptop = |w: &World| {
        w.client
            .client()
            .machines()
            .get(1)
            .map(|m| m.status.clone())
    };
    while laptop(&w) != Some(MachineStatus::Connected) {
        w.a.step();
        w.b.step();
        w.client.pump();
        rounds += 1;
        assert!(rounds < 100, "B never answered the hello");
    }
    assert!(!w.client.client().settled(), "B hasn't sent its sessions");
    w.b_running = false;
    // Past the commands' catch-up limit, B is given up on and the command goes on without it.
    *lock(&w.now) += shepherd::cli::PEER_CATCH_UP_LIMIT + jiff::SignedDuration::from_secs(1);
    w.client.tick();
    let listed = w.sessions_json();
    assert_eq!(listed["machines"][1]["status"], "unreachable", "{listed}");
    assert!(field(&listed, "id").is_empty(), "{listed}");
}

#[test]
fn new_on_this_machine_by_name_takes_a_relative_cwd_from_where_the_command_runs() {
    let mut w = World::new();
    let dir = w.here().join("app");
    let made = w
        .new_json(New {
            machine: Some("testbox".into()),
            cwd: Some("app".into()),
            ..New::default()
        })
        .unwrap();
    let id = SessionId::new(made["session"]["id"].as_str().unwrap());
    // Run there, and so given no worktree.
    assert_eq!(made["session"]["worktree"], Value::Null, "{made}");
    let session = w.a.host().sessions().into_iter().find(|s| s.id == id);
    assert!(session.is_some(), "made on this machine");
    let spawned = w.agent.shell_cwd(&id);
    assert_eq!(
        spawned.map(|p| std::fs::canonicalize(p).unwrap()),
        Some(std::fs::canonicalize(dir).unwrap())
    );
}

// ---------- prompt, reply, wait (T47) ----------

/// A prompt of `text` to `session`, bounded (for liveness) wherever it waits.
fn prompt_of(session: &str, text: &str) -> Prompt {
    Prompt {
        session: session.to_owned(),
        text: text.to_owned(),
        timeout: Some(LIVENESS),
        ..Prompt::default()
    }
}

/// What `shepherd reply --json` prints.
fn reply_json(w: &mut World, session: &str) -> Value {
    let mut out = Vec::new();
    shepherd::cli::prompt::reply(w, session, true, &mut out).unwrap();
    serde_json::from_slice(&out).unwrap()
}

/// What `shepherd wait --json` prints, or why it failed.
fn wait_json(w: &mut World, session: &str, timeout: Duration) -> Result<Value, CliError> {
    let mut out = Vec::new();
    shepherd::cli::prompt::wait(w, session, Some(timeout), true, &mut out)?;
    Ok(serde_json::from_slice(&out).unwrap())
}

#[test]
fn a_prompt_waited_for_comes_back_with_the_agents_reply() {
    let mut w = World::new();
    let id = w.agent_session("worker");
    let answer = w
        .prompt_json(Prompt {
            wait: true,
            ..prompt_of("worker", "fix the bug\nand say so")
        })
        .unwrap();
    assert_eq!(
        answer,
        json!({ "session": id, "status": "idle", "reply": "Done: fix the bug\nand say so",
                "asked": null })
    );
    // Typed as one paste, then submitted; recorded with who asked.
    assert_eq!(w.submitted, ["fix the bug\nand say so"]);
    assert_eq!(
        w.prompted(),
        [("fix the bug\nand say so".to_owned(), "cli".to_owned())]
    );
    // The reply was read from the transcript the hooks named, from when the prompt was recorded on.
    let asked = w.a.transcripts().asked();
    let read = asked.last().unwrap();
    assert_eq!(read.transcript_path, transcript_of(&id));
    assert_eq!(read.agent_home, PathBuf::from(CLAUDE_HOME));
    assert_eq!(
        read.since.map(|m| m.at),
        Some(w.prompted_at(&id)),
        "the turn's own start"
    );

    // And `shepherd reply` prints it again, without prompting.
    let mut out = Vec::new();
    shepherd::cli::prompt::reply(&mut w, "worker", false, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "Done: fix the bug\nand say so\n"
    );
    assert_eq!(
        reply_json(&mut w, "worker"),
        json!({ "session": id, "status": "idle", "reply": "Done: fix the bug\nand say so",
                "asked": null })
    );
    assert_eq!(w.submitted.len(), 1);
}

#[test]
fn a_turn_that_ends_without_a_reply_does_not_answer_with_the_turn_before() {
    let mut w = World::new();
    let id = w.agent_session("worker");
    let first = w
        .prompt_json(Prompt {
            wait: true,
            ..prompt_of("worker", "one")
        })
        .unwrap();
    assert_eq!(first["reply"], "Done: one", "{first}");
    w.answers.push_back(Answer::Quiet);
    let second = w
        .prompt_json(Prompt {
            wait: true,
            ..prompt_of("worker", "two")
        })
        .unwrap();
    assert_eq!(
        second,
        json!({ "session": id, "status": "idle", "reply": null, "asked": null })
    );
    // Without a turn, `reply` is the last one the agent wrote.
    assert_eq!(reply_json(&mut w, "worker")["reply"], "Done: one");
}

#[test]
fn a_second_prompt_while_the_agent_works_is_refused_as_busy_and_never_typed() {
    let mut w = World::new();
    w.agent_session("worker");
    w.answers.push_back(Answer::Silent);
    w.prompt_json(prompt_of("worker", "one")).unwrap();
    let err = w.prompt_json(prompt_of("worker", "two")).unwrap_err();
    assert!(
        matches!(&err, CliError::Failed(why) if why.contains("busy (running)")),
        "{err:?}"
    );
    assert_eq!(w.submitted, ["one"]);
    assert_eq!(w.prompted().len(), 1);
}

#[test]
fn a_queued_prompt_waits_for_the_turn_before_it() {
    let mut w = World::new();
    w.agent_session("worker");
    w.answers.push_back(Answer::Slow(5));
    w.prompt_json(prompt_of("worker", "first")).unwrap();
    let answer = w
        .prompt_json(Prompt {
            wait: true,
            queue: true,
            ..prompt_of("worker", "second")
        })
        .unwrap();
    assert_eq!(answer["reply"], "Done: second", "{answer}");
    assert_eq!(w.submitted, ["first", "second"]);
}

#[test]
fn a_queued_prompt_that_loses_the_race_waits_for_the_winners_turn_and_answers_with_its_own() {
    let mut w = World::new();
    let id = w.agent_session("worker");
    // Another client's prompt reaches the host first, before this client hears of it: the agent looks idle here.
    let mut rival = w.a.connect();
    rival.request(ClientRequest::SessionPrompt(SessionPrompt {
        session: SessionId::new(&id),
        text: "theirs".into(),
        by: None,
    }));
    w.answers.push_back(Answer::Slow(3));
    let answer = w
        .prompt_json(Prompt {
            wait: true,
            queue: true,
            ..prompt_of("worker", "mine")
        })
        .unwrap();
    assert_eq!(answer["reply"], "Done: mine", "{answer}");
    assert_eq!(w.submitted, ["theirs", "mine"]);
    assert_eq!(
        w.prompted(),
        [
            ("theirs".to_owned(), "client".to_owned()),
            ("mine".to_owned(), "cli".to_owned())
        ]
    );
}

#[test]
fn an_agent_that_asks_the_user_returns_at_once_saying_what_it_asked() {
    let mut w = World::new();
    let id = w.agent_session("worker");
    w.answers.push_back(Answer::Ask("Bash"));
    let answer = w
        .prompt_json(Prompt {
            wait: true,
            ..prompt_of("worker", "delete the logs")
        })
        .unwrap();
    assert_eq!(
        answer,
        json!({ "session": id, "status": "wants-user", "reply": null, "asked": "Bash" })
    );

    // `shepherd wait` says the same at once.
    let mut out = Vec::new();
    shepherd::cli::prompt::wait(&mut w, "worker", Some(LIVENESS), false, &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "wants-user: Bash\n");
    assert_eq!(
        wait_json(&mut w, "worker", LIVENESS).unwrap(),
        json!({ "session": id, "status": "wants-user", "reply": null, "asked": "Bash" })
    );
}

#[test]
fn a_session_closed_while_its_turn_is_waited_for_ends_the_wait_as_exited() {
    let mut w = World::new();
    let id = w.agent_session("worker");
    w.answers.push_back(Answer::Closed);
    let answer = w
        .prompt_json(Prompt {
            wait: true,
            ..prompt_of("worker", "go")
        })
        .unwrap();
    assert_eq!(
        answer,
        json!({ "session": id, "status": "exited", "reply": null, "asked": null })
    );
}

#[test]
fn an_agent_that_exits_during_its_turn_ends_the_wait_as_exited() {
    let mut w = World::new();
    let id = w.agent_session("worker");
    w.answers.push_back(Answer::Exits);
    let answer = w
        .prompt_json(Prompt {
            wait: true,
            ..prompt_of("worker", "go")
        })
        .unwrap();
    assert_eq!(
        answer,
        json!({ "session": id, "status": "exited", "reply": null, "asked": null })
    );
}

#[test]
fn a_wait_that_runs_out_says_it_timed_out_and_the_prompt_stays() {
    let mut w = World::new();
    w.agent_session("worker");
    w.answers.push_back(Answer::Silent);
    let err = w
        .prompt_json(Prompt {
            wait: true,
            timeout: Some(Duration::from_secs(1)),
            ..prompt_of("worker", "think forever")
        })
        .unwrap_err();
    assert!(
        matches!(&err, CliError::TimedOut(why) if why.contains("the prompt was sent and stays")),
        "{err:?}"
    );
    assert_eq!(err.exit_code(), shepherd::cli::TIMED_OUT_EXIT);
    assert_eq!(w.submitted, ["think forever"]);
    // Still working on it: nothing was withdrawn, and a queued prompt times out waiting its turn.
    let err = w
        .prompt_json(Prompt {
            queue: true,
            timeout: Some(Duration::from_secs(1)),
            ..prompt_of("worker", "next")
        })
        .unwrap_err();
    assert!(matches!(err, CliError::TimedOut(_)), "{err:?}");
    let err = wait_json(&mut w, "worker", Duration::from_secs(1)).unwrap_err();
    assert!(matches!(err, CliError::TimedOut(_)), "{err:?}");
    assert_eq!(w.submitted, ["think forever"]);
}

#[test]
fn a_session_with_no_agent_is_never_prompted() {
    let mut w = World::new();
    session_on(&mut w.a, "plain");
    let err = w.prompt_json(prompt_of("plain", "ls")).unwrap_err();
    assert!(
        matches!(&err, CliError::Failed(why) if why.contains("runs no agent")),
        "{err:?}"
    );
    assert!(w.prompted().is_empty());
}

// ---- shepherd ui (T50) ----

impl World {
    /// What `shepherd ui <op> --json` prints, or why it failed.
    fn ui_json(&mut self, op: Ui) -> Result<Value, String> {
        let mut out = Vec::new();
        ui(self, &op, true, &mut out)?;
        Ok(serde_json::from_slice(&out).unwrap())
    }

    /// What `shepherd ui screenshot` prints.
    fn screenshot(&mut self, shot: &Screenshot) -> String {
        let mut out = Vec::new();
        screenshot(self, shot, false, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Starts a session called `name` on A, running in a directory of its own (no worktree); its id.
    fn running_on_a(&mut self, name: &str) -> SessionId {
        let made = self
            .new_json(New {
                cwd: Some(self.here().to_str().unwrap().to_owned()),
                name: Some(name.into()),
                ..New::default()
            })
            .unwrap();
        SessionId::new(made["session"]["id"].as_str().unwrap())
    }
}

/// What pane `pane` of machine A's host's layout shows.
fn shown(w: &World, pane: u64) -> Content {
    match w.a.host().layout().pane(PaneId::new(pane)) {
        Some(Pane::Leaf { content, .. }) => content.clone(),
        other => panic!("pane {pane} is {other:?}"),
    }
}

#[test]
fn ui_put_pins_a_peers_session_with_its_machine_and_this_machines_without() {
    let mut w = World::new();
    let mine = session_on(&mut w.a, "mine");
    let theirs = session_on(&mut w.b, "theirs");

    let put = w
        .ui_json(Ui::Put {
            pane: PaneId::new(2),
            what: "theirs".into(),
        })
        .unwrap();
    let content = &put["layout"]["pages"][0]["root"]["content"];
    assert_eq!(content["id"], theirs.as_str());
    assert_eq!(content["name"], "theirs");
    assert_eq!(content["machine"], "laptop");
    assert_eq!(content["machine_id"], w.b.machine().as_str());
    assert_eq!(shown(&w, 2), Content::peer_session(w.b.machine(), theirs));

    let split = w
        .ui_json(Ui::Split {
            pane: PaneId::new(2),
            dir: SplitDir::Vertical,
            ratio: None,
            session: Some("mine".into()),
        })
        .unwrap();
    let b = split["layout"]["pages"][0]["root"]["b"]["id"]
        .as_u64()
        .unwrap();
    assert_eq!(shown(&w, b), Content::session(mine));
    assert_eq!(
        split["layout"]["pages"][0]["root"]["b"]["content"]["machine"],
        "testbox"
    );
}

#[test]
fn ui_screenshot_draws_each_panes_session_a_peers_too_and_resizes_none() {
    let mut w = World::new();
    let theirs = session_on(&mut w.b, "theirs");
    let mine = w.running_on_a("mine");
    w.ui_json(Ui::Split {
        pane: PaneId::new(2),
        dir: SplitDir::Horizontal,
        ratio: None,
        session: Some("theirs".into()),
    })
    .unwrap();
    let sizes = |w: &World| {
        (
            w.a.runtime().pty().size(&mine),
            w.b.runtime().pty().size(&theirs),
        )
    };
    let before = sizes(&w);

    let shot = w.screenshot(&Screenshot {
        cols: 100,
        rows: 12,
        page: None,
    });
    let lines: Vec<&str> = shot.lines().collect();
    assert_eq!(lines.len(), 12, "{shot}");
    // The selection's pane shows the first session listed (this machine's), the other the peer's.
    assert!(shot.contains("on testbox first line"), "{shot}");
    assert!(shot.contains("on laptop first line"), "{shot}");
    // What the session drew is shown display-safe: its right-to-left override is dropped.
    assert!(
        shot.contains("first line!") && !shot.contains('\u{202e}'),
        "{shot}"
    );
    // A line wider than its pane is cut, not rewrapped.
    assert!(shot.contains("this line is long"), "{shot}");
    assert!(!shot.contains("without stopping"), "{shot}");
    assert_eq!(sizes(&w), before);
}

#[test]
fn ui_screenshot_of_another_page_leaves_the_hosts_shown_page_alone() {
    let mut w = World::new();
    session_on(&mut w.b, "theirs");
    w.running_on_a("mine");
    let made = w
        .ui_json(Ui::PageNew {
            name: Some("remote".into()),
        })
        .unwrap();
    let page = made["layout"]["current"].as_u64().unwrap();
    let pane = made["layout"]["pages"][1]["root"]["id"].as_u64().unwrap();
    w.ui_json(Ui::Put {
        pane: PaneId::new(pane),
        what: "theirs".into(),
    })
    .unwrap();
    w.ui_json(Ui::PageSelect { page: "1".into() }).unwrap();

    let shot = w.screenshot(&Screenshot {
        cols: 100,
        rows: 12,
        page: Some(page.to_string()),
    });
    // Page 1 shows the selection, this machine's session; the page drawn, the peer's alone.
    assert!(shot.contains("on laptop first line"), "{shot}");
    assert!(!shot.contains("on testbox first line"), "{shot}");
    assert_eq!(w.a.host().layout().current().get(), 1);
    assert_eq!(w.client.client().host_layout().current().get(), 1);
}

#[test]
fn ui_screenshot_cuts_a_terminal_bigger_than_its_pane_keeping_the_cursor_row() {
    let mut w = World::new();
    let theirs = session_on(&mut w.b, "theirs");
    // A window on B made its terminal bigger than the 80x24 a session starts at.
    let mut window = w.b.connect();
    window.resize(&theirs, 150, 40);
    w.b.step();
    window.close();
    w.b.step();
    w.ui_json(Ui::Put {
        pane: PaneId::new(2),
        what: "theirs".into(),
    })
    .unwrap();

    let shot = w.screenshot(&Screenshot {
        cols: 100,
        rows: 12,
        page: None,
    });
    let lines: Vec<&str> = shot.lines().collect();
    // The pane (76x12, right of the sidebar) shows the terminal's last 12 rows, down to the cursor's: rows 29 to 39
    // and the wide line, each where the session drew it.
    assert!(lines[0].contains("on laptop row 29"), "{shot}");
    assert!(lines[10].contains("on laptop row 39"), "{shot}");
    assert!(lines[11].contains("wide xxxx"), "{shot}");
    assert!(!shot.contains("row 28"), "{shot}");
    // Cut at the pane's edge, not wrapped at 80 columns or the pane's width.
    assert!(!shot.contains("edge"), "{shot}");
    assert_eq!(lines.len(), 12, "{shot}");
}

/// What `shepherd screen` prints (`--json` with `json`), or why it failed.
fn screen_out(
    w: &mut World,
    session: &str,
    lines: Option<u32>,
    json: bool,
) -> Result<String, CliError> {
    let mut out = Vec::new();
    shepherd::cli::screen::screen(w, session, lines, json, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

fn screen_json(w: &mut World, session: &str, lines: Option<u32>) -> Value {
    serde_json::from_str(&screen_out(w, session, lines, true).unwrap()).unwrap()
}

/// A shell session named `name` on A, running in `app` (so at once, with no worktree to wait for), with what it
/// printed when it started reset away (scrollback too), so its screen starts blank.
fn shell_session(w: &mut World, name: &str) -> SessionId {
    let made = w
        .new_json(New {
            cwd: Some("app".into()),
            name: Some(name.into()),
            ..New::default()
        })
        .unwrap();
    let id = SessionId::new(made["session"]["id"].as_str().unwrap());
    type_into(w, &id, b"\x1bc");
    id
}

/// Types `bytes` into session `id` on A, whose echoing shell prints them back.
fn type_into(w: &mut World, id: &SessionId, bytes: &[u8]) {
    w.hooks.input(id, bytes);
    w.a.step();
}

#[test]
fn screen_shows_what_a_session_printed_as_text_without_its_control_sequences() {
    let mut w = World::new();
    let id = shell_session(&mut w, "plain");
    type_into(
        &mut w,
        &id,
        "\x1b[1;31mred\x1b[0m 漢字 done\r\n$ ".as_bytes(),
    );
    let shown = screen_json(&mut w, "plain", None);
    assert_eq!(shown["session"], id.as_str());
    assert_eq!(
        (shown["cols"].as_u64(), shown["rows"].as_u64()),
        (Some(80), Some(24))
    );
    assert_eq!(shown["scrollback"], 0);
    assert_eq!(shown["truncated"], false);
    let lines = shown["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 24);
    assert_eq!(lines[0], "red 漢字 done");
    assert_eq!(lines[1], "$");
    // For a human: the rows, without the blank ones below the last drawn.
    assert_eq!(
        screen_out(&mut w, "plain", None, false).unwrap(),
        "red 漢字 done\n$\n"
    );
}

#[test]
fn screen_with_lines_adds_that_much_scrollback_above_the_screen() {
    let mut w = World::new();
    let id = shell_session(&mut w, "plain");
    let printed: String = (1..=30).map(|n| format!("line {n}\r\n")).collect();
    type_into(&mut w, &id, printed.as_bytes());
    // 31 lines (the last one empty) on 24 rows: lines 1 to 7 scrolled off.
    let shown = screen_json(&mut w, "plain", Some(5));
    assert_eq!(shown["scrollback"], 5);
    let lines = shown["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 29);
    assert_eq!(lines[0], "line 3");
    assert_eq!(lines[5], "line 8");
    let shown = screen_json(&mut w, "plain", Some(100));
    assert_eq!(shown["scrollback"], 7);
    assert_eq!(shown["lines"][0], "line 1");
}

#[test]
fn screen_reads_a_peers_session_from_its_host() {
    let mut w = World::new();
    session_on(&mut w.b, "remote");
    let shown = screen_json(&mut w, "remote", None);
    assert_eq!(shown["rows"], 24);
    assert_eq!(shown["lines"].as_array().unwrap().len(), 24);
}

#[test]
fn screen_of_a_session_that_exited_is_refused_saying_so() {
    let mut w = World::new();
    let id = shell_session(&mut w, "plain");
    assert_eq!(screen_json(&mut w, "plain", None)["rows"], 24);
    type_into(&mut w, &id, b"exit\r");
    let err = screen_out(&mut w, "plain", None, true).unwrap_err();
    assert!(
        matches!(&err, CliError::Failed(why) if why.contains("isn't running")),
        "{err:?}"
    );
}

#[test]
fn ui_screenshot_and_screen_leave_out_concealed_text() {
    let mut w = World::new();
    let id = shell_session(&mut w, "plain");
    type_into(&mut w, &id, b"pw: \x1b[8msecret\x1b[0m ok\r\n");
    // Asking the host waits for its answer, and the output it sent before it reaches the client first.
    let shown = screen_json(&mut w, "plain", None);
    assert_eq!(shown["lines"][0], "pw:        ok");
    let shot = w.screenshot(&Screenshot {
        cols: 100,
        rows: 12,
        page: None,
    });
    assert!(shot.contains("pw:") && shot.contains("ok"), "{shot}");
    assert!(!shot.contains("secret"), "{shot}");
}
