//! `Screen` behavior through its public API: cells, cursor, modes, replies, sync updates.

mod support;

use shepherd_term::{
    Attrs, Color, CursorShape, KITTY_STACK_CAP, MAX_COLS, MAX_ROWS, MIN_COLS, MIN_ROWS,
    MouseEncoding, MouseMode, MouseTracking, Screen, Underline, Width,
};
use support::text;

fn screen_with(cols: u16, rows: u16, bytes: &[u8]) -> Screen {
    let mut screen = Screen::new(cols, rows);
    screen.feed(bytes);
    screen
}

#[test]
fn fed_text_appears_in_cells_and_moves_the_cursor() {
    let screen = screen_with(10, 3, b"hi\r\nthere");
    assert_eq!(text(&screen), ["hi", "there", ""]);
    let cursor = screen.cursor();
    assert_eq!((cursor.col, cursor.row), (5, 1));
    assert!(cursor.visible);
}

#[test]
fn rows_have_one_cell_per_column() {
    let screen = Screen::new(10, 3);
    assert_eq!(screen.size(), (10, 3));
    assert_eq!(screen.row(2).count(), 10);
    assert_eq!(screen.row(3).count(), 0);
    assert_eq!(screen.cell(9, 2).map(|c| c.ch), Some(' '));
    assert_eq!(screen.cell(10, 0), None);
}

#[test]
fn cells_carry_colors_and_attributes() {
    let screen = screen_with(
        10,
        1,
        b"\x1b[1;3;4;38;2;10;20;30;48;5;200mA\x1b[0;2;7;9;31;102mB\x1b[0;4:3;58:2::1:2:3mC",
    );
    let a = screen.cell(0, 0).unwrap();
    assert_eq!(a.fg, Color::Rgb(10, 20, 30));
    assert_eq!(a.bg, Color::Indexed(200));
    assert_eq!(
        a.attrs,
        Attrs {
            bold: true,
            italic: true,
            underline: Underline::Single,
            ..Attrs::default()
        }
    );

    let b = screen.cell(1, 0).unwrap();
    assert_eq!((b.fg, b.bg), (Color::Ansi(1), Color::Ansi(10)));
    assert_eq!(
        b.attrs,
        Attrs {
            dim: true,
            inverse: true,
            strikeout: true,
            ..Attrs::default()
        }
    );

    let c = screen.cell(2, 0).unwrap();
    assert_eq!(c.attrs.underline, Underline::Curly);
    assert_eq!(c.underline_color, Some(Color::Rgb(1, 2, 3)));
    assert_eq!(screen.cell(3, 0).unwrap().fg, Color::Default);
}

#[test]
fn wide_characters_take_a_cell_and_a_spacer() {
    let screen = screen_with(10, 1, "日x".as_bytes());
    let cells: Vec<_> = screen.row(0).take(3).map(|c| (c.ch, c.width)).collect();
    assert_eq!(
        cells,
        [
            ('日', Width::Wide),
            (' ', Width::Spacer),
            ('x', Width::Single)
        ]
    );
}

#[test]
fn combining_marks_attach_to_the_previous_cell() {
    let screen = screen_with(10, 1, "e\u{301}x".as_bytes());
    let e = screen.cell(0, 0).unwrap();
    assert_eq!((e.ch, e.zerowidth), ('e', &['\u{301}'][..]));
    assert_eq!(screen.cell(1, 0).unwrap().ch, 'x');
}

#[test]
fn resize_changes_the_grid_size() {
    let mut screen = screen_with(10, 3, b"hello");
    screen.resize(20, 5);
    assert_eq!(screen.size(), (20, 5));
    assert_eq!(screen.row(4).count(), 20);
    assert_eq!(text(&screen)[0], "hello");
}

