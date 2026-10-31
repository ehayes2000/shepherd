//! One client, every machine (T31b): the local host reports its trusted peers, the client reaches each one's host
//! through a tunnel on its own host, merges their sessions grouped by machine, and sends each session's requests to
//! its own host.

use serde_json::{Value, json};
use shepherd_core::acpp::{Seq, Timestamp};
use shepherd_core::client::{
    ClientCommand, ClientEvent, Intent, MachineStatus, NOTICE_TIMEOUT, PEER_RETRY, PEER_RETRY_MAX,
    PeerEvent, SidebarEntry,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{
    ClientMessage, ClientNotification, ClientRequest, InputKind, PROTOCOL_VERSION, RawResponse,
    Request, SessionClose, SessionInput, SessionRename, SessionResize, Subscribe, SubscribeFrom,
    Tunnel, WireError, WorktreeCheck,
};
use shepherd_core::wire::{HostMessage, HostNotification, LogEntry};
use shepherd_testkit::builders::{ClientDriver, HostDriver, requests, ts};

fn m(id: &str) -> MachineId {
    MachineId::new(id)
}

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn at(ms: i64) -> Timestamp {
    let t = ts("2026-10-29T12:00:00.000Z").instant() + jiff::SignedDuration::from_millis(ms);
    Timestamp::parse(t.to_string()).unwrap_or_else(|e| panic!("{e}"))
}

fn peer(machine: &str, event: PeerEvent) -> ClientEvent {
    ClientEvent::Peer {
        machine: m(machine),
        event,
    }
}

/// The requests among `commands` sent to peer `machine`'s host.
fn peer_requests<'a>(commands: &'a [ClientCommand], machine: &str) -> Vec<&'a Request> {
    commands
        .iter()
        .filter_map(|c| match c {
            ClientCommand::SendPeer {
                machine: to,
                message: ClientMessage::Request(r),
            } if to.as_str() == machine => Some(r),
            _ => None,
        })
        .collect()
}

/// The notifications among `commands` sent to peer `machine`'s host.
fn peer_notifications<'a>(
    commands: &'a [ClientCommand],
    machine: &str,
) -> Vec<&'a ClientNotification> {
    commands
        .iter()
        .filter_map(|c| match c {
            ClientCommand::SendPeer {
                machine: to,
                message: ClientMessage::Notification(n),
            } if to.as_str() == machine => Some(n),
            _ => None,
        })
        .collect()
}

/// Whether anything among `commands` goes to the local host.
fn sends_local(commands: &[ClientCommand]) -> bool {
    commands.iter().any(|c| matches!(c, ClientCommand::Send(_)))
}

/// A client whose local host (`m_test`) and peers each run a [`HostDriver`].
struct Machines {
    c: ClientDriver,
    peers: Vec<(String, HostDriver, usize)>,
}

impl Machines {
    /// A client connected to its local host, caught up, with the local host trusting `peers` (id, name), none of them
    /// reached yet. Returns it and what the peer list answer produced.
    fn new(peers: &[(&str, &str)]) -> (Self, Vec<ClientCommand>) {
        let mut c = ClientDriver::new();
        let subscribe = c.connect_named("studio");
        let caught_up = c.answer(&subscribe, Ok(json!({})));
        let list: Vec<Value> = peers
            .iter()
            .map(|(id, name)| json!({ "id": id, "name": name, "connected": true }))
            .collect();
        let dialled = c.answer(&caught_up, Ok(json!({ "peers": list })));
        let peers = peers
            .iter()
            .map(|(id, _)| {
                let mut host = HostDriver::new();
                host.stamp_machine(id);
                ((*id).to_owned(), host, 0)
            })
            .collect();
        (Self { c, peers }, dialled)
    }

    /// [`Machines::new`], with every peer reached: tunnelled, hello answered, caught up.
    fn connected(peers: &[(&str, &str)]) -> Self {
        let (mut net, _) = Self::new(peers);
        for (id, _) in peers {
            net.reach(id);
        }
        net
    }

