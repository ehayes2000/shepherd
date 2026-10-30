//! A session's pull request (T27): how checks roll up, how the host polls the `Forge` port from `Tick`s
//! (`core::host::pr`), and how `_shepherd/session/pr` folds into the session list.

use std::path::PathBuf;

use serde_json::json;
use shepherd_core::acpp::{Pr, PrChecks, PrState, ShepherdFrame, Source, SourceKind, Timestamp};
use shepherd_core::host::pr::{PrEvent, PrWatch};
use shepherd_core::host::{HostCommand, HostEvent, WorkspaceAsk};
use shepherd_core::ids::SessionId;
use shepherd_core::ports::forge::{Check, ForgeError, rollup};
use shepherd_core::ports::{BranchName, RepoRef};
use shepherd_testkit::builders::{ClientDriver, HostDriver, shell_session_spec};

/// `2026-10-29T12:00:00Z` plus `secs`.
fn t(secs: i64) -> Timestamp {
    let base: jiff::Timestamp = "2026-10-29T12:00:00Z"
        .parse()
        .unwrap_or_else(|e| panic!("{e}"));
    let at = base
        .checked_add(jiff::SignedDuration::from_secs(secs))
        .unwrap_or_else(|e| panic!("{e}"));
    Timestamp::parse(at.to_string()).unwrap_or_else(|e| panic!("{e}"))
}

fn instant(secs: i64) -> jiff::Timestamp {
    t(secs).instant()
}

fn sid(id: &str) -> SessionId {
    SessionId::new(id)
}

fn pr(number: u64, checks: PrChecks) -> Pr {
    Pr {
        number,
        state: PrState::Open,
        checks,
    }
}

// ---------- checks ----------

#[test]
fn no_checks_roll_up_to_none() {
    assert_eq!(rollup([]), PrChecks::None);
}

#[test]
fn checks_that_all_passed_roll_up_to_passing() {
    assert_eq!(rollup([Check::Passed, Check::Passed]), PrChecks::Passing);
}

#[test]
fn a_check_still_running_makes_them_pending() {
    assert_eq!(rollup([Check::Passed, Check::Pending]), PrChecks::Pending);
}

#[test]
fn a_failed_check_makes_them_failing_even_with_others_pending() {
    let checks = [Check::Pending, Check::Failed, Check::Passed];
    assert_eq!(rollup(checks), PrChecks::Failing);
}

// ---------- the watch ----------

/// A host log and a [`PrWatch`], run the way the host runtime runs them: every entry appended is applied to the
/// watch, and so is every `session/pr` the watch appends.
struct Driver {
    host: HostDriver,
    watch: PrWatch,
    applied: usize,
}

impl Driver {
    /// A host whose workspace is `/src/app`, the clock pinned at `t(0)`.
    fn new() -> Self {
        let mut host = HostDriver::new();
        host.pin_ts(t(0).as_str());
        let ask = WorkspaceAsk::Select(host.reply_to());
        host.handle(HostEvent::WorkspaceProbed {
            ask,
            repo: Ok(PathBuf::from("/src/app")),
        });
        let mut driver = Self {
            host,
            watch: PrWatch::new(),
            applied: 0,
        };
        driver.sync();
        driver
    }

    /// Applies the entries the watch hasn't seen yet.
    fn sync(&mut self) {
        for env in &self.host.log()[self.applied..] {
            self.watch.apply(env);
        }
        self.applied = self.host.log().len();
    }

    /// Session `id` in the workspace, its worktree (`/wt/<id>` on branch `<id>`) made at `t(at)`.
    fn session_with_worktree(&mut self, id: &str, at: i64) {
        self.host.pin_ts(t(at).as_str());
        self.host.new_session(id);
        self.host.worktree_ready(id);
        self.sync();
    }

    /// Runs `commands`' appends through the log, as the runtime does, and returns the commands.
    fn run(&mut self, commands: Vec<HostCommand>) -> Vec<HostCommand> {
        for command in &commands {
            if let HostCommand::Append(draft) = command {
                self.host.append_draft((**draft).clone());
            }
        }
        self.sync();
        commands
    }

    fn tick(&mut self, at: i64) -> Vec<HostCommand> {
        let commands = self.watch.handle(PrEvent::Tick { at: t(at) });
        self.run(commands)
    }

    fn answer(&mut self, id: &str, result: Result<Option<Pr>, ForgeError>) -> Vec<HostCommand> {
        let commands = self.watch.handle(PrEvent::Checked {
            session: sid(id),
            result,
        });
        self.run(commands)
    }

    /// A tick at `t(at)` that asks about `id`, answered with `result`. Returns the answer's commands.
    fn poll(
        &mut self,
        id: &str,
        at: i64,
        result: Result<Option<Pr>, ForgeError>,
    ) -> Vec<HostCommand> {
        assert_eq!(self.tick(at), vec![check(id)], "a check of {id} at {at}s");
        self.answer(id, result)
    }
}

