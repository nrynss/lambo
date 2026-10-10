//! Hybrid canonicalization step 6 (T7.2) — spec §7.1 step 6, §5, §3.2.
//!
//! On a canonical-key miss ([`CanonicalizeResult::Unmatched`]) under
//! `MatchStrategy::Hybrid`, this module embeds the concept **with context**
//! (name + origin interaction text — the live BGE-M3 calibration rule; see
//! `dev-diary/PHASE-7-embeddings.md`), queries [`GraphStore::vector_candidates_checked`],
//! and — when a candidate sits at or above `semantic_match_threshold` — records
//! the merge as a decaying [`EdgeType::Semantic`] edge to the matched concept.
//! Below the threshold, or when the **store capability** is absent, it degrades
//! to the byte-identical `MatchStrategy::Canonical` outcome (a fresh,
//! keyword-only concept), logging the fallback once per session.
//!
//! **An embedder failure does not degrade — it fails the write** (J3-R3-1).
//! A capability miss is a declared, deterministic configuration: every write of
//! the session takes the same keyword-only arm and the session says so once. An
//! embedder refusal or timeout is a *per-input* surprise: before this rule, the
//! concept was applied with `embedding: NULL`, reported to the caller as an
//! unqualified success, and became unfindable by semantic recall — the located
//! mechanism behind the dogfood store's 92/100 unembedded concepts
//! (DOGFOOD-FINDINGS 2026-08-20) and behind J3-R3-1's estimator poisoning (a
//! 3 ms non-embed sampled as a fast write inflated the drain rate 20–45× and
//! abandoned 326/361 acked writes). *Applied without its vector* is not a lesser
//! success; it is a different outcome, and the caller decides what to do with
//! it — retry, shorten, or accept keyword-only by configuring
//! `MatchStrategy::Canonical`.
//!
//! **The blast radius of that rule, stated (J3 round-1 F3).** "The
//! capability-absent arm stays a degrade" is a claim about the **store**, and it
//! is easy to misread as a claim about the embedder. `vector_ok` below is
//! `store.capabilities().contains(VECTOR_SEARCH)`, `SqliteStore` advertises that
//! capability **unconditionally**, `embed::build_embedder` yields either an
//! embedder or a startup error (there is no "no embedder configured" state that
//! reaches this function), and `MatchStrategy::Hybrid` is the config default. So
//! **there is no arm in which a missing or dead *embedder* degrades**: on a
//! default deployment an unreachable or timing-out llama.cpp fails *every*
//! `lambo_derive` for as long as it is unreachable. That is deliberate — it is
//! the honesty rule above, and the alternative is the silent
//! `embedding: NULL` write it replaced — but it is an **availability** fact, not
//! only a correctness one, and the lawful keyword-only mode spec §3.2 promises is
//! reached by *declaring* it (`match_strategy = "canonical"`), never by an
//! embedder's silence. A per-call failure cannot be promoted to a
//! session-uniform degrade because the two are indistinguishable at the
//! protocol: N1's classification can tell "not reached" from "refused", but not
//! "not reached, and will not be for the rest of this session" from "not
//! reached, for 200 ms". Declared or refused, never guessed.
//!
//! # The seam (design decision of record)
//!
//! The sync twin [`crate::graph::derive::derive`] takes `&mut Graph` and cannot
//! host the async hybrid step: embedding + `vector_candidates_checked` are I/O, and the
//! graph lock must **never** be held across an `.await` (spec §6.4; see
//! `src/graph/mod.rs`). This module is the async twin the session owner
//! (T8.1's `Memory`) calls in place of `derive` while `MatchStrategy::Hybrid` is
//! active. It manages the lock/await frontier itself:
//!
//! 1. **Plan (brief read lock, no I/O).** Canonicalize each unique concept,
//!    validate inputs (interaction exists, empty-key rejection, reflexive-`ParentOf`
//!    rejection — mirroring derive), read the session's stamped
//!    [`EmbeddingContract`], and perform the mid-session contract check
//!    ([`EmbeddingContract::ensure_compatible`]) **before** any embedding — a
//!    kind/model/dim swap is refused without re-embedding. Release the lock.
//! 2. **Gather (async, no lock).** For each `Unmatched` concept: build the
//!    context, `embedder.embed`, then the checked candidate read of the source
//!    this call was handed (`VectorCandidates`): the holder's graph, ranked in
//!    RAM under a brief read lock with no await, when the store ranks by an
//!    exact scan (#8) or lags the holder past its flush (the Elastic tier,
//!    #18); the store for flushed concepts plus the holder's unflushed ones
//!    ranked in RAM, when the store's search sees each flush commit (the
//!    Postgres family, #60); both per `GraphStore::holder_derive_source`.
//!    Otherwise `store.vector_candidates_checked` through the [`GraphStore`]
//!    trait only. A capability-miss marks the concept
//!    for the canonical fallback (logged once per session); an embed failure or
//!    timeout fails the whole call before anything is written (J3-R3-1); a
//!    genuine backend `StoreError` (not a `Capability` miss) propagates. The
//!    `parent_of` ends this call will create (in neither the graph nor
//!    `concepts`) are embedded here too, under the same rules but without the
//!    candidate lookup (issue #16 §2).
//! 3. **Commit (write lock, sync).** Re-acquire the write lock and compare the
//!    current epoch with the planned epoch. A concurrent daemon/MCP mutation
//!    discards the stale gather and retries. Revalidate the embedding contract
//!    under this lock, apply the logical derive to a cloned graph, and swap only
//!    after every write succeeds. The stamp and all node/edge mutations are thus
//!    atomic in RAM; no `.await` is held here.
//!
//! This remains sound even when future callers introduce concurrent writers;
//! correctness does not depend on every writer participating in a private mutex.
//!
//! # Merge shape (ambiguity resolved — see handoff)
//!
//! A `Semantic` edge is Concept→Concept (`record_edge` rejects any other
//! endpoint, adve-review GRAPH-2) and `record_edge` also rejects self-loops, so
//! a merge must realize the new content as its **own** concept node (distinct
//! canonical key — it was `Unmatched`, so it never duplicates an existing key)
//! joined to the matched concept by a decaying `Semantic` edge. This is the
//! "merge": recall expansion follows `Semantic` (spec §8) and canonization (P6)
//! later physically folds them. The new concept carries its computed embedding
//! so it too becomes a future vector candidate.
//! The merge target is surfaced in the outcome's [`DeriveOutcome::semantic_merged`]
//! — kept separate from `matched` because a merge does not re-upsert the target
//! nor `Derives`-reinforce it, and `matched` must stay faithful to the sync
//! `derive` contract (PHASE-7 T7.2 remediation, MINOR-3). A concept for which no
//! vector exists (capability-absent, a store that refuses
//! `vector_candidates_checked` after advertising the capability, or an
//! invalid non-Concept merge target — see the commit-time validation) is written
//! with `embedding: None` — an embed *failure* no longer reaches a write at all
//! (J3-R3-1, module doc above); a failed embed likewise never stamps the session's
//! embedding contract (MINOR-2). See "Vector persistence for fresh concepts"
//! below for the one arm that changed. At commit, each
//! content is re-canonicalized against the graph as written this call so that
//! distinct contents collapsing onto one canonical key resolve Matched to the
//! just-created node (mirroring sync `derive`'s within-call dedup) instead of
//! erroring on `insert_concept`'s UNIQUE key collision (MAJOR-1).
//!
//! # Vector persistence for fresh concepts (L82-4 — product decision 2026-08-14)
//!
//! A `Fresh` concept whose embedding was **successfully computed** now keeps that
//! vector even though no candidate cleared `semantic_match_threshold`. Until this
//! decision the below-threshold arm wrote `embedding: None`, and because *every*
//! organically-derived concept takes that arm, `concepts.embedding IS NULL` held
//! for every row the product itself wrote: the live-CockroachDB review measured
//! **0 of 13** organic concepts with a vector, so recall's vector leg could never
//! fire on organically-derived memory — only out-of-band seeding produced
//! vectors (`adversarial-review/adve-review-t8.2-t8.3-live.md`, finding L82-4).
//! Persisting the vector is what makes derived memory vector-recallable. Arms
//! where no vector exists still write `None` — a vector is never invented.
//!
//! ## The chosen semantic: **threshold-preserving** (recall-visible, merge bar unchanged)
//!
//! A persisted vector is read by [`GraphStore::vector_candidates_checked`], which serves
//! two callers: recall's vector leg (read-only ranking, T5.1) and this module's
//! merge step. The precision-bias / anti-over-merge law (MAJOR-1) survives as
//! three properties, each pinned by a test:
//!
//! 1. **A refusal is never recorded as an endorsement.** A below-threshold
//!    concept gets its vector but **no `Semantic` edge**. Recall expansion (spec
//!    §8) and P6 canonization's physical fold both travel `Semantic` edges, so a
//!    fresh concept is reachable only through its own similarity to the query —
//!    never transitively, out of an unrelated concept's neighbourhood. *The
//!    over-merge scenario this prevents:* concepts A and B sit at cosine 0.84
//!    (below the bar). Writing B's vector must not join B to A; if it did, a
//!    later recall on A would pull in B — and, through B, whatever later merges
//!    into B — collapsing two distinct topics into one recall neighbourhood via
//!    links no single comparison ever endorsed. No `Semantic` edge is written, so
//!    that chain cannot start.
//! 2. **The merge bar did not move.** A merge still requires
//!    `score >= semantic_match_threshold` (0.85 default). G1's BGE-M3 corpus
//!    overlaps real paraphrases and related-but-distinct pairs, so lowering the
//!    score-only bar would add measured false positives; 0.85 remains the
//!    deliberate precision bias. See `evidence/mooshik-g-recall-calibration/`.
//!    Selection still applies `top_tier`'s finite/`[0,1]`/at-or-above-threshold
//!    validation, and the commit phase both validates every tied candidate as a
//!    real Concept and picks the merge target stably (canonical key, then id).
//!    Persisting a vector lowers nothing.
//! 3. **A vector minted in this call can never drive a merge in this call.**
//!    Candidates come from the live graph (on a holder whose store ranks
//!    exactly, #8, or lags it, #18 and #60) or from the store, and neither can see this call's staged
//!    writes: they live on a private clone until the commit swaps it in;
//!    `*target != id` is the defence in depth.
//!
//! ## What is deliberately *not* claimed
//!
//! Once committed, a fresh vector **is** a legal merge *target* for a later
//! derive (`Resolution::HybridMerge { targets }`): at once on a holder that ranks
//! in its graph for derive (SQLite since #8, the Elastic tier since #18, the
//! Postgres family since #60, which is why an unflushed concept can be merged
//! into), after the next flush on one that reads the store. That is unavoidable here:
//! the checked candidate read targets `embedding IS NOT NULL` and cannot
//! tell the merge leg from the recall leg apart. A strict target-exclusion would
//! need durable per-vector provenance (a new `concepts` column plus a migration
//! in every adapter) and — because after this change *every* organic concept is
//! fresh-persisted — it would exclude every organic concept and leave the merge
//! leg permanently inert, a strictly larger product change than L82-4 asks for.
//! The threshold, not the provenance of the vector, is the precision instrument.
//!
//! One provenance rule does exist since #22, and it is about modality, not
//! freshness: `concepts.embedding_source` marks a supplied (image) vector,
//! and a text item's merge tier drops those candidates, so a text claim is
//! never absorbed into a picture. An image item never takes the merge leg at
//! all (see `derive_with`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;

