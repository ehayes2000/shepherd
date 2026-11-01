//! `shepherd sessions` and `shepherd new` (T46), and `shepherd screen` (T52), with the built binary, against a real
//! host (shell sessions only, no agent) in a temp `HOME`, never the real `~/.shepherd`. Each command runs in a
//! directory that is no git repo, so the host selects no workspace and makes no worktree. Peers and worktrees are
//! covered in-process (`cli_scenarios.rs`).
//! Timeouts are liveness bounds only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

const SHEPHERD: &str = env!("CARGO_BIN_EXE_shepherd");
const LIVENESS: Duration = Duration::from_secs(60);

/// The host, stopped when dropped (on failure too): exactly this pid, never by name.
struct Running {
    host: Child,
    home: tempfile::TempDir,
    /// Where each command runs: no git repo.
    here: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.host.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.host.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::yield_now();
        }
        let _ = self.host.kill();
        let _ = self.host.wait();
    }
}

/// Starts `shepherd host` in a fresh temp `HOME` whose default profile is a plain shell, and waits until it serves
/// (it releases its stderr then).
fn start_host() -> Running {
    // Under /tmp: the socket path must stay under the OS limit.
    let home = tempfile::Builder::new()
        .prefix("shse")
        .tempdir_in("/tmp")
        .unwrap();
    let root = home.path().join(".shepherd");
    shepherd::host::prepare_root(&root).unwrap();
    let mut config = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join(shepherd::host::CONFIG_FILE))
        .unwrap();
    config
        .write_all(b"[defaults]\nagent = \"shell\"\n")
        .unwrap();
    let here = home.path().join("here");
    fs::create_dir(&here).unwrap();
    let mut host = Command::new(SHEPHERD)
        .arg("host")
        .env("HOME", home.path())
        .env("SHELL", "/bin/sh")
        .current_dir(&here)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = host.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stderr.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    let said = rx.recv_timeout(LIVENESS).expect("the host started");
    assert!(said.is_empty(), "the host failed to start: {said}");
    Running { host, home, here }
}

