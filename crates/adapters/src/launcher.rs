//! `HostLauncher` that starts the host as a detached daemon: normally `<current exe> host`
//! ([`ProcessHostLauncher::shepherd_host`]), or any command (the tests launch a stand-in script).
//!
//! The host must not belong to the client. It is double-forked: the child std spawns forks the host and exits at
//! once, and `launch` reaps it, so the host is reparented to init straight away and the client never has a zombie
//! to collect, whenever either of them exits. The host calls `setsid`, so it has no controlling terminal: closing
//! the client's terminal doesn't hang it up, and nothing it runs can prompt on that terminal. Its stdin and stdout
//! are `/dev/null` (the client's terminal belongs to the TUI) and its working directory is `/`. It inherits no
//! descriptor the client leaked without close-on-exec, and starts with common signals (`RESET_SIGNALS`) at their
//! default action even if the client ignored them.
//!
//! Errors still reach `launch`: std reports a failed `exec` through a close-on-exec pipe that the host inherits,
//! and that pipe only closes once the host has called `exec` or reported why it couldn't. The host's stderr is a
//! pipe too, which `launch` reads until it closes. A host that can't start prints why there and exits, and `launch`
//! returns that as [`LaunchError::HostFailed`]. A host that starts calls [`release_stderr`] once it is serving,
//! which closes the pipe with nothing written. A host that does neither within [`STARTUP_TIMEOUT`] is left to the
//! client's connect retries.
//!
//! The rule this rests on: until it calls [`release_stderr`], the host writes nothing to stderr except its fatal
//! startup error. Any byte written before that (a warning, a progress line) makes `launch` report
//! [`LaunchError::HostFailed`] once the pipe closes, even if the host is serving.
//!
//! `terminate` finds the serving host by its socket ([`ProcessHostLauncher::socket`]): it connects, reads the listening
//! process's pid from the connection's peer credentials (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux) and sends
//! it `SIGTERM`, which a host of any build handles by shutting down cleanly.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use shepherd_core::ports::{HostLauncher, LaunchError, TerminateError};

use crate::private_fs;

pub struct ProcessHostLauncher {
    program: PathBuf,
    args: Vec<OsString>,
    startup_timeout: Duration,
    /// The socket the host serves on, by which [`HostLauncher::terminate`] finds it.
    socket: Option<PathBuf>,
}

/// How long `launch` waits for the host to report or hand off its stderr. A liveness bound, not a measurement:
/// a host still starting after it is left to the client's connect retries.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a host's startup error `launch` keeps.
pub const MAX_ERROR_BYTES: usize = 4096;

impl ProcessHostLauncher {
    /// Launches `program` with `args`.
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            startup_timeout: STARTUP_TIMEOUT,
            socket: None,
        }
    }

    /// Replaces [`STARTUP_TIMEOUT`].
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    /// Finds the host to terminate by `socket`, the socket it serves on. Without one, `terminate` fails.
    pub fn socket(mut self, socket: impl Into<PathBuf>) -> Self {
        self.socket = Some(socket.into());
        self
    }

    /// Launches this executable's `host` subcommand, which serves on `socket`.
    pub fn shepherd_host(socket: impl Into<PathBuf>) -> io::Result<Self> {
        Ok(Self::new(std::env::current_exe()?, ["host"]).socket(socket))
    }
}

impl HostLauncher for ProcessHostLauncher {
    fn launch(&self) -> Result<(), LaunchError> {
        let failed =
            |e: io::Error| LaunchError::SpawnFailed(format!("{}: {e}", self.program.display()));
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let max_fd = fd_limit();
        // SAFETY: `fork`, `setsid`, `fcntl`, `close`, `signal` and `_exit` are async-signal-safe, and the closure
        // touches nothing else (`max_fd` is computed before the fork).
        unsafe {
            command.pre_exec(move || match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                // The host: a new session, with no controlling terminal.
                0 => {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // Descriptors the client leaked without close-on-exec. The close-on-exec ones are left to
                    // `exec`, std's error pipe among them, so `closefrom` would be wrong here.
                    for fd in 3..max_fd {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                            libc::close(fd);
                        }
                    }
                    // Dispositions survive `exec`: a client started with these ignored mustn't pass that on.
                    for signal in RESET_SIGNALS {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                    Ok(())
                }
                // The intermediate child: done once the host exists.
                _ => libc::_exit(0),
            });
        }
        let mut intermediate = command.spawn().map_err(failed)?;
        let stderr = intermediate.stderr.take();
        let status = intermediate.wait().map_err(failed)?;
        if !status.success() {
            return Err(LaunchError::SpawnFailed(format!(
                "{}: the launcher's intermediate process {status}",
                self.program.display()
            )));
        }
        match stderr {
            Some(stderr) => startup_report(stderr, self.startup_timeout),
            None => Ok(()),
        }
    }

    /// Connects to the socket, reads the pid of the process listening on it from the connection's peer credentials,
    /// and sends it `SIGTERM`. This needs nothing from the host's protocol, so it works on a host of any build.
    fn terminate(&self) -> Result<(), TerminateError> {
        let socket = self.socket.as_ref().ok_or_else(|| {
            TerminateError::Failed("no host socket to find the host by".to_owned())
        })?;
        if !check_socket(socket)? {
            return Err(TerminateError::NotRunning);
        }
        let conn = match UnixStream::connect(socket) {
            Ok(conn) => conn,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(TerminateError::NotRunning);
            }
            Err(e) => {
                return Err(TerminateError::Failed(format!(
                    "cannot connect to {}: {e}",
                    socket.display()
                )));
            }
        };
        let failed = |e: io::Error| TerminateError::Failed(format!("cannot find the host: {e}"));
        let (pid, uid) = peer_cred(&conn).map_err(failed)?;
        // SAFETY: plain libc calls.
        let (own, me) = unsafe { (libc::getpid(), libc::getuid()) };
        if uid != me {
            return Err(TerminateError::Failed(format!(
                "{} is served by another user (uid {uid})",
                socket.display()
            )));
        }
        // Never 0 or negative (a process group, or every process), and never this process.
        if pid <= 0 || pid == own {
            return Err(TerminateError::Failed(format!(
                "{} is served by pid {pid}",
                socket.display()
            )));
        }
        // SAFETY: plain syscall, on a positive pid that isn't ours.
        if unsafe { libc::kill(pid, libc::SIGTERM) } == -1 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                return Err(TerminateError::NotRunning);
            }
            return Err(TerminateError::Failed(format!(
                "cannot stop the host (pid {pid}): {e}"
            )));
        }
        Ok(())
    }
}

