//! Layered recall (#33 PR 2): merge the ranked lists of N sessions (the
//! caller's own session first, then its layers, nearest tier first) into
//! one answer under one token budget. `merge` is crate-internal; the
//! library entry point is [`Memory::recall_layered`](crate::Memory::recall_layered).
//!
//! Design of record: the #33 layered-recall design, sections 1.1 and 2.
//! Every shape here is N-shaped: `merge` takes a slice of tier lists with
//! a weight and a floor each, and dedupe precedence is the slice order. v1
//! serves one layer (enforced by serve config, not here).
//!
//! ## The rule
//!
//! 1. **Admission.** Every hit of a list without a floor is a candidate (the
//!    caller's own session). A hit of a list with a floor is a candidate
//!    only with keyword evidence (a BM25 leg score) or vector evidence at
//!    cosine `>=` the floor, read from the session's phase-1 leg provenance
//!    (`DetailedRecall::legs`). A hit that arrived only through traversal
//!    expansion has no leg score and is not admitted from a layer. A
//!    **forced** hit (a Conflict, Hot or Reservation annotation) is always
//!    admitted.
//! 2. **Rank key.** Each list is already in its session's own final order.
//!    A hit at 1-based position `r` of a list with weight `w` gets the key
//!    `w / r`. `r` is the position in the session's own list, before any
//!    floor: a layer whose top hits fail its floor has shown weak relevance,
//!    and its later hits do not move up for it.
//! 3. **Dedupe by tier order.** Two candidates from different lists are the
//!    same memory when their canonical keys are equal, or both carry holder
//!    vectors with cosine `>=` the dedupe threshold. The copy from the
//!    nearer tier (earlier in the slice) is kept; it takes the larger of the
//!    two keys, so a memory is never lost because its nearer copy ranked
//!    low, and it is forced if either copy was. The dropped copy leaves the
//!    hits and the context, its warning lines stay in `warnings`, and the
//!    kept block's meta names it (`also <id> in <session>`).
//! 4. **Order and cut.** Candidates sort by key descending, then tier
//!    ascending (ties go to the nearer tier), then position ascending: a
//!    total order, so the merge is deterministic. The first `top_k`
//!    non-forced candidates are emitted, plus every forced one wherever it
//!    sorts.
//! 5. **One budget.** Every emitted hit's block is re-rendered (a layer
//!    hit's meta gains `from <session>`, and its truncation marker gets
//!    `(scope <session>)`), `format::fit_blocks` runs once over the merged
//!    list, and `DetailedRecall::mark_context` resets every per-session
//!    context flag
//!    from that one cut. Each session's own `context` is ignored.
//! 6. **Degradation.** A list whose recall is unavailable contributes no
//!    hits; the answer gets one `layer_unavailable` response annotation and
//!    one warning per such list, naming it and the sessions the results
//!    came from. A layer never fails the merge.
//!
//! Warnings from a layer, and its response annotations, carry the prefix
//! `[<session>] `. The caller's own session's warnings are unchanged, so a
//! merge whose layers all degrade answers exactly the caller's own recall
//! plus the degradation lines.
//!
//! ## Calibration
//!
//! [`DEFAULT_LAYER_WEIGHT`] and [`DEFAULT_LAYER_MIN_SIMILARITY`] are
//! **uncalibrated until #87**: they come from the design's reasoning (a 0.5
//! weight puts a layer's first hit beside the caller's second, and 0.40 is
//! just above the lowest durable true BGE-M3 hit seen, 0.3991), not from a
//! measured query set.

use std::collections::HashMap;

use crate::recall::candidates::LegScores;
use crate::recall::detail::{Annotation, AnnotationKind, DetailedHit, DetailedRecall};
use crate::recall::format::{self, ContextBlock};
use crate::types::{NodeId, RecallHit, SessionId};

/// The default merge weight of a layer: its hit at position `r` ranks as
/// `0.5 / r`, against `1 / r` for the caller's own session.
/// **Uncalibrated until #87.**
pub const DEFAULT_LAYER_WEIGHT: f64 = 0.5;

/// The default floor of a layer: a layer hit without keyword evidence is
/// admitted only at vector cosine `>=` this. **Uncalibrated until #87.**
pub const DEFAULT_LAYER_MIN_SIMILARITY: f64 = 0.40;

/// The merge weight of the caller's own session.
pub(crate) const PRIMARY_WEIGHT: f64 = 1.0;

/// What dedupe compares for one hit, read from its session's graph after
/// the recall. `None` fields mean "unknown" and never match.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HitIdentity {
    pub(crate) canonical_key: Option<String>,
    pub(crate) embedding: Option<Vec<f32>>,
}

