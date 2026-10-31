//! One client, every machine (T31b): the local host reports its trusted peers, the client reaches each one's host
//! through a tunnel on its own host, merges their sessions grouped by machine, and sends each session's requests to
//! its own host.

use serde_json::{Value, json};
use shepherd_core::acpp::{Envelope, Seq, Source, SourceKind, Timestamp};
use shepherd_core::client::{
    AskError, AskId, Cell, CleanupOutcome, Client, ClientCommand, ClientEvent, Intent,
    MachineStatus, MenuItem, NOTICE_TIMEOUT, PEER_RETRY, PEER_RETRY_MAX, PeerEvent, Popup,
    SidebarEntry,
};
use shepherd_core::config::WorkspaceHooks;
use shepherd_core::host::HostEvent;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::pty::PtySize;
use shepherd_core::wire::{
    BuildId, CloseWorktree, ExeDigest, HostMessage, HostNotification, HostResult, LogEntry,
    SessionNew, SessionNewOk, SessionResume,
};
use shepherd_core::wire::{
    ClientMessage, ClientNotification, ClientRequest, InputKind, PROTOCOL_VERSION, RawResponse,
    Request, SessionClose, SessionInput, SessionRename, SessionResize, Subscribe, SubscribeFrom,
    Tunnel, WireError, WorktreeCheck,
};
use shepherd_testkit::builders::{ClientDriver, HostDriver, claude_session_spec, requests, ts};

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
        Self::from_client(
            ClientDriver::new(),
            json!({ "notify": { "bell": false } }),
            peers,
        )
    }

    /// [`Machines::new`], driving `c`, whose local host's hello answer also says `extra`.
    fn from_client(
        mut c: ClientDriver,
        extra: Value,
        peers: &[(&str, &str)],
    ) -> (Self, Vec<ClientCommand>) {
        let started = c.handle(ClientEvent::Start);
        assert_eq!(started, [ClientCommand::Connect]);
        let hello = c.handle(ClientEvent::Connected { at: at(0) });
        let mut answer =
            json!({ "version": PROTOCOL_VERSION, "machine": "m_test", "name": "studio" });
        if let (Some(answer), Some(extra)) = (answer.as_object_mut(), extra.as_object()) {
            answer.extend(extra.clone());
        }
        let subscribe = c.answer(&hello, Ok(answer));
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
        let answer = json!({ "version": PROTOCOL_VERSION, "machine": machine });
        self.handshake_answering(machine, answer)
    }

    /// Opens `machine`'s conn and tunnel, and answers its hello with `answer`. Returns what the answer produced.
    fn handshake_answering(&mut self, machine: &str, answer: Value) -> Vec<ClientCommand> {
        let tunnel = self.handle(peer(machine, PeerEvent::Connected { at: at(0) }));
        let hello = self.answer(machine, &tunnel, Ok(json!({})));
        self.answer(machine, &hello, Ok(answer))
    }

    /// Hands the client `envelope` from `machine`'s host, whatever it was sent before.
    fn resend(&mut self, machine: &str, envelope: Envelope) -> Vec<ClientCommand> {
        let entry = HostNotification::LogEntry(Box::new(LogEntry { envelope }));
        self.handle(peer(
            machine,
            PeerEvent::Host(HostMessage::Notification(entry)),
        ))
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
fn the_layout_is_this_machines_hosts_not_a_peers() {
    let mut net = Machines::connected(&[("m_b", "laptop")]);
    net.host("m_b")
        .ui_apply(shepherd_core::layout::LayoutOp::PageNew {
            name: Some("theirs".into()),
            content: shepherd_core::layout::Content::Empty,
        });
    net.sync("m_b");
    assert_eq!(
        net.c.client().host_layout(),
        shepherd_core::layout::Layout::default()
    );
    net.c
        .host()
        .ui_apply(shepherd_core::layout::LayoutOp::PageNew {
            name: Some("ours".into()),
            content: shepherd_core::layout::Content::Empty,
        });
    net.c.sync();
    let names: Vec<String> = net
        .c
        .client()
        .layout()
        .pages()
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(names, ["1", "ours"]);
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
fn a_selected_pane_showing_a_peers_session_names_its_machine() {
    let net = with_peer_session_selected();
    let layout = net.c.client().layout();
    let shepherd_core::layout::Pane::Leaf { content, .. } = &layout.current_page().root else {
        panic!("{layout:?}");
    };
    assert_eq!(
        *content,
        shepherd_core::layout::Content::peer_session(m("m_b"), sid("b1"))
    );
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
    net.host("m_b").rename("b1", "first");
    net.sync("m_b");
    let first = net
        .host("m_b")
        .log()
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("a log"));
    net.handle(peer("m_b", PeerEvent::Disconnected));
    net.host("m_b").rename("b1", "second");
    let subscribe = net.handshake("m_b");
    net.sync("m_b");
    net.answer("m_b", &subscribe, Ok(json!({})));
    // A careless host sends the older rename again, after the newer one: folding it again would undo the newer.
    net.resend("m_b", first);
    assert!(
        net.names().contains(&"second".to_owned()),
        "{:?}",
        net.names()
    );
    // And the next reconnect still resumes after the newest entry.
    let newest = net
        .host("m_b")
        .log()
        .iter()
        .filter(|e| e.session() == Some(&sid("b1")))
        .map(|e| e.seq())
        .max();
    net.handle(peer("m_b", PeerEvent::Disconnected));
    let again = net.handshake("m_b");
    match &peer_requests(&again, "m_b")[..] {
        [
            Request {
                body: ClientRequest::Subscribe(sub),
                ..
            },
        ] => assert_eq!(sub.from.sessions.get(&sid("b1")).copied(), newest),
        other => panic!("expected a subscribe, got {other:?}"),
    }
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

// ---------- what stays this machine's ----------

fn alerts(commands: &[ClientCommand]) -> Vec<SessionId> {
    commands
        .iter()
        .filter_map(|c| match c {
            ClientCommand::Alert { session, .. } => Some(session.clone()),
            _ => None,
        })
        .collect()
}

/// The agent in `session` on `host` goes idle.
fn idle(host: &mut HostDriver, session: &str) {
    let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    let frame = json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} });
    host.append(Some(session), hook, frame);
}

