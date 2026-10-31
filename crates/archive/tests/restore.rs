//! Restore (T41): an archived session's transcripts go back where each agent's resume looks for them, for a given cwd,
//! under a given agent home, never overwriting anything and never following a symlink. On the provisional fixtures in
//! `tests/fixtures/{claude,codex}` (see their README), captured and restored in tempdirs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use jiff::Timestamp;
use shepherd_archive::{
    AgentKind, AgentSessionId, Archive, ArchiveError, Capture, CaptureLimits, CommitHash, FileRole,
    MachineId, NewFile, NewSnapshot, RepoId, RestoreError, Resume, Session, SessionId,
    SnapshotReason,
};
use tempfile::TempDir;

const CLAUDE_SESSION: &str = "7d3c2a10-5b8e-4f61-9a2d-0c1e2f3a4b5c";
const CODEX_SESSION: &str = "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b";
const CLAUDE_MAIN: &str = "projects/-work-app/7d3c2a10-5b8e-4f61-9a2d-0c1e2f3a4b5c.jsonl";
const CLAUDE_SUBAGENTS: &str = "projects/-work-app/7d3c2a10-5b8e-4f61-9a2d-0c1e2f3a4b5c/subagents";
const SUBAGENT_A: &str = "agent-a1b2c3d4.jsonl";
const SUBAGENT_B: &str = "agent-e5f6a7b8.jsonl";
const CODEX_ROLLOUT_NAME: &str =
    "rollout-2026-10-30T10-00-00-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl";
const CODEX_ROLLOUT: &str =
    "sessions/2026/10/30/rollout-2026-10-30T10-00-00-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl";

/// The cwd restores go to in most tests: not the captured `/work/app`. Claude's project dir for it is `NEW_PROJECT`.
const NEW_CWD: &str = "/srv/checkouts/app.v2/wt_3";
const NEW_PROJECT: &str = "projects/-srv-checkouts-app-v2-wt-3";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Copies the fixture tree `from` to `to`: directories 0700, files 0600, as the agents keep them.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    fs::set_permissions(to, fs::Permissions::from_mode(0o700)).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}

fn fixture_bytes(agent: &str, rel: &str) -> Vec<u8> {
    fs::read(fixtures().join(agent).join("home").join(rel)).unwrap()
}

fn at(micros: i64) -> Timestamp {
    Timestamp::from_microsecond(micros).unwrap()
}

fn sid() -> SessionId {
    "s1".parse().unwrap()
}

