//! `Peers` on an iroh endpoint (T30, decision 0019), bound with the machine's key ([`IrohMachineKey::secret`]).
//!
//! iroh is async, and shepherd's runtime isn't (decision 0011): one current-thread tokio runtime runs on a thread of
//! its own, and every call is a command sent to one task there, which takes them in order and never blocks the
//! caller. What happens comes back through the sink.
//!
//! - **Two ALPNs.** [`LINK_ALPN`] is for connections between trusted machines, [`PAIR_ALPN`] for pairing.
//! - **Trust** is checked in iroh's `after_handshake` hook, once the remote's node id is authenticated by TLS and
//!   before the connection reaches anything else: a link connection from a node outside the trusted set is reported
//!   ([`PeersEvent::Refused`]) and closed with [`REFUSED`]; a pairing connection while no pairing is open is closed
//!   with [`NO_PAIRING`]. Node ids are compared as `EndpointId`s, parsed from the canonical ids core pushes.
//! - **A link connection** that passes is greeted with a one-way stream, so the dialler knows it was accepted rather
//!   than about to be refused; it holds the connection (iroh's keep-alive keeps it up) until it closes or is dropped.
//! - **Pairing.** The joiner opens one stream and sends `{token, name, addr}` as JSON; the inviter takes the open pairing
//!   (so it is used up whatever the token), compares the token in constant time ([`subtle`]), reports the attempt, and
//!   then either answers `{name}` or closes with [`REFUSED`]. Each side keeps the other's address.
//! - **Tickets** are [`iroh_tickets`] of kind `shepherd`: postcard of the inviter's address (its relay URL and at most
//!   [`TICKET_IPS`] LAN addresses), its name and the token, in base32.
//! - **Addresses** learned from pairing go into a `MemoryLookup`, and into the last-address cache
//!   (`~/.shepherd/peer-addrs.json`, `0600` through [`private_fs`], non-authoritative: a cache that can't be read is
//!   ignored), which seeds the lookup on the next start. With [`Discovery::N0`] the endpoint also uses n0's relays and
//!   pkarr/DNS lookup, so a peer whose address changed is still found by its id. [`Discovery::Local`] binds localhost
//!   only, with relays and lookup off: no network, for tests.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{
    AfterHandshakeOutcome, Connection, ConnectionError, EndpointHooks, Side, VarInt, presets,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, TransportAddr};
use iroh_tickets::{ParseError, Ticket};
use serde::{Deserialize, Serialize};
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{
    ConnectError, JoinError, PairTicket, PairToken, PairingError, PairingId, Peers, PeersEvent,
    PeersEventSink, TOKEN_LEN,
};
use subtle::ConstantTimeEq;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::timeout;

use crate::identity::IrohMachineKey;
use crate::private_fs;

/// Connections between trusted machines.
pub const LINK_ALPN: &[u8] = b"shepherd/link/0";
/// Pairing.
pub const PAIR_ALPN: &[u8] = b"shepherd/pair/0";
/// The close code for a machine that isn't trusted, or a pairing token that didn't match.
pub const REFUSED: u32 = 403;
/// The close code for a pairing attempt while none is open.
pub const NO_PAIRING: u32 = 410;
/// The last-address cache, in the root.
pub const ADDRS_FILE: &str = "peer-addrs.json";
/// The most LAN addresses a ticket carries besides the relay URL.
pub const TICKET_IPS: usize = 4;
/// The longest a pairing message may be: a token, a name and an address.
const MAX_MESSAGE: usize = 16 * 1024;
/// How long a dial, a pairing exchange or a greeting may take: a bound on a peer that went quiet, not a timing.
const DIAL_LIMIT: Duration = Duration::from_secs(30);
/// How long a ticket waits for the endpoint to reach its home relay, so the ticket carries the relay URL.
const ONLINE_LIMIT: Duration = Duration::from_secs(10);
/// How long unbinding waits for the endpoint to close its connections cleanly.
const CLOSE_LIMIT: Duration = Duration::from_secs(5);

