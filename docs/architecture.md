# Architecture

These are the design rules every agent works within. If a rule is in the way, escalate to the PM. Only the PM changes
this file, and every change gets an entry in the decision log at the bottom.

## Shape: host + client, each a functional core in an imperative shell

Shepherd is two processes, and both run from the one `shepherd` binary:

- **host** (`shepherd host`): a long-lived daemon, one per machine. It owns the PTYs, agents, worktrees and the
  ACP++ logs (see `docs/acpp.md`). Agents keep running when the TUI closes.
- **client** (`shepherd`): the TUI. It connects to one or more hosts, starting the local host if it isn't
  running. A host on another machine is the same protocol over a different transport (iroh, M5).

```
        client process                         wire                         host process
 keys ─▶ Client::handle ─▶ ClientCommand ──▶ JSON-RPC (_shepherd/*) ──▶ Host::handle ─▶ HostCommand ─▶ ports
 render ◀ ClientState ◀── ClientEvent ◀──── envelopes + pty bytes ◀──── ACP++ log ◀── apply(frame) ◀─┘
                                                                         ▲
                                                 shepherd hook ──────────┘ (agent hooks → frames)
```

- **`shepherd-core`** holds all domain state and all decisions. Modules:
  - `acpp`: envelope and frame types, plus parse/serialize. Lossless, and unknown frames are preserved.
  - `host`: `Host::handle(HostEvent) -> Vec<HostCommand>`. Host state is **only** `fold(apply, frames)`,
    so any state change is an `Append(frame)` command followed by `apply`.
  - `session`: the per-session fold, giving agent status, turns, tool calls and attention.
  - `client`: `Client::handle(ClientEvent) -> Vec<ClientCommand>`. Holds view state: selection, focus, popups, the
    merged session list across hosts.
  - `wire`: client↔host JSON-RPC messages (`_shepherd/*` requests).
  - `agents`: pure translation of agent hook payloads into ACP++ frames.
  - `ports`: the traits.

  Core is pure: no IO, no threads, no `Instant::now()`, no randomness. Time and new IDs arrive as event fields
  (`HostEvent::NewSession { id, at, .. }`). The runtime generates them.
- **`shepherd-adapters`** holds real implementations of the port traits. Keep them thin and keep decisions out of
  them. If an adapter grows an `if` that encodes policy, that logic belongs in core.
- **`shepherd-tui`** holds `render(&ClientState, &Screens, &mut Frame)` and
  `map_key(KeyEvent, &ClientState) -> Option<ClientEvent>`. Both are pure. It owns layout, styling, and the terminal
  emulator that turns PTY bytes into a screen grid (`Screens`).
- **`shepherd-testkit`** holds one fake per port, one contract suite per port, and the scenario harness. Other crates
  depend on it as a dev-dependency only.
- **`shepherd`** is the binary plus a lib holding the two runtime loops (host and client) and the `hook`
  subcommand. The runtimes are generic over ports so scenario tests can drive a host and a client together with
  fakes. `main.rs` only parses argv, builds real adapters, and calls a runtime.

### Why

Every behavior the user cares about is a decision in core, and you can test it with
`app.handle(event)` and assertions on the returned commands and the resulting state. The tests are fast,
deterministic, and need no terminal, git repo or agent. The world-touching code is small and covered by contract
suites. A future remote shepherd is another source of `Event`s and another sink for `Command`s, so
the multi-machine work doesn't change core's shape.

## Dependency rules (enforced by `scripts/check-arch.sh`)

| crate               | may depend on (internal)        | external deps                                    |
|---------------------|---------------------------------|--------------------------------------------------|
| `shepherd-core`     | none                            | allowlist only: see `CORE_ALLOWED` in the script |
| `shepherd-term`     | none                            | alacritty_terminal                               |
| `shepherd-tui`      | `shepherd-core`, `shepherd-term`| ratatui, crossterm (types), plus allowlist       |
| `shepherd-adapters` | `shepherd-core`                 | anything needed for IO                           |
| `shepherd-testkit`  | `shepherd-core`                 | test helpers                                     |
| `shepherd`          | all of the above                | anything                                         |

Other crates may use `shepherd-testkit` only under `[dev-dependencies]`.

Core may use **pure, well-established crates** (allowlist in the script). Never hand-roll what one of them does
(dates and times, encodings, parsing). Adding one is a PM decision: ask, don't reimplement.

Core purity is also enforced by `crates/core/clippy.toml` `disallowed-methods`/`disallowed-types`: `std::fs`,
`std::process`, `std::net`, `std::thread`, `std::env`, `Instant::now`, `SystemTime::now`.

The TUI draws text only through `crates/tui/src/draw.rs`, whose functions take a `DisplayText` that only
`shepherd_core::display::display_safe` can build (drops Cc/Cf and Zl/Zp, caps chars per grapheme cluster, NFC, caps marks and length). `crates/tui/clippy.toml` bans
ratatui's raw text-drawing methods everywhere else (F35). The session's own terminal output (`render_screen`) is the one
deliberate exception: it is drawn faithfully by the emulator.

## Ports

Test exception (F33): an adapter may test a private bounded-memory helper directly when counting its work is the only
deterministic proof (e.g. `Tail::held()`, the cancel-before-spawn seam in `commands.rs`).

A port is a trait in `shepherd-core::ports`. It describes a capability in domain terms, such as
`Worktrees::create(&RepoRef, &BranchSpec)`, not `git_worktree_add(args)`. Each port comes with:

1. the trait and its domain types (in core)
2. a fake in `shepherd-testkit::fakes` (stateful, in-memory, and able to inject failures)
3. a contract suite in `shepherd-testkit::contracts`: one `pub fn` per case, plus a `<port>_contract!(make)` macro that
   expands to one `#[test]` per case (template: `contracts/pty.rs`)
4. a real adapter in `shepherd-adapters`, whose tests call the same contract suite
5. wiring in `shepherd/src/main.rs`

See the `new-port` skill. A port isn't done until all five exist.

Conventions for new ports: prefer an error enum **per operation** over one per port, so callers only handle what that call
can return. Never derive `Deserialize` on a type that validates in its constructor; use `#[serde(try_from = ...)]`.

The list of ports is created during planning and kept in `docs/plan.md`.

## Test layers

| layer         | where                                   | drives                    | catches                         |
|---------------|-----------------------------------------|---------------------------|---------------------------------|
| core unit     | `crates/core` `#[cfg(test)]` / `tests/` | `App::handle`             | domain logic regressions        |
| render        | `crates/tui/tests`                      | `render` + `TestBackend`  | UI regressions (insta snapshots)|
| contract      | `crates/testkit` + `crates/adapters`    | port trait                | fake/real drift, adapter bugs   |
| scenario      | `crates/shepherd/tests`                 | runtime + all fakes       | wiring, multi-step user flows   |

Most tests should be core unit tests. Scenario tests cover user flows from start to finish, e.g. "open workspace →
new session → agent goes idle → sidebar shows idle". Write one scenario per user-visible feature, not one per edge case.

## Testing conventions

- Name tests after behavior: `new_session_creates_worktree_from_default_branch`, not `test_handle_3`.
- Use the builders in testkit (`AppBuilder`, `SessionBuilder`) instead of constructing big structs inline,
  so a field change doesn't break 40 tests.
- Assert on outcomes the user would notice: commands emitted, state visible through public accessors, rendered output.
- No `#[ignore]`. No `sleep`. No real network. Real-adapter tests use `tempfile`.

## Spike T36: host hot-swap

Findings of the T36 spike (2026-10-30). This is not a decision yet: the PM records one (proposed text at the end of this
section). The prototypes were throwaway C and Python programs. They ran on macOS (Darwin 24.6, arm64, cargo 1.96) in a
scratch directory with a throwaway `HOME`. Linux claims come from documented behavior and were **not** run. Citations
are `file:line` in this repo, and `herdr:file:line` in herdr at commit `ee5bfb5`.