    fn host(&mut self, machine: &str) -> &mut HostDriver {
        &mut self
            .peers
            .iter_mut()
            .find(|(id, ..)| id == machine)
            .unwrap_or_else(|| panic!("no peer {machine}"))
            .1
    }

    fn handle(&mut self, event: ClientEvent) -> Vec<ClientCommand> {
        self.c.handle(event)
    }

    /// Answers the one request to `machine` among `commands` with `result`.
    fn answer(
        &mut self,
        machine: &str,
        commands: &[ClientCommand],
        result: Result<Value, WireError>,
    ) -> Vec<ClientCommand> {
        let [request] = peer_requests(commands, machine)[..] else {
            panic!("expected one request to {machine} in {commands:?}");
        };
        let id = request.id.clone();
        self.handle(peer(
            machine,
            PeerEvent::Host(HostMessage::Response(RawResponse { id, result })),
        ))
    }

    /// Opens `machine`'s conn, its tunnel and hello. Returns the subscribe commands.
    fn handshake(&mut self, machine: &str) -> Vec<ClientCommand> {
        let tunnel = self.handle(peer(machine, PeerEvent::Connected { at: at(0) }));
        let hello = self.answer(machine, &tunnel, Ok(json!({})));
        let answer = json!({ "version": PROTOCOL_VERSION, "machine": machine });
        self.answer(machine, &hello, Ok(answer))
    }

    /// Reaches `machine`: handshake, its whole log, and the subscribe answered.
    fn reach(&mut self, machine: &str) {
        let subscribe = self.handshake(machine);
        self.sync(machine);
        self.answer(machine, &subscribe, Ok(json!({})));
    }

    /// Sends the client every entry of `machine`'s log it hasn't been sent.
    fn sync(&mut self, machine: &str) -> Vec<ClientCommand> {
        let (_, host, delivered) = self
            .peers
            .iter_mut()
            .find(|(id, ..)| id == machine)
            .unwrap_or_else(|| panic!("no peer {machine}"));
        let fresh = host.log()[*delivered..].to_vec();
        *delivered = host.log().len();
        let mut commands = Vec::new();
        for envelope in fresh {
            let entry = HostNotification::LogEntry(Box::new(LogEntry { envelope }));
            commands.extend(self.c.handle(peer(
                machine,
                PeerEvent::Host(HostMessage::Notification(entry)),
            )));
        }
        commands
    }