fn build(byte: u8) -> BuildId {
    BuildId {
        version: "0.1.0".into(),
        digest: ExeDigest::from_bytes([byte; 32]),
    }
}

#[test]
fn with_a_peers_session_selected_the_finder_and_cleanup_still_ask_this_machines_host() {
    let mut net = with_peer_session_selected();
    let find = net.user(Intent::BeginFind);
    net.user(Intent::Dismiss);
    let cleanup = net.user(Intent::BeginCleanup);
    for (what, sent) in [("finder", find), ("cleanup", cleanup)] {
        assert_eq!(requests(&sent).len(), 1, "{what}: {sent:?}");
        assert_eq!(peer_requests(&sent, "m_b").len(), 0, "{what}: {sent:?}");
    }
}

#[test]
fn with_a_peers_session_selected_a_stale_host_is_asked_to_update_on_this_machine_only() {
    let c = ClientDriver::from_client(Client::new(m("m_client")).with_build(build(1)));
    let same = json!({ "build": build(1) });
    let (mut net, _) = Machines::from_client(c, same, &[("m_b", "laptop")]);
    net.host("m_b").new_session("b1");
    net.reach("m_b");
    net.user(Intent::SelectSession(sid("b1")));
    net.handle(ClientEvent::Disconnected);
    let hello = net.handle(ClientEvent::Connected { at: at(0) });
    let stale = json!({
        "version": PROTOCOL_VERSION,
        "machine": "m_test",
        "build": build(2),
        "installed": ExeDigest::from_bytes([1; 32]),
    });
    let sent = net.c.answer(&hello, Ok(stale));
    let upgrades: Vec<_> = requests(&sent)
        .into_iter()
        .filter(|r| matches!(r.body, ClientRequest::HostUpgrade(_)))
        .collect();
    assert_eq!(upgrades.len(), 1, "{sent:?}");
    assert_eq!(peer_requests(&sent, "m_b").len(), 0);
}

#[test]
fn restarting_this_machines_host_stops_only_its_own_sessions() {
    let net = with_peer_session_selected();
    assert_eq!(net.c.client().sessions().len(), 2);
    assert_eq!(net.c.client().sessions_to_stop(), Some(1));
}

/// Session `id` on `host`, with a worktree in `/src/app`, its PTY exited long before the ticks of these tests.
fn stale_on(host: &mut HostDriver, id: &str) {
    host.new_session(id);
    host.worktree_ready(id);
    host.pin_ts("2026-10-01T00:00:00Z");
    host.pty_exited(id, Some(0));
    host.unpin_ts();
}

