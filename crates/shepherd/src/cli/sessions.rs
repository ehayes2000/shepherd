//! `shepherd sessions [--json]` and `shepherd new [--profile P] [--machine M] [--cwd DIR] [--name N] [--json]` (T46):
//! every session on this machine and the peers it reaches, and starting one. Shapes in `docs/cli.md`.

use std::io::Write;
use std::path::Path;

use schemars::JsonSchema;
use serde::Serialize;
use shepherd_core::acpp::{Pr, PrChecks, PrState, Seq};
use shepherd_core::client::automation::{SessionStatus, newest_first, resolve_machine};
use shepherd_core::client::{Client, ClientSession, MachineGroup, MachineStatus};
use shepherd_core::wire::{ClientRequest, HostResult, SessionNew};

use super::{ANSWER_LIMIT, Args, Drive, request, safe, settle, write, write_json};

const SESSIONS_USAGE: &str = "usage: shepherd sessions [--json]";
const NEW_USAGE: &str =
    "usage: shepherd new [--profile P] [--machine M] [--cwd DIR] [--name N] [--json]";

// ---- JSON shapes (docs/cli.md) ----

/// A session.
#[derive(Serialize, JsonSchema)]
struct SessionJson {
    id: String,
    name: String,
    /// What the machine is called.
    machine: String,
    machine_id: String,
    /// `claude-code`, `codex` or `shell`; null from a host older than recording it.
    agent: Option<&'static str>,
    /// `running`, `idle` (at its prompt), `wants-user` (waits on the user) or `exited`.
    status: &'static str,
    /// The workspace it was started in.
    workspace: Option<String>,
    worktree: Option<String>,
    branch: Option<String>,
    pr: Option<PrJson>,
    /// Its worktree (or its post-create hooks) is still being made.
    starting: bool,
    /// Why it couldn't start.
    failed: Option<String>,
    /// RFC 3339.
    created_at: String,
    /// What its agent waits on the user for, which `answer` can answer.
    question: Option<QuestionJson>,
}

/// The question a session's agent waits on the user for, which `shepherd answer` can answer: its seq names it.
#[derive(Serialize, JsonSchema)]
struct QuestionJson {
    /// The seq of its `agent/asked` entry in the session's log: `answer` names the question by it.
    #[schemars(with = "u64")]
    seq: Seq,
    /// What it asks: the tool and its main argument, or the question.
    text: String,
    /// A question's options, in order (`answer` takes an option's number, from 1); null for a permission.
    choices: Option<Vec<String>>,
}

impl QuestionJson {
    fn of(client: &Client, s: &ClientSession) -> Option<Self> {
        let state = client.state(&s.id)?;
        let asked = state.question()?;
        Some(Self {
            seq: state.question_seq()?,
            text: asked.text.clone(),
            choices: asked.choices.clone(),
        })
    }
}

impl SessionJson {
    fn of(client: &Client, s: &ClientSession) -> Self {
        Self {
            id: s.id.to_string(),
            name: s.name.clone(),
            machine: client.machine_label(&s.machine).to_owned(),
            machine_id: s.machine.to_string(),
            agent: s.agent.map(|a| a.as_str()),
            status: SessionStatus::in_client(client, s).as_str(),
            workspace: s.workspace.as_deref().map(lossy),
            worktree: s.worktree.as_deref().map(lossy),
            branch: s.branch.clone(),
            pr: s.pr.as_ref().map(PrJson::from),
            starting: s.creating_worktree,
            failed: s.failed.clone(),
            created_at: s.created.to_string(),
            question: QuestionJson::of(client, s),
        }
    }
}

// Spelled by the CLI rather than the log, so the shape stays what `docs/cli.md` says whatever the log learns: a state or
// checks value this build doesn't know is `other`.
/// A pull request.
#[derive(Serialize, JsonSchema)]
struct PrJson {
    number: u64,
    /// `open`, `closed`, `merged` or `other`.
    state: &'static str,
    /// `none`, `pending`, `passing`, `failing` or `other`.
    checks: &'static str,
}

impl From<&Pr> for PrJson {
    fn from(pr: &Pr) -> Self {
        Self {
            number: pr.number,
            state: match pr.state {
                PrState::Open => "open",
                PrState::Closed => "closed",
                PrState::Merged => "merged",
                PrState::Other(_) => "other",
            },
            checks: match pr.checks {
                PrChecks::None => "none",
                PrChecks::Pending => "pending",
                PrChecks::Passing => "passing",
                PrChecks::Failing => "failing",
                PrChecks::Other(_) => "other",
            },
        }
    }
}

/// A machine.
#[derive(Serialize, JsonSchema)]
struct MachineJson {
    id: String,
    name: String,
    /// This machine.
    local: bool,
    /// `connected`, `unreachable` or `incompatible`.
    status: &'static str,
    /// Why it can't be reached.
    why: Option<String>,
}

/// `{ machines, sessions }`: this machine first, then the paired machines by name; sessions newest first.
#[derive(Serialize, JsonSchema)]
pub(crate) struct SessionsJson {
    machines: Vec<MachineJson>,
    sessions: Vec<SessionJson>,
}

