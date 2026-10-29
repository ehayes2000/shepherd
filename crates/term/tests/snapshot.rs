//! `snapshot_ansi` redraws a screen: feeding it into a fresh screen of the same size reproduces what a viewer sees,
//! and the copy then reacts to further output the same way as the original.

mod support;

use shepherd_term::{Screen, Width};
use support::{AGENTS, observe, replay, segments};

fn copy_of(screen: &Screen) -> Screen {
    let (cols, rows) = screen.size();
    let mut copy = Screen::new(cols, rows);
    copy.feed(&screen.snapshot_ansi());
    copy
}

fn assert_round_trips(screen: &Screen, label: &str) {
    let copy = copy_of(screen);
    assert_eq!(
        observe(&copy),
        observe(screen),
        "snapshot round trip differs: {label}"
    );
}

/// Feeds `rest` to both the original and its copy and checks they still match.
fn assert_same_after(original: &[u8], cols: u16, rows: u16, rest: &[u8]) {
    let mut screen = Screen::new(cols, rows);
    screen.feed(original);
    let mut copy = copy_of(&screen);
    screen.feed(rest);
    copy.feed(rest);
    assert_eq!(observe(&copy), observe(&screen));
}

#[test]
fn every_fixture_segment_round_trips() {
    for agent in AGENTS {
        let segments = segments(agent);
        for end in 1..=segments.len() {
            let screen = replay(&segments[..end], usize::MAX);
            assert_round_trips(&screen, &segments[end - 1].name);
        }
    }
}

#[test]
fn snapshot_of_a_blank_screen_round_trips() {
    assert_round_trips(&Screen::new(10, 3), "blank");
}

#[test]
fn styled_cells_round_trip() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"\x1b[1;2;3;4;5;7;8;9;31;42mA\x1b[0;4:2;38;5;100;48;2;1;2;3mB");
    screen.feed(b"\x1b[0;4:3;58:5:9;95;104mC\x1b[0;4:4;58:2::4:5:6mD\x1b[0;4:5;39;49mE");
    screen.feed("\x1b[0m\r\n日本 e\u{301} \t|".as_bytes());
    assert_round_trips(&screen, "styled");
}

#[test]
fn modes_and_cursor_state_round_trip() {
    let mut screen = Screen::new(20, 5);
    screen.feed(b"\x1b[?1049h\x1b[>5u\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?1h");
    screen.feed(b"\x1b[3 q\x1b[?25l\x1b[4;7Hx");
    assert_round_trips(&screen, "modes");
}

#[test]
fn utf8_mouse_and_click_tracking_round_trip() {
    let mut screen = Screen::new(20, 5);
    screen.feed(b"\x1b[?1000h\x1b[?1005h");
    assert_round_trips(&screen, "mouse");
}

#[test]
fn primary_screen_round_trips() {
    let mut screen = Screen::new(10, 4);
    screen.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
    assert_round_trips(&screen, "scrolled primary");
}

#[test]
fn soft_wrapped_lines_reflow_the_same_after_a_round_trip() {
    let mut screen = Screen::new(5, 4);
    screen.feed(b"abcdefgh");
    let mut copy = copy_of(&screen);
    screen.resize(10, 4);
    copy.resize(10, 4);
    assert_eq!(observe(&copy), observe(&screen));
    assert_eq!(support::text(&screen)[0], "abcdefgh");
}

#[test]
fn a_pending_wrap_survives_the_round_trip() {
    assert_same_after(b"abcde", 5, 3, b"X");
}

#[test]
fn a_wide_char_wrapped_from_the_last_column_round_trips() {
    let mut screen = Screen::new(5, 3);
    screen.feed("abcd日".as_bytes());
    assert_round_trips(&screen, "leading wide spacer");
    assert_same_after("abcd日".as_bytes(), 5, 3, b"z");

    let mut copy = copy_of(&screen);
    screen.resize(10, 3);
    copy.resize(10, 3);
    assert_eq!(observe(&copy), observe(&screen));
}

#[test]
fn the_current_pen_survives_the_round_trip() {
    assert_same_after(b"\x1b[1;38;2;9;8;7mbold", 10, 3, b"more");
}

#[test]
fn the_saved_cursor_survives_the_round_trip() {
    assert_same_after(b"\x1b[2;3H\x1b[32m\x1b7\x1b[0m\x1b[3;1Hx", 10, 4, b"\x1b8y");
}

