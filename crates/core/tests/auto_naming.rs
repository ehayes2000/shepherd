//! Sessions are named after their first user prompt unless the user named them (T24), through the public
//! `shepherd_core::host` API.

use std::path::PathBuf;

use serde_json::{Value, json};
use shepherd_core::acpp::{Dir, Frame, FrameDraft, Source, SourceKind};
use shepherd_core::host::naming::auto_name;
use shepherd_core::host::{Host, HostCommand, HostEvent, WorkspaceAsk};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::{BaseUsed, BranchName, Created, WorktreePath};
use shepherd_testkit::builders::{HostDriver, claude_session_spec};

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

fn auto_renamed(id: &str, name: &str) -> HostCommand {
    append(
        id,
        "_shepherd/session/renamed",
        json!({ "name": name, "auto": true }),
    )
}

fn names(host: &Host) -> Vec<String> {
    host.sessions().into_iter().map(|s| s.name).collect()
}

fn replay(driver: &HostDriver) -> Host {
    let mut host = Host::new();
    for env in driver.log() {
        host.apply(env);
    }
    host
}

/// A host with one Claude Code session, `s1`, still on its placeholder name.
fn claude_session() -> HostDriver {
    let mut host = HostDriver::new();
    host.new_session_with("s1", claude_session_spec());
    host
}

// ---------- the name a prompt gives ----------

#[test]
fn auto_names_are_the_first_meaningful_words_of_the_prompt() {
    let long = format!("a{}", "x".repeat(40));
    let cases: [(&str, &str); 19] = [
        (
            "Fix the login redirect bug on Safari",
            "fix-login-redirect-bug",
        ),
        ("Can you please add dark mode?", "add-dark-mode"),
        ("Refactor   SESSION fold!!!", "refactor-session-fold"),
        ("Issue 42: crash on start", "issue-42-crash-start"),
        // Contractions are one word, and stopwords once their apostrophe is gone.
        (
            "I'd like you to fix it, don't break tests",
            "fix-break-tests",
        ),
        ("I’m seeing flaky CI", "seeing-flaky-ci"),
        // Unicode letters are kept and lowercased; symbols are separators.
        ("Überprüfe die GRÖSSE", "überprüfe-die-grösse"),
        ("café crème brûlée", "café-crème-brûlée"),
        ("日本語のテスト", "日本語のテスト"),
        ("🚀 ship it", "ship"),
        // Paths and URLs aren't words.
        (
            "look at crates/core/src/session.rs and fix the panic",
            "look-fix-panic",
        ),
        ("see https://example.com/issue/42 for the bug", "see-bug"),
        ("C:\\Users\\me broke", "broke"),
        // Inline code is words; fenced code blocks are dropped.
        ("rename `foo_bar` to baz", "rename-foo-bar-baz"),
        (
            "Why does this fail?\n```rust\nfn main() { panic!() }\n```\nthe parser crashes",
            "fail-parser-crashes",
        ),
        ("~~~\nignored words here\n~~~\nreal task", "real-task"),
        // At most 30 characters: words stop at the first that doesn't fit.
        (
            "internationalization localization configuration",
            "internationalization",
        ),
        (
            "make the widget frobnicator extraordinarily fast",
            "make-widget-frobnicator",
        ),
        // A first word longer than 30 characters is cut.
        (&long, &long[..30]),
    ];
    for (prompt, expected) in cases {
        assert_eq!(auto_name(prompt).as_deref(), Some(expected), "{prompt:?}");
    }
}

#[test]
fn prompts_with_no_meaningful_words_give_no_name() {
    let cases = [
        "",
        "   \n\t",
        "!!! ???",
        "please can you help me?",
        "hi",
        "```\nonly code here\n```",
        "```\nan unterminated fence",
        "/usr/bin/env ./run.sh",
        "🚀🔥",
    ];
    for prompt in cases {
        assert_eq!(auto_name(prompt), None, "{prompt:?}");
    }
}

// ---------- when the host renames ----------

#[test]
fn a_new_session_keeps_its_placeholder_until_a_prompt() {
    let mut host = claude_session();
    let cmds = host.hook_translated("s1");
    assert_eq!(cmds, vec![]);
    assert_eq!(names(host.host()), ["claude-1"]);
}

#[test]
fn the_first_user_prompt_auto_renames_the_session() {
    let mut host = claude_session();
    let cmds = host.user_prompt("s1", "Fix the login redirect bug");
    assert_eq!(cmds, vec![auto_renamed("s1", "fix-login-redirect-bug")]);
    assert_eq!(names(host.host()), ["fix-login-redirect-bug"]);
}

#[test]
fn a_session_is_auto_renamed_only_once() {
    let mut host = claude_session();
    host.user_prompt("s1", "Fix the login redirect bug");
    assert_eq!(host.user_prompt("s1", "now add tests"), vec![]);
    assert_eq!(host.hook_translated("s1"), vec![]);
    assert_eq!(names(host.host()), ["fix-login-redirect-bug"]);
}

