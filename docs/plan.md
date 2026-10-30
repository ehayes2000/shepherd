# Plan: initial feature set

The source of truth for work in progress. Only the PM edits statuses. Task format is described in the
`write-task` skill. Background reading: `manifesto.md`, `docs/architecture.md`, `docs/acpp.md`.

## In flight

Updated by the PM on every transition (see `docs/pm.md`). On takeover, check it against git first.

| Task | Stage | Branch @ commit | Round | Open items / PM decisions |
|---|---|---|---|---|
| T28 spike iroh peering | building | task/t28-iroh-spike @ 72ebea3 | 0 | docs only |
| F36 display-safe gaps (minus ZWJ) | building | task/f36-display-gaps @ 72ebea3 | 0 | ZWJ item still awaits user |
| F38+F40 adapter test hygiene | building | task/f38-f40-test-hygiene @ 72ebea3 | 0 | – |
| F41 T27/T25b test gaps | building | task/f41-test-gaps @ 72ebea3 | 0 | – |
| F42 new session wrong size | building | task/f42-spawn-size @ 95d0821 | 0 | user bug; diagnosis in spec |
| F37 worktree lock/shutdown leftovers | fixing (CHANGES) | task/f37-worktree-leftovers @ 15e96c9 | 1 | Blocking: extra refspec locks a 2nd ref (use --refmap=); foreign-lock gap not accepted → fetch into private refs/shepherd/fetch/<name>, then local update-ref of origin/<default>. Minors: grace test, one group-wait failure under load |

Wave of 5 (user asked for more parallelism). Known small overlaps, merge in this order to keep rebases cheap: F11, F18, T33,
F14, T16 (F18/T33 both touch `core::client` and `shepherd/src/client.rs`; F18/F14/T16 all touch `crates/shepherd/src/host/`).
Next: T34 after T33; T17 after T16; T18b after F14 and T16.

## Completed task log

| Task | Review rounds | Notable findings |
|---|---|---|
| T03 | 2 | public sealing for adapters |
| T04 | 2 | silent data loss on recovery, durability claim, two hosts |
| T05 | 2 | phantom tool |
| T06 | 3 | replay order, string timestamps |
| T07 | 1 | notifications for input and resize |
| T08 | 2 | lost output under load, kill didn't kill |
| T09 | 2 | 1-column crash or hang |
| T10 | 2 | world-connectable socket |
| T14 | 1 | – |
| T15 | 2 | Stop blocked by another hook |
| T20 | 3 | data loss under user git config, ssh prompts |
| F06 | 3 | quadratic stall |
| F09 | 1 | – |
| F10 | 2 | symlinked log truncated an outside file |
| T13 | 2 | ctrl-symbol keys typed digits, Shift+Enter submitted, paste end-marker, 7 surviving mutations |
| T11 | 4 | 8 MB attach limit, stalled clients never dropped, quadratic re-reads, deadline starvation |
| T12 | 3 | weak tests (mutation), handshake crash loop, daemon inherits fds and signals, flaky test helper |
| F11 | 2 | doc claimed WrongType for every special file; sockets and dirs got Io |
| F18 | 2 | e2e test could leak a detached host on failure; unwritten no-stderr-before-release rule |
| T33 | 2 | stale press and sidebar clamp untested (mutation survivors) |
| T16 | 2 | reply-timeout bound untested (two mutations survived); refused/disconnect paths untested |
| T34 | 2 | same-button release rule untested; long-name truncation untested |
| F21 | 1 | – |
| T17 | 1 | 4 untested defensive lines in run_files (→ F22) |
| F19 | PM spot check | – |
| F07 | 2 | kitty_top refresh in expire_sync untested |
| T17b | 1 | Esc untested; fixed column widths; height overflow (→ F24) |
| T25b | 2 | orphan hook branch taken from the client; disconnect shown as kept |
| T27 | 2 | conflicted with F33; fixture leaked env to disk; CLICOLOR_FORCE broke JSON; real_worktrees could reach real gh |
| F33 | 2 | hooks started during shutdown never cancelled; vacuous cancel test; untested tail cap; macOS EPERM group wait |
| T26 | 5 | shared worktree ownership; hook-gap and path-alias bypasses; unresolved root spelling; untested host wiring; fixture briefly set core.bare on the real repo |
| T36b | 3 | bad config after adoption killed sessions; refuse() could killpg/close what isn't ours; several required steps untested |
| T37 | 2 | inherited GIT_ASKPASS could hang uncapped creates; 2-reader FIFO gate hang; restack dropped edits |
| T36a | 2 | stale restart dialog survived reconnect; terminate didn't validate socket path/dir/peer; leaking test stand-in |
| F35 | 1 | idempotence claim false (unbounded clusters), clippy ban gaps (title_top, List, &str Widget) → F36 |
| F32 | 2 | user core.filesRefLockTimeout let cleanup delete another process's lock (pinned =0) |
| T25 | 2 | Remove-anyway could force-delete unconfirmed new changes; escape test passed via an error |
| T24 | 4 | control chars, boundaries untested; NFD split; bidi/format chars and unbounded graphemes in names (decision 0017) |
| F28 | 2 | .git-named bare repo gave a root the port rejects |
| F34 | 1 | mixed-version: new client's `answer` not counted by an F31 host (accepted) |
| F31 | 2 | runtime kind forwarding untested (focus test passed via ESC rule) |
| T23 | 2 | timeout contract flaky (?E stat), scenario hold race; setsid/stdin cases weak |
| F25 | 2 (+1 stopped) | cleanup could delete a branch another process made during the fetch |
| T18b | 2 | append-failure hot loop; next_deadline wiring untested; spec defect: output-as-answer would clear untouched dialogs (amended) |
| F14 | 3 (+1 hung) | post-kill drop, shutdown ack, excess-ack banking untested; first reviewer hung 4 h |
| F30 | 2 | width-0 cut returned 1 column; added helper property table |
| F29 | 1 | cut/cut_left count chars not columns (→ F30) |
| T22 | 2 | worktree spawn cwd unchecked → cwd check made structural in SpawnPty; run-file cleanup untested |
| F24 | 2 | picker panicked in 0/1-row windows → every-popup tiny-window test |
| F26 | 1 | – |
| F15 | 2 | hold-deadline restart guard untested; notice table incomplete. Unknown-glyph snapshot deferred until a frame produces Unknown |
| F27 | 1 | – (2 minors → F28) |
| T21 | 2 | relative-path refusal passed for the wrong reason; refused-hello cwd and is_dir untested |
| T19b | 2 | resume ignored a deleted cwd (portable-pty falls back to $HOME); exited guard untested |
| F08 | 3 | load flakes (shared short timeout), stale ref lock, untested fallback, TOCTOU dir delete, T20 test flake |
| T19 | 2 | translator matching/resets/kind replacement untested; seed reuse across replaced translators |
| T18 | 2 | C1 strip untested; session name could form a ConEmu OSC 9 subcommand |
| F22 | 2 | hook-sourced agent/exited could delete a live session's run files (untested guard) |
| F23 | 2 | sync-timeout path unguarded; pop count, guard-flag reset untested; missing_trait_methods lint added |

## Milestones

| milestone | outcome | tasks |
|---|---|---|
| **M0 Foundation** | Workspace scaffolded, gate enforced, two open questions answered | T00–T02 |
| **M1 Log + host/client skeleton** | ACP++ log, host daemon, and a TUI that creates a session running a shell and swaps between sessions | T03–T13 |
| **M1.5 Point and click** | Everything in M1 can be done with the mouse: click rows, buttons, menus and dialog buttons (decision 0014) | T33–T34 |
| **M2 Agents** | Claude Code and Codex run in sessions with live status in the sidebar, and resume after a host restart | T14–T19 |
| **M3 Workspaces & worktrees** | Every new session gets a managed worktree from the latest origin default branch. Includes naming, cleanup, search, and PR display | T20–T27 |
| **M4 Multi-machine** | Named machines, peers over iroh, and a single sidebar that drives sessions on any machine | T28–T32 |

Critical path: T00 → T03 → T04 → T07 → T10 → T11 → T12 → T13, then M2. Most of the rest can run in parallel once its
dependencies are done.

## Ports (planned)

| port | side | first task | real adapter |
|---|---|---|---|
| `EventLog` | host | T04 | JSONL files in `~/.shepherd` |
| `Pty` | host | T08 | portable-pty (`PtyHandoff` for hot-swap, T36b) |
| `Link` (`UnixListenLink` / `UnixConnectLink`) | both | T10 | unix socket (M1), iroh (M4) |
| `Config` | both | T14 | `~/.shepherd/config.toml` |
| `HostLauncher` | client | T12 | detached `shepherd host` process (double fork + setsid); `terminate` via socket peer creds (T36a) |
| `Worktrees` | host | T20 | git CLI (`repo_root` added in F28) |
| `Commands` (run hook scripts) | host | T23 | `/bin/sh -c` via std::process |
| `Forge` | host | T27 | `gh` CLI |
| `Peers` | host | T29 | iroh |

## Follow-ups

### F01: Event-driven Pty pump
status: todo
depends: T08
touches: adapters::pty
goal: Remove the 20ms idle tick. Each idle session currently wakes 50 times a second.
acceptance:
- The pump waits on the master fd plus a kill self-pipe, and detects exits by SIGCHLD or pidfd/kqueue, so there are no timed wakeups.
- The full `pty_contract!` passes, including under parallel load (report the pass rate). An idle session uses about zero CPU.
scope: crates/adapters
notes: Low priority. Do it once there are real sessions to measure.

### F02: Contract case list generated by the macro
status: todo
depends: T08
touches: testkit::contracts
goal: A new contract case can't be forgotten in the macro list.
acceptance:
- Cases are declared once (e.g. inside the macro, or with a `cases!` helper) and the per-case `#[test]`s are generated from that.
- Applies to every existing contract.
scope: crates/testkit

### F03: Typed frame builders in acpp
status: done (a912f30)
depends: T06
touches: core::acpp
goal: Producers (host, hook translators) build frames from typed values instead of hand-written JSON.
acceptance:
- `ShepherdFrame::to_frame()` (and builders for the synthesized ACP frames T15 needs) produce a `Frame` that parses back
  to the same typed value. There's a round-trip property test.
