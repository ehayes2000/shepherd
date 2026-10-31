//! `shepherd pair`, `shepherd join <ticket>`, `shepherd peers` and `shepherd unpair <name|id>` (T30): each asks the
//! local host (launching it if none is running), which owns the machine's network endpoint and decides everything;
//! these only print what it answers. Names and ids are display-safe before they are printed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use shepherd_adapters::peers::{canonical_peer_id, is_ticket};
use shepherd_adapters::{ProcessHostLauncher, UnixConnectLink};
use shepherd_core::display::display_safe;
use shepherd_core::ports::{ConnId, HostLauncher, Link, LinkEvent, LinkEventSink};
use shepherd_core::wire::{
    ClientRequest, HostMessage, HostResult, PairJoin, PairOutcome, PairStart, PairWait, PeerInfo,
    PeerList, PeerRemove, Request, decode_host_message, encode_request,
};

/// How long a pairing's start may take: the host may wait for its home relay before it has a ticket.
const START_LIMIT: Duration = Duration::from_secs(60);
/// How long `shepherd pair` waits for the other machine: past the pairing's own expiry, which ends it first.
const WAIT_LIMIT: Duration = Duration::from_secs(11 * 60);
/// How long a join may take: the host dials the other machine, within its own bound.
const JOIN_LIMIT: Duration = Duration::from_secs(90);
/// How long listing or removing a peer may take.
const QUICK_LIMIT: Duration = Duration::from_secs(10);
/// The most grapheme clusters of a name or id printed.
const SHOWN_MAX: usize = 128;

/// Asks the host one request at a time.
pub trait HostRequests {
    /// Sends `body` and waits up to `limit` for its answer, or says why there is none.
    fn request(&mut self, body: ClientRequest, limit: Duration) -> Result<HostResult, String>;
}

fn safe(text: &str) -> String {
    display_safe(text, SHOWN_MAX).into_string()
}

fn shown(peer: &PeerInfo) -> String {
    format!("{} ({})", safe(&peer.name), safe(peer.id.as_str()))
}

fn unexpected(answer: HostResult) -> String {
    format!("the host answered something else: {answer:?}")
}

