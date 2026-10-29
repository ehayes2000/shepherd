# Plan: initial feature set

The source of truth for work in progress. Only the PM edits statuses. Task format is described in the
`write-task` skill. Background reading: `manifesto.md`, `docs/architecture.md`, `docs/acpp.md`.

## Milestones

| milestone | outcome | tasks |
|---|---|---|
| **M0 Foundation** | Workspace scaffolded, gate enforced, two open questions answered | T00–T02 |
| **M1 Log + host/client skeleton** | ACP++ log, host daemon, and a TUI that creates a session running a shell and swaps between sessions | T03–T13 |
| **M2 Agents** | Claude Code and Codex run in sessions with live status in the sidebar, and resume after a host restart | T14–T19 |
| **M3 Workspaces & worktrees** | Every new session gets a managed worktree from the latest origin default branch. Includes naming, cleanup, search, and PR display | T20–T27 |
| **M4 Multi-machine** | Named machines, peers over iroh, and a single sidebar that drives sessions on any machine | T28–T32 |

Critical path: T00 → T03 → T04 → T07 → T10 → T11 → T12 → T13, then M2. Most of the rest can run in parallel once its
dependencies are done.

## Ports (planned)

| port | side | first task | real adapter |
|---|---|---|---|
| `EventLog` | host | T04 | JSONL files in `~/.shepherd` |
| `Pty` | host | T08 | portable-pty |
| `HostLink` / `ClientLink` | both | T10 | unix socket (M1), iroh (M4) |
| `Config` | both | T14 | `~/.shepherd/config.toml` |
| `Worktrees` | host | T20 | git CLI |
| `Commands` (run hook scripts) | host | T23 | tokio::process |
| `Forge` | host | T27 | `gh` CLI |
| `Peers` | host | T29 | iroh |

## Out of scope for the initial feature set

None of these are planned yet, but the log was designed so each one is additive:
- **ACP-hosted sessions** (shepherd as an ACP client running `claude-agent-acp` / `codex-acp`): real ACP frames in the
  same log. This is a prerequisite for the orchestrator.
- **Orchestrator agent**: an agent that reads session logs and appends prompts to them.
- **Loops / triggers**: `on <frame pattern> from <source> do <action>`, implemented as log subscriptions.
- **Unified MCP and prompt config** translated per agent. M2 only covers launch profiles.
- Splits and tabs, scrollback persistence, and log snapshots or compaction.

---

## M0 Foundation

### T00: Wipe v1 and scaffold the workspace
status: done
owner: PM (not dispatched, because builders assume this structure exists)
depends: none
touches: everything
goal: An empty but fully gated Cargo workspace that matches docs/architecture.md, so that builder tasks can start.
acceptance:
- `src/` is removed. `crates/{core,tui,adapters,testkit,shepherd}` exist with the package names in the architecture doc.
- Workspace `Cargo.toml` sets `[workspace.lints]`: `clippy::unwrap_used`, `expect_used`, `dbg_macro`, `todo`, and
  `print_stdout` as deny. `clippy.toml` sets `allow-unwrap-in-tests = true` and `allow-expect-in-tests = true`.
- `crates/core/clippy.toml` sets disallowed-methods/types for `std::fs`, `std::process`, `std::net`,
  `std::thread`, `std::env`, `Instant::now`, and `SystemTime::now`. A deliberate violation in a scratch commit is
  rejected by the gate (verified, then reverted).
- testkit has an empty `fakes` module, an empty `contracts` module, and builder skeletons. The `shepherd` crate
  has an empty `tests/` scenario harness file.
- `git config core.hooksPath .githooks` is set, and `scripts/check.sh` is green.
- `insta` is added as a dev-dependency where needed.
notes: v1 code stays in git history and can be consulted, but is never copied wholesale.

### T01: Spike: ACP schema types in core
status: done
depends: T00
touches: docs only
goal: Decide how core gets ACP types without breaking purity.
acceptance:
- Report the dependency tree of `agent-client-protocol-schema` (or whichever crate holds the ACP types in the
  current release). Say whether it can join `CORE_ALLOWED` (only serde-level deps, no async runtime).
- Recommend one of: (a) depend on the schema crate in core; (b) vendor a minimal typed subset in
  `core::acpp::acp`, covering only the methods we fold, with everything else kept raw.
