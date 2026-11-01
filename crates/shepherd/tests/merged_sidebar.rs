//! One sidebar across machines (T31b), end to end: a client runtime on machine A's host lists the sessions of A and of
//! B, a trusted peer it reaches through a tunnel on A's host, drives B's session on B's host, keeps B's sessions while
//! B is gone, and picks up where it left off once B is back; and a session made on B from the new-session popup (T32)
//! runs on B's host and takes the keys. The client's conns are tapped: what it sends on each is
//! recorded, and a test can hand the runtime an event as if it came on one of them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use shepherd::client::{ClientRuntime, Connector};
use shepherd_core::acpp::{
    Dir, Envelope, FrameDraft, PeerTrusted, Seq, ShepherdFrame, Source, SourceKind, Timestamp,
};
use shepherd_core::client::{Intent, MachineStatus, PEER_RETRY, SidebarEntry};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::pty::PtySize;
use shepherd_core::ports::{
    ConnId, DisconnectReason, EventLog, FlushOutcome, Link, LinkError, LinkEvent, LinkEventSink,
    LogId,
};
use shepherd_core::wire::{
    ClientMessage, ClientRequest, HostNotification, LogEntry, PtyOutput, SessionRename,
    SubscribeFrom, decode_client_message, encode_host_notification,
};
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

/// One conn the client opened: what it sent, and the sink its events reach the runtime through.
struct Tap {
    conn: ConnId,
    sent: Vec<ClientMessage>,
    sink: Arc<dyn LinkEventSink>,
}

/// Every conn the client opened, in order.
#[derive(Clone, Default)]
struct Wire(Arc<Mutex<Vec<Tap>>>);

impl Wire {
    /// The conns whose first request asked for a tunnel to `machine`, oldest first.
    fn tunnels(&self, machine: &MachineId) -> Vec<usize> {
        let taps = lock(&self.0);
        (0..taps.len())
            .filter(|&i| {
                matches!(
                    taps[i].sent.first(),
                    Some(ClientMessage::Request(r))
                        if matches!(&r.body, ClientRequest::Tunnel(t) if &t.machine == machine)
                )
            })
            .collect()
    }

