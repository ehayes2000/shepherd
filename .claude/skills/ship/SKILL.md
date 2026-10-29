---
name: ship
description: Finish a shepherd builder task - run the gate, self-review the diff, commit on the task branch with a conventional message. Use at the end of every builder task. Does not merge or open PRs (the PM does).
---

# Shipping a task

1. `cargo fmt --all`
2. `scripts/check.sh` must be green. If it's red, fix it. Never use `--no-verify` and never weaken a test.
3. **Self-review** `git diff main...HEAD` as if you were the reviewer:
   - every acceptance criterion has a test that would fail without the change
   - no stray debug output, `dbg!`, commented-out code, or TODOs without a plan.md follow-up
   - no files outside the task's scope
   - any changed existing test or snapshot is justified
   - every guarantee your docs or report claim ("never follows symlinks", "no Line after close") has a test that would fail
     without it. If you can't test a claim, don't make it.
4. Commit on `task/<id>-<slug>`. Use a conventional, one-line message (`feat:`, `fix:`, `test:`, `refactor:`,
   `chore:`). If you changed an existing test's assertion or a snapshot, add a body line saying why. Prefer one
   commit per task. Use several only if they're each meaningful on their own.
5. Don't merge, push to main, or open a PR. Write your report (format in the `builder` agent definition).
