//! `shepherd archive list | show | find | restore | verify` (T43) with the built binary, in a temp `HOME` holding a
//! fixture archive: never the real `~/.shepherd`, `~/.claude` or `~/.codex`. The binary gets only `PATH`, `HOME` and
//! git's isolation variables, so `CLAUDE_CONFIG_DIR` and `CODEX_HOME` are unset and the agents' homes are under the temp
//! `HOME`. The `--json` shapes asserted here are the ones `docs/archive-cli.md` documents.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use jiff::Timestamp;
use serde_json::{Value, json};
use shepherd_archive::{
    AgentKind, Archive, BlobId, CommitHash, FileRole, NewFile, NewSnapshot, PatchId, RepoId,
    Session, SessionCommit, SnapshotReason,
};

const SHEPHERD: &str = env!("CARGO_BIN_EXE_shepherd");

const CLAUDE_ID: &str = "7d3c2a10-5b8e-4f61-9a2d-0c1e2f3a4b5c";
const CODEX_ID: &str = "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b";
const CODEX_ROLLOUT: &str =
    "rollout-2026-10-30T10-00-00-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl";
const CLAUDE_TRANSCRIPT: &[u8] = b"{\"type\":\"user\",\"message\":\"add a parser\"}\n";
const SUBAGENT_TRANSCRIPT: &[u8] = b"{\"type\":\"assistant\",\"message\":\"done\"}\n";
const CODEX_TRANSCRIPT: &[u8] = b"{\"type\":\"session_meta\"}\n";

fn hash(c: char) -> CommitHash {
    c.to_string().repeat(40).parse().unwrap()
}

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_second(seconds).unwrap()
}

