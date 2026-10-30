//! `shepherd hook`: agents' hooks deliver payloads to the host, which translates and appends them. On the fake
//! ports through the scenario harness, plus the real binary against a real host in a tempdir.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use shepherd::hook::{self, HookError};
use shepherd_core::acpp::acp::AcpFrame;
use shepherd_core::acpp::{Frame, RequestId, Seq};
use shepherd_core::config::AgentKind;
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{EventLog, LinkEvent, LinkEventSink, LogId};
use shepherd_core::session::{Activity, SessionState};
use shepherd_core::wire::{ClientRequest, HookFrame, WireError};
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use shepherd_testkit::fakes::pty::ProgramInput;
use support::{HostHarness, Program, SHELL, new_session_id, response};

/// A liveness bound for hook threads in scenarios, never reached when the host is stepped.
const GENEROUS: Duration = Duration::from_secs(60);

fn prompt(text: &str) -> Value {
    json!({ "hook_event_name": "UserPromptSubmit", "session_id": "c1", "prompt": text })
}

fn stop() -> Value {
    json!({ "hook_event_name": "Stop", "session_id": "c1" })
}

fn claude_hook(session: &SessionId, payload: Value) -> HookFrame {
    HookFrame {
        session: session.clone(),
        agent: AgentKind::ClaudeCode,
        payload,
    }
}

/// The session's activity, folded from its log as a client would.
fn activity(h: &HostHarness, session: &SessionId) -> Activity {
    let mut state = SessionState::default();
    for env in h
        .log()
        .read(LogId::Session(session.clone()), Seq::ZERO)
        .unwrap()
    {
        state.apply(&env);
    }
    state.activity().clone()
}

type Hooks = Arc<Mutex<Vec<JoinHandle<Result<(), HookError>>>>>;

/// A fake agent: typing `prompt <session>` or `stop <session>` makes it run its hook, which calls
/// `shepherd hook` on its own thread, like a separate hook process.
fn fake_agent(net: &FakeNetwork, hooks: &Hooks) -> Program {
    let (net, hooks) = (net.clone(), hooks.clone());
    Arc::new(move |input, _| {
        let ProgramInput::Stdin(bytes) = input else {
            return;
        };
        let line = String::from_utf8_lossy(bytes);
        let Some((what, session)) = line.trim_end().split_once(' ') else {
            return;
        };
        let payload = match what {
            "prompt" => prompt("hi"),
            "stop" => stop(),
            _ => return,
        };
        let frame = claude_hook(&SessionId::new(session), payload);
        let net = net.clone();
        let handle = std::thread::spawn(move || {
            hook::deliver(
                Box::new(move |sink| net.connect(sink).map_err(|e| e.to_string())),
                frame,
                GENEROUS,
            )
        });
        hooks.lock().unwrap().push(handle);
    })
}

/// Steps the host until every hook thread the fake agent started has its answer.
fn run_hooks(h: &mut HostHarness, hooks: &Hooks) -> Vec<Result<(), HookError>> {
    let handles: Vec<_> = hooks.lock().unwrap().drain(..).collect();
    let mut results = Vec::new();
    for handle in handles {
        while !handle.is_finished() {
            h.step();
            std::thread::yield_now();
        }
        results.push(handle.join().unwrap());
    }
    h.step();
    results
}

#[test]
fn an_agent_in_a_pty_running_its_hooks_changes_the_sessions_activity() {
    let net = FakeNetwork::new();
    let hooks = Hooks::default();
    let mut h = HostHarness::builder()
        .network(net.clone())
        .program(SHELL, fake_agent(&net, &hooks))
        .start();
    let mut client = h.connect();
    let id = client.new_session();
    h.step();
    let session = new_session_id(&client.take(), &id);
    assert_eq!(activity(&h, &session), Activity::Starting);

    client.input(&session, format!("prompt {session}\r").as_bytes());
    h.step();
    assert_eq!(run_hooks(&mut h, &hooks), vec![Ok(())]);
    assert_eq!(activity(&h, &session), Activity::Working { tool: None });

    client.input(&session, format!("stop {session}\r").as_bytes());
    h.step();
    assert_eq!(run_hooks(&mut h, &hooks), vec![Ok(())]);
    assert_eq!(activity(&h, &session), Activity::Idle);
}

/// The ids of the synthesized `session/prompt` requests in `session`'s log.
fn prompt_ids(h: &HostHarness, session: &SessionId) -> Vec<RequestId> {
    h.log()
        .read(LogId::Session(session.clone()), Seq::ZERO)
        .unwrap()
        .iter()
        .filter_map(|env| match env.frame() {
            Frame::Acp(m) if matches!(m.body(), AcpFrame::Prompt(_)) => m.id().cloned(),
            _ => None,
        })
        .collect()
}