    fn names(&self) -> Vec<String> {
        self.c
            .client()
            .sessions()
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// The sidebar, one line per entry: `# machine (status)` or the session's name.
    fn sidebar(&self) -> Vec<String> {
        self.c
            .client()
            .sidebar()
            .iter()
            .map(|e| match e {
                SidebarEntry::Machine(g) => format!("# {} {:?}", g.name, g.status),
                SidebarEntry::Session(s) => s.name.clone(),
            })
            .collect()
    }

    fn user(&mut self, intent: Intent) -> Vec<ClientCommand> {
        self.c.user(intent)
    }
}

// ---------- finding and reaching the peers ----------

#[test]
fn once_caught_up_the_client_asks_its_host_for_the_trusted_peers() {
    let mut c = ClientDriver::new();
    let subscribe = c.connect();
    let caught_up = c.answer(&subscribe, Ok(json!({})));
    let asked: Vec<_> = requests(&caught_up)
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert!(
        matches!(&asked[..], [ClientRequest::PeerList(_)]),
        "{asked:?}"
    );
}

#[test]
fn each_trusted_peer_gets_a_conn_of_its_own() {
    let (_, dialled) = Machines::new(&[("m_b", "laptop"), ("m_c", "tower")]);
    assert_eq!(
        dialled,
        [
            ClientCommand::ConnectPeer(m("m_b")),
            ClientCommand::ConnectPeer(m("m_c")),
        ]
    );
}

#[test]
fn a_host_older_than_peer_lists_means_no_peers_and_no_restart_offer() {
    let mut c = ClientDriver::new();
    let subscribe = c.connect();
    let caught_up = c.answer(&subscribe, Ok(json!({})));
    let after = c.answer(
        &caught_up,
        Err(WireError::MethodNotFound("_shepherd/peer/list".into())),
    );
    assert_eq!(after, []);
    assert_eq!(c.client().restart_offer(), None);
    assert_eq!(c.client().notice(), None);
}

#[test]
fn a_peers_conn_asks_for_a_tunnel_first_then_says_hello_without_the_local_cwd() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    let tunnel = net.handle(peer("m_b", PeerEvent::Connected { at: at(0) }));
    let bodies: Vec<_> = peer_requests(&tunnel, "m_b")
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(
        bodies,
        [ClientRequest::Tunnel(Tunnel { machine: m("m_b") })]
    );
    let hello = net.answer("m_b", &tunnel, Ok(json!({})));
    match &peer_requests(&hello, "m_b")[..] {
        [
            Request {
                body: ClientRequest::Hello(h),
                ..
            },
        ] => assert_eq!(h.cwd, None),
        other => panic!("expected a hello, got {other:?}"),
    }
    assert!(!sends_local(&hello));
}

#[test]
fn a_peer_that_answers_hello_is_subscribed_to_from_the_start() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    let subscribe = net.handshake("m_b");
    let bodies: Vec<_> = peer_requests(&subscribe, "m_b")
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(
        bodies,
        [ClientRequest::Subscribe(Subscribe {
            from: SubscribeFrom::default()
        })]
    );
    assert_eq!(
        net.c.client().machines()[1].status,
        MachineStatus::Connected
    );
}

// ---------- the merged list ----------

#[test]
fn sessions_are_grouped_by_machine_the_local_one_first_each_newest_first() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.c.host().new_session("a1");
    net.c.host().rename("a1", "a-old");
    net.c.host().new_session("a2");
    net.c.host().rename("a2", "a-new");
    net.c.sync();
    net.host("m_b").new_session("b1");
    net.host("m_b").rename("b1", "b-old");
    net.host("m_b").new_session("b2");
    net.host("m_b").rename("b2", "b-new");
    net.reach("m_b");
    assert_eq!(net.names(), ["a-new", "a-old", "b-new", "b-old"]);
    assert_eq!(
        net.sidebar(),
        [
            "# studio Connected",
            "a-new",
            "a-old",
            "# laptop Connected",
            "b-new",
            "b-old",
        ]
    );
}

#[test]
fn without_peers_the_sidebar_has_no_machine_headers() {
    let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    c.sync();
    let entries = c.client().sidebar();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|e| matches!(e, SidebarEntry::Session(_)))
    );
}

#[test]
fn a_peer_not_reached_yet_shows_as_a_connecting_group() {
    let (net, _) = Machines::new(&[("m_b", "laptop")]);
    assert_eq!(net.sidebar(), ["# studio Connected", "# laptop Connecting"]);
}

#[test]
fn a_peers_session_is_labelled_with_the_peers_name() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    // Even one its log recorded before machines had ids.
    net.host("m_b").stamp_machine("local");
    net.host("m_b").new_session("b1");
    net.reach("m_b");
    let client = net.c.client();
    let s = &client.sessions()[0];
    assert_eq!(s.machine, m("m_b"));
    assert_eq!(client.machine_label(&s.machine), "laptop");
}

#[test]
fn entries_from_each_host_are_folded_by_their_own_seqs() {
    // Both logs start at seq 1: the peer's entries are not taken for repeats of the local host's.
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.c.host().new_session("a1");
    net.c.sync();
    net.host("m_b").new_session("b1");
    net.reach("m_b");
    let mut ids: Vec<_> = net
        .c
        .client()
        .sessions()
        .iter()
        .map(|s| s.id.to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, ["a1", "b1"]);
}

