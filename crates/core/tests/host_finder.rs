//! What the finder asks of the host (T26): the workspace's managed worktrees no open session runs in, and starting a
//! session in one of them. Through the public `shepherd_core::host` API.

use std::path::PathBuf;

use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, Source, SourceKind};
use shepherd_core::host::{
    HostCommand, HostEvent, Inspect, NewSessionSpec, Remover, ReplyTo, WorkspaceAsk,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::SpawnSpec;
use shepherd_core::ports::{BranchName, RepoRef, Worktree, WorktreeError, WorktreePath};
use shepherd_core::wire::{
    CloseWorktree, HostResult, SessionNewOk, WireError, WorktreeInfo, WorktreeListOk,
};
use shepherd_testkit::builders::{HostDriver, shell_session_spec, shell_spawn};

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

fn worktree(path: &str, branch: Option<&str>) -> Worktree {
    Worktree {
        path: WorktreePath::new(path),
        branch: branch.map(BranchName::new),
    }
}

fn info(path: &str, branch: Option<&str>) -> WorktreeInfo {
    WorktreeInfo {
        path: path.to_owned(),
        branch: branch.map(str::to_owned),
    }
}

fn listed(
    host: &mut HostDriver,
    reply: ReplyTo,
    result: Result<Vec<Worktree>, WorktreeError>,
) -> Vec<HostCommand> {
    host.handle(HostEvent::WorktreesListed { reply, result })
}

#[test]
fn listing_worktrees_asks_the_port_about_the_current_workspaces_repo() {
    let mut host = in_workspace();
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::ListWorktrees { reply });
    assert_eq!(
        cmds,
        vec![HostCommand::ListWorktrees {
            reply,
            repo: RepoRef::new("/src/app"),
        }]
    );
}

#[test]
fn listing_worktrees_without_a_workspace_answers_that_there_are_none() {
    let mut host = HostDriver::new();
    let reply = host.reply_to();
    let cmds = host.handle(HostEvent::ListWorktrees { reply });
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeList(WorktreeListOk::default())),
        }]
    );
}

#[test]
fn the_list_leaves_out_the_worktrees_of_open_sessions() {
    let mut host = in_workspace();
    // s1 runs in its worktree; s2's is still being made; s3 closed keeping its worktree.
    for id in ["s1", "s2", "s3"] {
        host.new_session(id);
    }
    host.worktree_ready("s1");
    host.worktree_ready("s3");
    host.close_with(
        "s3",
        CloseWorktree::Keep,
        shepherd_core::config::WorkspaceHooks::default(),
    );
    let making = host
        .host()
        .sessions()
        .into_iter()
        .find(|s| s.id == sid("s2"))
        .and_then(|s| s.branch)
        .expect("s2 has a branch while its worktree is made");
    let reply = host.reply_to();
    let cmds = listed(
        &mut host,
        reply,
        Ok(vec![
            worktree("/wt/s1", Some("s1")),
            worktree(&format!("/managed/app/{making}"), Some(&making)),
            worktree("/wt/s3", Some("s3")),
            worktree("/wt/old-idea", Some("old-idea")),
            worktree("/wt/detached", None),
        ]),
    );
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeList(WorktreeListOk {
                worktrees: vec![
                    info("/wt/s3", Some("s3")),
                    info("/wt/old-idea", Some("old-idea")),
                    info("/wt/detached", None),
                ],
            })),
        }]
    );
}

#[test]
fn a_list_that_fails_is_refused_with_the_reason() {
    let mut host = in_workspace();
    let reply = host.reply_to();
    let error = WorktreeError::Timeout("git worktree list".to_owned());
    let cmds = listed(&mut host, reply, Err(error.clone()));
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Err(WireError::InvalidParams(error.to_string())),
        }]
    );
}

/// A new shell session asked to run in worktree `/wt/old-idea`, on branch `old-idea`.
fn in_old_idea() -> NewSessionSpec {
    NewSessionSpec {
        explicit_cwd: true,
        worktree_branch: Some("old-idea".to_owned()),
        spawn: SpawnSpec {
            cwd: PathBuf::from("/wt/old-idea"),
            ..shell_spawn()
        },
        ..shell_session_spec()
    }
}

#[test]
fn a_session_started_in_a_listed_worktree_spawns_there_and_records_it_as_its_worktree() {
    let mut host = in_workspace();
    let reply = host.reply_to();
    let at = host.now();
    let cmds = host.handle(HostEvent::NewSession {
        reply,
        id: sid("s1"),
        at,
        spec: in_old_idea(),
    });
    assert_eq!(
        cmds,
        vec![
            append(
                "s1",
                "_shepherd/session/created",
                json!({
                    "name": "shell-1", "workspace": "/src/app", "worktree": "/wt/old-idea",
                    "branch": "old-idea", "agent": "shell", "profile": "shell", "cwd": "/wt/old-idea"
                })
            ),
            HostCommand::SpawnPty {
                session: sid("s1"),
                spec: in_old_idea().spawn,
            },
            HostCommand::Reply {
                to: reply,
                result: Ok(HostResult::SessionNew(SessionNewOk { session: sid("s1") })),
            },
        ]
    );
    let session = &host.host().sessions()[0];
    assert_eq!(session.worktree, Some(PathBuf::from("/wt/old-idea")));
    assert_eq!(session.branch.as_deref(), Some("old-idea"));
    assert!(!session.creating_worktree);
}