/// One session's recall, ready to merge.
#[derive(Clone, Debug)]
pub(crate) struct TierRecall {
    pub(crate) recall: DetailedRecall,
    /// Parallel to `recall.hits`.
    pub(crate) identities: Vec<HitIdentity>,
}

/// One ranked list in tier order: the caller's own session first, then its
/// layers, nearest tier first.
#[derive(Clone, Debug)]
pub(crate) struct TierList {
    pub(crate) session: SessionId,
    /// `w` in the `w / rank` key. Finite and non-negative (checked by the
    /// caller).
    pub(crate) weight: f64,
    /// The per-layer floor (rule 1); `None` for the caller's own session.
    pub(crate) floor: Option<f64>,
    /// `None` when the session's recall was unavailable (rule 6).
    pub(crate) recall: Option<TierRecall>,
}

/// The merge's knobs that are not per list.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MergeParams {
    /// Bound on merged non-forced hits.
    pub(crate) top_k: usize,
    /// The one token budget for the merged context.
    pub(crate) max_tokens: usize,
    /// Cosine at or above which two holder vectors are the same memory (the
    /// caller's `semantic_match_threshold`).
    pub(crate) dedupe_threshold: f64,
}

/// Another tier's copy of a hit, dropped by dedupe in favour of the nearer
/// tier's copy.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerCopy {
    /// The session the dropped copy is in.
    pub session: SessionId,
    /// The dropped copy's node id in that session.
    pub node_id: NodeId,
}

/// One hit of a layered recall.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct LayeredHit {
    /// The session this hit belongs to: the caller's own, or a layer. Its
    /// `node_id` (and short id) resolve in that session only.
    pub session: SessionId,
    /// The hit as its own session ranked it. `score` is that session's
    /// score: scores are not comparable across sessions, and the merged
    /// order is not monotone in `score`.
    pub hit: RecallHit,
    /// Copies of the same memory in farther tiers, dropped by dedupe.
    pub also: Vec<LayerCopy>,
    /// Whether the hit's whole block is in [`LayeredRecall::context`].
    pub included_in_context: bool,
    /// Whether the hit's block is in the context truncated, with a marker
    /// (#130). Only ever the first hit.
    pub truncated_in_context: bool,
}

/// The answer of [`Memory::recall_layered`](crate::Memory::recall_layered):
/// the merged hits of the caller's session and its layers, in merged order,
/// each naming its session, with one context under one budget.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct LayeredRecall {
    /// Merged hits, in merged order.
    pub hits: Vec<LayeredHit>,
    /// The rendered context block for the merged hits, within `max_tokens`.
    pub context: String,
    /// Warning lines: the caller's session's, then each layer's prefixed
    /// with `[<session>] `, then one line per unavailable layer.
    pub warnings: Vec<String>,
    /// The layers whose recall was unavailable, in tier order.
    pub unavailable_layers: Vec<SessionId>,
    /// The merged presentation model (hits, annotations, context flags,
    /// legs), for the serve wire and the call ledger (#33 PR 3). Its
    /// `hits` and `detailed` are parallel to [`Self::hits`].
    #[allow(dead_code)] // read by the serve wire and ledger in #33 PR 3
    pub(crate) detail: DetailedRecall,
}

impl LayeredRecall {
    /// An unlayered recall's answer, unchanged: every hit is `session`'s,
    /// and `context` and `warnings` are the recall's own.
    pub(crate) fn single(session: &SessionId, detail: DetailedRecall) -> Self {
        let hits = detail
            .hits
            .iter()
            .zip(&detail.detailed)
            .map(|(hit, d)| LayeredHit {
                session: session.clone(),
                hit: hit.clone(),
                also: Vec::new(),
                included_in_context: d.included_in_context,
                truncated_in_context: d.truncated_in_context,
            })
            .collect();
        Self {
            hits,
            context: detail.context.clone(),
            warnings: detail.warnings.clone(),
            unavailable_layers: Vec::new(),
            detail,
        }
    }
}

/// A hit is forced (kept beyond `top_k`, and past a floor) when it carries
/// a Conflict, Hot or Reservation annotation (design 2.2, matching
/// assembly's own forced members).
fn is_forced(d: &DetailedHit) -> bool {
    d.annotations.iter().any(|a| {
        matches!(
            a.kind,
            AnnotationKind::Conflict | AnnotationKind::Hot | AnnotationKind::Reservation
        )
    })
}

/// Rule 1's evidence test for a layer hit.
fn has_evidence(legs: Option<&LegScores>, floor: f64) -> bool {
    legs.is_some_and(|l| l.keyword.is_some() || l.vector.is_some_and(|v| v >= floor))
}

