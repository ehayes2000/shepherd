//! `Forge` over the GitHub CLI. Each call runs, in the repo's checkout,
//!
//! ```text
//! gh pr list --head <branch> --state all --limit 20 --json number,state,statusCheckRollup
//! ```
//!
//! and parses its stdout as JSON with serde, never its text. The flags are gh's documented ones
//! (<https://cli.github.com/manual/gh_pr_list>): `--head` filters by head branch, `--state all` includes closed and
//! merged pull requests, and `--json` with those fields prints a JSON array, `[]` when nothing matches. gh lists them
//! newest first (`orderBy: {field: CREATED_AT, direction: DESC}` in its `PullRequestList` query), so the branch's pull
//! request is the first open one, else the first. `statusCheckRollup` is the head commit's checks, each a `CheckRun`
//! (`status`, `conclusion`) or a `StatusContext` (`state`), or `null` when it has none (`api/export_pr.go`); each is
//! bucketed as `gh pr checks` does (`pkg/cmd/pr/checks/aggregate.go`), except that a cancelled run and a startup
//! failure count as failed.
//!
//! Exit codes (<https://cli.github.com/manual/gh_help_exit-codes>): 0 is success, 4 means gh needs authentication
//! ([`ForgeError::Unavailable`], as is gh not being installed), anything else is [`ForgeError::Other`] with its stderr.
//!
//! # Never waiting on a person, never touching the user's gh setup
//!
//! gh starts a new session (`setsid`), so it has no controlling terminal; stdin is `/dev/null`; and the environment
//! turns prompts off: `GH_PROMPT_DISABLED=1` (<https://cli.github.com/manual/gh_help_environment>), plus an empty
//! `GIT_ASKPASS` and `GIT_TERMINAL_PROMPT=0` for the git gh runs to read the repo's remotes. `GH_NO_UPDATE_NOTIFIER`,
//! `GH_NO_EXTENSION_UPDATE_NOTIFIER`, `GH_SPINNER_DISABLED` and `NO_COLOR` keep it quiet. `GH_REPO` (which would point gh
//! at another repo than the checkout), `CLICOLOR_FORCE` and `GH_FORCE_TTY` (which would colour or format the JSON) and the git variables that would point git elsewhere are removed. gh reads its own
//! config and credentials as it always does; this port never reads or writes them. A call that runs past its timeout is
//! killed with its whole process group. Output goes to temp files rather than pipes.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use shepherd_core::ports::forge::{Check, Forge, ForgeError, Pr, PrState, rollup};
use shepherd_core::ports::{BranchName, RepoRef};
use wait_timeout::ChildExt;

use crate::process_group::{kill_group, wait_group_gone, wait_unreaped};

/// How long a call may take before gh is killed: it waits on the network.
pub const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// How many of a branch's pull requests to list, newest first.
const LIMIT: &str = "20";
/// The most of gh's stdout read back. Twenty pull requests with a hundred checks each are far less.
const MAX_OUTPUT: u64 = 8 * 1024 * 1024;
/// gh's exit code when a command needs authentication.
const NEEDS_AUTH: i32 = 4;