- List the ACP methods and `session/update` variants we need for M1–M2, and the pinned ACP version.
out of scope: writing production code.
notes: Commit the report as `docs/spikes/T01-acp-types.md` on the task branch. The PM turns it into a decision entry
in architecture.md and a note in T03.

### T02: Spike: terminal emulator
status: done (0c92cc6)
depends: T00
touches: docs only
goal: Pick the VT emulator for rendering PTY output in the TUI. v1's emulation bugs were a main failure.
acceptance:
- Compare `alacritty_terminal`, `vt100`, and `wezterm-term` (or its maintained fork) on: correctness with Claude
  Code and Codex TUIs (alt screen, 24-bit color, wide chars, cursor styles, bracketed paste, mouse modes),
  resize behavior, API fit for drawing into a ratatui `Buffer`, maintenance, and dependency weight.
- Include a recorded byte fixture of a real `claude` session (stored under `crates/term/tests/fixtures/`) to use for
  the comparison. Note how to re-record it.
- Give a recommendation and the `TUI_ALLOWED` change it implies.
out of scope: integrating it.
notes: Commit the report as `docs/spikes/T02-terminal-emulator.md` plus the fixtures on the task branch. Throwaway
comparison code goes in a scratch dir outside the repo, not in the workspace.

---

## M1 Log + host/client skeleton

### T03: ACP++ envelope and frame types
status: done (3540e22)
depends: T00, T01
touches: core::acpp
goal: Typed, lossless parsing and serialization of ACP++ entries per docs/acpp.md.
acceptance:
- `Envelope` round-trips through JSON with every field in docs/acpp.md, and unknown envelope fields survive the round trip.
- `Frame` parses into `Acp(..)` (the methods chosen in T01), `Shepherd(ShepherdFrame)` (the initial
  `_shepherd/*` table), or `Unknown(raw)`. Any well-formed JSON-RPC 2.0 message parses. None error.
- An unknown method, unknown `_meta` keys, and unknown `stopReason` values are preserved byte-equivalently
  (JSON value equality) when re-serialized.
- Malformed JSON-RPC (missing `jsonrpc`, or both `result` and `error`) is a typed error.
- `FrameDraft` (frame + source + dir + session) is what producers create. Only the host turns it into an `Envelope`.
- Synthesized frames are marked with `_meta.shepherd.synthesized`, and a helper reports whether a frame is synthesized.
- `StopReason`, `ToolCallStatus`, `ToolKind`, `SessionUpdate`, and `ContentBlock` are open enums with an
  `Other`/raw catch-all. An unknown `sessionUpdate` or `stopReason` parses successfully.
- JSON-RPC responses parse as a generic `Response { id, result | error }`. Typed interpretation of a response
  (e.g. `session/prompt` → `stopReason`) happens by correlating it with its request's method, through a helper the
  folds can use (`PendingRequests` mapping `id → method`).
- A dev-dependency conformance test checks that sample frames accepted by `agent-client-protocol-schema =1.13.0`
  round-trip identically through our types.
scope: crates/core
notes: Keep the raw `serde_json::Value` next to the typed parse (see "Lossless" in acpp.md). Decision 0006 and
`docs/spikes/T01-acp-types.md` §3–§5 are binding: types live in `core::acpp::acp` and cover exactly the methods and
variants in §5. `agent-client-protocol-schema` is allowed **only** as a core dev-dependency.

### T04: `EventLog` port
status: todo
depends: T03
touches: ports::event_log, testkit, adapters::event_log
goal: Persist and read ACP++ logs per session and for the host.
acceptance:
- Contract: `append(draft, stamp) -> Envelope` assigns gapless `seq` starting at 1 per log. `read(log, from_seq)`
  returns entries in order. `logs()` lists the session logs that exist. Reopening returns identical entries.
- Contract: appends to different logs are independent, so seq numbering is per log.
- Fake passes the contract. The fake can be preloaded with entries for tests.
- JSONL adapter passes the contract in a tempdir. A torn final line is truncated on open and the log stays
  usable. Corruption in the middle is a typed error.