/// Reads the host's stderr until it closes: with nothing written the host started, otherwise what it wrote is why
/// it couldn't. A host that does neither within `timeout` counts as started; the reading thread is left to finish
/// whenever the host closes its stderr or exits.
fn startup_report(mut stderr: ChildStderr, timeout: Duration) -> Result<(), LaunchError> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("host-launch-stderr".into())
        .spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = MAX_ERROR_BYTES.saturating_sub(kept.len());
                        kept.extend_from_slice(&buf[..n.min(room)]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = tx.send(kept);
        });
    if spawned.is_err() {
        // Can't watch it: the connect retries decide.
        return Ok(());
    }
    match rx.recv_timeout(timeout) {
        Ok(kept) => {
            let text = String::from_utf8_lossy(&kept);
            let text = text.trim();
            if text.is_empty() {
                Ok(())
            } else {
                Err(LaunchError::HostFailed(text.to_owned()))
            }
        }
        Err(_) => Ok(()),
    }
}

/// The host's half of the startup handshake: once it is serving, it points its stderr at `/dev/null`, which closes
/// the pipe `launch` is reading and tells it the host started. Nothing the host prints after that reaches anyone.
/// Before it, the host must write nothing to stderr but its fatal startup error (see the module docs).
pub fn release_stderr() -> io::Result<()> {
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    // SAFETY: plain syscall on a descriptor this function owns and the process's stderr.
    if unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), libc::STDERR_FILENO) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether `socket` is one this user's host serves on, before anything connects to it: its directory exists and is
/// private (decision 0010, as the host checks it), and `socket` itself is a socket this user owns, not a symlink.
/// `Ok(false)` when nothing is there; an error says what is wrong with what is.
fn check_socket(socket: &Path) -> Result<bool, TerminateError> {
    let refused = |why: String| TerminateError::Failed(format!("not stopping the host: {why}"));
    let dir = socket
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        _ => private_fs::check_private_dir(dir).map_err(|e| refused(e.to_string()))?,
    }
    let meta = match std::fs::symlink_metadata(socket) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(refused(format!("{}: {e}", socket.display()))),
    };
    private_fs::refuse_symlink(socket, meta.file_type()).map_err(|e| refused(e.to_string()))?;
    if !meta.file_type().is_socket() {
        return Err(refused(format!("{} is not a socket", socket.display())));
    }
    // SAFETY: plain libc call.
    let me = unsafe { libc::getuid() };
    if meta.uid() != me {
        return Err(refused(format!(
            "{} belongs to another user (uid {})",
            socket.display(),
            meta.uid()
        )));
    }
    Ok(true)
}

/// The pid and user of the process that listens on the socket `conn` is connected to, from the connection's peer
/// credentials (recorded when that process called `listen`).
#[cfg(target_vendor = "apple")]
fn peer_cred(conn: &UnixStream) -> io::Result<(libc::pid_t, libc::uid_t)> {
    let mut pid: libc::pid_t = 0;
    let mut len =
        libc::socklen_t::try_from(std::mem::size_of::<libc::pid_t>()).map_err(io::Error::other)?;
    // SAFETY: `pid` and `len` are valid for writes of the sizes given, and the fd is open for the call.
    let got = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &raw mut len,
        )
    };
    if got == -1 {
        return Err(io::Error::last_os_error());
    }
    let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
    // SAFETY: `uid` and `gid` are valid for writes, and the fd is open for the call.
    if unsafe { libc::getpeereid(conn.as_raw_fd(), &raw mut uid, &raw mut gid) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((pid, uid))
}

/// The pid and user of the process that listens on the socket `conn` is connected to, from the connection's peer
/// credentials (recorded when that process called `listen`).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_cred(conn: &UnixStream) -> io::Result<(libc::pid_t, libc::uid_t)> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len =
        libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).map_err(io::Error::other)?;
    // SAFETY: `cred` and `len` are valid for writes of the sizes given, and the fd is open for the call.
    let got = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    if got == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((cred.pid, cred.uid))
}