/// Removed from gh's environment: they would point it, or the git it runs, at something else than the checkout, or
/// make it print something other than plain JSON.
const REMOVED_ENV: &[&str] = &[
    "GH_REPO",
    // Would colour `--json` output (or format it for a terminal), which then doesn't parse.
    "CLICOLOR_FORCE",
    "GH_FORCE_TTY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];
/// Prefixes of the numbered variables that go with `GIT_CONFIG_COUNT`.
const REMOVED_ENV_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

#[derive(Debug, Clone)]
pub struct GhForge {
    /// Where to look for `gh`, instead of the inherited `PATH`.
    search_path: Option<OsString>,
    /// `Some(vars)`: gh gets these as if inherited, instead of this process's environment (for tests).
    inherited: Option<Vec<(OsString, OsString)>>,
    timeout: Duration,
}

impl Default for GhForge {
    fn default() -> Self {
        Self::new()
    }
}

impl GhForge {
    /// Runs the `gh` found on the inherited `PATH`.
    pub fn new() -> Self {
        Self {
            search_path: None,
            inherited: None,
            timeout: GH_TIMEOUT,
        }
    }

    /// Looks for `gh` on `path` instead, which is also gh's `PATH`: for tests, with a stand-in `gh`.
    pub fn with_search_path(mut self, path: impl Into<OsString>) -> Self {
        self.search_path = Some(path.into());
        self
    }

    /// Gives gh `vars` as its inherited environment instead of this process's, cleaned the same way: for tests of a
    /// hostile environment.
    pub fn with_inherited_env(mut self, vars: Vec<(OsString, OsString)>) -> Self {
        self.inherited = Some(vars);
        self
    }

    /// Kills gh once a call has taken `timeout`.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn command(&self, repo: &RepoRef, branch: &BranchName) -> Command {
        let mut command = Command::new("gh");
        command
            .args(["pr", "list", "--head"])
            .arg(branch.as_str())
            .args(["--state", "all", "--limit", LIMIT])
            .args(["--json", "number,state,statusCheckRollup"])
            .current_dir(repo.as_path())
            .stdin(Stdio::null());
        let inherited = match &self.inherited {
            Some(vars) => {
                command.env_clear();
                vars.clone()
            }
            None => std::env::vars_os().collect(),
        };
        for (key, value) in inherited {
            let key_str = key.to_string_lossy();
            if REMOVED_ENV.contains(&key_str.as_ref())
                || REMOVED_ENV_PREFIXES.iter().any(|p| key_str.starts_with(p))
            {
                command.env_remove(&key);
            } else if self.inherited.is_some() {
                command.env(&key, value);
            }
        }
        if let Some(path) = &self.search_path {
            command.env("PATH", path);
        }
        command
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env("NO_COLOR", "1")
            // Empty, not unset: git then skips `core.askPass` and `SSH_ASKPASS` too.
            .env("GIT_ASKPASS", "")
            .env("GIT_TERMINAL_PROMPT", "0");
        // SAFETY: `setsid` is async-signal-safe, and the closure touches nothing else.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    }
}

impl Forge for GhForge {
    fn pr_for_branch(&self, repo: &RepoRef, branch: &BranchName) -> Result<Option<Pr>, ForgeError> {
        let other = |what: &str, e: io::Error| ForgeError::Other(format!("{what}: {e}"));
        let no_file = |e| other("cannot make gh's output file", e);
        let mut stdout = tempfile::tempfile().map_err(no_file)?;
        let mut stderr = tempfile::tempfile().map_err(no_file)?;
        let mut command = self.command(repo, branch);
        command
            .stdout(stdout.try_clone().map_err(no_file)?)
            .stderr(stderr.try_clone().map_err(no_file)?);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // `current_dir` missing also spawns as NotFound: only a missing gh is unavailable.
                if !repo.as_path().is_dir() {
                    return Err(ForgeError::Other(format!("{repo} is not a directory")));
                }
                return Err(ForgeError::Unavailable("gh is not installed".to_owned()));
            }
            Err(e) => return Err(other("cannot run gh", e)),
        };
        drop(command);
        let status = match child.wait_timeout(self.timeout) {
            Ok(Some(status)) => status,
            Ok(None) => {
                kill_and_reap(&mut child);
                return Err(ForgeError::Timeout);
            }
            Err(e) => {
                kill_and_reap(&mut child);
                return Err(other("cannot wait for gh", e));
            }
        };
        match status.code() {
            Some(0) => {}
            Some(NEEDS_AUTH) => {
                let why = read_text(&mut stderr).unwrap_or_default();
                return Err(ForgeError::Unavailable(format!(
                    "gh is not signed in: {why}"
                )));
            }
            _ => {
                let why = read_text(&mut stderr).unwrap_or_default();
                return Err(ForgeError::Other(format!("gh failed ({status}): {why}")));
            }
        }
        let out = read_all(&mut stdout).map_err(|e| other("cannot read gh's output", e))?;
        let listed: Vec<GhPr> = serde_json::from_slice(&out)
            .map_err(|e| ForgeError::Other(format!("gh printed unreadable JSON: {e}")))?;
        let newest = listed.first().cloned();
        let chosen = listed
            .into_iter()
            .find(|pr| pr.state == GhState::Open)
            .or(newest);
        Ok(chosen.map(GhPr::into_pr))
    }
}

