//! Canonicalization pipeline (T2.2) — spec §7.1 steps 1–5.
//!
//! Steps, in order:
//! 1. Normalize — Unicode **NFC** (adve-review GRAPH-9: composed and decomposed
//!    spellings of the same word must not become two concepts), split camelCase
//!    boundaries, lowercase, split `[-_ ]` + whitespace, strip stopwords
//!    (`STOPWORDS`, pinned to the fixture convention
//!    `scripts/gen-fixtures.py`). See [`normalize_tokens`] for why NFC and not
//!    NFKC.
//! 2. Stem — Porter via `rust-stemmers` (`Algorithm::English`; snowball and
//!    custom stemmers are cut per spec §7.1).
//! 3. Token-sort → canonical key (sorted stems joined with single spaces).
//! 4. Synonym resolution — **direct lookup only**: no transitivity, no chains,
//!    no merge token. Pinned ordering (fixtures.rs + `gen-fixtures.py`): the RAW
//!    input string (trimmed, as-is) is looked up BEFORE normalization —
//!    `"register_user"` → synonym `"create_user"` → key `"creat user"`.
//! 5. Match against existing `canonical_key` — [`canonicalize`] returns
//!    [`CanonicalizeResult::Matched`] when a concept in the graph carries the
//!    key, else [`CanonicalizeResult::Unmatched`]. Step 6 (hybrid/vector match,
//!    `Semantic` edges) is T7.2's — the caller decides on `Unmatched`.
//!
//! Ordering note: the camelCase split must see the ORIGINAL case. Lowercasing
//! first would destroy the boundary (`"UserSchema"` → `"userschema"`, one token),
//! contradicting the fixture row `camelcase` → `"schema user"`. `gen-fixtures.py`
//! currently lowercases before splitting (a latent drift in that script); the
//! checked-in cases table is the frozen truth and this module matches it.
//!
//! Pure by design (the graph owns no lock, spec §6.4): [`normalize_tokens`] and
//! [`canonical_key`] have no `Graph` dependency, and [`canonicalize`] only reads
//! through `Graph::synonym` (T2.1 storage — no new synonym storage here).

use std::sync::LazyLock;

use rust_stemmers::{Algorithm, Stemmer};
use unicode_normalization::UnicodeNormalization;

use crate::graph::Graph;
use crate::types::{ConceptType, LamboError, NodeId};

/// Stopwords stripped during normalization — pinned to the fixture convention
/// (`scripts/gen-fixtures.py` `STOPWORDS`).
const STOPWORDS: [&str; 13] = [
    "the", "a", "an", "for", "of", "at", "in", "to", "on", "and", "or", "is", "are",
];

/// Shared Porter (English) stemmer — created once, reused for every call.
static STEMMER: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::English));