impl Running {
    fn shepherd(&self, args: &[&str]) -> Output {
        Command::new(SHEPHERD)
            .args(args)
            .env("HOME", self.home.path())
            .env("SHELL", "/bin/sh")
            .current_dir(&self.here)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Runs a command in `dir` that must succeed; its stdout.
    fn ok_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new(SHEPHERD)
            .args(args)
            .env("HOME", self.home.path())
            .env("SHELL", "/bin/sh")
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "shepherd {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs a command that must succeed; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.shepherd(args);
        assert!(out.status.success(), "shepherd {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// Runs a command that must fail; its stderr.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.shepherd(args);
        assert!(
            !out.status.success(),
            "shepherd {args:?} succeeded: {out:?}"
        );
        assert!(out.stdout.is_empty(), "nothing on stdout: {out:?}");
        String::from_utf8(out.stderr).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    fn dir(&self, name: &str) -> String {
        let dir = self.home.path().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap().to_str().unwrap().to_owned()
    }
}

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[test]
fn sessions_on_a_host_with_none_says_so_and_lists_this_machine() {
    let running = start_host();
    assert_eq!(running.ok(&["sessions"]), "No sessions.\n");
    let listed = running.json(&["sessions", "--json"]);
    assert_eq!(keys(&listed), ["machines", "sessions"]);
    assert_eq!(listed["sessions"], Value::Array(vec![]));
    let machines = listed["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 1, "{listed}");
    let me = &machines[0];
    assert_eq!(keys(me), ["id", "local", "name", "status", "why"]);
    assert_eq!(me["local"], true);
    assert_eq!(me["status"], "connected");
    assert_eq!(me["why"], Value::Null);
}

#[test]
fn new_prints_the_id_of_the_session_the_host_made_and_sessions_lists_it() {
    let running = start_host();
    let cwd = running.dir("project");
    let printed = running.ok(&["new", "--cwd", &cwd, "--name", "worker"]);
    let id = printed.strip_suffix('\n').expect("one line");
    assert!(
        !id.is_empty() && !id.contains(char::is_whitespace),
        "{printed:?}"
    );

    let listed = running.json(&["sessions", "--json"]);
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    let session = &sessions[0];
    assert_eq!(
        keys(session),
        [
            "agent",
            "branch",
            "created_at",
            "failed",
            "id",
            "machine",
            "machine_id",
            "name",
            "pr",
            "starting",
            "status",
            "workspace",
            "worktree",
        ]
    );
    assert_eq!(session["id"], id);
    assert_eq!(session["name"], "worker");
    assert_eq!(session["agent"], "shell");
    assert_eq!(session["status"], "running");
    assert_eq!(session["machine_id"], listed["machines"][0]["id"]);
    assert_eq!(session["machine"], listed["machines"][0]["name"]);
    assert_eq!(session["workspace"], Value::Null);
    assert_eq!(session["worktree"], Value::Null);
    assert_eq!(session["branch"], Value::Null);
    assert_eq!(session["pr"], Value::Null);
    assert_eq!(session["starting"], false);
    assert_eq!(session["failed"], Value::Null);

    let human = running.ok(&["sessions"]);
    assert!(
        human.contains(id) && human.contains("worker") && human.contains("running"),
        "{human}"
    );
}

#[test]
fn new_json_answers_with_the_session_and_sessions_lists_the_newest_first() {
    let running = start_host();
    let cwd = running.dir("project");
    let first = running.ok(&["new", "--cwd", &cwd]);
    let made = running.json(&["new", "--cwd", &cwd, "--profile", "shell", "--json"]);
    assert_eq!(keys(&made), ["session"]);
    assert_eq!(made["session"]["agent"], "shell");
    let second = made["session"]["id"].as_str().unwrap();

    let listed = running.json(&["sessions", "--json"]);
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [second, first.trim_end()]);
}

#[test]
fn new_with_an_unknown_profile_fails_saying_so_and_makes_nothing() {
    let running = start_host();
    let stderr = running.fails(&["new", "--profile", "nope"]);
    assert!(stderr.contains("unknown profile `nope`"), "{stderr}");
    assert_eq!(running.ok(&["sessions"]), "No sessions.\n");
}

#[test]
fn new_in_a_directory_that_does_not_exist_fails_and_makes_nothing() {
    let running = start_host();
    let stderr = running.fails(&["new", "--cwd", "/nonexistent/shepherd-t46"]);
    assert!(stderr.contains("/nonexistent/shepherd-t46"), "{stderr}");
    assert_eq!(running.ok(&["sessions"]), "No sessions.\n");
}

#[test]
fn new_on_an_unknown_machine_fails_listing_the_machines() {
    let running = start_host();
    let stderr = running.fails(&["new", "--machine", "laptop"]);
    assert!(stderr.contains("no machine is called `laptop`"), "{stderr}");
    let listed = running.json(&["sessions", "--json"]);
    let me = listed["machines"][0]["id"].as_str().unwrap();
    assert!(stderr.contains(me), "{stderr}");
    assert_eq!(running.ok(&["sessions"]), "No sessions.\n");
}

#[test]
fn new_on_this_machine_by_name_starts_it_here() {
    let running = start_host();
    let listed = running.json(&["sessions", "--json"]);
    let me = listed["machines"][0]["name"].as_str().unwrap().to_owned();
    let cwd = running.dir("project");
    let made = running.json(&["new", "--machine", &me, "--cwd", &cwd, "--json"]);
    assert_eq!(made["session"]["machine"], me.as_str());
}

#[test]
fn names_are_printed_display_safe() {
    let running = start_host();
    let cwd = running.dir("project");
    running.ok(&["new", "--cwd", &cwd, "--name", "evil\u{1b}]0;owned\u{7}"]);
    let human = running.ok(&["sessions"]);
    assert!(
        !human.contains('\u{1b}') && !human.contains('\u{7}'),
        "{human:?}"
    );
    assert!(human.contains("evil]0;owned"), "{human}");
}

#[test]
fn screen_json_answers_with_a_running_shells_screen_at_its_size() {
    let running = start_host();
    let cwd = running.dir("project");
    running.ok(&["new", "--cwd", &cwd, "--name", "worker"]);
    let shown = running.json(&["screen", "worker", "--lines", "10", "--json"]);
    assert_eq!(
        keys(&shown),
        [
            "cols",
            "lines",
            "rows",
            "scrollback",
            "session",
            "truncated"
        ]
    );
    assert_eq!(
        (shown["cols"].as_u64(), shown["rows"].as_u64()),
        (Some(80), Some(24))
    );
    assert_eq!(shown["truncated"], false);
    let lines = shown["lines"].as_array().unwrap();
    let scrollback = usize::try_from(shown["scrollback"].as_u64().unwrap()).unwrap();
    assert_eq!(lines.len(), scrollback + 24, "{shown}");
    assert!(scrollback <= 10, "{shown}");
    let stderr = running.fails(&["screen", "nobody"]);
    assert!(stderr.contains("nobody"), "{stderr}");
}

#[test]
fn bad_arguments_fail_with_the_usage() {
    let running = start_host();
    for args in [
        &["sessions", "--bogus"][..],
        &["sessions", "extra"],
        &["new", "--profile"],
        &["new", "positional"],
        &["screen"],
        &["screen", "a", "b"],
        &["screen", "a", "--lines"],
    ] {
        let stderr = running.fails(args);
        assert!(stderr.contains("usage: shepherd"), "{args:?}: {stderr}");
    }
}

#[test]
fn a_relative_cwd_is_relative_to_where_new_runs() {
    let running = start_host();
    fs::create_dir(running.here.join("sub")).unwrap();
    let made = running.json(&["new", "--cwd", "sub", "--json"]);
    let id = made["session"]["id"].as_str().unwrap();
    // The host records where a session without a worktree runs in its `session/created`.
    let log = fs::read_to_string(
        running
            .home
            .path()
            .join(".shepherd")
            .join("sessions")
            .join(format!("{id}.jsonl")),
    )
    .unwrap();
    let created: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|entry| entry["frame"]["method"] == "_shepherd/session/created")
        .collect();
    let [created] = &created[..] else {
        panic!("one session/created in {log}");
    };
    let cwd = created["frame"]["params"]["cwd"].as_str().unwrap();
    assert_eq!(
        Path::new(cwd),
        fs::canonicalize(running.here.join("sub")).unwrap()
    );
}

/// Runs git in `dir` with nothing from this process's environment but `PATH` and no user or system config (under
/// the pre-commit hook the environment points git at the repo being committed).
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .env_clear()
        .envs(std::env::vars_os().filter(|(k, _)| k == "PATH"))
        .env("HOME", dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A clone at `<base>/clone` of a local seed repo with one commit: a repo the host would take as a workspace (it has
/// an origin default branch). Nothing leaves the machine.
fn clone_in(base: &Path) -> PathBuf {
    let seed = base.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["commit", "-q", "--allow-empty", "-m", "first"]);
    git(base, &["clone", "-q", "seed", "clone"]);
    base.join("clone")
}

#[test]
fn commands_run_in_a_repo_never_make_the_host_select_it_as_its_workspace() {
    let running = start_host();
    let repo = clone_in(running.home.path());
    running.ok_in(&repo, &["sessions"]);
    // Made without --cwd from inside the repo: no workspace, so no worktree; it runs in the host's home.
    let made: Value = serde_json::from_str(&running.ok_in(&repo, &["new", "--json"])).unwrap();
    let session = &made["session"];
    assert_eq!(session["workspace"], Value::Null, "{made}");
    assert_eq!(session["worktree"], Value::Null, "{made}");
    assert_eq!(session["branch"], Value::Null, "{made}");
    assert_eq!(session["starting"], false, "{made}");
    // And a later command still finds none selected.
    let listed: Value =
        serde_json::from_str(&running.ok_in(&repo, &["sessions", "--json"])).unwrap();
    assert_eq!(listed["sessions"][0]["workspace"], Value::Null, "{listed}");
    let cwd = running.dir("project");
    let made: Value =
        serde_json::from_str(&running.ok_in(&repo, &["new", "--cwd", &cwd, "--json"])).unwrap();
    assert_eq!(made["session"]["workspace"], Value::Null, "{made}");
}
