//! New sessions in a workspace get a worktree (T22), through the public `shepherd_core::host` API.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, Source, SourceKind};
use shepherd_core::host::naming::worktree_name;
use shepherd_core::host::{
    Host, HostCommand, HostEvent, NewSessionSpec, PostCreate, ResumeUnavailable, WorkspaceAsk,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::{PtySize, SpawnSpec};
use shepherd_core::ports::{
    BaseSpec, BaseUsed, BranchName, Created, RepoRef, WorktreeError, WorktreeName, WorktreePath,
};
use shepherd_core::wire::{HostResult, SessionNewOk};
use shepherd_testkit::builders::{
    HostDriver, claude_session_spec, shell_session_spec, shell_spawn,
};

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn append(id: &str, method: &str, params: Value) -> HostCommand {
    let frame = Frame::from_value(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
        .unwrap_or_else(|e| panic!("bad frame: {e}"));
    HostCommand::Append(Box::new(FrameDraft::new(
        frame,
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        Some(sid(id)),
    )))
}

fn name(n: &str) -> WorktreeName {
    WorktreeName::new(n).unwrap_or_else(|e| panic!("{e}"))
}

/// A host with `/src/app` selected as its workspace.
fn in_workspace() -> HostDriver {
    let mut host = HostDriver::new();
    let ask = WorkspaceAsk::Select(host.reply_to());
    host.handle(HostEvent::WorkspaceProbed {
        ask,
        repo: Ok(PathBuf::from("/src/app")),
    });
    host
}

fn created(path: &str, branch: &str, base: BaseUsed) -> Created {
    Created {
        path: WorktreePath::new(path),
        branch: BranchName::new(branch),
        base,
    }
}

fn origin(branch: &str) -> BaseUsed {
    BaseUsed::Origin {
        branch: BranchName::new(branch),
    }
}

/// The worktree result for session `id`, as the runtime feeds it back with the spawn spec it was given.
fn worktree_result(
    host: &mut HostDriver,
    id: &str,
    spawn: SpawnSpec,
    result: Result<Created, WorktreeError>,
) -> Vec<HostCommand> {
    host.handle(HostEvent::WorktreeCreated {
        session: sid(id),
        spawn,
        hooks: PostCreate::default(),
        result,
    })
}

fn spawns(commands: &[HostCommand]) -> usize {
    commands
        .iter()
        .filter(|c| matches!(c, HostCommand::SpawnPty { .. }))
        .count()
}

fn replay(driver: &HostDriver) -> Host {
    let mut host = Host::new();
    for env in driver.log() {
        host.apply(env);
    }
    host
}

// ---------- worktree names ----------

#[test]
fn worktree_names_are_lowercase_ascii_slugs_of_the_session_name() {
    let long = "x".repeat(100);
    let cases: [(&str, &str); 16] = [
        ("claude-3", "claude-3"),
        ("Claude-3", "claude-3"),
        ("fix login!", "fix-login"),
        ("café-1", "caf-1"),
        ("naïve über", "na-ve-ber"),
        ("日本語", "session"),
        ("", "session"),
        ("--a--b--", "a-b"),
        ("a.b..c", "a-b-c"),
        ("a/b\\c:d", "a-b-c-d"),
        ("x.lock", "x-lock"),
        ("HEAD", "head"),
        ("_under_score_", "under_score"),
        // Separators alone leave nothing.
        ("___", "session"),
        ("-_-", "session"),
        (&long, &long[..64]),
    ];
    for (session, expected) in cases {
        assert_eq!(
            worktree_name(session, &BTreeSet::new()),
            name(expected),
            "{session:?}"
        );
    }
}

#[test]
fn a_taken_worktree_name_gets_the_first_free_numeric_suffix() {
    let x64 = "x".repeat(64);
    let x62 = format!("{}-2", "x".repeat(62));
    let cases: [(&str, &[&str], &str); 6] = [
        ("claude-3", &["claude-3"], "claude-3-2"),
        ("claude-3", &["claude-3", "claude-3-2"], "claude-3-3"),
        ("claude-3", &["claude-3-2"], "claude-3"),
        // Case-insensitive filesystems can't hold both, so taken names compare without case.
        ("claude-3", &["Claude-3"], "claude-3-2"),
        ("日本語", &["session"], "session-2"),
        // The suffix still fits in 64 characters.
        (&x64, &[&x64], &x62),
    ];
    for (session, taken, expected) in cases {
        let taken: BTreeSet<String> = taken.iter().map(|t| (*t).to_owned()).collect();
        assert_eq!(
            worktree_name(session, &taken),
            name(expected),
            "{session:?} with {taken:?}"
        );
    }
}

// ---------- new session flow ----------

#[test]
fn a_new_session_in_a_workspace_is_created_then_asks_for_a_worktree_instead_of_spawning() {
    let mut host = in_workspace();
    let reply = host.reply_to();
    let at = host.now();
    let cmds = host.handle(HostEvent::NewSession {
        reply,
        id: sid("s1"),
        at,
        spec: shell_session_spec(),
    });
    assert_eq!(
        cmds,
        vec![
            append(
                "s1",
                "_shepherd/session/created",
                json!({
                    "name": "shell-1", "workspace": "/src/app", "branch": "shell-1",
                    "agent": "shell", "profile": "shell"
                })
            ),
            HostCommand::CreateWorktree {
                session: sid("s1"),
                repo: RepoRef::new("/src/app"),
                name: name("shell-1"),
                base: BaseSpec::OriginDefault,
                spawn: shell_spawn(),
                hooks: PostCreate::default(),
            },
            HostCommand::Reply {
                to: reply,
                result: Ok(HostResult::SessionNew(SessionNewOk { session: sid("s1") })),
            },
        ]
    );
}

#[test]
fn a_session_waiting_for_its_worktree_is_listed_but_has_no_pty() {
    let mut host = in_workspace();
    host.new_session("s1");
    let listed = host.host().sessions();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].exited);
    assert_eq!(listed[0].failed, None);
    assert_eq!(listed[0].resume, Err(ResumeUnavailable::Starting));
    let input = host.handle(HostEvent::Input {
        session: sid("s1"),
        bytes: b"ls\n".to_vec(),
    });
    let resize = host.handle(HostEvent::Resize {
        session: sid("s1"),
        size: PtySize { cols: 10, rows: 5 },
    });
    assert_eq!((input, resize), (vec![], vec![]));
    let close = host.close("s1");
    assert!(
        !close
            .iter()
            .any(|c| matches!(c, HostCommand::KillPty { .. })),
        "{close:?}"
    );
}

