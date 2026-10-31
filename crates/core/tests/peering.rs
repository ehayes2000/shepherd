//! Pairing and trusted peers (T30, `core::host::peering`): who is trusted (folded from `peer/trusted` and
//! `peer/removed`), when the endpoint is bound, and how a pairing or a join runs and ends.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use shepherd_core::acpp::{
    Dir, Envelope, FrameDraft, MachineRenamed, PeerTrusted, ShepherdFrame, Source, SourceKind,
    Timestamp,
};
use shepherd_core::host::peering::{PAIRING_TTL, Peering, PeeringEvent, PeersCommand};
use shepherd_core::host::{HostCommand, ReplyTo};
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{
    ConnectError, JoinError, PairTicket, PairToken, PairingError, PairingId, PeersEvent,
};
use shepherd_core::wire::{HostResult, PairOutcome, PairStartOk, PeerInfo, PeerListOk, WireError};
use shepherd_testkit::builders::HostDriver;

/// `2026-10-29T12:00:00Z` plus `secs`.
fn t(secs: i64) -> Timestamp {
    let base: jiff::Timestamp = "2026-10-29T12:00:00Z"
        .parse()
        .unwrap_or_else(|e| panic!("{e}"));
    let at = base
        .checked_add(jiff::SignedDuration::from_secs(secs))
        .unwrap_or_else(|e| panic!("{e}"));
    Timestamp::parse(at.to_string()).unwrap_or_else(|e| panic!("{e}"))
}

fn m(id: &str) -> MachineId {
    MachineId::new(id)
}

fn token(byte: u8) -> PairToken {
    PairToken::new([byte; 16])
}

fn ticket(text: &str) -> PairTicket {
    PairTicket::new(text)
}

fn peers(cmd: PeersCommand) -> HostCommand {
    HostCommand::Peers(cmd)
}

fn set(ids: &[&str]) -> BTreeSet<MachineId> {
    ids.iter().map(|id| m(id)).collect()
}

fn reply(to: ReplyTo, result: HostResult) -> HostCommand {
    HostCommand::Reply {
        to,
        result: Ok(result),
    }
}

fn info(id: &str, name: &str, connected: bool) -> PeerInfo {
    PeerInfo {
        id: m(id),
        name: name.into(),
        connected,
    }
}

/// The method and params of an `Append`'s frame.
fn appended(command: &HostCommand) -> Option<(String, Value)> {
    let HostCommand::Append(draft) = command else {
        return None;
    };
    let frame = serde_json::to_value(&draft.frame).unwrap_or_else(|e| panic!("{e}"));
    Some((
        frame["method"].as_str().unwrap_or_default().to_owned(),
        frame["params"].clone(),
    ))
}

fn appends(commands: &[HostCommand]) -> Vec<(String, Value)> {
    commands.iter().filter_map(appended).collect()
}

fn not_appends(commands: Vec<HostCommand>) -> Vec<HostCommand> {
    commands
        .into_iter()
        .filter(|c| !matches!(c, HostCommand::Append(_)))
        .collect()
}

/// A host log and a [`Peering`], run the way the host runtime runs them: every entry appended is applied to the
/// peering, and after each batch the peering syncs what it pushes to the `Peers` port.
struct Driver {
    host: HostDriver,
    peering: Peering,
}

impl Driver {
    /// A host called `desk`, with nobody trusted.
    fn new() -> Self {
        let mut d = Self {
            host: HostDriver::new(),
            peering: Peering::new(),
        };
        d.record(ShepherdFrame::MachineRenamed(MachineRenamed {
            name: "desk".into(),
        }));
        d
    }

    /// A host called `desk` that already trusts each of `trusted` (id, name).
    fn trusting(trusted: &[(&str, &str)]) -> Self {
        let mut d = Self::new();
        for (id, name) in trusted {
            d.record(ShepherdFrame::PeerTrusted(PeerTrusted {
                node: m(id),
                name: (*name).into(),
            }));
        }
        d
    }

    /// Appends `frame` to the host log, as an earlier host run did.
    fn record(&mut self, frame: ShepherdFrame) {
        let draft = FrameDraft::new(
            frame.to_frame(),
            Source::new(SourceKind::Shepherd, None),
            Dir::Internal,
            None,
        );
        let env = self.host.append_draft(draft);
        self.peering.apply(&env);
    }

