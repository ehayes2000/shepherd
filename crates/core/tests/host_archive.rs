//! The host archives agent conversations (T42): on turn end, on close, and before any worktree removal, which waits
//! for that capture and keeps the worktree if it fails. Through the public `shepherd_core::host` API.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, Source, SourceKind};
use shepherd_core::config::{AgentKind, WorkspaceHooks};
use shepherd_core::host::capture::{AfterCapture, AgentHomes, CaptureJob, Found};
use shepherd_core::host::{
    HostCommand, HostEvent, Inspect, Inspection, PreRemove, Remover, ReplyTo,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{
    ArchiveError, ArchivedSession, BranchName, CaptureReason, CaptureRequest, CommandExit,
    CommandOutput, CommandSpec, CommitHash, PatchId, RepoRoot, WorktreePath, WorktreeStatus,
};
use shepherd_core::wire::{CloseWorktree, HostResult, WorktreeRemoveOk};
use shepherd_testkit::builders::{HostDriver, claude_session_spec, ts};

const REPO: &str = "/src/app";
const WORKTREE: &str = "/wt/s1";
const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TRANSCRIPT: &str = "/home/test/.claude/projects/-wt-s1/abc.jsonl";
const TIMEOUT: Duration = Duration::from_secs(30);

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn hash(s: &str) -> CommitHash {
    CommitHash::parse(s).unwrap_or_else(|e| panic!("{e}"))
}

fn path() -> WorktreePath {
    WorktreePath::new(WORKTREE)
}

fn hooks(pre_remove: &[&str]) -> WorkspaceHooks {
    WorkspaceHooks {
        post_create: Vec::new(),
        pre_remove: pre_remove.iter().map(|h| (*h).to_owned()).collect(),
        timeout: TIMEOUT,
    }
}

fn clean() -> WorktreeStatus {
    WorktreeStatus {
        dirty: false,
        ahead: 0,
        pushed: false,
        merged_into_default: true,
    }
}

fn listed() -> Result<Inspection, shepherd_core::ports::WorktreeError> {
    Ok(Inspection::Listed {
        status: Ok(clean()),
        exists: true,
        branch: Some("s1".to_owned()),
    })
}

fn closed(removed: bool, kept: Option<&str>) -> HostCommand {
    closed_with(json!({ "worktree_removed": removed }), kept)
}

/// A `session/closed` noting `note` about how closing went.
fn closed_noting(removed: bool, kept: Option<&str>, note: &str) -> HostCommand {
    closed_with(json!({ "worktree_removed": removed, "note": note }), kept)
}

fn closed_with(mut params: Value, kept: Option<&str>) -> HostCommand {
    if let Some(kept) = kept {
        params["worktree_kept"] = json!(kept);
    }
    let frame = Frame::from_value(json!({
        "jsonrpc": "2.0", "method": "_shepherd/session/closed", "params": params,
    }))
    .unwrap_or_else(|e| panic!("bad frame: {e}"));
    HostCommand::Append(Box::new(FrameDraft::new(
        frame,
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        Some(sid("s1")),
    )))
}

/// A host with [`REPO`] selected and Claude Code session `s1` running in its worktree at [`WORKTREE`], started at
/// [`BASE`], whose hooks named agent session `abc` and its transcript.
fn running() -> HostDriver {
    let mut host = HostDriver::new();
    host.select_workspace(REPO);
    host.new_session_with("s1", claude_session_spec());
    host.worktree_ready_at("s1", BASE);
    host.agent_started_with_transcript("s1", "claude-code", "abc", TRANSCRIPT);
    host
}

/// What capturing [`running`]'s `s1` for `reason` asks for.
fn job(host: &HostDriver, reason: CaptureReason) -> CaptureJob {
    // Its first entry is its `session/created`.
    let created = host
        .log()
        .iter()
        .find(|e| e.session() == Some(&sid("s1")))
        .map(|e| e.ts().clone())
        .unwrap_or_else(|| panic!("s1 was never created"));
    CaptureJob {
        session: sid("s1"),
        reason,
        agent: AgentKind::ClaudeCode,
        agent_session_id: "abc".to_owned(),
        transcript_path: PathBuf::from(TRANSCRIPT),
        name: "claude-1".to_owned(),
        machine: MachineId::new("m_test"),
        created,
        cwd: path(),
        branch: Some(BranchName::new("s1")),
        base: Some(hash(BASE)),
        profile: Some("claude".to_owned()),
    }
}

fn captures(commands: &[HostCommand]) -> Vec<(&[CaptureJob], &AfterCapture)> {
    commands
        .iter()
        .filter_map(|c| match c {
            HostCommand::Capture { captures, then } => Some((captures.as_slice(), then)),
            _ => None,
        })
        .collect()
}

fn removes(commands: &[HostCommand]) -> bool {
    commands
        .iter()
        .any(|c| matches!(c, HostCommand::RemoveWorktree { .. }))
}

fn hook(host: &mut HostDriver, agent: AgentKind, payload: Value) -> Vec<HostCommand> {
    let reply = host.reply_to();
    host.handle(HostEvent::Hook {
        reply,
        session: sid("s1"),
        agent,
        payload,
    })
}

fn turn_end_of(commands: &[HostCommand]) -> Option<bool> {
    commands.iter().find_map(|c| match c {
        HostCommand::TranslateHook { turn_end, .. } => Some(*turn_end),
        _ => None,
    })
}

/// The removal a closing `s1`'s capture before removal carries.
fn remove_s1() -> AfterCapture {
    AfterCapture::Remove {
        remover: Remover::Session(sid("s1")),
        path: path(),
        force: false,
    }
}

/// [`running`], closed (removing its worktree if safe), its worktree found clean. Returns what the inspection gave.
fn closed_and_inspected(pre_remove: &[&str]) -> (HostDriver, Vec<HostCommand>) {
    let mut host = running();
    host.close_with("s1", CloseWorktree::IfSafe, hooks(pre_remove));
    let commands = host.handle(HostEvent::WorktreeInspected {
        remover: Remover::Session(sid("s1")),
        path: path(),
        purpose: Inspect::Close {
            confirmed: None,
            hooks: PreRemove {
                hooks: pre_remove.iter().map(|h| (*h).to_owned()).collect(),
                timeout: TIMEOUT,
            },
        },
        result: listed(),
    });
    (host, commands)
}

// ---------- turn end ----------

#[test]
fn a_stop_hook_from_either_agent_is_a_turn_end_and_other_hooks_are_not() {
    let mut host = running();
    for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
        let stop = hook(&mut host, agent, json!({ "hook_event_name": "Stop" }));
        assert_eq!(turn_end_of(&stop), Some(true), "{agent:?}");
        let prompt = json!({ "hook_event_name": "UserPromptSubmit", "prompt": "hi" });
        let other = hook(&mut host, agent, prompt);
        assert_eq!(turn_end_of(&other), Some(false), "{agent:?}");
    }
}

