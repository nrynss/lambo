//! Transport-neutral validation of client-supplied strings.
//!
//! Every function here returns its refusal as a plain `String` message. Each
//! surface adapts it: MCP wraps it in a tool-level error, the CLI maps it to a
//! usage exit (`cli::caps::check_size_cli`), and the web portal answers with
//! its own status. The rule and the message text live here once, so the
//! surfaces cannot drift.

use super::limits::MAX_CONTENT_BYTES;
use crate::graph::canonical::{is_invisible, is_text_required_invisible};

/// Is `c` an invisible character refused by [`check_size`] (L82-2 / R1-2)?
///
/// `char::is_control()` covers **only** C0 (`U+0000–U+001F`) and C1
/// (`U+007F–U+009F`). Every invisible codepoint above that — bidi overrides, the
/// zero-width family, the BOM, the TAGS block, and the blank-rendering fillers —
/// sails straight past it, which is how a `U+202E` RIGHT-TO-LEFT OVERRIDE
/// reached a live `concepts.content` column in the T8.2/T8.3 review. Such a
/// character renders as nothing, so a recall context block containing one looks
/// innocuous to a human reviewer while reordering or hiding what the model
/// actually reads: a prompt-injection and spoofing vector, not a cosmetic
/// defect.
///
/// The table lives in [`crate::graph::canonical::INVISIBLE_RANGES`], next to the
/// tokenizer that strips it, because the surface rule and the canonical-key rule
/// are two halves of one policy and a second copy here would drift (R1-2). This
/// surface refuses everything in it except
/// [`crate::graph::canonical::TEXT_REQUIRED_INVISIBLE`] — the joiners, the
/// variation selectors and the combining grapheme joiner, which legitimate
/// Persian, Indic and emoji text needs. Those stay in `content` and are erased
/// from `canonical_key`, so allowing them cannot fork one concept into two.
///
/// **`U+2028`/`U+2029` are deliberately absent, and that is a decision rather
/// than an oversight (J1-R2-1, argument corrected J1-R3-3).** This table's
/// criterion is *invisibility* — characters that survive human review while
/// changing what the model reads or what the canonical key becomes — and on
/// that criterion the line separators do not qualify for `content`: they render
/// as visible line breaks (that is their entire function), and they cannot fork
/// a canonical key, because [`crate::graph::canonical::normalize_tokens`]
/// splits tokens on `char::is_whitespace()`, whose Unicode `White_Space` set
/// contains both — two contents differing only in `\u{2028}` vs `\n` normalize
/// to one key. Where a *single-line* invariant is the whole point they are
/// refused at the door that promises it — `mcp::server`'s `breaks_one_line`,
/// which guards `agent_id`. **Revisit trigger:** if `normalize_tokens` ever
/// stops splitting on `is_whitespace()` (a narrower splitter re-opens the
/// key-forking question), or if any renderer starts treating them as other
/// than a line break — not merely if `content`'s `\n` policy changes.
fn is_disallowed_format(c: char) -> bool {
    is_invisible(c) && !is_text_required_invisible(c)
}

/// Validate one client string before it reaches the store: refuse it if it is
/// over [`MAX_CONTENT_BYTES`], carries a control character other than
/// tab/newline, **or** carries an invisible character (see
/// `is_disallowed_format`).
///
/// The size cap is the single-process fairness guard. The character checks are
/// data-hygiene and anti-injection ones:
///
/// * a NUL or other C0/C1 control ends up verbatim in a concept's `content`,
///   its canonical key, and every downstream rendering, where it can corrupt
///   terminals, truncate at the NUL, or smuggle ANSI escapes. Tab and newline
///   are the only controls a legitimate multi-line concept needs;
/// * a bidi override, zero-width or blank-rendering character is *invisible*, so
///   it survives human review of a recall context block while changing what the
///   model reads (L82-2).
///
/// Both are refused here rather than sanitised silently — a validator that
/// rewrites content would make the stored concept differ from what the caller
/// acknowledged writing.
///
/// Names the offending codepoint; never echoes the raw byte. The two messages
/// are deliberately distinct and each states only what it actually enforces:
/// the control message may say "tab and newline are the only ones allowed"
/// because for *control* characters that is exactly true, while the invisible
/// message does not, because the joiners and variation selectors are allowed
/// (R: the pre-L82-2 wording claimed the stronger contract for both and the
/// check delivered neither).
///
/// This function is **only half** of the invisible-character policy (R1-2). It
/// decides what may be *stored*; [`crate::graph::canonical::normalize_tokens`]
/// decides what may reach a *canonical key*, and strips the whole table
/// including the exceptions. Neither is sufficient alone: refusing everything
/// would reject legitimate Persian, Indic and emoji text, and stripping alone
/// would leave a `U+202E` sitting in `content` where a reviewer cannot see it.
pub fn check_size(field: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_CONTENT_BYTES {
        return Err(format!(
            "{field} exceeds {MAX_CONTENT_BYTES} bytes ({} given)",
            value.len()
        ));
    }
    if let Some(c) = value
        .chars()
        .find(|c| c.is_control() && *c != '\n' && *c != '\t')
    {
        return Err(format!(
            "{field} contains a disallowed control character (U+{:04X}); tab and newline are the \
             only control characters allowed",
            c as u32
        ));
    }
    if let Some(c) = value.chars().find(|c| is_disallowed_format(*c)) {
        return Err(format!(
            "{field} contains a disallowed invisible formatting character (U+{:04X}); bidi \
             overrides, zero-width characters, blank fillers, the BOM and tag characters are \
             refused because they are invisible in review but not to the model (zero-width \
             joiner and non-joiner are allowed, and are stripped from canonical keys)",
            c as u32
        ));
    }
    Ok(())
}

