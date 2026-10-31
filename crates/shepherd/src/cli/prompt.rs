//! `shepherd prompt`, `shepherd reply` and `shepherd wait` (T47, decision 0022): prompt an agent and wait for its
//! answer, read its last reply, or wait for it to stop. Shapes in `docs/cli.md`.
//!
//! The host types the prompt (`_shepherd/session/prompt`), so every client sees it, and only while the agent is at
//! its prompt. Waiting is done here, on the client's own fold of the session's log: the prompt's
//! `_shepherd/session/prompted` entry starts the turn, and the turn is over once the agent is at its prompt again (its
//! reply is then read from its transcript by its host, `_shepherd/session/reply`), waits on the user, or exits.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serde::Serialize;
use shepherd_core::acpp::{PromptBy, Seq};
use shepherd_core::client::automation::SessionStatus;
use shepherd_core::client::{AskError, Client, ClientSession};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{ClientRequest, HostResult, SessionPrompt, SessionReply, WireError};

use super::{
    Args, CliError, Drive, WaitError, ask_host, request, safe, settle, target, write, write_json,
};

const PROMPT_USAGE: &str =
    "usage: shepherd prompt <session> <text | -> [--wait] [--queue] [--timeout T] [--json]";
const REPLY_USAGE: &str = "usage: shepherd reply <session> [--json]";
const WAIT_USAGE: &str = "usage: shepherd wait <session> [--timeout T] [--json]";

/// How long one wait without `--timeout` lasts before it is simply started again: waiting has no limit then.
const NO_LIMIT_STEP: Duration = Duration::from_secs(24 * 60 * 60);

/// What `shepherd prompt` was asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompt {
    /// The session, by id or name.
    pub session: String,
    pub text: String,
    /// Wait for the turn to end and print the reply.
    pub wait: bool,
    /// Wait for the agent to be at its prompt before sending, instead of being refused.
    pub queue: bool,
    /// How long `wait` and `queue` may wait. `None`: no limit.
    pub timeout: Option<Duration>,
}

/// `{ session, status, reply, asked }` (docs/cli.md).
#[derive(Serialize)]
struct Outcome {
    session: String,
    status: &'static str,
    reply: Option<String>,
    asked: Option<String>,
}

/// When a wait started, and how long it may take.
struct Budget {
    until: Option<Instant>,
}

impl Budget {
    fn new(timeout: Option<Duration>) -> Self {
        Self {
            until: timeout.map(|t| Instant::now() + t),
        }
    }

    /// Runs `drive` until `done`, within what is left: [`WaitError::TimedOut`] once it has run out.
    fn wait(
        &self,
        drive: &mut impl Drive,
        done: &mut dyn FnMut(&Client) -> bool,
    ) -> Result<(), WaitError> {
        loop {
            let left = match self.until {
                Some(until) => until.saturating_duration_since(Instant::now()),
                None => NO_LIMIT_STEP,
            };
            match drive.wait(done, left) {
                Err(WaitError::TimedOut) if self.until.is_none() => continue,
                other => return other,
            }
        }
    }
}

fn listed<'a>(client: &'a Client, id: &SessionId) -> Option<&'a ClientSession> {
    client.sessions().iter().find(|s| s.id == *id)
}

fn status_of(client: &Client, id: &SessionId) -> Option<SessionStatus> {
    listed(client, id).map(|s| SessionStatus::in_client(client, s))
}

/// The seq of the last prompt the host typed into session `id`, as the client has folded it.
fn told_seq(client: &Client, id: &SessionId) -> Option<Seq> {
    client.state(id).and_then(|s| s.told()).map(|t| t.seq)
}

/// What session `id`'s agent asked the user, if it is waiting on them for a permission.
fn asked(client: &Client, id: &SessionId) -> Option<String> {
    client.state(id).and_then(|s| s.asked()).map(str::to_owned)
}

/// The turn is over: the agent is at its prompt, waits on the user, or is gone.
fn stopped(status: Option<SessionStatus>) -> bool {
    !matches!(status, Some(SessionStatus::Running))
}

fn timed_out(name: &str, what: &str, timeout: Option<Duration>) -> CliError {
    let after = timeout.map_or_else(String::new, |t| format!(" after {}s", t.as_secs()));
    CliError::TimedOut(format!(
        "timed out{after} waiting for {} {what}",
        safe(name)
    ))
}

/// Asks `machine`'s host for `session`'s last reply written since `since`.
fn read_reply(
    drive: &mut impl Drive,
    machine: &MachineId,
    session: &SessionId,
    since: Option<jiff::Timestamp>,
) -> Result<Option<String>, CliError> {
    let body = ClientRequest::SessionReply(SessionReply {
        session: session.clone(),
        since,
    });
    match request(drive, Some(machine.clone()), body)? {
        HostResult::SessionReply(ok) => Ok(ok.reply.map(|r| r.text)),
        other => Err(format!("the host answered something else: {other:?}").into()),
    }
}

