# Sidebar and point-and-click UI
milestone: M1–M1.5
goal: The sidebar, terminal focus, menus, dialogs and popups, all usable with the mouse (decision 0014)

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T13 | TUI layout, sidebar, and keys | done | 918fdd4 | 2 | ctrl-symbol keys typed digits, Shift+Enter submitted, paste end-marker, 7 surviving mutations |
| T33 | Clickable sidebar and terminal focus | done | 264bf12 | 2 | stale press and sidebar clamp untested (mutation survivors) |
| T34 | Context menu, dialogs with buttons, detach button | done | 83f196a | 2 | same-button release rule untested; long-name truncation untested |
| F15 | Client display polish from T13 | done | 065f873 | 2 | hold-deadline restart guard untested; notice table incomplete. Unknown-glyph snapshot deferred until a frame produces Unknown |
| F21 | Mouse targets in the collapsed sidebar | done | c59acb7 | 1 | – |
| F24 | Profile picker polish | done | 520877d | 2 | picker panicked in 0/1-row windows → every-popup tiny-window test |
| F29 | Workspace switcher list scrolls and fits the window | done | a27fa98 | 1 | cut/cut_left count chars not columns (→ F30) |
| F30 | Truncation measures display width | done | faa7403 | 2 | width-0 cut returned 1 column; added helper property table |
| F34 | The client marks keys that can answer a dialog | done | 274b3e2 | 1 | mixed-version: new client's `answer` not counted by an F31 host (accepted) |
| F55 | A new session can push the selection out of view | todo |  |  |  |
| F56 | The starting glyph is invisible on a selected row | todo |  |  |  |

## Specs

### T13: TUI layout, sidebar, and keys
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

### T33: Clickable sidebar and terminal focus
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

### F15: Client display polish from T13
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

### F21: Mouse targets in the collapsed sidebar
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

### F24: Profile picker polish
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

### F29: Workspace switcher list scrolls and fits the window
depends: T21, F24
touches: tui::popup, core::client
goal: The T21 workspace switcher has the same unscrolled, uncapped list the profile picker had before F24.
acceptance:
- Reuse F24's approach (core-owned scroll, saturating height capped to the window, wheel scrolls, rows below the fold are
  reachable by wheel then click, long paths truncated from the left so the repo name stays visible). Hit tests click rendered
  cells; snapshot of a scrolled switcher; a huge list doesn't overflow.
scope: crates/core (client), crates/tui

### F30: Truncation measures display width
depends: F29
touches: tui::popup, tui::sidebar
goal: `cut` (picker, F24) and `cut_left` (switcher, F29), and any other truncation in the tui, count characters, not terminal
columns, so paths and names with double-width characters (CJK, emoji) overflow and push the mark/repo name off.
acceptance:
- All tui truncation measures display width via ratatui's `Span::width` / unicode-width (no new dependency), never splits a
  grapheme, and keeps the `(current)`/`(default)` marks and the repo name visible. Tests with a CJK path and an emoji name in
  the switcher, picker and sidebar rows. Column sizing (`name_width`, agent column) uses display width too.
scope: crates/tui

### F34: The client marks keys that can answer a dialog
depends: F31
touches: tui::keys, core::wire (input kind)
goal: F31 classifies answers from bytes, so Esc and Ctrl-C (which dismiss Claude's permission dialog without a hook) don't
count, and in kitty "report all keys" mode (flag 8) no key counts. The client knows the actual key event and can tag it.
acceptance:
- The tui tags key input that can answer (Enter, Esc, Ctrl-C, printable) as `kind: answer` (additive) regardless of encoding
  mode; the quiet watch counts `answer` and `paste`. Table tests over key events × keyboard modes (legacy, kitty flags 1/8).
scope: crates/core, crates/tui

### F55: A new session can push the selection out of view
depends: T45
touches: core::client
goal: Found by T45's property test (seed 1592590405): when the host lists a new session above the selection, the sidebar
doesn't scroll the selection back into view, contradicting "the selection keeps itself visible".
acceptance:
- Regression test first in core (fix-bug): a full sidebar with the selection at the bottom, the host lists a new session,
  and the selection stays visible.
- Re-fit the scroll whenever the session list changes (new, closed, renamed reordering), not only on selection or resize.
- Remove the known-gap exemption in `crates/tui/tests/ui/invariants.rs` (~150) so the property test checks host-made
  sessions too, and it passes. (T45's review: the exemption currently skips ~10% of selection checks, not just the gap.)
- From T55's review: `a_tree_shrinking_above_the_selection_keeps_the_selection_in_view` passes without the scroll (put
  sessions below s1 so the clamp can't do the work); the `tree_above_selection() != tree_above` half of the refresh
  condition is untested (test a new session above leaves the scroll, or simplify to `in_view`); assert the narrow
  sidebar's subagent glyph next to `└` hits `Target::Subagent`, not the branch toggle.
- From T45's review: `Ui::event`'s doc says it handles events as the runtime does, but it skips `from_legacy_terminal`
  (the runtime applies it when the keyboard isn't enhanced); say the harness models an enhanced terminal.
scope: crates/core, crates/tui (tests)

### F56: The starting glyph is invisible on a selected row
depends: T62
touches: tui::sidebar
goal: From T62's builder: the "starting" glyph `◌` is DarkGray, the same as the selected row's background, so a starting
session's status disappears when it's selected.
acceptance:
- Every status glyph is readable on both the plain and the selected row background (a test over every glyph and both
  backgrounds, like T62's machine-color readability test); snapshot of a selected starting session.
- From T62's review: build `UnknownMachineColor`'s list of names from `MachineColor::ALL` (core config.rs ~139), and pin
  one id that maps to LightMagenta in `the_color_of_an_id_is_pinned_across_builds`.
- From T50's review: `crates/tui/src/panes.rs` `focused_on` copies core's private `focused_in` (client.rs ~3438); make
  core's public (or a `Layout` helper) and use it, and assert the focused border in `render_page_draws_another_page_…`.
- From T49's review: the collapsed sidebar's `footer_height` is always 3 but `⊞` is drawn only when
  `inner.height >= HEADER_HEIGHT + 3` (sidebar.rs ~222); make the footer height follow the same condition.
scope: crates/tui, crates/core

