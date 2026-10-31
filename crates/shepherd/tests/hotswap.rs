//! The host updating itself in place (T36b, decision 0018), on fakes: answering `_shepherd/host/upgrade`, handing its
//! sessions over, and a new image adopting them. The exec itself, and the real adapters, are in `hotswap_e2e.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use shepherd::host::PreflightRunner;
use shepherd_core::acpp::{Dir, Frame, FrameDraft, PeerTrusted, ShepherdFrame, Source, SourceKind};
use shepherd_core::config::AgentKind;
use shepherd_core::host::upgrade::Preflight;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{DisconnectReason, LogId};
use shepherd_core::wire::{
    BuildId, ClientRequest, DigestHasher, ExeDigest, Hello, HookFrame, HostUpgrade,
    PROTOCOL_VERSION, PeerList, Tunnel, UpgradeAnswer,
};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::link::FakeNetwork;
use shepherd_testkit::fakes::peers::FakePeerNet;
use shepherd_testkit::fakes::pty::ProgramInput;
use support::{
    FakeClient, HostHarness, Program, SHELL, entries, machine_with, new_session_id, output,
    response,
};

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

/// F47: the exec ends every link thread, so an `Accepted` still queued for a slow client when the conns close would
/// be lost. `swap` closes the conns and then drops the old link, as the exec does, losing whatever is undelivered.
/// The client reads only once the swap is waiting for it to, so without that wait it never gets the answer.
#[test]
fn a_swap_waits_for_a_slow_client_to_read_its_accepted_answer() {
    let Stale { mut h, _dir } = stale();
    let mut client = h.connect_paced();
    let req = client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    h.step();
    assert!(client.take().is_empty(), "a paced client reads nothing yet");

    let (net, conn) = (h.network().clone(), client.conn());
    let reader = std::thread::spawn(move || {
        if !net.wait_until_flushing(Duration::from_secs(10)) {
            return false;
        }
        net.deliver(conn, usize::MAX);
        true
    });
    h.swap(|_| {});
    assert!(
        reader.join().unwrap(),
        "the swap never waited for the flush"
    );

    let answer = response(&client.take(), &req).expect("upgrade answered before the exec");
    assert_eq!(
        serde_json::from_value::<UpgradeAnswer>(answer).unwrap(),
        UpgradeAnswer::Accepted
    );
    // Its `Disconnected` really came, once it had read everything: not just a close it has yet to read up to.
    assert_eq!(client.disconnect_reason(), Some(DisconnectReason::PeerGone));
}