/// How the endpoint finds and reaches peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    /// n0's public relays and pkarr/DNS address lookup, plus the last-address cache (production).
    N0,
    /// Localhost only: no relays, no address lookup but what this machine learned. No network (tests).
    Local,
}

/// The machine id of node `id`: its canonical lowercase hex, as [`IrohMachineKey`] makes them.
fn machine(id: EndpointId) -> MachineId {
    MachineId::new(id.to_string())
}

/// `text` as a machine id, if it is a node id (hex or base32, any case), in canonical form. iroh reads hex in lowercase
/// only (and base32 in any case), so it gets lowercase.
pub fn canonical_peer_id(text: &str) -> Option<MachineId> {
    text.trim()
        .to_ascii_lowercase()
        .parse::<EndpointId>()
        .ok()
        .map(machine)
}

/// What a ticket carries.
#[derive(Debug, Serialize, Deserialize)]
struct Invite {
    addr: EndpointAddr,
    name: String,
    token: [u8; TOKEN_LEN],
}

impl Ticket for Invite {
    const KIND: &'static str = "shepherd";

    fn encode_bytes(&self) -> Vec<u8> {
        // Addresses, a string and bytes always serialize.
        postcard::to_stdvec(self).unwrap_or_default()
    }

    fn decode_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        postcard::from_bytes(bytes)
            .map_err(|_| ParseError::verification_failed("not a shepherd pairing ticket"))
    }
}

/// Joiner → inviter on [`PAIR_ALPN`].
#[derive(Debug, Serialize, Deserialize)]
struct JoinRequest {
    token: [u8; TOKEN_LEN],
    name: String,
    addr: EndpointAddr,
}

/// Inviter → joiner, when the token matched.
#[derive(Debug, Serialize, Deserialize)]
struct JoinReply {
    name: String,
}

/// `addr` cut down for a ticket: its relay URLs, and at most [`TICKET_IPS`] addresses on this machine or its LAN.
fn ticket_addr(addr: EndpointAddr) -> EndpointAddr {
    let local = |ip: &IpAddr| match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local(),
    };
    let relays = addr.addrs.iter().filter(|a| a.is_relay()).cloned();
    let ips = addr
        .addrs
        .iter()
        .filter(|a| matches!(a, TransportAddr::Ip(ip) if local(&ip.ip())))
        .take(TICKET_IPS)
        .cloned();
    EndpointAddr::from_parts(addr.id, relays.chain(ips).collect::<Vec<_>>())
}

enum Command {
    Bind,
    Unbind,
    SetTrusted(BTreeSet<MachineId>),
    AcceptPairing {
        id: PairingId,
        token: PairToken,
        name: String,
    },
    StopPairing,
    Join {
        id: PairingId,
        ticket: PairTicket,
        name: String,
    },
    Connect(MachineId),
    Addr(std_mpsc::Sender<Option<EndpointAddr>>),
    Learn(EndpointAddr),
}

/// The open pairing.
struct Pending {
    id: PairingId,
    token: [u8; TOKEN_LEN],
    name: String,
}

#[derive(Default)]
struct State {
    trusted: HashSet<EndpointId>,
    pending: Option<Pending>,
    /// Live link connections, by peer.
    conns: HashMap<EndpointId, Vec<Connection>>,
}