- `core::host` uses them instead of private JSON construction.
scope: crates/core

### F04: Replace hand-rolled base64 with the base64 crate
status: done (214833e)
depends: T06
touches: core::wire, core::acpp::envelope (docs and tests only)
goal: Remove reinvented code (decision 0008). The Timestamp half was folded into T06's review fixes.
acceptance:
- `core::wire` base64 uses the `base64` crate (standard alphabet, padded, strict decoding with canonical trailing bits).
  The hand-rolled module is deleted, and the existing behavior tests still pass unchanged (they're the spec).
- `acpp::Timestamp` docs and error messages say what's actually accepted: "an ISO 8601 instant as jiff parses it; writers
  emit RFC 3339 UTC with ms". The accept test lists representative extra forms (basic format, reduced precision,
  bracketed annotations), and a test pins that fractions longer than 9 digits are rejected.
scope: crates/core, workspace Cargo.toml

### F05: Validated types can't be deserialized around their validation
status: todo
depends: T04
touches: core::ids, every SessionId::new call site
goal: An invalid session id can't exist, so the log can't reject one at runtime. More broadly, no validated domain type can be
built by deserialization without its validation (the howtocodeit hexagonal guide's "serde on validated models" anti-pattern).
acceptance:
- `SessionId` construction validates `[a-z0-9_-]{1,64}` (`TryFrom<String>`/`parse`, a typed error, and serde deserialize
  validates too). `LogId::validate` relies on it or is removed.
- Ids coming from outside (hook env, wire requests, logs) fail cleanly at the boundary with a typed error.
- Audit every core and adapter type that validates in its constructor (`SessionId`, `MachineId`, `WorktreeName`, `KeyPrefix`,
  `Timestamp`, `RepoRef`, ...). Each either deserializes through its validating constructor (`#[serde(try_from = ...)]`) or
  doesn't derive `Deserialize`. There's a test per type that invalid JSON is rejected.
scope: crates/core, crates/testkit, crates/adapters

### F06: Snapshots preserve partial parser input
status: done (f7a6d79)
depends: T09
touches: term
goal: A snapshot taken in the middle of an escape sequence or UTF-8 char doesn't corrupt the reattached client's screen.
acceptance:
- `snapshot_ansi` carries any incomplete trailing escape sequence or UTF-8 bytes, so continuing both screens with the rest
  of the sequence gives identical screens. Repro: feed `x\x1b[?20`, snapshot, then feed `26hab` to both; they must match.
  Claude doesn't use sync output, so without this an attach during heavy output can show garbage text.
- The fuzz test gains a continuation phase: after the round trip, feed more tokens to both screens and expire any held
  sync on both, then compare again. Documented-gap tokens are excluded from that phase (scroll region, tab stops,
  charset, REP, kitty pops, `?1049l`).
- Fix the incorrect "has no effect there" comment about a wrap flag left set away from the last column, and add that
  case, REP's last character, and the saved cursor's pending wrap to the "Not carried over" docs.
- `has_unrebuildable_row` skips only the affected rows, not the whole comparison.
scope: crates/term

### F07: Carry palette colours and the empty-stack kitty flags in snapshots
status: done (6905cb5)
depends: F06
touches: term
goal: A reattached client draws colours set by programs, and answers kitty queries, the same as the host.
acceptance:
- Colours set with OSC 4/10/11 are carried by `snapshot_ansi`, so the copy renders and reports them identically. Remove them from
  the fuzz `GAPS` list and the "Not carried over" docs.
- Kitty flags set with `CSI = flags;1u` on an empty stack round-trip, so `CSI ? u` answers the same on both screens.
- The `unfinished.rs` cap proptest tries every next byte in `0x00..=0xFF` and adds `>`, `X`, `^`, `_`, `\x07`, `\x1a`, `\x9b`,
  `\xc2` and `\xe6` to its alphabet, so a vte upgrade that changes a transition is caught.
scope: crates/term

### F08: Clean up after a timed-out worktree create
status: done (da443a6)
depends: T20
touches: adapters::worktrees, ports::worktrees (docs)
goal: A `create` that times out (e.g. a slow post-checkout hook) leaves no half-made branch or worktree behind.
acceptance:
- On `Timeout` during `worktree add`, the adapter removes any registered worktree and the new branch, and a retry with
  the same name succeeds. There's an adapter test with a slow `post-checkout` hook, asserting nothing is left and the retry works.
- If cleanup itself fails, the error says so, and `Worktrees::create` docs describe it.
scope: crates/adapters, crates/core (docs only)

### F09: Event log files and dirs are private
status: done (a15dc56)
depends: T04
touches: adapters::event_log
goal: Logs that hold prompts and tool output can't be read by other users (decision 0010).
acceptance:
- `JsonlEventLog::open` creates its root and `sessions/` as 0700, and log files and the lock file as 0600, regardless of umask.
  It returns a typed error if an existing root is accessible to group or others (never chmods it). Mirror
  `UnixLinkError::InsecureDir`.
- Adapter tests under a permissive umask. The test tempdir helpers chmod their dirs to 0700.
scope: crates/adapters

### F10: One shared private-filesystem helper for adapters
status: done (3278967)
depends: F09, T10
touches: adapters::private_fs (new), adapters::event_log, adapters::link
goal: Every adapter under `~/.shepherd` follows the same race-free private-permission rules (decision 0010). T11 uses it to
create `~/.shepherd`.
acceptance:
- A new `crates/adapters/src/private_fs.rs` provides `ensure_private_dir(path)` and `create_or_open_private_file(path)` (or
  similar), used by both `event_log` and `link`. The duplicated helpers are deleted.
- No time-of-check/time-of-use gap in directory creation. Missing parents are created recursively, and the final part with a
  plain `create_dir`. `AlreadyExists` means re-check it. The exact mode is applied only to a directory this call created, through
  an fd opened with `O_DIRECTORY|O_NOFOLLOW` (or after a `symlink_metadata` check that refuses symlinks). An existing
  directory that is a symlink, or is open to group or others, is a typed error and is never chmodded.
- Link's existing behavior is preserved (`InsecureDir`, socket 0600). A directory created by link is now chmodded exactly 0700,
  the same as event_log.
- Tests: a symlink planted at the final part of the path is refused, not followed or chmodded, and the existing
  permission tests for both adapters still pass unchanged. Also document the "parents created under umask 277 are 0500" note.
scope: crates/adapters

### F11: private_fs hardening and untested guarantees
status: done (f8bc4d9)
depends: F10
touches: adapters::private_fs, adapters::event_log
goal: Close the last F10 review gaps. Every documented guarantee is proven by a test (CLAUDE.md, reviewer rule).
acceptance:
- `open_existing_private_file` can't hang on a FIFO or other special file. Open with `O_NONBLOCK`, fstat-check for a regular
  file, then clear `O_NONBLOCK`. There's a test with a 0600 FIFO at a log path, where append and read return a typed error promptly
  (enforced by a liveness bound, not a timing assertion).
- The directory `AlreadyExists` path is tested directly: pre-create a 0755 dir and a symlink at the final part, and assert
  `Insecure`/`Symlink` with the mode and target unchanged. Factor the step into a private fn if needed.
- The event_log "durable against a crash of this process once `append` returns" claim is tested: a child process appends and then
  `abort()`s, and the parent reopens and sees the entry.
scope: crates/adapters

### F12: Continuation prompts don't count as user turns
status: done (5d09c6d)
depends: T15
touches: core::session
goal: `SessionState::turns()` counts prompts the user actually sent.
acceptance:
- Prompts marked `_meta.shepherd.continuation: true` (T15's continuations of a blocked Stop) open a turn for activity purposes,
  but don't increment `turns()` or change `last_prompt()`. There's a table test.
scope: crates/core

### F13: Client reconnect backoff
status: todo
depends: T12
touches: core::client
goal: A host that accepts, completes hello, then drops right away can't make the client reconnect in a tight loop.
acceptance:
- Consecutive outages that each end within a short window back off exponentially (capped). A stable connection resets the
  backoff. Driven by `Tick`, no clocks. Core table tests.
scope: crates/core

### F14: PTY read backpressure
status: done (cd48050)
depends: T11
touches: ports::pty, adapters::pty, testkit pty, shepherd::host
goal: A session that floods output (`yes`, a huge `cat`) can't grow host memory without bound or starve other sessions.
acceptance:
- Credit-based flow control: the Pty adapter's pump stops reading a session once its unacknowledged output passes a limit
  (e.g. 1 MiB). The runtime acknowledges bytes after processing them (`Pty::ack(session, n)`, additive). The kernel pty buffer
  then fills and the child blocks, as in a real terminal.
- Contract cases: a child that floods while nothing is acknowledged stops being read (bounded outstanding bytes, counted not
  timed), and resumes after the ack. Exactly-one-`Exited` and output ordering still hold.
- A host scenario: one session floods, and another session's terminal query is still answered and a client request still
  handled, with bounded queued events (counted).
- Fake supports the same semantics.
scope: crates/core (port), crates/adapters, crates/testkit, crates/shepherd

### F15: Client display polish from T13
status: done (065f873)
depends: T13
touches: shepherd::client, core::client, tui
goal: Small display gaps found while building the TUI.
acceptance:
- The client runtime services `Screen::sync_hold()` deadlines (`expire_sync`), so a session stuck mid synchronized update
  doesn't freeze its displayed frame. Driven by runtime deadlines, no sleeps.
- `ClientState::notice()` is cleared on the next user action or after a deadline. Core table test.
- In the collapsed sidebar the help button `?` shares a column with `Activity::Unknown`'s `?` glyph; give Unknown a distinct
  glyph (e.g. `·`) and update snapshots (F21 review).
- While a notice is showing it gets the whole footer width and the Detach/? buttons hide; they return when it clears
  (T34 left notices at 13 columns because notices weren't transient yet). Snapshot both states.
- The cursor shape and blink from the session's `Screen` are passed to the real terminal.
scope: crates/core, crates/tui, crates/shepherd

### F16: Terminal setup hardening
status: done (2475af9)
depends: T13
touches: shepherd::terminal, shepherd::client, tui::keys
goal: Close the T13 review's last gaps.
acceptance:
- The restore guard exists before `enter` and `push_keyboard` run, so a panic inside either still restores the terminal. There's a
  test with a fake whose push panics, and one whose enter panics partway.
- A panic on a non-main thread makes `shepherd` exit non-zero (a dedicated event, not the detach path). Test it.
- `from_legacy_terminal` remaps whenever CONTROL is present (so ctrl+alt+\ reaches a kitty session as `CSI 92;7u`). Update the
  CONTROL|ALT row.
scope: crates/shepherd, crates/tui

### F17: Worktrees contract flake under heavy load
status: todo
depends: T20
touches: adapters::worktrees (tests)
goal: `worktrees::tests::remove_deletes_the_directory` failed once in 24 parallel runs (the `create` step at
contracts/worktrees.rs:162 errored during a 215 s run at load average ~70).
acceptance:
- Also seen: one unidentified `shepherd-adapters --lib` failure in the PM's merge gate for T22 (2026-10-30; passed on rerun;
  log not kept). Likely the same class.
- Reproduce and find the cause: a git timeout, or a real race. If it's a timeout under extreme load, make the fixture's bounds and
  error reporting robust (print the actual error). If it's a race, fix it. Then 0 failures in 5 rounds of 24 parallel copies.
scope: crates/adapters, crates/testkit

### F18: Host startup errors reach the user
status: done (03c6d2f)
depends: T12
touches: adapters::launcher, core::ports::launcher, core::client, shepherd::client, shepherd::host
goal: When the auto-launched host can't start (e.g. `~/.shepherd` is 0755), the user sees the host's own error message instead
of "connecting…" and a bare exit. Found in the user's first run.
acceptance:
- The real launcher gives the host a pipe as stderr. The host writes startup errors there (it already prints them via
  `report`), and once it is serving (socket bound, log open) it redirects its stderr to `/dev/null`, which closes the pipe.
- `launch` reads that pipe until EOF. EOF with no bytes means the host started: `Ok(())`. Bytes followed by EOF mean it failed:
  `LaunchError::HostFailed(String)` with the text (trimmed, capped at e.g. 4 KiB). A generous liveness bound (e.g. 10 s) applies,
  never a measurement; hitting it returns `Ok(())` and the connect retries decide.
- Adapter tests with stand-in scripts: one prints an error and exits 1 → `HostFailed` with that text; one closes stderr then keeps
  running → `Ok`; one that never writes and execs `sleep` → `Ok` within the bound. The existing "host outlives the client" test
  still holds.
- The client shows `HostFailed`'s message on its error screen and, after restoring the terminal, prints it to stderr and exits
  non-zero. A core table test for the `LaunchFailed` → notice/quit path; a scenario test with a fake launcher that fails.
- An end-to-end test: `shepherd` with a temp HOME whose `.shepherd` is 0755 exits non-zero and its stderr contains
  `chmod 700`. (Never the real `~/.shepherd`.)
scope: crates/core (port error variant, client), crates/adapters (launcher), crates/shepherd (host stderr handoff, client exit)
out of scope: changing the 0700 policy (decision 0010); fixing the user's directory automatically.
notes: `LaunchError` gains `HostFailed(String)` (additive). The fake launcher can be told to fail with `HostFailed`. Add a
contract case for it if the contract suite covers error variants.

### F19: Drop hook translator state when a session closes
status: done (b813457)
depends: T16
touches: core::agents (Translators), shepherd::host
goal: `Translators` keeps one translator per session for the life of the host run, even after the session closes.
acceptance:
- Closing a session (or its exit followed by close) removes its translator. A host test counts live translators after
  open/close cycles (counted, not measured).
scope: crates/core, crates/shepherd

### F20: Hook disconnect test: fake vs real link sink lifetime
status: done (2c5b3d0)
depends: T16
goal: The T16 review thought the fake drops a conn's sink earlier than the real link. The builder showed both drop it right
after `Disconnected`, so they already agree. Outcome: no fake/adapter change; the misleading comment in
`a_host_that_drops_the_connection_before_answering_is_a_disconnect` now explains the sink copy (it keeps the channel open so
only the `Disconnected` event can end the test).
### F21: Mouse targets in the collapsed sidebar
status: done (c59acb7)
depends: T34
touches: tui::sidebar, tui::layout
goal: In the narrow layout (sidebar collapsed to glyphs) Detach and help have no click target, and rows have no `×`
(decision 0014: every action is clickable).
acceptance:
- The collapsed sidebar shows compact glyph buttons for help and Detach at the bottom; clicking them works like the full
  footer. Close stays reachable via right-click → Close. Hit tests click rendered cells; narrow snapshot updated.
- Popups that close a session also close when the host removes that session meanwhile (seen in T34; applies to keyboard and
  mouse popups). Core table test.
scope: crates/core (client), crates/tui

### F22: Agent session polish from T17
status: done (83099a3)
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

### F23: A program's output can panic the emulator (and the host)
status: done (74e2abb)
depends: T09
touches: term (feed), shepherd::host (screen feed), shepherd::client (screen feed)
goal: Feeding `\x1b[>1u` about 5,000 times panics alacritty_terminal 0.26 (`term/mod.rs:1296`): when the kitty keyboard
stack reaches its max depth, `push_keyboard_mode` calls `self.title_stack.remove(0)` on the (empty) title stack. Any
program in any session can crash the host and every client. Found by the F07 builder.
acceptance:
- Regression test first (`fix-bug` skill): feeding `CSI > 1 u` well past the max depth to a `Screen` doesn't panic, and
  `CSI ? u` still answers the top's flags. Then fix in `shepherd-term` without forking alacritty: e.g. track the kitty stack
  depth in `Screen` (it already tracks `kitty_top` after F07, if merged) and drop `CSI > … u` pushes past a cap below
  alacritty's limit, filtering at the byte level only where the parser state allows it (reuse the existing unfinished-sequence
  tracking; never split a sequence wrongly). Document the cap.
- Defense in depth: a panic inside `Screen::feed` for one session must not take down the host or the client. The host and
  client runtimes run each feed under `catch_unwind`; on panic they replace that session's `Screen` with a fresh one of the
  same size, log it (host.log), and continue. Test with a test-only way to make a `Screen` panic (e.g. a cfg(test) hook or a
  trait seam), never by relying on the alacritty bug.
- Check the same pattern for other alacritty stacks reachable from output (title stack push `CSI 22 t`, etc.) and add a
  regression test for each that holds; report any that panic.
scope: crates/term, crates/shepherd

### F24: Profile picker polish
status: done (520877d)
depends: T17b
touches: tui::popup, tui::keys
goal: Gaps from the T17b review.
acceptance:
- A long profile list scrolls (wheel) instead of being clipped; dialog height uses saturating arithmetic and is capped to the
  window (no overflow panic for a huge list from the host).
- The agent and `(default)` columns are sized from the data (like `name_width`), and a long profile name is truncated so
  `(default)` stays visible.
- Esc on the picker has a key test (it mirrors the context menu).
- A client scenario: the fake host sends profiles, a click on `+ New session` then a profile row sends `SessionNew` with
  that profile.
scope: crates/tui, crates/shepherd (tests/support fake host)

### F25: Clean up after a failed worktree create
status: done (8861b54)
depends: F08
touches: adapters::worktrees
goal: A `post-checkout` hook that exits non-zero (or any non-timeout failure of `worktree add`) can leave the worktree and
branch behind, like the timeout case F08 fixed.
acceptance:
- Any `worktree add` failure after git registered the worktree runs the same cleanup as F08; a retry with the same name
  succeeds. Adapter test with a failing hook. Errors that happen before anything is created clean up nothing.
scope: crates/adapters

### F26: Attention follows window focus
status: done (9be58f9)
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

### F27: Worktrees calls run off the host runtime thread
status: done (db9a442)
depends: T21
touches: ports::worktrees (or a runtime worker), shepherd::host
goal: `Worktrees` calls (`default_branch` may run `git remote set-head --auto` over the network; T22's `create` fetches) run
synchronously on the host's single runtime thread, freezing every session's output and every client while git works.
acceptance:
- Worktrees operations run on a worker thread and report back as runtime events (decision 0011's pattern: ports push events
  into the runtime channel). Core issues the command and handles the result event; it never waits.
- A host scenario with a fake Worktrees whose call blocks until the test releases it: while it's blocked, another session's
  PTY output still reaches a client and a client request is still answered (counted, not timed). Then release → the result
  is applied.
- Results that arrive for a request that no longer matters (workspace already selected by someone else, session closed) are
  handled by core without side effects. Core table tests.
scope: crates/core, crates/shepherd, crates/testkit
notes: Decide in the report whether this is an async port shape (`start_*` + events) or a generic worker wrapper around the
sync port; prefer the wrapper if it keeps the port trait and its contract suite unchanged.

### F28: Workspace from a subdirectory, and `~` in typed paths
status: done (6f1c62b)
depends: T21
touches: ports::worktrees, adapters::worktrees, core::host (workspace)
goal: Running `shepherd` in `<repo>/crates` should pick `<repo>`, and typing `~/Code/x` in the switcher should work.
acceptance:
- `Worktrees` gains `repo_root(&Path) -> Result<PathBuf, WorktreeError>` (git `rev-parse --show-toplevel` with T20's pinned
  options and env stripping; for a linked worktree, the main repo root via `--git-common-dir`). Contract cases for: repo root,
  subdirectory, linked worktree, not a repo. Fake supports it.
- Workspace selection (hello cwd and typed paths) resolves to the repo root. Selecting a subdirectory of the current workspace
  is the same workspace (no append).
- A typed path starting with `~/` (or exactly `~`) expands to the host's home; other relative paths are still refused.
- From the F27 review: a scenario where a hello-cwd selection arrives live (probe held, hello with cwd, subscribe, release →
  the client gets `workspace/selected` live, not in catch-up); and either a worker unit test with a dead thread/dropped receiver
  or soften `Worker::submit`'s "every call is answered" doc (queued jobs are dropped if the thread dies).
scope: crates/core, crates/adapters, crates/testkit, crates/shepherd

### F29: Workspace switcher list scrolls and fits the window
status: done (a27fa98)
depends: T21, F24
touches: tui::popup, core::client
goal: The T21 workspace switcher has the same unscrolled, uncapped list the profile picker had before F24.
acceptance:
- Reuse F24's approach (core-owned scroll, saturating height capped to the window, wheel scrolls, rows below the fold are
  reachable by wheel then click, long paths truncated from the left so the repo name stays visible). Hit tests click rendered
  cells; snapshot of a scrolled switcher; a huge list doesn't overflow.
scope: crates/core (client), crates/tui

### F30: Truncation measures display width
status: done (faa7403)
depends: F29
touches: tui::popup, tui::sidebar
goal: `cut` (picker, F24) and `cut_left` (switcher, F29), and any other truncation in the tui, count characters, not terminal
columns, so paths and names with double-width characters (CJK, emoji) overflow and push the mark/repo name off.
acceptance:
- All tui truncation measures display width via ratatui's `Span::width` / unicode-width (no new dependency), never splits a
  grapheme, and keeps the `(current)`/`(default)` marks and the repo name visible. Tests with a CJK path and an emoji name in
  the switcher, picker and sidebar rows. Column sizing (`name_width`, agent column) uses display width too.
scope: crates/tui

### F31: Only real answers count as answering a permission dialog
status: done (2839492)
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

### F32: Create the worktree branch atomically
status: done (d93bb15)
depends: F25
touches: adapters::worktrees
goal: F25 re-checks that the branch is absent right before `worktree add -b`, but a branch created in the instant between that
check and git's own branch step would still be deleted by cleanup.
acceptance:
- Create the branch with `git update-ref refs/heads/<b> <base-oid> ""` (fails atomically if it exists → BranchExists, nothing
  to clean), then `git worktree add <path> <b>` without `-b`. Cleanup deletes the branch only if this create made it (and
  only with `update-ref -d refs/heads/<b> <base-oid>`, i.e. only if it still points where we put it). Regression tests: a branch
  appearing just before update-ref → BranchExists, untouched; a branch moved by someone after we created it → not deleted.
- F28 review: pin `main_checkout`'s checks with an adapter test for a main checkout with `core.worktree` set (and a comment
  that the same-repo check is defensive); fake `add_dir` parent semantics needs a contract case.
