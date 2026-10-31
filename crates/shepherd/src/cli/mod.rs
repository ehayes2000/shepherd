//! The command-line client for scripts and agents (T46, decision 0022): `shepherd sessions`, `shepherd new`, and the
//! session commands built on them. `docs/cli.md` documents each command and its `--json` shape.
//!
//! A command is a short-lived client of the local host (launching it if none is running), over the same link and
//! tunnels as the TUI: it runs the client's own core ([`Client`]) headless through a [`Drive`], so it folds the same
//! state the sidebar shows instead of parsing logs a second way. It waits until every reachable host has sent its
//! sessions ([`settle`]), asks what it needs ([`request`]), and names sessions the one way every command does
//! ([`target`]). Human output is display-safe; errors go to stderr.

pub mod sessions;

use std::io::Write;
use std::time::Duration;

use serde::Serialize;
use shepherd_core::client::automation::Target;
use shepherd_core::client::{AskId, Client, ClientSession};
use shepherd_core::display::display_safe;
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{HostLauncher, Link, LinkEventSink};
use shepherd_core::wire::{ClientRequest, HostResult};

use crate::client::{ClientRuntime, SocketRuntime, socket_runtime};

/// How long reaching the hosts may take: launching the local one, and each peer's tunnel and catch-up.
pub const SETTLE_LIMIT: Duration = Duration::from_secs(60);
/// How long a host has to answer a request.
pub const ANSWER_LIMIT: Duration = Duration::from_secs(60);
/// The most grapheme clusters of one field printed.
const SHOWN_MAX: usize = 512;

/// A running client, as a command drives it.
pub trait Drive {
    fn client(&self) -> &Client;

    /// Asks `machine`'s host (this machine's for `None`) `request`. Read the answer with [`Client::answer`].
    fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId;

    /// Runs the client until `done` holds of it. Fails saying why if that takes longer than `limit`, or the client
    /// gives up.
    fn wait(
        &mut self,
        done: &mut dyn FnMut(&Client) -> bool,
        limit: Duration,
    ) -> Result<(), String>;
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

    fn ask(&mut self, machine: Option<MachineId>, request: ClientRequest) -> AskId {
        ClientRuntime::ask(self, machine, request)
    }

    fn wait(
        &mut self,
        done: &mut dyn FnMut(&Client) -> bool,
        limit: Duration,
    ) -> Result<(), String> {
        self.run_until(done, limit).map_err(|e| e.to_string())
    }
}

/// A started client of the local host at `~/.shepherd/host.sock`.
pub fn connect() -> Result<SocketRuntime, String> {
    let mut runtime = socket_runtime(&crate::peers::socket()?).map_err(|e| e.to_string())?;
    runtime.start();
    Ok(runtime)
}

/// Waits until every host the client can reach has sent all its sessions ([`Client::settled`]).
pub fn settle(drive: &mut impl Drive) -> Result<(), String> {
    drive.wait(&mut Client::settled, SETTLE_LIMIT)
}

/// Asks `machine`'s host (this machine's for `None`) `request` and waits for its answer: the host's refusal, or why
/// it couldn't be asked, is the error.
pub fn request(
    drive: &mut impl Drive,
    machine: Option<MachineId>,
    request: ClientRequest,
) -> Result<HostResult, String> {
    let ask = drive.ask(machine, request);
    drive.wait(&mut |c| c.answer(ask).is_some(), ANSWER_LIMIT)?;
    match drive.client().answer(ask) {
        Some(Ok(result)) => Ok(result.clone()),
        Some(Err(e)) => Err(e.to_string()),
        None => Err("the shepherd host didn't answer".to_owned()),
    }
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