fn write(out: &mut impl Write, text: &str) -> Result<(), String> {
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

/// `shepherd pair`: prints a ticket, then waits until the other machine joins or the pairing ends.
pub fn pair(host: &mut impl HostRequests, out: &mut impl Write) -> Result<(), String> {
    let started = match host.request(ClientRequest::PairStart(PairStart {}), START_LIMIT)? {
        HostResult::PairStart(started) => started,
        other => return Err(unexpected(other)),
    };
    // Printed whole, never cut like a name: so it must be a ticket, and plain letters and digits.
    let ticket = &started.ticket;
    if !(is_ticket(ticket) && ticket.bytes().all(|b| b.is_ascii_alphanumeric())) {
        return Err("the host answered with something that isn't a pairing ticket".to_owned());
    }
    write(
        out,
        &format!(
            "On the other machine, run this before {}:\n\n    shepherd join {ticket}\n\nWaiting for it to join...\n",
            safe(started.expires.as_str()),
        ),
    )?;
    match host.request(ClientRequest::PairWait(PairWait {}), WAIT_LIMIT)? {
        HostResult::PairWait(PairOutcome::Paired { peer }) => {
            write(out, &format!("Paired with {}.\n", shown(&peer)))
        }
        HostResult::PairWait(PairOutcome::Refused { peer, name }) => Err(format!(
            "{} ({}) used the ticket with the wrong token, which used it up: run `shepherd pair` again",
            safe(&name),
            safe(peer.as_str())
        )),
        HostResult::PairWait(PairOutcome::Expired) => {
            Err("nobody joined in time: run `shepherd pair` again".to_owned())
        }
        HostResult::PairWait(PairOutcome::Replaced) => {
            Err("a newer `shepherd pair` replaced this one".to_owned())
        }
        other => Err(unexpected(other)),
    }
}

/// `shepherd join <ticket>`. Text that isn't a ticket is refused here, so the host never goes online for it.
pub fn join(
    host: &mut impl HostRequests,
    ticket: &str,
    out: &mut impl Write,
) -> Result<(), String> {
    if !is_ticket(ticket) {
        return Err(
            "that is not a shepherd pairing ticket: copy the whole line `shepherd pair` printed"
                .to_owned(),
        );
    }
    let ticket = ticket.trim().to_owned();
    match host.request(ClientRequest::PairJoin(PairJoin { ticket }), JOIN_LIMIT)? {
        HostResult::PairJoin(peer) => write(out, &format!("Paired with {}.\n", shown(&peer))),
        other => Err(unexpected(other)),
    }
}

/// `shepherd peers`: each trusted machine's name, id, and whether the host is connected to it.
pub fn peers(host: &mut impl HostRequests, out: &mut impl Write) -> Result<(), String> {
    let list = match host.request(ClientRequest::PeerList(PeerList {}), QUICK_LIMIT)? {
        HostResult::PeerList(list) => list.peers,
        other => return Err(unexpected(other)),
    };
    if list.is_empty() {
        return write(out, "No paired machines. Pair one with `shepherd pair`.\n");
    }
    let lines: String = list
        .iter()
        .map(|peer| {
            let state = if peer.connected {
                "connected"
            } else {
                "offline"
            };
            format!(
                "{}  {}  {state}\n",
                safe(&peer.name),
                safe(peer.id.as_str())
            )
        })
        .collect();
    write(out, &lines)
}

/// `shepherd unpair <name|id>`. An id is matched in any of the spellings a node id has.
pub fn unpair(host: &mut impl HostRequests, who: &str, out: &mut impl Write) -> Result<(), String> {
    let peer = canonical_peer_id(who).map_or_else(|| who.to_owned(), |id| id.as_str().to_owned());
    match host.request(ClientRequest::PeerRemove(PeerRemove { peer }), QUICK_LIMIT)? {
        HostResult::PeerRemove(peer) => write(out, &format!("Unpaired {}.\n", shown(&peer))),
        other => Err(unexpected(other)),
    }
}

/// The local host over its socket.
pub struct SocketHost {
    link: UnixConnectLink,
    conn: ConnId,
    events: Receiver<LinkEvent>,
    next_id: i64,
}

impl SocketHost {
    /// Connects to the host serving on `socket`, launching `shepherd host` first if none answers.
    pub fn connect(socket: &Path) -> Result<Self, String> {
        match Self::try_connect(socket) {
            Ok(host) => Ok(host),
            Err(_) => {
                let launcher = ProcessHostLauncher::shepherd_host(socket)
                    .map_err(|e| format!("can't find the shepherd executable: {e}"))?;
                launcher.launch().map_err(|e| e.to_string())?;
                Self::try_connect(socket)
                    .map_err(|e| format!("cannot reach the shepherd host: {e}"))
            }
        }
    }

    fn try_connect(socket: &Path) -> Result<Self, String> {
        let (tx, events) = mpsc::channel();
        let sink: Arc<dyn LinkEventSink> = Arc::new(ChannelSink(Mutex::new(tx)));
        let (link, conn) = UnixConnectLink::connect(socket, sink).map_err(|e| e.to_string())?;
        Ok(Self {
            link,
            conn,
            events,
            next_id: 0,
        })
    }
}

impl HostRequests for SocketHost {
    fn request(&mut self, body: ClientRequest, limit: Duration) -> Result<HostResult, String> {
        self.next_id += 1;
        let request = Request {
            id: self.next_id.into(),
            body,
        };
        let method = request.body.method();
        let line = encode_request(&request);
        self.link
            .send(self.conn, line.strip_suffix(b"\n").unwrap_or(&line))
            .map_err(|e| format!("cannot reach the shepherd host: {e}"))?;
        let deadline = Instant::now() + limit;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(wait) {
                Ok(LinkEvent::Line { line, .. }) => match decode_host_message(&line) {
                    Ok(HostMessage::Response(r)) if r.id == request.id => {
                        let typed = r.typed(method).map_err(|e| e.to_string())?;
                        return typed.result.map_err(|e| e.to_string());
                    }
                    _ => {}
                },
                Ok(LinkEvent::Connected { .. }) => {}
                Ok(LinkEvent::Disconnected { .. }) | Err(RecvTimeoutError::Disconnected) => {
                    return Err("the shepherd host went away".to_owned());
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err("the shepherd host didn't answer in time".to_owned());
                }
            }
        }
    }
}

impl Drop for SocketHost {
    fn drop(&mut self) {
        let _ = self.link.close(self.conn);
    }
}

struct ChannelSink(Mutex<Sender<LinkEvent>>);

impl LinkEventSink for ChannelSink {
    fn send(&self, event: LinkEvent) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(event);
    }
}

/// The local host's socket: `~/.shepherd/host.sock`.
pub fn socket() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".shepherd").join("host.sock"))
}

