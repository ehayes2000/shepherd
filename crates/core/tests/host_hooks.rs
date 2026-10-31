//! A new worktree runs its workspace's post-create hooks before the agent starts (T23), through the public
//! `shepherd_core::host` API.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, Source, SourceKind};
use shepherd_core::config::WorkspaceHooks;
use shepherd_core::host::{
    Host, HostCommand, HostEvent, Inspect, Inspection, NewSessionSpec, PostCreate, PreRemove,
    Remover, ResumeUnavailable,
};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::pty::SpawnSpec;
use shepherd_core::ports::{
    BaseUsed, BranchName, CommandError, CommandExit, CommandOutput, CommandSpec, Created,
    OUTPUT_TAIL, RepoRef, WorktreePath, WorktreeStatus,
};
use shepherd_core::wire::{CloseWorktree, InputKind};
use shepherd_testkit::builders::{HostDriver, shell_session_spec, shell_spawn};

const WORKTREE: &str = "/wt/app/shell-1";
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

fn post_create(hooks: &[&str]) -> PostCreate {
    PostCreate {
        hooks: hooks.iter().map(|h| (*h).to_owned()).collect(),
        timeout: TIMEOUT,
    }
}

/// A shell session whose workspace has `hooks` as its post-create hooks.
fn spec_with(hooks: &[&str]) -> NewSessionSpec {
    NewSessionSpec {
        hooks: WorkspaceHooks {
            post_create: hooks.iter().map(|h| (*h).to_owned()).collect(),
            pre_remove: vec!["make clean".to_owned()],
            timeout: TIMEOUT,
        },
        ..shell_session_spec()
    }
}

/// What spawns once the hooks are done: the shell, in the worktree.
fn spawn_in_worktree() -> SpawnSpec {
    SpawnSpec {
        cwd: PathBuf::from(WORKTREE),
        ..shell_spawn()
    }
}

/// `script` as it runs in session `s1`'s worktree.
fn hook(script: &str) -> CommandSpec {
    CommandSpec {
        script: script.to_owned(),
        cwd: PathBuf::from(WORKTREE),
        env: vec![
            ("SHEPHERD_SESSION".to_owned(), "s1".to_owned()),
            ("SHEPHERD_WORKTREE".to_owned(), WORKTREE.to_owned()),
            ("SHEPHERD_BRANCH".to_owned(), "shell-1".to_owned()),
            ("SHEPHERD_REPO".to_owned(), "/src/app".to_owned()),
        ],
        timeout: TIMEOUT,
    }
}

fn run_hook(script: &str, rest: &[&str]) -> HostCommand {
    HostCommand::RunHook {
        session: sid("s1"),
        command: hook(script),
        rest: post_create(rest),
        spawn: spawn_in_worktree(),
    }
}

/// A host with `/src/app` selected, session `s1` created there with `hooks`, and its worktree made at
/// [`WORKTREE`]. Returns the host and the commands of the worktree result.
fn worktree_made(hooks: &[&str]) -> (HostDriver, Vec<HostCommand>) {
    let mut host = HostDriver::new();
    host.select_workspace("/src/app");
    let created = host.new_session_with("s1", spec_with(hooks));
    let carried = created.iter().find_map(|c| match c {
        HostCommand::CreateWorktree { hooks, .. } => Some(hooks.clone()),
        _ => None,
    });
    assert_eq!(carried, Some(post_create(hooks)));
    let commands = host.handle(HostEvent::WorktreeCreated {
        session: sid("s1"),
        spawn: shell_spawn(),
        hooks: post_create(hooks),
        result: Ok(Created {
            path: WorktreePath::new(WORKTREE),
            branch: BranchName::new("shell-1"),
            base: BaseUsed::Origin {
                branch: BranchName::new("main"),
            },
            base_commit: None,
        }),
    });
    (host, commands)
}

fn exited(code: i32, tail: &[u8]) -> Result<CommandOutput, CommandError> {
    Ok(CommandOutput {
        exit: CommandExit::Code(code),
        tail: tail.to_vec(),
    })
}

/// The result of `script` (the first of `[script, rest..]`) for session `s1`.
fn finished(
    host: &mut HostDriver,
    script: &str,
    rest: &[&str],
    result: Result<CommandOutput, CommandError>,
) -> Vec<HostCommand> {
    host.handle(HostEvent::HookFinished {
        session: sid("s1"),
        command: hook(script),
        rest: post_create(rest),
        spawn: spawn_in_worktree(),
        result,
    })
}

fn replay(driver: &HostDriver) -> Host {
    let mut host = Host::new();
    for env in driver.log() {
        host.apply(env);
    }
    host
}

