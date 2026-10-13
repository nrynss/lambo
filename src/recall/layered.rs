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
//!    only with evidence: a keyword (BM25) leg score, a vector leg score at
//!    cosine `>=` the floor (both from the session's phase-1 leg
//!    provenance, `DetailedRecall::legs`), or an **anchored** answer: the
//!    session answered a dependency question by traversal from an anchor
//!    the query names (its `traversal` response annotation), so every
//!    dependent it returned is admitted. A hit that arrived only through
//!    phase-2 expansion of a blended recall has no leg score and is not
//!    admitted from a layer. **Nothing else passes a floor**, warnings
//!    included: a busy shared session's hot or reserved concepts must not
//!    crowd the caller's answer on that alone.
//! 2. **Rank key.** Each list is already in its session's own final order.
//!    A hit at 1-based position `r` of a list with weight `w` gets the key
//!    `w / r`. `r` is the position in the session's own list, before any
//!    floor: a layer whose top hits fail its floor has shown weak relevance,
//!    and its later hits do not move up for it.
//! 3. **Order, cut and dedupe.** Candidates are walked by key descending,
//!    then tier ascending (ties go to the nearer tier), then position
//!    ascending: a total order, so the merge is deterministic. A candidate
//!    that is the same memory as an already emitted hit joins that hit's
//!    group instead of being emitted: same memory means an equal canonical
//!    key, or holder vectors at cosine `>=` the dedupe threshold, compared
//!    against **every** member of the group (so dedupe is transitive), and
//!    a group takes at most one copy per tier. The group is shown as its
//!    nearest tier's copy, at the position of its best-ranked copy, so a
//!    memory is never lost because its nearer copy ranked low; the other
//!    copies are named in its meta (`also <id> in <session>`), leave the
//!    hits and the context, and keep their warning lines in `warnings`.
//!    A copy cut by `top_k` before its group was emitted still joins it
//!    when their canonical keys are equal (by key only: vectors are compared
//!    against emitted groups alone, which bounds the work).
//!    Otherwise the candidate is emitted while fewer than `top_k`
//!    non-forced hits are, or when it is **forced** (a Conflict, Hot or
//!    Reservation annotation, design 2.2, or #19(c)'s Contradiction or
//!    Contradicts annotation). Forcing only lifts the `top_k`
//!    cut, never a floor. The set is design 2.2's and is wider than
//!    assembly's own: assembly forces hot-list payloads (Conflict and the
//!    Hot kinds), and reservations only in cold mode.
//! 4. **One budget.** Every emitted hit's block is re-rendered (a layer
//!    hit's meta gains `from <session>`, and its truncation marker gets
//!    `(scope <session>)`), `format::fit_blocks` runs once over the merged
//!    list, and `DetailedRecall::mark_context` resets every per-session
//!    context flag from that one cut. Each session's own `context` is
//!    ignored. A session that answered by traversal gets its headline
//!    recomputed from the dependents of its own that the merged context
//!    renders (T9-R1-N4).
//! 5. **Degradation.** A list whose recall is unavailable contributes no
//!    hits; the answer gets one `layer_unavailable` response annotation and
//!    one warning per such list, naming it and the sessions the results
//!    came from. A layer never fails the merge.
//!
//! Warnings from a layer, and its response annotations, carry the prefix
//! `[<session>] `. The caller's own session's warnings are unchanged, so a
//! merge whose layers all degrade answers exactly the caller's own recall
//! plus the degradation lines.
//!
//! ## Cost
//!
//! Keys are matched through a hash map. Holder-vector comparisons are
//! bounded by [`MAX_VECTOR_DEDUPE_PAIRS`] per merge (a `top_k` near the
//! 2048 limit would otherwise mean millions of 1024-wide dot products);
//! past it, only canonical keys dedupe.
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
    /// `w` in the `w / rank` key. Finite and positive (the library
    /// boundary refuses anything else). A list of weight 0 would rank all
    /// its hits last and still fill free `top_k` slots.
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
    /// The most holder-vector comparisons dedupe makes
    /// ([`MAX_VECTOR_DEDUPE_PAIRS`] outside tests).
    pub(crate) vector_pairs: usize,
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
    /// The merged presentation model (hits, annotations, context flags),
    /// for the serve wire and the call ledger (#33 PR 3). Its `hits` and
    /// `detailed` are parallel to [`Self::hits`]. Its `legs` map holds the
    /// caller's own session's hits only: node ids are unique within a
    /// session, not across sessions (the #33 split replicates identity
    /// nodes), so per-hit legs for every session are in [`Self::hit_legs`].
    #[allow(dead_code)] // read by the serve wire and ledger in #33 PR 3
    pub(crate) detail: DetailedRecall,
    /// Phase-1 leg scores per merged hit, parallel to [`Self::hits`]; `None`
    /// for a hit with no leg score (expansion or traversal).
    #[allow(dead_code)] // read by the call ledger in #33 PR 3
    pub(crate) hit_legs: Vec<Option<LegScores>>,
}