/// What the command task, the hook and the connection tasks share.
struct Shared {
    sink: Arc<dyn PeersEventSink>,
    lookup: MemoryLookup,
    cache: Option<PathBuf>,
    state: Mutex<State>,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, event: PeersEvent) {
        self.sink.send(event);
    }

    /// Keeps `addr` for dialling its node: in the lookup, and in the cache.
    fn learn(&self, addr: EndpointAddr) {
        self.lookup.add_endpoint_info(addr.clone());
        if let Some(cache) = &self.cache {
            let mut addrs = read_cache(cache);
            addrs.insert(machine(addr.id).to_string(), addr);
            // Non-authoritative: a cache that can't be written is only a slower start next time.
            let _ = write_cache(cache, &addrs);
        }
    }

    /// Holds `conn`, a link connection with `peer`, until it closes. The first one with a peer announces it. A peer
    /// that stopped being trusted meanwhile is dropped instead.
    fn hold(self: &Arc<Self>, peer: EndpointId, conn: Connection) -> bool {
        {
            let mut state = self.state();
            if !state.trusted.contains(&peer) {
                drop(state);
                conn.close(VarInt::from_u32(REFUSED), b"untrusted");
                return false;
            }
            let held = state.conns.entry(peer).or_default();
            held.push(conn.clone());
            if held.len() == 1 {
                self.send(PeersEvent::Connected {
                    peer: machine(peer),
                });
            }
        }
        let shared = self.clone();
        tokio::spawn(async move {
            conn.closed().await;
            shared.release(peer, &conn);
        });
        true
    }

    /// `conn` with `peer` closed: the last one gone announces it, unless it was dropped on purpose already.
    fn release(&self, peer: EndpointId, conn: &Connection) {
        let mut state = self.state();
        let Some(held) = state.conns.get_mut(&peer) else {
            return;
        };
        let before = held.len();
        held.retain(|c| c.stable_id() != conn.stable_id());
        if held.len() < before && held.is_empty() {
            state.conns.remove(&peer);
            self.send(PeersEvent::Disconnected {
                peer: machine(peer),
            });
        }
    }

    /// Closes every connection of each peer in `peers` (all of them for `None`), announcing each peer gone.
    fn drop_peers(&self, peers: Option<&HashSet<EndpointId>>, code: u32, reason: &[u8]) {
        let dropped: Vec<(EndpointId, Vec<Connection>)> = {
            let mut state = self.state();
            let gone: Vec<EndpointId> = state
                .conns
                .keys()
                .filter(|peer| peers.is_none_or(|keep| !keep.contains(*peer)))
                .copied()
                .collect();
            gone.into_iter()
                .filter_map(|peer| state.conns.remove(&peer).map(|c| (peer, c)))
                .collect()
        };
        for (peer, conns) in dropped {
            for conn in conns {
                conn.close(VarInt::from_u32(code), reason);
            }
            self.send(PeersEvent::Disconnected {
                peer: machine(peer),
            });
        }
    }
}

/// The admission check, right after the TLS handshake.
#[derive(Clone)]
struct TrustHook(Arc<Shared>);

impl std::fmt::Debug for TrustHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TrustHook")
    }
}

impl EndpointHooks for TrustHook {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        // A connection this machine dialled is to a machine core chose.
        if conn.side() == Side::Client {
            return AfterHandshakeOutcome::Accept;
        }
        let peer = conn.remote_id();
        let reject = |code: u32, reason: &[u8]| AfterHandshakeOutcome::Reject {
            error_code: VarInt::from_u32(code),
            reason: reason.to_vec(),
        };
        let state = self.0.state();
        match conn.alpn() {
            PAIR_ALPN if state.pending.is_some() => AfterHandshakeOutcome::Accept,
            PAIR_ALPN => reject(NO_PAIRING, b"no pairing in progress"),
            LINK_ALPN if state.trusted.contains(&peer) => AfterHandshakeOutcome::Accept,
            _ => {
                drop(state);
                self.0.send(PeersEvent::Refused {
                    peer: machine(peer),
                });
                reject(REFUSED, b"untrusted machine")
            }
        }
    }
}

/// `Peers` on an iroh endpoint. See the module docs.
pub struct IrohPeers {
    commands: UnboundedSender<Command>,
}

