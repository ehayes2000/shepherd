//! What the cleanup view asks of the host (T25b): the workspace's orphan worktrees with their status, and removing one
//! through the same inspect, pre-remove hooks and remove path a closing session's worktree takes (T25). Through the
//! public `shepherd_core::host` API.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, Source, SourceKind};
use shepherd_core::config::WorkspaceHooks;
use shepherd_core::host::{
    Host, HostCommand, HostEvent, Inspect, Inspection, PreRemove, Remover, ReplyTo, WorkspaceAsk,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{
    BaseUsed, BranchName, CommandError, CommandExit, CommandOutput, CommandSpec, Created, RepoRef,
    Worktree, WorktreeError, WorktreePath, WorktreeStatus,
};
use shepherd_core::wire::{
    CleanupInfo, HostResult, WireError, WorktreeCleanupOk, WorktreeRemoveOk,
};
use shepherd_testkit::builders::{HostDriver, shell_session_spec};

const REPO: &str = "/src/app";
const ORPHAN: &str = "/wt/app/old-idea";
const TIMEOUT: Duration = Duration::from_secs(30);

fn host_append(method: &str, params: Value) -> HostCommand {
    let frame = Frame::from_value(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
        .unwrap_or_else(|e| panic!("bad frame: {e}"));
    HostCommand::Append(Box::new(FrameDraft::new(
        frame,
        Source::new(SourceKind::Shepherd, None),
        Dir::Internal,
        None,
    )))
}

fn in_workspace() -> HostDriver {
    let mut host = HostDriver::new();
    let ask = WorkspaceAsk::Select(host.reply_to());
    host.handle(HostEvent::WorkspaceProbed {
        ask,
        repo: Ok(PathBuf::from(REPO)),
    });
    host
}

fn status(dirty: bool, ahead: u32, pushed: bool, merged: bool) -> WorktreeStatus {
    WorktreeStatus {
        dirty,
        ahead,
        pushed,
        merged_into_default: merged,
    }
}

fn clean_merged() -> WorktreeStatus {
    status(false, 0, false, true)
}

fn hooks(pre_remove: &[&str]) -> WorkspaceHooks {
    WorkspaceHooks {
        post_create: Vec::new(),
        pre_remove: pre_remove.iter().map(|h| (*h).to_owned()).collect(),
        timeout: TIMEOUT,
    }
}

fn pre_remove(hooks: &[&str]) -> PreRemove {
    PreRemove {
        hooks: hooks.iter().map(|h| (*h).to_owned()).collect(),
        timeout: TIMEOUT,
    }
}

fn path() -> WorktreePath {
    WorktreePath::new(ORPHAN)
}

/// The orphan's remover once inspected: on the branch the port listed, `old-idea`.
fn remover(reply: ReplyTo) -> Remover {
    Remover::Orphan {
        reply,
        repo: PathBuf::from(REPO),
        branch: Some("old-idea".to_owned()),
    }
}

/// The orphan's remover until it is inspected: no branch known yet.
fn uninspected(reply: ReplyTo) -> Remover {
    Remover::Orphan {
        reply,
        repo: PathBuf::from(REPO),
        branch: None,
    }
}

/// Asks to remove [`ORPHAN`], the workspace's hooks being `hooks_`. Returns the reply token and the commands.
fn remove_orphan(host: &mut HostDriver, hooks_: &[&str]) -> (ReplyTo, Vec<HostCommand>) {
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::RemoveOrphan {
        reply,
        path: PathBuf::from(ORPHAN),
        hooks: hooks(hooks_),
    });
    (reply, commands)
}

fn inspected(
    host: &mut HostDriver,
    reply: ReplyTo,
    hooks_: &[&str],
    result: Result<Inspection, WorktreeError>,
) -> Vec<HostCommand> {
    host.handle(HostEvent::WorktreeInspected {
        remover: uninspected(reply),
        path: path(),
        purpose: Inspect::Close {
            confirmed: None,
            hooks: pre_remove(hooks_),
        },
        result,
    })
}

/// A managed worktree on branch `old-idea`, with `status`.
fn listed(status: Result<WorktreeStatus, WorktreeError>) -> Inspection {
    Inspection::Listed {
        status,
        exists: true,
        branch: Some("old-idea".to_owned()),
    }
}

/// `script` as it runs in the orphan: no session, its branch and the workspace's repo.
fn hook(script: &str) -> CommandSpec {
    CommandSpec {
        script: script.to_owned(),
        cwd: PathBuf::from(ORPHAN),
        env: vec![
            ("SHEPHERD_WORKTREE".to_owned(), ORPHAN.to_owned()),
            ("SHEPHERD_BRANCH".to_owned(), "old-idea".to_owned()),
            ("SHEPHERD_REPO".to_owned(), REPO.to_owned()),
        ],
        timeout: TIMEOUT,
    }
}

fn exited(code: i32) -> Result<CommandOutput, CommandError> {
    Ok(CommandOutput {
        exit: CommandExit::Code(code),
        tail: Vec::new(),
    })
}

/// The `worktree/removed` that ends the orphan's removal, and the answer to `reply`.
fn finished(reply: ReplyTo, removed: bool, kept: Option<&str>) -> Vec<HostCommand> {
    let mut params = json!({ "path": ORPHAN, "removed": removed });
    if let Some(kept) = kept {
        params["kept"] = json!(kept);
    }
    vec![
        host_append("_shepherd/worktree/removed", params),
        HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeRemove(WorktreeRemoveOk {
                removed,
                kept: kept.map(str::to_owned),
            })),
        },
    ]
}