#[test]
fn a_turn_end_captures_the_agent_sessions_transcript() {
    let mut host = running();
    let commands = host.turn_ended("s1");
    let expected = job(&host, CaptureReason::TurnEnd);
    assert_eq!(
        captures(&commands),
        [(&[expected][..], &AfterCapture::Nothing)]
    );
}

#[test]
fn a_hook_that_did_not_end_a_turn_captures_nothing() {
    let mut host = running();
    assert_eq!(captures(&host.hook_translated("s1")), []);
}

#[test]
fn a_turn_end_before_the_agent_named_its_transcript_captures_nothing() {
    let mut host = HostDriver::new();
    host.select_workspace(REPO);
    host.new_session_with("s1", claude_session_spec());
    host.worktree_ready_at("s1", BASE);
    host.agent_started("s1", "claude-code", Some("abc"));
    assert_eq!(captures(&host.turn_ended("s1")), []);
}

#[test]
fn a_shell_session_never_captures() {
    let mut host = HostDriver::new();
    host.select_workspace(REPO);
    host.new_session("s1");
    host.worktree_ready_at("s1", BASE);
    assert_eq!(captures(&host.turn_ended("s1")), []);
    assert_eq!(captures(&host.close("s1")), []);
}

#[test]
fn a_session_started_before_base_commits_were_recorded_captures_with_no_base() {
    let mut host = HostDriver::new();
    host.select_workspace(REPO);
    host.new_session_with("s1", claude_session_spec());
    host.worktree_ready("s1");
    host.agent_started_with_transcript("s1", "claude-code", "abc", TRANSCRIPT);
    let commands = host.turn_ended("s1");
    let found = captures(&commands);
    assert_eq!(found.len(), 1, "{commands:?}");
    assert_eq!(found[0].0[0].base, None);
}

