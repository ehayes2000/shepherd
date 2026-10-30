//! The host's re-exec in place (decision 0018), the parts that touch the OS: the `handoff-check` preflight, marking
//! exactly the carried descriptors inheritable, and `execv`.

use std::ffi::{CString, OsStr};
use std::io::{self, Read};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use shepherd_core::host::upgrade::Preflight;
use wait_timeout::ChildExt;

/// Runs `<exe> handoff-check` with no stdin and its stderr discarded, for at most `bound`, and reports how it
/// ended with what it printed.
pub fn preflight(exe: &Path, bound: Duration) -> Preflight {
    let child = Command::new(exe)
        .arg("handoff-check")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return Preflight::Failed(e.to_string()),
    };
    let status = match child.wait_timeout(bound) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Preflight::TimedOut;
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Preflight::Failed(e.to_string());
        }
    };
    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_end(&mut stdout);
    }
    match (status.code(), status.signal()) {
        (Some(code), _) => Preflight::Exited { code, stdout },
        (None, Some(signal)) => Preflight::Signaled(signal),
        (None, None) => Preflight::Failed(format!("ended without a code or a signal: {status}")),
    }
}

/// Clears (`inherit`) or sets `FD_CLOEXEC` on each of `fds`. Stops at the first failure and reports it; the caller
/// undoes it by calling this again with `!inherit`.
pub fn set_inherited(fds: &[RawFd], inherit: bool) -> io::Result<()> {
    for &fd in fds {
        // SAFETY: plain syscalls on descriptors the caller holds open.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        let flags = if inherit {
            flags & !libc::FD_CLOEXEC
        } else {
            flags | libc::FD_CLOEXEC
        };
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether `fd` is inherited across an exec (its `FD_CLOEXEC` is clear).
pub fn is_inherited(fd: RawFd) -> io::Result<bool> {
    // SAFETY: plain syscall.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags & libc::FD_CLOEXEC == 0)
}

/// Replaces this process's image with `exe`, run with `args` (`args[0]` included). Only returns if that fails, with
/// why. The pid, and every descriptor without `FD_CLOEXEC`, carry over.
pub fn exec(exe: &Path, args: &[&OsStr]) -> io::Error {
    let c = |s: &OsStr| CString::new(s.as_bytes()).map_err(io::Error::other);
    let path = match c(exe.as_os_str()) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let args: Vec<CString> = match args.iter().map(|a| c(a)).collect() {
        Ok(args) => args,
        Err(e) => return e,
    };
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: NUL-terminated strings and a NULL-terminated argv, all alive for the call.
    unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
    io::Error::last_os_error()
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    /// A stand-in executable running `body` as a shell script.
    fn stand_in(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("shepherd");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A liveness bound, generous: a loaded machine can take seconds to fork and exec a shell.
    const BOUND: Duration = Duration::from_secs(60);

    #[test]
    fn a_binary_without_handoff_check_exits_with_its_complaint() {
        let dir = tempfile::tempdir().unwrap();
        let exe = stand_in(
            dir.path(),
            "echo \"shepherd: unknown command \\`$1\\`\" >&2; exit 1",
        );
        assert_eq!(
            preflight(&exe, BOUND),
            Preflight::Exited {
                code: 1,
                stdout: Vec::new()
            }
        );
    }

    #[test]
    fn what_handoff_check_prints_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let exe = stand_in(
            dir.path(),
            "[ \"$1\" = handoff-check ] && echo '{\"ok\":1}'",
        );
        assert_eq!(
            preflight(&exe, BOUND),
            Preflight::Exited {
                code: 0,
                stdout: b"{\"ok\":1}\n".to_vec()
            }
        );
    }

    #[test]
    fn a_binary_killed_by_a_signal_is_reported_as_signaled() {
        let dir = tempfile::tempdir().unwrap();
        let exe = stand_in(dir.path(), "kill -9 $$");
        assert_eq!(preflight(&exe, BOUND), Preflight::Signaled(9));
    }

    #[test]
    fn a_binary_that_hangs_is_killed_at_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let exe = stand_in(dir.path(), "exec sleep 600");
        assert_eq!(
            preflight(&exe, Duration::from_millis(100)),
            Preflight::TimedOut
        );
    }

    #[test]
    fn a_missing_binary_cannot_run() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            preflight(&dir.path().join("gone"), BOUND),
            Preflight::Failed(_)
        ));
    }

    #[test]
    fn descriptors_are_made_inheritable_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = File::create(dir.path().join("f")).unwrap();
        let fd = file.as_raw_fd();
        assert!(!is_inherited(fd).unwrap());
        set_inherited(&[fd], true).unwrap();
        assert!(is_inherited(fd).unwrap());
        set_inherited(&[fd], false).unwrap();
        assert!(!is_inherited(fd).unwrap());
        drop(file);
    }

    #[test]
    fn exec_of_a_missing_binary_returns_why() {
        let dir = tempfile::tempdir().unwrap();
        let err = exec(&dir.path().join("gone"), &[OsStr::new("gone")]);
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
