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
//! shape: every hit carries `node_id` (added 2026-10-06, issue #9, beside the
//! originally pinned fields), `content`, `concept_type`, `status` (absent only
//! for `None`), `score`, `blast_radius` (when present),
//! `included_in_context`, `truncated_in_context` (#130, only when true),
//! and `annotations` as zero or more `{kind, text}` pairs;
//! `response_annotations` carries the response-global explanations.
//! Producer order is preserved within both arrays.
//!
//! Annotation kinds are derived from typed producers, never from text
//! patterns: `load_bearing` from a Canonical blast warning, `conflict` from
//! `HotListPayload::Conflict`, `hot` from HighRisk/Drift/Stale,
//! `contradiction` from `HotListPayload::Contradiction`, `contradicts` on
//! the contradicting concept pulled in beside it (#19(c)), `superseded` on a
//! hit whose latest demotion was a supersession (#20, history, not forced),
//! `reservation` from an active reservation, `traversal` from the
//! structural-dispatch explanation, and `vector_degraded` from a query
//! embedding failure. The two response-global kinds are never attached to a
//! hit and never duplicated across hits.

use serde::{Deserialize, Serialize};

use crate::types::{CanonizationStatus, ConceptType, NodeId, RecallHit, RecallResult};

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
    /// A layer of a layered recall was unavailable, so its hits are missing
    /// (#33) — response-global, one per unavailable layer.
    LayerUnavailable,
    /// `HotListPayload::Contradiction`: a newer assertion contradicts this
    /// hit (#19(c); any status since #179) — hit-owned.
    Contradiction,
    /// This hit contradicts a contradicted hit of the same result, and was
    /// pulled in beside it (#19(c)) — hit-owned.
    Contradicts,
    /// History (#20): this hit's latest demotion was a supersession and it is
    /// not Canonical again. Never forces the hit into the result — hit-owned.
    Superseded,
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
    /// The hit's node id (issue #9, an additive field beside the pinned H3
    /// shape): the rendered context block now carries the short-form id, so
    /// the portal's byte-for-byte parity check, which deserializes this
    /// payload and re-renders it, needs the id here; portal cards get the
    /// same durable handle the text surface has.
    pub(crate) node_id: NodeId,
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
    /// #129: the repository of a repository-scoped path Resource. Additive
    /// on the wire: serialized only when present, so every unscoped payload
    /// is byte-identical to before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) repo_scope: Option<String>,
    /// True exactly for the longest ranked prefix of hits whose complete
    /// rendered blocks appear in `context`. Hits after the token-budget cut
    /// remain in `hits` with `false`.
    pub(crate) included_in_context: bool,
    /// True for the one hit whose block was too long for the budget on its
    /// own and appears in `context` truncated, followed by the
    /// `[truncated: …; lambo_inspect <id> …]` marker (#130). Only ever the
    /// first hit, and then no hit is `included_in_context`. The wire
    /// projection of [`DetailedRecall::truncation`], written only by
    /// [`DetailedRecall::mark_context`]. Additive on the wire: serialized
    /// only when true, so every payload whose first block fits is
    /// byte-identical to before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) truncated_in_context: bool,
    /// The hit's typed warning lines, in producer order. Token exclusion
    /// discards the hit's complete block, never these annotations.
    pub(crate) annotations: Vec<Annotation>,
    /// Set when the hit is in the result **only** as a contradiction
    /// companion (#19(c), #179): the ids of the shown hits it contradicts,
    /// id-ascending. Such a hit holds no `top_k` slot, so a result can have
    /// up to one more hit than `top_k` per contradicted hit; its
    /// `contradicts` annotations say why. A contradicting concept that
    /// ranked into the result on its own carries the annotations but not
    /// this field. Additive on the wire: serialized only when non-empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) companion_of: Vec<NodeId>,
    /// The shown hits this hit's `contradicts` lines name, id-ascending,
    /// whether or not it is a forced companion: what a layered merge needs
    /// to show a companion only beside a hit it contradicts. Internal only.
    #[serde(skip)]
    pub(crate) contradicts: Vec<NodeId>,
}

