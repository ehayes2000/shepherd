//! Capture (T40): the archive snapshots exactly the transcript files an agent wrote, from the path its hook payload
//! gave, and refuses anything else. On the provisional fixtures in `tests/fixtures/{claude,codex}` (see their README),
//! copied into tempdirs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use jiff::Timestamp;
use serde_json::Value;
use shepherd_archive::{
    AgentKind, AgentSessionId, Archive, ArchiveError, Capture, CaptureError, CaptureLimits,
    CommitHash, FileRole, MachineId, RepoId, Session, SessionId, SnapshotId, SnapshotReason,
};
use tempfile::TempDir;

const CLAUDE_SESSION: &str = "7d3c2a10-5b8e-4f61-9a2d-0c1e2f3a4b5c";
const CODEX_SESSION: &str = "019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b";
const CLAUDE_MAIN: &str = "projects/-work-app/7d3c2a10-5b8e-4f61-9a2d-0c1e2f3a4b5c.jsonl";
const CLAUDE_SUBAGENTS: &str = "projects/-work-app/7d3c2a10-5b8e-4f61-9a2d-0c1e2f3a4b5c/subagents";
const CODEX_ROLLOUT: &str =
    "sessions/2026/10/30/rollout-2026-10-30T10-00-00-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl";

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

/// The hook payload fixture `<agent>/hooks/<hook>.json`, as the agent would send it with its home at `home`.
fn payload(agent: &str, hook: &str, home: &Path) -> Value {
    let text = fs::read_to_string(
        fixtures()
            .join(agent)
            .join("hooks")
            .join(format!("{hook}.json")),
    )
    .unwrap();
    let text = text.replace("${AGENT_HOME}", home.to_str().unwrap());
    serde_json::from_str(&text).unwrap()
}

fn path_field(payload: &Value, field: &str) -> PathBuf {
    PathBuf::from(payload[field].as_str().expect("a path field"))
}

fn at(micros: i64) -> Timestamp {
    Timestamp::from_microsecond(micros).unwrap()
}

/// An archive with one session for `agent` (its own session id `agent_session`), and that agent's fixture home
/// installed in a tempdir.
struct Setup {
    tmp: TempDir,
    home: PathBuf,
    archive: Archive,
}

impl Setup {
    fn new(agent: AgentKind, agent_session: &str) -> Self {
        Self::from_fixture(agent, agent_session, agent.as_str())
    }

    /// Like [`Setup::new`], the agent home installed from fixture `fixture` (`tests/fixtures/<fixture>/home`).
    fn from_fixture(agent: AgentKind, agent_session: &str, fixture: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let home = tmp.path().join("agent-home");
        copy_tree(&fixtures().join(fixture).join("home"), &home);
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
        Self { tmp, home, archive }
    }

    fn claude() -> Self {
        Self::new(AgentKind::Claude, CLAUDE_SESSION)
    }

    fn codex() -> Self {
        Self::new(AgentKind::Codex, CODEX_SESSION)
    }

    /// The Codex fixture after Codex compressed its rollout in place (`codex-compressed`).
    fn codex_compressed() -> Self {
        Self::from_fixture(AgentKind::Codex, CODEX_SESSION, "codex-compressed")
    }

