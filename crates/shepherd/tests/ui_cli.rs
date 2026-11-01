//! `shepherd ui` (T50) with the built binary, against a real host in a temp `HOME`, never the real `~/.shepherd`. Its
//! sessions run a plain `sh` that prints a line and then waits (no agent). Peers and pinning a peer's session are
//! covered in-process (`cli_scenarios.rs`). Timeouts are liveness bounds only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const SHEPHERD: &str = env!("CARGO_BIN_EXE_shepherd");
const LIVENESS: Duration = Duration::from_secs(60);
/// What every session prints when it starts.
const PAINTED: &str = "hello from the pane";

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

/// Starts `shepherd host` in a fresh temp `HOME` whose default profile is a shell that prints [`PAINTED`] and then
/// reads its terminal, and waits until it serves (it releases its stderr then).
fn start_host() -> Running {
    // Under /tmp: the socket path must stay under the OS limit.
    let home = tempfile::Builder::new()
        .prefix("shui")
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
    let toml = format!(
        "[defaults]\nagent = \"shell\"\n\n[agents.shell]\ncommand = \"/bin/sh\"\n\
         args = [\"-c\", \"printf '{PAINTED}\\\\n'; exec cat\"]\n"
    );
    config.write_all(toml.as_bytes()).unwrap();
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

    /// Starts a session called `name`, in a directory of its own; its id.
    fn session(&self, name: &str) -> String {
        let dir = self.home.path().join("project");
        fs::create_dir_all(&dir).unwrap();
        let made = self.json(&[
            "new",
            "--cwd",
            dir.to_str().unwrap(),
            "--name",
            name,
            "--json",
        ]);
        made["session"]["id"].as_str().unwrap().to_owned()
    }

    /// This machine, as `sessions --json` lists it.
    fn me(&self) -> Value {
        self.json(&["sessions", "--json"])["machines"][0].clone()
    }

    /// Takes screenshots with `args` until one shows what every session prints (it may not have printed it yet when
    /// it is made); that one's stdout.
    fn painted(&self, args: &[&str]) -> String {
        let deadline = Instant::now() + LIVENESS;
        loop {
            let shot = self.ok(args);
            if shot.contains(PAINTED) {
                return shot;
            }
            assert!(Instant::now() < deadline, "never painted: {shot}");
            std::thread::yield_now();
        }
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
fn get_prints_the_default_layout_one_page_showing_the_selection() {
    let running = start_host();
    assert_eq!(
        running.ok(&["ui", "get"]),
        "page 1 (shown): 1\n  pane 2: selected\n"
    );
    let got = running.json(&["ui", "get", "--json"]);
    assert_eq!(keys(&got), ["layout"]);
    assert_eq!(
        got["layout"],
        json!({
            "current": 1,
            "pages": [{ "id": 1, "name": "1", "root": { "kind": "leaf", "id": 2, "content": { "kind": "selected" } } }]
        })
    );
}

#[test]
fn split_with_a_session_pins_it_in_the_new_pane_and_prints_the_new_layout() {
    let running = start_host();
    let id = running.session("worker");
    let me = running.me();
    let split = running.json(&[
        "ui",
        "split",
        "2",
        "right",
        "--ratio",
        "0.3",
        "--session",
        "worker",
        "--json",
    ]);
    let root = &split["layout"]["pages"][0]["root"];
    assert_eq!(keys(root), ["a", "b", "dir", "id", "kind", "ratio"]);
    assert_eq!(root["kind"], "split");
    assert_eq!(root["dir"], "right");
    assert_eq!(root["ratio"], 0.3);
    assert_eq!(
        root["a"],
        json!({ "kind": "leaf", "id": 2, "content": { "kind": "selected" } })
    );
    assert_eq!(root["b"]["kind"], "leaf");
    assert_eq!(
        root["b"]["content"],
        json!({ "kind": "session", "id": id, "name": "worker", "machine": me["name"], "machine_id": me["id"] })
    );
    // The host keeps it: `get` shows the same.
    assert_eq!(running.json(&["ui", "get", "--json"]), split);

    let b = root["b"]["id"].as_u64().unwrap();
    let split_id = root["id"].as_u64().unwrap();
    let human = running.ok(&["ui", "get"]);
    let machine = me["name"].as_str().unwrap();
    assert_eq!(
        human,
        format!(
            "page 1 (shown): 1\n  split {split_id} right 0.3\n    pane 2: selected\n    \
             pane {b}: session worker ({id}) on {machine}\n"
        )
    );
}

#[test]
fn put_close_and_ratio_each_apply_one_change() {
    let running = start_host();
    let id = running.session("worker");
    let split = running.json(&["ui", "split", "2", "down", "--json"]);
    let root = &split["layout"]["pages"][0]["root"];
    assert_eq!(root["dir"], "down");
    assert_eq!(root["ratio"], 0.5);
    assert_eq!(root["b"]["content"], json!({ "kind": "empty" }));
    let (split_id, b) = (
        root["id"].as_u64().unwrap().to_string(),
        root["b"]["id"].as_u64().unwrap().to_string(),
    );

    let put = running.json(&["ui", "put", &b, "status", "--json"]);
    assert_eq!(
        put["layout"]["pages"][0]["root"]["b"]["content"],
        json!({ "kind": "view", "view": "status" })
    );
    let put = running.json(&["ui", "put", "2", &id, "--json"]);
    assert_eq!(
        put["layout"]["pages"][0]["root"]["a"]["content"]["id"],
        id.as_str()
    );
    let put = running.json(&["ui", "put", "2", "empty", "--json"]);
    assert_eq!(
        put["layout"]["pages"][0]["root"]["a"]["content"],
        json!({ "kind": "empty" })
    );

    let ratio = running.json(&["ui", "ratio", &split_id, "0.25", "--json"]);
    assert_eq!(ratio["layout"]["pages"][0]["root"]["ratio"], 0.25);
    // Clamped by the host, as the layout's ratios always are.
    let ratio = running.json(&["ui", "ratio", &split_id, "1", "--json"]);
    assert_eq!(ratio["layout"]["pages"][0]["root"]["ratio"], 0.95);

    let closed = running.json(&["ui", "close", "2", "--json"]);
    assert_eq!(
        closed["layout"]["pages"][0]["root"],
        json!({ "kind": "leaf", "id": b.parse::<u64>().unwrap(), "content": { "kind": "view", "view": "status" } })
    );
}

#[test]
fn pages_are_made_selected_renamed_and_closed_by_id_or_name() {
    let running = start_host();
    let made = running.json(&["ui", "page", "new", "logs", "--json"]);
    let pages = made["layout"]["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 2, "{made}");
    assert_eq!(pages[1]["name"], "logs");
    assert_eq!(made["layout"]["current"], pages[1]["id"]);
    assert_eq!(pages[1]["root"]["content"], json!({ "kind": "empty" }));

    let selected = running.json(&["ui", "page", "select", "1", "--json"]);
    assert_eq!(selected["layout"]["current"], 1);
    let renamed = running.json(&["ui", "page", "rename", "logs", "work", "--json"]);
    assert_eq!(renamed["layout"]["pages"][1]["name"], "work");
    let human = running.ok(&["ui", "page", "select", "work"]);
    assert!(
        human.contains(": work") && human.contains("(shown): work"),
        "{human}"
    );
    let closed = running.json(&["ui", "page", "close", "work", "--json"]);
    assert_eq!(closed["layout"]["pages"].as_array().unwrap().len(), 1);
    assert_eq!(closed["layout"]["current"], 1);

    let unnamed = running.json(&["ui", "page", "new", "--json"]);
    assert_eq!(unnamed["layout"]["pages"][1]["name"], "2");
}

#[test]
fn a_change_the_host_refuses_fails_saying_why_and_changes_nothing() {
    let running = start_host();
    let before = running.ok(&["ui", "get"]);
    let stderr = running.fails(&["ui", "close", "99"]);
    assert!(stderr.contains("no pane 99"), "{stderr}");
    let stderr = running.fails(&["ui", "ratio", "2", "0.5"]);
    assert!(stderr.contains("pane 2 isn't split"), "{stderr}");
    let stderr = running.fails(&["ui", "page", "select", "nope"]);
    assert!(stderr.contains("no page `nope`"), "{stderr}");
    let stderr = running.fails(&["ui", "put", "2", "nobody"]);
    assert!(stderr.contains("`nobody`"), "{stderr}");
    assert_eq!(running.ok(&["ui", "get"]), before);
}

#[test]
fn bad_arguments_fail_with_the_usage() {
    let running = start_host();
    for args in [
        &["ui"][..],
        &["ui", "bogus"],
        &["ui", "get", "extra"],
        &["ui", "split", "2", "left"],
        &["ui", "split", "x", "right"],
        &["ui", "split", "2", "right", "--ratio", "2"],
        &["ui", "ratio", "2", "half"],
        &["ui", "close", "2", "--session", "s"],
        &["ui", "screenshot", "--size", "120"],
        &["ui", "screenshot", "--size", "0x10"],
    ] {
        let stderr = running.fails(args);
        assert!(stderr.contains("usage: shepherd ui"), "{args:?}: {stderr}");
    }
}

#[test]
fn a_screenshot_draws_the_window_with_each_panes_session_as_it_is_now() {
    let running = start_host();
    let id = running.session("worker");
    let shot = running.painted(&["ui", "screenshot", "--size", "80x12"]);
    let lines: Vec<&str> = shot.lines().collect();
    assert_eq!(lines.len(), 12, "{shot}");
    assert!(lines.iter().all(|l| l.chars().count() <= 80), "{shot}");
    // The sidebar lists the session, and the one pane (showing the selection) its terminal.
    assert!(shot.contains("worker"), "{shot}");
    assert!(lines[0].contains(PAINTED), "{shot}");

    let json = running.json(&["ui", "screenshot", "--size", "80x12", "--json"]);
    assert_eq!(keys(&json), ["cols", "lines", "page", "panes", "rows"]);
    assert_eq!(json["cols"], 80);
    assert_eq!(json["rows"], 12);
    assert_eq!(json["page"], 1);
    let panes = json["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 1);
    assert_eq!(
        keys(&panes[0]),
        ["cols", "content", "id", "rows", "selected", "x", "y"]
    );
    // The selection is the session a newly opened window selects: the first listed.
    assert_eq!(panes[0]["content"]["id"], id.as_str());
    assert_eq!(panes[0]["selected"], true);
    // One pane takes the whole pane area, right of the sidebar.
    assert_eq!(
        (
            &panes[0]["x"],
            &panes[0]["y"],
            &panes[0]["cols"],
            &panes[0]["rows"]
        ),
        (&json!(24), &json!(0), &json!(56), &json!(12))
    );
    assert_eq!(json["lines"].as_array().unwrap().len(), 12);
}

#[test]
fn a_screenshot_of_another_page_draws_it_and_leaves_the_shown_page_alone() {
    let running = start_host();
    running.session("worker");
    running.ok(&["ui", "page", "new", "logs"]);
    running.ok(&["ui", "split", "4", "right", "--session", "worker"]);
    running.ok(&["ui", "page", "select", "1"]);
    let before = running.ok(&["ui", "get"]);

    let shot = running.painted(&["ui", "screenshot", "--page", "logs", "--size", "100x12"]);
    // With two pages, the page bar names both; the split's second pane shows the session.
    assert!(shot.contains("logs"), "{shot}");
    let json = running.json(&[
        "ui",
        "screenshot",
        "--page",
        "logs",
        "--size",
        "100x12",
        "--json",
    ]);
    assert_eq!(json["page"], 3);
    let panes = json["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 2, "{json}");
    assert_eq!(panes[1]["content"]["name"], "worker");
    assert_eq!(panes[1]["selected"], false);
    assert_eq!(running.ok(&["ui", "get"]), before);
}
