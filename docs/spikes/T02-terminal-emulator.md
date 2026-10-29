# T02 spike: terminal emulator

**Recommendation: `alacritty_terminal` 0.26 with `default-features = false`.** Add it to `TUI_ALLOWED`.

All three candidates draw today's real Claude Code and Codex sessions the same way, cell for cell. So the
choice comes down to everything around the grid: answering terminal queries, synchronized output, reflow,
mode introspection (which input encoding needs), testability, maintenance, and dependency weight.
`alacritty_terminal` is the only candidate that is good on all of these.

## Candidates and versions tested

| candidate | version | source |
|---|---|---|
| `vt100` | 0.18.1 (2026-10-19) | crates.io |
| `alacritty_terminal` | 0.26.0 (2026-04-06) | crates.io |
| `wezterm-term` | git `wezterm/wezterm@e68a8ec2` | not published on crates.io |
| `tattoy-wezterm-term` | 0.1.0-fork.5 (2025-07-11) | crates.io, third-party fork that v1 used |

Agents recorded: Claude Code 2.1.312 and Codex CLI 0.165.0.

## Method

The comparison harness lived in a scratch crate outside the repo and is not committed. It wraps each emulator
behind one trait (`feed`, `resize`, `snapshot`, `take_replies`). A snapshot is an emulator-neutral grid:
grapheme, width, fg/bg (`Default | Idx | Rgb`), bold/dim/italic/underline/inverse, cursor position, visibility
and shape, alt screen, bracketed paste, mouse mode, and title. It ran four checks:

1. **Fixture final state.** Replay each fixture segment, resizing between segments as recorded. Diff the final
   snapshots pairwise.
2. **Lockstep.** Feed all emulators the same 256-byte chunks and diff the text and cursor after every chunk.
   This catches transient differences, not just the final screen.
3. **Chunking robustness.** Feed in 1-byte and 7-byte chunks, which splits UTF-8 and escape sequences. The result
   must be identical to feeding the whole buffer.
4. **Synthetic probes.** Hand-written sequences for each feature named in the spec, plus the queries the
   recorded agents actually send.

The harness also drew every snapshot into a ratatui `Buffer` and read it back. That gave 0 mismatched rows on
both fixtures for every candidate, including the CJK and emoji lines.

## Fixtures (`crates/tui/tests/fixtures/`)

| file | size | what's in it |
|---|---|---|
| `claude-session.0-100x30.bin` | 23.8 KB | startup, two prompts (one echoes `日本語 テスト 🐑 ✅ café — done`, one writes a file and shows a highlighted code listing), the `?` shortcuts overlay, a bracketed paste |
| `claude-session.1-80x24.bin` | 2.1 KB | output after resizing to 80x24 |
| `claude-session.2-120x36.bin` | 2.8 KB | output after resizing to 120x36 |
| `claude-session.3-exit-120x36.bin` | 0.9 KB | Ctrl-C exit: modes reset, leaves the alt screen, prints the resume hint |
| `codex-session.0-100x30.bin` | 21.6 KB | startup animation, one prompt echoing the same unicode line |
| `codex-session.1-80x24.bin` | 2.3 KB | output after resizing to 80x24 |
| `codex-session.2-exit-80x24.bin` | 0.6 KB | Ctrl-C exit |

To replay, feed segment N's bytes after resizing the emulator to the size in its file name. The sizes come in
order, so start at the size of segment 0.

**What the agents actually send.** This is from inventorying the recorded bytes, and it matters for later tasks:

- **Claude Code 2.1** runs **full-screen on the alternate screen** (`?1049h`), not inline. It turns on
  any-motion SGR mouse (`?1000/1002/1003/1006h`), bracketed paste, focus events (`?1004h`) and color-scheme
  reports (`?2031h`). It sends the queries `CSI c`, `CSI > 0 q` (XTVERSION), `CSI ? u` (kitty keyboard) and
  `OSC 7501;?`. It draws only with truecolor SGR, CUP/CHA/CUF/CUD and EL. It shows the real cursor at the input
  caret. It does not use DECSCUSR or synchronized output.
