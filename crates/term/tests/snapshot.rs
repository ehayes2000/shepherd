//! `snapshot_ansi` redraws a screen: feeding it into a fresh screen of the same size reproduces what a viewer sees,
//! and the copy then reacts to further output the same way as the original.

mod support;

use shepherd_term::Screen;
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
