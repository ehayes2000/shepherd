//! The command-line client for scripts and agents (T46, decision 0022): `shepherd sessions`, `shepherd new`, the
//! session commands built on them, and `shepherd ui` (T50). `docs/cli.md` documents each command and its `--json` shape.
//! `shepherd mcp` ([`crate::mcp`]) runs these same functions as MCP tools.
//!
//! A command is a short-lived client of the local host (launching it if none is running), over the same link and
//! tunnels as the TUI: it runs the client's own core ([`Client`]) headless through a [`Drive`], so it folds the same
//! state the sidebar shows instead of parsing logs a second way. It waits until every reachable host has sent its
//! sessions ([`settle`]), asks what it needs ([`request`]), and names sessions the one way every command does
//! ([`target`]). Human output is display-safe; errors go to stderr.

pub mod answer;
pub mod prompt;
pub mod screen;
pub mod sessions;
pub mod stop;
pub mod ui;

use std::fmt;
use std::io::Write;
use std::time::Duration;

use serde::Serialize;
use shepherd_core::client::automation::Target;
use shepherd_core::client::{AskError, AskId, Client, ClientSession};
use shepherd_core::display::display_safe;
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{HostLauncher, Link, LinkEventSink};
use shepherd_core::wire::{ClientRequest, HostResult};
use shepherd_tui::Screens;

use crate::client::{ClientError, ClientRuntime, SocketRuntime, socket_runtime};

/// How long reaching the hosts may take: launching the local one, and each peer's tunnel and catch-up.
pub const SETTLE_LIMIT: Duration = Duration::from_secs(60);
/// How long a peer has, from when its conn opens, to send all its sessions before it is left out as unreachable:
/// well within [`SETTLE_LIMIT`], so a slow peer never fails the command.
pub const PEER_CATCH_UP_LIMIT: jiff::SignedDuration = jiff::SignedDuration::from_secs(20);
/// How long a host has to answer a request.
pub const ANSWER_LIMIT: Duration = Duration::from_secs(60);
/// The most grapheme clusters of one field printed.
const SHOWN_MAX: usize = 512;

/// The exit status of a command that gave up waiting (`--timeout`), as `timeout(1)` exits.
pub const TIMED_OUT_EXIT: u8 = 124;

/// Why [`Drive::wait`] stopped before its condition held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitError {
    /// Its limit passed first.
    TimedOut,
    /// The client gave up, saying why.
    Failed(String),
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => f.write_str("the shepherd host didn't answer in time"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

impl From<WaitError> for String {
    fn from(e: WaitError) -> Self {
        e.to_string()
    }
}

/// Why a command failed: its exit status says which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// Exit status 1.
    Failed(String),
    /// It gave up waiting at its `--timeout`: exit status [`TIMED_OUT_EXIT`].
    TimedOut(String),
}

impl CliError {
    pub fn why(&self) -> &str {
        match self {
            Self::Failed(why) | Self::TimedOut(why) => why,
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Failed(_) => 1,
            Self::TimedOut(_) => TIMED_OUT_EXIT,
        }
    }
}

impl From<String> for CliError {
    fn from(why: String) -> Self {
        Self::Failed(why)
    }
}

impl From<&str> for CliError {
    fn from(why: &str) -> Self {
        Self::Failed(why.to_owned())
    }
}

impl From<WaitError> for CliError {
    fn from(e: WaitError) -> Self {
        Self::Failed(e.to_string())
    }
}

/// A running client, as a command drives it.
pub trait Drive {
    fn client(&self) -> &Client;

    /// Each session's terminal, as the host's output has drawn it.
    fn screens(&self) -> &Screens;

    /// Asks `machine`'s host (this machine's for `None`) `request`. Read the answer with [`Client::answer`].
    fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId;

    /// Runs the client until `done` holds of it. Fails if that takes longer than `limit`, or the client gives up.
    fn wait(
        &mut self,
        done: &mut dyn FnMut(&Client) -> bool,
        limit: Duration,
    ) -> Result<(), WaitError>;
}

/// A borrowed client drives as the client itself does: `shepherd mcp` hands each tool call one.
impl<D: Drive + ?Sized> Drive for &mut D {
    fn client(&self) -> &Client {
        (**self).client()
    }

    fn screens(&self) -> &Screens {
        (**self).screens()
    }

    fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId {
        (**self).ask(machine, request)
    }

    fn wait(
        &mut self,
        done: &mut dyn FnMut(&Client) -> bool,
        limit: Duration,
    ) -> Result<(), WaitError> {
        (**self).wait(done, limit)
    }
}

impl<D: Drive + ?Sized> Drive for Box<D> {
    fn client(&self) -> &Client {
        (**self).client()
    }

    fn screens(&self) -> &Screens {
        (**self).screens()
    }

    fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId {
        (**self).ask(machine, request)
    }

    fn wait(
        &mut self,
        done: &mut dyn FnMut(&Client) -> bool,
        limit: Duration,
    ) -> Result<(), WaitError> {
        (**self).wait(done, limit)
    }
}