fn refused(commands: &[HostCommand]) -> Option<String> {
    match commands {
        [
            HostCommand::Reply {
                result: Err(WireError::InvalidParams(why)),
                ..
            },
        ] => Some(why.clone()),
        _ => None,
    }
}

// ---------- removing an orphan ----------

#[test]
fn removing_an_orphan_records_it_and_inspects_it_like_a_closing_sessions_worktree() {
    let mut host = in_workspace();
    let (reply, commands) = remove_orphan(&mut host, &["make clean"]);
    assert_eq!(
        commands,
        vec![
            host_append(
                "_shepherd/worktree/removing",
                json!({ "path": ORPHAN, "pre_remove": ["make clean"] }),
            ),
            HostCommand::InspectWorktree {
                remover: uninspected(reply),
                repo: RepoRef::new(REPO),
                path: path(),
                purpose: Inspect::Close {
                    confirmed: None,
                    hooks: pre_remove(&["make clean"]),
                },
            },
        ]
    );
}

#[test]
fn an_orphan_that_loses_nothing_runs_its_pre_remove_hooks_then_is_removed_and_answered() {
    let mut host = in_workspace();
    let (reply, _) = remove_orphan(&mut host, &["make clean"]);
    let found = inspected(
        &mut host,
        reply,
        &["make clean"],
        Ok(listed(Ok(clean_merged()))),
    );
    let run = HostCommand::RunPreRemove {
        remover: remover(reply),
        command: hook("make clean"),
        rest: pre_remove(&[]),
        path: path(),
        force: false,
    };
    assert_eq!(found, vec![run]);
    let done = host.handle(HostEvent::PreRemoveFinished {
        remover: remover(reply),
        command: hook("make clean"),
        rest: pre_remove(&[]),
        path: path(),
        force: false,
        result: exited(0),
    });
    assert_eq!(
        done[1..],
        [HostCommand::RemoveWorktree {
            remover: remover(reply),
            path: path(),
            force: false,
        }]
    );
    // Its hook is recorded in the host log, with no session.
    assert!(matches!(&done[0], HostCommand::Append(d) if d.session.is_none()));
    let removed = host.handle(HostEvent::WorktreeRemoved {
        remover: remover(reply),
        path: path(),
        result: Ok(()),
        exists: false,
    });
    assert_eq!(removed, finished(reply, true, None));
}