// ---------- close ----------

#[test]
fn closing_an_agent_session_captures_it() {
    let mut host = running();
    let expected = job(&host, CaptureReason::Close);
    let commands = host.close_with("s1", CloseWorktree::Keep, hooks(&[]));
    assert_eq!(
        captures(&commands),
        [(&[expected][..], &AfterCapture::Nothing)]
    );
    assert!(commands.contains(&closed(false, None)), "{commands:?}");
}

// ---------- before removal ----------

/// The ordering: nothing removes the worktree until its capture has answered.
#[test]
fn closing_removes_the_worktree_only_after_capturing_it() {
    let (host, commands) = closed_and_inspected(&[]);
    let expected = job(&host, CaptureReason::BeforeRemove);
    assert_eq!(
        commands,
        vec![HostCommand::Capture {
            captures: vec![expected],
            then: remove_s1(),
        }]
    );
    assert!(!removes(&commands));
}

#[test]
fn a_capture_that_succeeds_lets_the_worktree_go() {
    let (mut host, _) = closed_and_inspected(&[]);
    let commands = host.captured(remove_s1(), Ok(()));
    assert_eq!(
        commands,
        vec![HostCommand::RemoveWorktree {
            remover: Remover::Session(sid("s1")),
            path: path(),
            force: false,
            note: None,
        }]
    );
}

#[test]
fn a_capture_that_fails_keeps_the_worktree_and_says_why() {
    let (mut host, _) = closed_and_inspected(&[]);
    let refused = ArchiveError::Refused("the transcript is outside the agent home".into());
    let commands = host.captured(remove_s1(), Err(refused));
    assert_eq!(
        commands,
        vec![closed(
            false,
            Some(
                "archiving its conversation failed, so it was kept: the transcript is outside the \
                 agent home"
            )
        )]
    );
    assert!(!host.host().sessions().iter().any(|s| s.id == sid("s1")));
}

/// A session started in the worktree while its capture ran (one a newer request would refuse, recorded anyway, say
/// by another host run) keeps it, even though the capture worked.
#[test]
fn a_worktree_a_session_started_in_while_it_was_captured_is_kept() {
    let (mut host, _) = closed_and_inspected(&[]);
    host.append(
        Some("s2"),
        Source::new(SourceKind::Shepherd, None),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/session/created",
                "params": { "name": "other", "cwd": WORKTREE } }),
    );
    let commands = host.captured(remove_s1(), Ok(()));
    assert_eq!(
        commands,
        vec![closed(false, Some("it is still in use by s2"))]
    );
}

/// A transcript that doesn't exist (an agent that named one it never wrote, one Claude Code deleted after
/// `cleanupPeriodDays`) is nothing to save: the worktree goes, and the close says no transcript was found.
#[test]
fn a_missing_transcript_lets_the_worktree_go_and_the_close_says_so() {
    let (mut host, _) = closed_and_inspected(&[]);
    let missing = ArchiveError::Missing(format!("{TRANSCRIPT} doesn't exist"));
    let commands = host.captured(remove_s1(), Err(missing));
    let note = format!("no transcript was found to archive: {TRANSCRIPT} doesn't exist");
    assert_eq!(
        commands,
        vec![HostCommand::RemoveWorktree {
            remover: Remover::Session(sid("s1")),
            path: path(),
            force: false,
            note: Some(note.clone()),
        }]
    );
    let removed = host.handle(HostEvent::WorktreeRemoved {
        remover: Remover::Session(sid("s1")),
        path: path(),
        result: Ok(()),
        exists: false,
        note: Some(note.clone()),
    });
    assert_eq!(removed, vec![closed_noting(true, None, &note)]);
}

#[test]
fn a_missing_transcript_lets_an_orphan_go() {
    let mut host = running();
    host.close_with("s1", CloseWorktree::Keep, hooks(&[]));
    let reply = ReplyTo::new(99);
    let then = AfterCapture::Remove {
        remover: Remover::Orphan {
            reply,
            repo: PathBuf::from(REPO),
            branch: Some("s1".to_owned()),
        },
        path: path(),
        force: false,
    };
    host.handle(HostEvent::RemoveOrphan {
        reply,
        path: PathBuf::from(WORKTREE),
        hooks: hooks(&[]),
    });
    let commands = host.captured(then, Err(ArchiveError::Missing("gone".into())));
    assert!(removes(&commands), "{commands:?}");
}

