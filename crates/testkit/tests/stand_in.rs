//! `stand_in::install`: what the code under test execs is the path it was given, with `$0` and the arguments as for
//! a script written there, and no file the test wrote is executable.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use shepherd_testkit::stand_in;

#[test]
fn a_stand_in_runs_its_body_with_its_own_path_as_dollar_zero_and_the_arguments_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("tool");
    stand_in::install(&exe, "printf '%s\\n' \"$0\" \"$@\"\nexit 3\n");

    let out = Command::new(&exe).args(["a b", "", "-c"]).output().unwrap();

    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let said = String::from_utf8(out.stdout).unwrap();
    assert_eq!(said, format!("{}\na b\n\n-c\n", exe.display()));
}

#[test]
fn a_stand_in_is_a_symlink_to_the_checked_in_script() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("tool");
    stand_in::install(&exe, "true\n");

    assert_eq!(
        std::fs::read_link(&exe).unwrap(),
        Path::new(stand_in::SHARED)
    );
}