    fn apply_appends(&mut self, commands: &[HostCommand]) -> Vec<Envelope> {
        let mut envs = Vec::new();
        for command in commands {
            if let HostCommand::Append(draft) = command {
                let env = self.host.append_draft((**draft).clone());
                self.peering.apply(&env);
                envs.push(env);
            }
        }
        envs
    }

    /// Handles `event`, appends what it appends, then syncs: everything the runtime would execute, in order.
    fn handle(&mut self, event: PeeringEvent) -> Vec<HostCommand> {
        let mut commands = self.peering.handle(event);
        self.apply_appends(&commands);
        commands.extend(self.peering.sync());
        commands
    }

    fn peers(&mut self, event: PeersEvent) -> Vec<HostCommand> {
        self.handle(PeeringEvent::Peers(event))
    }

    fn reply_to(&mut self) -> ReplyTo {
        self.host.reply_to()
    }

    /// Starts a pairing with `token(1)` at `t(0)`, and returns its reply token and the commands.
    fn start(&mut self) -> (ReplyTo, Vec<HostCommand>) {
        self.start_at(token(1), 0)
    }

    fn start_at(&mut self, token: PairToken, secs: i64) -> (ReplyTo, Vec<HostCommand>) {
        let reply = self.reply_to();
        let commands = self.handle(PeeringEvent::Start {
            reply,
            token,
            at: t(secs),
        });
        (reply, commands)
    }

    /// Starts a pairing, shows its ticket, and waits on it: returns the waiter's reply token.
    fn open_and_wait(&mut self) -> ReplyTo {
        self.start();
        self.peers(PeersEvent::PairingOpen {
            id: PairingId::new(1),
            ticket: ticket("t1"),
        });
        let waiter = self.reply_to();
        assert_eq!(self.handle(PeeringEvent::Wait { reply: waiter }), vec![]);
        waiter
    }

    fn list(&mut self) -> Vec<PeerInfo> {
        let to = self.reply_to();
        match self.handle(PeeringEvent::List { reply: to }).as_slice() {
            [
                HostCommand::Reply {
                    result: Ok(HostResult::PeerList(PeerListOk { peers })),
                    ..
                },
            ] => peers.clone(),
            other => panic!("expected the peer list, got {other:?}"),
        }
    }
}

// ---------- binding lazily ----------

#[test]
fn a_host_that_never_paired_binds_nothing() {
    let mut d = Driver::new();
    assert_eq!(d.peering.sync(), vec![]);
    assert_eq!(d.list(), vec![]);
}

#[test]
fn a_host_with_trusted_peers_binds_on_start_trusting_them_and_dials_each() {
    let mut d = Driver::trusting(&[("bb", "laptop"), ("aa", "desk2")]);
    assert_eq!(
        d.peering.sync(),
        vec![
            peers(PeersCommand::SetTrusted(set(&["aa", "bb"]))),
            peers(PeersCommand::Bind),
            peers(PeersCommand::Connect { peer: m("aa") }),
            peers(PeersCommand::Connect { peer: m("bb") }),
        ]
    );
    assert_eq!(d.peering.sync(), vec![], "already pushed");
}

// ---------- the inviting side ----------

#[test]
fn starting_a_pairing_binds_then_opens_it_with_the_token_and_this_machines_name() {
    let mut d = Driver::new();
    let (_, commands) = d.start();
    assert_eq!(
        commands,
        vec![
            peers(PeersCommand::Bind),
            peers(PeersCommand::AcceptPairing {
                id: PairingId::new(1),
                token: token(1),
                name: "desk".into(),
            }),
        ]
    );
}

#[test]
fn the_start_is_answered_with_the_ticket_and_when_it_expires() {
    let mut d = Driver::new();
    let (start, _) = d.start_at(token(1), 30);
    let commands = d.peers(PeersEvent::PairingOpen {
        id: PairingId::new(1),
        ticket: ticket("t1"),
    });
    assert_eq!(
        commands,
        vec![reply(
            start,
            HostResult::PairStart(PairStartOk {
                ticket: "t1".into(),
                expires: t(30 + PAIRING_TTL.as_secs()),
            })
        )]
    );
}

#[test]
fn a_pairing_expires_ten_minutes_after_it_starts() {
    assert_eq!(PAIRING_TTL, jiff::SignedDuration::from_mins(10));
    let mut d = Driver::new();
    let waiter = d.open_and_wait();
    assert_eq!(d.peering.deadline(), Some(t(600).instant()));
    assert_eq!(d.handle(PeeringEvent::Tick { at: t(599) }), vec![]);
    assert_eq!(
        d.handle(PeeringEvent::Tick { at: t(600) }),
        vec![
            peers(PeersCommand::StopPairing),
            reply(waiter, HostResult::PairWait(PairOutcome::Expired)),
            peers(PeersCommand::Unbind),
        ]
    );
    assert_eq!(d.peering.deadline(), None);
}