#[test]
fn the_capture_comes_after_the_pre_remove_hooks() {
    let (mut host, commands) = closed_and_inspected(&["make clean"]);
    assert_eq!(captures(&commands), []);
    assert!(
        matches!(commands[..], [HostCommand::RunPreRemove { .. }]),
        "{commands:?}"
    );
    let HostCommand::RunPreRemove {
        remover,
        command,
        rest,
        path,
        force,
    } = commands[0].clone()
    else {
        unreachable!()
    };
    let finished = host.handle(HostEvent::PreRemoveFinished {
        remover,
        command: command.clone(),
        rest,
        path,
        force,
        result: Ok(CommandOutput {
            exit: CommandExit::Code(0),
            tail: Vec::new(),
        }),
    });
    let _: &CommandSpec = &command;
    let expected = job(&host, CaptureReason::BeforeRemove);
    assert_eq!(captures(&finished), [(&[expected][..], &remove_s1())]);
    assert!(!removes(&finished));
}

#[test]
fn a_shells_worktree_is_removed_without_a_capture() {
    let mut host = HostDriver::new();
    host.select_workspace(REPO);
    host.new_session("s1");
    host.worktree_ready_at("s1", BASE);
    host.close("s1");
    let commands = host.handle(HostEvent::WorktreeInspected {
        remover: Remover::Session(sid("s1")),
        path: path(),
        purpose: Inspect::Close {
            confirmed: None,
            hooks: PreRemove {
                hooks: Vec::new(),
                timeout: TIMEOUT,
            },
        },
        result: listed(),
    });
    assert!(removes(&commands), "{commands:?}");
    assert_eq!(captures(&commands), []);
}

#[test]
fn a_capture_for_a_removal_that_ended_does_nothing() {
    let (mut host, _) = closed_and_inspected(&[]);
    host.captured(remove_s1(), Ok(()));
    host.handle(HostEvent::WorktreeRemoved {
        remover: Remover::Session(sid("s1")),
        path: path(),
        result: Ok(()),
        exists: false,
        note: None,
    });
    assert_eq!(host.captured(remove_s1(), Ok(())), vec![]);
}

// ---------- orphans ----------

/// `s1` ran in the worktree and was closed keeping it; the cleanup view removes it.
#[test]
fn an_orphan_is_removed_only_after_the_closed_sessions_that_ran_there_are_captured() {
    let mut host = running();
    host.close_with("s1", CloseWorktree::Keep, hooks(&[]));
    let reply = ReplyTo::new(99);
    host.handle(HostEvent::RemoveOrphan {
        reply,
        path: PathBuf::from(WORKTREE),
        hooks: hooks(&[]),
    });
    let orphan = |branch: Option<&str>| Remover::Orphan {
        reply,
        repo: PathBuf::from(REPO),
        branch: branch.map(str::to_owned),
    };
    let commands = host.handle(HostEvent::WorktreeInspected {
        remover: orphan(None),
        path: path(),
        purpose: Inspect::Close {
            confirmed: None,
            hooks: PreRemove {
                hooks: Vec::new(),
                timeout: TIMEOUT,
            },
        },
        result: listed(),
    });
    let then = AfterCapture::Remove {
        remover: orphan(Some("s1")),
        path: path(),
        force: false,
    };
    let expected = job(&host, CaptureReason::BeforeRemove);
    assert_eq!(
        commands,
        vec![HostCommand::Capture {
            captures: vec![expected],
            then: then.clone(),
        }]
    );
    let failed = host.captured(then, Err(ArchiveError::Failed("disk full".into())));
    let kept = "archiving its conversation failed, so it was kept: disk full";
    assert!(
        failed.contains(&HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeRemove(WorktreeRemoveOk {
                removed: false,
                kept: Some(kept.to_owned()),
            })),
        }),
        "{failed:?}"
    );
    assert!(!removes(&failed));
}

// ---------- a new agent session in place (`/clear`) ----------

const NEW_TRANSCRIPT: &str = "/home/test/.claude/projects/-wt-s1/def.jsonl";

