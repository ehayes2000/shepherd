//! Replaying the recorded Claude and Codex sessions.

mod support;

use shepherd_term::{MouseEncoding, MouseTracking, Screen};
use support::{AGENTS, Segment, observe, replay, segments, text};

/// Replays and collects every reply, taking replies after each chunk like a host would.
fn replay_collecting_replies(segments: &[Segment], chunk: usize) -> (Screen, Vec<u8>) {
    let mut screen = Screen::new(segments[0].cols, segments[0].rows);
    let mut replies = Vec::new();
    for segment in segments {
        screen.resize(segment.cols, segment.rows);
        for piece in segment.bytes.chunks(chunk) {
            screen.feed(piece);
            replies.extend(screen.take_replies());
        }
    }
    (screen, replies)
}

#[test]
fn chunk_size_does_not_change_the_screen_or_replies() {
    for agent in AGENTS {
        let segments = segments(agent);
        for end in 1..=segments.len() {
            let (whole, whole_replies) = replay_collecting_replies(&segments[..end], usize::MAX);
            for chunk in [1, 7] {
                let (chunked, chunked_replies) = replay_collecting_replies(&segments[..end], chunk);
                let label = format!("{} in {chunk}-byte chunks", segments[end - 1].name);
                assert_eq!(observe(&chunked), observe(&whole), "{label}");
                assert_eq!(chunked_replies, whole_replies, "{label}");
            }
        }
    }
}

#[test]
fn claude_fixture_gets_kitty_and_device_attribute_replies() {
    let (_, replies) = replay_collecting_replies(&segments("claude"), usize::MAX);
    assert_eq!(String::from_utf8_lossy(&replies), "\x1b[?0u\x1b[?6c");
}

#[test]
fn codex_fixture_gets_replies_to_every_startup_query() {
    let (_, replies) = replay_collecting_replies(&segments("codex"), usize::MAX);
    let replies = String::from_utf8_lossy(&replies);
    for expected in ["R", "\x1b]10;rgb:", "\x1b]11;rgb:", "\x1b[?7u", "\x1b[?6c"] {
        assert!(
            replies.contains(expected),
            "missing {expected:?} in {replies:?}"
        );
    }
}

#[test]
fn claude_runs_on_the_alt_screen_with_mouse_paste_and_focus() {
    let screen = replay(&segments("claude")[..1], usize::MAX);
    let modes = screen.modes();
    assert!(modes.alt_screen);
    assert_eq!(modes.mouse.tracking, MouseTracking::Motion);
    assert_eq!(modes.mouse.encoding, MouseEncoding::Sgr);
    assert!(modes.bracketed_paste);
    assert!(modes.focus);
}

#[test]
fn codex_pushes_kitty_keyboard_flags() {
    let screen = replay(&segments("codex")[..1], usize::MAX);
    assert_eq!(screen.modes().kitty_keyboard, 7);
    assert!(screen.modes().alt_screen);
}

#[test]
fn claude_shows_the_unicode_prompt_line() {
    let screen = replay(&segments("claude")[..1], usize::MAX);
    let joined = text(&screen).join("\n");
    assert!(
        joined.contains("日本語 テスト 🐑 ✅ café — done"),
        "{joined}"
    );
}

#[test]
fn exiting_leaves_the_alt_screen_and_resets_input_modes() {
    for agent in AGENTS {
        let screen = replay(&segments(agent), usize::MAX);
        let modes = screen.modes();
        assert!(!modes.alt_screen, "{agent}");
        assert_eq!(modes.mouse.tracking, MouseTracking::Off, "{agent}");
        assert!(!modes.bracketed_paste, "{agent}");
    }
}
