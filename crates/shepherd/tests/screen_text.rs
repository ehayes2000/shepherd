//! A terminal's text as a user reads it (T52): what `_shepherd/session/screen` answers with.

use shepherd::screen_text::{row_text, screen_text};
use shepherd_term::Screen;

fn screen(cols: u16, rows: u16, bytes: &[u8]) -> Screen {
    let mut screen = Screen::new(cols, rows);
    screen.feed(bytes);
    screen
}

#[test]
fn control_sequences_are_stripped_and_trailing_blanks_trimmed() {
    let s = screen(
        20,
        2,
        b"\x1b[1;31mred\x1b[0m \x1b]0;title\x07plain\r\n\x1b[5Cindent  ",
    );
    let text = screen_text(&s, 0, usize::MAX);
    assert_eq!(text.lines, ["red plain", "     indent"]);
    assert_eq!((text.cols, text.rows, text.scrollback), (20, 2, 0));
    assert!(!text.truncated);
}

#[test]
fn wide_characters_and_combining_marks_are_kept_whole() {
    let s = screen(10, 1, "漢字x e\u{301}".as_bytes());
    assert_eq!(screen_text(&s, 0, usize::MAX).lines, ["漢字x é"]);
}

#[test]
fn every_row_of_the_screen_is_answered_blank_ones_too() {
    let s = screen(10, 3, b"top");
    assert_eq!(screen_text(&s, 0, usize::MAX).lines, ["top", "", ""]);
}

#[test]
fn scrollback_comes_first_oldest_first_and_only_as_much_as_kept() {
    let s = screen(10, 2, b"one\r\ntwo\r\nthree\r\nfour");
    let text = screen_text(&s, 1, usize::MAX);
    assert_eq!(text.lines, ["two", "three", "four"]);
    assert_eq!(text.scrollback, 1);
    let text = screen_text(&s, 50, usize::MAX);
    assert_eq!(text.lines, ["one", "two", "three", "four"]);
    assert_eq!(text.scrollback, 2);
}

#[test]
fn past_the_byte_budget_the_oldest_lines_are_left_out() {
    let s = screen(10, 2, b"one\r\ntwo\r\nthree\r\nfour");
    // "three\nfour" fits in 10 bytes; "two\nthree\nfour" would not.
    let text = screen_text(&s, 2, 10);
    assert_eq!(text.lines, ["three", "four"]);
    assert_eq!(text.scrollback, 0);
    assert!(text.truncated);
    // Each line break counts: "three" and "four" are 9 bytes, but not joined.
    let text = screen_text(&s, 2, 9);
    assert_eq!(text.lines, ["four"]);
    assert!(text.truncated);
    let text = screen_text(&s, 2, 4);
    assert_eq!(text.lines, ["four"]);
    assert!(text.truncated);
}

#[test]
fn a_row_skips_the_cell_a_wide_character_covers_and_makes_symbols_display_safe() {
    let cells = [
        ("漢", 2, false),
        ("", 1, false),
        ("a", 1, false),
        ("\u{202e}", 1, false),
        ("b", 1, false),
        (" ", 1, false),
    ];
    assert_eq!(row_text(cells), "漢a b");
}

#[test]
fn concealed_text_is_left_blank_keeping_what_follows_in_its_column() {
    let s = screen(30, 1, "pw: \x1b[8msecret 漢\x1b[0m ok".as_bytes());
    assert_eq!(screen_text(&s, 0, usize::MAX).lines, ["pw:           ok"]);
    let cells = [
        ("a", 1, false),
        ("漢", 2, true),
        ("", 1, true),
        ("b", 1, false),
    ];
    assert_eq!(row_text(cells), "a  b");
}
