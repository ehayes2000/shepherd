//! The host updating itself in place (T36b, decision 0018), on fakes: answering `_shepherd/host/upgrade`, handing its
//! sessions over, and a new image adopting them. The exec itself, and the real adapters, are in `hotswap_e2e.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use shepherd::host::PreflightRunner;
use shepherd_core::acpp::{Frame, ShepherdFrame};
use shepherd_core::config::AgentKind;
use shepherd_core::host::upgrade::Preflight;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::wire::{
    BuildId, ClientRequest, DigestHasher, ExeDigest, Hello, HookFrame, HostUpgrade,
    PROTOCOL_VERSION, UpgradeAnswer,
};
use shepherd_testkit::fakes::pty::ProgramInput;
use support::{FakeClient, HostHarness, Program, SHELL, entries, new_session_id, output, response};

fn digest(bytes: &[u8]) -> ExeDigest {
    let mut hasher = DigestHasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// A host started as one build, with another one installed at its path since.
struct Stale {
    h: HostHarness,
    _dir: tempfile::TempDir,
}

fn stale_with(preflight: Option<PreflightRunner>, program: Option<Program>) -> Stale {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    std::fs::write(&exe, b"a newer build").unwrap();
    let running = BuildId {
        version: "0.1.0".into(),
        digest: digest(b"the build the host started as"),
    };
    let mut builder = HostHarness::builder().build(running, &exe);
    if let Some(preflight) = preflight {
        builder = builder.preflight(preflight);
    }
    if let Some(program) = program {
        builder = builder.program(SHELL, program);
    }
    Stale {
        h: builder.start(),
        _dir: dir,
    }
}

fn stale() -> Stale {
    stale_with(None, None)
}

fn upgrade(h: &mut HostHarness, client: &mut FakeClient) -> UpgradeAnswer {
    let req = client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    h.step();
    let answer = response(&client.take(), &req).expect("upgrade answered");
    serde_json::from_value(answer).unwrap()
}

/// A session running the echo shell, which has echoed `typed`.
fn session_with(h: &mut HostHarness, client: &mut FakeClient, typed: &[u8]) -> SessionId {
    let req = client.new_session();
    h.step();
    let session = new_session_id(&client.take(), &req);
    client.input(&session, typed);
    h.step();
    session
}

fn exits_of(h: &HostHarness, session: &SessionId) -> usize {
    entries_of_log(h, session)
        .iter()
        .filter(|f| matches!(f, Some(ShepherdFrame::AgentExited(_))))
        .count()
}

fn entries_of_log(h: &HostHarness, session: &SessionId) -> Vec<Option<ShepherdFrame>> {
    use shepherd_core::acpp::Seq;
    use shepherd_core::ports::{EventLog, LogId};
    h.log()
        .read(LogId::Session(session.clone()), Seq::ZERO)
        .unwrap()
        .into_iter()
        .map(|e| match e.frame() {
            Frame::Shepherd(m) => Some(m.body().clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_host_running_what_is_installed_has_nothing_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    std::fs::write(&exe, b"the build").unwrap();
    let running = BuildId {
        version: "0.1.0".into(),
        digest: digest(b"the build"),
    };
    let mut h = HostHarness::builder().build(running, &exe).start();
    let mut client = h.connect();
    assert_eq!(upgrade(&mut h, &mut client), UpgradeAnswer::NothingToDo);
    assert!(h.runtime_mut().take_swap().is_none());
}

#[test]
fn a_stale_host_accepts_closes_every_connection_and_hands_over_its_sessions() {
    let Stale { mut h, _dir } = stale();
    let mut client = h.connect();
    let mut watcher = h.connect();
    watcher.subscribe_all();
    let session = session_with(&mut h, &mut client, b"hello");

    assert_eq!(upgrade(&mut h, &mut client), UpgradeAnswer::Accepted);
    let plan = h.runtime_mut().take_swap().expect("a swap is ready");
    // The caller closes the conns once it has stopped accepting new ones.
    assert!(!watcher.is_disconnected());
    h.runtime_mut().close_conns();
    assert!(client.is_disconnected());
    assert!(watcher.is_disconnected());

    assert_eq!(plan.sessions.len(), 1);
    let carried = &plan.sessions[0];
    assert_eq!(carried.id, session);
    assert_eq!((carried.cols, carried.rows), (80, 24));
    assert!(
        String::from_utf8_lossy(&carried.snapshot).contains("hello"),
        "{:?}",
        String::from_utf8_lossy(&carried.snapshot)
    );
}

#[test]
fn a_preflight_killed_by_a_signal_is_unavailable_and_changes_nothing() {
    let killed = PreflightRunner::new(|_| Preflight::Signaled(9));
    let Stale { mut h, _dir } = stale_with(Some(killed), None);
    let mut client = h.connect();
    client.subscribe_all();
    let session = session_with(&mut h, &mut client, b"before");

    match upgrade(&mut h, &mut client) {
        UpgradeAnswer::Unavailable { reason } => {
            assert!(
                reason.contains("overwritten in place? reinstall it"),
                "{reason}"
            )
        }
        other => panic!("expected unavailable, got {other:?}"),
    }
    assert!(!client.is_disconnected());
    assert!(h.runtime_mut().take_swap().is_none());
    client.input(&session, b"after");
    h.step();
    assert!(String::from_utf8_lossy(&output(&client.take(), &session)).contains("after"));
}

#[test]
fn the_preflight_runs_the_binary_at_the_startup_path() {
    let seen = Arc::new(std::sync::Mutex::new(None));
    let record = seen.clone();
    let runner = PreflightRunner::new(move |exe| {
        *record.lock().unwrap() = Some(exe.to_path_buf());
        Preflight::TimedOut
    });
    let Stale { mut h, _dir } = stale_with(Some(runner), None);
    let mut client = h.connect();
    upgrade(&mut h, &mut client);
    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some(Path::new(support::SHEPHERD_EXE))
    );
}

#[test]
fn an_upgrade_while_a_worktree_call_runs_is_busy() {
    let Stale { mut h, _dir } = stale();
    let hold = h.worktrees().hold_next();
    let mut client = h.connect();
    client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("laptop"),
        cwd: Some(env!("CARGO_MANIFEST_DIR").into()),
        build: None,
    }));
    h.poll();
    assert!(hold.wait_entered(Duration::from_secs(10)));

    let req = client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    h.poll();
    let answer: UpgradeAnswer =
        serde_json::from_value(response(&client.take(), &req).unwrap()).unwrap();
    assert!(matches!(answer, UpgradeAnswer::Busy { .. }), "{answer:?}");
    hold.release();
    h.step();
}