#[test]
fn cleanup_never_offers_a_peers_stale_session() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.c.host().select_workspace("/src/app");
    stale_on(net.c.host(), "a1");
    net.c.sync();
    net.host("m_b").select_workspace("/src/app");
    stale_on(net.host("m_b"), "b1");
    net.reach("m_b");
    net.handle(ClientEvent::Tick {
        at: ts("2026-10-29T12:00:00Z"),
    });
    let asked = net.user(Intent::BeginCleanup);
    let checks = net.c.answer(
        &asked,
        Ok(json!({ "worktrees": [], "cleanup_after_days": 7 })),
    );
    let bodies: Vec<_> = requests(&checks).iter().map(|r| r.body.clone()).collect();
    assert_eq!(
        bodies,
        [ClientRequest::WorktreeStatus(WorktreeCheck {
            session: sid("a1")
        })]
    );
}

#[test]
fn a_session_being_cleaned_up_here_is_not_finished_by_a_peer_closing_one_of_the_same_id() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.c.host().select_workspace("/src/app");
    stale_on(net.c.host(), "dup");
    net.c.sync();
    net.host("m_b").select_workspace("/src/app");
    net.host("m_b").new_session("dup");
    net.host("m_b").worktree_ready("dup");
    net.reach("m_b");
    net.handle(ClientEvent::Tick {
        at: ts("2026-10-29T12:00:00Z"),
    });
    let asked = net.user(Intent::BeginCleanup);
    let check = net.c.answer(
        &asked,
        Ok(json!({ "worktrees": [], "cleanup_after_days": 7 })),
    );
    let status =
        json!({ "dirty": false, "ahead": 0, "pushed": false, "merged_into_default": true });
    net.c.answer(&check, Ok(json!({ "status": status })));
    let close = net.user(Intent::CleanupRemoveSafe);
    assert_eq!(requests(&close).len(), 1, "{close:?}");
    net.host("m_b")
        .close_with("dup", CloseWorktree::IfSafe, WorkspaceHooks::default());
    net.host("m_b").handle(HostEvent::Restarted);
    net.sync("m_b");
    let outcomes: Vec<_> = net
        .c
        .client()
        .cleanup_rows()
        .iter()
        .map(|r| r.outcome.clone())
        .collect();
    assert_eq!(outcomes, [Some(CleanupOutcome::Removing)]);
}

#[test]
fn a_worktree_a_peer_kept_on_close_is_a_notice_once_live() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.host("m_b").select_workspace("/src/app");
    net.host("m_b").new_session("b1");
    net.host("m_b").worktree_ready("b1");
    net.reach("m_b");
    net.host("m_b")
        .close_with("b1", CloseWorktree::IfSafe, WorkspaceHooks::default());
    net.host("m_b").handle(HostEvent::Restarted);
    net.sync("m_b");
    assert_eq!(
        net.c.client().notice(),
        Some("kept the worktree of shell-1: the host stopped before the worktree was removed")
    );
}

// ---------- each host's own ----------

#[test]
fn resuming_a_peers_session_asks_its_host() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.host("m_b")
        .new_session_with("b1", claude_session_spec());
    net.host("m_b")
        .agent_started("b1", "claude-code", Some("abc123"));
    net.host("m_b").pty_exited("b1", Some(0));
    net.reach("m_b");
    net.user(Intent::OpenMenu {
        session: sid("b1"),
        at: Cell { col: 1, row: 1 },
    });
    let sent = net.user(Intent::MenuChoose(MenuItem::Resume));
    let bodies: Vec<_> = peer_requests(&sent, "m_b")
        .iter()
        .map(|r| r.body.clone())
        .collect();
    assert_eq!(
        bodies,
        [ClientRequest::SessionResume(SessionResume {
            session: sid("b1")
        })]
    );
    assert!(!sends_local(&sent));
}

/// A client whose own host (bell `local_bell`) runs `a1`, selected, and peer `m_b` (bell `peer_bell`) runs `b1`.
fn with_bells(local_bell: bool, peer_bell: bool) -> Machines {
    let extra = json!({ "notify": { "bell": local_bell } });
    let (mut net, _) = Machines::from_client(ClientDriver::new(), extra, &[("m_b", "laptop")]);
    net.c.host().new_session("a1");
    net.c.sync();
    net.host("m_b").new_session("b1");
    let answer = json!({
        "version": PROTOCOL_VERSION,
        "machine": "m_b",
        "notify": { "bell": peer_bell },
    });
    let subscribe = net.handshake_answering("m_b", answer);
    net.sync("m_b");
    net.answer("m_b", &subscribe, Ok(json!({})));
    net.user(Intent::SelectSession(sid("a1")));
    net
}

#[test]
fn a_peers_session_wanting_the_user_alerts_by_the_peers_bell() {
    let mut net = with_bells(false, true);
    idle(net.host("m_b"), "b1");
    assert_eq!(alerts(&net.sync("m_b")), [sid("b1")]);
}