/// Codepoints that render as nothing, or as an empty cell — the single authority
/// on "invisible" for the whole codebase (L82-2 / R1-2).
///
/// # Why one table with two consumers
///
/// An invisible codepoint is dangerous in two unrelated ways, and the two need
/// opposite treatments:
///
/// * **Concealment / reordering.** A `U+202E` RIGHT-TO-LEFT OVERRIDE in a
///   concept's `content` survives human review of a recall context block while
///   changing what the model reads. Nothing legitimate needs it, so
///   [`crate::cli::caps::check_size`] **refuses** it at the surface.
/// * **Key forking.** An invisible codepoint that a *human cannot see* still
///   sits inside a token, defeats the stemmer, and yields an unrelated canonical
///   key. `"billing retries change"` and `"billing\u{200D} retries change"`
///   render identically and used to produce `"bill chang retri"` and
///   `"billing\u{200d} chang retri"` — two concepts that canonization can never
///   merge, and that the partial unique index never sees collide. That is a
///   duplicate/shadow-concept vector, and refusing the character is not the only
///   cure: erasing it *before tokenizing* makes the key invariant under it.
///
/// So this table is the **strip** set — [`normalize_tokens`] removes every
/// codepoint in it, so no invisible character can ever fork a key — and
/// [`crate::cli::caps`] refuses the whole table **except**
/// [`TEXT_REQUIRED_INVISIBLE`]. The two rules compose: whatever a caller is
/// allowed to store cannot affect a key, and whatever could affect a key cannot
/// be stored. One table means the two cannot drift apart.
///
/// # Scope
///
/// Unicode general category *Cf* as of Unicode 16 (bidi controls, the zero-width
/// family, the BOM, the deprecated format controls, the whole `U+E0000–U+E007F`
/// TAGS block including its unassigned holes — a superset costs nothing and
/// survives future assignments), **plus** two classes that are not *Cf* and were
/// therefore missed entirely by the first L82-2 pass (R1-2b):
///
/// * the *filler* codepoints, which are category `Lo`/`Mn` but render blank:
///   `U+115F`, `U+1160`, `U+3164`, `U+FFA0` (`U+3164` HANGUL FILLER is the
///   canonical real-world invisible-smuggling codepoint) and `U+17B4`/`U+17B5`;
/// * `U+2800` BRAILLE PATTERN BLANK, which occupies width but paints nothing.
///
/// Arabic number-formatting signs (`U+0600–U+0605`, `U+06DD`, `U+070F`,
/// `U+0890–U+0891`, `U+08E2`) are *Cf* but deliberately **absent**: they prefix
/// digits in ordinary Arabic text, carry no direction or concealment capability,
/// and are not invisible — they render as a mark over the digits they govern.
/// `U+061C` ARABIC LETTER MARK *is* listed; it is a bidi control.
///
/// Refusing the fillers costs archaic Hangul jamo-filler sequences and braille
/// written with explicit blank cells. Both are outside what a concept's content
/// is for, and the alternative — leaving the one codepoint most used for
/// invisible smuggling accepted — is worse.
///
/// Ascending and non-overlapping; `invisible_table_is_ordered_and_disjoint`
/// pins that so a future edit cannot make a range unreachable.
pub const INVISIBLE_RANGES: &[(char, char)] = &[
    ('\u{00AD}', '\u{00AD}'),   // SOFT HYPHEN
    ('\u{034F}', '\u{034F}'),   // COMBINING GRAPHEME JOINER (blocks composition)
    ('\u{061C}', '\u{061C}'),   // ARABIC LETTER MARK (bidi)
    ('\u{115F}', '\u{1160}'),   // HANGUL CHOSEONG / JUNGSEONG FILLER — blank
    ('\u{17B4}', '\u{17B5}'),   // KHMER VOWEL INHERENT AQ / AA — blank
    ('\u{180E}', '\u{180E}'),   // MONGOLIAN VOWEL SEPARATOR
    ('\u{200B}', '\u{200D}'),   // ZERO WIDTH SPACE, ZWNJ, ZWJ
    ('\u{200E}', '\u{200F}'),   // LEFT-TO-RIGHT / RIGHT-TO-LEFT MARK
    ('\u{202A}', '\u{202E}'),   // LRE, RLE, PDF, LRO, RLO — the U+202E family
    ('\u{2060}', '\u{2064}'),   // WORD JOINER, invisible operators
    ('\u{2066}', '\u{206F}'),   // isolates (LRI/RLI/FSI/PDI) + deprecated controls
    ('\u{2800}', '\u{2800}'),   // BRAILLE PATTERN BLANK
    ('\u{3164}', '\u{3164}'),   // HANGUL FILLER — the classic smuggling codepoint
    ('\u{FE00}', '\u{FE0F}'),   // VARIATION SELECTORS 1–16 (VS16 = emoji presentation)
    ('\u{FEFF}', '\u{FEFF}'),   // ZERO WIDTH NO-BREAK SPACE / BOM
    ('\u{FFA0}', '\u{FFA0}'),   // HALFWIDTH HANGUL FILLER
    ('\u{FFF9}', '\u{FFFB}'),   // interlinear annotation
    ('\u{110BD}', '\u{110BD}'), // KAITHI NUMBER SIGN
    ('\u{110CD}', '\u{110CD}'), // KAITHI NUMBER SIGN ABOVE
    ('\u{13430}', '\u{1343F}'), // Egyptian Hieroglyph format controls
    ('\u{1BCA0}', '\u{1BCA3}'), // shorthand format controls
    ('\u{1D173}', '\u{1D17A}'), // musical format controls
    ('\u{E0000}', '\u{E007F}'), // TAGS block — invisible ASCII smuggling
    ('\u{E0100}', '\u{E01EF}'), // VARIATION SELECTORS SUPPLEMENT
];

/// The subset of [`INVISIBLE_RANGES`] that legitimate text genuinely needs, and
/// which the surface therefore still **accepts** into stored content.
///
/// * `U+200C` ZERO WIDTH NON-JOINER and `U+200D` ZERO WIDTH JOINER are
///   orthographically required in Persian and several Indic scripts and are the
///   glue in emoji ZWJ sequences (`👨‍👩‍👧`).
/// * `U+FE00–U+FE0F` and `U+E0100–U+E01EF` variation selectors pick a glyph
///   form; `U+FE0F` is what makes `❤️` render as an emoji rather than a dingbat.
/// * `U+034F` COMBINING GRAPHEME JOINER separates grapheme clusters in a handful
///   of orthographies and collation contexts.
///
/// None of them can reorder or conceal a visible character — they only join,
/// separate or restyle adjacent glyphs — so the concealment half of the threat
/// does not apply. The key-forking half does, and is handled by the fact that
/// [`normalize_tokens`] strips them like everything else in the table: they are
/// preserved in `content` and erased from `canonical_key`.
pub const TEXT_REQUIRED_INVISIBLE: &[(char, char)] = &[
    ('\u{034F}', '\u{034F}'),
    ('\u{200C}', '\u{200D}'),
    ('\u{FE00}', '\u{FE0F}'),
    ('\u{E0100}', '\u{E01EF}'),
];

fn in_ranges(c: char, ranges: &[(char, char)]) -> bool {
    ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi)
}