#[test]
fn sizes_below_the_minimum_are_clamped_to_two_columns_and_one_row() {
    let mut screen = Screen::new(0, 0);
    assert_eq!(screen.size(), (MIN_COLS, MIN_ROWS));
    assert_eq!((MIN_COLS, MIN_ROWS), (2, 1));
    screen.resize(1, 5);
    assert_eq!(screen.size(), (2, 5));
}

#[test]
fn sizes_above_the_maximum_are_clamped() {
    let mut screen = Screen::new(u16::MAX, u16::MAX);
    assert_eq!(screen.size(), (MAX_COLS, MAX_ROWS));
    assert_eq!((MAX_COLS, MAX_ROWS), (1000, 500));
    screen.resize(2000, 3);
    assert_eq!(screen.size(), (1000, 3));
}

#[test]
fn a_wide_char_on_the_narrowest_screen_does_not_panic() {
    let mut screen = Screen::new(0, 0);
    screen.feed("日".as_bytes());
    let mut narrow = Screen::new(1, 3);
    narrow.feed("日日日".as_bytes());
    assert_eq!(narrow.size(), (2, 3));
}

#[test]
fn resizing_to_one_column_with_wide_chars_in_scrollback_terminates() {
    let mut screen = Screen::new(8, 5);
    screen.feed("日\r\n\r\n\r\n\r\n\r\n\r\n".as_bytes());
    screen.resize(1, 5);
    assert_eq!(screen.size(), (2, 5));
}

#[test]
fn cursor_reports_shape_and_visibility() {
    let screen = screen_with(10, 3, b"\x1b[5 q\x1b[?25l\x1b[2;4H");
    let cursor = screen.cursor();
    assert_eq!((cursor.col, cursor.row), (3, 1));
    assert!(!cursor.visible);
    assert_eq!((cursor.shape, cursor.blinking), (CursorShape::Beam, true));
}

#[test]
fn modes_default_to_legacy_input() {
    let modes = Screen::new(10, 3).modes();
    assert_eq!(modes.kitty_keyboard, 0);
    assert_eq!(modes.mouse, MouseMode::default());
    assert!(!modes.bracketed_paste && !modes.focus && !modes.alt_screen && !modes.app_cursor);
}

#[test]
fn modes_follow_what_claude_turns_on() {
    let screen = screen_with(
        10,
        3,
        b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?1h",
    );
    let modes = screen.modes();
    assert!(modes.alt_screen);
    assert_eq!(
        modes.mouse,
        MouseMode {
            tracking: MouseTracking::Motion,
            encoding: MouseEncoding::Sgr
        }
    );
    assert!(modes.bracketed_paste);
    assert!(modes.focus);
    assert!(modes.app_cursor);
}

#[test]
fn kitty_keyboard_flags_follow_push_and_pop() {
    let mut screen = screen_with(10, 3, b"\x1b[>7u");
    assert_eq!(screen.modes().kitty_keyboard, 7);
    screen.feed(b"\x1b[<u");
    assert_eq!(screen.modes().kitty_keyboard, 0);
}

#[test]
fn mouse_tracking_and_encoding_variants() {
    let click = screen_with(10, 3, b"\x1b[?1000h\x1b[?1005h").modes().mouse;
    assert_eq!(
        click,
        MouseMode {
            tracking: MouseTracking::Click,
            encoding: MouseEncoding::Utf8
        }
    );
    let drag = screen_with(10, 3, b"\x1b[?1002h").modes().mouse;
    assert_eq!(
        drag,
        MouseMode {
            tracking: MouseTracking::Drag,
            encoding: MouseEncoding::X10
        }
    );
}

#[test]
fn leaving_the_alt_screen_restores_the_primary_screen() {
    let mut screen = screen_with(10, 3, b"shell\x1b[?1049h\x1b[Hagent");
    assert_eq!(text(&screen)[0], "agent");
    screen.feed(b"\x1b[?1049l");
    assert!(!screen.modes().alt_screen);
    assert_eq!(text(&screen)[0], "shell");
}