    /// A path in the tempdir, outside the agent home.
    fn outside(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn capture_with(&self, path: &Path, limits: CaptureLimits) -> Result<SnapshotId, ArchiveError> {
        self.archive.capture(&Capture {
            session: sid(),
            captured_at: at(2_000),
            head_commit: Some(hash('c')),
            reason: SnapshotReason::TurnEnd,
            commits: Vec::new(),
            agent_home: &self.home,
            transcript_path: path,
            limits,
        })
    }

    fn capture(&self, path: &Path) -> Result<SnapshotId, ArchiveError> {
        self.capture_with(path, CaptureLimits::default())
    }

    /// What capturing `path` failed with, after checking it archived nothing at all.
    fn refused_with(&self, path: &Path, limits: CaptureLimits) -> CaptureError {
        let err = match self.capture_with(path, limits) {
            Ok(id) => panic!("capturing {} succeeded as snapshot {id}", path.display()),
            Err(ArchiveError::Capture(e)) => e,
            Err(e) => panic!("expected a capture error, got {e:?}"),
        };
        assert!(
            self.archive.snapshots(&sid()).unwrap().is_empty(),
            "a snapshot was recorded"
        );
        assert_eq!(self.archive.blob_writes(), 0, "a blob was written");
        err
    }

    fn refused(&self, path: &Path) -> CaptureError {
        self.refused_with(path, CaptureLimits::default())
    }

    /// The files of snapshot `id`: role, path and content.
    fn files(&self, id: SnapshotId) -> Vec<(FileRole, PathBuf, Vec<u8>)> {
        self.archive
            .read(id)
            .unwrap()
            .into_iter()
            .map(|f| (f.file.role, f.file.original_path, f.content))
            .collect()
    }

    /// The canonical home joined with `rel`: where capture reads.
    fn real(&self, rel: &str) -> PathBuf {
        fs::canonicalize(&self.home).unwrap().join(rel)
    }
}

fn sid() -> SessionId {
    "s1".parse().unwrap()
}

fn hash(c: char) -> CommitHash {
    c.to_string().repeat(40).parse().unwrap()
}

fn fixture_bytes(agent: &str, rel: &str) -> Vec<u8> {
    fs::read(fixtures().join(agent).join("home").join(rel)).unwrap()
}

// ---------- the fixtures ----------

#[test]
fn every_hook_payload_fixture_names_the_sessions_transcript() {
    let home = Path::new("/home/me/.claude");
    for hook in ["SessionStart", "Stop", "SubagentStop"] {
        let p = payload("claude", hook, home);
        assert_eq!(p["session_id"], CLAUDE_SESSION, "{hook}");
        assert_eq!(
            path_field(&p, "transcript_path"),
            home.join(CLAUDE_MAIN),
            "{hook}"
        );
    }
    let sub = payload("claude", "SubagentStop", home);
    assert_eq!(
        path_field(&sub, "agent_transcript_path"),
        home.join(CLAUDE_SUBAGENTS).join("agent-a1b2c3d4.jsonl")
    );
    let home = Path::new("/home/me/.codex");
    for hook in ["SessionStart", "Stop"] {
        let p = payload("codex", hook, home);
        assert_eq!(p["session_id"], CODEX_SESSION, "{hook}");
        assert_eq!(
            path_field(&p, "transcript_path"),
            home.join(CODEX_ROLLOUT),
            "{hook}"
        );
    }
}

// ---------- Claude Code ----------

#[test]
fn claude_capture_archives_the_main_transcript_and_every_subagent_transcript() {
    let s = Setup::claude();
    let path = path_field(&payload("claude", "Stop", &s.home), "transcript_path");
    let id = s.capture(&path).unwrap();
    assert_eq!(
        s.files(id),
        vec![
            (
                FileRole::Main,
                s.real(CLAUDE_MAIN),
                fixture_bytes("claude", CLAUDE_MAIN)
            ),
            (
                FileRole::Subagent,
                s.real(&format!("{CLAUDE_SUBAGENTS}/agent-a1b2c3d4.jsonl")),
                fixture_bytes(
                    "claude",
                    &format!("{CLAUDE_SUBAGENTS}/agent-a1b2c3d4.jsonl")
                )
            ),
            (
                FileRole::Subagent,
                s.real(&format!("{CLAUDE_SUBAGENTS}/agent-e5f6a7b8.jsonl")),
                fixture_bytes(
                    "claude",
                    &format!("{CLAUDE_SUBAGENTS}/agent-e5f6a7b8.jsonl")
                )
            ),
        ]
    );
}

#[test]
fn claude_capture_records_the_snapshot_it_was_asked_for() {
    let s = Setup::claude();
    let path = path_field(
        &payload("claude", "SessionStart", &s.home),
        "transcript_path",
    );
    let id = s.capture(&path).unwrap();
    let snapshots = s.archive.snapshots(&sid()).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, id);
    assert_eq!(snapshots[0].reason, SnapshotReason::TurnEnd);
    assert_eq!(snapshots[0].captured_at, at(2_000));
    assert_eq!(snapshots[0].head_commit, Some(hash('c')));
}

#[test]
fn claude_capture_without_subagents_archives_only_the_main_transcript() {
    let s = Setup::claude();
    fs::remove_dir_all(s.home.join(CLAUDE_SUBAGENTS).parent().unwrap()).unwrap();
    let id = s.capture(&s.home.join(CLAUDE_MAIN)).unwrap();
    let roles: Vec<FileRole> = s.files(id).into_iter().map(|f| f.0).collect();
    assert_eq!(roles, vec![FileRole::Main]);
}

#[test]
fn claude_capture_with_a_session_dir_but_no_subagents_archives_only_the_main_transcript() {
    let s = Setup::claude();
    fs::remove_dir_all(s.home.join(CLAUDE_SUBAGENTS)).unwrap();
    let id = s.capture(&s.home.join(CLAUDE_MAIN)).unwrap();
    assert_eq!(s.files(id).len(), 1);
}

#[test]
fn claude_capture_skips_what_is_not_a_jsonl_file_in_the_subagents_dir() {
    let s = Setup::claude();
    let sub = s.home.join(CLAUDE_SUBAGENTS);
    fs::write(sub.join("agent-a1b2c3d4.meta.json"), b"{}").unwrap();
    fs::create_dir(sub.join("nested.jsonl")).unwrap();
    let id = s.capture(&s.home.join(CLAUDE_MAIN)).unwrap();
    assert_eq!(s.files(id).len(), 3);
}

#[test]
fn capture_reads_the_path_the_payload_gave() {
    // The same session id in another project dir: capture reads what the payload names, never a path it worked out.
    let s = Setup::claude();
    let other = s.home.join("projects/-work-other");
    fs::create_dir(&other).unwrap();
    let file = other.join(format!("{CLAUDE_SESSION}.jsonl"));
    fs::write(&file, b"{\"other\":true}\n").unwrap();
    let id = s.capture(&file).unwrap();
    assert_eq!(
        s.files(id),
        vec![(
            FileRole::Main,
            s.real(&format!("projects/-work-other/{CLAUDE_SESSION}.jsonl")),
            b"{\"other\":true}\n".to_vec()
        )]
    );
}

