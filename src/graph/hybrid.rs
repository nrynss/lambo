//! Hybrid canonicalization step 6 (T7.2) — spec §7.1 step 6, §5, §3.2.
//!
//! On a canonical-key miss ([`CanonicalizeResult::Unmatched`]) under
//! `MatchStrategy::Hybrid`, this module embeds the concept **with context**
//! (name + origin interaction text — the live BGE-M3 calibration rule; see
//! `dev-diary/PHASE-7-embeddings.md`), queries [`GraphStore::vector_candidates`],
//! and — when a candidate sits at or above `semantic_match_threshold` — records
//! the merge as a decaying [`EdgeType::Semantic`] edge to the matched concept.
//! Below the threshold, or when the capability / embedder is unavailable, it
//! degrades to the byte-identical `MatchStrategy::Canonical` outcome (a fresh,
//! keyword-only concept), logging the fallback once per session.
//!
//! # The seam (design decision of record)
//!
//! The sync twin [`crate::graph::derive::derive`] takes `&mut Graph` and cannot
//! host the async hybrid step: embedding + `vector_candidates` are I/O, and the
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
//!    context, `embedder.embed`, `store.vector_candidates`. The store call goes
//!    through the [`GraphStore`] trait only. A capability-miss or embed failure
//!    marks the concept for the canonical fallback (logged once per session); a
//!    genuine backend `StoreError` (not a `Capability` miss) propagates.
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
//! `derive` contract (PHASE-7 T7.2 remediation, MINOR-3). A concept degraded to
//! keyword-only (below-threshold, capability-miss, embed-failure, or an invalid
//! non-Concept merge target — see the commit-time validation) is written with
//! `embedding: None`: the precision bias must never persist a vector for a
//! concept it refused to merge (MAJOR-1 / MINOR-2); likewise a failed embed
//! never stamps the session's embedding contract (MINOR-2). At commit, each
//! content is re-canonicalized against the graph as written this call so that
//! distinct contents collapsing onto one canonical key resolve Matched to the
//! just-created node (mirroring sync `derive`'s within-call dedup) instead of
//! erroring on `insert_concept`'s UNIQUE key collision (MAJOR-1).

use std::collections::HashSet;
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
use crate::store::{Capabilities, GraphStore};
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, EmbeddingContract,
    LamboError, Node, NodeId, SessionId, StoreError,
};

/// Default merge threshold (spec §7.1 step 6). Configurable per call
/// (driven by `Config::semantic_match_threshold`); biased toward precision —
/// under-merging into separate concepts is safe for canonization.
pub const SEMANTIC_MATCH_THRESHOLD_DEFAULT: f64 = 0.85;

/// How many vector candidates to request from the store per concept.
pub const VECTOR_CANDIDATE_LIMIT: usize = 8;

/// Hard request bounds are checked before the first await. They prevent one
/// derive call from turning attacker-controlled input into unbounded external
/// embed/store work while leaving normal multi-concept calls ample headroom.
pub const MAX_HYBRID_CONCEPTS: usize = 256;
pub const MAX_HYBRID_PARENT_PAIRS: usize = 256;
pub const MAX_HYBRID_WORK_ITEMS: usize = 512;
pub const MAX_HYBRID_CONTEXT_BYTES: usize = 16 * 1024;
pub const HYBRID_IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HYBRID_REPLANS: usize = 8;

/// Initial weight of a `Semantic` merge edge: the accepted cosine similarity,
/// clamped into the legal `[0, MAX_EDGE_WEIGHT]` range. A merge only happens
/// at >= 0.85, so the weight is always positive and finite, and the edge
/// decays over time (see the spec §5 decay table, where `Semantic` decays).
fn semantic_weight(score: f64) -> f64 {
    score.clamp(0.0, crate::graph::MAX_EDGE_WEIGHT)
}

fn best_candidate(
    hits: &[crate::types::Scored<NodeId>],
    threshold: f64,
) -> Option<&crate::types::Scored<NodeId>> {
    hits.iter()
        .filter(|c| c.score.is_finite() && (0.0..=1.0).contains(&c.score) && c.score >= threshold)
        // Prefer higher similarity, then the lexicographically smaller UUID.
        // Reversing the UUID comparison is required because `max_by` selects
        // Ordering::Greater.
        .max_by(|a, b| {
            a.score
                .total_cmp(&b.score)
                .then_with(|| b.item.0.cmp(&a.item.0))
        })
}