fn hash(c: char) -> CommitHash {
    c.to_string().repeat(40).parse().unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

/// Every path under `dir` (directories and files), relative to it, sorted. Doesn't follow symlinks.
fn tree(dir: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// An archive holding one session for `agent` (its own session id `agent_session`), captured from that agent's
/// fixture home (installed at `<tmp>/source-home`), and an empty agent home to restore into at `<tmp>/home`.
struct Setup {
    tmp: TempDir,
    source: PathBuf,
    home: PathBuf,
    archive: Archive,
}

impl Setup {
    fn new(agent: AgentKind, agent_session: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let source = tmp.path().join("source-home");
        copy_tree(&fixtures().join(agent.as_str()).join("home"), &source);
        let home = tmp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let archive = Archive::open(&tmp.path().join("archive")).unwrap();
        archive
            .put_session(&Session {
                id: sid(),
                repo: RepoId::from_root_commit(hash('a')),
                machine: "laptop".parse::<MachineId>().unwrap(),
                agent,
                agent_session_id: agent_session.parse::<AgentSessionId>().unwrap(),
                name: None,
                branch: None,
                pr: None,
                base_commit: None,
                cwd: PathBuf::from("/work/app"),
                created_at: at(1_000),
                archived_at: None,
            })
            .unwrap();
        Self {
            tmp,
            source,
            home,
            archive,
        }
    }

    /// A Claude session captured from the fixture: the main transcript and two subagent transcripts.
    fn claude() -> Self {
        let s = Self::new(AgentKind::Claude, CLAUDE_SESSION);
        s.capture(&s.source.join(CLAUDE_MAIN), at(2_000));
        s
    }

    /// A Codex session captured from the fixture: its rollout.
    fn codex() -> Self {
        let s = Self::new(AgentKind::Codex, CODEX_SESSION);
        s.capture(&s.source.join(CODEX_ROLLOUT), at(2_000));
        s
    }

    fn capture(&self, transcript: &Path, when: Timestamp) {
        self.archive
            .capture(&Capture {
                session: sid(),
                captured_at: when,
                head_commit: None,
                reason: SnapshotReason::TurnEnd,
                commits: Vec::new(),
                agent_home: &self.source,
                transcript_path: transcript,
                limits: CaptureLimits::default(),
            })
            .unwrap();
    }

    fn restore_to(&self, cwd: &Path, home: &Path) -> Result<Resume, ArchiveError> {
        self.archive.restore(&sid(), cwd, home)
    }

    fn restore(&self) -> Result<Resume, ArchiveError> {
        self.restore_to(Path::new(NEW_CWD), &self.home)
    }

    /// What restoring failed with, after checking the home is exactly as `before`.
    fn refused(&self, cwd: &Path, home: &Path, before: &[PathBuf]) -> RestoreError {
        let err = match self.restore_to(cwd, home) {
            Ok(r) => panic!("restoring succeeded: {r:?}"),
            Err(ArchiveError::Restore(e)) => e,
            Err(e) => panic!("expected a restore error, got {e:?}"),
        };
        assert_eq!(tree(&self.home), before, "the home changed");
        err
    }

    fn outside(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }
}

fn claude_resume(cwd: &str) -> Resume {
    Resume {
        agent: AgentKind::Claude,
        agent_session_id: CLAUDE_SESSION.parse().unwrap(),
        cwd: PathBuf::from(cwd),
    }
}

/// The files a Claude restore of the fixture session writes into the project dir `project`.
fn claude_files(project: &str) -> Vec<PathBuf> {
    let session = format!("{project}/{CLAUDE_SESSION}");
    [
        "projects".to_owned(),
        project.to_owned(),
        session.clone(),
        format!("{session}.jsonl"),
        format!("{session}/subagents"),
        format!("{session}/subagents/{SUBAGENT_A}"),
        format!("{session}/subagents/{SUBAGENT_B}"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect::<std::collections::BTreeSet<_>>()
    .into_iter()
    .collect()
}

// ---------- round trips ----------

#[test]
fn claude_restore_puts_every_transcript_in_the_project_dir_for_the_new_cwd() {
    let s = Setup::claude();
    assert_eq!(s.restore().unwrap(), claude_resume(NEW_CWD));
    assert_eq!(tree(&s.home), claude_files(NEW_PROJECT));
    let project = s.home.join(NEW_PROJECT);
    assert_eq!(
        fs::read(project.join(format!("{CLAUDE_SESSION}.jsonl"))).unwrap(),
        fixture_bytes("claude", CLAUDE_MAIN)
    );
    for name in [SUBAGENT_A, SUBAGENT_B] {
        assert_eq!(
            fs::read(project.join(CLAUDE_SESSION).join("subagents").join(name)).unwrap(),
            fixture_bytes("claude", &format!("{CLAUDE_SUBAGENTS}/{name}")),
            "{name}"
        );
    }
}

#[test]
fn claude_restore_does_not_rewrite_the_cwd_inside_the_transcript() {
    // Nothing in Claude Code's docs says resume checks the transcript's `cwd` fields (T44 checks the real agent).
    let s = Setup::claude();
    s.restore().unwrap();
    let restored = fs::read_to_string(
        s.home
            .join(NEW_PROJECT)
            .join(format!("{CLAUDE_SESSION}.jsonl")),
    )
    .unwrap();
    assert!(restored.contains("\"cwd\":\"/work/app\""));
    assert!(!restored.contains(NEW_CWD));
}

#[test]
fn claude_project_dir_is_the_cwd_with_every_non_alphanumeric_character_a_dash() {
    // The documented rule ("Manage sessions", "Where transcripts are stored"), and the docs' own examples.
    for (cwd, project) in [
        ("/home/user/work/my-repo", "-home-user-work-my-repo"),
        ("/Users/you/my-project", "-Users-you-my-project"),
        ("/work/app", "-work-app"),
        ("/a b/c.d/e_f/G9~h", "-a-b-c-d-e-f-G9-h"),
        ("/srv/x/", "-srv-x"),
        ("/", "-"),
    ] {
        let s = Setup::claude();
        s.restore_to(Path::new(cwd), &s.home).unwrap();
        let main = s
            .home
            .join("projects")
            .join(project)
            .join(format!("{CLAUDE_SESSION}.jsonl"));
        assert!(main.is_file(), "{cwd} -> {project}: {:?}", tree(&s.home));
    }
}

#[test]
fn claude_restore_into_the_capture_home_for_another_cwd_leaves_the_original() {
    let s = Setup::claude();
    s.restore_to(Path::new(NEW_CWD), &s.source).unwrap();
    assert_eq!(
        fs::read(
            s.source
                .join(NEW_PROJECT)
                .join(format!("{CLAUDE_SESSION}.jsonl"))
        )
        .unwrap(),
        fixture_bytes("claude", CLAUDE_MAIN)
    );
    assert_eq!(
        fs::read(s.source.join(CLAUDE_MAIN)).unwrap(),
        fixture_bytes("claude", CLAUDE_MAIN)
    );
}

#[test]
fn claude_restore_without_subagents_writes_only_the_main_transcript() {
    let s = Setup::new(AgentKind::Claude, CLAUDE_SESSION);
    fs::remove_dir_all(s.source.join("projects/-work-app").join(CLAUDE_SESSION)).unwrap();
    s.capture(&s.source.join(CLAUDE_MAIN), at(2_000));
    s.restore().unwrap();
    assert_eq!(
        tree(&s.home),
        vec![
            PathBuf::from("projects"),
            PathBuf::from(NEW_PROJECT),
            PathBuf::from(format!("{NEW_PROJECT}/{CLAUDE_SESSION}.jsonl")),
        ]
    );
}

#[test]
fn codex_restore_puts_the_rollout_in_its_dated_sessions_dir() {
    // codex-rs/rollout/src/recorder.rs: `sessions/<year>/<MM>/<DD>/`, the date the file name carries; resume by id
    // walks that tree by file name and doesn't filter by cwd (codex-rs/rollout/src/list.rs).
    let s = Setup::codex();
    assert_eq!(
        s.restore().unwrap(),
        Resume {
            agent: AgentKind::Codex,
            agent_session_id: CODEX_SESSION.parse().unwrap(),
            cwd: PathBuf::from(NEW_CWD),
        }
    );
    assert_eq!(
        tree(&s.home),
        [
            "sessions",
            "sessions/2026",
            "sessions/2026/10",
            "sessions/2026/10/30",
            CODEX_ROLLOUT
        ]
        .map(PathBuf::from)
    );
    assert_eq!(
        fs::read(s.home.join(CODEX_ROLLOUT)).unwrap(),
        fixture_bytes("codex", CODEX_ROLLOUT)
    );
}

#[test]
fn codex_dirs_come_from_the_date_in_the_rollout_name_not_where_it_was_captured() {
    let s = Setup::new(AgentKind::Codex, CODEX_SESSION);
    let odd = s.source.join("sessions/elsewhere");
    fs::create_dir(&odd).unwrap();
    fs::set_permissions(&odd, fs::Permissions::from_mode(0o700)).unwrap();
    let name = format!("rollout-2025-01-02T03-04-05-{CODEX_SESSION}_9.jsonl");
    fs::write(odd.join(&name), b"{\"type\":\"session_meta\"}\n").unwrap();
    s.capture(&odd.join(&name), at(2_000));
    s.restore().unwrap();
    assert!(
        s.home.join("sessions/2025/01/02").join(&name).is_file(),
        "{:?}",
        tree(&s.home)
    );
}

#[test]
fn restore_takes_the_latest_snapshot() {
    let s = Setup::claude();
    let mut grown = fixture_bytes("claude", CLAUDE_MAIN);
    grown.extend_from_slice(b"{\"type\":\"user\",\"later\":true}\n");
    fs::write(s.source.join(CLAUDE_MAIN), &grown).unwrap();
    s.capture(&s.source.join(CLAUDE_MAIN), at(3_000));
    s.restore().unwrap();
    assert_eq!(
        fs::read(
            s.home
                .join(NEW_PROJECT)
                .join(format!("{CLAUDE_SESSION}.jsonl"))
        )
        .unwrap(),
        grown
    );
}

#[test]
fn created_dirs_are_0700_and_files_0600_and_existing_dirs_are_left_alone() {
    let s = Setup::claude();
    let projects = s.home.join("projects");
    fs::create_dir(&projects).unwrap();
    fs::set_permissions(&projects, fs::Permissions::from_mode(0o755)).unwrap();
    s.restore().unwrap();
    assert_eq!(mode(&projects), 0o755, "an existing dir is not chmodded");
    let project = s.home.join(NEW_PROJECT);
    assert_eq!(mode(&project), 0o700);
    assert_eq!(mode(&project.join(CLAUDE_SESSION)), 0o700);
    assert_eq!(mode(&project.join(CLAUDE_SESSION).join("subagents")), 0o700);
    assert_eq!(
        mode(&project.join(format!("{CLAUDE_SESSION}.jsonl"))),
        0o600
    );
    assert_eq!(
        mode(
            &project
                .join(CLAUDE_SESSION)
                .join("subagents")
                .join(SUBAGENT_A)
        ),
        0o600
    );
}

#[test]
fn a_home_reached_through_a_symlink_is_resolved_first() {
    let s = Setup::codex();
    let link = s.outside("home-link");
    symlink(&s.home, &link).unwrap();
    s.restore_to(Path::new(NEW_CWD), &link).unwrap();
    assert!(s.home.join(CODEX_ROLLOUT).is_file());
}

// ---------- never overwrite ----------

#[test]
fn an_existing_different_file_is_an_error_naming_it_and_nothing_is_written() {
    let s = Setup::claude();
    let project = s.home.join(NEW_PROJECT);
    fs::create_dir_all(&project).unwrap();
    let main = project.join(format!("{CLAUDE_SESSION}.jsonl"));
    fs::write(&main, b"someone else's\n").unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    let real_main = fs::canonicalize(&project)
        .unwrap()
        .join(format!("{CLAUDE_SESSION}.jsonl"));
    assert_eq!(err, RestoreError::Exists(real_main.clone()));
    assert!(err.to_string().contains(real_main.to_str().unwrap()));
    assert_eq!(fs::read(&main).unwrap(), b"someone else's\n");
}

#[test]
fn a_different_subagent_file_is_refused_before_the_main_transcript_is_written() {
    let s = Setup::claude();
    let subagents = s
        .home
        .join(NEW_PROJECT)
        .join(CLAUDE_SESSION)
        .join("subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(subagents.join(SUBAGENT_B), b"different\n").unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    assert!(matches!(err, RestoreError::Exists(p) if p.ends_with(SUBAGENT_B)));
}

#[test]
fn a_prefix_of_the_content_is_still_different() {
    let s = Setup::codex();
    let rollout = s.home.join(CODEX_ROLLOUT);
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let full = fixture_bytes("codex", CODEX_ROLLOUT);
    fs::write(&rollout, &full[..full.len() - 1]).unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    assert!(matches!(err, RestoreError::Exists(p) if p.ends_with(CODEX_ROLLOUT_NAME)));
}

#[test]
fn identical_files_are_a_no_op() {
    let s = Setup::claude();
    s.restore().unwrap();
    let main = s
        .home
        .join(NEW_PROJECT)
        .join(format!("{CLAUDE_SESSION}.jsonl"));
    let before = fs::metadata(&main).unwrap();
    let tree_before = tree(&s.home);
    assert_eq!(s.restore().unwrap(), claude_resume(NEW_CWD));
    let after = fs::metadata(&main).unwrap();
    assert_eq!(tree(&s.home), tree_before);
    assert_eq!(after.ino(), before.ino(), "the file was not replaced");
    assert_eq!(
        (after.mtime(), after.mtime_nsec()),
        (before.mtime(), before.mtime_nsec()),
        "the file was not rewritten"
    );
}

#[test]
fn some_identical_files_and_some_missing_restores_the_missing_ones() {
    let s = Setup::claude();
    s.restore().unwrap();
    let a = s
        .home
        .join(NEW_PROJECT)
        .join(CLAUDE_SESSION)
        .join("subagents")
        .join(SUBAGENT_A);
    fs::remove_file(&a).unwrap();
    s.restore().unwrap();
    assert_eq!(
        fs::read(&a).unwrap(),
        fixture_bytes("claude", &format!("{CLAUDE_SUBAGENTS}/{SUBAGENT_A}"))
    );
}

// ---------- symlinks and the wrong kinds of file ----------

#[test]
fn a_symlink_at_any_directory_below_the_home_is_refused_and_never_written_through() {
    for dir in [
        "projects".to_owned(),
        NEW_PROJECT.to_owned(),
        format!("{NEW_PROJECT}/{CLAUDE_SESSION}"),
        format!("{NEW_PROJECT}/{CLAUDE_SESSION}/subagents"),
    ] {
        let s = Setup::claude();
        let target = s.outside("elsewhere");
        fs::create_dir(&target).unwrap();
        let link = s.home.join(&dir);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();
        let before = tree(&s.home);
        let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
        assert!(
            matches!(&err, RestoreError::Symlink(p) if p.ends_with(&dir)),
            "{dir}: {err:?}"
        );
        assert!(tree(&target).is_empty(), "{dir}: written through the link");
    }
}

#[test]
fn a_symlink_at_a_codex_date_dir_is_refused() {
    let s = Setup::codex();
    let target = s.outside("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::create_dir(s.home.join("sessions")).unwrap();
    symlink(&target, s.home.join("sessions/2026")).unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    assert!(matches!(err, RestoreError::Symlink(p) if p.ends_with("sessions/2026")));
    assert!(tree(&target).is_empty());
}

#[test]
fn a_symlink_at_the_file_itself_is_refused_even_to_identical_content() {
    let s = Setup::codex();
    let rollout = s.home.join(CODEX_ROLLOUT);
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let same = s.outside("same.jsonl");
    fs::write(&same, fixture_bytes("codex", CODEX_ROLLOUT)).unwrap();
    symlink(&same, &rollout).unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    assert!(matches!(err, RestoreError::Symlink(p) if p.ends_with(CODEX_ROLLOUT_NAME)));
}

#[test]
fn a_dangling_symlink_at_the_file_is_refused_and_not_created_through() {
    let s = Setup::codex();
    let rollout = s.home.join(CODEX_ROLLOUT);
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let missing = s.outside("would-be-created.jsonl");
    symlink(&missing, &rollout).unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    assert!(matches!(err, RestoreError::Symlink(_)), "{err:?}");
    assert!(!missing.exists());
}

#[test]
fn a_file_where_a_directory_goes_is_refused() {
    let s = Setup::claude();
    fs::write(s.home.join("projects"), b"").unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    assert!(matches!(err, RestoreError::WrongType(p) if p.ends_with("projects")));
}

#[test]
fn a_directory_where_a_file_goes_is_refused() {
    let s = Setup::claude();
    fs::create_dir_all(
        s.home
            .join(NEW_PROJECT)
            .join(format!("{CLAUDE_SESSION}.jsonl")),
    )
    .unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    assert!(matches!(err, RestoreError::WrongType(_)), "{err:?}");
}

// ---------- bad input ----------

#[test]
fn a_relative_cwd_or_home_is_refused() {
    let s = Setup::claude();
    let before = tree(&s.home);
    assert_eq!(
        s.refused(Path::new("work/app"), &s.home, &before),
        RestoreError::NotAbsolute(PathBuf::from("work/app"))
    );
    assert_eq!(
        s.refused(Path::new(NEW_CWD), Path::new("home"), &before),
        RestoreError::NotAbsolute(PathBuf::from("home"))
    );
}

#[test]
fn a_dot_dot_in_the_cwd_or_home_is_refused() {
    let s = Setup::claude();
    let before = tree(&s.home);
    assert!(matches!(
        s.refused(Path::new("/work/../app"), &s.home, &before),
        RestoreError::ParentDir(_)
    ));
    let climbing = s.home.join("projects/../..").join("home");
    assert!(matches!(
        s.refused(Path::new(NEW_CWD), &climbing, &before),
        RestoreError::ParentDir(_)
    ));
}

#[test]
fn a_missing_home_is_an_error() {
    let s = Setup::claude();
    let before = tree(&s.home);
    let missing = s.outside("no-such-home");
    assert!(matches!(
        s.refused(Path::new(NEW_CWD), &missing, &before),
        RestoreError::Io { .. }
    ));
    assert!(!missing.exists());
}

#[test]
fn a_claude_cwd_that_is_not_ascii_is_refused() {
    // The docs don't say how Claude Code encodes non-ASCII characters (T44 checks the real agent).
    let s = Setup::claude();
    let before = tree(&s.home);
    assert!(matches!(
        s.refused(Path::new("/work/café"), &s.home, &before),
        RestoreError::UnsupportedCwd { .. }
    ));
}

#[test]
fn a_claude_cwd_whose_project_dir_name_exceeds_200_characters_is_refused() {
    // Over 200 characters Claude Code truncates and appends a hash of the path, which the docs don't specify.
    let s = Setup::claude();
    let before = tree(&s.home);
    let long = format!("/{}", "a".repeat(200));
    assert!(matches!(
        s.refused(Path::new(&long), &s.home, &before),
        RestoreError::UnsupportedCwd { .. }
    ));
    let exactly = format!("/{}", "a".repeat(199));
    s.restore_to(Path::new(&exactly), &s.home).unwrap();
    assert!(
        s.home
            .join("projects")
            .join(format!("-{}", "a".repeat(199)))
            .is_dir()
    );
}

#[test]
fn a_codex_cwd_needs_no_encoding() {
    let s = Setup::codex();
    s.restore_to(Path::new("/work/café"), &s.home).unwrap();
    assert!(s.home.join(CODEX_ROLLOUT).is_file());
}

#[test]
fn an_unknown_session_is_an_error() {
    let s = Setup::claude();
    let err = s
        .archive
        .restore(&"nope".parse().unwrap(), Path::new(NEW_CWD), &s.home)
        .unwrap_err();
    assert!(matches!(err, ArchiveError::UnknownSession(_)), "{err:?}");
}

#[test]
fn a_session_with_no_snapshot_is_an_error() {
    let s = Setup::new(AgentKind::Claude, CLAUDE_SESSION);
    let before = tree(&s.home);
    assert_eq!(
        s.refused(Path::new(NEW_CWD), &s.home, &before),
        RestoreError::NoSnapshot(sid())
    );
}

/// A snapshot's files: role and recorded path.
type Files<'a> = Vec<(FileRole, &'a str)>;

/// A session with one snapshot recorded directly (not captured) from `files`.
fn recorded(agent: AgentKind, agent_session: &str, files: &[(FileRole, &str)]) -> Setup {
    let s = Setup::new(agent, agent_session);
    s.archive
        .add_snapshot(&NewSnapshot {
            session: sid(),
            captured_at: at(2_000),
            head_commit: None,
            reason: SnapshotReason::Manual,
            files: files
                .iter()
                .map(|(role, path)| NewFile {
                    role: *role,
                    original_path: PathBuf::from(path),
                    content: b"{}\n",
                })
                .collect(),
            commits: Vec::new(),
        })
        .unwrap();
    s
}

#[test]
fn the_claude_main_transcript_is_named_after_the_session_whatever_it_was_recorded_as() {
    // Resume looks for `<session-id>.jsonl`; the recorded path is only where the file was.
    let s = recorded(
        AgentKind::Claude,
        CLAUDE_SESSION,
        &[(FileRole::Main, "/old/home/projects/-x/renamed.jsonl")],
    );
    s.restore().unwrap();
    assert_eq!(
        fs::read(
            s.home
                .join(NEW_PROJECT)
                .join(format!("{CLAUDE_SESSION}.jsonl"))
        )
        .unwrap(),
        b"{}\n"
    );
}

#[test]
fn a_snapshot_whose_files_dont_fit_the_agents_layout_is_refused() {
    let main = format!("/h/projects/-p/{CLAUDE_SESSION}.jsonl");
    let main2 = format!("/h/projects/-q/{CLAUDE_SESSION}.jsonl");
    let rollout = format!("/h/{CODEX_ROLLOUT}");
    let other_thread = "/h/sessions/2026/10/30/rollout-2026-10-30T10-00-00-0000.jsonl";
    let cases: Vec<(AgentKind, &str, Files)> = vec![
        // No main transcript.
        (AgentKind::Claude, CLAUDE_SESSION, vec![]),
        // Two.
        (
            AgentKind::Claude,
            CLAUDE_SESSION,
            vec![(FileRole::Main, &main), (FileRole::Main, &main2)],
        ),
        // A rollout in a Claude snapshot.
        (
            AgentKind::Claude,
            CLAUDE_SESSION,
            vec![(FileRole::Main, &main), (FileRole::Rollout, &rollout)],
        ),
        // Subagent names that aren't plain `.jsonl` file names, or repeat.
        (
            AgentKind::Claude,
            CLAUDE_SESSION,
            vec![
                (FileRole::Main, &main),
                (FileRole::Subagent, "/h/s/notes.txt"),
            ],
        ),
        (
            AgentKind::Claude,
            CLAUDE_SESSION,
            vec![(FileRole::Main, &main), (FileRole::Subagent, "/h/s/.jsonl")],
        ),
        (
            AgentKind::Claude,
            CLAUDE_SESSION,
            vec![(FileRole::Main, &main), (FileRole::Subagent, "/")],
        ),
        (
            AgentKind::Claude,
            CLAUDE_SESSION,
            vec![
                (FileRole::Main, &main),
                (FileRole::Subagent, "/h/a/x.jsonl"),
                (FileRole::Subagent, "/h/b/x.jsonl"),
            ],
        ),
        // A Claude session id that is not one path component.
        (
            AgentKind::Claude,
            "a/b",
            vec![(FileRole::Main, "/h/projects/-p/b.jsonl")],
        ),
        (
            AgentKind::Claude,
            "..",
            vec![(FileRole::Main, "/h/projects/-p/...jsonl")],
        ),
        // A Codex rollout for another thread, or not a rollout name at all, or a Claude role.
        (
            AgentKind::Codex,
            CODEX_SESSION,
            vec![(FileRole::Rollout, other_thread)],
        ),
        (
            AgentKind::Codex,
            CODEX_SESSION,
            vec![(FileRole::Rollout, "/h/sessions/x.jsonl")],
        ),
        (
            AgentKind::Codex,
            CODEX_SESSION,
            vec![(FileRole::Main, &rollout)],
        ),
        (AgentKind::Codex, CODEX_SESSION, vec![]),
    ];
    for (agent, session, files) in cases {
        let s = recorded(agent, session, &files);
        let before = tree(&s.home);
        let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
        assert!(
            matches!(err, RestoreError::BadSnapshot { .. }),
            "{agent} {session} {files:?}: {err:?}"
        );
    }
}

// ---------- failing midway ----------

#[test]
fn a_write_failing_midway_leaves_no_file_and_no_temp_behind() {
    // The subagents dir exists but can't be written: the main transcript's temp file is already written by then.
    let s = Setup::claude();
    let subagents = s
        .home
        .join(NEW_PROJECT)
        .join(CLAUDE_SESSION)
        .join("subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::set_permissions(&subagents, fs::Permissions::from_mode(0o500)).unwrap();
    let before = tree(&s.home);
    let err = s.refused(Path::new(NEW_CWD), &s.home, &before);
    fs::set_permissions(&subagents, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(err, RestoreError::Io { .. }), "{err:?}");
}

// ---------- changes between the check and the write ----------

/// The parent passes the child the tempdir holding the archive and the agent home, through this variable.
const CHILD_DIR: &str = "T41_CHILD_DIR";
const PAUSED: &str = shepherd_archive::PAUSE_BEFORE_PUBLISH_MARKER;
const PAUSING: &str = shepherd_archive::PAUSE_BEFORE_PUBLISH_SESSION;

/// Runs only as a child process started by `paused_restore`; does nothing in a normal test run. Restores the pausing
/// session and prints the outcome. With `T41_UMASK` set, restores under umask 277 without pausing.
#[test]
fn child_process() {
    #[allow(
        clippy::disallowed_methods,
        reason = "test plumbing: the parent passes its tempdir to its child"
    )]
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    #[allow(clippy::disallowed_methods, reason = "test plumbing")]
    if std::env::var_os("T41_UMASK").is_some() {
        // SAFETY: umask only sets this process's file mode mask; this child runs a single test.
        unsafe { libc::umask(0o277) };
    }
    let dir = Path::new(&dir);
    let archive = Archive::open(&dir.join("archive")).unwrap();
    let result = archive.restore(&sid(), Path::new(NEW_CWD), &dir.join("home"));
    println!("outcome: {result:?}");
}

/// A Claude session whose agent session id makes restore pause before publishing, captured with a main transcript and
/// one subagent transcript. Returns where restore puts them.
fn pausing_setup() -> (Setup, PathBuf, PathBuf) {
    let s = Setup::new(AgentKind::Claude, PAUSING);
    let project = s.source.join("projects/-work-app");
    let subagents = project.join(PAUSING).join("subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(project.join(format!("{PAUSING}.jsonl")), b"{\"main\":1}\n").unwrap();
    fs::write(subagents.join(SUBAGENT_A), b"{\"sub\":1}\n").unwrap();
    s.capture(&project.join(format!("{PAUSING}.jsonl")), at(2_000));
    let to = s.home.join(NEW_PROJECT);
    (
        s,
        to.join(format!("{PAUSING}.jsonl")),
        to.join(PAUSING).join("subagents").join(SUBAGENT_A),
    )
}

fn child(s: &Setup) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "child_process",
        "--test-threads=1",
        "--nocapture",
    ])
    .env(CHILD_DIR, s.tmp.path());
    cmd
}

/// Restores the pausing session in a child process, runs `act` while it is paused after writing its temp files and
/// before publishing any of them, and returns what the child printed after that.
fn paused_restore(s: &Setup, act: impl FnOnce()) -> String {
    let mut child = child(s)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    while !line.trim_end().ends_with(PAUSED) {
        line.clear();
        assert_ne!(
            out.read_line(&mut line).unwrap(),
            0,
            "the child exited before pausing"
        );
    }
    act();
    child.stdin.take().unwrap().write_all(b"go").unwrap();
    let mut rest = String::new();
    out.read_to_string(&mut rest).unwrap();
    assert!(child.wait().unwrap().success());
    rest
}

/// The names in `dir` that look like restore's temp files.
fn temps(dir: &Path) -> Vec<PathBuf> {
    tree(dir)
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".tmp"))
        .collect()
}

