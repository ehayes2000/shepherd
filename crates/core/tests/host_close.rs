//! Closing a session removes its worktree when that loses nothing (T25), through the public `shepherd_core::host` API.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, Source, SourceKind};
use shepherd_core::config::WorkspaceHooks;
use shepherd_core::host::{
    Host, HostCommand, HostEvent, Inspect, Inspection, PreRemove, ResumeUnavailable,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{
    CommandError, CommandExit, CommandOutput, CommandSpec, RepoRef, WorktreeError, WorktreePath,
    WorktreeStatus,
};
use shepherd_core::wire::{CloseWorktree, HostResult, InputKind, WireError, WorktreeStatusOk};
use shepherd_testkit::builders::HostDriver;

const REPO: &str = "/src/app";
const WORKTREE: &str = "/wt/s1";
const TIMEOUT: Duration = Duration::from_secs(30);

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

fn closed(removed: bool, kept: Option<&str>) -> HostCommand {
    let mut params = json!({ "worktree_removed": removed });
    if let Some(kept) = kept {
        params["worktree_kept"] = json!(kept);
    }
    append("s1", "_shepherd/session/closed", params)
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

fn pre_remove(hooks: &[&str]) -> PreRemove {
    PreRemove {
        hooks: hooks.iter().map(|h| (*h).to_owned()).collect(),
        timeout: TIMEOUT,
    }
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

/// A host with [`REPO`] selected and session `s1` running in its worktree at [`WORKTREE`].
fn running() -> HostDriver {
    let mut host = HostDriver::new();
    host.select_workspace(REPO);
    host.new_session("s1");
    host.worktree_ready("s1");
    host
}

/// [`running`], then closed with `how` and the workspace's `pre_remove` hooks. Returns the close's commands.
fn closing(how: CloseWorktree, hooks_: &[&str]) -> (HostDriver, Vec<HostCommand>) {
    let mut host = running();
    let commands = host.close_with("s1", how, hooks(hooks_));
    (host, commands)
}

fn inspected(
    host: &mut HostDriver,
    purpose: Inspect,
    result: Result<Inspection, WorktreeError>,
) -> Vec<HostCommand> {
    host.handle(HostEvent::WorktreeInspected {
        session: sid("s1"),
        path: path(),
        purpose,
        result,
    })
}

/// The purpose of a closing session's inspection: remove it if safe (`confirmed: None`), or even though removing loses
/// what the user `confirmed`.
fn close_purpose(confirmed: Option<WorktreeStatus>, hooks_: &[&str]) -> Inspect {
    Inspect::Close {
        confirmed,
        hooks: pre_remove(hooks_),
    }
}

/// What a dialog showed the user who chose to remove anyway: uncommitted changes and 3 unpushed commits.
fn everything() -> WorktreeStatus {
    status(true, 3, false, false)
}

fn remove_anyway() -> CloseWorktree {
    CloseWorktree::Remove {
        confirmed: everything(),
    }
}

/// A managed worktree, with `status`.
fn listed(status: Result<WorktreeStatus, WorktreeError>) -> Inspection {
    Inspection::Listed {
        status,
        exists: true,
    }
}

/// `script` as it runs in `s1`'s worktree.
fn hook(script: &str) -> CommandSpec {
    CommandSpec {
        script: script.to_owned(),
        cwd: PathBuf::from(WORKTREE),
        env: vec![
            ("SHEPHERD_SESSION".to_owned(), "s1".to_owned()),
            ("SHEPHERD_WORKTREE".to_owned(), WORKTREE.to_owned()),
            ("SHEPHERD_BRANCH".to_owned(), "s1".to_owned()),
            ("SHEPHERD_REPO".to_owned(), REPO.to_owned()),
        ],
        timeout: TIMEOUT,
    }
}

fn run_pre_remove(script: &str, rest: &[&str], force: bool) -> HostCommand {
    HostCommand::RunPreRemove {
        session: sid("s1"),
        command: hook(script),
        rest: pre_remove(rest),
        path: path(),
        force,
    }
}

fn hook_finished(
    host: &mut HostDriver,
    script: &str,
    rest: &[&str],
    force: bool,
    result: Result<CommandOutput, CommandError>,
) -> Vec<HostCommand> {
    host.handle(HostEvent::PreRemoveFinished {
        session: sid("s1"),
        command: hook(script),
        rest: pre_remove(rest),
        path: path(),
        force,
        result,
    })
}

fn exited(code: i32) -> Result<CommandOutput, CommandError> {
    Ok(CommandOutput {
        exit: CommandExit::Code(code),
        tail: Vec::new(),
    })
}

/// The result of removing `s1`'s worktree; `exists`: something is still at its path afterwards.
fn removed_with(
    host: &mut HostDriver,
    result: Result<(), WorktreeError>,
    exists: bool,
) -> Vec<HostCommand> {
    host.handle(HostEvent::WorktreeRemoved {
        session: sid("s1"),
        path: path(),
        result,
        exists,
    })
}

fn removed(host: &mut HostDriver, result: Result<(), WorktreeError>) -> Vec<HostCommand> {
    let exists = result.is_err();
    removed_with(host, result, exists)
}

fn remove(force: bool) -> HostCommand {
    HostCommand::RemoveWorktree {
        session: sid("s1"),
        path: path(),
        force,
    }
}

fn replay(driver: &HostDriver) -> Host {
    let mut host = Host::new();
    for env in driver.log() {
        host.apply(env);
    }
    host
}

// ---------- closing ----------

#[test]
fn closing_a_worktree_session_kills_its_pty_records_the_removal_and_inspects_the_worktree() {
    let (host, commands) = closing(CloseWorktree::IfSafe, &["make clean"]);
    let reply = commands.iter().find_map(|c| match c {
        HostCommand::Reply { to, .. } => Some(*to),
        _ => None,
    });
    assert_eq!(
        commands,
        vec![
            HostCommand::KillPty { session: sid("s1") },
            append(
                "s1",
                "_shepherd/worktree/removing",
                json!({ "path": WORKTREE, "pre_remove": ["make clean"] })
            ),
            HostCommand::InspectWorktree {
                session: sid("s1"),
                repo: RepoRef::new(REPO),
                path: path(),
                purpose: close_purpose(None, &["make clean"]),
            },
            HostCommand::Reply {
                to: reply.unwrap_or_else(|| panic!("no reply")),
                result: Ok(HostResult::Ack),
            },
        ]
    );
    // Still listed until the outcome is known, as removing its worktree, with its PTY gone.
    let listed = &host.host().sessions()[0];
    assert!(listed.removing_worktree);
    assert!(listed.exited);
    assert_eq!(listed.resume, Err(ResumeUnavailable::Closing));
    assert_eq!(
        listed.worktree.as_deref(),
        Some(PathBuf::from(WORKTREE).as_path())
    );
}

#[test]
fn removing_anyway_records_a_forced_removal() {
    let (_, commands) = closing(remove_anyway(), &[]);
    assert!(commands.contains(&append(
        "s1",
        "_shepherd/worktree/removing",
        json!({ "path": WORKTREE, "force": true })
    )));
    assert!(commands.contains(&HostCommand::InspectWorktree {
        session: sid("s1"),
        repo: RepoRef::new(REPO),
        path: path(),
        purpose: close_purpose(Some(everything()), &[]),
    }));
}

#[test]
fn keeping_the_worktree_closes_at_once() {
    let (host, commands) = closing(CloseWorktree::Keep, &["make clean"]);
    assert_eq!(commands[0], HostCommand::KillPty { session: sid("s1") });
    assert_eq!(commands[1], closed(false, None));
    assert_eq!(commands.len(), 3, "{commands:?}");
    assert!(host.host().sessions().is_empty());
}

#[test]
fn a_session_without_a_worktree_closes_at_once() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let commands = host.close_with("s1", CloseWorktree::IfSafe, hooks(&["make clean"]));
    assert_eq!(commands[1], closed(false, None));
    assert!(host.host().sessions().is_empty());
}

#[test]
fn an_exited_session_closes_with_its_worktree_inspected_and_nothing_to_kill() {
    let mut host = running();
    host.pty_exited("s1", Some(0));
    let commands = host.close_with("s1", CloseWorktree::IfSafe, hooks(&[]));
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, HostCommand::KillPty { .. }))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, HostCommand::InspectWorktree { .. }))
    );
}

