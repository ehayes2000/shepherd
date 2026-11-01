//! Reading a session's screen (T52): the host answers only for a running session, which has a screen, and never reads
//! more scrollback than [`SCREEN_LINES_MAX`]. Through the public `shepherd_core::host` API.

use std::path::PathBuf;

use shepherd_core::host::{HostCommand, HostEvent, ReplyTo, WorkspaceAsk};
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{SCREEN_LINES_MAX, WireError};
use shepherd_testkit::builders::HostDriver;

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn read(host: &mut HostDriver, session: &str, lines: Option<u32>) -> (ReplyTo, Vec<HostCommand>) {
    let reply = host.reply_to();
    let commands = host.handle(HostEvent::ReadScreen {
        reply,
        session: sid(session),
        lines,
    });
    (reply, commands)
}

fn refusal(commands: &[HostCommand], to: ReplyTo) -> Option<&WireError> {
    commands.iter().find_map(|c| match c {
        HostCommand::Reply {
            to: t,
            result: Err(e),
        } if *t == to => Some(e),
        _ => None,
    })
}

#[test]
fn a_running_sessions_screen_is_read_with_the_scrollback_asked_for() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    let (reply, commands) = read(&mut host, "s1", Some(40));
    assert_eq!(
        commands,
        [HostCommand::ReadScreen {
            reply,
            session: sid("s1"),
            scrollback: 40,
        }]
    );
    let (reply, commands) = read(&mut host, "s1", None);
    assert_eq!(
        commands,
        [HostCommand::ReadScreen {
            reply,
            session: sid("s1"),
            scrollback: 0,
        }]
    );
}

#[test]
fn scrollback_asked_for_past_the_cap_is_capped() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    for asked in [SCREEN_LINES_MAX + 1, u32::MAX] {
        let (reply, commands) = read(&mut host, "s1", Some(asked));
        assert_eq!(
            commands,
            [HostCommand::ReadScreen {
                reply,
                session: sid("s1"),
                scrollback: SCREEN_LINES_MAX,
            }]
        );
    }
}

#[test]
fn an_unknown_or_closed_session_is_refused_as_unknown() {
    let mut host = HostDriver::new();
    let (reply, commands) = read(&mut host, "nope", None);
    assert_eq!(
        refusal(&commands, reply),
        Some(&WireError::UnknownSession {
            session: sid("nope")
        })
    );
    host.new_session("s1");
    host.close("s1");
    let (reply, commands) = read(&mut host, "s1", None);
    assert_eq!(
        refusal(&commands, reply),
        Some(&WireError::UnknownSession { session: sid("s1") })
    );
}

#[test]
fn a_session_without_a_terminal_is_refused_as_not_running() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    host.pty_exited("s1", Some(0));
    let (reply, commands) = read(&mut host, "s1", None);
    assert!(
        matches!(refusal(&commands, reply), Some(WireError::InvalidParams(why)) if why.contains("isn't running")),
        "{commands:?}"
    );
    // One still waiting for its worktree has no terminal yet.
    let ask = WorkspaceAsk::Select(host.reply_to());
    host.handle(HostEvent::WorkspaceProbed {
        ask,
        repo: Ok(PathBuf::from("/src/app")),
    });
    host.new_session("s2");
    let (reply, commands) = read(&mut host, "s2", None);
    assert!(
        matches!(refusal(&commands, reply), Some(WireError::InvalidParams(why)) if why.contains("isn't running")),
        "{commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, HostCommand::ReadScreen { .. }))
    );
}