#[test]
fn the_new_worktree_is_recorded_then_the_pty_spawns_in_it() {
    let mut host = in_workspace();
    host.new_session("s1");
    let cmds = worktree_result(
        &mut host,
        "s1",
        shell_spawn(),
        Ok(created("/wt/app/shell-1", "shell-1", origin("main"))),
    );
    assert_eq!(
        cmds,
        vec![
            append(
                "s1",
                "_shepherd/worktree/created",
                json!({ "path": "/wt/app/shell-1", "branch": "shell-1", "base": "origin/main" })
            ),
            HostCommand::SpawnPty {
                session: sid("s1"),
                spec: SpawnSpec {
                    cwd: PathBuf::from("/wt/app/shell-1"),
                    ..shell_spawn()
                },
            },
        ]
    );
    let listed = host.host().sessions();
    assert!(!listed[0].exited);
    assert_eq!(listed[0].resume, Err(ResumeUnavailable::Running));
    assert_eq!(replay(&host), *host.host());
}

#[test]
fn a_worktree_started_from_a_fallback_base_records_why_the_fetch_failed() {
    let cases = [
        (
            BaseUsed::Stale {
                branch: BranchName::new("main"),
                fetch_error: "offline".to_owned(),
            },
            json!({ "path": "/wt/app/shell-1", "branch": "shell-1", "base": "origin/main", "fetch_error": "offline" }),
        ),
        (
            BaseUsed::Local {
                branch: BranchName::new("main"),
                fetch_error: "offline".to_owned(),
            },
            json!({ "path": "/wt/app/shell-1", "branch": "shell-1", "base": "main", "fetch_error": "offline" }),
        ),
    ];
    for (base, recorded) in cases {
        let mut host = in_workspace();
        host.new_session("s1");
        let cmds = worktree_result(
            &mut host,
            "s1",
            shell_spawn(),
            Ok(created("/wt/app/shell-1", "shell-1", base)),
        );
        assert_eq!(
            cmds[0],
            append("s1", "_shepherd/worktree/created", recorded)
        );
        assert_eq!(spawns(&cmds), 1);
    }
}

