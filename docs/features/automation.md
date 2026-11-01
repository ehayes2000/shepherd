# Automation: drive agents and the UI from a CLI
milestone: M5
goal: Agents and scripts can run shepherd without a human: start sessions, prompt an agent and get its reply, and arrange the screen (pages, splits), and every UI change is testable headless

The user's request (2026-10-31): prompt an agent running inside shepherd from a CLI (so a manager agent can drive
subagents), manage the UI from a CLI (agents will split the screen and more), and be able to test complex UI, since there
will be several pages. Decision 0022 records the choices: the host owns the layout, and `shepherd prompt --wait` returns the
agent's reply. This amends the manifesto's "the initial version will not handle splits or tabs".

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T45 | Headless UI test harness | done | caa414b | 1 | APPROVE; exemption wider than gap (removed by F55) |
| T46 | CLI client: list and start sessions | done | 627e4d3 | 2 | round 2 APPROVE; 2 minors → T47 |
| T47 | Prompt an agent and wait for its reply | done | 1f2c898 | 2 | round 2 APPROVE; minors → T56, T61 |
| T48 | Host-owned layout: pages and splits in core | done | f27c9dc | 1 | APPROVE; seeded property tests (b0b110b) |
| T49 | Render pages and split panes | done | 18a0aff | 2 | round 2 APPROVE; footer-height minor → F56 |
| T50 | `shepherd ui` CLI and screenshots | done | 3efa8cc | 2 | round 2 APPROVE; focused_on dup → F56 |
| T51 | Answer an agent's pending question | review | task/t51-answer @ e766620 |  | built; no question id on answer (PM: add optional asked seq); Claude always=Shift+Tab unverified (T44) |
| T52 | Read a session's screen as text | building | task/t52-screen-text |  |  |
| T53 | Shepherd MCP server for agents | todo |  |  |  |

## Specs

### T45: Headless UI test harness
depends: T32
touches: testkit (ui harness), tui tests
goal: A UI test can drive the real client and renderer the way a user does (click on what's drawn, type, resize) and assert
on what's on screen, so complex UI (several pages, splits) is cheap to test before it's built.
acceptance:
- A harness in `crates/testkit` (or a `tui` test-support module) runs a real `Client` against a scripted fake host and
  renders with ratatui's `TestBackend` at a given size. Steps: `click("text")` clicks the cell of a visible label (fails,
  naming what's on screen, if the label is absent or ambiguous), `click_at(x, y)`, `scroll`, `type`/`key`, `resize(w, h)`,
  `host(event)` to inject host messages. Assertions: `screen()` as text, `contains`, `region(x, y, w, h)`, and an
  `insta` snapshot of the screen at any step.
- Existing point-and-click tests (sidebar, popups, finder) are expressible in it: port at least three of them, showing
  they read shorter and still fail when the behaviour breaks (mutation).
- A property test: random sequences of UI steps (clicks on visible labels, keys, resizes down to 1x1) never panic and keep
  the documented invariants (every visible button is hittable, the selection stays visible). Seeded and reproducible;
  a failure prints its seed and steps. Bounded by step count, not time.
scope: crates/testkit, crates/tui (tests), crates/core (test-support only)
notes: (binding) No sleeps or wall clock; the harness is deterministic. It replaces nothing yet: old tests stay unless
ported. Decision 0014 (point and click first) is why clicking on drawn text is the primary step.

### T46: CLI client: list and start sessions
depends: T31b
touches: shepherd (cli), core::wire (if needed)
goal: A script or agent can see every session and start one, the building blocks for a manager agent.
acceptance:
- `shepherd sessions [--json]` lists every session on this machine and its reachable peers (id, name, machine, agent,
  status: running / idle / wants-user / exited, workspace, branch, pr), newest first. Human output through `display_safe`.
- `shepherd new [--profile P] [--machine M] [--cwd DIR] [--name N] [--json]` starts a session (a managed worktree unless
  `--cwd`) and prints its id once the host has created it; failures (unknown profile, unreachable machine) exit non-zero
  with the reason.
- Sessions are addressed by id or by a unique name (an ambiguous or unknown name is an error listing the candidates); a
  shared `Target` resolver is used by every later session command.
- `--json` shapes are documented in `docs/cli.md` and each covered by a test. Tests run the binary against a real host on
  fakes with a tempdir HOME.
scope: crates/shepherd, crates/core
notes: The CLI is a short-lived client over the existing link (and `_shepherd/tunnel` for peers); it reuses core's client
fold for state rather than a second parser.

### T47: Prompt an agent and wait for its reply
depends: T46, T42
touches: core::host, core::wire, core::agents, shepherd (cli, host), crates/archive (transcript reading)
goal: A manager agent can prompt a subagent and get its answer: `shepherd prompt <session> "..." --wait`.
acceptance:
- New request `_shepherd/session/prompt { session, text }`: the host submits `text` to the agent as one prompt (the way the
  agent's own input accepts a pasted multi-line prompt, then submit; cite each agent's docs or source for the bytes) and
  answers once it's sent. An agent that isn't at its prompt (busy, or wanting the user) is refused with its status, never
  typed into; `--queue` waits for it to go idle first (bounded by `--timeout`).
- With `--wait`, the CLI waits for that prompt's turn to end and prints the agent's final reply. `--json` gives
  `{ session, status, reply }`. If the agent instead stops to ask the user (permission, elicitation), it returns at once
  with `status: "wants-user"` and what it asked, never hanging. `--timeout` bounds the wait (exit code says timed out); the
  prompt is not withdrawn.
- The reply is the agent's last assistant message of that turn, read from the transcript the hooks named, through the same
  path checks as capture (crates/archive), parsed per agent (Claude Code JSONL, Codex rollout) from recorded fixtures.
- Two prompts to one session at once: the second is refused as busy. A session with no agent (a plain shell) is refused.
- From T46's review: the CLI's `connect()` wiring of `set_catch_up_limit` is untested (`cli_scenarios` builds its own
  runtime); share one setup helper with the scenarios. And test `new --machine <this machine> --cwd <relative>` is
  accepted relative to where the command runs.
- `shepherd reply <session> [--json]` prints the last reply without prompting; `shepherd wait <session> [--timeout]`
  waits for idle or wants-user.
- Scenario test with a fake agent: new session, prompt, wait, reply comes back; busy refusal; wants-user return; timeout.
- Each prompt is recorded in the session's log as `_shepherd/session/prompted { text, by }` (`by`: `cli`, `mcp:<session>`
  for an agent such as the project manager, or `client`), so the UI can show the last thing an agent was told and by whom
  (status page, PM panel). Long text is kept whole in the log and shortened only when drawn.