/// The wait is bounded, in real time: a client that never reads holds the swap for about `CLOSE_FLUSH`, however
/// far the host's own clock has moved. The upper bound is a generous liveness bound, not a measurement: a deadline
/// taken from the (advanced) test clock would hold it for `CLOSE_FLUSH` plus the advance.
#[test]
fn a_swap_proceeds_at_the_bound_when_a_client_never_reads() {
    let Stale { mut h, _dir } = stale();
    let mut client = h.connect_paced();
    client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    h.step();
    let advance = Duration::from_secs(60);
    h.clock().advance(advance);

    let started = std::time::Instant::now();
    h.swap(|_| {});
    let held = started.elapsed();
    assert!(held < advance / 2, "the swap was held for {held:?}");
    assert!(
        client.take().is_empty(),
        "the undelivered answer was cut off"
    );
    // Cut off, by the link giving up at the bound or else by the exec (the old link dropped): its `Disconnected` came
    // without the lines it never read.
    assert_eq!(client.disconnect_reason(), Some(DisconnectReason::PeerGone));
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

#[test]
fn an_upgrade_while_a_hook_runs_for_a_session_waiting_to_spawn_is_busy_and_says_both() {
    use std::collections::BTreeMap;

    use shepherd_core::config::{RawConfig, RawHooks, RawWorkspace};
    use shepherd_testkit::fakes::commands::FakeCommands;

    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let app = base.join("app");
    std::fs::create_dir(&app).unwrap();
    let worktrees = shepherd_testkit::fakes::worktrees::FakeWorktrees::new(base.join("worktrees"));
    worktrees.add_repo(&app, "main");
    let commands = FakeCommands::new();
    commands.add_script("make setup", |c| c.print("ok\n"));
    let workspace = RawWorkspace {
        hooks: Some(RawHooks {
            post_create: Some(vec!["make setup".to_owned()]),
            pre_remove: None,
        }),
        hook_timeout_s: Some(90),
    };
    let config = RawConfig {
        workspaces: Some(BTreeMap::from([(
            app.to_str().unwrap().to_owned(),
            workspace,
        )])),
        ..support::shell_default_config()
    };
    let exe = base.join("shepherd");
    std::fs::write(&exe, b"a newer build").unwrap();
    let running = BuildId {
        version: "0.1.0".into(),
        digest: digest(b"the build the host started as"),
    };
    let mut h = HostHarness::builder()
        .build(running, &exe)
        .worktrees(worktrees)
        .commands(commands.clone())
        .config(config)
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

    // The worktree is made, and its hook holds the session's spawn.
    let hold = commands.hold_next();
    client.new_session();
    assert!(
        (0..500).any(|_| {
            h.runtime_mut().run_next(Duration::from_millis(20));
            hold.wait_entered(Duration::from_millis(1))
        }),
        "the hook never started"
    );
    let req = client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    h.poll();
    match serde_json::from_value(response(&client.take(), &req).unwrap()).unwrap() {
        UpgradeAnswer::Busy { reason } => {
            assert!(reason.contains("starting"), "{reason}");
            assert!(reason.contains("running"), "{reason}");
        }
        other => panic!("expected busy, got {other:?}"),
    }
    std::fs::create_dir_all(base.join("worktrees").join("app").join("shell-1")).unwrap();
    hold.release();
    h.step();
}

/// A preflight that never answers on its own (its thread waits until the test ends), so the test queues the answer
/// itself, with whatever it wants queued behind it.
fn parked_preflight() -> (PreflightRunner, std::sync::mpsc::Sender<()>) {
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    let runner = PreflightRunner::new(move |_| {
        let _ = wait.lock().unwrap().recv();
        Preflight::Failed("released at the end of the test".into())
    });
    (runner, release)
}

/// What `handoff-check` prints when it passes, as the runtime gets it.
fn passed() -> shepherd::host::RuntimeEvent {
    let support::PassingPreflight(preflight) = support::passing_preflight_result();
    shepherd::host::RuntimeEvent::Preflighted(preflight)
}

/// A stale host asked to upgrade, its preflight parked. Returns the session and the client that asked.
fn upgrade_pending() -> (Stale, FakeClient, SessionId, std::sync::mpsc::Sender<()>) {
    let (runner, release) = parked_preflight();
    let mut stale = stale_with(Some(runner), None);
    let mut client = stale.h.connect();
    let session = session_with(&mut stale.h, &mut client, b"hello");
    client.take();
    client.request(ClientRequest::HostUpgrade(HostUpgrade::default()));
    stale.h.poll();
    (stale, client, session, release)
}

#[test]
fn output_and_hook_frames_queued_when_the_upgrade_is_accepted_are_taken_in_first() {
    let (Stale { mut h, _dir }, mut client, session, _release) = upgrade_pending();
    h.runtime().sender().send(passed());
    h.runtime().sender().send(shepherd::host::RuntimeEvent::Pty(
        shepherd_core::ports::PtyEvent::Output {
            session: session.clone(),
            bytes: b"late output".to_vec(),
        },
    ));
    let prompt =
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "c1", "prompt": "hi" });
    client.request(ClientRequest::HookFrame(Box::new(HookFrame {
        session: session.clone(),
        agent: AgentKind::ClaudeCode,
        payload: prompt,
    })));
    h.poll();

    let plan = h.runtime_mut().take_swap().expect("a swap is ready");
    let snapshot = String::from_utf8_lossy(&plan.sessions[0].snapshot).into_owned();
    assert!(snapshot.contains("late output"), "{snapshot:?}");
    // The hook was translated before the handoff: its session's translator is carried.
    let carried = serde_json::to_value(&plan.translators).unwrap();
    assert!(
        carried["sessions"].get(session.as_str()).is_some(),
        "the hook frame wasn't taken in: {carried}"
    );
}