/// Runs subcommand `command` (`pair`, `join`, `peers` or `unpair`) with the arguments after it, printing to stdout.
pub fn main(command: &str, args: &[String]) -> Result<(), String> {
    let arg = |what: &str| {
        args.first()
            .cloned()
            .ok_or_else(|| format!("usage: shepherd {command} <{what}>"))
    };
    let mut out = std::io::stdout();
    match command {
        "pair" => pair(&mut SocketHost::connect(&socket()?)?, &mut out),
        "join" => {
            let ticket = arg("ticket")?;
            join(&mut SocketHost::connect(&socket()?)?, &ticket, &mut out)
        }
        "peers" => peers(&mut SocketHost::connect(&socket()?)?, &mut out),
        "unpair" => {
            let who = arg("name or id")?;
            unpair(&mut SocketHost::connect(&socket()?)?, &who, &mut out)
        }
        other => Err(format!("unknown command `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use shepherd_core::acpp::Timestamp;
    use shepherd_core::ids::MachineId;
    use shepherd_core::wire::{PairStartOk, PeerListOk, WireError};

    use super::*;

    /// A host that answers from a script, and records what it was asked.
    #[derive(Default)]
    struct Scripted {
        answers: VecDeque<Result<HostResult, String>>,
        asked: Vec<ClientRequest>,
    }

    impl Scripted {
        fn answering(answers: impl IntoIterator<Item = Result<HostResult, String>>) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                asked: Vec::new(),
            }
        }
    }

    impl HostRequests for Scripted {
        fn request(&mut self, body: ClientRequest, _: Duration) -> Result<HostResult, String> {
            self.asked.push(body);
            self.answers.pop_front().unwrap_or(Err("no answer".into()))
        }
    }

    fn peer(name: &str, id: &str, connected: bool) -> PeerInfo {
        PeerInfo {
            id: MachineId::new(id),
            name: name.into(),
            connected,
        }
    }

    /// A ticket as a machine on n0's relays with four LAN addresses and a 12-character name hands it out (made by
    /// `shepherd-adapters`' ticket tests): longer than any name is ever shown.
    const TICKET: &str = "shepherdzkj2yfyfdbyhdvt3qpd76dx6qeeor3cfgblv25zgq6jthw62xz6akabdnb2hi4dthixs65ltmuys2mjoojswyylzfzxdaltjojxwqltmnfxgwlrpaeaauaaaa6zn4aybacwbabacwlpagaiaycuacf5s3ybqcagavbaadmw6amgg6ztgnfrwkllun53wk4qha4dqobyha4dqobyha4dqobyh";

    fn started_with(ticket: &str) -> HostResult {
        HostResult::PairStart(PairStartOk {
            ticket: ticket.into(),
            expires: Timestamp::parse("2026-10-29T12:10:00Z").unwrap(),
        })
    }

    fn started() -> HostResult {
        started_with(TICKET)
    }

    fn printed(
        run: impl FnOnce(&mut Vec<u8>) -> Result<(), String>,
    ) -> (String, Result<(), String>) {
        let mut out = Vec::new();
        let result = run(&mut out);
        (String::from_utf8(out).unwrap(), result)
    }

    #[test]
    fn pair_prints_the_join_command_then_who_joined() {
        let mut host = Scripted::answering([
            Ok(started()),
            Ok(HostResult::PairWait(PairOutcome::Paired {
                peer: peer("laptop", "ab12", false),
            })),
        ]);
        let (out, result) = printed(|out| pair(&mut host, out));
        assert_eq!(result, Ok(()));
        assert!(TICKET.len() > 200);
        assert!(
            out.contains(&format!("    shepherd join {TICKET}\n")),
            "{out}"
        );
        assert!(out.contains("2026-10-29T12:10:00Z"), "{out}");
        assert!(out.ends_with("Paired with laptop (ab12).\n"), "{out}");
        assert_eq!(
            host.asked,
            [
                ClientRequest::PairStart(PairStart {}),
                ClientRequest::PairWait(PairWait {})
            ]
        );
    }

    #[test]
    fn pair_fails_saying_why_when_the_pairing_ends_without_a_join() {
        for (outcome, says) in [
            (PairOutcome::Expired, "nobody joined in time"),
            (PairOutcome::Replaced, "replaced this one"),
            (
                PairOutcome::Refused {
                    peer: MachineId::new("cd34"),
                    name: "stranger".into(),
                },
                "stranger (cd34) used the ticket with the wrong token",
            ),
        ] {
            let mut host = Scripted::answering([Ok(started()), Ok(HostResult::PairWait(outcome))]);
            let (_, result) = printed(|out| pair(&mut host, out));
            assert!(
                result.as_ref().is_err_and(|e| e.contains(says)),
                "{result:?}"
            );
        }
    }

    #[test]
    fn pair_reports_a_start_the_host_refused_and_waits_for_nothing() {
        let mut host = Scripted::answering([Err(
            WireError::Internal("cannot go online".into()).to_string()
        )]);
        let (out, result) = printed(|out| pair(&mut host, out));
        assert!(result.is_err_and(|e| e.contains("cannot go online")));
        assert_eq!(out, "");
        assert_eq!(host.asked.len(), 1);
    }

    #[test]
    fn pair_refuses_a_ticket_that_is_not_plain_letters_and_digits_and_waits_for_nothing() {
        let mut host = Scripted::answering([Ok(started_with("shepherd\u{1b}[2Jabc"))]);
        let (out, result) = printed(|out| pair(&mut host, out));
        assert!(result.is_err(), "{result:?}");
        assert_eq!(out, "");
        assert_eq!(host.asked.len(), 1);
    }

    #[test]
    fn join_sends_the_ticket_trimmed_and_prints_who_it_paired_with() {
        let mut host =
            Scripted::answering([Ok(HostResult::PairJoin(peer("office", "ab12", false)))]);
        let (out, result) = printed(|out| join(&mut host, &format!("  {TICKET}\n"), out));
        assert_eq!(result, Ok(()));
        assert_eq!(out, "Paired with office (ab12).\n");
        assert_eq!(
            host.asked,
            [ClientRequest::PairJoin(PairJoin {
                ticket: TICKET.into()
            })]
        );
    }

    #[test]
    fn join_with_text_that_is_not_a_ticket_never_asks_the_host() {
        for text in ["shepherdabc", &TICKET[..TICKET.len() - 4], "hello"] {
            let mut host = Scripted::default();
            let (_, result) = printed(|out| join(&mut host, text, out));
            assert!(
                result
                    .as_ref()
                    .is_err_and(|e| e.contains("not a shepherd pairing ticket")),
                "{result:?}"
            );
            assert_eq!(host.asked, [], "{text}");
        }
    }

    #[test]
    fn peers_lists_each_name_id_and_state_and_says_when_there_are_none() {
        let mut host = Scripted::answering([Ok(HostResult::PeerList(PeerListOk {
            peers: vec![peer("laptop", "ab12", true), peer("tower", "cd34", false)],
        }))]);
        let (out, _) = printed(|out| peers(&mut host, out));
        assert_eq!(out, "laptop  ab12  connected\ntower  cd34  offline\n");
        let mut host =
            Scripted::answering([Ok(HostResult::PeerList(PeerListOk { peers: vec![] }))]);
        let (out, _) = printed(|out| peers(&mut host, out));
        assert!(out.starts_with("No paired machines."), "{out}");
    }

    #[test]
    fn names_and_ids_are_printed_display_safe() {
        let mut host = Scripted::answering([Ok(HostResult::PeerList(PeerListOk {
            peers: vec![peer("evil\u{1b}]0;owned\u{7}", "ab\u{202e}12", false)],
        }))]);
        let (out, _) = printed(|out| peers(&mut host, out));
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}') && !out.contains('\u{202e}'));
        assert_eq!(out, "evil]0;owned  ab12  offline\n");
    }

    #[test]
    fn unpair_sends_a_node_id_in_its_canonical_spelling_and_a_name_as_it_is() {
        let id = "AE58FF8833241AC82D6FF7611046ED67B5072D142C588D0063E942D9A75502B6";
        let mut host = Scripted::answering([
            Ok(HostResult::PeerRemove(peer("laptop", "ae58", false))),
            Ok(HostResult::PeerRemove(peer("laptop", "ae58", false))),
        ]);
        let (out, result) = printed(|out| unpair(&mut host, id, out));
        assert_eq!(result, Ok(()));
        assert_eq!(out, "Unpaired laptop (ae58).\n");
        let _ = printed(|out| unpair(&mut host, "laptop", out));
        assert_eq!(
            host.asked,
            [
                ClientRequest::PeerRemove(PeerRemove {
                    peer: id.to_lowercase()
                }),
                ClientRequest::PeerRemove(PeerRemove {
                    peer: "laptop".into()
                }),
            ]
        );
    }
}