- Wired into the testkit harness defaults.
scope: core::ports, testkit, adapters
notes: The trait is sync (append+flush is fast). `Stamp { id, ts, machine }` (from `core::acpp`) comes from the runtime, so core
stays pure. The log assigns `seq` and seals with `FrameDraft::seal(seq, stamp)`.
`trait EventLog { fn append(&mut self, log: LogId, draft: FrameDraft, stamp: Stamp) -> Result<Envelope, LogError>; fn read(&self, log: LogId, from: Seq) -> Result<Vec<Envelope>, LogError>; fn logs(&self) -> Result<Vec<LogId>, LogError>; }`
with `LogId = Host | Session(SessionId)`.

### T05: Session activity fold
status: done (6919323)
depends: T03
touches: core::session
goal: Derive agent status from a session's frames. This is the heart of the sidebar.
acceptance:
- `SessionState::apply(&Envelope)` folds into an `Activity`: `Starting`, `Working { tool: Option<ToolSummary> }`,
  `NeedsPermission { title }`, `NeedsInput`, `Idle`, `Exited { code }`, or `Unknown`.
- Table-driven tests cover each transition: a prompt request leads to Working, a `tool_call` leads to Working with
  that tool, a `tool_call_update` completed clears the tool, `session/request_permission` leads to
  NeedsPermission, a later tool update or prompt response clears it, the prompt response leads to Idle,
  `_shepherd/agent/idle` leads to Idle, and `_shepherd/agent/exited` leads to Exited.
- Unknown frames don't change state. A property test folds arbitrary JSON-RPC frames and never panics.
- `attention()` is true for NeedsPermission, NeedsInput, and for Idle reached since `last_seen_seq` (input to the fold).
- Turn count and the last user prompt text are accessible (used by auto-naming in T24).
scope: crates/core
out of scope: hook translation (T15).

### T06: Host session registry
status: todo
depends: T03
touches: core::host
goal: Host state built only by folding `_shepherd/session/*` frames, plus the commands for creating, renaming, and closing.
acceptance:
- `HostEvent::NewSession { id, at, spec }` → `[Append(session/created), SpawnPty { session, command, cwd, env, size }]`.
  In M1 the command is the user's `$SHELL`, supplied in `spec` by the runtime.
- `Rename` → `Append(session/renamed)`. `Close` → `KillPty` plus `Append(session/closed)`.
- PTY exit event → `Append(_shepherd/agent/exited)`.
- State only changes in `apply`. A test feeds the emitted Append frames into a fresh `Host` via apply and gets an
  equal state (replay equality).
- `sessions()` returns the list newest first, and closed sessions are excluded.
- Errors are reported as frames, not panics. For example, renaming an unknown session yields a `HostCommand::Reply(Err)`.
scope: crates/core

### T07: Wire protocol
status: done (0777b72)
depends: T03
touches: core::wire
goal: Client↔host messages as JSON-RPC, with encode and decode as pure functions.
acceptance:
- Requests: `_shepherd/hello` (version, machine), `_shepherd/session/new`, `/rename`, `/close`, `/input`
  (bytes as base64), `/resize`, `/subscribe { from: {session|host → seq} }`, and `_shepherd/hook/frame`
  (a FrameDraft from `shepherd hook`).
- Host→client notifications: `_shepherd/log/entry { envelope }` and `_shepherd/pty/output { session, bytes }`.
- Newline-delimited framing codec: `Decoder` takes arbitrary byte chunks and yields whole messages. Tests split
  messages at every byte boundary.
- Version mismatch in hello → typed error response.
scope: crates/core

### T08: `Pty` port
status: in-progress
depends: T00
touches: ports::pty, testkit, adapters::pty
goal: Spawn processes in PTYs, write input, resize them, stream output, and observe exit.
acceptance:
- Contract: spawn `printf hi` and observe the output `hi` then exit code 0. Writes reach the child (`cat` echoes them).
  Resize is observable (`stty size` reports it). Kill leads to an exit event. Env and cwd are honored.
