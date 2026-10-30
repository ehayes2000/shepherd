//! The `Forge` contract. A [`ForgeFixture`] arranges the forge's world (pull requests, a missing or signed-out tool,
//! a forge that fails or never answers): the fake keeps it in memory, the `gh` adapter's fixture puts a stand-in `gh`
//! on its search path that answers as `gh` does. Never the real `gh`.
//!
//! Run it with [`forge_contract!`](crate::forge_contract), which expands to one `#[test]` per case. Add every new case
//! to the macro's list.

/// Expands to one `#[test]` per `Forge` contract case. `$fixture` is an expression building a fresh
/// [`ForgeFixture`] for each test, e.g. `shepherd_testkit::forge_contract!(GhFixture::new());`.
#[macro_export]
macro_rules! forge_contract {
    ($fixture:expr) => {
        $crate::forge_contract!(@cases $fixture;
            a_branch_without_a_pr_has_none,
            an_open_pr_is_found_with_its_number,
            a_pr_without_checks_has_none,
            checks_that_passed_roll_up_to_passing,
            a_failed_check_makes_them_failing,
            a_running_check_makes_them_pending,
            a_merged_pr_is_found_as_merged,
            a_closed_pr_is_found_as_closed,
            the_open_pr_wins_over_a_newer_closed_one,
            without_an_open_pr_the_newest_wins,
            another_branchs_pr_is_not_reported,
            a_missing_tool_is_unavailable,
            a_signed_out_tool_is_unavailable,
            a_failing_forge_is_an_error,
            a_forge_that_never_answers_times_out,
        );
    };
    (@cases $fixture:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::contracts::forge::$case($fixture);
            }
        )*
    };
}

use shepherd_core::ports::forge::{Check, Forge, ForgeError, Pr, PrChecks, PrState};
use shepherd_core::ports::{BranchName, RepoRef};

/// A port, and the world it asks.
pub trait ForgeFixture {
    type Port: Forge;
    fn port(&self) -> &Self::Port;
    /// A checkout of the repo the pull requests are in.
    fn repo(&self) -> RepoRef;
    /// Opens pull request `number` from `branch`, newer than every one opened before, now in `state`, its head commit
    /// having `checks`.
    fn add_pr(&mut self, branch: &str, number: u64, state: PrState, checks: &[Check]);
    /// The forge's tool isn't installed.
    fn uninstall(&mut self);
    /// The forge's tool isn't signed in.
    fn sign_out(&mut self);
    /// The forge answers every call with an error, e.g. a server error.
    fn break_forge(&mut self);
    /// The forge never answers. The port is made to give up soon, so the case doesn't take long.
    fn hang(&mut self);
}

const BRANCH: &str = "fix-login";

fn ask<F: ForgeFixture>(f: &F, branch: &str) -> Result<Option<Pr>, ForgeError> {
    f.port().pr_for_branch(&f.repo(), &BranchName::new(branch))
}

fn found<F: ForgeFixture>(f: &F, branch: &str) -> Option<Pr> {
    ask(f, branch).unwrap_or_else(|e| panic!("asking about {branch}: {e}"))
}

fn open(number: u64, checks: PrChecks) -> Option<Pr> {
    Some(Pr {
        number,
        state: PrState::Open,
        checks,
    })
}

pub fn a_branch_without_a_pr_has_none<F: ForgeFixture>(f: F) {
    assert_eq!(found(&f, BRANCH), None);
}

pub fn an_open_pr_is_found_with_its_number<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 123, PrState::Open, &[Check::Passed]);
    assert_eq!(found(&f, BRANCH), open(123, PrChecks::Passing));
}

pub fn a_pr_without_checks_has_none<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 4, PrState::Open, &[]);
    assert_eq!(found(&f, BRANCH), open(4, PrChecks::None));
}

pub fn checks_that_passed_roll_up_to_passing<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 5, PrState::Open, &[Check::Passed, Check::Passed]);
    assert_eq!(found(&f, BRANCH), open(5, PrChecks::Passing));
}

pub fn a_failed_check_makes_them_failing<F: ForgeFixture>(mut f: F) {
    let checks = [Check::Passed, Check::Pending, Check::Failed];
    f.add_pr(BRANCH, 6, PrState::Open, &checks);
    assert_eq!(found(&f, BRANCH), open(6, PrChecks::Failing));
}

pub fn a_running_check_makes_them_pending<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 7, PrState::Open, &[Check::Passed, Check::Pending]);
    assert_eq!(found(&f, BRANCH), open(7, PrChecks::Pending));
}

pub fn a_merged_pr_is_found_as_merged<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 8, PrState::Merged, &[Check::Passed]);
    let expected = Pr {
        number: 8,
        state: PrState::Merged,
        checks: PrChecks::Passing,
    };
    assert_eq!(found(&f, BRANCH), Some(expected));
}

pub fn a_closed_pr_is_found_as_closed<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 9, PrState::Closed, &[]);
    let expected = Pr {
        number: 9,
        state: PrState::Closed,
        checks: PrChecks::None,
    };
    assert_eq!(found(&f, BRANCH), Some(expected));
}

pub fn the_open_pr_wins_over_a_newer_closed_one<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 10, PrState::Open, &[]);
    f.add_pr(BRANCH, 11, PrState::Closed, &[]);
    assert_eq!(found(&f, BRANCH), open(10, PrChecks::None));
}

pub fn without_an_open_pr_the_newest_wins<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 12, PrState::Closed, &[]);
    f.add_pr(BRANCH, 13, PrState::Merged, &[]);
    assert_eq!(found(&f, BRANCH).map(|pr| pr.number), Some(13));
}

pub fn another_branchs_pr_is_not_reported<F: ForgeFixture>(mut f: F) {
    f.add_pr("other-branch", 14, PrState::Open, &[]);
    assert_eq!(found(&f, BRANCH), None);
    assert_eq!(found(&f, "other-branch"), open(14, PrChecks::None));
}

pub fn a_missing_tool_is_unavailable<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 15, PrState::Open, &[]);
    f.uninstall();
    let got = ask(&f, BRANCH);
    assert!(
        matches!(got, Err(ForgeError::Unavailable(_))),
        "expected Unavailable, got {got:?}"
    );
}

pub fn a_signed_out_tool_is_unavailable<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 16, PrState::Open, &[]);
    f.sign_out();
    let got = ask(&f, BRANCH);
    assert!(
        matches!(got, Err(ForgeError::Unavailable(_))),
        "expected Unavailable, got {got:?}"
    );
}

pub fn a_failing_forge_is_an_error<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 17, PrState::Open, &[]);
    f.break_forge();
    let got = ask(&f, BRANCH);
    assert!(
        matches!(got, Err(ForgeError::Other(_))),
        "expected Other, got {got:?}"
    );
}

pub fn a_forge_that_never_answers_times_out<F: ForgeFixture>(mut f: F) {
    f.add_pr(BRANCH, 18, PrState::Open, &[]);
    f.hang();
    assert_eq!(ask(&f, BRANCH), Err(ForgeError::Timeout));
}