fn check(id: &str) -> HostCommand {
    HostCommand::CheckPr {
        session: sid(id),
        repo: RepoRef::new(format!("/wt/{id}")),
        branch: BranchName::new(id),
    }
}

/// The `session/pr` an append command carries, if it is one.
fn appended_pr(commands: &[HostCommand]) -> Vec<(SessionId, Option<Pr>)> {
    commands
        .iter()
        .filter_map(|c| match c {
            HostCommand::Append(draft) => {
                let shepherd_core::acpp::Frame::Shepherd(m) = &draft.frame else {
                    return None;
                };
                let ShepherdFrame::SessionPr(pr) = m.body() else {
                    return None;
                };
                assert_eq!(draft.source.kind, SourceKind::Shepherd);
                Some((draft.session.clone()?, pr.pr.clone()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_new_worktree_is_first_checked_a_poll_interval_after_it_was_made() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 10);
    assert_eq!(d.watch.deadline(), Some(instant(70)));
    assert_eq!(d.tick(69), vec![]);
    assert_eq!(d.tick(70), vec![check("s1")]);
}

#[test]
fn a_session_is_never_asked_about_twice_while_a_check_is_running() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    assert_eq!(d.tick(60), vec![check("s1")]);
    assert_eq!(d.watch.deadline(), None);
    assert_eq!(d.tick(600), vec![]);
}

#[test]
fn the_next_check_is_a_poll_interval_after_the_last_call() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.poll("s1", 65, Ok(None));
    assert_eq!(d.watch.deadline(), Some(instant(125)));
    assert_eq!(d.tick(124), vec![]);
    assert_eq!(d.tick(125), vec![check("s1")]);
}

#[test]
fn a_pr_found_is_appended_as_session_pr() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    let found = pr(123, PrChecks::Pending);
    let commands = d.poll("s1", 60, Ok(Some(found.clone())));
    assert_eq!(appended_pr(&commands), vec![(sid("s1"), Some(found))]);
}

#[test]
fn the_same_pr_again_appends_nothing() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.poll("s1", 60, Ok(Some(pr(123, PrChecks::Pending))));
    let again = d.poll("s1", 120, Ok(Some(pr(123, PrChecks::Pending))));
    assert_eq!(again, vec![]);
}

#[test]
fn a_change_in_its_checks_is_appended() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.poll("s1", 60, Ok(Some(pr(123, PrChecks::Pending))));
    let commands = d.poll("s1", 120, Ok(Some(pr(123, PrChecks::Passing))));
    assert_eq!(
        appended_pr(&commands),
        vec![(sid("s1"), Some(pr(123, PrChecks::Passing)))]
    );
}

#[test]
fn a_pr_that_is_gone_is_appended_as_none() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.poll("s1", 60, Ok(Some(pr(123, PrChecks::Passing))));
    let commands = d.poll("s1", 120, Ok(None));
    assert_eq!(appended_pr(&commands), vec![(sid("s1"), None)]);
}

#[test]
fn no_pr_while_none_was_known_appends_nothing() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    assert_eq!(d.poll("s1", 60, Ok(None)), vec![]);
}

#[test]
fn a_forge_that_is_unavailable_is_shown_as_no_pr() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.poll("s1", 60, Ok(Some(pr(123, PrChecks::Passing))));
    let gone = ForgeError::Unavailable("gh is not installed".to_owned());
    let commands = d.poll("s1", 120, Err(gone.clone()));
    assert_eq!(appended_pr(&commands), vec![(sid("s1"), None)]);
}

#[test]
fn an_unavailable_forge_with_no_pr_known_appends_nothing() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    let gone = ForgeError::Unavailable("not signed in".to_owned());
    assert_eq!(d.poll("s1", 60, Err(gone)), vec![]);
}

#[test]
fn a_failed_check_keeps_the_pr_last_shown() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.poll("s1", 60, Ok(Some(pr(123, PrChecks::Passing))));
    assert_eq!(d.poll("s1", 120, Err(ForgeError::Timeout)), vec![]);
    assert_eq!(
        d.host.host().sessions()[0].pr,
        Some(pr(123, PrChecks::Passing))
    );
}

#[test]
fn failures_back_off_doubling_up_to_a_cap_and_an_answer_resets_it() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    let mut at = 60;
    // 2, 4, 8, 16 minutes, then 16 again.
    for wait in [120, 240, 480, 960, 960] {
        d.poll("s1", at, Err(ForgeError::Other("HTTP 502".to_owned())));
        assert_eq!(
            d.watch.deadline(),
            Some(instant(at + wait)),
            "after the call at {at}s"
        );
        assert_eq!(d.tick(at + wait - 1), vec![]);
        at += wait;
    }
    d.poll("s1", at, Ok(None));
    assert_eq!(d.watch.deadline(), Some(instant(at + 60)));
}

