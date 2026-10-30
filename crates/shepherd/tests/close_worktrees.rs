//! Closing a session removes its worktree when that's safe (T25): a host runtime on fake ports, the fake `Worktrees`
//! and `Commands` ports on their worker threads; and, for paths that try to escape the managed root, the real git
//! adapter. As in `worktree_hooks.rs`, the workspace and each worktree are real directories in a tempdir, made by the
//! test where git would make them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use shepherd::host::{HostRuntime, runtime_channel};
use shepherd_adapters::GitWorktrees;
use shepherd_core::acpp::{Envelope, Frame, RequestId, Seq, ShepherdFrame};
use shepherd_core::config::{RawConfig, RawHooks, RawWorkspace};
use shepherd_core::host::{HostEvent, Inspect, Inspection, PreRemove};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::WorktreeStatus;
use shepherd_core::ports::{
    BaseUsed, BranchName, Created, EventLog, LogId, WorktreePath, Worktrees,
};
use shepherd_core::wire::{
    ClientRequest, CloseWorktree, Hello, PROTOCOL_VERSION, SessionClose, SessionNew, WorktreeCheck,
};
use shepherd_testkit::builders::HostDriver;
use shepherd_testkit::fakes::commands::FakeCommands;
use shepherd_testkit::fakes::event_log::FakeEventLog;
use shepherd_testkit::fakes::hold::Hold;
use shepherd_testkit::fakes::link::FakeNetwork;
use shepherd_testkit::fakes::pty::FakePty;
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

/// A liveness bound for a held call, never a measurement.
const LIMIT: Duration = Duration::from_secs(10);
/// How long each wait in a loop polling for two things at once lasts: a liveness slice of [`LIMIT`], never a measurement.
const STEP: Duration = Duration::from_millis(20);

/// The workspace config giving `pre_remove` as the pre-remove hooks of the repo at `app`.
fn config_with(app: &Path, pre_remove: &[&str]) -> RawConfig {
    let workspace = RawWorkspace {
        hooks: Some(RawHooks {
            post_create: None,
            pre_remove: Some(pre_remove.iter().map(|h| (*h).to_owned()).collect()),
        }),
        hook_timeout_s: Some(90),
    };
    RawConfig {
        workspaces: Some(BTreeMap::from([(
            app.to_str().unwrap().to_owned(),
            workspace,
        )])),
        ..shell_default_config()
    }
}

/// A host whose workspace is `<tempdir>/app` with `make clean` as its pre-remove hook (succeeding), a subscribed
/// client, and a shell session running in its worktree. Worktrees go under `<tempdir>/worktrees`.
struct Setup {
    _tmp: tempfile::TempDir,
    worktrees: FakeWorktrees,
    commands: FakeCommands,
    h: HostHarness,
    client: FakeClient,
    id: SessionId,
    path: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let app = base.join("app");
        let root = base.join("worktrees");
        std::fs::create_dir(&app).unwrap();
        let worktrees = FakeWorktrees::new(&root);
        worktrees.add_repo(&app, "main");
        let commands = FakeCommands::new();
        commands.add_script("make clean", |c| c.print("cleaned\n"));
        let mut h = HostHarness::builder()
            .worktrees(worktrees.clone())
            .commands(commands.clone())
            .config(config_with(&app, &["make clean"]))
            .start();
        let mut client = h.connect();
        client.request(ClientRequest::Hello(Hello {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("laptop"),
            cwd: Some(app.to_str().unwrap().to_owned()),
            build: None,
        }));
        h.step();
        client.subscribe_all();
        h.step();
        client.take();
        // The shell spawns in the worktree, so it must exist where git would have made it.
        let path = root.join("app").join("shell-1");
        std::fs::create_dir_all(&path).unwrap();
        let req = client.request(ClientRequest::SessionNew(SessionNew::default()));
        h.step();
        let id = new_session_id(&client.take(), &req);
        assert!(h.runtime().pty().is_running(&id));
        Self {
            _tmp: tmp,
            worktrees,
            commands,
            h,
            client,
            id,
            path,
        }
    }

    fn close(&mut self, worktree: CloseWorktree) -> RequestId {
        self.client
            .request(ClientRequest::SessionClose(SessionClose {
                session: self.id.clone(),
                worktree,
            }))
    }

    /// Closes the session with its pre-remove hook held: returns once the hook is running and waiting, with the close's
    /// request id.
    fn close_with_hook_held(&mut self) -> (RequestId, Hold) {
        let hold = self.commands.hold_next();
        let req = self.close(CloseWorktree::IfSafe);
        // The close, then the worktree's inspection on its worker, whose answer runs the hook.
        self.h.poll();
        let mut waited = Duration::ZERO;
        while !hold.wait_entered(STEP) {
            assert!(waited < LIMIT, "the hook never reached the port");
            self.h.runtime_mut().run_next(STEP);
            waited += STEP * 2;
        }
        (req, hold)
    }

    /// Whether the session's worktree is still one of the repo's.
    fn listed(&self) -> bool {
        let app = self.h.host().workspace().unwrap().to_owned();
        self.worktrees
            .list_managed(&shepherd_core::ports::RepoRef::new(app))
            .unwrap()
            .iter()
            .any(|w| w.path.as_path() == self.path)
    }

    fn frames(&self) -> Vec<ShepherdFrame> {
        self.h
            .log()
            .read(LogId::Session(self.id.clone()), Seq::ZERO)
            .expect("session log")
            .iter()
            .filter_map(shepherd_frame)
            .collect()
    }
}