// Startup queries from the T02 report. Claude: `CSI c`, `CSI > 0 q`, `CSI ? u`, `OSC 7501;?`.
// Codex: `CSI 6 n`, `OSC 10;?`, `OSC 11;?`, `CSI ? u`, `CSI c`. XTVERSION and OSC 7501 get no answer (decision 0007).

#[test]
fn primary_device_attributes_get_a_reply() {
    let mut screen = screen_with(10, 3, b"\x1b[c");
    assert_eq!(screen.take_replies(), b"\x1b[?6c");
}

#[test]
fn replies_are_taken_once() {
    let mut screen = screen_with(10, 3, b"\x1b[c");
    assert!(!screen.take_replies().is_empty());
    assert!(screen.take_replies().is_empty());
}

#[test]
fn cursor_position_report_gets_a_one_based_reply() {
    let mut screen = screen_with(10, 5, b"\x1b[3;4H\x1b[6n");
    assert_eq!(screen.take_replies(), b"\x1b[3;4R");
}

#[test]
fn kitty_keyboard_query_reports_the_current_flags() {
    let mut screen = screen_with(10, 3, b"\x1b[?u\x1b[>7u\x1b[?u");
    assert_eq!(screen.take_replies(), b"\x1b[?0u\x1b[?7u");
}

#[test]
fn default_color_queries_get_replies_in_order() {
    let mut screen = screen_with(10, 3, b"\x1b]10;?\x1b\\\x1b]11;?\x07");
    let replies = String::from_utf8(screen.take_replies()).unwrap();
    assert_eq!(
        replies,
        "\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x07"
    );
}

#[test]
fn color_queries_report_colors_the_program_set() {
    let mut screen = screen_with(10, 3, b"\x1b]11;rgb:12/34/56\x07\x1b]11;?\x07");
    let replies = String::from_utf8(screen.take_replies()).unwrap();
    assert_eq!(replies, "\x1b]11;rgb:1212/3434/5656\x07");
}

#[test]
fn codex_startup_queries_all_get_replies() {
    let mut screen = screen_with(10, 3, b"\x1b[6n\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?u\x1b[c");
    let replies = String::from_utf8(screen.take_replies()).unwrap();
    assert_eq!(
        replies,
        "\x1b[1;1R\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?0u\x1b[?6c"
    );
}

#[test]
fn claude_startup_queries_get_replies_where_supported() {
    let mut screen = screen_with(10, 3, b"\x1b[c\x1b[>0q\x1b[?u\x1b]7501;?\x07");
    assert_eq!(screen.take_replies(), b"\x1b[?6c\x1b[?0u");
}

#[test]
fn output_inside_a_sync_update_is_held_until_the_end_marker() {
    let mut screen = screen_with(20, 3, b"\x1b[?2026hhello");
    assert_eq!(text(&screen)[0], "");
    assert!(screen.sync_hold().is_some());

    screen.feed(b" world\x1b[?2026l");
    assert_eq!(text(&screen)[0], "hello world");
    assert_eq!(screen.sync_hold(), None);
}

#[test]
fn held_output_is_released_when_the_injected_timeout_expires() {
    let mut screen = screen_with(10, 3, b"\x1b[?2026hhello");
    let hold = screen.sync_hold().unwrap();
    assert!(hold.timeout > std::time::Duration::ZERO);

    screen.expire_sync(hold.id);
    assert_eq!(text(&screen)[0], "hello");
    assert_eq!(screen.sync_hold(), None);

    screen.feed(b" more");
    assert_eq!(text(&screen)[0], "hello more");
}

#[test]
fn a_stale_timeout_does_not_release_a_newer_sync_update() {
    let mut screen = screen_with(10, 3, b"\x1b[?2026hone");
    let stale = screen.sync_hold().unwrap().id;
    screen.feed(b"\x1b[?2026l\x1b[?2026htwo");
    let current = screen.sync_hold().unwrap().id;

    screen.expire_sync(stale);
    assert_eq!(text(&screen)[0], "one");
    screen.expire_sync(current);
    assert_eq!(text(&screen)[0], "onetwo");
}

