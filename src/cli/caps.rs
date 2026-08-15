//! Shared surface caps and validators (CLI + MCP).
//!
//! These bounds started on the MCP tools (T8.2). T8.3 owns them so both
//! surfaces refuse the same oversized / control-character input and cannot
//! drift. MCP wraps [`check_size`]'s `String` error in a tool-level error;
//! the CLI maps it to a usage exit.

use clap::ValueEnum;

use crate::graph::canonical::{is_invisible, is_text_required_invisible};
use crate::types::ConceptType;

/// Upper bound on `top_k` a client may ask for. Recall assembles and renders
/// every hit, so an unbounded `top_k` from one client is a cheap way to stall
/// the single process every other client shares.
pub const MAX_TOP_K: usize = 100;
/// Upper bound on `traversal_depth` (spec §8 phase 2 is a BFS — depth is an
/// exponent, not a linear cost).
pub const MAX_TRAVERSAL_DEPTH: usize = 5;
/// Upper bound on `max_tokens` for one context block.
pub const MAX_MAX_TOKENS: usize = 100_000;
/// Upper bound on concepts in a single `derive` call.
pub const MAX_CONCEPTS_PER_DERIVE: usize = 64;
/// Upper bound on the combined `produces` + `modifies` + `depends_on` target
/// count in a single `record-action` call.
pub const MAX_ACTION_TARGETS: usize = 64;
/// Upper bound on `reserve` TTL — a soft lock (spec §11), not a lease.
pub const MAX_RESERVE_TTL_SECS: u64 = 3600;
/// Upper bound on `inspect` depth.
pub const MAX_INSPECT_DEPTH: usize = 5;
/// Cap on neighbours rendered per `inspect` frontier level.
pub const MAX_INSPECT_NODES: usize = 200;
/// Upper bound on **every** client-supplied string this surface accepts.
///
/// Sized to match `graph::hybrid::MAX_HYBRID_CONTEXT_BYTES` so the surface
/// refuses before the graph does.
pub const MAX_CONTENT_BYTES: usize = 16_384;
/// Candidate concepts listed when `inspect`'s focus is ambiguous.
pub const MAX_INSPECT_CANDIDATES: usize = 10;

/// Concept type as it crosses the CLI (`--kind` / `--concept CONTENT:KIND`).
///
/// Snake_case to match MCP [`crate::mcp::server::WireConceptType`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum ConceptKind {
    Entity,
    Logic,
    Constraint,
    Resource,
    Observation,
}

impl From<ConceptKind> for ConceptType {
    fn from(k: ConceptKind) -> Self {
        match k {
            ConceptKind::Entity => ConceptType::Entity,
            ConceptKind::Logic => ConceptType::Logic,
            ConceptKind::Constraint => ConceptType::Constraint,
            ConceptKind::Resource => ConceptType::Resource,
            ConceptKind::Observation => ConceptType::Observation,
        }
    }
}

impl ConceptKind {
    /// Parse a `entity|logic|constraint|resource|observation` token.
    pub fn parse_token(s: &str) -> Result<Self, CliError> {
        match s.to_ascii_lowercase().as_str() {
            "entity" => Ok(Self::Entity),
            "logic" => Ok(Self::Logic),
            "constraint" => Ok(Self::Constraint),
            "resource" => Ok(Self::Resource),
            "observation" => Ok(Self::Observation),
            _ => Err(CliError::Usage(format!(
                "kind must be entity|logic|constraint|resource|observation, got '{s}'"
            ))),
        }
    }
}

/// CLI command failure. Usage (bad flags/values) exits 2; runtime (store,
/// lease, close) exits 1. Never a panic on bad input.
#[derive(Debug)]
pub enum CliError {
    Usage(String),
    Runtime(String),
}

impl CliError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => 2,
            Self::Runtime(_) => 1,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(m) | Self::Runtime(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CliError {}

impl From<crate::types::LamboError> for CliError {
    fn from(err: crate::types::LamboError) -> Self {
        Self::Runtime(err.to_string())
    }
}

/// Clamp a config-derived default into the surface-enforced range.
///
/// A session config can set a `default_top_k` (etc.) wider than the surface
/// maximum; a caller that omits the knob would then inherit a value the
/// surface refuses. Clamp it into `lo..=hi` and log when that changes it.
pub fn clamp_cfg_default(name: &str, value: usize, lo: usize, hi: usize) -> usize {
    let clamped = value.clamp(lo, hi);
    if clamped != value {
        tracing::warn!(
            config_key = name,
            configured = value,
            clamped_to = clamped,
            "session config default is outside the surface bound — using the clamped value"
        );
    }
    clamped
}

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
fn is_disallowed_format(c: char) -> bool {
    is_invisible(c) && !is_text_required_invisible(c)
}

/// Validate one client string before it reaches the store: refuse it if it is
/// over [`MAX_CONTENT_BYTES`], carries a control character other than
/// tab/newline, **or** carries an invisible character (see
/// [`is_disallowed_format`]).
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

/// [`check_size`] mapped to a CLI usage error.
pub fn check_size_cli(field: &str, value: &str) -> Result<(), CliError> {
    check_size(field, value).map_err(CliError::Usage)
}

/// Refuse an empty (after trim) required string.
pub fn require_nonempty(field: &str, value: &str) -> Result<(), CliError> {
    if value.trim().is_empty() {
        return Err(CliError::Usage(format!(
            "{field} must be a non-empty string"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_the_t8_2_values() {
        assert_eq!(MAX_TOP_K, 100);
        assert_eq!(MAX_TRAVERSAL_DEPTH, 5);
        assert_eq!(MAX_MAX_TOKENS, 100_000);
        assert_eq!(MAX_CONCEPTS_PER_DERIVE, 64);
        assert_eq!(MAX_ACTION_TARGETS, 64);
        assert_eq!(MAX_RESERVE_TTL_SECS, 3600);
        assert_eq!(MAX_INSPECT_DEPTH, 5);
        assert_eq!(MAX_INSPECT_NODES, 200);
        assert_eq!(MAX_CONTENT_BYTES, 16_384);
        assert_eq!(MAX_INSPECT_CANDIDATES, 10);
    }

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

    #[test]
    fn clamp_cfg_default_pins_the_bounds() {
        assert_eq!(
            clamp_cfg_default("default_top_k", MAX_TOP_K + 500, 1, MAX_TOP_K),
            MAX_TOP_K
        );
        assert_eq!(clamp_cfg_default("default_top_k", 0, 1, MAX_TOP_K), 1);
        assert_eq!(clamp_cfg_default("default_top_k", 7, 1, MAX_TOP_K), 7);
    }

    #[test]
    fn concept_kind_parses_snake_case() {
        assert_eq!(
            ConceptKind::parse_token("entity").unwrap(),
            ConceptKind::Entity
        );
        assert_eq!(
            ConceptKind::parse_token("Observation").unwrap(),
            ConceptKind::Observation
        );
        assert!(ConceptKind::parse_token("nope").is_err());
    }
}
