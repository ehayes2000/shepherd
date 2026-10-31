//! `shepherd sessions [--json]` and `shepherd new [--profile P] [--machine M] [--cwd DIR] [--name N] [--json]` (T46):
//! every session on this machine and the peers it reaches, and starting one. Shapes in `docs/cli.md`.

use std::io::Write;
use std::path::Path;

use serde::Serialize;
use shepherd_core::acpp::Pr;
use shepherd_core::client::automation::{SessionStatus, newest_first, resolve_machine};
use shepherd_core::client::{Client, ClientSession, MachineGroup, MachineStatus};
use shepherd_core::wire::{ClientRequest, HostResult, SessionNew};

use super::{ANSWER_LIMIT, Args, Drive, request, safe, settle, write, write_json};

const SESSIONS_USAGE: &str = "usage: shepherd sessions [--json]";
const NEW_USAGE: &str =
    "usage: shepherd new [--profile P] [--machine M] [--cwd DIR] [--name N] [--json]";

// ---- JSON shapes (docs/cli.md) ----

#[derive(Serialize)]
struct SessionJson {
    id: String,
    name: String,
    machine: String,
    machine_id: String,
    agent: Option<&'static str>,
    status: &'static str,
    workspace: Option<String>,
    worktree: Option<String>,
    branch: Option<String>,
    pr: Option<Pr>,
    starting: bool,
    failed: Option<String>,
    created_at: String,
}

impl SessionJson {
    fn of(client: &Client, s: &ClientSession) -> Self {
        Self {
            id: s.id.to_string(),
            name: s.name.clone(),
            machine: client.machine_label(&s.machine).to_owned(),
            machine_id: s.machine.to_string(),
            agent: s.agent.map(|a| a.as_str()),
            status: SessionStatus::of(s).as_str(),
            workspace: s.workspace.as_deref().map(lossy),
            worktree: s.worktree.as_deref().map(lossy),
            branch: s.branch.clone(),
            pr: s.pr.clone(),
            starting: s.creating_worktree,
            failed: s.failed.clone(),
            created_at: s.created.to_string(),
        }
    }
}

#[derive(Serialize)]
struct MachineJson {
    id: String,
    name: String,
    local: bool,
    status: &'static str,
    why: Option<String>,
}

impl From<&MachineGroup> for MachineJson {
    fn from(m: &MachineGroup) -> Self {
        let (status, why) = machine_status(&m.status);
        Self {
            id: m.id.to_string(),
            name: m.name.clone(),
            local: m.local,
            status,
            why,
        }
    }
}

fn machine_status(status: &MachineStatus) -> (&'static str, Option<String>) {
    match status {
        MachineStatus::Connected => ("connected", None),
        MachineStatus::Connecting => ("connecting", None),
        MachineStatus::Offline(why) => ("unreachable", why.clone()),
        MachineStatus::Incompatible => ("incompatible", None),
    }
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---- sessions ----

/// `shepherd sessions`: every session, newest first, then a line for each machine that couldn't be reached.
pub fn sessions(drive: &mut impl Drive, json: bool, out: &mut impl Write) -> Result<(), String> {
    settle(drive)?;
    let client = drive.client();
    let listed = newest_first(client);
    let machines = client.machines();
    if json {
        #[derive(Serialize)]
        struct Sessions {
            machines: Vec<MachineJson>,
            sessions: Vec<SessionJson>,
        }
        return write_json(
            out,
            &Sessions {
                machines: machines.iter().map(MachineJson::from).collect(),
                sessions: listed.iter().map(|s| SessionJson::of(client, s)).collect(),
            },
        );
    }
    let mut text: String = listed
        .iter()
        .map(|s| format!("{}\n", session_line(client, s)))
        .collect();
    if listed.is_empty() {
        text.push_str("No sessions.\n");
    }
    for machine in machines
        .iter()
        .filter(|m| m.status != MachineStatus::Connected)
    {
        let (status, why) = machine_status(&machine.status);
        let why = why
            .map(|why| format!(": {}", safe(&why)))
            .unwrap_or_default();
        text.push_str(&format!("({} is {status}{why})\n", safe(&machine.name)));
    }
    write(out, &text)
}

/// `<id>  <name>  <status>  <agent>  <machine>  <branch>  <#pr>  <workspace>`, `-` for what it hasn't.
fn session_line(client: &Client, s: &ClientSession) -> String {
    let dash = |text: Option<String>| text.map_or_else(|| "-".to_owned(), |t| safe(&t));
    format!(
        "{}  {}  {}  {}  {}  {}  {}  {}",
        safe(s.id.as_str()),
        safe(&s.name),
        SessionStatus::of(s).as_str(),
        s.agent.map_or("-", |a| a.as_str()),
        safe(client.machine_label(&s.machine)),
        dash(s.branch.clone()),
        s.pr.as_ref()
            .map_or_else(|| "-".to_owned(), |pr| format!("#{}", pr.number)),
        dash(s.workspace.as_deref().map(lossy)),
    )
}

// ---- new ----

/// What `shepherd new` was asked for.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct New {
    pub profile: Option<String>,
    /// A machine's id or name; this one for `None`.
    pub machine: Option<String>,
    /// Where to run, as given: relative to where the command runs. `None` makes a managed worktree in the host's
    /// workspace (or runs in its home without one).
    pub cwd: Option<String>,
    pub name: Option<String>,
}