#[test]
fn a_session_whose_worktree_is_still_being_made_closes_at_once() {
    let mut host = HostDriver::new();
    host.select_workspace(REPO);
    host.new_session("s1");
    let commands = host.close_with("s1", remove_anyway(), hooks(&["make clean"]));
    assert_eq!(commands[0], closed(false, None));
}

#[test]
fn a_session_removing_its_worktree_takes_no_input_and_no_second_close() {
    let (mut host, _) = closing(CloseWorktree::IfSafe, &[]);
    let input = host.handle(HostEvent::Input {
        session: sid("s1"),
        bytes: b"ls\n".to_vec(),
        kind: InputKind::Keys,
    });
    assert_eq!(input, vec![]);
    assert_eq!(host.pty_exited("s1", None), vec![]);
    let again = host.close_with("s1", CloseWorktree::Keep, hooks(&[]));
    assert!(
        matches!(
            &again[..],
            [HostCommand::Reply {
                result: Err(WireError::UnknownSession { .. }),
                ..
            }]
        ),
        "{again:?}"
    );
}

// ---------- what the inspection finds ----------

#[test]
fn a_worktree_that_loses_nothing_runs_its_pre_remove_hooks_in_order_then_is_removed() {
    let (mut host, _) = closing(CloseWorktree::IfSafe, &["a", "b"]);
    let found = inspected(
        &mut host,
        close_purpose(None, &["a", "b"]),
        Ok(listed(Ok(clean_merged()))),
    );
    assert_eq!(found, vec![run_pre_remove("a", &["b"], false)]);
    let first = hook_finished(&mut host, "a", &["b"], false, exited(0));
    assert_eq!(
        first,
        vec![
            append(
                "s1",
                "_shepherd/worktree/hook",
                json!({ "hook": "a", "exit_code": 0, "output_tail": "" })
            ),
            run_pre_remove("b", &[], false),
        ]
    );
    let last = hook_finished(&mut host, "b", &[], false, exited(0));
    assert_eq!(
        last,
        vec![
            append(
                "s1",
                "_shepherd/worktree/hook",
                json!({ "hook": "b", "exit_code": 0, "output_tail": "" })
            ),
            remove(false),
        ]
    );
    assert!(host.host().sessions()[0].removing_worktree);
    assert_eq!(removed(&mut host, Ok(())), vec![closed(true, None)]);
    assert!(host.host().sessions().is_empty());
    assert_eq!(replay(&host), *host.host());
}