use crate::embed::Embedder;
use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::derive::{
    DeriveOutcome, ParentOf, COOCCURRENCE_WEIGHT, HIERARCHICAL_WEIGHT, PARENT_OF_CONCEPT_TYPE,
};
use crate::graph::Graph;
use crate::store::vector_source::VectorCandidates;
use crate::store::GraphStore;
use crate::types::{
    tie_break_by_key, AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType,
    EmbeddingContract, EmbeddingSource, LamboError, Node, NodeId, SessionId, StoreError,
    SuppliedVector,
};

/// Default merge threshold (spec §7.1 step 6). Configurable per call
/// (driven by `Config::semantic_match_threshold`); G1's BGE-M3 measurements
/// retain 0.85 because score-only paraphrase and related-distinct bands overlap.
/// It is intentionally biased toward precision: under-merging into separate
/// concepts is safe for canonization.
pub const SEMANTIC_MATCH_THRESHOLD_DEFAULT: f64 = 0.85;

/// How many vector candidates to request from the store per concept.
pub const VECTOR_CANDIDATE_LIMIT: usize = 8;

/// Hard request bounds are checked before the first await. They prevent one
/// derive call from turning attacker-controlled input into unbounded external
/// embed/store work while leaving normal multi-concept calls ample headroom.
///
/// `MAX_HYBRID_CONCEPTS`, `MAX_HYBRID_PARENT_PAIRS` and `MAX_HYBRID_WORK_ITEMS`
/// bound the request's size (concepts plus pairs, each pair one item): the
/// canonicalization work under the read lock. They do not bound the embeds,
/// because each pair can name two new ends; [`MAX_HYBRID_EMBEDS`] does.
pub const MAX_HYBRID_CONCEPTS: usize = 256;
pub const MAX_HYBRID_PARENT_PAIRS: usize = 256;
pub const MAX_HYBRID_WORK_ITEMS: usize = 512;
/// The most embeds one hybrid derive may make: one per unmatched concept plus
/// one per `parent_of` end it will create (issue #16 §2), all inside one
/// [`HYBRID_IO_TIMEOUT`]. It keeps the ceiling the call had before ends were
/// embedded (`MAX_HYBRID_CONCEPTS`), so embedding ends cannot push a call
/// that fit the deadline past it. Checked at the J3 ack
/// ([`validate_embed_budget`]) and again at plan, before any I/O; a call
/// over it is refused with a `Config` error rather than timing out at apply.
pub const MAX_HYBRID_EMBEDS: usize = MAX_HYBRID_CONCEPTS;
pub const MAX_HYBRID_CONTEXT_BYTES: usize = 16 * 1024;
pub const HYBRID_IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HYBRID_REPLANS: usize = 8;

/// Initial weight of a `Semantic` merge edge: the accepted cosine similarity,
/// clamped into the legal `[0, MAX_EDGE_WEIGHT]` range. A merge only happens
/// at or above the configured precision threshold, so the weight is always
/// positive and finite, and the edge
/// decays over time (see the spec §5 decay table, where `Semantic` decays).
fn semantic_weight(score: f64) -> f64 {
    score.clamp(0.0, crate::graph::MAX_EDGE_WEIGHT)
}

/// The candidates tied at the highest valid score: finite, inside `[0,1]`, and
/// at/above the configured threshold. Usually exactly one element; the whole
/// tied tier is carried back rather than one pick because the raw UUID must
/// not decide an exact tie (ids are run-minted) and this async gather phase
/// holds no graph to read canonical keys from. The commit phase owns the
/// stable pick: smallest canonical key, then smallest id (issue #2,
/// remediation round 1). Works on unsorted input.
fn top_tier(
    hits: &[crate::types::Scored<NodeId>],
    threshold: f64,
) -> Vec<&crate::types::Scored<NodeId>> {
    let mut tier: Vec<&crate::types::Scored<NodeId>> = Vec::new();
    let mut best: Option<&crate::types::Scored<NodeId>> = None;
    for c in hits
        .iter()
        .filter(|c| c.score.is_finite() && (0.0..=1.0).contains(&c.score) && c.score >= threshold)
    {
        match best {
            None => {
                best = Some(c);
                tier.push(c);
            }
            Some(b) => match c.score.total_cmp(&b.score) {
                std::cmp::Ordering::Greater => {
                    best = Some(c);
                    tier.clear();
                    tier.push(c);
                }
                std::cmp::Ordering::Equal => tier.push(c),
                std::cmp::Ordering::Less => {}
            },
        }
    }
    tier
}

/// A caller-supplied action to run **inside the commit critical section**,
/// under the same write-lock hold as the committed mutations, with the final
/// outcome in hand (J3 durable intents: the write pipeline consumes the job's
/// intent here, so the applied mutations and the intent consumption travel in
/// one flush batch — one store transaction — and a crash can never leave the
/// write durable beside a still-unconsumed intent).
pub type CommitHook = Box<dyn FnOnce(&mut Graph, &DeriveOutcome) + Send>;

/// The per-concept resolution computed by the async gather phase.
enum Resolution {
    /// Canonical key already matched an existing concept — reuse it (byte-identical
    /// to derive's `Matched` path).
    CanonicalMatch { node: NodeId },
    /// `Unmatched` with no viable vector hit (capability absent, below
    /// threshold, or invalid candidate — an embed failure fails the call
    /// instead, J3-R3-1): create a fresh concept and no `Semantic` edge.
    ///
    /// `embedding` is `Some` exactly when a vector was actually computed for
    /// this concept and the store can serve vectors — the below-threshold arm
    /// (L82-4). It is `None` on every arm where no vector exists (or none can
    /// ever be queried): a vector is never invented, and the absent-capability
    /// path stays byte-identical to `MatchStrategy::Canonical`.
    ///
    /// `source` is `Some` only for the image item of an image derive (#22):
    /// its vector was supplied, not embedded from its content.
    Fresh {
        key: String,
        embedding: Option<Vec<f32>>,
        source: Option<EmbeddingSource>,
    },
    /// `Unmatched` with a vector hit at/above threshold: create the concept and
    /// a decaying `Semantic` edge to the matched concept. `targets` is the
    /// whole tier tied at the top score ([`top_tier`], usually length 1): the
    /// gather phase holds no graph, so the stable pick — smallest canonical
    /// key, then id (issue #2) — is made here at commit, where the graph is.
    HybridMerge {
        key: String,
        targets: Vec<NodeId>,
        score: f64,
        embedding: Vec<f32>,
    },
}

/// "log the fallback once per session" — module-level, keyed by session id,
/// because there is no session owner yet (T8.1) and the fallback is cross-call.
fn note_fallback_logged(session: &SessionId) -> bool {
    static LOGGED: LazyLock<Mutex<HashSet<SessionId>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    LOGGED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session.clone())
}