    /// Where each subscribe sent on conn `tap` resumed from.
    fn subscribes(&self, tap: usize) -> Vec<SubscribeFrom> {
        lock(&self.0)[tap]
            .sent
            .iter()
            .filter_map(|m| match m {
                ClientMessage::Request(r) => match &r.body {
                    ClientRequest::Subscribe(s) => Some(s.from.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// Hands the runtime `event` as if conn `tap`'s link had sent it.
    fn inject(&self, tap: usize, event: impl FnOnce(ConnId) -> LinkEvent) {
        let (sink, conn) = {
            let taps = lock(&self.0);
            (taps[tap].sink.clone(), taps[tap].conn)
        };
        sink.send(event(conn));
    }

    /// Hands the runtime `note` as a line on conn `tap`.
    fn inject_line(&self, tap: usize, note: &HostNotification) {
        let line = encode_host_notification(note);
        let line = line.strip_suffix(b"\n").unwrap_or(&line).to_vec();
        self.inject(tap, |conn| LinkEvent::Line { conn, line });
    }
}

/// A [`FakeLink`] that records what is sent on it.
struct TapLink {
    inner: FakeLink,
    tap: usize,
    wire: Wire,
}

impl Link for TapLink {
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError> {
        if let Ok(message) = decode_client_message(line) {
            lock(&self.wire.0)[self.tap].sent.push(message);
        }
        self.inner.send(conn, line)
    }
    fn close(&mut self, conn: ConnId) -> Result<(), LinkError> {
        self.inner.close(conn)
    }
    fn queued_bytes(&self, conn: ConnId) -> Option<usize> {
        self.inner.queued_bytes(conn)
    }
    fn flush_closed(&mut self, within: Duration) -> FlushOutcome {
        self.inner.flush_closed(within)
    }
    fn peer(&self, conn: ConnId) -> Result<Option<MachineId>, LinkError> {
        self.inner.peer(conn)
    }
    fn drained_bytes(&self, conn: ConnId) -> Option<u64> {
        self.inner.drained_bytes(conn)
    }
    fn pause_reading(&mut self, conn: ConnId, paused: bool) -> Result<(), LinkError> {
        self.inner.pause_reading(conn, paused)
    }
    fn open(&mut self, peer: &MachineId) -> Result<ConnId, LinkError> {
        self.inner.open(peer)
    }
}

type Runtime = ClientRuntime<TapLink, Connector<TapLink>, FakeHostLauncher>;

/// Machine A's host trusting B (called "laptop"), B's host letting A in, and a client runtime on A's host, with a
/// clock the test moves.
struct World {
    a: HostHarness,
    b: HostHarness,
    now: Arc<Mutex<jiff::Timestamp>>,
    wire: Wire,
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
        let wire = Wire::default();
        let taps = wire.clone();
        let connect: Connector<TapLink> = Box::new(move |sink: Arc<dyn LinkEventSink>| {
            let (inner, conn) = net.connect(sink.clone()).map_err(|e| e.to_string())?;
            let tap = {
                let mut all = lock(&taps.0);
                all.push(Tap {
                    conn,
                    sent: Vec::new(),
                    sink,
                });
                all.len() - 1
            };
            let link = TapLink {
                inner,
                tap,
                wire: taps.clone(),
            };
            Ok((link, conn))
        });
        let client = ClientRuntime::new(
            MachineId::new("m_client"),
            connect,
            FakeHostLauncher::new(),
            clock,
        );
        Self {
            a,
            b,
            now,
            wire,
            client,
        }
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
                SidebarEntry::Subagent { subagent, .. } => format!("  {}", subagent.agent_type()),
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

/// Where a client that has folded all of `log` resumes each of its logs.
fn resume_point(log: &FakeEventLog) -> SubscribeFrom {
    let mut from = SubscribeFrom::default();
    for id in log.logs().expect("logs") {
        let last = log
            .read(id.clone(), Seq::ZERO)
            .expect("read")
            .iter()
            .map(Envelope::seq)
            .max();
        match (id, last) {
            (LogId::Host, last) => from.host = last,
            (LogId::Session(session), Some(last)) => {
                from.sessions.insert(session, last);
            }
            (LogId::Session(_), None) => {}
        }
    }
    from
}

/// The entry of `session`'s log in `log` that renamed it to `name`.
fn rename_entry(log: &FakeEventLog, session: &SessionId, name: &str) -> Envelope {
    let quoted = format!("\"{name}\"");
    log.read(LogId::Session(session.clone()), Seq::ZERO)
        .expect("read")
        .into_iter()
        .rev()
        .find(|e| serde_json::to_string(e).expect("json").contains(&quoted))
        .unwrap_or_else(|| panic!("no rename to {name}"))
}

fn output(session: &SessionId, text: &str) -> HostNotification {
    HostNotification::PtyOutput(PtyOutput {
        session: session.clone(),
        bytes: text.as_bytes().to_vec(),
    })
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
    let left_at = resume_point(w.b.log());
    let old_rename = rename_entry(w.b.log(), &theirs, "renamed");
    w.b.stop();
    w.settle(false);
    assert_eq!(w.sidebar(), ["# testbox", "mine", "# laptop", "renamed"]);
    assert!(w.client.client().offline(&theirs));
    assert!(
        matches!(w.laptop(), MachineStatus::Offline(Some(ref why)) if why.contains("laptop")),
        "{:?}",
        w.laptop()
    );

    // Back, and renamed meanwhile by someone else: the client resumes from where it left B's logs, and sees it.
    w.b.restart();
    session_rename(&mut w.b, &theirs, "back again");
    w.tick_after(jiff::SignedDuration::ZERO, true);
    w.tick_after(PEER_RETRY, true);
    assert_eq!(w.laptop(), MachineStatus::Connected);
    assert!(!w.client.client().offline(&theirs));
    assert_eq!(w.sidebar(), ["# testbox", "mine", "# laptop", "back again"]);
    let tunnels = w.wire.tunnels(&w.b.machine());
    let (first, now) = (tunnels[0], tunnels[tunnels.len() - 1]);
    assert_ne!(first, now);
    assert_eq!(w.wire.subscribes(first), [SubscribeFrom::default()]);
    assert_eq!(
        w.wire.subscribes(now),
        [left_at],
        "resumed after the last entry folded"
    );
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

    // An older entry B sends again is not folded again: it would undo the newer name.
    w.wire.inject_line(
        now,
        &HostNotification::LogEntry(Box::new(LogEntry {
            envelope: old_rename,
        })),
    );
    w.settle(true);
    assert_eq!(w.sidebar(), ["# testbox", "mine", "# laptop", "back again"]);

    // Late events from B's first conn, already replaced, change nothing.
    w.wire.inject_line(first, &output(&theirs, "ghost"));
    w.wire.inject(first, |conn| LinkEvent::Disconnected {
        conn,
        reason: DisconnectReason::PeerGone,
    });
    w.settle(true);
    assert!(
        !w.screen(&theirs).contains("ghost"),
        "{}",
        w.screen(&theirs)
    );
    assert_eq!(w.laptop(), MachineStatus::Connected);
    assert_eq!(w.wire.tunnels(&w.b.machine()), tunnels, "not dialled again");

    // B's host can't draw into a session that is A's.
    w.user(Intent::SelectSession(mine.clone()));
    w.user(Intent::keys("local"));
    assert_eq!(w.screen(&mine), "local");
    w.wire.inject_line(now, &output(&mine, " intruder"));
    w.settle(true);
    assert_eq!(w.screen(&mine), "local");
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

#[test]
fn a_session_made_on_a_peer_from_the_new_session_popup_runs_there_and_takes_the_keys() {
    let mut w = World::new();
    let mine = session_on(&mut w.a, "mine");
    w.client.start();
    w.settle(true);
    w.user(Intent::Resize(PtySize { cols: 80, rows: 24 }));
    w.user(Intent::SelectSession(mine.clone()));

    // The popup opens on the selected session's machine; the laptop is chosen instead.
    w.user(Intent::BeginNewSession);
    assert_eq!(w.client.client().popup_machine(), Some(&w.a.machine()));
    w.user(Intent::PickMachine(w.b.machine()));
    w.user(Intent::Confirm);

    let made: Vec<SessionId> = w.b.host().sessions().into_iter().map(|s| s.id).collect();
    let [theirs] = &made[..] else {
        panic!("expected one session on B, got {made:?}");
    };
    assert_eq!(w.a.host().sessions().len(), 1, "none made on A");
    assert_eq!(w.client.client().selected(), Some(theirs));
    assert_eq!(w.sidebar()[..3], ["# testbox", "mine", "# laptop"]);

    w.user(Intent::keys("hello from A"));
    assert_eq!(w.screen(theirs), "hello from A");
    assert_eq!(w.screen(&mine), "");
}