/// Kills gh's whole process group and reaps gh, once nothing in the group is left. gh is unreaped until then (a
/// `wait_timeout` that timed out doesn't reap), so its pid, the group id, can't have been reused.
fn kill_and_reap(child: &mut Child) {
    let pid = child.id();
    kill_group(pid);
    let _ = wait_unreaped(pid);
    // Again, for anything forked while the first kill was being delivered.
    kill_group(pid);
    let _ = child.wait();
    wait_group_gone(pid);
}

/// gh's stdout, at most [`MAX_OUTPUT`] bytes.
fn read_all(file: &mut File) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut out = Vec::new();
    file.take(MAX_OUTPUT + 1).read_to_end(&mut out)?;
    if out.len() as u64 > MAX_OUTPUT {
        return Err(io::Error::other("gh printed too much"));
    }
    Ok(out)
}

/// gh's stderr, trimmed, as text for the user.
fn read_text(file: &mut File) -> io::Result<String> {
    Ok(String::from_utf8_lossy(&read_all(file)?).trim().to_owned())
}

/// One element of `gh pr list --json number,state,statusCheckRollup`.
#[derive(Debug, Clone, Deserialize)]
struct GhPr {
    number: u64,
    state: GhState,
    #[serde(rename = "statusCheckRollup", default)]
    checks: Option<Vec<GhCheck>>,
}

/// A pull request's `state` (GraphQL `PullRequestState`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum GhState {
    Open,
    Closed,
    Merged,
    #[serde(untagged)]
    Other(String),
}

/// A `CheckRun` (`status`, and `conclusion` once completed) or a `StatusContext` (`state`).
#[derive(Debug, Clone, Deserialize)]
struct GhCheck {
    #[serde(default)]
    state: Option<CheckValue>,
    #[serde(default)]
    status: Option<CheckValue>,
    #[serde(default)]
    conclusion: Option<CheckValue>,
}

/// Every value of the fields above that matters. The rest (`QUEUED`, `IN_PROGRESS`, `PENDING`, `EXPECTED`, `""`
/// for a run not yet concluded, ...) are pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum CheckValue {
    Completed,
    Success,
    Neutral,
    Skipped,
    Error,
    Failure,
    TimedOut,
    ActionRequired,
    Cancelled,
    StartupFailure,
    #[serde(other)]
    Unfinished,
}

impl GhCheck {
    fn check(&self) -> Check {
        // As `gh pr checks` derives it: a status context's state, else a completed run's conclusion, else its status.
        let value = self.state.or(match self.status {
            Some(CheckValue::Completed) => self.conclusion,
            status => status,
        });
        match value {
            Some(CheckValue::Success | CheckValue::Neutral | CheckValue::Skipped) => Check::Passed,
            Some(
                CheckValue::Error
                | CheckValue::Failure
                | CheckValue::TimedOut
                | CheckValue::ActionRequired
                | CheckValue::Cancelled
                | CheckValue::StartupFailure,
            ) => Check::Failed,
            Some(CheckValue::Completed | CheckValue::Unfinished) | None => Check::Pending,
        }
    }
}