#[test]
fn an_orphans_hooks_get_the_branch_the_port_listed() {
    let mut host = in_workspace();
    let (reply, _) = remove_orphan(&mut host, &["make clean"]);
    let found = inspected(
        &mut host,
        reply,
        &["make clean"],
        Ok(Inspection::Listed {
            status: Ok(clean_merged()),
            exists: true,
            branch: Some("what-git-says".to_owned()),
        }),
    );
    let [HostCommand::RunPreRemove { command, .. }] = &found[..] else {
        panic!("{found:?}")
    };
    assert!(
        command
            .env
            .contains(&("SHEPHERD_BRANCH".to_owned(), "what-git-says".to_owned())),
        "{:?}",
        command.env
    );
}

#[test]
fn a_detached_orphans_hooks_get_an_empty_branch() {
    let mut host = in_workspace();
    let (reply, _) = remove_orphan(&mut host, &["make clean"]);
    let found = inspected(
        &mut host,
        reply,
        &["make clean"],
        Ok(Inspection::Listed {
            status: Ok(clean_merged()),
            exists: true,
            branch: None,
        }),
    );
    let [HostCommand::RunPreRemove { command, .. }] = &found[..] else {
        panic!("{found:?}")
    };
    let branches: Vec<&(String, String)> = command
        .env
        .iter()
        .filter(|(key, _)| key == "SHEPHERD_BRANCH")
        .collect();
    assert_eq!(
        branches,
        [&("SHEPHERD_BRANCH".to_owned(), String::new())],
        "{:?}",
        command.env
    );
}

#[test]
fn an_orphan_that_would_lose_work_is_kept_and_never_forced() {
    let cases = [
        (status(true, 0, false, true), "it has uncommitted changes"),
        (
            status(false, 2, false, false),
            "it has 2 commits not pushed or merged",
        ),
    ];
    for (now, why) in cases {
        let mut host = in_workspace();
        let (reply, _) = remove_orphan(&mut host, &[]);
        let commands = inspected(&mut host, reply, &[], Ok(listed(Ok(now))));
        assert_eq!(commands, finished(reply, false, Some(why)), "{now:?}");
    }
}

#[test]
fn a_failing_pre_remove_hook_keeps_the_orphan_and_says_why() {
    let mut host = in_workspace();
    let (reply, _) = remove_orphan(&mut host, &["make clean", "rm -rf node_modules"]);
    inspected(
        &mut host,
        reply,
        &["make clean", "rm -rf node_modules"],
        Ok(listed(Ok(clean_merged()))),
    );
    let done = host.handle(HostEvent::PreRemoveFinished {
        remover: remover(reply),
        command: hook("make clean"),
        rest: pre_remove(&["rm -rf node_modules"]),
        path: path(),
        force: false,
        result: exited(2),
    });
    assert_eq!(
        done[1..],
        finished(
            reply,
            false,
            Some("pre_remove hook failed (exit 2): make clean")
        )[..]
    );
}

#[test]
fn an_orphan_outside_the_managed_root_or_unreadable_is_left_alone() {
    // (what inspecting found, how it ends)
    let cases = [
        (
            Ok(Inspection::Unlisted { exists: true }),
            Some(format!("{ORPHAN} isn't a worktree shepherd manages")),
            false,
        ),
        (
            Ok(listed(Err(WorktreeError::NotARepo(PathBuf::from(ORPHAN))))),
            Some(format!("{ORPHAN} is not a git repository")),
            false,
        ),
        (
            Err(WorktreeError::Other("git broke".into())),
            Some("git broke".to_owned()),
            false,
        ),
        // Already gone: nothing kept, nothing removed.
        (Ok(Inspection::Unlisted { exists: false }), None, false),
    ];
    for (result, why, removed) in cases {
        let mut host = in_workspace();
        let (reply, _) = remove_orphan(&mut host, &[]);
        let commands = inspected(&mut host, reply, &[], result.clone());
        assert_eq!(
            commands,
            finished(reply, removed, why.as_deref()),
            "{result:?}"
        );
    }
}

