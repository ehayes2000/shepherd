//! `shepherd answer` (T51): answer the permission prompt or question a session's agent is blocked on, without
//! attaching. Shape in `docs/cli.md`.
//!
//! The host types the agent's own keys for the choice (`_shepherd/session/answer`), and only while that question is
//! still pending as the host sees it: a question the agent moved on from, or the user is answering in the pane, is
//! refused with the session's status, never typed. The answer names the question it means (the seq of its
//! `_shepherd/agent/asked` entry, from this command's own fold, or `--asked`), so it can't land on a later one.

use std::io::Write;

use serde::Serialize;
use shepherd_core::acpp::{Choice, PromptBy, Seq};
use shepherd_core::client::AskError;
use shepherd_core::wire::{ClientRequest, SessionAnswer, WireError};

use super::{Args, CliError, Drive, ask_host, safe, settle, target, write, write_json};

const USAGE: &str =
    "usage: shepherd answer <session> <yes | always | no | N> [--asked SEQ] [--json]";

/// What `shepherd answer` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The session, by id or name.
    pub session: String,
    pub choice: Choice,
    /// The question meant, by its `agent/asked` entry's seq; `None`: the one this command sees pending.
    pub asked: Option<Seq>,
}

/// `{ session, choice, asked, question }` (docs/cli.md).
#[derive(Serialize)]
struct Answered {
    session: String,
    choice: Choice,
    asked: Option<String>,
    question: Option<Seq>,
}

/// `shepherd answer`: has the session's host answer its agent's pending question with `answer.choice`.
pub fn answer(
    drive: &mut impl Drive,
    answer: &Answer,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    settle(drive)?;
    let session = target(drive, &answer.session)?;
    let (id, name) = (session.id.clone(), session.name.clone());
    // The question as this client has folded it; the host decides whether it is still the one pending.
    let state = drive.client().state(&id);
    let asked = state.and_then(|s| s.question()).map(|q| q.text.clone());
    let question = answer.asked.or(state.and_then(|s| s.question_seq()));
    let body = ClientRequest::SessionAnswer(SessionAnswer {
        session: id.clone(),
        choice: answer.choice,
        asked: question,
        by: Some(PromptBy::Cli),
    });
    match ask_host(drive, Some(session.machine), body)? {
        Ok(_) => {}
        Err(AskError::Refused(WireError::NothingAsked { status, .. })) => {
            return Err(CliError::Failed(format!(
                "{} has nothing to answer ({status}): nothing was typed",
                safe(&name)
            )));
        }
        Err(AskError::Refused(WireError::InvalidParams(why))) => {
            return Err(CliError::Failed(safe(&why)));
        }
        Err(e) => return Err(e.to_string().into()),
    }
    let choice = answer.choice;
    if json {
        let answered = Answered {
            session: id.to_string(),
            choice,
            asked,
            question,
        };
        return Ok(write_json(out, &answered)?);
    }
    let text = match asked {
        Some(asked) => format!("answered {} {choice}: {}\n", safe(&name), safe(&asked)),
        None => format!("answered {} {choice}\n", safe(&name)),
    };
    Ok(write(out, &text)?)
}

fn answer_args(args: &[String]) -> Result<(Answer, bool), String> {
    let parsed = Args::parse(args, &["--asked"], &["--json"]).ok_or(USAGE)?;
    let [session, choice] = parsed.positional.as_slice() else {
        return Err(USAGE.to_owned());
    };
    let choice = choice.parse::<Choice>().map_err(|why| safe(&why))?;
    let asked = parsed
        .value("--asked")
        .map(|seq| {
            seq.parse::<u64>().map(Seq::new).map_err(|_| {
                format!(
                    "--asked {}: give the seq of the question's entry",
                    safe(seq)
                )
            })
        })
        .transpose()?;
    let answer = Answer {
        session: session.clone(),
        choice,
        asked,
    };
    Ok((answer, parsed.switch("--json")))
}

/// `shepherd answer <args>`.
pub fn answer_main(args: &[String]) -> Result<(), CliError> {
    let (asked, json) = answer_args(args)?;
    answer(&mut super::connect()?, &asked, json, &mut std::io::stdout())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn answer_takes_a_session_a_word_or_an_options_number_and_maybe_the_question() {
        let answer = |session: &str, choice, asked| Answer {
            session: session.to_owned(),
            choice,
            asked,
        };
        assert_eq!(
            answer_args(&args(&["api", "yes"])),
            Ok((answer("api", Choice::Yes, None), false))
        );
        assert_eq!(
            answer_args(&args(&["api", "2", "--asked", "17", "--json"])),
            Ok((
                answer(
                    "api",
                    Choice::Option(NonZeroU32::new(2).unwrap()),
                    Some(Seq::new(17))
                ),
                true
            ))
        );
        assert!(answer_args(&args(&["api", "0"])).is_err());
        assert!(answer_args(&args(&["api", "maybe"])).is_err());
        assert!(answer_args(&args(&["api", "yes", "--asked", "soon"])).is_err());
        assert!(answer_args(&args(&["api"])).is_err());
    }
}
