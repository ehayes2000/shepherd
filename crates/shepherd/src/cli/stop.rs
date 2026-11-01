//! `shepherd stop` (T53): close a session, as the sidebar's Close does (`_shepherd/session/close`). Shape in
//! `docs/cli.md`.
//!
//! Like the Close dialog, it first asks the session's host what removing its worktree would lose
//! (`_shepherd/worktree/status`). Nothing: the session is closed and its worktree removed. Unsaved work (or a status
//! that can't be told) refuses to stop it, and nothing is closed, unless `--force`, which stops it and keeps the
//! worktree: stopping never throws work away.

use std::io::Write;

use schemars::JsonSchema;
use serde::Serialize;
use shepherd_core::client::AskError;
use shepherd_core::wire::{
    ClientRequest, CloseWorktree, HostResult, SessionClose, WorktreeCheck, WorktreeStatusOk,
};

use super::{Args, CliError, Drive, ask_host, request, safe, settle, target, write, write_json};

const USAGE: &str = "usage: shepherd stop <session> [--force] [--json]";

/// What `shepherd stop` was asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stop {
    /// The session, by id or name.
    pub session: String,
    /// Stop it even with unsaved work in its worktree, keeping the worktree.
    pub force: bool,
}

/// `{ session, worktree, unsaved }` (docs/cli.md).
#[derive(Serialize, JsonSchema)]
pub(crate) struct StoppedJson {
    session: String,
    // `remove`: once its pre-remove hooks have run, unless it has something to lose by then.
    /// What happens to its worktree: `remove` (it loses nothing) or `keep`; null when it has none.
    worktree: Option<&'static str>,
    /// What the kept worktree holds that removing it would lose.
    unsaved: Vec<String>,
}

/// What stopping the session does with its worktree, and what that worktree holds that removing it would lose.
enum Plan {
    /// It has no worktree (or it is already gone).
    NoWorktree,
    /// Removing it loses nothing.
    Remove,
    /// Removing it would lose this; or (`Err`) whether it would can't be told, saying why.
    AtRisk(Result<Vec<String>, String>),
}

/// `shepherd stop`: has the session's host close it, unless its worktree holds unsaved work and `stop.force` is off.
pub fn stop(
    drive: &mut impl Drive,
    stop: &Stop,
    json: bool,
    out: &mut impl Write,
) -> Result<(), CliError> {
    settle(drive)?;
    let session = target(drive, &stop.session)?;
    let (id, machine, name) = (session.id, session.machine, session.name);
    let plan = if session.worktree.is_none() {
        Plan::NoWorktree
    } else {
        let check = ClientRequest::WorktreeStatus(WorktreeCheck {
            session: id.clone(),
        });
        match ask_host(drive, Some(machine.clone()), check)? {
            Ok(HostResult::WorktreeStatus(WorktreeStatusOk { status: None })) => Plan::NoWorktree,
            Ok(HostResult::WorktreeStatus(WorktreeStatusOk {
                status: Some(status),
            })) => {
                let losses: Vec<String> = status.losses().iter().map(ToString::to_string).collect();
                if losses.is_empty() {
                    Plan::Remove
                } else {
                    Plan::AtRisk(Ok(losses))
                }
            }
            Ok(other) => return Err(format!("the host answered something else: {other:?}").into()),
            Err(AskError::Refused(e)) => Plan::AtRisk(Err(e.to_string())),
            Err(e) => return Err(e.to_string().into()),
        }
    };
    let (worktree, shown, unsaved) = match plan {
        Plan::NoWorktree => (CloseWorktree::IfSafe, None, Vec::new()),
        Plan::Remove => (CloseWorktree::IfSafe, Some("remove"), Vec::new()),
        Plan::AtRisk(at_risk) if stop.force => (
            CloseWorktree::Keep,
            Some("keep"),
            at_risk.unwrap_or_default(),
        ),
        Plan::AtRisk(Ok(losses)) => {
            return Err(CliError::Failed(format!(
                "{} has unsaved work in its worktree ({}): it was not stopped; --force stops it and keeps the worktree",
                safe(&name),
                losses.join(", ")
            )));
        }
        Plan::AtRisk(Err(why)) => {
            return Err(CliError::Failed(format!(
                "can't tell whether {} has unsaved work in its worktree ({}): it was not stopped; --force stops it and keeps the worktree",
                safe(&name),
                safe(&why)
            )));
        }
    };
    let close = ClientRequest::SessionClose(SessionClose {
        session: id.clone(),
        worktree,
    });
    request(drive, Some(machine), close)?;
    if json {
        let stopped = StoppedJson {
            session: id.to_string(),
            worktree: shown,
            unsaved,
        };
        return Ok(write_json(out, &stopped)?);
    }
    let text = match shown {
        Some("keep") if unsaved.is_empty() => {
            format!("stopped {}, keeping its worktree\n", safe(&name))
        }
        Some("keep") => format!(
            "stopped {}, keeping its worktree ({})\n",
            safe(&name),
            unsaved.join(", ")
        ),
        Some(_) => format!("stopped {}, removing its worktree\n", safe(&name)),
        None => format!("stopped {}\n", safe(&name)),
    };
    Ok(write(out, &text)?)
}

fn stop_args(args: &[String]) -> Result<(Stop, bool), String> {
    let parsed = Args::parse(args, &[], &["--force", "--json"]).ok_or(USAGE)?;
    let [session] = parsed.positional.as_slice() else {
        return Err(USAGE.to_owned());
    };
    let stop = Stop {
        session: session.clone(),
        force: parsed.switch("--force"),
    };
    Ok((stop, parsed.switch("--json")))
}

/// `shepherd stop <args>`.
pub fn stop_main(args: &[String]) -> Result<(), CliError> {
    let (asked, json) = stop_args(args)?;
    stop(&mut super::connect()?, &asked, json, &mut std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn stop_takes_a_session_and_maybe_force() {
        let stop = |session: &str, force| Stop {
            session: session.to_owned(),
            force,
        };
        assert_eq!(stop_args(&args(&["api"])), Ok((stop("api", false), false)));
        assert_eq!(
            stop_args(&args(&["api", "--force", "--json"])),
            Ok((stop("api", true), true))
        );
        assert!(stop_args(&args(&[])).is_err());
        assert!(stop_args(&args(&["api", "web"])).is_err());
        assert!(stop_args(&args(&["api", "--remove"])).is_err());
    }
}