impl DetailedHit {
    /// The presentation view of a pipeline hit, with the full graph-snapshot
    /// status the public [`RecallHit::is_canonical`] collapses to a bool.
    pub(crate) fn new(hit: &RecallHit, status: Option<CanonizationStatus>) -> Self {
        Self {
            node_id: hit.node_id,
            content: hit.content.clone(),
            concept_type: hit.concept_type,
            status,
            score: hit.score,
            blast_radius: hit.blast_radius,
            repo_scope: hit.repo_scope.clone(),
            included_in_context: false,
            truncated_in_context: false,
            annotations: Vec::new(),
            companion_of: Vec::new(),
            contradicts: Vec::new(),
        }
    }
}

/// The H3 detailed recall: the pipeline's flattened shape plus the
/// presentation model, produced by ONE execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct DetailedRecall {
    /// Resolved normalized subject captured under the pipeline graph guard.
    /// Also present for a structural blend whose anchor has no dependents.
    #[serde(skip)]
    pub(crate) anchor: Option<String>,
    /// The public flattened hits (node_id, is_canonical) — the existing
    /// pipeline shape, projected into [`crate::types::RecallResult`] by the
    /// daemon. Internal only; never serialized.
    #[serde(skip)]
    pub(crate) hits: Vec<RecallHit>,

    /// The rendered context block, as `format::fit_blocks` cut it: the
    /// longest ranked prefix of whole blocks joined with blank lines, or the
    /// first block truncated with its marker (#130), or `""`
    /// (pre-⚑-header). Internal only; the wire's `context` is the full
    /// `lambo recall` string rendered over this presentation model.
    #[serde(skip)]
    pub(crate) context: String,
    /// How the first hit was truncated into `context`, when it was (#130).
    /// **The single source of truth** for truncation: it is set only by
    /// [`DetailedRecall::mark_context`], which also sets the per-hit
    /// `included_in_context` / `truncated_in_context` wire projections from
    /// the same cut. When `Some`, `detailed[0]` is the truncated hit and no
    /// hit is `included_in_context`. Internal only.
    ///
    /// #33 PR 2 note: a layered merge re-runs `fit_blocks` over the merged
    /// list and must call `mark_context` on the merged hits, so the flags a
    /// per-session recall set are reset rather than carried over.
    #[serde(skip)]
    pub(crate) truncation: Option<crate::recall::format::Truncation>,
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
    /// Whether phase-3 assembly ranked this recall in cold mode (#79): a
    /// fresh query-backed concept the daemon had not yet scored would be
    /// shown, so every member scored `w_query × q` and the daemon share was
    /// withheld. `false` for the ordinary blend, a dispatched structural
    /// query and the warning-only refusals.
    ///
    /// **Internal only, like [`Self::legs`]:** the H3 wire shape is pinned,
    /// so the serve call ledger reads this in Rust and writes it as the
    /// recall line's `cold_start` key (an additive ledger field, `v` stays
    /// 1). It is what makes the cold window observable in production.
    #[serde(skip)]
    pub(crate) cold_start: bool,
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
            anchor: None,
            hits: Vec::new(),
            context: String::new(),
            truncation: None,
            warnings: vec![warning],
            legs: Default::default(),
            cold_start: false,
            detailed: Vec::new(),
            response_annotations: Vec::new(),
        }
    }
}

impl DetailedRecall {
    /// Record a budget cut on the presentation hits: hits `0..whole` are
    /// `included_in_context`, hit 0 is `truncated_in_context` exactly when
    /// the cut truncated it, every other flag is cleared, and
    /// [`Self::truncation`] is set. The one place either flag is written.
    pub(crate) fn mark_context(
        detailed: &mut [DetailedHit],
        fitted: &crate::recall::format::FittedContext,
    ) -> Option<crate::recall::format::Truncation> {
        debug_assert!(fitted.truncation.is_none() || fitted.whole == 0);
        for (i, d) in detailed.iter_mut().enumerate() {
            d.included_in_context = i < fitted.whole;
            d.truncated_in_context = i == 0 && fitted.truncation.is_some();
        }
        fitted.truncation
    }

    /// The truncated first hit and how it was truncated, if any. Reads
    /// [`Self::truncation`] only; the per-hit flag is its wire projection.
    pub(crate) fn truncated_hit(
        &self,
    ) -> Option<(&DetailedHit, crate::recall::format::Truncation)> {
        let form = self.truncation?;
        let hit = self.detailed.first()?;
        debug_assert!(hit.truncated_in_context);
        debug_assert!(self.detailed.iter().all(|h| !h.included_in_context));
        Some((hit, form))
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
