# Agent sessions: Claude Code and Codex
milestone: M2
goal: Claude Code and Codex run in sessions with live status, attention and resume after restart

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T14 | `Config` port and agent profiles | done | d8173f4 | 1 | – |
| T15 | Claude Code hook translation | done | 7eb7375 | 2 | Stop blocked by another hook |
| T16 | `shepherd hook` subcommand | done | 01acabc | 2 | reply-timeout bound untested (two mutations survived); refused/disconnect paths untested |
| T17 | Launch Claude Code sessions | done | b61bd59 | 1 | 4 untested defensive lines in run_files (→ F22) |
| T17b | Profile picker | done | a18bc42 | 1 | Esc untested; fixed column widths; height overflow (→ F24) |
| T18 | Attention and notifications | done | b17c1b4 | 2 | C1 strip untested; session name could form a ConEmu OSC 9 subcommand |
| T18b | Quiet-output idle fallback | done | 76b5bae | 2 | append-failure hot loop; next_deadline wiring untested; spec defect: output-as-answer would clear untouched dialogs (amended) |
| T19 | Codex integration | done | 27ddd29 | 2 | translator matching/resets/kind replacement untested; seed reuse across replaced translators |
| T19b | Resume after restart | done | a87962e | 2 | resume ignored a deleted cwd (portable-pty falls back to $HOME); exited guard untested |
| F12 | Continuation prompts don't count as user turns | done | 5d09c6d |  |  |
| F19 | Drop hook translator state when a session closes | done | b813457 | PM spot check | – |
| F20 | Hook disconnect test: fake vs real link sink lifetime | done | 2c5b3d0 |  |  |
| F22 | Agent session polish from T17 | done | 83099a3 | 2 | hook-sourced agent/exited could delete a live session's run files (untested guard) |
| F26 | Attention follows window focus | done | 9be58f9 | 1 | – |
| F31 | Only real answers count as answering a permission dialog | done | 2839492 | 2 | runtime kind forwarding untested (focus test passed via ESC rule) |

## Specs

### T14: `Config` port and agent profiles
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
depends: T07, T10, T11
touches: shepherd::hook, core::host (hook frames)
goal: Agents' hooks call `shepherd hook <agent>`, which delivers frames to the host.
acceptance:
- Reads the hook JSON from stdin and the session from `SHEPHERD_SESSION`, then sends `_shepherd/hook/frame` with the raw
  payload to the host over the socket. It exits 0 quickly (target < 50ms) and never blocks the agent. If the host
  is unreachable it exits 0 silently.
- The host translates the payload with the agent's translator (T15 or T19), keeping per-session translation state, and appends.
  `ClaudeTranslation::new(session, id_seed)` must get a seed unique within that session's log (e.g. the log's current last
  seq plus the host start's entry id), or restore persisted state. A test shows ids don't collide across a host restart.
- Scenario: a fake agent process in a PTY invokes the hook path, and the session's Activity changes.
scope: crates/shepherd, crates/core

### T17: Launch Claude Code sessions
depends: T14, T15, T16
touches: core::host (spawn spec), shepherd::host
goal: A new session with profile `claude` runs Claude Code with shepherd's hooks injected and shows live status.
acceptance:
- The host builds the spawn spec from the profile: hooks are injected without touching the user's settings files
  (`--settings` with a generated file under `~/.shepherd/run/<session>/`; verify the mechanism in the current
  Claude Code docs). `SHEPHERD_SESSION` and `SHEPHERD_SOCK` are set.