#[test]
fn a_new_worktree_with_hooks_records_them_and_runs_the_first_in_it_instead_of_spawning() {
    let (_, commands) = worktree_made(&["cp ../.envrc .", "npm ci"]);
    assert_eq!(
        commands,
        vec![
            append(
                "s1",
                "_shepherd/worktree/created",
                json!({
                    "path": WORKTREE, "branch": "shell-1", "base": "origin/main",
                    "post_create": ["cp ../.envrc .", "npm ci"]
                })
            ),
            run_hook("cp ../.envrc .", &["npm ci"]),
        ]
    );
}

#[test]
fn a_new_worktree_with_no_hooks_spawns_at_once() {
    let (_, commands) = worktree_made(&[]);
    assert_eq!(
        commands,
        vec![
            append(
                "s1",
                "_shepherd/worktree/created",
                json!({ "path": WORKTREE, "branch": "shell-1", "base": "origin/main" })
            ),
            HostCommand::SpawnPty {
                session: sid("s1"),
                spec: spawn_in_worktree(),
            },
        ]
    );
}

#[test]
fn a_session_running_its_hooks_is_starting_and_has_no_pty() {
    let (mut host, _) = worktree_made(&["npm ci"]);
    let listed = &host.host().sessions()[0];
    assert!(listed.creating_worktree);
    assert!(!listed.exited);
    assert_eq!(listed.resume, Err(ResumeUnavailable::Starting));
    assert_eq!(listed.warning, None);
    let input = host.handle(HostEvent::Input {
        session: sid("s1"),
        bytes: b"ls\n".to_vec(),
        kind: InputKind::Keys,
    });
    assert_eq!(input, vec![]);
    let close = host.close("s1");
    assert!(
        !close
            .iter()
            .any(|c| matches!(c, HostCommand::KillPty { .. })),
        "{close:?}"
    );
}

#[test]
fn each_hook_that_succeeds_is_recorded_and_the_next_runs_then_the_pty_spawns() {
    let (mut host, _) = worktree_made(&["a", "b"]);
    let first = finished(&mut host, "a", &["b"], exited(0, b"copied\n"));
    assert_eq!(
        first,
        vec![
            append(
                "s1",
                "_shepherd/worktree/hook",
                json!({ "hook": "a", "exit_code": 0, "output_tail": "copied\n" })
            ),
            run_hook("b", &[]),
        ]
    );
    assert!(host.host().sessions()[0].creating_worktree);
    let last = finished(&mut host, "b", &[], exited(0, b""));
    assert_eq!(
        last,
        vec![
            append(
                "s1",
                "_shepherd/worktree/hook",
                json!({ "hook": "b", "exit_code": 0, "output_tail": "" })
            ),
            HostCommand::SpawnPty {
                session: sid("s1"),
                spec: spawn_in_worktree(),
            },
        ]
    );
    let listed = &host.host().sessions()[0];
    assert!(!listed.creating_worktree);
    assert_eq!(listed.warning, None);
    assert_eq!(listed.resume, Err(ResumeUnavailable::Running));
}

#[test]
fn the_first_failing_hook_stops_the_rest_warns_and_still_spawns_the_agent() {
    let cases: [(Result<CommandOutput, CommandError>, Value, &str); 5] = [
        (
            exited(1, b"npm ERR!\n"),
            json!({ "hook": "a", "exit_code": 1, "output_tail": "npm ERR!\n" }),
            "hook failed (exit 1): a",
        ),
        (
            Ok(CommandOutput {
                exit: CommandExit::TimedOut,
                tail: b"installing".to_vec(),
            }),
            json!({ "hook": "a", "output_tail": "installing", "error": "timed out after 30s" }),
            "hook failed (timed out after 30s): a",
        ),
        (
            Ok(CommandOutput {
                exit: CommandExit::Signal(9),
                tail: Vec::new(),
            }),
            json!({ "hook": "a", "output_tail": "", "error": "killed by signal 9" }),
            "hook failed (killed by signal 9): a",
        ),
        (
            Err(CommandError::Failed(
                "cannot run /bin/sh: no such file".to_owned(),
            )),
            json!({ "hook": "a", "output_tail": "", "error": "cannot run /bin/sh: no such file" }),
            "hook failed (cannot run /bin/sh: no such file): a",
        ),
        (
            Err(CommandError::NoSuchDir(PathBuf::from(WORKTREE))),
            json!({
                "hook": "a", "output_tail": "",
                "error": "/wt/app/shell-1 is not an existing directory"
            }),
            "hook failed (/wt/app/shell-1 is not an existing directory): a",
        ),
    ];
    for (result, frame, warning) in cases {
        let (mut host, _) = worktree_made(&["a", "b"]);
        let commands = finished(&mut host, "a", &["b"], result);
        assert_eq!(
            commands,
            vec![
                append("s1", "_shepherd/worktree/hook", frame),
                HostCommand::SpawnPty {
                    session: sid("s1"),
                    spec: spawn_in_worktree(),
                },
            ],
            "{warning}"
        );
        let listed = &host.host().sessions()[0];
        assert_eq!(listed.warning.as_deref(), Some(warning));
        assert!(!listed.creating_worktree);
        assert_eq!(listed.failed, None);
        // The warning is in the log, not just in this host's memory.
        assert_eq!(replay(&host).sessions(), host.host().sessions());
    }
}