#[test]
fn a_session_started_in_the_orphan_while_it_is_inspected_keeps_it() {
    let mut host = in_workspace();
    let (reply, _) = remove_orphan(&mut host, &[]);
    // Not through `session/new`, which refuses a worktree being removed: a new session's worktree, made at that path
    // (as a racing git could).
    host.new_session("s1");
    host.worktree_result(
        "s1",
        Ok(Created {
            path: path(),
            branch: BranchName::new("x"),
            base: BaseUsed::Origin {
                branch: BranchName::new("main"),
            },
        }),
    );
    let commands = inspected(&mut host, reply, &[], Ok(listed(Ok(clean_merged()))));
    assert_eq!(
        commands,
        finished(reply, false, Some("it is still in use by s1"))
    );
}

#[test]
fn removing_an_orphan_a_session_uses_or_that_is_being_removed_is_refused() {
    let mut host = in_workspace();
    let mut spec = shell_session_spec();
    spec.explicit_cwd = true;
    spec.spawn.cwd = PathBuf::from(ORPHAN);
    host.new_session_with("s1", spec);
    let (_, commands) = remove_orphan(&mut host, &[]);
    assert_eq!(
        refused(&commands),
        Some(format!("{ORPHAN} is in use by s1"))
    );

    let mut host = in_workspace();
    remove_orphan(&mut host, &[]);
    let (_, again) = remove_orphan(&mut host, &[]);
    assert_eq!(
        refused(&again),
        Some(format!("{ORPHAN} is already being removed"))
    );
}

#[test]
fn removing_an_orphan_without_a_workspace_is_refused() {
    let mut host = HostDriver::new();
    let (_, commands) = remove_orphan(&mut host, &[]);
    assert_eq!(refused(&commands), Some("no workspace is selected".into()));
}

#[test]
fn nothing_starts_in_an_orphan_being_removed_and_the_finder_doesnt_offer_it() {
    let mut host = in_workspace();
    remove_orphan(&mut host, &[]);
    let mut spec = shell_session_spec();
    spec.explicit_cwd = true;
    spec.spawn.cwd = PathBuf::from(ORPHAN);
    spec.worktree_branch = Some("old-idea".into());
    let commands = host.new_session_with("s1", spec);
    assert!(
        refused(&commands).is_some_and(|why| why.contains("is being removed")),
        "{commands:?}"
    );
    let reply = host.reply_to();
    let listed = host.handle(HostEvent::WorktreesListed {
        reply,
        result: Ok(vec![Worktree {
            path: path(),
            branch: Some(BranchName::new("old-idea")),
        }]),
    });
    assert!(
        matches!(&listed[..], [HostCommand::Reply { result: Ok(HostResult::WorktreeList(ok)), .. }] if ok.worktrees.is_empty()),
        "{listed:?}"
    );
}

#[test]
fn once_removed_the_orphan_is_no_longer_being_removed() {
    let mut host = in_workspace();
    let (reply, _) = remove_orphan(&mut host, &[]);
    inspected(
        &mut host,
        reply,
        &[],
        Ok(listed(Ok(status(true, 0, false, true)))),
    );
    let (_, again) = remove_orphan(&mut host, &[]);
    assert_eq!(refused(&again), None, "{again:?}");
}

#[test]
fn results_for_an_orphan_that_is_not_being_removed_do_nothing() {
    let mut host = in_workspace();
    let reply = host.reply_to();
    assert_eq!(
        inspected(&mut host, reply, &[], Ok(listed(Ok(clean_merged())))),
        vec![]
    );
    let removed = host.handle(HostEvent::WorktreeRemoved {
        remover: remover(reply),
        path: path(),
        result: Ok(()),
        exists: false,
    });
    assert_eq!(removed, vec![]);
}