#[test]
fn a_worktree_with_no_pre_remove_hooks_is_removed_at_once() {
    let (mut host, _) = closing(CloseWorktree::IfSafe, &[]);
    let found = inspected(
        &mut host,
        close_purpose(None, &[]),
        Ok(listed(Ok(clean_merged()))),
    );
    assert_eq!(found, vec![remove(false)]);
}

#[test]
fn only_a_clean_worktree_that_is_merged_or_pushed_is_removed_unasked() {
    // (status, what closing does: remove, or keep it saying why)
    let cases = [
        (status(false, 0, false, true), None),
        (status(false, 2, true, false), None),
        (status(false, 0, true, true), None),
        (
            status(true, 0, false, true),
            Some("it has uncommitted changes"),
        ),
        (
            status(false, 1, false, false),
            Some("it has 1 commit not pushed or merged"),
        ),
        (
            status(true, 3, false, false),
            Some("it has uncommitted changes and 3 commits not pushed or merged"),
        ),
    ];
    for (found, kept) in cases {
        let (mut host, _) = closing(CloseWorktree::IfSafe, &["make clean"]);
        let commands = inspected(
            &mut host,
            close_purpose(None, &["make clean"]),
            Ok(listed(Ok(found))),
        );
        let expected = match kept {
            None => vec![run_pre_remove("make clean", &[], false)],
            Some(why) => vec![closed(false, Some(why))],
        };
        assert_eq!(commands, expected, "{found:?}");
    }
}

