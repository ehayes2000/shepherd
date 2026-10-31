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
        // Inline code is words (a snake_case identifier is one word, UAX #29); fenced code blocks are dropped.
        ("rename `foo_bar` to baz", "rename-foo_bar-baz"),
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
fn auto_names_are_cut_at_30_characters_between_words() {
    let cases: [(&str, &str); 3] = [
        // Exactly 30 fits.
        (
            "aaaaaaaaaa bbbbbbbbbb cccccccc",
            "aaaaaaaaaa-bbbbbbbbbb-cccccccc",
        ),
        // 31 doesn't: the last word goes.
        ("aaaaaaaaaa bbbbbbbbbb ccccccccc", "aaaaaaaaaa-bbbbbbbbbb"),
        // It stops at the first word that doesn't fit, though a shorter later one would, and fewer than 4 words.
        (
            "fix internationalization subsystem ui",
            "fix-internationalization",
        ),
    ];
    for (prompt, expected) in cases {
        assert_eq!(auto_name(prompt).as_deref(), Some(expected), "{prompt:?}");
    }
}

#[test]
fn indented_fences_still_drop_their_code() {
    let cases: [(&str, &str); 2] = [
        (
            "Fix this:\n    ```\n    let x = 1;\n    ```\n  then deploy",
            "fix-deploy",
        ),
        ("Rename it\n  ~~~\n  old code\n  ~~~\n", "rename"),
    ];
    for (prompt, expected) in cases {
        assert_eq!(auto_name(prompt).as_deref(), Some(expected), "{prompt:?}");
    }
}

#[test]
fn words_follow_unicode_segmentation_in_nfc_and_cuts_keep_graphemes_whole() {
    // `x́` has no precomposed form, so it stays two chars in one grapheme cluster.
    let cut = format!("{}x\u{301}z", "a".repeat(29));
    let cut_expected = format!("{}x\u{301}", "a".repeat(29));
    let cases: [(&str, &str); 4] = [
        // Decomposed accents stay in their word, and are composed.
        ("cre\u{300}me bru\u{302}le\u{301}e", "crème-brûlée"),
        // Devanagari vowel signs and viramas stay in their word.
        ("नमस्ते दुनिया", "नमस्ते-दुनिया"),
        // 30 grapheme clusters, the last `x́`: the cut doesn't strip its accent.
        (&cut, &cut_expected),
        // Words count grapheme clusters, not chars: 30 of them (31 chars) still fit.
        (
            "aaaaaaaaaa bbbbbbbbbb cccccccx\u{301}",
            "aaaaaaaaaa-bbbbbbbbbb-cccccccx\u{301}",
        ),
    ];
    for (prompt, expected) in cases {
        assert_eq!(auto_name(prompt).as_deref(), Some(expected), "{prompt:?}");
    }
}

#[test]
fn invisible_format_characters_are_dropped_from_words() {
    let cases: [(&str, &str); 7] = [
        // Bidi override, isolates, and marks.
        ("fix \u{202E}login bug", "fix-login-bug"),
        ("\u{2066}deploy\u{2069} staging", "deploy-staging"),
        ("fix\u{200E} it", "fix"),
        // Joiners, soft hyphen, BOM, tags.
        ("a\u{200D}b\u{200C}c test", "abc-test"),
        ("co\u{AD}operate nicely", "cooperate-nicely"),
        ("\u{FEFF}hello world", "world"),
        ("tag\u{E0041}\u{E007F}ged run", "tagged-run"),
    ];
    for (prompt, expected) in cases {
        assert_eq!(auto_name(prompt).as_deref(), Some(expected), "{prompt:?}");
    }
}