#[test]
fn hook_request_ids_do_not_collide_across_a_host_restart() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    let id = client.new_session();
    h.step();
    let session = new_session_id(&client.take(), &id);

    let sent = client.request(ClientRequest::HookFrame(Box::new(claude_hook(
        &session,
        prompt("one"),
    ))));
    h.step();
    assert_eq!(response(&client.take(), &sent), Ok(json!({})));

    h.restart();
    let mut client = h.connect();
    let sent = client.request(ClientRequest::HookFrame(Box::new(claude_hook(
        &session,
        prompt("two"),
    ))));
    h.step();
    assert_eq!(response(&client.take(), &sent), Ok(json!({})));

    let ids = prompt_ids(&h, &session);
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn a_hook_for_an_unknown_session_is_refused_and_appends_nothing() {
    let mut h = HostHarness::new();
    let mut client = h.connect();
    let sent = client.request(ClientRequest::HookFrame(Box::new(claude_hook(
        &SessionId::new("s_nope"),
        stop(),
    ))));
    h.step();
    assert_eq!(
        response(&client.take(), &sent),
        Err(WireError::UnknownSession {
            session: SessionId::new("s_nope")
        })
    );
    assert!(h.log().logs().unwrap().is_empty());
}

#[test]
fn delivering_with_no_host_fails_without_waiting() {
    let net = FakeNetwork::new();
    let result = hook::deliver(
        Box::new(move |sink| net.connect(sink).map_err(|e| e.to_string())),
        claude_hook(&SessionId::new("s_1"), stop()),
        GENEROUS,
    );
    assert!(
        matches!(result, Err(HookError::Unreachable(_))),
        "{result:?}"
    );
}

/// Forwards a fake host link's events into a channel the test may or may not read.
struct ChannelSink(Mutex<std::sync::mpsc::Sender<LinkEvent>>);

impl LinkEventSink for ChannelSink {
    fn send(&self, event: LinkEvent) {
        let _ = self.0.lock().unwrap().send(event);
    }
}

/// A host that accepts connections and reads lines, and never answers on its own.
fn silent_host(net: &FakeNetwork) -> (FakeLink, std::sync::mpsc::Receiver<LinkEvent>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let link = net.listen(Arc::new(ChannelSink(Mutex::new(tx)))).unwrap();
    (link, rx)
}

fn deliver_on(net: &FakeNetwork, frame: HookFrame, timeout: Duration) -> Result<(), HookError> {
    let net = net.clone();
    hook::deliver(
        Box::new(move |sink| net.connect(sink).map_err(|e| e.to_string())),
        frame,
        timeout,
    )
}

#[test]
fn a_host_that_never_answers_times_the_hook_out() {
    let net = FakeNetwork::new();
    let (_host, _events) = silent_host(&net);
    // On its own thread, so a hook that ignores its timeout fails the test (after GENEROUS) instead of hanging it.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(deliver_on(
            &net,
            claude_hook(&SessionId::new("s_1"), stop()),
            Duration::from_millis(10),
        ));
    });
    let result = rx
        .recv_timeout(GENEROUS)
        .expect("the hook gave up waiting for the host");
    assert_eq!(result, Err(HookError::Timeout));
}

#[test]
fn a_host_that_drops_the_connection_before_answering_is_a_disconnect() {
    let net = FakeNetwork::new();
    let (host, events) = silent_host(&net);
    // The fake network drops a conn's sinks with it; a socket link keeps its sink until the link is dropped. Hold
    // a copy, as that link would, so the hook learns of the drop from the `Disconnected` event alone.
    let kept: Arc<Mutex<Option<Arc<dyn LinkEventSink>>>> = Arc::default();
    let hook = {
        let (net, kept) = (net.clone(), kept.clone());
        std::thread::spawn(move || {
            hook::deliver(
                Box::new(move |sink: Arc<dyn LinkEventSink>| {
                    *kept.lock().unwrap() = Some(sink.clone());
                    net.connect(sink).map_err(|e| e.to_string())
                }),
                claude_hook(&SessionId::new("s_1"), stop()),
                GENEROUS,
            )
        })
    };
    // Once the hook's request has arrived, the host goes away without answering.
    loop {
        match events
            .recv_timeout(GENEROUS)
            .expect("the hook connects and sends")
        {
            LinkEvent::Line { .. } => break,
            _ => continue,
        }
    }
    drop(host);
    assert_eq!(hook.join().unwrap(), Err(HookError::Disconnected));
}

