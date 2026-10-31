# Host hot-swap
milestone: M3
goal: A new build replaces the running host in place without stopping sessions

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T36 | Spike: hot-swap the host on a version mismatch | done | spike merged 4e2ec1d; decision 0018 |  |  |
| T36a | Build identity in the hello, and a clear mismatch UX | done | 44dac96 | 2 | stale restart dialog survived reconnect; terminate didn't validate socket path/dir/peer; leaking test stand-in |
| T36b | Hot-swap the host by re-exec, keeping sessions running | done | 2de1f2f | 3 | bad config after adoption killed sessions; refuse() could killpg/close what isn't ours; several required steps untested |
| F44 | hotswap_e2e times out under load | done | ec85617 | PM spot check | stall from macOS scan of copied binaries; residual product race → F47 |
| F47 | A swap can lose its Accepted answer before exec | merging | task/f47-swap-flush @ 8684af0 | 2 | round 2 APPROVE; minors filed to F50; 12-copy round deferred (machine slow) |
| F49 | Shutdown exits without flushing closed connections | todo |  |  |  |
| F50 | Tighten F47's flush tests | todo |  |  |  |

## Specs

### T36: Spike: hot-swap the host on a version mismatch
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

### T36a: Build identity in the hello, and a clear mismatch UX
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

### F44: hotswap_e2e times out under load
depends: T36b
touches: shepherd tests (hotswap_e2e)
goal: `a_swap_after_a_new_build_is_renamed_over_the_startup_path_runs_the_new_file` once hit "nothing more from the host
within 60s" while other worktrees were building; passes alone. Find what it waits on (likely a build or a fixed bound) and
make every bound a generous liveness bound or count progress; 0 failures in a 12-copy round under parallel builds.
scope: crates/shepherd (tests)

### F47: A swap can lose its Accepted answer before exec
depends: T36b
touches: shepherd::host::swap, ports::link, adapters::link
goal: From F44: `hand_off` calls `close_conns()`, which only drops each connection's queue sender; the writer threads
flush in the background, and `swap.rs` writes the handoff file and `execv`s at once, so exec can kill a writer before it
sends `Accepted`. The client then sees "host closed the connection" without the answer (1 in ~50-70 runs of the hot-swap
e2e tests under load).
acceptance:
- Regression test first (fix-bug), deterministic where possible (e.g. a fake/slow link writer in the runtime test, or a
  contract case that a closed conn's queued bytes are delivered before `close` reports done).
- Before exec, the swap waits until every closed connection's queued bytes are written (or the conn is gone), bounded by
  `CLOSE_FLUSH`; then execs. A peer that never reads can't hold the swap past the bound.
- 0 failures in a 12-copy round of hotswap_e2e under load.
scope: crates/core (port), crates/testkit, crates/adapters, crates/shepherd
notes: (binding) PM-approved port change: `Link` gains an additive way to wait for closed conns to finish flushing, e.g.
`fn flush_closed(&mut self, within: Duration) -> FlushOutcome`, with a contract case run against the fake and the unix
adapter. (Amended after round 1: a duration, not an `Instant` from env, so the port doesn't mix clocks.) The runtime's single-thread rule (0011) holds: the wait is bounded and only during a swap.

### F49: Shutdown exits without flushing closed connections
depends: F47
touches: shepherd::host::runtime (finish)
goal: From the F47 review: `HostRuntime::finish` closes connections and the process exits without waiting for their
writers, so trailing notifications are lost. Nobody waits on an answer there (unlike a swap's `Accepted`), so it's minor.
acceptance:
- After the loop is done, `finish` calls `flush_closed(CLOSE_FLUSH)` (the same bounded call as a swap); a test shows a slow
  client receives what was queued before shutdown, and a never-reading client can't hold shutdown past the bound.
scope: crates/shepherd

### F50: Tighten F47's flush tests
depends: F47
touches: shepherd tests (hotswap, support), testkit::contracts::link
goal: From F47's round-2 review: two of its tests can stay green when the thing they guard breaks.
acceptance:
- `FakeClient::is_disconnected` (crates/shepherd/tests/support/mod.rs ~845) also returns true when the host closed with
  lines unread, so the hotswap tests' `assert!(client.is_disconnected())` (hotswap.rs ~171, ~194) pass even if the fake
  never cuts the client off. The two new hotswap tests assert the disconnect really happened (a stricter helper or a
  delivered `Disconnected`); breaking the fake's cut-off on link drop makes a hotswap test fail.
- The unix link contract's second `flush_closed(QUIET)` expecting `TimedOut` (contracts/link.rs ~756) depends on finishing
  within `CLOSE_FLUSH` of the close; restructure so a scheduling stall can't flip it (check right after `close`, or accept
  either outcome once `CLOSE_FLUSH` has passed).
- Run the 12-copy hotswap_e2e round F47 deferred (when the machine allows): 0 failures.
scope: crates/shepherd (tests), crates/testkit

