//! `HostLauncher` that starts the host as a detached daemon: normally `<current exe> host`
//! ([`ProcessHostLauncher::shepherd_host`]), or any command (the tests launch a stand-in script).
//!
//! The host must not belong to the client. It is double-forked: the child std spawns forks the host and exits at
//! once, and `launch` reaps it, so the host is reparented to init straight away and the client never has a zombie
//! to collect, whenever either of them exits. The host calls `setsid`, so it has no controlling terminal: closing
//! the client's terminal doesn't hang it up, and nothing it runs can prompt on that terminal. Its stdio is
//! `/dev/null` (the client's terminal belongs to the TUI) and its working directory is `/`. It inherits no
//! descriptor the client leaked without close-on-exec, and starts with common signals (`RESET_SIGNALS`) at their
//! default action even if the client ignored them.
//!
//! Errors still reach `launch`: std reports a failed `exec` through a close-on-exec pipe that the host inherits,
//! and that pipe only closes once the host has called `exec` or reported why it couldn't.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use shepherd_core::ports::{HostLauncher, LaunchError};

pub struct ProcessHostLauncher {
    program: PathBuf,
    args: Vec<OsString>,
}

impl ProcessHostLauncher {
    /// Launches `program` with `args`.
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// Launches this executable's `host` subcommand.
    pub fn shepherd_host() -> io::Result<Self> {
        Ok(Self::new(std::env::current_exe()?, ["host"]))
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
            .stderr(Stdio::null());
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
        let status = intermediate.wait().map_err(failed)?;
        if !status.success() {
            return Err(LaunchError::SpawnFailed(format!(
                "{}: the launcher's intermediate process {status}",
                self.program.display()
            )));
        }
        Ok(())
    }
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

    /// Reports `started <pid> <tty|none>` to the fifo in `$1`, writes to stdout and stderr (which must go nowhere),
    /// then runs until killed (at most 2 minutes, in case a failed test leaks it). On `SIGUSR1` it reports
    /// `alive <pid> <ppid>` and exits.
    const STAND_IN: &str = r#"
trap 'echo "alive $$ $(ps -o ppid= -p $$)" > "$1"; kill $! 2>/dev/null; exit 0' USR1
if (exec 3</dev/tty) 2>/dev/null; then t=tty; else t=none; fi
echo LEAKED-STDOUT
echo LEAKED-STDERR >&2
echo "started $$ $t" > "$1"
sleep 120 &
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

    /// A tempdir holding the stand-in's report fifo, read line by line on a thread. Kills every stand-in that
    /// reported when dropped.
    struct StandInFixture {
        dir: TempDir,
        reports: Receiver<Report>,
        /// A write end of the fifo, so the reader never sees EOF between writers, and can be told to stop.
        stop: File,
        pids: Vec<i32>,
    }

    impl StandInFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
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
                pids: Vec::new(),
            }
        }

        fn fifo(&self) -> PathBuf {
            self.dir.path().join("report")
        }

        fn stand_in(&self) -> ProcessHostLauncher {
            stand_in(&self.fifo())
        }

        /// Signals the stand-in `pid` until it reports that it is alive, or `None` if it never does. Repeated,
        /// because `sh` can lose a trapped signal that arrives while it forks (between `started` and `wait`).
        fn ask_alive(&mut self, pid: i32) -> Option<Report> {
            let deadline = std::time::Instant::now() + TIMEOUT;
            while std::time::Instant::now() < deadline {
                // SAFETY: plain syscall.
                if unsafe { libc::kill(pid, libc::SIGUSR1) } != 0 {
                    return None;
                }
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
            let report = self
                .reports
                .recv_timeout(timeout)
                .unwrap_or_else(|_| panic!("the stand-in didn't report"));
            if let Report::Started { pid, .. } = report {
                self.pids.push(pid);
            }
            report
        }
    }

    impl Drop for StandInFixture {
        fn drop(&mut self) {
            // Stand-ins that reported after a failed test stopped reading.
            while let Ok(report) = self.reports.recv_timeout(Duration::from_millis(100)) {
                if let Report::Started { pid, .. } = report {
                    self.pids.push(pid);
                }
            }
            for &pid in &self.pids {
                // SAFETY: plain syscall. Each stand-in leads its own process group (`setsid`), its `sleep` included.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
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

        fn wait_started(&mut self, n: usize, timeout: Duration) {
            for _ in 0..n {
                assert!(matches!(self.next_within(timeout), Report::Started { .. }));
            }
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

    fn mkfifo(path: &Path) {
        let c_path = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    }

    fn stand_in(fifo: &Path) -> ProcessHostLauncher {
        ProcessHostLauncher::new(
            "/bin/sh",
            ["-c", STAND_IN, "stand-in", fifo.to_str().unwrap()],
        )
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
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "launcher::tests::launched_host_inherits_no_leaked_fd_or_ignored_signal",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(HYGIENE_VAR, "1")
            .env(FIFO_VAR, f.fifo())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
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
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let err = ProcessHostLauncher::new(&script, ["host"])
            .launch()
            .unwrap_err();
        let LaunchError::SpawnFailed(why) = err;
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
        let mut child = pty.slave.spawn_command(cmd).unwrap();
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
        let status = child.wait().unwrap();
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
}