#[test]
fn a_restart_ends_an_orphan_removal_that_was_cut_short_keeping_it() {
    let mut host = in_workspace();
    remove_orphan(&mut host, &[]);
    let mut replayed = Host::new();
    for env in host.log() {
        replayed.apply(env);
    }
    assert_eq!(
        replayed.handle(HostEvent::Restarted),
        vec![host_append(
            "_shepherd/worktree/removed",
            json!({ "path": ORPHAN, "removed": false,
                    "kept": "the host stopped before the worktree was removed" }),
        )]
    );
}

// ---------- listing for cleanup ----------

#[test]
fn listing_for_cleanup_inspects_the_workspaces_worktrees_skipping_those_in_use() {
    let mut host = in_workspace();
    host.new_session("s1");
    host.worktree_ready("s1");
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::ListCleanup {
        reply,
        after_days: 7,
    });
    assert_eq!(
        commands,
        vec![HostCommand::InspectOrphans {
            reply,
            repo: RepoRef::new(REPO),
            skip: vec![PathBuf::from("/wt/s1")],
            after_days: 7,
        }]
    );
}

#[test]
fn listing_for_cleanup_without_a_workspace_answers_that_there_are_none() {
    let mut host = HostDriver::new();
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::ListCleanup {
        reply,
        after_days: 3,
    });
    assert_eq!(
        commands,
        vec![HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeCleanup(WorktreeCleanupOk {
                worktrees: Vec::new(),
                cleanup_after_days: 3,
            })),
        }]
    );
}

#[test]
fn the_cleanup_list_answers_each_orphan_with_its_status_or_why_it_has_none() {
    let mut host = in_workspace();
    host.new_session("s1");
    host.worktree_ready("s1");
    let reply = host.reply_to();
    let worktree = |path: &str, branch: Option<&str>| Worktree {
        path: WorktreePath::new(path),
        branch: branch.map(BranchName::new),
    };
    let commands = host.handle(HostEvent::OrphansInspected {
        reply,
        after_days: 7,
        result: Ok(vec![
            (worktree(ORPHAN, Some("old-idea")), Ok(clean_merged())),
            (
                worktree("/wt/app/spike", None),
                Err(WorktreeError::Other("git broke".into())),
            ),
            // In use by s1 since it was inspected: no orphan.
            (worktree("/wt/s1", Some("s1")), Ok(clean_merged())),
        ]),
    });
    assert_eq!(
        commands,
        vec![HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeCleanup(WorktreeCleanupOk {
                worktrees: vec![
                    CleanupInfo {
                        path: ORPHAN.into(),
                        branch: Some("old-idea".into()),
                        status: Some(clean_merged()),
                        error: None,
                    },
                    CleanupInfo {
                        path: "/wt/app/spike".into(),
                        branch: None,
                        status: None,
                        error: Some("git broke".into()),
                    },
                ],
                cleanup_after_days: 7,
            })),
        }]
    );
}

#[test]
fn a_cleanup_list_that_fails_is_refused_with_the_reason() {
    let mut host = in_workspace();
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::OrphansInspected {
        reply,
        after_days: 7,
        result: Err(WorktreeError::NotARepo(PathBuf::from(REPO))),
    });
    assert_eq!(
        refused(&commands),
        Some(format!("{REPO} is not a git repository"))
    );
}

// ---------- when a session exited ----------

#[test]
fn the_host_knows_when_a_session_exited() {
    let mut host = in_workspace();
    host.new_session("s1");
    assert_eq!(host.host().exited_at(&SessionId::new("s1")), None);
    host.pin_ts("2026-10-20T08:00:00Z");
    host.pty_exited("s1", Some(0));
    host.unpin_ts();
    assert_eq!(
        host.host().exited_at(&SessionId::new("s1")),
        Some(&shepherd_testkit::builders::ts("2026-10-20T08:00:00Z"))
    );
}