#[test]
fn the_new_image_keeps_its_sessions_running_with_their_screens() {
    let Stale { mut h, _dir } = stale();
    let mut client = h.connect();
    let session = session_with(&mut h, &mut client, b"hello");
    upgrade(&mut h, &mut client);

    h.swap(|_| {});

    assert_eq!(exits_of(&h, &session), 0);
    let mut client = h.connect();
    client.subscribe_all();
    h.step();
    let caught_up = client.take();
    assert!(String::from_utf8_lossy(&output(&caught_up, &session)).contains("hello"));
    client.input(&session, b"again");
    h.step();
    assert!(String::from_utf8_lossy(&output(&client.take(), &session)).contains("again"));
    let listed = support::fold(&entries(&caught_up)).sessions();
    let listed = listed.iter().find(|s| s.id == session).expect("listed");
    assert!(!listed.exited);
}

#[test]
fn an_adopted_session_is_nudged_to_redraw_at_its_carried_size() {
    let program: Program = Arc::new(|input, p| match input {
        ProgramInput::Resized => {
            let size = p.size();
            p.print(format!("[size {}x{}]", size.cols, size.rows));
        }
        ProgramInput::Stdin(bytes) => p.print(bytes),
        ProgramInput::Start => {}
    });
    let Stale { mut h, _dir } = stale_with(None, Some(program));
    let mut client = h.connect();
    let session = session_with(&mut h, &mut client, b"hi");
    upgrade(&mut h, &mut client);

    h.swap(|_| {});

    let size = h.runtime().pty().size(&session).expect("running");
    assert_eq!((size.cols, size.rows), (80, 24));
    let mut client = h.connect();
    client.subscribe_all();
    h.step();
    let screen = String::from_utf8_lossy(&output(&client.take(), &session)).into_owned();
    assert!(screen.contains("[size 80x24]"), "{screen:?}");
}