#[test]
fn a_matching_attempt_trusts_the_peer_answers_the_waiter_and_keeps_the_endpoint_bound() {
    let mut d = Driver::new();
    let waiter = d.open_and_wait();
    let commands = d.peers(PeersEvent::PairRequested {
        id: PairingId::new(1),
        peer: m("bb"),
        name: "laptop".into(),
        accepted: true,
    });
    assert_eq!(
        appends(&commands),
        vec![(
            "_shepherd/peer/trusted".to_owned(),
            json!({ "node": "bb", "name": "laptop" })
        )]
    );
    assert_eq!(
        not_appends(commands),
        vec![
            reply(
                waiter,
                HostResult::PairWait(PairOutcome::Paired {
                    peer: info("bb", "laptop", false)
                })
            ),
            peers(PeersCommand::SetTrusted(set(&["bb"]))),
        ]
    );
    assert_eq!(d.list(), vec![info("bb", "laptop", false)]);
    assert_eq!(d.peering.deadline(), None, "the pairing is used up");
}

#[test]
fn a_wrong_token_uses_the_pairing_up_and_a_host_with_no_peers_unbinds() {
    let mut d = Driver::new();
    let waiter = d.open_and_wait();
    let commands = d.peers(PeersEvent::PairRequested {
        id: PairingId::new(1),
        peer: m("cc"),
        name: "stranger".into(),
        accepted: false,
    });
    assert_eq!(
        commands,
        vec![
            reply(
                waiter,
                HostResult::PairWait(PairOutcome::Refused {
                    peer: m("cc"),
                    name: "stranger".into(),
                })
            ),
            peers(PeersCommand::Unbind),
        ]
    );
    assert_eq!(d.list(), vec![]);
    let late = d.reply_to();
    assert!(matches!(
        d.handle(PeeringEvent::Wait { reply: late }).as_slice(),
        [HostCommand::Reply {
            result: Err(WireError::InvalidParams(_)),
            ..
        }]
    ));
}

#[test]
fn waiting_with_no_pairing_open_is_answered_at_once() {
    let mut d = Driver::new();
    let waiter = d.reply_to();
    assert!(matches!(
        d.handle(PeeringEvent::Wait { reply: waiter }).as_slice(),
        [HostCommand::Reply { to, result: Err(WireError::InvalidParams(_)) }] if *to == waiter
    ));
}

#[test]
fn a_new_pairing_replaces_the_open_one_and_events_about_the_old_one_change_nothing() {
    let mut d = Driver::new();
    let waiter = d.open_and_wait();
    let (start, commands) = d.start_at(token(2), 60);
    assert_eq!(
        commands,
        vec![
            reply(waiter, HostResult::PairWait(PairOutcome::Replaced)),
            peers(PeersCommand::AcceptPairing {
                id: PairingId::new(2),
                token: token(2),
                name: "desk".into(),
            }),
        ]
    );
    // A late ticket for the old one doesn't answer the new start.
    let stale = d.peers(PeersEvent::PairingOpen {
        id: PairingId::new(1),
        ticket: ticket("t1"),
    });
    assert_eq!(stale, vec![]);
    // Nor does a refused attempt on the old one use up the new one.
    let stale = d.peers(PeersEvent::PairRequested {
        id: PairingId::new(1),
        peer: m("cc"),
        name: "x".into(),
        accepted: false,
    });
    assert_eq!(stale, vec![]);
    assert_eq!(d.peering.deadline(), Some(t(660).instant()));
    let commands = d.peers(PeersEvent::PairingOpen {
        id: PairingId::new(2),
        ticket: ticket("t2"),
    });
    assert_eq!(
        commands,
        vec![reply(
            start,
            HostResult::PairStart(PairStartOk {
                ticket: "t2".into(),
                expires: t(660),
            })
        )]
    );
}

#[test]
fn a_start_replaced_before_its_ticket_came_is_answered_with_an_error() {
    let mut d = Driver::new();
    let (first, _) = d.start();
    let (_, commands) = d.start_at(token(2), 1);
    assert!(
        matches!(&commands[0], HostCommand::Reply { to, result: Err(WireError::InvalidParams(_)) } if *to == first),
        "{commands:?}"
    );
}