#[test]
fn a_new_begin_marker_restarts_the_timeout() {
    let mut screen = screen_with(10, 3, b"\x1b[?2026ha");
    let first = screen.sync_hold().unwrap().id;
    screen.feed(b"\x1b[?2026hb");
    let second = screen.sync_hold().unwrap().id;
    assert_ne!(first, second);

    screen.expire_sync(first);
    assert_eq!(text(&screen)[0], "");
    screen.expire_sync(second);
    assert_eq!(text(&screen)[0], "ab");
}

#[test]
fn queries_inside_a_sync_update_are_answered_when_it_ends() {
    let mut screen = screen_with(10, 3, b"\x1b[?2026h\x1b[c");
    assert!(screen.take_replies().is_empty());
    screen.feed(b"\x1b[?2026l");
    assert_eq!(screen.take_replies(), b"\x1b[?6c");
}

#[test]
fn screen_can_move_between_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<Screen>();
}

#[test]
fn reported_colors_can_be_set_by_the_caller() {
    let mut screen = Screen::new(10, 3);
    screen.set_reported_colors((0x11, 0x22, 0x33), (0xfe, 0xdc, 0xba));
    screen.feed(b"\x1b]10;?\x07\x1b]11;?\x07");
    let replies = String::from_utf8(screen.take_replies()).unwrap();
    assert_eq!(
        replies,
        "\x1b]10;rgb:1111/2222/3333\x07\x1b]11;rgb:fefe/dcdc/baba\x07"
    );
}

#[test]
fn colors_the_program_set_win_over_reported_colors() {
    let mut screen = Screen::new(10, 3);
    screen.set_reported_colors((1, 1, 1), (2, 2, 2));
    screen.feed(b"\x1b]11;rgb:12/34/56\x07\x1b]11;?\x07");
    let replies = String::from_utf8(screen.take_replies()).unwrap();
    assert_eq!(replies, "\x1b]11;rgb:1212/3434/5656\x07");
}

// alacritty 0.26 panics when the kitty keyboard stack is full (F23): it removes from the title stack instead.

/// More pushes than alacritty's stack limit (4096).
const FLOOD: usize = 5000;

fn repeated(seq: &[u8], times: usize) -> Vec<u8> {
    seq.repeat(times)
}

#[test]
fn kitty_push_flood_does_not_panic_and_still_reports_the_top() {
    let mut screen = screen_with(10, 3, &repeated(b"\x1b[>1u", FLOOD));
    assert_eq!(screen.modes().kitty_keyboard, 1);
    screen.feed(b"\x1b[?u");
    assert_eq!(screen.take_replies(), b"\x1b[?1u");
}

#[test]
fn kitty_push_flood_split_into_byte_chunks_does_not_panic() {
    let mut screen = Screen::new(10, 3);
    for byte in repeated(b"\x1b[>1u", FLOOD) {
        screen.feed(&[byte]);
    }
    screen.feed(b"\x1b[?u");
    assert_eq!(screen.take_replies(), b"\x1b[?1u");
}

#[test]
fn kitty_pushes_work_again_after_a_pop_from_a_full_stack() {
    let mut screen = screen_with(10, 3, &repeated(b"\x1b[>1u", FLOOD));
    screen.feed(b"\x1b[<u\x1b[>7u\x1b[?u");
    assert_eq!(screen.take_replies(), b"\x1b[?7u");
}

#[test]
fn kitty_stack_keeps_every_entry_up_to_the_cap_and_drops_pushes_past_it() {
    let below = KITTY_STACK_CAP - 1;
    let mut bytes = b"\x1b[>7u".to_vec();
    bytes.extend(repeated(b"\x1b[>1u", below));
    bytes.extend_from_slice(b"\x1b[>5u\x1b[?u");
    bytes.extend_from_slice(format!("\x1b[<{below}u\x1b[?u").as_bytes());
    let mut screen = screen_with(10, 3, &bytes);
    assert_eq!(screen.take_replies(), b"\x1b[?1u\x1b[?7u");
}

