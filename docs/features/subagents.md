# Subagents
milestone: M5
goal: Each session shows its agent's live subagents as a tree in the sidebar, attention from a blocked subagent bubbles up, and a subagent's transcript can be read

From the TUI mocks (`docs/mocks/shepherd-tui.html`, "Subagents" tab), approved by the user 2026-10-31.

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T54 | Subagent lifecycle in the event log | ready |  |  |  |
| T55 | Subagent tree in the sidebar | todo |  |  |  |
| T56 | Read a subagent's transcript | todo |  |  |  |

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
scope: crates/core, crates/tui, crates/shepherd