/// `shepherd new`: asks the machine's host for a session and, once the host has made it and it is listed, prints its
/// id (or, with `json`, the session). `here` is what a relative `cwd` is relative to.
pub fn new(
    drive: &mut impl Drive,
    new: &New,
    here: &Path,
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    settle(drive)?;
    let machine = match &new.machine {
        None => None,
        Some(text) => Some(
            resolve_machine(drive.client(), text)
                .map_err(|e| e.to_string())?
                .id,
        ),
    };
    let cwd = match &new.cwd {
        None => None,
        Some(dir) => Some(
            here.join(dir)
                .into_os_string()
                .into_string()
                .map_err(|_| format!("{} is not UTF-8", safe(dir)))?,
        ),
    };
    let body = ClientRequest::SessionNew(SessionNew {
        profile: new.profile.clone(),
        cwd,
        name: new.name.clone(),
        ..SessionNew::default()
    });
    let session = match request(drive, machine, body)? {
        HostResult::SessionNew(ok) => ok.session,
        other => return Err(format!("the host answered something else: {other:?}")),
    };
    // Its own host lists it once its `session/created` arrives.
    drive.wait(
        &mut |c| c.sessions().iter().any(|s| s.id == session),
        ANSWER_LIMIT,
    )?;
    let client = drive.client();
    let Some(listed) = client.sessions().iter().find(|s| s.id == session) else {
        return Err(format!("session {session} was made but isn't listed"));
    };
    if json {
        #[derive(Serialize)]
        struct Made {
            session: SessionJson,
        }
        return write_json(
            out,
            &Made {
                session: SessionJson::of(client, listed),
            },
        );
    }
    write(out, &format!("{}\n", safe(session.as_str())))
}

// ---- argv ----

fn sessions_args(args: &[String]) -> Option<bool> {
    let parsed = Args::parse(args, &[], &["--json"])?;
    parsed
        .positional
        .is_empty()
        .then(|| parsed.switch("--json"))
}

fn new_args(args: &[String]) -> Option<(New, bool)> {
    let parsed = Args::parse(
        args,
        &["--profile", "--machine", "--cwd", "--name"],
        &["--json"],
    )?;
    if !parsed.positional.is_empty() {
        return None;
    }
    let value = |flag| parsed.value(flag).map(str::to_owned);
    let new = New {
        profile: value("--profile"),
        machine: value("--machine"),
        cwd: value("--cwd"),
        name: value("--name"),
    };
    Some((new, parsed.switch("--json")))
}

/// `shepherd sessions <args>`.
pub fn sessions_main(args: &[String]) -> Result<(), String> {
    let json = sessions_args(args).ok_or(SESSIONS_USAGE)?;
    sessions(&mut super::connect()?, json, &mut std::io::stdout())
}

/// `shepherd new <args>`.
pub fn new_main(args: &[String]) -> Result<(), String> {
    let (new_args, json) = new_args(args).ok_or(NEW_USAGE)?;
    let here = std::env::current_dir().map_err(|e| format!("the current directory: {e}"))?;
    new(
        &mut super::connect()?,
        &new_args,
        &here,
        json,
        &mut std::io::stdout(),
    )
}