#[test]
fn an_undisturbed_paused_restore_succeeds() {
    let (s, main, sub) = pausing_setup();
    let outcome = paused_restore(&s, || {});
    assert!(outcome.contains("outcome: Ok("), "{outcome}");
    assert_eq!(fs::read(main).unwrap(), b"{\"main\":1}\n");
    assert_eq!(fs::read(sub).unwrap(), b"{\"sub\":1}\n");
    assert!(temps(&s.home).is_empty());
}

#[test]
fn a_file_appearing_before_publishing_is_never_replaced_and_nothing_partial_is_left() {
    // The main transcript is published first; the subagent's target then turns out to be taken.
    let (s, main, sub) = pausing_setup();
    let outcome = paused_restore(&s, || fs::write(&sub, b"theirs\n").unwrap());
    assert!(outcome.contains("Err(Restore(Exists("), "{outcome}");
    assert_eq!(
        fs::read(&sub).unwrap(),
        b"theirs\n",
        "the newcomer was replaced"
    );
    assert!(
        !main.exists(),
        "a partial restore was left at {}",
        main.display()
    );
    assert!(temps(&s.home).is_empty(), "{:?}", tree(&s.home));
    let files: Vec<_> = tree(&s.home)
        .into_iter()
        .filter(|p| s.home.join(p).is_file())
        .collect();
    assert_eq!(
        files,
        vec![sub.strip_prefix(&s.home).unwrap().to_path_buf()]
    );
}