#[test]
fn removing_anyway_removes_a_worktree_that_would_lose_work_after_its_hooks() {
    let (mut host, _) = closing(remove_anyway(), &["make clean"]);
    let found = inspected(
        &mut host,
        close_purpose(Some(everything()), &["make clean"]),
        Ok(listed(Ok(status(true, 3, false, false)))),
    );
    assert_eq!(found, vec![run_pre_remove("make clean", &[], true)]);
    let done = hook_finished(&mut host, "make clean", &[], true, exited(0));
    assert_eq!(done[1], remove(true));
}

#[test]
fn removing_anyway_forces_only_when_the_user_confirmed_uncommitted_changes() {
    let unpushed = status(false, 2, false, false);
    let (mut host, _) = closing(
        CloseWorktree::Remove {
            confirmed: unpushed,
        },
        &[],
    );
    let found = inspected(
        &mut host,
        close_purpose(Some(unpushed), &[]),
        Ok(listed(Ok(unpushed))),
    );
    assert_eq!(found, vec![remove(false)]);
}

#[test]
fn removing_anyway_keeps_a_worktree_that_has_more_to_lose_than_the_user_confirmed() {
    // (confirmed, found now, why it's kept)
    let cases = [
        (
            status(false, 2, false, false),
            status(true, 2, false, false),
            "it has uncommitted changes, which closing didn't confirm",
        ),
        (
            status(true, 0, false, true),
            status(true, 1, false, false),
            "it has 1 commit not pushed or merged, which closing didn't confirm",
        ),
        (
            status(false, 1, false, false),
            status(false, 3, false, false),
            "it has 3 commits not pushed or merged, which closing didn't confirm",
        ),
    ];
    for (confirmed, now, why) in cases {
        let (mut host, _) = closing(CloseWorktree::Remove { confirmed }, &["make clean"]);
        let commands = inspected(
            &mut host,
            close_purpose(Some(confirmed), &["make clean"]),
            Ok(listed(Ok(now))),
        );
        assert_eq!(commands, vec![closed(false, Some(why))], "{now:?}");
    }
    // Less to lose than confirmed is fine.
    let (mut host, _) = closing(remove_anyway(), &[]);
    let found = inspected(
        &mut host,
        close_purpose(Some(everything()), &[]),
        Ok(listed(Ok(clean_merged()))),
    );
    assert_eq!(found, vec![remove(true)]);
}

#[test]
fn a_worktree_that_is_already_gone_closes_normally() {
    for found in [
        Inspection::Unlisted { exists: false },
        Inspection::Listed {
            status: Err(WorktreeError::NotARepo(PathBuf::from(WORKTREE))),
            exists: false,
        },
    ] {
        let (mut host, _) = closing(remove_anyway(), &["make clean"]);
        let commands = inspected(
            &mut host,
            close_purpose(Some(everything()), &["make clean"]),
            Ok(found.clone()),
        );
        assert_eq!(commands, vec![closed(false, None)], "{found:?}");
        assert!(host.host().sessions().is_empty());
    }
}

#[test]
fn a_recorded_path_that_is_not_a_managed_worktree_is_left_alone() {
    // E.g. a log saying the worktree is at `<root>/app/../../home`, or at a symlink out of the root: the port doesn't
    // list it, so no hook runs there and nothing is removed, even when forced.
    let (mut host, _) = closing(remove_anyway(), &["rm -rf ."]);
    let commands = inspected(
        &mut host,
        close_purpose(Some(everything()), &["rm -rf ."]),
        Ok(Inspection::Unlisted { exists: true }),
    );
    assert_eq!(
        commands,
        vec![closed(
            false,
            Some("/wt/s1 isn't a worktree shepherd manages")
        )]
    );
}

