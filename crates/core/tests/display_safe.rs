//! One rule for text from agents, hooks, git and other clients before it is drawn or put in an alert (F35), through
//! the public `shepherd_core::display` API.

use std::cell::Cell;

use proptest::prelude::*;
use shepherd_core::display::{CHARS_READ_PER_GRAPHEME, display_safe, display_safe_iter};
use unicode_segmentation::UnicodeSegmentation;

/// Room for every case below: none is cut by length unless it says so.
const ROOMY: usize = 200;

fn shown(text: &str) -> String {
    display_safe(text, ROOMY).as_str().to_owned()
}

/// Cc and Cf, the categories that must never be shown.
fn invisible(c: char) -> bool {
    use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
    matches!(
        c.general_category(),
        GeneralCategory::Control | GeneralCategory::Format
    )
}

#[test]
fn visible_text_punctuation_and_symbols_are_kept() {
    let cases = [
        "fix-login-redirect-bug",
        "git: fatal: 'origin' does not appear to be a git repository",
        "/Users/me/Code/羊飼い — “quoted” 🚀 → ok…",
        "hook post-create failed (exit 1): npm ERR! 404",
        "👍🏽 🇯🇵 ❤\u{fe0f}",
        "",
    ];
    for text in cases {
        assert_eq!(shown(text), text, "{text:?}");
    }
}

#[test]
fn trojan_source_bidi_controls_are_dropped() {
    // The Trojan Source attacks (CVE-2021-42574): overrides, embeddings, isolates and marks reorder what is shown.
    let cases: [(&str, &str); 6] = [
        (
            "/*\u{202E} } \u{2066}if (isAdmin)\u{2069} \u{2066} begin admins only */",
            "/* } if (isAdmin)  begin admins only */",
        ),
        (
            "access\u{202E}\u{2066}// check\u{2069}\u{2066}level",
            "access// checklevel",
        ),
        ("fix\u{202A}\u{202B}\u{202C}\u{202D}ed", "fixed"),
        ("\u{2067}rtl\u{2068}first\u{2069}", "rtlfirst"),
        ("mark\u{200E}\u{200F}\u{61C}s", "marks"),
        ("evil.\u{202E}txt.exe", "evil.txt.exe"),
    ];
    for (text, expected) in cases {
        assert_eq!(shown(text), expected, "{text:?}");
    }
}

#[test]
fn invisible_format_characters_are_dropped() {
    let cases: [(&str, &str); 7] = [
        // Joiners and zero-width space.
        ("a\u{200D}b\u{200C}c\u{200B}d", "abcd"),
        // A ZWJ emoji sequence falls apart into its emoji.
        ("👨\u{200D}👩\u{200D}👧", "👨👩👧"),
        // BOM, word joiner, soft hyphen.
        ("\u{FEFF}co\u{AD}op\u{2060}erate", "cooperate"),
        // Tags, which can hide a whole ASCII message.
        ("tag\u{E0001}\u{E0068}\u{E0069}\u{E007F}ged", "tagged"),
        // Interlinear annotation.
        ("\u{FFF9}note\u{FFFA}hidden\u{FFFB}", "notehidden"),
        // Invisible math operators and Arabic number sign.
        ("f\u{2061}x\u{2062}y \u{600}1", "fxy 1"),
        ("only\u{200B}\u{200B}\u{200B}", "only"),
    ];
    for (text, expected) in cases {
        assert_eq!(shown(text), expected, "{text:?}");
    }
}

#[test]
fn control_characters_are_dropped() {
    let cases: [(&str, &str); 4] = [
        ("\x1b[31mred\x1b[0m", "[31mred[0m"),
        ("a\x1b]9;evil\x07b", "a]9;evilb"),
        ("tab\there\nnext\r\nline\u{0}\u{7f}", "tabherenextline"),
        // C1: CSI and ST where a terminal reads them.
        ("a\u{9b}b\u{9c}c\u{85}d", "abcd"),
    ];
    for (text, expected) in cases {
        assert_eq!(shown(text), expected, "{text:?}");
    }
}

#[test]
fn text_is_shown_in_nfc() {
    assert_eq!(shown("cre\u{300}me bru\u{302}le\u{301}e"), "crème brûlée");
    // `x́` has no precomposed form: it stays a base and its mark.
    assert_eq!(shown("x\u{301}"), "x\u{301}");
}