- Fake passes the contract. In fake mode it uses scripted programs: a test registers a handler for a command name.
- portable-pty adapter passes the contract.
scope: core::ports, testkit, adapters
notes: The trait is synchronous, and output is pushed to a sink, so core needs no async dependency (portable-pty is blocking
anyway, and the adapter uses a reader thread per PTY). Put it in `crates/core/src/ports/pty.rs` and re-export it from `ports`:
```rust
pub struct SpawnSpec { pub command: String, pub args: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)>, pub size: PtySize }
pub struct PtySize { pub cols: u16, pub rows: u16 }
pub enum PtyEvent { Output { session: SessionId, bytes: Vec<u8> }, Exited { session: SessionId, code: Option<i32> } }
pub trait PtyEventSink: Send + Sync + 'static { fn send(&self, event: PtyEvent); }
pub trait Pty {
    fn spawn(&mut self, session: SessionId, spec: SpawnSpec) -> Result<(), PtyError>;
    fn write(&mut self, session: &SessionId, bytes: &[u8]) -> Result<(), PtyError>;
    fn resize(&mut self, session: &SessionId, size: PtySize) -> Result<(), PtyError>;
    fn kill(&mut self, session: &SessionId) -> Result<(), PtyError>;
}
// PtyError (thiserror): UnknownSession, AlreadyExists, SpawnFailed(String), Io(String)
```
Adapters are constructed with an `Arc<dyn PtyEventSink>`. The contract harness wraps the port plus a channel-backed sink
with `wait_for(predicate, timeout)` (std mpsc `recv_timeout`; no sleeps). Exactly one `Exited` per session, and it comes after all
its `Output`. Use `SessionId` from `shepherd_core::ids`. New deps allowed: `portable-pty` (adapters).

### T09: `shepherd-term` screen + TUI rendering
status: in-progress
depends: T02
touches: term (new crate), tui::screen
goal: Turn PTY bytes into a screen, answer terminal queries, snapshot for attach, and render into ratatui (decision 0007).
acceptance:
- New crate `crates/term` (`shepherd-term`), depending on `alacritty_terminal` with `default-features = false`.
  `Screen::new(cols, rows)`, `feed(&[u8])`, `resize(cols, rows)`, `take_replies() -> Vec<u8>` (query answers such as DA
  and cursor position), `modes() -> Modes { kitty_keyboard, mouse, bracketed_paste, focus, alt_screen }`, `cursor()`,
  read-only cell iteration, and `snapshot_ansi() -> Vec<u8>`.
- Snapshot round-trip: feeding `snapshot_ansi()` of screen A into a fresh screen B of the same size gives identical
  cells, cursor and modes. Tested on every T02 fixture segment.
- Chunking invariance: feeding a fixture in 1-byte, 7-byte, and whole chunks gives identical screens.
- The queries both agents send at startup (listed in the T02 report) get replies.
- The sync-update timeout is driven by an injected `Timeout` with no clock reads. A test shows output held during sync
  and released on the end marker or on an injected timeout.
- `shepherd-tui::render_screen(&Screen, Rect, &mut Buffer)` draws cells with fg, bg and modifiers, mapping 24-bit color
  to `Color::Rgb`, and wide chars take two cells. Snapshot tests render the Claude and Codex fixtures at their recorded sizes.
scope: crates/term (new), crates/tui, workspace Cargo.toml
notes: The T02 report's alacritty section is binding on API gotchas. The `check-arch.sh` rules for `shepherd-term` already
exist.

### T10: Link ports (transport)
status: todo
depends: T07
touches: ports::link, testkit, adapters::link
goal: Move wire messages between client and host, independent of transport.
acceptance:
- Contract: host `listen` and client `connect`. Messages arrive in both directions, in order. Disconnect is observable on
  both ends. Multiple clients can connect to one host.
- An in-memory fake pair passes the contract.
- The unix socket adapter (`~/.shepherd/host.sock`) passes the contract. A stale socket file is detected and replaced.
scope: core::ports, testkit, adapters

### T11: Host runtime
status: todo
depends: T04, T06, T08, T09, T10
touches: shepherd::host
goal: The `shepherd host` daemon loop. Events come in, `Host::handle` runs, and commands execute through the ports.
acceptance:
- On start, it folds every log from `EventLog` to rebuild state. Sessions whose PTY no longer exists become Exited
  (an exited frame is appended).
- Executes `Append` (stamping it with id, ts, and machine), `SpawnPty`, `WritePty`, `ResizePty`, `KillPty`, and `Reply`.
- Keeps a `shepherd_term::Screen` per PTY. PTY output is fed into it, and `take_replies()` is written back to the PTY, so
  agents get their terminal-query answers with no client attached (decision 0007).
- Attach: subscribing to a session's output first sends `snapshot_ansi()`, then live bytes. A scenario test shows a
  client attaching mid-session and seeing the current screen.
- Fans new envelopes and PTY output out to subscribed clients. A subscribe with `from` replays history first, then goes live
  with no gap and no duplicate.
