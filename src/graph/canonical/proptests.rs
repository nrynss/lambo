//! Property tests: [`canonical_key`] against
//! `formal/LamboProofs/Canonical.lean`.

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::test_util::props;

/// The separators `tokens` splits on: `-`, `_` and every `char::is_whitespace`
/// character (Unicode `White_Space`), the non-ASCII ones included.
const SEPARATORS: [char; 20] = [
    '-', '_', ' ', '\t', '\n', '\u{0B}', '\u{0C}', '\r', '\u{85}', '\u{A0}', '\u{1680}',
    '\u{2000}', '\u{2009}', '\u{200A}', '\u{2028}', '\u{2029}', '\u{202F}', '\u{205F}', '\u{3000}',
    '\u{2003}',
];

/// A piece between separators: ASCII letters of both cases (camelCase
/// boundaries), identifier characters, an apostrophe (`''s` stems to the
/// empty string), and a few non-ASCII ones: a precomposed and a combining
/// accent, `ß`, `İ` (lowercases to two chars), `Σ` (final-sigma rule) and an
/// invisible zero-width space.
fn piece() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9./'\u{E9}\u{301}\u{DF}\u{130}\u{3A3}\u{200B}]{0,6}"
}

/// A run of one to three separators.
fn sep_run() -> impl Strategy<Value = String> {
    vec(proptest::sample::select(&SEPARATORS[..]), 1..4).prop_map(|cs| cs.into_iter().collect())
}

/// `pieces` joined by `seps` (one run between each pair), with an optional
/// leading and trailing run.
fn join(pieces: &[String], seps: &[String], lead: &str, trail: &str) -> String {
    let mut s = String::from(lead);
    for (i, p) in pieces.iter().enumerate() {
        if i > 0 {
            s.push_str(&seps[i - 1]);
        }
        s.push_str(p);
    }
    s.push_str(trail);
    s
}

/// Two spellings of the same pieces: the second permutes them and uses
/// other separator runs everywhere.
type Spellings = (
    Vec<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
    [String; 4],
);

fn spellings() -> impl Strategy<Value = Spellings> {
    vec(piece(), 1..6).prop_flat_map(|pieces| {
        let n = pieces.len();
        (
            Just(pieces.clone()),
            Just(pieces).prop_shuffle(),
            vec(sep_run(), n - 1),
            vec(sep_run(), n - 1),
            [
                prop_oneof![Just(String::new()), sep_run()],
                prop_oneof![Just(String::new()), sep_run()],
                prop_oneof![Just(String::new()), sep_run()],
                prop_oneof![Just(String::new()), sep_run()],
            ],
        )
    })
}

/// A plain word the Snowball English stemmer keeps as it is: lowercase
/// ASCII consonants only (no `y`, which Porter2 may treat as a vowel). With
/// no vowel there is no `R1` region, so no suffix rule fires and step 1a's
/// `s` rule needs a vowel before it; no stopword is vowel-free.
fn plain_word() -> impl Strategy<Value = String> {
    "[bcdfghjklmnpqrstvwxz]{1,6}"
}

/// Lean `tidy`: no leading, trailing or doubled space.
fn tidy(key: &str) -> bool {
    !key.starts_with(' ') && !key.ends_with(' ') && !key.contains("  ")
}

proptest! {
    #![proptest_config(props::config())]

    /// `Canonical.key_perm`, `Canonical.key_segments_perm`,
    /// `Canonical.key_swap`, `Canonical.key_sep_alike` and
    /// `Canonical.key_sep_run` (`formal/LamboProofs/Canonical.lean`), with
    /// the real stemmer and stopword list: the same pieces in any order,
    /// joined by any runs of `-`, `_` or whitespace, have the same
    /// `canonical_key`. The model leaves out the invisible strip and NFC;
    /// the pieces here exercise both.
    #[test]
    fn canonical_key_ignores_order_and_separator_spelling(
        (pieces, shuffled, seps_a, seps_b, [lead_a, trail_a, lead_b, trail_b]) in spellings()
    ) {
        let a = join(&pieces, &seps_a, &lead_a, &trail_a);
        let b = join(&shuffled, &seps_b, &lead_b, &trail_b);
        prop_assert_eq!(canonical_key(&a, |_| None), canonical_key(&b, |_| None), "{:?} vs {:?}", a, b);
    }

    /// The key is not degenerate: `Canonical.joinSp_ne_nil` (a text with a
    /// token has a non-empty key) and `Canonical.key_eq_iff_perm` (equal keys
    /// iff the same multiset of stems, `formal/LamboProofs/Canonical.lean`):
    /// one plain word has a non-empty key; two different plain words have
    /// different keys, and adding a second word changes the key.
    #[test]
    fn canonical_key_separates_plain_words(
        a in plain_word(),
        b in plain_word(),
        sep in sep_run(),
    ) {
        let ka = canonical_key(&a, |_| None);
        prop_assert!(!ka.is_empty(), "empty key for {:?}", a);
        let kb = canonical_key(&b, |_| None);
        if a != b {
            prop_assert_ne!(&ka, &kb, "{:?} and {:?} share a key", a, b);
        }
        let both = canonical_key(&format!("{a}{sep}{b}"), |_| None);
        prop_assert_ne!(&both, &ka, "adding {:?} to {:?} left the key alone", b, a);
    }

    /// `Canonical.keyF_tidy` (`formal/LamboProofs/Canonical.lean`), with the
    /// real stemmer (which meets `NoSep`): a key never has a leading,
    /// trailing or doubled space, for structured input and for any string.
    #[test]
    fn canonical_key_is_tidy(
        (pieces, _, seps, _, [lead, trail, _, _]) in spellings(),
        any_text in any::<String>(),
    ) {
        let key = canonical_key(&join(&pieces, &seps, &lead, &trail), |_| None);
        prop_assert!(tidy(&key), "untidy key {:?}", key);
        let key = canonical_key(&any_text, |_| None);
        prop_assert!(tidy(&key), "untidy key {:?} for {:?}", key, any_text);
    }
}
