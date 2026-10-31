//! Reading an agent's last reply from its transcript (T47): through the same path checks as a capture, parsed per agent
//! (Claude Code JSONL, Codex rollout). On the provisional fixtures in `tests/fixtures/{reply,claude,codex,
//! codex-compressed}` (see their README), copied into tempdirs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use shepherd_archive::{AgentKind, CaptureError, CaptureLimits, Reply, ReplyQuery, last_reply};
use tempfile::TempDir;

const CLAUDE_TWO_TURNS: &str = "projects/-work-app/2b4d6f80-1a3c-4e5f-8a7b-9c0d1e2f3a4b.jsonl";
const CODEX_TWO_TURNS: &str =
    "sessions/2026/10/31/rollout-2026-10-31T10-00-00-019b3c4d-5e6f-7a80-9b0c-1d2e3f4a5b6c.jsonl";
const CLAUDE_ONE_TURN: &str = "projects/-work-app/7d3c2a10-5b8e-4f61-9a2d-0c1e2f3a4b5c.jsonl";
const CODEX_ONE_TURN: &str =
    "sessions/2026/10/30/rollout-2026-10-30T10-00-00-019a2b3c-4d5e-7f60-8a9b-0c1d2e3f4a5b.jsonl";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

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

/// The fixture home `tests/fixtures/<fixture>/home`, installed in a tempdir.
struct Home {
    _tmp: TempDir,
    home: PathBuf,
}

impl Home {
    fn of(fixture: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(tmp.path()).unwrap().join("home");
        copy_tree(&fixtures().join(fixture).join("home"), &home);
        Self { _tmp: tmp, home }
    }

    fn read(
        &self,
        agent: AgentKind,
        rel: &str,
        since: Option<&str>,
    ) -> Result<Option<Reply>, CaptureError> {
        self.read_at(agent, &self.home.join(rel), since, CaptureLimits::default())
    }

    fn read_at(
        &self,
        agent: AgentKind,
        path: &Path,
        since: Option<&str>,
        limits: CaptureLimits,
    ) -> Result<Option<Reply>, CaptureError> {
        last_reply(&ReplyQuery {
            agent,
            agent_home: &self.home,
            transcript_path: path,
            since: since.map(at),
            limits,
        })
    }
}