scope: crates/core, crates/archive, crates/shepherd, crates/testkit
notes: (binding) The host does the typing, so every client sees the prompt in the session's screen. Port shape for
reading a reply (PM decision): `trait Transcripts { fn last_reply(&self, req: &ReplyRequest) -> Result<Option<Reply>,
TranscriptError>; }` with `ReplyRequest { agent: AgentKind, agent_home: PathBuf, transcript_path: PathBuf,
since: Option<TurnMarker> }`; a stateful fake in testkit and a contract suite run against the fake and the archive-crate
adapter. Builders never run the real agents: fixtures first, and `scripts/e2e-agents.sh` (T44) gains a prompt round trip
for the user to run.

### T48: Host-owned layout: pages and splits in core
depends: T31b
touches: core::layout (new), core::host, core::wire, core::client
goal: The screen's arrangement is data the host owns, so a CLI, an agent or any window can change it and every window
shows it.
acceptance:
- A pure `core::layout` model: `Layout { pages, current }`, `Page { id, name, root }`, `Pane = Split { id, dir, ratio, a,
  b } | Leaf { id, content }` with `content = Session(SessionId) | Empty | View(ViewKind)` (`ViewKind` is an open enum for
  future pages: archive browser, cleanup, ...). Stable ids for pages and panes.
- Operations, each a pure function returning the new layout or a typed error: page new / select / rename / close; split a
  pane (horizontal or vertical, ratio); close a pane (its sibling takes its place); put a session or view in a pane; move
  focus; set a ratio (clamped). Closing the last pane leaves one Empty pane; a closed session's panes become Empty.
- The host holds the layout, records each change in its log (survives a host restart and a hot swap) and broadcasts it to
  subscribed clients; new wire requests `_shepherd/ui/get` and `_shepherd/ui/apply { op }` (local clients only, decision
  0021) answer with the new layout or the error.
- Exhaustive core tests for every operation and error, plus a property test: any sequence of operations keeps ids unique,
  ratios in range, and every Session leaf naming a listed session.
- Clients fold the layout into their state; with only the default layout (one page, the selected session) today's UI is
  unchanged (every existing snapshot passes).
scope: crates/core, crates/shepherd, crates/testkit
- (Amended after the build, before review) `Content::Selected`: "whatever this window has selected", resolved per window;
  the default layout is one page with one `Selected` pane, so today's UI is unchanged and sidebar clicks stay per window.
  `Content::Session { machine: Option<MachineId>, id }` can pin a peer's session; `retain_sessions` empties only this host's
  own unlisted sessions.
notes: (binding) Decision 0022. Focus and selection: which pane is focused is per window (it's where that window's input
goes), not host state. PTY size with several panes: a session's PTY is sized to the pane it's shown in on the window that
last focused it; record the rule in the decision.