#[test]
fn auto_names_are_bounded_in_bytes_and_combining_marks() {
    // At most two combining marks stay on each base.
    let zalgo = format!("a{}", "\u{301}".repeat(100_000));
    let name = auto_name(&zalgo).unwrap_or_default();
    assert!(name.len() <= 120, "{} bytes", name.len());
    assert_eq!(name, "á\u{301}\u{301}");
    // Also for combining marks that are alphabetic, like the Greek ypogegrammeni.
    let alphabetic = format!("a{}", "\u{345}".repeat(1_000));
    assert_eq!(auto_name(&alphabetic).as_deref(), Some("a\u{345}\u{345}"));
    // 30 graphemes of 5 bytes each are 150 bytes: cut to the 24 that fit in 120.
    let heavy = "x\u{301}\u{302}".repeat(30);
    assert_eq!(auto_name(&heavy), Some("x\u{301}\u{302}".repeat(24)));
    // A later word that would pass 120 bytes doesn't fit, though it is within 30 graphemes (4 × 6 + 3 = 27).
    let word = "x\u{301}\u{302}".repeat(6);
    assert_eq!(
        auto_name(&format!("{word} {word} {word} {word}")),
        Some(format!("{word}-{word}-{word}"))
    );
}

#[test]
fn a_first_word_with_no_cluster_that_fits_is_dropped_and_the_name_never_starts_with_a_dash() {
    // Viramas join Devanagari consonants into one grapheme cluster (UAX #29 GB9c): 40 of them are 240 bytes, more
    // than 120, so not even the first cluster of the first word fits.
    let cluster = "\u{915}\u{94d}".repeat(40);
    assert_eq!(cluster.len(), 240);
    assert_eq!(
        auto_name(&format!("{cluster} fix login bug")).as_deref(),
        Some("fix-login-bug")
    );
    assert_eq!(auto_name(&cluster), None);
}

#[test]
fn a_later_word_fits_at_exactly_120_bytes_and_not_at_121() {
    // `𝐀́̂` is one 8-byte grapheme cluster (a 4-byte letter with no case and no precomposed accents).
    let heavy = "\u{1D400}\u{301}\u{302}";
    let first = heavy.repeat(7); // 56 bytes, 7 graphemes
    let second = |ascii: usize| format!("{}{}", heavy.repeat(7), "b".repeat(ascii));
    // 56 + 1 + 63 = 120 bytes in 22 graphemes: fits.
    let exact = format!("{first}-{}", second(7));
    assert_eq!(exact.len(), 120);
    assert_eq!(
        auto_name(&format!("{first} {}", second(7))).as_deref(),
        Some(exact.as_str())
    );
    // One more byte (and grapheme, 23 still under 30) is 121: the second word goes.
    assert_eq!(
        auto_name(&format!("{first} {}", second(8))).as_deref(),
        Some(first.as_str())
    );
}

#[test]
fn combining_marks_never_follow_in_word_punctuation() {
    let cases: [(&str, &str); 3] = [
        ("v3.\u{301}14 release", "v3.14-release"),
        ("foo_\u{301}\u{302}bar", "foo_bar"),
        ("a:\u{20DD}b", "a:b"),
    ];
    for (prompt, expected) in cases {
        assert_eq!(auto_name(prompt).as_deref(), Some(expected), "{prompt:?}");
    }
}

#[test]
fn in_word_punctuation_is_kept_inside_and_trimmed_after_a_cut() {
    let cut = format!("{}_bc", "a".repeat(29));
    let cases: [(&str, &str); 3] = [
        ("bump version 3.14", "bump-version-3.14"),
        ("rename _private_ helper", "rename-private-helper"),
        // Cut to 30, `aaa…a_`, then the trailing `_` goes.
        (&cut, &cut[..29]),
    ];
    for (prompt, expected) in cases {
        assert_eq!(auto_name(prompt).as_deref(), Some(expected), "{prompt:?}");
    }
}

/// What an auto-name may hold: letters, digits and combining marks, the `-` between words, and the punctuation
/// UAX #29 keeps inside a word.
fn displayable(c: char) -> bool {
    c.is_alphanumeric()
        || unicode_normalization::char::is_combining_mark(c)
        || matches!(c, '-' | '.' | ':' | ',' | '_')
}