#[test]
fn a_home_reached_through_a_symlink_is_resolved_first() {
    // Only components below the home must not be symlinks (macOS's /var is one, above any home).
    let s = Setup::claude();
    let link = s.outside("home-link");
    symlink(&s.home, &link).unwrap();
    let id = s
        .archive
        .capture(&Capture {
            session: sid(),
            captured_at: at(2_000),
            head_commit: None,
            reason: SnapshotReason::Manual,
            commits: Vec::new(),
            agent_home: &link,
            transcript_path: &link.join(CLAUDE_MAIN),
            limits: CaptureLimits::default(),
        })
        .unwrap();
    assert_eq!(s.files(id)[0].1, s.real(CLAUDE_MAIN));
}

// ---------- Codex ----------

#[test]
fn codex_capture_archives_the_rollout_file() {
    let s = Setup::codex();
    let path = path_field(&payload("codex", "Stop", &s.home), "transcript_path");
    let id = s.capture(&path).unwrap();
    assert_eq!(
        s.files(id),
        vec![(
            FileRole::Rollout,
            s.real(CODEX_ROLLOUT),
            fixture_bytes("codex", CODEX_ROLLOUT)
        )]
    );
}

#[test]
fn codex_capture_accepts_a_reverted_threads_rollout_name() {
    // `rollout-<ts>-<thread>_<rollout>.jsonl` (codex-rs/rollout/src/rollout_file_name.rs).
    let s = Setup::codex();
    let name = format!(
        "rollout-2026-10-30T11-00-00-{CODEX_SESSION}_019a2b3c-9999-7000-8000-000000000002.jsonl"
    );
    let path = s.home.join("sessions/2026/10/30").join(name);
    fs::write(&path, b"{}\n").unwrap();
    let id = s.capture(&path).unwrap();
    assert_eq!(s.files(id)[0].2, b"{}\n");
}

// ---------- Codex rollouts compressed in place ----------

/// The fixture's rollout, compressed in place as Codex does it after 7 days.
const CODEX_ROLLOUT_ZST: &str = "sessions/2026/10/30/rollout-2026-10-30T10-00-00-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl.zst";

#[test]
fn the_compressed_fixture_is_the_plain_rollout_compressed() {
    let compressed = fs::read(
        fixtures()
            .join("codex-compressed/home")
            .join(CODEX_ROLLOUT_ZST),
    )
    .unwrap();
    assert_eq!(
        zstd::decode_all(compressed.as_slice()).unwrap(),
        fixture_bytes("codex", CODEX_ROLLOUT)
    );
}

#[test]
fn a_rollout_compressed_in_place_is_captured_as_the_rollout_it_holds() {
    // The hook named the plain rollout before Codex compressed it: the payload's path is gone, its `.zst` is there.
    let s = Setup::codex_compressed();
    let path = path_field(&payload("codex", "Stop", &s.home), "transcript_path");
    assert!(!path.exists());
    let id = s.capture(&path).unwrap();
    assert_eq!(
        s.files(id),
        vec![(
            FileRole::Rollout,
            s.real(CODEX_ROLLOUT),
            fixture_bytes("codex", CODEX_ROLLOUT)
        )]
    );
}

#[test]
fn a_compressed_rollouts_own_path_is_captured_too() {
    let s = Setup::codex_compressed();
    let id = s.capture(&s.home.join(CODEX_ROLLOUT_ZST)).unwrap();
    assert_eq!(
        s.files(id),
        vec![(
            FileRole::Rollout,
            s.real(CODEX_ROLLOUT),
            fixture_bytes("codex", CODEX_ROLLOUT)
        )]
    );
}

#[test]
fn a_plain_rollout_is_read_before_a_compressed_one_beside_it() {
    // Codex prefers the plain file (it decompresses a rollout back to plain to append to it).
    let s = Setup::codex();
    let stale = zstd::encode_all(&b"stale\n"[..], 3).unwrap();
    fs::write(s.home.join(CODEX_ROLLOUT_ZST), stale).unwrap();
    for path in [CODEX_ROLLOUT, CODEX_ROLLOUT_ZST] {
        let id = s.capture(&s.home.join(path)).unwrap();
        assert_eq!(
            s.files(id)[0].2,
            fixture_bytes("codex", CODEX_ROLLOUT),
            "{path}"
        );
    }
}

#[test]
fn a_rollout_compressed_since_it_was_captured_adds_no_blob() {
    let s = Setup::codex();
    let path = s.home.join(CODEX_ROLLOUT);
    s.capture(&path).unwrap();
    assert_eq!(s.archive.blob_writes(), 1);
    let compressed = zstd::encode_all(fs::read(&path).unwrap().as_slice(), 3).unwrap();
    fs::write(s.home.join(CODEX_ROLLOUT_ZST), compressed).unwrap();
    fs::remove_file(&path).unwrap();
    let id = s.capture(&path).unwrap();
    assert_eq!(s.archive.blob_writes(), 1);
    assert_eq!(s.files(id)[0].1, s.real(CODEX_ROLLOUT));
}

