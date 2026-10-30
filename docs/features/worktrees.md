# Workspaces and worktrees
milestone: M3
goal: Every new session gets a managed worktree from the latest origin default branch, with hooks, naming, close, cleanup and search

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T20 | `Worktrees` port | done | 9a7d007 | 3 | data loss under user git config, ssh prompts |
| T21 | Workspaces | done | a05e6ae | 2 | relative-path refusal passed for the wrong reason; refused-hello cwd and is_dir untested |
| T22 | New session creates a worktree | done | 10681b4 | 2 | worktree spawn cwd unchecked → cwd check made structural in SpawnPty; run-file cleanup untested |
| T23 | Worktree hooks | done | 1c18b85 | 2 | timeout contract flaky (?E stat), scenario hold race; setsid/stdin cases weak |
| T24 | Auto-naming | done | 6bfeb19 | 4 | control chars, boundaries untested; NFD split; bidi/format chars and unbounded graphemes in names (decision 0017) |
| T25 | Close removes the worktree when it's safe | done | a812e2a | 2 | Remove-anyway could force-delete unconfirmed new changes; escape test passed via an error |
| T25b | Cleanup view | done | 877d23b | 2 | orphan hook branch taken from the client; disconnect shown as kept |
| T26 | Search and open | done | 103ca34 | 5 | shared worktree ownership; hook-gap and path-alias bypasses; unresolved root spelling; untested host wiring; fixture briefly set core.bare on the real repo |
| T37 | Worktree creation has no time cap, runs concurrently, and cancels on close | done | 850906b | 2 | inherited GIT_ASKPASS could hang uncapped creates; 2-reader FIFO gate hang; restack dropped edits |
| F08 | Clean up after a timed-out worktree create | done | da443a6 | 3 | load flakes (shared short timeout), stale ref lock, untested fallback, TOCTOU dir delete, T20 test flake |
| F17 | Worktrees contract flake under heavy load | todo |  |  |  |
| F25 | Clean up after a failed worktree create | done | 8861b54 | 2 (+1 stopped) | cleanup could delete a branch another process made during the fetch |
| F28 | Workspace from a subdirectory, and `~` in typed paths | done | 6f1c62b | 2 | .git-named bare repo gave a root the port rejects |
| F32 | Create the worktree branch atomically | done | d93bb15 | 2 | user core.filesRefLockTimeout let cleanup delete another process's lock (pinned =0) |
| F33 | Hooks don't queue behind each other or outlive the host | done | 5cf8a10 | 2 | hooks started during shutdown never cancelled; vacuous cancel test; untested tail cap; macOS EPERM group wait |
| F37 | A killed fetch can leave a stale remote-ref lock | merging | task/f37-worktree-leftovers @ e8ae243 | 3 | approved r3; rebased over F42; --no-deref + absent-ref test (PM spot check) |
| F39 | T26 leftovers | todo |  |  |  |

## Specs

### T20: `Worktrees` port
depends: T00
touches: ports::worktrees, testkit, adapters::worktrees
goal: Managed git worktrees, expressed in domain terms.
acceptance:
- Contract: `default_branch(repo)`. `create(repo, name, base: OriginDefault) -> Worktree { path, branch }` fetches
  origin first, then branches from `origin/<default>`. `list_managed(repo)`. `status(path) -> { dirty, ahead,
  pushed, merged_into_default }`. `remove(path, force)`. Errors: `NotARepo`, `BranchExists`, `Dirty`, `FetchFailed`.
- Worktrees live under `~/.shepherd/worktrees/<repo-name>/<session-slug>`. The repo name comes from
  `git rev-parse --git-common-dir` (v1 lesson: this is stable across worktrees).