/// Signals the host starts with at their default action, whatever the client had.
const RESET_SIGNALS: [libc::c_int; 6] = [
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGPIPE,
    libc::SIGCHLD,
];

/// One past the highest descriptor to check for leaks: the open-file limit, capped so an unlimited limit doesn't
/// mean billions of `fcntl` calls.
fn fd_limit() -> libc::c_int {
    // SAFETY: plain libc call.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    libc::c_int::try_from(limit.clamp(256, 65_536)).unwrap_or(65_536)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::path::Path;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use portable_pty::{CommandBuilder, native_pty_system};
    use shepherd_testkit::contracts::launcher::LauncherFixture;
    use tempfile::TempDir;

    use super::*;

    /// Set in a test re-run inside a pty, see [`launched_host_outlives_the_launcher_and_its_terminal`].
    const CHILD_VAR: &str = "SHEPHERD_LAUNCHER_TEST_CHILD";
    /// The report fifo, passed to the re-run.
    const FIFO_VAR: &str = "SHEPHERD_LAUNCHER_TEST_FIFO";
    /// A fifo the re-run waits on before it exits, passed to the re-run.
    const GO_VAR: &str = "SHEPHERD_LAUNCHER_TEST_GO";
    /// Set in the re-run of [`launched_host_inherits_no_leaked_fd_or_ignored_signal`].
    const HYGIENE_VAR: &str = "SHEPHERD_LAUNCHER_TEST_HYGIENE";

    /// Reports `hygiene <open|closed> <ignored|default>` to the fifo in `$1`: whether descriptor `$2` is open, and
    /// whether `SIGTERM` is ignored (a child shell that sends itself `SIGTERM` survives to say so). Then exits.
    const HYGIENE: &str = r#"
if { true >&"$2"; } 2>/dev/null; then f=open; else f=closed; fi
t=$(sh -c 'kill -TERM $$; echo ignored')
[ -n "$t" ] || t=default
echo "hygiene $f $t" > "$1"
"#;
    /// Generous: under parallel load a shell and `ps` can take seconds to start.
    const TIMEOUT: Duration = Duration::from_secs(60);

    /// How every stand-in host starts, `$1` being its report fifo and `$2` its lifetime in seconds. It records its pid
    /// in `$1.pids` first, a plain file that never blocks, so [`StandInFixture`] kills it whether or not anyone read its
    /// reports. Then it starts a watchdog that kills its whole process group (it leads one: the launcher calls
    /// `setsid`) once the lifetime ends: no stand-in outlives that, even when its test died without dropping the
    /// fixture, or it is stuck opening a fifo nobody reads. A stand-in that keeps running does so by `wait`ing on the
    /// watchdog, so it ends with it.
    const PRELUDE: &str = r#"
echo $$ >> "$1.pids"
(sleep "$2"; kill -KILL -- -$$) </dev/null >/dev/null 2>&1 &
"#;

    /// After [`PRELUDE`], reports `started <pid> <tty|none>` to the fifo in `$1`, writes to stdout and, once it has
    /// handed its stderr off as a host that started does, to stderr (neither may reach a terminal), then runs until
    /// killed or its lifetime ends. On `SIGUSR1` it reports `alive <pid> <ppid>` and exits, its watchdog with it.
    const STAND_IN: &str = r#"
trap 'echo "alive $$ $(ps -o ppid= -p $$)" > "$1"; kill -KILL -- -$$; exit 0' USR1
if (exec 3</dev/tty) 2>/dev/null; then t=tty; else t=none; fi
echo LEAKED-STDOUT
exec 2>/dev/null
echo LEAKED-STDERR >&2
echo "started $$ $t" > "$1"
wait
"#;

    /// What the stand-in reports.
    #[derive(Debug, PartialEq, Eq)]
    enum Report {
        Started { pid: i32, tty: bool },
        Alive { pid: i32, ppid: i32 },
        Hygiene { fd_open: bool, term_ignored: bool },
    }

    fn parse(line: &str) -> Report {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words[..] {
            ["started", pid, tty] => Report::Started {
                pid: pid.parse().unwrap(),
                tty: tty == "tty",
            },
            ["alive", pid, ppid] => Report::Alive {
                pid: pid.parse().unwrap(),
                ppid: ppid.parse().unwrap(),
            },
            ["hygiene", fd, term] => Report::Hygiene {
                fd_open: fd == "open",
                term_ignored: term == "ignored",
            },
            _ => panic!("bad report {line:?}"),
        }
    }

    /// A tempdir holding the stand-ins' report fifo, read line by line on a thread. When dropped, kills the process
    /// group of every stand-in that recorded its pid (see [`PRELUDE`]), whether or not it reported.
    struct StandInFixture {
        dir: TempDir,
        reports: Receiver<Report>,
        /// A write end of the fifo, so the reader never sees EOF between writers, and can be told to stop.
        stop: File,
        /// The stand-in listener [`LauncherFixture::running`] started, until it exits.
        listener: Option<ChildGuard>,
    }

    impl StandInFixture {
        fn new() -> Self {
            let dir = private_tempdir();
            let fifo = dir.path().join("report");
            mkfifo(&fifo);
            // Opening a fifo read-write doesn't wait for a writer.
            let file = File::options().read(true).write(true).open(&fifo).unwrap();
            let stop = file.try_clone().unwrap();
            let (tx, reports) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(file).lines() {
                    let Ok(line) = line else { return };
                    if line == "stop" || tx.send(parse(&line)).is_err() {
                        return;
                    }
                }
            });
            Self {
                dir,
                reports,
                stop,
                listener: None,
            }
        }

        fn fifo(&self) -> PathBuf {
            self.dir.path().join("report")
        }

        fn stand_in(&self) -> ProcessHostLauncher {
            stand_in(&self.fifo())
        }

        /// The pids the stand-ins launched so far recorded.
        fn recorded_pids(&self) -> Vec<i32> {
            let pids = self.fifo().with_extension("pids");
            std::fs::read_to_string(pids)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        }

        /// Whether process group `pgid` has a live member launched through this fixture: a stand-in, or a subshell of
        /// one such as its watchdog, whose arguments name this fixture's fifo. (`ps` prints each process on one line,
        /// escaping the newlines in a stand-in's script.) `ps` is retried a few times, with a pause between tries, if it
        /// can't be run or fails, as it can under load; if it never answers, the group counts as not ours: it is never
        /// signalled unverified, and its watchdog ends it within [`STAND_IN_LIFETIME`].
        fn group_is_ours(&self, pgid: i32) -> bool {
            let fifo = self.fifo();
            let Some(fifo) = fifo.to_str() else {
                return false;
            };
            let Some(out) = (0..5).find_map(|attempt| {
                if attempt > 0 {
                    // Whatever made `ps` fail (a fork or exec refused under load) may pass in a moment.
                    std::thread::sleep(Duration::from_millis(100));
                }
                std::process::Command::new("ps")
                    .args(["-A", "-o", "pgid=,stat=,args="])
                    .output()
                    .ok()
                    .filter(|out| out.status.success())
            }) else {
                return false;
            };
            String::from_utf8_lossy(&out.stdout).lines().any(|line| {
                let Some((group, rest)) = line.trim_start().split_once(char::is_whitespace) else {
                    return false;
                };
                let Some((stat, args)) = rest.trim_start().split_once(char::is_whitespace) else {
                    return false;
                };
                group.parse() == Ok(pgid) && !stat.contains('Z') && args.contains(fifo)
            })
        }

        /// Signals the stand-in `pid` until it reports that it is alive, or `None` if it never does. Repeated,
        /// because `sh` can lose a trapped signal that arrives while it forks (between `started` and `wait`). Each signal
        /// goes out only while the stand-in's group (it leads one) is proven ours ([`StandInFixture::group_is_ours`]):
        /// SIGUSR1 ends a process that doesn't trap it, and once our stand-in is gone its pid may name someone else's.
        fn ask_alive(&mut self, pid: i32) -> Option<Report> {
            let deadline = std::time::Instant::now() + TIMEOUT;
            let mut signalled = false;
            while std::time::Instant::now() < deadline {
                // SAFETY: plain syscall, on a group that has one of our stand-ins in it, so its id can't have been
                // reused (short of all of it dying and the id being handed out in between).
                if !self.group_is_ours(pid) || unsafe { libc::kill(pid, libc::SIGUSR1) } != 0 {
                    if !signalled {
                        // Gone (or never ours) before it was ever asked: it can't have answered.
                        return None;
                    }
                    // Gone after a signal got through: it may have answered and exited. Wait for that answer.
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    return self.reports.recv_timeout(left).ok();
                }
                signalled = true;
                if let Ok(report) = self.reports.recv_timeout(Duration::from_millis(200)) {
                    return Some(report);
                }
            }
            None
        }

        fn next(&mut self) -> Report {
            self.next_within(TIMEOUT)
        }

        fn next_within(&mut self, timeout: Duration) -> Report {
            self.reports
                .recv_timeout(timeout)
                .unwrap_or_else(|_| panic!("the stand-in didn't report"))
        }
    }

    impl Drop for StandInFixture {
        fn drop(&mut self) {
            for pid in self.recorded_pids() {
                // Each stand-in leads its own process group (`setsid`), its watchdog included, so this reaches the
                // stand-in itself too. Killed again for as long as the group has a live member whose arguments name our
                // fifo ([`StandInFixture::group_is_ours`]), as a precaution (untested) against a stand-in or subshell
                // forked while the group is signalled. A member without our fifo in its arguments (the watchdog's
                // `sleep`) isn't waited for: the watchdog ends anything left. Never once no member is verified ours:
                // its id may then lead someone else's group, such as a git another test started (F48).
                let deadline = std::time::Instant::now() + TIMEOUT;
                while self.group_is_ours(pid)
                    // SAFETY: plain syscall, on a group that has one of our stand-ins in it, so its id can't have been
                    // reused (short of all of it dying and the id being handed out in between).
                    && unsafe { libc::kill(-pid, libc::SIGKILL) } == 0
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let _ = self.stop.write_all(b"stop\n");
        }
    }

    impl LauncherFixture for StandInFixture {
        type Launcher = ProcessHostLauncher;

        fn working(&mut self) -> ProcessHostLauncher {
            self.stand_in()
        }

        fn broken(&mut self) -> ProcessHostLauncher {
            ProcessHostLauncher::new(self.dir.path().join("no-such-host"), ["host"])
        }

        fn failing_host(&mut self, message: &str) -> ProcessHostLauncher {
            ProcessHostLauncher::new(
                "/bin/sh",
                ["-c", r#"printf '%s\n' "$1" >&2; exit 1"#, "script", message],
            )
        }

        fn wait_started(&mut self, n: usize, timeout: Duration) {
            for _ in 0..n {
                assert!(matches!(self.next_within(timeout), Report::Started { .. }));
            }
        }

        fn running(&mut self) -> ProcessHostLauncher {
            let socket = self.dir.path().join("host.sock");
            self.listener = Some(spawn_listener(&socket));
            self.broken().socket(socket)
        }

        fn idle(&mut self) -> ProcessHostLauncher {
            self.broken().socket(self.dir.path().join("nothing.sock"))
        }

        fn wait_terminated(&mut self, timeout: Duration) {
            let mut listener = self.listener.take().expect("a running stand-in listener");
            assert_eq!(listener.exit_signal(timeout), Some(libc::SIGTERM));
        }
    }

    /// Launches on another thread, so a `launch` that blocks fails the test instead of hanging it.
    fn launch(launcher: ProcessHostLauncher) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(launcher.launch());
        });
        let launched = rx.recv_timeout(TIMEOUT);
        assert!(matches!(launched, Ok(Ok(()))), "{launched:?}");
    }

    /// Launches `sh -c body`, with no arguments: a host that must exit by itself.
    fn script(body: &str) -> ProcessHostLauncher {
        ProcessHostLauncher::new("/bin/sh", ["-c", body, "script"])
    }

    /// Launches `body` as a stand-in host: after [`PRELUDE`], with `fifo` as `$1` and `lifetime` as `$2`.
    fn stand_in_script(body: &str, fifo: &Path, lifetime: Duration) -> ProcessHostLauncher {
        let body = format!("{PRELUDE}{body}");
        ProcessHostLauncher::new(
            "/bin/sh",
            [
                "-c",
                &body,
                "stand-in",
                fifo.to_str().unwrap(),
                &lifetime.as_secs().to_string(),
            ],
        )
    }

    /// Launches on another thread and returns what `launch` did. Fails the test if it blocks.
    fn launch_result(launcher: ProcessHostLauncher) -> Result<(), LaunchError> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(launcher.launch());
        });
        rx.recv_timeout(TIMEOUT)
            .unwrap_or_else(|_| panic!("launch blocked"))
    }

    fn mkfifo(path: &Path) {
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    }

    fn stand_in(fifo: &Path) -> ProcessHostLauncher {
        stand_in_script(STAND_IN, fifo, STAND_IN_LIFETIME)
    }

    mod contract {
        use super::StandInFixture;

        shepherd_testkit::launcher_contract!(StandInFixture::new());
    }

    #[test]
    fn launched_host_is_not_a_child_of_the_launcher() {
        let mut f = StandInFixture::new();
        launch(f.stand_in());
        let Report::Started { pid, .. } = f.next() else {
            panic!("expected started");
        };
        let Some(Report::Alive { ppid, .. }) = f.ask_alive(pid) else {
            panic!("the host didn't answer");
        };
        assert_ne!(
            ppid as u32,
            std::process::id(),
            "the host would be our zombie"
        );
    }

    /// A stand-in that answers `SIGUSR1` takes its watchdog with it, long before its lifetime would end it.
    #[test]
    fn a_stand_in_that_answers_ends_with_its_watchdog() {
        let mut f = StandInFixture::new();
        launch(f.stand_in());
        let Report::Started { pid, .. } = f.next() else {
            panic!("expected started");
        };
        assert!(
            matches!(f.ask_alive(pid), Some(Report::Alive { .. })),
            "the host didn't answer"
        );
        assert!(TIMEOUT < STAND_IN_LIFETIME);
        assert!(
            group_gone(pid),
            "stand-in {pid}'s group outlived its answer"
        );
    }

    /// Whether no process is left in group `pgid`, within a generous liveness bound.
    fn group_gone(pgid: i32) -> bool {
        let deadline = std::time::Instant::now() + TIMEOUT;
        while std::time::Instant::now() < deadline {
            // SAFETY: plain syscall; signal 0 only checks that the group has a member.
            if unsafe { libc::kill(-pgid, 0) } == -1 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// A test that fails before it reads a stand-in's report still leaves nothing running: the fixture kills every
    /// stand-in launched through it, its children included.
    #[test]
    fn a_dropped_fixture_kills_its_unread_stand_ins_and_their_children() {
        let f = StandInFixture::new();
        launch(f.stand_in());
        let pids = f.recorded_pids();
        assert_eq!(pids.len(), 1, "{pids:?}");
        drop(f);
        assert!(group_gone(pids[0]), "stand-in {} left running", pids[0]);
    }

    /// A stand-in whose group is gone may have its pid handed to another process leading a group of its own (as a git
    /// another test runs does) by the time the fixture is dropped: the drop leaves that group alone (F48).
    #[test]
    fn a_dropped_fixture_spares_a_group_that_took_a_dead_stand_ins_pid() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let f = StandInFixture::new();
        let mut stranger = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap();
        std::fs::write(
            f.fifo().with_extension("pids"),
            format!("{}\n", stranger.id()),
        )
        .unwrap();
        drop(f);
        // A SIGKILL the drop sent would be acted on before this SIGTERM.
        let id = libc::pid_t::try_from(stranger.id()).unwrap();
        // SAFETY: plain syscall on our own unreaped child.
        unsafe { libc::kill(id, libc::SIGTERM) };
        let status = stranger.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
    }

    /// A stand-in that died before it was asked whether it is alive may have its pid handed to another process (one
    /// leading a group of its own, as a git another test runs does). Asking signals only a stand-in proven ours:
    /// SIGUSR1 would end that process (F51).
    #[test]
    fn asking_if_alive_spares_a_process_that_took_a_dead_stand_ins_pid() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        let mut f = StandInFixture::new();
        let mut stranger = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap();
        let id = libc::pid_t::try_from(stranger.id()).unwrap();
        let answer = f.ask_alive(id);
        // A SIGUSR1 the ask sent would be acted on before this SIGTERM.
        // SAFETY: plain syscall on our own unreaped child.
        unsafe { libc::kill(id, libc::SIGTERM) };
        let status = stranger.wait().unwrap();
        assert_eq!(answer, None);
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
    }

    /// A stand-in nobody stops, as when its test process dies without dropping the fixture, ends with its lifetime,
    /// children and all.
    #[test]
    fn a_stand_in_nobody_stops_ends_with_its_lifetime() {
        let mut f = StandInFixture::new();
        launch(stand_in_script(STAND_IN, &f.fifo(), Duration::from_secs(1)));
        let Report::Started { pid, .. } = f.next() else {
            panic!("expected started");
        };
        assert!(group_gone(pid), "stand-in {pid} outlived its lifetime");
    }

    /// Even one with a child stuck writing a report nobody reads, as a stand-in reporting after its test died would
    /// be. (In a child: `sh` itself would give up the write when its watchdog exits and interrupts it.)
    #[test]
    fn a_stand_in_stuck_on_a_fifo_nobody_reads_ends_with_its_lifetime() {
        let f = StandInFixture::new();
        mkfifo(&f.fifo().with_extension("unread"));
        launch(stand_in_script(
            r#"exec 2>&-; (echo stuck > "$1.unread") & wait"#,
            &f.fifo(),
            Duration::from_secs(1),
        ));
        let pids = f.recorded_pids();
        assert_eq!(pids.len(), 1, "{pids:?}");
        assert!(
            group_gone(pids[0]),
            "stand-in {} outlived its lifetime",
            pids[0]
        );
    }

    /// Re-runs itself with `SIGTERM` ignored and a file open without close-on-exec, as a careless parent of the
    /// client might leave them, and launches from there. Neither may reach the host.
    #[test]
    fn launched_host_inherits_no_leaked_fd_or_ignored_signal() {
        if std::env::var_os(HYGIENE_VAR).is_some() {
            let fifo = PathBuf::from(std::env::var_os(FIFO_VAR).unwrap());
            let leaked = File::open("/dev/null").unwrap();
            let fd = std::os::fd::AsRawFd::as_raw_fd(&leaked);
            // SAFETY: plain syscalls on a descriptor this test owns, and a valid disposition.
            unsafe {
                assert_eq!(libc::fcntl(fd, libc::F_SETFD, 0), 0);
                assert_ne!(libc::signal(libc::SIGTERM, libc::SIG_IGN), libc::SIG_ERR);
            }
            let fifo = fifo.to_str().unwrap().to_owned();
            launch(ProcessHostLauncher::new(
                "/bin/sh",
                ["-c", HYGIENE, "hygiene", &fifo, &fd.to_string()],
            ));
            return;
        }
        let mut f = StandInFixture::new();
        let mut rerun = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "launcher::tests::launched_host_inherits_no_leaked_fd_or_ignored_signal",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(HYGIENE_VAR, "1")
                .env(FIFO_VAR, f.fifo())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        // Read while it runs, so a full pipe can't stall it.
        let stdout = read_on_thread(rerun.0.stdout.take().unwrap());
        let stderr = read_on_thread(rerun.0.stderr.take().unwrap());
        let status = rerun.wait(TIMEOUT);
        let stdout = stdout.join().unwrap();
        assert!(status.success(), "{stdout}{}", stderr.join().unwrap());
        assert!(stdout.contains("1 passed"), "{stdout}");
        assert_eq!(
            f.next(),
            Report::Hygiene {
                fd_open: false,
                term_ignored: false
            }
        );
    }

    #[test]
    fn a_launcher_whose_command_is_not_executable_says_which() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("host");
        // A script without its executable bit: the spawn fails, so it never runs.
        std::fs::write(&script, "exit 0\n").unwrap();
        let err = ProcessHostLauncher::new(&script, ["host"])
            .launch()
            .unwrap_err();
        let LaunchError::SpawnFailed(why) = err else {
            panic!("expected SpawnFailed, got {err:?}");
        };
        assert!(why.contains(script.to_str().unwrap()), "{why}");
    }

    /// Re-runs itself in a pty, so the launcher has a controlling terminal, and launches the stand-in from there.
    /// The re-run stays up until the stand-in has reported, which it does after writing to stdout and stderr, then
    /// exits and its terminal closes. The stand-in must have had no terminal, written nothing to the launcher's, and
    /// still be running afterwards (a process left in the terminal's session would be hung up).
    #[test]
    fn launched_host_outlives_the_launcher_and_its_terminal() {
        if std::env::var_os(CHILD_VAR).is_some() {
            assert!(
                File::open("/dev/tty").is_ok(),
                "the re-run should have a controlling terminal"
            );
            let fifo = PathBuf::from(std::env::var_os(FIFO_VAR).unwrap());
            launch(stand_in(&fifo));
            let go = PathBuf::from(std::env::var_os(GO_VAR).unwrap());
            let _ = File::open(go).unwrap().read_to_end(&mut Vec::new());
            return;
        }
        let mut f = StandInFixture::new();
        let go = f.dir.path().join("go");
        mkfifo(&go);
        let pty = native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "launcher::tests::launched_host_outlives_the_launcher_and_its_terminal",
            "--test-threads=1",
            "--nocapture",
        ]);
        cmd.env(CHILD_VAR, "1");
        cmd.env(FIFO_VAR, f.fifo());
        cmd.env(GO_VAR, &go);
        let mut child = PtyChildGuard(pty.slave.spawn_command(cmd).unwrap());
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let output = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            buf
        });
        let Report::Started { pid, tty } = f.next() else {
            panic!("expected started");
        };
        // Let the re-run exit. Opening the fifo waits for the re-run to open it too, so do it on a thread.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(File::options().write(true).open(go).map(drop));
        });
        assert!(
            matches!(rx.recv_timeout(TIMEOUT), Ok(Ok(()))),
            "the re-run never waited"
        );
        let status = child.0.wait().unwrap();
        drop(pty.master);
        let output = String::from_utf8_lossy(&output.join().unwrap()).into_owned();
        assert!(status.success(), "{output}");
        assert!(output.contains("1 passed"), "{output}");
        assert!(!tty, "the host has a controlling terminal");
        assert!(
            !output.contains("LEAKED"),
            "the host wrote to the terminal: {output}"
        );
        assert!(
            matches!(f.ask_alive(pid), Some(Report::Alive { pid: p, .. }) if p == pid),
            "the host died with its launcher's terminal"
        );
    }

    #[test]
    fn a_host_that_prints_an_error_and_exits_is_host_failed_with_its_text() {
        let launcher = script(r#"printf '\nshepherd host: line one\nline two\n\n' >&2; exit 1"#);
        assert_eq!(
            launch_result(launcher),
            Err(LaunchError::HostFailed(
                "shepherd host: line one\nline two".into()
            ))
        );
    }

    #[test]
    fn a_host_failure_message_is_capped() {
        let launcher = script(
            r#"i=0; while [ $i -lt 2000 ]; do printf 'xxxxxxxxx\n' >&2; i=$((i+1)); done; exit 1"#,
        );
        let Err(LaunchError::HostFailed(why)) = launch_result(launcher) else {
            panic!("expected HostFailed");
        };
        assert!(why.starts_with("xxxxxxxxx\n"), "{why}");
        assert!(why.len() <= MAX_ERROR_BYTES, "{} bytes", why.len());
    }

    #[test]
    fn a_host_that_closes_stderr_and_keeps_running_started() {
        let mut f = StandInFixture::new();
        let fifo = f.fifo();
        let launcher = stand_in_script(
            r#"exec 2>&-; echo "started $$ none" > "$1"; wait"#,
            &fifo,
            STAND_IN_LIFETIME,
        );
        assert_eq!(launch_result(launcher), Ok(()));
        assert!(matches!(f.next(), Report::Started { .. }));
    }

    /// A host that neither reports nor hands its stderr off within the bound is left to the connect retries.
    #[test]
    fn a_host_that_never_writes_or_closes_stderr_is_started_after_the_bound() {
        let mut f = StandInFixture::new();
        let fifo = f.fifo();
        let launcher = stand_in_script(
            r#"echo "started $$ none" > "$1"; wait"#,
            &fifo,
            STAND_IN_LIFETIME,
        )
        .startup_timeout(Duration::from_millis(500));
        assert_eq!(launch_result(launcher), Ok(()));
        assert!(matches!(f.next(), Report::Started { .. }));
    }

    /// Set in the re-run of [`stand_in_listener`]: the socket it listens on.
    const LISTEN_VAR: &str = "SHEPHERD_LAUNCHER_TEST_LISTEN";
    /// What the stand-in listener prints once it listens.
    const LISTENING: &str = "stand-in listening";
    /// How long a stand-in host or listener lives at most, in case a failed test leaks it.
    const STAND_IN_LIFETIME: Duration = Duration::from_secs(120);

    /// A stand-in for a running host, of any build: re-run with [`LISTEN_VAR`] set, it listens on that socket and
    /// accepts connections until a signal stops it, or [`STAND_IN_LIFETIME`] passes. It knows nothing of shepherd's
    /// protocol, like a host older than `terminate`. Run normally, it does nothing.
    #[test]
    fn stand_in_listener() {
        let Some(socket) = std::env::var_os(LISTEN_VAR) else {
            return;
        };
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        // Polled, so the lifetime is honoured even when nothing connects.
        listener.set_nonblocking(true).unwrap();
        let mut out = std::io::stdout();
        writeln!(out, "{LISTENING}").unwrap();
        out.flush().unwrap();
        let deadline = std::time::Instant::now() + STAND_IN_LIFETIME;
        // Kept open, as a host keeps its clients': a connection closed at once can't be asked for its peer.
        let mut conns = Vec::new();
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((conn, _)) => conns.push(conn),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("{e}"),
            }
        }
    }

    /// A child process of this test, killed and reaped when dropped, however the test ends.
    struct ChildGuard(std::process::Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            // Our own child, by its handle: never by name.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl ChildGuard {
        /// Whether it is still running.
        fn alive(&mut self) -> bool {
            matches!(self.0.try_wait(), Ok(None))
        }

        /// The signal that ended it, waiting at most `timeout` for it to end. Panics (and the drop kills it) if it
        /// doesn't.
        fn exit_signal(&mut self, timeout: Duration) -> Option<i32> {
            use std::os::unix::process::ExitStatusExt;
            self.wait(timeout).signal()
        }

        /// How it ended, waiting at most `timeout` for it to. Panics (and the drop kills it) if it doesn't.
        fn wait(&mut self, timeout: Duration) -> std::process::ExitStatus {
            use wait_timeout::ChildExt;
            match self.0.wait_timeout(timeout).unwrap() {
                Some(status) => status,
                None => panic!("child {} is still running", self.0.id()),
            }
        }
    }

    /// A child of this test in a pty, killed and reaped when dropped, however the test ends.
    struct PtyChildGuard(Box<dyn portable_pty::Child + Send + Sync>);

    impl Drop for PtyChildGuard {
        fn drop(&mut self) {
            // Our own child, by its handle. Once reaped, killing it signals nothing.
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    /// Reads `from` to its end on a thread, lossily decoded.
    fn read_on_thread(mut from: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = from.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    }

    /// Starts [`stand_in_listener`] on `socket` and waits until it listens.
    fn spawn_listener(socket: &Path) -> ChildGuard {
        let mut child = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "launcher::tests::stand_in_listener",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(LISTEN_VAR, socket)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if line.contains(LISTENING) {
                    let _ = tx.send(());
                }
            }
        });
        assert!(
            rx.recv_timeout(TIMEOUT).is_ok(),
            "the stand-in listener never listened"
        );
        child
    }

    /// A tempdir only this user can access, as `~/.shepherd` is (decision 0010), whatever the umask.
    fn private_tempdir() -> TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn launcher_for(socket: &Path) -> ProcessHostLauncher {
        ProcessHostLauncher::new("/nonexistent", ["host"]).socket(socket)
    }

    #[test]
    fn terminate_signals_the_process_listening_on_the_socket() {
        let dir = private_tempdir();
        let socket = dir.path().join("host.sock");
        let mut listener = spawn_listener(&socket);
        assert_eq!(launcher_for(&socket).terminate(), Ok(()));
        assert_eq!(listener.exit_signal(TIMEOUT), Some(libc::SIGTERM));
    }

    #[test]
    fn terminate_without_a_socket_says_so() {
        let launcher = ProcessHostLauncher::new("/nonexistent", ["host"]);
        assert!(matches!(
            launcher.terminate(),
            Err(TerminateError::Failed(_))
        ));
    }

    #[test]
    fn terminate_on_a_socket_nothing_listens_on_is_not_running() {
        let dir = private_tempdir();
        let socket = dir.path().join("host.sock");
        // Left by a listener that was killed, as after a host that crashed: the file stays. Bound in another process,
        // because a child another test forks from this one could still hold a listener bound here.
        drop(spawn_listener(&socket));
        assert!(socket.exists());
        assert_eq!(
            launcher_for(&socket).terminate(),
            Err(TerminateError::NotRunning)
        );
    }

    #[test]
    fn terminate_in_a_directory_that_does_not_exist_is_not_running() {
        let dir = private_tempdir();
        let socket = dir.path().join("gone").join("host.sock");
        assert_eq!(
            launcher_for(&socket).terminate(),
            Err(TerminateError::NotRunning)
        );
    }

    #[test]
    fn terminate_refuses_a_symlink_at_the_socket_path_and_signals_nothing() {
        let dir = private_tempdir();
        let real = dir.path().join("elsewhere.sock");
        let mut listener = spawn_listener(&real);
        let socket = dir.path().join("host.sock");
        std::os::unix::fs::symlink(&real, &socket).unwrap();
        let result = launcher_for(&socket).terminate();
        assert!(
            matches!(result, Err(TerminateError::Failed(_))),
            "{result:?}"
        );
        assert!(listener.alive(), "the stand-in was signalled");
    }

    #[test]
    fn terminate_refuses_a_socket_in_a_directory_others_can_write_and_signals_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_tempdir();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let socket = shared.join("host.sock");
        let mut listener = spawn_listener(&socket);
        let result = launcher_for(&socket).terminate();
        assert!(
            matches!(result, Err(TerminateError::Failed(_))),
            "{result:?}"
        );
        assert!(listener.alive(), "the stand-in was signalled");
    }

    #[test]
    fn terminate_refuses_a_path_that_is_not_a_socket() {
        let dir = private_tempdir();
        let socket = dir.path().join("host.sock");
        std::fs::write(&socket, b"").unwrap();
        let result = launcher_for(&socket).terminate();
        assert!(
            matches!(result, Err(TerminateError::Failed(_))),
            "{result:?}"
        );
    }

    /// A socket this very process listens on: `terminate` must never signal itself.
    #[test]
    fn terminate_never_signals_its_own_process() {
        let dir = private_tempdir();
        let socket = dir.path().join("host.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let result = launcher_for(&socket).terminate();
        assert!(
            matches!(result, Err(TerminateError::Failed(_))),
            "{result:?}"
        );
        // Still here to assert it.
    }
}