impl GhPr {
    fn into_pr(self) -> Pr {
        let checks = rollup(self.checks.iter().flatten().map(GhCheck::check));
        let state = match self.state {
            GhState::Open => PrState::Open,
            GhState::Closed => PrState::Closed,
            GhState::Merged => PrState::Merged,
            GhState::Other(other) => PrState::Other(other.to_lowercase()),
        };
        Pr {
            number: self.number,
            state,
            checks,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use serde_json::{Value, json};
    use shepherd_core::ports::forge::{Check, Forge, ForgeError, Pr, PrChecks, PrState};
    use shepherd_core::ports::{BranchName, RepoRef};
    use shepherd_testkit::contracts::forge::ForgeFixture;
    use tempfile::TempDir;

    use super::GhForge;

    /// A stand-in `gh`, never the real one. It records its arguments, environment, cwd, pid and process group in its
    /// directory, then answers by `mode`: as gh does for `pr list --head <branch> --json ...` (the JSON in
    /// `prs/<branch>.json`, else `[]`), signed out (exit 4), failing (exit 1), or hanging (a `sleep` it records).
    const STAND_IN: &str = r#"#!/bin/sh
dir="${0%/*}/.."
mode="$(/bin/cat "$dir/mode")"
if [ "$mode" = hanging ]; then /bin/sleep 300 & echo $! > "$dir/sleep.pid"; wait; exit 0; fi
printf '%s\n' "$@" > "$dir/args"
/usr/bin/env > "$dir/env"
pwd -P > "$dir/cwd"
echo "$$ $(/bin/ps -o pgid= -p $$)" > "$dir/group"
if (: < /dev/tty) 2>/dev/null; then echo yes > "$dir/tty"; else echo no > "$dir/tty"; fi
if [ /dev/fd/0 -ef /dev/null ]; then echo null > "$dir/stdin"; else echo other > "$dir/stdin"; fi
case "$mode" in
  flooding) printf '['; /bin/dd if=/dev/zero bs=1048576 count=9 2>/dev/null | /usr/bin/tr '\0' ' '; printf ']'; exit 0 ;;
  signed-out) echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4 ;;
  failing) echo 'HTTP 502: Bad Gateway' >&2; exit 1 ;;
