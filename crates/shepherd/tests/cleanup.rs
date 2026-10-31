//! The cleanup view's requests (T25b): orphan worktrees listed with their status, and one removed through the same
//! inspect, pre-remove hooks and remove path a closing session's worktree takes. A host runtime on fake ports, the fake
//! `Worktrees` and `Commands` ports on their worker threads.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use shepherd_core::acpp::{Frame, Seq, ShepherdFrame};
use shepherd_core::config::{RawConfig, RawHooks, RawWorkspace};
use shepherd_core::ids::MachineId;
use shepherd_core::ports::{BaseSpec, EventLog, LogId, RepoRef, WorktreeName, Worktrees};
use shepherd_core::wire::{
    ClientRequest, Hello, PROTOCOL_VERSION, WorktreeCleanup, WorktreeRemove,
};
use shepherd_testkit::fakes::commands::FakeCommands;
use shepherd_testkit::fakes::hold::Hold;
use shepherd_testkit::fakes::worktrees::FakeWorktrees;
use support::*;

/// A liveness bound for a held call, never a measurement.
const LIMIT: Duration = Duration::from_secs(10);
/// How long each wait in a loop polling for two things at once lasts: a liveness slice of [`LIMIT`], never a measurement.
const STEP: Duration = Duration::from_millis(20);

/// A host whose workspace is `<tempdir>/app` with `make clean` as its pre-remove hook, a subscribed client, and two
/// orphan worktrees: `old-idea` (clean, merged) and `wip` (dirty).
struct Setup {
    _tmp: tempfile::TempDir,
    worktrees: FakeWorktrees,
    commands: FakeCommands,
    h: HostHarness,
    client: FakeClient,
    repo: RepoRef,
    old_idea: PathBuf,
    wip: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let app = base.join("app");
        std::fs::create_dir(&app).unwrap();
        let worktrees = FakeWorktrees::new(base.join("worktrees"));
        let repo = worktrees.add_repo(&app, "main");
        let make = |name: &str| {
            let name = WorktreeName::new(name).unwrap();
            let made = worktrees.create(&repo, &name, BaseSpec::OriginDefault);
            let path = made.unwrap().path.as_path().to_path_buf();
            // Where git would have made it: its hooks run there.
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        let old_idea = make("old-idea");
        let wip = make("wip");
        worktrees.set_dirty(&wip, true);
        let commands = FakeCommands::new();
        commands.add_script("make clean", |c| c.print("cleaned\n"));
        let mut h = HostHarness::builder()
            .worktrees(worktrees.clone())
            .commands(commands.clone())
            .config(config_with(&app))
            .start();
        let mut client = h.connect();
        client.request(ClientRequest::Hello(Hello {
            version: PROTOCOL_VERSION,
            machine: MachineId::new("laptop"),
            cwd: Some(app.to_str().unwrap().to_owned()),
            build: None,
        }));
        h.step();
        client.subscribe_all();
        h.step();
        client.take();
        Self {
            _tmp: tmp,
            worktrees,
            commands,
            h,
            client,
            repo,
            old_idea,
            wip,
        }
    }

    fn remove(&mut self, path: &Path) -> shepherd_core::acpp::RequestId {
        self.client
            .request(ClientRequest::WorktreeRemove(WorktreeRemove {
                path: path.to_str().unwrap().to_owned(),
            }))
    }

    fn listed(&self, path: &Path) -> bool {
        let listed = self.worktrees.list_managed(&self.repo).unwrap();
        listed.iter().any(|w| w.path.as_path() == path)
    }

    /// The methods of the host log's shepherd frames.
    fn host_methods(&self) -> Vec<&'static str> {
        let entries = self.h.log().read(LogId::Host, Seq::ZERO).expect("host log");
        entries
            .iter()
            .filter_map(|e| match e.frame() {
                Frame::Shepherd(m) => Some(m.body().method()),
                _ => None,
            })
            .collect()
    }
}

fn config_with(app: &Path) -> RawConfig {
    let workspace = RawWorkspace {
        hooks: Some(RawHooks {
            post_create: None,
            pre_remove: Some(vec!["make clean".to_owned()]),
        }),
        hook_timeout_s: Some(90),
    };
    RawConfig {
        workspaces: Some(BTreeMap::from([(
            app.to_str().unwrap().to_owned(),
            workspace,
        )])),
        ..shell_default_config()
    }
}

#[test]
fn the_cleanup_list_has_each_orphan_with_its_status_and_the_configured_days() {
    let mut s = Setup::new();
    let req = s
        .client
        .request(ClientRequest::WorktreeCleanup(WorktreeCleanup::default()));
    s.h.step();
    let answer = response(&s.client.take(), &req).unwrap();
    assert_eq!(
        answer,
        json!({
            "worktrees": [
                { "path": s.old_idea, "branch": "old-idea",
                  "status": { "dirty": false, "ahead": 0, "pushed": false, "merged_into_default": true } },
                { "path": s.wip, "branch": "wip",
                  "status": { "dirty": true, "ahead": 0, "pushed": false, "merged_into_default": true } },
            ],
            "cleanup_after_days": 7,
        })
    );
}

