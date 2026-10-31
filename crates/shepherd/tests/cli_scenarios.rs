//! `shepherd sessions` and `shepherd new` (T46) across machines, end to end on fakes: the commands drive a real
//! client runtime on machine A's host, which trusts B ("laptop") and reaches it through a tunnel, with both hosts'
//! runtimes on all-fake ports. The built binary's own tests are in `sessions_cli.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::Value;
use shepherd::cli::sessions::{New, new, sessions};
use shepherd::cli::{Drive, target};
use shepherd::client::{ClientError, ClientRuntime, Connector};
use shepherd_core::acpp::Timestamp;
use shepherd_core::acpp::{
    Dir, Envelope, FrameDraft, PeerTrusted, ShepherdFrame, Source, SourceKind,
};
use shepherd_core::client::{AskId, Client, MachineStatus};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{Link, LinkEvent, LinkEventSink, LogId};
use shepherd_core::wire::{
    ClientMessage, ClientRequest, Hello, HostResponse, PROTOCOL_VERSION, SessionRename, WireError,
    decode_client_message, encode_response,
};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::launcher::FakeHostLauncher;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

const KEY_A: [u8; 32] = [0xaa; 32];
const KEY_B: [u8; 32] = [0xbb; 32];
/// A liveness bound, never a measurement.
const LIVENESS: Duration = Duration::from_secs(60);

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

/// Machine A's host trusting B ("laptop"), in workspace `app` (a repo the fake worktrees know); B's host, letting A
/// in unless told not to; and a command-line client on A's host.
struct World {
    _tmp: tempfile::TempDir,
    a: HostHarness,
    b: HostHarness,
    client: Runtime,
    /// B's host is running: a stopped one isn't stepped.
    b_running: bool,
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
        let mut a = HostHarness::builder()
            .machines(machines.clone())
            .machine_key(KEY_A)
            .env(env.clone())
            .log(a_log)
            .worktrees(worktrees)
            .start();
        let b = HostHarness::builder()
            .machines(machines.clone())
            .machine_key(KEY_B)
            .env(env)
            .start();
        if b_lets_a_in {
            machines.trust(&b.machine(), &a_id);
        }
        select_workspace(&mut a, &app);
        let now = Arc::new(Mutex::new(
            "2026-10-29T12:00:00Z".parse::<jiff::Timestamp>().unwrap(),
        ));
        let clock = move || Timestamp::parse(lock(&now).to_string()).unwrap();
        let net = a.network().clone();
        let connect: Connector<FakeLink> = Box::new(move |sink: Arc<dyn LinkEventSink>| {
            net.connect(sink).map_err(|e| e.to_string())
        });
        let mut client = ClientRuntime::new(
            MachineId::new("m_cli"),
            connect,
            FakeHostLauncher::new(),
            clock,
        );
        client.start();
        Self {
            _tmp: tmp,
            a,
            b,
            client,
            b_running: true,
        }
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

    fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId {
        self.client.ask(machine, request)
    }

    /// Runs both hosts and the client until `done`; fails once none of them has anything left to do.
    fn wait(&mut self, done: &mut dyn FnMut(&Client) -> bool, _: Duration) -> Result<(), String> {
        let mut quiet = 0;
        while quiet < 3 {
            if done(self.client.client()) {
                return Ok(());
            }
            self.a.step();
            if self.b_running {
                self.b.step();
            }
            quiet = match self.client.pump() {
                0 => quiet + 1,
                _ => 0,
            };
        }
        Err("nothing more happened".to_owned())
    }
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