#[test]
fn a_session_id_two_hosts_both_claim_is_listed_once_as_the_local_hosts() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.c.host().new_session("dup");
    net.c.host().rename("dup", "mine");
    net.c.sync();
    net.host("m_b").new_session("dup");
    net.host("m_b").rename("dup", "theirs");
    net.reach("m_b");
    assert_eq!(net.names(), ["mine"]);
}

// ---------- routing ----------

/// A client with session `a1` on its own host and `b1` on peer `m_b`, with `b1` selected and the view sized.
fn with_peer_session_selected() -> Machines {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.c.host().new_session("a1");
    net.c.sync();
    net.host("m_b").new_session("b1");
    net.reach("m_b");
    net.user(Intent::Resize(PtySize { cols: 80, rows: 24 }));
    net.user(Intent::SelectSession(sid("b1")));
    net
}

#[test]
fn input_for_a_peers_session_goes_to_its_host() {
    let mut net = with_peer_session_selected();
    let sent = net.user(Intent::keys("ls\r"));
    assert_eq!(
        peer_notifications(&sent, "m_b"),
        [&ClientNotification::SessionInput(SessionInput {
            session: sid("b1"),
            data: b"ls\r".to_vec(),
            kind: InputKind::Keys,
        })]
    );
    assert!(!sends_local(&sent));
}

#[test]
fn selecting_a_peers_session_resizes_it_on_its_host() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.c.host().new_session("a1");
    net.c.sync();
    net.host("m_b").new_session("b1");
    net.reach("m_b");
    net.user(Intent::Resize(PtySize { cols: 90, rows: 30 }));
    let sent = net.user(Intent::SelectSession(sid("b1")));
    assert_eq!(
        peer_notifications(&sent, "m_b"),
        [&ClientNotification::SessionResize(SessionResize {
            session: sid("b1"),
            cols: 90,
            rows: 30,
        })]
    );
    assert!(!sends_local(&sent));
}

#[test]
fn renaming_a_peers_session_asks_its_host() {
    let mut net = with_peer_session_selected();
    net.user(Intent::BeginRename);
    net.user(Intent::EditRename("api".into()));
    let sent = net.user(Intent::Confirm);
    let bodies: Vec<_> = peer_requests(&sent, "m_b")
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(
        bodies,
        [ClientRequest::SessionRename(SessionRename {
            session: sid("b1"),
            name: "api".into(),
        })]
    );
    assert!(!sends_local(&sent));
}

#[test]
fn closing_a_peers_session_asks_its_host() {
    let mut net = with_peer_session_selected();
    net.user(Intent::BeginClose);
    let sent = net.user(Intent::Confirm);
    let bodies: Vec<_> = peer_requests(&sent, "m_b")
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert!(
        matches!(&bodies[..], [ClientRequest::SessionClose(SessionClose { session, .. })] if *session == sid("b1")),
        "{bodies:?}"
    );
    assert!(!sends_local(&sent));
}

#[test]
fn the_close_check_of_a_peers_worktree_asks_its_host_and_takes_its_answer() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.host("m_b").select_workspace("/src/app");
    net.host("m_b").new_session("b1");
    net.host("m_b").worktree_ready("b1");
    net.reach("m_b");
    let asked = net.user(Intent::BeginCloseSession(sid("b1")));
    let bodies: Vec<_> = peer_requests(&asked, "m_b")
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(
        bodies,
        [ClientRequest::WorktreeStatus(WorktreeCheck {
            session: sid("b1")
        })]
    );
    net.answer("m_b", &asked, Ok(json!({})));
    assert_eq!(
        net.c.client().close_check(),
        Some(&shepherd_core::client::CloseCheck::Gone)
    );
}