/// Prints `outcome`: as JSON, or (for a human, display-safe) the reply, or what the agent is waiting for.
fn print(out: &mut impl Write, outcome: &Outcome, name: &str, json: bool) -> Result<(), CliError> {
    if json {
        return Ok(write_json(out, outcome)?);
    }
    let text = match (outcome.status, &outcome.reply, &outcome.asked) {
        ("wants-user", _, Some(asked)) => {
            format!("{} wants the user: {}\n", safe(name), safe(asked))
        }
        ("wants-user", _, None) => format!("{} wants the user\n", safe(name)),
        (_, Some(reply), _) => format!("{}\n", safe_reply(reply)),
        (status, None, _) => format!("{} is {status}, with no reply\n", safe(name)),
    };
    Ok(write(out, &text)?)
}

/// A reply made safe to print, line by line (its newlines kept).
fn safe_reply(reply: &str) -> String {
    reply.lines().map(safe).collect::<Vec<_>>().join("\n")
}

/// `shepherd prompt`: has the session's host type `prompt.text` into its agent; with `wait`, waits for that turn to end
/// and prints the reply (or what the agent asked the user).
pub fn prompt(
    drive: &mut impl Drive,
    prompt: &Prompt,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    settle(drive)?;
    let session = target(drive, &prompt.session)?;
    let (id, machine, name) = (session.id, session.machine, session.name);
    let budget = Budget::new(prompt.timeout);
    let before = loop {
        if prompt.queue {
            let mut ready = |c: &Client| {
                matches!(
                    status_of(c, &id),
                    Some(SessionStatus::Idle | SessionStatus::Exited) | None
                )
            };
            budget.wait(drive, &mut ready).map_err(|e| match e {
                WaitError::TimedOut => timed_out(&name, "to be at its prompt", prompt.timeout),
                other => other.into(),
            })?;
        }
        let before = told_seq(drive.client(), &id);
        let body = ClientRequest::SessionPrompt(SessionPrompt {
            session: id.clone(),
            text: prompt.text.clone(),
            by: Some(PromptBy::Cli),
        });
        match ask_host(drive, Some(machine.clone()), body)? {
            Ok(_) => break before,
            // Someone else's prompt got there first: wait for that turn too.
            Err(AskError::Refused(WireError::SessionBusy { .. })) if prompt.queue => continue,
            Err(AskError::Refused(WireError::SessionBusy { status, .. })) => {
                return Err(CliError::Failed(format!(
                    "{} is busy ({status}): it was not prompted; --queue waits for it",
                    safe(&name)
                )));
            }
            Err(e) => return Err(e.to_string().into()),
        }
    };
    if !prompt.wait {
        if json {
            let outcome = Outcome {
                session: id.to_string(),
                status: SessionStatus::Running.as_str(),
                reply: None,
                asked: None,
            };
            return Ok(write_json(out, &outcome)?);
        }
        return Ok(write(out, &format!("prompted {}\n", safe(&name)))?);
    }
    // Its own `session/prompted` starts the turn: once the client has it, the agent stopping ends that turn.
    // A session closed meanwhile is no longer listed (nor is its log kept): that ends the turn too.
    let mut over = |c: &Client| {
        let told = told_seq(c, &id);
        listed(c, &id).is_none() || (told.is_some() && told != before && stopped(status_of(c, &id)))
    };
    budget.wait(drive, &mut over).map_err(|e| match e {
        WaitError::TimedOut => timed_out(
            &name,
            "to finish its turn; the prompt was sent and stays",
            prompt.timeout,
        ),
        other => other.into(),
    })?;
    let client = drive.client();
    let closed = listed(client, &id).is_none();
    let status = status_of(client, &id).unwrap_or(SessionStatus::Exited);
    // The turn started at its own `session/prompted`: only what the agent wrote since then is its reply.
    let since = client
        .state(&id)
        .and_then(|s| s.told())
        .map(|t| t.at.instant());
    let asked = asked(client, &id);
    let reply = match status {
        // Its host knows a closed session no more than the client does.
        _ if closed => None,
        SessionStatus::WantsUser => None,
        _ => read_reply(drive, &machine, &id, since)?,
    };
    let outcome = Outcome {
        session: id.to_string(),
        status: status.as_str(),
        reply,
        asked,
    };
    print(out, &outcome, &name, json)
}

/// `shepherd reply`: the session's agent's last reply, without prompting it.
pub fn reply(
    drive: &mut impl Drive,
    session: &str,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    settle(drive)?;
    let session = target(drive, session)?;
    let reply = read_reply(drive, &session.machine, &session.id, None)?;
    let client = drive.client();
    let status = status_of(client, &session.id).unwrap_or(SessionStatus::Exited);
    let outcome = Outcome {
        session: session.id.to_string(),
        status: status.as_str(),
        reply,
        asked: asked(client, &session.id),
    };
    if !json && outcome.reply.is_none() {
        return Ok(write(
            out,
            &format!("{} has no reply yet\n", safe(&session.name)),
        )?);
    }
    print(out, &outcome, &session.name, json)
}

