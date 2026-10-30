# PM handbook

How the project manager (the main Claude session) runs this project, written so a successor can take over in the middle
of work. Read with `CLAUDE.md`, `docs/architecture.md`, `docs/plan.md` and the `dispatch` / `write-task` skills.

## Taking over (do this first)

1. Read `CLAUDE.md`, this file, the **In flight** table at the top of `docs/plan.md`, and the decision log in
   `docs/architecture.md`.
2. **Check reality against the table, and trust git, not the table.** Run `git log --oneline --merges main | head -30`,
   `git branch | grep task/`, `git worktree list`, and `grep -n "status:" docs/plan.md`. A task whose merge commit is on main
   is done, whatever the table says. Fix the table first.
3. Subagents don't survive a session. To continue an in-flight task:
   - **Building/fixing:** dispatch a fresh `builder` (worktree isolation). Give it the spec block, "continue on existing branch
     `task/<id>` (do not reset it)", the base commit, and the open review findings and PM decisions from the table.
   - **In review:** dispatch a fresh `reviewer` with the spec block plus the PM-accepted decisions from the table.
   - Stale worktrees left by dead agents: `git worktree remove --force <path>` (twice `-f` if it's locked by a pid that no longer
     exists), then `git branch -D task/<id>` **only after** its merge commit is on main.
4. Tell the user what you found and what you're resuming.

## The loop

