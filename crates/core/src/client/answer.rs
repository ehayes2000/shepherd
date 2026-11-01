//! Answering in place (T58): the status page's preview shows the question the table's selected session's agent asks
//! ([`Client::status_asked`]) with a button for each answer, and a click (or `y`/`a`/`n`, or an option's number) answers
//! it through `_shepherd/session/answer`, `by: client`, naming the question by its seq so the answer never lands on a
//! later one.
//!
//! A button is offered for what the question takes: `yes`, `always` and `no` for a permission, each option then `no`
//! for a question. Each says whether the host would refuse it ([`Button::refused`]), decided by the very keys the host
//! would type ([`crate::host::answer::keys`]), so `always` is refused where the agent's dialog doesn't offer it. A
//! question shepherd may not answer (the user typed into it in the pane, or another dialog may still be in front of it)
//! or whose machine can't be reached is shown without buttons ([`Answers::Unanswerable`]).
//!
//! The last answer is kept as an [`AnswerReport`]: sent, answered, or refused and why. An answer to a question no longer
//! pending is refused here, asking nothing ([`AnswerRefusal::AlreadyAnswered`]), as is one the host refuses as nothing
//! asked; one to a question the user is answering in the pane meanwhile is refused as [`AnswerRefusal::InPane`]. The
//! report is this window's own.

use super::{At, Client, ClientCommand, refusal};
use crate::acpp::{AgentAsked, BlockKind, Choice, PromptBy, RequestId, Seq};
use crate::config::AgentKind;
use crate::host::answer::keys;
use crate::ids::SessionId;
use crate::wire::{ClientRequest, HostResult, SessionAnswer, WireError};

/// What a session's agent asks the user, for the preview.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked<'a> {
    pub session: &'a SessionId,
    /// What it asks, if it said: the agent's text, raw. Display it only through `display_safe`.
    pub text: Option<&'a str>,
    pub answers: Answers<'a>,
}

/// Whether the preview can answer it.
#[derive(Debug, Clone, PartialEq)]
pub enum Answers<'a> {
    /// It can: the seq of the question (what an answer names) and a button for each answer it takes.
    Open {
        asked: Seq,
        buttons: Vec<Button<'a>>,
    },
    /// It can't: answer it in the session's pane.
    Unanswerable(Unanswerable),
}

/// Why a question can't be answered from the preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unanswerable {
    /// Its machine can't be reached now.
    Offline,
    /// It isn't shepherd's to answer: the user typed into it in the pane, another dialog may still be up in front of
    /// it, or its agent didn't say what it asks.
    InPane,
}

/// An answer the preview offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button<'a> {
    pub choice: Choice,
    /// `yes`, `always`, `no`, or the option's text (the agent's, raw).
    pub label: &'a str,
    /// Why the host would refuse it, if it would: the button is shown disabled.
    pub refused: Option<String>,
}

/// How the last answer went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerReport {
    pub session: SessionId,
    /// The question it answered, by its seq.
    pub asked: Seq,
    pub choice: Choice,
    pub outcome: AnswerOutcome,
    /// The request asking it, while unanswered, and the host it went to.
    request: Option<(At, RequestId)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// Asked of its host, not answered yet.
    Sending,
    /// Its host typed it in.
    Answered,
    Refused(AnswerRefusal),
}

/// Why an answer wasn't typed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerRefusal {
    /// The question was answered (or moved on from) before this answer.
    AlreadyAnswered,
    /// The question is being answered in the session's pane (the user typed into it), or isn't shepherd's to answer
    /// for another reason: attach to answer it.
    InPane,
    /// The question doesn't take this answer, and why.
    NotOffered(String),
    /// The host's reason, or that its machine can't be reached or its link dropped before it answered.
    Other(String),
}

