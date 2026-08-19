//! H3 presentation model (H3 — structured recall results beside the verbatim
//! block).
//!
//! The public pipeline ([`crate::types::RecallResult`]) flattens warning
//! provenance: each hit's warning lines and the traversal explanation lose
//! their typed owners the moment `assemble`/`dispatch` extend the flat
//! `warnings` vector. This module is the `pub(crate)` seam that captures the
//! same information **where it still exists** — status from the graph snapshot
//! a hit was assembled from, and each typed warning alongside the line that
//! renders it — so the HTTP payload can present cards and annotations without
//! parsing `context` text or re-reading the store.
//!
//! The wire contract (pinned in the H3 spec section of
//! `dev-diary/notes/hardening-tasks.md`) is exactly this module's `Serialize`
//! shape: every hit carries `content`, `concept_type`, `status` (absent only
//! for `None`), `score`, `blast_radius` (when present),
//! `included_in_context`, and `annotations` as zero or more `{kind, text}`
//! pairs; `response_annotations` carries the response-global explanations.
//! Producer order is preserved within both arrays.
//!
//! Annotation kinds are derived from typed producers, never from text
//! patterns: `load_bearing` from a Canonical blast warning, `conflict` from
//! `HotListPayload::Conflict`, `hot` from HighRisk/Drift/Stale,
//! `reservation` from an active reservation, `traversal` from the
//! structural-dispatch explanation, and `vector_degraded` from a query
//! embedding failure. The two response-global kinds are never attached to a
//! hit and never duplicated across hits.

use serde::{Deserialize, Serialize};

use crate::types::{CanonizationStatus, ConceptType, RecallHit, RecallResult};

/// The pinned H3 annotation kinds. Wire values are stable (`snake_case`);
/// clients treat kinds differently instead of pattern-matching on text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnnotationKind {
    /// The Canonical blast warning (spec §13 load-bearing pillar) — hit-owned.
    LoadBearing,
    /// `HotListPayload::Conflict` — hit-owned.
    Conflict,
    /// A non-conflict hot condition: HighRisk / Drift / Stale — hit-owned.
    Hot,
    /// An active reservation (soft lock) — hit-owned.
    Reservation,
    /// The structural-dispatch explanation — response-global.
    Traversal,
    /// Query embedding failed; the vector leg was skipped — response-global.
    VectorDegraded,
}

/// One typed warning: its kind and the exact line that renders it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Annotation {
    pub(crate) kind: AnnotationKind,
    pub(crate) text: String,
}

impl Annotation {
    pub(crate) fn new(kind: AnnotationKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// One ranked hit in the presentation model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct DetailedHit {
    pub(crate) content: String,
    pub(crate) concept_type: Option<ConceptType>,
    /// The concept's full status from the SAME graph snapshot the hit was
    /// assembled from. Serialized as absent only for status `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<CanonizationStatus>,
    pub(crate) score: f64,
    /// Serialized only when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blast_radius: Option<u64>,
    /// True exactly for the longest ranked prefix of hits whose complete
    /// rendered blocks appear in `context`. Hits after the token-budget cut
    /// remain in `hits` with `false`.
    pub(crate) included_in_context: bool,
    /// The hit's typed warning lines, in producer order. Token exclusion
    /// discards the hit's complete block, never these annotations.
    pub(crate) annotations: Vec<Annotation>,
}

impl DetailedHit {
    /// The presentation view of a pipeline hit, with the full graph-snapshot
    /// status the public [`RecallHit::is_canonical`] collapses to a bool.
    pub(crate) fn new(hit: &RecallHit, status: Option<CanonizationStatus>) -> Self {
        Self {
            content: hit.content.clone(),
            concept_type: hit.concept_type,
            status,
            score: hit.score,
            blast_radius: hit.blast_radius,
            included_in_context: false,
            annotations: Vec::new(),
        }
    }
}

/// The H3 detailed recall: the pipeline's flattened shape plus the
/// presentation model, produced by ONE execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct DetailedRecall {
    /// The public flattened hits (node_id, is_canonical) — the existing
    /// pipeline shape, projected into [`crate::types::RecallResult`] by the
    /// daemon. Internal only; never serialized.
    #[serde(skip)]
    pub(crate) hits: Vec<RecallHit>,

    /// The rendered context block: whole blocks in score order, joined with
    /// blank lines (pre-⚑-header). Internal only; the wire's `context` is the
    /// full `lambo recall` string rendered over this presentation model.
    #[serde(skip)]
    pub(crate) context: String,
    /// The flattened warning lines in producer order (the pipeline's
    /// `RecallResult.warnings`). Internal only; the wire carries the typed
    /// counterparts instead.
    #[serde(skip)]
    pub(crate) warnings: Vec<String>,
    /// Phase-1 leg provenance per node (I1): the vector-cosine / BM25 /
    /// recent-floor scores that the max-merge in
    /// [`crate::recall::candidates::candidates_with_legs`] collapses into one
    /// number.
    ///
    /// **These are phase-1 INPUTS, not components of [`DetailedHit::score`].**
    /// `score` is the final ranking score, which phase-3 assembly computes from
    /// the merged phase-1 score together with the daemon's score table and the
    /// configured [`crate::config::RecallWeights`] — so `score` is routinely
    /// larger or smaller than `max(legs)` and the two must never be expected to
    /// agree. What the legs answer is the question the final score cannot:
    /// *which retrieval arm found this, and how strongly*.
    ///
    /// **Internal only, and deliberately `#[serde(skip)]`.** The H3 wire
    /// contract above is pinned; adding a serialized field here would change
    /// the `serve-web` payload, which I1 has no business doing. The serve call
    /// ledger reads this field in Rust and writes its own JSON.
    ///
    /// A hit whose node id is absent from the map was not a phase-1 candidate
    /// — it arrived through phase-2 traversal expansion, which has no leg score
    /// by construction. Empty for a dispatched structural query, which skips
    /// the blend entirely.
    #[serde(skip)]
    pub(crate) legs: crate::recall::candidates::LegProvenance,
    /// The presentation hits, serialized on the wire as `hits`.
    #[serde(rename = "hits")]
    pub(crate) detailed: Vec<DetailedHit>,
    /// Response-global annotations (traversal / vector_degraded), in
    /// producer order. Never hit-owned, never duplicated.
    pub(crate) response_annotations: Vec<Annotation>,
}

impl DetailedRecall {
    /// A warning-only result (no hits, no context) — the pipeline's early
    /// refusal paths (limit validation, session mismatch).
    pub(crate) fn warn_only(warning: String) -> Self {
        Self {
            hits: Vec::new(),
            context: String::new(),
            warnings: vec![warning],
            legs: Default::default(),
            detailed: Vec::new(),
            response_annotations: Vec::new(),
        }
    }
}

/// Project the detailed result back onto the public flattened shape — the
/// daemon's `recall` entry keeps its [`RecallResult`] return type while the
/// H3 seam carries the same execution's presentation model.
impl From<DetailedRecall> for RecallResult {
    fn from(d: DetailedRecall) -> Self {
        RecallResult {
            hits: d.hits,
            context: d.context,
            warnings: d.warnings,
        }
    }
}