`write-task` spec → `dispatch` builder → builder report → `reviewer` (spec plus branch only, never the builder's report) →
CHANGES: PM decides open questions and sends the findings plus decisions to the **same** builder (SendMessage keeps its context),
then re-review by the **same** reviewer, which checks only what changed → APPROVE → merge queue → mark done → lessons.

**Update the In flight table on every transition** (dispatch, report, verdict, merge). It's the only state that survives a
context reset.

### Dispatching
- Up to about 5 builders at once (raised from 3 at the user's request), with no overlapping `touches` beyond separate files
  of one module. Reviewers' 24-copy load tests compete for the machine: run at most two load tests at once, and treat a
  failure that appears only while another load test runs as a lead to re-run alone, not a verdict. Ports should follow the existing template (`contracts/pty.rs`,
  fixture-trait variant `contracts/config.rs`).
- Every builder prompt includes the spec block verbatim, the main repo path, the **base commit hash** (worktrees can start from a
  stale `origin/main`), the branch name, and anything concurrent it must avoid (shared files: `crates/*/src/lib.rs` module
  lists, `crates/shepherd/src/main.rs`, `Cargo.toml`).
- Design calls (trait shapes, error rules, semantics) go **in the spec** before dispatch, as binding notes, never as hints.

### Reviewing
- A full independent review for anything with IO, concurrency, security, or a protocol/semantic decision.
- PM spot check (read the diff, run the gate) is acceptable for small pure refactors whose existing tests are the spec and
  pass unchanged (precedent: F04, F12). If in doubt, review.
- Accepting a builder's choice: write it into the reviewer prompt as "PM-accepted (flag only if wrong)".
- A spike (docs-only research) is reviewed by the PM: check the evidence, record a decision.

### Merge queue (one at a time)
```
git merge --no-ff task/<id> -m "feat: merge <ID> <title>"
scripts/check.sh > <scratchpad>/pm-gate.log 2>&1   # on the merged tree, every time; keep the log to name any flake
# conflict in Cargo.lock: git checkout --theirs Cargo.lock && cargo generate-lockfile, then gate
# conflict in module lists / main.rs: keep both sides
```
- `git merge` doesn't run the pre-commit hook. Always run the gate yourself. If you resolved a conflict and already ran the gate,
  `git -c core.hooksPath=/dev/null commit --no-edit` avoids a second identical run (only then).
- Then: set `status: done (<merge hash>)` in plan.md, remove the worktree and branch, push.
- macOS has no `timeout(1)`. Gate runs can exceed 10 minutes when run twice, so use background runs if needed.
- Never use `git add -A` while builder worktrees exist under the repo unless `.claude/worktrees/` is in `.gitignore` (it is now).
- Edit files with the Edit tool or a script that reads before it writes. Opening a file for writing truncates it (this
  once emptied plan.md; the gate now fails on empty core docs).

### Smoke run (after each milestone)
`cargo build --release -p shepherd && python3 scripts/smoke.py "$PWD/target/release/shepherd"` drives the real binary in a
pty with an isolated temporary HOME (never the user's `~/.shepherd`), and prints PASS/FAIL per step. Extend it as features land.

### Lessons
After each verdict, ask "what general cause produced this finding?" and make it a rule in the cheapest place that enforces it:
a gate check or lint (best), the reviewer checklist, a skill, `CLAUDE.md`, or a decision entry. Anything a reviewer flags twice
gets enforced mechanically. File non-blocking findings as follow-ups (`F<NN>`) in plan.md rather than holding up merges.

## User preferences (standing)
- Pushing `main` to origin after merges is authorized. Merges happen on local main, with no PRs per task.
- **Ask before** repository-wide or outward-facing actions not covered above (e.g. `git worktree prune`, deleting branches that
  aren't task branches, anything touching the user's other repos or machine config).
- Commits: conventional one-line messages (see CLAUDE.md).
- Status updates: concise. Report state changes, not "still waiting" heartbeats. Own mistakes plainly.
- The user wants something hands-on ASAP. M1 (runnable shell multiplexer) has priority, then M2 (Claude sessions with status).

## Things that have bitten us (and the rule that resulted)
| What happened | Rule now |
|---|---|
| Worktrees started from stale `origin/main` | Base commit in every dispatch; builder checks it |
| Builder upgraded the user's Codex, trusted a dir in Claude config | Builders never touch the user's machine; isolate HOME |
| Adapter passed 40/40 serially, lost output 4% under load | Load-test the whole adapter binary, 24 copies (macOS pty cap 511) |
| Lock bug only appeared next to PTY tests (fork inherits fds) | Load-test the **whole** binary, not one port's tests |
| PM's tight core allowlist caused hand-rolled base64 and timestamps; string-compared timestamps | Decision 0008: allow pure crates; reviewer flags reinvention |
| `remove` deleted untracked files under the user's git config | Adapters pin options and strip env injection; hostile-config tests |
| Wall-clock ratio test flaked under load | No wall-clock assertions in the gate; count work |
| Docs claimed "never follows symlinks", which was false | Every documented guarantee needs a test |
| Tests passed with the guarded line deleted | Reviewers run mutation checks on key lines |
| A dev-dependency feature changed prod serialization | Decision 0012; behavior-changing features in normal builds only |
| Builder `pkill -f` killed another agent's tests | Never kill by pattern; only pids you started |
| PM reported a merged task as still in review | The In flight table plus checking it against git on takeover |
| portable-pty silently starts in `$HOME` when the cwd doesn't exist (T19b, then again T22) | The runtime's single SpawnPty handler validates the cwd for every spawn (made structural in T22) |
| F14 reviewer hung 4 h inside one tool call; a queued SendMessage never arrived | Tell reviewers to run long load tests in the background with a liveness cap (e.g. 20 min) and check in; if an agent is silent >1 h, stop it and dispatch fresh |
| A fixed 120 s git timeout killed real checkouts (LFS) on the user's repo; tests only used toy repos | Timeouts only on network/metadata calls, never on local work the user can cancel; compare designs with a working peer (herdr) before inventing |
| Releasing a held test hook by closing a FIFO strands readers that open after the close on macOS (T37) | Release held hooks by writing bytes (adapter tests' `Gate`), never by closing |
| Builders twice used `--no-verify` on intermediate commits (T33, T37) | Say again in every dispatch: no `--no-verify`, even for WIP; commit WIP with the hook or don't commit |
| A builder rebuilt its branch from a staged diff and dropped later edits; its commit message described work not in the commit (T37) | Reviewers check the diff against the commit message's claims; builders run the gate on the exact committed tree (the pre-commit hook does) and never restack by hand |
| Paths compared in core came from different spellings (configured root vs canonicalized cwd) — T26 took 4 rounds | Canonicalize at the boundary once (roots at startup, client paths on arrival) so core only ever sees one spelling; reviewers probe symlinked roots |
| A test fixture's `git init` inherited the pre-commit hook's `GIT_DIR`/`GIT_INDEX_FILE` and set `core.bare=true` on the real repo for ~1 min (T26) | Every test that runs git clears its env (keep PATH), uses a throwaway HOME and empty git config; reviewers grep new tests for `Command::new("git")` without `env_clear` |
| Agents share one scratchpad; one overwrote another's `mutate.py` mid-run | Tell agents to use task-prefixed file names in the scratchpad (`f08-…`) |
| PM wrapped a merge in `git stash`/`stash pop` on a clean tree; the pop applied the user's old stash | Never use `git stash` in the merge queue. Commit plan edits first; merge only on a clean tree (`git status --short` empty) |