impl Client {
    /// What the status page's selected session's agent asks the user, while it waits on them.
    pub fn status_asked(&self) -> Option<Asked<'_>> {
        self.asked_of(self.status_selected()?)
    }

    /// How the last answer from the preview went, until the next.
    pub fn answer_report(&self) -> Option<&AnswerReport> {
        self.answer.as_ref()
    }

    /// What listed session `session`'s agent asks the user, while it (or a subagent of its) waits on them.
    pub fn asked_of<'a>(&'a self, session: &SessionId) -> Option<Asked<'a>> {
        let listed = self.sessions.iter().find(|s| &s.id == session)?;
        let state = self.state(session)?;
        if !state.wants_user() {
            return None;
        }
        let home = self.home(session);
        let answers = match (state.question(), state.question_seq(), listed.agent) {
            _ if self.offline(session) || !self.ready(&home) => {
                Answers::Unanswerable(Unanswerable::Offline)
            }
            (
                Some(question),
                Some(asked),
                Some(agent @ (AgentKind::ClaudeCode | AgentKind::Codex)),
            ) => Answers::Open {
                asked,
                buttons: buttons(agent, question),
            },
            _ => Answers::Unanswerable(Unanswerable::InPane),
        };
        Some(Asked {
            session: &listed.id,
            text: state.asked(),
            answers,
        })
    }

    /// Answers question `asked` of session `session` with `choice`, if it is still the one pending and takes it.
    pub(super) fn status_answer(
        &mut self,
        session: SessionId,
        asked: Seq,
        choice: Choice,
    ) -> Vec<ClientCommand> {
        if !self.listed(&session) {
            return Vec::new();
        }
        let sending = self.answer.as_ref().is_some_and(|r| {
            r.session == session && r.asked == asked && r.outcome == AnswerOutcome::Sending
        });
        if sending {
            return Vec::new();
        }
        let refused = match self.asked_of(&session).map(|a| a.answers) {
            Some(Answers::Open {
                asked: pending,
                buttons,
            }) if pending == asked => match buttons.into_iter().find(|b| b.choice == choice) {
                Some(Button { refused: None, .. }) => None,
                Some(Button {
                    refused: Some(why), ..
                }) => Some(AnswerRefusal::NotOffered(why)),
                None => Some(AnswerRefusal::NotOffered(format!(
                    "the question doesn't take {choice}"
                ))),
            },
            Some(Answers::Unanswerable(Unanswerable::Offline)) => {
                let machine = self
                    .sessions
                    .iter()
                    .find(|s| s.id == session)
                    .map(|s| self.machine_label(&s.machine).to_owned())
                    .unwrap_or_default();
                Some(AnswerRefusal::Other(format!(
                    "{machine} can't be reached now"
                )))
            }
            Some(Answers::Unanswerable(Unanswerable::InPane)) => Some(AnswerRefusal::InPane),
            _ => Some(AnswerRefusal::AlreadyAnswered),
        };
        let mut report = AnswerReport {
            session: session.clone(),
            asked,
            choice,
            outcome: AnswerOutcome::Sending,
            request: None,
        };
        let mut commands = Vec::new();
        match refused {
            Some(why) => report.outcome = AnswerOutcome::Refused(why),
            None => {
                let home = self.home(&session);
                let body = ClientRequest::SessionAnswer(SessionAnswer {
                    session,
                    choice,
                    asked: Some(asked),
                    by: Some(PromptBy::Client),
                });
                let (request, command) = self.request_to(&home, body);
                commands.push(command);
                report.request = Some((home, request));
            }
        }
        self.answer = Some(report);
        commands
    }

    /// Whether request `id` is the answer the last report waits on.
    pub(super) fn answer_asked(&self, id: &RequestId) -> bool {
        self.answer
            .as_ref()
            .and_then(|r| r.request.as_ref())
            .is_some_and(|(_, asked)| asked == id)
    }

    /// The host answered the last answer's request.
    pub(super) fn answer_answered(&mut self, result: Result<HostResult, WireError>) {
        let Some(report) = &mut self.answer else {
            return;
        };
        report.request = None;
        report.outcome = match result {
            Ok(_) => AnswerOutcome::Answered,
            Err(WireError::NothingAsked { .. }) => {
                AnswerOutcome::Refused(AnswerRefusal::AlreadyAnswered)
            }
            Err(e) => AnswerOutcome::Refused(AnswerRefusal::Other(refusal(e))),
        };
    }

    /// Host `at`'s link dropped: an answer it hadn't answered may or may not have been typed in.
    pub(super) fn answer_dropped(&mut self, at: &At, machine: &str) {
        let Some(report) = &mut self.answer else {
            return;
        };
        if report
            .request
            .as_ref()
            .is_some_and(|(asked, _)| asked == at)
        {
            report.request = None;
            report.outcome = AnswerOutcome::Refused(AnswerRefusal::Other(format!(
                "the connection to {machine} dropped before it answered"
            )));
        }
    }
}

/// The buttons for `question` asked by `agent`: `yes`, `always`, `no` for a permission; each option then `no` for a
/// question. Each is refused where the host would refuse it.
fn buttons(agent: AgentKind, question: &AgentAsked) -> Vec<Button<'_>> {
    let mut offered: Vec<(Choice, &str)> = Vec::new();
    match (&question.kind, &question.choices) {
        (BlockKind::Question, Some(choices)) => {
            offered.extend(choices.iter().zip(1u32..).filter_map(|(label, n)| {
                std::num::NonZeroU32::new(n).map(|n| (Choice::Option(n), label.as_str()))
            }));
        }
        (BlockKind::Question, None) => {}
        _ => {
            offered.push((Choice::Yes, "yes"));
            offered.push((Choice::Always, "always"));
        }
    }
    offered.push((Choice::No, "no"));
    offered
        .into_iter()
        .map(|(choice, label)| Button {
            choice,
            label,
            refused: keys(agent, question, choice).err(),
        })
        .collect()
}
