# Display-safe text
milestone: M2–M3
goal: Text that agents, hooks or git can name never reaches the screen or stderr raw

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| F35 | One display-safe text rule for everything agents and hooks can name | done | f79d427 | 1 | idempotence claim false (unbounded clusters), clippy ban gaps (title_top, List, &str Widget) → F36 |
| F36 | Close the display-safe gaps from F35's review | fixing | task/f36-display-gaps @ 3921826 | 2 | Blocking: Buffer::merge/filled, buffer::Cell::new/from, .content[ writes bypass draw. Minor: Block::border_set |

## Specs

### F35: One display-safe text rule for everything agents and hooks can name
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

### F36: Close the display-safe gaps from F35's review
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
