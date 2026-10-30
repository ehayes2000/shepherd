# PR status
milestone: M3
goal: Each session's PR and its checks show in the sidebar

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T27 | PR status in the sidebar | done | 6936444 | 2 | conflicted with F33; fixture leaked env to disk; CLICOLOR_FORCE broke JSON; real_worktrees could reach real gh |
| F41 | T27 and T25b leftovers | done | a971711 |  |  |

## Specs

### T27: PR status in the sidebar
depends: T13, T22
touches: ports::forge, core::host, adapters::forge, tui::sidebar
goal: Sidebar line 2 shows the session's PR.
acceptance:
- `Forge::pr_for_branch(repo, branch) -> Option<Pr { number, state, checks }>`, with contract, fake, and `gh` adapter.
  A missing `gh` or no auth → `Unavailable`, shown as nothing (not an error).
- The host polls open sessions on a timer driven by `Tick` events, with backoff and at most one call per session
  every 60s. Changes are appended as `_shepherd/session/pr`.
- The sidebar shows `#123 ✓/✗/…`. Snapshot test.
scope: crates/core, crates/testkit, crates/adapters, crates/tui, crates/shepherd
notes: (binding) `Forge` calls run off the runtime thread (F27 Worker pattern), with a bound (network), non-interactive
(`gh` with `GH_PROMPT_DISABLED=1`, no tty, setsid, `GIT_ASKPASS=` like T37), stdout parsed as JSON with serde (`gh pr list
--head <branch> --json number,state,statusCheckRollup` or equivalent — verify the flags in gh's docs; never parse text).
Never touch the user's gh config/auth; adapter tests use a stand-in `gh` script on PATH, never the real `gh`. Poll only
sessions whose worktree has a branch and that are not closed; polling state (last call, backoff) is pure core driven by
`Tick { at }`. The PR number/state/checks shown go through draw.rs (F35). Sidebar line 2 shows `#123 ✓` etc. after the
machine name (`local · #123 ✓`); clicking it is out of scope.

---

### F41: T27 and T25b leftovers
depends: T27
touches: adapters::forge (tests), shepherd tests, core tests
goal: Test gaps from the T27 and T25b final reviews.
acceptance:
- `gh_reads_nothing_from_stdin` passes regardless of the test run's own stdin: run the port from a helper whose stdin is a
  pipe with data, so `.stdin(Stdio::inherit())` fails it under the gate's `/dev/null` stdin too.
- From T25b: `inspect_worktree`'s membership `find(|w| w.path == *path)` → `next()` survives; add a case with a real managed
  worktree beside the crafted outside path/symlink. Pin a detached orphan's hooks getting `SHEPHERD_BRANCH=""`.
- `variables_that_point_gh_or_git_elsewhere_are_not_passed_on` also asserts an always-present variable (e.g. HOME) doesn't
  reach the stand-in, so deleting `env_clear()` fails it.
scope: crates/adapters, crates/core (tests), crates/shepherd (tests)