- The new-session request takes an optional profile name, defaulting to `[defaults] agent`. (The clickable profile picker is
  T17b, after T34; this task doesn't touch tui.)
- The hook command written into the generated settings is the absolute path of the running `shepherd` executable plus
  `hook claude-code`, so it works without `shepherd` on PATH. Files under `~/.shepherd/run/` are created with the private_fs
  helpers (dirs 0700, files 0600) and removed when the session closes.
- Scenario with a fake `claude` that emits recorded hook calls: the sidebar status follows Working → NeedsPermission →
  Working → Idle.
- Builders never run the real `claude` (it writes to the user's ~/.claude state). The PM does the manual check after merge.
scope: crates/core, crates/shepherd

### T17b: Profile picker
depends: T17, T34
touches: tui::popup, core::client
goal: Clicking `+ New session` lets you pick which agent to run (decision 0014).
acceptance:
- With more than one profile configured, clicking `+ New session` opens a popup listing profiles as clickable rows (the default
  first and marked); clicking one creates the session with that profile. With exactly one profile it creates it directly.
- `ctrl-b n` keeps creating the default profile without a popup.
- Clicking outside or `[Cancel]` dismisses. Hit tests click rendered cells (T33/T34 pattern). Snapshot of the picker.
scope: crates/core (client, wire, host hello), crates/tui, crates/shepherd (host hello wiring)
notes: Profiles live in the host's config, so the host tells the client: the hello response gains `profiles: [{ name, agent }]`
and `default_profile` (additive; old clients ignore it). The client keeps them in `ClientState`. The new-session request
carries the chosen profile name (`wire::SessionNew.profile` already exists). The client never reads config itself (in M4 the
host may be another machine).

### T18: Attention and notifications
depends: T13, T17
touches: core::client (attention), tui::sidebar
goal: Never miss an agent that's waiting on you.
acceptance:
- Unselected sessions entering NeedsPermission, NeedsInput, or newly Idle are marked for attention. Selecting the
  session clears the mark (the client tracks `last_seen_seq`).
- Attention rows are visually marked so they can be clicked (decision 0014). No new hotkey.
- An optional terminal bell or OSC 9 notification on attention, controlled by config `[notify] bell = true`.
- Snapshot of the sidebar with attention markers.
scope: crates/core (client, wire hello), crates/tui, crates/shepherd (client runtime bell output, host hello wiring)
notes: The client never reads config (T17b), so the host sends `notify: { bell: bool }` in the hello response (additive,
default false), like `profiles`. Attention is pure client state in core: computed from log entries folded with
`core::session` activity, the selected session, and `last_seen_seq` per session. The bell/OSC 9 is a `ClientCommand` the
runtime writes to the real terminal (behind the existing terminal seam, so tests assert the command, not bytes on a tty).
Attention for a session that becomes selected is cleared immediately; a session that needs attention while selected and
focused is not marked.

### T18b: Quiet-output idle fallback
depends: T11, T15
touches: core::session (or core::agents), core::host, shepherd::host
goal: A session interrupted with Esc (no `Stop` hook fires) stops showing Working within seconds, not minutes.
acceptance:
- Pure policy in core. When a session's Activity is Working and its PTY has produced no output for a quiet period (config
  `[agents.<name>] idle_after_quiet_ms`, default about 3000; agent TUIs animate a spinner while working, so silence means
  waiting), the host appends `_shepherd/agent/idle { inferred: "quiet" }`. Time comes from runtime events: PTY output
  carries `at`, plus a `Tick { at }` deadline the runtime schedules. No clocks in core.
- Output after the inferred idle doesn't flip back to Working by itself. Only real turn signals (a prompt or a tool call) do,
  per T05's rules.
- NeedsPermission: an untouched dialog is legitimately quiet, so quiet alone never clears it. But choosing "No" in Claude's
  permission dialog fires **no hook** (`PermissionDenied` is auto-mode only). So: in NeedsPermission, if client input is written
  to that session's PTY *after* the permission request (someone answered), then output follows, then a quiet period, append
  `_shepherd/agent/idle { inferred: "quiet-after-permission" }`. (Amended after T18b review: output alone isn't evidence —
  the dialog's own first draw and resize redraws arrive after the request too.)
- The same fallback covers the other no-signal cases from the T15 review: a prompt blocked by another `UserPromptSubmit` hook,
  and Esc during a hook.
- Whatever sets NeedsInput (or idle-by-inference) must also set the session's `waiting_since` seq, or T18 attention never
  marks it (T18 review).
- (binding) Applies to agent profiles only (claude-code, codex), never shell. The inferred idle sets `waiting_since` like a
  real idle so T18 attention marks it. The quiet deadline is serviced from the host runtime's existing deadline/tick loop.
- Tests: core table tests (quiet then idle, output resets the timer, already Idle means no duplicate, an untouched NeedsPermission
  stays put, NeedsPermission plus output plus quiet goes idle) and a scenario with the fake PTY.
scope: crates/core, crates/shepherd

### T19: Codex integration
depends: T14, T16, T17
touches: core::agents::codex, core::host (spawn spec)
goal: Codex sessions with status, matching what Claude Code gets wherever Codex exposes the data.
acceptance:
- Verify Codex's current hook and notify surface. Record fixtures. Document the gaps compared with Claude Code in the report.
- Translator as in T15, with `_codex/*` for frames that have no equivalent.
- The spawn spec injects hooks without touching the user's `~/.codex` config, using a per-session config override.
- Scenario with a fake `codex`, as in T17.
scope: crates/core, crates/shepherd
notes: Builders never run the real `codex` binary (a T02 builder upgraded the user's Codex): verify the surface from the
official docs and source (github.com/openai/codex: hooks / `notify` / `-c key=value` overrides / `CODEX_HOME`), cite what
you found, and build fixtures from documented payload shapes. Follow T17's structure (`core::host::launch`, run files under
`~/.shepherd/run/<session>/`, `shepherd hook codex`). If Codex offers no way to add hooks without touching `~/.codex`, stop and
report options rather than editing user config. The PM does the real-codex check after merge.

### T19b: Resume after restart
depends: T17, T19
touches: core::host, tui
goal: Exited agent sessions can be resumed where they left off.
acceptance:
- `_shepherd/agent/started.agent_session_id` is captured from the first hook.
- On an Exited session, a `Resume` item in the row's context menu (decision 0014) respawns with the resume args for that profile kind (Claude Code's resume
  flag, Codex's resume subcommand; verify them in the docs) in the same cwd. The log continues in the same session.
- Sessions with no captured agent session ID show resume as unavailable instead of failing.
scope: crates/core, crates/shepherd, crates/tui

---

### F12: Continuation prompts don't count as user turns
depends: T15
touches: core::session
goal: `SessionState::turns()` counts prompts the user actually sent.
acceptance:
- Prompts marked `_meta.shepherd.continuation: true` (T15's continuations of a blocked Stop) open a turn for activity purposes,
  but don't increment `turns()` or change `last_prompt()`. There's a table test.
scope: crates/core

### F19: Drop hook translator state when a session closes
depends: T16
touches: core::agents (Translators), shepherd::host
goal: `Translators` keeps one translator per session for the life of the host run, even after the session closes.
acceptance:
- Closing a session (or its exit followed by close) removes its translator. A host test counts live translators after
  open/close cycles (counted, not measured).
scope: crates/core, crates/shepherd

### F20: Hook disconnect test: fake vs real link sink lifetime
depends: T16
goal: The T16 review thought the fake drops a conn's sink earlier than the real link. The builder showed both drop it right
after `Disconnected`, so they already agree. Outcome: no fake/adapter change; the misleading comment in
`a_host_that_drops_the_connection_before_answering_is_a_disconnect` now explains the sink copy (it keeps the channel open so
only the `Disconnected` event can end the test).

### F22: Agent session polish from T17
depends: T17
touches: core::host (new session spec), shepherd::host (run files)
goal: Small gaps left by T17.
acceptance:
- Placeholder names follow the profile (`claude-<n>`, not `shell-<n>`), and `session/created.agent` records the agent kind.
- `wire::SessionNew.cwd` is honored (validated: must be an existing directory; otherwise InvalidParams).
- Run directories are removed when the PTY exits as well as on close, and stale `<root>/run/*` dirs with no live session
  are swept at host startup (never following symlinks; never outside `<root>/run`).
- Close the T17 review's untested lines in `crates/shepherd/src/host/run_files.rs`: writing settings over a longer stale file
  (truncate), the `BadName` guard (`"../x"`, `"a/b"`), closing a session with no run dir logs nothing (NotFound arm), and
  `pending_run_files` is cleared when core refuses a session.
scope: crates/core, crates/shepherd, crates/testkit

### F26: Attention follows window focus
depends: T18
touches: core::client (attention), tui::keys (focus events)
goal: T18 treats "selected" as "seen". If the shepherd window is in the background, the selected session going idle
should still alert and be marked until the window regains focus.
acceptance:
- The client tracks terminal window focus from focus-in/out events (crossterm `FocusGained/Lost`, already enabled) as an
  intent; while unfocused, the selected session is treated like an unselected one for attention and alerts; on focus-in the
  selected session's mark clears. Core table tests; the existing focus-forwarding keys test keeps passing (the event is both
  forwarded to a session that asked for focus events and reported to core).
scope: crates/core (client), crates/tui

### F31: Only real answers count as answering a permission dialog
depends: T18b
touches: core::wire (input), core::host::quiet, tui::keys
goal: T18b counts any client input after a permission request as an answer. Focus reports (`CSI I`/`CSI O`, forwarded when the
app enabled DECSET 1004), mouse reports and in-dialog navigation keys aren't answers; a 3 s pause after them shows an open dialog
as idle.
acceptance:
- The client tags input it sends (`/input` gains an additive `kind`: `keys` | `paste` | `focus` | `mouse`). Only `keys` and
  `paste` that contain Enter (or a printable key, decide and document) count as an answer for the quiet watch. Core table tests.
- Screen query replies never count (already true; add the scenario check: the fake Claude prints a terminal query on resize and
  the dialog stays put).
scope: crates/core, crates/tui, crates/shepherd
