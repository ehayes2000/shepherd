//! `Commands` over `/bin/sh -c`, with `std::process` (decision 0011: no async runtime).
//!
//! Each command starts a new session (`setsid`): it leads a process group of its own and has no controlling terminal,
//! so nothing it runs can open `/dev/tty` to prompt. Stdin is `/dev/null`. Stdout and stderr are one pipe, interleaved
//! as written, which the calling thread reads while the command runs, keeping only the tail in memory: however much a
//! hook writes, nothing grows on disk. A helper thread waits for the shell to exit without reaping it and then wakes
//! the reader, which takes what is left in the pipe and returns, so a background process the command leaves behind
//! can't hold the call open. What that process writes afterwards is read and thrown away by a thread of its own until
//! the pipe closes, so it never blocks or dies writing.
//!
//! A command that runs past its timeout, or that [`Commands::cancel`] names, is killed with its whole process group,
//! and the call returns once the group is gone ([`process_group`]). Until the shell is reaped its pid (the group id)
//! can't be reused, so a cancel from another thread may kill the group at any time until then.

use std::collections::HashMap;
use std::io::{self, PipeReader, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use shepherd_core::ports::{
    CommandError, CommandExit, CommandOutput, CommandSpec, Commands, OUTPUT_TAIL, RunId,
};

use crate::process_group::{kill_group, wait_group_gone, wait_unreaped};

/// The shell every command runs under: portable, and not the user's login shell.
const SHELL: &str = "/bin/sh";

#[derive(Debug, Default)]
pub struct ShellCommands {
    /// Runs going on, and runs cancelled before they started, for [`Commands::cancel`].
    runs: Mutex<HashMap<RunId, Flight>>,
}

/// A run [`ShellCommands::runs`] knows of.
#[derive(Debug, Default)]
struct Flight {
    /// `false`: cancelled before it started.
    started: bool,
    cancelled: bool,
    /// The shell's process group, while the shell is unreaped (so the id can't be reused).
    pgid: Option<u32>,
}

/// A run going on, registered in [`ShellCommands::runs`] until it is dropped.
struct Registered<'a> {
    runs: &'a Mutex<HashMap<RunId, Flight>>,
    id: RunId,
}

impl Registered<'_> {
    /// Records that the shell runs as process group `pgid`. Whether the run is already cancelled, so the caller kills
    /// it.
    fn spawned(&self, pgid: u32) -> bool {
        let mut runs = lock(self.runs);
        let Some(run) = runs.get_mut(&self.id) else {
            return false;
        };
        run.pgid = Some(pgid);
        run.cancelled
    }

    /// Forgets the shell's process group, once it has exited and before it is reaped. Whether the run was cancelled.
    fn exited(&self) -> bool {
        let mut runs = lock(self.runs);
        let Some(run) = runs.get_mut(&self.id) else {
            return false;
        };
        run.pgid = None;
        run.cancelled
    }
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        lock(self.runs).remove(&self.id);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ShellCommands {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers run `id`, unless it was cancelled before it started (`None`, consuming that cancel).
    fn start(&self, id: RunId) -> Option<Registered<'_>> {
        let mut runs = lock(&self.runs);
        if runs.get(&id).is_some_and(|run| !run.started) {
            runs.remove(&id);
            return None;
        }
        let started = Flight {
            started: true,
            ..Flight::default()
        };
        runs.insert(id, started);
        Some(Registered {
            runs: &self.runs,
            id,
        })
    }
}

