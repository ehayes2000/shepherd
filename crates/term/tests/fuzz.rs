//! Random output, resizes and sync timeouts never panic, and `snapshot_ansi` always reproduces what a viewer sees.
//!
//! Run longer with `PROPTEST_CASES=40000 cargo test -p shepherd-term --test fuzz`.

mod support;

use proptest::prelude::*;
use shepherd_term::{Screen, Width};
use support::observe;

/// Text and escape sequences, weighted toward the ones that stress wide chars, screens, sync and input modes.
#[rustfmt::skip]
const TOKENS: &[&str] = &[
    "a", "b", "日", "🐑", "e\u{301}", "\u{3000}", "\t", "\r", "\n", "\x08", " ",
    // Cursor movement and erasing.
    "\x1b[H", "\x1b[5;3H", "\x1b[2;9H", "\x1b[200C", "\x1b[200B", "\x1b[3D", "\x1b[2J", "\x1b[J", "\x1b[K",
    "\x1b[1K", "\x1b[2K", "\x1b[5X", "\x1b[X", "\x1b[3P", "\x1b[P", "\x1b[4@", "\x1b[@", "\x1b[3L", "\x1b[2M",
    "\x1b[3S", "\x1b[2T", "\x1bM", "\x1bD", "\x1bE", "\x1b[3b", "\x1b7", "\x1b8",
    // Screens, sync updates, scroll regions, tabs, charsets, reset.
    "\x1b[?1049h", "\x1b[?1049l", "\x1b[?1047h", "\x1b[?2026h", "\x1b[?2026l", "\x1b[2;4r", "\x1b[r", "\x1b[3g",
    "\x1bH", "\x1b(0", "\x1b(B", "q", "\x0e", "\x0f", "\x1bc", "\x1b[3J",
    // Input modes.
    "\x1b[>7u", "\x1b[<u", "\x1b[>1u", "\x1b[=3;1u", "\x1b[?1h", "\x1b[?1l", "\x1b[?1000h", "\x1b[?1002h",
    "\x1b[?1003h", "\x1b[?1003l", "\x1b[?1005h", "\x1b[?1006h", "\x1b[?2004h", "\x1b[?2004l", "\x1b[?1004h",
    "\x1b=", "\x1b>",
    // Other modes.
    "\x1b[?6h", "\x1b[?6l", "\x1b[?7l", "\x1b[?7h", "\x1b[4h", "\x1b[4l", "\x1b[20h", "\x1b[20l", "\x1b[?25l",
    "\x1b[?25h", "\x1b[3 q", "\x1b[0 q", "\x1b[?12h",
    // Pen.
    "\x1b[1;31m", "\x1b[0m", "\x1b[38;2;1;2;3m", "\x1b[48;5;100m", "\x1b[4:3m", "\x1b[58;5;3m",
    "\x1b[58:2::9:8:7m", "\x1b[1;2;3;4;5;6;7;8;9m", "\x1b[22m", "\x1b[44m", "\x1b[97;101m",
    // Queries.
    "\x1b[c", "\x1b[>c", "\x1b[6n", "\x1b[5n", "\x1b]10;?\x07", "\x1b]11;?\x1b\\", "\x1b[?2026$p", "\x1b[14t",
    "\x1b[18t", "\x1b[?u",
    // Title and colour setting.
    "\x1b]0;title\x07", "\x1b]11;rgb:12/34/56\x07",
];

#[derive(Debug, Clone)]
enum Op {
    Tokens(Vec<usize>),
    Bytes(Vec<u8>),
    Resize(u16, u16),
    ExpireSync,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => prop::collection::vec(0..TOKENS.len(), 1..8).prop_map(Op::Tokens),
        2 => prop::collection::vec(any::<u8>(), 1..48).prop_map(Op::Bytes),
        // Includes sizes below the minimum.
        2 => (0u16..14, 0u16..8).prop_map(|(c, r)| Op::Resize(c, r)),
        1 => Just(Op::ExpireSync),
    ]
}

fn apply(screen: &mut Screen, op: &Op) {
    match op {
        Op::Tokens(tokens) => {
            for &t in tokens {
                screen.feed(TOKENS[t].as_bytes());
            }
        }
        Op::Bytes(bytes) => screen.feed(bytes),
        Op::Resize(cols, rows) => screen.resize(*cols, *rows),
        Op::ExpireSync => {
            if let Some(hold) = screen.sync_hold() {
                screen.expire_sync(hold.id);
            }
        }
    }
    screen.take_replies();
}

/// The ops as readable text, for failure messages.
fn describe(ops: &[Op]) -> String {
    let parts: Vec<String> = ops
        .iter()
        .map(|op| match op {
            Op::Tokens(tokens) => tokens
                .iter()
                .map(|&t| format!("{:?}", TOKENS[t]))
                .collect::<Vec<_>>()
                .join(" "),
            Op::Bytes(bytes) => format!("bytes {:?}", String::from_utf8_lossy(bytes)),
            Op::Resize(cols, rows) => format!("resize {cols}x{rows}"),
            Op::ExpireSync => "expire".to_owned(),
        })
        .collect();
    parts.join(" | ")
}

/// The one case `snapshot_ansi` documents it can't rebuild: a broken wide-char half in the last column with only
/// broken halves before it (so there's nothing whole to shift it into place from).
fn has_unrebuildable_row(screen: &Screen) -> bool {
    let (_, rows) = screen.size();
    (0..rows).any(|row| {
        let cells: Vec<Width> = screen.row(row).map(|cell| cell.width).collect();
        let broken = |i: usize| match cells[i] {
            Width::Single => false,
            Width::Wide => cells.get(i + 1) != Some(&Width::Spacer),
            Width::Spacer => i == 0 || cells[i - 1] != Width::Wide,
        };
        let last = cells.len() - 1;
        broken(last) && (0..last).all(broken)
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

    #[test]
    fn random_output_never_panics_and_snapshots_round_trip(
        cols in 0u16..14,
        rows in 0u16..8,
        ops in prop::collection::vec(op(), 0..40),
    ) {
        let log = format!("new {cols}x{rows} | {}", describe(&ops));
        let mut screen = Screen::new(cols, rows);
        for op in &ops {
            apply(&mut screen, op);
        }
        let (cols, rows) = screen.size();
        let mut copy = Screen::new(cols, rows);
        copy.feed(&screen.snapshot_ansi());

        prop_assert_eq!(copy.sync_hold().is_some(), screen.sync_hold().is_some(), "after {}", log);
        if !has_unrebuildable_row(&screen) {
            prop_assert_eq!(observe(&copy), observe(&screen), "after {}", log);
        }
    }
}