#[test]
fn only_the_first_prompt_counts_even_when_it_gives_no_name() {
    let mut host = claude_session();
    assert_eq!(host.user_prompt("s1", "hi"), vec![]);
    assert_eq!(host.user_prompt("s1", "Fix the login redirect bug"), vec![]);
    assert_eq!(names(host.host()), ["claude-1"]);
}

#[test]
fn a_continuation_prompt_is_not_the_first_user_prompt() {
    let mut host = claude_session();
    host.prompt_frame("s1", "Keep going with the refactor", true);
    assert_eq!(host.hook_translated("s1"), vec![]);
    let cmds = host.user_prompt("s1", "Fix the login redirect bug");
    assert_eq!(cmds, vec![auto_renamed("s1", "fix-login-redirect-bug")]);
}

#[test]
fn a_manual_rename_before_the_first_prompt_is_never_overridden() {
    let mut host = claude_session();
    host.rename("s1", "api");
    assert_eq!(host.user_prompt("s1", "Fix the login redirect bug"), vec![]);
    assert_eq!(names(host.host()), ["api"]);
}

#[test]
fn a_manual_rename_after_an_auto_rename_wins_and_stays() {
    let mut host = claude_session();
    host.user_prompt("s1", "Fix the login redirect bug");
    let cmds = host.rename("s1", "api");
    assert_eq!(
        cmds[0],
        append(
            "s1",
            "_shepherd/session/renamed",
            json!({ "name": "api", "auto": false })
        )
    );
    assert_eq!(host.user_prompt("s1", "Something else entirely"), vec![]);
    assert_eq!(names(host.host()), ["api"]);
}

#[test]
fn a_closed_session_is_not_auto_renamed() {
    let mut host = claude_session();
    host.close("s1");
    host.prompt_frame("s1", "Fix the login redirect bug", false);
    assert_eq!(host.hook_translated("s1"), vec![]);
}

#[test]
fn an_unknown_session_is_not_auto_renamed() {
    let mut host = claude_session();
    assert_eq!(host.hook_translated("nope"), vec![]);
}

// ---------- decided from the log ----------

#[test]
fn a_restarted_host_does_not_rename_again() {
    let mut host = claude_session();
    host.user_prompt("s1", "Fix the login redirect bug");
    let restarted = replay(&host);
    assert_eq!(names(&restarted), ["fix-login-redirect-bug"]);
    let cmds = restarted.handle(HostEvent::HookTranslated { session: sid("s1") });
    assert_eq!(cmds, vec![]);
}

#[test]
fn a_restarted_host_names_a_session_whose_prompt_it_never_named() {
    let mut host = claude_session();
    // The host stopped after appending the prompt, before handling what came of it.
    host.prompt_frame("s1", "Fix the login redirect bug", false);
    let restarted = replay(&host);
    let cmds = restarted.handle(HostEvent::HookTranslated { session: sid("s1") });
    assert_eq!(cmds, vec![auto_renamed("s1", "fix-login-redirect-bug")]);
}

#[test]
fn replaying_the_log_keeps_the_auto_name() {
    let mut host = claude_session();
    host.user_prompt("s1", "Fix the login redirect bug");
    host.pty_exited("s1", Some(0));
    assert_eq!(replay(&host), *host.host());
}

// ---------- only the display name ----------

#[test]
fn auto_renaming_a_worktree_session_leaves_its_branch_alone() {
    let mut host = HostDriver::new();
    let ask = WorkspaceAsk::Select(host.reply_to());
    host.handle(HostEvent::WorkspaceProbed {
        ask,
        repo: Ok(PathBuf::from("/src/app")),
    });
    host.new_session_with("s1", claude_session_spec());
    let spawn = claude_session_spec().spawn;
    host.handle(HostEvent::WorktreeCreated {
        session: sid("s1"),
        spawn,
        hooks: Default::default(),
        result: Ok(Created {
            path: WorktreePath::new("/wt/app/claude-1"),
            branch: BranchName::new("claude-1"),
            base: BaseUsed::Origin {
                branch: BranchName::new("main"),
            },
        }),
    });
    // Only the renamed frame: nothing touches the worktree or its branch.
    let cmds = host.user_prompt("s1", "Fix the login redirect bug");
    assert_eq!(cmds, vec![auto_renamed("s1", "fix-login-redirect-bug")]);
    // The old branch is still taken: a session named like it gets a suffixed branch.
    let cmds = host.new_session_with(
        "s2",
        shepherd_core::host::NewSessionSpec {
            name: Some("claude-1".to_owned()),
            ..claude_session_spec()
        },
    );
    assert!(
        matches!(&cmds[1], HostCommand::CreateWorktree { name, .. } if name.as_str() == "claude-1-2"),
        "{cmds:?}"
    );
}
