//! `shepherd answer` (T51): answer the permission prompt or question a session's agent is blocked on, without
//! attaching. Shape in `docs/cli.md`.
//!
//! The host types the agent's own keys for the choice (`_shepherd/session/answer`), and only while that question is
//! still pending as the host sees it: a question the agent moved on from, or the user is answering in the pane, is
//! refused with the session's status, never typed.

use std::io::Write;

use serde::Serialize;
use shepherd_core::acpp::{Choice, PromptBy};
use shepherd_core::client::AskError;
use shepherd_core::wire::{ClientRequest, SessionAnswer, WireError};

use super::{Args, CliError, Drive, ask_host, safe, settle, target, write, write_json};

const USAGE: &str = "usage: shepherd answer <session> <yes | always | no | N> [--json]";

/// `{ session, choice, asked }` (docs/cli.md).
#[derive(Serialize)]
struct Answered {
    session: String,
    choice: Choice,
    asked: Option<String>,
}

/// `shepherd answer`: has the session's host answer its agent's pending question with `choice`.
pub fn answer(
    drive: &mut impl Drive,
    session: &str,
    choice: Choice,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    settle(drive)?;
    let session = target(drive, session)?;
    let (id, name) = (session.id.clone(), session.name.clone());
    // What it asked, as this client has folded it: for the output only; the host decides.
    let asked = drive
        .client()
        .state(&id)
        .and_then(|s| s.question())
        .map(|q| q.text.clone());
    let body = ClientRequest::SessionAnswer(SessionAnswer {
        session: id.clone(),
        choice,
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
    if json {
        let answered = Answered {
            session: id.to_string(),
            choice,
            asked,
        };
        return Ok(write_json(out, &answered)?);
    }
    let text = match asked {
        Some(asked) => format!("answered {} {choice}: {}\n", safe(&name), safe(&asked)),
        None => format!("answered {} {choice}\n", safe(&name)),
    };
    Ok(write(out, &text)?)
}

fn answer_args(args: &[String]) -> Result<(String, Choice, bool), String> {
    let parsed = Args::parse(args, &[], &["--json"]).ok_or(USAGE)?;
    let [session, choice] = parsed.positional.as_slice() else {
        return Err(USAGE.to_owned());
    };
    let choice = choice.parse::<Choice>().map_err(|why| safe(&why))?;
    Ok((session.clone(), choice, parsed.switch("--json")))
}

/// `shepherd answer <args>`.
pub fn answer_main(args: &[String]) -> Result<(), CliError> {
    let (session, choice, json) = answer_args(args)?;
    answer(
        &mut super::connect()?,
        &session,
        choice,
        json,
        &mut std::io::stdout(),
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn answer_takes_a_session_and_a_word_or_an_options_number() {
        assert_eq!(
            answer_args(&args(&["api", "yes"])),
            Ok(("api".to_owned(), Choice::Yes, false))
        );
        assert_eq!(
            answer_args(&args(&["api", "2", "--json"])),
            Ok((
                "api".to_owned(),
                Choice::Option(NonZeroU32::new(2).unwrap()),
                true
            ))
        );
        assert!(answer_args(&args(&["api", "0"])).is_err());
        assert!(answer_args(&args(&["api", "maybe"])).is_err());
        assert!(answer_args(&args(&["api"])).is_err());
    }
}