#[test]
fn a_new_session_starts_on_the_selected_sessions_machine() {
    let mut net = with_peer_session_selected();
    let sent = net.user(Intent::NewSession);
    let bodies: Vec<_> = peer_requests(&sent, "m_b")
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert!(
        matches!(&bodies[..], [ClientRequest::SessionNew(_)]),
        "{bodies:?}"
    );
    assert!(!sends_local(&sent));
}

#[test]
fn a_new_session_made_on_a_peer_is_selected_once_listed() {
    let mut net = with_peer_session_selected();
    let sent = net.user(Intent::NewSession);
    net.host("m_b").new_session("b2");
    net.answer("m_b", &sent, Ok(json!({ "session": "b2" })));
    net.sync("m_b");
    assert_eq!(net.c.client().selected(), Some(&sid("b2")));
}

#[test]
fn a_local_session_still_goes_to_the_local_host() {
    let mut net = with_peer_session_selected();
    net.user(Intent::SelectSession(sid("a1")));
    let sent = net.user(Intent::keys("x"));
    assert!(sends_local(&sent));
    assert_eq!(
        peer_notifications(&sent, "m_b"),
        Vec::<&ClientNotification>::new()
    );
}

#[test]
fn a_peer_that_does_not_know_a_request_gets_no_restart_offer() {
    let mut net = with_peer_session_selected();
    net.user(Intent::BeginRename);
    let sent = net.user(Intent::Confirm);
    net.answer(
        "m_b",
        &sent,
        Err(WireError::MethodNotFound("_shepherd/session/rename".into())),
    );
    assert_eq!(net.c.client().restart_offer(), None);
}

// ---------- an unreachable peer ----------

#[test]
fn a_peer_that_drops_keeps_its_sessions_in_a_dimmed_group_and_is_dialled_again() {
    let mut net = with_peer_session_selected();
    let after = net.handle(peer("m_b", PeerEvent::Disconnected));
    assert_eq!(after, [ClientCommand::ConnectPeer(m("m_b"))]);
    assert!(net.names().contains(&"shell-1".to_owned()));
    let laptop = &net.c.client().machines()[1];
    assert!(
        !matches!(laptop.status, MachineStatus::Connected),
        "{:?}",
        laptop.status
    );
    assert!(net.c.client().offline(&sid("b1")));
    assert!(!net.c.client().offline(&sid("a1")));
}

#[test]
fn input_for_an_unreachable_peers_session_is_dropped() {
    let mut net = with_peer_session_selected();
    net.handle(peer("m_b", PeerEvent::Disconnected));
    assert_eq!(net.user(Intent::keys("x")), []);
}

#[test]
fn a_request_for_an_unreachable_peers_session_says_that_machine_is_unreachable() {
    let mut net = with_peer_session_selected();
    net.handle(peer("m_b", PeerEvent::Disconnected));
    net.user(Intent::BeginRename);
    assert_eq!(net.user(Intent::Confirm), []);
    assert_eq!(net.c.client().notice(), Some("not connected to laptop"));
}

#[test]
fn a_reconnected_peer_is_subscribed_to_from_the_last_entry_of_each_of_its_logs() {
    let mut net = with_peer_session_selected();
    // The last seq of each of B's logs, from B's own log.
    let last = |log: Option<&SessionId>, net: &mut Machines| -> Option<Seq> {
        net.host("m_b")
            .log()
            .iter()
            .filter(|e| e.session() == log)
            .map(|e| e.seq())
            .max()
    };
    let host_seq = last(None, &mut net);
    let b1_seq = last(Some(&sid("b1")), &mut net);
    net.handle(peer("m_b", PeerEvent::Disconnected));
    let subscribe = net.handshake("m_b");
    let bodies: Vec<_> = peer_requests(&subscribe, "m_b")
        .iter()
        .map(|r| r.body.clone())
        .collect();
    let mut expected = SubscribeFrom {
        host: host_seq,
        ..SubscribeFrom::default()
    };
    expected
        .sessions
        .insert(sid("b1"), b1_seq.unwrap_or(Seq::ZERO));
    assert_eq!(
        bodies,
        [ClientRequest::Subscribe(Subscribe { from: expected })]
    );
}