fn at(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn reply(text: &str, when: &str) -> Option<Reply> {
    Some(Reply {
        text: text.to_owned(),
        at: Some(at(when)),
    })
}

#[test]
fn claude_codes_reply_is_its_last_assistant_message_with_text_whole() {
    let home = Home::of("reply/claude");
    // Split over three lines of one message (thinking, then two texts): the texts, joined. Not the subagent's line,
    // the summary, or a line still being written.
    let expected = reply(
        "All 12 tests pass.\nNothing needed fixing.",
        "2026-10-31T10:05:33Z",
    );
    // Without a turn to start from, the last message counts though it says no time.
    assert_eq!(
        home.read(AgentKind::Claude, CLAUDE_TWO_TURNS, None)
            .unwrap(),
        Some(Reply {
            text: "Written with no time.".to_owned(),
            at: None,
        })
    );
    // Since a turn began, a message that says no time can't be shown to be in it: skipped.
    assert_eq!(
        home.read(
            AgentKind::Claude,
            CLAUDE_TWO_TURNS,
            Some("2026-10-31T10:05:00Z")
        )
        .unwrap(),
        expected
    );
}

#[test]
fn a_reply_from_before_the_turn_does_not_count() {
    let home = Home::of("reply/claude");
    assert_eq!(
        home.read(
            AgentKind::Claude,
            CLAUDE_TWO_TURNS,
            Some("2026-10-31T10:06:00Z")
        )
        .unwrap(),
        None
    );
    let home = Home::of("reply/codex");
    assert_eq!(
        home.read(
            AgentKind::Codex,
            CODEX_TWO_TURNS,
            Some("2026-10-31T10:06:00Z")
        )
        .unwrap(),
        None
    );
}

#[test]
fn codexs_reply_is_its_last_assistant_message_in_the_rollout() {
    let home = Home::of("reply/codex");
    assert_eq!(
        home.read(
            AgentKind::Codex,
            CODEX_TWO_TURNS,
            Some("2026-10-31T10:05:00Z")
        )
        .unwrap(),
        reply(
            "All 12 tests pass.\nNothing needed fixing.",
            "2026-10-31T10:05:33Z"
        )
    );
}

#[test]
fn the_capture_fixtures_replies_are_read_too() {
    let claude = Home::of("claude");
    assert_eq!(
        claude
            .read(AgentKind::Claude, CLAUDE_ONE_TURN, None)
            .unwrap(),
        reply("Created NOTES.md and committed it.", "2026-10-30T10:00:15Z")
    );
    let codex = Home::of("codex");
    let expected = reply("Created NOTES.md and committed it.", "2026-10-30T10:00:09Z");
    assert_eq!(
        codex.read(AgentKind::Codex, CODEX_ONE_TURN, None).unwrap(),
        expected
    );
    // A rollout Codex compressed in place is read the same way, named either way.
    let compressed = Home::of("codex-compressed");
    assert_eq!(
        compressed
            .read(AgentKind::Codex, CODEX_ONE_TURN, None)
            .unwrap(),
        expected
    );
}

#[test]
fn a_path_outside_the_home_or_not_a_transcript_is_refused_like_a_capture() {
    let home = Home::of("reply/claude");
    let outside = home._tmp.path().join("elsewhere.jsonl");
    fs::write(&outside, b"{}\n").unwrap();
    assert!(matches!(
        home.read_at(AgentKind::Claude, &outside, None, CaptureLimits::default()),
        Err(CaptureError::Outside { .. })
    ));
    let dotdot = home.home.join("projects/-work-app/../-work-app/x.jsonl");
    assert!(matches!(
        home.read_at(AgentKind::Claude, &dotdot, None, CaptureLimits::default()),
        Err(CaptureError::ParentDir(_))
    ));
    // Under the home, but not where Claude Code keeps a transcript.
    fs::write(home.home.join("notes.jsonl"), b"{}\n").unwrap();
    assert!(matches!(
        home.read(AgentKind::Claude, "notes.jsonl", None),
        Err(CaptureError::NotATranscript { .. })
    ));
    // Claude Code's layout isn't Codex's.
    assert!(matches!(
        home.read(AgentKind::Codex, CLAUDE_TWO_TURNS, None),
        Err(CaptureError::NotATranscript { .. })
    ));
}

#[test]
fn a_symlinked_transcript_is_refused() {
    let home = Home::of("reply/claude");
    let real = home.home.join(CLAUDE_TWO_TURNS);
    let link = home
        .home
        .join("projects/-work-app/0f0f0f0f-0000-4000-8000-000000000000.jsonl");
    symlink(&real, &link).unwrap();
    assert!(matches!(
        home.read_at(AgentKind::Claude, &link, None, CaptureLimits::default()),
        Err(CaptureError::Symlink(_))
    ));
}

#[test]
fn a_transcript_that_is_not_there_is_an_io_error_and_one_past_the_limit_is_refused() {
    let home = Home::of("reply/claude");
    let gone = "projects/-work-app/0f0f0f0f-0000-4000-8000-000000000000.jsonl";
    assert!(matches!(
        home.read(AgentKind::Claude, gone, None),
        Err(CaptureError::Io { .. })
    ));
    let small = CaptureLimits {
        max_bytes: 100,
        max_files: 1,
    };
    assert!(matches!(
        home.read_at(
            AgentKind::Claude,
            &home.home.join(CLAUDE_TWO_TURNS),
            None,
            small
        ),
        Err(CaptureError::TooLarge { .. })
    ));
}
