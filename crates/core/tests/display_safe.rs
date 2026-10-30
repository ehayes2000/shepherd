//! One rule for text from agents, hooks, git and other clients before it is drawn or put in an alert (F35), through
//! the public `shepherd_core::display` API.

use std::cell::Cell;

use proptest::prelude::*;
use shepherd_core::display::{
    CHARS_KEPT_PER_GRAPHEME, CHARS_READ_PER_GRAPHEME, display_safe, display_safe_iter,
};
use unicode_segmentation::UnicodeSegmentation;

/// Room for every case below: none is cut by length unless it says so.
const ROOMY: usize = 200;

fn shown(text: &str) -> String {
    display_safe(text, ROOMY).as_str().to_owned()
}

/// Cc, Cf, Zl and Zp, the categories that must never be shown.
fn invisible(c: char) -> bool {
    use unicode_properties::{GeneralCategory, UnicodeGeneralCategory};
    matches!(
        c.general_category(),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
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
fn line_and_paragraph_separators_are_dropped() {
    assert_eq!(shown("one\u{2028}line\u{2029}para"), "onelinepara");
    assert_eq!(shown("\u{2028}\u{2029}"), "");
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

/// Each grapheme cluster of `text`, as made safe at `max`, holds at most [`CHARS_KEPT_PER_GRAPHEME`] chars, and making
/// it safe again leaves it as it is.
fn assert_clusters_capped_and_idempotent(text: &str, max: usize) -> String {
    let once = display_safe(text, max);
    for cluster in once.as_str().graphemes(true) {
        assert!(
            cluster.chars().count() <= CHARS_KEPT_PER_GRAPHEME,
            "{cluster:?} in {once:?}"
        );
    }
    assert_eq!(display_safe(once.as_str(), max), once, "{text:?} at {max}");
    once.into_string()
}

#[test]
fn one_grapheme_cluster_cannot_grow_without_bound() {
    // Extenders that aren't combining marks, so the per-base mark cap doesn't hold them.
    let voiced = format!("ｶ{}", "\u{FF9E}".repeat(200));
    let shown = assert_clusters_capped_and_idempotent(&voiced, 3);
    assert_eq!(shown.graphemes(true).count(), 1);
    assert!(shown.starts_with("ｶ\u{FF9E}"), "{shown:?}");
    let tones = format!("👍{}", "🏽".repeat(200));
    let shown = assert_clusters_capped_and_idempotent(&tones, 3);
    assert!(shown.starts_with("👍🏽"), "{shown:?}");
    // Hangul leading jamo all join one cluster.
    assert_clusters_capped_and_idempotent(&"\u{1100}".repeat(500), 2);
    // Indic conjuncts: consonant, virama, consonant, ... is one cluster with one mark per base.
    assert_clusters_capped_and_idempotent(&"क्".repeat(200), 2);
}

#[test]
fn a_long_run_of_flags_keeps_its_pairs() {
    let flags = "🇯🇵🇺🇸".repeat(60);
    assert_eq!(shown(&flags), flags);
    // One more indicator in front shifts every pair.
    assert_eq!(
        display_safe(&format!("🇯{flags}"), 3).as_str(),
        "🇯🇯\u{1F1F5}\u{1F1FA}\u{1F1F8}🇯"
    );
}

#[test]
fn the_review_counterexample_is_idempotent() {
    // 31 leading jamo and a mark that NFC splits in two: one cluster of 33 chars, more than a second pass reads.
    let text = format!("{}\u{344}", "\u{1100}".repeat(31));
    assert_clusters_capped_and_idempotent(&text, 1);
}

#[test]
fn an_unbounded_cluster_after_earlier_ones_is_capped_too() {
    let voiced = format!("ab{}", "\u{FF9E}".repeat(200));
    let shown = assert_clusters_capped_and_idempotent(&voiced, 10);
    assert!(shown.starts_with("ab\u{FF9E}"), "{shown:?}");
    let tones = format!("x🇯🇵👍{}", "🏽".repeat(200));
    let shown = assert_clusters_capped_and_idempotent(&tones, 10);
    assert!(shown.starts_with("x🇯🇵👍🏽"), "{shown:?}");
}

#[test]
fn a_rejected_extender_does_not_make_room_for_more_marks() {
    // A full cluster whose base has used its marks, an extender it can't take, then a mark that would otherwise start a
    // cluster of its own (U+102B joins nothing before it).
    let full = format!("a{}\u{301}\u{302}", "\u{FF9E}".repeat(13));
    assert_eq!(full.chars().count(), CHARS_KEPT_PER_GRAPHEME);
    let text = format!("{full}\u{FF9E}\u{102B}");
    assert_eq!(assert_clusters_capped_and_idempotent(&text, 10), full);
}

#[test]
fn a_full_cluster_never_ends_in_a_char_that_swallows_the_rest() {
    // U+0D4E is a Prepend: it joins whatever follows it, so it may not take a cluster's last place.
    let prepends = "\u{0D4E}".repeat(CHARS_KEPT_PER_GRAPHEME);
    let text = format!("{prepends}hello world");
    let shown = assert_clusters_capped_and_idempotent(&text, 50);
    assert_eq!(
        shown,
        format!(
            "{}hello world",
            "\u{0D4E}".repeat(CHARS_KEPT_PER_GRAPHEME - 1)
        )
    );
}

#[test]
fn a_full_cluster_is_followed_by_the_next_one() {
    let text = format!("ｶ{}b", "\u{FF9E}".repeat(200));
    let shown = assert_clusters_capped_and_idempotent(&text, 50);
    assert!(shown.ends_with("\u{FF9E}b"), "{shown:?}");
    assert_eq!(shown.graphemes(true).count(), 2);
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

    #[test]
    fn text_heavy_in_extenders_is_shown_capped_and_stays_put(
        text in "[\u{1100}\u{1161}\u{11A8}\u{344}\u{301}\u{FF9E}ｶ👍🏽क्a\u{200D}\u{2028}\u{1F1EF}\u{1F1F5}\u{102B}\u{0D4E}]{0,300}",
        max in 0usize..12,
    ) {
        let once = display_safe(&text, max);
        prop_assert!(!once.as_str().chars().any(invisible), "{:?}", once);
        prop_assert!(once.as_str().graphemes(true).count() <= max);
        for cluster in once.as_str().graphemes(true) {
            prop_assert!(cluster.chars().count() <= CHARS_KEPT_PER_GRAPHEME, "{:?}", cluster);
        }
        prop_assert_eq!(display_safe(once.as_str(), max), once);
    }
}
