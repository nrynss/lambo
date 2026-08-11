//! Canonicalization pipeline (T2.2) — spec §7.1 steps 1–5.
//!
//! Steps, in order:
//! 1. Normalize — split camelCase boundaries, lowercase, split `[-_ ]` +
//!    whitespace, strip stopwords ([`STOPWORDS`], pinned to the fixture
//!    convention `scripts/gen-fixtures.py`).
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

use crate::graph::Graph;
use crate::types::{LamboError, NodeId};

/// Stopwords stripped during normalization — pinned to the fixture convention
/// (`scripts/gen-fixtures.py` `STOPWORDS`).
const STOPWORDS: [&str; 13] = [
    "the", "a", "an", "for", "of", "at", "in", "to", "on", "and", "or", "is", "are",
];

/// Shared Porter (English) stemmer — created once, reused for every call.
static STEMMER: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::English));

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
/// Lowercase, split `[-_ ]` and camelCase boundaries, drop stopwords, Porter
/// stem. NO sort, NO synonym lookup, NO join (those are [`canonical_key`]'s job).
/// Pure — no `Graph` dependency, so recall's keyword index (T2.6) can tokenize
/// without a graph.
pub fn normalize_tokens(content: &str) -> Vec<String> {
    split_camel_case(content)
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
        .find(|c| c.canonical_key == key)
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