/// Build the context text hybrid embeds — the calibration rule: embed the
/// concept WITH its origin interaction text, never the bare label.
///
/// The "Concept: " framing guards the no-origin case so even a missing origin is
/// never a bare label; the calibration evidence that separates the classes comes
/// from the real interaction text, which the tests carry.
///
/// # The asymmetry this creates with recall (F-R1-5)
///
/// What lands in `concepts.embedding` is the vector of this **framed** string, but
/// `cli::recall` embeds the user's query **bare**. So a recall probe is always
/// compared against `"{content} — {origin}"`, never against `content` alone. It is a
/// real cost, not a bug: with a semantic embedder the framing shifts similarity
/// slightly and the ranking survives it (see `evidence/mooshik-f-sqlite-bge/`, where a
/// query sharing no vocabulary with the concept still recalls it). With a
/// *deterministic* embedder it is fatal to the match — `FixtureEmbedder` keys on the
/// exact normalized phrase, so the framed string and a bare query are near-orthogonal
/// by construction, which is why the SQLite acceptance test needs a test-only
/// `ContextTolerantEmbedder` that strips the framing before delegating. No such
/// wrapper exists (or should exist) in production; anything relying on it is testing
/// the wrapper, not the product.
pub(crate) fn context_text(content: &str, origin: Option<&str>) -> String {
    match origin.map(str::trim).filter(|s| !s.is_empty()) {
        Some(origin) => format!("{content} — {origin}"),
        None => format!("Concept: {content}"),
    }
}

/// The interaction prompt a derive opens: its concepts' contents joined by
/// `"; "`. It is the origin [`context_text`] frames every concept of the call
/// with, so each concept's embed carries the whole call's text, and the bytes a
/// `k`-concept derive embeds grow as about `(k + 1)` times its concept bytes.
///
/// One function for the derive paths and for the write queue's calibration
/// probe (#11), which times a representative write by embedding exactly what
/// a derive of that shape embeds.
pub(crate) fn derive_prompt<'a>(contents: impl IntoIterator<Item = &'a str>) -> String {
    contents.into_iter().collect::<Vec<_>>().join("; ")
}

/// The exact byte length of [`context_text`]`(content, origin)`, without
/// building it. The context cap is checked against this, so it must follow
/// `context_text`'s two arms: `" — "` is 5 bytes (the dash is 3), and
/// `"Concept: "` is 9.
fn context_len(content: &str, origin: Option<&str>) -> usize {
    match origin.map(str::trim).filter(|s| !s.is_empty()) {
        Some(origin) => content.len() + " — ".len() + origin.len(),
        None => "Concept: ".len() + content.len(),
    }
}

/// The longest concept a hybrid derive can embed when it is the call's only
/// concept (8,189 bytes): the call's text is then the concept itself, so the
/// concept is framed with itself (`"{concept} — {concept}"`) and that must
/// fit [`MAX_HYBRID_CONTEXT_BYTES`]. Every other concept in the call only
/// lengthens the call's text, so no concept of any call embeds past this.
pub const MAX_SINGLE_CONCEPT_CONTEXT_BYTES: usize = (MAX_HYBRID_CONTEXT_BYTES - " — ".len()) / 2;

/// Refuse, before a write is accepted, a hybrid derive whose embedding
/// context would exceed [`MAX_HYBRID_CONTEXT_BYTES`] at apply (#74).
///
/// The same arithmetic as the apply-time checks in `derive_planned`, which
/// stay as defence in depth: the call's text is [`derive_prompt`] over
/// `concepts` (the interaction prompt every `Memory` derive path opens), and
/// each embedded item's text is [`context_len`] of it framed with that text.
///
/// Matching is not known at acceptance, so this is conservative: it checks
/// every concept and every `parent_of` end, where apply embeds only the
/// unmatched concepts and the ends it will create. A call whose over-long
/// concept or end would have matched an existing concept is refused too;
/// splitting or shortening it always fits. `supplied` names an image item's
/// content, which is never embedded (its vector arrived with it) and is not
/// checked, as at apply.
///
/// Call it only when the derive will embed (`Hybrid` on a store with vector
/// search); nothing is embedded otherwise and nothing is refused at apply.
/// The message names the limit and which part is too long, never the text.
pub fn check_embed_context(
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    supplied: Option<&str>,
) -> Result<(), String> {
    let origin = derive_prompt(concepts.iter().map(|(content, _)| *content));
    if origin.len() > MAX_HYBRID_CONTEXT_BYTES {
        return Err(format!(
            "the call's text (its concepts joined by \"; \") is {} bytes; a hybrid derive \
             embeds every new concept with it, so it may be at most {MAX_HYBRID_CONTEXT_BYTES} \
             bytes: split the call",
            origin.len()
        ));
    }
    let embedded = |content: &str| supplied != Some(content);
    let framed = |part: String, len: usize| {
        format!(
            "{part} framed with the call's text for embedding (the text, \" — \", then the \
             call's concepts joined by \"; \") is {len} bytes; at most \
             {MAX_HYBRID_CONTEXT_BYTES} bytes fit, so a concept sent alone may be at most \
             {MAX_SINGLE_CONCEPT_CONTEXT_BYTES} bytes: shorten it or split the call"
        )
    };
    for (i, (content, _)) in concepts.iter().enumerate() {
        let len = context_len(content, Some(&origin));
        if embedded(content) && len > MAX_HYBRID_CONTEXT_BYTES {
            return Err(framed(format!("concepts[{i}]"), len));
        }
    }
    for (i, (parent, child)) in parent_of.pairs().iter().enumerate() {
        for (end, content) in [("parent", parent), ("child", child)] {
            let len = context_len(content, Some(&origin));
            if embedded(content) && len > MAX_HYBRID_CONTEXT_BYTES {
                return Err(framed(format!("parent_of[{i}].{end}"), len));
            }
        }
    }
    Ok(())
}

/// [`check_embed_context`] for the library's `Memory` calls: a no-op when
/// the derive will not embed, else its refusal as a `Config` error.
pub fn validate_embed_context(
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    supplied: Option<&str>,
    embeds: bool,
) -> Result<(), LamboError> {
    if !embeds {
        return Ok(());
    }
    check_embed_context(concepts, parent_of, supplied).map_err(LamboError::Config)
}

/// Build a fresh concept node for a hybrid-produced content (mirrors
/// `derive::resolve_concept`'s `Unmatched` branch, plus an optional embedding).
#[allow(clippy::too_many_arguments)]
fn new_concept(
    session_id: &SessionId,
    content: &str,
    concept_type: ConceptType,
    key: String,
    interaction: NodeId,
    agent: &AgentId,
    created_at: DateTime<Utc>,
    embedding: Option<Vec<f32>>,
    embedding_source: Option<EmbeddingSource>,
) -> Concept {
    Concept {
        id: NodeId::new(),
        session_id: session_id.clone(),
        content: content.to_string(),
        canonical_key: key,
        concept_type,
        origin_interaction: interaction,
        origin_agent: agent.clone(),
        created_at,
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding,
        human_confirmed: 0,
        embedding_source,
        chunk_group_id: None,
    }
}

/// Reflect the GRAPH-8 guard from `derive.rs` (rejected: empty/whitespace-only/
/// stopword-only content collapsing onto the empty key).
fn reject_empty_key(content: &str, key: &str) -> Result<(), LamboError> {
    if key.is_empty() {
        return Err(LamboError::Store(StoreError::Invariant(format!(
            "hybrid derive: content {:?} canonicalizes to an empty key (empty, whitespace-only, \
             or stopword-only content is rejected)",
            content
        ))));
    }
    Ok(())
}

/// The async twin of [`crate::graph::derive::derive`] for `MatchStrategy::Hybrid`.
///
/// Semantically identical to `derive` for everything the hybrid step does not
/// touch (canonical matches, co-occurrence, `ParentOf` hierarchies); only the
/// `Unmatched` branch is replaced by the embed→`vector_candidates` merge step.
/// When the store lacks `Capabilities::VECTOR_SEARCH`, the outcome is
/// byte-identical to `derive` (fresh keyword-only concepts, no store I/O beyond
/// the capability probe). When the **embedder** fails or times out, the call
/// fails and nothing is written (J3-R3-1): applying the concept with
/// `embedding: NULL` would report a success the semantic tier can never serve,
/// and — on the async path — hand the write queue a 3 ms non-embed to sample as
/// a fast write. **Which** failure it was is carried by the type, because the
/// durable-intent replay's consume-or-keep decision turns on it (J3 round-1 N1):
/// [`LamboError::EmbedUnavailable`] when the embedder could not be reached or
/// did not answer in time, [`LamboError::Embed`] when it answered and the answer
/// was unusable for this input. The two render identically to a caller.
///
/// `embedding` is the live, resolved [`EmbeddingContract`] for the process's
/// embedder (from `ResolvedBackends`); it is stamped on the graph at first embed
/// and checked via [`EmbeddingContract::ensure_compatible`] on later hybrid
/// writes — a mid-session kind/model/dim swap is refused without re-embedding.
#[allow(clippy::too_many_arguments)]
pub async fn derive(
    graph: Arc<RwLock<Graph>>,
    store: &dyn GraphStore,
    embedder: &dyn Embedder,
    embedding: &EmbeddingContract,
    interaction: NodeId,
    agent: &AgentId,
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    max_cooccurrence_per_derive: usize,
    semantic_match_threshold: f64,
    on_commit: Option<CommitHook>,
) -> Result<DeriveOutcome, LamboError> {
    derive_with(
        graph,
        VectorCandidates::from_store(store),
        embedder,
        embedding,
        interaction,
        agent,
        concepts,
        parent_of,
        max_cooccurrence_per_derive,
        semantic_match_threshold,
        None,
        on_commit,
    )
    .await
}