/// `shepherd wait`: waits until the session's agent is at its prompt, waits on the user, or exits, and says which.
pub fn wait(
    drive: &mut impl Drive,
    session: &str,
    timeout: Option<Duration>,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    settle(drive)?;
    let session = target(drive, session)?;
    let id = session.id.clone();
    Budget::new(timeout)
        .wait(drive, &mut |c| stopped(status_of(c, &id)))
        .map_err(|e| match e {
            WaitError::TimedOut => timed_out(&session.name, "to stop", timeout),
            other => other.into(),
        })?;
    let client = drive.client();
    let status = status_of(client, &id).unwrap_or(SessionStatus::Exited);
    let asked = asked(client, &id);
    if json {
        let outcome = Outcome {
            session: id.to_string(),
            status: status.as_str(),
            reply: None,
            asked,
        };
        return Ok(write_json(out, &outcome)?);
    }
    let text = match asked {
        Some(asked) => format!("{}: {}\n", status.as_str(), safe(&asked)),
        None => format!("{}\n", status.as_str()),
    };
    Ok(write(out, &text)?)
}

// ---- argv ----

/// A `--timeout`: seconds (`90`), or a duration jiff reads (`30s`, `5m`, `1h 30m`, `PT2M`).
pub fn parse_timeout(text: &str) -> Result<Duration, String> {
    let bad = || {
        format!(
            "--timeout {}: give seconds, or a duration like 30s or 5m",
            safe(text)
        )
    };
    if let Ok(secs) = text.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let span: jiff::SignedDuration = text.parse().map_err(|_| bad())?;
    Duration::try_from(span).map_err(|_| bad())
}

fn timeout_arg(parsed: &Args) -> Result<Option<Duration>, String> {
    parsed.value("--timeout").map(parse_timeout).transpose()
}

fn prompt_args(args: &[String]) -> Result<(Prompt, bool), String> {
    let parsed =
        Args::parse(args, &["--timeout"], &["--wait", "--queue", "--json"]).ok_or(PROMPT_USAGE)?;
    let [session, text] = parsed.positional.as_slice() else {
        return Err(PROMPT_USAGE.to_owned());
    };
    let text = match text.as_str() {
        "-" => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| format!("reading the prompt from stdin: {e}"))?;
            text
        }
        text => text.to_owned(),
    };
    let prompt = Prompt {
        session: session.clone(),
        text,
        wait: parsed.switch("--wait"),
        queue: parsed.switch("--queue"),
        timeout: timeout_arg(&parsed)?,
    };
    Ok((prompt, parsed.switch("--json")))
}

/// `shepherd prompt <args>`.
pub fn prompt_main(args: &[String]) -> Result<(), CliError> {
    let (asked, json) = prompt_args(args)?;
    prompt(&mut super::connect()?, &asked, json, &mut std::io::stdout())
}

/// `shepherd reply <args>`.
pub fn reply_main(args: &[String]) -> Result<(), CliError> {
    let parsed = Args::parse(args, &[], &["--json"]).ok_or(REPLY_USAGE)?;
    let [session] = parsed.positional.as_slice() else {
        return Err(REPLY_USAGE.into());
    };
    reply(
        &mut super::connect()?,
        session,
        parsed.switch("--json"),
        &mut std::io::stdout(),
    )
}

/// `shepherd wait <args>`.
pub fn wait_main(args: &[String]) -> Result<(), CliError> {
    let parsed = Args::parse(args, &["--timeout"], &["--json"]).ok_or(WAIT_USAGE)?;
    let [session] = parsed.positional.as_slice() else {
        return Err(WAIT_USAGE.into());
    };
    let timeout = timeout_arg(&parsed)?;
    wait(
        &mut super::connect()?,
        session,
        timeout,
        parsed.switch("--json"),
        &mut std::io::stdout(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_is_seconds_or_a_duration() {
        assert_eq!(parse_timeout("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_timeout("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_timeout("1h 30m"), Ok(Duration::from_secs(5400)));
        assert!(parse_timeout("-5s").is_err());
        assert!(parse_timeout("soon").is_err());
    }

    #[test]
    fn prompt_takes_a_session_and_its_text() {
        let args: Vec<String> = ["api", "fix it", "--wait", "--timeout", "10"]
            .map(str::to_owned)
            .to_vec();
        let (prompt, json) = prompt_args(&args).unwrap();
        assert_eq!(
            prompt,
            Prompt {
                session: "api".into(),
                text: "fix it".into(),
                wait: true,
                queue: false,
                timeout: Some(Duration::from_secs(10)),
            }
        );
        assert!(!json);
        assert!(prompt_args(&["api".to_owned()]).is_err());
    }
}