- Scenario (all fakes): client sends session/new, then input, then sees the output and the log entries. The host
  restarts and the client sees the same session list.
- Replay equality: after the scenario, a fresh host built from the fake log has the same state as the live one.
scope: crates/shepherd

### T12: Client core + runtime
status: todo
depends: T07, T10
touches: core::client, shepherd::client
goal: The TUI's brain: connect, subscribe, keep view state, and turn user intents into wire requests.
acceptance:
- `Client::handle` keeps the merged session list (newest first) from log entries, the selected session, the focus
  mode (`Terminal` | `Command`), and the popup state.
- Next and previous session wrap around. When the selected session is closed, selection moves to its neighbor
  instead of jumping to the top.
- In Terminal focus, keystrokes become `/input` for the selected session. Resizes become `/resize`.
- The runtime starts `shepherd host` in the background if connecting to the socket fails, then retries with a bounded
  number of attempts. On final failure it shows a clear error.
- Scenario: two sessions, switching between them, input routed to the right one.
scope: crates/core, crates/shepherd

### T13: TUI layout, sidebar, and keys
status: todo
depends: T05, T09, T12
touches: tui::layout, tui::sidebar, tui::keys, tui::help
goal: The visible app: a left sidebar of sessions and the selected session's terminal on the right.
acceptance:
- Each sidebar row is 2 lines. Line 1 is a status glyph and the name. Line 2 is `machine · PR` (placeholders until
  M3/M4). The selected row is highlighted, and rows needing attention are marked.
- Prefix key (default `ctrl-b`, configurable later): `n` new, `j`/`k` or arrows next/prev, `r` rename (inline
  input), `x` close (confirm), `?` help, `q` detach (agents keep running). Everything else goes to the terminal.
  `ctrl-b ctrl-b` sends a literal `ctrl-b`.
- Snapshot tests: empty state (a hint for how to create a session), 3 sessions with mixed status, rename input,
  close confirmation, help popup, and a narrow terminal (40 cols) where the sidebar collapses to glyphs.
- `map_key` unit tests cover each binding and the passthrough.
- Input encoding follows the selected session's `Screen::modes()`: kitty keyboard, SGR mouse, bracketed paste, and focus
  events. Mouse and wheel input go to the agent when it has mouse mode on.
scope: crates/tui
notes: The status glyph shapes and colors are decided in this task and recorded in tui module docs.

---

## M2 Agents

### T14: `Config` port and agent profiles
status: todo
depends: T00
touches: ports::config, core::config, testkit, adapters::config
goal: One config file with global defaults and per-agent profiles.
acceptance:
- `~/.shepherd/config.toml` supports `[defaults] agent = "claude"`, a `[agents.<name>]` table with `kind`
  (`claude-code` | `codex` | `shell`), `command`, `args`, and `env`, plus `[keys] prefix`.
- Profiles merge as global defaults overlaid by the per-agent table. Merge rules are unit-tested.
- A missing file gives built-in defaults (`claude`, `codex`, `shell` profiles). An invalid file gives a typed error with
  the line and column.
- Contract plus fake plus file adapter. The config is reloaded on host start only (no hot reload).
scope: core, testkit, adapters

### T15: Claude Code hook translation
status: todo
depends: T03, T05
touches: core::agents::claude
goal: Pure translation of Claude Code hook payloads into ACP++ FrameDrafts.
acceptance:
- Verify the current Claude Code hook event list and payload shapes against the docs, then record fixtures in
  `crates/core/tests/fixtures/claude/`.
- Mapping, where each synthesized frame is marked: `UserPromptSubmit` → a `session/prompt` request plus
  `user_message_chunk`. `PreToolUse` → `tool_call` (in_progress, id = tool_use_id). `PostToolUse` →
  `tool_call_update` completed. Permission prompt (`PermissionRequest`, or `Notification` of the permission type) →
  `session/request_permission`. `Stop` → the `session/prompt` response with `end_turn`. Idle notification →
  `_shepherd/agent/idle`. `SessionStart` → `_shepherd/agent/started { agent_session_id }`. `SessionEnd` →
  `_shepherd/agent/exited`. `PreCompact` → the ACP compaction update if T01 pins it, otherwise `_claude-code/compact`.
  Any other hook → `_claude-code/<hook_name>` with the raw payload.