impl Commands for ShellCommands {
    fn run(&self, id: RunId, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        if !std::fs::metadata(&spec.cwd).is_ok_and(|m| m.is_dir()) {
            return Err(CommandError::NoSuchDir(spec.cwd.clone()));
        }
        let Some(registered) = self.start(id) else {
            return Ok(CommandOutput {
                exit: CommandExit::Cancelled,
                tail: Vec::new(),
            });
        };
        let failed = |what: &str, e: io::Error| CommandError::Failed(format!("{what}: {e}"));
        let no_pipe = |e| failed("cannot make its output pipe", e);
        let (mut output, stdout) = io::pipe().map_err(no_pipe)?;
        let stderr = stdout.try_clone().map_err(no_pipe)?;
        let (wake, exited) = io::pipe().map_err(no_pipe)?;
        let mut command = Command::new(SHELL);
        command
            .arg("-c")
            .arg(&spec.script)
            .current_dir(&spec.cwd)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        // SAFETY: `setsid` is async-signal-safe, and the closure touches nothing else.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| failed(&format!("cannot run {SHELL}"), e))?;
        // Our copies of the pipe's write end go with it, so only the command holds it open.
        drop(command);
        let pid = child.id();
        // Closes `exited` once the shell has exited, leaving it unreaped.
        let waiter = std::thread::Builder::new()
            .name("hook-wait".into())
            .spawn(move || {
                wait_unreaped(pid);
                drop(exited);
            });
        if let Err(e) = waiter {
            kill_group(pid);
            let _ = child.wait();
            wait_group_gone(pid);
            return Err(failed("cannot wait for it", e));
        }
        // A cancel kills the group from here on (or now, if it came first), until the shell has exited.
        if registered.spawned(pid) {
            kill_group(pid);
        }
        let mut tail = Tail::default();
        let (timed_out, open) = read_until_exit(&mut output, &wake, &mut tail, spec.timeout, pid);
        // The shell has exited, and is still unreaped: everything it wrote is in the pipe, and a cancel from now on
        // mustn't kill its group id.
        let cancelled = registered.exited();
        if open {
            take_waiting(&output, &mut tail);
        }
        let kill = timed_out || cancelled;
        if kill {
            // Again, for anything forked while the first kill was being delivered.
            kill_group(pid);
        }
        let status = child.wait();
        if kill {
            wait_group_gone(pid);
        }
        if open {
            discard_the_rest(output);
        }
        let status = status.map_err(|e| failed("cannot wait for it", e))?;
        let exit = match (status.code(), status.signal()) {
            (_, Some(_)) if cancelled => CommandExit::Cancelled,
            _ if timed_out => CommandExit::TimedOut,
            (Some(code), _) => CommandExit::Code(code),
            (None, Some(signal)) => CommandExit::Signal(signal),
            (None, None) => {
                return Err(CommandError::Failed(format!("it ended oddly: {status}")));
            }
        };
        Ok(CommandOutput {
            exit,
            tail: tail.into_bytes(),
        })
    }

    /// Kills the run's process group if its shell is running (the run's own thread then waits for the group to be
    /// gone); a run that hasn't spawned its shell yet kills it as it does.
    fn cancel(&self, id: RunId) {
        let mut runs = lock(&self.runs);
        let run = runs.entry(id).or_default();
        run.cancelled = true;
        if let Some(pgid) = run.pgid {
            kill_group(pgid);
        }
    }
}

/// The last [`OUTPUT_TAIL`] bytes of what was pushed, holding at most twice that.
#[derive(Default)]
struct Tail(Vec<u8>);

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
        if self.0.len() > 2 * OUTPUT_TAIL {
            let cut = self.0.len() - OUTPUT_TAIL;
            self.0.drain(..cut);
        }
    }

    fn into_bytes(mut self) -> Vec<u8> {
        let cut = self.0.len().saturating_sub(OUTPUT_TAIL);
        self.0.split_off(cut)
    }
}

/// Reads `output` into `tail` until `exited` closes (the shell has exited), killing group `pid` if `timeout` passes
/// first. Whether it timed out, and whether `output` is still open (something holds its write end).
fn read_until_exit(
    output: &mut PipeReader,
    exited: &PipeReader,
    tail: &mut Tail,
    timeout: Duration,
    pid: u32,
) -> (bool, bool) {
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut open = true;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let wait = (!timed_out).then(|| deadline.saturating_duration_since(Instant::now()));
        let fds = [exited.as_raw_fd(), output.as_raw_fd()];
        let ready = poll_readable(if open { &fds } else { &fds[..1] }, wait);
        if ready.get(1).copied().unwrap_or(false) {
            match output.read(&mut buf) {
                Ok(0) => open = false,
                Ok(n) => tail.push(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => open = false,
            }
        }
        if ready[0] {
            return (timed_out, open);
        }
        if !timed_out && Instant::now() >= deadline {
            kill_group(pid);
            timed_out = true;
        }
    }
}