/// The per-concept resolution computed by the async gather phase.
enum Resolution {
    /// Canonical key already matched an existing concept — reuse it (byte-identical
    /// to derive's `Matched` path).
    CanonicalMatch { node: NodeId },
    /// `Unmatched` with no viable vector hit (capability absent, embed failure,
    /// below threshold, or invalid candidate): create a fresh keyword-only concept.
    Fresh {
        key: String,
        embedding: Option<Vec<f32>>,
    },
    /// `Unmatched` with a vector hit at/above threshold: create the concept and
    /// a decaying `Semantic` edge to the matched concept.
    HybridMerge {
        key: String,
        target: NodeId,
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
fn context_text(content: &str, origin: Option<&str>) -> String {
    match origin.map(str::trim).filter(|s| !s.is_empty()) {
        Some(origin) => format!("{content} — {origin}"),
        None => format!("Concept: {content}"),
    }
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
/// When the store lacks `Capabilities::VECTOR_SEARCH` or embedding fails, the
/// outcome is byte-identical to `derive` (fresh keyword-only concepts, no
/// store I/O beyond the capability probe).
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
) -> Result<DeriveOutcome, LamboError> {
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

    // Writers outside hybrid (daemon maintenance, future MCP tasks) need not
    // share a mutex with this function. Epoch validation makes their mutations
    // visible; a stale gather is discarded and planned again.
    let io_deadline = tokio::time::Instant::now() + HYBRID_IO_TIMEOUT;
    for _attempt in 0..MAX_HYBRID_REPLANS {
        // -----------------------------------------------------------------------
        // Phase 1 — plan under a brief read lock (no I/O, no await).
        // -----------------------------------------------------------------------
        let (planned_epoch, session_id, interaction_created_at, origin_text, stamped, items) = {
            let g = graph.read();
            let planned_epoch = g.epoch();
            let session_id = g.session_id().clone();
            let (interaction_created_at, origin_text) = match g.node(interaction) {
                Some(Node::Interaction(i)) => (i.created_at, i.prompt_text.clone()),
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

            // Step 1 — validate every parent_of reflexivity + empty key up front
            // (mirror derive validate-then-mutate; nothing written yet).
            for &(parent, child) in parent_of.pairs() {
                if parent == child {
                    return Err(LamboError::Store(StoreError::Invariant(format!(
                        "hybrid derive: parent_of pair ({parent}, {child}) is reflexive — a \
                     Hierarchical self-loop is a cycle (spec §5.7)"
                    ))));
                }
                let pk = match canonicalize(parent, &g)? {
                    CanonicalizeResult::Matched { key, .. }
                    | CanonicalizeResult::Unmatched { key } => key,
                };
                let ck = match canonicalize(child, &g)? {
                    CanonicalizeResult::Matched { key, .. }
                    | CanonicalizeResult::Unmatched { key } => key,
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

            // Step 2 — dedup by content + canonicalize. Items are the unique concepts
            // of this call with their resolution so far (canonical match id if any).
            let mut seen: HashSet<&str> = HashSet::with_capacity(concepts.len());
            let mut items: Vec<(&str, ConceptType, String, Option<NodeId>)> =
                Vec::with_capacity(concepts.len());
            for &(content, concept_type) in concepts {
                if !seen.insert(content) {
                    continue;
                }
                let (key, matched) = match canonicalize(content, &g)? {
                    CanonicalizeResult::Matched { key, node } => (key, Some(node)),
                    CanonicalizeResult::Unmatched { key } => (key, None),
                };
                reject_empty_key(content, &key)?;
                items.push((content, concept_type, key, matched));
            }

            if origin_text
                .as_ref()
                .is_some_and(|origin| origin.len() > MAX_HYBRID_CONTEXT_BYTES)
            {
                return Err(LamboError::Config(format!(
                    "hybrid interaction context exceeds {MAX_HYBRID_CONTEXT_BYTES} bytes"
                )));
            }
            let origin_len = origin_text.as_deref().map(str::trim).map_or(0, str::len);
            if items.iter().any(|(content, _, _, matched)| {
                matched.is_none()
                    && content.len().saturating_add(origin_len).saturating_add(3)
                        > MAX_HYBRID_CONTEXT_BYTES
            }) {
                return Err(LamboError::Config(format!(
                    "hybrid embedding context exceeds {MAX_HYBRID_CONTEXT_BYTES} bytes"
                )));
            }

            (
                planned_epoch,
                session_id,
                interaction_created_at,
                origin_text,
                stamped,
                items,
            )
        };

        // The vector leg can run only when the store advertises it. Probed once,
        // synchronously, before any I/O (mirrors the recall RAM-tier promise:
        // zero async store calls when the capability is absent).
        let vector_ok = store.capabilities().contains(Capabilities::VECTOR_SEARCH);

        // Mid-session contract check — refuse a kind/model/dim swap BEFORE any embed
        // ("without re-embed"). Only enforced when we are actually about to embed
        // (capability present and at least one unmatched concept). ensure_compatible
        // is a pure comparison; it never embeds.
        let has_unmatched = items.iter().any(|(_, _, _, matched)| matched.is_none());
        if vector_ok && has_unmatched {
            if let Some(existing) = &stamped {
                existing.ensure_compatible(embedding)?;
            }
        }

        // -----------------------------------------------------------------------
        // Phase 2 — async gather (no lock held).
        // -----------------------------------------------------------------------
        // First-session fallback log for the capability-absent path (zero I/O, so
        // acceptable here); embed-failure logging happens per concept below.
        let mut attempted_embed = false;
        let mut resolutions: Vec<Resolution> = Vec::with_capacity(items.len());
        for (content, _concept_type, key, matched) in &items {
            let res = match matched {
                Some(node) => Resolution::CanonicalMatch { node: *node },
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
                    }
                }
                None => {
                    let context = context_text(content, origin_text.as_deref());
                    match tokio::time::timeout_at(io_deadline, embedder.embed(&context)).await {
                        Err(_) => {
                            if note_fallback_logged(&session_id) {
                                tracing::warn!(
                                    target: "lambo::hybrid",
                                    session = %session_id,
                                    "hybrid embed timed out - degrading to MatchStrategy::Canonical"
                                );
                            }
                            Resolution::Fresh {
                                key: key.clone(),
                                embedding: None,
                            }
                        }
                        Ok(Err(e)) => {
                            if note_fallback_logged(&session_id) {
                                tracing::warn!(
                                    target: "lambo::hybrid",
                                    session = %session_id,
                                    error = %e,
                                    "hybrid embed failed — degrading to MatchStrategy::Canonical \
                                     (creating keyword-only concept)"
                                );
                            }
                            Resolution::Fresh {
                                key: key.clone(),
                                embedding: None,
                            }
                        }
                        Ok(Ok(emb)) => {
                            // An embed only counts as "attempted" for the contract
                            // stamp once it actually returned a vector — a failed
                            // attempt must not bind the session to an embedding
                            // space it produced no vector in (MINOR-2).
                            attempted_embed = true;
                            match tokio::time::timeout_at(
                                io_deadline,
                                store.vector_candidates(&session_id, &emb, VECTOR_CANDIDATE_LIMIT),
                            )
                            .await
                            {
                                Err(_) => {
                                    return Err(StoreError::Backend(format!(
                                        "hybrid vector candidate lookup timed out after \
                                         {HYBRID_IO_TIMEOUT:?}"
                                    ))
                                    .into())
                                }
                                Ok(Ok(hits)) => {
                                    // Highest-scoring candidate at/above threshold (store
                                    // results are not guaranteed sorted). The candidate is
                                    // validated to be a real distinct concept at commit.
                                    let best = best_candidate(&hits, semantic_match_threshold);
                                    match best {
                                        Some(c) => Resolution::HybridMerge {
                                            key: key.clone(),
                                            target: c.item,
                                            score: c.score,
                                            embedding: emb,
                                        },
                                        // Below threshold: fresh concept, keyword-only.
                                        // Writing the vector here would let a 'far'
                                        // concept become a future vector candidate —
                                        // the exact over-merge the precision bias
                                        // prevents (PHASE-7 T7.2 law, MAJOR-1).
                                        None => Resolution::Fresh {
                                            key: key.clone(),
                                            embedding: None,
                                        },
                                    }
                                }
                                Ok(Err(StoreError::Capability(_))) => {
                                    if note_fallback_logged(&session_id) {
                                        tracing::warn!(
                                            target: "lambo::hybrid",
                                            session = %session_id,
                                            "store refused vector_candidates (capability miss) — \
                                             degrading to MatchStrategy::Canonical (creating \
                                             keyword-only concept)"
                                        );
                                    }
                                    Resolution::Fresh {
                                        key: key.clone(),
                                        embedding: None,
                                    }
                                }
                                Ok(Err(e)) => return Err(e.into()),
                            }
                        }
                    }
                }
            };
            resolutions.push(res);
        }
        let _ = has_unmatched;

        // -----------------------------------------------------------------------
        // Phase 3 — commit under a write lock (sync, no await).
        // -----------------------------------------------------------------------
        let mut guard = graph.write();
        if guard.epoch() != planned_epoch {
            continue;
        }
        if attempted_embed {
            if let Some(existing) = guard.embedding() {
                // Revalidate under the commit lock. Two first writers can both plan
                // against `None`; only the winner may stamp its vector space.
                existing.ensure_compatible(embedding)?;
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
            if let CanonicalizeResult::Matched { node, .. } = canonicalize(content, &g)? {
                if written.contains(&node) {
                    outcome.matched.push(node);
                    if !call_nodes.contains(&node) {
                        call_nodes.push(node);
                    }
                    continue;
                }
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
                        g.insert_concept(existing, interaction)?;
                        written.insert(*node);
                        outcome.matched.push(*node);
                        this_node = *node;
                    }
                }
                Resolution::Fresh { key, embedding } => {
                    let concept = new_concept(
                        &session_id,
                        content,
                        *concept_type,
                        key.clone(),
                        interaction,
                        agent,
                        interaction_created_at,
                        embedding.clone(),
                    );
                    let id = concept.id;
                    g.insert_concept(concept, interaction)?;
                    written.insert(id);
                    outcome.created.push(id);
                    this_node = id;
                }
                Resolution::HybridMerge {
                    key,
                    target,
                    score,
                    embedding,
                } => {
                    // MINOR-2 (P7 remediation): the concept keeps its vector ONLY if
                    // the merge Semantic edge is actually written. Both endpoints
                    // must be concepts (GRAPH-2); validate the target up front and,
                    // when the store handed us a bogus non-Concept candidate, refuse
                    // the merge and degrade to a TRUE keyword-only concept
                    // (embedding: None) — the concept must never persist a vector for
                    // a merge it refused to make (consistent with the other
                    // below-threshold / capability-miss / embed-failure fallbacks).
                    let can_merge = matches!(g.node(*target), Some(Node::Concept(_)));
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
                    );
                    let id = concept.id;
                    g.insert_concept(concept, interaction)?;
                    written.insert(id);
                    outcome.created.push(id);
                    if can_merge && *target != id {
                        // Decaying Semantic edge to the matched concept (deterministic
                        // direction: order endpoints by NodeId's inner UUID, since
                        // NodeId itself is not Ord). `*target != id` is defense-in-depth:
                        // a store-returned target can never equal this fresh id.
                        let (s, t) = if target.0 < id.0 {
                            (*target, id)
                        } else {
                            (id, *target)
                        };
                        g.upsert_edge(Edge {
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
                        outcome.semantic_merged.push(*target);
                    }
                    this_node = id;
                }
            }
            if !call_nodes.contains(&this_node) {
                call_nodes.push(this_node);
            }
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

/// Mirror `derive::resolve_concept`'s canonical path for `ParentOf` contents
/// (these never go through the hybrid step here: `ParentOf` creates/reuses
/// concepts with the generic `Entity` type and a Hierarchical edge, exactly as
/// sync derive's step 6 does).
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
) -> Result<NodeId, LamboError> {
    match canonicalize(content, graph)? {
        CanonicalizeResult::Unmatched { key } => {
            let concept = new_concept(
                session_id,
                content,
                concept_type,
                key,
                interaction,
                agent,
                created_at,
                None,
            );
            let id = concept.id;
            graph.insert_concept(concept, interaction)?;
            written.insert(id);
            outcome.created.push(id);
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
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use async_trait::async_trait;
    use chrono::{DateTime, TimeZone, Utc};
    use tokio::sync::{Barrier, Notify};
    use uuid::Uuid;

    use super::*;
    use crate::embed::{EmbedError, Embedder, FixtureEmbedder, FAR, NEAR_A, NEAR_B};

    use crate::types::{Interaction, MutationBatch, Scored};

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid(name: &str) -> SessionId {
        SessionId::from(name)
    }

    fn agent() -> AgentId {
        AgentId::from("agent-a")
    }

    fn contract(kind: &str, dim: usize) -> EmbeddingContract {
        EmbeddingContract {
            kind: kind.into(),
            model: None,
            dim,
        }
    }

    /// Interaction with a recognizable origin `prompt_text` (the calibration
    /// context the hybrid step must embed alongside the concept name).
    fn interaction(id: u64, prev: Option<NodeId>, at_min: i64, prompt: &str) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid("hybrid-test"),
            agent_id: agent(),
            prompt_text: Some(prompt.to_string()),
            previous_id: prev,
            created_at: ts(at_min),
        }
    }

    fn graph_with_interaction(
        sess: &str,
        id: u64,
        at_min: i64,
        prompt: &str,
    ) -> (Arc<RwLock<Graph>>, NodeId) {
        let mut g = Graph::new(sid(sess));
        let mut i = interaction(id, None, at_min, prompt);
        i.session_id = sid(sess);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        (Arc::new(RwLock::new(g)), iid)
    }

    /// Deterministic `Scored<NodeId>: {item, score}` helper.
    fn hit(id: NodeId, score: f64) -> Scored<NodeId> {
        Scored { item: id, score }
    }

    /// Store double advertising configurable capabilities and returning canned
    /// vector hits (mirrors recall's `SpyVectorStore`). A non-`Capability`
    /// backend error from `vector_candidates` can be forced for the propagate
    /// case. Any async method other than `vector_candidates` panics so a test
    /// cannot silently reach the store through the wrong surface.
    struct SpyStore {
        caps: Capabilities,
        hits: Vec<Scored<NodeId>>,
        backend_err: bool,
        vector_calls: Arc<AtomicUsize>,
    }

    impl SpyStore {
        fn with_vector(hits: Vec<Scored<NodeId>>) -> Self {
            Self {
                caps: Capabilities::VECTOR_SEARCH | Capabilities::HISTORY,
                hits,
                backend_err: false,
                vector_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn without_vector() -> Self {
            Self {
                caps: Capabilities::HISTORY,
                hits: Vec::new(),
                backend_err: false,
                vector_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn failing() -> Self {
            Self {
                caps: Capabilities::VECTOR_SEARCH | Capabilities::HISTORY,
                hits: Vec::new(),
                backend_err: true,
                vector_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn vector_calls(&self) -> usize {
            self.vector_calls.load(Ordering::SeqCst)
        }
        fn unexpected(&self) -> ! {
            panic!("SpyStore: unexpected async store call (only vector_candidates allowed)")
        }
    }
    #[async_trait]
    impl GraphStore for SpyStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.unexpected()
        }
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        async fn flush(&self, _batch: &MutationBatch) -> Result<(), StoreError> {
            self.unexpected()
        }
        async fn load_session(
            &self,
            _session: &SessionId,
        ) -> Result<crate::types::GraphSnapshot, StoreError> {
            self.unexpected()
        }
        async fn keyword_candidates(
            &self,
            _session: &SessionId,
            _tokens: &[String],
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.unexpected()
        }
        async fn vector_candidates(
            &self,
            _session: &SessionId,
            _embedding: &[f32],
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.vector_calls.fetch_add(1, Ordering::SeqCst);
            if self.backend_err {
                return Err(StoreError::Backend("boom".into()));
            }
            Ok(self.hits.clone())
        }
        async fn blast_radius(
            &self,
            _session: &SessionId,
            _node: NodeId,
            _min_edge_age: std::time::Duration,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, StoreError> {
            self.unexpected()
        }
        async fn interaction_span(
            &self,
            _session: &SessionId,
            _node: NodeId,
            _min_age: std::time::Duration,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            self.unexpected()
        }
        async fn record_canonization(
            &self,
            _event: &crate::types::CanonizationEvent,
        ) -> Result<(), StoreError> {
            self.unexpected()
        }
    }

    /// Records every text handed to `embed` (so the context rule is assertable)
    /// while delegating the actual vector to `FixtureEmbedder` — the production
    /// FixtureEmbedder used by the whole test suite for near/far geometry.
    #[derive(Debug, Clone)]
    struct RecordingEmbedder {
        inner: FixtureEmbedder,
        texts: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingEmbedder {
        fn new() -> Self {
            Self {
                inner: FixtureEmbedder::new(),
                texts: Arc::new(Mutex::new(Vec::new())),
            }
        }
        fn embedded_texts(&self) -> Vec<String> {
            self.texts.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Embedder for RecordingEmbedder {
        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            self.texts.lock().unwrap().push(text.to_string());
            self.inner.embed(text).await
        }
    }

    /// Embedder whose `embed` always fails (the degradation / logged-once case).
    #[derive(Debug, Clone)]
    struct FailingEmbedder {
        calls: Arc<AtomicUsize>,
    }
    impl FailingEmbedder {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl Embedder for FailingEmbedder {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(EmbedError::Unavailable("server down".into()))
        }
    }

    #[derive(Debug)]
    struct BarrierEmbedder {
        barrier: Barrier,
        calls: AtomicUsize,
    }

    impl BarrierEmbedder {
        fn new(parties: usize) -> Self {
            Self {
                barrier: Barrier::new(parties),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Embedder for BarrierEmbedder {
        fn dimensions(&self) -> usize {
            1024
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < 2 {
                self.barrier.wait().await;
            }
            Ok(vec![0.0; 1024])
        }
    }

    #[derive(Debug)]
    struct PausingEmbedder {
        started: Notify,
        release: Notify,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Embedder for PausingEmbedder {
        fn dimensions(&self) -> usize {
            1024
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(vec![0.0; 1024])
        }
    }

    #[test]
    fn candidate_validation_and_ties_are_deterministic() {
        let lower = NodeId(Uuid::from_u64_pair(0, 1));
        let higher = NodeId(Uuid::from_u64_pair(0, 2));
        let invalid = NodeId(Uuid::from_u64_pair(0, 3));
        let a = vec![
            hit(higher, 0.9),
            hit(invalid, f64::NAN),
            hit(lower, 0.9),
            hit(invalid, 1.1),
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(best_candidate(&a, 0.85).unwrap().item, lower);
        assert_eq!(best_candidate(&b, 0.85).unwrap().item, lower);
        assert!(best_candidate(&[hit(invalid, f64::INFINITY)], 0.85).is_none());
    }

    #[tokio::test]
    async fn concurrent_first_writers_cannot_mix_embedding_contracts() {
        let (graph, interaction) =
            graph_with_interaction("hybrid-contract-race", 1, 0, "concurrent contract race");
        let embedder = Arc::new(BarrierEmbedder::new(2));
        let store = Arc::new(SpyStore::with_vector(Vec::new()));

        let spawn = |kind: &'static str, content: &'static str| {
            let graph = graph.clone();
            let embedder = embedder.clone();
            let store = store.clone();
            tokio::spawn(async move {
                derive(
                    graph,
                    store.as_ref(),
                    embedder.as_ref(),
                    &contract(kind, 1024),
                    interaction,
                    &agent(),
                    &[(content, ConceptType::Entity)],
                    &ParentOf::none(),
                    10,
                    SEMANTIC_MATCH_THRESHOLD_DEFAULT,
                )
                .await
            })
        };
        let (a, b) = tokio::join!(spawn("fixture-a", "alpha"), spawn("fixture-b", "beta"));
        let outcomes = [a.unwrap(), b.unwrap()];
        assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(outcomes.iter().filter(|r| r.is_err()).count(), 1);
        let g = graph.read();
        assert_eq!(
            g.concepts().count(),
            1,
            "losing vector space writes nothing"
        );
        g.assert_invariants().unwrap();
    }

    #[tokio::test]
    async fn first_use_empty_candidates_still_commits_contract() {
        // Cockroach returns this safe empty shape for a missing/unstamped
        // session before the first SetEmbedding commit.
        let (graph, interaction) =
            graph_with_interaction("hybrid-first-use", 1, 0, "first use context");
        let store = SpyStore::with_vector(Vec::new());
        let embedder = FixtureEmbedder::new();
        derive(
            graph.clone(),
            &store,
            &embedder,
            &contract("fixture", 1024),
            interaction,
            &agent(),
            &[("first embedded concept", ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();

        assert_eq!(store.vector_calls(), 1);
        let mut g = graph.write();
        assert_eq!(g.embedding(), Some(&contract("fixture", 1024)));
        assert_eq!(g.concepts().count(), 1);
        // Precision-biased hybrid writes a fresh candidate keyword-only when
        // the store has no trusted matches; the contract is still committed
        // so later writes/searches use one known vector space.
        assert!(g.concepts().all(|concept| concept.embedding.is_none()));
        assert!(g.drain_log().mutations.iter().any(|mutation| matches!(
            mutation,
            crate::types::Mutation::SetEmbedding {
                embedding: Some(_),
                ..
            }
        )));
    }

    #[tokio::test]
    async fn intervening_graph_mutation_discards_stale_gather_and_replans() {
        let (graph, interaction) = graph_with_interaction("hybrid-epoch-race", 1, 0, "epoch race");
        let embedder = Arc::new(PausingEmbedder {
            started: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        });
        let store = Arc::new(SpyStore::with_vector(Vec::new()));
        let task = {
            let graph = graph.clone();
            let embedder = embedder.clone();
            let store = store.clone();
            tokio::spawn(async move {
                derive(
                    graph,
                    store.as_ref(),
                    embedder.as_ref(),
                    &contract("fixture", 1024),
                    interaction,
                    &agent(),
                    &[("stale plan", ConceptType::Entity)],
                    &ParentOf::none(),
                    10,
                    SEMANTIC_MATCH_THRESHOLD_DEFAULT,
                )
                .await
            })
        };
        embedder.started.notified().await;
        graph
            .write()
            .set_root_goal(Some(serde_json::json!("concurrent daemon mutation")));
        embedder.release.notify_one();
        task.await.unwrap().unwrap();

        assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);
        assert_eq!(store.vector_calls(), 2);
        let g = graph.read();
        assert_eq!(g.concepts().count(), 1, "stale attempt wrote nothing");
        g.assert_invariants().unwrap();
    }

    #[tokio::test]
    async fn intervening_synonym_change_discards_stale_gather_and_replans() {
        let (graph, interaction) =
            graph_with_interaction("hybrid-synonym-race", 1, 0, "synonym race");
        let embedder = Arc::new(PausingEmbedder {
            started: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        });
        let store = Arc::new(SpyStore::with_vector(Vec::new()));
        let task = {
            let graph = graph.clone();
            let embedder = embedder.clone();
            let store = store.clone();
            tokio::spawn(async move {
                derive(
                    graph,
                    store.as_ref(),
                    embedder.as_ref(),
                    &contract("fixture", 1024),
                    interaction,
                    &agent(),
                    &[("alias", ConceptType::Entity)],
                    &ParentOf::none(),
                    10,
                    SEMANTIC_MATCH_THRESHOLD_DEFAULT,
                )
                .await
            })
        };
        embedder.started.notified().await;
        graph.write().declare_synonym("alias", "canonical target");
        embedder.release.notify_one();
        task.await.unwrap().unwrap();

        assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);
        assert_eq!(store.vector_calls(), 2);
        let concept = graph.read().concepts().next().unwrap().clone();
        assert_eq!(concept.canonical_key, "canon target");
    }

    #[tokio::test]
    async fn invalid_or_oversized_requests_do_no_external_work() {
        let (graph, interaction) = graph_with_interaction("hybrid-bounds", 1, 0, "bounded");
        let embedder = FailingEmbedder::new();
        let store = SpyStore::with_vector(Vec::new());
        let concepts = vec![("x", ConceptType::Entity); MAX_HYBRID_CONCEPTS + 1];
        let err = derive(
            graph.clone(),
            &store,
            &embedder,
            &contract("fixture", 1024),
            interaction,
            &agent(),
            &concepts,
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LamboError::Config(_)));
        assert_eq!(embedder.calls(), 0);
        assert_eq!(store.vector_calls(), 0);

        let parents: Vec<(String, String)> = (0..=MAX_HYBRID_PARENT_PAIRS)
            .map(|n| (format!("parent-{n}"), format!("child-{n}")))
            .collect();
        let parent_refs: Vec<(&str, &str)> = parents
            .iter()
            .map(|(parent, child)| (parent.as_str(), child.as_str()))
            .collect();
        let before = graph.read().snapshot();
        let err = derive(
            graph.clone(),
            &store,
            &embedder,
            &contract("fixture", 1024),
            interaction,
            &agent(),
            &[("valid", ConceptType::Entity)],
            &ParentOf::from_pairs(&parent_refs),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LamboError::Config(_)));
        assert_eq!(embedder.calls(), 0);
        assert_eq!(store.vector_calls(), 0);
        assert_eq!(graph.read().snapshot(), before, "rejection mutates nothing");

        let err = derive(
            graph,
            &store,
            &embedder,
            &contract("fixture", 1024),
            interaction,
            &agent(),
            &[("valid", ConceptType::Entity)],
            &ParentOf::none(),
            10,
            f64::NAN,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LamboError::Config(_)));
        assert_eq!(embedder.calls(), 0);
        assert_eq!(store.vector_calls(), 0);
    }

    #[tokio::test]
    async fn near_pair_merges_with_decaying_semantic_edge() {
        let sess = "hybrid-near";
        // Interaction 1 derives "register user" -> fresh concept C1.
        let (graph, i1) = graph_with_interaction(sess, 1, 0, "user signs up for the platform");
        // Interaction 2 (origin context for the near content).
        let mut second = interaction(2, Some(i1), 60, "an admin creates an account for a user");
        second.session_id = sid(sess);
        let i2 = second.id;
        graph.write().insert_interaction(second).unwrap();

        // Interaction 1: derive "register user" (canonical miss -> fresh concept).
        let out1 = derive(
            graph.clone(),
            &SpyStore::with_vector(Vec::new()),
            &RecordingEmbedder::new(),
            &contract("fixture", 1024),
            i1,
            &agent(),
            &[(NEAR_A, ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();
        assert_eq!(out1.created.len(), 1);
        let c1 = out1.created[0];

        // Interaction 2: "create account" — store returns C1 at 0.9 (>= 0.85).
        let recorder = RecordingEmbedder::new();
        let out2 = derive(
            graph.clone(),
            &SpyStore::with_vector(vec![hit(c1, 0.9)]),
            &recorder,
            &contract("fixture", 1024),
            i2,
            &agent(),
            &[(NEAR_B, ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();

        // Calibration rule: the embed was called WITH context (name + origin),
        // never the bare label.
        let texts = recorder.embedded_texts();
        assert_eq!(
            texts.len(),
            1,
            "exactly one embed for the single unmatched concept"
        );
        let ctx = &texts[0];
        assert!(
            ctx.contains(NEAR_B) && ctx.contains("creates an account"),
            "embed must carry the concept name AND origin interaction text, got {ctx:?}"
        );
        assert!(
            !ctx.contains("Concept:"),
            "origin text present so the context must not fall back to the bare framing"
        );

        // The near pair merged into a SECOND concept + a decaying Semantic edge
        // (a merge cannot be a bare reuse: a `Semantic` edge legally connects two
        // concepts, and the new content has a distinct canonical key).
        assert_eq!(out2.created.len(), 1, "new concept for the near surface");
        let c2 = out2.created[0];
        // MINOR-3: the merge target reports in `semantic_merged`, NOT `matched`.
        // A merge does not re-upsert the target nor Derives-reinforce it, so
        // `matched` must not over-count it as "re-derived" (derive contract).
        assert!(
            out2.matched.is_empty(),
            "merge target must not pollute matched"
        );
        assert_eq!(out2.semantic_merged, vec![c1]);
        let sem = {
            let g = graph.read();
            g.edge_between(c1, c2, EdgeType::Semantic)
                .or_else(|| g.edge_between(c2, c1, EdgeType::Semantic))
                .cloned()
        }
        .expect("Semantic merge edge exists");
        assert!(
            sem.weight >= 0.85,
            "edge weight reflects the accepted score"
        );
        // No canonical duplicate: the two concepts carry distinct canonical keys.
        let (k1, k2) = {
            let g = graph.read();
            let a = match g.node(c1) {
                Some(Node::Concept(c)) => c.canonical_key.clone(),
                _ => unreachable!(),
            };
            let b = match g.node(c2) {
                Some(Node::Concept(c)) => c.canonical_key.clone(),
                _ => unreachable!(),
            };
            (a, b)
        };
        assert_ne!(k1, k2, "hybrid never creates a canonical-key duplicate");
        // Contract stamped on first embed.
        assert_eq!(graph.read().embedding().unwrap().kind, "fixture");
        graph.read().assert_invariants().unwrap();
    }

    #[tokio::test]
    async fn far_text_creates_fresh_keyword_concept() {
        let sess = "hybrid-far";
        let (graph, i1) = graph_with_interaction(sess, 1, 0, "user signs up for the platform");
        let c1 = {
            let out = derive(
                graph.clone(),
                &SpyStore::with_vector(Vec::new()),
                &RecordingEmbedder::new(),
                &contract("fixture", 1024),
                i1,
                &agent(),
                &[(NEAR_A, ConceptType::Entity)],
                &ParentOf::none(),
                10,
                SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            )
            .await
            .unwrap();
            out.created[0]
        };

        let mut second = interaction(2, Some(i1), 60, "physics notes on gauge theories");
        second.session_id = sid(sess);
        let i2 = second.id;
        graph.write().insert_interaction(second).unwrap();

        // FAR is well below threshold: 0.2 < 0.85 -> fresh concept, keyword-only.
        let out2 = derive(
            graph.clone(),
            &SpyStore::with_vector(vec![hit(c1, 0.2)]),
            &RecordingEmbedder::new(),
            &contract("fixture", 1024),
            i2,
            &agent(),
            &[(FAR, ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();

        assert_eq!(out2.created.len(), 1);
        assert!(out2.matched.is_empty());
        let c2 = out2.created[0];
        let g = graph.read();
        // MAJOR-1: the below-threshold fresh concept must be keyword-only — a
        // written vector here would let a 'far' concept become a vector
        // candidate on a later hybrid derive (the exact over-merge the
        // precision bias prevents).
        match g.node(c2) {
            Some(Node::Concept(con)) => assert!(
                con.embedding.is_none(),
                "below-threshold fresh concept must not carry a vector"
            ),
            _ => unreachable!(),
        }
        // No Semantic edge: keyword-only fresh concept.
        assert!(g.edge_between(c1, c2, EdgeType::Semantic).is_none());
        assert!(g.edge_between(c2, c1, EdgeType::Semantic).is_none());
        assert!(
            g.edge_between(i2, c2, EdgeType::Derives).is_some(),
            "derives edge present"
        );
        g.assert_invariants().unwrap();
    }

    #[tokio::test]
    async fn no_capability_is_byte_identical_to_canonical() {
        // Run the SAME interaction twice: once through the sync `derive`
        // (MatchStrategy::Canonical) and once through `hybrid::derive` against a
        // store without VECTOR_SEARCH. The graph shapes must be identical
        // (fresh concept + Derives, no Semantic edge, zero embed / vector calls).
        use crate::graph::derive::derive as canonical_derive;

        let sess = "hybrid-nocap";
        let (graph_h, i1) = graph_with_interaction(sess, 1, 0, "auth flow for users");
        let recorder = RecordingEmbedder::new();
        let out_h = derive(
            graph_h.clone(),
            &SpyStore::without_vector(),
            &recorder,
            &contract("fixture", 1024),
            i1,
            &agent(),
            &[
                ("register user", ConceptType::Entity),
                ("create account", ConceptType::Entity),
            ],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();

        // No embedding was attempted (capability gated before any I/O) and no
        // contract was stamped — byte-identical to Canonical.
        assert!(
            recorder.embedded_texts().is_empty(),
            "no embed when no VECTOR_SEARCH"
        );
        assert!(
            graph_h.read().embedding().is_none(),
            "no contract stamp on degraded path"
        );
        assert!(!out_h.matched.iter().any(|n| n == &i1));

        // Sync canonical twin for the same inputs.
        let (mut g_c, i1c) = {
            let mut g = Graph::new(sid(sess));
            let mut i = interaction(1, None, 0, "auth flow for users");
            i.session_id = sid(sess);
            let id = i.id;
            g.insert_interaction(i).unwrap();
            (g, id)
        };
        let out_c = canonical_derive(
            &mut g_c,
            i1c,
            &agent(),
            &[
                ("register user", ConceptType::Entity),
                ("create account", ConceptType::Entity),
            ],
            &ParentOf::none(),
            10,
        )
        .unwrap();

        fn shape(
            g: &Graph,
        ) -> (
            Vec<(String, String)>,
            std::collections::HashMap<EdgeType, usize>,
        ) {
            let mut nodes: Vec<(String, String)> = g
                .concepts()
                .map(|c| (c.canonical_key.clone(), format!("{:?}", c.concept_type)))
                .collect();
            nodes.sort();
            let mut edges = std::collections::HashMap::new();
            for e in g.edges() {
                *edges.entry(e.edge_type).or_insert(0) += 1;
            }
            (nodes, edges)
        }

        assert_eq!(out_h.created.len(), out_c.created.len());
        assert_eq!(shape(&graph_h.read()), shape(&g_c));
    }

    #[tokio::test]
    async fn mid_session_kind_swap_refused_without_reembed() {
        let sess = "hybrid-swap";
        // Session already stamped with fixture (kind "fixture"). A hybrid write
        // arriving with a different live kind (bedrock) must be refused BEFORE
        // embedding — the embedder must never be called.
        let (graph, i1) = graph_with_interaction(sess, 1, 0, "auth flow");
        graph
            .write()
            .stamp_embedding(contract("fixture", 1024))
            .unwrap();
        let failing = FailingEmbedder::new(); // would panic the test if called via embed -> no, it fails; use a panic embedder

        let err = derive(
            graph.clone(),
            &SpyStore::with_vector(vec![]),
            &failing,
            &contract("bedrock", 1024), // same dim, different kind — the trap
            i1,
            &agent(),
            &[("create account", ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, LamboError::Config(_)),
            "unexpected error: {err:?}"
        );
        // Refused WITHOUT re-embed: embedder never invoked, graph unchanged.
        assert_eq!(failing.calls(), 0);
        let g = graph.read();
        assert_eq!(g.node_count(), 1, "interaction only — no concept written");
        assert_eq!(g.embedding().unwrap().kind, "fixture", "stamp preserved");
    }

    #[tokio::test]
    async fn embed_failure_degrades_to_fresh_concept() {
        let sess = "hybrid-embedfail";
        let (graph, i1) = graph_with_interaction(sess, 1, 0, "auth flow");
        let failing = FailingEmbedder::new();
        let out = derive(
            graph.clone(),
            &SpyStore::with_vector(vec![]),
            &failing,
            &contract("fixture", 1024),
            i1,
            &agent(),
            &[("create account", ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();
        assert_eq!(failing.calls(), 1, "embed attempted once then degraded");
        assert_eq!(out.created.len(), 1);
        assert!(out.matched.is_empty());
        let g = graph.read();
        let c = out.created[0];
        assert!(g.edge_between(i1, c, EdgeType::Derives).is_some());
        // Concept has no embedding (the embed itself failed).
        match g.node(c) {
            Some(Node::Concept(con)) => assert!(con.embedding.is_none()),
            _ => unreachable!(),
        }
        // MINOR-2: the session's embedding contract must NOT be stamped — no
        // embed ever returned a vector, so this is not a "first embed".
        assert!(
            g.embedding().is_none(),
            "a fully-failed embed must not bind the session to an embedding contract"
        );
        g.assert_invariants().unwrap();
    }

    #[tokio::test]
    async fn real_backend_error_propagates_not_degrade() {
        let sess = "hybrid-backend";
        let (graph, i1) = graph_with_interaction(sess, 1, 0, "auth flow");
        // A genuine (non-Capability) store error must NOT be swallowed into a
        // fresh concept — it propagates, so a broken backend is visible.
        let err = derive(
            graph.clone(),
            &SpyStore::failing(),
            &RecordingEmbedder::new(),
            &contract("fixture", 1024),
            i1,
            &agent(),
            &[("create account", ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, LamboError::Store(StoreError::Backend(_))),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn semantic_edge_decays() {
        assert!(EdgeType::Semantic.decays());
        // And it is Concept->Concept (the reason a merge needs a second concept).
        let mut g = Graph::new(sid("matrix"));
        let i = NodeId(Uuid::from_u64_pair(1, 99));
        let a = NodeId(Uuid::from_u64_pair(2, 1));
        let b = NodeId(Uuid::from_u64_pair(2, 2));
        let at = ts(0);
        g.insert_interaction(Interaction {
            id: i,
            session_id: sid("matrix"),
            agent_id: agent(),
            prompt_text: Some("go".into()),
            previous_id: None,
            created_at: at,
        })
        .unwrap();
        g.insert_concept(
            Concept {
                id: a,
                session_id: sid("matrix"),
                content: "x".into(),
                canonical_key: "x".into(),
                concept_type: ConceptType::Entity,
                origin_interaction: i,
                origin_agent: agent(),
                created_at: at,
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding: None,
                chunk_group_id: None,
            },
            i,
        )
        .unwrap();
        g.insert_concept(
            Concept {
                id: b,
                session_id: sid("matrix"),
                content: "y".into(),
                canonical_key: "y".into(),
                concept_type: ConceptType::Entity,
                origin_interaction: i,
                origin_agent: agent(),
                created_at: at,
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding: None,
                chunk_group_id: None,
            },
            i,
        )
        .unwrap();
        g.upsert_edge(Edge {
            id: NodeId::new(),
            session_id: sid("matrix"),
            source: a,
            target: b,
            edge_type: EdgeType::Semantic,
            weight: 0.9,
            reinforcements: 1,
            created_at: at,
            last_reinforced: at,
        })
        .unwrap();
        g.assert_invariants().unwrap();
    }
    /// P7 MAJOR-1 regression — mirrors sync
    /// `derive::derive_collapses_contents_sharing_a_canonical_key` through
    /// `hybrid::derive`. Without a VECTOR_SEARCH capability, every unmatched
    /// concept is byte-identical to canonical derive: two distinct contents
    /// that collapse onto one canonical key must yield ONE node — the second
    /// content resolves `Matched` to the first's just-created node — never an
    /// `insert_concept` UNIQUE (session_id, canonical_key) hard error + partial
    /// write.
    #[tokio::test]
    async fn hybrid_collapses_contents_sharing_a_canonical_key() {
        let sess = "hybrid-collapse";
        let (graph, iid) = graph_with_interaction(sess, 1, 0, "user signs up");
        let out = derive(
            graph.clone(),
            &SpyStore::without_vector(),
            &RecordingEmbedder::new(),
            &contract("fixture", 1024),
            iid,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("schema user", ConceptType::Entity),
                ("auth middleware", ConceptType::Logic),
            ],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();

        assert_eq!(out.created.len(), 2); // "user schema" + "auth middleware"
        assert_eq!(out.matched.len(), 1); // "schema user" -> the first node
        assert_eq!(out.matched[0], out.created[0]);
        let g = graph.read();
        assert_eq!(g.node_count(), 3); // interaction + 2 concepts
                                       // No self-loop from the key collision: the collapsed node is the SAME
                                       // node as the first concept, so `call_nodes` held it once.
        assert!(
            g.edge_between(out.created[0], out.created[0], EdgeType::CoOccurrence)
                .is_none(),
            "no self-loop from a key collision"
        );
        assert!(g
            .edge_between(out.created[0], out.created[1], EdgeType::CoOccurrence)
            .is_some());
        g.assert_invariants().unwrap();
    }

    /// P7 MAJOR-1 regression (merge path): two distinct contents that collapse
    /// onto one canonical key BOTH hybrid-merge against the same target. The
    /// first creates its node + Semantic edge; the second must collapse to that
    /// node (recorded in `matched`) rather than creating a canonical-key
    /// duplicate or erroring.
    #[tokio::test]
    async fn hybrid_collapses_shared_key_under_merge() {
        let sess = "hybrid-collapse-merge";
        // Interaction 1 seeds a pre-existing concept C ("billing") with a key
        // distinct from "schema user" so the colliding pair stays Unmatched.
        let (graph, i1) = graph_with_interaction(sess, 1, 0, "invoicing a customer");
        let c1 = {
            let out = derive(
                graph.clone(),
                &SpyStore::with_vector(Vec::new()),
                &RecordingEmbedder::new(),
                &contract("fixture", 1024),
                i1,
                &agent(),
                &[("billing flow", ConceptType::Entity)],
                &ParentOf::none(),
                10,
                SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            )
            .await
            .unwrap();
            out.created[0]
        };
        let mut second = interaction(2, Some(i1), 60, "designing the data model");
        second.session_id = sid(sess);
        let i2 = second.id;
        graph.write().insert_interaction(second).unwrap();

        // Both colliding contents embed, and the store returns C1 at 0.9 for
        // each — so both resolve HybridMerge { target: c1 }.
        let out = derive(
            graph.clone(),
            &SpyStore::with_vector(vec![hit(c1, 0.9)]),
            &RecordingEmbedder::new(),
            &contract("fixture", 1024),
            i2,
            &agent(),
            &[
                ("user schema", ConceptType::Entity),
                ("schema user", ConceptType::Entity),
            ],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();

        // One created node; the second content collapses to it (matched),
        // rather than a canonical-key duplicate or an insert error.
        assert_eq!(out.created.len(), 1);
        assert_eq!(out.matched, vec![out.created[0]]);
        assert_eq!(out.semantic_merged, vec![c1]);
        let g = graph.read();
        let n1 = out.created[0];
        assert_eq!(g.node_count(), 4); // i1, i2, c1, n1
                                       // Exactly one Semantic merge edge (c1 <-> n1, direction by UUID order)
                                       // — the collapse wrote no second edge and no duplicate node.
        assert!(g
            .edge_between(c1, n1, EdgeType::Semantic)
            .or_else(|| g.edge_between(n1, c1, EdgeType::Semantic))
            .is_some());
        assert_eq!(
            g.edges()
                .filter(|e| e.edge_type == EdgeType::Semantic)
                .count(),
            1
        );
        g.assert_invariants().unwrap();
    }

    /// P7 MINOR-2: when the store returns a candidate that is NOT a Concept
    /// (a bogus/refused merge target), the concept must degrade to a TRUE
    /// keyword-only node — `embedding: None`, no Semantic edge — never persist
    /// a vector for a merge it refused to make.
    #[tokio::test]
    async fn hybrid_refused_merge_target_is_keyword_only() {
        let sess = "hybrid-refused";
        let (graph, iid) = graph_with_interaction(sess, 1, 0, "auth flow");
        // The store hands back a hit pointing at the INTERACTION node (not a
        // Concept) at/above threshold — the merge must be refused.
        let out = derive(
            graph.clone(),
            &SpyStore::with_vector(vec![hit(iid, 0.9)]),
            &RecordingEmbedder::new(),
            &contract("fixture", 1024),
            iid,
            &agent(),
            &[("create account", ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();

        assert_eq!(out.created.len(), 1);
        assert!(out.matched.is_empty());
        assert!(out.semantic_merged.is_empty());
        let c = out.created[0];
        let g = graph.read();
        match g.node(c) {
            Some(Node::Concept(con)) => assert!(
                con.embedding.is_none(),
                "refused-merge concept must be keyword-only (embedding: None)"
            ),
            _ => unreachable!(),
        }
        assert!(g.edge_between(iid, c, EdgeType::Semantic).is_none());
        assert!(g.edge_between(c, iid, EdgeType::Semantic).is_none());
        assert!(g.edge_between(iid, c, EdgeType::Derives).is_some());
        g.assert_invariants().unwrap();
    }
}
