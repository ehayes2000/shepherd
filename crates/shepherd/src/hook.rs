//! `shepherd hook <agent>`: what an agent's hooks run. It reads the hook payload from stdin and the session from
//! `SHEPHERD_SESSION`, and sends them to the host as `_shepherd/hook/frame`. The host translates the payload and
//! appends the frames, so this process holds no state and makes no decisions.
//!
//! It must never get in the agent's way: it always exits 0 and prints nothing, whatever goes wrong (no session, an
//! unknown agent, no host). It waits for the host's answer, so hooks the agent runs one after another reach the
//! log in order.
//!
//! What is bounded: only the wait for the host's answer, by [`REPLY_TIMEOUT`]. Reading stdin (until the agent closes
//! it) and connecting to the socket are not bounded; a connect to a live host whose accept backlog is full can block.

use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use shepherd_adapters::UnixConnectLink;
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{ConnId, Link, LinkEvent, LinkEventSink};
use shepherd_core::wire::{
    ClientRequest, HookFrame, HostMessage, Request, decode_host_message, encode_request,
};

/// The longest `shepherd hook` waits for the host's answer before giving up.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(1);
/// The session the agent runs in, and the host socket (when unset, `~/.shepherd/host.sock`). The host sets both
/// when it launches a session.
pub use shepherd_core::host::launch::{SESSION_VAR, SOCKET_VAR};

/// Why a hook wasn't delivered. Only tests look: `shepherd hook` exits 0 regardless.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookError {
    #[error("cannot reach the host: {0}")]
    Unreachable(String),
    #[error("the host went away before answering")]
    Disconnected,
    #[error("the host didn't answer in time")]
    Timeout,
    #[error("the host refused the hook: {0}")]
    Refused(String),
}

/// The request for one hook call: the agent named on the command line, the session from the environment, and the
/// payload from stdin. `None` when the agent or session is missing or unknown. A payload that isn't JSON is sent as
/// a string, so nothing the agent wrote is lost.
pub fn hook_frame(agent: &str, session: Option<&str>, stdin: &[u8]) -> Option<HookFrame> {
    let agent = AgentKind::parse(agent)?;
    let session = session.filter(|s| !s.is_empty())?;
    let payload = serde_json::from_slice(stdin)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(stdin).into_owned()));
    Some(HookFrame {
        session: SessionId::new(session),
        agent,
        payload,
    })
}

/// Opens a link to the host, sending its events to the sink.
pub type Connect<L> = dyn FnOnce(Arc<dyn LinkEventSink>) -> Result<(L, ConnId), String>;

/// Sends `frame` to the host over a link from `connect`, and waits up to `timeout` for its answer. `connect` itself
/// is not bounded by `timeout`.
pub fn deliver<L: Link>(
    connect: Box<Connect<L>>,
    frame: HookFrame,
    timeout: Duration,
) -> Result<(), HookError> {
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel();
    let sink: Arc<dyn LinkEventSink> = Arc::new(ChannelSink(Mutex::new(tx)));
    let (mut link, conn) = connect(sink).map_err(HookError::Unreachable)?;
    let request = Request {
        id: 1.into(),
        body: ClientRequest::HookFrame(Box::new(frame)),
    };
    let line = encode_request(&request);
    link.send(conn, line.strip_suffix(b"\n").unwrap_or(&line))
        .map_err(|e| HookError::Unreachable(e.to_string()))?;
    let answer = wait_for_answer(&rx, &request, deadline);
    let _ = link.close(conn);
    answer
}

fn wait_for_answer(
    rx: &Receiver<LinkEvent>,
    request: &Request,
    deadline: Instant,
) -> Result<(), HookError> {
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(LinkEvent::Line { line, .. }) => match decode_host_message(&line) {
                Ok(HostMessage::Response(r)) if r.id == request.id => {
                    return r
                        .result
                        .map(drop)
                        .map_err(|e| HookError::Refused(e.to_string()));
                }
                _ => {}
            },
            Ok(LinkEvent::Disconnected { .. }) | Err(RecvTimeoutError::Disconnected) => {
                return Err(HookError::Disconnected);
            }
            Ok(LinkEvent::Connected { .. }) => {}
            Err(RecvTimeoutError::Timeout) => return Err(HookError::Timeout),
        }
    }
}

struct ChannelSink(Mutex<Sender<LinkEvent>>);

impl LinkEventSink for ChannelSink {
    fn send(&self, event: LinkEvent) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(event);
    }
}

/// `shepherd hook <agent>`, with the rest of argv after `hook`. Never fails: see the module docs.
pub fn main(args: &[String]) {
    let Some(agent) = args.first() else {
        return;
    };
    let mut stdin = Vec::new();
    if std::io::stdin().read_to_end(&mut stdin).is_err() {
        return;
    }
    let session = std::env::var(SESSION_VAR).ok();
    let Some(frame) = hook_frame(agent, session.as_deref(), &stdin) else {
        return;
    };
    let Some(socket) = socket_path() else {
        return;
    };
    let connect: Box<Connect<UnixConnectLink>> =
        Box::new(move |sink| UnixConnectLink::connect(&socket, sink).map_err(|e| e.to_string()));
    let _ = deliver(connect, frame, REPLY_TIMEOUT);
}

/// `$SHEPHERD_SOCK`, else `~/.shepherd/host.sock`.
fn socket_path() -> Option<PathBuf> {
    if let Some(sock) = std::env::var_os(SOCKET_VAR).filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(sock));
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".shepherd").join("host.sock"))
}
