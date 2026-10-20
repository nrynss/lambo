//! Near-duplicate alias **suggestions** (#178): concepts the canonical key
//! does not unify but whose text, or whose vector, says they are the same
//! thing. Suggestion only: nothing here merges anything. The pairs feed
//! `lambo canon alias-preview --similar`, which shows what redirecting them
//! would change, and an operator who agrees writes them into a reviewed
//! `--map`.
//!
//! # Lexical candidates
//!
//! Each live concept's content is *folded*: lowercased, with every run of
//! `_`, `-` and whitespace turned into one space, trimmed. A concept whose
//! folded text is shorter than [`SimilarParams::min_chars`] is skipped
//! (Jaccard over a handful of 3-grams is unstable). The rest get a MinHash
//! signature over their character 3-grams: [`PERMUTATIONS`] (128)
//! deterministic hash permutations. LSH with [`BANDS`] bands of [`ROWS`] rows
//! (32 × 4) proposes candidate pairs without comparing every pair, and each
//! candidate is then scored by its **exact** 3-gram Jaccard, kept at
//! [`SimilarParams::jaccard`] (0.7) or above. MinHash only nominates; the
//! exact Jaccard decides.
//!
//! # Vector candidates
//!
//! Lambo decides vector merges once, at write time, so a pair whose older
//! concept had no embedding when the newer one was written was never
//! compared. With [`SimilarParams::cosine`] set (the session's
//! `semantic_match_threshold`), every pair of embedded concepts at or above
//! it is a candidate too. That is a full pairwise scan, `O(n²)` dot products
//! of unit vectors split across the cores: an offline tool's cost (about
//! 1.5 s for 10k 1024-wide vectors on 8 cores, see `similar_timing`);
//! `--no-vectors` turns it off.
//!
//! # Guards (applied to both sources)
//!
//! * **Digit runs equal**, in order: `release v0.5.0` and `release v0.6.0`
//!   differ in their digit runs and are never suggested, whatever their
//!   Jaccard. This is the #148 failure (versions, hashes, ports and issue
//!   numbers folded together), and the evidence on #178 shows lexical
//!   similarity alone suggests most such pairs.
//! * **Identifier tokens equal**: the tokens
//!   [`crate::graph::canonical::is_identifier_token`] calls identifiers (a
//!   digit, `/` or `.`) must be the same list. `src/main.rs` and
//!   `src/mein.rs` are different files.
//! * **Same concept type**, unless the folded texts or the canonical keys
//!   are identical: the same
//!   text stored once as an `Entity` and once as an `Observation` is the
//!   duplicate this exists to find.
//! * **No `Contradicts` edge** either way between the two (#19, #181): a
//!   contradiction is two different claims, however alike they read.
//! * **Canonical concepts** are left out unless
//!   [`SimilarParams::include_canonical`] is set.
//! * **Overrides**: a pair an operator rejected or accepted
//!   ([`SimilarParams::rejected`], [`SimilarParams::accepted`], from an
//!   `--overrides` file, see [`split_overrides`]) is never suggested again.
//!
//! # Direction and overlap
//!
//! The pair's *ruling* is the concept with more concept citations, then the
//! higher canonization status, then the older, then the smaller id; the
//! other is the *alias*. Pairs are taken strongest first, and a pair that
//! would make a concept an alias twice, or both an alias and a ruling, is
//! dropped as an overlap (counted), so the redirects the preview plans are a
//! set of stars: no chains, no ambiguity. At most
//! [`SimilarParams::max_pairs`] are suggested.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

pub use crate::graph::canonical::fold;
use crate::graph::canonical::{canonical_key, digit_runs, identifier_tokens};
use crate::graph::Graph;
use crate::types::{CanonizationStatus, ConceptType, EdgeType, Node, NodeId};

/// MinHash permutations per signature.
pub const PERMUTATIONS: usize = 128;
/// LSH bands.
pub const BANDS: usize = 32;
/// Rows per LSH band. `BANDS * ROWS == PERMUTATIONS`.
pub const ROWS: usize = 4;
const _: () = assert!(BANDS * ROWS == PERMUTATIONS);