### T49: Render pages and split panes
depends: T48, T45
touches: tui (layout, draw, sidebar), core::client
goal: The TUI draws the host's layout: a clickable page bar, split panes each showing its session, and point-and-click
control of all of it.
acceptance:
- A page bar (only when there is more than one page) with clickable page names and a `+` to add one; panes drawn with
  borders and titles (session name and machine); the focused pane highlighted; input goes to the focused pane.
- Mouse: click a pane to focus it; drag a split border to change its ratio; each pane title has a small menu (split
  right, split down, close, move session here) that applies a layout op through the host.
- Each pane's session is resized to its pane (per T48's rule); a pane too small to be useful shows a placeholder, never a
  panic.
- Tests through the T45 harness: snapshots of two pages, a three-way split and a tiny window; click-to-focus, border drag,
  pane menu ops; the property test extended with layout ops.
scope: crates/tui, crates/core
- From T48's review: a pinned peer's machine id and `ViewKind::Other(name)` are only checked for being non-empty; draw them
  through `display_safe` and clipped, never raw.
notes: Decision 0014: every action is clickable; keys are optional extras.

### T50: `shepherd ui` CLI and screenshots
depends: T48, T46
touches: shepherd (cli)
goal: An agent can arrange the screen and see it.
acceptance:
- `shepherd ui get [--json]` prints the layout; `shepherd ui page new|select|rename|close`, `shepherd ui split <pane>
  right|down [--ratio R] [--session S]`, `shepherd ui put <pane> <session|view>`, `shepherd ui close <pane>`,
  `shepherd ui ratio <pane> R`: each applies one op and prints the new layout or the error.
- `shepherd ui screenshot [--size WxH] [--page P] [--json]` renders the current layout headless (core + tui at that size,
  with the sessions' current screens) and prints it as text, so an agent can see what a user would.
- `--json` shapes documented in `docs/cli.md`, each with a test; tests run the binary against a real host on fakes.
scope: crates/shepherd

### T51: Answer an agent's pending question
depends: T47
touches: core::host, core::agents, core::wire, shepherd (cli)
goal: A permission prompt or question an agent is blocked on can be answered without attaching: from the status page
(y/a/n), the CLI, or the project manager's `answer` tool.
acceptance:
- The host knows when a session's agent is blocked on a question and what it asked (Claude Code's `Notification` /
  permission hooks, Codex's equivalent; cite each agent's docs or source), recorded in the log as
  `_shepherd/agent/asked { kind: permission | question, text, choices? }` and cleared when it's answered or the turn moves on.
- `_shepherd/session/answer { session, choice }` (`yes`, `always`, `no`, or a choice index for a question) types the
  agent's own keys for that choice (cited per agent) only while that question is pending; otherwise it's refused with the
  session's status. `shepherd answer <session> <choice> [--json]`.
- Scenario tests with a fake agent: asked → answered → working; answer with nothing pending refused; a stale answer after
  the user already answered in the pane is refused, never typed.
scope: crates/core, crates/shepherd, crates/testkit
notes: Builders never run the real agents; `scripts/e2e-agents.sh` gains an answer round trip for the user to run.

### T52: Read a session's screen as text
depends: T46
touches: core::wire, shepherd (host, cli)
goal: An agent can see what a session shows (`read_pane`), as a user would.
acceptance:
- `_shepherd/session/screen { session, lines? }` answers the session's current screen (and up to `lines` of scrollback)
  as text from the host's emulator, with control sequences stripped and wide characters intact.
- `shepherd screen <session> [--lines N] [--json]`. Tests against a real host on fakes with a scripted PTY.
scope: crates/core, crates/shepherd

### T53: Shepherd MCP server for agents
depends: T46, T47, T51, T52
touches: shepherd (mcp, new), docs
goal: An agent (first the project manager) runs other agents through shepherd's own tools.
acceptance:
- `shepherd mcp` serves MCP over stdio (use a maintained MCP crate if one fits; record the dependency in the decision)
  with tools `status` (every session on every machine, as T46), `read_pane` (T52), `send` (T47 prompt, with optional
  wait), `answer` (T51), `new_session` (T46) and `stop` (close a session; refuses one with unsaved work unless told). Each
  tool's input and output schema is documented in `docs/cli.md` next to the CLI it mirrors.
- A tool call is recorded with its origin (`by: mcp:<calling session>`), so the UI shows what the agent did. The server
  only talks to the local host over its socket; it gets no capabilities the CLI doesn't have.
- Tests drive the server over stdio with scripted MCP requests against a real host on fakes: every tool, errors (unknown
  session, busy, nothing to answer), and schema snapshots.
scope: crates/shepherd, docs
notes: Decision 0022. The tools mirror the CLI one to one; no tool does anything the CLI can't.