fn shepherd_frame(env: &Envelope) -> Option<ShepherdFrame> {
    match env.frame() {
        Frame::Shepherd(m) => Some(m.body().clone()),
        _ => None,
    }
}

fn last_closed(frames: &[ShepherdFrame]) -> shepherd_core::acpp::SessionClosed {
    match frames.last() {
        Some(ShepherdFrame::SessionClosed(closed)) => closed.clone(),
        other => panic!("the log doesn't end in session/closed: {other:?}"),
    }
}

#[test]
fn closing_a_session_whose_worktree_loses_nothing_runs_its_pre_remove_hook_then_removes_it() {
    let mut s = Setup::new();
    assert!(s.listed());
    // While the hook runs, the session is closing, its PTY gone, and the host has answered.
    let (req, hold) = s.close_with_hook_held();
    let msgs = s.client.take();
    assert_eq!(response(&msgs, &req), Ok(json!({})));
    assert!(!s.h.runtime().pty().is_running(&s.id));
    let listed = &s.h.host().sessions()[0];
    assert!(listed.removing_worktree);
    assert!(s.listed(), "not removed before its hook finishes");

    hold.release();
    s.h.step();
    let runs = s.commands.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].script, "make clean");
    assert_eq!(runs[0].cwd, s.path);
    assert_eq!(runs[0].timeout, Duration::from_secs(90));
    assert!(!s.listed(), "removed");
    let frames = s.frames();
    let methods: Vec<&str> = frames.iter().map(ShepherdFrame::method).collect();
    assert_eq!(
        methods,
        [
            "_shepherd/session/created",
            "_shepherd/worktree/created",
            "_shepherd/worktree/removing",
            "_shepherd/worktree/hook",
            "_shepherd/session/closed",
        ]
    );
    let closed = last_closed(&frames);
    assert!(closed.worktree_removed);
    assert_eq!(closed.worktree_kept, None);
    assert!(s.h.host().sessions().is_empty());
    assert_eq!(s.h.replayed(), *s.h.host());
}

#[test]
fn a_client_hears_what_a_dirty_worktree_would_lose_and_removing_anyway_removes_it() {
    let mut s = Setup::new();
    s.worktrees.set_dirty(&s.path, true);
    // The client asks first, and hears what would be lost.
    let ask = s
        .client
        .request(ClientRequest::WorktreeStatus(WorktreeCheck {
            session: s.id.clone(),
        }));
    s.h.step();
    let answer = response(&s.client.take(), &ask).unwrap();
    let shown: WorktreeStatus = serde_json::from_value(answer["status"].clone()).unwrap();
    assert!(shown.dirty);

    s.close(CloseWorktree::Remove { confirmed: shown });
    s.h.step();
    assert!(!s.listed(), "removed anyway");
    assert_eq!(s.commands.runs().len(), 1, "after its hook");
    let closed = last_closed(&s.frames());
    assert!(closed.worktree_removed);
}

#[test]
fn removing_anyway_keeps_a_worktree_that_got_more_to_lose_after_the_user_looked() {
    let mut s = Setup::new();
    // The user is shown an unpushed commit, and chooses to remove anyway; meanwhile the agent writes a file.
    s.worktrees.commit(&s.path);
    let ask = s
        .client
        .request(ClientRequest::WorktreeStatus(WorktreeCheck {
            session: s.id.clone(),
        }));
    s.h.step();
    let answer = response(&s.client.take(), &ask).unwrap();
    let shown: WorktreeStatus = serde_json::from_value(answer["status"].clone()).unwrap();
    assert!(!shown.dirty && shown.ahead == 1);
    s.worktrees.set_dirty(&s.path, true);

    s.close(CloseWorktree::Remove { confirmed: shown });
    s.h.step();
    assert!(s.listed(), "kept");
    assert!(s.commands.runs().is_empty(), "no hook ran");
    let closed = last_closed(&s.frames());
    assert!(!closed.worktree_removed);
    assert_eq!(
        closed.worktree_kept.as_deref(),
        Some("it has uncommitted changes, which closing didn't confirm")
    );
}

