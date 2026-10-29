# PTY byte fixtures

Raw PTY output recorded from real agent sessions, for terminal-emulator tests.

`<agent>-session.<n>-<cols>x<rows>.bin`: segment `n` was produced at `<cols>x<rows>`. To replay, resize the
emulator to that size and then feed the bytes, one segment after another in order.

This is the only copy. `shepherd-term`'s tests load it through `crates/term/tests/support/mod.rs`, and
`shepherd-tui`'s render tests include that same module by path.

Re-record with `record/record.py` (`python3 record.py claude.json`). See "How to re-record" in
`docs/spikes/T02-terminal-emulator.md` for setup, and for the scrubbing and inspection to do before committing. The
spike predates the move here from `crates/tui/tests/fixtures/`, so read its paths as this directory.
