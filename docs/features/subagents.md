# Subagents
milestone: M5
goal: Each session shows its agent's live subagents as a tree in the sidebar, attention from a blocked subagent bubbles up, and a subagent's transcript can be read

From the TUI mocks (`docs/mocks/shepherd-tui.html`, "Subagents" tab), approved by the user 2026-10-31.

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T54 | Subagent lifecycle in the event log | done | 7157843 | 5 | round 5: merged with rare cross-turn repeat-start residual → F57 |
| T55 | Subagent tree in the sidebar | done | 1b91ee4 | 2 | round 2 APPROVE; minors → F55 |
| T56 | Read a subagent's transcript | todo |  |  |  |
| F57 | Subagent task pairing: cross-turn repeat start and failed guessed call | todo |  |  |  |

## Specs

### T54: Subagent lifecycle in the event log
depends: T19b
touches: core::agents, core::host, core::acpp
goal: The host knows each session's subagents (started, working, blocked, finished) from the agent's own hooks.
acceptance:
- Claude Code: `SubagentStart` / `SubagentStop` hooks (and the Task tool's `PreToolUse`/`PostToolUse` if the start hook
  lacks the task description); Codex where it exposes subagents (cite its source; if it doesn't, say so and Codex sessions
  show no tree). Cite each agent's current hook schema, with recorded fixtures.
- Frames in the session's log: `_shepherd/subagent/started { id, agent_type, task }`, `.../blocked { id, kind, text }`,
  `.../finished { id }`; the parent's turn end drops finished subagents from the live count.
- The host's session state carries the live subagents; a subagent blocked on permission or a question makes the session
  want the user (it bubbles up even while the parent is working).
- Core tests from fixtures: start, block, finish, parent turn end, a subagent that never reports finishing (dropped at the
  parent's turn end), and hostile text in `agent_type`/`task` (kept raw in the log, display-safe when drawn).
scope: crates/core, crates/shepherd, crates/testkit
notes: (binding) Hook payload parsing stays pure in core. Builders never run the real agents; `scripts/e2e-agents.sh`
(T44) gains a subagent round to re-record fixtures.

### T55: Subagent tree in the sidebar
depends: T54, T45
touches: core::client, tui::sidebar
goal: A session row shows a live subagent count, and the tree under it is one click away.
acceptance:
- The row's badge is the live count: `▸N` collapsed, `▾N` expanded; no badge with none. Clicking the badge (or `⇥` on the
  selected row) expands or collapses it.
- Expanded: one line per subagent under its parent: `├`/`└`, status glyph, agent type, task (clipped). Finished ones stay,
  dimmed, until the parent's turn ends, then drop out of the tree and the count.
- A subagent blocked on a prompt turns the parent's glyph `▲`, collapsed or not.
- Tests through the T45 harness: snapshots collapsed and expanded, the `▲` bubble, finished-dimmed then gone at turn end,
  click targets, a tree taller than the sidebar (the selection stays visible), tiny window.
- From T54: the parent's turn end drops every subagent, including a background one still running (it reappears if it
  blocks). If T44's real-agent round shows background subagents are common, keep unfinished ones listed across a turn end.
  T54's review: a background subagent whose dialog is open when the parent's Stop fires loses its wait, so the session
  stops wanting the user until it blocks again (under-alerting). Keeping unfinished ones across a turn end fixes both.
scope: crates/core, crates/tui
notes: Decision 0014: the badge is clickable; `⇥` is the optional key.

### T56: Read a subagent's transcript
depends: T55, T47
touches: core::client, tui, shepherd (host)
goal: Selecting a subagent shows what it's doing, read-only.
acceptance:
- Selecting a subagent row shows its transcript (Claude Code's `<session>/subagents/*.jsonl`, read through T47's
  `Transcripts` port and path checks) as readable turns in the main pane, following new lines while it runs.
- The pane says it's read-only and that the subagent's prompts are answered in the parent session (with a clickable
  "go to parent").
- Tests: a fixture transcript rendered (snapshot), live append, a transcript that's gone or refused shows why, hostile
  content display-safe.
- From T47's review: the Transcripts adapter's missing-vs-failed split (crates/shepherd/src/archive.rs ~192) is untested
  for an unreadable file: add a contract or adapter case with a chmod-000 transcript asserting `Failed`, not `reply: null`.
scope: crates/core, crates/tui, crates/shepherd

### F57: Subagent task pairing: cross-turn repeat start and failed guessed call
depends: T54
touches: core::agents::subagents
goal: From T54's round-5 review (merged with these open): two rare sequences can still leave a subagent with another call's
task, against the "own call's, or none" rule.
acceptance:
- `end_turn` forgets which subagents started, so a repeat SubagentStart in a later turn guesses. Remember started,
  unstopped ids across turns (bounded like `named`) so a repeat start never guesses. Regression test: UserPromptSubmit,
  Pre.Agent.second, Post.Agent.second, Start.second, UserPromptSubmit, Pre.Agent, Start.second, PostToolUseFailure.Agent →
  agent-8b4c keeps "Find fibonacci callers".
- Decide and test what a failed call that was guessed for a subagent does (retract, or keep, with the reasoning in the doc).
- Minor: fix the comment in `a_retraction_spares_a_task_already_corrected_to_its_own_call` (7f3a does guess; the sequence is
  out of documented order on purpose); the retraction compares task text, not origin (note it or track origin); the
  `wrong.task.is_some()` guard is untested (test or drop).
- Until this lands, the module doc says the guarantee holds except these cases.
scope: crates/core