#[test]
fn kitty_keyboard_stack_survives_the_round_trip() {
    let mut screen = Screen::new(10, 3);
    screen.feed(b"\x1b[>7u");
    let mut copy = copy_of(&screen);
    screen.feed(b"\x1b[<u");
    copy.feed(b"\x1b[<u");
    assert_eq!(copy.modes(), screen.modes());
    assert_eq!(copy.modes().kitty_keyboard, 0);
}

#[test]
fn leaving_the_alt_screen_after_a_round_trip_matches_modes() {
    assert_same_after(
        b"\x1b[?1049h\x1b[?2004hagent",
        10,
        3,
        b"\x1b[?2004l\x1b[?1049l",
    );
}

#[test]
fn snapshot_into_a_used_screen_replaces_its_contents() {
    let mut original = Screen::new(10, 3);
    original.feed(b"fresh");
    let mut used = Screen::new(10, 3);
    used.feed(b"\x1b[?1049h\x1b[?2004h\x1b[>1ustale junk\x1b[31m");
    used.feed(&original.snapshot_ansi());
    assert_eq!(observe(&used), observe(&original));
}

#[test]
fn a_pending_wrap_left_behind_by_a_widening_resize_is_not_reprinted() {
    let mut screen = Screen::new(7, 5);
    screen.feed("日a".as_bytes());
    screen.resize(2, 4);
    screen.resize(10, 3);
    assert_round_trips(&screen, "stale pending wrap");
}

#[test]
fn half_erased_wide_chars_round_trip() {
    let cases: [(&str, &str); 6] = [
        ("wide whose spacer was erased", "日\x1b[1;2H\x1b[X"),
        (
            "wide whose spacer was overwritten after erasing",
            "日\x1b[1;2H\x1b[Xx",
        ),
        ("spacer whose wide char was erased", "日\x1b[1;1H\x1b[X"),
        (
            "erase-to-start ending on a wide char",
            "ab日\x1b[1;3H\x1b[1K",
        ),
        ("insert splitting a wide char", "日\x1b[1;2H\x1b[@"),
        (
            "insert pushing a wide char into the last column",
            "abc日\x1b[1;4H\x1b[@",
        ),
    ];
    for (label, bytes) in cases {
        let bytes = format!("\x1b[44m{bytes}");
        let mut screen = Screen::new(5, 2);
        screen.feed(bytes.as_bytes());
        assert_round_trips(&screen, label);
        assert_same_after(bytes.as_bytes(), 5, 2, b"\x1b[2;1Hzz");
    }
}

#[test]
fn output_held_by_an_open_sync_update_is_carried_over() {
    let mut screen = Screen::new(10, 3);
    screen.feed(b"shown\x1b[?2026h\r\nheld");
    let mut copy = copy_of(&screen);
    assert_eq!(observe(&copy), observe(&screen));
    assert!(copy.sync_hold().is_some());

    screen.feed(b" more\x1b[?2026l");
    copy.feed(b" more\x1b[?2026l");
    assert_eq!(observe(&copy), observe(&screen));
    assert_eq!(support::text(&screen)[1], "held more");
}

#[test]
fn held_output_after_an_end_and_a_new_begin_is_carried_over() {
    let mut screen = Screen::new(10, 3);
    screen.feed(b"\x1b[?2026hone\x1b[?2026l\x1b[?2026htwo");
    let mut copy = copy_of(&screen);
    let (a, b) = (screen.sync_hold().unwrap().id, copy.sync_hold().unwrap().id);
    screen.expire_sync(a);
    copy.expire_sync(b);
    assert_eq!(observe(&copy), observe(&screen));
    assert_eq!(support::text(&screen)[0], "onetwo");
}

/// A step in a fuzz-found sequence: feed text, or resize (`"R<cols>x<rows>"`).
fn run(cols: u16, rows: u16, steps: &[&str]) -> Screen {
    let mut screen = Screen::new(cols, rows);
    for step in steps {
        match step.strip_prefix('R').and_then(|size| size.split_once('x')) {
            Some((c, r)) => {
                let size = |n: &str| {
                    n.parse()
                        .unwrap_or_else(|e| panic!("bad size in {step}: {e}"))
                };
                screen.resize(size(c), size(r));
            }
            None => screen.feed(step.as_bytes()),
        }
    }
    screen
}

