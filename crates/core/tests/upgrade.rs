//! Whether the host updates itself in place when a client asks (`_shepherd/host/upgrade`): the pure decision.

use shepherd_core::host::upgrade::{
    HANDOFF_VERSION, HandoffCheck, Preflight, UpgradeFacts, UpgradeStep, Work, decide,
};
use shepherd_core::wire::{BuildId, ExeDigest, UpgradeAnswer};

fn digest(byte: u8) -> ExeDigest {
    ExeDigest::from_bytes([byte; 32])
}

/// A host running build 1 with build 2 installed at its path, idle, never failed, not forced, preflight not run.
fn stale() -> UpgradeFacts {
    UpgradeFacts {
        running: Some(digest(1)),
        installed: Some(digest(2)),
        force: false,
        work: Work::default(),
        failed: None,
        preflight: None,
    }
}

/// What a handoff-capable installed build prints.
fn check_output(versions: &[u32]) -> Vec<u8> {
    let check = HandoffCheck {
        build: BuildId {
            version: "0.1.0".into(),
            digest: digest(2),
        },
        handoff_versions: versions.to_vec(),
    };
    let mut line = serde_json::to_vec(&check).unwrap_or_else(|e| panic!("{e}"));
    line.push(b'\n');
    line
}

fn preflighted(preflight: Preflight) -> UpgradeFacts {
    UpgradeFacts {
        preflight: Some(preflight),
        ..stale()
    }
}

fn unavailable(step: UpgradeStep) -> String {
    match step {
        UpgradeStep::Answer(UpgradeAnswer::Unavailable { reason }) => reason,
        other => panic!("expected unavailable, got {other:?}"),
    }
}

#[test]
fn a_host_running_what_is_installed_has_nothing_to_do() {
    let facts = UpgradeFacts {
        installed: Some(digest(1)),
        ..stale()
    };
    assert_eq!(
        decide(&facts),
        UpgradeStep::Answer(UpgradeAnswer::NothingToDo)
    );
}

#[test]
fn forcing_swaps_even_onto_the_same_build() {
    let facts = UpgradeFacts {
        installed: Some(digest(1)),
        force: true,
        ..stale()
    };
    assert_eq!(decide(&facts), UpgradeStep::Preflight);
}

#[test]
fn a_stale_idle_host_runs_the_preflight_first() {
    assert_eq!(decide(&stale()), UpgradeStep::Preflight);
}

#[test]
fn work_in_flight_is_busy_and_says_what() {
    let facts = UpgradeFacts {
        work: Work {
            port_calls: 2,
            run_files: 0,
        },
        ..stale()
    };
    match decide(&facts) {
        UpgradeStep::Answer(UpgradeAnswer::Busy { reason }) => {
            assert!(reason.contains('2'), "{reason}")
        }
        other => panic!("expected busy, got {other:?}"),
    }
}

#[test]
fn sessions_waiting_to_spawn_are_busy() {
    let facts = UpgradeFacts {
        work: Work {
            port_calls: 0,
            run_files: 1,
        },
        ..preflighted(Preflight::Exited {
            code: 0,
            stdout: check_output(&[HANDOFF_VERSION]),
        })
    };
    assert!(matches!(
        decide(&facts),
        UpgradeStep::Answer(UpgradeAnswer::Busy { .. })
    ));
}

#[test]
fn a_passed_preflight_is_accepted() {
    let facts = preflighted(Preflight::Exited {
        code: 0,
        stdout: check_output(&[HANDOFF_VERSION + 1, HANDOFF_VERSION]),
    });
    assert_eq!(decide(&facts), UpgradeStep::Answer(UpgradeAnswer::Accepted));
}

#[test]
fn a_binary_without_handoff_check_cannot_hand_off() {
    let facts = preflighted(Preflight::Exited {
        code: 1,
        stdout: b"shepherd: unknown command `handoff-check`\n".to_vec(),
    });
    let reason = unavailable(decide(&facts));
    assert!(reason.contains("can't hand off"), "{reason}");
}

#[test]
fn a_preflight_killed_by_a_signal_says_overwritten_in_place_reinstall_it() {
    let reason = unavailable(decide(&preflighted(Preflight::Signaled(9))));
    assert!(
        reason.contains("overwritten in place? reinstall it"),
        "{reason}"
    );
}

#[test]
fn an_installed_build_that_reads_no_format_this_host_writes_is_unavailable() {
    let facts = preflighted(Preflight::Exited {
        code: 0,
        stdout: check_output(&[HANDOFF_VERSION + 1]),
    });
    let reason = unavailable(decide(&facts));
    assert!(reason.contains("handoff format"), "{reason}");
}

#[test]
fn an_unreadable_preflight_answer_is_unavailable() {
    let facts = preflighted(Preflight::Exited {
        code: 0,
        stdout: b"not json\n".to_vec(),
    });
    unavailable(decide(&facts));
}

#[test]
fn a_preflight_that_hangs_or_cannot_start_is_unavailable() {
    unavailable(decide(&preflighted(Preflight::TimedOut)));
    let reason = unavailable(decide(&preflighted(Preflight::Failed(
        "No such file or directory".into(),
    ))));
    assert!(reason.contains("No such file or directory"), "{reason}");
}

#[test]
fn an_unreadable_installed_binary_is_unavailable() {
    let facts = UpgradeFacts {
        installed: None,
        ..stale()
    };
    unavailable(decide(&facts));
}

#[test]
fn after_a_failed_swap_the_host_says_why_instead_of_trying_again() {
    let facts = UpgradeFacts {
        failed: Some("execv: No such file or directory".into()),
        ..stale()
    };
    let reason = unavailable(decide(&facts));
    assert!(reason.contains("execv"), "{reason}");
}