impl LayeredRecall {
    /// An unlayered recall's answer, unchanged: every hit is `session`'s,
    /// and `context` and `warnings` are the recall's own.
    pub(crate) fn single(session: &SessionId, detail: DetailedRecall) -> Self {
        let hit_legs = detail
            .hits
            .iter()
            .map(|h| detail.legs.get(&h.node_id).copied())
            .collect();
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
            hit_legs,
        }
    }
}

/// Most holder-vector comparisons one merge makes before dedupe falls back
/// to canonical keys alone (see the module docs' Cost section). 65,536
/// comparisons of 1024-wide vectors is about 67 M multiply-adds.
pub(crate) const MAX_VECTOR_DEDUPE_PAIRS: usize = 65_536;

/// A hit is forced (kept beyond `top_k`, never past a floor) when it
/// carries a Conflict, Hot or Reservation annotation (design 2.2), or a
/// contradiction annotation on either side (#19(c)).
fn is_forced(d: &DetailedHit) -> bool {
    d.annotations.iter().any(|a| {
        matches!(
            a.kind,
            AnnotationKind::Conflict
                | AnnotationKind::Hot
                | AnnotationKind::Reservation
                | AnnotationKind::Contradiction
                | AnnotationKind::Contradicts
        )
    })
}

/// Whether a session's recall answered a dependency question by traversal
/// (its anchor resolved in that session's graph).
fn is_anchored(recall: &DetailedRecall) -> bool {
    recall
        .response_annotations
        .iter()
        .any(|a| a.kind == AnnotationKind::Traversal)
}

/// Rule 1's leg-evidence test for a layer hit.
fn has_evidence(legs: Option<&LegScores>, floor: f64) -> bool {
    legs.is_some_and(|l| l.keyword.is_some() || l.vector.is_some_and(|v| v >= floor))
}

/// Holder-vector sameness.
fn same_vector(a: &[f32], b: &[f32], threshold: f64) -> bool {
    a.len() == b.len() && !a.is_empty() && f64::from(crate::embed::cosine(a, b)) >= threshold
}

/// The `[<session>] ` prefix on a layer's warning lines.
fn prefixed(session: &SessionId, line: &str) -> String {
    format!("[{session}] {line}")
}

/// A candidate of the merge.
#[derive(Clone, Copy)]
struct Candidate {
    tier: usize,
    /// 0-based position in its session's own list.
    pos: usize,
    key: f64,
    forced: bool,
}

/// An emitted hit and the copies of the same memory merged into it.
struct Group {
    /// The shown copy: the nearest tier's.
    shown: (usize, usize),
    /// The other copies, in the order they joined.
    also: Vec<(usize, usize)>,
}

impl Group {
    fn members(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        std::iter::once(self.shown).chain(self.also.iter().copied())
    }

    fn has_tier(&self, tier: usize) -> bool {
        self.members().any(|(t, _)| t == tier)
    }
}

