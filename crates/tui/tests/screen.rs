//! Drawing a `shepherd_term::Screen` into a ratatui `Buffer`.

#[path = "../../term/tests/support/mod.rs"]
mod support;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use shepherd_term::Screen;
use shepherd_tui::render_screen;
use support::{AGENTS, replay, segments};

fn screen_with(cols: u16, rows: u16, bytes: &[u8]) -> Screen {
    let mut screen = Screen::new(cols, rows);
    screen.feed(bytes);
    screen
}

fn render(screen: &Screen) -> Buffer {
    let (cols, rows) = screen.size();
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);
    render_screen(screen, area, &mut buf);
    buf
}

#[test]
fn text_lands_in_the_matching_cells() {
    let buf = render(&screen_with(5, 2, b"ab\r\ncd"));
    assert_eq!(buf, Buffer::with_lines(["ab   ", "cd   "]));
}

#[test]
fn truecolor_maps_to_rgb() {
    let buf = render(&screen_with(3, 1, b"\x1b[38;2;1;2;3;48;2;4;5;6mx"));
    let cell = &buf[(0, 0)];
    assert_eq!(cell.fg, Color::Rgb(1, 2, 3));
    assert_eq!(cell.bg, Color::Rgb(4, 5, 6));
}

#[test]
fn default_colors_map_to_reset() {
    let buf = render(&screen_with(3, 1, b"x"));
    assert_eq!(
        (buf[(0, 0)].fg, buf[(0, 0)].bg),
        (Color::Reset, Color::Reset)
    );
}

#[test]
fn ansi_and_indexed_colors_map_to_their_palette_entries() {
    let buf = render(&screen_with(
        4,
        1,
        b"\x1b[31;47ma\x1b[91;100mb\x1b[38;5;200;48;5;17mc",
    ));
    assert_eq!((buf[(0, 0)].fg, buf[(0, 0)].bg), (Color::Red, Color::Gray));
    assert_eq!(
        (buf[(1, 0)].fg, buf[(1, 0)].bg),
        (Color::LightRed, Color::DarkGray)
    );
    assert_eq!(
        (buf[(2, 0)].fg, buf[(2, 0)].bg),
        (Color::Indexed(200), Color::Indexed(17))
    );
}

#[test]
fn attributes_map_to_modifiers() {
    let buf = render(&screen_with(4, 1, b"\x1b[1;2;3;4;7;8;9mx\x1b[0;4:3my"));
    let all = Modifier::BOLD
        | Modifier::DIM
        | Modifier::ITALIC
        | Modifier::UNDERLINED
        | Modifier::REVERSED
        | Modifier::HIDDEN
        | Modifier::CROSSED_OUT;
    assert_eq!(buf[(0, 0)].modifier, all);
    assert_eq!(buf[(1, 0)].modifier, Modifier::UNDERLINED);
    assert_eq!(buf[(2, 0)].modifier, Modifier::empty());
}

#[test]
fn wide_chars_take_two_cells() {
    let buf = render(&screen_with(6, 1, "日本x".as_bytes()));
    assert_eq!(buf, Buffer::with_lines(["日本x "]));
    assert_eq!(buf[(0, 0)].symbol(), "日");
    assert_eq!(buf[(2, 0)].symbol(), "本");
    assert_eq!(buf[(4, 0)].symbol(), "x");
}

#[test]
fn combining_marks_stay_with_their_base_character() {
    let buf = render(&screen_with(3, 1, "e\u{301}x".as_bytes()));
    assert_eq!(buf[(0, 0)].symbol(), "e\u{301}");
    assert_eq!(buf[(1, 0)].symbol(), "x");
}

#[test]
fn tabs_render_as_blanks() {
    let buf = render(&screen_with(10, 1, b"\tx"));
    assert_eq!(buf[(0, 0)].symbol(), " ");
    assert_eq!(buf[(8, 0)].symbol(), "x");
}

#[test]
fn drawing_is_offset_to_the_area_and_clipped_to_it() {
    let screen = screen_with(4, 2, b"abcd\r\nefgh");
    let mut buf = Buffer::empty(Rect::new(0, 0, 5, 2));
    render_screen(&screen, Rect::new(2, 1, 3, 1), &mut buf);
    assert_eq!(buf, Buffer::with_lines(["     ", "  abc"]));
}

#[test]
fn an_area_larger_than_the_screen_leaves_the_rest_untouched() {
    let screen = screen_with(2, 1, b"ab");
    let mut buf = Buffer::with_lines(["....", "...."]);
    render_screen(&screen, Rect::new(0, 0, 4, 2), &mut buf);
    assert_eq!(buf, Buffer::with_lines(["ab..", "...."]));
}

#[test]
fn a_wide_char_cut_by_the_area_edge_is_dropped() {
    let screen = screen_with(4, 1, "a日".as_bytes());
    let mut buf = Buffer::with_lines(["...."]);
    render_screen(&screen, Rect::new(0, 0, 2, 1), &mut buf);
    assert_eq!(buf, Buffer::with_lines(["a ..",]));
}

#[test]
fn fixtures_render_at_their_recorded_sizes() {
    for agent in AGENTS {
        let segments = segments(agent);
        for end in 1..=segments.len() {
            let screen = replay(&segments[..end], usize::MAX);
            let segment = &segments[end - 1];
            assert_eq!(screen.size(), (segment.cols, segment.rows));
            insta::assert_debug_snapshot!(segment.name.clone(), render(&screen));
        }
    }
}