#[test]
fn requests_queued_when_the_upgrade_is_accepted_start_nothing() {
    let (Stale { mut h, _dir }, mut client, session, _release) = upgrade_pending();
    h.runtime().sender().send(passed());
    let new = client.new_session();
    client.input(&session, b"typed during the swap");
    h.poll();

    let plan = h.runtime_mut().take_swap().expect("a swap is ready");
    assert_eq!(
        plan.sessions.len(),
        1,
        "a session was started during the swap"
    );
    assert_eq!(h.host().sessions().len(), 1);
    let msgs = client.take();
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, shepherd_core::wire::HostMessage::Response(r) if r.id == new)),
        "the new session was answered"
    );
    let snapshot = String::from_utf8_lossy(&plan.sessions[0].snapshot).into_owned();
    assert!(!snapshot.contains("typed during"), "{snapshot:?}");
}

// ---------- peers across a swap ----------

const KEY_A: [u8; 32] = [0xaa; 32];
const KEY_B: [u8; 32] = [0xbb; 32];

/// The host log of a machine that trusts `peer`, called `name`.
fn trusting(peer: MachineId, name: &str) -> FakeEventLog {
    let mut driver = HostDriver::new();
    let frame = ShepherdFrame::PeerTrusted(PeerTrusted {
        node: peer,
        name: name.into(),
    });
    driver.append_draft(FrameDraft::new(
        frame.to_frame(),
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        None,
    ));
    FakeEventLog::new().with_envelopes(LogId::Host, driver.log().to_vec())
}

/// A stale host A and a host B that trust each other on one fake peer network, connected.
fn stale_and_peer() -> (FakePeerNet, Stale, HostHarness) {
    let net = FakePeerNet::new();
    let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    std::fs::write(&exe, b"a newer build").unwrap();
    let running = BuildId {
        version: "0.1.0".into(),
        digest: digest(b"the build the host started as"),
    };
    let mut b = HostHarness::builder()
        .peer_net(net.clone())
        .machine_key(KEY_B)
        .log(trusting(a_id.clone(), "office"))
        .start();
    let mut a = HostHarness::builder()
        .build(running, &exe)
        .peer_net(net.clone())
        .machine_key(KEY_A)
        .log(trusting(b_id.clone(), "laptop"))
        .start();
    // A finds B when it dials it again, twice the first wait after the dial on start failed.
    net.introduce(&a_id, &b_id);
    a.clock().advance(Duration::from_secs(2));
    a.step();
    b.step();
    assert!(connected_to(&mut b), "B sees A before the swap");
    (net, Stale { h: a, _dir: dir }, b)
}

/// Whether `h` lists its one peer as connected.
fn connected_to(h: &mut HostHarness) -> bool {
    let mut client = h.connect();
    let list = client.request(ClientRequest::PeerList(PeerList {}));
    h.step();
    response(&client.take(), &list).expect("peer list")["peers"][0]["connected"] == true
}

/// A swap takes this machine offline before the exec, so its peers see it go at once rather than when the connection
/// times out, and the new image comes back online as the same machine.
#[test]
fn a_swap_goes_offline_before_the_exec_and_the_new_image_comes_back_as_the_same_machine() {
    let (net, Stale { h: mut a, _dir }, mut b) = stale_and_peer();
    let a_id = a.machine();
    let mut client = a.connect();
    assert_eq!(upgrade(&mut a, &mut client), UpgradeAnswer::Accepted);

    a.swap(|_| {
        assert!(!net.is_bound(&a_id), "still online at the exec");
        b.step();
        assert!(!connected_to(&mut b), "B still sees A at the exec");
    });

    assert_eq!(a.machine(), a_id);
    assert!(net.is_bound(&a_id));
    b.step();
    assert!(connected_to(&mut b), "B sees A again");
}

