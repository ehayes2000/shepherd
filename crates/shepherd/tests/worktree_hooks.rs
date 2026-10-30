//! A new worktree runs its workspace's post-create hooks before the agent starts (T23): a host runtime on all-fake
//! ports, with the fake `Commands` port running the hooks on its own worker thread. As in `session_worktrees.rs`, the
//! workspace and each worktree are real directories in a tempdir, made by the test where git would make them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shepherd_core::acpp::{Envelope, Frame, RequestId, Seq, ShepherdFrame};
use shepherd_core::config::{RawConfig, RawHooks, RawWorkspace};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{CommandSpec, EventLog, LogId};
use shepherd_core::wire::{
    ClientRequest, Hello, PROTOCOL_VERSION, SessionClose, SessionNew, SessionRename,
};
use shepherd_testkit::fakes::commands::FakeCommands;
use shepherd_testkit::fakes::pty::ProgramInput;
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

/// A liveness bound for a held call, never a measurement.
const LIMIT: Duration = Duration::from_secs(10);
/// How long each wait in a loop polling for two things at once lasts: a liveness slice of [`LIMIT`], never a measurement.
const STEP: Duration = Duration::from_millis(20);

/// A fake `claude` that records, each time it starts, its cwd and whether its `--settings` file (a run file) exists.
fn recording_claude(seen: &Arc<Mutex<Vec<(PathBuf, bool)>>>) -> Program {
    let record = seen.clone();
    Arc::new(move |input, p| {
        if input == ProgramInput::Start {
            let args = &p.spec().args;
            let at = args.iter().position(|a| a == "--settings").unwrap();
            let settings = Path::new(&args[at + 1]).is_file();
            record
                .lock()
                .unwrap()
                .push((p.spec().cwd.clone(), settings));
        }
    })
}

/// A host whose workspace is `<tempdir>/app`, whose config gives it `hooks` as post-create hooks (keyed by its path),
/// each succeeding, with a subscribed client and a fake `claude`. Worktrees go under `<tempdir>/worktrees`.
struct Setup {
    _tmp: tempfile::TempDir,
    app: PathBuf,
    root: PathBuf,
    commands: FakeCommands,
    claude: Arc<Mutex<Vec<(PathBuf, bool)>>>,
    h: HostHarness,
    client: FakeClient,
}

impl Setup {
    fn new(hooks: &[&str]) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let app = base.join("app");
        let root = base.join("worktrees");
        std::fs::create_dir(&app).unwrap();
        let worktrees = FakeWorktrees::new(&root);
        worktrees.add_repo(&app, "main");
        // Each hook succeeds unless a test says otherwise.
        let commands = FakeCommands::new();
        for hook in hooks {
            commands.add_script(*hook, |c| c.print("ok\n"));
        }
        let workspace = RawWorkspace {
            hooks: Some(RawHooks {
                post_create: Some(hooks.iter().map(|h| (*h).to_owned()).collect()),
                pre_remove: None,
            }),
            hook_timeout_s: Some(90),
        };
        let config = RawConfig {
            workspaces: Some(BTreeMap::from([(
                app.to_str().unwrap().to_owned(),
                workspace,
            )])),
            ..shell_default_config()
        };
        let claude = Arc::default();
        let mut h = HostHarness::builder()
            .program("claude", recording_claude(&claude))
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
        assert_eq!(h.host().workspace(), Some(app.as_path()));
        client.subscribe_all();
        h.step();
        client.take();
        Self {
            _tmp: tmp,
            app,
            root,
            commands,
            claude,
            h,
            client,
        }
    }

    /// Makes `app`'s worktree `name`'s directory, as git would.
    fn make_worktree_dir(&self, name: &str) -> PathBuf {
        let path = self.root.join("app").join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn new_claude_session(&mut self) -> RequestId {
        self.client.request(ClientRequest::SessionNew(SessionNew {
            profile: Some("claude".to_owned()),
            cwd: None,
        }))
    }

    /// Starts a claude session with the first hook held: returns its id once the hook is running and waiting.
    fn start_with_first_hook_held(&mut self) -> (SessionId, shepherd_testkit::fakes::hold::Hold) {
        let hold = self.commands.hold_next();
        let req = self.new_claude_session();
        // The request, then the worktree's result, which runs the first hook. The result may already have been handled
        // by the poll, so wait for the hook to start rather than for any one event: handle what arrives meanwhile.
        self.h.poll();
        let mut waited = Duration::ZERO;
        while !hold.wait_entered(STEP) {
            assert!(waited < LIMIT, "the hook never reached the port");
            self.h.runtime_mut().run_next(STEP);
            waited += STEP * 2;
        }
        let id = new_session_id(&self.client.take(), &req);
        (id, hold)
    }
}

/// The `_shepherd/*` frames in session `id`'s log, in order.
fn frames(h: &HostHarness, id: &SessionId) -> Vec<ShepherdFrame> {
    h.log()
        .read(LogId::Session(id.clone()), Seq::ZERO)
        .expect("session log")
        .iter()
        .filter_map(shepherd_frame)
        .collect()
}

fn shepherd_frame(env: &Envelope) -> Option<ShepherdFrame> {
    match env.frame() {
        Frame::Shepherd(m) => Some(m.body().clone()),
        _ => None,
    }
}

