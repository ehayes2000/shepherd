//! The index's typed values reject anything that isn't one spelling of a valid value (T39).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;

use shepherd_archive::{
    AgentKind, AgentSessionId, BadValue, Branch, CommitHash, FileRole, MachineId, PatchId, RepoId,
    SessionId, SnapshotReason,
};

fn accepts<T: FromStr<Err = BadValue>>(s: &str) -> bool {
    s.parse::<T>().is_ok()
}

/// Every git object name type: 40 or 64 lowercase hex digits, nothing else.
fn object_name_rules<T: FromStr<Err = BadValue> + std::fmt::Display>() {
    for good in [
        "0123456789abcdef".repeat(4)[..40].to_owned(),
        "f".repeat(64),
    ] {
        let parsed: T = good.parse().unwrap();
        assert_eq!(parsed.to_string(), good, "round trips unchanged");
    }
    for bad in [
        String::new(),
        "A".repeat(40),
        format!("{}A", "a".repeat(39)),
        "a".repeat(39),
        "a".repeat(41),
        "a".repeat(63),
        "a".repeat(65),
        "g".repeat(40),
        format!("{} ", "a".repeat(39)),
        format!(" {}", "a".repeat(40)),
        "é".repeat(20),
    ] {
        let Err(err) = bad.parse::<T>() else {
            panic!("{bad:?} was accepted");
        };
        assert_eq!(err.value, bad);
    }
}

#[test]
fn commit_hashes_patch_ids_and_repo_ids_are_40_or_64_lowercase_hex_digits() {
    object_name_rules::<CommitHash>();
    object_name_rules::<PatchId>();
    object_name_rules::<RepoId>();
}

/// Every opaque label type: non-empty, at most 1024 bytes, no control characters.
fn label_rules<T: FromStr<Err = BadValue>>() {
    assert!(accepts::<T>("fix-parser"));
    assert!(accepts::<T>("feature/ünïcode name"));
    assert!(accepts::<T>(&"a".repeat(1024)));
    assert!(!accepts::<T>(""));
    assert!(!accepts::<T>(&"a".repeat(1025)));
    // 1024 bytes is the limit, not 1024 characters.
    assert!(!accepts::<T>(&"é".repeat(513)));
    for control in [
        "a\nb",
        "a\tb",
        "a\0b",
        "bell\u{7}",
        "\u{1b}[31m",
        "a\u{85}b",
    ] {
        assert!(!accepts::<T>(control), "{control:?}");
    }
}

#[test]
fn session_agent_session_machine_and_branch_names_are_non_empty_bounded_and_printable() {
    label_rules::<SessionId>();
    label_rules::<AgentSessionId>();
    label_rules::<MachineId>();
    label_rules::<Branch>();
}

#[test]
fn enums_parse_only_their_stored_names() {
    for kind in [AgentKind::Claude, AgentKind::Codex] {
        assert_eq!(kind.as_str().parse::<AgentKind>().unwrap(), kind);
    }
    for reason in [
        SnapshotReason::TurnEnd,
        SnapshotReason::Close,
        SnapshotReason::BeforeRemove,
        SnapshotReason::Manual,
    ] {
        assert_eq!(reason.as_str().parse::<SnapshotReason>().unwrap(), reason);
    }
    for role in [FileRole::Main, FileRole::Subagent, FileRole::Rollout] {
        assert_eq!(role.as_str().parse::<FileRole>().unwrap(), role);
    }
    assert!(!accepts::<AgentKind>("Claude"));
    assert!(!accepts::<SnapshotReason>("turn-end"));
    assert!(!accepts::<FileRole>(""));
}