impl IrohPeers {
    /// The port for the machine with `key`, finding peers by `discovery`, keeping addresses in `cache` (if any), and
    /// reporting to `sink`. Starts its runtime thread; binds nothing until [`Peers::bind`]. Without a key it never
    /// binds: each bind fails ([`PeersEvent::BindFailed`]).
    pub fn new(
        key: Option<&IrohMachineKey>,
        discovery: Discovery,
        cache: Option<PathBuf>,
        sink: Arc<dyn PeersEventSink>,
    ) -> io::Result<Self> {
        let lookup = MemoryLookup::new();
        if let Some(cache) = &cache {
            for addr in read_cache(cache).into_values() {
                lookup.add_endpoint_info(addr);
            }
        }
        let shared = Arc::new(Shared {
            sink,
            lookup,
            cache,
            state: Mutex::new(State::default()),
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (commands, rx) = unbounded_channel();
        let actor = Actor {
            shared,
            secret: key.map(|k| k.secret().clone()),
            discovery,
            endpoint: None,
        };
        std::thread::Builder::new()
            .name("peers".into())
            .spawn(move || runtime.block_on(actor.run(rx)))?;
        Ok(Self { commands })
    }

    /// The cache file in `root` (`~/.shepherd`).
    pub fn cache_in(root: &Path) -> PathBuf {
        root.join(ADDRS_FILE)
    }

    /// This machine's address while bound, waiting for the runtime thread to answer. For tests and diagnostics; never
    /// call it from the runtime thread.
    pub fn addr(&self) -> Option<EndpointAddr> {
        let (tx, rx) = std_mpsc::channel();
        self.send(Command::Addr(tx));
        rx.recv().ok().flatten()
    }

    /// Keeps `addr` for dialling its node, as if it had been learned from a pairing.
    pub fn remember(&self, addr: EndpointAddr) {
        self.send(Command::Learn(addr));
    }

    fn send(&self, command: Command) {
        // The runtime thread only stops when this value is dropped.
        let _ = self.commands.send(command);
    }
}

impl Peers for IrohPeers {
    fn bind(&mut self) {
        self.send(Command::Bind);
    }

    fn unbind(&mut self) {
        self.send(Command::Unbind);
    }

    fn set_trusted(&mut self, peers: &BTreeSet<MachineId>) {
        self.send(Command::SetTrusted(peers.clone()));
    }

    fn accept_pairing(&mut self, id: PairingId, token: PairToken, name: &str) {
        self.send(Command::AcceptPairing {
            id,
            token,
            name: name.to_owned(),
        });
    }

    fn stop_pairing(&mut self) {
        self.send(Command::StopPairing);
    }

    fn join(&mut self, id: PairingId, ticket: &PairTicket, name: &str) {
        self.send(Command::Join {
            id,
            ticket: ticket.clone(),
            name: name.to_owned(),
        });
    }

    fn connect(&mut self, peer: &MachineId) {
        self.send(Command::Connect(peer.clone()));
    }
}

/// The task on the runtime thread that takes the commands, in order.
struct Actor {
    shared: Arc<Shared>,
    secret: Option<SecretKey>,
    discovery: Discovery,
    endpoint: Option<Endpoint>,
}

impl Actor {
    async fn run(mut self, mut commands: UnboundedReceiver<Command>) {
        while let Some(command) = commands.recv().await {
            self.handle(command).await;
        }
        // The port was dropped.
        self.unbind().await;
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Bind => self.bind().await,
            Command::Unbind => self.unbind().await,
            Command::SetTrusted(peers) => {
                let trusted: HashSet<EndpointId> = peers
                    .iter()
                    .filter_map(|id| id.as_str().parse::<EndpointId>().ok())
                    .collect();
                self.shared.state().trusted = trusted.clone();
                self.shared
                    .drop_peers(Some(&trusted), REFUSED, b"no longer trusted");
            }
            Command::AcceptPairing { id, token, name } => {
                let Some(endpoint) = self.endpoint.clone() else {
                    self.shared.send(PeersEvent::PairingFailed {
                        id,
                        error: PairingError::Unbound,
                    });
                    return;
                };
                self.shared.state().pending = Some(Pending {
                    id,
                    token: *token.as_bytes(),
                    name: name.clone(),
                });
                let (shared, discovery) = (self.shared.clone(), self.discovery);
                tokio::spawn(async move {
                    if discovery == Discovery::N0 {
                        // Without its home relay the ticket still works on the LAN, and by lookup.
                        let _ = timeout(ONLINE_LIMIT, endpoint.online()).await;
                    }
                    let invite = Invite {
                        addr: ticket_addr(endpoint.addr()),
                        name,
                        token: *token.as_bytes(),
                    };
                    let ticket = PairTicket::new(invite.encode_string());
                    shared.send(PeersEvent::PairingOpen { id, ticket });
                });
            }
            Command::StopPairing => self.shared.state().pending = None,
            Command::Join { id, ticket, name } => {
                let fail = |error| PeersEvent::JoinFailed { id, error };
                let Ok(invite) = Invite::decode_string(ticket.as_str().trim()) else {
                    return self.shared.send(fail(JoinError::BadTicket));
                };
                let Some(endpoint) = self.endpoint.clone() else {
                    return self.shared.send(fail(JoinError::Unbound));
                };
                tokio::spawn(join(endpoint, self.shared.clone(), id, invite, name));
            }
            Command::Connect(peer) => {
                let fail = |error| PeersEvent::ConnectFailed {
                    peer: peer.clone(),
                    error,
                };
                let Some(endpoint) = self.endpoint.clone() else {
                    return self.shared.send(fail(ConnectError::Unbound));
                };
                let Ok(node) = peer.as_str().parse::<EndpointId>() else {
                    let why = "not a machine id".to_owned();
                    return self.shared.send(fail(ConnectError::Unreachable(why)));
                };
                if !self.shared.state().trusted.contains(&node) {
                    return self.shared.send(fail(ConnectError::Untrusted));
                }
                tokio::spawn(dial(endpoint, self.shared.clone(), node));
            }
            Command::Addr(answer) => {
                let _ = answer.send(self.endpoint.as_ref().map(Endpoint::addr));
            }
            Command::Learn(addr) => self.shared.learn(addr),
        }
    }

    async fn bind(&mut self) {
        if self.endpoint.is_some() {
            return;
        }
        let Some(secret) = self.secret.clone() else {
            let error = "this machine has no usable key".to_owned();
            return self.shared.send(PeersEvent::BindFailed { error });
        };
        let built = match self.discovery {
            Discovery::N0 => Ok(Endpoint::builder(presets::N0)),
            Discovery::Local => Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .clear_ip_transports()
                .bind_addr("127.0.0.1:0")
                .map_err(|e| e.to_string()),
        };
        let bound = match built {
            Ok(builder) => builder
                .secret_key(secret)
                .alpns(vec![LINK_ALPN.to_vec(), PAIR_ALPN.to_vec()])
                .address_lookup(self.shared.lookup.clone())
                .hooks(TrustHook(self.shared.clone()))
                .bind()
                .await
                .map_err(|e| e.to_string()),
            Err(error) => Err(error),
        };
        match bound {
            Ok(endpoint) => {
                tokio::spawn(accept(endpoint.clone(), self.shared.clone()));
                self.endpoint = Some(endpoint);
            }
            Err(error) => self.shared.send(PeersEvent::BindFailed { error }),
        }
    }

    async fn unbind(&mut self) {
        let Some(endpoint) = self.endpoint.take() else {
            return;
        };
        self.shared.state().pending = None;
        self.shared.drop_peers(None, 0, b"going offline");
        let _ = timeout(CLOSE_LIMIT, endpoint.close()).await;
    }
}

/// Takes every incoming connection that passed the hook, until the endpoint closes.
async fn accept(endpoint: Endpoint, shared: Arc<Shared>) {
    while let Some(incoming) = endpoint.accept().await {
        let shared = shared.clone();
        tokio::spawn(async move {
            let Ok(accepting) = incoming.accept() else {
                return;
            };
            // A connection the hook refused ends here.
            let Ok(Ok(conn)) = timeout(DIAL_LIMIT, accepting).await else {
                return;
            };
            match conn.alpn() {
                LINK_ALPN => greet(conn, shared).await,
                PAIR_ALPN => serve_pair(conn, shared).await,
                _ => {}
            }
        });
    }
}

/// Holds an accepted link connection and tells the dialler it was accepted.
async fn greet(conn: Connection, shared: Arc<Shared>) {
    let peer = conn.remote_id();
    if !shared.hold(peer, conn.clone()) {
        return;
    }
    if let Ok(mut send) = conn.open_uni().await
        && send.write_all(b"welcome").await.is_ok()
    {
        let _ = send.finish();
    }
}

/// The inviting side of one pairing attempt.
async fn serve_pair(conn: Connection, shared: Arc<Shared>) {
    let peer = conn.remote_id();
    let request = timeout(DIAL_LIMIT, async {
        let (send, mut recv) = conn.accept_bi().await.ok()?;
        let bytes = recv.read_to_end(MAX_MESSAGE).await.ok()?;
        let request: JoinRequest = serde_json::from_slice(&bytes).ok()?;
        Some((send, request))
    })
    .await;
    let Ok(Some((mut send, request))) = request else {
        conn.close(VarInt::from_u32(REFUSED), b"bad pairing request");
        return;
    };
    // Used up by this attempt, whatever its token.
    let Some(pending) = shared.state().pending.take() else {
        conn.close(VarInt::from_u32(NO_PAIRING), b"no pairing in progress");
        return;
    };
    let accepted: bool = request.token.ct_eq(&pending.token).into();
    shared.send(PeersEvent::PairRequested {
        id: pending.id,
        peer: machine(peer),
        name: request.name,
        accepted,
    });
    if !accepted {
        conn.close(VarInt::from_u32(REFUSED), b"wrong pairing token");
        return;
    }
    // Where the joiner says it is, for the node TLS says it is.
    shared.learn(EndpointAddr::from_parts(peer, request.addr.addrs));
    let reply = serde_json::to_vec(&JoinReply { name: pending.name }).unwrap_or_default();
    if send.write_all(&reply).await.is_ok() && send.finish().is_ok() {
        // The joiner closes once it has read the answer.
        let _ = timeout(DIAL_LIMIT, conn.closed()).await;
    }
}

/// The joining side of a pairing.
async fn join(
    endpoint: Endpoint,
    shared: Arc<Shared>,
    id: PairingId,
    invite: Invite,
    name: String,
) {
    let inviter = invite.addr.id;
    let fail = |error| PeersEvent::JoinFailed { id, error };
    shared.lookup.add_endpoint_info(invite.addr.clone());
    let conn = match timeout(DIAL_LIMIT, endpoint.connect(invite.addr.clone(), PAIR_ALPN)).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(error)) => return shared.send(fail(JoinError::Unreachable(error.to_string()))),
        Err(_) => return shared.send(fail(JoinError::Unreachable("no answer".into()))),
    };
    let request = JoinRequest {
        token: invite.token,
        name,
        addr: endpoint.addr(),
    };
    let reply = timeout(DIAL_LIMIT, async {
        let (mut send, mut recv) = conn.open_bi().await.ok()?;
        send.write_all(&serde_json::to_vec(&request).ok()?)
            .await
            .ok()?;
        send.finish().ok()?;
        let bytes = recv.read_to_end(MAX_MESSAGE).await.ok()?;
        serde_json::from_slice::<JoinReply>(&bytes).ok()
    })
    .await;
    match reply {
        Ok(Some(reply)) => {
            conn.close(VarInt::from_u32(0), b"paired");
            shared.learn(invite.addr);
            shared.send(PeersEvent::Paired {
                id,
                peer: machine(inviter),
                name: reply.name,
            });
        }
        _ if refused(conn.close_reason().as_ref()) => shared.send(fail(JoinError::Refused)),
        _ => {
            conn.close(VarInt::from_u32(0), b"no answer");
            shared.send(fail(JoinError::Unreachable("no answer".into())));
        }
    }
}