/// [`derive`](fn@derive), reaching vector candidates through the source the
/// caller was given (#27's caller-side seam) instead of a store. `Memory` and
/// the write queue call this; with `VectorCandidates::Store` and no
/// `supplied` vector it is exactly `derive`.
///
/// # A supplied vector (#22, an image derive)
///
/// With `supplied`, the concept whose content is `supplied.content` (the
/// image item) takes `supplied.vector` instead of an embed:
///
/// * It **never takes the semantic-merge leg** (design section 4.4): it is
///   either a canonical match (the same image id and caption) or a fresh
///   concept carrying the supplied vector and its `embedding_source`. No
///   `embed` and no candidate lookup is made for it.
/// * A canonical match on an image concept keeps it as it is, with one
///   exception: an image concept whose vector is missing (a quarantine or
///   `re-embed --drop-image-vectors` nulled it and kept its source) takes the
///   supplied vector, so re-deriving the same image repairs it.
/// * A canonical match on a **text** concept (a text write that produced the
///   same key first) is refused with [`LamboError::Embed`] under the commit
///   lock: the image would otherwise have no vector while the call reported
///   success.
/// * Every **text** item's merge tier excludes image-sourced candidates, in
///   every derive: a text claim is never absorbed into a picture.
/// * The vector is checked against the live contract first
///   ([`SuppliedVector::check`]); a refusal is [`LamboError::Embed`], so a
///   durable intent replayed under a changed live contract settles `failed`.
///   Under the commit lock the session stamp must equal the supplied
///   contract.
/// * The store must advertise `VECTOR_SEARCH`: an image concept no vector
///   leg can find is refused with [`LamboError::Config`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn derive_with(
    graph: Arc<RwLock<Graph>>,
    vectors: VectorCandidates<'_>,
    embedder: &dyn Embedder,
    embedding: &EmbeddingContract,
    interaction: NodeId,
    agent: &AgentId,
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    max_cooccurrence_per_derive: usize,
    semantic_match_threshold: f64,
    supplied: Option<&SuppliedVector>,
    on_commit: Option<CommitHook>,
) -> Result<DeriveOutcome, LamboError> {
    validate_limits(concepts, parent_of, semantic_match_threshold)?;
    if let Some(supplied) = supplied {
        check_supplied(supplied, embedding, concepts, vectors.available())?;
    }

    // Writers outside hybrid (daemon maintenance, future MCP tasks) need not
    // share a mutex with this function. Epoch validation makes their mutations
    // visible; a stale gather is discarded and planned again.
    let io_deadline = tokio::time::Instant::now() + HYBRID_IO_TIMEOUT;
    derive_planned(
        graph,
        vectors,
        embedder,
        embedding,
        interaction,
        agent,
        concepts,
        parent_of,
        max_cooccurrence_per_derive,
        semantic_match_threshold,
        io_deadline,
        supplied,
        on_commit,
    )
    .await
}

/// The apply-time checks on a supplied vector (see [`derive_with`]).
///
/// Order matters for replay: the contract and the vector are checked first,
/// as [`LamboError::Embed`], because a mismatch is a fact about this input
/// (a durable intent written under another live contract is never
/// applicable here, so it settles `failed` rather than blocking the replay).
/// The capability and shape checks after it are configuration or caller
/// errors.
fn check_supplied(
    supplied: &SuppliedVector,
    embedding: &EmbeddingContract,
    concepts: &[(&str, ConceptType)],
    vector_search: bool,
) -> Result<(), LamboError> {
    supplied
        .check(embedding)
        .map_err(|e| LamboError::Embed(format!("{e}; nothing was written")))?;
    if !vector_search {
        return Err(LamboError::Config(
            "an image derive needs a store with vector search (VECTOR_SEARCH): an image \
             concept is found only through its vector, so it is refused rather than written \
             where no vector leg can reach it"
                .into(),
        ));
    }
    match concepts {
        [(content, _)] if *content == supplied.content => Ok(()),
        _ => Err(LamboError::Store(StoreError::Invariant(
            "image derive: the call must carry exactly one concept, the one the supplied \
             vector belongs to"
                .into(),
        ))),
    }
}

/// What a canonically matched image item does to the concept it matched,
/// decided under the commit lock (see [`derive_with`]).
///
/// * No match (the image item is fresh): `None`.
/// * An image concept whose vector is missing: `Some`, the repair target.
/// * An image concept that already has a vector: `None`, the first vector
///   is kept.
/// * A **text** concept (no `embedding_source`): refused with
///   [`LamboError::ImageIdTaken`]. Any text write can produce the image's canonical
///   key (a derive, an action's resources, a `parent_of` end; case and token
///   order fold together), and keeping the text concept would leave the
///   image with no vector while the call reported success. A fact about
///   this input, like `Embed`, so a replayed intent settles `failed` rather
///   than blocking the replay.
fn supplied_match_repair<'s>(
    graph: &Graph,
    items: &[(&str, ConceptType, String, Option<NodeId>)],
    resolutions: &[Resolution],
    supplied: &'s SuppliedVector,
) -> Result<Option<(NodeId, &'s SuppliedVector)>, LamboError> {
    let Some(node) =
        items
            .iter()
            .zip(resolutions.iter())
            .find_map(|((content, ..), res)| match res {
                Resolution::CanonicalMatch { node } if *content == supplied.content => Some(*node),
                _ => None,
            })
    else {
        return Ok(None);
    };
    match graph.node(node) {
        Some(Node::Concept(c)) if c.embedding_source.is_none() => {
            // The node id is the operator's; the caller is told only its own
            // image id (`LamboError::ImageIdTaken`).
            tracing::info!(
                target: "lambo::image",
                text_concept = %node,
                "image derive refused: a text concept already holds the image's canonical key"
            );
            Err(LamboError::ImageIdTaken(
                crate::graph::image::image_id_of(&supplied.content)
                    .unwrap_or_default()
                    .to_owned(),
            ))
        }
        Some(Node::Concept(c)) if c.embedding.is_none() => Ok(Some((node, supplied))),
        _ => Ok(None),
    }
}

