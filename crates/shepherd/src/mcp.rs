//! `shepherd mcp` (T53, decision 0022): shepherd's command-line tools for an agent, served as MCP over stdio.
//!
//! Each tool is one CLI command, run by the very code the command runs (`crate::cli`), with `--json`: its result is
//! that command's JSON (as structured content, and as text), and its error is the command's error (`isError`). So a
//! tool can do nothing the CLI can't: `status` is `shepherd sessions`, `read_pane` is `screen`, `send` is `prompt`,
//! `answer` is `answer`, `new_session` is `new` and `stop` is `stop`. `docs/cli.md` documents each tool's input and
//! output next to its command.
//!
//! Like each command, each call is a short-lived client of this machine's host ([`Connect`]), started for the call and
//! dropped after it. Calls are served one at a time, in order: a `send` that waits holds the next call until it is done.
//! What a tool types into a session (`send`, `answer`) is recorded with who asked: `mcp:<session>`, the session that
//! launched the server, named by `SHEPHERD_SESSION` (which shepherd sets in every session it spawns), or `mcp` when it
//! wasn't launched from one.

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use rmcp::handler::server::common::{schema_for_output, schema_for_type};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::transport::IntoTransport;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use shepherd_core::acpp::{Choice, PromptBy, Seq};
use shepherd_core::ids::SessionId;

use crate::cli::answer::{Answer, AnsweredJson, answer_by, choice_schema};
use crate::cli::prompt::{OutcomeJson, Prompt, prompt_by};
use crate::cli::screen::{ShownJson, screen};
use crate::cli::sessions::{MadeJson, New, SessionsJson, new, sessions};
use crate::cli::stop::{Stop, StoppedJson, stop};
use crate::cli::{CliError, Drive};

/// The variable that names the session the server runs in: shepherd sets it in every session it spawns.
pub use crate::hook::SESSION_VAR as CALLER_VAR;

const USAGE: &str = "usage: shepherd mcp";

/// Where each tool call gets its client of this machine's host.
pub trait Connect {
    /// A client for one call, as each CLI command starts one.
    fn connect(&mut self) -> Result<Box<dyn Drive + '_>, String>;
}

/// This machine's host, at `~/.shepherd/host.sock` (launched if it isn't running), as every CLI command reaches it.
pub struct LocalHost;

impl Connect for LocalHost {
    fn connect(&mut self) -> Result<Box<dyn Drive + '_>, String> {
        Ok(Box::new(crate::cli::connect()?))
    }
}

/// Who a session's log says typed what a tool typed: the session named by `caller` (the value of [`CALLER_VAR`]), or
/// just `mcp` when there is none.
pub fn caller(caller: Option<&str>) -> PromptBy {
    match caller.map(str::trim) {
        Some(session) if !session.is_empty() => PromptBy::Mcp(SessionId::new(session)),
        _ => PromptBy::Other("mcp".to_owned()),
    }
}

/// The MCP server: shepherd's tools, each run with a client from `C`.
pub struct Server<C> {
    connect: RefCell<C>,
    by: PromptBy,
    /// What a relative `new_session` `cwd` is relative to: where the server runs.
    here: PathBuf,
}

impl<C: Connect> Server<C> {
    pub fn new(connect: C, by: PromptBy, here: PathBuf) -> Self {
        Self {
            connect: RefCell::new(connect),
            by,
            here,
        }
    }