- F25 review: the signal-kill path sends SIGKILL to the group after git was reaped (theoretical pid reuse). Learn how git
  died with `waitid(WNOWAIT)` (as `wait_unreaped` does), kill the group, then reap.
scope: crates/adapters

### F33: Hooks don't queue behind each other or outlive the host
status: done (5cf8a10)
depends: T23
touches: adapters::commands, shepherd::host
goal: From the T23 review: one shared `hooks` worker means session B's post-create hooks wait behind session A's (up to the
10 min timeout), closing A doesn't cancel its hook, a hook still running at host shutdown keeps running (it's `setsid`), and
hook output goes to an unlinked temp file with no size cap.
acceptance:
- Hooks for different sessions run concurrently (a worker per running hook, or a small pool); a held hook for one session
  doesn't delay another's (scenario with two held hooks, counted).
- Closing a session kills its running hook's process group (and waits for it); host shutdown kills every running hook group.
  No leaked processes (check pids you recorded).
- Hook output is bounded on disk (cap the file, e.g. 1 MiB, then keep only the tail) — contract case with a chatty hook.
scope: crates/adapters, crates/core, crates/testkit, crates/shepherd

### F34: The client marks keys that can answer a dialog
status: done (274b3e2)
depends: F31
touches: tui::keys, core::wire (input kind)
goal: F31 classifies answers from bytes, so Esc and Ctrl-C (which dismiss Claude's permission dialog without a hook) don't
count, and in kitty "report all keys" mode (flag 8) no key counts. The client knows the actual key event and can tag it.
acceptance:
- The tui tags key input that can answer (Enter, Esc, Ctrl-C, printable) as `kind: answer` (additive) regardless of encoding
  mode; the quiet watch counts `answer` and `paste`. Table tests over key events × keyboard modes (legacy, kitty flags 1/8).
scope: crates/core, crates/tui

### F35: One display-safe text rule for everything agents and hooks can name
status: done (f79d427)
depends: T24
touches: tui (render), core (display text)
goal: Several strings shown in the UI come from untrusted sources: auto names (T24), manual renames from any client, hook
output tails (T23), failure reasons (T22), git error text. T24 now sanitizes its slug, but there's no single rule at display
time.
acceptance:
- A single pure `display_safe(&str, max) -> String` in core (or tui) used wherever such text is rendered or put in an OSC 9
  alert: drops Cc and Cf (bidi controls, ZWJ, BOM, tags), caps combining marks per base, caps length by graphemes, NFC.
  Table tests with Trojan-Source bidi strings, Zalgo, and huge inputs (counted work). Every render/alert site uses it (grep
  test or a reviewer checklist item).
- From the T24 final review: a name must never start with `-` when the first word is dropped (over-long first cluster);
  test the exact 120-byte boundary for later words; no combining marks after in-word punctuation; drop the redundant
  apostrophe strip.
scope: crates/core, crates/tui, crates/shepherd

### T36: Spike: hot-swap the host on a version mismatch
status: done (spike merged 4e2ec1d; decision 0018)
depends: –
touches: docs only (spike)
goal: Installing or rebuilding `shepherd` leaves the old host daemon running; new clients then hit "method not found" on new
requests (seen 2026-10-30: a host from before T21 refused `_shepherd/workspace/select`). The user wants the host to update
itself in place on macOS/Linux when it detects a mismatch, ideally without killing running sessions.
acceptance (spike: findings + a recommended design in docs/architecture.md, no production code):
- Mismatch detection: what the hello carries (build id: crate version + git hash baked at build time, or the exe's path +
  mtime/inode), and who decides (client sees an older host; host sees a newer client).
- Swap mechanism, options compared with a small throwaway prototype each:
  (a) re-exec in place: the host `execve`s the new binary at its path, passing PTY master fds and the listening socket across
  exec (cleared CLOEXEC) plus a handoff file (session → fd, screen snapshots via `Screen::snapshot_ansi`, run/translator
  state); the log is already the source of truth. Agents keep running.
  (b) the new binary starts as a second host, takes over the listening socket and receives the PTY fds from the old one over a
  unix socket (SCM_RIGHTS), then the old exits.
  (c) simple restart (old host exits, sessions die, client relaunches) as the fallback when handoff isn't possible.
- What breaks: the exe replaced on disk while running (cargo install / rebuild — macOS code signing, Linux "text file busy"),
  log lock handoff, half-written frames, a handoff that fails midway (must fall back to (c) with nothing lost from the log),
  a downgrade, two clients of different versions.
- UX: the client shows "host updated" / "host is older — updating…"; never silently kills sessions unless (c) is the only
  option, and then asks first.
notes: Follow-ups after the spike: T36a mismatch detection + clear UX (can ship first, small), T36b the chosen swap.
herdr (github.com/herdrdev/herdr) has a unix-only "live handoff" (`PlatformCapabilities::live_handoff`, src/main.rs,
src/persist.rs, src/update.rs): read it first; it's likely prior art for (a)/(b).

### T37: Worktree creation has no time cap, runs concurrently, and cancels on close
status: done (850906b)
depends: T22, F27, F32
touches: adapters::worktrees, shepherd::host (worker), core::host (close while creating)
goal: On the user's real repo (macro: large, git-lfs) a new session stuck on "creating worktree…": the 120 s GIT_TIMEOUT
covers `worktree add`, so a long checkout (LFS smudge) is killed and cleaned up; and the single Worktrees worker made a second
new session queue behind it. herdr (github.com/herdrdev/herdr, src/worktree.rs + src/app/api/worktrees/deferred.rs) runs
`git worktree add -b <b> <path> HEAD` with no timeout on its own thread per operation, and it works well.
acceptance:
- `worktree add` (and the checkout/LFS work it does) has no time cap. Git still runs non-interactively (no tty, setsid,
  `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never` — already set), so a credential prompt fails instead of hanging.
  Short metadata calls (rev-parse, show-ref, update-ref, worktree list) keep a generous bound.
- `fetch` keeps a bound; on timeout or failure the worktree starts from the last fetched `origin/<default>` and the session
  says so (`fetch_error`, existing Stale/Local base path).
- Each `create` runs on its own thread (herdr's model), so creations for different sessions run concurrently; the worker
  for other Worktrees calls stays. A scenario with two held creates proves both start before either finishes (counted).
- Closing a session whose worktree is being created kills that git process group (wait for it to be gone), runs the
  existing F08/F25/F32 cleanup (never deleting what isn't ours), and records the close. Test with a create held on a FIFO.
- No big-repo tests (user decision): prove "no cap" with a create held well past the old bound via an injected fake clock
  or a held hook, never by size or wall-clock.
- From the F32 review: give `an_add_blocked_by_a_foreign_branch_lock_…` a short cleanup timeout and assert no "timed out",
  so removing the cleanup-delete lock pin fails fast instead of hanging.
scope: crates/adapters, crates/core, crates/shepherd, crates/testkit

### T36a: Build identity in the hello, and a clear mismatch UX
status: done (44dac96)
depends: –
touches: core::wire, core::client, ports::launcher, adapters::launcher, shepherd (build id, host hello), tui::popup
goal: When the client and host are different builds, the user finds out at hello and is told what to do, instead of a
later "method not found". Restarting the host is offered with a count of the sessions it will stop, and never happens
without the user's click.
acceptance:
- Each process computes a `BuildId { version, digest }` once at startup. `digest` is the sha256 of the file at
  `current_exe()`, and the host keeps that path. `Hello.build`, `HelloOk.build` and `HelloOk.installed` are additive,
  defaulted fields. Wire tests: the old shapes (no build) still decode, and the new shapes round-trip.
- The host answers `installed`: the digest of the file now at its startup exe path. It recomputes the digest only
  when the file's `(dev, ino, size, mtime)` changes. Test: rename a different file over a tempdir path and count the
  digests computed (no timing).
- The client's core decides each row of the spike T36 table, with one unit test per row: same build, host without
  build (and `VersionMismatch`, which no longer quits), host stale, client odd one out, all differ.
- The restart dialog has clickable [Restart host] and [Not now] buttons (decision 0014) and says how many sessions
  will stop. Restart calls a new `HostLauncher::terminate`: it reads the host's pid from the socket's peer
  credentials (`LOCAL_PEERPID`, `SO_PEERCRED`) and sends SIGTERM. It works against a pre-T36a host. Real-adapter test:
  a stand-in listener process gets the signal.
- A `-32601` answer shows "the shepherd host is older than this client: restart it to use this" (unit test).
- Scenario: host build A, client build B → dialog → Restart → host shuts down → the reconnect loop relaunches (the
  fake launcher records the launch). Snapshots of the dialog and the notice; hit tests click the buttons.
scope: crates/core, crates/tui, crates/adapters, crates/testkit, crates/shepherd
notes: This ships before T36b. Its stale-host row is replaced by the swap in T36b.

### T36b: Hot-swap the host by re-exec, keeping sessions running
status: done (2de1f2f)
depends: T36a
touches: ports::pty (new `PtyHandoff` port — PM-approved 2026-10-30, decision 0018; follow the new-port skill), adapters::{pty, event_log, link},
shepherd::host (swap, `host --adopt`, `handoff-check`), core::wire (`_shepherd/host/upgrade`), core::client
goal: When the host is stale, the client asks it to upgrade, and it re-execs the installed binary in place. The agents
keep running, and screens and status carry over. If that isn't possible, the user is asked before anything is killed.
acceptance:
- `shepherd handoff-check` prints one JSON line `{build, handoff_versions}` and exits 0. A binary without it counts as
  "can't hand off" (test with a stand-in exe that prints "unknown command" and exits 1).
- `_shepherd/host/upgrade` answers `NothingToDo`, `Busy { reason }`, `Unavailable { reason }` or `Accepted`, decided
  by a pure core function with one unit test per answer. A preflight child killed by a signal is reported as
  "overwritten in place? reinstall it".
- The swap runs in the order given in spike T36: preflight, pause and ack the pumps, drain, close conns, handoff file
  (0600), clear `CLOEXEC` on exactly the carried fds, then `execv` of the startup path (never `current_exe()` at
  swap time). If `execv` returns an error, the host restores `CLOEXEC`, resumes its pumps and keeps serving (test with
  a nonexistent path).
- The log is never dropped before the exec. The new image adopts the lock fd and the listener fd. Real-adapter
  tests: a second `JsonlEventLog::open` fails `Locked` while a lock is adopted, and a connect made during the
  hand-over is accepted by the adopter.
- `PtyHandoff` comes with a fake, a contract suite and the real adapter. Export then adopt into a fresh instance: a
  write echoes, resize works, kill works, and the exit code is reported. Export then resume: nothing changes. The real
  adopted sessions use the carried fds and `waitpid`.
- The new image adopts the PTYs before folding the logs. It feeds each `snapshot_ansi` into a `Screen` of the carried
  size and restores the translators (one that doesn't parse is dropped, tested). It records no exit for an adopted
  session and doesn't sweep its run directory. It nudges a redraw. A session it can't adopt is killed (`killpg`) and
  its exit is appended.
- End to end, with the built binary (`CARGO_BIN_EXE_shepherd`) and a tempdir `HOME`: start a shell session, send
  `upgrade { force: true }` (same-binary swap, for tests). Then check: same host pid; the shell's pid alive and
  echoing; a reconnecting client resubscribes with no missing or duplicate entries; no exit in the log for that
  session. Timeouts are liveness bounds only.
- Client: the stale row sends upgrade and shows "host is older — updating…", then "host updated". `Busy` retries on a
  deadline. `Unavailable` falls back to T36a's dialog, which asks first. Snapshots of both notices.
scope: crates/core, crates/adapters, crates/testkit, crates/shepherd, crates/tui
notes: Not verified on Linux in the spike (`ETXTBSY`, `/proc/self/exe` "(deleted)"). Run the end-to-end test in CI on
Linux too.

### F36: Close the display-safe gaps from F35's review
status: in-progress
depends: F35
touches: core::display, tui (clippy.toml, check-arch), shepherd (stderr)
goal: Small gaps left by F35.
acceptance:
- Cap chars kept per grapheme cluster (non-mark extenders like U+FF9E and emoji modifiers can grow one cluster without
  bound), so `display_safe` is truly idempotent; the proptest then holds (include the reviewer's counterexample:
  31×U+1100 + U+0344 at max 1).
- Also drop Zl/Zp (U+2028, U+2029).
- `crates/tui/clippy.toml`: add `Block::title_top`/`title_bottom` and the `List`/`Table`/`Tabs`/`Row`/`Cell` constructors;
  check-arch greps tui for `Line::from(`, `Text::from(`, `Span::from(` on non-`draw` input and for `.render(` on strings.
- Sanitize the two stderr prints that can carry host text (`crates/shepherd/src/main.rs` fatal error,
  `crates/shepherd/src/host/mod.rs` startup report) with `display_safe`.
- Pending user decision: keep a ZWJ when it joins two visible chars inside one grapheme (emoji families, Indic half-forms).
scope: crates/core, crates/tui, crates/shepherd, scripts

### F37: A killed fetch can leave a stale remote-ref lock
status: in-progress
depends: T37
touches: adapters::worktrees
goal: From the T37 review: a fetch killed by a cancel or its timeout can leave `refs/remotes/origin/<default>.lock`; every
later fetch then fails and falls back to the stale base.
acceptance:
- After a fetch that was killed (timeout/cancel) with `core.filesRefLockTimeout=0` pinned, remove only the remote-tracking ref
  locks that fetch could have held, and only if git died holding them (same rules as F32's branch lock). Test with a hook or
  a fake remote that hangs mid-fetch; a foreign lock is never removed.
- From F33: `worktrees.rs`'s own `wait_group_gone` returns early on macOS EPERM (group members dying/zombie); wait through EPERM
  until ESRCH as `process_group.rs` now does (or share that helper and drop the duplicate `kill_group`/`wait_unreaped`).
- From F33: a create that finishes as shutdown cancels it, with no hooks, can still spawn a PTY during shutdown; don't spawn
  once shutdown began.
- From the T37 review: host shutdown cancels creates but doesn't wait for their cleanup; delay `finish` until no create is
  running (bounded) so a cancelled-at-shutdown create never leaves an unrecorded worktree/branch.
scope: crates/adapters, crates/shepherd (host shutdown)

### F38: PTY backpressure contract case fails under load
status: in-progress
depends: F14
touches: testkit contracts::pty, adapters::pty (tests)
goal: `pty::tests::unacked_output_stops_reading_until_acked` failed 12/12 under a 12-copy load round on main (99e47e6) with
"seq didn't exit: Timeout after ~1.1MB" — a 10 s total deadline that's a measurement under load, not a liveness bound.
acceptance:
- Make the bound a generous liveness bound (or count progress instead of total time) so the case still fails if backpressure
  breaks; 0 failures in two 24-copy rounds of the whole adapters binary.
scope: crates/testkit, crates/adapters

### F39: T26 leftovers
status: todo
depends: T26
touches: shepherd::host (replay), shepherd tests
goal: Small gaps from the T26 final review.
acceptance:
- The replay test also covers `session/created`'s `worktree` and `cwd` (adopted / explicit-cwd sessions recorded with a
  symlinked spelling).
- A test-only way to give the real host an isolated git environment (HOME, GIT_CONFIG_GLOBAL/NOSYSTEM) so real-host tests
  never read the user's global git config or hooks.
- A detached orphan worktree (no branch) can be adopted and owned.
- From F33: `next_hook` calls `Commands::new_run()` before `run_alone`; a failed thread spawn leaks that id forever — call
  `new_run` after a successful spawn or forget the id on the error path.
scope: crates/shepherd, crates/core

### F40: Launcher test stand-ins can leak
status: in-progress
depends: F18
touches: adapters::launcher (tests)
goal: Two launcher hygiene stand-ins (`/bin/sh -c … sleep 120 & wait` with a USR1 trap, report under a tempdir) were found
orphaned for 5 hours on the PM's machine after load rounds; the PM stopped them by pid.
acceptance:
- Every stand-in a launcher test spawns is reaped or killed by a guard on every path (panics, timeouts), the shell exits when
  its child does (no orphaned `wait`), and a load round leaves no stand-in processes (checked by recorded pids).
scope: crates/adapters

### F41: T27 and T25b leftovers
status: in-progress
depends: T27
touches: adapters::forge (tests), shepherd tests, core tests
goal: Test gaps from the T27 and T25b final reviews.
acceptance:
- `gh_reads_nothing_from_stdin` passes regardless of the test run's own stdin: run the port from a helper whose stdin is a
  pipe with data, so `.stdin(Stdio::inherit())` fails it under the gate's `/dev/null` stdin too.
- From T25b: `inspect_worktree`'s membership `find(|w| w.path == *path)` → `next()` survives; add a case with a real managed
  worktree beside the crafted outside path/symlink. Pin a detached orphan's hooks getting `SHEPHERD_BRANCH=""`.
- `variables_that_point_gh_or_git_elsewhere_are_not_passed_on` also asserts an always-present variable (e.g. HOME) doesn't
  reach the stand-in, so deleting `env_clear()` fails it.
scope: crates/adapters, crates/core (tests), crates/shepherd (tests)

### F42: A new session starts at the wrong size until the window is resized
status: in-progress
depends: T22
touches: core::host, core::client, wire (SessionNew), shepherd::host::runtime (spawn size)
goal: User report: a new session's terminal is sized wrong until the window is resized. PM diagnosis: every spawn uses
`runtime::INITIAL_SIZE` (80×24), and `Host::handle` drops `Resize` for a session that isn't running yet — so the client's
resize on selecting the new session (sent before the PTY exists, always so for worktree sessions) is lost.
acceptance:
- Regression test first (fix-bug), at core level: a client resize for a created-but-not-yet-spawned session is not lost;
  the PTY spawns at that size (worktree and plain sessions), and a later resize still resizes.
- `SessionNew` gains an optional size (additive both ways); the client sends its current view size with it; the host uses
  the latest known size for the session (request size, then any resize before spawn), falling back to INITIAL_SIZE.
- Restarts/resumes and adopted sessions spawn at the last size the host knows for them if any.
- Scenario test through the real runtime with a fake PTY: the spawned size equals the client's view size; the TUI's first
  frame for the new session matches (no resize needed).
scope: crates/core, crates/shepherd, crates/testkit
notes: (binding) Pending size is pure core state per session (no clock); `SpawnSpec` carries the size; `INITIAL_SIZE` is
only the fallback. Also check the client sends a resize when a new session becomes selected via `session/created` (not only
on user selection) and add a test either way.

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
status: done (8900d15)
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
status: done (93cd863)
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
  (`{ session, agent, payload }`: the raw hook payload from `shepherd hook`, translated by the host; changed in T16).
- Host→client notifications: `_shepherd/log/entry { envelope }` and `_shepherd/pty/output { session, bytes }`.
- Newline-delimited framing codec: `Decoder` takes arbitrary byte chunks and yields whole messages. Tests split
  messages at every byte boundary.
- Version mismatch in hello → typed error response.
scope: crates/core

### T08: `Pty` port
status: done (bc7023e)
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
status: done (435d44b)
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
status: done (e9835a9)
depends: T07
touches: ports::link, testkit, adapters::link
goal: Move wire messages between client and host, independent of transport.
acceptance:
- Contract: host `listen` and client `connect`. Messages arrive in both directions, in order. Disconnect is observable on
  both ends. Multiple clients can connect to one host.
- An in-memory fake pair passes the contract.
- The unix socket adapter (`~/.shepherd/host.sock`) passes the contract. A stale socket file is detected and replaced.
scope: core::ports, testkit, adapters
notes: The trait moves **lines** (one encoded wire message each, with no `\n`). Framing uses `core::wire::Decoder` inside the
adapter. Same sync-plus-sink shape as the Pty port, in `crates/core/src/ports/link.rs`:
```rust
pub struct ConnId(u64);   // assigned by the adapter, unique per adapter instance
pub enum LinkEvent { Connected { conn: ConnId }, Line { conn: ConnId, line: Vec<u8> }, Disconnected { conn: ConnId, reason: DisconnectReason } }
pub enum DisconnectReason { Closed, PeerGone, SlowConsumer, Protocol(String) }   // Protocol: e.g. LineTooLong
pub trait LinkEventSink: Send + Sync + 'static { fn send(&self, event: LinkEvent); }
pub trait Link {
    fn send(&mut self, conn: ConnId, line: &[u8]) -> Result<(), LinkError>;   // never blocks on a slow peer
    fn close(&mut self, conn: ConnId) -> Result<(), LinkError>;
}
// LinkError (thiserror): UnknownConn, InvalidLine (contains '\n'), Io(String)
```
The host side is built with `UnixListenLink::bind(path, sink)` and emits `Connected` for each client. The client side is
built with `UnixConnectLink::connect(path, sink) -> (Self, ConnId)`. Both implement `Link`. Each connection has a bounded
outbound queue and a writer thread, so `send` never blocks. A peer that doesn't drain its queue past the bound (e.g. 8 MiB)
is disconnected with `SlowConsumer`. A line over the decoder limit disconnects with `Protocol`. Exactly one `Disconnected`
per conn, after all its `Line`s. Contract cases include: ordering both ways, several clients at once, 10k lines,
a peer that never reads (it must get disconnected while `send` stays fast), abrupt peer death, close from either end,
a stale socket file, and a live socket that is already bound (`AddrInUse`, never replaced). Fake: an in-memory pair with
the same semantics. Copy the Pty contract template (macro, one test per case).

### T11: Host runtime
status: done (e3b135e)
depends: T04, T06, T08, T09, T10, F06, F09, F10
touches: shepherd::host
goal: The `shepherd host` daemon loop. Events come in, `Host::handle` runs, and commands execute through the ports.
acceptance:
- Startup creates or validates `~/.shepherd` as 0700 (decision 0010) with `adapters::private_fs`, before anything else, with a typed, user-facing error
  if its permissions are too open.
- Startup order: open the `EventLog` first, which takes its exclusive root lock, and only then bind the host socket. The lock
  makes "a second host" fail cleanly before it can treat the live socket as stale and replace it.
- On start, it folds every log from `EventLog` to rebuild state. Sessions whose PTY no longer exists become Exited
  (an exited frame is appended).
- Executes `Append` (stamping it with id, ts, and machine), `SpawnPty`, `WritePty`, `ResizePty`, `KillPty`, and `Reply`.
- Keeps a `shepherd_term::Screen` per PTY. PTY output is fed into it, and `take_replies()` is written back to the PTY, so
  agents get their terminal-query answers with no client attached (decision 0007).
- Session ids are generated as `s_<lowercase ulid>` (they must match `[a-z0-9_-]{1,64}`). Timestamps are stamped as RFC 3339 UTC with ms.
- Port event sinks (`PtyEventSink`, `LinkEventSink`) are unbounded channels into the runtime loop. A sink must never block on
  the loop that owns the port (deadlock with `close`).
- Live log fan-out uses the `Envelope` returned by `append`. `EventLog::read(log, from)` is only for catching up a client
  on subscribe and for startup replay.
- Attach: subscribing to a session's output first sends `snapshot_ansi()`, then live bytes. A scenario test shows a
  client attaching mid-session and seeing the current screen.
- Fans new envelopes and PTY output out to subscribed clients. A subscribe with `from` replays history first, then goes live
  with no gap and no duplicate.
- Scenario (all fakes): client sends session/new, then input, then sees the output and the log entries. The host
  restarts and the client sees the same session list.
- Replay equality: after the scenario, a fresh host built from the fake log has the same state as the live one.
scope: crates/shepherd
notes (PM design, binding):
- Decision 0011: one runtime thread looping over an unbounded channel of `RuntimeEvent`s (Pty events, Link events, deadline
  expiry, shutdown signal). For each event: decode, call `Host::handle` (and `Host::apply` after each `Append`), then execute
  the commands in order. Follow the runtime contract documented on `Host` exactly (seal-append-apply, spawn failure becomes
  `PtyExited{code:None}`, a failed Append aborts the batch and replies Internal).
- Ids and time: a small `Env` trait in the shepherd crate (not a core port; core never calls it) with `now() -> Timestamp`,
  `new_session_id() -> SessionId` (`s_<lowercase ulid>`), and `new_entry_id() -> EntryId`. The real impl uses `jiff` and `ulid`
  (both allowed in the shepherd crate). The scenario impl is deterministic. `MachineId` is a constant `local` until T29.
- Wire: decode each client line with `core::wire`. Map `ClientRequest`s to `HostEvent`s with a `ReplyTo` token that maps back to
  `(ConnId, RequestId)`. Notifications (input, resize) carry no reply. `hello` is answered by `wire::negotiate`. Malformed
  lines follow `RequestError::into_response` (answer or stay silent).
- Subscriptions are per connection: a host-log `from` seq plus a session-log `from` per session. Handle a subscribe in one loop
  iteration: `EventLog::read(log, from)` catch-up, then register for live fan-out (no gap, no duplicate, by construction).
  Output subscription for a session sends `snapshot_ansi()` first, then live bytes.
- Screens: one `shepherd_term::Screen` per live PTY, fed every Output event. After each feed, write `take_replies()` back to the
  PTY. Service the sync-update timeout as a deadline (`sync_hold()` → `expire_sync(id)`), with no sleeps. Resize the Screen
  on ResizePty.
- New sessions: the command comes from `Config::resolve(raw, login_shell)` (the `shell` profile, with `$SHELL` read by the runtime),
  cwd is `$HOME`, and the initial size is 80x24 until the client sends a resize.
- Startup: PTYs never survive a host restart, so every session that is open and not exited after replay gets
  `PtyExited{code: None}` fed through `Host::handle` (which appends `agent/exited`).
- Shutdown on SIGTERM/SIGINT (`signal-hook` is allowed): kill PTYs, let their exits append, close links, drop the EventLog
  (releasing the lock). There's a scenario test using a fake shutdown event.
- Logging: no printing (clippy denies it). Add `tracing` plus a file writer to `~/.shepherd/host.log` (0600 via `private_fs`).
  Errors that aren't sent to a client go there.
- `main.rs`: `shepherd host` runs the host runtime. Keep the edit minimal (one match arm), because T12 is adding the client
  arm concurrently.
- Scenario tests in `crates/shepherd/tests/` use FakePty, FakeEventLog, FakeNetwork, FakeConfigSource and a deterministic `Env`.
  Put the reusable harness (build a host runtime on fakes, drive a fake client conn, step the loop until idle) in
  `crates/shepherd/tests/support/` or testkit, so T12, T13 and M2 reuse it.

### T12: Client core + runtime
status: done (5d47ee2)
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
notes: `Client::handle(ClientEvent) -> Vec<ClientCommand>` in `core::client` is pure: no IO, no clock, and request ids and
timestamps come in events. Events come from the link (decoded `HostMessage`s), from the user (abstract intents like
`NextSession`, `Input(bytes)` and `Resize`; the TUI's key mapping is T13), and from the runtime (`Connected`, `Disconnected`,
`LaunchFailed`). Commands are `Send(ClientMessage)`, `LaunchHost`, `Connect` and `Quit`. The client tracks pending request ids
to type responses (`RawResponse::typed`) and keeps the session list by folding `_shepherd/log/entry` envelopes. Reuse
`core::session` for activity and `core::host`'s session fold where it fits; don't fork them.
Starting the host is a side effect behind a new small port `HostLauncher { fn launch(&self) -> Result<(), LaunchError> }`
(`core::ports::launcher`). The real adapter spawns `<current exe> host` detached (`setsid`, stdio to null or a log file
under `~/.shepherd`). The fake records launches and can be wired to start a fake host on the fake link. The contract covers
launch success and failure, and that a launched process outlives the client (adapter test). Connect retries are client
policy in core: up to N attempts, with the next attempt triggered by a runtime `Tick { at }` event, with no sleeps.
The client runtime in `crates/shepherd/src/client.rs` is generic over `Link` and `HostLauncher`. It feeds PTY output bytes to a
`shepherd_term::Screen` per session (no rendering in this task). Scenario tests use the fake link plus a fake host driven by
`core::host` (or a scripted fake host).

### T13: TUI layout, sidebar, and keys
status: done (918fdd4)
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
**This task finishes M1, the first thing the user can run.** It includes wiring the TUI into the real terminal:
- `shepherd-tui`: pure `render(&ClientState, &Screens, &mut Frame)` (sidebar plus the selected session's `Screen` via the existing
  `render_screen`, popups, help) and pure `map_key(KeyEvent, &ClientState) -> Option<ClientEvent>` plus input encoding per
  `Screen::modes()` (kitty keyboard, SGR mouse, bracketed paste, focus). Encoding is a pure function, and table-tested.
- Client runtime wiring (`crates/shepherd/src/client.rs`): crossterm raw mode, alternate screen, and restoring the terminal on
  every exit path (normal quit, error, panic hook, SIGTERM/SIGHUP). Read crossterm events on a thread into the runtime channel
  (decision 0011), and redraw with ratatui after state or screen changes, coalesced so a flood of output doesn't redraw per byte.
  Terminal resize sends `Resize` for the selected session's pane size (window minus sidebar), not the whole window.
- The pane size passed to resize must match exactly what `render` gives the session, so the agent's TUI fits. Test that the
  layout function and the resize computation agree.
- `q` detaches: it quits the client and leaves the host and sessions running. Starting `shepherd` again reattaches (T11
  snapshots).
- Scenario test: a client runtime with a ratatui `TestBackend` (instead of the real terminal), wired to the T11 host harness or
  T12's fake host. Keys create two sessions, switch, type, and the rendered buffer shows the right session's output and sidebar
  state. Put the real-terminal setup behind a tiny seam so it isn't exercised in tests (no real tty needed in the gate).
- T11 (host runtime) may merge while you work: it also edits crates/shepherd (main.rs `host` arm, lib.rs, tests/support,
  Cargo.toml). Keep your edits to those files minimal, and rebase if it lands.
- After it merges, the PM will do a manual smoke run of the real binary.

---

## M1.5 Point and click

Decision 0014: every action is reachable with the mouse. The `ctrl-b` bindings from T13 stay as they are.

### T33: Clickable sidebar and terminal focus
status: done (264bf12)
depends: T13
touches: tui::layout, tui::sidebar, tui::keys (mouse), core::client, shepherd::terminal
goal: Click a session to switch to it, click a `+ New session` button to make one, click the terminal to type in it.
acceptance:
- Mouse capture is on for the whole client lifetime (not only when the session asks for mouse), and still restored on every
  exit path. Update `terminal.rs` and its tests.
- The sidebar has a header row with a `+ New session` button. Clicking it (left button, on release inside the same control it
  was pressed on) emits the same intent as `ctrl-b n`.
- Left-clicking a session row (either of its 2 lines) selects that session and sets focus to Terminal. Clicking the already
  selected row only sets focus. Clicking empty sidebar space does nothing.
- The wheel over the sidebar scrolls the session list when it's longer than the sidebar (scroll offset is client view state in
  core; selection keeps itself visible). Rows below the fold are reachable by wheel then click.
- In the terminal pane: if the session has mouse tracking on, events are encoded to it as today. If not, a left click sets
  focus to Terminal and nothing is sent; the wheel is not sent either (scrollback is out of scope).
- In the narrow layout (sidebar collapsed to glyphs), clicking a glyph row selects it and the `+` glyph creates a session.
- The empty-state hint and the help popup say "click + New session" first and mention the keys second. Help mentions
  shift-drag / option-drag for selecting text.
- Hit testing is a pure function `hit(layout, &ClientState, col, row) -> Option<Target>` in `tui`, computed from the same
  layout and scroll offset `render` uses. Tests click the cell where each control is rendered (find it in the `TestBackend`
  buffer, don't hard-code coordinates) and assert the intent. Include off-by-one tests on row and button boundaries.
- Snapshots: updated sidebar with the header button, a scrolled list, and the narrow layout.
- Scenario (client runtime with `TestBackend`): click `+ New session` twice, click the first row, type, and the first session
  gets the input.
scope: crates/core (client), crates/tui, crates/shepherd (terminal, client runtime wiring)
out of scope: context menus, rename/close by mouse, dialog buttons (T34); hover effects (no motion tracking needed);
drag-reordering; scrollback.
notes: New intents in `core::client`: `SelectSession(SessionId)` and `ScrollSidebar(i32)`. `NewSession` already exists, so reuse it.
Press/release: act on `Up(Left)` when the `Down(Left)` hit the same target. Keep the pressed target in tui-side input state
owned by the client runtime, or in `ClientState` if core needs it; the builder decides and says why in the report.

### T34: Context menu, dialogs with buttons, detach button
status: done (83f196a)
depends: T33
touches: tui::popup, tui::sidebar, tui::keys (mouse), core::client
goal: Rename, close and detach with the mouse; every popup can be answered by clicking.
acceptance:
- Right-clicking a session row opens a context menu at the click (clamped to the window) with `Rename` and `Close`. Clicking an
  item runs it. Clicking outside the menu, or Esc, dismisses it.
- Each row's line 1 shows a `×` at its right edge; clicking it opens the close confirmation for that session (it doesn't
  select it first).
- The close confirmation has clickable `[Close]` and `[Cancel]` buttons. Rename has an input plus `[Save]` and `[Cancel]`.
  Help has `[Close]`. Clicking outside any popup cancels it. Keyboard answers (Enter/Esc/y/n) keep working.
- A sidebar footer has a `Detach` button (same as `ctrl-b q`) and a `?` button that opens help.
- Pure hit testing extends T33's `hit` with popup and menu targets; popups take priority over what's under them.
  Tests click rendered cells, as in T33.
- Snapshots: context menu open, close confirmation with buttons, rename dialog with buttons, footer.
- Scenario: right-click a row → Rename → type a name → click `[Save]`, and the sidebar shows the new name; click `×` →
  `[Close]`, and the session is gone.
scope: crates/core (client), crates/tui
out of scope: new hotkeys; hover highlighting; menus for features that don't exist yet.
notes: Menu and popup state live in `ClientState` (it already holds popup state). New intents: `OpenMenu { session, at }`,
`MenuChoose(MenuItem)`, `Dismiss`, plus reuse of the existing rename/close/detach intents.

---

## M2 Agents

### T14: `Config` port and agent profiles
status: done (d8173f4)
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
status: done (7eb7375)
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
status: done (01acabc)
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
status: done (b61bd59)
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
status: done (a18bc42)
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
status: done (b17c1b4)
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
status: done (76b5bae)
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
status: done (27ddd29)
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
status: done (a87962e)
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

## M3 Workspaces & worktrees

### T20: `Worktrees` port
status: done (9a7d007)
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
notes: Use the `git` CLI, not a git library. Worktree support in gix and git2 is incomplete, and `git` is the reference
implementation the user's hooks expect. Run it with an explicit `-C <dir>` and parse only `--porcelain` or plumbing
output (`worktree list --porcelain`, `status --porcelain=v2 --branch`, `rev-parse`, `for-each-ref`). Never parse
human-readable output. Tests must never read or write the user's git config: set `GIT_CONFIG_GLOBAL=/dev/null`,
`GIT_CONFIG_NOSYSTEM=1` and a temp `HOME`, plus a fixed author/committer identity. The managed root (`~/.shepherd/worktrees`)
is a constructor parameter, and tests use a tempdir. The trait is sync, and git calls are bounded by a timeout (fetch
especially), returning a typed error. Path safety: `remove` refuses any path outside the managed root, tested with
`..` escapes and symlinks.

### T21: Workspaces
status: done (a05e6ae)
depends: T06, T13
touches: core::host (workspace), core::client, tui::header
goal: Choose a workspace (repo) once and have it remembered, visible, and easy to swap.
acceptance:
- `_shepherd/workspace/selected` is in the host log. The last selection is the default on the next start.
  `shepherd` run inside a repo with no selection picks that repo.
- A header or sidebar title shows the current workspace. Clicking it (decision 0014) opens a switcher listing the workspaces
  used before, with an input for a new path (validated through `Worktrees::default_branch`).
- Sessions record their workspace. The sidebar shows all sessions and marks those from other workspaces subtly.
  The decision on whether to filter instead is recorded in the task report.
- New sessions record the current workspace in `session/created` (additive `workspace` field) and, until T22 adds worktrees,
  start in the workspace root when the request has no `cwd`.
scope: crates/core, crates/tui, crates/shepherd (host runtime wiring), crates/testkit
notes: The `Worktrees` port (T20) isn't wired into the host runtime yet: add it as a runtime generic like `Pty`, with the
real git adapter in `host::open` and the fake in the harness. Workspace selection is host state (one per host, folded from the
log). The client sends its own cwd in hello (additive `cwd` field); if the host has no selection yet and that cwd is inside a
git repo (`Worktrees::default_branch` / repo root succeeds), the host selects that repo. The workspace is shown on a sidebar
header line above `+ New session`; clicking it opens a popup: clickable rows for previously used workspaces, a path input with
an `[Open]` button, and `[Cancel]` (T33/T34 patterns, hit tests click rendered cells). An invalid path shows the error in the
popup and selects nothing. Paths are stored canonicalized (no `..`, symlinks resolved) and compared as paths, not strings.
New wire request `_shepherd/workspace/select { path }`.

### T22: New session creates a worktree
status: done (10681b4)
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
scope: crates/core, crates/shepherd, crates/testkit
notes: (binding) Every new session gets a worktree when a workspace is selected and the request has no explicit `cwd`; with
no workspace, or with an explicit `cwd`, it behaves as today (no worktree). All profiles, shell included. Base is the latest
origin default branch (`BaseSpec` per T20). `create` runs through F27's `Worker` (never on the runtime thread); the session
is appended as created (Starting) before the worktree exists, and its PTY spawns only when the result event arrives.
Results that no longer matter (session closed while git was working) leave no PTY and record what happened; the created
worktree is left in place for T25's cleanup, and that's logged. Core table rows for this. Skip the post-create hooks step
(T23 adds it). The worktree path becomes the session's cwd for everything after: record it in `_shepherd/worktree/created`,
and T19b resume must use it (resume of a worktree session whose worktree is gone is refused, per the existing cwd check).
The branch name is a slug of the session's placeholder name (`claude-3` etc.) made unique in core against branches the host
knows about; `create` refusing an existing branch is a `session/failed`, not a retry loop. All slugging/uniqueness logic is
pure core with table tests (unicode, punctuation, collisions).

### T23: Worktree hooks
status: done (1c18b85)
depends: T22
touches: ports::commands, core::host, adapters::commands
goal: Run user-defined setup in new worktrees (copy `.envrc`, install deps, ...).
acceptance:
- Config: `[workspaces."<repo path or name>".hooks] post_create = ["cp ../.envrc .", ...]`, plus a `pre_remove` list.
- Each hook runs in the worktree with `SHEPHERD_*` env set. Its output tail and exit code go to `_shepherd/worktree/hook`.
  A failing post_create hook marks the session with a warning but still spawns the agent.
- `Commands` port with contract, fake, and a `std::process` adapter (decision 0011: no tokio), run through F27's `Worker` pattern so hooks never block the runtime thread.
scope: crates/core, crates/testkit, crates/adapters, crates/shepherd
notes: (binding) Hooks run with `/bin/sh -c <hook>` (portable; not the user's login shell), cwd = the worktree (validated to
exist), stdin closed, in their own process group, env = the host env plus `SHEPHERD_SESSION`, `SHEPHERD_WORKTREE`,
`SHEPHERD_BRANCH`, `SHEPHERD_REPO` (the main repo root, so `cp "$SHEPHERD_REPO/.envrc" .` works). Each hook has a generous
timeout (default 10 min, config `hook_timeout_s`) that kills the whole group (F08's wait-for-group pattern). Output tail is
capped (last 4 KiB, decoded lossily, control characters stripped before display). post_create hooks run in order; the first
failure (non-zero, timeout, spawn error) stops the rest, records `_shepherd/worktree/hook { hook, exit_code, output_tail }`,
marks the session with a warning (visible on sidebar line 2 like T22's text) and the agent still spawns. Workspace keys match
the canonical repo path or the repo directory's name; path match wins. `pre_remove` is parsed and validated now but run by
T25. The PTY spawns only after all post_create hooks finished (or the first failed); a session closed meanwhile spawns nothing.
Contract cases: exit code, output tail cap, timeout kills the group (no leaked children — check with a hook that forks
`sleep`), env and cwd, a missing cwd is an error.

### T24: Auto-naming
status: done (6bfeb19)
depends: T05, T22
touches: core::host (naming)
goal: Sessions get useful names without effort. Manual rename always wins.
acceptance:
- New sessions keep F22's placeholder name (`<profile>-<n>`).
- After the first user prompt in the fold, if the session has never been manually renamed, append
  `_shepherd/session/renamed { auto: true }` with a deterministic slug built from the prompt's first meaningful words
  (stopwords dropped, max 4 words or 30 chars).
- A manual rename sets `auto: false`, and auto-naming never overrides it afterwards.
- Only the display name changes. The branch name is not renamed.
scope: crates/core, crates/shepherd (only if the runtime must feed anything)
notes: LLM-generated names are a possible follow-up, behind a port, not in this task. (binding) Pure core: the slug function
is table-tested (unicode, punctuation, code blocks/paths in prompts, empty or all-stopword prompts → no rename). The rename is
appended by the host when the fold sees the first user prompt (Claude `UserPromptSubmit` / Codex equivalent via the
translators); exactly once per session; a host restart must not re-name (decide from the log). Display-name uniqueness is not
required. Resume and replay keep the name.

### T25: Close removes the worktree when it's safe
status: done (a812e2a)
depends: T22, T23
touches: core::host (close), core::client, tui::popup
goal: Worktrees go away when they should. No more pile of abandoned worktrees.
acceptance:
- Closing a session with a worktree checks `Worktrees::status` (through F27's Worker). If the worktree is clean and merged or
  pushed, it is removed automatically after its `pre_remove` hooks (T23's config, run through the Commands port/worker like
  post_create). If it is dirty or unpushed, the close confirmation lists what would be lost and offers `[Remove anyway]` /
  `[Keep worktree]` / `[Cancel]` (clickable, decision 0014). The outcome is recorded in `session/closed.worktree_removed`.
- A failing pre_remove hook keeps the worktree and says why (notice). A worktree status/remove error keeps it and says why.
- Removal never touches anything outside `<root>/worktrees`; tested with a recorded path that tries to escape (`..`, symlink).
scope: crates/core, crates/tui, crates/shepherd, crates/testkit
notes: The session closes immediately (PTY killed) and shows "removing worktree…" on line 2 until the removal finishes;
the close is recorded once the outcome is known. A session whose worktree is already gone closes normally.

### T25b: Cleanup view
status: done (877d23b)
depends: T25
touches: core::client, tui::popup, core::host
goal: Find and remove abandoned worktrees in bulk.
acceptance:
- A cleanup view (opened from a `Cleanup` button in the sidebar footer, decision 0014) lists managed worktrees with no open
  session and stale sessions (exited, and merged for more than N days) with status columns, and supports bulk removal of the
  safe ones (same safety rules and pre_remove hooks as T25). Hit tests click rendered cells; snapshot.
- From the T25 final review: a runtime test that a listed worktree with status NotARepo whose directory still exists is kept
  (the `exists` computation in runtime.rs is untested); force only when `confirmed.dirty && status.dirty`; the safe and
  unpushed-only dialogs mention that ignored files are removed.
scope: crates/core, crates/tui, crates/shepherd, crates/testkit
notes: (binding) Reuse T26's `_shepherd/worktree/list` (orphans) and T25's per-worktree inspect/remove path for every
removal, so every guarantee holds per item (list_managed membership under the resolved root, still-used re-check, pre_remove
hooks, force only with confirmed dirty). "Safe" = clean and merged/pushed (T25's rule); unsafe rows are shown with what would
be lost and are never in a bulk removal. Stale sessions: exited and their branch merged; N days defaults to 7 (config
`cleanup_after_days`), time from Tick, never a clock in core. Bulk removal runs items one at a time through the worker and
reports per-row outcomes; closing the view doesn't cancel removals already started. Popup patterns as T26/F29 (core-owned
scroll, hit tests on rendered cells, tiny-window test, draw.rs).

### T26: Search and open
status: done (103ca34)
depends: T13, T21
touches: core::client (finder), tui::finder
goal: Find and jump to any session or worktree quickly.
acceptance:
- A clickable search field at the top of the sidebar (decision 0014) opens a fuzzy finder over sessions (name, branch, workspace, machine) and managed worktrees with no
  session. Clicking or Enter selects a session. On an orphan worktree, it offers to start a new session in that worktree.
- Matching and ranking are pure core functions with unit tests (prefix > word-boundary > subsequence).
- Snapshots: empty query, a filtered list, and no results.
scope: crates/core, crates/tui, crates/shepherd (host: list worktrees), crates/testkit
notes: (binding) Orphan worktrees come from the host: a new request `_shepherd/worktree/list` answered from
`Worktrees::list_managed` for the current workspace's repo (through F27's worker), minus worktrees of sessions the host
knows. The client asks when the finder opens and shows "loading…" until the answer arrives. "Start a session here" sends
`session/new` with an explicit `cwd` of that worktree (T22: an explicit cwd gets no new worktree) and records its branch.
All displayed text goes through `draw.rs` (F35). The finder is a popup (T33/T34/F24 patterns: core-owned scroll, saturating
height, hit tests on rendered cells, every-popup tiny-window test). Typing filters; Esc/click outside closes. Matching is
pure, Unicode-aware (graphemes, case-insensitive via `to_lowercase`), never hand-rolled tables.

### T27: PR status in the sidebar
status: done (6936444)
depends: T13, T22
touches: ports::forge, core::host, adapters::forge, tui::sidebar
goal: Sidebar line 2 shows the session's PR.
acceptance:
- `Forge::pr_for_branch(repo, branch) -> Option<Pr { number, state, checks }>`, with contract, fake, and `gh` adapter.
  A missing `gh` or no auth → `Unavailable`, shown as nothing (not an error).
- The host polls open sessions on a timer driven by `Tick` events, with backoff and at most one call per session
  every 60s. Changes are appended as `_shepherd/session/pr`.
- The sidebar shows `#123 ✓/✗/…`. Snapshot test.
scope: crates/core, crates/testkit, crates/adapters, crates/tui, crates/shepherd
notes: (binding) `Forge` calls run off the runtime thread (F27 Worker pattern), with a bound (network), non-interactive
(`gh` with `GH_PROMPT_DISABLED=1`, no tty, setsid, `GIT_ASKPASS=` like T37), stdout parsed as JSON with serde (`gh pr list
--head <branch> --json number,state,statusCheckRollup` or equivalent — verify the flags in gh's docs; never parse text).
Never touch the user's gh config/auth; adapter tests use a stand-in `gh` script on PATH, never the real `gh`. Poll only
sessions whose worktree has a branch and that are not closed; polling state (last call, backoff) is pure core driven by
`Tick { at }`. The PR number/state/checks shown go through draw.rs (F35). Sidebar line 2 shows `#123 ✓` etc. after the
machine name (`local · #123 ✓`); clicking it is out of scope.

---

## M4 Multi-machine

### T28: Spike: iroh peering
status: in-progress
depends: T10
touches: docs only
goal: Confirm iroh fits the `Link` port and decide the pairing and discovery design.
acceptance:
- A prototype in a scratch crate (not merged): two hosts exchange wire messages over iroh across NAT through relays.
- Decide on: node identity storage, pairing (ticket via `shepherd pair` / `shepherd join <ticket>`), discovery of known
  peers on start, and trust (an allowlist of node IDs).
- Report the latency of keystroke round-trips through a relay compared with a direct connection.
out of scope: production code.
notes: (binding) One machine only: two host processes on localhost, one run relay-only (direct paths disabled) to stand in
for NAT; measure both. Use n0's default public relays (no self-hosted relay, no accounts). Prototype lives under the
scratchpad, never in the repo or the user's home; identity keys in a tempdir. Deliverable: a "Spike T28" section in
docs/architecture.md with evidence (iroh version, API used, numbers with how measured) and a recommended design, plus the
proposed `Link` adaptation; the PM records the decision.

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
- Catch-up stall detection (T11) counts "queue shrank", so a single huge line over a slow link (over 30 s to deliver) looks stalled.
  Add a drained-bytes counter to `Link` and use it for progress.
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