#[test]
fn auto_names_never_hold_control_characters() {
    // The printable remains of an escape sequence (`31m`, `0m`, `9`) can be words; the controls never are.
    let cases: [(&str, Option<&str>); 6] = [
        ("fix \u{202E}evil\u{202C} \u{2067}bug", Some("fix-evil-bug")),
        ("\u{FFF9}note\u{FFFB} \u{61C}arabic", Some("note-arabic")),
        (
            "\x1b[31mFix\x1b[0m the \x1b]9;evil\x07 bug",
            Some("31mfix-0m-9-evil"),
        ),
        // The OSC ends in `ESC \`, so its token is a path; `hello` is a stopword.
        ("\x1b]0;t\x1b\\ hello", None),
        ("\x1b]0;title\x07 deploy", Some("0-title-deploy")),
        (
            "tab\there\u{0}nul\u{7f}del\r\nnext",
            Some("tab-nul-del-next"),
        ),
    ];
    for (prompt, expected) in cases {
        let name = auto_name(prompt);
        assert_eq!(name.as_deref(), expected, "{prompt:?}");
        assert!(
            name.unwrap_or_default().chars().all(displayable),
            "{prompt:?}"
        );
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

/// Appends a hook-sourced `session/prompt` frame with these `params` (and `id`, if any) to session `id`'s log.
fn raw_prompt(host: &mut HostDriver, session: &str, request: Option<&str>, params: Value) {
    let mut frame = json!({ "jsonrpc": "2.0", "method": "session/prompt", "params": params });
    if let Some(request) = request {
        frame["id"] = json!(request);
    }
    let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
    host.append(Some(session), source, frame);
}

#[test]
fn a_prompts_text_blocks_are_joined_by_newlines_and_other_blocks_skipped() {
    let mut host = claude_session();
    let params = json!({
        "sessionId": "c1",
        "prompt": [
            { "type": "text", "text": "Fix the" },
            { "type": "image", "mimeType": "image/png", "data": "AAAA" },
            { "type": "text", "text": "login bug" },
        ],
    });
    raw_prompt(&mut host, "s1", Some("p1"), params);
    assert_eq!(
        host.hook_translated("s1"),
        vec![auto_renamed("s1", "fix-login-bug")]
    );
}

#[test]
fn a_prompt_without_a_request_id_is_not_the_first_user_prompt() {
    let mut host = claude_session();
    let params =
        json!({ "sessionId": "c1", "prompt": [{ "type": "text", "text": "Deploy staging" }] });
    raw_prompt(&mut host, "s1", None, params);
    assert_eq!(host.hook_translated("s1"), vec![]);
    let cmds = host.user_prompt("s1", "Fix the login redirect bug");
    assert_eq!(cmds, vec![auto_renamed("s1", "fix-login-redirect-bug")]);
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
fn a_session_created_with_a_name_is_never_auto_renamed() {
    let mut host = HostDriver::new();
    host.new_session_with(
        "s1",
        shepherd_core::host::NewSessionSpec {
            name: Some("worker".to_owned()),
            ..claude_session_spec()
        },
    );
    assert_eq!(host.user_prompt("s1", "Fix the login redirect bug"), vec![]);
    assert_eq!(names(host.host()), ["worker"]);
    // Decided from the log: a restarted host doesn't name it either.
    let restarted = replay(&host);
    let cmds = restarted.handle(HostEvent::HookTranslated {
        session: sid("s1"),
        turn_end: false,
    });
    assert_eq!(cmds, vec![]);
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
    let cmds = restarted.handle(HostEvent::HookTranslated {
        session: sid("s1"),
        turn_end: false,
    });
    assert_eq!(cmds, vec![]);
}

#[test]
fn a_restarted_host_names_a_session_whose_prompt_it_never_named() {
    let mut host = claude_session();
    // The host stopped after appending the prompt, before handling what came of it.
    host.prompt_frame("s1", "Fix the login redirect bug", false);
    let restarted = replay(&host);
    let cmds = restarted.handle(HostEvent::HookTranslated {
        session: sid("s1"),
        turn_end: false,
    });
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
            base_commit: None,
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