/// The default exact-Jaccard floor.
pub const DEFAULT_JACCARD: f64 = 0.7;
/// The default minimum folded length.
pub const DEFAULT_MIN_CHARS: usize = 12;
/// The default cap on suggestions.
pub const DEFAULT_MAX_PAIRS: usize = 200;
/// An LSH bucket larger than this is not expanded pairwise: a bucket that
/// big is a boilerplate prefix shared by many concepts, and its pairs are
/// still found through their other bands when they are genuinely close.
const MAX_BUCKET: usize = 256;

/// What [`detect_similar`] looks for.
#[derive(Clone, Debug, PartialEq)]
pub struct SimilarParams {
    /// Exact 3-gram Jaccard floor for a lexical pair.
    pub jaccard: f64,
    /// Folded characters a concept needs to take part in the lexical source.
    pub min_chars: usize,
    /// Cosine floor for the vector source; `None` turns it off.
    pub cosine: Option<f64>,
    /// Also suggest pairs where a concept is Canonical.
    pub include_canonical: bool,
    /// Pairs an operator rejected, either order: never suggested.
    pub rejected: HashSet<(NodeId, NodeId)>,
    /// Pairs an operator accepted (alias, ruling): planned as reviewed pairs
    /// by the caller, never suggested again, and their concepts take no
    /// other suggestion that would chain with them.
    pub accepted: Vec<(NodeId, NodeId)>,
    /// At most this many suggestions.
    pub max_pairs: usize,
}

impl Default for SimilarParams {
    fn default() -> Self {
        Self {
            jaccard: DEFAULT_JACCARD,
            min_chars: DEFAULT_MIN_CHARS,
            cosine: Some(crate::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT),
            include_canonical: false,
            rejected: HashSet::new(),
            accepted: Vec::new(),
            max_pairs: DEFAULT_MAX_PAIRS,
        }
    }
}

/// Which source nominated a pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimilarSource {
    /// MinHash/LSH, confirmed by exact Jaccard.
    Lexical,
    /// Cosine at or above the threshold.
    Vector,
    /// Both.
    Both,
}

/// One suggested pair.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimilarPair {
    /// The concept whose citations would move.
    pub alias: NodeId,
    /// The concept they would move to.
    pub ruling: NodeId,
    /// Which source nominated it.
    pub source: SimilarSource,
    /// Exact 3-gram Jaccard of the folded texts (always computed).
    pub jaccard: f64,
    /// Cosine of the two embeddings, when both have one of one width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cosine: Option<f64>,
    /// Concept citations of the alias.
    pub alias_citations: usize,
    /// Concept citations of the ruling.
    pub ruling_citations: usize,
    /// The alias's text.
    pub alias_content: String,
    /// The ruling's text.
    pub ruling_content: String,
}

/// Why candidates were not suggested, by guard.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimilarGuarded {
    /// Concepts too short for the lexical source.
    pub too_short: usize,
    /// Candidates below the Jaccard floor after the exact check.
    pub below_jaccard: usize,
    /// Digit runs differ.
    pub digits_differ: usize,
    /// Identifier tokens differ.
    pub identifiers_differ: usize,
    /// Types differ and the texts are not identical.
    pub type_differs: usize,
    /// A `Contradicts` edge joins them.
    pub contradicts: usize,
    /// A Canonical concept, without the operator flag.
    pub canonical: usize,
    /// An operator rejected the pair.
    pub rejected: usize,
    /// An operator already accepted the pair.
    pub accepted: usize,
    /// Would chain or double up with a stronger pair.
    pub overlap: usize,
    /// Past [`SimilarParams::max_pairs`].
    pub capped: usize,
}

/// What [`detect_similar`] found.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SimilarReport {
    /// Suggestions, strongest first.
    pub pairs: Vec<SimilarPair>,
    /// Candidates the guards removed.
    pub guarded: SimilarGuarded,
    /// Distinct candidate pairs LSH nominated.
    pub lexical_candidates: usize,
    /// Distinct candidate pairs at or above the cosine floor.
    pub vector_candidates: usize,
}