#[test]
fn this_machines_bell_does_not_ring_for_a_peers_session() {
    let mut net = with_bells(true, false);
    idle(net.host("m_b"), "b1");
    assert_eq!(alerts(&net.sync("m_b")), Vec::<SessionId>::new());
    // This machine's own still does.
    net.user(Intent::WindowFocus(false));
    idle(net.c.host(), "a1");
    assert_eq!(alerts(&net.c.sync()), [sid("a1")]);
}

/// A peer's session shadowed by a local one of the same id is listed once this machine's goes: that local entry rings
/// nothing for it, since only its own host (whose bell is off) can say it wants the user.
#[test]
fn a_peers_session_unshadowed_by_a_local_entry_does_not_ring_this_machines_bell() {
    let mut net = with_bells(true, false);
    net.c.host().new_session("dup");
    net.c.sync();
    net.host("m_b").new_session("dup");
    idle(net.host("m_b"), "dup");
    assert_eq!(alerts(&net.sync("m_b")), Vec::<SessionId>::new());
    net.user(Intent::WindowFocus(false));
    net.c.host().close("dup");
    let commands = net.c.sync();
    assert!(
        net.c
            .client()
            .sessions()
            .iter()
            .any(|s| s.id == sid("dup") && s.attention),
        "the peer's session is listed now, wanting the user"
    );
    assert_eq!(alerts(&commands), Vec::<SessionId>::new());
}

#[test]
fn a_peer_dropped_while_catching_up_is_live_once_caught_up_again() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.c.host().new_session("a1");
    net.c.sync();
    net.host("m_b").new_session("b1");
    let bell = json!({ "version": PROTOCOL_VERSION, "machine": "m_b", "notify": { "bell": true } });
    // Its first subscribe is never answered.
    net.handshake_answering("m_b", bell.clone());
    net.sync("m_b");
    net.handle(peer("m_b", PeerEvent::Disconnected));
    let subscribe = net.handshake_answering("m_b", bell);
    net.answer("m_b", &subscribe, Ok(json!({})));
    net.user(Intent::SelectSession(sid("a1")));
    idle(net.host("m_b"), "b1");
    assert_eq!(alerts(&net.sync("m_b")), [sid("b1")]);
}

#[test]
fn a_reconnected_peers_selected_session_is_resized_to_the_view() {
    let mut net = with_peer_session_selected();
    net.handle(peer("m_b", PeerEvent::Disconnected));
    let subscribe = net.handshake("m_b");
    assert_eq!(
        peer_notifications(&subscribe, "m_b"),
        [&ClientNotification::SessionResize(SessionResize {
            session: sid("b1"),
            cols: 80,
            rows: 24,
        })]
    );
}

#[test]
fn entries_on_a_peers_conn_before_its_hello_are_ignored() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.host("m_b").new_session("b1");
    net.handle(peer("m_b", PeerEvent::Connected { at: at(0) }));
    net.sync("m_b");
    assert_eq!(net.names(), Vec::<String>::new());
}

#[test]
fn a_stray_connected_for_a_reached_peer_asks_for_no_tunnel() {
    let mut net = Machines::connected(&[("m_b", "laptop")]);
    assert_eq!(
        net.handle(peer("m_b", PeerEvent::Connected { at: at(5) })),
        []
    );
}

#[test]
fn a_peer_answering_hello_as_another_machine_is_a_failed_attempt() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.host("m_b").new_session("b1");
    let impostor = json!({ "version": PROTOCOL_VERSION, "machine": "m_test" });
    let after = net.handshake_answering("m_b", impostor);
    assert_eq!(after, [ClientCommand::DisconnectPeer(m("m_b"))]);
    let status = net.c.client().machines()[1].status.clone();
    assert!(
        matches!(&status, MachineStatus::Offline(Some(why)) if why.contains("another machine")),
        "{status:?}"
    );
    net.sync("m_b");
    assert_eq!(net.names(), Vec::<String>::new());
}

// ---------- choosing where a new session runs (T32) ----------

/// A hello answer from `machine`'s host offering `profiles`, the first the default.
fn hello_with_profiles(machine: &str, profiles: &[&str]) -> Value {
    let list: Vec<Value> = profiles
        .iter()
        .map(|name| json!({ "name": name, "agent": "shell" }))
        .collect();
    json!({
        "version": PROTOCOL_VERSION,
        "machine": machine,
        "profiles": list,
        "default_profile": profiles.first(),
        "notify": { "bell": false },
    })
}