/// `{ session }`: the session made.
#[derive(Serialize, JsonSchema)]
pub(crate) struct MadeJson {
    session: SessionJson,
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

/// The sessions to list, newest first: none of a machine that can't be reached now, whose last-known sessions may be
/// stale or half sent.
fn listed(client: &Client) -> Vec<&ClientSession> {
    newest_first(client)
        .into_iter()
        .filter(|s| !client.offline(&s.id))
        .collect()
}

/// `shepherd sessions`: every session, newest first, then a line for each machine that couldn't be reached.
pub fn sessions(drive: &mut impl Drive, json: bool, out: &mut impl Write) -> Result<(), String> {
    settle(drive)?;
    let client = drive.client();
    let listed = listed(client);
    let machines = client.machines();
    if json {
        return write_json(
            out,
            &SessionsJson {
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
        SessionStatus::in_client(client, s).as_str(),
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
        Some(text) => Some(resolve_machine(drive.client(), text).map_err(|e| e.to_string())?),
    };
    let cwd = match &new.cwd {
        None => None,
        // A path here means nothing on another machine.
        Some(dir)
            if !Path::new(dir).is_absolute() && machine.as_ref().is_some_and(|m| !m.local) =>
        {
            return Err(format!(
                "--cwd {} is relative: on another machine, give an absolute path",
                safe(dir)
            ));
        }
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
    let session = match request(drive, machine.map(|m| m.id), body)? {
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
        return write_json(
            out,
            &MadeJson {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};
    use shepherd_core::acpp::{Source, SourceKind};
    use shepherd_core::client::{AskId, Client, ClientEvent};
    use shepherd_core::config::AgentKind;
    use shepherd_core::host::NewSessionSpec;
    use shepherd_core::ids::MachineId;
    use shepherd_testkit::builders::{ClientDriver, claude_session_spec, shell_session_spec, ts};

    use super::*;
    use crate::cli::WaitError;

    /// A client fed from a [`ClientDriver`]'s host: every entry is delivered before each check.
    struct Fed(ClientDriver, shepherd_tui::Screens);

    impl Drive for Fed {
        fn client(&self) -> &Client {
            self.0.client()
        }

        fn screens(&self) -> &shepherd_tui::Screens {
            &self.1
        }

        fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId {
            let ask = AskId(1);
            self.0.handle(ClientEvent::Ask {
                ask,
                machine,
                request,
            });
            ask
        }

        fn wait(
            &mut self,
            done: &mut dyn FnMut(&Client) -> bool,
            _: Duration,
        ) -> Result<(), WaitError> {
            self.0.sync();
            if done(self.0.client()) {
                Ok(())
            } else {
                Err(WaitError::Failed("not done".to_owned()))
            }
        }
    }

    /// A settled client of a host in workspace `/src/app` whose sessions are a Claude Code one with a worktree and an
    /// open pull request, and a newer Codex one whose pull request's state and checks this build doesn't know.
    fn fed() -> Fed {
        let mut c = ClientDriver::new();
        c.handle(ClientEvent::Start);
        let hello = c.handle(ClientEvent::Connected {
            at: ts("2026-10-29T12:00:00.000Z"),
        });
        let answer =
            json!({ "version": shepherd_core::wire::PROTOCOL_VERSION, "machine": "m_test" });
        let subscribe = c.answer(&hello, Ok(answer));
        let list = c.answer(&subscribe, Ok(json!({})));
        c.answer(&list, Ok(json!({ "peers": [] })));
        let host = c.host();
        host.select_workspace("/src/app");
        host.new_session_with("s1", claude_session_spec());
        host.worktree_ready("s1");
        let codex = NewSessionSpec {
            profile: "codex".to_owned(),
            agent: AgentKind::Codex,
            explicit_cwd: true,
            ..shell_session_spec()
        };
        host.new_session_with("s2", codex);
        let pr = |state: &str, checks: &str| {
            json!({ "jsonrpc": "2.0", "method": "_shepherd/session/pr",
                    "params": { "pr": { "number": 7, "state": state, "checks": checks } } })
        };
        let shepherd = Source::new(SourceKind::Shepherd, None);
        host.append(Some("s1"), shepherd.clone(), pr("open", "passing"));
        host.append(Some("s2"), shepherd, pr("draft", "flaky"));
        c.sync();
        Fed(c, shepherd_tui::Screens::new())
    }

    fn listed(drive: &mut Fed) -> Value {
        let mut out = Vec::new();
        sessions(drive, true, &mut out).unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn a_sessions_agent_worktree_pr_and_creation_are_listed_as_documented() {
        let mut drive = fed();
        let created = drive.client().sessions()[1].created.clone();
        let listed = listed(&mut drive);
        let codex = &listed["sessions"][0];
        let claude = &listed["sessions"][1];
        assert_eq!(claude["id"], "s1");
        assert_eq!(claude["agent"], "claude-code");
        assert_eq!(codex["agent"], "codex");
        assert_eq!(claude["worktree"], "/wt/s1");
        assert_eq!(claude["branch"], "s1");
        assert_eq!(claude["workspace"], "/src/app");
        assert_eq!(
            claude["pr"],
            json!({ "number": 7, "state": "open", "checks": "passing" })
        );
        // RFC 3339, the instant the host stamped its `session/created` with.
        let shown: jiff::Timestamp = claude["created_at"].as_str().unwrap().parse().unwrap();
        assert_eq!(shown, created.instant());
    }

    #[test]
    fn a_pr_state_or_checks_this_build_does_not_know_is_other() {
        let listed = listed(&mut fed());
        assert_eq!(listed["sessions"][0]["id"], "s2");
        assert_eq!(
            listed["sessions"][0]["pr"],
            json!({ "number": 7, "state": "other", "checks": "other" })
        );
    }
}
