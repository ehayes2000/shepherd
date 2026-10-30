# Terminal: PTYs and the emulator
milestone: M1
goal: Sessions run in real PTYs with backpressure, and the client renders them through shepherd-term

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T02 | Spike: terminal emulator | done | 0c92cc6 |  |  |
| T08 | `Pty` port | done | bc7023e | 2 | lost output under load, kill didn't kill |
| T09 | `shepherd-term` screen + TUI rendering | done | 435d44b | 2 | 1-column crash or hang |
| F01 | Event-driven Pty pump | todo |  |  |  |
| F06 | Snapshots preserve partial parser input | done | f7a6d79 | 3 | quadratic stall |
| F07 | Carry palette colours and the empty-stack kitty flags in snapshots | done | 6905cb5 | 2 | kitty_top refresh in expire_sync untested |
| F14 | PTY read backpressure | done | cd48050 | 3 (+1 hung) | post-kill drop, shutdown ack, excess-ack banking untested; first reviewer hung 4 h |
| F23 | A program's output can panic the emulator (and the host) | done | 74e2abb | 2 | sync-timeout path unguarded; pop count, guard-flag reset untested; missing_trait_methods lint added |
| F38 | PTY backpressure contract case fails under load | done | 24f1b49 | 1 | 10 s total deadline was a measurement; now an idle bound plus byte limit |
| F42 | A new session starts at the wrong size until the window is resized | done | 2d82053 | 2 | runtime adopt-size wiring untested; sizes unclamped |
| F43 | Tab next to wide chars stored as a '\t' spacer | building | task/f43-tab-spacer @ 33fd53a | 0 |  |

## Specs

### T02: Spike: terminal emulator
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

### T08: `Pty` port
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

### F01: Event-driven Pty pump
depends: T08
touches: adapters::pty
goal: Remove the 20ms idle tick. Each idle session currently wakes 50 times a second.
acceptance:
- The pump waits on the master fd plus a kill self-pipe, and detects exits by SIGCHLD or pidfd/kqueue, so there are no timed wakeups.
- The full `pty_contract!` passes, including under parallel load (report the pass rate). An idle session uses about zero CPU.
scope: crates/adapters
notes: Low priority. Do it once there are real sessions to measure.

### F06: Snapshots preserve partial parser input
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

### F14: PTY read backpressure
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

### F23: A program's output can panic the emulator (and the host)
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

### F38: PTY backpressure contract case fails under load
depends: F14
touches: testkit contracts::pty, adapters::pty (tests)
goal: `pty::tests::unacked_output_stops_reading_until_acked` failed 12/12 under a 12-copy load round on main (99e47e6) with
"seq didn't exit: Timeout after ~1.1MB" — a 10 s total deadline that's a measurement under load, not a liveness bound.
acceptance:
- Make the bound a generous liveness bound (or count progress instead of total time) so the case still fails if backpressure
  breaks; 0 failures in two 24-copy rounds of the whole adapters binary.
scope: crates/testkit, crates/adapters

### F42: A new session starts at the wrong size until the window is resized
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

### F43: Tab next to wide chars stored as a '\t' spacer
depends: T03
touches: term (grid)
goal: `crates/term/tests/fuzz.rs` `random_output_never_panics_and_snapshots_round_trip` failed once (F41's gate): a tab cell
came back as a `'\t'` Spacer instead of `' '`. Input (6 cols): `"a" | ESC[4h | ESC[2;4r | "\t" | "日" "日" ESC[3D | "\t" |
"\r   "`. Proptest seed saved by the PM at scratchpad `f43-fuzz-seed.txt` (also reproducible from the input above).
acceptance:
- Regression test first (fix-bug) with that exact input as a deterministic case; fix; the fuzz property holds.
scope: crates/term