- If fetch fails while offline, it falls back to the local `<default>` and the result reports that.
- Fake plus git CLI adapter. Adapter contract tests use a tempdir bare "origin" and a clone.
scope: core::ports, testkit, adapters
notes: Use the `git` CLI, not a git library. Worktree support in gix and git2 is incomplete, and `git` is the reference
implementation the user's hooks expect. Run it with an explicit `-C <dir>` and parse only `--porcelain` or plumbing
output (`worktree list --porcelain`, `status --porcelain=v2 --branch`, `rev-parse`, `for-each-ref`). Never parse
human-readable output. Tests must never read or write the user's git config: set `GIT_CONFIG_GLOBAL=/dev/null`,
`GIT_CONFIG_NOSYSTEM=1` and a temp `HOME`, plus a fixed author/committer identity. The managed root (`~/.shepherd/worktrees`)
is a constructor parameter, and tests use a tempdir. The trait is sync, and git calls are bounded by a timeout (fetch
especially), returning a typed error. Path safety: `remove` refuses any path outside the managed root, tested with
`..` escapes and symlinks.

### T21: Workspaces
depends: T06, T13
touches: core::host (workspace), core::client, tui::header
goal: Choose a workspace (repo) once and have it remembered, visible, and easy to swap.
acceptance:
- `_shepherd/workspace/selected` is in the host log. The last selection is the default on the next start.
  `shepherd` run inside a repo with no selection picks that repo.
- A header or sidebar title shows the current workspace. Clicking it (decision 0014) opens a switcher listing the workspaces
  used before, with an input for a new path (validated through `Worktrees::default_branch`).
- Sessions record their workspace. The sidebar shows all sessions and marks those from other workspaces subtly.
  The decision on whether to filter instead is recorded in the task report.
- New sessions record the current workspace in `session/created` (additive `workspace` field) and, until T22 adds worktrees,
  start in the workspace root when the request has no `cwd`.
scope: crates/core, crates/tui, crates/shepherd (host runtime wiring), crates/testkit
notes: The `Worktrees` port (T20) isn't wired into the host runtime yet: add it as a runtime generic like `Pty`, with the
real git adapter in `host::open` and the fake in the harness. Workspace selection is host state (one per host, folded from the
log). The client sends its own cwd in hello (additive `cwd` field); if the host has no selection yet and that cwd is inside a
git repo (`Worktrees::default_branch` / repo root succeeds), the host selects that repo. The workspace is shown on a sidebar
header line above `+ New session`; clicking it opens a popup: clickable rows for previously used workspaces, a path input with
an `[Open]` button, and `[Cancel]` (T33/T34 patterns, hit tests click rendered cells). An invalid path shows the error in the
popup and selects nothing. Paths are stored canonicalized (no `..`, symlinks resolved) and compared as paths, not strings.
New wire request `_shepherd/workspace/select { path }`.

### T22: New session creates a worktree
depends: T17, T20, T21
touches: core::host (new session flow)
goal: You never think about worktrees, because a new session just has one.
acceptance:
- NewSession in a workspace runs CreateWorktree, then `_shepherd/worktree/created`, then post-create hooks (T23),
  then SpawnPty with cwd set to the worktree.
- The session is visible immediately as `Starting` while the worktree is being created.
- Failure (fetch, branch exists, not a repo) → `_shepherd/session/failed { reason }`. The sidebar shows the error,
  and no PTY is spawned.
- The branch name is derived from the session slug, with uniqueness suffixes handled in core.
- Scenario with fake Worktrees and fake Pty, covering the happy path and the fetch-failed path.
scope: crates/core, crates/shepherd, crates/testkit
notes: (binding) Every new session gets a worktree when a workspace is selected and the request has no explicit `cwd`; with
no workspace, or with an explicit `cwd`, it behaves as today (no worktree). All profiles, shell included. Base is the latest
origin default branch (`BaseSpec` per T20). `create` runs through F27's `Worker` (never on the runtime thread); the session
is appended as created (Starting) before the worktree exists, and its PTY spawns only when the result event arrives.
Results that no longer matter (session closed while git was working) leave no PTY and record what happened; the created
worktree is left in place for T25's cleanup, and that's logged. Core table rows for this. Skip the post-create hooks step
(T23 adds it). The worktree path becomes the session's cwd for everything after: record it in `_shepherd/worktree/created`,
and T19b resume must use it (resume of a worktree session whose worktree is gone is refused, per the existing cwd check).
The branch name is a slug of the session's placeholder name (`claude-3` etc.) made unique in core against branches the host
knows about; `create` refusing an existing branch is a `session/failed`, not a retry loop. All slugging/uniqueness logic is
pure core with table tests (unicode, punctuation, collisions).

