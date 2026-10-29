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
fn a_copy_taken_anywhere_in_a_fixture_follows_the_rest_of_it() {
    for agent in AGENTS {
        let first = &segments(agent)[0];
        // An odd stride, so the cuts land inside escape sequences and UTF-8 chars as well as between them.
        for cut in (0..first.bytes.len()).step_by(97) {
            let mut screen = Screen::new(first.cols, first.rows);
            screen.feed(&first.bytes[..cut]);
            let mut copy = copy_of(&screen);
            screen.feed(&first.bytes[cut..]);
            copy.feed(&first.bytes[cut..]);
            assert_eq!(
                observe(&copy),
                observe(&screen),
                "{} cut at byte {cut}",
                first.name
            );
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

/// Feeds `chunks` one call each, snapshots, then feeds `rest` to both the original and the copy. They must match
/// right after, and again once any held sync update is expired on both.
fn assert_continues(cols: u16, rows: u16, chunks: &[&[u8]], rest: &[u8]) {
    let mut screen = Screen::new(cols, rows);
    for chunk in chunks {
        screen.feed(chunk);
    }
    let mut copy = copy_of(&screen);
    screen.feed(rest);
    copy.feed(rest);
    assert_eq!(
        observe(&copy),
        observe(&screen),
        "after {chunks:?} + {rest:?}"
    );
    for s in [&mut screen, &mut copy] {
        if let Some(hold) = s.sync_hold() {
            s.expire_sync(hold.id);
        }
    }
    assert_eq!(
        observe(&copy),
        observe(&screen),
        "after {chunks:?} + {rest:?} and expiring"
    );
}

#[test]
fn a_snapshot_mid_escape_sequence_carries_the_unfinished_sequence() {
    // Splits a sync begin marker, so the copy must start holding at the same byte as the original.
    assert_continues(10, 3, &[b"x\x1b[?20"], b"26hab");
    assert_continues(10, 3, &[b"ab\x1b[3"], b"1mc");
    assert_continues(10, 3, &[b"ab\x1b"], b"[2Dc");
    assert_continues(10, 3, &[b"ab\x1b]0;tit"], b"le\x07c");
    assert_continues(10, 3, &[b"ab\x1b]0;title\x1b"], b"\\c");
    assert_continues(10, 3, &[b"ab\x1bP1$"], b"qm\x1b\\c");
}

#[test]
fn controls_inside_an_unfinished_sequence_are_not_run_twice() {
    assert_continues(10, 3, &[b"ab\x1b[3\t\x08"], b"Dc");
    assert_continues(10, 3, &[b"ab\x1b\r\n"], b"7c\x1b8d");
}

#[test]
fn sequences_ending_in_a_later_feed_are_followed() {
    // An ignored CSI ends without a callback.
    assert_continues(10, 3, &[b"ab\x1b[<", b"<5", b"mc\xe6"], b"\x97\xa5");
    assert_continues(10, 3, &[b"ab\x1b", b"[<<mc\xe6"], b"\x97\xa5");
    assert_continues(10, 3, &[b"ab\x1b[<", b"<5", b"m\n"], b"c");
    assert_continues(10, 3, &[b"ab\x1b]0;t", b"itle", b"\x07c\xe6"], b"\x97\xa5");
    assert_continues(10, 3, &[b"ab\x1b]0;t", b"it\x18c"], b"d");
    assert_continues(10, 3, &[b"ab\x1bP1$", b"q", b"m\x9cc"], b"d");
    assert_continues(10, 3, &[b"ab\x1b[3", b"1", b"mc\x1b[1"], b"Cd");
}

#[test]
fn a_snapshot_mid_utf8_char_carries_the_unfinished_char() {
    assert_continues(10, 3, &["a日".as_bytes(), b"\xe6\x97"], b"\xa5b");
    assert_continues(10, 3, &[b"a\xf0"], b"\x9f\x90\x91b");
    assert_continues(10, 3, &[b"a\xf0\x9f"], b"\x1b[31mb");
}

#[test]
fn finished_sequences_before_an_unfinished_one_are_not_replayed() {
    assert_continues(10, 3, &[b"abc\x1b[H\x1b[2@\xe6"], b"\x97\xa5");
    assert_continues(10, 3, &[b"abc\x1b[H\x1b[2@\x1b[1"], b"Cx");
    assert_continues(10, 3, &[b"abc\x1b[H\x1b[2@", b"\x1b[1"], b"Cx");
}

#[test]
fn an_unfinished_sequence_inside_a_sync_update_is_carried_over() {
    assert_continues(10, 3, &[b"\x1b[?2026hab\x1b[3"], b"1mc");
    assert_continues(10, 3, &[b"\x1b[?2026h", b"ab\xe6\x97"], b"\xa5");
    // The end marker itself split across feeds.
    assert_continues(10, 3, &[b"\x1b[?2026hab\x1b[?20"], b"26lc");
}

#[test]
fn an_unfinished_sequence_left_when_a_sync_update_ends_is_carried_over() {
    assert_continues(10, 3, &[b"\x1b[?2026hab\x1b[?2026l\x1b[3"], b"1mc");
    assert_continues(10, 3, &[b"\x1b[?2026hab", b"\x1b[?2026lc\xe6"], b"\x97\xa5");
    let mut screen = Screen::new(10, 3);
    screen.feed(b"\x1b[?2026hab\x1b[3");
    let id = screen.sync_hold().unwrap().id;
    screen.expire_sync(id);
    let mut copy = copy_of(&screen);
    screen.feed(b"1mc");
    copy.feed(b"1mc");
    assert_eq!(observe(&copy), observe(&screen));
}

#[test]
fn an_unfinished_sequence_before_a_new_sync_begin_is_carried_over() {
    // The end marker makes the parser run up to the new begin marker, leaving it inside the sequence.
    assert_continues(
        10,
        3,
        &[b"\x1b[?2026ha", b"\x1b[?2026l\x1b]0;ti\x1b[?2026hzz"],
        b"\x1b[?2026l",
    );
    assert_continues(
        10,
        3,
        &[b"\x1b[?2026ha", b"\x1b[?2026lb\x1b[3\x1b[?2026hzz"],
        b"y",
    );
    assert_continues(
        10,
        3,
        &[b"\x1b[?2026ha", b"\x1b[?2026lb\xe6\x1b[?2026hzz"],
        b"y",
    );
}

/// Feeds `intro` and then `len` bytes of `body`, `chunk` bytes at a time.
fn open_sequence(intro: &[u8], body: u8, len: usize, chunk: usize) -> Screen {
    let mut screen = Screen::new(10, 3);
    screen.feed(intro);
    let piece = vec![body; chunk];
    for _ in 0..len / chunk {
        screen.feed(&piece);
    }
    screen
}

#[test]
fn a_sequence_that_never_ends_costs_linear_work_and_a_bounded_snapshot() {
    let cases: [(&str, &[u8], u8); 6] = [
        ("OSC", b"\x1b]0;", b'x'),
        ("DCS", b"\x1bPq", b'x'),
        ("APC", b"\x1b_", b'x'),
        ("CSI params", b"\x1b[", b'1'),
        ("CSI ignored", b"\x1b[<", b'<'),
        ("ESC intermediates", b"\x1b", b' '),
    ];
    for (label, intro, body) in cases {
        for (len, chunk) in [(8 << 20, 4096), (256 << 10, 1)] {
            let screen = open_sequence(intro, body, len, chunk);
            let fed = (intro.len() + len) as u64;
            // Probing the start of the sequence, then one tracking parser reading every byte once.
            assert!(
                screen.tracking_work() <= 3 * fed,
                "{label} in {chunk}-byte feeds: {} bytes of work for {fed} fed",
                screen.tracking_work()
            );
            let snapshot = screen.snapshot_ansi();
            assert!(
                snapshot.len() < 80 * 1024,
                "{label}: snapshot is {} bytes",
                snapshot.len()
            );
        }
    }
}

/// Opens a sequence with `intro` and 70 000 bytes of `body` (past the carried cap), feeds `mid`, then snapshots and
/// feeds more `body` and `end` to both. The copy must match and the first row read `text`.
fn assert_over_long_ends_alike(intro: &[u8], body: u8, mid: &[u8], end: &[u8], text: &str) {
    let mut screen = open_sequence(intro, body, 70_000, 4096);
    screen.feed(mid);
    let mut copy = copy_of(&screen);
    for s in [&mut screen, &mut copy] {
        s.feed(&[body; 100]);
        s.feed(end);
    }
    let label = format!("{intro:?} {mid:?} {end:?}");
    assert_eq!(observe(&copy), observe(&screen), "{label}");
    assert_eq!(support::text(&screen)[0], text, "{label}");
}

#[test]
fn a_copy_taken_inside_an_over_long_sequence_ends_it_on_the_same_byte() {
    assert_over_long_ends_alike(b"\x1b]0;", b'x', b"", b"\x07ab", "ab");
    assert_over_long_ends_alike(b"\x1bPq", b'x', b"", b"\x1b\\ab", "ab");
    assert_over_long_ends_alike(b"\x1b[", b'1', b"", b"mab", "ab");
    assert_over_long_ends_alike(b"\x1b", b' ', b"", b"Bab", "ab");
    // A byte past the cap that sends the DCS header to its ignore state, so the 8-bit ST doesn't end it.
    assert_over_long_ends_alike(b"\x1bP", b'1', b"<q", b"data\x9cvisible", "");
    // A byte past the cap that makes the CSI ignored, so it doesn't turn bracketed paste on.
    assert_over_long_ends_alike(b"\x1b[?2004;", b'0', b"<", b"hX", "X");
    assert_over_long_ends_alike(b"\x1b[", b'0', b" ", b"5mX", "X");
}