/// A `SessionStart` hook payload naming agent session `id` and its transcript at `path`, started from `source`.
fn session_start(id: &str, path: &str, source: &str) -> Value {
    json!({
        "hook_event_name": "SessionStart",
        "session_id": id,
        "transcript_path": path,
        "source": source,
    })
}

#[test]
fn a_new_agent_session_in_place_captures_the_old_transcript_before_its_hook_is_translated() {
    for agent in [AgentKind::ClaudeCode, AgentKind::Codex] {
        let mut host = running();
        let commands = hook(
            &mut host,
            agent,
            session_start("def", NEW_TRANSCRIPT, "clear"),
        );
        // The old agent session's job, built before the new `agent/started` replaces its id and path.
        let old = job(&host, CaptureReason::TurnEnd);
        assert_eq!(
            captures(&commands),
            [(&[old][..], &AfterCapture::Nothing)],
            "{agent:?}"
        );
        let at = |want: fn(&HostCommand) -> bool| commands.iter().position(want);
        let capture = at(|c| matches!(c, HostCommand::Capture { .. }));
        let translate = at(|c| matches!(c, HostCommand::TranslateHook { .. }));
        assert!(
            capture.is_some() && capture < translate,
            "{agent:?}: {commands:?}"
        );
    }
}

#[test]
fn a_session_start_of_the_same_agent_session_captures_nothing() {
    // A resume or a compaction names the agent session it already is.
    let mut host = running();
    for source in ["resume", "compact"] {
        let start = session_start("abc", TRANSCRIPT, source);
        assert_eq!(captures(&hook(&mut host, AgentKind::ClaudeCode, start)), []);
    }
}

#[test]
fn a_compaction_captures_nothing_even_naming_another_agent_session() {
    // A compaction goes on in the same agent session: its `SessionStart` starts none (the translators skip it too).
    let mut host = running();
    let start = session_start("def", NEW_TRANSCRIPT, "compact");
    assert_eq!(captures(&hook(&mut host, AgentKind::ClaudeCode, start)), []);
}

#[test]
fn the_first_session_start_has_no_old_transcript_to_capture() {
    let mut host = HostDriver::new();
    host.select_workspace(REPO);
    host.new_session_with("s1", claude_session_spec());
    host.worktree_ready_at("s1", BASE);
    let start = session_start("abc", TRANSCRIPT, "startup");
    assert_eq!(captures(&hook(&mut host, AgentKind::ClaudeCode, start)), []);
}

#[test]
fn after_a_new_agent_session_in_place_a_turn_end_captures_the_new_one() {
    let mut host = running();
    host.agent_started_with_transcript("s1", "claude-code", "def", NEW_TRANSCRIPT);
    let commands = host.turn_ended("s1");
    let expected = CaptureJob {
        agent_session_id: "def".to_owned(),
        transcript_path: PathBuf::from(NEW_TRANSCRIPT),
        ..job(&host, CaptureReason::TurnEnd)
    };
    assert_eq!(
        captures(&commands),
        [(&[expected][..], &AfterCapture::Nothing)]
    );
}

// ---------- agent homes ----------

#[test]
fn a_profile_that_sets_its_agents_home_is_where_that_agents_transcripts_are() {
    let homes = AgentHomes::under(std::path::Path::new("/home/u"));
    let cwd = std::path::Path::new("/wt/s1");
    let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    let claude = |e: &BTreeMap<String, String>| homes.launched_with(AgentKind::ClaudeCode, e, cwd);
    let codex = |e: &BTreeMap<String, String>| homes.launched_with(AgentKind::Codex, e, cwd);

    // Nothing set: the host's own.
    assert_eq!(claude(&env(&[])), Some(PathBuf::from("/home/u/.claude")));
    assert_eq!(codex(&env(&[])), Some(PathBuf::from("/home/u/.codex")));
    // Each agent's own variable, for that agent only.
    let set = env(&[
        ("CLAUDE_CONFIG_DIR", "/cfg/claude"),
        ("CODEX_HOME", "/cfg/codex"),
    ]);
    assert_eq!(claude(&set), Some(PathBuf::from("/cfg/claude")));
    assert_eq!(codex(&set), Some(PathBuf::from("/cfg/codex")));
    let only_claude = env(&[("CLAUDE_CONFIG_DIR", "/cfg/claude")]);
    assert_eq!(codex(&only_claude), Some(PathBuf::from("/home/u/.codex")));
    // A relative one is relative to where the agent starts, as the agent reads it.
    let relative = env(&[("CODEX_HOME", "state/codex")]);
    assert_eq!(codex(&relative), Some(PathBuf::from("/wt/s1/state/codex")));
    // A `HOME` of its own moves the defaults, under the agent's own variable.
    let home = env(&[("HOME", "/other"), ("CODEX_HOME", "/cfg/codex")]);
    assert_eq!(claude(&home), Some(PathBuf::from("/other/.claude")));
    assert_eq!(codex(&home), Some(PathBuf::from("/cfg/codex")));
    // Empty counts as unset, as `AgentHomes::resolve` reads the host's.
    let empty = env(&[("CLAUDE_CONFIG_DIR", ""), ("HOME", "")]);
    assert_eq!(claude(&empty), Some(PathBuf::from("/home/u/.claude")));
    assert_eq!(homes.launched_with(AgentKind::Shell, &set, cwd), None);
}