#[test]
fn sequences_found_by_the_fuzz_test_round_trip() {
    let cases: &[(u16, u16, &[&str])] = &[
        // A leading spacer left behind when its wide char was repeated (REP) and the row then unwrapped.
        (
            0,
            0,
            &[
                "a",
                "\x1b[38;2;1;2;3m",
                "日",
                "\r",
                "\x1b[3b",
                "\0",
                "\x1b[>7u",
                "R3x2",
                "\x1b7",
            ],
        ),
        // Tab cells at the end of a wrapped row.
        (9, 0, &["\t               \t", "R6x2"]),
        // A spacer whose wide char was erased, in the last column of a wrapped row.
        (0, 0, &["日", "a", "R0x2", "\x1b[2;4r", "\x1b[1K"]),
        // A spacer with only a tab cell before it.
        (
            2,
            1,
            &[
                "a",
                "a",
                "\x1b[?6h",
                "\x1b[44m",
                "\x1b[5;3H",
                "R6x0",
                "\x1b[3J",
                "e\u{301}",
                "    ",
                "\x1b[?2026h",
                "\r",
                "日",
                "\x1b[?1049l",
                "R5x2",
                "\x1b[?2026l",
                "\x1b[?2026l",
                "R3x3",
                "\r",
                "\x1b[1K",
                "\t",
            ],
        ),
        // A spacer pulled into column 0 by DCH.
        (
            0,
            0,
            &[
                "a", "\x1b[4h", "\r", "R0x0", "日", "R4x0", "\x1b[3b", "\x1b8", "\x1b[3P",
            ],
        ),
        // A tab over a spacer.
        (0, 0, &["a", "a", "日", "\x1b[5;3H", "\t"]),
        // Two spacers in a row at the end of a line.
        (
            4,
            4,
            &[
                "                         ",
                "a",
                "\x1b8",
                "R6x0",
                "\x1b[4h",
                "a",
                "日",
                "\t",
                "R9x2",
                "\x1b[3D",
                "日",
                "R7x0",
                "   ",
            ],
        ),
        // A wide char in the last column with the cursor waiting to wrap after it.
        (
            0,
            0,
            &[
                "R3x0",
                "日",
                "\x1b[?1049h",
                "R10x0",
                "               ",
                "R8x0",
                "\x08",
                "日",
                "\x1b7",
                "R7x0",
            ],
        ),
        // A wide char in the last column after whole wide chars.
        (
            0,
            0,
            &["\x1b[?1049h", "R6x0", "日", "\t", "\x1b[3b", "R5x0"],
        ),
        // A wide char in the last column after a single blank.
        (
            0,
            0,
            &[
                "R3x0",
                "日",
                "\x1b[?1049h",
                "R12x0",
                "                 ",
                "\x08",
                "日",
                "a",
                "R7x0",
            ],
        ),
        // A leading spacer that is no longer in the last column.
        (
            0,
            0,
            &[
                "R0x3",
                "\x1b[?1049h",
                "\x1b[44m",
                "日",
                "a",
                "日",
                "R4x3",
                "\x1b[2;9H",
                "日",
            ],
        ),
    ];
    for (i, (cols, rows, steps)) in cases.iter().enumerate() {
        let screen = run(*cols, *rows, steps);
        assert_round_trips(&screen, &format!("case {i}: {steps:?}"));
    }
}

#[test]
fn a_broken_wide_half_with_nothing_whole_before_it_comes_out_blank() {
    // Two wide chars on a two-column row, neither with a spacer. There is no edit sequence that rebuilds the
    // second one, so the copy shows a blank there instead (documented on `snapshot_ansi`).
    let screen = run(
        0,
        0,
        &[
            "R4x0",
            "\x1b[?1049h",
            "\x1b[2;4r",
            "日",
            "\x1b[4h",
            "\x08",
            "日",
            "\t",
            "R0x0",
        ],
    );
    let widths: Vec<_> = screen.row(0).map(|c| c.width).collect();
    assert_eq!(widths, [Width::Wide, Width::Wide]);
    let copy = copy_of(&screen);
    let widths: Vec<_> = copy.row(0).map(|c| (c.ch, c.width)).collect();
    assert_eq!(widths, [('日', Width::Wide), (' ', Width::Single)]);
}