    /// Runs tool `name` with `arguments`: its command's JSON, or its error as a tool error. Arguments that don't fit
    /// the tool's input schema, or a tool that doesn't exist, are the protocol's invalid-params error.
    fn call(&self, name: &str, arguments: Option<JsonObject>) -> Result<CallToolResult, ErrorData> {
        let arguments = Value::Object(arguments.unwrap_or_default());
        let mut connect = self.connect.borrow_mut();
        let mut out = Vec::new();
        let ran: Result<(), CliError> = match name {
            "status" => {
                let StatusArgs {} = args(arguments)?;
                run(&mut *connect, |d| Ok(sessions(d, true, &mut out)?))
            }
            "read_pane" => {
                let a: ReadPaneArgs = args(arguments)?;
                run(&mut *connect, |d| {
                    screen(d, &a.session, a.lines, true, &mut out)
                })
            }
            "send" => {
                let a: SendArgs = args(arguments)?;
                let prompt = Prompt {
                    session: a.session,
                    text: a.text,
                    wait: a.wait,
                    queue: a.queue,
                    timeout: a.timeout.map(Duration::from_secs),
                };
                let by = self.by.clone();
                run(&mut *connect, |d| prompt_by(d, &prompt, by, true, &mut out))
            }
            "answer" => {
                let a: AnswerArgs = args(arguments)?;
                let answer = Answer {
                    session: a.session,
                    choice: a.choice,
                    asked: a.asked.map(Seq::new),
                };
                let by = self.by.clone();
                run(&mut *connect, |d| answer_by(d, &answer, by, true, &mut out))
            }
            "new_session" => {
                let a: NewSessionArgs = args(arguments)?;
                let asked = New {
                    profile: a.profile,
                    machine: a.machine,
                    cwd: a.cwd,
                    name: a.name,
                };
                let here = self.here.clone();
                run(&mut *connect, |d| {
                    Ok(new(d, &asked, &here, true, &mut out)?)
                })
            }
            "stop" => {
                let a: StopArgs = args(arguments)?;
                let asked = Stop {
                    session: a.session,
                    force: a.force,
                };
                run(&mut *connect, |d| stop(d, &asked, true, &mut out))
            }
            other => {
                return Err(ErrorData::invalid_params(
                    format!("no tool is called `{other}`"),
                    None,
                ));
            }
        };
        Ok(match ran {
            Ok(()) => match serde_json::from_slice::<Value>(&out) {
                Ok(value) => {
                    let text = String::from_utf8_lossy(&out).trim_end().to_owned();
                    let mut result = CallToolResult::structured(value);
                    result.content = vec![ContentBlock::text(text)];
                    result
                }
                Err(e) => failed(format!("the command printed no JSON: {e}")),
            },
            Err(e) => failed(e.why().to_owned()),
        })
    }
}

/// Runs `command` with a client from `connect`: its own failure to connect is the command's error.
fn run<C: Connect + ?Sized>(
    connect: &mut C,
    command: impl FnOnce(&mut Box<dyn Drive + '_>) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut drive = connect.connect()?;
    command(&mut drive)
}

/// A tool's error, saying why.
fn failed(why: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(why)])
}

/// A tool's `arguments`, read as its input type.
fn args<T: DeserializeOwned>(arguments: Value) -> Result<T, ErrorData> {
    serde_json::from_value(arguments).map_err(|e| ErrorData::invalid_params(e.to_string(), None))
}

// ---- tools: inputs (docs/cli.md) ----

