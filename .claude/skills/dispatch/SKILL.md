---
name: dispatch
description: (PM) Run ready shepherd tasks (tracked in docs/features/*.md) through builder subagents in isolated worktrees, independent reviewer subagents, and a serial merge queue that re-runs the gate. Use when executing the plan.
---

# Dispatching work

You are the PM. You don't write feature code here. You schedule, review and merge. `docs/pm.md` is the full manual.
**Update the task's tracker row on every transition** with `scripts/tasks.py set <ID> <status> [--branch ..] [--round ..]
[--note ..]` (statuses: todo, ready, building, review, fixing, merging, blocked, parked, done), then commit and push. It's
the only state that survives a context reset; `docs/plan.md`'s In flight table is generated from it.

## 1. Pick a batch
- Read `docs/plan.md`'s generated "Ready to start" table (ready tasks, and todo tasks whose dependencies are done).
- Choose up to **5** whose `touches` sets don't overlap, or overlap only in separate files of one module (then fix a merge
  order in the In flight notes and tell each builder which concurrent tasks share its files). Prefer tasks that unblock the most others.
- `scripts/tasks.py set <ID> building --branch task/<id>-<slug> --round 0` for each.

## 2. Start builders (Codex agents), in parallel
Builders and reviewers run as Codex agents (Codex Sol on the Fast tier), not Claude Code subagents (user's choice,
2026-11-01). For each task, write a brief file in the scratchpad (unique name) containing:
- the full task spec block, copied verbatim (`scripts/tasks.py show <ID>`)
- the current `main` commit hash as the base, and "rebase onto current local main before committing"
- the PM's context notes (what's on main to reuse, concurrent tasks to stay clear of)

then start it in the background (one Bash call per task, `run_in_background: true`):
```
scripts/codex-agent.sh build <ID> task/<id>-<slug> <brief-file>
```
It makes the worktree `.claude/worktrees/codex-<ID>` on the branch from `main`, and tells the agent to read CLAUDE.md,
`.claude/agents/builder.md` and the architecture doc. When it exits, read the report at `.claude/codex/<ID>/build.md`.

Don't add hints about how to implement it unless they're in the spec. If you want to add a hint, it belongs in the spec.

## 3. Review each finished builder
- Read the builder's report. If it's **blocked**, resolve it (amend the spec, record a decision in
  `docs/architecture.md`, or split the task), set the task back to `ready`, and re-dispatch.
- Spikes (docs-only research tasks) are reviewed by the PM directly: check the evidence, accept or reject the
  recommendation, record the decision.
- Otherwise start a reviewer with **only** the spec block (plus PM decisions to judge) and the branch name, never the
  builder's report: `scripts/codex-agent.sh review <ID> task/<id>-<slug> <brief-file>` in the background; its report is
  `.claude/codex/<ID>/review.md`. Each review round is a fresh reviewer.
- `CHANGES` → write the findings to a message file and continue the same builder session, context intact:
  `scripts/codex-agent.sh resume <ID> build <message-file>` (background), then review again when it's done. After 2 failed rounds, stop and rethink the spec. The spec is probably the problem.

## 4. Merge queue (serial, one branch at a time)
```
git switch main
git merge --no-ff task/<id>-<slug>   # or rebase onto main first if it no longer applies cleanly
scripts/check.sh                     # on the merged result, every time
```
- If the gate is red after merging, run `git reset --hard ORIG_HEAD` and send it back to the builder. Branches
  green on their own can still conflict in behavior. The second merge is the one that rebases.
- If the gate is green, the merge stays on local `main`. Don't push. The user pushes or asks for a PR.
  `scripts/tasks.py set <ID> done --branch <merge hash> --round <n> --note "<notable findings>"`.
- Delete the merged task branch and its worktree.

## 5. Learn
After each batch, check the reports for:
- **Follow-ups** → new tasks via `write-task`.
- **"Why didn't tests catch this"** answers → a missing contract case, builder method or lint. Make it a task.
- **Repeated reviewer findings** → the rule is unclear. Tighten CLAUDE.md, the architecture doc or a skill, or
  make it mechanical in `scripts/check-arch.sh` or clippy config. Anything reviewers flag twice should be checked
  mechanically.

Then pick the next batch.