#[test]
fn a_plain_close_keeps_a_dirty_worktree_and_says_why() {
    let mut s = Setup::new();
    s.worktrees.set_dirty(&s.path, true);
    s.close(CloseWorktree::IfSafe);
    s.h.step();
    assert!(s.listed(), "kept");
    assert!(
        s.commands.runs().is_empty(),
        "no hook runs for a kept worktree"
    );
    let closed = last_closed(&s.frames());
    assert!(!closed.worktree_removed);
    assert_eq!(
        closed.worktree_kept.as_deref(),
        Some("it has uncommitted changes")
    );
    assert!(s.h.host().sessions().is_empty());
}

#[test]
fn a_host_restarting_while_a_session_closes_closes_it_keeping_its_worktree() {
    let mut s = Setup::new();
    let (_, hold) = s.close_with_hook_held();
    s.h.restart();
    hold.release();
    let closed = last_closed(&s.frames());
    assert!(!closed.worktree_removed);
    assert_eq!(
        closed.worktree_kept.as_deref(),
        Some("the host stopped before the worktree was removed")
    );
    assert!(s.h.host().sessions().is_empty());
}

// ---------- the real git adapter: recorded paths that try to escape ----------

/// Runs git in `dir` isolated from the user's machine, as the adapter's tests do.
fn git(dir: &Path, env: &[(OsString, OsString)], args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .envs(env.iter().cloned())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn git_env(home: &Path) -> Vec<(OsString, OsString)> {
    [
        ("GIT_CONFIG_GLOBAL", OsString::from("/dev/null")),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("HOME", home.into()),
        ("GIT_AUTHOR_NAME", "t".into()),
        ("GIT_AUTHOR_EMAIL", "t@example.com".into()),
        ("GIT_COMMITTER_NAME", "t".into()),
        ("GIT_COMMITTER_EMAIL", "t@example.com".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), v))
    .collect()
}

/// A log in which session `s1` of workspace `app` recorded its worktree at `recorded`, as a tampered or corrupt log
/// might.
fn log_recording(app: &Path, recorded: &Path) -> FakeEventLog {
    let mut driver = HostDriver::new();
    driver.select_workspace(app.to_str().unwrap());
    driver.new_session("s1");
    driver.worktree_result(
        "s1",
        Ok(Created {
            path: WorktreePath::new(recorded),
            branch: BranchName::new("shell-1"),
            base: BaseUsed::Origin {
                branch: BranchName::new("main"),
            },
        }),
    );
    let mut logs: BTreeMap<LogId, Vec<Envelope>> = BTreeMap::new();
    for env in driver.log() {
        let log = match env.session() {
            None => LogId::Host,
            Some(s) => LogId::Session(s.clone()),
        };
        logs.entry(log).or_default().push(env.clone());
    }
    logs.into_iter()
        .fold(FakeEventLog::new(), |log, (id, envs)| {
            log.with_envelopes(id, envs)
        })
}

#[test]
fn removal_never_touches_a_recorded_path_outside_the_managed_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let (origin, seed, app, root, victim, home) = (
        base.join("origin.git"),
        base.join("seed"),
        base.join("app"),
        base.join("worktrees"),
        base.join("victim"),
        base.join("home"),
    );
    for dir in [&origin, &seed, &root.join("app"), &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let env = git_env(&home);
    // An origin with `precious` committed on main, and the workspace a clone of it.
    git(&origin, &env, &["init", "-q", "--bare", "-b", "main"]);
    git(&seed, &env, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("precious"), "keep me").unwrap();
    git(&seed, &env, &["add", "precious"]);
    git(&seed, &env, &["commit", "-q", "-m", "first"]);
    git(
        &seed,
        &env,
        &["push", "-q", origin.to_str().unwrap(), "main"],
    );
    git(
        &base,
        &env,
        &["clone", "-q", origin.to_str().unwrap(), "app"],
    );
    // The victim is a worktree of the workspace outside the managed root, on a branch at origin/main: clean and merged,
    // so nothing about its status would stop a removal or its hooks.
    let victim_arg = victim.to_str().unwrap();
    git(
        &app,
        &env,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "victim",
            victim_arg,
            "origin/main",
        ],
    );
    std::os::unix::fs::symlink(&victim, root.join("app").join("link")).unwrap();
    let probe = GitWorktrees::new(&root).with_isolated_env(env.clone());
    let victim_status = probe.status(&WorktreePath::new(&victim)).unwrap();
    assert!(victim_status.losses().is_empty(), "{victim_status:?}");

    for recorded in [
        root.join("app").join("..").join("..").join("victim"),
        root.join("app").join("link"),
    ] {
        let commands = FakeCommands::new();
        let worktrees = GitWorktrees::new(&root).with_isolated_env(env.clone());
        let net = FakeNetwork::new();
        let (tx, rx) = runtime_channel();
        let pty = FakePty::new(tx.pty_sink());
        let link = net.listen(tx.link_sink()).expect("fake host listens");
        let run = tempfile::tempdir().expect("run root");
        let settings = settings(config_with(&app, &["rm -rf ."]), run.path().join("run"));
        let mut runtime = HostRuntime::start(
            pty,
            worktrees,
            commands.clone(),
            link,
            log_recording(&app, &recorded),
            TestEnv::new(),
            settings,
            tx,
            rx,
        )
        .expect("host runtime starts");
        let mut client = FakeClient::connect_with(&net, false);
        assert!(runtime.run_until_settled(LIMIT));
        client.request(ClientRequest::SessionClose(SessionClose {
            session: SessionId::new("s1"),
            worktree: CloseWorktree::IfSafe,
        }));
        assert!(runtime.run_until_settled(LIMIT));

        assert!(
            commands.runs().is_empty(),
            "{recorded:?}: no hook ran there"
        );
        assert_eq!(
            std::fs::read_to_string(victim.join("precious")).unwrap(),
            "keep me",
            "{recorded:?}"
        );
        let frames: Vec<ShepherdFrame> = runtime
            .log()
            .read(LogId::Session(SessionId::new("s1")), Seq::ZERO)
            .unwrap()
            .iter()
            .filter_map(shepherd_frame)
            .collect();
        let closed = last_closed(&frames);
        assert!(!closed.worktree_removed, "{recorded:?}");
        let kept = closed.worktree_kept.unwrap_or_default();
        assert!(
            kept.contains("isn't a worktree shepherd manages"),
            "{recorded:?}: {kept}"
        );
    }
}