#[test]
fn closing_a_session_started_in_a_listed_worktree_inspects_that_worktree() {
    let mut host = in_workspace();
    host.new_session_with("s1", in_old_idea());
    let cmds = host.close("s1");
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            HostCommand::InspectWorktree { path, purpose: Inspect::Close { .. }, .. }
                if *path == WorktreePath::new("/wt/old-idea")
        )),
        "{cmds:?}"
    );
}

/// A new shell session asked to run in `/wt/old-idea` without saying it is a worktree: it doesn't own it.
fn plain_in_old_idea() -> NewSessionSpec {
    NewSessionSpec {
        worktree_branch: None,
        ..in_old_idea()
    }
}

fn closed(id: &str, why: &str) -> HostCommand {
    append(
        id,
        "_shepherd/session/closed",
        json!({ "worktree_removed": false, "worktree_kept": why }),
    )
}

fn refused(cmds: &[HostCommand]) -> Option<String> {
    cmds.iter().find_map(|c| match c {
        HostCommand::Reply {
            result: Err(WireError::InvalidParams(why)),
            ..
        } => Some(why.clone()),
        _ => None,
    })
}

#[test]
fn starting_in_a_worktree_another_session_owns_or_runs_in_is_refused() {
    for (why, first) in [("owns", in_old_idea()), ("runs in", plain_in_old_idea())] {
        let mut host = in_workspace();
        host.new_session_with("s1", first);
        let cmds = host.new_session_with("s2", in_old_idea());
        assert_eq!(
            refused(&cmds),
            Some("/wt/old-idea is already in use by s1".to_owned()),
            "{why}: {cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, HostCommand::Append(_) | HostCommand::SpawnPty { .. })),
            "{why}: nothing is created"
        );
        assert_eq!(host.host().sessions().len(), 1, "{why}");
    }
}

#[test]
fn closing_a_session_keeps_its_worktree_while_another_session_runs_in_it() {
    let mut host = in_workspace();
    host.new_session_with("s1", in_old_idea());
    host.new_session_with("s2", plain_in_old_idea());
    let cmds = host.close("s1");
    assert!(
        cmds.contains(&closed("s1", "it is still in use by s2")),
        "{cmds:?}"
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, HostCommand::InspectWorktree { .. })),
        "{cmds:?}"
    );
}

#[test]
fn a_worktree_another_session_started_in_while_closing_is_kept() {
    let mut host = in_workspace();
    host.new_session_with("s1", in_old_idea());
    let close = host.close_with(
        "s1",
        CloseWorktree::Remove {
            confirmed: shepherd_core::ports::WorktreeStatus {
                dirty: true,
                ..Default::default()
            },
        },
        shepherd_core::config::WorkspaceHooks::default(),
    );
    let Some(HostCommand::InspectWorktree { purpose, .. }) = close
        .iter()
        .find(|c| matches!(c, HostCommand::InspectWorktree { .. }))
        .cloned()
    else {
        panic!("{close:?}");
    };
    // A new session there is refused now, so one that got past that (say, recorded by another host run) stands in.
    started_there_anyway(&mut host, "s2");
    let cmds = host.handle(HostEvent::WorktreeInspected {
        remover: Remover::Session(sid("s1")),
        path: WorktreePath::new("/wt/old-idea"),
        purpose,
        result: Ok(shepherd_core::host::Inspection::Listed {
            status: Ok(shepherd_core::ports::WorktreeStatus {
                dirty: true,
                merged_into_default: true,
                ..Default::default()
            }),
            exists: true,
        }),
    });
    assert_eq!(cmds, vec![closed("s1", "it is still in use by s2")]);
}

#[test]
fn a_worktree_a_session_runs_in_without_owning_it_is_no_orphan() {
    let mut host = in_workspace();
    host.new_session_with("s1", plain_in_old_idea());
    let reply = host.reply_to();
    let cmds = listed(&mut host, reply, Ok(vec![worktree("/wt/old-idea", None)]));
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeList(WorktreeListOk::default())),
        }]
    );
}