/// [`Machines::reach`], answering `machine`'s hello with `answer`.
fn reach_answering(net: &mut Machines, machine: &str, answer: Value) {
    let subscribe = net.handshake_answering(machine, answer);
    net.sync(machine);
    net.answer(machine, &subscribe, Ok(json!({})));
}

/// This machine (`m_test`, profiles `here-a`, `here-b`) runs `a1`; peer `m_b` (`laptop`, profiles `there-a`,
/// `there-b`) runs `b1`; peer `m_c` (`tower`) can't be reached. The view is sized.
fn three_machines() -> Machines {
    let mut c = ClientDriver::new();
    c.host().new_session("a1");
    let (mut net, _) = Machines::from_client(
        c,
        hello_with_profiles("m_test", &["here-a", "here-b"]),
        &[("m_b", "laptop"), ("m_c", "tower")],
    );
    net.c.sync();
    net.host("m_b").new_session("b1");
    reach_answering(
        &mut net,
        "m_b",
        hello_with_profiles("m_b", &["there-a", "there-b"]),
    );
    net.handle(peer(
        "m_c",
        PeerEvent::ConnectFailed {
            at: at(0),
            error: "cannot reach tower".into(),
        },
    ));
    net.user(Intent::Resize(PtySize { cols: 80, rows: 24 }));
    net
}

fn profile_names(net: &Machines) -> Vec<String> {
    net.c
        .client()
        .profiles()
        .iter()
        .map(|p| p.name.clone())
        .collect()
}

/// The profile of each session asked of `machine`'s host (this machine's for `None`) among `commands`.
fn session_news(commands: &[ClientCommand], machine: Option<&str>) -> Vec<Option<String>> {
    let sent = match machine {
        Some(machine) => peer_requests(commands, machine),
        None => requests(commands),
    };
    sent.iter()
        .filter_map(|r| match &r.body {
            ClientRequest::SessionNew(new) => Some(new.profile.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn with_peers_the_new_session_popup_opens_on_the_selected_sessions_machine() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("b1")));
    assert_eq!(net.user(Intent::BeginNewSession), []);
    assert_eq!(net.c.client().popup(), Some(&Popup::PickProfile));
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_b")));
    assert_eq!(profile_names(&net), ["there-a", "there-b"]);
}

#[test]
fn with_a_local_session_selected_the_new_session_popup_opens_on_this_machine() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
    assert_eq!(profile_names(&net), ["here-a", "here-b"]);
}

#[test]
fn with_nothing_selected_the_new_session_popup_opens_on_this_machine() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.reach("m_b");
    assert_eq!(net.c.client().selected(), None);
    net.user(Intent::BeginNewSession);
    assert_eq!(net.c.client().popup(), Some(&Popup::PickProfile));
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
}

#[test]
fn with_peers_the_popup_opens_even_with_one_profile_so_the_machine_can_be_chosen() {
    let mut c = ClientDriver::new();
    c.host().new_session("a1");
    let (mut net, _) = Machines::from_client(
        c,
        hello_with_profiles("m_test", &["only"]),
        &[("m_b", "laptop")],
    );
    net.c.sync();
    net.reach("m_b");
    assert_eq!(net.user(Intent::BeginNewSession), []);
    assert_eq!(net.c.client().popup(), Some(&Popup::PickProfile));
}

#[test]
fn choosing_a_machine_shows_its_hosts_profiles() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    assert_eq!(net.user(Intent::PickMachine(m("m_b"))), []);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_b")));
    assert_eq!(profile_names(&net), ["there-a", "there-b"]);
    assert_eq!(net.c.client().default_profile(), Some("there-a"));
}

#[test]
fn picking_a_profile_starts_the_session_on_the_chosen_machine() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    net.user(Intent::PickMachine(m("m_b")));
    let sent = net.user(Intent::PickProfile("there-b".into()));
    assert_eq!(
        session_news(&sent, Some("m_b")),
        [Some("there-b".to_owned())]
    );
    assert!(!sends_local(&sent));
    assert_eq!(net.c.client().popup(), None);
}

#[test]
fn choosing_this_machine_back_starts_the_session_here() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("b1")));
    net.user(Intent::BeginNewSession);
    net.user(Intent::PickMachine(m("m_test")));
    assert_eq!(profile_names(&net), ["here-a", "here-b"]);
    let sent = net.user(Intent::PickProfile("here-b".into()));
    assert_eq!(session_news(&sent, None), [Some("here-b".to_owned())]);
    assert_eq!(peer_requests(&sent, "m_b").len(), 0);
}