- Direction rule (from `PendingRequests`): synthesized `session/prompt` requests are `dir: to_agent` and their responses
  are `dir: from_agent`. Otherwise the response doesn't correlate and the session stays Working. Test this through the T05 fold.
- The prompt request `id` is derived deterministically from state carried between calls (pass in the session's
  translation state and get it back) so that request and response pair up.
- Folding the translated fixtures through T05 produces the expected Activity sequence.
- Interrupts: Claude Code does not fire `Stop` when the user interrupts with Esc. Find the signal that does exist
  (an idle notification, the next `UserPromptSubmit`, or something else in the current docs) and make sure a
  hook-fed session doesn't stay Working forever after an interrupt. Write a fixture-based test for it. If no hook signal
  exists, report it so the PM can decide on a non-hook fallback (e.g. PTY output going quiet).
- A permission notification arriving while Idle moves the fold into NeedsPermission (by T05's design). Only emit
  `session/request_permission` for real permission prompts, not for generic notifications.
scope: crates/core
out of scope: launching claude (T17).

### T16: `shepherd hook` subcommand
status: todo
depends: T07, T10, T11
touches: shepherd::hook, core::host (hook frames)
goal: Agents' hooks call `shepherd hook <agent>`, which delivers frames to the host.
acceptance:
- Reads the hook JSON from stdin and the session from `SHEPHERD_SESSION`, then sends `_shepherd/hook/frame` with the raw
  payload to the host over the socket. It exits 0 quickly (target < 50ms) and never blocks the agent. If the host
  is unreachable it exits 0 silently.
- The host translates the payload with the agent's translator (T15 or T19), keeping per-session translation state, and appends.
- Scenario: a fake agent process in a PTY invokes the hook path, and the session's Activity changes.
scope: crates/shepherd, crates/core

### T17: Launch Claude Code sessions
status: todo
depends: T14, T15, T16
touches: core::host (spawn spec), shepherd::host
goal: A new session with profile `claude` runs Claude Code with shepherd's hooks injected and shows live status.
acceptance:
- The host builds the spawn spec from the profile: hooks are injected without touching the user's settings files
  (`--settings` with a generated file under `~/.shepherd/run/<session>/`; verify the mechanism in the current
  Claude Code docs). `SHEPHERD_SESSION` and `SHEPHERD_SOCK` are set.
- The new-session flow takes a profile name, defaulting to `[defaults] agent`. The TUI new-session popup lists profiles.
- Scenario with a fake `claude` that emits recorded hook calls: the sidebar status follows Working → NeedsPermission →
  Working → Idle.
- A manual check against real `claude` is documented in the report (not a CI test).
scope: crates/core, crates/shepherd, crates/tui (profile picker)

### T18: Attention and notifications
status: todo
depends: T13, T17
touches: core::client (attention), tui::sidebar
goal: Never miss an agent that's waiting on you.
acceptance:
- Unselected sessions entering NeedsPermission, NeedsInput, or newly Idle are marked for attention. Selecting the
  session clears the mark (the client tracks `last_seen_seq`).
- A prefix-key binding `a` jumps to the oldest session needing attention.
- An optional terminal bell or OSC 9 notification on attention, controlled by config `[notify] bell = true`.
- Snapshot of the sidebar with attention markers.
scope: crates/core, crates/tui

### T19: Codex integration
status: todo
depends: T14, T16, T17
touches: core::agents::codex, core::host (spawn spec)
goal: Codex sessions with status, matching what Claude Code gets wherever Codex exposes the data.
acceptance:
- Verify Codex's current hook and notify surface. Record fixtures. Document the gaps compared with Claude Code in the report.
- Translator as in T15, with `_codex/*` for frames that have no equivalent.
- The spawn spec injects hooks without touching the user's `~/.codex` config, using a per-session config override.
- Scenario with a fake `codex`, as in T17.
scope: crates/core, crates/shepherd

### T19b: Resume after restart
status: todo
depends: T17, T19
touches: core::host, tui
goal: Exited agent sessions can be resumed where they left off.
acceptance:
- `_shepherd/agent/started.agent_session_id` is captured from the first hook.
- On an Exited session, the prefix key `R` respawns with the resume args for that profile kind (Claude Code's resume
  flag, Codex's resume subcommand; verify them in the docs) in the same cwd. The log continues in the same session.
- Sessions with no captured agent session ID show resume as unavailable instead of failing.
scope: crates/core, crates/shepherd, crates/tui

---

## M3 Workspaces & worktrees

### T20: `Worktrees` port
status: todo
depends: T00
touches: ports::worktrees, testkit, adapters::worktrees
goal: Managed git worktrees, expressed in domain terms.
acceptance:
- Contract: `default_branch(repo)`. `create(repo, name, base: OriginDefault) -> Worktree { path, branch }` fetches
  origin first, then branches from `origin/<default>`. `list_managed(repo)`. `status(path) -> { dirty, ahead,
  pushed, merged_into_default }`. `remove(path, force)`. Errors: `NotARepo`, `BranchExists`, `Dirty`, `FetchFailed`.
- Worktrees live under `~/.shepherd/worktrees/<repo-name>/<session-slug>`. The repo name comes from
  `git rev-parse --git-common-dir` (v1 lesson: this is stable across worktrees).
- If fetch fails while offline, it falls back to the local `<default>` and the result reports that.
- Fake plus git CLI adapter. Adapter contract tests use a tempdir bare "origin" and a clone.
scope: core::ports, testkit, adapters

### T21: Workspaces
status: todo
depends: T06, T13
touches: core::host (workspace), core::client, tui::header
goal: Choose a workspace (repo) once and have it remembered, visible, and easy to swap.
acceptance:
- `_shepherd/workspace/selected` is in the host log. The last selection is the default on the next start.
  `shepherd` run inside a repo with no selection picks that repo.
- A header or sidebar title shows the current workspace. The prefix key `w` opens a switcher listing the workspaces
  used before, with an input for a new path (validated through `Worktrees::default_branch`).
- Sessions record their workspace. The sidebar shows all sessions and marks those from other workspaces subtly.
  The decision on whether to filter instead is recorded in the task report.
scope: crates/core, crates/tui

### T22: New session creates a worktree
status: todo
depends: T17, T20, T21
touches: core::host (new session flow)
goal: You never think about worktrees, because a new session just has one.
acceptance:
- NewSession in a workspace runs CreateWorktree, then `_shepherd/worktree/created`, then post-create hooks (T23),
  then SpawnPty with cwd set to the worktree.
- The session is visible immediately as `Starting` while the worktree is being created.
- Failure (fetch, branch exists, not a repo) → `_shepherd/session/failed { reason }`. The sidebar shows the error,
  and no PTY is spawned.
- The branch name is derived from the session slug, with uniqueness suffixes handled in core.
- Scenario with fake Worktrees and fake Pty, covering the happy path and the fetch-failed path.
scope: crates/core, crates/shepherd

### T23: Worktree hooks
status: todo
depends: T22
touches: ports::commands, core::host, adapters::commands
goal: Run user-defined setup in new worktrees (copy `.envrc`, install deps, ...).
acceptance:
- Config: `[workspaces."<repo path or name>".hooks] post_create = ["cp ../.envrc .", ...]`, plus a `pre_remove` list.
- Each hook runs in the worktree with `SHEPHERD_*` env set. Its output tail and exit code go to `_shepherd/worktree/hook`.
  A failing post_create hook marks the session with a warning but still spawns the agent.
- `Commands` port with contract, fake, and tokio::process adapter.
scope: core, testkit, adapters

### T24: Auto-naming
status: todo
depends: T05, T22
touches: core::host (naming)
goal: Sessions get useful names without effort. Manual rename always wins.
acceptance:
- New sessions get a placeholder name (`<workspace>-<n>`).
- After the first user prompt in the fold, if the session has never been manually renamed, append
  `_shepherd/session/renamed { auto: true }` with a deterministic slug built from the prompt's first meaningful words
  (stopwords dropped, max 4 words or 30 chars).
- A manual rename sets `auto: false`, and auto-naming never overrides it afterwards.
- Only the display name changes. The branch name is not renamed.
scope: crates/core
notes: LLM-generated names are a possible follow-up, behind a port, not in this task.

### T25: Close and cleanup
status: todo
depends: T22
touches: core::host (close), core::client, tui::cleanup
goal: Worktrees go away when they should. No more pile of abandoned worktrees.
acceptance:
- Closing a session checks `Worktrees::status`. If the worktree is clean and merged or pushed, it is removed
  automatically. If it is dirty or unpushed, a confirmation lists what would be lost (`remove anyway` / `keep worktree`).
  The outcome is recorded in `session/closed.worktree_removed`.
- `pre_remove` hooks run before removal.
- A cleanup view (prefix `C`) lists managed worktrees with no open session and stale sessions (exited, and merged
  for more than N days) with status columns, and supports bulk removal of the safe ones.
- Removal never touches worktrees outside `~/.shepherd/worktrees`. This is tested with a path that tries to escape.
scope: crates/core, crates/tui

### T26: Search and open
status: todo
depends: T13, T21
touches: core::client (finder), tui::finder
goal: Find and jump to any session or worktree quickly.
acceptance:
- Prefix `/` opens a fuzzy finder over sessions (name, branch, workspace, machine) and managed worktrees with no
  session. Enter selects a session. On an orphan worktree, Enter offers to start a new session in that worktree.
- Matching and ranking are pure core functions with unit tests (prefix > word-boundary > subsequence).
- Snapshots: empty query, a filtered list, and no results.
scope: crates/core, crates/tui

### T27: PR status in the sidebar
status: todo
depends: T13, T22
touches: ports::forge, core::host, adapters::forge, tui::sidebar
goal: Sidebar line 2 shows the session's PR.
acceptance:
- `Forge::pr_for_branch(repo, branch) -> Option<Pr { number, state, checks }>`, with contract, fake, and `gh` adapter.
  A missing `gh` or no auth → `Unavailable`, shown as nothing (not an error).
- The host polls open sessions on a timer driven by `Tick` events, with backoff and at most one call per session
  every 60s. Changes are appended as `_shepherd/session/pr`.
- The sidebar shows `#123 ✓/✗/…`. Snapshot test.
scope: core, testkit, adapters, tui

---

## M4 Multi-machine

### T28: Spike: iroh peering
status: todo
depends: T10
touches: docs only
goal: Confirm iroh fits the `Link` port and decide the pairing and discovery design.
acceptance:
- A prototype in a scratch crate (not merged): two hosts exchange wire messages over iroh across NAT through relays.
- Decide on: node identity storage, pairing (ticket via `shepherd pair` / `shepherd join <ticket>`), discovery of known
  peers on start, and trust (an allowlist of node IDs).
- Report the latency of keystroke round-trips through a relay compared with a direct connection.
out of scope: production code.

### T29: Machine identity and names
status: todo
depends: T28, T11
touches: core::host, adapters::peers, tui::sidebar
goal: Every host has a stable identity and a human name.
acceptance:
- The host generates and persists a key on first run. `machine` in envelopes is derived from it.
- The name comes from config `[machine] name`, defaulting to the hostname. A rename appends `_shepherd/machine/renamed`.
- Sidebar line 2 shows the machine name.
scope: core, adapters, tui

### T30: Pairing and trusted peers
status: todo
depends: T29
touches: ports::peers, core::host, adapters::peers, shepherd (cli)
goal: Connect your machines once, and they find each other on every start.
acceptance:
- `shepherd pair` prints a ticket. `shepherd join <ticket>` on another machine adds each machine to the other's
  trusted peers, as `_shepherd/peer/trusted` in each host log.
- Connections from untrusted node IDs are rejected. This is tested in the contract with the fake.
- `Peers` port with contract, fake, and iroh adapter.
scope: core, testkit, adapters, shepherd

### T31: Remote link and merged sidebar
status: todo
depends: T30, T12
touches: adapters::link (iroh), core::client, shepherd::client
goal: The client drives every trusted machine's host as if it were local.
acceptance:
- The iroh `Link` adapter passes the T10 contract.
- The client connects to the local host and to every reachable trusted peer. The session list is merged and
  sorted newest first. Each session is routed to its own host for input, resize, rename, and close.
- An unreachable peer shows as a dimmed machine group with its last-known sessions. Reconnects resume
  subscriptions from the last seen `seq` with no gaps or duplicates.
- Scenario with two fake hosts over the fake link.
scope: adapters, core, shepherd

### T32: Create sessions on any machine
status: todo
depends: T31, T22
touches: core::client, tui::new_session
goal: Choose where a new session runs.
acceptance:
- The new-session popup has a machine selector, defaulting to the local machine or the machine of the selected session.
- Workspaces are per machine. The workspace switcher shows the selected machine's workspaces.
- Scenario: create a session on a remote fake host, then type into it and see the output.
scope: crates/core, crates/tui
