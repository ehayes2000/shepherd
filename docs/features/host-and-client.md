# Host daemon and client runtime
milestone: M1
goal: A long-lived host owns the sessions; clients attach over a link, reconnect, and see startup errors

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T10 | Link ports (transport) | done | e9835a9 | 2 | world-connectable socket |
| T11 | Host runtime | done | e3b135e | 4 | 8 MB attach limit, stalled clients never dropped, quadratic re-reads, deadline starvation |
| T12 | Client core + runtime | done | 5d47ee2 | 3 | weak tests (mutation), handshake crash loop, daemon inherits fds and signals, flaky test helper |
| F13 | Client reconnect backoff | todo |  |  |  |
| F16 | Terminal setup hardening | done | 2475af9 |  |  |
| F18 | Host startup errors reach the user | done | 03c6d2f | 2 | e2e test could leak a detached host on failure; unwritten no-stderr-before-release rule |
| F27 | Worktrees calls run off the host runtime thread | done | db9a442 | 1 | – (2 minors → F28) |
| F40 | Launcher test stand-ins can leak | done | 24f1b49 | 1 | stand-ins orphaned; now a watchdog per stand-in and a group kill on drop |
| F45 | Fresh `#!/bin/sh` stand-ins time out under heavy load (handoff, forge) | done | 1709ed0 | 1 | cause: macOS syspolicyd scans each new executable on first exec; fixtures symlink one committed script |
| F46 | Fake claude/codex scripts are fresh executables (macOS scan stall) | done | ec85617 | PM spot check | fake agents via testkit stand-in; check-arch rule |

## Specs

### T10: Link ports (transport)
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

### F13: Client reconnect backoff
depends: T12
touches: core::client
goal: A host that accepts, completes hello, then drops right away can't make the client reconnect in a tight loop.
acceptance:
- Consecutive outages that each end within a short window back off exponentially (capped). A stable connection resets the
  backoff. Driven by `Tick`, no clocks. Core table tests.
scope: crates/core

### F16: Terminal setup hardening
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

### F18: Host startup errors reach the user
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

### F27: Worktrees calls run off the host runtime thread
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

### F40: Launcher test stand-ins can leak
depends: F18
touches: adapters::launcher (tests)
goal: Two launcher hygiene stand-ins (`/bin/sh -c … sleep 120 & wait` with a USR1 trap, report under a tempdir) were found
orphaned for 5 hours on the PM's machine after load rounds; the PM stopped them by pid.
acceptance:
- Every stand-in a launcher test spawns is reaped or killed by a guard on every path (panics, timeouts), the shell exits when
  its child does (no orphaned `wait`), and a load round leaves no stand-in processes (checked by recorded pids).
scope: crates/adapters

### F45: Fresh `#!/bin/sh` stand-ins time out under heavy load (handoff, forge)
depends: T36a, T27
touches: adapters (tests: handoff, forge)
goal: In F38's load rounds, 22/24 copies failed `handoff::*` and `forge::gh_that_never_answers_is_killed_with_what_it_started`
with 60 s TimedOut — even a stand-in doing `kill -9 $$` — on old and new code. Builder's guess: macOS stalls checking newly
written executables under load. Confirm (e.g. run as `/bin/sh <file>` instead of exec'ing the file), then make these fixtures
robust; 0 failures in a 24-copy round of the whole adapters binary.
scope: crates/adapters
notes: The same family may explain the occasional worktrees `git clone`/NotARepo flakes under load (F17).
- From the F38+F40 review (launcher fixture): drop the plain `kill(pid)` in `StandInFixture::drop` (the group kill reaches
  the leader), or use a lifeline fifo the watchdog reads so stand-ins die at once when the fixture or test process goes;
  the "killed until empty" loop and the USR1 trap's group kill are untested (weaken the comments or test them).

### F46: Fake claude/codex scripts are fresh executables (macOS scan stall)
depends: F45
touches: shepherd tests (claude.rs, codex.rs, other scenario tests writing scripts)
goal: F45 showed macOS syspolicyd scans every newly written executable on first exec (one machine-wide queue), stalling
tests under load. `crates/shepherd/tests/codex.rs` `FAKE_CODEX` and `claude.rs` `FAKE_CLAUDE` (and any other test writing an
executable) are written per test.
acceptance:
- Every test that writes an executable uses F45's committed stand-in (symlink + body as data) or runs it via `/bin/sh`, as
  long as what the code under test execs is unchanged; grep proves no test writes a 0755 file any more (add a check-arch
  rule if cheap). 0 failures in a 12-copy round of each changed binary.
scope: crates/shepherd (tests), crates/testkit, scripts