#[test]
fn an_inspection_error_keeps_the_worktree_and_says_why() {
    let cases = [
        Err(WorktreeError::Timeout("worktree list".into())),
        Ok(listed(Err(WorktreeError::NoDefaultBranch(PathBuf::from(
            REPO,
        ))))),
    ];
    let reasons = [
        "git worktree list timed out",
        "/src/app has no origin default branch",
    ];
    for (result, why) in cases.into_iter().zip(reasons) {
        let (mut host, _) = closing(remove_anyway(), &["make clean"]);
        let commands = inspected(
            &mut host,
            close_purpose(Some(everything()), &["make clean"]),
            result,
        );
        assert_eq!(commands, vec![closed(false, Some(why))]);
    }
}

#[test]
fn a_failing_pre_remove_hook_keeps_the_worktree_and_says_why() {
    let cases = [
        (exited(2), "pre_remove hook failed (exit 2): a"),
        (
            Ok(CommandOutput {
                exit: CommandExit::TimedOut,
                tail: Vec::new(),
            }),
            "pre_remove hook failed (timed out after 30s): a",
        ),
        (
            Err(CommandError::NoSuchDir(PathBuf::from(WORKTREE))),
            "pre_remove hook failed (/wt/s1 is not an existing directory): a",
        ),
    ];
    for (result, why) in cases {
        let (mut host, _) = closing(CloseWorktree::IfSafe, &["a", "b"]);
        inspected(
            &mut host,
            close_purpose(None, &["a", "b"]),
            Ok(listed(Ok(clean_merged()))),
        );
        let commands = hook_finished(&mut host, "a", &["b"], false, result);
        assert_eq!(commands.len(), 2, "{why}: {commands:?}");
        assert_eq!(commands[1], closed(false, Some(why)));
        assert!(host.host().sessions().is_empty());
    }
}

#[test]
fn a_remove_error_keeps_the_worktree_and_says_why_but_a_gone_one_closes_normally() {
    let cases = [
        (
            Err(WorktreeError::Dirty(path())),
            closed(false, Some("/wt/s1 has uncommitted changes")),
        ),
        (
            Err(WorktreeError::OutsideManagedRoot(PathBuf::from(WORKTREE))),
            closed(
                false,
                Some("/wt/s1 is not inside the managed worktree root"),
            ),
        ),
    ];
    for (result, expected) in cases {
        let (mut host, _) = closing(CloseWorktree::IfSafe, &[]);
        assert_eq!(removed(&mut host, result), vec![expected]);
    }
    let (mut host, _) = closing(CloseWorktree::IfSafe, &[]);
    let gone = Err(WorktreeError::NotARepo(PathBuf::from(WORKTREE)));
    assert_eq!(
        removed_with(&mut host, gone, false),
        vec![closed(false, None)]
    );
}

#[test]
fn not_a_repo_with_something_still_at_the_path_keeps_it_and_says_why() {
    let not_a_repo = || Err(WorktreeError::NotARepo(PathBuf::from(WORKTREE)));
    let why = "/wt/s1 is not a git repository";
    // Inspected,
    let (mut host, _) = closing(CloseWorktree::IfSafe, &["make clean"]);
    let found = Inspection::Listed {
        status: not_a_repo(),
        exists: true,
    };
    let commands = inspected(&mut host, close_purpose(None, &["make clean"]), Ok(found));
    assert_eq!(commands, vec![closed(false, Some(why))]);
    // removed,
    let (mut host, _) = closing(CloseWorktree::IfSafe, &[]);
    assert_eq!(
        removed_with(
            &mut host,
            Err(WorktreeError::NotARepo(PathBuf::from(WORKTREE))),
            true
        ),
        vec![closed(false, Some(why))]
    );
    // and asked about.
    let mut host = running();
    let reply = host.reply_to();
    let found = Inspection::Listed {
        status: not_a_repo(),
        exists: true,
    };
    let answer = host.handle(HostEvent::WorktreeInspected {
        session: sid("s1"),
        path: path(),
        purpose: Inspect::Ask(reply),
        result: Ok(found),
    });
    assert_eq!(
        answer,
        vec![HostCommand::Reply {
            to: reply,
            result: Err(WireError::InvalidParams(why.into())),
        }]
    );
}

