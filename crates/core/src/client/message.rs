//! The status page's message line (T59): text the user types, sent as a prompt (`_shepherd/session/prompt`, `by:
//! client`) to the status table's selected agent, or to every eligible agent of a group ([`MessageTarget`]).
//!
//! Sending resolves the target to its sessions at that moment ([`Client::message_recipients`]). A session that can't be
//! prompted is skipped, saying why ([`Skip`]); each other is prompted through its own host, and the host's answer says
//! whether it was typed in or refused ([`Refusal`]: busy in a turn, waiting on the user, or the host's reason). The
//! last send is kept as a [`Report`] for the line to show; a link that drops before answering refuses what it hadn't
//! answered. What is typed, whether the line takes the keys, the target and the report are this window's own.

use super::status::Group;
use super::{At, Client, ClientCommand, refusal};
use crate::acpp::{PromptBy, RequestId};
use crate::config::AgentKind;
use crate::ids::{MachineId, SessionId};
use crate::wire::{Busy, ClientRequest, HostResult, SessionPrompt, WireError};

/// Who the message line sends to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MessageTarget {
    /// The status table's selected session.
    #[default]
    Selected,
    /// Every session working (in a turn, starting, or running no agent): see [`Group::Working`].
    Working,
    /// Every session whose agent is at its prompt.
    Idle,
    /// Every session on this machine.
    Machine(MachineId),
}

/// How sending to one session went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Asked of its host, not answered yet.
    Sending,
    /// Its host typed it in.
    Sent,
    Refused(Refusal),
    /// Not sent: it can't be prompted.
    Skipped(Skip),
}

/// Why a host refused a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The agent isn't at its prompt: in a turn ([`Busy::Running`]) or waiting on the user ([`Busy::WantsUser`]).
    Busy(Busy),
    /// The host's reason, or that its link dropped before it answered.
    Other(String),
}

/// Why a session wasn't sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Its process ended.
    Exited,
    /// It runs no agent to prompt.
    NoAgent,
    /// Its host can't be reached now.
    Offline,
}

/// One session sent to (or skipped) by the last send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub session: SessionId,
    /// What it was called when sent to.
    pub name: String,
    pub outcome: Outcome,
    /// The request asking it, while unanswered, and the host it went to.
    asked: Option<(At, RequestId)>,
}

/// The last send: to whom, and how it went for each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub target: MessageTarget,
    /// Each session the target had, in the sidebar's order.
    pub deliveries: Vec<Delivery>,
}

impl Report {
    /// Whether every host sent to has answered.
    pub fn done(&self) -> bool {
        !self
            .deliveries
            .iter()
            .any(|d| d.outcome == Outcome::Sending)
    }
}

/// The message line's state, this window's own.
#[derive(Debug, Clone, Default)]
pub(super) struct MessageLine {
    /// It takes the keys.
    composing: bool,
    text: String,
    target: MessageTarget,
    report: Option<Report>,
}

impl Client {
    /// Whether the message line takes the keys (it was clicked).
    pub fn composing(&self) -> bool {
        self.message.composing
    }

    /// What is typed in the message line.
    pub fn message_text(&self) -> &str {
        &self.message.text
    }

    /// Who the message line sends to.
    pub fn message_target(&self) -> &MessageTarget {
        &self.message.target
    }

    /// The targets `tab` (or a click on the target) goes through, in order: the selected session, all working, all
    /// idle, then each machine in the sidebar's order.
    pub fn message_targets(&self) -> Vec<MessageTarget> {
        let mut targets = vec![
            MessageTarget::Selected,
            MessageTarget::Working,
            MessageTarget::Idle,
        ];
        targets.extend(
            self.machines()
                .into_iter()
                .map(|m| MessageTarget::Machine(m.id)),
        );
        targets
    }

    /// The target after the current one, wrapping round to the selected session.
    pub fn next_message_target(&self) -> MessageTarget {
        let targets = self.message_targets();
        targets
            .iter()
            .position(|t| t == &self.message.target)
            .and_then(|i| targets.get(i + 1))
            .cloned()
            .unwrap_or_default()
    }

    /// The sessions the current target has now, in the sidebar's order.
    pub fn message_recipients(&self) -> Vec<SessionId> {
        match &self.message.target {
            MessageTarget::Selected => self.status_selected().cloned().into_iter().collect(),
            MessageTarget::Working => self.in_group(Group::Working),
            MessageTarget::Idle => self.in_group(Group::Idle),
            MessageTarget::Machine(machine) => self
                .sessions
                .iter()
                .filter(|s| &s.machine == machine)
                .map(|s| s.id.clone())
                .collect(),
        }
    }

