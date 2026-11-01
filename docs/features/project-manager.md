# Project manager view
milestone: M5
goal: A Claude Code session you talk to runs your other agents through shepherd's MCP tools, with a panel showing what they're doing and what's waiting on you

From the TUI mocks (`docs/mocks/shepherd-tui.html`, "Project manager" tab, three variants: morning briefing, dispatching,
asking you), approved by the user 2026-10-31. It's a page in the host-owned layout (decision 0022): `ctrl-b p` selects it.
The user removed the plan and permission sections from the panel: it has exactly two sections. Don't add others.

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T60 | Project manager session and page | todo |  |  |  |
| T61 | Project manager panel | todo |  |  |  |

## Specs

### T60: Project manager session and page
depends: T53, T49
touches: core::config (pm profile), core::layout, shepherd (host), tui
goal: `ctrl-b p` opens a page with an ordinary Claude Code session that has shepherd's MCP server.
acceptance:
- A built-in `pm` profile launches Claude Code with the `shepherd mcp` server configured for that session only (through
  the agent's per-session config flag; cite its docs), never by editing the user's `~/.claude*`. The user can override the
  profile in config.
- The PM page is a layout page: the PM session on the left, the PM panel (T61) on the right. Opening it starts the PM
  session if there isn't one, or shows the existing one. It's selectable from the page bar (decision 0014) and `ctrl-b p`.
- The PM is an ordinary session: its own row in the sidebar, archived like any other, resumable.
- Tests: the profile's launch command and env (no user config touched), page creation and reuse, harness snapshot.
scope: crates/core, crates/shepherd, crates/tui
notes: Builders never run the real Claude Code; `scripts/e2e-agents.sh` gains a PM round (it lists sessions through the
MCP tool) for the user to run.

### T61: Project manager panel
depends: T60, T51, T54
touches: tui (pm panel), core::client
goal: Beside the PM, see what its agents are working on and every decision waiting on you.
acceptance:
- Exactly two sections:
  - WORKING: one line per working agent: status glyph, name, machine (in its color), and the last thing the PM told it
    (the latest `session/prompted` with `by: mcp:<pm session>`, clipped).
  - WAITING ON YOU: two lines per item: the agent (or machine), then the question. Items are every pending question (T51)
    or blocked subagent (T54) across machines, plus questions the PM itself is asking.
- Clicking a WORKING line selects that session; clicking a WAITING item shows it (and, for an agent's permission question,
  offers the same answer buttons as T58).
- Tests through the harness for the mock's three variants (snapshots): morning briefing, dispatching, the PM asking with
  Claude Code's picker (the picker is the PM's own PTY content; the panel shows the question in WAITING). Empty sections
  say so. Hostile text display-safe.
- From T47's review: `session/prompted`'s `by` is the client's own claim (any local client can write `mcp:<session>`); the
  panel treats it as a label, never as proof of who sent it.
scope: crates/tui, crates/core