#[test]
fn results_for_a_session_that_is_not_removing_its_worktree_do_nothing() {
    let mut host = running();
    let found = inspected(
        &mut host,
        close_purpose(None, &[]),
        Ok(listed(Ok(clean_merged()))),
    );
    assert_eq!(found, vec![]);
    assert_eq!(hook_finished(&mut host, "a", &[], false, exited(0)), vec![]);
    assert_eq!(removed(&mut host, Ok(())), vec![]);
    // Nor once it has closed.
    let (mut host, _) = closing(CloseWorktree::IfSafe, &[]);
    removed(&mut host, Ok(()));
    assert_eq!(removed(&mut host, Ok(())), vec![]);
    let mut empty = HostDriver::new();
    assert_eq!(removed(&mut empty, Ok(())), vec![]);
}

#[test]
fn a_restart_closes_a_session_whose_removal_was_cut_short_keeping_its_worktree() {
    let (mut host, _) = closing(CloseWorktree::IfSafe, &[]);
    let commands = host.handle(HostEvent::Restarted);
    assert_eq!(
        commands,
        vec![closed(
            false,
            Some("the host stopped before the worktree was removed")
        )]
    );
    assert!(host.host().sessions().is_empty());
    assert_eq!(host.handle(HostEvent::Restarted), vec![]);
}

#[test]
fn removing_survives_replay() {
    let (host, _) = closing(CloseWorktree::IfSafe, &["make clean"]);
    assert_eq!(replay(&host), *host.host());
}

// ---------- asking for the status ----------

fn ask(host: &mut HostDriver, id: &str) -> Vec<HostCommand> {
    let reply = host.reply_to();
    host.handle(HostEvent::WorktreeStatus {
        reply,
        session: sid(id),
    })
}

#[test]
fn asking_for_a_worktrees_status_inspects_it_and_answers_what_it_finds() {
    let mut host = running();
    let commands = ask(&mut host, "s1");
    let [
        HostCommand::InspectWorktree {
            session,
            repo,
            path: at,
            purpose: purpose @ Inspect::Ask(reply),
        },
    ] = &commands[..]
    else {
        panic!("{commands:?}")
    };
    assert_eq!(
        (session, repo, at),
        (&sid("s1"), &RepoRef::new(REPO), &path())
    );
    let cases = [
        (
            Ok(listed(Ok(clean_merged()))),
            Ok(HostResult::WorktreeStatus(WorktreeStatusOk {
                status: Some(clean_merged()),
            })),
        ),
        (
            Ok(Inspection::Unlisted { exists: false }),
            Ok(HostResult::WorktreeStatus(WorktreeStatusOk {
                status: None,
            })),
        ),
        (
            Ok(Inspection::Listed {
                status: Err(WorktreeError::NotARepo(PathBuf::from(WORKTREE))),
                exists: false,
            }),
            Ok(HostResult::WorktreeStatus(WorktreeStatusOk {
                status: None,
            })),
        ),
        (
            Ok(Inspection::Unlisted { exists: true }),
            Err(WireError::InvalidParams(
                "/wt/s1 isn't a worktree shepherd manages".into(),
            )),
        ),
        (
            Err(WorktreeError::Timeout("status".into())),
            Err(WireError::InvalidParams("git status timed out".into())),
        ),
    ];
    for (found, answer) in cases {
        let answered = inspected(&mut host, purpose.clone(), found);
        assert_eq!(
            answered,
            vec![HostCommand::Reply {
                to: *reply,
                result: answer
            }]
        );
    }
    // Asking changes nothing.
    assert!(!host.host().sessions()[0].removing_worktree);
}

#[test]
fn asking_about_a_session_with_no_worktree_to_remove_is_refused() {
    let mut host = running();
    host.new_session("s2");
    for id in ["s2", "nope"] {
        let commands = ask(&mut host, id);
        assert!(
            matches!(
                &commands[..],
                [HostCommand::Reply {
                    result: Err(WireError::InvalidParams(_) | WireError::UnknownSession { .. }),
                    ..
                }]
            ),
            "{id}: {commands:?}"
        );
    }
}