#[test]
fn a_session_closed_while_its_hooks_run_records_the_result_and_runs_and_spawns_nothing_more() {
    let (mut host, _) = worktree_made(&["a", "b"]);
    host.close("s1");
    let commands = finished(&mut host, "a", &["b"], exited(0, b"ok"));
    assert_eq!(
        commands,
        vec![append(
            "s1",
            "_shepherd/worktree/hook",
            json!({ "hook": "a", "exit_code": 0, "output_tail": "ok" })
        )]
    );
    // Still closing: its worktree is dealt with like any closing session's.
    let listed = &host.host().sessions()[0];
    assert!(listed.removing_worktree);
    assert!(!listed.creating_worktree);
    assert_eq!(listed.resume, Err(ResumeUnavailable::Closing));
}

/// F33: the hook would otherwise go on running (up to its timeout) for a session nobody wants any more. The close then
/// deals with the worktree like any close (T25): the runtime inspects it once the cancelled hook is gone.
#[test]
fn closing_a_session_while_its_hooks_run_cancels_the_running_hook_and_inspects_its_worktree() {
    let (mut host, _) = worktree_made(&["a", "b"]);
    let pre_remove = WorkspaceHooks {
        post_create: Vec::new(),
        pre_remove: vec!["make clean".to_owned()],
        timeout: TIMEOUT,
    };
    let commands = host.close_with("s1", CloseWorktree::IfSafe, pre_remove);
    assert_eq!(
        commands[..3],
        [
            HostCommand::CancelHook { session: sid("s1") },
            append(
                "s1",
                "_shepherd/worktree/removing",
                json!({ "path": WORKTREE, "pre_remove": ["make clean"] })
            ),
            HostCommand::InspectWorktree {
                remover: Remover::Session(sid("s1")),
                repo: RepoRef::new("/src/app"),
                path: WorktreePath::new(WORKTREE),
                purpose: Inspect::Close {
                    confirmed: None,
                    hooks: PreRemove {
                        hooks: vec!["make clean".to_owned()],
                        timeout: TIMEOUT,
                    },
                },
            },
        ]
    );
    assert!(!commands.iter().any(|c| matches!(
        c,
        HostCommand::KillPty { .. } | HostCommand::CancelWorktree { .. }
    )));
}

/// A half-run hook usually leaves untracked files: the worktree is kept, and the close says why.
#[test]
fn a_session_closed_mid_hook_keeps_a_worktree_the_hook_left_files_in_and_says_why() {
    let (mut host, _) = worktree_made(&["a", "b"]);
    host.close("s1");
    let cancelled = Ok(CommandOutput {
        exit: CommandExit::Cancelled,
        tail: Vec::new(),
    });
    finished(&mut host, "a", &["b"], cancelled);
    let dirty = WorktreeStatus {
        dirty: true,
        ahead: 0,
        pushed: false,
        merged_into_default: true,
    };
    let commands = host.handle(HostEvent::WorktreeInspected {
        remover: Remover::Session(sid("s1")),
        path: WorktreePath::new(WORKTREE),
        purpose: Inspect::Close {
            confirmed: None,
            hooks: PreRemove {
                hooks: Vec::new(),
                timeout: TIMEOUT,
            },
        },
        result: Ok(Inspection::Listed {
            status: Ok(dirty),
            exists: true,
            branch: None,
        }),
    });
    assert_eq!(
        commands,
        [append(
            "s1",
            "_shepherd/session/closed",
            json!({
                "worktree_removed": false,
                "worktree_kept": "it has uncommitted changes"
            })
        )]
    );
    assert!(host.host().sessions().is_empty());
}

