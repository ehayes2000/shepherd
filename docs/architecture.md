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

## Ports

A port is a trait in `shepherd-core::ports`. It describes a capability in domain terms, such as
`Worktrees::create(&RepoRef, &BranchSpec)`, not `git_worktree_add(args)`. Each port comes with:

1. the trait and its domain types (in core)
2. a fake in `shepherd-testkit::fakes` (stateful, in-memory, and able to inject failures)
3. a contract suite in `shepherd-testkit::contracts`: one `pub fn` per case, plus a `<port>_contract!(make)` macro that
   expands to one `#[test]` per case (template: `contracts/pty.rs`)
4. a real adapter in `shepherd-adapters`, whose tests call the same contract suite
5. wiring in `shepherd/src/main.rs`

See the `new-port` skill. A port isn't done until all five exist.

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