/// `status` takes nothing.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadPaneArgs {
    /// The session, by id or by its name when no other session has it.
    session: String,
    /// Up to this many lines of scrollback above the screen (at most 1000).
    #[serde(default)]
    lines: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    /// The session, by id or by its name when no other session has it.
    session: String,
    /// The prompt, typed into the agent as one paste, then Enter.
    text: String,
    /// Wait for the prompt's turn to end, and return the agent's reply (or what it asked the user).
    #[serde(default)]
    wait: bool,
    /// Wait for the agent to be at its prompt before sending, instead of being refused as busy.
    #[serde(default)]
    queue: bool,
    /// How many seconds `wait` and `queue` may take together; without it, as long as it takes.
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AnswerArgs {
    /// The session, by id or by its name when no other session has it.
    session: String,
    /// `yes`, `always` or `no` for a permission; an option's number (from 1), or `no`, for a question.
    #[schemars(schema_with = "choice_schema")]
    choice: Choice,
    /// The question meant: its `seq`, as `status` shows it. Without it, the question pending when the tool looks.
    #[serde(default)]
    asked: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NewSessionArgs {
    /// The profile to start (the host's default without it).
    #[serde(default)]
    profile: Option<String>,
    /// The machine to start it on, by id or name (this one without it).
    #[serde(default)]
    machine: Option<String>,
    // Relative to where the server runs; on another machine it must be absolute.
    /// Run it in this directory, with no worktree; without it, it gets a managed worktree in the host's workspace.
    #[serde(default)]
    cwd: Option<String>,
    /// What to call it (and its worktree's branch).
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StopArgs {
    /// The session, by id or by its name when no other session has it.
    session: String,
    /// Stop it even though its worktree has unsaved work (or that can't be told), keeping the worktree.
    #[serde(default)]
    force: bool,
}

/// One tool: its name, what it does, what it takes and returns, and whether it only reads.
fn tool<I: JsonSchema + 'static, O: JsonSchema + 'static>(
    name: &'static str,
    description: &'static str,
    read_only: bool,
) -> Tool {
    let annotations = ToolAnnotations::new()
        .read_only(read_only)
        .destructive(name == "stop");
    Tool::new(name, description, schema_for_type::<I>())
        .with_raw_output_schema(schema_for_output::<O>())
        .with_annotations(annotations)
}

/// Every tool, in the order `docs/cli.md` lists them.
pub fn tools() -> Vec<Tool> {
    vec![
        tool::<StatusArgs, SessionsJson>(
            "status",
            "Every session on this machine and on each paired machine shepherd can reach, newest first, with its \
             status (running, idle, wants-user, exited) and the question it waits on, if any. As `shepherd sessions --json`.",
            true,
        ),
        tool::<ReadPaneArgs, ShownJson>(
            "read_pane",
            "What a session's terminal shows now, as text, one string per row, with optional scrollback above. As \
             `shepherd screen --json`.",
            true,
        ),
        tool::<SendArgs, OutcomeJson>(
            "send",
            "Types a prompt into a session's agent, only while it is at its prompt (or, with queue, once it is). With \
             wait, returns once its turn ends: its reply, or what it asked the user (status wants-user). As \
             `shepherd prompt --json`.",
            false,
        ),
        tool::<AnswerArgs, AnsweredJson>(
            "answer",
            "Answers the permission prompt or question a session's agent waits on (status wants-user), naming it by \
             the seq `status` shows, so it can't land on another. Refused, typing nothing, when that question isn't \
             pending. As `shepherd answer --json`.",
            false,
        ),
        tool::<NewSessionArgs, MadeJson>(
            "new_session",
            "Starts a session on this or a paired machine, and returns it once its host has made it. As \
             `shepherd new --json`.",
            false,
        ),
        tool::<StopArgs, StoppedJson>(
            "stop",
            "Stops (closes) a session, removing its worktree when that loses nothing. Refused when its worktree has \
             unsaved work, unless force, which stops it and keeps the worktree. As `shepherd stop --json`.",
            false,
        ),
    ]
}

impl<C: Connect + 'static> ServerHandler for Server<C> {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("shepherd", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Shepherd runs coding-agent sessions across machines. `status` lists them; `send` prompts one \
                 (`wait` for its reply); `answer` answers what it asked; `read_pane` reads its terminal; \
                 `new_session` and `stop` start and stop them. Name a session by its id.",
            )
    }

    fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(tools())))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> + '_ {
        std::future::ready(
            self.call(&request.name, request.arguments)
                .map(CallToolResponse::from),
        )
    }
}

/// Serves `server` over `transport` until the client hangs up. Call within a [`tokio::task::LocalSet`]: a tool call
/// runs its client on this thread.
pub async fn serve<C, T, E, A>(server: Server<C>, transport: T) -> Result<(), String>
where
    C: Connect + 'static,
    T: IntoTransport<RoleServer, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let running = server
        .serve(transport)
        .await
        .map_err(|e| format!("starting the MCP session: {e}"))?;
    running
        .waiting()
        .await
        .map(|_| ())
        .map_err(|e| format!("serving MCP: {e}"))
}

/// `shepherd mcp`: serves the tools on stdin and stdout until the client hangs up.
pub fn main(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(USAGE.to_owned());
    }
    let here = std::env::current_dir().map_err(|e| format!("the current directory: {e}"))?;
    let by = caller(std::env::var(CALLER_VAR).ok().as_deref());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e: io::Error| e.to_string())?;
    let local = tokio::task::LocalSet::new();
    let server = Server::new(LocalHost, by, here);
    local.block_on(&runtime, serve(server, rmcp::transport::stdio()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_is_by_the_session_the_server_was_launched_from_or_just_mcp() {
        assert_eq!(caller(Some("s_pm")), PromptBy::Mcp(SessionId::new("s_pm")));
        assert_eq!(String::from(caller(Some("s_pm"))), "mcp:s_pm");
        assert_eq!(String::from(caller(None)), "mcp");
        assert_eq!(String::from(caller(Some(""))), "mcp");
    }
}