#[test]
fn an_unavailable_forge_backs_off_too() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.poll("s1", 60, Err(ForgeError::Unavailable("no gh".to_owned())));
    assert_eq!(d.watch.deadline(), Some(instant(180)));
}

#[test]
fn each_session_is_checked_when_its_own_time_comes() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.session_with_worktree("s2", 30);
    assert_eq!(d.tick(60), vec![check("s1")]);
    assert_eq!(d.watch.deadline(), Some(instant(90)));
    assert_eq!(d.tick(90), vec![check("s2")]);
}

#[test]
fn a_session_without_a_branch_is_never_checked() {
    let mut d = Driver::new();
    let mut spec = shell_session_spec();
    spec.explicit_cwd = true;
    d.host.new_session_with("s1", spec);
    d.sync();
    assert_eq!(d.watch.deadline(), None);
    assert_eq!(d.tick(3600), vec![]);
}

#[test]
fn a_session_still_making_its_worktree_is_not_checked() {
    let mut d = Driver::new();
    d.host.new_session("s1");
    d.sync();
    assert_eq!(d.watch.deadline(), None);
    assert_eq!(d.tick(3600), vec![]);
}

#[test]
fn a_closed_session_is_no_longer_checked() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.host.close_with(
        "s1",
        shepherd_core::wire::CloseWorktree::Keep,
        Default::default(),
    );
    d.sync();
    assert_eq!(d.watch.deadline(), None);
    assert_eq!(d.tick(3600), vec![]);
}

#[test]
fn a_session_closing_its_worktree_is_no_longer_checked() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.host.close("s1");
    d.sync();
    assert_eq!(d.watch.deadline(), None);
    assert_eq!(d.tick(3600), vec![]);
}

#[test]
fn a_worktree_recorded_after_its_session_closed_is_not_checked() {
    let mut d = Driver::new();
    d.host.new_session("s1");
    d.host.close("s1");
    d.host.worktree_ready("s1");
    d.sync();
    assert_eq!(d.watch.deadline(), None);
}

#[test]
fn an_answer_for_a_session_closed_meanwhile_is_dropped() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    assert_eq!(d.tick(60), vec![check("s1")]);
    d.host.close("s1");
    d.sync();
    let commands = d.answer("s1", Ok(Some(pr(1, PrChecks::None))));
    assert_eq!(commands, vec![]);
}

#[test]
fn a_restarted_host_checks_at_once_and_knows_the_pr_it_recorded() {
    let mut d = Driver::new();
    d.session_with_worktree("s1", 0);
    d.poll("s1", 60, Ok(Some(pr(5, PrChecks::Passing))));
    // A new host run folds the same log into a fresh watch.
    let mut watch = PrWatch::new();
    for env in d.host.log() {
        watch.apply(env);
    }
    assert_eq!(watch.deadline(), Some(instant(60)));
    assert_eq!(
        watch.handle(PrEvent::Tick { at: t(3600) }),
        vec![check("s1")]
    );
    let same = watch.handle(PrEvent::Checked {
        session: sid("s1"),
        result: Ok(Some(pr(5, PrChecks::Passing))),
    });
    assert_eq!(same, vec![]);
}

// ---------- the session list ----------

#[test]
fn the_session_list_shows_the_latest_session_pr() {
    let mut host = HostDriver::new();
    host.new_session("s1");
    assert_eq!(host.host().sessions()[0].pr, None);
    let frame = |pr: serde_json::Value| json!({ "jsonrpc": "2.0", "method": "_shepherd/session/pr", "params": pr });
    let shepherd = || Source::new(SourceKind::Shepherd, None);
    host.append(
        Some("s1"),
        shepherd(),
        frame(json!({ "pr": { "number": 12, "state": "open", "checks": "failing" } })),
    );
    assert_eq!(
        host.host().sessions()[0].pr,
        Some(pr(12, PrChecks::Failing))
    );
    host.append(Some("s1"), shepherd(), frame(json!({})));
    assert_eq!(host.host().sessions()[0].pr, None);
}

#[test]
fn a_client_lists_each_session_with_its_pr() {
    let mut c = ClientDriver::with_sessions(&["s1"]);
    let frame = json!({ "jsonrpc": "2.0", "method": "_shepherd/session/pr",
                        "params": { "pr": { "number": 77, "state": "open", "checks": "pending" } } });
    c.host()
        .append(Some("s1"), Source::new(SourceKind::Shepherd, None), frame);
    c.sync();
    assert_eq!(c.client().sessions()[0].pr, Some(pr(77, PrChecks::Pending)));
}
