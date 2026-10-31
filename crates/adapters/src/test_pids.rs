//! Telling whether a pid a test recorded still names that test's process before signalling it (F48, F51).
//!
//! Once a test's process is gone its pid may be handed to anyone's, such as a git another test started, so a test only
//! signals a pid while the process holding it is live and has the test's marker (a path in its tempdir) in its
//! arguments.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Whether `pid` is a live process (not a zombie) whose arguments name `marker`. `ps` is retried a few times, with a
/// pause between tries, if it can't be run or fails, as it can under load (it also fails for a pid that is gone, which
/// costs only the retries); if it never answers, the process counts as not ours: it is never signalled unverified.
pub fn is_ours(pid: libc::pid_t, marker: &Path) -> bool {
    let Some(marker) = marker.to_str() else {
        return false;
    };
    let Some(out) = (0..5).find_map(|attempt| {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        Command::new("ps")
            .args(["-ww", "-o", "stat=,args=", "-p", &pid.to_string()])
            .output()
            .ok()
            .filter(|out| out.status.success())
    }) else {
        return false;
    };
    let line = String::from_utf8_lossy(&out.stdout);
    let Some((stat, args)) = line.trim().split_once(char::is_whitespace) else {
        return false;
    };
    !stat.contains('Z') && args.contains(marker)
}

/// Sends `signal` to `pid` if it [`is_ours`]. Whether it was sent.
pub fn signal_if_ours(pid: libc::pid_t, marker: &Path, signal: libc::c_int) -> bool {
    // SAFETY: plain syscall, on a live process whose arguments name the test's marker: ours (short of it dying and its
    // pid being handed out between the `ps` and here).
    is_ours(pid, marker) && unsafe { libc::kill(pid, signal) } == 0
}