### What happens today

- Besides the machine and cwd, the hello carries only `version: PROTOCOL_VERSION`. That is still `1`, the value it had when the wire landed
  (`crates/core/src/wire/message.rs:36-37`, `Hello` at `:158-165`, `HelloOk` at `:366-379`). `negotiate` only checks
  that the versions are equal (`:579-593`). Nothing identifies the build, so a host from before T21 accepts a newer
  client, and the first new method fails with `-32601 method not found` (`crates/core/src/wire/error.rs:26-27`).
  Prototype (c) reproduced this against today's binary: hello v1 is accepted, an unknown method gets `-32601`, hello
  v2 gets `1000 protocol version mismatch`, and a second `shepherd host` fails with "already running" because of the
  log lock (`crates/shepherd/src/host/mod.rs:138`, `:172-174`).
- Bumping `PROTOCOL_VERSION` alone makes things worse: the client treats any hello error as fatal and quits ("the
  shepherd host refused us", `crates/core/src/client.rs:681`, `:631-634`).
- Today, updating the host is option (c). Shutdown kills every PTY (`start_shutdown`,
  `crates/shepherd/src/host/runtime.rs:1541-1551`; `PortablePty::drop`, `crates/adapters/src/pty.rs:173-184`). A new
  host marks every session that hasn't exited as exited (`runtime.rs:348-363`) and sweeps every run directory
  (`runtime.rs:365-366`).
- What a swap has to carry. Host state is a fold of the logs, and `start` rebuilds it (`runtime.rs:312-317`). The
  runtime also keeps state that isn't a fold (`runtime.rs:5-33`, struct at `:250-286`):
  - screens
  - per-conn subscriptions and replies
  - worker calls in flight (`worktree_calls`, `hook_calls`, `:255-258`)
  - pending and awaiting run files (`:277-279`)
  - sync-update deadlines
  - the quiet watch, which is rebuilt from the log at start (`:313-316`)
  - translators and the hook seed (`:273-275`). Both translation types already derive `Serialize`/`Deserialize`
    (`crates/core/src/agents/claude.rs:68-71`, `crates/core/src/agents/codex.rs:179-182`). `Translators` itself does
    not (`crates/core/src/agents.rs:43-56`).

### Prior art: herdr's live handoff

- **Opt-in, not automatic.** The user runs `herdr update --handoff` (`herdr:src/update.rs:1155-1164`). Without the
  flag, running servers are left alone (`:1233-1241`). The capability is advertised as `live_handoff: cfg!(unix)`
  (`herdr:src/platform/mod.rs:308-320`) and checked from the runtime status (`herdr:src/update.rs:896-901`).
- **Mechanism (b).** The old server spawns the updated exe as `server --handoff-import <socket> <token>`, detached
  (`herdr:src/server/handoff.rs:69-112`), on a private one-shot socket (`:64-66`, `:141-148`). The exchange runs:
  1. token
  2. JSON manifest: version, source and expected version and protocol, a layout snapshot, and per pane the child pid,
     size, input modes, title and history ANSI (`:36-54`, `herdr:src/handoff_runtime.rs:11-32`)
  3. `validated`
  4. fds over `SCM_RIGHTS` in batches of 64 (`handoff.rs:25-29`, `:386-426`)
  5. `restored`, then `ready`, then `committed`, then `owned` (`:182-230`, `:279-305`)

  The importer rejects a mismatched handoff, protocol or version before it takes any fd (`:233-277`).
- **Old side** (`herdr:src/server/headless/lifecycle.rs:25-225`):
  1. disconnects every client (`:57-59`)
  2. pauses each PTY reader, which first drains queued writes (`:61-70`; `herdr:src/pty/actor/unix.rs:250-290`,
     `:737-780`)
  3. captures state and spawns the importer
  4. validates and sends the fds
  5. removes its public sockets (`:176-181`)
  6. waits for `ready` and commits
  7. only then marks its runtimes "preserve processes on drop" (`:215-222`; `herdr:src/pane.rs:2254-2266`)

  Any failure before the commit rolls back: readers resume and the sockets are restored (`:182-213`, `:275-288`).
- **New side** (`herdr:src/server/headless/bootstrap.rs:138-217`). It rebuilds panes from the fds and keeps the child
  pid only as a number (`herdr:src/pane.rs:2502-2560`). It polls until the old public sockets stop answering, then
  **binds fresh ones** (`bootstrap.rs:179-186`, `lifecycle.rs:386-403`), so there is a window in which connects fail.
  It nudges each child to redraw on the first attach (`bootstrap.rs:204`, `lifecycle.rs:290-299`,
  `herdr:src/pty/actor/unix.rs:228-247`). It carries terminal history only for panes without an agent session
  (`lifecycle.rs:98-100`).
- **Failure UX.** herdr checks which server is now answering (`herdr:src/update.rs:1427-1438`), then asks before it
  stops the old server, defaulting to No (`:1442-1490`). It never kills panes without asking.
- **Install.** The new binary is renamed over the old one (`herdr:src/update.rs:730-745`). The wire protocol is
  checked for strict equality (`herdr:src/protocol/wire.rs:20`, `:1749-1767`).
- **What to take:** the version check before any fd moves, quiescing readers, the redraw nudge, asking before killing,
  and rollback where possible. **What to avoid:** the rebind gap (a shepherd hook would lose its frame, and a client
  would launch a second host, `client.rs:604-608`), and losing the agent as a child (exit codes).

### Prototype results

| prototype | what it did | result |
|---|---|---|
| (a) re-exec | v1 holds a PTY master and slave (agent: `sh`), a listening unix socket and a `flock`. A v2 build is renamed over its path. v1 clears `FD_CLOEXEC` on those four fds, writes a handoff file and `execv`s the path. | Same pid after exec. All 4 fds are open in v2. The lock was never free: another process's `LOCK_NB` fails both before and after the exec. Output the agent wrote that v1 never read is read by v2. Input still echoes. A hook-like client that connected, sent a line and hung up **before any accept** was accepted and read by v2, because the listen backlog survives exec. `waitpid(agent)` in v2 returned exit code 7: **the agent is still our child.** |
| (a) in-place overwrite | Same, but the binary is overwritten with `cp`, so it keeps the same inode. | The exec'd image is SIGKILLed (`Killed: 9`), its PTYs close and the agent dies. A fresh spawn of that file is SIGKILLed too (exit 137), while the old process keeps running. **A preflight spawn catches this before the exec.** |
| replace | `cargo build` and `cargo install --force` rebuild a binary while a copy of it runs. | Both replace it with a **new inode**. The running copies keep running. |
| (b) SCM_RIGHTS | The old process passes the same four fds to a `setsid`'d second process, then exits. | The fds work, and the lock survives the old process exiting. **But** an explicit `flock(LOCK_UN)` in the old process before it exits (what `JsonlEventLog::drop` does, `crates/adapters/src/event_log.rs:127-133`) unlocks the new host's lock too, because both hold one open file description. The new host can't `waitpid` the agent (`ECHILD`). kqueue `NOTE_EXIT` fires for a non-child, but without the exit status (`NOTE_EXITSTATUS` unset, data 0), so exit codes become `None`. |
| (c) restart | Today's binary, a throwaway `HOME`, a Python probe. | See "What happens today". |

Other facts the prototypes confirmed:
- On macOS, a dying child can block in `exit` until its tty output is read. This is the same effect noted in
  `pty.rs:222-223`, so an adopted pump must keep draining.
- `File::try_lock` is `flock(2)` on Linux and Apple (std 1.96, `library/std/src/sys/fs/unix.rs:1530-1543`). A lock
  belongs to the open file description, so it survives an exec when `CLOEXEC` is cleared, and it is shared with any
  fd received over `SCM_RIGHTS`.

### What breaks, and the answer for each

- **Exe replaced on disk.**
  - macOS: a rename is safe. An in-place overwrite gets every new exec of that file SIGKILLed (prototypes).
  - Linux (documented, not run): writing in place to a running exe fails with `ETXTBSY`, and a rename is fine. After
    a replace, `current_exe()` (`/proc/self/exe`) reads `<path> (deleted)`.
  - The swap must therefore exec the path recorded at startup (`settings.shepherd`, `host/mod.rs:151-152`, `:155-161`),
    never `current_exe()` called at swap time.
  - A preflight spawn rejects a binary that can't run.
- **Executing an unverified binary.** `main.rs` ignores extra arguments after `host` (`crates/shepherd/src/main.rs:8`).
  A pre-T36b binary exec'd as `host --adopt …` would start a fresh host, mark every session exited and leak the
  inherited masters, which leaves the agents hung. So the preflight must use a **new subcommand**:
  `shepherd handoff-check`. Old binaries answer it with "unknown command" and exit 1 (`main.rs:14`), which counts as
  "can't hand off".
- **Log lock.** The lock fd is carried across the exec with `CLOEXEC` cleared. Exec doesn't run `Drop`, so the lock is
  never released. The runtime must never drop the `JsonlEventLog` before the exec: its `Drop` unlocks the shared
  description (shown in the (b) prototype). The new image adopts the fd and calls `try_lock` on it again, which is a
  no-op on the same description. At no point can a racing `shepherd host` (launched by another client's reconnect)
  take the lock and declare every session dead (`runtime.rs:348-363`).
- **Listening socket.** The listening socket fd is carried and the socket file is never unlinked. The link's `Drop`
  would unlink it (`crates/adapters/src/link.rs:170-185`), but exec skips `Drop`. Clients and hooks that connect
  during the exec wait in the backlog (shown in the prototype), so no connect fails and no client launches a second
  host.
- **Half-written frames.** Client conns are `CLOEXEC`, so the exec closes them.
  - The link reader only emits whole lines (`link.rs:468-499`, `crates/core/src/wire/codec.rs:15-20`), so a client
    never acts on a torn line.
  - The client reconnects from `Ready` (`client.rs:618-627`) and subscribes again from its last seqs (`:749-760`):
    no gap and no duplicate (decision 0013).
  - Before the exec, the runtime still stops sending and closes the conns itself.
  - A hook frame on an already-accepted conn is read and appended while the runtime drains its channel. What's left
    is a tiny race in which one hook frame is lost. `shepherd hook` still exits 0 (`crates/shepherd/src/hook.rs:5-6`),
    and the next hook corrects the status. Accepted.
- **PTY output in flight.** Pumps are paused, and each acks before the runtime drains its channel into the screens.
  Output still in the kernel buffer survives the exec (shown in the prototype).
- **Screens.** Each session's `snapshot_ansi` is carried. It includes an unfinished escape sequence and an open sync
  hold (`crates/term/src/lib.rs:213-247`, which also lists the gaps: scrollback, the primary screen under the alt
  screen, and so on). The new image feeds it into `Screen::new(cols, rows)`, then nudges a redraw with a same-size
  resize, as herdr does.
- **Work in flight.** The swap waits for worker calls, run-file writes and closes to finish (Busy) instead of carrying
  them.
- **Translators.** Translators are carried serialized. A translator the new build can't parse is dropped. A fresh one
  gets a new seed, so ids stay unique (`agents.rs:43-50`).
- **A handoff that fails midway.**
  - *Before the exec* (busy, preflight fails, the handoff file can't be written, or `execv` returns an error): nothing
    has changed. The old image restores `CLOEXEC`, resumes its pumps and keeps serving, and the client says why.
  - *After the exec*, if the new image can't adopt a session, it kills that session's process group (as
    `pty.rs:308-314` does) and appends its exit. If the new image dies outright, the masters close and the agents get
    SIGHUP. The next client launches a host, which records their exits (`runtime.rs:348-363`).
  - The log is intact in every case. Appends are whole lines written only on the runtime thread (`runtime.rs:17`,
    decision 0009), and the swap runs on that thread between appends. This is the one case where sessions die without
    being asked about, and the preflight limits it to bugs in the adopt code.
- **Downgrade and two clients of different builds.** The swap target is always *the binary now at the host's own exe
  path*.
  - An older client can't pull the host back. It is told it's the odd one out.
  - A real downgrade (an older binary installed at that path) is an ordinary swap, gated by the preflight. An older
    binary without handoff support fails the preflight and falls back to (c), with consent.
  - Because the target is never a client's binary, two clients can't ping-pong the host.

### Options compared

| | (a) re-exec in place | (b) second host + SCM_RIGHTS | (c) restart |
|---|---|---|---|
| sessions survive | yes | yes | no |
| agent stays our child (waitpid, exit codes, pump unchanged) | yes, same pid | no: `ECHILD`, and kqueue/pidfd give no exit status | n/a |
| rollback if the new binary fails | preflight before exec; none after | full, until commit | n/a |
| socket gap (hooks lost, client relaunches) | none, the listener fd is carried | none if the listener fd is passed; herdr rebinds, so it has a gap | yes |
| log lock | carried, never free | shared description: the old host must not unlock, and two runtimes overlap | released, then retaken |
| moving parts | one process, one handoff file, `execv` | two live runtimes, a 5-step commit protocol, a detached spawn, per-OS exit watching | exists today |

### Recommendation

**(a) re-exec in place, gated by a preflight, with (c) as the fallback the user consents to. Don't build (b).**

(a) keeps the agents as our children, so `waitpid` and exit codes keep working, and `pty.rs` keeps its reaping model.
It has no socket gap and no window on the lock, and it adds the fewest states. Its weakness is that it can't roll back
after the exec. The preflight covers the real failures seen here: a binary overwritten in place, an old binary, an
incompatible handoff format. An `execv` that fails returns to the intact old image. (b)'s rollback costs exit codes,
two overlapping runtimes and per-OS process watching. That price is too high for a local daemon whose log already
survives losing every PTY.

**Mismatch detection: what the hello carries.**
- `BuildId { version: String, digest: [u8; 32] }`:
  - `version` is `CARGO_PKG_VERSION`, for display only.
  - `digest` is the sha256 of the process's own executable, read once at startup.
- Why not a git hash: the workspace version is fixed (`Cargo.toml:5-6`), and a dirty rebuild keeps the commit, which
  is exactly the "rebuilding" case.
- Why not path plus mtime/inode: a client and host started from different paths (the installed binary and
  `target/debug`) can't be compared that way. The digest is exact and doesn't depend on the path.
- Where it's computed: in the `shepherd` crate (IO), then passed to core as data. Digests are compared as bytes.
- `Hello.build`, `HelloOk.build` (running) and `HelloOk.installed` (the digest of the file now at the host's startup
  exe path) are all additive serde-default fields. The host recomputes `installed` only when that file's
  `(dev, ino, size, mtime)` changes.
- A `HelloOk` without `build` means the host predates T36a.
- `PROTOCOL_VERSION` stays the hard compatibility gate, but a mismatch no longer quits the client.

**Who decides.** The client's pure core decides, from the hello answer. The host never swaps unless asked, because a
swap disconnects every client and the user should see it happen.

| case | meaning | client does |
|---|---|---|
| host `build` == client `build` | same build | nothing |
| host has no `build`, or version mismatch | host older than T36a | dialog: "The shepherd host is older and can't update itself. Restart it? N running sessions will stop." [Restart host] [Not now] |
| host `installed` == client ≠ host `build` | host is stale | T36a: same restart dialog. T36b: `_shepherd/host/upgrade`, "host is older — updating…", then "host updated" once the reconnect hello matches |
| host `build` == host `installed` ≠ client | this client is the odd one out | notice: "this window runs a different shepherd build than the host; restart it to use the installed one". No swap |
| all three differ | e.g. client from `target/debug`, host from `~/.cargo/bin` | notice only |

"Restart host" (c) has to work against hosts older than T36a, which have no shutdown method. The client reads the
host's pid from the connected socket's peer credentials (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux) and sends it
SIGTERM. The host shuts down cleanly (`host/mod.rs:241-259`). The existing reconnect loop then launches the client's
binary (`client.rs:604-608`). A `-32601` from an old host turns into the notice "the shepherd host is older than this
client: restart it to use this".

**Swap sequence (T36b).**
1. The host answers `_shepherd/host/upgrade` with one of:
   - `NothingToDo`: installed == running.
   - `Busy { reason }`: worker calls, run files or closes are pending.
   - `Unavailable { reason }`: the preflight failed.
   - `Accepted`, and then it swaps.

   The decision is a pure function.
2. Preflight: run `<settings.shepherd> handoff-check` as a bounded child process. It prints
   `{build, handoff_versions}`, or it fails: "unknown command", or killed by SIGKILL ("overwritten in place; reinstall
   it").
3. Quiesce: pause every pump (new `Control::Pause`, acked), drain the runtime channel through `handle`, close every
   conn.
4. Write `<root>/run/handoff-<pid>.json` (0600). It holds the handoff version and the source build. For each session
   it holds the session id, pid, master and slave fds, size, `snapshot_ansi` (base64) and the translator. It also
   holds the listener and lock fds.
5. Clear `CLOEXEC` on exactly those fds, then `execv(settings.shepherd, ["shepherd", "host", "--adopt", file])`. If
   `execv` returns, undo step 5, resume the pumps and keep serving.
6. The new image reads and then deletes the file, and adopts in this order: lock, listener, PTYs (before folding the
   logs, so the agents' output keeps draining). Then it starts the runtime with the adopted set: no orphan exits for
   those sessions, no sweep of their run directories, translators restored. Last, it nudges a redraw.

**Ports.** This needs a port change, a PM call. Proposal: a new core trait `PtyHandoff` with an associated
serializable `Token`:
- `export(&mut self) -> Result<Vec<(SessionId, Token)>, _>` quiesces and returns the tokens.
- `resume(&mut self)` undoes a failed export.
- `adopt(sink, tokens)` is a constructor on the adapter.
- The fake's token is its in-memory session. The real token is `(pid, master fd, slave fd, size)`.
- portable-pty can't wrap an existing fd: `UnixMasterPty` is private (portable-pty 0.9.0 `src/unix.rs:303-308`). So
  adopted sessions use the raw fds directly: read and write, `TIOCSWINSZ`, `waitpid`.
- `JsonlEventLog::adopt(root, lock_fd)` and `UnixListenLink::adopt(path, listener_fd)` are concrete adapter
  constructors, not trait changes.

### Follow-up tasks (for `docs/features/`)

#### T36a: Build identity in the hello, and a clear mismatch UX
status: todo
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

#### T36b: Hot-swap the host by re-exec, keeping sessions running
status: todo
depends: T36a
touches: ports::pty (new `PtyHandoff` port, needs PM approval: new-port skill), adapters::{pty, event_log, link},
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

### Proposed decision (for the PM to record)

`### 0018: The host updates itself by re-exec in place`. Context: spike T36. Decision: hellos carry a `BuildId` (exe
digest). The client decides from the hello. The host re-execs the binary at its startup path, carrying the PTY, lock
and listener fds and a handoff file, and only after a `handoff-check` preflight passes. A restart that kills sessions
happens only with the user's click. Consequences: one new port (`PtyHandoff`). A handoff format that is versioned and
read by older and newer builds. Agents survive host updates on unix.

## Spike T28: iroh peering

Findings of the T28 spike (2026-10-30). This is not a decision yet: the PM records one (proposed text at the end). The
prototype was a throwaway crate in the session scratchpad, never in the repo. It ran on one Mac (Darwin 24.6, arm64,
14 cores, rustc 1.96), with its own `CARGO_HOME` and identity keys in fresh tempdirs. It depended on this repo's
`shepherd-core` (real `wire` messages, the real `Link` trait) and `shepherd-adapters` (the unix-link baseline).

**Versions and API.** iroh **1.4.1** (crates.io, 2026-10-21; MSRV 1.91; QUIC stack `noq` 1.4.0), built with
`default-features = false, features = ["tls-ring", "fast-apple-datapath"]`, which drops `portmapper` and `metrics`.
iroh-tickets **1.0.0** for the pairing ticket. iroh's own normal dependency tree is about 240 crates: tokio, rustls/ring,
reqwest, hickory. Sources: <https://docs.rs/iroh/1.4.1/iroh/> and <https://github.com/n0-computer/iroh> (tag
`iroh-v1.4.1`). The API used:
- `Endpoint::builder(presets::N0)`, then `.secret_key(..)`, `.alpns(..)`, `.hooks(..)`, `.clear_address_lookup()` and
  `.clear_ip_transports()` (the last one is relay-only mode), then `.bind()` and `.online()`.
- `Endpoint::connect(EndpointAddr, alpn)` and `Endpoint::accept()`, then `Incoming::accept()`.
- `Connection::{open_bi, accept_bi, remote_id, alpn, paths, close}`.
- `EndpointHooks::after_handshake` returning `AfterHandshakeOutcome::{Accept, Reject}`.
- `SecretKey::{generate, to_bytes, from_bytes}`.
- `iroh_tickets::Ticket`.

`.clear_address_lookup()` was used only because of the spike's network constraint, which allowed n0's relays and
localhost and nothing else. It means nothing was published to, or resolved from, n0's pkarr/DNS service (`iroh.link`).

### What the prototype is

- `IrohLink` implements `shepherd_core::ports::Link` unchanged: `send`, `close`, `queued_bytes`, and events into a
  `LinkEventSink`.
  - One tokio **current-thread** runtime runs on its own thread, and the caller stays synchronous (decision 0011).
  - One iroh connection is one `ConnId`. Each conn carries one bidirectional QUIC stream, opened by the dialler, framed
    with `wire::Decoder` like the unix adapter.
  - Each conn has a writer task that drains an unbounded queue, with the same `MAX_QUEUED` slow-consumer rule, and a
    reader task that is the only one emitting that conn's `Line`s and its one `Disconnected`.
- A "host" process runs the shepherd host loop in miniature. It answers every `_shepherd/session/input` with a
  `_shepherd/pty/output` echo of the same bytes, standing in for a PTY that echoes.
- A "bench" process sends one-byte `session/input` keystrokes through `Link::send` and times each one until its echo
  arrives as a `LinkEvent::Line`. The timer runs on the caller's thread, so each round trip includes four runtime
  thread hops and JSON encoding and decoding on both sides.
- Pairing, the allowlist hook, identity files and a `peers.json` are described below.

### Results

Everything below worked on the first run unless it says otherwise. The run scripts are `run.sh`, `gaps.sh` and
`dupkey.sh` in the scratchpad.

| check | result |
|---|---|
| Two hosts exchange wire messages over a relay | Yes. `bench --relay-only` → host: every sample's selected path was `Relay(https://use1-1.relay.n0.iroh.link./)`, and `Connection::paths()` showed no IP path. |
| …and directly | Yes. With IP transports on both sides, the selected path was `Ip(192.168.x.x)` (sometimes the Tailscale `100.x` address). |
| Identity persists | `identity.key` is 32 bytes, mode 0600, created with `O_EXCL`. A host restarted from the same dir has the same node ID. |
| Pairing | `join <ticket>` added each side to the other's `peers.json`. The ticket is 256 characters: it carries 7 IP addresses plus the relay URL. |
| Ticket replay | A second `join` with the same ticket, from a fresh key, was refused: `closed by peer: no pairing in progress (code 410)`. A wrong token is refused with code 403 by the same code path, but that wasn't exercised separately. |
| Untrusted node | A fresh key that knows the host's address but isn't in the allowlist was rejected in `after_handshake` (host log: `rejected untrusted node …`). The host emitted no `Connected`. The dialler saw `Disconnected(PeerGone)`. |
| Known peer found on start | The host was restarted relay-only, with the same identity and `peers.json`. B reconnected using its stored `last_addr` (the relay URL is unchanged) in 29 ms. |
| `close` semantics | The closing end saw `Disconnected(Closed)` and the peer saw `PeerGone`, as the port requires. |
| Endpoint start | `online()` (home relay up) took 3.0–3.2 s with IP transports, and 1.35–1.47 s relay-only. |
| Connect (stored address, handshake + stream) | 24–56 ms, once 135–189 ms. Relay-only and direct dials take about the same time: both are about one relay round trip. |
| Direct upgrade | Usually 29–300 ms after connecting, or direct from the first packet when the stored address had the IP. It took 2.2 s once, with debug logging on. **Once, it stayed on the relay for the whole 20 s wait.** That run's host had already served a pairing and a rejected connection. Not reproduced in about 10 later direct runs; cause unknown. |
| Two endpoints, same key | B's key online twice at once, both relay-only, both talking to A. One connection saw **one 8.8 s stall** (max sample 8776 ms, otherwise normal). See "Where the endpoint lives". |

**Keystroke round trips** (ms, `session/input` → `pty/output` echo, through the `Link` port on both ends):

| path | gap between keys | n | min | p50 | p90 | p99 |
|---|---|---|---|---|---|---|
| unix socket (today's adapter, baseline) | 30 ms | 300 | 0.07 | 0.18 | 12.9 | 38.0 |
| iroh direct (loopback/LAN interface) | 0 | 300 | 0.13 | 0.21 | 0.32 | 0.98 |
| iroh direct | 5 ms | 200 | 0.21 | 0.37 | 0.50 | 0.69 |
| iroh direct | 30 ms | 300 | 0.20 | 0.53 | 9.0 | 76 |
| iroh relay (use1, n0) | 0 | 300 | 22.7 | 27.9 | 65 | 140 |
| iroh relay | 5 ms | 200 | 21.3 | 25.9 | 40 | 123 |
| iroh relay | 30 ms | 300 | 21.0 | 27.5 | 112 | 161 |
| iroh relay (host relay-only, client direct-capable) | 30 ms | 300 | 22.4 | 26.5 | 109 | 146 |
| iroh relay | 100 ms | 200 | 22.6 | 30.4 | 46 | 130 |

How these were measured, and the caveats:
- `Instant` was taken around each keystroke on the bench's main thread. Percentiles are nearest-rank over the sorted
  samples. The gap is a `thread::sleep` between keys.
- The machine was **heavily loaded** by other agents' builds: load average 28–94 on 14 cores, read with `uptime` at the
  start and end of each run. The tails (p90/p99) track load: even the unix baseline has a 13 ms p90. **Read the
  medians, not the tails.** One "direct, 100 ms gap" run had a 10 ms median, which is load noise, not iroh.
- `ping use1-1.relay.n0.iroh.link` gave min/avg 11.6/28.0 ms. A relayed keystroke crosses the relay twice
  (client→relay→host and back), so the relay floor is about 2× the relay RTT. The measured minimum of 21–23 ms matches
  that.
- About 10% of relayed keystrokes take about 100–120 ms when typing at a 30 ms pace. This showed up in every gapped
  relay run; at 0 or 5 ms gaps it moves to p99. iroh-relay sets `TCP_NODELAY` on both ends (iroh-relay 1.4.1
  `client/tls.rs:293`, `server/streams.rs:221`), so Nagle isn't the cause. Not isolated.

**Bottom line:**
- A direct connection costs about what the unix socket costs (sub-millisecond medians).
- Through n0's relay a keystroke echo costs about 26–30 ms median from this location. That is usable for typing, but
  it is noticeable, with occasional spikes of about 100+ ms.
- Most real pairs (LAN, or punchable NAT) should end up direct within about 0.1–0.3 s.

### Recommended design

**Where the endpoint lives: the host owns the machine's only iroh endpoint.**
- A machine has one identity, and iroh routes by node ID. Two endpoints online with the same key make the relay flip
  between them: the spike saw an 8.8 s stall. So a TUI must never bind a second endpoint with the machine key.
- The host is long-lived, so it is what peers can reach.
- The client stays tokio-free and unix-only.
- A client reaches a remote host **through its local host**. It opens a unix conn and sends one first request,
  `_shepherd/tunnel { machine }`. The local host opens (or reuses) an iroh conn to that peer and then pipes lines both
  ways verbatim.
- The remote host sees an ordinary client conn from a trusted node and runs the normal hello. The wire protocol stays
  end to end, client ↔ remote host, so T31's routing, subscribe and catch-up logic is unchanged.
- The extra local hop costs about what the unix baseline costs (0.2 ms median), against 26 ms for a relay.
- What the tunnel needs: read pausing for flow control. The local host stops reading the iroh stream while the unix
  conn's `queued_bytes` is above a high-water mark, so QUIC flow control pushes back. The remote host's 0013 pacing then
  sees the backlog in its own `queued_bytes`. The splice is plumbing, so it belongs in the adapter or runtime, not core.
- The alternative is to give each client its own key and an iroh `Link` in the client (T31 as written today). That
  needs a second trust relationship per client and tokio in the client process. Not recommended.

**Node identity storage.**
- `~/.shepherd/identity.key`: 32 raw ed25519 secret-key bytes, mode 0600, created with `O_EXCL` inside the 0700 root
  (decision 0010). The key is generated by the runtime, never by core.
- The node ID is its public key. `MachineId` (T29) is the node ID's canonical lowercase hex, iroh's `Display`, 64
  characters. Core keeps treating it as opaque.
- Trust checks compare `EndpointId`s as typed values in the adapter, never strings. If core ever needs to compare node
  IDs itself, add a `NodeId([u8; 32])` newtype and an encoding crate to `CORE_ALLOWED` (a PM call).
- A reinstalled machine has a new identity and must pair again. That's acceptable, and it's also what revoking a lost
  machine needs.

**Pairing (`shepherd pair` / `shepherd join <ticket>`).**
- `shepherd pair` asks the local host for a ticket: `_shepherd/pair/start`.
  - The runtime generates a 16-byte token, which reaches core as an event field.
  - Core records one pending pairing: single use, expires after 10 minutes.
  - The answer is a `PairTicket { addr: EndpointAddr, name, token }`, encoded with `iroh_tickets::Ticket`, kind
    `shepherd`, as postcard + base32.
- The host accepts the pair ALPN (`shepherd/pair/0`) only while a pairing is pending.
- `shepherd join <ticket>` asks the local host to dial the inviter on the pair ALPN and send
  `{token, name, addr}`. TLS authenticates both node IDs.
- On a matching token, the inviter appends `_shepherd/peer/trusted {node, name}` and answers with its name. The
  joiner appends the same frame for the inviter.
- A pending pairing is consumed by the first attempt, right or wrong, so a ticket can't be brute-forced. Compare
  tokens in constant time (`subtle` or ring's `constant_time`).
- To shorten the ticket, carry only the relay URL plus at most the LAN addresses (it's 256 characters with 7
  addresses). Direct addresses still help a LAN pair go direct immediately.

**Trust.**
- The allowlist is the set of `peer/trusted` node IDs folded from the host log (T30). `_shepherd/peer/removed`
  revokes one.
- Core owns the set and pushes it to the adapter as data. The iroh hook rejects any other node in `after_handshake`,
  before any `Connected`, so untrusted peers never reach core.
- This keeps policy (who is trusted) in core and leaves the adapter a mechanical membership test, the same split as
  `MAX_QUEUED`.

**Discovery of known peers on start.**
- Bind with `presets::N0`, which keeps address lookup on (pkarr publish plus DNS resolve). Its publisher defaults to
  `AddrFilter::relay_only`, so no IP addresses leak to n0.
- Also seed iroh's `address_lookup::MemoryLookup` with each peer's last address that worked. Persist those hints in a
  small non-authoritative cache (`~/.shepherd/peer-addrs.json`), not as log frames.
- Then dial trusted peers by node ID when a client tunnels to them, and reconnect with backoff.
- The spike verified the stored-address route. **The pkarr/DNS route was not exercised**: the network constraint
  excluded `iroh.link`. It is what lets a peer whose home relay changed still be found. T30 should verify it.

### Proposed `Link` adaptation

- **The `Link` trait needs no change for transport.** `IrohLink` meets the port's semantics as written: order per conn,
  one `Disconnected` after all lines, non-blocking `send`, `MAX_QUEUED`, `close` flushing, `queued_bytes`. It must pass
  the T10 contract suite in T31. The spike didn't run the contract.
- **Additive, PM call: `fn peer(&self, conn: ConnId) -> Option<PeerId>`**, in the style of `queued_bytes`. It says which
  authenticated node a host-side conn is from. The unix adapter returns `None` (the local user, by file permissions).
  Core uses it to stamp who is connected, and to let `_shepherd/pair/*` and `_shepherd/tunnel` through only from local
  conns.
- **Additive, PM call: a distinct disconnect reason for a refused peer**, e.g. `DisconnectReason::Refused`. Today a
  rejected dialler sees `PeerGone`, so the UI can't say "this machine doesn't trust you". The iroh close code (403) is
  available to the adapter.
- **Runtime:** tokio stays inside `shepherd-adapters`, on one current-thread runtime thread per process. That was
  enough for every run here. Constructors that dial (`connect`) block only the calling thread, while it waits on a
  oneshot. The runtime never blocks.
- **Unmeasured, and to test in T31:** throughput and 0013 catch-up of a multi-megabyte history over a relay, QUIC
  stream flow-control windows against `MAX_LINE` (16 MiB), roaming/network change, two real machines behind real NATs,
  and Linux.

### Proposed decision (for the PM to record)

`### 0019: Machines peer over iroh; the host owns the endpoint`. Context: spike T28. Decision:
- Each host binds one iroh endpoint (iroh 1.x, `presets::N0`, n0's public relays) with the machine identity in
  `~/.shepherd/identity.key`. `MachineId` is its node ID.
- Machines pair with a single-use ticket (`shepherd pair` / `shepherd join`). Trust is the allowlist of node IDs folded
  from `_shepherd/peer/trusted` frames, enforced in an iroh handshake hook.
- Clients reach remote hosts through a tunnel in their local host, never with their own endpoint.

Consequences:
- tokio enters `shepherd-adapters`, on one thread (amends 0011).
- `Link` gains `peer()` and possibly a `Refused` reason.
- T31 changes from "iroh Link in the client" to "tunnel via the local host".
- Relayed typing costs about 26–30 ms per echo; direct costs about the same as the unix socket.

## Decision log

Append-only. Format: `### NNNN: title (YYYY-MM-DD)`, then context, decision, consequences.

### 0001: Functional core / ports and adapters (2026-10-29)
Context: v1 mixed terminal emulation, IO and logic, which made it untestable and full of regressions. The
project is now built by agents working in parallel, so behavior has to be checkable mechanically.
Decision: use the shape described above, with the crate layout and dependency rules enforced by the gate.
Consequences: more crates and traits up front. Each feature costs a little more boilerplate, and in exchange
core logic can be tested without IO.

### 0002: ACP++ append-only log is the source of truth (2026-10-29)
Context: we need agent events from many sources (ACP wire, Claude Code hooks, Codex hooks, shepherd itself), and
an event model we can extend without migrations.
Decision: use ACP++ as specified in `docs/acpp.md`. Host and session state are folds over the logs. `serde_json`
joins `CORE_ALLOWED`.
Consequences: replay, debugging, triggers and sync all build on the log. Folds must stay backward-compatible
with old logs forever.

### 0003: Host daemon + TUI client from day one (2026-10-29)
Context: agents must outlive the TUI, and the manifesto wants remote machines to work exactly like local ones.
Decision: split into a per-machine host and a client that talks to it over JSON-RPC. The local transport is a
unix socket. The remote transport (M5) is iroh, using the same messages.
Consequences: M1 costs more. M5 becomes "add a transport" instead of a rewrite.

### 0004: v1 hosts native agent TUIs in PTYs (2026-10-29)
Context: users want the real Claude Code / Codex TUI. Hosting agents over ACP would mean building our own chat UI.
Decision: v1 runs the agent CLIs in PTYs and gets events from their hooks, which `shepherd hook` translates into
synthesized ACP frames. ACP-hosted sessions (shepherd as ACP client) come later and write real ACP frames into
the same log.
Consequences: status fidelity is limited by the hooks. The log format doesn't change when ACP hosting arrives.

### 0005: Async shell on tokio (2026-10-29)
Context: the ACP Rust SDK, iroh and process and socket IO are all async.
Decision: runtimes and adapters use tokio. Core stays synchronous and pure. Port traits are async only where
the capability is inherently streaming or slow, decided per port in its task.

### 0006: Vendor a lossless ACP subset instead of depending on the schema crate (2026-10-29)
Context: T01 (`docs/spikes/T01-acp-types.md`). `agent-client-protocol-schema` 1.13.0 is pure, but it drops unknown
fields and fails to parse unknown `sessionUpdate`/`stopReason` values. It would also add 22 crates to core and it
releases weekly.
Decision: `core::acpp::acp` holds a hand-written typed subset: the methods and variants listed in the spike's §5,
with open enums (`Other(String)`), unknown fields preserved, and responses typed by correlating them with their
request `id`. Pinned to ACP protocol v1 / schema 1.13.0. The schema crate may be used as a core dev-dependency
for conformance tests only.
Consequences: we maintain about 300 lines of types. We get losslessness and a fold that never panics, and core
keeps its tiny dependency set.

### 0007: `alacritty_terminal` emulator in a `shepherd-term` crate, run by both host and client (2026-10-29)
Context: T02 (`docs/spikes/T02-terminal-emulator.md`). All candidates draw real Claude and Codex output the same way.
alacritty_terminal wins on the edge cases: query replies, synchronized output, reflow, visible input modes, and 33
crates against about 200. Agents query the terminal at startup and block until it answers, so something next to the PTY
must answer even when no client is attached. A client attaching to a running session also needs the current screen.
Decision: a new crate, `shepherd-term`, wraps `alacritty_terminal` (`default-features = false`). It exposes a
`Screen` with `feed`, `resize`, `take_replies`, `modes` (keyboard, mouse, paste, focus), `snapshot_ansi`
(a redraw sequence), and read-only cell access. It holds no policy. The host keeps one `Screen` per PTY, writes its
replies back to the PTY, and sends `snapshot_ansi` to a client on attach, followed by live bytes. The client keeps
its own `Screen` per session for rendering and input encoding. `shepherd-tui` renders a `Screen` into a ratatui `Buffer`.
The sync-update timeout comes from the runtime clock, not from a clock read inside the crate.
Consequences: PTY bytes are parsed twice (host and client), which is cheap. Agents behave correctly with no client
attached, and reattaching shows the right screen immediately. Known gaps we accept: `❤️`-style emoji are 1 cell
wide, multi-person emoji are split across cells, and XTVERSION gets no answer.

### 0008: Allow pure, established crates in core instead of hand-rolling (2026-10-29)
Context: the core allowlist (serde, serde_json, thiserror) pushed builders to hand-roll base64 and RFC 3339 timestamps.
The ordering by timestamp string comparison that followed in T06 is wrong for timestamps with offsets. Purity is
about IO, not about having few dependencies.
Decision: core may use pure, widely used crates through `CORE_ALLOWED`. Added now: `jiff` (with
`default-features = false` and features `std` and `serde` only, so no system timezone database reads) and `base64`. Their
clock-reading functions are in core's clippy `disallowed-methods`. Hand-rolling something a standard crate does is
a review finding. Builders ask the PM to add the crate rather than reimplementing it.
Consequences: a slightly bigger dependency tree. Timestamps get real ordering and arithmetic. F04 replaces the hand-rolled code.

### 0009: Log durability is process-crash safe, not power-loss safe (2026-10-29)
Context: T04 review. fsync on every append (F_FULLFSYNC on macOS) costs milliseconds per hook event. Losing the last
few events on power loss is acceptable for an agent manager, but losing or corrupting acknowledged history after an
ordinary crash is not.
Decision: appends write a complete line with no fsync. They survive a process crash, not power loss or a kernel panic.
Recovery truncates only (a) bytes after the last newline, and (b) a trailing region made up only of NUL bytes and
newlines. Any complete line that doesn't parse is `Corrupt` and is never silently dropped. Each root has one writer,
enforced with an exclusive file lock.
Consequences: a power cut may lose the newest events and need recovery, but it never silently rewrites history.
Revisit this (fsync per batch) if acks to remote peers start depending on durability (M4).

### 0010: `~/.shepherd` is private (0700) (2026-10-29)
Context: T10 review. The host socket lets anyone who can connect drive your agents, and the ACP++ logs hold prompts, tool
inputs and outputs.
Decision: the host runtime creates `~/.shepherd` with mode 0700 before opening anything, and refuses to start if it exists
with group or other permissions (it reports the fix, and never chmods it silently). Adapters that create directories under
it use 0700, and files use 0600. The socket adapter enforces this for its own dir (`InsecureDir`).
Consequences: one clear startup error for a misconfigured home dir. No other user on the machine can read the logs or reach the socket.

### 0011: Runtimes are a single-threaded event loop over channels, not tokio (2026-10-29)
Context: decision 0005 assumed async. In practice every port (Pty, EventLog, Link, Worktrees, Config, HostLauncher) came out
synchronous, with background threads pushing events into a sink.
Decision: each runtime (host, client) is one thread looping over a single unbounded channel of runtime events (port
events, timer deadlines, signals). It calls the pure core and executes the resulting commands through the ports, in order.
Deadlines (the sync-update timeout, connect retries, polling) are computed from state and serviced with `recv_timeout`,
with no sleeps. No tokio for M1. Revisit at M4 if iroh needs it, by putting iroh behind the `Link` port on its own runtime thread.
Consequences: simple, deterministic ordering (a subscribe and its catch-up read happen in one loop iteration, so there's no
gap and no duplicate). Supersedes 0005 for M1.

### 0012: serde_json `preserve_order` is on in every build (2026-10-29)
Context: F03 found `preserve_order` was enabled only in test builds, through feature unification from a dev-dependency (the ACP
schema crate). Tests and production serialized JSON objects differently: insertion order against sorted keys. Golden-string
tests passed for a reason that doesn't hold in the shipped binary.
Decision: the workspace `serde_json` dependency enables `preserve_order` for every build. Logs keep each frame's original key
order, which fits the lossless rule better.
Consequences: a dev-dependency can never again change production serialization this way. In general, a feature a
dev-dependency switches on that changes behavior must be enabled in normal builds or not at all.

### 0013: Subscribe catch-up is paced by link backpressure (2026-10-29)
Context: T11 review. Decision 0011's "catch-up in one loop iteration" pushes the whole log history into the link at once. The
Link adapter disconnects any peer with more than `MAX_QUEUED` (8 MiB) pending, so a host with more than about 8 MB of history
can't be attached to. Reproduced on the real adapters.
Decision: the `Link` port gains `fn queued_bytes(&self, conn) -> Option<usize>` (additive). The host keeps a per-connection
catch-up cursor (a `(log, seq)` per subscribed log) and sends entries from `EventLog::read` while `queued_bytes` is below a
low-water mark (e.g. `MAX_QUEUED / 4`). It resumes on a short deadline while any catch-up is pending. Live entries for a log
whose cursor hasn't reached the end are **not** sent live: the cursor will read them from the log, so there's no buffering
and no gap or duplicate by construction. When a cursor reaches the end of its log in the same iteration it switches that log to
live. PTY output for a connection starts (snapshot first, then live bytes) only after its log catch-up completes. The ack is sent
when catch-up completes.
Consequences: attaching works with any history size. A truly slow client is still disconnected as `SlowConsumer` by the adapter.
Amends 0011.

### 0014: Point-and-click first; hotkeys are optional (2026-10-29)
Context: the user's first hands-on run of M1. The product is driven by the mouse, not by memorized prefix keys.
Decision: every user action (select, new, rename, close, detach, and every later feature: profile pick, resume, workspace
switch, cleanup, search, attention) must be reachable by clicking visible UI: buttons, rows, context menus, and dialogs with
clickable buttons. A spec that adds an action names its click target first. Keyboard shortcuts are secondary: the existing
`ctrl-b` bindings stay, but new tasks don't add hotkeys unless the spec says so (a later task adds them across the board).
Mechanics: the client keeps mouse capture on all the time. Clicks are hit-tested by a pure function of the same layout `render`
uses (`tui`), mapped to `Intent`s, and the pure core decides. Mouse events inside the terminal pane go to the session only when
it has mouse tracking on; otherwise a click there just focuses the pane. Text selection in the pane uses the terminal's own
bypass (shift-drag in most terminals, option-drag in iTerm2 and Terminal.app); the help popup says so.
Consequences: snapshot tests show the clickable controls, and each control has a hit-test test that clicks its rendered
cell. Keyboard-only use keeps working through the existing bindings.

### 0015: Inert test-only fault-injection features are allowed (amends 0012) (2026-10-30)
Context: F23 needed a way to make `Screen::feed` panic in tests without relying on the alacritty bug. The builder added a
`panic-trigger` cargo feature on `shepherd-term`, enabled only by the `shepherd` crate's dev-dependencies. Decision 0012 says a
dev-dependency feature that changes behavior must be on in normal builds or not at all. Normal and release builds of the
binary (`cargo build`, `cargo install`) don't contain it; `cargo test` leaves a test build of `target/debug/shepherd` that does.
Decision: a test-only feature is allowed when it is pure fault injection that is inert unless a specific marker input
arrives, no test passes *because* of it (it only makes failures reachable), and its name says what it is. Features that
change serialization, defaults or any normal-path behavior remain banned by 0012.
Consequences: fault-injection tests stay simple. Never ship or smoke-test a binary from `cargo test`'s target dir; the smoke
script uses `cargo build --release`.

### 0016: Codex hooks are trusted per hook via a session-flags `hooks.state` override; `sha2` allowed in core (2026-10-30)
Context: T19. Codex (rust-v0.166.0) loads hooks passed with `-c hooks.<Event>=[...]` from its SessionFlags config layer, so
nothing touches `~/.codex`, but it skips non-managed hooks until trusted (`hooks.state."<key>".trusted_hash`). The two ways
to get trust: `--dangerously-bypass-hook-trust`, which also runs the user's own untrusted hooks in those sessions, or passing
our own hooks' trust state with `-c hooks.state=...`, which requires computing Codex's hook hash (sha256 over a canonical
JSON of the hook) and key format, both internal to Codex.
Decision: trust only our hooks (the `hooks.state` override). Never pass `--dangerously-bypass-hook-trust`: it weakens the
user's own hook review. `sha2` joins the core allowlist (pure, established, decision 0008) so the launch plan stays pure.
A test pins the hash and key for a fixed hook against values derived from Codex's source, citing the file and version.
Codex parses every `-c` value as TOML, so `toml` also joins the core allowlist: the launch plan builds overrides as serde values
and encodes them with `toml`, never by hand, keeping `Launch.spawn.args` complete.
Consequences: if Codex changes the format, our hooks are skipped (fails safe: no status, Codex asks the user to review them
in `/hooks`, and trusting once persists because our command string is stable). Recheck on Codex upgrades.

### 0017: Text segmentation in core uses `unicode-segmentation` (2026-10-30)
Context: T24's auto-name slug split words with `char::is_alphanumeric`, which breaks decomposed (NFD) accents and Indic
viramas apart (`crème` typed as `cre\u{300}me` became `cre`), and macOS paths are often NFD.
Decision: core may use `unicode-segmentation` (pure, established; already in the tree via ratatui) for word and grapheme
segmentation (UAX #29). No hand-rolled Unicode tables.
Amended after the T24 re-review: UAX #29 keeps invisible format characters (bidi overrides, ZWJ, soft hyphen, BOM, tags)
inside words and lets one grapheme cluster grow without bound (stacked combining marks). So `unicode-normalization` is
allowed too: text that becomes a display name is NFC-normalized, and only alphanumeric characters plus a bounded number of
combining marks per base (`is_combining_mark`) are kept; every other character (all of category Cf/Cc) is dropped.
Consequences: names are display-safe and bounded; no hand-rolled Unicode tables.
Second amendment (F35): `unicode-properties` (unicode-rs, no deps) is allowed in core for exact general-category checks, so
`display_safe` can drop every Cc/Cf character while keeping spaces, punctuation and symbols in error text and paths.

### 0018: The host updates itself by re-exec in place (2026-10-30)
Context: spike T36 (section "Spike T36: host hot-swap" above). A host started before a feature landed answered new requests
with "method not found".
Decision: hellos carry a `BuildId` (sha256 digest of the executable; version for display). The client's core decides from
the hello. The host re-execs the binary at its startup path, carrying the PTY, lock and listener fds plus a versioned handoff
file, only after a `handoff-check` preflight passes. A restart that kills sessions happens only when the user clicks it.
Option (b) (second host + SCM_RIGHTS, herdr's approach) is not built: it loses agent exit codes and overlaps two runtimes.
Consequences: one new port (`PtyHandoff`); the handoff format is versioned and read by older and newer builds; agents
survive host updates on unix. Linux `ETXTBSY` / `/proc/self/exe (deleted)` behaviour must be verified in T36b.
Recorded from T36b: the listener is handed over before connections close (a reconnect racing the exec was otherwise
accepted by the old image and cut off); the redraw nudge resizes one column narrower and back (same-size doesn't SIGWINCH);
the preflight validates `config.toml`, and after adoption a broken config falls back to defaults with a persistent host
warning instead of killing sessions; adoption kills only processes `waitid` proves are our children and closes only carried
terminals; a SIGTERM between Accepted and exec is dropped (the new image then runs the installed build anyway).

### 0019: Machines peer over iroh; the host owns the endpoint (2026-10-30)
Context: spike T28 (section above): iroh 1.4.1 fits `Link` unchanged; direct ≈ unix-socket latency, relayed ≈ 26–30 ms
median per keystroke echo; two endpoints with one key stall each other. Decision:
- Each host binds one iroh endpoint (iroh 1.x, `presets::N0`, n0's public relays) with the machine identity in
  `~/.shepherd/identity.key` (0600, O_EXCL). `MachineId` is its node ID.
- Machines pair with a single-use, expiring ticket (`shepherd pair` / `shepherd join`), token compared in constant time.
  Trust is the allowlist of node IDs folded from `_shepherd/peer/trusted` (revoked by `peer/removed`), owned by core and
  enforced by the adapter in iroh's `after_handshake` hook before any `Connected`.
- Clients never bind an endpoint: they reach a remote host through `_shepherd/tunnel { machine }` on their local host,
  which splices lines both ways with read-pausing flow control. The wire protocol stays end to end.
- `Link` gains `peer(conn) -> Option<PeerId>` and `DisconnectReason::Refused` (both additive).
Consequences: tokio enters `shepherd-adapters` only, one current-thread runtime thread per process (amends 0011: core and the
runtime stay synchronous). T29–T32 follow the spike's proposals (now accepted). pkarr/DNS discovery, relay throughput and
catch-up, and real NATs/Linux are verified in T30/T31.

### 0020: Conversation archive as an independent crate (2026-10-30)
Context: the user's proposal `docs/proposals/conversation-archive.md`; cleanup must never lose agent context. Decision:
- `crates/archive` (`shepherd-archive`) depends on no internal crate; only the `shepherd` binary depends on it, wiring it to
  core's `Archive` port through an adapter in `crates/shepherd`. check-arch enforces both directions.
- Allowed deps there: `rusqlite` (feature `bundled`, no system SQLite) and `zstd`, plus workspace crates already allowed
  (`sha2`, `serde`, `serde_json`, `jiff`, `thiserror`). Not in core.
  Amended (T39): `percent-encoding` too, for SQLite URIs (`mode=ro&readonly_shm=1`) that need the path escaped.
- The archive is on-device only (`~/.shepherd/archive/`, 0700/0600), one per machine; no network ever.
- Real-agent end-to-end tests (T44) are outside the gate: they need the network, auth and tokens. They live behind an opt-in
  script, `scripts/e2e-agents.sh`, run only by the user (or the PM with the user's go-ahead), with agent state in throwaway
  dirs, and they re-record the fixtures the gated tests use. Builders never run the real agents (standing rule).
Consequences: one new crate and port; T42 adds `Worktrees::commits_since`; a Claude cwd-rewrite on restore is decided by
T44's evidence, not assumed.

### 0021: `Link` gains reading pause, dialling and per-conn peer identity (amends 0019) (2026-10-31)
Context: T31a tunnels a local client conn to a peer's host over iroh. Flow control needs the host to stop reading one side
while the other is backed up, and something has to dial a peer. Decision: `Link` gains, additively and with contract cases
run against the fake, the unix adapter and the iroh adapter:
- `peer(conn) -> Result<Option<MachineId>, LinkError>` (`Ok(None)` for a local conn; an unknown or closed conn is an
  error, so a local-only check fails closed; amended after T31a's review) and `DisconnectReason::Refused`;
- `drained_bytes(conn)`, which catch-up stall detection uses as its progress signal;
- `pause_reading(conn, paused)`: no lines are emitted for a paused conn until it is resumed; it still closes;
- `open(peer)`: dial a trusted peer; a link that can't dial (unix) returns `LinkError::NoPeers`.
The runtime joins the unix and iroh links into one `Link` (`HostLinks`: local conn ids even, peer ids odd). Pausing is
best-effort per line: lines already handed to the loop before a pause are held by the tunnel. A hard bound would need
credits on `Link` (like the PTY acks); not now.
Consequences: every `Link` implementation provides these. Requests that only local clients may make check `peer(conn)`.

### 0022: Automation: the host owns the layout; prompts can wait for the reply (2026-10-31)
Context: the user wants agents and scripts to drive shepherd: prompt an agent from a CLI (manager and subagents), arrange
the screen from a CLI (pages, splits), and test complex UI headless. This amends the manifesto's "the initial version will
not handle splits or tabs". Decision:
- The screen layout (pages, split panes, what each pane shows) is pure data in `core::layout`, owned by the host,
  recorded in its log and broadcast to clients, changed through `_shepherd/ui/apply` (local clients only, per 0021). Every
  window shows the same layout, drawn at its own size. Focus is per window (it decides where that window's input goes).
- A session's PTY is sized to the pane showing it on the window that last focused that pane.
- `_shepherd/session/prompt` is answered by the host, which types the prompt into the agent (only when it's at its prompt)
  so every client sees it. `shepherd prompt --wait` waits for the turn to end and returns the agent's last reply, read from
  its transcript through a `Transcripts` port (archive-crate adapter, same path checks as capture). An agent that stops to
  ask the user returns `wants-user` at once.
- UI is tested through a headless harness (real client + renderer on `TestBackend`, clicking on drawn text), including a
  seeded property test over random UI steps.
Consequences: feature `docs/features/automation.md` (T45-T50); `docs/cli.md` documents every `--json` shape.

