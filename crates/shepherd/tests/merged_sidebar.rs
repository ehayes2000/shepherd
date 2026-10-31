//! One sidebar across machines (T31b), end to end: a client runtime on machine A's host lists the sessions of A and of
//! B, a trusted peer it reaches through a tunnel on A's host, drives B's session on B's host, keeps B's sessions while
//! B is gone, and picks up where it left off once B is back.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use shepherd::client::{ClientRuntime, Connector};
use shepherd_core::acpp::Timestamp;
use shepherd_core::acpp::{
    Dir, Envelope, FrameDraft, PeerTrusted, ShepherdFrame, Source, SourceKind,
};
use shepherd_core::client::{Intent, MachineStatus, PEER_RETRY, SidebarEntry};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::LogId;
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{ClientRequest, SessionRename};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::launcher::FakeHostLauncher;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use support::*;

const KEY_A: [u8; 32] = [0xaa; 32];
const KEY_B: [u8; 32] = [0xbb; 32];

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

/// Machine A's host trusting B (called "laptop"), B's host letting A in, and a client runtime on A's host, with a
/// clock the test moves.
struct World {
    a: HostHarness,
    b: HostHarness,
    now: Arc<Mutex<jiff::Timestamp>>,
    client: Runtime,
}

impl World {
    fn new() -> Self {
        let machines = FakeNetwork::new();
        let env = TestEnv::new();
        let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
        let a_log = FakeEventLog::new().with_envelopes(LogId::Host, trusting(&b_id, "laptop"));
        let host = |key, log| {
            HostHarness::builder()
                .machines(machines.clone())
                .machine_key(key)
                .env(env.clone())
                .log(log)
                .start()
        };
        let a = host(KEY_A, a_log);
        let b = host(KEY_B, FakeEventLog::new());
        machines.trust(&b.machine(), &a_id);
        let now = Arc::new(Mutex::new(
            "2026-10-29T12:00:00Z"
                .parse::<jiff::Timestamp>()
                .expect("timestamp"),
        ));
        let clock = {
            let now = now.clone();
            move || Timestamp::parse(lock(&now).to_string()).expect("timestamp")
        };
        let net = a.network().clone();
        let connect: Connector<FakeLink> =
            Box::new(move |sink| net.connect(sink).map_err(|e| e.to_string()));
        let client = ClientRuntime::new(
            MachineId::new("m_client"),
            connect,
            FakeHostLauncher::new(),
            clock,
        );
        Self { a, b, now, client }
    }

    /// Runs both hosts and the client until none has anything left to do. A stopped host isn't run.
    fn settle(&mut self, b_running: bool) {
        let mut quiet = 0;
        while quiet < 2 {
            self.a.step();
            if b_running {
                self.b.step();
            }
            quiet = match self.client.pump() {
                0 => quiet + 1,
                _ => 0,
            };
        }
    }

    fn user(&mut self, intent: Intent) {
        self.client.user(intent);
        self.settle(true);
    }

    /// Moves the clock on by `by` and ticks the client.
    fn tick_after(&mut self, by: jiff::SignedDuration, b_running: bool) {
        {
            let mut now = lock(&self.now);
            *now = now.checked_add(by).expect("clock");
        }
        self.client.tick();
        self.settle(b_running);
    }

    /// The sidebar, one line per entry: `# machine` or the session's name.
    fn sidebar(&self) -> Vec<String> {
        self.client
            .client()
            .sidebar()
            .iter()
            .map(|e| match e {
                SidebarEntry::Machine(g) => format!("# {}", g.name),
                SidebarEntry::Session(s) => s.name.clone(),
            })
            .collect()
    }

    fn laptop(&self) -> MachineStatus {
        self.client.client().machines()[1].status.clone()
    }

    fn screen(&self, session: &SessionId) -> String {
        self.client
            .screen(session)
            .and_then(|s| s.row(0).map(|c| c.ch).collect::<String>().into())
            .unwrap_or_default()
            .trim_end()
            .to_owned()
    }
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

#[test]
fn one_sidebar_drives_each_machines_sessions_on_its_own_host_and_survives_a_peer_going_away() {
    let mut w = World::new();
    let mine = session_on(&mut w.a, "mine");
    let theirs = session_on(&mut w.b, "theirs");

    w.client.start();
    w.settle(true);
    w.user(Intent::Resize(PtySize { cols: 80, rows: 24 }));
    assert_eq!(w.sidebar(), ["# testbox", "mine", "# laptop", "theirs"]);
    assert_eq!(w.laptop(), MachineStatus::Connected);

    // B's session is driven on B's host.
    w.user(Intent::SelectSession(theirs.clone()));
    w.user(Intent::keys("hi from A"));
    assert_eq!(w.screen(&theirs), "hi from A");
    w.user(Intent::BeginRename);
    w.user(Intent::EditRename("renamed".into()));
    w.user(Intent::Confirm);
    let names = |h: &HostHarness| -> Vec<String> {
        h.host().sessions().into_iter().map(|s| s.name).collect()
    };
    assert_eq!(names(&w.b), ["renamed"]);
    assert_eq!(names(&w.a), ["mine"], "A's own session is untouched");

    // B goes away: its group stays, with its last-known session, while A's host can't reach it.
    w.b.stop();
    w.settle(false);
    assert_eq!(w.sidebar(), ["# testbox", "mine", "# laptop", "renamed"]);
    assert!(w.client.client().offline(&theirs));
    assert!(
        matches!(w.laptop(), MachineStatus::Offline(Some(ref why)) if why.contains("laptop")),
        "{:?}",
        w.laptop()
    );

    // Back, and renamed meanwhile by someone else: the client resumes and sees it.
    w.b.restart();
    session_rename(&mut w.b, &theirs, "back again");
    w.tick_after(jiff::SignedDuration::ZERO, true);
    w.tick_after(PEER_RETRY, true);
    assert_eq!(w.laptop(), MachineStatus::Connected);
    assert!(!w.client.client().offline(&theirs));
    assert_eq!(w.sidebar(), ["# testbox", "mine", "# laptop", "back again"]);
    let listed = w
        .client
        .client()
        .sessions()
        .iter()
        .filter(|s| s.id == theirs)
        .count();
    assert_eq!(listed, 1);
    assert!(
        w.client.client().sessions().iter().any(|s| s.id == mine),
        "A's session is still listed"
    );
}

fn session_rename(host: &mut HostHarness, session: &SessionId, name: &str) {
    let mut client = host.connect();
    client.request(ClientRequest::SessionRename(SessionRename {
        session: session.clone(),
        name: name.into(),
    }));
    host.step();
    client.close();
    host.step();
}
