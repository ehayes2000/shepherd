# Status page
milestone: M5
goal: One page lists every agent on every machine, triaged by what you owe it, and lets you answer or message agents without attaching

From the TUI mocks (`docs/mocks/shepherd-tui.html`, "Status page" tab), approved by the user 2026-10-31. It's a page in the
host-owned layout (decision 0022, T48/T49): `ctrl-b s` selects it, and the page bar makes it clickable.

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T57 | Status table | done | 27c7f26 | 1 | round 1 fixed exactly as asked, each mutation shown; merged without a 2nd review |
| T58 | Preview and answer in place | todo |  |  |  |
| T59 | Events feed and message line | building | task/t59-events-message |  |  |

## Specs

### T57: Status table
depends: T49
touches: core::client, tui (status view)
goal: Every agent on every machine in one table, grouped by what you owe it.
acceptance:
- A `View(Status)` page: one table of every session on every reachable machine (T31b's per-host state). Grouped by state:
  needs you, working, idle, exited; `g` (and a clickable toggle) switches to grouping by machine. A header line counts each
  group.
- Columns: status glyph, name, agent, machine (in its color, T62), subagent count (blank until T54 lands), what it's
  doing (last activity / pending question), PR and checks.
- Rows are clickable (select), the table scrolls with the selection kept visible, `⏎` or double-click attaches (goes to the
  session's pane).
- Tests through the T45 harness: both groupings (snapshots), counts, an offline machine's sessions shown dimmed, scrolling,
  tiny window, hostile names display-safe.
scope: crates/core, crates/tui
notes: Decision 0014: everything clickable; `ctrl-b s`, `g`, `⏎` are the optional keys.

### T58: Preview and answer in place
depends: T57, T51
touches: tui (status view), core::client
goal: See the selected agent's live screen and answer its prompt without switching sessions.
acceptance:
- A preview pane shows the selected session's live screen (the client's emulator for it), read-only.
- When that session's agent is asked something (T51), the preview shows the question with buttons for yes / always / no
  (keys `y`/`a`/`n`); choosing one sends `_shepherd/session/answer`. A refused answer (already answered) says so.
- `⏎` attaches.
- Tests: preview follows selection; answer buttons appear only while a question is pending; click and key both answer;
  stale answer refused.
scope: crates/tui, crates/core

### T59: Events feed and message line
depends: T57, T47
touches: tui (status view), core::client
goal: Watch what's happening across agents and message one or a group.
acceptance:
- An events feed: the latest host events across sessions (started, asked, answered, prompted with its origin, finished,
  PR changes), newest last, each clickable to select its session. No "loops watching" label until loops exist.
- A message line that prompts the selected agent (T47, `by: client`); `tab` (or a clickable target chip) widens the target
  to a group: all working, all idle, all on a machine. Sending to a group prompts each eligible agent and reports which
  were refused (busy, wants-user) and why.
- Tests: feed contents and order, click-to-select, single and group send, partial refusal report.
scope: crates/tui, crates/core
