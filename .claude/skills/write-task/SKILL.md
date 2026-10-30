---
name: write-task
description: (PM) Break a shepherd feature into builder-sized task specs in docs/features/<feature>.md with testable acceptance criteria, scope, and dependencies. Use when planning new work or when a builder reports a follow-up.
---

# Writing tasks

A builder knows only what's in the spec, plus CLAUDE.md and the architecture doc. If the spec is vague the
builder will guess, and guesses cause regressions. Write it so two different builders would produce
equivalent code.

## Sizing
- About one sitting: one behavior slice, roughly 50–400 lines including tests.
- It must be finishable with the gate green. No "part 1 of 3, leaves things broken."
- Split by **vertical behavior** ("new session creates worktree"), not by layer ("write the git code").
  The exception is port creation: a port (trait, fake, contract suite, adapter) is a good standalone task.

## Where it goes
Every task belongs to one feature file, `docs/features/<feature>.md`. Pick the feature it serves (a follow-up goes in
the feature of the task it came from). A genuinely new feature gets a new file: `# <title>`, then `milestone:` and
`goal:` lines, then `## Tracker` (the table, copied from any feature file) and `## Specs`. Ids are global: take the next
free `T<NN>` / `F<NN>` (`grep -rho '^### [TF][0-9]*' docs/features | sort -V | tail`).

Add a row to the feature's `## Tracker` table (`| T<NN> | <title> | todo |  |  |  |`; no `|` inside cells) and the spec
block under `## Specs`. The tracker row is the only place status lives. Then run `scripts/tasks.py` to regenerate the
index in `docs/plan.md` (the gate checks it).

## Template (under `## Specs`)
```markdown
### T<NN>: <imperative title>
depends: T<a>, T<b>
touches: core::session, ports::worktrees, tui::sidebar      # for conflict detection
goal: <one or two sentences of user-visible outcome>
acceptance:
- <observable behavior, phrased so it maps to a test>
- <error / edge case>
scope: <crates/modules the builder may change>
out of scope: <tempting adjacent work NOT to do>
notes: <decided trait shapes, names, links to architecture decisions>
```

## Acceptance criteria
- Each one is observable through a public surface: a `Command` emitted, a state accessor, rendered output, or a
  contract case. Write "when X, then Y", not "implement Z".
- Include at least one failure or edge case per task.
- If the task adds or changes a port trait, **write the trait signature in `notes`**. Trait shape is an
  architecture decision, so builders don't invent it.

## Dependencies and parallelism
- `depends` lists tasks whose code this task needs.
- `touches` lists modules and ports. Two tasks with overlapping `touches` don't run at the same time, even if
  neither depends on the other.
- Put shared foundations (domain types, new port traits) in their own early task so later tasks can run in parallel.

A task is `ready` when its deps are `done` and its spec has no open questions.
