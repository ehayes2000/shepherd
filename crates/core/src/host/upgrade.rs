//! Whether the host can update itself in place when a client asks (`_shepherd/host/upgrade`, spike T36, decision
//! 0018). The host re-execs the binary at its startup path, carrying its sessions, only after `shepherd
//! handoff-check` run from that path (the preflight) proves the installed build can take them over. [`decide`] is the
//! whole decision; the runtime gathers the [`UpgradeFacts`], runs the preflight when told to, and asks again.

use serde::{Deserialize, Serialize};

use crate::wire::{BuildId, ExeDigest, UpgradeAnswer};

/// The version of the handoff file this build writes. A build lists in [`HandoffCheck::handoff_versions`] every
/// version it can read.
pub const HANDOFF_VERSION: u32 = 1;

/// What `shepherd handoff-check` prints, as one JSON line: the build it is, and the handoff versions it can adopt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffCheck {
    pub build: BuildId,
    pub handoff_versions: Vec<u32>,
}

impl HandoffCheck {
    /// This build's answer, for `shepherd handoff-check`.
    pub fn of(build: BuildId) -> Self {
        Self {
            build,
            handoff_versions: vec![HANDOFF_VERSION],
        }
    }
}

/// How `<installed> handoff-check` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    Exited {
        code: i32,
        stdout: Vec<u8>,
    },
    /// Killed by this signal. On macOS, a binary overwritten in place (same inode) is SIGKILLed on exec.
    Signaled(i32),
    /// It ran past its bound and was killed.
    TimedOut,
    /// It couldn't be run at all.
    Failed(String),
}

/// What the host is in the middle of that a swap can't carry: it waits for these to finish instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Work {
    /// `Worktrees` and `Commands` calls submitted and not answered (making or removing a worktree, a hook).
    pub port_calls: usize,
    /// Sessions whose run files wait for their spawn.
    pub run_files: usize,
}

/// Everything [`decide`] looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeFacts {
    /// The digest of the build the host runs, if it could read it at startup.
    pub running: Option<ExeDigest>,
    /// The digest of the file at its startup path now, if it can be read.
    pub installed: Option<ExeDigest>,
    /// Swap even onto the running build (tests).
    pub force: bool,
    pub work: Work,
    /// Why the last swap failed after it was accepted, if one did since the installed build last changed.
    pub failed: Option<String>,
    /// The preflight's result, once it has run for this request.
    pub preflight: Option<Preflight>,
}

/// What to do about an upgrade request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeStep {
    /// Answer it.
    Answer(UpgradeAnswer),
    /// Run the preflight, then decide again with its result.
    Preflight,
}

/// Decides an upgrade request: nothing to do, busy, unavailable (and why), accepted, or run the preflight first.
pub fn decide(facts: &UpgradeFacts) -> UpgradeStep {
    let answer = |a| UpgradeStep::Answer(a);
    let unavailable = |reason: String| answer(UpgradeAnswer::Unavailable { reason });
    let Some(installed) = facts.installed else {
        return unavailable("cannot read the installed shepherd".to_owned());
    };
    if facts.running == Some(installed) && !facts.force {
        return answer(UpgradeAnswer::NothingToDo);
    }
    if let Some(why) = &facts.failed {
        return unavailable(format!("updating in place failed: {why}"));
    }
    if let Some(reason) = busy(facts.work) {
        return answer(UpgradeAnswer::Busy { reason });
    }
    match &facts.preflight {
        None => UpgradeStep::Preflight,
        Some(preflight) => match can_hand_off(preflight) {
            Ok(()) => answer(UpgradeAnswer::Accepted),
            Err(reason) => unavailable(reason),
        },
    }
}

fn busy(work: Work) -> Option<String> {
    match work {
        Work {
            port_calls: 0,
            run_files: 0,
        } => None,
        Work {
            port_calls: 0,
            run_files,
        } => Some(format!("{run_files} session(s) are starting")),
        Work { port_calls, .. } => Some(format!(
            "{port_calls} worktree or hook operation(s) are running"
        )),
    }
}

/// Whether the preflight shows the installed build can adopt what this one hands off, or why not.
fn can_hand_off(preflight: &Preflight) -> Result<(), String> {
    match preflight {
        Preflight::Exited { code: 0, stdout } => {
            let line = stdout.split(|&b| b == b'\n').next().unwrap_or_default();
            let check: HandoffCheck = serde_json::from_slice(line).map_err(|e| {
                format!("cannot read the installed shepherd's handoff-check answer: {e}")
            })?;
            if check.handoff_versions.contains(&HANDOFF_VERSION) {
                Ok(())
            } else {
                Err(format!(
                    "the installed shepherd reads handoff formats {:?}, and this host writes handoff format \
                     {HANDOFF_VERSION}",
                    check.handoff_versions
                ))
            }
        }
        Preflight::Exited { code, .. } => Err(format!(
            "the installed shepherd can't hand off (handoff-check exited with {code}): it is older than hot-swap"
        )),
        Preflight::Signaled(signal) => Err(format!(
            "the installed shepherd was killed by signal {signal} when run: overwritten in place? reinstall it"
        )),
        Preflight::TimedOut => {
            Err("the installed shepherd didn't answer handoff-check in time".to_owned())
        }
        Preflight::Failed(why) => Err(format!("cannot run the installed shepherd: {why}")),
    }
}