#[test]
fn an_attempt_that_matched_an_older_pairing_still_trusts_the_peer() {
    // The adapter checked the token of a pairing core had replaced a moment before: the user did hand that ticket out.
    let mut d = Driver::new();
    d.start();
    d.start_at(token(2), 1);
    let commands = d.peers(PeersEvent::PairRequested {
        id: PairingId::new(1),
        peer: m("bb"),
        name: "laptop".into(),
        accepted: true,
    });
    assert_eq!(
        appends(&commands),
        vec![(
            "_shepherd/peer/trusted".to_owned(),
            json!({ "node": "bb", "name": "laptop" })
        )]
    );
    assert_eq!(
        d.peering.deadline(),
        Some(t(601).instant()),
        "the newer pairing stays open"
    );
}

#[test]
fn a_pairing_that_cannot_open_answers_the_start_with_why_and_unbinds() {
    let mut d = Driver::new();
    let (start, _) = d.start();
    let commands = d.peers(PeersEvent::PairingFailed {
        id: PairingId::new(1),
        error: PairingError::Unbound,
    });
    assert_eq!(
        commands,
        vec![
            HostCommand::Reply {
                to: start,
                result: Err(WireError::Internal(
                    "cannot open a pairing: the network endpoint isn't bound".into()
                )),
            },
            peers(PeersCommand::Unbind),
        ]
    );
    assert_eq!(d.peering.deadline(), None);
}

#[test]
fn a_peers_claimed_name_is_made_display_safe_and_short_before_it_is_recorded() {
    let mut d = Driver::new();
    d.open_and_wait();
    let long = format!("lap\u{1b}[31mtop\u{202e} {}", "x".repeat(200));
    let commands = d.peers(PeersEvent::PairRequested {
        id: PairingId::new(1),
        peer: m("bb"),
        name: long,
        accepted: true,
    });
    let (_, params) = appends(&commands).remove(0);
    let name = params["name"].as_str().unwrap_or_default().to_owned();
    assert!(name.starts_with("lap[31mtop x"), "{name:?}");
    assert!(
        !name.contains('\u{1b}') && !name.contains('\u{202e}'),
        "{name:?}"
    );
    assert_eq!(name.chars().count(), shepherd_core::machine::MAX_NAME);
}

#[test]
fn a_peer_with_no_usable_name_is_recorded_as_unnamed() {
    let mut d = Driver::new();
    d.open_and_wait();
    let commands = d.peers(PeersEvent::PairRequested {
        id: PairingId::new(1),
        peer: m("bb"),
        name: " \u{200b}\u{1b} ".into(),
        accepted: true,
    });
    let (_, params) = appends(&commands).remove(0);
    assert_eq!(params["name"], shepherd_core::machine::UNNAMED_PEER);
}

// ---------- the joining side ----------

#[test]
fn joining_binds_then_dials_the_ticket_saying_this_machines_name() {
    let mut d = Driver::new();
    let to = d.reply_to();
    let commands = d.handle(PeeringEvent::Join {
        reply: to,
        ticket: ticket("t9"),
    });
    assert_eq!(
        commands,
        vec![
            peers(PeersCommand::Bind),
            peers(PeersCommand::Join {
                id: PairingId::new(1),
                ticket: ticket("t9"),
                name: "desk".into(),
            }),
        ]
    );
}

#[test]
fn a_join_that_works_trusts_the_inviter_and_answers_it() {
    let mut d = Driver::new();
    let to = d.reply_to();
    d.handle(PeeringEvent::Join {
        reply: to,
        ticket: ticket("t9"),
    });
    let commands = d.peers(PeersEvent::Paired {
        id: PairingId::new(1),
        peer: m("aa"),
        name: "office".into(),
    });
    assert_eq!(
        appends(&commands),
        vec![(
            "_shepherd/peer/trusted".to_owned(),
            json!({ "node": "aa", "name": "office" })
        )]
    );
    assert_eq!(
        not_appends(commands),
        vec![
            reply(to, HostResult::PairJoin(info("aa", "office", false))),
            peers(PeersCommand::SetTrusted(set(&["aa"]))),
        ]
    );
}