#[test]
fn a_session_the_new_image_cannot_adopt_is_recorded_as_exited() {
    let Stale { mut h, _dir } = stale();
    let mut client = h.connect();
    let session = session_with(&mut h, &mut client, b"hello");
    upgrade(&mut h, &mut client);

    h.swap(|plan| {
        let carried = plan.sessions.remove(0);
        plan.sessions.push(shepherd::host::CarriedSession {
            token: carried.token.unadoptable(),
            ..carried
        });
    });

    assert_eq!(exits_of(&h, &session), 1);
    assert!(!h.runtime().pty().is_running(&session));
}

#[test]
fn hook_translators_survive_the_swap() {
    let Stale { mut h, _dir } = stale();
    let mut client = h.connect();
    let session = session_with(&mut h, &mut client, b"hello");
    let prompt =
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "c1", "prompt": "hi" });
    client.request(ClientRequest::HookFrame(Box::new(HookFrame {
        session: session.clone(),
        agent: AgentKind::ClaudeCode,
        payload: prompt,
    })));
    h.step();
    assert_eq!(h.runtime().translators().len(), 1);
    upgrade(&mut h, &mut client);

    h.swap(|_| {});

    assert_eq!(h.runtime().translators().len(), 1);
}

#[test]
fn the_new_image_keeps_the_run_files_of_adopted_sessions_and_sweeps_the_rest() {
    let Stale { mut h, _dir } = stale();
    let mut client = h.connect();
    let session = session_with(&mut h, &mut client, b"hello");
    upgrade(&mut h, &mut client);
    let run_root = h.run_root().to_path_buf();
    std::fs::create_dir_all(run_root.join(session.as_str())).unwrap();
    std::fs::create_dir_all(run_root.join("s_leftover")).unwrap();

    h.swap(|_| {});

    assert!(run_root.join(session.as_str()).exists());
    assert!(!run_root.join("s_leftover").exists());
}

#[test]
fn a_swap_whose_exec_fails_resumes_its_sessions_and_says_why_next_time() {
    let Stale { mut h, _dir } = stale();
    let mut client = h.connect();
    let session = session_with(&mut h, &mut client, b"hello");
    upgrade(&mut h, &mut client);
    let plan = h.runtime_mut().take_swap().expect("a swap is ready");
    drop(plan);

    h.runtime_mut()
        .swap_failed("execv: No such file or directory".into());
    h.step();

    let mut client = h.connect();
    client.subscribe_all();
    h.step();
    client.take();
    client.input(&session, b"still here");
    h.step();
    assert!(String::from_utf8_lossy(&output(&client.take(), &session)).contains("still here"));
    match upgrade(&mut h, &mut client) {
        UpgradeAnswer::Unavailable { reason } => assert!(reason.contains("execv"), "{reason}"),
        other => panic!("expected unavailable, got {other:?}"),
    }
}

#[test]
fn an_upgrade_while_a_session_s_worktree_is_being_created_is_busy_until_it_is_done() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let app = base.join("app");
    std::fs::create_dir(&app).unwrap();
    let worktrees = shepherd_testkit::fakes::worktrees::FakeWorktrees::new(base.join("worktrees"));
    worktrees.add_repo(&app, "main");
    let exe = base.join("shepherd");
    std::fs::write(&exe, b"a newer build").unwrap();
    let running = BuildId {
        version: "0.1.0".into(),
        digest: digest(b"the build the host started as"),
    };
    let mut h = HostHarness::builder()
        .build(running, &exe)
        .worktrees(worktrees.clone())
        .start();
    let mut client = h.connect();
    client.request(ClientRequest::Hello(Hello {
        version: PROTOCOL_VERSION,
        machine: MachineId::new("laptop"),
        cwd: Some(app.to_str().unwrap().to_owned()),
        build: None,
    }));
    h.step();
    client.take();

    let hold = worktrees.hold_next();
    client.new_session();
    h.poll();
    assert!(
        hold.wait_entered(Duration::from_secs(10)),
        "the create never started"
    );
    let req = client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    h.poll();
    let answer: UpgradeAnswer =
        serde_json::from_value(response(&client.take(), &req).unwrap()).unwrap();
    assert!(matches!(answer, UpgradeAnswer::Busy { .. }), "{answer:?}");

    std::fs::create_dir_all(base.join("worktrees").join("app").join("shell-1")).unwrap();
    hold.release();
    h.step();
    assert_eq!(upgrade(&mut h, &mut client), UpgradeAnswer::Accepted);
}
