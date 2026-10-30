//! The finder's matching and ranking (T26): pure functions, prefix > word boundary > subsequence.

use shepherd_core::client::finder::{Rank, rank};

#[test]
fn a_prefix_ranks_above_a_word_boundary_which_ranks_above_a_subsequence() {
    assert_eq!(rank("fix", "fix-login"), Some(Rank::Prefix));
    assert_eq!(rank("log", "fix-login"), Some(Rank::WordBoundary));
    assert_eq!(rank("fxl", "fix-login"), Some(Rank::Subsequence));
    assert!(Rank::Prefix < Rank::WordBoundary);
    assert!(Rank::WordBoundary < Rank::Subsequence);
}

#[test]
fn a_query_that_isnt_a_subsequence_doesnt_match() {
    assert_eq!(rank("xyz", "fix-login"), None);
    assert_eq!(rank("nigol", "fix-login"), None);
    assert_eq!(rank("fix-login-2", "fix-login"), None);
}

#[test]
fn an_empty_query_matches_everything_alike() {
    assert_eq!(rank("", "fix-login"), Some(Rank::Prefix));
    assert_eq!(rank("", ""), Some(Rank::Prefix));
}

#[test]
fn a_word_boundary_follows_punctuation_and_spaces() {
    assert_eq!(rank("api", "/src/api"), Some(Rank::WordBoundary));
    assert_eq!(rank("main", "feature main"), Some(Rank::WordBoundary));
    assert_eq!(rank("bar", "foo_bar"), Some(Rank::WordBoundary));
    // Inside a word is only a subsequence.
    assert_eq!(rank("ogin", "fix-login"), Some(Rank::Subsequence));
}

#[test]
fn matching_ignores_case_beyond_ascii() {
    assert_eq!(rank("FIX", "fix-login"), Some(Rank::Prefix));
    assert_eq!(rank("école", "ÉCOLE-api"), Some(Rank::Prefix));
    assert_eq!(
        rank("straße", "STRASSE"),
        None,
        "lowercasing, not case folding"
    );
    assert_eq!(rank("ΣΟΦ", "σοφία"), Some(Rank::Prefix));
}

#[test]
fn composed_and_decomposed_spellings_match() {
    // `é` typed composed, the name decomposed (as macOS paths often are), and the other way round.
    assert_eq!(rank("caf\u{e9}", "cafe\u{301}-menu"), Some(Rank::Prefix));
    assert_eq!(rank("cafe\u{301}", "caf\u{e9}-menu"), Some(Rank::Prefix));
}

#[test]
fn a_query_never_matches_part_of_a_grapheme() {
    // `e` is only part of `é` (e + combining acute), and a lone mark is only part of it too.
    assert_eq!(rank("cafe", "cafe\u{301}"), None);
    assert_eq!(rank("\u{301}", "cafe\u{301}"), None);
    // A flag is one grapheme of two regional indicators: half of it is no match.
    assert_eq!(rank("\u{1f1eb}", "\u{1f1eb}\u{1f1f7}-team"), None);
    assert_eq!(
        rank("\u{1f1eb}\u{1f1f7}", "\u{1f1eb}\u{1f1f7}-team"),
        Some(Rank::Prefix)
    );
}

#[test]
fn a_word_boundary_is_found_in_non_latin_text() {
    assert_eq!(rank("ログ", "修正 ログイン"), Some(Rank::WordBoundary));
}
