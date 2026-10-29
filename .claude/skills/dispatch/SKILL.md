---
name: dispatch
description: (PM) Run ready shepherd tasks from docs/plan.md through builder subagents in isolated worktrees, independent reviewer subagents, and a serial merge queue that re-runs the gate. Use when executing the plan.
---

# Dispatching work

You are the PM. You don't write feature code here. You schedule, review and merge.

## 1. Pick a batch
- Read `docs/plan.md`. Candidates are tasks with `status: ready`.
- Choose up to **3** whose `touches` sets don't overlap. Prefer tasks that unblock the most others.
- Mark them `in-progress` in plan.md.

## 2. Spawn builders, in parallel, in one message
For each task, call `Agent` with `subagent_type: "builder"` and `isolation: "worktree"`. The prompt is:
- the full task spec block, copied verbatim from plan.md
- the main repo path (so the builder can check it isn't working there)
- the current `main` commit hash as the base (worktrees may otherwise start from a stale `origin/main`)
- "Branch name: task/<id>-<slug>"

Don't add hints about how to implement it unless they're in the spec. If you want to add a hint, it belongs in the spec.

## 3. Review each finished builder
- Read the builder's report. If it's **blocked**, resolve it (amend the spec, record a decision in
  `docs/architecture.md`, or split the task), set the task back to `ready`, and re-dispatch.
- Spikes (docs-only research tasks) are reviewed by the PM directly: check the evidence, accept or reject the
  recommendation, record the decision.
- Otherwise spawn a `reviewer` with **only** the spec block and the branch name. Never pass along the builder's report.
- `CHANGES` → send the findings to the builder with SendMessage (same agent, context intact) and review again
  when it's done. After 2 failed rounds, stop and rethink the spec. The spec is probably the problem.

## 4. Merge queue (serial, one branch at a time)
```
git switch main
git merge --no-ff task/<id>-<slug>   # or rebase onto main first if it no longer applies cleanly
scripts/check.sh                     # on the merged result, every time
```
- If the gate is red after merging, run `git reset --hard ORIG_HEAD` and send it back to the builder. Branches
  green on their own can still conflict in behavior. The second merge is the one that rebases.
- If the gate is green, the merge stays on local `main`. Don't push. The user pushes or asks for a PR.
  Mark the task `done` and record the merge commit hash in plan.md.
- Delete the merged task branch and its worktree.

## 5. Learn
After each batch, check the reports for:
- **Follow-ups** → new tasks via `write-task`.
- **"Why didn't tests catch this"** answers → a missing contract case, builder method or lint. Make it a task.
- **Repeated reviewer findings** → the rule is unclear. Tighten CLAUDE.md, the architecture doc or a skill, or
  make it mechanical in `scripts/check-arch.sh` or clippy config. Anything reviewers flag twice should be checked
  mechanically.

Then pick the next batch.