- **Codex 0.165** also runs on the alt screen with the same mouse modes. It wraps every frame in synchronized
  output (`?2026h … ?2026l`, 107 pairs in this short session). It pushes kitty keyboard flags (`CSI > 7 u`),
  sets `CSI > 4;0 m` and DECSCUSR `CSI 0 q`, and queries `CSI 6 n`, `OSC 10;?`, `OSC 11;?`, `CSI ? u` and
  `CSI c` at startup.

### How to re-record

The recorder is `crates/tui/tests/fixtures/record/record.py`, a small Python pty driver. Its scripts are
`claude.json` and `codex.json` in the same directory.

```
mkdir -p /tmp/shepherd-demo                     # neutral cwd; the path appears in the output
cd crates/tui/tests/fixtures/record
python3 record.py claude.json                   # writes ./claude.<n>.bin per segment, plus ./claude.log
python3 record.py codex.json
```

- Each `resize` or `split` step starts a new segment file. Rename the outputs to the `<agent>-session.<n>-<cols>x<rows>.bin` scheme.
- The recorder sets `TERM=xterm-256color` and `COLORTERM=truecolor`. It removes `CLAUDE*`, `CODEX*`, `SHEPHERD*`
  and terminal-identifying variables from the environment. Without this, running under Claude Code leaks a
  "child session" banner into the recording.
- The recorder answers DA1, CPR, kitty-keyboard and OSC 10/11 queries with fixed values, so the agents don't stall.
- `codex.json` passes `-c check_for_update_on_startup=false`. Without it, the "Update available" dialog can
  swallow the scripted keystrokes and **run the updater**. That happened once during this spike: it upgraded
  the local Codex CLI from 0.162.1 to 0.165.0.
- Claude's trust-folder dialog only appears the first time a directory is used. Run `claude` once in
  `/tmp/shepherd-demo` and accept it before recording.