#[test]
fn entries_a_reconnected_peer_sends_again_are_not_applied_twice_and_new_ones_are() {
    let mut net = with_peer_session_selected();
    net.handle(peer("m_b", PeerEvent::Disconnected));
    net.host("m_b").rename("b1", "renamed-while-away");
    let subscribe = net.handshake("m_b");
    // A careless host resends everything: only the new entry changes anything.
    let everything = net.host("m_b").log().to_vec();
    for envelope in everything {
        net.handle(peer(
            "m_b",
            PeerEvent::Host(HostMessage::Notification(HostNotification::LogEntry(
                Box::new(LogEntry { envelope }),
            ))),
        ));
    }
    net.answer("m_b", &subscribe, Ok(json!({})));
    assert!(net.names().contains(&"renamed-while-away".to_owned()));
    assert_eq!(net.names().len(), 2);
}

#[test]
fn a_refused_tunnel_shows_why_and_is_tried_again_after_a_backoff() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    let tunnel = net.handle(peer("m_b", PeerEvent::Connected { at: at(0) }));
    let after = net.answer(
        "m_b",
        &tunnel,
        Err(WireError::InvalidParams("cannot reach laptop".into())),
    );
    assert_eq!(after, [ClientCommand::DisconnectPeer(m("m_b"))]);
    assert_eq!(
        net.c.client().machines()[1].status,
        MachineStatus::Offline(Some("cannot reach laptop".into()))
    );
    // The drop that follows changes nothing.
    assert_eq!(net.handle(peer("m_b", PeerEvent::Disconnected)), []);
    assert_eq!(net.handle(ClientEvent::Tick { at: at(10) }), []);
    let early = PEER_RETRY.as_millis() as i64 - 1;
    assert_eq!(net.handle(ClientEvent::Tick { at: at(10 + early) }), []);
    let due = PEER_RETRY.as_millis() as i64;
    assert_eq!(
        net.handle(ClientEvent::Tick { at: at(10 + due) }),
        [ClientCommand::ConnectPeer(m("m_b"))]
    );
}

#[test]
fn each_failed_attempt_doubles_the_wait_up_to_the_cap() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    let mut now = 0i64;
    let mut waits = Vec::new();
    for _ in 0..8 {
        net.handle(peer(
            "m_b",
            PeerEvent::ConnectFailed {
                at: at(now),
                error: "no".into(),
            },
        ));
        net.handle(ClientEvent::Tick { at: at(now) });
        let start = now;
        loop {
            now += 250;
            if !net.handle(ClientEvent::Tick { at: at(now) }).is_empty() {
                break;
            }
        }
        waits.push(now - start);
    }
    let ms = |d: jiff::SignedDuration| d.as_millis() as i64;
    assert_eq!(waits[0], ms(PEER_RETRY));
    assert_eq!(waits[1], 2 * ms(PEER_RETRY));
    assert_eq!(*waits.last().unwrap_or(&0), ms(PEER_RETRY_MAX));
}

#[test]
fn clicking_an_unreachable_machine_tries_it_again_at_once() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.handle(peer(
        "m_b",
        PeerEvent::ConnectFailed {
            at: at(0),
            error: "no".into(),
        },
    ));
    assert_eq!(
        net.user(Intent::SelectMachine(m("m_b"))),
        [ClientCommand::ConnectPeer(m("m_b"))]
    );
}

#[test]
fn clicking_a_reachable_machine_selects_its_newest_session() {
    let mut net = with_peer_session_selected();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::SelectMachine(m("m_b")));
    assert_eq!(net.c.client().selected(), Some(&sid("b1")));
}

