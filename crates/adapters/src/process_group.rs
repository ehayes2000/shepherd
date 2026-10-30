//! Killing a child's whole process group and waiting until it is gone (F08's pattern). The child must lead its own
//! group (`setsid` or `setpgid(0, 0)` before exec) and must not have been reaped yet.

use std::io;
use std::process::Child;
use std::time::{Duration, Instant};

/// How long to wait for the members of a killed process group to finish dying. SIGKILL can't be caught, so this is
/// only a bound for a process stuck in an uninterruptible syscall.
const GROUP_EXIT_WAIT: Duration = Duration::from_secs(10);

/// Kills `child`'s process group, reaps `child`, and waits until no member of the group is left.
pub(crate) fn kill_and_reap(child: &mut Child) {
    let pid = child.id();
    kill_group(pid);
    // Kill the group again once the leader is dead but not yet reaped, for a child it forked while the first kill was
    // being delivered. Until the leader is reaped its pid (the group id) can't be reused by another process.
    wait_unreaped(pid);
    kill_group(pid);
    let _ = child.wait();
    wait_group_gone(pid);
}

fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain syscall. The leader is not reaped yet, so the group id still names our group; if the group
        // has no members left it fails with ESRCH, which is fine.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
}

/// Blocks until child `pid` has exited, leaving it a zombie (`WNOWAIT`) so its pid stays taken.
fn wait_unreaped(pid: u32) {
    let pid = libc::id_t::from(pid);
    loop {
        // SAFETY: `info` is a valid out-pointer; `pid` is our own unreaped child.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if rc == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// Blocks until process group `pgid` has no members left, or [`GROUP_EXIT_WAIT`] has passed. Its id can't be reused
/// by a new process while the group has members.
fn wait_group_gone(pgid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    let started = Instant::now();
    // SAFETY: plain syscall; signal 0 only checks that the group exists.
    while unsafe { libc::killpg(pgid, 0) } == 0 && started.elapsed() < GROUP_EXIT_WAIT {
        std::thread::sleep(Duration::from_millis(10));
    }
}