esac
head=""
while [ $# -gt 0 ]; do
  if [ "$1" = "--head" ]; then head="$2"; fi
  shift
done
if [ -f "$dir/prs/$head.json" ]; then /bin/cat "$dir/prs/$head.json"; else echo '[]'; fi
"#;

    /// How long a call to the stand-in may take, except where a case wants a timeout.
    const LIVENESS: Duration = Duration::from_secs(120);

    /// A tempdir with `bin/gh` (the stand-in), its data, and a checkout directory to run in.
    pub struct GhFixture {
        port: GhForge,
        tmp: TempDir,
        /// Each branch's pull requests as gh prints them, newest first.
        prs: Vec<(String, Value)>,
    }

    impl GhFixture {
        pub fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let bin = tmp.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(tmp.path().join("prs")).unwrap();
            std::fs::create_dir_all(tmp.path().join("checkout")).unwrap();
            let gh = bin.join("gh");
            std::fs::write(&gh, STAND_IN).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let fixture = Self {
                // A generous liveness bound: under parallel load a shell script can take long to start.
                // Nothing of this process's environment reaches the stand-in, which writes what it gets to disk.
                port: GhForge::new()
                    .with_search_path(&bin)
                    .with_inherited_env(Vec::new())
                    .with_timeout(LIVENESS),
                tmp,
                prs: Vec::new(),
            };
            fixture.set_mode("answering");
            fixture
        }

        fn dir(&self) -> &Path {
            self.tmp.path()
        }

        fn set_mode(&self, mode: &str) {
            std::fs::write(self.dir().join("mode"), mode).unwrap();
        }

        /// Writes `json` as what gh prints for `branch`.
        fn answer(&self, branch: &str, json: &Value) {
            let path = self.dir().join("prs").join(format!("{branch}.json"));
            std::fs::write(path, serde_json::to_vec(json).unwrap()).unwrap();
        }

        fn recorded(&self, name: &str) -> String {
            std::fs::read_to_string(self.dir().join(name)).unwrap()
        }

        fn ask(&self, branch: &str) -> Result<Option<Pr>, ForgeError> {
            self.port
                .pr_for_branch(&self.repo(), &BranchName::new(branch))
        }
    }

    /// A `CheckRun` as gh exports it.
    fn run(status: &str, conclusion: &str) -> Value {
        json!({ "__typename": "CheckRun", "name": "ci", "workflowName": "CI", "status": status,
                "conclusion": conclusion, "startedAt": "2026-10-29T12:00:00Z",
                "completedAt": "0001-01-01T00:00:00Z", "detailsUrl": "https://example.invalid/run" })
    }

    /// A `StatusContext` as gh exports it.
    fn context(state: &str) -> Value {
        json!({ "__typename": "StatusContext", "context": "ci/legacy", "state": state,
                "targetUrl": "https://example.invalid/status", "startedAt": "2026-10-29T12:00:00Z" })
    }

    fn gh_check(check: Check) -> Value {
        match check {
            Check::Passed => run("COMPLETED", "SUCCESS"),
            Check::Failed => run("COMPLETED", "FAILURE"),
            Check::Pending => run("IN_PROGRESS", ""),
        }
    }

    fn gh_state(state: &PrState) -> &'static str {
        match state {
            PrState::Open => "OPEN",
            PrState::Closed => "CLOSED",
            PrState::Merged => "MERGED",
            PrState::Other(_) => "SOMETHING_NEW",
        }
    }

    impl ForgeFixture for GhFixture {
        type Port = GhForge;

        fn port(&self) -> &GhForge {
            &self.port
        }

        fn repo(&self) -> RepoRef {
            RepoRef::new(self.dir().join("checkout"))
        }

        fn add_pr(&mut self, branch: &str, number: u64, state: PrState, checks: &[Check]) {
            let rollup = match checks {
                [] => Value::Null,
                checks => checks.iter().copied().map(gh_check).collect(),
            };
            let pr =
                json!({ "number": number, "state": gh_state(&state), "statusCheckRollup": rollup });
            self.prs.insert(0, (branch.to_owned(), pr));
            let listed: Vec<Value> = self
                .prs
                .iter()
                .filter(|(head, _)| head == branch)
                .map(|(_, pr)| pr.clone())
                .collect();
            self.answer(branch, &Value::Array(listed));
        }

        fn uninstall(&mut self) {
            std::fs::remove_file(self.dir().join("bin/gh")).unwrap();
        }

        fn sign_out(&mut self) {
            self.set_mode("signed-out");
        }

        fn break_forge(&mut self) {
            self.set_mode("failing");
        }

        fn hang(&mut self) {
            self.set_mode("hanging");
            self.port = self.port.clone().with_timeout(Duration::from_millis(500));
        }
    }

    /// Whether `pid` is running: not exited, nor a zombie waiting to be reaped.
    fn sleep_alive(pid: libc::pid_t) -> bool {
        let Ok(ps) = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
        else {
            return false;
        };
        let stat = String::from_utf8_lossy(&ps.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.contains('Z') && !stat.contains('E')
    }

    mod contract {
        use super::GhFixture;

        shepherd_testkit::forge_contract!(GhFixture::new());
    }

    fn checks_of(rollup: Value) -> PrChecks {
        let f = GhFixture::new();
        f.answer(
            "b",
            &json!([{ "number": 1, "state": "OPEN", "statusCheckRollup": rollup }]),
        );
        f.ask("b").unwrap().unwrap().checks
    }

    #[test]
    fn gh_is_asked_with_its_documented_json_flags_in_the_checkout() {
        let f = GhFixture::new();
        f.ask("fix-login").unwrap();
        let args: Vec<String> = f.recorded("args").lines().map(str::to_owned).collect();
        assert_eq!(
            args,
            [
                "pr",
                "list",
                "--head",
                "fix-login",
                "--state",
                "all",
                "--limit",
                "20",
                "--json",
                "number,state,statusCheckRollup"
            ]
        );
        let checkout = std::fs::canonicalize(f.repo().as_path()).unwrap();
        assert_eq!(PathBuf::from(f.recorded("cwd").trim()), checkout);
    }

    #[test]
    fn gh_never_prompts_and_never_gets_a_terminal() {
        let f = GhFixture::new();
        f.ask("b").unwrap();
        let env = f.recorded("env");
        for var in [
            "GH_PROMPT_DISABLED=1",
            "GIT_ASKPASS=",
            "GIT_TERMINAL_PROMPT=0",
            "GH_NO_UPDATE_NOTIFIER=1",
        ] {
            assert!(env.lines().any(|l| l == var), "{var} not in {env}");
        }
        assert_eq!(f.recorded("tty").trim(), "no");
        let group = f.recorded("group");
        let mut ids = group.split_whitespace();
        assert_eq!(ids.next(), ids.next(), "gh leads its own process group");
    }

    #[test]
    fn checks_with_no_contexts_are_none() {
        assert_eq!(checks_of(json!([])), PrChecks::None);
    }

    #[test]
    fn neutral_and_skipped_runs_and_successful_statuses_pass() {
        let rollup = json!([
            run("COMPLETED", "NEUTRAL"),
            run("COMPLETED", "SKIPPED"),
            context("SUCCESS")
        ]);
        assert_eq!(checks_of(rollup), PrChecks::Passing);
    }

    #[test]
    fn queued_waiting_and_expected_checks_are_pending() {
        for pending in [
            run("QUEUED", ""),
            run("WAITING", ""),
            run("PENDING", ""),
            run("REQUESTED", ""),
            context("PENDING"),
            context("EXPECTED"),
        ] {
            let rollup = json!([run("COMPLETED", "SUCCESS"), pending.clone()]);
            assert_eq!(checks_of(rollup), PrChecks::Pending, "{pending}");
        }
    }

    #[test]
    fn every_kind_of_failure_fails() {
        for failed in [
            run("COMPLETED", "TIMED_OUT"),
            run("COMPLETED", "ACTION_REQUIRED"),
            run("COMPLETED", "CANCELLED"),
            run("COMPLETED", "STARTUP_FAILURE"),
            context("ERROR"),
            context("FAILURE"),
        ] {
            let rollup = json!([run("IN_PROGRESS", ""), failed.clone()]);
            assert_eq!(checks_of(rollup), PrChecks::Failing, "{failed}");
        }
    }

    #[test]
    fn a_state_gh_adds_later_is_kept() {
        let f = GhFixture::new();
        f.answer(
            "b",
            &json!([{ "number": 3, "state": "QUEUED_FOR_MERGE", "statusCheckRollup": null }]),
        );
        assert_eq!(
            f.ask("b").unwrap().map(|pr| pr.state),
            Some(PrState::Other("queued_for_merge".to_owned()))
        );
    }

    #[test]
    fn output_that_is_not_json_is_an_error() {
        let f = GhFixture::new();
        std::fs::write(f.dir().join("prs/b.json"), "no pull requests match").unwrap();
        let got = f.ask("b");
        assert!(matches!(got, Err(ForgeError::Other(_))), "{got:?}");
    }

    #[test]
    fn a_checkout_that_is_gone_is_an_error_not_a_missing_gh() {
        let f = GhFixture::new();
        std::fs::remove_dir(f.dir().join("checkout")).unwrap();
        let got = f.ask("b");
        assert!(matches!(got, Err(ForgeError::Other(_))), "{got:?}");
    }

    #[test]
    fn gh_that_never_answers_is_killed_with_what_it_started() {
        let mut f = GhFixture::new();
        f.hang();
        f.port = f.port.clone().with_timeout(Duration::from_secs(5));
        // Under heavy load the timeout can pass before the stand-in has started its sleep: ask again until it has.
        // Each call is killed with whatever it started either way.
        for _ in 0..10 {
            assert_eq!(f.ask("b"), Err(ForgeError::Timeout));
            // Empty if it was killed between creating the file and writing to it.
            let recorded = std::fs::read_to_string(f.dir().join("sleep.pid")).unwrap_or_default();
            if let Ok(pid) = recorded.trim().parse::<libc::pid_t>() {
                assert!(!sleep_alive(pid), "the stand-in's sleep outlived the call");
                return;
            }
        }
        panic!("the stand-in never got as far as starting its sleep");
    }

    #[test]
    fn variables_that_point_gh_or_git_elsewhere_are_not_passed_on() {
        let mut f = GhFixture::new();
        let var = |k: &str, v: &str| (k.into(), v.into());
        f.port = f.port.clone().with_inherited_env(vec![
            var("GH_REPO", "someone/else"),
            var("GIT_DIR", "/elsewhere/.git"),
            var("GIT_CONFIG_KEY_0", "core.askPass"),
            var("CLICOLOR_FORCE", "1"),
            var("GH_FORCE_TTY", "100%"),
            var("T27_KEPT", "kept"),
        ]);
        f.ask("b").unwrap();
        let env = f.recorded("env");
        for gone in [
            "GH_REPO=",
            "GIT_DIR=",
            "GIT_CONFIG_KEY_0=",
            "CLICOLOR_FORCE=",
            "GH_FORCE_TTY=",
        ] {
            assert!(
                !env.lines().any(|l| l.starts_with(gone)),
                "{gone} reached gh"
            );
        }
        assert!(env.lines().any(|l| l == "T27_KEPT=kept"), "{env}");
        // Nothing of this process's own environment is passed on either, beyond what it was given.
        assert!(std::env::var_os("HOME").is_some(), "the test run has HOME");
        assert!(!env.lines().any(|l| l.starts_with("HOME=")), "{env}");
    }

    /// Set for [`gh_stdin_helper`]: the [`GhFixture`] directory it asks gh in.
    const STDIN_HELPER_DIR: &str = "SHEPHERD_TEST_GH_STDIN_HELPER_DIR";

    /// Not a case on its own: [`gh_reads_nothing_from_stdin`] runs this test binary with only this test, its stdin a
    /// pipe with data, and [`STDIN_HELPER_DIR`] set. Then it asks gh as the port does. Otherwise it does nothing.
    #[test]
    fn gh_stdin_helper() {
        let Some(dir) = std::env::var_os(STDIN_HELPER_DIR) else {
            return;
        };
        let dir = PathBuf::from(dir);
        GhForge::new()
            .with_search_path(dir.join("bin"))
            .with_inherited_env(Vec::new())
            .with_timeout(LIVENESS)
            .pr_for_branch(&RepoRef::new(dir.join("checkout")), &BranchName::new("b"))
            .unwrap();
    }

    #[test]
    fn gh_reads_nothing_from_stdin() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // Whatever this test run's own stdin is (`/dev/null` under the gate), the port runs in a process whose stdin
        // is a pipe with data in it: gh must not get it.
        let f = GhFixture::new();
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "forge::tests::gh_stdin_helper", "--nocapture"])
            .args(["--test-threads", "1"])
            .env(STDIN_HELPER_DIR, f.dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        helper.stdin.take().unwrap().write_all(b"yes\n").unwrap();
        let out = helper.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(f.recorded("stdin").trim(), "null");
    }

    #[test]
    fn output_past_the_cap_is_an_error_even_if_it_would_parse() {
        let f = GhFixture::new();
        // Nine MiB of JSON whitespace around an empty list: it parses, but is more than the port reads.
        f.set_mode("flooding");
        let got = f.ask("b");
        assert!(
            matches!(&got, Err(ForgeError::Other(why)) if why.contains("too much")),
            "{got:?}"
        );
    }
}
