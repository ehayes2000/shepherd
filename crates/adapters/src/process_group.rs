//! Killing a child's whole process group and waiting until it is gone (F08's pattern). The child must lead its own
//! group (`setsid` or `setpgid(0, 0)` before exec), and its group may only be killed while it is unreaped, so its pid
//! (the group id) can't have been reused: wait for it with [`wait_unreaped`], kill the group, then reap it and
//! [`wait_group_gone`].

use std::io;
use std::time::{Duration, Instant};

/// How long to wait for the members of a killed process group to finish dying. SIGKILL can't be caught, so this is
/// only a bound for a process stuck in an uninterruptible syscall.
const GROUP_EXIT_WAIT: Duration = Duration::from_secs(10);

/// Sends SIGKILL to process group `pid`, whose leader must be our unreaped child.
pub(crate) fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain syscall. The leader is not reaped yet, so the group id still names our group; if the group
        // has no members left it fails with ESRCH, which is fine.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
}

/// Blocks until child `pid` has exited, leaving it a zombie (`WNOWAIT`) so its pid stays taken, and returns how it
/// exited: the `si_code` of `waitid`, e.g. `CLD_EXITED` or `CLD_KILLED`.
pub(crate) fn wait_unreaped(pid: u32) -> io::Result<libc::c_int> {
    let pid = libc::id_t::from(pid);
    loop {
        // SAFETY: `info` is a valid out-pointer; `pid` is our own unreaped child.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if rc == 0 {
            return Ok(info.si_code);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Whether a child that exited as `code` (from [`wait_unreaped`]) was killed by a signal.
pub(crate) fn killed_by_signal(code: libc::c_int) -> bool {
    code == libc::CLD_KILLED || code == libc::CLD_DUMPED
}

/// Blocks until process group `pgid` has no members left, or [`GROUP_EXIT_WAIT`] has passed. Its id can't be reused
/// by a new process while the group has members.
pub(crate) fn wait_group_gone(pgid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    let started = Instant::now();
    while group_exists(pgid) && started.elapsed() < GROUP_EXIT_WAIT {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether process group `pgid` has any member. macOS answers EPERM, not ESRCH, while the only members left are dying
/// or zombies not yet reaped (by launchd, once their parent is gone): they still hold the group id, so they count.
fn group_exists(pgid: libc::pid_t) -> bool {
    // SAFETY: plain syscall; signal 0 only checks that the group exists.
    let rc = unsafe { libc::killpg(pgid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