/// [`derive`](fn@derive)'s size and range checks, on their own so J3's
/// asynchronous ack path can run them on the call path.
///
/// These are the ones a caller can act on — too many concepts, too many parent
/// pairs, an over-long string, a threshold outside `[0, 1]` — and they need
/// neither the graph nor the embedder, so nothing is gained by deferring them
/// to a receipt.
pub fn validate_limits(
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    semantic_match_threshold: f64,
) -> Result<(), LamboError> {
    if !semantic_match_threshold.is_finite() || !(0.0..=1.0).contains(&semantic_match_threshold) {
        return Err(LamboError::Config(format!(
            "semantic_match_threshold must be finite and in [0, 1], got {semantic_match_threshold}"
        )));
    }
    if concepts.len() > MAX_HYBRID_CONCEPTS {
        return Err(LamboError::Config(format!(
            "hybrid derive accepts at most {MAX_HYBRID_CONCEPTS} concepts, got {}",
            concepts.len()
        )));
    }
    if parent_of.pairs().len() > MAX_HYBRID_PARENT_PAIRS
        || concepts.len().saturating_add(parent_of.pairs().len()) > MAX_HYBRID_WORK_ITEMS
    {
        return Err(LamboError::Config(format!(
            "hybrid derive accepts at most {MAX_HYBRID_PARENT_PAIRS} parent pairs and \
             {MAX_HYBRID_WORK_ITEMS} combined work items"
        )));
    }
    for text in concepts
        .iter()
        .map(|(content, _)| *content)
        .chain(parent_of.pairs().iter().flat_map(|(a, b)| [*a, *b]))
    {
        if text.len() > MAX_HYBRID_CONTEXT_BYTES {
            return Err(LamboError::Config(format!(
                "hybrid derive input exceeds {MAX_HYBRID_CONTEXT_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

/// [`derive`](fn@derive)'s graph-dependent pre-pass: `parent_of` reflexivity
/// and empty canonical keys, resolved against the graph.
///
/// **Hybrid's pre-pass is not the synchronous path's.** It deliberately omits
/// `graph::derive`'s repeated-`Observation` and single-`Hierarchical`-parent
/// rejections — hybrid matches semantically and resolves `Observation`s through
/// a different path — so J3's asynchronous ack has to run *this* one when the
/// session's strategy is `Hybrid`. Running the synchronous path's pre-pass here
/// instead would refuse writes hybrid has always accepted, which is what it did
/// on the first attempt: a concurrent re-derive of one content started failing
/// with a `store error` at ack time.
///
/// Read-only: [`canonicalize`] writes nothing.
pub fn validate_graph_inputs(graph: &Graph, parent_of: &ParentOf<'_>) -> Result<(), LamboError> {
    for &(parent, child) in parent_of.pairs() {
        if parent == child {
            return Err(LamboError::Store(StoreError::Invariant(format!(
                "hybrid derive: parent_of pair ({parent}, {child}) is reflexive — a \
                 Hierarchical self-loop is a cycle (spec §5.7)"
            ))));
        }
        let pk = match canonicalize(parent, graph)? {
            CanonicalizeResult::Matched { key, .. } | CanonicalizeResult::Unmatched { key } => key,
        };
        let ck = match canonicalize(child, graph)? {
            CanonicalizeResult::Matched { key, .. } | CanonicalizeResult::Unmatched { key } => key,
        };
        reject_empty_key(parent, &pk)?;
        reject_empty_key(child, &ck)?;
        if pk == ck {
            return Err(LamboError::Store(StoreError::Invariant(format!(
                "hybrid derive: parent_of pair ({parent}, {child}) resolves to the same \
                 canonical key ({pk:?}) — a Hierarchical self-loop is a cycle (spec §5.7)"
            ))));
        }
    }
    Ok(())
}

/// A call's unique concepts with their canonical key and match so far, in
/// call order.
type PlannedItems<'a> = Vec<(&'a str, ConceptType, String, Option<NodeId>)>;
/// The `parent_of` ends a call will create, with their canonical key.
type PlannedEnds<'a> = Vec<(&'a str, String)>;

/// Plan a call against `g`: its unique concepts, and the `parent_of` ends it
/// will create. Read-only; the caller holds the read lock.
fn plan_inputs<'a>(
    g: &Graph,
    concepts: &[(&'a str, ConceptType)],
    parent_of: &ParentOf<'a>,
) -> Result<(PlannedItems<'a>, PlannedEnds<'a>), LamboError> {
    // Step 2 — dedup by content + canonicalize. Items are the unique concepts
    // of this call with their resolution so far (canonical match id if any).
    let mut seen: HashSet<&str> = HashSet::with_capacity(concepts.len());
    let mut items: PlannedItems<'a> = Vec::with_capacity(concepts.len());
    for &(content, concept_type) in concepts {
        if !seen.insert(content) {
            continue;
        }
        let (key, matched) = match canonicalize(content, g)? {
            CanonicalizeResult::Matched { key, node } => (key, Some(node)),
            CanonicalizeResult::Unmatched { key } => (key, None),
        };
        reject_empty_key(content, &key)?;
        items.push((content, concept_type, key, matched));
    }

    // Issue #16 §2: the `parent_of` ends this call will CREATE — not matched
    // in the graph, and not one of this call's own `concepts` (those resolve
    // to the item at commit) — one per canonical key, first spelling wins, as
    // commit's re-canonicalization does. Before this they were created with
    // `embedding: None` and stayed invisible to recall's vector leg.
    let item_keys: HashSet<&str> = items.iter().map(|(_, _, k, _)| k.as_str()).collect();
    let mut end_keys: HashSet<String> = HashSet::new();
    let mut parent_ends: PlannedEnds<'a> = Vec::new();
    for &(parent, child) in parent_of.pairs() {
        for content in [parent, child] {
            if let CanonicalizeResult::Unmatched { key } = canonicalize(content, g)?
                && !item_keys.contains(key.as_str())
                && end_keys.insert(key.clone())
            {
                parent_ends.push((content, key));
            }
        }
    }
    Ok((items, parent_ends))
}

/// Refuse a plan whose embeds exceed [`MAX_HYBRID_EMBEDS`]: one per
/// unmatched concept and one per `parent_of` end the call will create.
fn check_embed_budget(
    items: &[(&str, ConceptType, String, Option<NodeId>)],
    parent_ends: &PlannedEnds<'_>,
) -> Result<(), LamboError> {
    let planned = items
        .iter()
        .filter(|(_, _, _, matched)| matched.is_none())
        .count()
        + parent_ends.len();
    if planned > MAX_HYBRID_EMBEDS {
        return Err(LamboError::Config(format!(
            "hybrid derive would embed {planned} new concepts and parent_of ends in one call; \
             at most {MAX_HYBRID_EMBEDS} fit the {HYBRID_IO_TIMEOUT:?} embed deadline, so split \
             the call"
        )));
    }
    Ok(())
}

/// [`derive`](fn@derive)'s embed budget, on its own so J3's asynchronous ack
/// path can refuse an over-budget call at call time instead of on a receipt.
///
/// Counts what the plan phase will embed against `graph` as it is now:
/// unmatched concepts plus the `parent_of` ends the call will create. When
/// the store has no vector search nothing is embedded, so nothing is
/// refused. The plan phase runs the same check again, against the graph as
/// it is at apply, and that check is the authoritative one.
///
/// The two counts can differ when the graph changes between ack and apply,
/// and both outcomes are safe. If an earlier queued write has not applied yet
/// and will create concepts this call's `parent_of` names, the ack counts
/// those ends as new and may refuse a call that would have fit; a retry after
/// that write applies succeeds. If a concept this call would match is removed
/// in between, the ack accepts and the plan refuses, which yields a `Failed`
/// receipt with nothing written. Nothing is ever embedded past the budget,
/// because the plan re-checks before any I/O.
pub fn validate_embed_budget(
    graph: &Graph,
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    vector_search: bool,
) -> Result<(), LamboError> {
    if !vector_search {
        return Ok(());
    }
    let (items, parent_ends) = plan_inputs(graph, concepts, parent_of)?;
    check_embed_budget(&items, &parent_ends)
}

/// [`derive`](fn@derive)'s plan/embed/write loop, after [`validate_limits`].
#[allow(clippy::too_many_arguments)]
async fn derive_planned(
    graph: Arc<RwLock<Graph>>,
    vectors: VectorCandidates<'_>,
    embedder: &dyn Embedder,
    embedding: &EmbeddingContract,
    interaction: NodeId,
    agent: &AgentId,
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    max_cooccurrence_per_derive: usize,
    semantic_match_threshold: f64,
    io_deadline: tokio::time::Instant,
    supplied: Option<&SuppliedVector>,
    on_commit: Option<CommitHook>,
) -> Result<DeriveOutcome, LamboError> {
    let is_supplied =
        |content: &str| supplied.is_some_and(|supplied| supplied.content.as_str() == content);
    // Survives replans: the hook fires exactly once, at the commit that wins.
    let mut on_commit = on_commit;
    for _attempt in 0..MAX_HYBRID_REPLANS {
        // The vector leg can run only when the store advertises it. Probed once,
        // synchronously, before any I/O (mirrors the recall RAM-tier promise:
        // zero async store calls when the capability is absent).
        let vector_ok = vectors.available();

        // -----------------------------------------------------------------------
        // Phase 1 — plan under a brief read lock (no I/O, no await).
        // -----------------------------------------------------------------------
        let (
            planned_epoch,
            session_id,
            interaction_created_at,
            interaction_event_time,
            origin_text,
            stamped,
            items,
            parent_ends,
            image_nodes,
        ) = {
            let g = graph.read();
            let planned_epoch = g.epoch();
            let session_id = g.session_id().clone();
            let (interaction_created_at, interaction_event_time, origin_text) = match g
                .node(interaction)
            {
                Some(Node::Interaction(i)) => (i.created_at, i.event_time, i.prompt_text.clone()),
                Some(_) => {
                    return Err(LamboError::Store(StoreError::NotFound(format!(
                        "hybrid derive: node {interaction} exists but is not an Interaction"
                    ))))
                }
                None => {
                    return Err(LamboError::Store(StoreError::NotFound(format!(
                        "hybrid derive: interaction node {interaction} not found in graph"
                    ))))
                }
            };
            let stamped = g.embedding().cloned();

            validate_graph_inputs(&g, parent_of)?;

            let (items, parent_ends) = plan_inputs(&g, concepts, parent_of)?;
            // Bound the embed work before any I/O (see MAX_HYBRID_EMBEDS).
            if vector_ok {
                check_embed_budget(&items, &parent_ends)?;
            }

            // The context caps bound what an embedder is sent, so, like the
            // embed budget above, they apply only when this call will embed:
            // on a store without vector search nothing is embedded.
            if vector_ok
                && origin_text
                    .as_ref()
                    .is_some_and(|origin| origin.len() > MAX_HYBRID_CONTEXT_BYTES)
            {
                return Err(LamboError::Config(format!(
                    "hybrid interaction context exceeds {MAX_HYBRID_CONTEXT_BYTES} bytes"
                )));
            }
            // A supplied (image) item's text is never embedded: its vector
            // arrived with it. Its content may fill the whole per-string cap
            // (a caption plus its suffix), so framing it would refuse an
            // image derive the surface accepted (#22 PR 4 review M2).
            let unmatched_contents = items
                .iter()
                .filter(|(content, _, _, matched)| matched.is_none() && !is_supplied(content))
                .map(|(content, _, _, _)| *content)
                .chain(parent_ends.iter().map(|(content, _)| *content));
            if vector_ok
                && unmatched_contents.into_iter().any(|content| {
                    context_len(content, origin_text.as_deref()) > MAX_HYBRID_CONTEXT_BYTES
                })
            {
                return Err(LamboError::Config(format!(
                    "hybrid embedding context exceeds {MAX_HYBRID_CONTEXT_BYTES} bytes"
                )));
            }

            // #22: the image-sourced concepts a text item may not merge into
            // (design section 4.4), read under this lock. A concept written
            // after this read moves the epoch, so the commit replans.
            let text_will_rank = vector_ok
                && items
                    .iter()
                    .any(|(content, _, _, matched)| matched.is_none() && !is_supplied(content));
            let image_nodes: HashSet<NodeId> = if text_will_rank {
                g.concepts()
                    .filter(|c| c.embedding_source.is_some())
                    .map(|c| c.id)
                    .collect()
            } else {
                HashSet::new()
            };

            (
                planned_epoch,
                session_id,
                interaction_created_at,
                interaction_event_time,
                origin_text,
                stamped,
                items,
                parent_ends,
                image_nodes,
            )
        };

        // Mid-session contract check — refuse a kind/model/dim swap BEFORE any embed
        // ("without re-embed"). Only enforced when we are actually about to embed
        // (capability present and at least one unmatched concept). ensure_compatible
        // is a pure comparison; it never embeds.
        let has_unmatched =
            items.iter().any(|(_, _, _, matched)| matched.is_none()) || !parent_ends.is_empty();
        if vector_ok
            && has_unmatched
            && let Some(existing) = &stamped
        {
            existing.ensure_compatible(embedding)?;
        }

        // -----------------------------------------------------------------------
        // Phase 2 — async gather (no lock held).
        // -----------------------------------------------------------------------
        // First-session fallback log for the capability-absent path (zero I/O, so
        // acceptable here); embed-failure logging happens per concept below.
        let mut attempted_embed = false;
        // Set when the store refuses `vector_candidates_checked` with a
        // capability miss after advertising the capability: the session then
        // writes keyword-only for this call, `parent_of` ends included.
        let mut store_refused_vectors = false;
        // Whether this attempt has asked the store, through the checked
        // lookup, whether it will serve vectors at all. Only an unmatched
        // concept asks on its own; the `parent_of` ends below ask when no
        // concept did.
        let mut probed_store = false;
        let mut resolutions: Vec<Resolution> = Vec::with_capacity(items.len());
        for (content, _concept_type, key, matched) in &items {
            let res = match matched {
                Some(node) => Resolution::CanonicalMatch { node: *node },
                // #22: the image item takes its supplied vector. No embed, no
                // candidate lookup, no merge (design section 4.4).
                None if let Some(supplied) = supplied.filter(|s| s.content == *content) => {
                    attempted_embed = true;
                    Resolution::Fresh {
                        key: key.clone(),
                        embedding: Some(supplied.vector.clone()),
                        source: Some(supplied.source.clone()),
                    }
                }
                None if !vector_ok => {
                    if note_fallback_logged(&session_id) {
                        tracing::warn!(
                            target: "lambo::hybrid",
                            session = %session_id,
                            "hybrid matching disabled: store lacks VECTOR_SEARCH — degrading to \
                             MatchStrategy::Canonical (creating keyword-only concept)"
                        );
                    }
                    Resolution::Fresh {
                        key: key.clone(),
                        embedding: None,
                        source: None,
                    }
                }
                None => {
                    let context = context_text(content, origin_text.as_deref());
                    // An embed failure or timeout FAILS the write — see
                    // `embed_or_refuse`. Nothing has been written at this
                    // point, so "nothing was written" is exact.
                    let emb = embed_or_refuse(embedder, &context, io_deadline).await?;
                    // An embed only counts as "attempted" for the contract
                    // stamp once it actually returned a vector — a failed
                    // attempt must not bind the session to an embedding
                    // space it produced no vector in (MINOR-2).
                    attempted_embed = true;
                    probed_store = true;
                    match checked_candidates(
                        vectors,
                        &session_id,
                        &emb,
                        embedding,
                        VECTOR_CANDIDATE_LIMIT,
                        io_deadline,
                    )
                    .await?
                    {
                        Some(mut hits) => {
                            // #22: a text item never merges into an image
                            // concept (design section 4.4).
                            hits.retain(|hit| !image_nodes.contains(&hit.item));
                            // The tier tied at the highest score
                            // at/above threshold (store results are not
                            // guaranteed sorted). Every member is
                            // validated as a real distinct concept at
                            // commit, which also makes the stable
                            // canonical-key pick.
                            let tier = top_tier(&hits, semantic_match_threshold);
                            if tier.is_empty() {
                                // Below threshold: fresh concept, NO
                                // `Semantic` edge — the merge is refused.
                                // L82-4 (product decision 2026-08-14):
                                // the vector it just computed IS
                                // persisted, so organically-derived data
                                // becomes vector-recallable; before this,
                                // every organic concept stored NULL and
                                // recall's vector leg was dead on real
                                // data (0 of 13 live). The precision bias
                                // is preserved by the refusal itself: no
                                // `Semantic` edge means this concept is
                                // never pulled into another concept's
                                // recall neighbourhood, and the merge bar
                                // is still `>= semantic_match_threshold`.
                                // See the module doc, "Vector persistence
                                // for fresh concepts".
                                Resolution::Fresh {
                                    key: key.clone(),
                                    embedding: Some(emb),
                                    source: None,
                                }
                            } else {
                                Resolution::HybridMerge {
                                    key: key.clone(),
                                    targets: tier.iter().map(|c| c.item).collect(),
                                    score: tier[0].score,
                                    embedding: emb,
                                }
                            }
                        }
                        None => {
                            store_refused_vectors = true;
                            note_store_refused_vectors(&session_id);
                            // A vector exists here, but the store just
                            // refused to query vectors at all — so it can
                            // serve neither a merge nor recall's vector
                            // leg from it. Persisting an unqueryable
                            // vector buys nothing and would break the
                            // "capability miss == MatchStrategy::Canonical"
                            // promise, so this arm stays `None` (L82-4
                            // changes only the below-threshold arm).
                            Resolution::Fresh {
                                key: key.clone(),
                                embedding: None,
                                source: None,
                            }
                        }
                    }
                }
            };
            resolutions.push(res);
        }

        // Issue #16 §2: embed the `parent_of` ends this call will create, under
        // the same rules as the call's own concepts — same origin-framed
        // context, same deadline, same refusal on an embedder failure or
        // timeout (J3-R3-1), and nothing at all when the store cannot serve
        // vectors. They are embedded, not merged: a `parent_of` end is a
        // structural label the caller named, and sending it through the merge
        // leg would be a new matching behaviour, not this fix. Keyed by
        // canonical key, which is what `resolve_concept` sees at commit.
        let mut parent_vectors: HashMap<String, Vec<f32>> =
            HashMap::with_capacity(parent_ends.len());
        if vector_ok && !store_refused_vectors {
            for (content, key) in &parent_ends {
                let context = context_text(content, origin_text.as_deref());
                let emb = embed_or_refuse(embedder, &context, io_deadline).await?;
                attempted_embed = true;
                // A call whose concepts all matched (or that has none) has
                // not asked the store whether it serves vectors, so the
                // capability-miss rule above would depend on what else is
                // in the call. Ask once with the first end's vector (one
                // candidate, result discarded): a refusal leaves every end
                // keyword-only, exactly as it leaves a concept. A source that
                // cannot refuse (the holder's graph) is not asked: the answer
                // is known, and asking would scan the whole graph (#60 L3).
                if !probed_store {
                    probed_store = true;
                    if vectors.can_refuse()
                        && checked_candidates(vectors, &session_id, &emb, embedding, 1, io_deadline)
                            .await?
                            .is_none()
                    {
                        // `parent_vectors` is still empty: this is the first end.
                        note_store_refused_vectors(&session_id);
                        break;
                    }
                }
                parent_vectors.insert(key.clone(), emb);
            }
        }

        // -----------------------------------------------------------------------
        // Phase 3 — commit under a write lock (sync, no await).
        // -----------------------------------------------------------------------
        let mut guard = graph.write();
        if guard.epoch() != planned_epoch {
            continue;
        }
        // #22: a canonically matched image item repairs an image concept whose
        // vector is missing (see `derive_with`); that writes a vector too.
        let repair = match supplied {
            Some(supplied) => supplied_match_repair(&guard, &items, &resolutions, supplied)?,
            None => None,
        };
        let attempted_embed = attempted_embed || repair.is_some();
        if attempted_embed && let Some(existing) = guard.embedding() {
            // Revalidate under the commit lock. Two first writers can both plan
            // against `None`; only the winner may stamp its vector space.
            existing.ensure_compatible(embedding)?;
            // #22 design section 3.3 (2): the session's space must be the one
            // the supplied vector was declared in, not only the live one.
            //
            // Defence in depth, not a reachable refusal today:
            // `check_supplied` already requires `supplied.contract ==
            // embedding`, so this cannot fail where the line above passed,
            // and a rival first writer moves the epoch, so the replan's
            // plan-time check refuses it before this lock is taken. It stays
            // so that the commit itself states the invariant it writes
            // under, independent of how the call path checked the vector.
            if let Some(supplied) = supplied {
                existing.ensure_compatible(&supplied.contract)?;
            }
        }

        // Stage every graph mutation on a private clone. Any invariant failure in
        // concept, co-occurrence, or hierarchy construction drops the clone and
        // leaves both live state and its ordered mutation log untouched.
        let mut g = guard.clone();
        if attempted_embed && g.embedding().is_none() {
            g.stamp_embedding(embedding.clone())?;
        }
        let session_id = g.session_id().clone();

        let mut outcome = DeriveOutcome::default();
        let mut written: HashSet<NodeId> = HashSet::new();
        let mut call_nodes: Vec<NodeId> = Vec::with_capacity(items.len());
        // C4: the concepts THIS call wrote, by canonical key, so step 6's
        // `parent_of` resolution can find them.
        //
        // `canonicalize` deliberately never matches an `Observation`
        // (GRAPH-1: agent-declared content must not attach to a *demoted*
        // context-overflow record, spec section 7 demote semantics). That rule is
        // about Observations the graph already held; it was never meant to
        // hide a concept the same derive just declared. Without this index a
        // call carrying `concepts: [(X, Observation)]` and
        // `parent_of: [(doc, X)]` resolves the pair's child to Unmatched and
        // creates X a SECOND time as a bare `Entity` -- unembedded, untyped,
        // and splitting X's supporting interactions across two nodes. That is
        // exactly what held Mooshik's bootstrap graph at ~50% embedding
        // coverage across 170 contents.
        //
        // Keyed by canonical key rather than raw content so two spellings that
        // normalize together ("UserSchema" / "user schema") resolve to one
        // node, matching what the non-Observation path already does.
        let mut call_by_key: HashMap<String, NodeId> = HashMap::with_capacity(items.len());

        for ((content, concept_type, _key, _matched), res) in items.iter().zip(resolutions.iter()) {
            // MAJOR-1 (P7 remediation): re-canonicalize `content` against the graph
            // AS WRITTEN THIS CALL. Phase-1 canonicalization ran under the read
            // lock before ANY node was written, so two distinct contents that
            // collide on one canonical key (e.g. "user schema" + "schema user" ->
            // "schema user") both resolved `Unmatched`. The first created its node
            // above; the second must now collapse to it — mirroring sync derive's
            // `resolve_concept` (canonicalize -> insert -> written_this_call dedup)
            // — instead of re-inserting the same key and tripping
            // `insert_concept`'s UNIQUE (session_id, canonical_key) invariant
            // (hard error + partial write). Epoch validation means any external
            // writer would have forced a re-plan, so the only newly Matched node
            // here is one written earlier in THIS staged call.
            if let CanonicalizeResult::Matched { node, .. } = canonicalize(content, &g)?
                && written.contains(&node)
            {
                outcome.matched.push(node);
                if !call_nodes.contains(&node) {
                    call_nodes.push(node);
                }
                call_by_key.entry(_key.clone()).or_insert(node);
                continue;
            }
            let this_node: NodeId;
            match res {
                Resolution::CanonicalMatch { node } => {
                    if written.contains(node) {
                        // Key collision with a node written earlier this call — skip
                        // the write (one call never self-reinforces), record the match.
                        outcome.matched.push(*node);
                        this_node = *node;
                    } else {
                        let existing = match g.node(*node) {
                            Some(Node::Concept(c)) => c.clone(),
                            _ => {
                                return Err(LamboError::Store(StoreError::Invariant(format!(
                                    "hybrid derive: canonicalize matched {node} but the stored \
                                 node is not a Concept"
                                ))))
                            }
                        };
                        if g.edge_between(interaction, *node, EdgeType::Derives)
                            .is_some()
                        {
                            outcome.reinforced += 1;
                        }
                        let mut existing = existing;
                        if let Some((target, supplied)) = repair
                            && target == *node
                        {
                            existing.embedding = Some(supplied.vector.clone());
                            existing.embedding_source = Some(supplied.source.clone());
                            outcome.embedded += 1;
                        }
                        g.insert_concept(existing, interaction)?;
                        written.insert(*node);
                        outcome.matched.push(*node);
                        this_node = *node;
                    }
                }
                Resolution::Fresh {
                    key,
                    embedding,
                    source,
                } => {
                    let concept = new_concept(
                        &session_id,
                        content,
                        *concept_type,
                        key.clone(),
                        interaction,
                        agent,
                        interaction_created_at,
                        embedding.clone(),
                        source.clone(),
                    );
                    let id = concept.id;
                    g.insert_concept(concept, interaction)?;
                    written.insert(id);
                    outcome.created.push(id);
                    if embedding.is_some() {
                        // The below-threshold arm (L82-4): the row carries its
                        // vector. Counted so the receipt can say *embedded*,
                        // not just *applied* (J3-R3-1).
                        outcome.embedded += 1;
                    }
                    this_node = id;
                }
                Resolution::HybridMerge {
                    key,
                    targets,
                    score,
                    embedding,
                } => {
                    // MINOR-2 (P7 remediation): the concept keeps its vector ONLY if
                    // the merge Semantic edge is actually written. Both endpoints
                    // must be concepts (GRAPH-2); validate EVERY carried candidate
                    // up front and, when the store handed us a bogus non-Concept
                    // candidate, refuse the merge and degrade to a TRUE keyword-only
                    // concept (embedding: None) — regardless of which tier member a
                    // run-minted tie-break would have picked, an untrustworthy
                    // vector view for this call means no new row in it. L82-4 did
                    // NOT relax this arm. (The below-threshold arm, where the
                    // store behaved correctly and simply had no near neighbour, does
                    // persist its vector — see the module doc.)
                    let can_merge = targets
                        .iter()
                        .all(|t| matches!(g.node(*t), Some(Node::Concept(_))));
                    let embedding = if can_merge {
                        Some(embedding.clone())
                    } else {
                        None
                    };
                    let concept = new_concept(
                        &session_id,
                        content,
                        *concept_type,
                        key.clone(),
                        interaction,
                        agent,
                        interaction_created_at,
                        embedding,
                        None,
                    );
                    let id = concept.id;
                    g.insert_concept(concept, interaction)?;
                    written.insert(id);
                    outcome.created.push(id);
                    if can_merge {
                        // The merge arm persists its vector (`embedding` above
                        // is `Some` exactly when `can_merge`); count it for the
                        // receipt's applied-vs-embedded distinction (J3-R3-1).
                        outcome.embedded += 1;
                    }
                    if can_merge {
                        // The stable merge-target pick (issue #2, remediation
                        // round 1): the tier's members are tied at one score, so
                        // the run-minted UUID must not decide; smallest canonical
                        // key wins, smallest id behind it (`tie_break_by_key`).
                        // `can_merge` proved every target is a Concept, so the
                        // key arm is always `Some` here — the `None` arm keeps
                        // the closure total, it never fires.
                        let key_of = |t: NodeId| match g.node(t) {
                            Some(Node::Concept(c)) => Some(c.canonical_key.as_str()),
                            _ => None,
                        };
                        let target = targets
                            .iter()
                            .copied()
                            .min_by(|a, b| tie_break_by_key(key_of(*a), a, key_of(*b), b))
                            .expect("can_merge proved the validated tier is non-empty");
                        // Decaying Semantic edge to the matched concept (deterministic
                        // direction: order endpoints by NodeId's inner UUID, since
                        // NodeId itself is not Ord). `target != id` is defense-in-depth:
                        // a store-returned target can never equal this fresh id.
                        if target != id {
                            let (s, t) = if target.0 < id.0 {
                                (target, id)
                            } else {
                                (id, target)
                            };
                            g.upsert_edge(Edge {
                                event_time: interaction_event_time,
                                id: NodeId::new(),
                                session_id: session_id.clone(),
                                source: s,
                                target: t,
                                edge_type: EdgeType::Semantic,
                                weight: semantic_weight(*score),
                                reinforcements: 1,
                                created_at: interaction_created_at,
                                last_reinforced: interaction_created_at,
                            })?;
                            // Recorded separately from `matched`: a merge does not
                            // re-upsert the target nor Derives-reinforce it, so the
                            // outcome must not over-count `matched` as "re-derived"
                            // (DeriveOutcome contract, MINOR-3).
                            outcome.semantic_merged.push(target);
                        }
                    }
                    this_node = id;
                }
            }
            if !call_nodes.contains(&this_node) {
                call_nodes.push(this_node);
            }
            // First writer of a key wins, mirroring `call_nodes`' dedup: a
            // later item that collapsed onto it must not repoint the index.
            call_by_key.entry(_key.clone()).or_insert(this_node);
        }

        // Step 5 — pairwise CoOccurrence (mirror derive: earlier-in-call -> later,
        // reinforce an existing edge, cap at max_cooccurrence_per_derive).
        let mut written_co = 0usize;
        'pairs: for i in 0..call_nodes.len() {
            for j in (i + 1)..call_nodes.len() {
                if written_co >= max_cooccurrence_per_derive {
                    break 'pairs;
                }
                let (source, target) = pair_direction(&g, call_nodes[i], call_nodes[j]);
                if g.edge_between(source, target, EdgeType::CoOccurrence)
                    .is_some()
                {
                    outcome.reinforced += 1;
                }
                g.upsert_edge(Edge {
                    event_time: interaction_event_time,
                    id: NodeId::new(),
                    session_id: session_id.clone(),
                    source,
                    target,
                    edge_type: EdgeType::CoOccurrence,
                    weight: COOCCURRENCE_WEIGHT,
                    reinforcements: 1,
                    created_at: interaction_created_at,
                    last_reinforced: interaction_created_at,
                })?;
                written_co += 1;
            }
        }

        // Step 6 — Hierarchical edges from ParentOf (mirror derive: reflexivity
        // already rejected in phase 1; dedup on resolved pair).
        let mut seen_pairs: HashSet<(NodeId, NodeId)> =
            HashSet::with_capacity(parent_of.pairs().len());
        for &(parent, child) in parent_of.pairs() {
            if parent == child {
                return Err(LamboError::Store(StoreError::Invariant(format!(
                    "hybrid derive: parent_of pair ({parent}, {child}) is reflexive — a \
                 Hierarchical self-loop is a cycle (spec §5.7)"
                ))));
            }
            let parent_node = self::resolve_concept(
                &mut g,
                parent,
                PARENT_OF_CONCEPT_TYPE,
                interaction,
                agent,
                interaction_created_at,
                &session_id,
                &mut written,
                &mut outcome,
                &call_by_key,
                &parent_vectors,
            )?;
            let child_node = self::resolve_concept(
                &mut g,
                child,
                PARENT_OF_CONCEPT_TYPE,
                interaction,
                agent,
                interaction_created_at,
                &session_id,
                &mut written,
                &mut outcome,
                &call_by_key,
                &parent_vectors,
            )?;
            if parent_node == child_node {
                return Err(LamboError::Store(StoreError::Invariant(format!(
                "hybrid derive: parent_of pair ({parent}, {child}) resolves to the same concept \
                 {parent_node} — a Hierarchical self-loop is a cycle (spec §5.7)"
            ))));
            }
            if !seen_pairs.insert((parent_node, child_node)) {
                continue;
            }
            if g.edge_between(parent_node, child_node, EdgeType::Hierarchical)
                .is_some()
            {
                outcome.reinforced += 1;
            }
            g.upsert_edge(Edge {
                event_time: interaction_event_time,
                id: NodeId::new(),
                session_id: session_id.clone(),
                source: parent_node,
                target: child_node,
                edge_type: EdgeType::Hierarchical,
                weight: HIERARCHICAL_WEIGHT,
                reinforcements: 1,
                created_at: interaction_created_at,
                last_reinforced: interaction_created_at,
            })?;
        }

        *guard = g;
        // J3: run the commit hook (intent consumption) under THIS write-lock
        // hold, after the swap — the flush drain takes the same lock, so the
        // committed mutations and whatever the hook appends travel in one
        // batch, i.e. one store transaction. See `CommitHook`.
        if let Some(hook) = on_commit.take() {
            hook(&mut guard, &outcome);
        }
        return Ok(outcome);
    }

    Err(LamboError::Store(StoreError::Backend(format!(
        "hybrid derive could not commit after {MAX_HYBRID_REPLANS} concurrent graph changes"
    ))))
}

/// CoOccurrence is symmetric; adopt the existing direction so a swapped re-derive
/// reinforces rather than inserting a reverse duplicate (mirror derive.rs).
fn pair_direction(graph: &Graph, a: NodeId, b: NodeId) -> (NodeId, NodeId) {
    if graph.edge_between(a, b, EdgeType::CoOccurrence).is_some() {
        (a, b)
    } else if graph.edge_between(b, a, EdgeType::CoOccurrence).is_some() {
        (b, a)
    } else {
        (a, b)
    }
}

/// One `vector_candidates_checked` call under the call's I/O deadline.
///
/// `Ok(None)` is the store's capability refusal: it advertised
/// `VECTOR_SEARCH` but will not serve the checked lookup, and the caller
/// degrades to keyword-only. A timeout or any other store error fails the
/// write.
async fn checked_candidates(
    vectors: VectorCandidates<'_>,
    session_id: &SessionId,
    emb: &[f32],
    embedding: &EmbeddingContract,
    limit: usize,
    io_deadline: tokio::time::Instant,
) -> Result<Option<Vec<crate::types::Scored<NodeId>>>, LamboError> {
    match tokio::time::timeout_at(
        io_deadline,
        vectors.checked(session_id, emb, embedding, limit),
    )
    .await
    {
        Err(_) => Err(StoreError::Backend(format!(
            "hybrid vector candidate lookup timed out after {HYBRID_IO_TIMEOUT:?}"
        ))
        .into()),
        Ok(Ok(hits)) => Ok(Some(hits)),
        Ok(Err(StoreError::Capability(_))) => Ok(None),
        Ok(Err(e)) => Err(e.into()),
    }
}

/// Log, once per session, that the store refused the checked vector lookup.
fn note_store_refused_vectors(session_id: &SessionId) {
    if note_fallback_logged(session_id) {
        tracing::warn!(
            target: "lambo::hybrid",
            session = %session_id,
            "store refused vector_candidates (capability miss) — degrading to \
             MatchStrategy::Canonical (creating keyword-only concept)"
        );
    }
}

/// Embed `context`, or refuse the whole write.
///
/// An embed failure or timeout FAILS the write — it does not degrade it
/// (J3-R3-1). The old arms applied the concept with `embedding: NULL` and
/// returned `Ok`: an unqualified success over a concept semantic recall can
/// never find (the mechanism behind the dogfood store's 92/100 unembedded
/// concepts), and on the async path a ~3 ms non-embed handed to the write
/// queue's observed rate as evidence of a fast deployment (326/361 acked writes
/// abandoned at a clean close, round 3). Every caller runs in the gather phase,
/// before the commit, so "nothing was written" is exact. The capability-absent
/// arm is not here: that is a declared, session-uniform configuration, not a
/// per-input surprise.
///
/// J3 round-1 N1: a timeout is `EmbedUnavailable`, not `Embed`. We never got an
/// answer, so nothing was learned about this *input* — and the durable-intent
/// replay's consume/keep decision turns on exactly that difference.
async fn embed_or_refuse(
    embedder: &dyn Embedder,
    context: &str,
    io_deadline: tokio::time::Instant,
) -> Result<Vec<f32>, LamboError> {
    match tokio::time::timeout_at(io_deadline, embedder.embed(context)).await {
        Err(_) => Err(LamboError::EmbedUnavailable(format!(
            "hybrid embed timed out after {HYBRID_IO_TIMEOUT:?}; nothing was written — the write \
             is refused rather than applied without its vector (a concept stored with no \
             embedding is unfindable by semantic recall)"
        ))),
        Ok(Err(e)) if e.is_transient() => Err(LamboError::EmbedUnavailable(format!(
            "the embedder could not be reached ({e}); nothing was written — the write is \
             refused rather than applied without its vector (a concept stored with no \
             embedding is unfindable by semantic recall)"
        ))),
        Ok(Err(e)) => Err(LamboError::Embed(format!(
            "the embedder refused this content ({e}); nothing was written — the write is \
             refused rather than applied without its vector (a concept stored with no \
             embedding is unfindable by semantic recall)"
        ))),
        Ok(Ok(emb)) => Ok(emb),
    }
}

/// Mirror `derive::resolve_concept`'s canonical path for `ParentOf` contents
/// (these never go through the merge leg here: `ParentOf` creates/reuses
/// concepts with the generic `Entity` type and a Hierarchical edge, exactly as
/// sync derive's step 6 does).
///
/// A content this call creates takes its vector from `parent_vectors`, keyed
/// by canonical key and filled by the gather phase (issue #16 §2: before that,
/// every such end was written with `embedding: None` and counted as created
/// but never as embedded). A content that resolves to an existing concept is
/// left as it is — its vector belongs to the write that created it.
#[allow(clippy::too_many_arguments)]
fn resolve_concept(
    graph: &mut Graph,
    content: &str,
    concept_type: ConceptType,
    interaction: NodeId,
    agent: &AgentId,
    created_at: DateTime<Utc>,
    session_id: &SessionId,
    written: &mut HashSet<NodeId>,
    outcome: &mut DeriveOutcome,
    call_by_key: &HashMap<String, NodeId>,
    parent_vectors: &HashMap<String, Vec<f32>>,
) -> Result<NodeId, LamboError> {
    match canonicalize(content, graph)? {
        CanonicalizeResult::Unmatched { key } => {
            // C4: `canonicalize` returns Unmatched for a key whose only holder
            // is an `Observation` (GRAPH-1). When that holder is a concept
            // THIS call just declared, creating a second node for the same
            // content is a duplicate, not a fresh end -- so consult the
            // call's own index before creating. Scoped to `call_by_key`, so a
            // pre-existing *demoted* Observation from an earlier interaction
            // is still never matched and GRAPH-1 stands.
            if let Some(&node) = call_by_key.get(&key)
                && written.contains(&node)
            {
                outcome.matched.push(node);
                return Ok(node);
            }
            let embedding = parent_vectors.get(&key).cloned();
            let embedded = embedding.is_some();
            let concept = new_concept(
                session_id,
                content,
                concept_type,
                key,
                interaction,
                agent,
                created_at,
                embedding,
                None,
            );
            let id = concept.id;
            graph.insert_concept(concept, interaction)?;
            written.insert(id);
            outcome.created.push(id);
            if embedded {
                // Applied ≠ embedded (J3-R3-1): the receipt's "(M embedded)"
                // must count the ends too, or it under-reports exactly as the
                // "2 created (1 embedded)" reproduction did.
                outcome.embedded += 1;
            }
            Ok(id)
        }
        CanonicalizeResult::Matched { node, .. } => {
            if written.contains(&node) {
                outcome.matched.push(node);
                return Ok(node);
            }
            let existing = match graph.node(node) {
                Some(Node::Concept(c)) => c.clone(),
                _ => {
                    return Err(LamboError::Store(StoreError::Invariant(format!(
                        "hybrid derive: canonicalize matched {node} but the stored node is not \
                         a Concept"
                    ))))
                }
            };
            if graph
                .edge_between(interaction, node, EdgeType::Derives)
                .is_some()
            {
                outcome.reinforced += 1;
            }
            graph.insert_concept(existing, interaction)?;
            written.insert(node);
            outcome.matched.push(node);
            Ok(node)
        }
    }
}

#[cfg(all(test, feature = "embed-fixture"))]
mod tests;