/// F33: a pre-remove hook still running when the host stopped outlived it. Its worktree is kept: nothing removed it.
#[test]
fn shutting_the_host_down_cancels_a_running_pre_remove_hook() {
    let mut s = Setup::new();
    let (_, hold) = s.close_with_hook_held();
    s.h.runtime()
        .sender()
        .send(shepherd::host::RuntimeEvent::Shutdown);
    s.h.poll();
    assert!(hold.wait_cancelled(LIMIT), "the hook was left running");
    assert!(s.h.runtime_mut().run_until_settled(LIMIT));
    assert!(s.h.runtime().is_finished());
    assert_eq!(s.commands.running(), 0);
    assert!(s.listed(), "the worktree was removed");
}

/// F33 review: a close's inspection that answers after shutdown began must not start the pre-remove hook (nothing would
/// cancel it, and it would outlive the host). Another session's PTY keeps the shutdown waiting, and the inspection's
/// answer is handed to the runtime right after the shutdown, before that PTY's exit: the order that let it start.
#[test]
fn a_hook_due_after_shutdown_began_never_starts() {
    let mut s = Setup::new();
    let other_path = s.path.parent().unwrap().join("shell-2");
    std::fs::create_dir_all(&other_path).unwrap();
    let req = s
        .client
        .request(ClientRequest::SessionNew(SessionNew::default()));
    s.h.step();
    let other = new_session_id(&s.client.take(), &req);
    assert!(s.h.runtime().pty().is_running(&other));
    // The real inspection stays held: its answer is the one handed over below.
    let hold = s.worktrees.hold_next();
    s.close(CloseWorktree::IfSafe);
    s.h.poll();
    assert!(hold.wait_entered(LIMIT), "the inspection never started");

    s.h.runtime_mut()
        .handle(shepherd::host::RuntimeEvent::Shutdown);
    assert!(
        !s.h.runtime().is_finished(),
        "the other PTY's exit is still queued"
    );
    let clean = WorktreeStatus {
        dirty: false,
        ahead: 0,
        pushed: false,
        merged_into_default: true,
    };
    s.h.runtime_mut()
        .handle(shepherd::host::RuntimeEvent::Worktrees(
            HostEvent::WorktreeInspected {
                session: s.id.clone(),
                path: WorktreePath::new(&s.path),
                purpose: Inspect::Close {
                    confirmed: None,
                    hooks: PreRemove {
                        hooks: vec!["make clean".to_owned()],
                        timeout: Duration::from_secs(90),
                    },
                },
                result: Ok(Inspection::Listed {
                    status: Ok(clean),
                    exists: true,
                }),
            },
        ));
    assert!(
        s.commands.runs().is_empty(),
        "a hook started after shutdown"
    );
    hold.release();
    assert!(s.h.runtime_mut().run_until_settled(LIMIT));
    assert!(s.h.runtime().is_finished());
    assert!(
        s.commands.runs().is_empty(),
        "a hook started after shutdown"
    );
    assert!(s.listed(), "the worktree was removed");
}