/// Reads into `tail` what is waiting in `output` now, and no more: a background process may go on writing for ever.
fn take_waiting(output: &PipeReader, tail: &mut Tail) {
    let mut waiting: libc::c_int = 0;
    // SAFETY: FIONREAD writes one int, through a valid pointer, about an fd we hold open.
    if unsafe { libc::ioctl(output.as_raw_fd(), libc::FIONREAD, &mut waiting) } == -1 {
        return;
    }
    let mut left = usize::try_from(waiting).unwrap_or(0);
    let mut buf = [0u8; 16 * 1024];
    let mut reader = output;
    while left > 0 {
        let want = left.min(buf.len());
        match reader.read(&mut buf[..want]) {
            Ok(0) => return,
            Ok(n) => {
                tail.push(&buf[..n]);
                left -= n;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Reads `output` until it closes and throws it away, on a thread of its own, so a background process the command
/// left writing to it neither blocks nor gets SIGPIPE. If no thread can be started, `output` is just closed.
fn discard_the_rest(mut output: PipeReader) {
    let _ = std::thread::Builder::new()
        .name("hook-drain".into())
        .spawn(move || io::copy(&mut output, &mut io::sink()));
}

/// Which of `fds` are readable (or closed), waiting up to `timeout` (`None`: for ever) for one to be.
fn poll_readable(fds: &[RawFd], timeout: Option<Duration>) -> Vec<bool> {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let ms = timeout.map_or(-1, |t| {
        // Rounded up, so a wait never ends just short of a deadline and spins.
        libc::c_int::try_from(t.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
    });
    let nfds = libc::nfds_t::try_from(pfds.len()).unwrap_or(0);
    // SAFETY: `pfds` is a valid, exclusively borrowed array of `nfds` pollfds for the duration of the call.
    let ready = unsafe { libc::poll(pfds.as_mut_ptr(), nfds, ms) };
    if ready <= 0 {
        return vec![false; fds.len()];
    }
    pfds.iter().map(|p| p.revents != 0).collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use shepherd_core::ports::{CommandExit, CommandOutput, CommandSpec, Commands, RunId};
    use shepherd_testkit::contracts::commands::CommandsFixture;
    use tempfile::TempDir;

    use super::ShellCommands;

    /// A fresh port and a tempdir to run in. Kills a background child the contract's scripts left running.
    pub struct ShellFixture {
        port: ShellCommands,
        _tmp: TempDir,
        dir: PathBuf,
    }

    impl ShellFixture {
        pub fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = std::fs::canonicalize(tmp.path()).unwrap();
            Self {
                port: ShellCommands::new(),
                _tmp: tmp,
                dir,
            }
        }

        /// The pid the contract's scripts record in `child.pid`, once they have.
        fn child_pid(&self) -> Option<libc::pid_t> {
            let text = std::fs::read_to_string(self.dir.join("child.pid")).ok()?;
            text.trim().parse().ok()
        }

        /// The recorded child and its process group, if it is running: not exiting (`E`, its SIGKILL delivered) and
        /// not a zombie (`Z`) waiting for launchd (its parent once the hook's shell is gone) to reap it, which may be
        /// a moment after the port returns. Neither runs anything.
        fn running_child(&self) -> Option<(libc::pid_t, String)> {
            let pid = self.child_pid()?;
            let ps = std::process::Command::new("ps")
                .args(["-o", "stat=,pgid=", "-p", &pid.to_string()])
                .output()
                .ok()?;
            let line = String::from_utf8_lossy(&ps.stdout);
            let mut fields = line.split_whitespace();
            let (stat, pgid) = (fields.next()?, fields.next()?);
            (!stat.contains('Z') && !stat.contains('E')).then(|| (pid, pgid.to_owned()))
        }
    }

    impl Drop for ShellFixture {
        fn drop(&mut self) {
            if let Some((pid, _)) = self.running_child() {
                // SAFETY: plain syscall, on the child our own script started and recorded.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }

    impl CommandsFixture for ShellFixture {
        type Port = ShellCommands;

        fn port(&self) -> &ShellCommands {
            &self.port
        }

        fn dir(&self) -> &Path {
            &self.dir
        }

        /// Alive means running ([`ShellFixture::running_child`]). A live child must be in the shell's process group,
        /// which the shell leads: the port's group kill depends on it, and without this check a missing `setsid` would
        /// only show as the timeout case hanging for minutes.
        fn forked_child_alive(&self) -> Option<bool> {
            self.child_pid()?;
            let Some((_, pgid)) = self.running_child() else {
                return Some(false);
            };
            let shell = std::fs::read_to_string(self.dir.join("shell.pid")).unwrap();
            assert_eq!(
                pgid,
                shell.trim(),
                "the hook's child is not in the group the hook's shell leads"
            );
            Some(true)
        }

        /// Polls for `child.pid`, which the script writes once its child has started.
        fn wait_for_child(&self, limit: Duration) -> bool {
            let started = std::time::Instant::now();
            while self.child_pid().is_none() {
                if started.elapsed() > limit {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            true
        }
    }

    mod contract {
        use super::ShellFixture;

        shepherd_testkit::commands_contract!(ShellFixture::new());
    }

    fn run(f: &ShellFixture, script: &str) -> CommandOutput {
        let spec = CommandSpec {
            script: script.to_owned(),
            cwd: f.dir.clone(),
            env: vec![("T23_ADDED".to_owned(), "added".to_owned())],
            timeout: Duration::from_secs(60),
        };
        f.port.run(RunId(1), &spec).unwrap()
    }

    /// Set in the environment of the child test process [`stdin_probe`] runs in.
    const STDIN_PROBE: &str = "T23_STDIN_PROBE";

    /// Only does something in the child test process [`stdin_is_closed_whatever_the_runners_stdin`] starts: runs a
    /// hook that reads stdin, and prints what it saw.
    #[test]
    fn stdin_probe() {
        if std::env::var_os(STDIN_PROBE).is_none() {
            return;
        }
        let f = ShellFixture::new();
        let out = run(&f, "cat; printf done");
        let seen = format!("T23PROBE[{}]", String::from_utf8_lossy(&out.tail));
        std::io::Write::write_all(&mut std::io::stdout(), seen.as_bytes()).unwrap();
    }

    /// The contract's `stdin_is_closed` would pass with an inherited stdin whenever the test runner's own stdin is
    /// `/dev/null`. Here the runner (a child copy of this test binary) has a pipe holding data as its stdin, which a
    /// hook with an inherited stdin would read.
    #[test]
    fn stdin_is_closed_whatever_the_runners_stdin() {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["commands::tests::stdin_probe", "--exact", "--nocapture"])
            .env(STDIN_PROBE, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        std::io::Write::write_all(&mut stdin, b"data from the runner\n").unwrap();
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}");
        assert!(stdout.contains("T23PROBE[done]"), "{stdout}");
    }

    #[test]
    fn runs_under_bin_sh_whatever_the_login_shell() {
        let f = ShellFixture::new();
        assert_eq!(run(&f, "printf %s \"$0\"").tail, b"/bin/sh");
    }

    #[test]
    fn env_is_added_to_the_inherited_environment() {
        let f = ShellFixture::new();
        let path = std::env::var("PATH").unwrap();
        let out = run(&f, "printf '%s|%s' \"$PATH\" \"$T23_ADDED\"");
        assert_eq!(
            String::from_utf8(out.tail).unwrap(),
            format!("{path}|added")
        );
    }

    #[test]
    fn it_has_no_controlling_terminal() {
        let f = ShellFixture::new();
        let out = run(&f, "exec 3</dev/tty");
        assert_ne!(
            out.exit,
            CommandExit::Code(0),
            "{:?}",
            String::from_utf8_lossy(&out.tail)
        );
    }

    /// F33: output went to an unlinked temp file, which a chatty hook (or a background process it left writing) could
    /// grow without bound. Nothing a hook writes lands in a file now: only its tail is kept, in memory.
    #[test]
    fn its_output_never_goes_to_a_file() {
        let f = ShellFixture::new();
        let script =
            "if [ -f /dev/stdout ] || [ -f /dev/stderr ]; then printf file; else printf none; fi";
        assert_eq!(run(&f, script).tail, b"none");
    }

    /// A background process the hook leaves behind may write long after the call returned: it is neither blocked nor
    /// killed (SIGPIPE) for it. It waits for the test to say the call returned (`go`), writes, then records that it
    /// lived through it (`survived`).
    #[test]
    fn a_background_child_can_keep_writing_after_it_returns() {
        let f = ShellFixture::new();
        let script = "(while [ ! -f go ]; do sleep 0.01; done; head -c 1048576 /dev/zero; echo yes > survived) & \
                      echo $! > child.pid";
        let out = run(&f, script);
        assert_eq!(out.exit, CommandExit::Code(0));
        std::fs::write(f.dir.join("go"), b"").unwrap();
        let started = std::time::Instant::now();
        while !f.dir.join("survived").exists() {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the background child never got past writing"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