#[test]
fn agent_homes_default_under_the_users_home() {
    let homes = AgentHomes::resolve(std::path::Path::new("/home/u"), None, None);
    assert_eq!(homes.claude, PathBuf::from("/home/u/.claude"));
    assert_eq!(homes.codex, PathBuf::from("/home/u/.codex"));
}

#[test]
fn agent_homes_follow_their_variables_and_an_empty_one_counts_as_unset() {
    let home = std::path::Path::new("/home/u");
    let set = AgentHomes::resolve(
        home,
        Some(std::ffi::OsStr::new("/cfg/claude")),
        Some(std::ffi::OsStr::new("/cfg/codex")),
    );
    assert_eq!(set.claude, PathBuf::from("/cfg/claude"));
    assert_eq!(set.codex, PathBuf::from("/cfg/codex"));
    let empty = AgentHomes::resolve(
        home,
        Some(std::ffi::OsStr::new("")),
        Some(std::ffi::OsStr::new("")),
    );
    assert_eq!(empty, AgentHomes::under(home));
}

// ---------- requests ----------

#[test]
fn a_job_becomes_the_session_record_and_a_snapshot_at_its_latest_commit() {
    let mut host = running();
    host.stamp_machine("local");
    let mut job = job(&host, CaptureReason::Close);
    job.machine = MachineId::new("local");
    let commits = vec![
        (
            hash(&"1".repeat(40)),
            Some(PatchId::parse(&"a".repeat(40)).unwrap()),
        ),
        (hash(&"2".repeat(40)), None),
    ];
    let root = RepoRoot::new(hash(&"0".repeat(40)));
    let at = ts("2026-10-31T09:00:00.000Z").instant();
    let this = MachineId::new("ab12");
    let found = Found {
        repo: root.clone(),
        commits: commits.clone(),
    };
    let (session, request) = job.requests(found, at, &this, PathBuf::from("/home/test/.claude"));
    assert_eq!(
        session,
        ArchivedSession {
            id: sid("s1"),
            repo: root,
            machine: this,
            agent: AgentKind::ClaudeCode,
            agent_session_id: "abc".to_owned(),
            name: Some("claude-1".to_owned()),
            branch: Some(BranchName::new("s1")),
            base: Some(hash(BASE)),
            cwd: PathBuf::from(WORKTREE),
            created_at: job.created.instant(),
        }
    );
    assert_eq!(
        request,
        CaptureRequest {
            session: sid("s1"),
            at,
            head: Some(hash(&"2".repeat(40))),
            reason: CaptureReason::Close,
            commits,
            agent_home: PathBuf::from("/home/test/.claude"),
            transcript_path: PathBuf::from(TRANSCRIPT),
        }
    );
}

#[test]
fn a_job_with_no_commits_of_its_own_has_no_known_head() {
    let host = running();
    let found = Found {
        repo: RepoRoot::new(hash(&"0".repeat(40))),
        commits: Vec::new(),
    };
    let at = ts("2026-10-31T09:00:00.000Z").instant();
    let (_, request) = job(&host, CaptureReason::TurnEnd).requests(
        found,
        at,
        &MachineId::new("m_test"),
        PathBuf::from("/h"),
    );
    assert_eq!(request.head, None);
}