#[test]
fn with_peers_confirming_the_popup_starts_the_chosen_machines_default_profile() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    net.user(Intent::PickMachine(m("m_b")));
    let sent = net.user(Intent::Confirm);
    assert_eq!(session_news(&sent, Some("m_b")), [None]);
    assert_eq!(net.c.client().popup(), None);
}

#[test]
fn an_unreachable_machine_cannot_be_chosen() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    assert_eq!(net.user(Intent::PickMachine(m("m_c"))), []);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
    // Nor one that isn't a machine at all.
    net.user(Intent::PickMachine(m("m_nowhere")));
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
}

#[test]
fn when_the_selected_sessions_machine_is_unreachable_the_popup_opens_on_this_machine() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("b1")));
    net.handle(peer("m_b", PeerEvent::Disconnected));
    net.user(Intent::BeginNewSession);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
    assert_eq!(profile_names(&net), ["here-a", "here-b"]);
}

#[test]
fn a_machine_chosen_and_then_lost_says_so_instead_of_starting_elsewhere() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    net.user(Intent::PickMachine(m("m_b")));
    net.handle(peer("m_b", PeerEvent::Disconnected));
    let sent = net.user(Intent::PickProfile("there-a".into()));
    assert_eq!(sent, []);
    assert_eq!(net.c.client().notice(), Some("not connected to laptop"));
}

#[test]
fn choosing_a_machine_with_no_popup_open_does_nothing() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    assert_eq!(net.user(Intent::PickMachine(m("m_b"))), []);
    assert_eq!(net.c.client().popup_machine(), None);
    assert_eq!(profile_names(&net), ["here-a", "here-b"]);
    net.user(Intent::BeginNewSession);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
}

#[test]
fn each_popup_starts_from_the_default_machine_again() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    net.user(Intent::PickMachine(m("m_b")));
    net.user(Intent::Dismiss);
    net.user(Intent::BeginNewSession);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
}

#[test]
fn choosing_a_machine_scrolls_the_profiles_back_to_the_top() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    net.user(Intent::ScrollPicker { by: 1, shown: 1 });
    assert_eq!(net.c.client().picker_scroll(), 1);
    net.user(Intent::PickMachine(m("m_b")));
    assert_eq!(net.c.client().picker_scroll(), 0);
}

#[test]
fn the_new_session_key_still_starts_on_the_selected_sessions_machine() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("b1")));
    let sent = net.user(Intent::NewSession);
    assert_eq!(session_news(&sent, Some("m_b")), [None]);
}

// ---------- workspaces per machine (T32) ----------

/// The paths of the workspace selects asked of `machine`'s host (this machine's for `None`) among `commands`.
fn workspace_selects(commands: &[ClientCommand], machine: Option<&str>) -> Vec<String> {
    let sent = match machine {
        Some(machine) => peer_requests(commands, machine),
        None => requests(commands),
    };
    sent.iter()
        .filter_map(|r| match &r.body {
            ClientRequest::WorkspaceSelect(w) => Some(w.path.clone()),
            _ => None,
        })
        .collect()
}

/// [`three_machines`], this machine's host in workspace `/here/app`, and the laptop's in `/there/api` after
/// `/there/old`.
fn three_machines_in_workspaces() -> Machines {
    let mut c = ClientDriver::new();
    c.host().select_workspace("/here/app");
    c.host().new_session("a1");
    let (mut net, _) = Machines::from_client(
        c,
        hello_with_profiles("m_test", &["here-a"]),
        &[("m_b", "laptop"), ("m_c", "tower")],
    );
    net.c.sync();
    net.host("m_b").select_workspace("/there/old");
    net.host("m_b").select_workspace("/there/api");
    net.host("m_b").new_session("b1");
    net.reach("m_b");
    net
}

fn paths(list: &[std::path::PathBuf]) -> Vec<&str> {
    list.iter().filter_map(|p| p.to_str()).collect()
}

#[test]
fn the_workspace_is_the_selected_sessions_machines() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("b1")));
    assert_eq!(
        net.c.client().workspace(),
        Some(std::path::Path::new("/there/api"))
    );
    net.user(Intent::SelectSession(sid("a1")));
    assert_eq!(
        net.c.client().workspace(),
        Some(std::path::Path::new("/here/app"))
    );
}

#[test]
fn the_switcher_opens_on_the_selected_sessions_machine_and_lists_its_workspaces() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("b1")));
    net.user(Intent::BeginWorkspace);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_b")));
    assert_eq!(
        paths(net.c.client().workspaces()),
        ["/there/api", "/there/old"]
    );
}