    /// How the last send went, until the next.
    pub fn message_report(&self) -> Option<&Report> {
        self.message.report.as_ref()
    }

    fn in_group(&self, group: Group) -> Vec<SessionId> {
        self.sessions
            .iter()
            .filter(|s| Group::of(self, s) == group)
            .map(|s| s.id.clone())
            .collect()
    }

    pub(super) fn compose(&mut self, on: bool) -> Vec<ClientCommand> {
        self.message.composing = on;
        Vec::new()
    }

    pub(super) fn edit_message(&mut self, text: String) -> Vec<ClientCommand> {
        self.message.text = text;
        Vec::new()
    }

    pub(super) fn set_message_target(&mut self, target: MessageTarget) -> Vec<ClientCommand> {
        if self.message_targets().contains(&target) {
            self.message.target = target;
        }
        Vec::new()
    }

    /// Sends what is typed to each session the target has now. Blank text sends nothing. The text is cleared once
    /// anything was asked of a host.
    pub(super) fn send_message(&mut self) -> Vec<ClientCommand> {
        let text = self.message.text.trim().to_owned();
        if text.is_empty() {
            return Vec::new();
        }
        let mut commands = Vec::new();
        let mut deliveries = Vec::new();
        for id in self.message_recipients() {
            let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
                continue;
            };
            let name = session.name.clone();
            let home = self.home(&id);
            let skip = if session.exited {
                Some(Skip::Exited)
            } else if !matches!(
                session.agent,
                Some(AgentKind::ClaudeCode | AgentKind::Codex)
            ) {
                Some(Skip::NoAgent)
            } else if self.offline(&id) || !self.ready(&home) {
                Some(Skip::Offline)
            } else {
                None
            };
            let (outcome, asked) = match skip {
                Some(skip) => (Outcome::Skipped(skip), None),
                None => {
                    let prompt = ClientRequest::SessionPrompt(SessionPrompt {
                        session: id.clone(),
                        text: text.clone(),
                        by: Some(PromptBy::Client),
                    });
                    let (request, command) = self.request_to(&home, prompt);
                    commands.push(command);
                    (Outcome::Sending, Some((home, request)))
                }
            };
            deliveries.push(Delivery {
                session: id,
                name,
                outcome,
                asked,
            });
        }
        if !commands.is_empty() {
            self.message.text.clear();
        }
        self.message.report = Some(Report {
            target: self.message.target.clone(),
            deliveries,
        });
        commands
    }

    /// Whether request `id` is a prompt the last send is waiting on.
    pub(super) fn message_asked(&self, id: &RequestId) -> bool {
        self.message.report.as_ref().is_some_and(|r| {
            r.deliveries
                .iter()
                .any(|d| d.asked.as_ref().is_some_and(|(_, asked)| asked == id))
        })
    }

    /// A host answered prompt `id` of the last send.
    pub(super) fn message_answered(
        &mut self,
        id: &RequestId,
        result: Result<HostResult, WireError>,
    ) {
        let Some(report) = &mut self.message.report else {
            return;
        };
        let Some(delivery) = report
            .deliveries
            .iter_mut()
            .find(|d| d.asked.as_ref().is_some_and(|(_, asked)| asked == id))
        else {
            return;
        };
        delivery.asked = None;
        delivery.outcome = match result {
            Ok(_) => Outcome::Sent,
            Err(WireError::SessionBusy { status, .. }) => Outcome::Refused(Refusal::Busy(status)),
            Err(e) => Outcome::Refused(Refusal::Other(refusal(e))),
        };
    }

    /// Host `at`'s link dropped: what it hadn't answered of the last send may or may not have been typed in.
    pub(super) fn message_dropped(&mut self, at: &At, machine: &str) {
        let Some(report) = &mut self.message.report else {
            return;
        };
        for delivery in &mut report.deliveries {
            if delivery
                .asked
                .as_ref()
                .is_some_and(|(asked, _)| asked == at)
            {
                delivery.asked = None;
                delivery.outcome = Outcome::Refused(Refusal::Other(format!(
                    "the connection to {machine} dropped before it answered"
                )));
            }
        }
    }
}