/// FNV-1a over a 3-gram's UTF-8 bytes: stable across runs and platforms.
fn hash_gram(gram: &[char]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut buf = [0u8; 4];
    for c in gram {
        for b in c.encode_utf8(&mut buf).bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// The distinct hashed character 3-grams of folded text, sorted.
fn shingles(folded: &str) -> Vec<u64> {
    let chars: Vec<char> = folded.chars().collect();
    let mut out: Vec<u64> = chars.windows(3).map(hash_gram).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// splitmix64's finalizer: one permutation of the 3-gram hashes.
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The deterministic seeds of the permutations.
fn seeds() -> [u64; PERMUTATIONS] {
    std::array::from_fn(|i| mix(0x5EED_0178_u64 ^ (i as u64).wrapping_mul(0xA24B_AED4_963E_E407)))
}

/// A MinHash signature: per permutation, the least permuted 3-gram hash.
fn signature(shingles: &[u64], seeds: &[u64; PERMUTATIONS]) -> [u64; PERMUTATIONS] {
    let mut sig = [u64::MAX; PERMUTATIONS];
    for &s in shingles {
        for (slot, seed) in sig.iter_mut().zip(seeds) {
            let v = mix(s ^ seed);
            if v < *slot {
                *slot = v;
            }
        }
    }
    sig
}

/// Exact Jaccard of two sorted, deduplicated shingle sets.
fn jaccard(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut both) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                both += 1;
                i += 1;
                j += 1;
            }
        }
    }
    both as f64 / (a.len() + b.len() - both) as f64
}

/// One concept as the detector sees it.
struct Entry<'g> {
    id: NodeId,
    content: &'g str,
    folded: String,
    /// The canonical key without synonyms: "identical normalized content".
    key: String,
    shingles: Vec<u64>,
    concept_type: ConceptType,
    status: CanonizationStatus,
    created_at: chrono::DateTime<chrono::Utc>,
    embedding: Option<&'g [f32]>,
}

fn status_rank(s: CanonizationStatus) -> u8 {
    match s {
        CanonizationStatus::Canonical => 3,
        CanonizationStatus::Venerable => 2,
        CanonizationStatus::Candidate => 1,
        CanonizationStatus::None => 0,
    }
}

/// Concept citations: `Dependency` edges into the concept from another
/// concept.
fn citation_counts(g: &Graph) -> HashMap<NodeId, usize> {
    let mut out: HashMap<NodeId, usize> = HashMap::new();
    for e in g.edges() {
        if e.edge_type == EdgeType::Dependency
            && e.source != e.target
            && matches!(g.node(e.source), Some(Node::Concept(_)))
        {
            *out.entry(e.target).or_default() += 1;
        }
    }
    out
}

/// A unit-length copy of `v`, or `None` for a zero or non-finite vector.
fn unit(v: &[f32]) -> Option<Vec<f32>> {
    let n = v
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    (n.is_finite() && n > 0.0).then(|| v.iter().map(|x| (f64::from(*x) / n) as f32).collect())
}

/// The dot product of two unit vectors, in 16 independent lanes so the
/// compiler vectorizes it.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let (ca, ta) = a.as_chunks::<16>();
    let (cb, tb) = b.as_chunks::<16>();
    let mut acc = [0f32; 16];
    for (x, y) in ca.iter().zip(cb) {
        for k in 0..16 {
            acc[k] += x[k] * y[k];
        }
    }
    acc.iter().sum::<f32>() + ta.iter().zip(tb).map(|(x, y)| x * y).sum::<f32>()
}

/// Every pair `(i, j)`, `i < j`, of embedded entries whose cosine is at or
/// above `floor`, sorted. A full pairwise scan, split by rows across the
/// machine's cores: the order of the result does not depend on the split.
/// The unit-vector dot is only the filter; a pair within `1e-4` of the
/// floor is decided by the exact [`crate::embed::cosine`], so the source
/// agrees with the cosine the report prints.
fn vector_pairs(entries: &[Entry<'_>], floor: f64) -> Vec<(usize, usize)> {
    let units: Vec<(usize, Vec<f32>)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.embedding.and_then(unit).map(|u| (i, u)))
        .collect();
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .clamp(1, 16)
        .min(units.len().max(1));
    let exact = |a: usize, b: usize| match (entries[a].embedding, entries[b].embedding) {
        (Some(x), Some(y)) => f64::from(crate::embed::cosine(x, y)) >= floor,
        _ => false,
    };
    let scan = |t: usize| {
        let mut out = Vec::new();
        for x in (t..units.len()).step_by(threads) {
            let (a, ua) = &units[x];
            for (b, ub) in &units[x + 1..] {
                if ua.len() != ub.len() {
                    continue;
                }
                let d = f64::from(dot(ua, ub));
                if d >= floor + 1e-4 || (d >= floor - 1e-4 && exact(*a, *b)) {
                    out.push((*a, *b));
                }
            }
        }
        out
    };
    let mut pairs: Vec<(usize, usize)> = if threads == 1 {
        scan(0)
    } else {
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads).map(|t| s.spawn(move || scan(t))).collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("a vector scan thread panicked"))
                .collect()
        })
    };
    pairs.sort_unstable();
    pairs
}