#[test]
fn choosing_another_machine_in_the_switcher_lists_its_workspaces() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("b1")));
    net.user(Intent::BeginWorkspace);
    net.user(Intent::ScrollSwitcher { by: 1, shown: 1 });
    assert_eq!(net.c.client().switcher_scroll(), 1);
    net.user(Intent::PickMachine(m("m_test")));
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
    assert_eq!(paths(net.c.client().workspaces()), ["/here/app"]);
    assert_eq!(
        net.c.client().workspace(),
        Some(std::path::Path::new("/here/app"))
    );
    assert_eq!(net.c.client().switcher_scroll(), 0);
    // An unreachable one can't be chosen here either.
    net.user(Intent::PickMachine(m("m_c")));
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
}

#[test]
fn a_workspace_picked_in_the_switcher_is_selected_on_its_machine() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginWorkspace);
    net.user(Intent::PickMachine(m("m_b")));
    let sent = net.user(Intent::PickWorkspace("/there/old".into()));
    assert_eq!(workspace_selects(&sent, Some("m_b")), ["/there/old"]);
    assert!(!sends_local(&sent));
}

#[test]
fn a_path_typed_in_the_switcher_is_opened_on_its_machine_which_closes_it_once_agreed() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("b1")));
    net.user(Intent::BeginWorkspace);
    net.user(Intent::EditWorkspacePath("/there/new".into()));
    let sent = net.user(Intent::Confirm);
    assert_eq!(workspace_selects(&sent, Some("m_b")), ["/there/new"]);
    assert!(!sends_local(&sent));
    assert!(matches!(
        net.c.client().popup(),
        Some(Popup::Workspace { .. })
    ));
    net.answer("m_b", &sent, Ok(json!({})));
    assert_eq!(net.c.client().popup(), None);
}

#[test]
fn a_path_a_peer_refuses_is_shown_in_the_switcher() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("b1")));
    net.user(Intent::BeginWorkspace);
    net.user(Intent::EditWorkspacePath("/nope".into()));
    let sent = net.user(Intent::Confirm);
    net.answer(
        "m_b",
        &sent,
        Err(WireError::InvalidParams("/nope is not a git repo".into())),
    );
    assert_eq!(
        net.c.client().popup(),
        Some(&Popup::Workspace {
            path: "/nope".into(),
            error: Some("/nope is not a git repo".into()),
        })
    );
}

#[test]
fn a_peers_sessions_are_marked_elsewhere_by_its_own_workspace() {
    let mut net = three_machines_in_workspaces();
    net.host("m_b").select_workspace("/there/old");
    net.sync("m_b");
    let elsewhere = |net: &Machines, id: &str| {
        net.c
            .client()
            .sessions()
            .iter()
            .find(|s| s.id == sid(id))
            .map(|s| s.elsewhere)
    };
    assert_eq!(elsewhere(&net, "b1"), Some(true));
    assert_eq!(elsewhere(&net, "a1"), Some(false));
}

#[test]
fn when_the_selected_sessions_machine_is_unreachable_the_switcher_opens_on_this_machine() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("b1")));
    net.handle(peer("m_b", PeerEvent::Disconnected));
    net.user(Intent::BeginWorkspace);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
    assert_eq!(paths(net.c.client().workspaces()), ["/here/app"]);
}

#[test]
fn a_machine_that_is_neither_this_one_nor_a_peer_cannot_be_chosen_from_a_peer() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    net.user(Intent::PickMachine(m("m_b")));
    net.user(Intent::PickMachine(m("m_nowhere")));
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_b")));
    assert_eq!(profile_names(&net), ["there-a", "there-b"]);
}

/// This machine's host reconnects and lists `peers` (id, name) as the ones it trusts.
fn relist(net: &mut Machines, peers: &[(&str, &str)]) {
    net.handle(ClientEvent::Disconnected);
    let hello = net.handle(ClientEvent::Connected { at: at(0) });
    let subscribe = net.c.answer(
        &hello,
        Ok(hello_with_profiles("m_test", &["here-a", "here-b"])),
    );
    let caught_up = net.c.answer(&subscribe, Ok(json!({})));
    let list: Vec<Value> = peers
        .iter()
        .map(|(id, name)| json!({ "id": id, "name": name }))
        .collect();
    net.c.answer(&caught_up, Ok(json!({ "peers": list })));
}

#[test]
fn a_chosen_machine_no_longer_trusted_puts_the_popup_back_on_this_machine_and_says_so() {
    let mut net = three_machines();
    net.user(Intent::SelectSession(sid("a1")));
    net.user(Intent::BeginNewSession);
    net.user(Intent::PickMachine(m("m_b")));
    relist(&mut net, &[("m_c", "tower")]);
    assert_eq!(net.c.client().popup(), Some(&Popup::PickProfile));
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
    assert_eq!(profile_names(&net), ["here-a", "here-b"]);
    assert_eq!(
        net.c.client().notice(),
        Some("laptop is no longer a trusted machine")
    );
}