#[test]
fn a_failed_worktree_fails_the_session_with_the_reason_and_spawns_nothing() {
    let errors = [
        WorktreeError::FetchFailed("could not resolve host".to_owned()),
        WorktreeError::BranchExists(BranchName::new("shell-1")),
        WorktreeError::NotARepo(PathBuf::from("/src/app")),
    ];
    for error in errors {
        let reason = error.to_string();
        let mut host = in_workspace();
        host.new_session("s1");
        let cmds = worktree_result(&mut host, "s1", shell_spawn(), Err(error));
        assert_eq!(
            cmds,
            vec![append(
                "s1",
                "_shepherd/session/failed",
                json!({ "reason": reason })
            )],
            "{reason}"
        );
        let listed = host.host().sessions();
        assert_eq!(listed[0].failed.as_deref(), Some(reason.as_str()));
        assert!(listed[0].exited, "a failed session has no PTY");
        assert_eq!(listed[0].resume, Err(ResumeUnavailable::Failed));
        assert_eq!(
            host.handle(HostEvent::Input {
                session: sid("s1"),
                bytes: b"x".to_vec()
            }),
            vec![]
        );
        assert_eq!(replay(&host), *host.host());
    }
}

#[test]
fn a_result_for_a_session_closed_meanwhile_is_recorded_and_spawns_nothing() {
    // Each row: what git came back with after the user closed the session, and the frame that records it. A created
    // worktree stays in place for cleanup (T25), which finds it through the record.
    let cases = [
        (
            Ok(created("/wt/app/shell-1", "shell-1", origin("main"))),
            append(
                "s1",
                "_shepherd/worktree/created",
                json!({ "path": "/wt/app/shell-1", "branch": "shell-1", "base": "origin/main" }),
            ),
        ),
        (
            Err(WorktreeError::FetchFailed("offline".to_owned())),
            append(
                "s1",
                "_shepherd/session/failed",
                json!({ "reason": "fetching origin failed: offline" }),
            ),
        ),
    ];
    for (result, recorded) in cases {
        let mut host = in_workspace();
        host.new_session("s1");
        host.close("s1");
        let cmds = worktree_result(&mut host, "s1", shell_spawn(), result);
        assert_eq!(cmds, vec![recorded]);
        assert!(host.host().sessions().is_empty());
        assert_eq!(replay(&host), *host.host());
    }
}

#[test]
fn a_result_for_a_session_that_is_not_waiting_changes_nothing() {
    let mut host = in_workspace();
    host.new_session("s1");
    let ok = Ok(created("/wt/app/shell-1", "shell-1", origin("main")));
    worktree_result(&mut host, "s1", shell_spawn(), ok.clone());
    let before = host.host().clone();
    assert_eq!(
        worktree_result(&mut host, "s1", shell_spawn(), ok.clone()),
        vec![]
    );
    assert_eq!(
        worktree_result(&mut host, "nope", shell_spawn(), ok),
        vec![]
    );
    // A session from before workspaces never waited for a worktree.
    let mut plain = HostDriver::new();
    plain.new_session("s1");
    let result = Err(WorktreeError::Other("late".to_owned()));
    assert_eq!(
        worktree_result(&mut plain, "s1", shell_spawn(), result),
        vec![]
    );
    assert_eq!(*host.host(), before);
}

#[test]
fn without_a_workspace_or_with_an_explicit_cwd_a_new_session_spawns_where_asked() {
    let explicit = NewSessionSpec {
        explicit_cwd: true,
        ..shell_session_spec()
    };
    for (why, mut host, spec) in [
        ("no workspace", HostDriver::new(), shell_session_spec()),
        ("explicit cwd", in_workspace(), explicit),
    ] {
        let cmds = host.new_session_with("s1", spec);
        assert_eq!(spawns(&cmds), 1, "{why}: {cmds:?}");
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, HostCommand::CreateWorktree { .. })),
            "{why}"
        );
    }
}

#[test]
fn branch_names_are_unique_against_every_branch_the_host_has_recorded() {
    let mut host = in_workspace();
    let api = || NewSessionSpec {
        name: Some("API".to_owned()),
        ..shell_session_spec()
    };
    host.new_session_with("s1", api());
    // A closed session's branch is still taken: removing a worktree keeps its branch.
    host.close("s1");
    let cmds = host.new_session_with("s2", api());
    assert!(
        cmds.iter().any(|c| matches!(c,
            HostCommand::CreateWorktree { name: n, .. } if *n == name("api-2"))),
        "{cmds:?}"
    );
    // The fold knows them too, so a restarted host keeps counting.
    let mut restarted = HostDriver::new();
    for env in host.log() {
        restarted.append(
            env.session().map(SessionId::as_str),
            env.source().clone(),
            env.frame().raw().clone(),
        );
    }
    let cmds = restarted.new_session_with("s3", api());
    assert!(
        cmds.iter().any(|c| matches!(c,
            HostCommand::CreateWorktree { name: n, .. } if *n == name("api-3"))),
        "{cmds:?}"
    );
}