/// Find near-duplicate pairs in `g` (see the module docs). Deterministic:
/// the same graph and parameters give the same report.
pub fn detect_similar(g: &Graph, params: &SimilarParams) -> SimilarReport {
    let mut report = SimilarReport::default();
    let mut entries: Vec<Entry<'_>> = g
        .concepts()
        .map(|c| {
            let folded = fold(&c.content);
            Entry {
                id: c.id,
                content: c.content.as_str(),
                shingles: shingles(&folded),
                key: canonical_key(&c.content, |_| None),
                folded,
                concept_type: c.concept_type,
                status: c.canonization_status,
                created_at: c.created_at,
                embedding: c.embedding.as_deref().filter(|v| !v.is_empty()),
            }
        })
        .collect();
    entries.sort_by_key(|e| e.id.0);

    // Candidates, as (lower index, higher index), with their source.
    let mut candidates: BTreeMap<(usize, usize), (bool, bool)> = BTreeMap::new();

    // Lexical: LSH over MinHash signatures.
    let seeds = seeds();
    let mut buckets: HashMap<(usize, [u64; ROWS]), Vec<u32>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.folded.chars().count() < params.min_chars {
            report.guarded.too_short += 1;
            continue;
        }
        let sig = signature(&e.shingles, &seeds);
        for band in 0..BANDS {
            let mut key = [0u64; ROWS];
            key.copy_from_slice(&sig[band * ROWS..(band + 1) * ROWS]);
            buckets.entry((band, key)).or_default().push(i as u32);
        }
    }
    // Members are pushed in index order, so every pair is (lower, higher).
    let mut lexical: Vec<(u32, u32)> = Vec::new();
    for members in buckets.values() {
        if members.len() < 2 || members.len() > MAX_BUCKET {
            continue;
        }
        for (x, &a) in members.iter().enumerate() {
            for &b in &members[x + 1..] {
                lexical.push((a, b));
            }
        }
    }
    drop(buckets);
    lexical.sort_unstable();
    lexical.dedup();
    report.lexical_candidates = lexical.len();
    for (a, b) in lexical {
        let (a, b) = (a as usize, b as usize);
        if jaccard(&entries[a].shingles, &entries[b].shingles) >= params.jaccard {
            candidates.entry((a, b)).or_default().0 = true;
        } else {
            report.guarded.below_jaccard += 1;
        }
    }

    // Vector: every embedded pair at or above the floor.
    let cosine_of = |a: &Entry<'_>, b: &Entry<'_>| match (a.embedding, b.embedding) {
        (Some(x), Some(y)) if x.len() == y.len() => Some(f64::from(crate::embed::cosine(x, y))),
        _ => None,
    };
    if let Some(floor) = params.cosine {
        for (a, b) in vector_pairs(&entries, floor) {
            report.vector_candidates += 1;
            candidates.entry((a, b)).or_default().1 = true;
        }
    }

    // Guards.
    let citations = citation_counts(g);
    let cites = |id: NodeId| citations.get(&id).copied().unwrap_or(0);
    let mut kept: Vec<SimilarPair> = Vec::new();
    for ((a, b), (lex, vec)) in candidates {
        let (ea, eb) = (&entries[a], &entries[b]);
        let guarded = &mut report.guarded;
        let either = |set: &dyn Fn(NodeId, NodeId) -> bool| set(ea.id, eb.id) || set(eb.id, ea.id);
        if either(&|x, y| params.rejected.contains(&(x, y))) {
            guarded.rejected += 1;
            continue;
        }
        if either(&|x, y| params.accepted.contains(&(x, y))) {
            guarded.accepted += 1;
            continue;
        }
        if digit_runs(&ea.folded) != digit_runs(&eb.folded) {
            guarded.digits_differ += 1;
            continue;
        }
        if identifier_tokens(&ea.folded) != identifier_tokens(&eb.folded) {
            guarded.identifiers_differ += 1;
            continue;
        }
        if ea.concept_type != eb.concept_type && ea.folded != eb.folded && ea.key != eb.key {
            guarded.type_differs += 1;
            continue;
        }
        if crate::daemon::contradiction::contradicts_either_way(g, ea.id, eb.id) {
            guarded.contradicts += 1;
            continue;
        }
        if !params.include_canonical
            && (ea.status == CanonizationStatus::Canonical
                || eb.status == CanonizationStatus::Canonical)
        {
            guarded.canonical += 1;
            continue;
        }
        // The ruling: more citations, higher status, older, smaller id.
        let rank = |e: &Entry<'_>| {
            (
                std::cmp::Reverse(cites(e.id)),
                std::cmp::Reverse(status_rank(e.status)),
                e.created_at,
                e.id.0,
            )
        };
        let (ruling, alias) = if rank(ea) <= rank(eb) {
            (ea, eb)
        } else {
            (eb, ea)
        };
        kept.push(SimilarPair {
            alias: alias.id,
            ruling: ruling.id,
            source: match (lex, vec) {
                (true, true) => SimilarSource::Both,
                (true, false) => SimilarSource::Lexical,
                _ => SimilarSource::Vector,
            },
            jaccard: jaccard(&ea.shingles, &eb.shingles),
            cosine: cosine_of(ea, eb),
            alias_citations: cites(alias.id),
            ruling_citations: cites(ruling.id),
            alias_content: alias.content.to_owned(),
            ruling_content: ruling.content.to_owned(),
        });
    }

    // Strongest first, then a star forest: no concept is an alias twice or
    // both an alias and a ruling.
    let strength = |p: &SimilarPair| p.jaccard.max(p.cosine.unwrap_or(0.0));
    kept.sort_by(|x, y| {
        strength(y)
            .total_cmp(&strength(x))
            .then_with(|| x.alias.0.cmp(&y.alias.0))
            .then_with(|| x.ruling.0.cmp(&y.ruling.0))
    });
    let mut aliases: HashSet<NodeId> = params.accepted.iter().map(|p| p.0).collect();
    let mut rulings: HashSet<NodeId> = params.accepted.iter().map(|p| p.1).collect();
    for p in kept {
        if aliases.contains(&p.alias) || rulings.contains(&p.alias) || aliases.contains(&p.ruling) {
            report.guarded.overlap += 1;
            continue;
        }
        if report.pairs.len() >= params.max_pairs {
            report.guarded.capped += 1;
            continue;
        }
        aliases.insert(p.alias);
        rulings.insert(p.ruling);
        report.pairs.push(p);
    }
    report
}

/// Split an overrides file into a reviewed-map text of its `accept` lines
/// and one of its `reject` lines, both for [`super::alias::parse_map`].
///
/// One override per line: `accept <alias> <ruling>` (the pair is decided:
/// it is planned as a reviewed pair and never suggested again) or
/// `reject <a> <b>` (never suggested again, either order). References are
/// node ids or 8-hex short ids, never content, so the file holds no concept
/// text. `#` starts a comment. Every line keeps its number in both outputs
/// (the other kind's lines are blank), so a refusal names the file's line.
pub fn split_overrides(text: &str) -> Result<(String, String), String> {
    let (mut accept, mut reject) = (String::new(), String::new());
    for (n, raw) in text.lines().enumerate() {
        let body = raw.split('#').next().unwrap_or("").trim();
        let (verb, rest) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
        match verb {
            "" => {}
            "accept" => accept.push_str(rest.trim()),
            "reject" => reject.push_str(rest.trim()),
            other => {
                return Err(format!(
                    "line {}: {other:?}: expected `accept <alias> <ruling>` or `reject <a> <b>`",
                    n + 1
                ))
            }
        }
        accept.push('\n');
        reject.push('\n');
    }
    Ok((accept, reject))
}