#[test]
fn kitty_push_flood_on_the_alt_screen_does_not_panic() {
    let mut bytes = repeated(b"\x1b[>1u", FLOOD);
    bytes.extend_from_slice(b"\x1b[?1049h");
    bytes.extend(repeated(b"\x1b[>3u", FLOOD));
    bytes.extend_from_slice(b"\x1b[?u\x1b[?1049l\x1b[?u");
    let mut screen = screen_with(10, 3, &bytes);
    assert_eq!(screen.take_replies(), b"\x1b[?3u\x1b[?1u");
}

#[test]
fn kitty_pushes_work_again_after_a_reset_of_a_full_stack() {
    let mut bytes = repeated(b"\x1b[>1u", FLOOD);
    bytes.extend_from_slice(b"\x1bc\x1b[>7u\x1b[?u");
    let mut screen = screen_with(10, 3, &bytes);
    assert_eq!(screen.take_replies(), b"\x1b[?7u");
}

#[test]
fn kitty_push_flood_inside_a_synchronized_update_does_not_panic() {
    let mut bytes = b"\x1b[?2026h".to_vec();
    bytes.extend(repeated(b"\x1b[>1u", FLOOD));
    bytes.extend_from_slice(b"\x1b[?2026l\x1b[?u");
    let mut screen = screen_with(10, 3, &bytes);
    assert_eq!(screen.take_replies(), b"\x1b[?1u");
}

#[test]
fn kitty_pop_past_the_bottom_does_not_panic() {
    let mut screen = screen_with(10, 3, b"\x1b[>7u\x1b[<65535u\x1b[<u\x1b[?u");
    assert_eq!(screen.take_replies(), b"\x1b[?0u");
}

#[test]
fn title_push_flood_does_not_panic() {
    let mut bytes = repeated(b"\x1b[22t", FLOOD);
    bytes.extend(repeated(b"\x1b[23t", FLOOD + 1));
    bytes.extend(repeated(b"\x1b[22;0t", FLOOD));
    let mut screen = screen_with(10, 3, &bytes);
    screen.feed(b"ok");
    assert_eq!(text(&screen)[0], "ok");
}

#[test]
fn kitty_push_flood_with_titles_on_their_stack_does_not_panic() {
    let mut bytes = repeated(b"\x1b[22t", 10);
    bytes.extend(repeated(b"\x1b[>1u", FLOOD));
    bytes.extend_from_slice(b"\x1b[?u");
    let mut screen = screen_with(10, 3, &bytes);
    assert_eq!(screen.take_replies(), b"\x1b[?1u");
}

#[test]
fn kitty_push_flood_released_by_the_sync_timeout_does_not_panic() {
    let mut bytes = b"\x1b[?2026h".to_vec();
    bytes.extend(repeated(b"\x1b[>1u", FLOOD));
    let mut screen = screen_with(10, 3, &bytes);
    let hold = screen.sync_hold().expect("an open synchronized update");
    screen.expire_sync(hold.id);
    screen.feed(b"\x1b[?u");
    assert_eq!(screen.take_replies(), b"\x1b[?1u");
}

#[test]
fn popping_many_kitty_entries_frees_as_many_slots() {
    let cap = KITTY_STACK_CAP;
    let mut bytes = repeated(b"\x1b[>1u", cap);
    bytes.extend_from_slice(format!("\x1b[<{cap}u").as_bytes());
    bytes.extend_from_slice(b"\x1b[>2u\x1b[>7u\x1b[?u");
    let mut screen = screen_with(10, 3, &bytes);
    assert_eq!(screen.take_replies(), b"\x1b[?7u");
}