/// A temp `HOME` (under /tmp, private), and git's isolation for anything run in it.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("shac")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn archive_root(&self) -> PathBuf {
        self.path().join(".shepherd").join("archive")
    }

    /// A worktree directory under the temp home, made.
    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.path().join("work").join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn shepherd_in(&self, cwd: &Path, args: &[&str]) -> Output {
        Command::new(SHEPHERD)
            .arg("archive")
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(std::env::vars_os().filter(|(k, _)| k == "PATH"))
            .env("HOME", self.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn shepherd(&self, args: &[&str]) -> Output {
        self.shepherd_in(self.path(), args)
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .env_clear()
            .envs(std::env::vars_os().filter(|(k, _)| k == "PATH"))
            .env("HOME", self.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn ok(out: &Output) -> String {
    assert!(out.status.success(), "failed: {out:?}");
    assert_eq!(stderr(out), "", "nothing on stderr");
    stdout(out)
}

fn json_of(out: &Output) -> Value {
    serde_json::from_str(&ok(out)).unwrap()
}

fn failed(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    stderr(out)
}

fn claude_session(id: &str, cwd: &Path) -> Session {
    Session {
        id: id.parse().unwrap(),
        repo: RepoId::from_root_commit(hash('1')),
        machine: "laptop".parse().unwrap(),
        agent: AgentKind::Claude,
        agent_session_id: CLAUDE_ID.parse().unwrap(),
        name: Some("add a parser".to_owned()),
        branch: Some("parser".parse().unwrap()),
        pr: None,
        base_commit: Some(hash('2')),
        cwd: cwd.to_owned(),
        created_at: at(1_793_400_000),
        archived_at: None,
    }
}

fn codex_session(id: &str, cwd: &Path) -> Session {
    Session {
        id: id.parse().unwrap(),
        repo: RepoId::from_root_commit(hash('1')),
        machine: "desktop".parse().unwrap(),
        agent: AgentKind::Codex,
        agent_session_id: CODEX_ID.parse().unwrap(),
        name: None,
        branch: None,
        pr: None,
        base_commit: None,
        cwd: cwd.to_owned(),
        created_at: at(1_793_500_000),
        archived_at: Some(at(1_793_600_000)),
    }
}

/// The fixture archive: a Claude session (main + subagent transcript, one commit) and a Codex session, both in repo
/// `1111…`, with worktrees under the temp home that `gone` lists as removed.
fn fixture(home: &Home) -> (PathBuf, PathBuf) {
    for agent_home in [".claude", ".codex"] {
        fs::create_dir(home.path().join(agent_home)).unwrap();
    }
    let claude_cwd = home.worktree("parser");
    let codex_cwd = home.path().join("work").join("gone");
    let archive = Archive::open(&home.archive_root()).unwrap();
    archive
        .put_session(&claude_session("s-claude", &claude_cwd))
        .unwrap();
    archive
        .put_session(&codex_session("s-codex", &codex_cwd))
        .unwrap();
    let project = home.path().join(".claude/projects/-work-parser");
    archive
        .add_snapshot(&NewSnapshot {
            session: "s-claude".parse().unwrap(),
            captured_at: at(1_793_400_600),
            head_commit: Some(hash('3')),
            reason: SnapshotReason::TurnEnd,
            files: vec![
                NewFile {
                    role: FileRole::Main,
                    original_path: project.join(format!("{CLAUDE_ID}.jsonl")),
                    content: CLAUDE_TRANSCRIPT,
                },
                NewFile {
                    role: FileRole::Subagent,
                    original_path: project
                        .join(CLAUDE_ID)
                        .join("subagents/agent-a1b2c3d4.jsonl"),
                    content: SUBAGENT_TRANSCRIPT,
                },
            ],
            commits: vec![SessionCommit {
                commit: hash('3'),
                patch_id: Some("4".repeat(40).parse().unwrap()),
            }],
        })
        .unwrap();
    archive
        .add_snapshot(&NewSnapshot {
            session: "s-codex".parse().unwrap(),
            captured_at: at(1_793_500_600),
            head_commit: None,
            reason: SnapshotReason::Close,
            files: vec![NewFile {
                role: FileRole::Rollout,
                original_path: home
                    .path()
                    .join(".codex/sessions/2026/10/30")
                    .join(CODEX_ROLLOUT),
                content: CODEX_TRANSCRIPT,
            }],
            commits: vec![],
        })
        .unwrap();
    (claude_cwd, codex_cwd)
}

fn session_json(s: &Session) -> Value {
    json!({
        "id": s.id.as_str(),
        "repo": s.repo.as_str(),
        "machine": s.machine.as_str(),
        "agent": s.agent.as_str(),
        "agent_session_id": s.agent_session_id.as_str(),
        "name": s.name,
        "branch": s.branch.as_ref().map(|b| b.as_str()),
        "pr": s.pr.map(|p| p.get()),
        "base_commit": s.base_commit.as_ref().map(|c| c.as_str()),
        "cwd": s.cwd.to_str().unwrap(),
        "created_at": s.created_at.to_string(),
        "archived_at": s.archived_at.map(|t| t.to_string()),
    })
}

// ---- list ----

#[test]
fn list_without_an_archive_says_so_and_creates_nothing() {
    let home = Home::new();
    let human = ok(&home.shepherd(&["list"]));
    assert_eq!(human, "No archived conversations yet.\n");
    assert_eq!(
        json_of(&home.shepherd(&["list", "--json"])),
        json!({ "sessions": [] })
    );
    assert!(!home.path().join(".shepherd").exists(), "nothing made");
}

#[test]
fn list_shows_every_session_oldest_first() {
    let home = Home::new();
    let (claude_cwd, codex_cwd) = fixture(&home);
    let human = ok(&home.shepherd(&["list"]));
    let lines: Vec<&str> = human.lines().collect();
    assert_eq!(lines.len(), 2, "{human}");
    assert!(lines[0].starts_with("s-claude  claude  "), "{human}");
    assert!(
        lines[0].contains("parser") && lines[0].contains("add a parser"),
        "{human}"
    );
    assert!(lines[1].starts_with("s-codex  codex  "), "{human}");

    assert_eq!(
        json_of(&home.shepherd(&["list", "--json"])),
        json!({ "sessions": [
            session_json(&claude_session("s-claude", &claude_cwd)),
            session_json(&codex_session("s-codex", &codex_cwd)),
        ]})
    );
}

/// Text from a transcript's session (a name, a branch) can't drive the terminal; JSON carries it as it is.
#[test]
fn hostile_names_are_printed_display_safe() {
    let home = Home::new();
    let cwd = home.worktree("x");
    let mut s = claude_session("s-hostile", &cwd);
    s.name = Some("evil\u{1b}]0;pwned\u{7}name\u{1b}[2J".to_owned());
    Archive::open(&home.archive_root())
        .unwrap()
        .put_session(&s)
        .unwrap();
    for args in [&["list"][..], &["show", "s-hostile"]] {
        let human = ok(&home.shepherd(args));
        assert!(
            !human.contains('\u{1b}') && !human.contains('\u{7}'),
            "{human:?}"
        );
        assert!(human.contains("evil"), "{human:?}");
    }
    let listed = json_of(&home.shepherd(&["list", "--json"]));
    assert_eq!(listed["sessions"][0]["name"], json!(s.name));
}

// ---- show ----

#[test]
fn show_prints_a_session_and_its_snapshots() {
    let home = Home::new();
    let (claude_cwd, _) = fixture(&home);
    let human = ok(&home.shepherd(&["show", "s-claude"]));
    for expected in [
        "Session s-claude",
        CLAUDE_ID,
        "add a parser",
        "parser",
        claude_cwd.to_str().unwrap(),
        "turn_end",
        &"3".repeat(40),
        "2 files",
    ] {
        assert!(human.contains(expected), "{expected:?} in {human}");
    }

    let project = home.path().join(".claude/projects/-work-parser");
    let shown = json_of(&home.shepherd(&["show", "s-claude", "--json"]));
    let snapshot_id = shown["snapshots"][0]["id"].clone();
    assert!(snapshot_id.is_i64(), "{shown}");
    assert_eq!(
        shown,
        json!({
            "session": session_json(&claude_session("s-claude", &claude_cwd)),
            "snapshots": [{
                "id": snapshot_id,
                "captured_at": at(1_793_400_600).to_string(),
                "reason": "turn_end",
                "head_commit": "3".repeat(40),
                "files": [
                    {
                        "role": "main",
                        "original_path": project.join(format!("{CLAUDE_ID}.jsonl")).to_str().unwrap(),
                        "blob": BlobId::of(CLAUDE_TRANSCRIPT).to_string(),
                        "bytes": CLAUDE_TRANSCRIPT.len(),
                    },
                    {
                        "role": "subagent",
                        "original_path": project.join(CLAUDE_ID).join("subagents/agent-a1b2c3d4.jsonl").to_str().unwrap(),
                        "blob": BlobId::of(SUBAGENT_TRANSCRIPT).to_string(),
                        "bytes": SUBAGENT_TRANSCRIPT.len(),
                    },
                ],
            }],
        })
    );
}

#[test]
fn show_of_an_unknown_session_fails_naming_it() {
    let home = Home::new();
    fixture(&home);
    let why = failed(&home.shepherd(&["show", "s-nope"]));
    assert!(
        why.contains("s-nope") && why.contains("not in the archive"),
        "{why}"
    );
}

// ---- find ----

/// A repo with a root commit and one change on `topic`, archived as the session that made it (with its patch-id from
/// capture's own path), plus a cherry-pick of that change on `trunk` that no session made.
struct FindRepo {
    dir: PathBuf,
    made: String,
    picked: String,
}

fn find_fixture(home: &Home) -> FindRepo {
    let dir = home.worktree("repo");
    home.git(&dir, &["init", "-q", "-b", "trunk"]);
    fs::write(dir.join("a.txt"), "one\n").unwrap();
    home.git(&dir, &["add", "a.txt"]);
    home.git(&dir, &["commit", "-q", "-m", "root"]);
    let root = home.git(&dir, &["rev-parse", "HEAD"]);
    home.git(&dir, &["switch", "-q", "-c", "topic"]);
    fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    home.git(&dir, &["commit", "-q", "-am", "the change"]);
    let made = home.git(&dir, &["rev-parse", "HEAD"]);
    home.git(&dir, &["switch", "-q", "trunk"]);
    home.git(&dir, &["cherry-pick", "-x", &made]);
    let picked = home.git(&dir, &["rev-parse", "HEAD"]);
    assert_ne!(made, picked);

    // The patch-id capture records: `Worktrees::commits_since`, the pinned path.
    use shepherd_core::ports::Worktrees;
    let git = shepherd_adapters::GitWorktrees::new(home.path().join("unused"));
    let since = git
        .commits_since(
            &shepherd_core::ports::WorktreePath::new(dir.clone()),
            &shepherd_core::ports::CommitHash::parse(&root).unwrap(),
        )
        .unwrap();
    let patch: PatchId = since[0].1.as_ref().unwrap().as_str().parse().unwrap();

    let archive = Archive::open(&home.archive_root()).unwrap();
    let mut s = claude_session("s-topic", &dir);
    s.repo = RepoId::from_root_commit(root.parse().unwrap());
    s.branch = Some("topic".parse().unwrap());
    s.pr = Some(shepherd_archive::PrNumber::new(
        std::num::NonZeroU32::new(7).unwrap(),
    ));
    archive.put_session(&s).unwrap();
    let mut other = claude_session("s-other", &dir);
    other.repo = s.repo.clone();
    other.branch = Some("elsewhere".parse().unwrap());
    archive.put_session(&other).unwrap();
    archive
        .add_snapshot(&NewSnapshot {
            session: "s-topic".parse().unwrap(),
            captured_at: at(1_793_400_600),
            head_commit: Some(made.parse().unwrap()),
            reason: SnapshotReason::TurnEnd,
            files: vec![NewFile {
                role: FileRole::Main,
                original_path: PathBuf::from(format!("/x/{CLAUDE_ID}.jsonl")),
                content: CLAUDE_TRANSCRIPT,
            }],
            commits: vec![SessionCommit {
                commit: made.parse().unwrap(),
                patch_id: Some(patch),
            }],
        })
        .unwrap();
    FindRepo { dir, made, picked }
}

fn found(home: &Home, cwd: &Path, query: &str) -> Vec<(String, String)> {
    let out = json_of(&home.shepherd_in(cwd, &["find", query, "--json"]));
    assert_eq!(out["query"], json!(query));
    out["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["how"].as_str().unwrap().to_owned(),
                m["session"]["id"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pair(how: &str, id: &str) -> (String, String) {
    (how.to_owned(), id.to_owned())
}

#[test]
fn find_takes_an_abbreviated_hash_a_rev_or_a_full_hash() {
    let home = Home::new();
    let repo = find_fixture(&home);
    let expected = vec![pair("commit", "s-topic")];
    assert_eq!(
        found(&home, &repo.dir, &repo.made[..8]),
        expected,
        "what git blame prints"
    );
    assert_eq!(found(&home, &repo.dir, &repo.made), expected);
    assert_eq!(found(&home, &repo.dir, "topic~0"), expected);

    let out = json_of(&home.shepherd_in(&repo.dir, &["find", &repo.made[..8], "--json"]));
    let m = &out["matches"][0];
    assert_eq!(m["commit"], json!(repo.made));
    assert!(
        m["patch_id"].is_string() && m["first_snapshot"].is_i64(),
        "{out}"
    );
    assert_eq!(m["session"]["branch"], json!("topic"));

    let human = ok(&home.shepherd_in(&repo.dir, &["find", &repo.made[..8]]));
    assert!(
        human.contains("s-topic") && human.contains(&repo.made),
        "{human}"
    );
}

#[test]
fn find_falls_back_to_the_patch_id_for_a_rebased_or_cherry_picked_commit() {
    let home = Home::new();
    let repo = find_fixture(&home);
    assert_eq!(
        found(&home, &repo.dir, &repo.picked[..10]),
        vec![pair("patch_id", "s-topic")]
    );
    assert_eq!(
        found(&home, &repo.dir, "HEAD"),
        vec![pair("patch_id", "s-topic")]
    );
    let human = ok(&home.shepherd_in(&repo.dir, &["find", "HEAD"]));
    assert!(human.contains("patch-id"), "{human}");
}

/// A squash merge makes a commit no session made: the branch or the PR finds it.
#[test]
fn find_by_branch_and_by_pr_in_the_current_repo() {
    let home = Home::new();
    let repo = find_fixture(&home);
    // `topic` names a commit (s-topic made it) and is a branch s-topic worked on: listed once, by commit.
    assert_eq!(
        found(&home, &repo.dir, "topic"),
        vec![pair("commit", "s-topic")]
    );
    // A branch that no longer exists in git is still a name the archive knows.
    assert_eq!(
        found(&home, &repo.dir, "elsewhere"),
        vec![pair("branch", "s-other")]
    );
    assert_eq!(found(&home, &repo.dir, "#7"), vec![pair("pr", "s-topic")]);
    assert_eq!(found(&home, &repo.dir, "#8"), vec![]);
    assert_eq!(found(&home, &repo.dir, "no-such-thing"), vec![]);
    let human = ok(&home.shepherd_in(&repo.dir, &["find", "no-such-thing"]));
    assert!(human.contains("No archived conversation"), "{human}");
}

#[test]
fn find_outside_a_repo_takes_a_full_hash_only() {
    let home = Home::new();
    let repo = find_fixture(&home);
    let outside = home.worktree("plain");
    assert_eq!(
        found(&home, &outside, &repo.made),
        vec![pair("commit", "s-topic")]
    );
    assert_eq!(found(&home, &outside, &repo.made[..8]), vec![]);
    let why = failed(&home.shepherd_in(&outside, &["find", "#7"]));
    assert!(why.contains("repo"), "{why}");
}

// ---- restore ----

#[test]
fn restore_into_the_recorded_worktree_prints_how_to_resume() {
    let home = Home::new();
    let (claude_cwd, _) = fixture(&home);
    let human = ok(&home.shepherd(&["restore", "s-claude"]));
    assert!(
        human.contains(&format!("claude --resume {CLAUDE_ID}")),
        "{human}"
    );
    assert!(
        human.contains(&format!("cd {}", claude_cwd.display())),
        "{human}"
    );

    let project = home.path().join(".claude/projects").join(
        claude_cwd
            .to_str()
            .unwrap()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
    );
    assert_eq!(
        fs::read(project.join(format!("{CLAUDE_ID}.jsonl"))).unwrap(),
        CLAUDE_TRANSCRIPT
    );
    assert_eq!(
        fs::read(
            project
                .join(CLAUDE_ID)
                .join("subagents/agent-a1b2c3d4.jsonl")
        )
        .unwrap(),
        SUBAGENT_TRANSCRIPT
    );

    // Again: the same files are already there, so it is a no-op, with the same answer.
    assert_eq!(
        json_of(&home.shepherd(&["restore", "s-claude", "--json"])),
        json!({
            "session": "s-claude",
            "agent": "claude",
            "agent_session_id": CLAUDE_ID,
            "cwd": claude_cwd.to_str().unwrap(),
            "command": ["claude", "--resume", CLAUDE_ID],
        })
    );
}

#[test]
fn restore_needs_a_cwd_once_the_worktree_is_gone() {
    let home = Home::new();
    let (_, codex_cwd) = fixture(&home);
    let why = failed(&home.shepherd(&["restore", "s-codex"]));
    assert!(
        why.contains("--cwd") && why.contains(codex_cwd.to_str().unwrap()),
        "{why}"
    );
    assert!(
        !home.path().join(".codex/sessions").exists(),
        "nothing restored"
    );

    let target = home.worktree("again");
    let restored = json_of(&home.shepherd(&[
        "restore",
        "s-codex",
        "--cwd",
        target.to_str().unwrap(),
        "--json",
    ]));
    assert_eq!(
        restored,
        json!({
            "session": "s-codex",
            "agent": "codex",
            "agent_session_id": CODEX_ID,
            "cwd": target.to_str().unwrap(),
            "command": ["codex", "resume", CODEX_ID],
        })
    );
    let rollout = home
        .path()
        .join(".codex/sessions/2026/10/30")
        .join(CODEX_ROLLOUT);
    assert_eq!(fs::read(&rollout).unwrap(), CODEX_TRANSCRIPT);
}

#[test]
fn restore_never_overwrites_a_different_file() {
    let home = Home::new();
    let (claude_cwd, _) = fixture(&home);
    ok(&home.shepherd(&["restore", "s-claude"]));
    let project = home.path().join(".claude/projects").join(
        claude_cwd
            .to_str()
            .unwrap()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
    );
    let main = project.join(format!("{CLAUDE_ID}.jsonl"));
    fs::write(&main, b"the agent kept going\n").unwrap();
    let why = failed(&home.shepherd(&["restore", "s-claude"]));
    assert!(why.contains(main.to_str().unwrap()), "{why}");
    assert_eq!(fs::read(&main).unwrap(), b"the agent kept going\n");
}

#[test]
fn restore_needs_the_agents_home() {
    let home = Home::new();
    fixture(&home);
    fs::remove_dir(home.path().join(".claude")).unwrap();
    let why = failed(&home.shepherd(&["restore", "s-claude"]));
    assert!(
        why.contains(".claude") && why.contains("CLAUDE_CONFIG_DIR"),
        "{why}"
    );
    assert!(!home.path().join(".claude").exists(), "not made");
}

// ---- verify ----

#[test]
fn verify_reports_a_healthy_archive() {
    let home = Home::new();
    fixture(&home);
    assert!(ok(&home.shepherd(&["verify"])).contains("healthy"));
    assert_eq!(
        json_of(&home.shepherd(&["verify", "--json"])),
        json!({ "ok": true, "bad_rows": [], "orphans": [] })
    );
}

#[test]
fn verify_reports_corrupt_and_missing_blobs_and_orphans_and_fails() {
    let home = Home::new();
    fixture(&home);
    let root = home.archive_root();
    let blob_path = |content: &[u8]| {
        let hex = BlobId::of(content).to_string();
        root.join("blobs")
            .join(&hex[..2])
            .join(format!("{hex}.zst"))
    };
    fs::remove_file(blob_path(SUBAGENT_TRANSCRIPT)).unwrap();
    fs::write(blob_path(CODEX_TRANSCRIPT), b"not zstd").unwrap();
    let orphan = shepherd_archive::BlobStore::open(&root)
        .unwrap()
        .put(b"nobody refers to me")
        .unwrap();

    let out = home.shepherd(&["verify", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["orphans"], json!([orphan.to_string()]));
    let bad = report["bad_rows"].as_array().unwrap();
    let kinds: Vec<(&str, &str)> = bad
        .iter()
        .map(|r| (r["role"].as_str().unwrap(), r["problem"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [("subagent", "missing"), ("rollout", "corrupt")],
        "{report}"
    );
    for row in bad {
        assert!(row["snapshot"].is_i64() && row["blob"].is_string() && row["error"].is_string());
        assert!(row["original_path"].is_string());
    }

    let human = home.shepherd(&["verify"]);
    assert_eq!(human.status.code(), Some(1));
    let text = stdout(&human);
    assert!(
        text.contains("missing") && text.contains("corrupt"),
        "{text}"
    );
    assert!(text.contains(&orphan.to_string()), "{text}");
}

// ---- reading beside a host ----

/// What a host would be doing: a read-write handle open (and writing) the whole time. The commands read beside it,
/// never block it, and leave the index and its WAL as they were.
#[test]
fn reads_beside_a_writing_host_change_nothing() {
    let home = Home::new();
    let repo = find_fixture(&home);
    let host = Archive::open(&home.archive_root()).unwrap();
    host.put_session(&claude_session("s-live", &repo.dir))
        .unwrap();
    let index =
        |suffix: &str| fs::read(home.archive_root().join(format!("index.sqlite{suffix}"))).unwrap();
    let before = (index(""), index("-wal"));

    let listed = json_of(&home.shepherd(&["list", "--json"]));
    assert_eq!(
        listed["sessions"].as_array().unwrap().len(),
        3,
        "sees the host's write"
    );
    ok(&home.shepherd(&["show", "s-topic"]));
    ok(&home.shepherd_in(&repo.dir, &["find", &repo.made[..8]]));
    ok(&home.shepherd(&["verify"]));

    assert!(
        (index(""), index("-wal")) == before,
        "the index and its WAL are untouched"
    );
    host.put_session(&claude_session("s-after", &repo.dir))
        .unwrap();
}

// ---- usage ----

#[test]
fn usage_errors_name_the_commands() {
    let home = Home::new();
    for args in [
        &[][..],
        &["bogus"],
        &["show"],
        &["list", "--bogus"],
        &["restore", "s", "--cwd"],
    ] {
        let why = failed(&home.shepherd(args));
        assert!(why.contains("usage: shepherd archive"), "{args:?}: {why}");
    }
}