- **Before committing, scrub and inspect the recording.** Session UUIDs, which appear in the exit "resume"
  hints, were replaced with zeros of the same length so the layout doesn't change. I grepped for the user name,
  `/Users/`, email addresses, `sk-` keys and `/Code`. Nothing else turned up. The recordings do show the
  account's plan name ("Claude Max"), the model names, and the recording machine's Claude hooks ("running Stop
  hooks", "1 agent"). None of these are sensitive.
- Limitations: the sessions are short and scripted, so tool-permission dialogs, long scrolling transcripts and
  the Codex transcript overlay are not covered. Answers are nondeterministic, so a re-recording won't be
  byte-identical.

## Results on the real fixtures

| check | vt100 | alacritty | wezterm (git) | tattoy fork |
|---|---|---|---|---|
| Claude, final screen after all segments and resizes | ✔ | ✔ | ✔ | ✔ |
| Claude, lockstep (117 checkpoints) | ✔ no divergence | ✔ | ✔ | ✔ |
| Codex, final screen | ✔ | ✔ (see bold+dim) | ✔ | ✔ |
| Codex, lockstep (97 checkpoints) | — | shows only whole frames (83 checkpoints inside a sync update) | — | — |
| 1-byte and 7-byte chunking identical to whole feed | ✔ | ✔ | ✔ | ✔ |
| replies sent to the agent during the Claude fixture | none | `?0u`, DA1 | XTVERSION, `?0u`, DA1 | same as wezterm |

The concrete differences on real output were all cosmetic:

- **Wide-char continuation cell background.** vt100 gives the second half of a wide char the default bg. The
  others copy the wide char's bg. That was 8 cells on the highlighted prompt row with CJK in it. It's harmless
  if the renderer skips continuation cells, which ratatui does after a wide symbol.
- **SGR 1 followed by SGR 2.** Codex draws its `›` prompt with `ESC[1m ESC[2m`. alacritty keeps BOLD and DIM as
  separate flags, so the cell is bold and dim. vt100 and wezterm store a single intensity value, so the cell is
  dim only.
- **Synchronized output.** Codex's first frame is a `?2026h` that stays open for about 10 KB, through the
  loading animation. alacritty buffers until the matching `?2026l` or a timeout. vt100 and wezterm render
  half-drawn frames, which shows up as tearing if we sample the screen mid-frame. In the harness, alacritty's
  timeout is a deterministic stub; a real host drives it from its clock (see API fit).

So on today's agents, plain grid correctness doesn't separate the candidates. The probes below do.

## Synthetic probes (6x20 terminal)

✔ means correct or matching xterm. Probes where every candidate behaves the same are summarized on the last row.

| feature | vt100 | alacritty | wezterm / fork |
|---|---|---|---|
| alt screen `1049` | ✔ | ✔ | ✔ |
| alt screen `47` / `1047` (not used by targets) | 47 ✔, 1047 ✘ | ✘ both (drawn on main) | ✔ |
| truecolor `38;2;r;g;b` | ✔ | ✔ | ✔ (stored as f32 sRGB, round-trips exactly) |
| truecolor colon form `38:2::r:g:b` | **✘ (ignored)** | ✔ | ✔ |
| curly underline `4:3`, underline colour `58:…` | ✘ | ✔ | ✔ |
| wide CJK and emoji | ✔ | ✔ | ✔ |
| VS16 `❤️` width (Claude's string-width and ratatui both say 2) | ✔ 2 | **✘ 1** | ✔ 2 (needs `unicode_version: 14`; the default 9 gives 1) |
| ZWJ family emoji as one cluster | ✘ split | ✘ split | ✔ |
| wide char typed in the last column wraps to the next line | ✔ | ✔ | **✘ placed in the last column, half clipped** |
| DECSCUSR cursor shape | ✔ (since 0.17) | ✔ | ✔ |
| bracketed paste mode flag | ✔ | ✔ | ✔ |
| mouse mode / encoding introspection (1000/1002/1003, SGR) | ✔ enum | ✔ `TermMode` bits | upstream ✔; **fork: only `is_mouse_grabbed()`** |
| kitty keyboard flags introspection (Codex pushes `>7u`) | ✘ not tracked | ✔ `TermMode` bits | ✔ `get_keyboard_encoding()` |
| synchronized output `2026` | ✘ ignored | ✔ buffered, pluggable timeout | ✘ ignored (reports it as supported) |
| DA1 / DA2 / CPR / DSR replies | **✘ none** (must hand-roll in callbacks, as v1 did) | ✔ | ✔ |
| DECRQM, kitty `CSI ? u` | ✘ | ✔ | ✔ |
| OSC 10/11 colour query | ✘ | ✔ (via `ColorRequest`, host supplies the colour) | ✔ (from the palette) |
| XTVERSION (Claude sends it) | ✘ | ✘ | ✔ |
| how replies are delivered | n/a | synchronous `Event::PtyWrite` | **background writer thread** spawned per `Terminal` |
| reflow when width grows or shrinks | **✘ truncates, never unwraps** | ✔ | ✔ |
| row shrink/grow with cursor at bottom | ✔ (since 0.18) | ✔ | ✔ |
| same in all four: SGR basics, 256 colours, background colour erase, combining marks, overwriting half a wide char, scroll regions, REP, `CSI 3 J`, OSC 0 title, OSC 8 text | ✔ | ✔ | ✔ |

## Resize behaviour

- Both agents are full-screen on the alt screen and repaint completely on SIGWINCH. Every candidate ends with
  the same screen after each recorded resize (100x30 → 80x24 → 120x36), and lockstep showed no transient
  divergence there either.
- Primary-screen content (shell sessions, any agent running inline, the post-exit screen) is where they differ.
  vt100 truncates on shrink and never rejoins wrapped lines. alacritty and wezterm reflow. When shrinking,
  alacritty keeps the cursor row fixed and pushes the overflow into scrollback. wezterm pushes rows down.
- vt100 0.18 changed `set_size` to keep the cursor at the bottom and use scrollback, matching real terminals.
  v1's vt100 0.16 dropped the bottom rows instead. That was likely part of v1's "resize issue".

## API fit for drawing into a ratatui `Buffer`

The harness glue (emulator → neutral snapshot) was about 110 lines for vt100, 180 for alacritty and 165 for
wezterm. Drawing the snapshot into a `Buffer` is the same for all three: copy symbol and style, and skip cells
with width 0.

- **vt100.** Simplest: `screen.cell(row, col)`, with `is_wide`/`is_wide_continuation` and plain `u8` RGB.
  Mode getters are complete for what it supports. But query replies, kitty flags and sync are missing, and
  shepherd would have to build them.
- **alacritty_terminal.** `term.grid()[Line(y)][Column(x)]` gives `c`, `zerowidth()`, `fg`, `bg` and `flags`
  (`WIDE_CHAR`, `WIDE_CHAR_SPACER`, `LEADING_WIDE_CHAR_SPACER`, every underline style). The cursor comes from
  `grid.cursor.point` and `cursor_style()`, and modes from one `TermMode` bitset (mouse, kitty, paste, app
  cursor, focus). Replies, title and colour requests arrive through an `EventListener` you implement. A
  `Vec`-backed listener is deterministic. You must map `NamedColor` (Foreground/Background/Dim*) to ratatui
  yourself. `vte::ansi::Processor<T: Timeout>` takes the sync timeout as a type parameter, so tests can use a
  stub and the runtime can drive real time. That fits "time arrives as data". `Term` itself has no clocks or
  threads; those live only in the `tty` and `event_loop` modules, which we would not use.
- **wezterm-term.** Line access goes through `screen.lines_in_phys_range(screen.phys_range(..))`, which clones
  lines; `visible_lines()` is `#[cfg(test)]`. Cells are iterated per line with `cell_index()`, and colours are
  f32 sRGB. Replies go through a `BufWriter` around a **writer thread spawned in `Terminal::new`**, so a test
  can't read replies synchronously. The harness had to sleep to see them, which breaks the "no sleeps" testing
  rule.

## Maintenance

| | vt100 | alacritty_terminal | wezterm-term | tattoy fork |
|---|---|---|---|---|
| maintainer | one person (doy) | Alacritty org | mostly one person (wez) | third party, for tattoy |
| activity | dormant 2025-07 → burst of 0.17 + 0.18 on 2026-10-18/19 | releases 2025-02, 2025-10, 2026-04; `alacritty_terminal/` commits every 1-2 months (latest 2026-10-16) | `term/` commits a few times a year (latest 2026-09) | last release 2025-07, repo last pushed 2025-09 |
| distribution | crates.io | crates.io, semver | **git only** (monorepo), so a git dependency pinned by rev | crates.io, `-fork.N` pre-release versions |
| repo stars / open issues | 123 / 12 | 66k / 342 | 29k / 1.9k | 574 / 41 |

## Dependency weight

Measured with `cargo tree -e normal` (unique crates excluding the root) and a clean `--release` build, on the
same machine:

| | crates | clean release build | notes |
|---|---|---|---|
| vt100 | 6 | 0.7 s | vte, unicode-width, itoa, memchr, arrayvec |
| alacritty_terminal | 33 | 4.0 s | Always pulls `polling`, `rustix-openpty`, `signal-hook`, `parking_lot`, `regex-automata` and `base64`, even though we wouldn't use its pty layer. `default-features = false` drops serde. |
| wezterm-term (git) | 200 | 18.9 s | image codecs including the **rav1e AV1 encoder**, rayon, terminfo, icu, pest, nix |
| tattoy fork | 216 | 17.7 s | same as wezterm plus schemars and serde_json |
| ratatui 0.30, for scale | 68 | 3.8 s | already in the tree |

Throughput was not a differentiator. Even with construction counted on these tiny fixtures, the candidates ran
at 30–150 MB/s, far above what an agent writes.

## What went wrong in v1 (git history before 0c9215e)

- **Jan 16 2026, `tattoy-wezterm-term`.** Commits 4cbc134 and 97897ae ("this sucks"). v1 re-serialized the
  emulator grid as escape codes straight to stdout and positioned the cursor itself. It hit "`cursor_pos()`
  returns wrong x value (always 0) for Claude Code". That was most likely correct emulation: Claude Code at the
  time parked the hidden cursor at column 0 and drew its own caret. Today all four candidates agree on the
  cursor position in the Claude fixture, (27,17) and visible.
- **Jan 17 2026, switch to `vt100` 0.16 and ratatui** (cb8bb6a, "fix resize issue"). Problems carried into the
  final v1:
  - `TerminalCallbacks` hand-rolled DA1/DA2/CPR replies and was marked "TODO: this is incomplete + likely
    wrong". vt100 answers nothing on its own.
  - It had no cursor shape, no reflow, no colon SGR, and 0.16's resize dropped the bottom rows.
  - Scrollback used `vt100::Screen::set_scrollback`, which only covers the primary screen.
  - v1's own bug: `SharedSize` masked columns with `0x00FF` in cb8bb6a, fixed to `0xFFFF` later.
- **v1 swallowed all mouse events** and used the wheel for its own vt100 scrollback (08d3f5d). Claude 2.1 and
  Codex now run on the alt screen with any-motion mouse tracking. There's no emulator scrollback there, and the
  agents expect wheel and click events themselves.

## Recommendation

Use **`alacritty_terminal = { version = "0.26", default-features = false }`** in `shepherd-tui`.

Why:

1. It matches the others cell for cell on both real fixtures, and it is the only candidate that handles Codex's
   synchronized output, which avoids torn frames. The sync timeout is a type parameter, so tests stay
   deterministic and the runtime supplies time.
2. It answers DA1, DA2, CPR, DSR, DECRQM, the kitty query and OSC 10/11 synchronously through a listener we own.
   That replaces v1's hand-rolled responder.
3. A single `TermMode` bitset exposes the mouse mode and encoding, kitty keyboard flags, bracketed paste, app
   cursor and focus reporting. The input path needs this to encode keys and mouse events the way the agent
   asked for.
4. It reflows on resize, and it fixes v1's resize and cursor problems.
5. It's well maintained, published on crates.io, and costs 33 crates at about 4 s, against about 200 crates and
   19 s for wezterm.

Known gaps to accept or work around:

- **VS16 emoji width is 1.** The agents assume 2, so a line containing `❤️` can drift by one column until the
  next repaint.
- **ZWJ sequences are split** into separate cells. vt100 has the same problem.
- **No XTVERSION reply.** Claude just times out the query. If needed, shepherd can answer it in a few lines.
- **`?47`/`?1047` aren't supported.** Neither target uses them.
- **Unused tty and pty crates get compiled.**

Why not the others:

- **vt100** is lightest and simplest, but shepherd would have to re-implement query replies, sync and kitty
  flags, and live without reflow. Those gaps are exactly where v1 broke.
- **wezterm-term** has the best grapheme handling, but it's git-only, pulls about 200 crates including an AV1
  encoder, spawns a reply thread inside the emulator (breaks deterministic tests), and puts wide chars in the
  last column instead of wrapping.
- **The tattoy fork** adds staleness and hides the mouse mode.

### Implied `TUI_ALLOWED` change

```diff
-TUI_ALLOWED="ratatui crossterm unicode-width"
+TUI_ALLOWED="ratatui crossterm unicode-width alacritty_terminal"
```

Also add `alacritty_terminal = { version = "0.26", default-features = false }` to `[workspace.dependencies]`.
Use `vte` through the re-export `alacritty_terminal::vte`, so `vte` does not need its own allowlist entry.
This needs a decision-log entry in `docs/architecture.md`, as the script header requires.

## Follow-ups found (not done here)

- **Where query replies come from.** Both agents query the terminal at startup and wait (with timeouts) for an
  answer. The architecture puts the emulator in the TUI client, but the PTY lives in the host and keeps running
  with no client attached. Either the host runs a headless emulator (or a minimal query responder) per PTY, or
  replies are routed client → host → pty and are missing when nobody is attached. This needs a PM decision
  before the PTY/screen tasks.
- **Input encoding** must follow the emulator's modes: kitty keyboard flags (Codex pushes `>7u`), SGR mouse
  (both agents use any-motion tracking), bracketed paste and focus events. Mouse and wheel should be forwarded
  to the agent, not used for local scrollback, while the agent is on the alt screen.
- **The sync-update timeout** (about 150 ms in alacritty's `StdSyncHandler`) should be driven by the runtime's
  clock through a custom `Timeout`, not `Instant::now()` inside the TUI.
- **Answer the `?2031` / `OSC 7501` / XTVERSION queries** if we want to mirror Claude's behaviour in a real
  terminal. Not needed to render correctly.