#[test]
fn a_switcher_on_a_machine_no_longer_trusted_goes_back_to_this_machine() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("b1")));
    net.user(Intent::BeginWorkspace);
    relist(&mut net, &[]);
    assert_eq!(net.c.client().popup_machine(), Some(&m("m_test")));
    assert_eq!(paths(net.c.client().workspaces()), ["/here/app"]);
}

#[test]
fn this_machines_workspace_is_its_hosts_whichever_session_is_selected() {
    let mut net = three_machines_in_workspaces();
    net.user(Intent::SelectSession(sid("b1")));
    assert_eq!(
        net.c.client().local_workspace(),
        Some(std::path::Path::new("/here/app"))
    );
}

// ---------- scripts (T46) ----------

fn ask_of(id: u64, machine: &str, request: ClientRequest) -> ClientEvent {
    ClientEvent::Ask {
        ask: AskId(id),
        machine: Some(m(machine)),
        request,
    }
}

fn session_new() -> ClientRequest {
    ClientRequest::SessionNew(SessionNew::default())
}

#[test]
fn the_client_is_settled_once_every_peer_is_caught_up_or_unreachable() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop"), ("m_c", "tower")]);
    assert!(!net.c.client().settled(), "peers being reached");
    let subscribe = net.handshake("m_b");
    assert!(!net.c.client().settled(), "laptop catching up");
    net.answer("m_b", &subscribe, Ok(json!({})));
    assert!(!net.c.client().settled(), "tower still being reached");
    net.handle(peer(
        "m_c",
        PeerEvent::ConnectFailed {
            at: at(0),
            error: "no route".into(),
        },
    ));
    assert!(net.c.client().settled());
}

#[test]
fn an_ask_of_a_peer_goes_to_its_host_and_its_answer_is_kept() {
    let mut net = Machines::connected(&[("m_b", "laptop")]);
    let sent = net.handle(ask_of(3, "m_b", session_new()));
    assert!(!sends_local(&sent));
    net.answer("m_b", &sent, Ok(json!({ "session": "s_b1" })));
    assert_eq!(
        net.c.client().answer(AskId(3)),
        Some(&Ok(HostResult::SessionNew(SessionNewOk {
            session: sid("s_b1")
        })))
    );
}

#[test]
fn an_ask_of_a_peer_that_cannot_be_reached_says_why_and_sends_nothing() {
    let (mut net, _) = Machines::new(&[("m_b", "laptop")]);
    net.handle(peer(
        "m_b",
        PeerEvent::ConnectFailed {
            at: at(0),
            error: "no route".into(),
        },
    ));
    let sent = net.handle(ask_of(1, "m_b", session_new()));
    assert_eq!(sent, []);
    let shown = match net.c.client().answer(AskId(1)) {
        Some(Err(e)) => e.to_string(),
        other => panic!("{other:?}"),
    };
    assert!(
        shown.contains("laptop") && shown.contains("no route"),
        "{shown}"
    );
}

#[test]
fn an_ask_of_a_peer_whose_conn_drops_is_answered_as_dropped() {
    let mut net = Machines::connected(&[("m_b", "laptop")]);
    net.handle(ask_of(1, "m_b", session_new()));
    net.handle(peer("m_b", PeerEvent::Disconnected));
    assert!(
        matches!(
            net.c.client().answer(AskId(1)),
            Some(Err(AskError::Dropped { .. }))
        ),
        "{:?}",
        net.c.client().answer(AskId(1))
    );
}

#[test]
fn a_peer_is_named_by_its_name_unless_two_share_it_and_by_its_id_always() {
    use shepherd_core::client::automation::{MachineError, resolve_machine};
    let net = Machines::connected(&[("m_b", "laptop"), ("m_c", "tower"), ("m_d", "tower")]);
    let client = net.c.client();
    assert_eq!(
        resolve_machine(client, "laptop").map(|g| g.id),
        Ok(m("m_b"))
    );
    assert_eq!(resolve_machine(client, "m_d").map(|g| g.id), Ok(m("m_d")));
    let err = resolve_machine(client, "tower").unwrap_err();
    assert!(matches!(err, MachineError::Ambiguous { .. }), "{err:?}");
    let shown = err.to_string();
    assert!(
        shown.contains("m_c") && shown.contains("m_d") && !shown.contains("m_b"),
        "{shown}"
    );
}