#[test]
fn a_peer_of_another_protocol_version_shows_as_incompatible() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    let tunnel = net.handle(peer("m_b", PeerEvent::Connected { at: at(0) }));
    let hello = net.answer("m_b", &tunnel, Ok(json!({})));
    net.answer(
        "m_b",
        &hello,
        Err(WireError::VersionMismatch {
            host: 99,
            client: 1,
        }),
    );
    assert_eq!(
        net.c.client().machines()[1].status,
        MachineStatus::Incompatible
    );
    assert_eq!(net.c.client().popup(), None, "no restart dialog for a peer");
}

#[test]
fn a_peer_that_never_answers_its_tunnel_is_given_up_on_and_tried_again() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.handle(peer("m_b", PeerEvent::Connected { at: at(0) }));
    let timeout = shepherd_core::client::HELLO_TIMEOUT.as_millis() as i64;
    assert_eq!(
        net.handle(ClientEvent::Tick {
            at: at(timeout - 1)
        }),
        []
    );
    assert_eq!(
        net.handle(ClientEvent::Tick { at: at(timeout) }),
        [ClientCommand::DisconnectPeer(m("m_b"))]
    );
}

#[test]
fn a_peer_no_longer_trusted_is_dropped_with_its_sessions() {
    let mut net = Machines::connected(&[("m_b", "laptop")]);
    net.host("m_b").new_session("b1");
    net.sync("m_b");
    assert_eq!(net.names().len(), 1);
    // The local host reconnects and no longer lists it.
    net.handle(ClientEvent::Disconnected);
    let hello = net.handle(ClientEvent::Connected { at: at(0) });
    let answer = json!({ "version": PROTOCOL_VERSION, "machine": "m_test", "name": "studio" });
    let subscribe = net.c.answer(&hello, Ok(answer));
    let caught_up = net.c.answer(&subscribe, Ok(json!({})));
    let after = net.c.answer(&caught_up, Ok(json!({ "peers": [] })));
    assert_eq!(after, [ClientCommand::DisconnectPeer(m("m_b"))]);
    assert_eq!(net.names(), Vec::<String>::new());
    assert_eq!(net.c.client().machines().len(), 1);
}

#[test]
fn a_peer_still_trusted_on_a_local_reconnect_is_not_dialled_again() {
    let mut net = Machines::connected(&[("m_b", "laptop")]);
    net.handle(ClientEvent::Disconnected);
    let hello = net.handle(ClientEvent::Connected { at: at(0) });
    let answer = json!({ "version": PROTOCOL_VERSION, "machine": "m_test" });
    let subscribe = net.c.answer(&hello, Ok(answer));
    let caught_up = net.c.answer(&subscribe, Ok(json!({})));
    let list = json!({ "peers": [{ "id": "m_b", "name": "laptop" }] });
    assert_eq!(net.c.answer(&caught_up, Ok(list)), []);
}

#[test]
fn a_notice_from_a_peer_times_out_like_any_other() {
    let mut net = with_peer_session_selected();
    net.user(Intent::BeginRename);
    let sent = net.user(Intent::Confirm);
    net.answer(
        "m_b",
        &sent,
        Err(WireError::InvalidParams("name taken".into())),
    );
    assert_eq!(net.c.client().notice(), Some("name taken"));
    net.handle(ClientEvent::Tick { at: at(0) });
    let gone = NOTICE_TIMEOUT.as_millis() as i64;
    net.handle(ClientEvent::Tick { at: at(gone) });
    assert_eq!(net.c.client().notice(), None);
}

#[test]
fn local_log_entries_still_arrive_as_before() {
    let mut net = Machines::connected(&[("m_b", "laptop")]);
    net.c.host().new_session("a9");
    net.c.sync();
    assert_eq!(net.c.client().sessions()[0].id, sid("a9"));
}