/// Rule 3's sameness test.
fn same_memory(a: &HitIdentity, b: &HitIdentity, threshold: f64) -> bool {
    if let (Some(ka), Some(kb)) = (&a.canonical_key, &b.canonical_key)
        && ka == kb
    {
        return true;
    }
    match (&a.embedding, &b.embedding) {
        (Some(va), Some(vb)) if va.len() == vb.len() && !va.is_empty() => {
            f64::from(crate::embed::cosine(va, vb)) >= threshold
        }
        _ => false,
    }
}

/// The `[<session>] ` prefix on a layer's warning lines.
fn prefixed(session: &SessionId, line: &str) -> String {
    format!("[{session}] {line}")
}

/// A candidate of the merge.
struct Candidate {
    tier: usize,
    /// 0-based position in its session's own list.
    pos: usize,
    key: f64,
    forced: bool,
    /// Farther-tier copies merged into this one: `(tier, pos)`.
    also: Vec<(usize, usize)>,
}

/// Merge `lists` (tier order, the caller's own session first) into one
/// layered recall. See the module docs for the rule.
pub(crate) fn merge<F>(lists: &[TierList], params: MergeParams, token_fn: F) -> LayeredRecall
where
    F: Fn(&str) -> usize,
{
    // Rules 1 and 2: admission and keys.
    let mut admitted: Vec<Candidate> = Vec::new();
    for (tier, list) in lists.iter().enumerate() {
        let Some(t) = &list.recall else { continue };
        debug_assert_eq!(t.recall.hits.len(), t.recall.detailed.len());
        debug_assert_eq!(t.recall.hits.len(), t.identities.len());
        let n = t.recall.hits.len().min(t.recall.detailed.len());
        for pos in 0..n {
            let hit = &t.recall.hits[pos];
            let forced = is_forced(&t.recall.detailed[pos]);
            let admit = match list.floor {
                None => true,
                Some(floor) => forced || has_evidence(t.recall.legs.get(&hit.node_id), floor),
            };
            if admit {
                admitted.push(Candidate {
                    tier,
                    pos,
                    key: list.weight / (pos as f64 + 1.0),
                    forced,
                    also: Vec::new(),
                });
            }
        }
    }

    // Rule 3: dedupe in tier order, nearer copies first. `admitted` is
    // already in (tier, pos) order.
    let identity = |c: &Candidate| -> Option<&HitIdentity> {
        lists[c.tier]
            .recall
            .as_ref()
            .and_then(|t| t.identities.get(c.pos))
    };
    let mut kept: Vec<Candidate> = Vec::new();
    for cand in admitted {
        let dup = identity(&cand).and_then(|ci| {
            kept.iter().position(|k| {
                k.tier < cand.tier
                    && identity(k).is_some_and(|ki| same_memory(ki, ci, params.dedupe_threshold))
            })
        });
        match dup {
            Some(i) => {
                let k = &mut kept[i];
                if cand.key > k.key {
                    k.key = cand.key;
                }
                k.forced |= cand.forced;
                k.also.push((cand.tier, cand.pos));
            }
            None => kept.push(cand),
        }
    }

    // Rule 4: total order, then the top_k cut with forced members kept.
    kept.sort_by(|a, b| {
        b.key
            .total_cmp(&a.key)
            .then(a.tier.cmp(&b.tier))
            .then(a.pos.cmp(&b.pos))
    });
    let mut slots = 0usize;
    let emitted: Vec<Candidate> = kept
        .into_iter()
        .filter(|c| {
            if c.forced {
                return true;
            }
            if slots >= params.top_k {
                return false;
            }
            slots += 1;
            true
        })
        .collect();

    // Every (tier, pos) whose copy reached the answer, itself or as `also`.
    let mut contributed: Vec<Vec<bool>> = lists
        .iter()
        .map(|l| vec![false; l.recall.as_ref().map_or(0, |t| t.recall.hits.len())])
        .collect();
    for c in &emitted {
        contributed[c.tier][c.pos] = true;
        for &(t, p) in &c.also {
            contributed[t][p] = true;
        }
    }

    // Rule 5: re-render and cut once.
    let mut hits = Vec::with_capacity(emitted.len());
    let mut detailed = Vec::with_capacity(emitted.len());
    let mut blocks = Vec::with_capacity(emitted.len());
    let mut sessions = Vec::with_capacity(emitted.len());
    let mut alsos = Vec::with_capacity(emitted.len());
    let mut legs = HashMap::new();
    for c in &emitted {
        let list = &lists[c.tier];
        let t = list
            .recall
            .as_ref()
            .expect("emitted from an available tier");
        let hit = &t.recall.hits[c.pos];
        let mut d = t.recall.detailed[c.pos].clone();
        d.included_in_context = false;
        d.truncated_in_context = false;
        let also: Vec<LayerCopy> = c
            .also
            .iter()
            .map(|&(tier, pos)| LayerCopy {
                session: lists[tier].session.clone(),
                node_id: lists[tier]
                    .recall
                    .as_ref()
                    .expect("merged from an available tier")
                    .recall
                    .hits[pos]
                    .node_id,
            })
            .collect();
        let mut meta = Vec::new();
        if c.tier > 0 {
            meta.push(format!("from {}", list.session));
        }
        meta.extend(
            also.iter()
                .map(|a| format!("also {} in {}", format::short_id(a.node_id), a.session)),
        );
        let lines: Vec<String> = d.annotations.iter().map(|a| a.text.clone()).collect();
        let scope = (c.tier > 0).then(|| list.session.to_string());
        blocks.push(block_with_meta(hit, &lines, &meta, scope));
        if let Some(l) = t.recall.legs.get(&hit.node_id) {
            legs.insert(hit.node_id, *l);
        }
        hits.push(hit.clone());
        detailed.push(d);
        sessions.push(list.session.clone());
        alsos.push(also);
    }
    let fitted = format::fit_blocks(&blocks, params.max_tokens, token_fn);
    let truncation = DetailedRecall::mark_context(&mut detailed, &fitted);

    // Warnings and response annotations, tier by tier (rule 6 and the
    // prefix rule). A hit's lines leave `warnings` only when no copy of it
    // reached the answer.
    let mut warnings = Vec::new();
    let mut response_annotations = Vec::new();
    let mut unavailable = Vec::new();
    for (tier, list) in lists.iter().enumerate() {
        let Some(t) = &list.recall else {
            unavailable.push(list.session.clone());
            continue;
        };
        let mut lines = t.recall.warnings.clone();
        for (pos, d) in t.recall.detailed.iter().enumerate() {
            if contributed[tier].get(pos).copied().unwrap_or(false) {
                continue;
            }
            for a in &d.annotations {
                if let Some(i) = lines.iter().position(|l| *l == a.text) {
                    lines.remove(i);
                }
            }
        }
        if tier == 0 {
            warnings.extend(lines);
            response_annotations.extend(t.recall.response_annotations.iter().cloned());
        } else {
            warnings.extend(lines.iter().map(|l| prefixed(&list.session, l)));
            response_annotations.extend(
                t.recall
                    .response_annotations
                    .iter()
                    .map(|a| Annotation::new(a.kind, prefixed(&list.session, &a.text))),
            );
        }
    }
    if !unavailable.is_empty() {
        let available: Vec<String> = lists
            .iter()
            .filter(|l| l.recall.is_some())
            .map(|l| l.session.to_string())
            .collect();
        let from = match available.len() {
            0 => "no session".to_string(),
            1 => format!("{} only", available[0]),
            _ => available.join(", "),
        };
        for session in &unavailable {
            let text = format!("recall: layer {session} is unavailable; results are from {from}");
            response_annotations.push(Annotation::new(
                AnnotationKind::LayerUnavailable,
                text.clone(),
            ));
            warnings.push(text);
        }
    }

    let detail = DetailedRecall {
        hits,
        context: fitted.context,
        truncation,
        warnings,
        legs,
        cold_start: lists
            .first()
            .and_then(|l| l.recall.as_ref())
            .is_some_and(|t| t.recall.cold_start),
        detailed,
        response_annotations,
    };
    let hits = detail
        .hits
        .iter()
        .zip(&detail.detailed)
        .zip(sessions)
        .zip(alsos)
        .map(|(((hit, d), session), also)| LayeredHit {
            session,
            hit: hit.clone(),
            also,
            included_in_context: d.included_in_context,
            truncated_in_context: d.truncated_in_context,
        })
        .collect();
    LayeredRecall {
        hits,
        context: detail.context.clone(),
        warnings: detail.warnings.clone(),
        unavailable_layers: unavailable,
        detail,
    }
}

/// [`ContextBlock::for_hit`], with `meta` items appended inside the label's
/// closing parenthesis (`(score 0.42, id 1a2b3c4d, from general)`) and the
/// block's `scope` set. With no `meta` and no `scope` it is exactly
/// `for_hit`'s block, so the caller's own hits render as an unlayered
/// recall renders them.
pub(crate) fn block_with_meta(
    hit: &RecallHit,
    warnings: &[String],
    meta: &[String],
    scope: Option<String>,
) -> ContextBlock {
    let mut block = ContextBlock::for_hit(hit, warnings);
    if !meta.is_empty() && block.label_end > 0 && block.text[..block.label_end].ends_with(')') {
        let extra = format!(", {}", meta.join(", "));
        block.text.insert_str(block.label_end - 1, &extra);
        block.label_end += extra.len();
    }
    block.scope = scope;
    block
}

#[cfg(test)]
mod tests;