/// Is `c` invisible — see [`INVISIBLE_RANGES`]? Stripped by
/// [`normalize_tokens`]; refused by the surface unless
/// [`is_text_required_invisible`].
pub fn is_invisible(c: char) -> bool {
    in_ranges(c, INVISIBLE_RANGES)
}

/// Is `c` one of the invisible codepoints legitimate text needs — see
/// [`TEXT_REQUIRED_INVISIBLE`]?
pub fn is_text_required_invisible(c: char) -> bool {
    in_ranges(c, TEXT_REQUIRED_INVISIBLE)
}

/// Insert a space at every lower→upper (camelCase) boundary, on the ORIGINAL
/// case. Matches the fixture convention `([a-z])([A-Z]) -> \1 \2`, applied before
/// lowercasing so the boundary survives (`"UserSchema"` → `"User Schema"`).
fn split_camel_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_lower = false;
    for c in s.chars() {
        if prev_lower && c.is_ascii_uppercase() {
            out.push(' ');
        }
        prev_lower = c.is_ascii_lowercase();
        out.push(c);
    }
    out
}

/// Spec §7.1 steps 1–2 — normalize + stem.
///
/// **Unicode NFC first** (adve-review GRAPH-9), then lowercase, split `[-_ ]`
/// and camelCase boundaries, drop stopwords, Porter stem. NO sort, NO synonym
/// lookup, NO join (those are [`canonical_key`]'s job). Pure — no `Graph`
/// dependency, so recall's keyword index (T2.6) can tokenize without a graph.
///
/// ## Why NFC (GRAPH-9)
///
/// `"café"` composed (NFC, `é` = U+00E9) and decomposed (NFD, `e` + U+0301) are
/// different byte strings and were therefore different canonical keys — so the
/// same word typed by two agents on two platforms produced two concepts that
/// canonicalization could never merge. macOS filesystems hand back NFD while
/// most editors and HTTP clients emit NFC, so both reach the same session.
/// Normalizing at the head of the tokenizer fixes **both** `canonical_key`
/// paths, [`canonicalize`] included, and the T2.6 keyword index with them —
/// they all tokenize through here, so key and index agree by construction.
///
/// NFC, not NFKC: NFKC folds compatibility characters (`ﬁ` → `fi`, `²` → `2`,
/// full-width forms to ASCII), which changes what the content *says*. Canonical
/// equivalence is the property we need — same character, same bytes — and it is
/// the only one that is lossless.
///
/// Pure ASCII is a fixed point of NFC, so every committed fixture and every
/// pinned canonicalization case is byte-identical through this change.
///
/// The raw synonym lookup in [`canonical_key`] / [`canonicalize`] stays
/// byte-exact on the trimmed input (the pinned muse-spark S2 contract: synonym
/// keys must match the raw call-site spelling, case included). A synonym key and
/// a call site that disagree about composition therefore miss each other — the
/// same shape as the existing case-sensitivity, and not widened here. A synonym
/// miss is harmless: the raw text falls through to this function and is
/// normalized anyway.
///
/// ## Why invisible characters are stripped first (R1-2)
///
/// [`INVISIBLE_RANGES`] is erased **before** NFC, so the key is invariant under
/// every codepoint a reviewer cannot see. Two texts that render identically
/// therefore always produce the same key, which is what stops an invisible
/// character from forking one concept into two permanently unmergeable ones.
///
/// Before NFC rather than after, because `U+034F` COMBINING GRAPHEME JOINER
/// exists precisely to *block* composition: stripping it first lets the
/// surrounding sequence compose normally, where stripping afterwards would leave
/// an already-blocked decomposition behind.
///
/// Pure ASCII contains none of these, so every committed fixture and every
/// pinned canonicalization case is byte-identical through this change — the same
/// argument the NFC change above rests on.
pub fn normalize_tokens(content: &str) -> Vec<String> {
    let nfc: String = content
        .chars()
        .filter(|c| !is_invisible(*c))
        .nfc()
        .collect();
    split_camel_case(&nfc)
        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .map(|t| STEMMER.stem(&t).into_owned())
        .collect()
}

/// Step 3 — sort stems and join with single spaces.
fn tokens_to_key(stems: &mut [String]) -> String {
    stems.sort();
    stems.join(" ")
}

/// Spec §7.1 steps 1–4 — full canonical-key derivation.
///
/// RAW-input synonym lookup FIRST (trimmed, as-is, per the pinned fixture
/// convention), then [`normalize_tokens`], token-sort, join with single spaces.
///
/// Synonym keys are matched EXACTLY on the trimmed input: case-sensitive, no
/// case folding, no whitespace collapsing beyond the leading/trailing trim.
/// `declare_synonym` keys must match the raw call-site spelling (muse-spark S2).
pub fn canonical_key(content: &str, synonyms: impl Fn(&str) -> Option<&str>) -> String {
    let raw = content.trim();
    let effective = synonyms(raw).unwrap_or(raw);
    tokens_to_key(&mut normalize_tokens(effective))
}