fn methods(frames: &[ShepherdFrame]) -> Vec<&'static str> {
    frames.iter().map(ShepherdFrame::method).collect()
}

fn env_of<'a>(spec: &'a CommandSpec, name: &str) -> Option<&'a str> {
    spec.env
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn post_create_hooks_run_in_the_worktree_without_blocking_the_host_then_the_agent_starts_there() {
    let mut s = Setup::new(&["setup-a", "setup-b"]);
    let path = s.make_worktree_dir("claude-1");
    let (id, hold) = s.start_with_first_hook_held();

    // While the hook runs, the host still answers, and the session is listed as starting with no PTY.
    let rename = s
        .client
        .request(ClientRequest::SessionRename(SessionRename {
            session: id.clone(),
            name: "api".to_owned(),
        }));
    s.h.poll();
    let msgs = s.client.take();
    assert_eq!(response(&msgs, &rename), Ok(serde_json::json!({})));
    let listed = &s.h.host().sessions()[0];
    assert!(listed.creating_worktree);
    assert!(!s.h.runtime().pty().is_running(&id));
    assert_eq!(s.h.runtime().pending_run_files(), 1, "they wait with it");

    hold.release();
    s.h.step();
    let runs = s.commands.runs();
    let scripts: Vec<&str> = runs.iter().map(|r| r.script.as_str()).collect();
    assert_eq!(scripts, ["setup-a", "setup-b"]);
    for run in &runs {
        assert_eq!(run.cwd, path);
        assert_eq!(run.timeout, Duration::from_secs(90));
        assert_eq!(env_of(run, "SHEPHERD_SESSION"), Some(id.as_str()));
        assert_eq!(env_of(run, "SHEPHERD_WORKTREE"), path.to_str());
        assert_eq!(env_of(run, "SHEPHERD_BRANCH"), Some("claude-1"));
        assert_eq!(env_of(run, "SHEPHERD_REPO"), s.app.to_str());
    }
    assert_eq!(
        methods(&frames(&s.h, &id)),
        [
            "_shepherd/session/created",
            "_shepherd/worktree/created",
            "_shepherd/session/renamed",
            "_shepherd/worktree/hook",
            "_shepherd/worktree/hook",
        ]
    );
    // The agent starts in the worktree, after the hooks, with its run files.
    assert!(s.h.runtime().pty().is_running(&id));
    assert_eq!(*s.claude.lock().unwrap(), [(path, true)]);
    assert_eq!(s.h.runtime().pending_run_files(), 0);
    assert_eq!(s.h.host().sessions()[0].warning, None);
    assert_eq!(s.h.replayed(), *s.h.host());
}

#[test]
fn a_failing_hook_stops_the_rest_warns_and_the_agent_still_starts() {
    let mut s = Setup::new(&["setup-a", "setup-b"]);
    s.commands.add_script("setup-a", |c| {
        c.print("npm ERR! missing script\n");
        c.exit(1);
    });
    let path = s.make_worktree_dir("claude-1");
    let req = s.new_claude_session();
    s.h.step();
    let msgs = s.client.take();
    let id = new_session_id(&msgs, &req);

    let runs: Vec<String> = s.commands.runs().into_iter().map(|r| r.script).collect();
    assert_eq!(runs, ["setup-a"]);
    let frames = frames(&s.h, &id);
    assert!(
        matches!(&frames[2], ShepherdFrame::WorktreeHook(hook)
            if hook.hook == "setup-a" && hook.exit_code == Some(1)
                && hook.output_tail == "npm ERR! missing script\n"),
        "{frames:?}"
    );
    assert_eq!(frames.len(), 3, "{frames:?}");
    assert!(s.h.runtime().pty().is_running(&id));
    assert_eq!(*s.claude.lock().unwrap(), [(path, true)]);
    // The client sees the warning from the log.
    let listed = fold(&entries(&msgs)).sessions();
    assert_eq!(
        listed[0].warning.as_deref(),
        Some("hook failed (exit 1): setup-a")
    );
}

#[test]
fn a_session_closed_while_a_hook_runs_runs_no_more_hooks_and_never_spawns() {
    let mut s = Setup::new(&["setup-a", "setup-b"]);
    s.make_worktree_dir("claude-1");
    let (id, hold) = s.start_with_first_hook_held();
    s.client.request(ClientRequest::SessionClose(SessionClose {
        session: id.clone(),
        worktree: shepherd_core::wire::CloseWorktree::IfSafe,
    }));
    s.h.poll();

    hold.release();
    s.h.step();
    let runs: Vec<String> = s.commands.runs().into_iter().map(|r| r.script).collect();
    assert_eq!(runs, ["setup-a"]);
    assert_eq!(
        methods(&frames(&s.h, &id)),
        [
            "_shepherd/session/created",
            "_shepherd/worktree/created",
            "_shepherd/session/closed",
            "_shepherd/worktree/hook",
        ]
    );
    assert!(!s.h.runtime().pty().is_running(&id));
    assert!(s.claude.lock().unwrap().is_empty());
    assert_eq!(s.h.runtime().pending_run_files(), 0);
}