#[test]
fn only_a_worktree_being_made_in_this_workspace_hides_one_on_its_branch() {
    let mut host = HostDriver::new();
    let ask = WorkspaceAsk::Select(host.reply_to());
    host.handle(HostEvent::WorkspaceProbed {
        ask,
        repo: Ok(PathBuf::from("/src/other")),
    });
    // Waiting for a worktree of `/src/other`, on branch `shell-1`.
    host.new_session("s1");
    let ask = WorkspaceAsk::Select(host.reply_to());
    host.handle(HostEvent::WorkspaceProbed {
        ask,
        repo: Ok(PathBuf::from("/src/app")),
    });
    let reply = host.reply_to();
    let cmds = listed(
        &mut host,
        reply,
        Ok(vec![worktree("/wt/app/shell-1", Some("shell-1"))]),
    );
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeList(WorktreeListOk {
                worktrees: vec![info("/wt/app/shell-1", Some("shell-1"))],
            })),
        }]
    );
}

/// Records session `id` running in `/wt/old-idea` straight into the log, past `session/new`'s refusals.
fn started_there_anyway(host: &mut HostDriver, id: &str) {
    host.append(
        Some(id),
        Source::new(SourceKind::Shepherd, None),
        json!({ "jsonrpc": "2.0", "method": "_shepherd/session/created",
                "params": { "name": "other", "cwd": "/wt/old-idea" } }),
    );
}

/// `s1` started in `/wt/old-idea` and closing with one pre-remove hook, which is running. Returns that hook's
/// command.
fn closing_with_a_hook(host: &mut HostDriver) -> HostCommand {
    host.new_session_with("s1", in_old_idea());
    let hooks = shepherd_core::config::WorkspaceHooks {
        pre_remove: vec!["make clean".to_owned()],
        ..Default::default()
    };
    let close = host.close_with("s1", CloseWorktree::IfSafe, hooks);
    let Some(HostCommand::InspectWorktree { purpose, .. }) = close
        .iter()
        .find(|c| matches!(c, HostCommand::InspectWorktree { .. }))
        .cloned()
    else {
        panic!("{close:?}");
    };
    let cmds = host.handle(HostEvent::WorktreeInspected {
        remover: Remover::Session(sid("s1")),
        path: WorktreePath::new("/wt/old-idea"),
        purpose,
        result: Ok(shepherd_core::host::Inspection::Listed {
            status: Ok(shepherd_core::ports::WorktreeStatus {
                merged_into_default: true,
                ..Default::default()
            }),
            exists: true,
        }),
    });
    match &cmds[..] {
        [run @ HostCommand::RunPreRemove { .. }] => run.clone(),
        _ => panic!("{cmds:?}"),
    }
}

#[test]
fn nothing_starts_in_a_worktree_being_removed() {
    let mut host = in_workspace();
    closing_with_a_hook(&mut host);
    let cmds = host.new_session_with("s2", plain_in_old_idea());
    assert_eq!(
        refused(&cmds),
        Some("/wt/old-idea is being removed by closing s1".to_owned()),
        "{cmds:?}"
    );
}

#[test]
fn a_worktree_a_session_started_in_while_its_pre_remove_hook_ran_is_kept() {
    let mut host = in_workspace();
    let HostCommand::RunPreRemove {
        command,
        rest,
        path,
        force,
        ..
    } = closing_with_a_hook(&mut host)
    else {
        unreachable!()
    };
    // A session in it got past the refusal (say, recorded by another host run).
    started_there_anyway(&mut host, "s2");
    let cmds = host.handle(HostEvent::PreRemoveFinished {
        remover: Remover::Session(sid("s1")),
        command,
        rest,
        path,
        force,
        result: Ok(shepherd_core::ports::CommandOutput {
            exit: shepherd_core::ports::CommandExit::Code(0),
            tail: Vec::new(),
        }),
    });
    assert_eq!(cmds.last(), Some(&closed("s1", "it is still in use by s2")));
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, HostCommand::RemoveWorktree { .. })),
        "{cmds:?}"
    );
}

#[test]
fn a_session_whose_worktree_failed_or_was_cancelled_hides_no_orphan_on_its_branch() {
    let mut host = in_workspace();
    host.new_session("s1");
    host.new_session("s2");
    host.worktree_result("s1", Err(WorktreeError::FetchFailed("offline".to_owned())));
    host.close("s2");
    let reply = host.reply_to();
    let cmds = listed(
        &mut host,
        reply,
        Ok(vec![
            worktree("/wt/app/shell-1", Some("shell-1")),
            worktree("/wt/app/shell-2", Some("shell-2")),
        ]),
    );
    assert_eq!(
        cmds,
        vec![HostCommand::Reply {
            to: reply,
            result: Ok(HostResult::WorktreeList(WorktreeListOk {
                worktrees: vec![
                    info("/wt/app/shell-1", Some("shell-1")),
                    info("/wt/app/shell-2", Some("shell-2")),
                ],
            })),
        }]
    );
}

#[test]
fn a_branch_given_without_an_explicit_cwd_is_ignored() {
    let mut host = HostDriver::new();
    let spec = NewSessionSpec {
        worktree_branch: Some("old-idea".to_owned()),
        ..shell_session_spec()
    };
    host.new_session_with("s1", spec);
    let session = &host.host().sessions()[0];
    assert_eq!(
        (session.worktree.clone(), session.branch.clone()),
        (None, None)
    );
}