#[test]
fn a_compressed_rollout_that_inflates_past_the_limit_is_refused_without_inflating_it() {
    let s = Setup::codex_compressed();
    let bomb = zstd::encode_all(&vec![0u8; 8 << 20][..], 19).unwrap();
    assert!(bomb.len() < 4096, "{} bytes", bomb.len());
    fs::write(s.home.join(CODEX_ROLLOUT_ZST), &bomb).unwrap();
    let limits = CaptureLimits {
        max_bytes: 4096,
        ..CaptureLimits::default()
    };
    assert_eq!(
        s.refused_with(&s.home.join(CODEX_ROLLOUT), limits),
        CaptureError::TooLarge {
            path: s.real(CODEX_ROLLOUT_ZST),
            max: 4096
        }
    );
    assert!(
        s.archive.capture_bytes_read() <= 4097,
        "inflated {} bytes",
        s.archive.capture_bytes_read()
    );
}

#[test]
fn a_compressed_rollout_bigger_than_the_limit_is_refused_unread() {
    let s = Setup::codex_compressed();
    let zst = s.home.join(CODEX_ROLLOUT_ZST);
    let size = fs::metadata(&zst).unwrap().len();
    let limits = CaptureLimits {
        max_bytes: size - 1,
        ..CaptureLimits::default()
    };
    assert!(matches!(
        s.refused_with(&s.home.join(CODEX_ROLLOUT), limits),
        CaptureError::TooLarge { .. }
    ));
    assert_eq!(s.archive.capture_bytes_read(), 0);
}

#[test]
fn a_corrupt_compressed_rollout_is_refused() {
    let s = Setup::codex_compressed();
    let zst = s.home.join(CODEX_ROLLOUT_ZST);
    let mut truncated = fs::read(&zst).unwrap();
    truncated.truncate(truncated.len() / 2);
    for bytes in [b"not zstd at all\n".to_vec(), truncated] {
        fs::write(&zst, &bytes).unwrap();
        let err = s.refused(&s.home.join(CODEX_ROLLOUT));
        assert!(
            matches!(&err, CaptureError::Corrupt { path, .. } if *path == s.real(CODEX_ROLLOUT_ZST)),
            "{err:?}"
        );
    }
}

/// A frame that asks for a bigger window than Codex's level-3 frames ever use is refused before the decoder allocates
/// it, however little it holds.
#[test]
fn a_compressed_rollout_asking_for_a_huge_window_is_refused() {
    use std::io::Write as _;
    let s = Setup::codex_compressed();
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    encoder.window_log(27).unwrap();
    encoder.include_contentsize(false).unwrap();
    encoder.write_all(b"{}\n").unwrap();
    let frame = encoder.finish().unwrap();
    assert_eq!(
        zstd::decode_all(frame.as_slice()).unwrap(),
        b"{}\n",
        "a valid frame"
    );
    fs::write(s.home.join(CODEX_ROLLOUT_ZST), &frame).unwrap();
    let err = s.refused(&s.home.join(CODEX_ROLLOUT));
    assert!(
        matches!(&err, CaptureError::Corrupt { path, .. } if *path == s.real(CODEX_ROLLOUT_ZST)),
        "{err:?}"
    );
}

#[test]
fn a_symlinked_compressed_rollout_is_refused() {
    let s = Setup::codex_compressed();
    let zst = s.home.join(CODEX_ROLLOUT_ZST);
    let moved = s.outside("rollout.jsonl.zst");
    fs::rename(&zst, &moved).unwrap();
    symlink(&moved, &zst).unwrap();
    assert_eq!(
        s.refused(&s.home.join(CODEX_ROLLOUT)),
        CaptureError::Symlink(s.real(CODEX_ROLLOUT_ZST))
    );
}

#[test]
fn a_compressed_rollout_of_another_thread_is_refused() {
    let s = Setup::codex_compressed();
    let other = "sessions/2026/10/30/rollout-2026-10-30T10-00-00-019a2b3c-0000-7000-8000-00000000beef.jsonl";
    fs::copy(
        s.home.join(CODEX_ROLLOUT_ZST),
        s.home.join(format!("{other}.zst")),
    )
    .unwrap();
    for path in [other.to_owned(), format!("{other}.zst")] {
        assert!(
            matches!(
                s.refused(&s.home.join(&path)),
                CaptureError::NotTheSessionsTranscript { .. }
            ),
            "{path}"
        );
    }
}

#[test]
fn a_rollout_missing_in_both_forms_is_an_error_naming_the_plain_one() {
    let s = Setup::codex_compressed();
    fs::remove_file(s.home.join(CODEX_ROLLOUT_ZST)).unwrap();
    assert!(matches!(
        s.refused(&s.home.join(CODEX_ROLLOUT)),
        CaptureError::Io { path, .. } if path == s.real(CODEX_ROLLOUT)
    ));
}

// ---------- refusals ----------

#[test]
fn a_path_outside_the_agent_home_is_refused() {
    let s = Setup::claude();
    assert!(matches!(
        s.refused(Path::new("/etc/passwd")),
        CaptureError::Outside { .. }
    ));
}

#[test]
fn a_path_climbing_out_with_dot_dot_is_refused() {
    let s = Setup::claude();
    let path = s
        .home
        .join("projects/-work-app/../../../../../../etc/passwd");
    assert!(matches!(s.refused(&path), CaptureError::ParentDir(_)));
}