#[test]
fn combining_marks_are_capped_per_base() {
    let cases: [(String, String); 5] = [
        // Zalgo: two marks stay on each base, after composing.
        (
            format!("a{}", "\u{301}".repeat(100)),
            "á\u{301}\u{301}".to_owned(),
        ),
        (
            "Z\u{334}\u{321}\u{31b}\u{34e}a\u{337}\u{33a}\u{35a}\u{316}l\u{338}g\u{336}\u{30c}\u{30c}\u{30c}o".to_owned(),
            "Z\u{334}\u{321}a\u{337}\u{33a}l\u{338}\u{1e7}\u{336}\u{30c}o".to_owned(),
        ),
        // Marks with no base before them are dropped.
        ("\u{301}\u{302}abc".to_owned(), "abc".to_owned()),
        // A dropped character doesn't make room for more marks.
        (
            "x\u{301}\u{302}\u{200B}\u{303}\u{304}".to_owned(),
            "x\u{301}\u{302}".to_owned(),
        ),
        // Devanagari vowel signs and viramas are marks, at most two per letter.
        ("नमस्ते".to_owned(), "नमस्ते".to_owned()),
    ];
    for (text, expected) in cases {
        assert_eq!(shown(&text), expected, "{text:?}");
    }
}

#[test]
fn text_is_capped_by_graphemes_never_inside_one() {
    let cases: [(&str, usize, &str); 6] = [
        ("abcdef", 3, "abc"),
        ("abc", 3, "abc"),
        ("abc", 0, ""),
        ("e\u{301}x\u{301}y", 2, "éx\u{301}"),
        ("🇯🇵🇺🇸", 1, "🇯🇵"),
        ("👍🏽👍🏽", 1, "👍🏽"),
    ];
    for (text, max, expected) in cases {
        assert_eq!(
            display_safe(text, max).as_str(),
            expected,
            "{text:?} at {max}"
        );
    }
}

/// How many chars `display_safe_iter` reads of an endless `chars`, kept to `max` graphemes.
fn chars_read(chars: impl Iterator<Item = char>, max: usize) -> (usize, String) {
    let read = Cell::new(0);
    let shown = display_safe_iter(chars.inspect(|_| read.set(read.get() + 1)), max);
    (read.get(), shown.as_str().to_owned())
}

#[test]
fn huge_inputs_are_read_only_as_far_as_the_cap_needs() {
    let max = 40;
    let budget = max * CHARS_READ_PER_GRAPHEME;
    // Endless inputs: if any of these read without bound, the test never ends.
    let (read, text) = chars_read(std::iter::repeat('a'), max);
    assert!(read <= budget, "plain: read {read}");
    assert_eq!(text, "a".repeat(max));
    let (read, text) = chars_read(std::iter::repeat('\u{202E}'), max);
    assert!(read <= budget, "bidi: read {read}");
    assert_eq!(text, "");
    let zalgo = std::iter::once('a').chain(std::iter::repeat('\u{301}'));
    let (read, text) = chars_read(zalgo, max);
    assert!(read <= budget, "zalgo: read {read}");
    assert_eq!(text, "á\u{301}\u{301}");
    let zwj = std::iter::repeat(['👨', '\u{200D}']).flatten();
    let (read, text) = chars_read(zwj, max);
    assert!(read <= budget, "zwj: read {read}");
    assert_eq!(text, "👨".repeat(max));
}

#[test]
fn a_huge_string_is_cut_to_the_cap() {
    let huge = "ab\u{202E}c\u{301}\u{302}\u{303}".repeat(1_000_000);
    let text = display_safe(&huge, 100);
    assert_eq!(text.as_str().graphemes(true).count(), 100);
    assert!(!text.as_str().chars().any(invisible));
}

proptest! {
    #[test]
    fn any_text_is_shown_without_cc_or_cf_within_the_cap_and_stays_put(text in any::<String>(), max in 0usize..50) {
        let once = display_safe(&text, max);
        prop_assert!(!once.as_str().chars().any(invisible), "{:?}", once);
        prop_assert!(once.as_str().graphemes(true).count() <= max);
        // Safe text is left as it is, so cut or joined safe text can be made safe again for free.
        prop_assert_eq!(display_safe(once.as_str(), max), once);
    }
}