#[test]
fn a_join_that_fails_answers_why_and_a_host_with_no_peers_unbinds() {
    let mut d = Driver::new();
    let to = d.reply_to();
    d.handle(PeeringEvent::Join {
        reply: to,
        ticket: ticket("nope"),
    });
    let commands = d.peers(PeersEvent::JoinFailed {
        id: PairingId::new(1),
        error: JoinError::BadTicket,
    });
    assert_eq!(
        commands,
        vec![
            HostCommand::Reply {
                to,
                result: Err(WireError::InvalidParams(JoinError::BadTicket.to_string())),
            },
            peers(PeersCommand::Unbind),
        ]
    );
}

#[test]
fn a_join_failing_while_a_pairing_is_open_keeps_the_endpoint_bound() {
    let mut d = Driver::new();
    d.open_and_wait();
    let to = d.reply_to();
    let commands = d.handle(PeeringEvent::Join {
        reply: to,
        ticket: ticket("t9"),
    });
    assert_eq!(
        commands,
        vec![peers(PeersCommand::Join {
            id: PairingId::new(2),
            ticket: ticket("t9"),
            name: "desk".into(),
        })],
        "already bound"
    );
    let commands = d.peers(PeersEvent::JoinFailed {
        id: PairingId::new(2),
        error: JoinError::Refused,
    });
    assert_eq!(commands.len(), 1, "only the answer: {commands:?}");
}

// ---------- trusted peers ----------

#[test]
fn unpairing_by_name_records_the_removal_untrusts_it_and_unbinds_when_none_is_left() {
    let mut d = Driver::trusting(&[("bb", "laptop")]);
    d.peering.sync();
    let to = d.reply_to();
    let commands = d.handle(PeeringEvent::Remove {
        reply: to,
        peer: "laptop".into(),
    });
    assert_eq!(
        appends(&commands),
        vec![("_shepherd/peer/removed".to_owned(), json!({ "node": "bb" }))]
    );
    assert_eq!(
        not_appends(commands),
        vec![
            reply(to, HostResult::PeerRemove(info("bb", "laptop", false))),
            peers(PeersCommand::SetTrusted(set(&[]))),
            peers(PeersCommand::Unbind),
        ]
    );
    assert_eq!(d.list(), vec![]);
}

#[test]
fn unpairing_by_id_keeps_the_others_trusted_and_the_endpoint_bound() {
    let mut d = Driver::trusting(&[("bb", "laptop"), ("cc", "tower")]);
    d.peering.sync();
    let to = d.reply_to();
    let commands = d.handle(PeeringEvent::Remove {
        reply: to,
        peer: "cc".into(),
    });
    assert_eq!(
        not_appends(commands),
        vec![
            reply(to, HostResult::PeerRemove(info("cc", "tower", false))),
            peers(PeersCommand::SetTrusted(set(&["bb"]))),
        ]
    );
}

#[test]
fn unpairing_a_name_no_peer_has_or_two_peers_share_is_refused() {
    let mut d = Driver::trusting(&[("bb", "laptop"), ("cc", "laptop")]);
    d.peering.sync();
    for who in ["nobody", "laptop"] {
        let to = d.reply_to();
        let commands = d.handle(PeeringEvent::Remove {
            reply: to,
            peer: who.into(),
        });
        assert!(
            matches!(
                commands.as_slice(),
                [HostCommand::Reply {
                    result: Err(WireError::InvalidParams(_)),
                    ..
                }]
            ),
            "{who}: {commands:?}"
        );
    }
    assert_eq!(d.list().len(), 2);
}

#[test]
fn a_removed_peer_paired_again_is_trusted_again_under_its_new_name() {
    let mut d = Driver::trusting(&[("bb", "laptop")]);
    d.record(ShepherdFrame::PeerRemoved(
        shepherd_core::acpp::PeerRemoved { node: m("bb") },
    ));
    d.record(ShepherdFrame::PeerTrusted(PeerTrusted {
        node: m("bb"),
        name: "laptop2".into(),
    }));
    d.peering.sync();
    assert_eq!(d.list(), vec![info("bb", "laptop2", false)]);
}

#[test]
fn the_list_is_by_name_and_says_which_peers_are_connected() {
    let mut d = Driver::trusting(&[("cc", "tower"), ("bb", "laptop")]);
    d.peering.sync();
    d.peers(PeersEvent::Connected { peer: m("cc") });
    assert_eq!(
        d.list(),
        vec![info("bb", "laptop", false), info("cc", "tower", true)]
    );
    d.peers(PeersEvent::Disconnected { peer: m("cc") });
    assert_eq!(d.list()[1], info("cc", "tower", false));
}

