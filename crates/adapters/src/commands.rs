//! `Commands` over `/bin/sh -c`, with `std::process` (decision 0011: no async runtime).
//!
//! Each command starts a new session (`setsid`): it leads a process group of its own and has no controlling terminal,
//! so nothing it runs can open `/dev/tty` to prompt. Stdin is `/dev/null`. Stdout and stderr both go to one unlinked
//! temp file rather than pipes, interleaved as written, so a background process the command leaves behind can't hold
//! the call open, and only the tail is read back. A command that runs past its timeout is killed with its whole
//! process group, and the call returns once the group is gone ([`process_group`]).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};

use shepherd_core::ports::{
    CommandError, CommandExit, CommandOutput, CommandSpec, Commands, OUTPUT_TAIL,
};
use wait_timeout::ChildExt;

use crate::process_group;

/// The shell every command runs under: portable, and not the user's login shell.
const SHELL: &str = "/bin/sh";

#[derive(Debug, Clone, Default)]
pub struct ShellCommands;

impl ShellCommands {
    pub fn new() -> Self {
        Self
    }
}

impl Commands for ShellCommands {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        if !std::fs::metadata(&spec.cwd).is_ok_and(|m| m.is_dir()) {
            return Err(CommandError::NoSuchDir(spec.cwd.clone()));
        }
        let failed = |what: &str, e: io::Error| CommandError::Failed(format!("{what}: {e}"));
        let no_file = |e| failed("cannot make its output file", e);
        let mut output = tempfile::tempfile().map_err(no_file)?;
        let stdout = output.try_clone().map_err(no_file)?;
        let stderr = output.try_clone().map_err(no_file)?;
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
        // Our copies of the output file's descriptors go with it, so only the command holds the file open.
        drop(command);
        let exit = match child.wait_timeout(spec.timeout) {
            Ok(Some(status)) => match (status.code(), status.signal()) {
                (Some(code), _) => CommandExit::Code(code),
                (None, Some(signal)) => CommandExit::Signal(signal),
                (None, None) => {
                    return Err(CommandError::Failed(format!("it ended oddly: {status}")));
                }
            },
            Ok(None) => {
                process_group::kill_and_reap(&mut child);
                CommandExit::TimedOut
            }
            Err(e) => {
                process_group::kill_and_reap(&mut child);
                return Err(failed("cannot wait for it", e));
            }
        };
        let tail = read_tail(&mut output).map_err(|e| failed("cannot read its output", e))?;
        Ok(CommandOutput { exit, tail })
    }
}

/// The last [`OUTPUT_TAIL`] bytes written to `file`, at most.
fn read_tail(file: &mut File) -> io::Result<Vec<u8>> {
    let len = file.seek(SeekFrom::End(0))?;
    let cap = u64::try_from(OUTPUT_TAIL).unwrap_or(u64::MAX);
    file.seek(SeekFrom::Start(len.saturating_sub(cap)))?;
    let mut tail = Vec::with_capacity(OUTPUT_TAIL);
    file.take(cap).read_to_end(&mut tail)?;
    Ok(tail)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use shepherd_core::ports::{CommandExit, CommandOutput, CommandSpec, Commands};
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
        f.port.run(&spec).unwrap()
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
}