/// Merge `lists` (tier order, the caller's own session first) into one
/// layered recall. See the module docs for the rule.
pub(crate) fn merge<F>(lists: &[TierList], params: MergeParams, token_fn: F) -> LayeredRecall
where
    F: Fn(&str) -> usize,
{
    let recall_of = |tier: usize| lists[tier].recall.as_ref();
    let identity = |(tier, pos): (usize, usize)| -> Option<&HitIdentity> {
        recall_of(tier).and_then(|t| t.identities.get(pos))
    };

    // Rules 1 and 2: admission and keys.
    let mut candidates: Vec<Candidate> = Vec::new();
    for (tier, list) in lists.iter().enumerate() {
        let Some(t) = &list.recall else { continue };
        debug_assert_eq!(t.recall.hits.len(), t.recall.detailed.len());
        debug_assert_eq!(t.recall.hits.len(), t.identities.len());
        let anchored = is_anchored(&t.recall)
            && (tier == 0
                || recall_of(0)
                    .and_then(|p| p.recall.anchor.as_ref())
                    .is_none_or(|a| t.recall.anchor.as_ref() == Some(a)));
        let n = t.recall.hits.len().min(t.recall.detailed.len());
        for pos in 0..n {
            let hit = &t.recall.hits[pos];
            let admit = match list.floor {
                None => true,
                Some(floor) => anchored || has_evidence(t.recall.legs.get(&hit.node_id), floor),
            };
            if admit {
                candidates.push(Candidate {
                    tier,
                    pos,
                    key: list.weight / (pos as f64 + 1.0),
                    forced: is_forced(&t.recall.detailed[pos]),
                });
            }
        }
    }

    // Rule 3: walk in merged order; dedupe against emitted groups; cut.
    candidates.sort_by(|a, b| {
        b.key
            .total_cmp(&a.key)
            .then(a.tier.cmp(&b.tier))
            .then(a.pos.cmp(&b.pos))
    });
    let mut groups: Vec<Group> = Vec::new();
    let mut by_key: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut cut_by_key: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    let mut vector_budget = params.vector_pairs;
    let mut slots = 0usize;
    for c in candidates {
        let member = (c.tier, c.pos);
        let ci = identity(member);
        let mut found = ci
            .and_then(|ci| ci.canonical_key.as_deref())
            .and_then(|k| by_key.get(k))
            .and_then(|gs| gs.iter().copied().find(|&g| !groups[g].has_tier(c.tier)));
        // #129 (review M2): a repository-scoped path is its exact key. Two
        // repositories' `README.md` embed alike, so a scoped copy joins a
        // group only by an equal key, never by vector, and no copy joins a
        // scoped one by vector.
        let scoped = |i: Option<&HitIdentity>| {
            i.and_then(|i| i.canonical_key.as_deref())
                .is_some_and(crate::graph::resource::is_scoped_key)
        };
        if found.is_none()
            && !scoped(ci)
            && let Some(cv) = ci.and_then(|ci| ci.embedding.as_deref())
        {
            'groups: for (g, group) in groups.iter().enumerate() {
                if group.has_tier(c.tier) {
                    continue;
                }
                for m in group.members() {
                    if scoped(identity(m)) {
                        continue;
                    }
                    let Some(mv) = identity(m).and_then(|i| i.embedding.as_deref()) else {
                        continue;
                    };
                    if vector_budget == 0 {
                        break 'groups;
                    }
                    vector_budget -= 1;
                    if same_vector(cv, mv, params.dedupe_threshold) {
                        found = Some(g);
                        break 'groups;
                    }
                }
            }
        }
        let g = match found {
            Some(g) => {
                let group = &mut groups[g];
                if c.tier < group.shown.0 {
                    let previous = std::mem::replace(&mut group.shown, member);
                    group.also.push(previous);
                } else {
                    group.also.push(member);
                }
                g
            }
            None => {
                if !c.forced {
                    if slots >= params.top_k {
                        // Cut, but remembered by key: a forced farther copy
                        // emitted later is still shown as this nearer one.
                        if let Some(k) = ci.and_then(|ci| ci.canonical_key.as_deref()) {
                            cut_by_key.entry(k).or_default().push(member);
                        }
                        continue;
                    }
                    slots += 1;
                }
                groups.push(Group {
                    shown: member,
                    also: Vec::new(),
                });
                groups.len() - 1
            }
        };
        if let Some(k) = ci.and_then(|ci| ci.canonical_key.as_deref()) {
            let entry = by_key.entry(k).or_default();
            if !entry.contains(&g) {
                entry.push(g);
            }
            if let Some(cut) = cut_by_key.get_mut(k) {
                let group = &mut groups[g];
                cut.retain(|&m| {
                    if group.has_tier(m.0) {
                        return true;
                    }
                    if m.0 < group.shown.0 {
                        let previous = std::mem::replace(&mut group.shown, m);
                        group.also.push(previous);
                    } else {
                        group.also.push(m);
                    }
                    false
                });
            }
        }
    }

    // Every (tier, pos) whose copy reached the answer, shown or as `also`.
    let mut contributed: Vec<Vec<bool>> = lists
        .iter()
        .map(|l| vec![false; l.recall.as_ref().map_or(0, |t| t.recall.hits.len())])
        .collect();
    for g in &groups {
        for (t, p) in g.members() {
            contributed[t][p] = true;
        }
    }

    // Rule 4: re-render and cut once.
    let mut hits = Vec::with_capacity(groups.len());
    let mut detailed = Vec::with_capacity(groups.len());
    let mut blocks = Vec::with_capacity(groups.len());
    let mut sessions = Vec::with_capacity(groups.len());
    let mut tiers = Vec::with_capacity(groups.len());
    let mut alsos = Vec::with_capacity(groups.len());
    let mut hit_legs = Vec::with_capacity(groups.len());
    let mut own_legs = HashMap::new();
    for g in &groups {
        let (tier, pos) = g.shown;
        let list = &lists[tier];
        let t = recall_of(tier).expect("shown from an available tier");
        let hit = &t.recall.hits[pos];
        let mut d = t.recall.detailed[pos].clone();
        d.included_in_context = false;
        d.truncated_in_context = false;
        let also: Vec<LayerCopy> = g
            .also
            .iter()
            .map(|&(at, ap)| LayerCopy {
                session: lists[at].session.clone(),
                node_id: recall_of(at)
                    .expect("merged from an available tier")
                    .recall
                    .hits[ap]
                    .node_id,
            })
            .collect();
        let mut meta = Vec::new();
        if tier > 0 {
            meta.push(format!("from {}", list.session));
        }
        meta.extend(
            also.iter()
                .map(|a| format!("also {} in {}", format::short_id(a.node_id), a.session)),
        );
        let lines: Vec<String> = d.annotations.iter().map(|a| a.text.clone()).collect();
        let scope = (tier > 0).then(|| list.session.to_string());
        blocks.push(block_with_meta(hit, &lines, &meta, scope));
        let legs = t.recall.legs.get(&hit.node_id).copied();
        if tier == 0
            && let Some(l) = legs
        {
            own_legs.insert(hit.node_id, l);
        }
        hit_legs.push(legs);
        hits.push(hit.clone());
        detailed.push(d);
        sessions.push(list.session.clone());
        tiers.push(tier);
        alsos.push(also);
    }
    let fitted = format::fit_blocks(&blocks, params.max_tokens, token_fn);
    let truncation = DetailedRecall::mark_context(&mut detailed, &fitted);
    let rendered = fitted.rendered();

    // Warnings and response annotations, tier by tier (rule 5, the prefix
    // rule and the traversal headline). A hit's lines leave `warnings` only
    // when no copy of it reached the answer.
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
        let mut annotations = t.recall.response_annotations.clone();
        if tier > 0
            && recall_of(0)
                .and_then(|p| p.recall.anchor.as_ref())
                .is_some_and(|a| t.recall.anchor.as_ref() != Some(a))
        {
            for a in &annotations {
                if a.kind == AnnotationKind::Traversal {
                    lines.retain(|line| *line != a.text);
                }
            }
            annotations.retain(|a| a.kind != AnnotationKind::Traversal);
        }
        for a in annotations
            .iter_mut()
            .filter(|a| a.kind == AnnotationKind::Traversal)
        {
            let count = tiers[..rendered].iter().filter(|&&x| x == tier).count();
            let headline = crate::recall::dispatch::traversal_headline(count);
            if let Some(line) = lines.iter_mut().find(|l| **l == a.text) {
                *line = headline.clone();
            }
            a.text = headline;
        }
        if tier == 0 {
            warnings.extend(lines);
            response_annotations.extend(annotations);
        } else {
            warnings.extend(lines.iter().map(|l| prefixed(&list.session, l)));
            response_annotations.extend(
                annotations
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
        anchor: recall_of(0).and_then(|t| t.recall.anchor.clone()),
        hits,
        context: fitted.context,
        truncation,
        warnings,
        legs: own_legs,
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
        hit_legs,
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