#[test]
fn refusals_and_failed_dials_change_nothing() {
    let mut d = Driver::trusting(&[("bb", "laptop")]);
    d.peering.sync();
    assert_eq!(d.peers(PeersEvent::Refused { peer: m("zz") }), vec![]);
    assert_eq!(
        d.peers(PeersEvent::ConnectFailed {
            peer: m("bb"),
            error: ConnectError::Refused,
        }),
        vec![]
    );
    assert_eq!(d.list(), vec![info("bb", "laptop", false)]);
}

#[test]
fn a_connection_from_a_machine_not_trusted_yet_is_not_counted_once_it_is() {
    let mut d = Driver::trusting(&[("bb", "laptop")]);
    d.peering.sync();
    d.peers(PeersEvent::Connected { peer: m("cc") });
    d.record(ShepherdFrame::PeerTrusted(PeerTrusted {
        node: m("cc"),
        name: "tower".into(),
    }));
    d.peering.sync();
    assert_eq!(
        d.list(),
        vec![info("bb", "laptop", false), info("cc", "tower", false)]
    );
}

#[test]
fn a_paired_answer_to_a_join_this_host_never_asked_for_trusts_nobody() {
    let mut d = Driver::new();
    let commands = d.peers(PeersEvent::Paired {
        id: PairingId::new(7),
        peer: m("aa"),
        name: "office".into(),
    });
    assert_eq!(commands, vec![]);
    assert_eq!(d.list(), vec![]);
}

#[test]
fn a_pairing_is_stopped_once_every_client_waiting_on_it_has_gone() {
    let mut d = Driver::new();
    let (start, _) = d.start();
    d.peers(PeersEvent::PairingOpen {
        id: PairingId::new(1),
        ticket: ticket("t1"),
    });
    let waiter = d.reply_to();
    d.handle(PeeringEvent::Wait { reply: waiter });
    let other = d.reply_to();
    d.handle(PeeringEvent::Wait { reply: other });
    // A reply this pairing doesn't hold changes nothing.
    let stranger = d.reply_to();
    assert_eq!(d.handle(PeeringEvent::Gone { reply: stranger }), vec![]);
    assert_eq!(d.handle(PeeringEvent::Gone { reply: start }), vec![]);
    assert_eq!(d.handle(PeeringEvent::Gone { reply: waiter }), vec![]);
    assert_eq!(
        d.handle(PeeringEvent::Gone { reply: other }),
        vec![
            peers(PeersCommand::StopPairing),
            peers(PeersCommand::Unbind)
        ]
    );
    assert_eq!(d.peering.deadline(), None);
}

#[test]
fn another_client_going_while_nobody_waits_yet_leaves_the_pairing_open() {
    let mut d = Driver::new();
    d.start();
    d.peers(PeersEvent::PairingOpen {
        id: PairingId::new(1),
        ticket: ticket("t1"),
    });
    // The start is answered and its wait not sent yet: some other client's request goes with its conn.
    let other = d.reply_to();
    assert_eq!(d.handle(PeeringEvent::Gone { reply: other }), vec![]);
    assert_eq!(d.peering.deadline(), Some(t(600).instant()));
}

#[test]
fn a_start_whose_client_left_before_its_ticket_stops_the_pairing() {
    let mut d = Driver::new();
    let (start, _) = d.start();
    assert_eq!(
        d.handle(PeeringEvent::Gone { reply: start }),
        vec![
            peers(PeersCommand::StopPairing),
            peers(PeersCommand::Unbind)
        ]
    );
    // Its ticket, arriving late, answers nobody.
    let late = d.peers(PeersEvent::PairingOpen {
        id: PairingId::new(1),
        ticket: ticket("t1"),
    });
    assert_eq!(late, vec![]);
}

#[test]
fn trust_is_only_read_from_the_host_log() {
    // A `peer/trusted` in a session's log (which a host never writes) trusts nobody.
    let mut d = Driver::new();
    let frame = ShepherdFrame::PeerTrusted(PeerTrusted {
        node: m("bb"),
        name: "laptop".into(),
    });
    let env = d.host.append(
        Some("s_1"),
        Source::new(SourceKind::Shepherd, None),
        serde_json::to_value(frame.to_frame()).unwrap_or_else(|e| panic!("{e}")),
    );
    d.peering.apply(&env);
    assert_eq!(d.peering.sync(), vec![]);
}