/// Dials trusted peer `node` on the link ALPN, and holds the connection once it greets this machine.
async fn dial(endpoint: Endpoint, shared: Arc<Shared>, node: EndpointId) {
    let fail = |error| PeersEvent::ConnectFailed {
        peer: machine(node),
        error,
    };
    let conn = match timeout(DIAL_LIMIT, endpoint.connect(node, LINK_ALPN)).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(error)) => return shared.send(fail(ConnectError::Unreachable(error.to_string()))),
        Err(_) => return shared.send(fail(ConnectError::Unreachable("no answer".into()))),
    };
    let greeted = timeout(DIAL_LIMIT, async {
        let mut recv = conn.accept_uni().await.ok()?;
        recv.read_to_end(MAX_MESSAGE).await.ok()
    })
    .await;
    match greeted {
        // Untrusted meanwhile: `hold` closed it.
        Ok(Some(_)) => {
            if !shared.hold(node, conn) {
                shared.send(fail(ConnectError::Untrusted));
            }
        }
        _ if refused(conn.close_reason().as_ref()) => shared.send(fail(ConnectError::Refused)),
        _ => {
            conn.close(VarInt::from_u32(0), b"no greeting");
            shared.send(fail(ConnectError::Unreachable("no greeting".into())));
        }
    }
}