#[test]
fn an_identical_file_appearing_before_publishing_is_fine() {
    let (s, main, sub) = pausing_setup();
    let outcome = paused_restore(&s, || fs::write(&sub, b"{\"sub\":1}\n").unwrap());
    assert!(outcome.contains("outcome: Ok("), "{outcome}");
    assert_eq!(fs::read(main).unwrap(), b"{\"main\":1}\n");
    assert!(temps(&s.home).is_empty());
}

#[test]
fn a_directory_swapped_for_a_symlink_before_publishing_is_refused() {
    let (s, main, _sub) = pausing_setup();
    let elsewhere = s.outside("moved-project");
    let outcome = paused_restore(&s, || {
        let dir = main.parent().unwrap();
        fs::rename(dir, &elsewhere).unwrap();
        symlink(&elsewhere, dir).unwrap();
    });
    assert!(outcome.contains("Err(Restore(Changed("), "{outcome}");
    assert!(
        !elsewhere.join(format!("{PAUSING}.jsonl")).exists(),
        "published through the symlink"
    );
}

#[test]
fn modes_are_exact_whatever_the_umask() {
    let s = Setup::claude();
    let out = child(&s).env("T41_UMASK", "1").output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("outcome: Ok("), "{out}");
    let project = s.home.join(NEW_PROJECT);
    assert_eq!(mode(&s.home.join("projects")), 0o700);
    assert_eq!(mode(&project), 0o700);
    assert_eq!(mode(&project.join(CLAUDE_SESSION).join("subagents")), 0o700);
    assert_eq!(
        mode(&project.join(format!("{CLAUDE_SESSION}.jsonl"))),
        0o600
    );
}