impl<L, C, H> Drive for ClientRuntime<L, C, H>
where
    L: Link,
    C: FnMut(
        std::sync::Arc<dyn LinkEventSink>,
    ) -> Result<(L, shepherd_core::ports::ConnId), String>,
    H: HostLauncher,
{
    fn client(&self) -> &Client {
        ClientRuntime::client(self)
    }

    fn screens(&self) -> &Screens {
        ClientRuntime::screens(self)
    }

    fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId {
        ClientRuntime::ask(self, machine, request)
    }

    fn wait(
        &mut self,
        done: &mut dyn FnMut(&Client) -> bool,
        limit: Duration,
    ) -> Result<(), WaitError> {
        self.run_until(done, limit).map_err(|e| match e {
            ClientError::TimedOut => WaitError::TimedOut,
            other => WaitError::Failed(other.to_string()),
        })
    }
}

/// A started client of the local host at `~/.shepherd/host.sock`. Its hello names no directory, so the host never
/// selects a workspace because of where a command ran.
pub fn connect() -> Result<SocketRuntime, String> {
    let runtime = socket_runtime(&crate::peers::socket()?).map_err(|e| e.to_string())?;
    Ok(started(runtime))
}

/// `runtime`, set up as every command's client and started: a peer that doesn't send its sessions within
/// [`PEER_CATCH_UP_LIMIT`] is left out. [`connect`] uses it, and so do the scenario tests.
pub fn started<L, C, H>(mut runtime: ClientRuntime<L, C, H>) -> ClientRuntime<L, C, H>
where
    L: Link,
    C: FnMut(
        std::sync::Arc<dyn LinkEventSink>,
    ) -> Result<(L, shepherd_core::ports::ConnId), String>,
    H: HostLauncher,
{
    runtime.set_catch_up_limit(PEER_CATCH_UP_LIMIT);
    runtime.start();
    runtime
}

/// Waits until every host the client can reach has sent all its sessions ([`Client::settled`]).
pub fn settle(drive: &mut impl Drive) -> Result<(), String> {
    Ok(drive.wait(&mut Client::settled, SETTLE_LIMIT)?)
}

/// Asks `machine`'s host (this machine's for `None`) `request` and waits for its answer: the host's refusal, or why
/// it couldn't be asked, is the error.
pub fn request(
    drive: &mut impl Drive,
    machine: Option<MachineId>,
    request: ClientRequest,
) -> Result<HostResult, String> {
    ask_host(drive, machine, request)?.map_err(|e| e.to_string())
}

/// Asks `machine`'s host (this machine's for `None`) `request` and waits for its answer, which is the host's result or
/// why it refused (or couldn't be asked). Fails only if no answer comes.
pub fn ask_host(
    drive: &mut impl Drive,
    machine: Option<MachineId>,
    request: ClientRequest,
) -> Result<Result<HostResult, AskError>, String> {
    let ask = drive.ask(machine, request);
    drive.wait(&mut |c| c.answer(ask).is_some(), ANSWER_LIMIT)?;
    drive
        .client()
        .answer(ask)
        .cloned()
        .ok_or_else(|| "the shepherd host didn't answer".to_owned())
}

/// The session `text` names, by id or by unique name ([`Target`]), on any machine the client reaches. Call once
/// settled.
pub fn target(drive: &impl Drive, text: &str) -> Result<ClientSession, String> {
    Target::new(text)
        .resolve(drive.client())
        .cloned()
        .map_err(|e| e.to_string())
}

/// A command line's flags and other arguments: `--flag value` or `--flag=value` for the flags in `valued`, `--flag`
/// alone for those in `switches`. Any other flag is an error.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    values: Vec<(String, String)>,
    switches: Vec<String>,
    pub positional: Vec<String>,
}

impl Args {
    pub fn parse(args: &[String], valued: &[&str], switches: &[&str]) -> Option<Self> {
        let mut parsed = Self::default();
        let mut rest = args.iter();
        while let Some(arg) = rest.next() {
            if let Some((flag, value)) = arg.split_once('=')
                && valued.contains(&flag)
            {
                parsed.values.push((flag.to_owned(), value.to_owned()));
            } else if valued.contains(&arg.as_str()) {
                parsed.values.push((arg.clone(), rest.next()?.clone()));
            } else if switches.contains(&arg.as_str()) {
                parsed.switches.push(arg.clone());
            } else if arg.starts_with("--") {
                return None;
            } else {
                parsed.positional.push(arg.clone());
            }
        }
        Some(parsed)
    }

    /// The last value given for `flag`.
    pub fn value(&self, flag: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(f, _)| f == flag)
            .map(|(_, v)| v.as_str())
    }

    pub fn switch(&self, flag: &str) -> bool {
        self.switches.iter().any(|f| f == flag)
    }
}

pub fn write(out: &mut impl Write, text: &str) -> Result<(), String> {
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

pub fn write_json(out: &mut impl Write, value: &impl Serialize) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write(out, &format!("{text}\n"))
}

/// `text` made safe to print to a terminal.
pub fn safe(text: &str) -> String {
    display_safe(text, SHOWN_MAX).into_string()
}