/// A cancelled hook that left nothing behind: the worktree is removed, as for any close that loses nothing.
#[test]
fn a_session_closed_mid_hook_removes_a_worktree_that_loses_nothing() {
    let (mut host, _) = worktree_made(&["a"]);
    host.close("s1");
    let cancelled = Ok(CommandOutput {
        exit: CommandExit::Cancelled,
        tail: Vec::new(),
    });
    finished(&mut host, "a", &[], cancelled);
    let clean = WorktreeStatus {
        dirty: false,
        ahead: 0,
        pushed: false,
        merged_into_default: true,
    };
    let commands = host.handle(HostEvent::WorktreeInspected {
        remover: Remover::Session(sid("s1")),
        path: WorktreePath::new(WORKTREE),
        purpose: Inspect::Close {
            confirmed: None,
            hooks: PreRemove {
                hooks: Vec::new(),
                timeout: TIMEOUT,
            },
        },
        result: Ok(Inspection::Listed {
            status: Ok(clean),
            exists: true,
            branch: None,
        }),
    });
    assert_eq!(
        commands,
        [HostCommand::RemoveWorktree {
            remover: Remover::Session(sid("s1")),
            path: WorktreePath::new(WORKTREE),
            force: false,
        }]
    );
}

#[test]
fn a_cancelled_hook_is_recorded_as_cancelled_with_its_output() {
    let (mut host, _) = worktree_made(&["a", "b"]);
    host.close("s1");
    let cancelled = Ok(CommandOutput {
        exit: CommandExit::Cancelled,
        tail: b"installing".to_vec(),
    });
    let commands = finished(&mut host, "a", &["b"], cancelled);
    assert_eq!(
        commands,
        vec![append(
            "s1",
            "_shepherd/worktree/hook",
            json!({ "hook": "a", "output_tail": "installing", "error": "cancelled" })
        )]
    );
}

#[test]
fn a_hook_result_for_a_session_not_running_hooks_does_nothing() {
    let (mut host, _) = worktree_made(&[]);
    assert_eq!(finished(&mut host, "a", &[], exited(0, b"")), vec![]);
    let mut empty = HostDriver::new();
    assert_eq!(finished(&mut empty, "a", &[], exited(0, b"")), vec![]);
}

#[test]
fn hook_output_is_decoded_lossily_with_control_characters_stripped() {
    let (mut host, _) = worktree_made(&["a"]);
    let tail = b"\x1b[31mred\x1b[0m\r\nok\tdone\x07\xff";
    let commands = finished(&mut host, "a", &[], exited(1, tail));
    assert_eq!(
        commands[0],
        append(
            "s1",
            "_shepherd/worktree/hook",
            json!({ "hook": "a", "exit_code": 1, "output_tail": "[31mred[0m\nok\tdone\u{FFFD}" })
        )
    );
}

#[test]
fn a_full_tail_cut_inside_a_character_drops_the_partial_character() {
    let (mut host, _) = worktree_made(&["a"]);
    // `é` is two bytes; the tail starts at its second.
    let mut tail = "é".as_bytes()[1..].to_vec();
    tail.resize(OUTPUT_TAIL, b'x');
    let commands = finished(&mut host, "a", &[], exited(0, &tail));
    let expected = "x".repeat(OUTPUT_TAIL - 1);
    assert_eq!(
        commands[0],
        append(
            "s1",
            "_shepherd/worktree/hook",
            json!({ "hook": "a", "exit_code": 0, "output_tail": expected })
        )
    );
}

#[test]
fn a_host_restarted_while_hooks_ran_fails_the_session() {
    let (mut host, _) = worktree_made(&["a"]);
    let commands = host.pty_exited("s1", None);
    assert_eq!(
        commands,
        vec![append(
            "s1",
            "_shepherd/session/failed",
            json!({ "reason": "the host stopped before the worktree hooks finished" })
        )]
    );
    let listed = &host.host().sessions()[0];
    assert_eq!(
        listed.failed.as_deref(),
        Some("the host stopped before the worktree hooks finished")
    );
    assert_eq!(listed.resume, Err(ResumeUnavailable::Failed));
}

#[test]
fn a_session_with_an_explicit_cwd_gets_no_worktree_and_runs_no_hooks() {
    let mut host = HostDriver::new();
    host.select_workspace("/src/app");
    let spec = NewSessionSpec {
        explicit_cwd: true,
        ..spec_with(&["a"])
    };
    let commands = host.new_session_with("s1", spec);
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, HostCommand::SpawnPty { .. })),
        "{commands:?}"
    );
    assert!(
        !commands.iter().any(|c| matches!(
            c,
            HostCommand::CreateWorktree { .. } | HostCommand::RunHook { .. }
        )),
        "{commands:?}"
    );
}