/// The other end closed the connection refusing this machine: untrusted, a wrong token, or no pairing open.
fn refused(reason: Option<&ConnectionError>) -> bool {
    match reason {
        Some(ConnectionError::ApplicationClosed(close)) => {
            close.error_code == VarInt::from_u32(REFUSED)
                || close.error_code == VarInt::from_u32(NO_PAIRING)
        }
        _ => false,
    }
}

/// The cache's addresses by machine id. Missing, unreadable or malformed reads as empty: it is only a hint.
fn read_cache(path: &Path) -> BTreeMap<String, EndpointAddr> {
    let mut read = OpenOptions::new();
    read.read(true);
    let Ok(file) = private_fs::open_existing_private_file(path, &read) else {
        return BTreeMap::new();
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_MESSAGE as u64 * 64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return BTreeMap::new();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Replaces the cache with `addrs`: written to a private temporary file beside it, then renamed over it.
fn write_cache(path: &Path, addrs: &BTreeMap<String, EndpointAddr>) -> io::Result<()> {
    let text = serde_json::to_vec_pretty(addrs).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    let mut write = OpenOptions::new();
    write.write(true).truncate(true);
    let mut file =
        private_fs::create_or_open_private_file(&tmp, &write).map_err(io::Error::other)?;
    file.write_all(&text)?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use shepherd_core::ports::MachineKey;
    use shepherd_testkit::contracts::peers::{Machine, PeersFixture, channel};
    use tempfile::TempDir;

    use super::*;
    use crate::identity::KeyFileIdentity;
    use shepherd_core::ports::MachineIdentity;

    /// Real endpoints on localhost, each with a fresh key in a root of its own in one temp dir.
    struct LocalFixture {
        dir: TempDir,
        next: u32,
    }

    impl LocalFixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            Self { dir, next: 0 }
        }

        fn root(&mut self) -> PathBuf {
            self.next += 1;
            self.dir.path().join(format!("root-{}", self.next))
        }
    }

    impl PeersFixture for LocalFixture {
        type Peers = IrohPeers;

        fn machine(&mut self) -> Machine<IrohPeers> {
            let root = self.root();
            let key = KeyFileIdentity::new(&root).load_or_create().unwrap();
            let (sink, events) = channel();
            let cache = Some(IrohPeers::cache_in(&root));
            Machine {
                id: key.machine_id(),
                peers: IrohPeers::new(Some(&key), Discovery::Local, cache, sink).unwrap(),
                events,
            }
        }

        fn introduce(&mut self, from: &mut Machine<IrohPeers>, to: &Machine<IrohPeers>) {
            let addr = to.peers.addr().expect("introduce: `to` is bound");
            from.peers.remember(addr);
        }
    }

    shepherd_testkit::peers_contract!(LocalFixture::new());

    impl LocalFixture {
        /// A machine whose key and cache are in `root`, made if missing: the same machine every time.
        fn machine_at(&mut self, root: &Path) -> Machine<IrohPeers> {
            let key = KeyFileIdentity::new(root).load_or_create().unwrap();
            let (sink, events) = channel();
            let cache = Some(IrohPeers::cache_in(root));
            Machine {
                id: key.machine_id(),
                peers: IrohPeers::new(Some(&key), Discovery::Local, cache, sink).unwrap(),
                events,
            }
        }
    }

    fn token(byte: u8) -> PairToken {
        PairToken::new([byte; TOKEN_LEN])
    }

    /// Pairs bound machines `a` (inviting) and `b`, each then trusting the other.
    fn pair(a: &mut Machine<IrohPeers>, b: &mut Machine<IrohPeers>) -> PairTicket {
        a.peers.accept_pairing(PairingId::new(1), token(3), "a");
        let PeersEvent::PairingOpen { ticket, .. } = a.next() else {
            panic!("no ticket");
        };
        b.peers.join(PairingId::new(1), &ticket, "b");
        assert!(matches!(b.next(), PeersEvent::Paired { .. }));
        assert!(matches!(
            a.next(),
            PeersEvent::PairRequested { accepted: true, .. }
        ));
        a.peers.set_trusted(&BTreeSet::from([b.id.clone()]));
        b.peers.set_trusted(&BTreeSet::from([a.id.clone()]));
        ticket
    }

    #[test]
    fn node_ids_are_read_in_any_case_and_spacing_and_given_canonical() {
        let id = SecretKey::from_bytes(&[9; 32]).public().to_string();
        let canonical = Some(MachineId::new(id.clone()));
        assert_eq!(canonical_peer_id(&id), canonical);
        assert_eq!(canonical_peer_id(&id.to_uppercase()), canonical);
        assert_eq!(canonical_peer_id(&format!("  {id}\n")), canonical);
        assert_eq!(canonical_peer_id("laptop"), None);
        assert_eq!(canonical_peer_id(&id[..63]), None);
    }

    #[test]
    fn a_ticket_is_a_shepherd_ticket() {
        let mut f = LocalFixture::new();
        let mut a = f.machine();
        a.peers.bind();
        a.peers.accept_pairing(PairingId::new(1), token(1), "a");
        let PeersEvent::PairingOpen { ticket, .. } = a.next() else {
            panic!("no ticket");
        };
        assert!(ticket.as_str().starts_with("shepherd"), "{ticket:?}");
    }

    #[test]
    fn a_restarted_machine_reaches_its_peer_at_the_address_its_cache_kept() {
        let mut f = LocalFixture::new();
        let (root_a, root_b) = (f.root(), f.root());
        let mut a = f.machine_at(&root_a);
        let mut b = f.machine_at(&root_b);
        a.peers.bind();
        b.peers.bind();
        pair(&mut a, &mut b);
        let cache = IrohPeers::cache_in(&root_b);
        let mode = std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(b);
        // A new port for the same machine knows only what its cache kept.
        let mut b = f.machine_at(&root_b);
        b.peers.bind();
        b.peers.set_trusted(&BTreeSet::from([a.id.clone()]));
        b.peers.connect(&a.id);
        assert_eq!(b.next(), PeersEvent::Connected { peer: a.id.clone() });
    }

    #[test]
    fn a_cache_that_is_not_an_address_list_is_ignored() {
        let mut f = LocalFixture::new();
        let root = f.root();
        let key = KeyFileIdentity::new(&root).load_or_create().unwrap();
        let cache = IrohPeers::cache_in(&root);
        std::fs::write(&cache, b"not json").unwrap();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (sink, events) = channel();
        let mut machine = Machine {
            id: key.machine_id(),
            peers: IrohPeers::new(Some(&key), Discovery::Local, Some(cache), sink).unwrap(),
            events,
        };
        machine.peers.bind();
        machine
            .peers
            .accept_pairing(PairingId::new(1), token(1), "a");
        assert!(matches!(machine.next(), PeersEvent::PairingOpen { .. }));
    }

    #[test]
    fn a_port_without_a_key_never_binds() {
        let (sink, events) = channel();
        let mut machine = Machine {
            id: MachineId::new("none"),
            peers: IrohPeers::new(None, Discovery::Local, None, sink).unwrap(),
            events,
        };
        machine.peers.bind();
        assert!(matches!(machine.next(), PeersEvent::BindFailed { .. }));
        machine
            .peers
            .accept_pairing(PairingId::new(1), token(1), "a");
        assert!(matches!(
            machine.next(),
            PeersEvent::PairingFailed {
                error: PairingError::Unbound,
                ..
            }
        ));
    }
}