#[test]
fn removing_an_orphan_that_loses_nothing_runs_its_pre_remove_hook_then_removes_it() {
    let mut s = Setup::new();
    let req = s.remove(&s.old_idea.clone());
    s.h.step();
    assert_eq!(
        response(&s.client.take(), &req),
        Ok(json!({ "removed": true }))
    );
    assert!(!s.listed(&s.old_idea), "removed");
    let runs = s.commands.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].script, "make clean");
    assert_eq!(runs[0].cwd, s.old_idea);
    assert!(
        runs[0]
            .env
            .contains(&("SHEPHERD_BRANCH".to_owned(), "old-idea".to_owned())),
        "{:?}",
        runs[0].env
    );
    assert_eq!(
        s.host_methods(),
        [
            "_shepherd/machine/renamed",
            "_shepherd/workspace/selected",
            "_shepherd/worktree/removing",
            "_shepherd/worktree/hook",
            "_shepherd/worktree/removed",
        ]
    );
    assert_eq!(s.h.replayed(), *s.h.host());
}

/// A stale or crafted request naming another branch (an older client sent one) doesn't reach the hooks: they get the
/// branch git lists for the worktree.
#[test]
fn an_orphans_hooks_get_the_listed_branch_never_one_the_client_named() {
    let mut s = Setup::new();
    let line = json!({
        "jsonrpc": "2.0", "id": "crafted", "method": "_shepherd/worktree/remove",
        "params": { "path": s.old_idea, "branch": "main" },
    });
    s.client.send_raw(line.to_string().as_bytes());
    s.h.step();
    let msgs = s.client.take();
    let id = shepherd_core::acpp::RequestId::String("crafted".into());
    assert_eq!(response(&msgs, &id), Ok(json!({ "removed": true })));
    let runs = s.commands.runs();
    assert_eq!(runs.len(), 1);
    let branch: Vec<&(String, String)> = runs[0]
        .env
        .iter()
        .filter(|(k, _)| k == "SHEPHERD_BRANCH")
        .collect();
    assert_eq!(
        branch,
        [&("SHEPHERD_BRANCH".to_owned(), "old-idea".to_owned())]
    );
}

#[test]
fn an_orphans_hook_record_says_which_worktree_it_ran_in() {
    let mut s = Setup::new();
    s.remove(&s.old_idea.clone());
    s.h.step();
    let entries = s.h.log().read(LogId::Host, Seq::ZERO).expect("host log");
    let hook = entries.iter().find_map(|e| match e.frame() {
        Frame::Shepherd(m) => match m.body() {
            ShepherdFrame::WorktreeHook(hook) => Some(hook.clone()),
            _ => None,
        },
        _ => None,
    });
    let hook = hook.expect("a worktree/hook in the host log");
    assert_eq!(hook.path.as_deref(), s.old_idea.to_str());
}

#[test]
fn removing_an_orphan_that_would_lose_work_keeps_it_and_runs_no_hook() {
    let mut s = Setup::new();
    let req = s.remove(&s.wip.clone());
    s.h.step();
    assert_eq!(
        response(&s.client.take(), &req),
        Ok(json!({ "removed": false, "kept": "it has uncommitted changes" }))
    );
    assert!(s.listed(&s.wip), "kept");
    assert!(s.commands.runs().is_empty());
}

/// Removes `old-idea` with its pre-remove hook held: returns once the hook is running and waiting.
fn remove_with_hook_held(s: &mut Setup) -> Hold {
    let hold = s.commands.hold_next();
    s.remove(&s.old_idea.clone());
    s.h.poll();
    let mut waited = Duration::ZERO;
    while !hold.wait_entered(STEP) {
        assert!(waited < LIMIT, "the hook never reached the port");
        s.h.runtime_mut().run_next(STEP);
        waited += STEP * 2;
    }
    hold
}

#[test]
fn shutting_the_host_down_cancels_an_orphans_running_pre_remove_hook() {
    let mut s = Setup::new();
    let hold = remove_with_hook_held(&mut s);
    s.h.runtime()
        .sender()
        .send(shepherd::host::RuntimeEvent::Shutdown);
    s.h.poll();
    assert!(hold.wait_cancelled(LIMIT), "the hook was left running");
    assert!(s.h.runtime_mut().run_until_settled(LIMIT));
    assert!(s.h.runtime().is_finished());
    assert_eq!(s.commands.running(), 0);
    assert!(s.listed(&s.old_idea), "the worktree was removed");
}

#[test]
fn a_host_restarting_while_an_orphan_is_removed_records_it_kept() {
    let mut s = Setup::new();
    let hold = remove_with_hook_held(&mut s);
    s.h.restart();
    hold.release();
    let entries = s.h.log().read(LogId::Host, Seq::ZERO).expect("host log");
    let last = entries.iter().rev().find_map(|e| match e.frame() {
        Frame::Shepherd(m) => match m.body() {
            ShepherdFrame::WorktreeRemoved(removed) => Some(removed.clone()),
            _ => None,
        },
        _ => None,
    });
    let removed = last.expect("a worktree/removed");
    assert!(!removed.removed);
    assert_eq!(
        removed.kept.as_deref(),
        Some("the host stopped before the worktree was removed")
    );
    // Nothing is being removed any more: it can be asked again.
    let mut client = s.h.connect();
    let req = client.request(ClientRequest::WorktreeRemove(WorktreeRemove {
        path: s.old_idea.to_str().unwrap().to_owned(),
    }));
    s.h.step();
    assert_eq!(
        response(&client.take(), &req),
        Ok(json!({ "removed": true }))
    );
}