/// Outcome of step 5 — matching a derived key against the graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalizeResult {
    /// A concept in the graph already carries this canonical key.
    Matched { key: String, node: NodeId },
    /// No concept carries the key; the caller (T2.3+) decides hybrid step 6.
    Unmatched { key: String },
}

/// Spec §7.1 step 5 — match against existing `canonical_key`.
///
/// Step 4 reads `Graph::synonym` (T2.1 storage; `declare_synonym` lives on
/// `Graph`). `Result` is part of the pinned contract; this step itself cannot
/// fail — the hybrid step 6 is deliberately left to the caller.
///
/// **Observations are never matched** (adve-review GRAPH-1): demoted
/// context-overflow records skip the match step per spec §7 demote semantics,
/// so `derive`/`record_action` must not attach agent-declared content to an
/// Observation — even when one carries the same canonical key (legal for
/// Observations under the partial-UNIQUE errata). The remaining candidates are
/// unique by key (schema §4 partial `UNIQUE`), and the lowest-`NodeId`
/// tie-break makes the match deterministic by construction even if duplicates
/// ever slip through — never HashMap iteration order.
pub fn canonicalize(content: &str, graph: &Graph) -> Result<CanonicalizeResult, LamboError> {
    // Raw-lookup-before-normalization, identical to `canonical_key`'s pinned
    // ordering, but reading T2.1's storage directly (the public synonym callback
    // signature cannot borrow from the graph — see `canonical_key`).
    let raw = content.trim();
    let key = match graph.synonym(raw) {
        Some(mapped) => tokens_to_key(&mut normalize_tokens(mapped)),
        None => tokens_to_key(&mut normalize_tokens(raw)),
    };
    match graph
        .concepts()
        .filter(|c| c.concept_type != ConceptType::Observation)
        .filter(|c| c.canonical_key == key)
        .min_by_key(|c| c.id.0)
        .map(|c| c.id)
    {
        Some(node) => Ok(CanonicalizeResult::Matched { key, node }),
        None => Ok(CanonicalizeResult::Unmatched { key }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GraphSnapshot;

    /// Fixture STEM table (`scripts/gen-fixtures.py`, probe-verified against this
    /// module's stemmer — Porter English). `(word, expected stem)`.
    const STEM: &[(&str, &str)] = &[
        ("registering", "regist"),
        ("register", "regist"),
        ("registered", "regist"),
        ("users", "user"),
        ("user", "user"),
        ("systems", "system"),
        ("system", "system"),
        ("connecting", "connect"),
        ("connect", "connect"),
        ("schema", "schema"),
        ("schemas", "schema"),
        ("authentication", "authent"),
        ("authorization", "author"),
        ("validating", "valid"),
        ("validated", "valid"),
        ("validation", "valid"),
        ("rules", "rule"),
        ("creating", "creat"),
        ("created", "creat"),
        ("creat", "creat"),
        ("create", "creat"),
        ("pagination", "pagin"),
        ("paginate", "pagin"),
        ("rate", "rate"),
        ("limits", "limit"),
        ("limiter", "limit"),
        ("limit", "limit"),
        ("caching", "cach"),
        ("cache", "cach"),
        ("documentation", "document"),
        ("docs", "doc"),
        ("doc", "doc"),
        ("password", "password"),
        ("passwords", "password"),
        ("reset", "reset"),
        ("resetting", "reset"),
        ("logging", "log"),
        ("loadtesting", "loadtest"),
        ("testing", "test"),
        ("load", "load"),
        ("id", "id"),
        ("ratelimit", "ratelimit"),
        ("registration", "registr"),
        ("time", "time"),
        ("birth", "birth"),
        ("join", "join"),
        ("updated", "updat"),
        ("update", "updat"),
        ("profile", "profil"),
        ("middleware", "middlewar"),
        ("response", "respons"),
        ("responses", "respons"),
        ("launch", "launch"),
        ("product", "product"),
        ("path", "path"),
        ("step", "step"),
        ("far", "far"),
        ("budget", "budget"),
        ("concept", "concept"),
        ("isolated", "isol"),
        ("widget", "widget"),
        ("sibling", "sibl"),
        ("web", "web"),
        ("framework", "framework"),
        ("database", "databas"),
        ("layer", "layer"),
        ("authenticate", "authent"),
        ("auth", "auth"),
        ("role", "role"),
        ("email", "email"),
        ("hash", "hash"),
        ("status", "status"),
        ("error", "error"),
        ("api", "api"),
        ("account", "account"),
        ("one", "one"),
        ("two", "two"),
        ("three", "three"),
        ("four", "four"),
        ("five", "five"),
    ];

    /// No-op synonym table. A fn item (not a closure): the pinned
    /// `impl Fn(&str) -> Option<&str>` callback is higher-ranked, and fn items —
    /// unlike closures — bind the elided output lifetime to the input.
    fn no_synonym(_: &str) -> Option<&str> {
        None
    }

    /// Synonym table mirroring the fixture snapshot (`register_user` ->
    /// `create_user`, direct lookup only).
    fn fixture_synonyms(src: &str) -> Option<&str> {
        match src {
            "register_user" => Some("create_user"),
            _ => None,
        }
    }

    /// Minimal graph with a single concept carrying `canonical_key` (built via
    /// snapshot round-trip so the test avoids the write-path mutation log).
    fn graph_with_concept(canonical_key: &str) -> Graph {
        graph_with_concept_typed(canonical_key, "Entity")
    }

    /// Like [`graph_with_concept`], but with an arbitrary JSON concept type —
    /// GRAPH-1's regression needs an Observation-carrying graph.
    fn graph_with_concept_typed(canonical_key: &str, concept_type: &str) -> Graph {
        let snap: GraphSnapshot = serde_json::from_value(serde_json::json!({
            "session_id": "test-session",
            "root_goal": null,
            "created_at": "2026-08-10T09:00:00Z",
            "closed_at": null,
            "interactions": [{
                "id": "f0000000-0000-4000-8000-000000000001",
                "session_id": "test-session",
                "agent_id": "agent-a",
                "prompt_text": "seed",
                "previous_id": null,
                "created_at": "2026-08-10T09:00:00Z"
            }],
            "concepts": [{
                "id": "f0000000-0000-4000-8000-000000000002",
                "session_id": "test-session",
                "content": "user schema",
                "canonical_key": canonical_key,
                "concept_type": concept_type,
                "origin_interaction": "f0000000-0000-4000-8000-000000000001",
                "origin_agent": "agent-a",
                "created_at": "2026-08-10T09:00:00Z",
                "access_count": 0,
                "last_accessed": null,
                "gc_survived": 0,
                "canonization_status": "None",
                "blast_radius": null,
                "last_demotion_time": null,
                "embedding": null
            }],
            "edges": [{
                "id": "f0000000-0000-4000-8000-000000000003",
                "session_id": "test-session",
                "source": "f0000000-0000-4000-8000-000000000001",
                "target": "f0000000-0000-4000-8000-000000000002",
                "edge_type": "Derives",
                "weight": 0.9,
                "reinforcements": 1,
                "created_at": "2026-08-10T09:00:00Z",
                "last_reinforced": "2026-08-10T09:00:00Z"
            }],
            "synonyms": [],
            "reservations": [],
            "canonization_events": [],
            "embedding": null
        }))
        .unwrap();
        Graph::from_snapshot(snap).unwrap()
    }

    #[test]
    fn normalize_splits_camel_case_on_original_case() {
        // Boundary must be seen before lowercasing ("UserSchema" -> user + schema).
        assert_eq!(normalize_tokens("UserSchema"), vec!["user", "schema"]);
        assert_eq!(normalize_tokens("registerUser"), vec!["regist", "user"]);
    }

    #[test]
    fn normalize_splits_hyphen_underscore_mix() {
        assert_eq!(normalize_tokens("user-schema"), vec!["user", "schema"]);
        assert_eq!(normalize_tokens("user_schema"), vec!["user", "schema"]);
        assert_eq!(normalize_tokens("user-_schema"), vec!["user", "schema"]);
        // '.' is not a split char — "v1.2" stays one token.
        assert_eq!(
            normalize_tokens("auth-middleware_v1.2"),
            vec!["auth", "middlewar", "v1.2"]
        );
    }

    #[test]
    fn normalize_strips_stopwords_and_lowercases() {
        assert_eq!(
            normalize_tokens("THE user schema api"),
            vec!["user", "schema", "api"]
        );
        // Stopword-only input -> empty (sort/join downstream yields "").
        assert!(normalize_tokens("the and of").is_empty());
    }

    #[test]
    fn normalize_empty_input_yields_empty_vec() {
        assert!(normalize_tokens("").is_empty());
        assert!(normalize_tokens("   ").is_empty());
        assert!(normalize_tokens("-_").is_empty());
    }

    #[test]
    fn normalize_keeps_input_order_no_sort() {
        assert_eq!(normalize_tokens("schema user"), vec!["schema", "user"]);
    }

    #[test]
    fn porter_stems_match_fixture_stem_table() {
        for (word, expected) in STEM {
            assert_eq!(STEMMER.stem(word), *expected, "stem of {word:?}");
        }
    }

    #[test]
    fn canonical_key_sorts_and_joins() {
        assert_eq!(canonical_key("User Schema", no_synonym), "schema user");
        assert_eq!(canonical_key("user-schema", no_synonym), "schema user");
        assert_eq!(canonical_key("user_schema", no_synonym), "schema user");
        assert_eq!(canonical_key("UserSchema", no_synonym), "schema user");
        assert_eq!(
            canonical_key("the user schema api", no_synonym),
            "api schema user"
        );
        assert_eq!(
            canonical_key("registering users", no_synonym),
            "regist user"
        );
        assert_eq!(
            canonical_key("creating cached systems", no_synonym),
            "cach creat system"
        );
        assert_eq!(canonical_key("schema the user", no_synonym), "schema user");
        // Semantic near-pair stays distinct (hybrid step 6 is out of scope).
        assert_eq!(canonical_key("register user", no_synonym), "regist user");
        assert_eq!(canonical_key("create account", no_synonym), "account creat");
    }

    #[test]
    fn canonical_key_raw_synonym_lookup_before_normalization() {
        // RAW "register_user" maps to "create_user" BEFORE normalization; the
        // normalized "register user" must NOT map (direct lookup, no chain).
        assert_eq!(
            canonical_key("register_user", fixture_synonyms),
            "creat user"
        );
        assert_eq!(
            canonical_key("register user", fixture_synonyms),
            "regist user"
        );
        // Trimmed raw lookup: surrounding whitespace does not defeat the synonym.
        assert_eq!(
            canonical_key("  register_user  ", fixture_synonyms),
            "creat user"
        );
    }

    // ------------------------------------------------------------------
    // GRAPH-9 — Unicode NFC
    // ------------------------------------------------------------------

    /// GRAPH-9: composed (NFC) and decomposed (NFD) spellings of the same word
    /// must produce the same canonical key — and therefore resolve to the same
    /// concept — instead of two concepts canonicalization can never merge.
    #[test]
    fn nfc_and_nfd_spellings_share_one_canonical_key() {
        // "café server": é as U+00E9 (composed) vs e + U+0301 (decomposed).
        let nfc = "caf\u{e9} server";
        let nfd = "cafe\u{301} server";
        assert_ne!(nfc, nfd, "the two inputs must differ byte-wise");
        assert_eq!(
            canonical_key(nfc, no_synonym),
            canonical_key(nfd, no_synonym),
            "canonical equivalence must collapse to one key"
        );
        assert_eq!(normalize_tokens(nfc), normalize_tokens(nfd));

        // Step 5 follows: the decomposed spelling matches the concept created
        // from the composed one. Pre-fix this was Unmatched — a duplicate.
        let g = graph_with_concept(&canonical_key(nfc, no_synonym));
        let expected = g.concepts().next().unwrap().id;
        match canonicalize(nfd, &g).unwrap() {
            CanonicalizeResult::Matched { node, .. } => assert_eq!(node, expected),
            other => panic!("decomposed spelling must match the composed concept: {other:?}"),
        }

        // NFC, not NFKC: compatibility folding would change what the content
        // says, so distinct characters stay distinct.
        assert_ne!(
            canonical_key("\u{fb01}le", no_synonym),
            canonical_key("file", no_synonym),
            "the ﬁ ligature is compatibility-equivalent, not canonically equal"
        );
    }

    /// **R1-2(a), the pin.** The reviewer's reproduction: a ZWJ inside a token
    /// used to survive NFC, defeat the stemmer and yield an unrelated key —
    /// `"billing retries change"` → `"bill chang retri"` but
    /// `"billing\u{200D} retries change"` → `"billing\u{200d} chang retri"`.
    /// Both were accepted by the surface, so a caller could mint a second
    /// concept that renders identically to the first and that canonization could
    /// never merge, with the partial unique index never seeing a collision.
    ///
    /// Every invisible codepoint gets the same treatment, whether the surface
    /// refuses it (defence in depth — content loaded from a store written before
    /// the refusal existed still tokenizes cleanly) or allows it.
    #[test]
    fn invisible_characters_cannot_fork_a_canonical_key() {
        let plain = "billing retries change";
        let expected = canonical_key(plain, no_synonym);
        assert_eq!(expected, "bill chang retri", "the pinned key must not move");

        for (label, spoof) in [
            ("zwj (allowed in content)", "billing\u{200D} retries change"),
            (
                "zwnj (allowed in content)",
                "billing\u{200C} retries change",
            ),
            (
                "vs16 (allowed in content)",
                "billing\u{FE0F} retries change",
            ),
            ("cgj (allowed in content)", "billing\u{034F} retries change"),
            ("zero width space", "billing\u{200B} retries change"),
            ("rtl override", "billing\u{202E} retries change"),
            ("bom", "\u{FEFF}billing retries change"),
            ("soft hyphen", "bil\u{00AD}ling retries change"),
            ("hangul filler", "billing\u{3164} retries change"),
            ("braille blank", "billing\u{2800} retries change"),
            ("tag character", "billing\u{E0062} retries change"),
        ] {
            assert_ne!(spoof, plain, "{label}: the inputs must differ byte-wise");
            assert_eq!(
                canonical_key(spoof, no_synonym),
                expected,
                "{label}: text that renders identically must collapse to one key"
            );
        }

        // Step 5 follows: the spoofed spelling matches the concept created from
        // the plain one, so it reinforces rather than forking a duplicate.
        let g = graph_with_concept(&expected);
        let target = g.concepts().next().unwrap().id;
        match canonicalize("billing\u{200D} retries change", &g).unwrap() {
            CanonicalizeResult::Matched { node, .. } => assert_eq!(node, target),
            other => panic!("the joiner spelling must match the plain concept: {other:?}"),
        }
    }

    /// Stripping happens **before** NFC so a composition blocker cannot be used
    /// to fork a key that canonical equivalence would otherwise collapse.
    #[test]
    fn a_composition_blocker_cannot_fork_a_key() {
        // "café": e + CGJ + combining acute does not compose under NFC, so
        // stripping afterwards would leave "cafe" + U+0301 behind.
        assert_eq!(
            canonical_key("cafe\u{034F}\u{301} server", no_synonym),
            canonical_key("caf\u{e9} server", no_synonym),
            "CGJ must be removed before NFC runs"
        );
    }

    /// The table is searched linearly and documented as ascending; an edit that
    /// broke either would be silent.
    #[test]
    fn invisible_table_is_ordered_and_disjoint() {
        let mut previous: Option<char> = None;
        for &(lo, hi) in INVISIBLE_RANGES {
            assert!(
                lo <= hi,
                "range U+{:04X}..U+{:04X} is inverted",
                lo as u32,
                hi as u32
            );
            if let Some(prev_hi) = previous {
                assert!(
                    prev_hi < lo,
                    "ranges must ascend and not overlap: U+{:04X} then U+{:04X}",
                    prev_hi as u32,
                    lo as u32
                );
            }
            previous = Some(hi);
        }
        // The exceptions are a strict subset — a codepoint the surface allows
        // but the tokenizer does not strip would be a key-forking hole.
        for &(lo, hi) in TEXT_REQUIRED_INVISIBLE {
            for c in (lo as u32)..=(hi as u32) {
                let c = char::from_u32(c).expect("exception ranges hold no surrogates");
                assert!(
                    is_invisible(c),
                    "U+{:04X} is allowed in content but not stripped from keys",
                    c as u32
                );
            }
        }
    }

    /// GRAPH-9: pure ASCII is a fixed point of NFC, so no pinned key moves.
    #[test]
    fn ascii_canonical_keys_are_unchanged_by_nfc() {
        for input in [
            "UserSchema",
            "create_user",
            "auth-middleware",
            "  schema the user  ",
            "creating cached systems",
            "",
        ] {
            let nfc: String = input.nfc().collect();
            assert_eq!(nfc, input, "ASCII must be a fixed point: {input:?}");
            assert_eq!(
                canonical_key(input, no_synonym),
                canonical_key(&nfc, no_synonym)
            );
        }
    }

    #[cfg(feature = "fixtures")]
    #[test]
    fn nfc_leaves_every_pinned_canonicalization_case_unchanged() {
        // The frozen cases table is the contract (module docs); GRAPH-9 must be
        // invisible to it. `canonicalization_cases` is ASCII throughout.
        let cases = crate::fixtures::load_canonicalization_cases().unwrap();
        let cases = cases.as_array().expect("cases table is a JSON array");
        assert!(!cases.is_empty());
        for case in cases {
            let input = case["input"].as_str().expect("every case has an input");
            let nfc: String = input.nfc().collect();
            assert_eq!(nfc, input, "fixture case must be ASCII: {input:?}");
            // And the derived key is unchanged by the normalization step.
            assert_eq!(
                canonical_key(input, no_synonym),
                canonical_key(&nfc, no_synonym)
            );
        }
    }

    #[test]
    fn canonicalize_matches_existing_concept() {
        let g = graph_with_concept("schema user");
        let expected = g.concepts().next().unwrap().id;
        match canonicalize("UserSchema", &g).unwrap() {
            CanonicalizeResult::Matched { key, node } => {
                assert_eq!(key, "schema user");
                assert_eq!(node, expected);
            }
            CanonicalizeResult::Unmatched { .. } => panic!("expected Matched"),
        }
    }

    #[test]
    fn canonicalize_returns_unmatched_for_unknown_key() {
        let g = graph_with_concept("schema user");
        let res = canonicalize("create account", &g).unwrap();
        assert_eq!(
            res,
            CanonicalizeResult::Unmatched {
                key: "account creat".to_string()
            }
        );
    }

    #[test]
    fn canonicalize_applies_graph_synonyms() {
        // Concept carries the mapped key -> Matched with the mapped key.
        let mut g = graph_with_concept("creat user");
        g.declare_synonym("register_user", "create_user");
        match canonicalize("register_user", &g).unwrap() {
            CanonicalizeResult::Matched { key, .. } => assert_eq!(key, "creat user"),
            CanonicalizeResult::Unmatched { .. } => panic!("expected Matched"),
        }
        // No concept carries the mapped key -> Unmatched, but the key reflects
        // the direct mapping (raw lookup before normalization).
        let mut g = graph_with_concept("schema user");
        g.declare_synonym("register_user", "create_user");
        let res = canonicalize("register_user", &g).unwrap();
        assert_eq!(
            res,
            CanonicalizeResult::Unmatched {
                key: "creat user".to_string()
            }
        );
    }

    /// Acceptance contract: every row of `fixtures/canonicalization-cases.json`
    /// passes through [`canonical_key`] with a synonym table mirroring the fixture
    /// snapshot (`register_user` -> `create_user`).

    #[test]
    fn canonicalize_never_matches_observations() {
        // GRAPH-1: demote creates Observations that may legally carry the same
        // canonical key as agent-declared content (partial-UNIQUE errata,
        // muse-spark M1-M2). The step-5 matcher must skip them — new agent
        // content must never attach to a context-overflow record (spec §7
        // demote semantics).
        let g = graph_with_concept_typed("schema user", "Observation");
        match canonicalize("UserSchema", &g).unwrap() {
            CanonicalizeResult::Matched { .. } => panic!("Observation must not match"),
            CanonicalizeResult::Unmatched { key } => assert_eq!(key, "schema user"),
        }

        // An Entity carrying the key still matches (the filter is type-based).
        let g = graph_with_concept("schema user");
        assert!(matches!(
            canonicalize("UserSchema", &g).unwrap(),
            CanonicalizeResult::Matched { .. }
        ));

        // An Observation that shadows an Entity's key never wins: the Entity is
        // the only candidate. (Two same-key non-Observation concepts are
        // impossible — schema UNIQUE + insert_concept's collision check.)
        let snap: GraphSnapshot = serde_json::from_value(serde_json::json!({
            "session_id": "test-session",
            "root_goal": null,
            "created_at": "2026-08-10T09:00:00Z",
            "closed_at": null,
            "interactions": [{
                "id": "f0000000-0000-4000-8000-000000000001",
                "session_id": "test-session",
                "agent_id": "agent-a",
                "prompt_text": "seed",
                "previous_id": null,
                "created_at": "2026-08-10T09:00:00Z"
            }],
            "concepts": [
                {
                    "id": "f0000000-0000-4000-8000-000000000002",
                    "session_id": "test-session",
                    "content": "drift note",
                    "canonical_key": "schema user",
                    "concept_type": "Observation",
                    "origin_interaction": "f0000000-0000-4000-8000-000000000001",
                    "origin_agent": "agent-a",
                    "created_at": "2026-08-10T09:00:00Z",
                    "access_count": 0,
                    "last_accessed": null,
                    "gc_survived": 0,
                    "canonization_status": "None",
                    "blast_radius": null,
                    "last_demotion_time": null,
                    "embedding": null
                },
                {
                    "id": "f0000000-0000-4000-8000-000000000004",
                    "session_id": "test-session",
                    "content": "user schema",
                    "canonical_key": "schema user",
                    "concept_type": "Entity",
                    "origin_interaction": "f0000000-0000-4000-8000-000000000001",
                    "origin_agent": "agent-a",
                    "created_at": "2026-08-10T09:00:00Z",
                    "access_count": 0,
                    "last_accessed": null,
                    "gc_survived": 0,
                    "canonization_status": "None",
                    "blast_radius": null,
                    "last_demotion_time": null,
                    "embedding": null
                }
            ],
            "edges": [
                {
                    "id": "f0000000-0000-4000-8000-000000000003",
                    "session_id": "test-session",
                    "source": "f0000000-0000-4000-8000-000000000001",
                    "target": "f0000000-0000-4000-8000-000000000002",
                    "edge_type": "Derives",
                    "weight": 0.9,
                    "reinforcements": 1,
                    "created_at": "2026-08-10T09:00:00Z",
                    "last_reinforced": "2026-08-10T09:00:00Z"
                },
                {
                    "id": "f0000000-0000-4000-8000-000000000005",
                    "session_id": "test-session",
                    "source": "f0000000-0000-4000-8000-000000000001",
                    "target": "f0000000-0000-4000-8000-000000000004",
                    "edge_type": "Derives",
                    "weight": 0.9,
                    "reinforcements": 1,
                    "created_at": "2026-08-10T09:00:00Z",
                    "last_reinforced": "2026-08-10T09:00:00Z"
                }
            ],
            "synonyms": [],
            "reservations": [],
            "canonization_events": [],
            "embedding": null
        }))
        .unwrap();
        let g = Graph::from_snapshot(snap).unwrap();
        match canonicalize("UserSchema", &g).unwrap() {
            CanonicalizeResult::Matched { node, .. } => {
                assert_eq!(node.0.to_string(), "f0000000-0000-4000-8000-000000000004")
            }
            CanonicalizeResult::Unmatched { .. } => panic!("Entity must match"),
        }
    }

    #[cfg(feature = "fixtures")]
    #[test]
    fn fixture_canonicalization_cases_all_pass() {
        let cases = crate::fixtures::load_canonicalization_cases().unwrap();
        let cases = cases.as_array().expect("cases file is an array");
        assert_eq!(cases.len(), 11, "fixture row count is part of the contract");
        for case in cases {
            let input = case["input"].as_str().expect("case input");
            let expected = case["expected_key"].as_str().expect("case expected_key");
            let category = case["category"].as_str().unwrap_or("?");
            assert_eq!(
                canonical_key(input, fixture_synonyms),
                expected,
                "category {category}, input {input:?}"
            );
        }
    }
}