#[test]
fn resuming_a_worktree_session_starts_it_in_its_worktree() {
    let mut host = in_workspace();
    host.new_session_with("s1", claude_session_spec());
    worktree_result(
        &mut host,
        "s1",
        claude_session_spec().spawn,
        Ok(created("/wt/app/claude-1", "claude-1", origin("main"))),
    );
    host.agent_started("s1", "claude-code", Some("abc123"));
    host.pty_exited("s1", Some(0));
    let target = host
        .host()
        .resume_target(&sid("s1"))
        .and_then(Result::ok)
        .unwrap_or_else(|| panic!("not resumable"));
    assert_eq!(target.cwd, PathBuf::from("/wt/app/claude-1"));
}

#[test]
fn frames_that_arrive_in_the_wrong_phase_change_nothing() {
    // What a session looks like: (exited, failed, creating_worktree, resume).
    type Look = (bool, Option<String>, bool, Result<(), ResumeUnavailable>);
    let starting: Look = (false, None, true, Err(ResumeUnavailable::Starting));
    let running: Look = (false, None, false, Err(ResumeUnavailable::Running));
    let failed: Look = (
        true,
        Some("boom".to_owned()),
        false,
        Err(ResumeUnavailable::Failed),
    );
    let worktree = json!({ "method": "_shepherd/worktree/created",
        "params": { "path": "/wt/late", "branch": "late", "base": "origin/main" } });
    let session_failed =
        json!({ "method": "_shepherd/session/failed", "params": { "reason": "late" } });
    let exited = json!({ "method": "_shepherd/agent/exited", "params": { "code": 1 } });
    let started =
        json!({ "method": "_shepherd/agent/started", "params": { "agent": "claude-code" } });
    // Each row: how the session got into its phase, the stray frame (source `shepherd`), and how it must still look.
    let cases: [(&str, Look, Value); 7] = [
        (
            "worktree/created on a session with no worktree",
            running.clone(),
            worktree.clone(),
        ),
        (
            "session/failed on a running session",
            running.clone(),
            session_failed.clone(),
        ),
        (
            "agent/exited while the worktree is made",
            starting.clone(),
            exited.clone(),
        ),
        (
            "agent/started while the worktree is made",
            starting.clone(),
            started.clone(),
        ),
        (
            "worktree/created on a failed session",
            failed.clone(),
            worktree,
        ),
        ("agent/exited on a failed session", failed.clone(), exited),
        ("agent/started on a failed session", failed.clone(), started),
    ];
    let look = |host: &HostDriver| -> Look {
        let s = &host.host().sessions()[0];
        (s.exited, s.failed.clone(), s.creating_worktree, s.resume)
    };
    for (why, phase, stray) in cases {
        let mut host = if phase == running {
            HostDriver::new()
        } else {
            in_workspace()
        };
        host.new_session_with("s1", claude_session_spec());
        if phase == failed {
            host.worktree_result("s1", Err(WorktreeError::Other("boom".to_owned())));
        }
        assert_eq!(look(&host), phase, "{why}: setup");
        let mut frame = stray;
        frame["jsonrpc"] = json!("2.0");
        host.append(Some("s1"), Source::new(SourceKind::Shepherd, None), frame);
        assert_eq!(look(&host), phase, "{why}");
    }
}

#[test]
fn a_session_still_waiting_for_its_worktree_when_the_host_restarts_fails() {
    let mut host = in_workspace();
    host.new_session("s1");
    // On start, the runtime reports every session that isn't exited as exited: its PTY, or its worktree creation, is
    // gone with the old host.
    let cmds = host.pty_exited("s1", None);
    assert_eq!(
        cmds,
        vec![append(
            "s1",
            "_shepherd/session/failed",
            json!({ "reason": "the host stopped before the worktree was ready" })
        )]
    );
    assert!(host.host().sessions()[0].failed.is_some());
}