### T23: Worktree hooks
depends: T22
touches: ports::commands, core::host, adapters::commands
goal: Run user-defined setup in new worktrees (copy `.envrc`, install deps, ...).
acceptance:
- Config: `[workspaces."<repo path or name>".hooks] post_create = ["cp ../.envrc .", ...]`, plus a `pre_remove` list.
- Each hook runs in the worktree with `SHEPHERD_*` env set. Its output tail and exit code go to `_shepherd/worktree/hook`.
  A failing post_create hook marks the session with a warning but still spawns the agent.
- `Commands` port with contract, fake, and a `std::process` adapter (decision 0011: no tokio), run through F27's `Worker` pattern so hooks never block the runtime thread.
scope: crates/core, crates/testkit, crates/adapters, crates/shepherd
notes: (binding) Hooks run with `/bin/sh -c <hook>` (portable; not the user's login shell), cwd = the worktree (validated to
exist), stdin closed, in their own process group, env = the host env plus `SHEPHERD_SESSION`, `SHEPHERD_WORKTREE`,
`SHEPHERD_BRANCH`, `SHEPHERD_REPO` (the main repo root, so `cp "$SHEPHERD_REPO/.envrc" .` works). Each hook has a generous
timeout (default 10 min, config `hook_timeout_s`) that kills the whole group (F08's wait-for-group pattern). Output tail is
capped (last 4 KiB, decoded lossily, control characters stripped before display). post_create hooks run in order; the first
failure (non-zero, timeout, spawn error) stops the rest, records `_shepherd/worktree/hook { hook, exit_code, output_tail }`,
marks the session with a warning (visible on sidebar line 2 like T22's text) and the agent still spawns. Workspace keys match
the canonical repo path or the repo directory's name; path match wins. `pre_remove` is parsed and validated now but run by
T25. The PTY spawns only after all post_create hooks finished (or the first failed); a session closed meanwhile spawns nothing.
Contract cases: exit code, output tail cap, timeout kills the group (no leaked children — check with a hook that forks
`sleep`), env and cwd, a missing cwd is an error.

### T24: Auto-naming
depends: T05, T22
touches: core::host (naming)
goal: Sessions get useful names without effort. Manual rename always wins.
acceptance:
- New sessions keep F22's placeholder name (`<profile>-<n>`).
- After the first user prompt in the fold, if the session has never been manually renamed, append
  `_shepherd/session/renamed { auto: true }` with a deterministic slug built from the prompt's first meaningful words
  (stopwords dropped, max 4 words or 30 chars).
- A manual rename sets `auto: false`, and auto-naming never overrides it afterwards.
- Only the display name changes. The branch name is not renamed.
scope: crates/core, crates/shepherd (only if the runtime must feed anything)
notes: LLM-generated names are a possible follow-up, behind a port, not in this task. (binding) Pure core: the slug function
is table-tested (unicode, punctuation, code blocks/paths in prompts, empty or all-stopword prompts → no rename). The rename is
appended by the host when the fold sees the first user prompt (Claude `UserPromptSubmit` / Codex equivalent via the
translators); exactly once per session; a host restart must not re-name (decide from the log). Display-name uniqueness is not
required. Resume and replay keep the name.

### T25: Close removes the worktree when it's safe
depends: T22, T23
touches: core::host (close), core::client, tui::popup
goal: Worktrees go away when they should. No more pile of abandoned worktrees.
acceptance:
- Closing a session with a worktree checks `Worktrees::status` (through F27's Worker). If the worktree is clean and merged or
  pushed, it is removed automatically after its `pre_remove` hooks (T23's config, run through the Commands port/worker like
  post_create). If it is dirty or unpushed, the close confirmation lists what would be lost and offers `[Remove anyway]` /
  `[Keep worktree]` / `[Cancel]` (clickable, decision 0014). The outcome is recorded in `session/closed.worktree_removed`.
- A failing pre_remove hook keeps the worktree and says why (notice). A worktree status/remove error keeps it and says why.
- Removal never touches anything outside `<root>/worktrees`; tested with a recorded path that tries to escape (`..`, symlink).
scope: crates/core, crates/tui, crates/shepherd, crates/testkit
notes: The session closes immediately (PTY killed) and shows "removing worktree…" on line 2 until the removal finishes;
the close is recorded once the outcome is known. A session whose worktree is already gone closes normally.

### T25b: Cleanup view
depends: T25
touches: core::client, tui::popup, core::host
goal: Find and remove abandoned worktrees in bulk.
acceptance:
- A cleanup view (opened from a `Cleanup` button in the sidebar footer, decision 0014) lists managed worktrees with no open
  session and stale sessions (exited, and merged for more than N days) with status columns, and supports bulk removal of the
  safe ones (same safety rules and pre_remove hooks as T25). Hit tests click rendered cells; snapshot.
- From the T25 final review: a runtime test that a listed worktree with status NotARepo whose directory still exists is kept
  (the `exists` computation in runtime.rs is untested); force only when `confirmed.dirty && status.dirty`; the safe and
  unpushed-only dialogs mention that ignored files are removed.
scope: crates/core, crates/tui, crates/shepherd, crates/testkit
notes: (binding) Reuse T26's `_shepherd/worktree/list` (orphans) and T25's per-worktree inspect/remove path for every
removal, so every guarantee holds per item (list_managed membership under the resolved root, still-used re-check, pre_remove
hooks, force only with confirmed dirty). "Safe" = clean and merged/pushed (T25's rule); unsafe rows are shown with what would
be lost and are never in a bulk removal. Stale sessions: exited and their branch merged; N days defaults to 7 (config
`cleanup_after_days`), time from Tick, never a clock in core. Bulk removal runs items one at a time through the worker and
reports per-row outcomes; closing the view doesn't cancel removals already started. Popup patterns as T26/F29 (core-owned
scroll, hit tests on rendered cells, tiny-window test, draw.rs).

### T26: Search and open
depends: T13, T21
touches: core::client (finder), tui::finder
goal: Find and jump to any session or worktree quickly.
acceptance:
- A clickable search field at the top of the sidebar (decision 0014) opens a fuzzy finder over sessions (name, branch, workspace, machine) and managed worktrees with no
  session. Clicking or Enter selects a session. On an orphan worktree, it offers to start a new session in that worktree.
- Matching and ranking are pure core functions with unit tests (prefix > word-boundary > subsequence).
- Snapshots: empty query, a filtered list, and no results.
scope: crates/core, crates/tui, crates/shepherd (host: list worktrees), crates/testkit
notes: (binding) Orphan worktrees come from the host: a new request `_shepherd/worktree/list` answered from
`Worktrees::list_managed` for the current workspace's repo (through F27's worker), minus worktrees of sessions the host
knows. The client asks when the finder opens and shows "loading…" until the answer arrives. "Start a session here" sends
`session/new` with an explicit `cwd` of that worktree (T22: an explicit cwd gets no new worktree) and records its branch.
All displayed text goes through `draw.rs` (F35). The finder is a popup (T33/T34/F24 patterns: core-owned scroll, saturating
height, hit tests on rendered cells, every-popup tiny-window test). Typing filters; Esc/click outside closes. Matching is
pure, Unicode-aware (graphemes, case-insensitive via `to_lowercase`), never hand-rolled tables.

### T37: Worktree creation has no time cap, runs concurrently, and cancels on close
depends: T22, F27, F32
touches: adapters::worktrees, shepherd::host (worker), core::host (close while creating)
goal: On the user's real repo (macro: large, git-lfs) a new session stuck on "creating worktree…": the 120 s GIT_TIMEOUT
covers `worktree add`, so a long checkout (LFS smudge) is killed and cleaned up; and the single Worktrees worker made a second
new session queue behind it. herdr (github.com/herdrdev/herdr, src/worktree.rs + src/app/api/worktrees/deferred.rs) runs
`git worktree add -b <b> <path> HEAD` with no timeout on its own thread per operation, and it works well.
acceptance:
- `worktree add` (and the checkout/LFS work it does) has no time cap. Git still runs non-interactively (no tty, setsid,
  `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never` — already set), so a credential prompt fails instead of hanging.
  Short metadata calls (rev-parse, show-ref, update-ref, worktree list) keep a generous bound.
- `fetch` keeps a bound; on timeout or failure the worktree starts from the last fetched `origin/<default>` and the session
  says so (`fetch_error`, existing Stale/Local base path).
- Each `create` runs on its own thread (herdr's model), so creations for different sessions run concurrently; the worker
  for other Worktrees calls stays. A scenario with two held creates proves both start before either finishes (counted).
- Closing a session whose worktree is being created kills that git process group (wait for it to be gone), runs the
  existing F08/F25/F32 cleanup (never deleting what isn't ours), and records the close. Test with a create held on a FIFO.
- No big-repo tests (user decision): prove "no cap" with a create held well past the old bound via an injected fake clock
  or a held hook, never by size or wall-clock.
- From the F32 review: give `an_add_blocked_by_a_foreign_branch_lock_…` a short cleanup timeout and assert no "timed out",
  so removing the cleanup-delete lock pin fails fast instead of hanging.
scope: crates/adapters, crates/core, crates/shepherd, crates/testkit

### F08: Clean up after a timed-out worktree create
depends: T20
touches: adapters::worktrees, ports::worktrees (docs)
goal: A `create` that times out (e.g. a slow post-checkout hook) leaves no half-made branch or worktree behind.
acceptance:
- On `Timeout` during `worktree add`, the adapter removes any registered worktree and the new branch, and a retry with
  the same name succeeds. There's an adapter test with a slow `post-checkout` hook, asserting nothing is left and the retry works.
- If cleanup itself fails, the error says so, and `Worktrees::create` docs describe it.
scope: crates/adapters, crates/core (docs only)

### F17: Worktrees contract flake under heavy load
depends: T20
touches: adapters::worktrees (tests)
goal: `worktrees::tests::remove_deletes_the_directory` failed once in 24 parallel runs (the `create` step at
contracts/worktrees.rs:162 errored during a 215 s run at load average ~70).
acceptance:
- Also seen: one unidentified `shepherd-adapters --lib` failure in the PM's merge gate for T22 (2026-10-30; passed on rerun;
  log not kept). Likely the same class.
- Reproduce and find the cause: a git timeout, or a real race. If it's a timeout under extreme load, make the fixture's bounds and
  error reporting robust (print the actual error). If it's a race, fix it. Then 0 failures in 5 rounds of 24 parallel copies.
scope: crates/adapters, crates/testkit

### F25: Clean up after a failed worktree create
depends: F08
touches: adapters::worktrees
goal: A `post-checkout` hook that exits non-zero (or any non-timeout failure of `worktree add`) can leave the worktree and
branch behind, like the timeout case F08 fixed.
acceptance:
- Any `worktree add` failure after git registered the worktree runs the same cleanup as F08; a retry with the same name
  succeeds. Adapter test with a failing hook. Errors that happen before anything is created clean up nothing.
scope: crates/adapters

### F28: Workspace from a subdirectory, and `~` in typed paths
depends: T21
touches: ports::worktrees, adapters::worktrees, core::host (workspace)
goal: Running `shepherd` in `<repo>/crates` should pick `<repo>`, and typing `~/Code/x` in the switcher should work.
acceptance:
- `Worktrees` gains `repo_root(&Path) -> Result<PathBuf, WorktreeError>` (git `rev-parse --show-toplevel` with T20's pinned
  options and env stripping; for a linked worktree, the main repo root via `--git-common-dir`). Contract cases for: repo root,
  subdirectory, linked worktree, not a repo. Fake supports it.
- Workspace selection (hello cwd and typed paths) resolves to the repo root. Selecting a subdirectory of the current workspace
  is the same workspace (no append).
- A typed path starting with `~/` (or exactly `~`) expands to the host's home; other relative paths are still refused.
- From the F27 review: a scenario where a hello-cwd selection arrives live (probe held, hello with cwd, subscribe, release →
  the client gets `workspace/selected` live, not in catch-up); and either a worker unit test with a dead thread/dropped receiver
  or soften `Worker::submit`'s "every call is answered" doc (queued jobs are dropped if the thread dies).
scope: crates/core, crates/adapters, crates/testkit, crates/shepherd

### F32: Create the worktree branch atomically
depends: F25
touches: adapters::worktrees
goal: F25 re-checks that the branch is absent right before `worktree add -b`, but a branch created in the instant between that
check and git's own branch step would still be deleted by cleanup.
acceptance:
- Create the branch with `git update-ref refs/heads/<b> <base-oid> ""` (fails atomically if it exists → BranchExists, nothing
  to clean), then `git worktree add <path> <b>` without `-b`. Cleanup deletes the branch only if this create made it (and
  only with `update-ref -d refs/heads/<b> <base-oid>`, i.e. only if it still points where we put it). Regression tests: a branch
  appearing just before update-ref → BranchExists, untouched; a branch moved by someone after we created it → not deleted.
- F28 review: pin `main_checkout`'s checks with an adapter test for a main checkout with `core.worktree` set (and a comment
  that the same-repo check is defensive); fake `add_dir` parent semantics needs a contract case.
- F25 review: the signal-kill path sends SIGKILL to the group after git was reaped (theoretical pid reuse). Learn how git
  died with `waitid(WNOWAIT)` (as `wait_unreaped` does), kill the group, then reap.
scope: crates/adapters

### F33: Hooks don't queue behind each other or outlive the host
depends: T23
touches: adapters::commands, shepherd::host
goal: From the T23 review: one shared `hooks` worker means session B's post-create hooks wait behind session A's (up to the
10 min timeout), closing A doesn't cancel its hook, a hook still running at host shutdown keeps running (it's `setsid`), and
hook output goes to an unlinked temp file with no size cap.
acceptance:
- Hooks for different sessions run concurrently (a worker per running hook, or a small pool); a held hook for one session
  doesn't delay another's (scenario with two held hooks, counted).
- Closing a session kills its running hook's process group (and waits for it); host shutdown kills every running hook group.
  No leaked processes (check pids you recorded).
- Hook output is bounded on disk (cap the file, e.g. 1 MiB, then keep only the tail) — contract case with a chatty hook.
scope: crates/adapters, crates/core, crates/testkit, crates/shepherd

### F37: A killed fetch can leave a stale remote-ref lock
depends: T37
touches: adapters::worktrees
goal: From the T37 review: a fetch killed by a cancel or its timeout can leave `refs/remotes/origin/<default>.lock`; every
later fetch then fails and falls back to the stale base.
acceptance:
- After a fetch that was killed (timeout/cancel) with `core.filesRefLockTimeout=0` pinned, remove only the remote-tracking ref
  locks that fetch could have held, and only if git died holding them (same rules as F32's branch lock). Test with a hook or
  a fake remote that hangs mid-fetch; a foreign lock is never removed.
- From F33: `worktrees.rs`'s own `wait_group_gone` returns early on macOS EPERM (group members dying/zombie); wait through EPERM
  until ESRCH as `process_group.rs` now does (or share that helper and drop the duplicate `kill_group`/`wait_unreaped`).
- From F33: a create that finishes as shutdown cancels it, with no hooks, can still spawn a PTY during shutdown; don't spawn
  once shutdown began.
- From the T37 review: host shutdown cancels creates but doesn't wait for their cleanup; delay `finish` until no create is
  running (bounded) so a cancelled-at-shutdown create never leaves an unrecorded worktree/branch.
scope: crates/adapters, crates/shepherd (host shutdown)

### F39: T26 leftovers
depends: T26
touches: shepherd::host (replay), shepherd tests
goal: Small gaps from the T26 final review.
acceptance:
- The replay test also covers `session/created`'s `worktree` and `cwd` (adopted / explicit-cwd sessions recorded with a
  symlinked spelling).
- A test-only way to give the real host an isolated git environment (HOME, GIT_CONFIG_GLOBAL/NOSYSTEM) so real-host tests
  never read the user's global git config or hooks.
- A detached orphan worktree (no branch) can be adopted and owned.
- From F33: `next_hook` calls `Commands::new_run()` before `run_alone`; a failed thread spawn leaks that id forever — call
  `new_run` after a successful spawn or forget the id on the error path.
scope: crates/shepherd, crates/core