#[test]
fn a_swap_whose_exec_fails_comes_back_online() {
    let (net, Stale { h: mut a, _dir }, mut b) = stale_and_peer();
    let mut client = a.connect();
    assert_eq!(upgrade(&mut a, &mut client), UpgradeAnswer::Accepted);
    drop(a.runtime_mut().take_swap().expect("a swap is ready"));
    a.runtime_mut().close_conns();
    assert!(!net.is_bound(&a.machine()));

    a.runtime_mut()
        .swap_failed("execv: No such file or directory".into());
    a.step();

    assert!(net.is_bound(&a.machine()));
    b.step();
    assert!(connected_to(&mut b), "B sees A again");
    assert!(connected_to(&mut a), "A sees B again");
}

// ---------- tunnels across a swap ----------

/// A stale host A trusting B, and B's host letting A in, their links to each other's machines on one fake network.
fn stale_and_tunnel_peer() -> (Stale, HostHarness) {
    let machines = FakeNetwork::new();
    let (a_id, b_id) = (machine_with(KEY_A), machine_with(KEY_B));
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("shepherd");
    std::fs::write(&exe, b"a newer build").unwrap();
    let running = BuildId {
        version: "0.1.0".into(),
        digest: digest(b"the build the host started as"),
    };
    let a = HostHarness::builder()
        .build(running, &exe)
        .machines(machines.clone())
        .machine_key(KEY_A)
        .log(trusting(b_id, "laptop"))
        .start();
    let b = HostHarness::builder()
        .machines(machines.clone())
        .machine_key(KEY_B)
        .start();
    machines.trust(&b.machine(), &a_id);
    (Stale { h: a, _dir: dir }, b)
}

/// T31a: a swap closes the tunnels with the other conns, so a tunnelled client is flushed and cut off before the exec,
/// like a client of the host's own. The tunnelled client is slow and reads only once the swap waits for it: what was
/// queued for it (the tunnel's answer) arrives, and its `Disconnected` comes before the old link goes.
#[test]
fn a_swap_flushes_and_closes_a_tunnelled_client_before_the_exec() {
    let (Stale { h: mut a, _dir }, mut b) = stale_and_tunnel_peer();
    let mut tunnelled = a.connect_paced();
    let req = tunnelled.request(ClientRequest::Tunnel(Tunnel {
        machine: b.machine(),
    }));
    for _ in 0..4 {
        a.step();
        b.step();
    }
    assert!(
        tunnelled.take().is_empty(),
        "a paced client reads nothing yet"
    );
    assert!(tunnelled.unread() > 0, "the tunnel's answer waits for it");

    let mut client = a.connect();
    assert_eq!(upgrade(&mut a, &mut client), UpgradeAnswer::Accepted);
    let (net, conn) = (a.network().clone(), tunnelled.conn());
    let reader = std::thread::spawn(move || {
        if !net.wait_until_flushing(Duration::from_secs(10)) {
            return false;
        }
        net.deliver(conn, usize::MAX);
        true
    });
    let mut at_exec = None;
    a.swap(|_| at_exec = tunnelled.disconnect_reason());
    assert!(
        reader.join().unwrap(),
        "the swap never waited for the flush"
    );

    assert_eq!(
        response(&tunnelled.take(), &req),
        Ok(serde_json::Value::Object(Default::default())),
        "the tunnel's answer was delivered before the exec"
    );
    assert_eq!(
        at_exec,
        Some(DisconnectReason::PeerGone),
        "closed before the exec"
    );
}