/// Refuse an empty (after trim) required string.
pub fn require_nonempty(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must be a non-empty string"));
    }
    Ok(())
}

/// Refuse a numeric knob outside `lo..=hi`.
///
/// `field` is the surface's own spelling of the knob (`top-k` on the CLI,
/// `top_k` over MCP); the bound and the wording are shared.
pub fn check_in_range<T>(field: &str, value: T, lo: T, hi: T) -> Result<(), String>
where
    T: PartialOrd + std::fmt::Display,
{
    if value < lo || value > hi {
        return Err(format!("{field} must be in {lo}..={hi}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_string_is_refused() {
        let big = "A".repeat(MAX_CONTENT_BYTES + 1);
        let err = check_size("query", &big).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
        assert!(err.contains(&MAX_CONTENT_BYTES.to_string()), "{err}");
        assert!(!err.contains(&big), "must not echo the payload");
    }

    #[test]
    fn control_char_is_refused_by_codepoint() {
        let err = check_size("query", "ok\u{0001}no").unwrap_err();
        assert!(
            err.contains("U+0001"),
            "must name the codepoint, never echo the raw byte: {err}"
        );
        assert!(!err.contains('\u{0001}'), "must not echo U+0001: {err}");
    }

    #[test]
    fn tab_and_newline_are_allowed() {
        check_size("query", "ok\tline\nnext").unwrap();
        check_size("query", "A".repeat(MAX_CONTENT_BYTES).as_str()).unwrap();
    }

    /// **L82-2.** `char::is_control()` is C0/C1 only, so the pre-fix validator
    /// let every category-*Cf* codepoint through — a live `lambo_derive` with a
    /// `U+202E` returned `isError:false` and the byte landed in
    /// `concepts.content`. Each case here is one of those, by class.
    #[test]
    fn invisible_format_characters_are_refused_by_codepoint() {
        for (label, bad, codepoint) in [
            ("rtl override", "amount: 100\u{202E}DSU 5", "U+202E"),
            ("lrm", "a\u{200E}b", "U+200E"),
            ("first-strong isolate", "a\u{2066}b", "U+2066"),
            ("pop directional isolate", "a\u{2069}b", "U+2069"),
            ("zero width space", "pass\u{200B}word", "U+200B"),
            ("word joiner", "a\u{2060}b", "U+2060"),
            ("bom", "\u{FEFF}leading", "U+FEFF"),
            ("soft hyphen", "so\u{00AD}ft", "U+00AD"),
            ("tag latin small a", "a\u{E0061}b", "U+E0061"),
            ("language tag", "a\u{E0001}b", "U+E0001"),
            // R1-2(b): invisible but NOT category Cf, so the first L82-2 pass
            // missed all of them. U+3164 is the codepoint most used in the wild
            // for invisible smuggling — it is a *letter* as far as Unicode is
            // concerned, and paints nothing.
            ("hangul filler", "a\u{3164}b", "U+3164"),
            ("halfwidth hangul filler", "a\u{FFA0}b", "U+FFA0"),
            ("hangul choseong filler", "a\u{115F}b", "U+115F"),
            ("hangul jungseong filler", "a\u{1160}b", "U+1160"),
            ("braille pattern blank", "a\u{2800}b", "U+2800"),
            ("khmer vowel inherent aq", "a\u{17B4}b", "U+17B4"),
            ("khmer vowel inherent aa", "a\u{17B5}b", "U+17B5"),
        ] {
            let err = check_size("concept.content", bad).unwrap_err();
            assert!(
                err.contains(codepoint),
                "{label}: must name the codepoint {codepoint}, got: {err}"
            );
            assert!(
                !err.chars().any(is_disallowed_format),
                "{label}: must not echo the raw invisible character back: {err}"
            );
            assert!(
                err.contains("invisible formatting character"),
                "{label}: must name the class, got: {err}"
            );
        }
    }

    /// The documented exceptions. ZWNJ/ZWJ carry orthographic meaning in Persian
    /// and Indic scripts and glue emoji sequences together, variation selectors
    /// choose a glyph form, and CGJ separates grapheme clusters; refusing any of
    /// them would reject legitimate concept text, and none can reorder or
    /// conceal a visible character. Arabic number signs are *Cf* but are
    /// ordinary text, not a direction or concealment control.
    #[test]
    fn joiners_and_arabic_number_signs_are_still_allowed() {
        check_size("concept.content", "\u{200C}").unwrap();
        check_size(
            "concept.content",
            "family: \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        )
        .unwrap();
        check_size("concept.content", "\u{0600}12").unwrap();
        // VS16 is what makes a dingbat render as an emoji.
        check_size("concept.content", "love \u{2764}\u{FE0F}").unwrap();
        check_size("concept.content", "ideograph \u{845B}\u{E0100}").unwrap();
        check_size("concept.content", "a\u{034F}b").unwrap();
        // Plain multilingual text and emoji are untouched.
        check_size("concept.content", "درخواست — 请求 — request 🚀").unwrap();
    }

    /// **R1-2(a).** Allowing the joiners is only safe because the canonical key
    /// cannot see them. Two strings that render identically must be accepted
    /// *and* collapse to one key — otherwise a caller can mint a second concept
    /// that looks like the first and can never be merged with it.
    ///
    /// This is the half of the policy that lives in
    /// [`crate::graph::canonical::normalize_tokens`]; it is asserted here too
    /// because the surface's decision to accept these characters is only
    /// defensible in combination with it.
    #[test]
    fn characters_this_surface_allows_cannot_fork_a_canonical_key() {
        use crate::graph::canonical::canonical_key;
        let plain = "billing retries change";
        for spoof in [
            "billing\u{200D} retries change",
            "billing\u{200C} retries change",
            "billing\u{FE0F} retries change",
            "billing\u{034F} retries change",
            "billing\u{E0100} retries change",
        ] {
            check_size("concept.content", spoof)
                .unwrap_or_else(|e| panic!("{spoof:?} must still be accepted: {e}"));
            assert_eq!(
                canonical_key(spoof, |_| None),
                canonical_key(plain, |_| None),
                "an accepted invisible character must not fork the key of text that renders \
                 identically ({spoof:?})"
            );
        }
    }

    /// The control-character message must not claim a contract the check does
    /// not enforce, and the format message must not claim the joiners are
    /// refused (L82-2: the pre-fix single message overstated both).
    #[test]
    fn refusal_messages_match_what_is_enforced() {
        let control = check_size("query", "a\u{0}b").unwrap_err();
        assert!(
            control.contains("only control characters allowed"),
            "control message must scope its claim to control characters: {control}"
        );
        let format = check_size("query", "a\u{202E}b").unwrap_err();
        assert!(
            format.contains("zero-width joiner and non-joiner are allowed"),
            "format message must name its exceptions: {format}"
        );
    }

    /// The shared wording every surface's refusal carries. Each surface passes
    /// its own field spelling, so the CLI's `top-k` and MCP's `top_k` read
    /// alike apart from the name.
    #[test]
    fn range_and_nonempty_refusals_carry_the_shared_wording() {
        assert_eq!(
            check_in_range("top_k", 0usize, 1, 100).unwrap_err(),
            "top_k must be in 1..=100"
        );
        assert_eq!(
            check_in_range("ttl-seconds", 3601u64, 1, 3600).unwrap_err(),
            "ttl-seconds must be in 1..=3600"
        );
        check_in_range("depth", 0usize, 0, 5).unwrap();
        check_in_range("depth", 5usize, 0, 5).unwrap();
        assert_eq!(
            require_nonempty("focus", " \t\n").unwrap_err(),
            "focus must be a non-empty string"
        );
        require_nonempty("focus", " x ").unwrap();
    }
}