#[test]
fn a_dot_dot_that_would_land_back_on_the_transcript_is_refused_too() {
    let s = Setup::claude();
    let path = s
        .home
        .join(format!("projects/-work-app/x/../{CLAUDE_SESSION}.jsonl"));
    assert!(matches!(s.refused(&path), CaptureError::ParentDir(_)));
}

#[test]
fn a_relative_path_is_refused() {
    let s = Setup::claude();
    assert!(matches!(
        s.refused(Path::new(CLAUDE_MAIN)),
        CaptureError::NotAbsolute(_)
    ));
}

#[test]
fn the_agent_home_itself_is_refused() {
    let s = Setup::claude();
    let home = s.home.clone();
    assert!(matches!(s.refused(&home), CaptureError::Outside { .. }));
}

#[test]
fn another_sessions_transcript_in_the_same_project_is_refused() {
    let s = Setup::claude();
    let other = s
        .home
        .join("projects/-work-app/0000aaaa-0000-4000-8000-000000000000.jsonl");
    fs::write(&other, b"{}\n").unwrap();
    assert!(matches!(
        s.refused(&other),
        CaptureError::NotTheSessionsTranscript { .. }
    ));
}

#[test]
fn a_claude_file_anywhere_but_a_project_dir_is_refused() {
    let s = Setup::claude();
    let name = format!("{CLAUDE_SESSION}.jsonl");
    for rel in [
        name.clone(),
        format!("projects/{name}"),
        format!("history/-work-app/{name}"),
        format!("projects/-work-app/{CLAUDE_SESSION}/subagents/{name}"),
    ] {
        let path = s.home.join(&rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{}\n").unwrap();
        assert!(
            matches!(
                s.refused(&path),
                CaptureError::NotTheSessionsTranscript { .. }
            ),
            "{rel}"
        );
    }
}

#[test]
fn a_symlinked_transcript_pointing_out_is_refused() {
    let s = Setup::claude();
    let secret = s.outside("secret.jsonl");
    fs::write(&secret, b"secret\n").unwrap();
    let main = s.home.join(CLAUDE_MAIN);
    fs::remove_file(&main).unwrap();
    symlink(&secret, &main).unwrap();
    assert!(matches!(s.refused(&main), CaptureError::Symlink(_)));
}

#[test]
fn a_symlink_in_any_directory_below_the_home_is_refused() {
    // `projects`, then the project dir: each replaced by a symlink to a real copy elsewhere.
    for rel in ["projects", "projects/-work-app"] {
        let s = Setup::claude();
        let dir = s.home.join(rel);
        let moved = s.outside("moved");
        fs::rename(&dir, &moved).unwrap();
        symlink(&moved, &dir).unwrap();
        let err = s.refused(&s.home.join(CLAUDE_MAIN));
        assert_eq!(err, CaptureError::Symlink(s.real(rel)), "{rel}");
    }
}

#[test]
fn a_symlinked_session_dir_is_refused() {
    let s = Setup::claude();
    let dir = s
        .home
        .join(CLAUDE_SUBAGENTS)
        .parent()
        .unwrap()
        .to_path_buf();
    let moved = s.outside("session-dir");
    fs::rename(&dir, &moved).unwrap();
    symlink(&moved, &dir).unwrap();
    assert!(matches!(
        s.refused(&s.home.join(CLAUDE_MAIN)),
        CaptureError::Symlink(_)
    ));
}

#[test]
fn a_symlinked_subagents_dir_is_refused() {
    let s = Setup::claude();
    let dir = s.home.join(CLAUDE_SUBAGENTS);
    let moved = s.outside("subagents");
    fs::rename(&dir, &moved).unwrap();
    symlink(&moved, &dir).unwrap();
    assert_eq!(
        s.refused(&s.home.join(CLAUDE_MAIN)),
        CaptureError::Symlink(s.real(CLAUDE_SUBAGENTS))
    );
}

#[test]
fn a_symlinked_subagent_transcript_is_refused() {
    let s = Setup::claude();
    let secret = s.outside("secret.jsonl");
    fs::write(&secret, b"secret\n").unwrap();
    let link = s.home.join(CLAUDE_SUBAGENTS).join("agent-evil.jsonl");
    symlink(&secret, &link).unwrap();
    assert!(matches!(
        s.refused(&s.home.join(CLAUDE_MAIN)),
        CaptureError::Symlink(_)
    ));
}

/// A FIFO at `path`, with a thread waiting to open it for writing. Opening a FIFO for reading blocks until a writer
/// opens it, so if capture ever opens one, this writer unblocks it (and closes at once, so the read sees EOF) and the
/// test fails on its assertion instead of hanging. [`release_fifo`] lets the writer finish when nothing opened it.
fn fifo_with_writer(path: &Path) -> std::thread::JoinHandle<()> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c_path` is a valid NUL-terminated path for the duration of the call.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = fs::OpenOptions::new().write(true).open(&path);
    })
}

/// Opens the FIFO for reading without blocking and holds it while the writer's open completes, so the writer thread
/// always finishes, whether or not it had started opening yet.
fn release_fifo(path: &Path, writer: std::thread::JoinHandle<()>) {
    use std::os::unix::fs::OpenOptionsExt;
    let reader = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .unwrap();
    writer.join().unwrap();
    drop(reader);
}

#[test]
fn a_transcript_that_is_not_a_regular_file_is_refused_without_opening_it() {
    // A FIFO would block a read forever; it is refused from its lstat.
    let s = Setup::claude();
    let main = s.home.join(CLAUDE_MAIN);
    fs::remove_file(&main).unwrap();
    let writer = fifo_with_writer(&main);
    let err = s.refused(&main);
    release_fifo(&main, writer);
    assert!(matches!(err, CaptureError::WrongType(_)), "{err:?}");
}

#[test]
fn a_subagent_transcript_that_is_not_a_regular_file_is_refused_without_opening_it() {
    let s = Setup::claude();
    let fifo = s.home.join(CLAUDE_SUBAGENTS).join("agent-x.jsonl");
    let writer = fifo_with_writer(&fifo);
    let err = s.refused(&s.home.join(CLAUDE_MAIN));
    release_fifo(&fifo, writer);
    assert_eq!(
        err,
        CaptureError::WrongType(s.real(&format!("{CLAUDE_SUBAGENTS}/agent-x.jsonl")))
    );
}

#[test]
fn a_missing_transcript_is_an_error() {
    let s = Setup::claude();
    let main = s.home.join(CLAUDE_MAIN);
    fs::remove_file(&main).unwrap();
    assert!(matches!(s.refused(&main), CaptureError::Io { .. }));
}

#[test]
fn a_codex_rollout_outside_the_sessions_tree_is_refused() {
    let s = Setup::codex();
    let archived = s.home.join("archived_sessions");
    fs::create_dir(&archived).unwrap();
    let name = Path::new(CODEX_ROLLOUT).file_name().unwrap();
    fs::copy(s.home.join(CODEX_ROLLOUT), archived.join(name)).unwrap();
    assert!(matches!(
        s.refused(&archived.join(name)),
        CaptureError::NotTheSessionsTranscript { .. }
    ));
}

#[test]
fn another_threads_rollout_is_refused() {
    let s = Setup::codex();
    let path = s
        .home
        .join("sessions/2026/10/30/rollout-2026-10-30T10-00-00-019a2b3c-0000-7000-8000-00000000beef.jsonl");
    fs::write(&path, b"{}\n").unwrap();
    assert!(matches!(
        s.refused(&path),
        CaptureError::NotTheSessionsTranscript { .. }
    ));
}

#[test]
fn a_file_that_is_not_a_rollout_is_refused() {
    let s = Setup::codex();
    for name in [
        format!("{CODEX_SESSION}.jsonl"),
        format!("rollout-{CODEX_SESSION}.jsonl"),
        format!("rollout-2026-13-30T10-00-00-{CODEX_SESSION}.jsonl"),
        format!("rollout-2026-13-30T10-00-00-{CODEX_SESSION}.jsonl.zst"),
        format!("rollout-2026-10-30T10-00-00-{CODEX_SESSION}.zst"),
        format!("rollout-2026-10-30T10-00-00-{CODEX_SESSION}.jsonl.gz"),
    ] {
        let path = s.home.join("sessions/2026/10/30").join(&name);
        fs::write(&path, b"{}\n").unwrap();
        assert!(
            matches!(
                s.refused(&path),
                CaptureError::NotTheSessionsTranscript { .. }
            ),
            "{name}"
        );
    }
}

#[test]
fn a_symlinked_codex_date_dir_is_refused() {
    let s = Setup::codex();
    let day = s.home.join("sessions/2026/10/30");
    let moved = s.outside("day");
    fs::rename(&day, &moved).unwrap();
    symlink(&moved, &day).unwrap();
    assert_eq!(
        s.refused(&s.home.join(CODEX_ROLLOUT)),
        CaptureError::Symlink(s.real("sessions/2026/10/30"))
    );
}

#[test]
fn a_claude_session_id_that_is_not_one_path_component_is_refused() {
    // Its subagents' directory is named by it: `..` would point that above the project dir.
    for id in ["..", "a/b"] {
        let s = Setup::new(AgentKind::Claude, id);
        let path = s.home.join(format!("projects/-work-app/{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{}\n").unwrap();
        let err = s.refused(&path);
        assert!(
            matches!(
                err,
                CaptureError::NotTheSessionsTranscript { .. } | CaptureError::ParentDir(_)
            ),
            "{id}: {err:?}"
        );
    }
}

#[test]
fn a_claude_transcript_is_refused_for_a_codex_session() {
    let s = Setup::codex();
    let path = s.home.join(CLAUDE_MAIN);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{}\n").unwrap();
    assert!(matches!(
        s.refused(&path),
        CaptureError::NotTheSessionsTranscript { .. }
    ));
}

#[test]
fn capture_for_an_unrecorded_session_stores_nothing() {
    let s = Setup::claude();
    let err = s
        .archive
        .capture(&Capture {
            session: "nobody".parse().unwrap(),
            captured_at: at(2_000),
            head_commit: None,
            reason: SnapshotReason::Manual,
            commits: Vec::new(),
            agent_home: &s.home,
            transcript_path: &s.home.join(CLAUDE_MAIN),
            limits: CaptureLimits::default(),
        })
        .unwrap_err();
    assert!(matches!(err, ArchiveError::UnknownSession(_)), "{err:?}");
    assert_eq!(s.archive.blob_writes(), 0);
}

// ---------- bounds ----------

/// The bytes of every file the Claude fixture captures.
fn claude_total() -> u64 {
    [
        CLAUDE_MAIN.to_owned(),
        format!("{CLAUDE_SUBAGENTS}/agent-a1b2c3d4.jsonl"),
        format!("{CLAUDE_SUBAGENTS}/agent-e5f6a7b8.jsonl"),
    ]
    .iter()
    .map(|rel| fixture_bytes("claude", rel).len() as u64)
    .sum()
}

#[test]
fn a_capture_of_exactly_the_byte_limit_succeeds() {
    let s = Setup::claude();
    let limits = CaptureLimits {
        max_bytes: claude_total(),
        ..CaptureLimits::default()
    };
    assert!(s.capture_with(&s.home.join(CLAUDE_MAIN), limits).is_ok());
}

#[test]
fn a_capture_one_byte_over_the_limit_in_all_is_refused() {
    // The limit is over all the files: here the last subagent transcript tips it over.
    let s = Setup::claude();
    let limits = CaptureLimits {
        max_bytes: claude_total() - 1,
        ..CaptureLimits::default()
    };
    let err = s.refused_with(&s.home.join(CLAUDE_MAIN), limits);
    assert_eq!(
        err,
        CaptureError::TooLarge {
            path: s.real(&format!("{CLAUDE_SUBAGENTS}/agent-e5f6a7b8.jsonl")),
            max: claude_total() - 1,
        }
    );
}

#[test]
fn an_oversized_transcript_is_never_read_whole() {
    let s = Setup::codex();
    let rollout = s.home.join(CODEX_ROLLOUT);
    fs::write(&rollout, vec![b'x'; 1 << 20]).unwrap();
    let limits = CaptureLimits {
        max_bytes: 1024,
        ..CaptureLimits::default()
    };
    assert!(matches!(
        s.refused_with(&rollout, limits),
        CaptureError::TooLarge { max: 1024, .. }
    ));
    assert!(
        s.archive.capture_bytes_read() <= 1025,
        "read {} bytes",
        s.archive.capture_bytes_read()
    );
}

#[test]
fn more_files_than_the_limit_is_refused() {
    let s = Setup::claude();
    let limits = CaptureLimits {
        max_files: 2,
        ..CaptureLimits::default()
    };
    assert!(matches!(
        s.refused_with(&s.home.join(CLAUDE_MAIN), limits),
        CaptureError::TooManyFiles { max: 2, .. }
    ));
    let limits = CaptureLimits {
        max_files: 3,
        ..CaptureLimits::default()
    };
    assert!(s.capture_with(&s.home.join(CLAUDE_MAIN), limits).is_ok());
}

// ---------- dedupe ----------

#[test]
fn capturing_unchanged_files_adds_a_snapshot_but_no_blobs() {
    let s = Setup::claude();
    let path = s.home.join(CLAUDE_MAIN);
    s.capture(&path).unwrap();
    assert_eq!(s.archive.blob_writes(), 3);
    let second = s.capture(&path).unwrap();
    assert_eq!(s.archive.blob_writes(), 3);
    assert_eq!(s.archive.snapshots(&sid()).unwrap().len(), 2);
    assert_eq!(s.files(second).len(), 3);
}

#[test]
fn capturing_after_the_transcript_grew_stores_only_the_changed_file() {
    let s = Setup::claude();
    let path = s.home.join(CLAUDE_MAIN);
    s.capture(&path).unwrap();
    let mut grown = fs::read(&path).unwrap();
    grown.extend_from_slice(
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"more\"}}\n",
    );
    fs::write(&path, &grown).unwrap();
    let id = s.capture(&path).unwrap();
    assert_eq!(s.archive.blob_writes(), 4);
    assert_eq!(s.files(id)[0].2, grown);
}

// ---------- swaps between the check and the open ----------

/// The parent passes the child the tempdir holding the archive and the agent home, through this variable.
const CHILD_DIR: &str = "T40_CHILD_DIR";
const PAUSED: &str = shepherd_archive::PAUSE_BEFORE_OPEN_MARKER;
const PAUSING: &str = shepherd_archive::PAUSE_BEFORE_OPEN_SESSION;

/// Runs only as a child process started by `paused_capture`; does nothing in a normal test run. Captures the pausing
/// session's transcript and prints the outcome.
#[test]
fn child_process() {
    #[allow(
        clippy::disallowed_methods,
        reason = "test plumbing: the parent passes its tempdir to its child"
    )]
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    let dir = Path::new(&dir);
    let home = dir.join("agent-home");
    let archive = Archive::open(&dir.join("archive")).unwrap();
    let result = archive.capture(&Capture {
        session: sid(),
        captured_at: at(3_000),
        head_commit: None,
        reason: SnapshotReason::Manual,
        commits: Vec::new(),
        agent_home: &home,
        transcript_path: &home.join(format!("projects/-work-app/{PAUSING}.jsonl")),
        limits: CaptureLimits::default(),
    });
    println!("outcome: {result:?}");
}

/// The Codex variant of [`child_process`]: captures the pausing thread's rollout, by the plain path its hook named.
#[test]
fn codex_child_process() {
    #[allow(
        clippy::disallowed_methods,
        reason = "test plumbing: the parent passes its tempdir to its child"
    )]
    let Some(dir) = std::env::var_os(CODEX_CHILD_DIR) else {
        return;
    };
    let dir = Path::new(&dir);
    let home = dir.join("agent-home");
    let archive = Archive::open(&dir.join("archive")).unwrap();
    let result = archive.capture(&Capture {
        session: sid(),
        captured_at: at(3_000),
        head_commit: None,
        reason: SnapshotReason::Manual,
        commits: Vec::new(),
        agent_home: &home,
        transcript_path: &home.join(pausing_rollout()),
        limits: CaptureLimits::default(),
    });
    println!("outcome: {result:?}");
}

/// The parent passes [`codex_child_process`] its tempdir through this variable.
const CODEX_CHILD_DIR: &str = "T42B_CODEX_CHILD_DIR";

/// The pausing thread's plain rollout, below the agent home.
fn pausing_rollout() -> String {
    format!("sessions/2026/10/30/rollout-2026-10-30T10-00-00-{PAUSING}.jsonl")
}

/// A Claude session whose agent session id makes capture pause before opening its transcript, with that transcript.
fn pausing_setup() -> (Setup, PathBuf) {
    let s = Setup::new(AgentKind::Claude, PAUSING);
    let main = s.home.join(format!("projects/-work-app/{PAUSING}.jsonl"));
    fs::write(&main, b"{\"mine\":true}\n").unwrap();
    (s, main)
}

/// Captures the pausing session in a child process, runs `swap` while it is paused between checking the transcript's
/// path and opening it, and returns what the child printed after that.
fn paused_capture(s: &Setup, swap: impl FnOnce()) -> String {
    paused_capture_in("child_process", CHILD_DIR, s, swap)
}

/// [`paused_capture`] in the child test `child`, given the tempdir through `var`.
fn paused_capture_in(child: &str, var: &str, s: &Setup, swap: impl FnOnce()) -> String {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", child, "--test-threads=1", "--nocapture"])
        .env(var, s.tmp.path())
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
    swap();
    child.stdin.take().unwrap().write_all(b"go").unwrap();
    let mut rest = String::new();
    out.read_to_string(&mut rest).unwrap();
    assert!(child.wait().unwrap().success());
    rest
}

#[test]
fn a_transcript_replaced_by_another_file_after_its_check_is_refused() {
    // A hard link to a file outside the home, renamed over the transcript: no symlink anywhere, but not the file that
    // was checked.
    let (s, main) = pausing_setup();
    let secret = s.outside("secret.jsonl");
    fs::write(&secret, b"secret\n").unwrap();
    let outcome = paused_capture(&s, || {
        let staged = main.with_extension("staged");
        fs::hard_link(&secret, &staged).unwrap();
        fs::rename(&staged, &main).unwrap();
    });
    assert!(outcome.contains("Err(Capture(Changed("), "{outcome}");
    assert!(s.archive.snapshots(&sid()).unwrap().is_empty());
}

#[test]
fn a_directory_swapped_for_a_symlink_after_its_check_is_refused() {
    // The project dir moved aside and a symlink to it put in its place: the same file, reached through a symlink.
    let (s, main) = pausing_setup();
    let outcome = paused_capture(&s, || {
        let dir = main.parent().unwrap();
        let moved = s.outside("moved-project");
        fs::rename(dir, &moved).unwrap();
        symlink(&moved, dir).unwrap();
    });
    assert!(outcome.contains("Err(Capture(Changed("), "{outcome}");
    assert!(s.archive.snapshots(&sid()).unwrap().is_empty());
}

/// Codex compresses the rollout (writes its `.zst`, removes the plain file) after capture chose the plain file and
/// before it opened it: capture looks again and reads the `.zst`, never reporting the conversation missing.
#[test]
fn a_rollout_compressed_between_its_check_and_its_open_is_read_compressed() {
    let s = Setup::new(AgentKind::Codex, PAUSING);
    let plain = s.home.join(pausing_rollout());
    let content = b"{\"type\":\"session_meta\"}\n".to_vec();
    fs::write(&plain, &content).unwrap();
    let outcome = paused_capture_in("codex_child_process", CODEX_CHILD_DIR, &s, || {
        let compressed = zstd::encode_all(content.as_slice(), 3).unwrap();
        fs::write(format!("{}.zst", plain.display()), compressed).unwrap();
        fs::remove_file(&plain).unwrap();
    });
    assert!(outcome.contains("outcome: Ok("), "{outcome}");
    let snapshots = s.archive.snapshots(&sid()).unwrap();
    assert_eq!(snapshots.len(), 1);
    let files = s.files(snapshots[0].id);
    assert_eq!(files[0].1, s.real(&pausing_rollout()));
    assert_eq!(files[0].2, content);
}

#[test]
fn an_undisturbed_paused_capture_succeeds() {
    let (s, _main) = pausing_setup();
    let outcome = paused_capture(&s, || {});
    assert!(outcome.contains("outcome: Ok("), "{outcome}");
}