#[test]
fn a_hook_the_host_refuses_is_reported_as_refused() {
    let net = FakeNetwork::new();
    let mut h = HostHarness::builder().network(net.clone()).start();
    let hook = {
        let net = net.clone();
        std::thread::spawn(move || {
            deliver_on(
                &net,
                claude_hook(&SessionId::new("s_nope"), stop()),
                GENEROUS,
            )
        })
    };
    while !hook.is_finished() {
        h.step();
        std::thread::yield_now();
    }
    let result = hook.join().unwrap();
    assert!(matches!(result, Err(HookError::Refused(_))), "{result:?}");
}

#[test]
fn a_hook_call_reads_the_session_and_any_payload() {
    let frame = hook::hook_frame("claude-code", Some("s_1"), br#"{"hook_event_name":"Stop"}"#);
    assert_eq!(
        frame,
        Some(claude_hook(
            &SessionId::new("s_1"),
            json!({ "hook_event_name": "Stop" })
        ))
    );
    let raw = hook::hook_frame("claude-code", Some("s_1"), b"not json").unwrap();
    assert_eq!(raw.payload, json!("not json"));
    assert_eq!(hook::hook_frame("claude-code", None, b"{}"), None);
    assert_eq!(hook::hook_frame("robot", Some("s_1"), b"{}"), None);
}

// ---------- the real binary ----------

mod real {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};

    use shepherd::host::{self, RuntimeEvent, SOCKET_FILE};
    use shepherd_adapters::JsonlEventLog;
    use shepherd_core::acpp::Seq;
    use shepherd_core::ids::SessionId;
    use shepherd_core::ports::{EventLog, LogId};
    use shepherd_core::wire::{
        ClientRequest, HostMessage, Request, SessionNew, decode_host_message, encode_request,
    };

    /// Runs `shepherd hook <args>` with only the given environment, `stdin` on its stdin.
    fn run_hook(args: &[&str], env: &[(&str, &Path)], session: &str, stdin: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_shepherd"))
            .arg("hook")
            .args(args)
            .env_clear()
            .envs(env.iter().copied())
            .env("SHEPHERD_SESSION", session)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn with_no_host_running_it_exits_0_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("nobody.sock");
        let out = run_hook(
            &["claude-code"],
            &[("HOME", tmp.path()), ("SHEPHERD_SOCK", &socket)],
            "s_1",
            br#"{"hook_event_name":"Stop"}"#,
        );
        assert!(out.status.success(), "{out:?}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
    }

    #[test]
    fn misuse_exits_0_silently() {
        let tmp = tempfile::tempdir().unwrap();
        for args in [&[][..], &["robot"][..]] {
            let out = run_hook(args, &[("HOME", tmp.path())], "s_1", b"{}");
            assert!(out.status.success(), "{out:?}");
            assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
        }
    }

    /// Asks the host on `socket` for a new `shell` session and returns its id. Never the default profile, which
    /// would run the real `claude`.
    fn new_session(socket: &Path) -> SessionId {
        let mut stream = UnixStream::connect(socket).unwrap();
        let request = encode_request(&Request {
            id: 1.into(),
            body: ClientRequest::SessionNew(SessionNew {
                profile: Some("shell".to_owned()),
                cwd: None,
            }),
        });
        stream.write_all(&request).unwrap();
        let mut line = Vec::new();
        BufReader::new(stream).read_until(b'\n', &mut line).unwrap();
        match decode_host_message(line.strip_suffix(b"\n").unwrap()).unwrap() {
            HostMessage::Response(r) => {
                SessionId::new(r.result.unwrap()["session"].as_str().unwrap())
            }
            other => panic!("expected the session/new answer, got {other:?}"),
        }
    }

    #[test]
    fn it_delivers_the_payload_to_a_running_host_which_appends_its_frames() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("shepherd");
        let mut host = host::open(&root, PathBuf::from("/"), Some("/bin/sh".into())).unwrap();
        let tx = host.sender();
        let socket = root.join(SOCKET_FILE);
        let home = tmp.path().to_path_buf();
        let client = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                let session = new_session(&socket);
                let payload =
                    br#"{"hook_event_name":"UserPromptSubmit","session_id":"c1","prompt":"hi"}"#;
                let out = run_hook(
                    &["claude-code"],
                    &[
                        ("HOME", home.as_path()),
                        ("SHEPHERD_SOCK", socket.as_path()),
                    ],
                    session.as_str(),
                    payload,
                );
                (session, out)
            });
            tx.send(RuntimeEvent::Shutdown);
            result
        });
        host.run();
        drop(host);
        let (session, out) = client
            .join()
            .unwrap()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));
        assert!(out.status.success(), "{out:?}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");

        let log = JsonlEventLog::open(&root).unwrap();
        let methods: Vec<String> = log
            .read(LogId::Session(session), Seq::ZERO)
            .unwrap()
            .iter()
            .filter_map(|env| env.frame().method().map(str::to_owned))
            .collect();
        assert!(methods.iter().any(|m| m == "session/prompt"), "{methods:?}");
    }
}
