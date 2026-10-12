//! Dependency-question dispatch (T9) — route by query kind, not by blending
//! arms.
//!
//! The recall pipeline (`candidates` → `expand` → `assemble`) answers every
//! query by *ranking*: a structural dependent that is reachable by traversal
//! still has to beat word-matched members at `top_k`, and a prose question
//! against identifier-shaped content can collapse to a flat, uninformative
//! floor. Both are category errors the doc calls out — "what depends on X"
//! has an exact answer reachable by traversal, so ranking it is wrong.
//!
//! `try_structural` recognizes a dependency question, resolves the entity it
//! names, and answers it **by traversal** over the structural edge types
//! (`Dependency` / `Hierarchical` / `Causal`), returning the anchor's
//! dependents ranked by how strongly the graph binds each one to it. It is a
//! *dispatch*: when it fires it produces the whole `RecallResult` and the
//! blended pipeline never runs. When it cannot (a general query, or a
//! structural phrasing with no resolvable anchor / no dependents) it returns
//! `None` and the caller falls through to the normal word/blend path — the
//! refusal is as important as the dispatch.
//!
//! The traversal is **one hop by design** (T9-R1-6): it returns the anchor's
//! DIRECT structural dependents, which is exactly the §4.1 blast-radius
//! predicate. `RecallQuery::traversal_depth` is intentionally not honored —
//! multi-hop expansion would turn a "depends on" answer into a transitive
//! reachability question, which belongs to the blended pipeline's `expand`,
//! not to blast-radius traversal. Ordering, however, matches the blend: spec
//! §10 canonical-first promotion is applied ([`dependents`]).
//!
//! Every step logs at `tracing::trace!` (`target: "lambo::recall"`): which
//! arm a question is classified into, the resolved anchor, and each dependent
//! collected. This is default-invisible (no subscriber enables trace on the
//! CLI/daemon path) and is the T9 instrumentation for the structural arm.

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::graph::Graph;
use crate::recall::detail::{Annotation, AnnotationKind, DetailedHit, DetailedRecall};
use crate::recall::format;
use crate::types::{CanonizationStatus, Node, NodeId, RecallHit, Scored, tie_break_by_key};

/// What kind of question a recall query is, for routing (T9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecallKind {
    /// A general / word-match question — the blended pipeline answers it.
    General,
    /// A structural / dependency question — the traversal answers it.
    Structural,
}

/// The traversal answers a dependency question with the #36 dependents
/// relation ([`format::dependents_of`]): the concepts that declare a
/// dependence on the anchor (`Dependency` sources into it, its `Hierarchical`
/// children). `Causal`, provenance and association edges never carry
/// dependency. The returned set and the `blast_radius` field stamped on each
/// hit are the same relation, so they always agree.
const STRUCTURAL_MARKERS: [&str; 7] = [
    "depends on",
    "depend on",
    "depends-on",
    "dependents of",
    "blast radius",
    "safe to delete",
    "what uses",
];

/// Phrasings that mark a question as structural / dependency rather than
/// plain word recall. Kept deliberately small and EXPLICIT (T9-R1-1): every
/// marker is a dependency/structural phrasing, never an ordinary substring.
/// In particular bare `"depend"` is NOT a marker — it would match inside
/// `"independent"` — and bare `"references"` / `"is it safe"` are ordinary
/// prose, so they are dropped. A marker-bearing but non-dependency question
/// ("is the system independent of a single region", "the report references
/// the changelog") stays `General`.
///
/// Classify a query into a recall arm (lexical/vector vs structural).
pub fn classify(query: &str) -> RecallKind {
    let lower = query.to_lowercase();
    let structural = STRUCTURAL_MARKERS.iter().any(|m| lower.contains(m));
    tracing::trace!(
        target: "lambo::recall",
        query = %query,
        structural = structural,
        "recall dispatch: classified query arm"
    );
    if structural {
        RecallKind::Structural
    } else {
        RecallKind::General
    }
}

/// Closed-class scaffolding never anchors, except explicitly named uppercase
/// acronyms. Ordinary recall may also omit these words in its tokenizer.
const ANCHOR_STOP_WORDS: &[&str] = &[
    "a", "an", "and", "any", "anything", "are", "be", "can", "do", "does", "for", "if", "in", "is",
    "it", "its", "my", "of", "on", "or", "our", "the", "this", "to", "what", "which", "that",
    "with", "from", "by", "at", "how", "who", "when", "will", "i", "we",
];

const MAX_ANCHOR_QUERY_BYTES: usize = 16 * 1024;
const MAX_ANCHOR_SPANS: usize = 256;

/// NFC followed by per-character lowercase avoids contextual final-sigma
/// folding and makes composed and decomposed names equivalent.
pub(crate) fn anchor_text(text: &str) -> String {
    text.nfc().flat_map(char::to_lowercase).nfc().collect()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || is_combining_mark(c)
}

/// A hyphen joins an identifier only between word characters. Standalone
/// punctuation still delimits a word; SG-Base cannot name SG-Base-VPC.
fn word_at(hay: &str, at: usize) -> bool {
    let c = hay[at..].chars().next().expect("character offset");
    is_word_char(c)
        || (c == '-'
            && hay[..at].chars().next_back().is_some_and(is_word_char)
            && hay[at + 1..].chars().next().is_some_and(is_word_char))
}

/// Byte spans of every occurrence of `needle` in `hay` that sits on word
/// boundaries. Both strings are expected lowercased already. A side of the
/// needle that ends in a word character must meet the start/end of `hay` or a
/// non-word character; a side that ends in punctuation (`.env`, `/etc/`) is
/// its own delimiter and needs nothing more. Overlapping occurrences are all
/// tried, so a first embedded hit cannot hide a later whole-word one.
fn word_bounded_spans(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let (Some(first), Some(last)) = (needle.chars().next(), needle.chars().next_back()) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let start = from + i;
        let end = start + needle.len();
        let left_ok = !is_word_char(first)
            || hay[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !word_at(hay, start - c.len_utf8()));
        let right_ok =
            !is_word_char(last) || hay[end..].chars().next().is_none_or(|_| !word_at(hay, end));
        if left_ok && right_ok {
            spans.push((start, end));
            if spans.len() > MAX_ANCHOR_SPANS {
                break;
            }
        }
        // Step one char, not one match, so overlapping occurrences are seen.
        from = start + hay[start..].chars().next().map_or(1, char::len_utf8);
    }
    spans
}

/// Byte spans of every structural-marker occurrence in the lowercased query.
/// A concept match that lies wholly inside one of these is the question's
/// own wording (a concept literally named `"blast radius"`), not the thing
/// it names, so it does not anchor.
fn marker_spans(lower_q: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for m in STRUCTURAL_MARKERS {
        let mut from = 0;
        while let Some(i) = lower_q[from..].find(m) {
            let start = from + i;
            spans.push((start, start + m.len()));
            from = start + lower_q[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    spans
}

/// Whether every word of `lower_content` is an [`ANCHOR_STOP_WORDS`] entry.
fn is_stop_content(lower_content: &str) -> bool {
    let mut words = lower_content
        .split(|c: char| !is_word_char(c))
        .filter(|w| !w.is_empty());
    let mut any = false;
    let all = words.all(|w| {
        any = true;
        ANCHOR_STOP_WORDS.contains(&w)
    });
    any && all
}

/// Resolve a named subject, removing strictly contained occurrences before
/// ranking by distance after the marker, type, length, content and stable key.
/// Over-budget queries or candidates refuse dispatch instead of using partial
/// evidence. The security-group prose fallback remains the second choice.
pub(crate) fn resolve_anchor(graph: &Graph, query: &str) -> Option<NodeId> {
    if query.len() > MAX_ANCHOR_QUERY_BYTES {
        return None;
    }
    let raw_q: String = query.nfc().collect();
    let lower_q = anchor_text(query);
    if lower_q.len() > MAX_ANCHOR_QUERY_BYTES {
        return None;
    }
    let markers = marker_spans(&lower_q);
    let mut candidates = Vec::new();
    let mut spans = Vec::new();
    let mut marker_clauses = Vec::new();
    for c in graph.concepts() {
        let lower_c = anchor_text(&c.content);
        if !lower_c.chars().any(is_word_char) {
            continue;
        }
        let mut found = word_bounded_spans(&lower_q, &lower_c);
        if found.len() > MAX_ANCHOR_SPANS {
            return None;
        }
        if found.is_empty() {
            continue;
        }
        if is_stop_content(&lower_c) {
            let raw_c: String = c.content.nfc().collect();
            let uppercase =
                raw_c.chars().any(char::is_uppercase) && !raw_c.chars().any(char::is_lowercase);
            if !uppercase {
                continue;
            }
            found = word_bounded_spans(&raw_q, &raw_c)
                .into_iter()
                .map(|(s, e)| {
                    (
                        anchor_text(&raw_q[..s]).len(),
                        anchor_text(&raw_q[..e]).len(),
                    )
                })
                .collect();
            if found.is_empty() {
                continue;
            }
        }
        let index = candidates.len();
        candidates.push(c);
        for (start, end) in found {
            // Marker-overlapping clauses must not suppress the subject.
            if markers.iter().any(|&(ms, me)| ms <= start && end <= me) {
                continue;
            }
            if markers.iter().any(|&(ms, me)| start < me && ms < end) {
                marker_clauses.push(index);
            } else {
                spans.push((start, end, index));
            }
        }
    }
    spans.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let mut best = vec![None::<usize>; candidates.len()];
    for index in marker_clauses {
        best[index] = Some(usize::MAX);
    }
    let mut furthest: Option<(usize, usize)> = None;
    for (start, end, index) in spans {
        let contained =
            furthest.is_some_and(|(s, e)| s <= start && end <= e && (s < start || end < e));
        if !contained {
            let distance = markers
                .iter()
                .filter(|&&(_, e)| e <= start)
                .map(|&(_, e)| start - e)
                .min()
                .unwrap_or(usize::MAX);
            best[index] = Some(best[index].map_or(distance, |d| d.min(distance)));
        }
        if furthest.is_none_or(|(_, e)| end > e) {
            furthest = Some((start, end));
        }
    }
    let mut matches: Vec<_> = candidates
        .into_iter()
        .zip(best)
        .filter_map(|(c, d)| d.map(|d| (c, d)))
        .collect();
    let ty_prio = |c: &crate::types::Concept| match c.concept_type {
        crate::types::ConceptType::Entity | crate::types::ConceptType::Resource => 0,
        _ => 1,
    };
    matches.sort_by(|(a, ad), (b, bd)| {
        ad.cmp(bd)
            .then_with(|| ty_prio(a).cmp(&ty_prio(b)))
            .then_with(|| b.content.chars().count().cmp(&a.content.chars().count()))
            .then_with(|| a.content.cmp(&b.content))
            .then_with(|| {
                tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
            })
    });
    if let Some((c, _)) = matches.first() {
        return Some(c.id);
    }

    if !lower_q.contains("security group") {
        return None;
    }
    let sg: Vec<NodeId> = graph
        .concepts()
        .filter(|c| {
            let upper = c.content.to_uppercase();
            upper.starts_with("SG-") || upper.contains(" SECURITY GROUP")
        })
        .map(|c| c.id)
        .collect();
    // Each `dependents(graph, *id)` walks the SG's incident edges, run twice
    // per recall (brief read + final lock); counts are precomputed so the
    // sort calls it once per SG instead of once per comparison.
    // Most structural dependents wins; equal-dependent SGs fall to the stable
    // issue-2 chain (canonical key ascending, NodeId ascending) because
    // concept-store iteration order is HashMap order and never run-stable
    // (remediation round 2; the count-only sort silently leaned on it).
    let mut sg: Vec<(usize, NodeId)> = sg
        .into_iter()
        .map(|id| (dependents(graph, id).len(), id))
        .collect();
    sg.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            let key = |n: NodeId| match graph.node(n) {
                Some(crate::types::Node::Concept(c)) => Some(c.canonical_key.as_str()),
                _ => None,
            };
            tie_break_by_key(key(a.1), &a.1, key(b.1), &b.1)
        })
    });
    if let Some((_, id)) = sg.first() {
        tracing::trace!(
            target: "lambo::recall",
            anchor = %id,
            "recall dispatch: prose structural query, chose most-shared SG anchor"
        );
        return Some(*id);
    }
    None
}

/// Cheap in-memory predicate: would a structural dispatch fire for `query` on
/// `graph`? True iff the query classifies `Structural` AND an anchor resolves
/// WITH at least one structural dependent. The daemon routing (T9-R1-3) uses
/// this to decide whether the async store-gather can be skipped — it skips the
/// gather only when the dispatch is actually about to fire, so a structural
/// phrasing that cannot dispatch falls through to the FULL blend (never a
/// degraded keyword-only answer). The dispatch itself re-validates under the
/// final graph lock inside `try_structural`, so this is a gather-skip
/// decision, not a short-circuit of correctness.
pub fn fits_structural(graph: &Graph, query: &str) -> bool {
    if !matches!(classify(query), RecallKind::Structural) {
        return false;
    }
    match resolve_anchor(graph, query) {
        Some(anchor) => !dependents(graph, anchor).is_empty(),
        None => false,
    }
}

/// The structural dependents of `anchor`, ranked by how strongly the graph
/// binds each to it.
///
/// Membership is the #36 dependents relation ([`format::dependents_of`]): a
/// concept depends on `anchor` when it is the source of a `Dependency` edge
/// into `anchor` or `anchor`'s `Hierarchical` child. Every such concept is
/// returned, including one that also depends on something else: the
/// delete-safety question is "what rests on this", and a dependent with a
/// second pillar still rests on this one. The set is exactly what
/// `blast_radius` counts, so the two agree. The anchor itself is never its
/// own dependent.
///
/// Score is the strongest weight among the edges declaring the dependence.
/// Ordering applies spec §10 canonical-first promotion, then strength, then
/// the issue-2 tie-break (canonical key ascending, node id ascending behind
/// it): the same partition and tie order `assemble` applies, so the traversal
/// answer orders identically to the blend.
pub fn dependents(graph: &Graph, anchor: NodeId) -> Vec<Scored<NodeId>> {
    let mut out: Vec<Scored<NodeId>> = format::dependents_of(graph, anchor)
        .into_iter()
        .map(|(id, w)| Scored::new(id, w))
        .collect();
    let is_canonical = |id: NodeId| {
        matches!(
            graph.node(id),
            Some(crate::types::Node::Concept(c))
                if c.canonization_status == CanonizationStatus::Canonical
        )
    };
    let key = |id: NodeId| match graph.node(id) {
        Some(Node::Concept(c)) => Some(c.canonical_key.as_str()),
        _ => None,
    };
    out.sort_by(|a, b| {
        is_canonical(b.item)
            .cmp(&is_canonical(a.item))
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| tie_break_by_key(key(a.item), &a.item, key(b.item), &b.item))
    });
    for s in &out {
        tracing::trace!(
            target: "lambo::recall",
            node = %s.item,
            score = s.score,
            "recall dispatch: structural dependent"
        );
    }
    out
}

/// Answer a structural query by traversal when possible.
///
/// Returns `Some(DetailedRecall)` only when the query is structural AND an
/// anchor resolves AND it has at least one dependent. Otherwise `None`
/// (refusal — caller falls through to the blended pipeline).
pub(crate) fn try_structural(
    graph: &Graph,
    query: &str,
    top_k: usize,
    max_tokens: usize,
) -> Option<DetailedRecall> {
    if !matches!(classify(query), RecallKind::Structural) {
        return None;
    }
    let anchor = resolve_anchor(graph, query)?;
    let deps = dependents(graph, anchor);
    if deps.is_empty() {
        return None;
    }

    let radii = format::blast_radii(graph);
    let mut hits: Vec<RecallHit> = Vec::new();
    let mut detailed: Vec<DetailedHit> = Vec::new();
    let mut blocks: Vec<format::ContextBlock> = Vec::new();
    for s in deps.into_iter().take(top_k) {
        let crate::types::Node::Concept(c) = graph.node(s.item)? else {
            continue;
        };
        let canonical = c.canonization_status == CanonizationStatus::Canonical;
        let hit = RecallHit {
            node_id: c.id,
            content: c.content.clone(),
            concept_type: Some(c.concept_type),
            score: s.score,
            is_canonical: canonical,
            blast_radius: if canonical {
                Some(radii.get(&c.id).copied().unwrap_or(0))
            } else {
                None
            },
        };
        // H3: status from the same graph snapshot, typed annotation attached
        // where its producer is known (mirrors assemble). Status `None` is
        // carried as absent (the wire contract).
        let mut detailed_hit = DetailedHit::new(
            &hit,
            (c.canonization_status != CanonizationStatus::None).then_some(c.canonization_status),
        );
        // T9-R1-5: canonical structural hits render the §13 load-bearing-pillar
        // warning exactly as the blended pipeline does (assemble), so the
        // structural and blend surfaces look identical.
        let mut lines: Vec<String> = Vec::new();
        if canonical {
            let text = format::blast_radius_warning(hit.blast_radius.unwrap_or_default());
            detailed_hit
                .annotations
                .push(Annotation::new(AnnotationKind::LoadBearing, text.clone()));
            lines.push(text);
        }
        blocks.push(format::ContextBlock::for_hit(&hit, &lines));
        hits.push(hit);
        detailed.push(detailed_hit);
    }
    if hits.is_empty() {
        return None;
    }

    // The shared budget cut (`format::fit_blocks`, the same one assemble
    // runs): whole blocks, longest prefix first, or the first block's
    // truncated head with its marker when it alone is too long (#130).
    let fitted = format::fit_blocks(&blocks, max_tokens, format::default_token_count);

    tracing::debug!(
        target: "lambo::recall",
        query = %query,
        hits = hits.len(),
        "recall dispatched dependency question to structural traversal"
    );
    // T9-R1-N4: headline names only the dependents the context actually
    // renders (kept whole blocks, plus a truncated first block, which the
    // context does show), not the pre-truncation hit set.
    let count = fitted.rendered();
    let mut warnings = Vec::new();
    let traversal = traversal_headline(count);
    warnings.push(traversal.clone());
    // H3: included_in_context at the cut; the traversal explanation is
    // response-global — one annotation, never attached to a hit.
    let truncation = DetailedRecall::mark_context(&mut detailed, &fitted);
    Some(DetailedRecall {
        hits,
        context: fitted.context,
        truncation,
        warnings,
        // A dispatched structural query answers by traversal and skips the
        // phase-1 blend entirely (T9-R1-3), so there are no leg scores to
        // report. Empty here means "no leg ran", not "the legs were dropped".
        legs: Default::default(),
        // No blend ran, so there is no daemon share to withhold.
        cold_start: false,
        detailed,
        response_annotations: vec![Annotation::new(AnnotationKind::Traversal, traversal)],
    })
}

/// The structural-dispatch headline (the `traversal` response annotation
/// and its warning line) for `count` rendered dependents (T9-R1-N4). The
/// layered merge recomputes it after its own budget cut (#33 PR 2).
pub(crate) fn traversal_headline(count: usize) -> String {
    format!("recall: dependency question answered by graph traversal ({count} dependents)")
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "fixtures")]
    use parking_lot::RwLock;
    #[cfg(feature = "fixtures")]
    use std::sync::Arc;

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::graph::Graph;
    use crate::types::{
        AgentId, Concept, ConceptType, Edge, EdgeType, Interaction, RecallQuery, SessionId,
    };

    fn sid() -> SessionId {
        SessionId::from("t9-exhibit")
    }

    fn ts(min: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(min)
    }

    fn concept(id: u64, content: &str, ty: ConceptType) -> Concept {
        Concept {
            id: NodeId(uuid::Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.to_string(),
            canonical_key: content.to_lowercase(),
            concept_type: ty,
            origin_interaction: NodeId(uuid::Uuid::from_u64_pair(1, 1)),
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        }
    }

    fn edge(id: u64, src: u64, tgt: u64, ty: EdgeType, weight: f64) -> Edge {
        Edge {
            event_time: None,
            id: NodeId(uuid::Uuid::from_u64_pair(3, id)),
            session_id: sid(),
            source: NodeId(uuid::Uuid::from_u64_pair(2, src)),
            target: NodeId(uuid::Uuid::from_u64_pair(2, tgt)),
            edge_type: ty,
            weight,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        }
    }

    /// The cloudops-exhibit shape measured from the live Cockroach store
    /// (psql, 2026-08-17): `SG-Base-VPC` owns its four constraint rules and
    /// one real dependent, `RDS-Lambo-Demo-DB`, all via `Hierarchical`
    /// parent→child edges; `SG-PublicWeb` owns its rules. No concept content
    /// matches the prose of the two exemplar queries except the bare
    /// identifiers.
    fn exhibit() -> Graph {
        let mut g = Graph::new(sid());
        let i1 = Interaction {
            event_time: None,
            id: NodeId(uuid::Uuid::from_u64_pair(1, 1)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("provision network".into()),
            previous_id: None,
            created_at: ts(0),
        };
        g.insert_interaction(i1.clone()).unwrap();

        let concepts: &[(u64, &str, ConceptType)] = &[
            (10, "SG-Base-VPC", ConceptType::Entity),
            (
                11,
                "SG-Base-VPC = sg-071b52ffe5950efdf",
                ConceptType::Entity,
            ),
            (
                12,
                "SG-Base-VPC egress all protocols to 0.0.0.0/0",
                ConceptType::Constraint,
            ),
            (
                13,
                "SG-Base-VPC ingress all protocols from SG-Base-VPC",
                ConceptType::Constraint,
            ),
            (
                14,
                "SG-Base-VPC ingress tcp/5432 from SG-PublicWeb",
                ConceptType::Constraint,
            ),
            (20, "SG-PublicWeb", ConceptType::Entity),
            (
                21,
                "SG-PublicWeb = sg-0cf21b70c346eaa99",
                ConceptType::Entity,
            ),
            (
                22,
                "SG-PublicWeb egress all protocols to 0.0.0.0/0",
                ConceptType::Constraint,
            ),
            (30, "RDS-Lambo-Demo-DB", ConceptType::Entity),
            (
                31,
                "RDS-Lambo-Demo-DB = rds-lambo-demo-db.cm3yke2423t5.us-east-1.rds.amazonaws.com",
                ConceptType::Entity,
            ),
            (
                32,
                "RDS-Lambo-Demo-DB is not publicly accessible",
                ConceptType::Constraint,
            ),
        ];
        for (id, content, ty) in concepts {
            g.insert_concept(concept(*id, content, *ty), i1.id).unwrap();
        }

        // Structural edges (measured live): SG-Base-VPC → its rules/alias and
        // → RDS-Lambo-Demo-DB (Hierarchical); RDS → its own alias/constraints.
        let edges = [
            edge(1, 10, 12, EdgeType::Hierarchical, 4.5),
            edge(2, 10, 13, EdgeType::Hierarchical, 4.5),
            edge(3, 10, 14, EdgeType::Hierarchical, 4.5),
            edge(4, 10, 11, EdgeType::Hierarchical, 4.5),
            edge(5, 10, 30, EdgeType::Hierarchical, 9.5), // RDS depends on SG-Base-VPC
            edge(6, 30, 31, EdgeType::Hierarchical, 4.5),
            edge(7, 30, 32, EdgeType::Hierarchical, 4.0),
            // SG-PublicWeb owns its rules.
            edge(8, 20, 22, EdgeType::Hierarchical, 4.5),
            edge(9, 20, 21, EdgeType::Hierarchical, 4.5),
        ];
        for e in edges {
            g.upsert_edge(e).unwrap();
        }
        g
    }

    fn query(q: &str) -> RecallQuery {
        RecallQuery {
            query: q.into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        }
    }

    fn content_of(hits: &[RecallHit]) -> Vec<&str> {
        hits.iter().map(|h| h.content.as_str()).collect()
    }

    #[test]
    fn classifies_dependency_and_delete_safety_questions_structural() {
        assert_eq!(
            classify("what depends on SG-Base-VPC"),
            RecallKind::Structural
        );
        assert_eq!(
            classify("is it safe to delete the shared security group"),
            RecallKind::Structural
        );
        assert_eq!(
            classify("does anything depend on this"),
            RecallKind::Structural
        );
        // General questions stay general (refusal path).
        assert_eq!(classify("update user schema"), RecallKind::General);
        assert_eq!(classify("pagination"), RecallKind::General);
    }

    #[test]
    fn marker_bearing_non_dependency_questions_stay_general() {
        // T9-R1-1: a marker-bearing but genuinely lexical/vector question must
        // NOT route to the traversal. "independent" embeds the bare "depend"
        // substring and "references" is ordinary prose — both must stay
        // General, and must not dispatch.
        assert_eq!(
            classify("is the system independent of a single region"),
            RecallKind::General
        );
        assert_eq!(
            classify("the report references the changelog"),
            RecallKind::General
        );
        let g = exhibit();
        assert_eq!(
            try_structural(&g, "is the system independent of a single region", 5, 500),
            None,
            "a non-dependency marker-bearing question must not dispatch"
        );
    }

    #[test]
    fn dependency_question_returns_structural_dependent_by_traversal() {
        let g = exhibit();
        let result = try_structural(&g, "what depends on SG-Base-VPC", 5, 500)
            .expect("structural question must dispatch");
        let contents = content_of(&result.hits);
        assert!(
            contents.contains(&"RDS-Lambo-Demo-DB"),
            "the dependent must be surfaced, got {contents:?}"
        );
        // The traversal ranks the strongest-bound dependent first: RDS is
        // bound to SG-Base-VPC at weight 9.5 (> its rules' 4.5).
        assert_eq!(contents[0], "RDS-Lambo-Demo-DB");
        // The anchor itself is answered by traversal, not echoed back.
        assert!(!contents.contains(&"SG-Base-VPC"));
    }

    #[test]
    fn delete_safety_question_returns_real_ranking_not_flat_floor() {
        let g = exhibit();
        let result = try_structural(&g, "is it safe to delete the shared security group", 5, 500)
            .expect("delete-safety question must dispatch");
        let scores: Vec<f64> = result.hits.iter().map(|h| h.score).collect();
        assert!(result.hits.len() >= 2, "expected a ranked dependent set");
        let distinct: std::collections::HashSet<u64> = scores.iter().map(|s| s.to_bits()).collect();
        assert!(
            distinct.len() > 1,
            "scores must not be a flat constant floor, got {scores:?}"
        );
        // Scores are descending (a real ranking).
        assert!(scores.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn general_question_is_refused_and_falls_through_to_blend() {
        let g = exhibit();
        // Not structural: dispatch refuses.
        assert_eq!(
            try_structural(&g, "update user schema", 5, 500),
            None,
            "a general question must NOT be answered by traversal"
        );
        // And a structural phrasing with no resolvable anchor refuses too.
        assert_eq!(
            try_structural(&g, "what depends on nothing-imaginary", 5, 500),
            None
        );
    }

    /// Remediation round 2: among same-type, equal-length substring matches
    /// the anchor must not fall to the run-minted id. `alpha`/`gamma` are both
    /// five bytes and compare OPPOSITE to their ids, so the old
    /// `(type, length, id)` rank picked `gamma`; the extended chain compares
    /// content first.
    #[test]
    fn anchor_substring_ties_break_on_content_ahead_of_id() {
        let mut g = Graph::new(sid());
        let i1 = Interaction {
            event_time: None,
            id: NodeId(uuid::Uuid::from_u64_pair(1, 1)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("q".into()),
            previous_id: None,
            created_at: ts(0),
        };
        let i1_id = i1.id;
        g.insert_interaction(i1.clone()).unwrap();
        // `alpha` rides the LARGER id; `gamma` the smaller one.
        let alpha = concept(9, "alpha", ConceptType::Entity);
        let alpha_id = alpha.id;
        let gamma = concept(8, "gamma", ConceptType::Entity);
        g.insert_concept(alpha, i1_id).unwrap();
        g.insert_concept(gamma, i1_id).unwrap();

        assert_eq!(
            resolve_anchor(&g, "zz alpha yy gamma zz"),
            Some(alpha_id),
            "equal (type, length) matches compare content before the run-minted id"
        );
    }

    /// Remediation round 2: equal-dependent SG anchors must not lean on
    /// concept-store iteration order (`Graph::concepts` is a HashMap view, so
    /// the old count-only stable sort was not run-stable). Both SGs here have
    /// zero dependents and keys ordered opposite to ids; the pick must be the
    /// smaller canonical key whichever way the concepts went into the graph.
    #[test]
    fn sg_prose_anchor_ties_break_on_canonical_key_ahead_of_id() {
        let build = |alpha_first: bool| -> Graph {
            let mut g = Graph::new(sid());
            let i1 = Interaction {
                event_time: None,
                id: NodeId(uuid::Uuid::from_u64_pair(1, 1)),
                session_id: sid(),
                agent_id: AgentId::from("agent-a"),
                prompt_text: Some("q".into()),
                previous_id: None,
                created_at: ts(0),
            };
            let i1_id = i1.id;
            g.insert_interaction(i1.clone()).unwrap();
            // `SG-Alpha` rides the LARGER id; `SG-Beta` the smaller one.
            let alpha = concept(9, "SG-Alpha", ConceptType::Entity);
            let beta = concept(8, "SG-Beta", ConceptType::Entity);
            let (first, second) = if alpha_first {
                (alpha, beta)
            } else {
                (beta, alpha)
            };
            g.insert_concept(first, i1_id).unwrap();
            g.insert_concept(second, i1_id).unwrap();
            g
        };
        let expected = NodeId(uuid::Uuid::from_u64_pair(2, 9)); // key "sg-alpha"
        for alpha_first in [true, false] {
            assert_eq!(
                resolve_anchor(&build(alpha_first), "which security group is most shared"),
                Some(expected),
                "equal-dependent SGs follow canonical key, not store iteration order"
            );
        }
    }

    /// A graph of `(id, content, type)` concepts, each the sole structural
    /// source of one dependent (id + 100, content `"<content> consumer"`), so
    /// whichever concept anchors, `try_structural` dispatches and its first
    /// hit names the anchor.
    fn anchors(concepts: &[(u64, &str, ConceptType)]) -> Graph {
        let mut g = Graph::new(sid());
        let i1 = Interaction {
            event_time: None,
            id: NodeId(uuid::Uuid::from_u64_pair(1, 1)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("q".into()),
            previous_id: None,
            created_at: ts(0),
        };
        g.insert_interaction(i1.clone()).unwrap();
        for (n, (id, content, ty)) in concepts.iter().enumerate() {
            g.insert_concept(concept(*id, content, *ty), i1.id).unwrap();
            let dep = format!("{content} consumer");
            g.insert_concept(concept(id + 100, &dep, ConceptType::Logic), i1.id)
                .unwrap();
            g.upsert_edge(edge(n as u64 + 1, id + 100, *id, EdgeType::Dependency, 5.0))
                .unwrap();
        }
        g
    }

    fn anchor_content(g: &Graph, query: &str) -> Option<String> {
        resolve_anchor(g, query).map(|id| match g.node(id) {
            Some(Node::Concept(c)) => c.content.clone(),
            other => panic!("anchor is not a concept: {other:?}"),
        })
    }

    #[test]
    fn separate_names_choose_the_subject_nearest_after_the_marker() {
        let g = anchors(&[
            (10, "SG-Base-VPC", ConceptType::Entity),
            (11, "VPC-Enterprise-Prod", ConceptType::Resource),
        ]);
        for marker in ["safe to delete", "depends on", "dependents of", "what uses"] {
            let q = format!("is it {marker} SG-Base-VPC in VPC-Enterprise-Prod");
            assert_eq!(anchor_content(&g, &q).as_deref(), Some("SG-Base-VPC"));
            assert_eq!(
                content_of(&try_structural(&g, &q, 10, 500).unwrap().hits),
                vec!["SG-Base-VPC consumer"]
            );
        }
        assert_eq!(
            anchor_content(&g, "SG-Base-VPC depends on VPC-Enterprise-Prod").as_deref(),
            Some("VPC-Enterprise-Prod")
        );
    }

    #[test]
    fn identifier_components_do_not_anchor_underscore_or_hyphen_names() {
        let g = anchors(&[
            (10, "db", ConceptType::Entity),
            (11, "SG-Base", ConceptType::Entity),
        ]);
        for q in ["what depends on user_db", "what depends on SG-Base-VPC"] {
            assert_eq!(anchor_content(&g, q), None, "{q}");
        }
        assert_eq!(
            anchor_content(&g, "what depends on (SG-Base) - db").as_deref(),
            Some("SG-Base")
        );
    }

    #[test]
    fn uppercase_scaffolding_acronyms_require_an_exact_uppercase_occurrence() {
        for name in ["IT", "CAN", "OR", "DO", "IS", "MY", "IF", "ANY"] {
            let g = anchors(&[(10, name, ConceptType::Entity)]);
            assert_eq!(
                anchor_content(&g, &format!("what depends on {name}")).as_deref(),
                Some(name)
            );
            assert_eq!(
                anchor_content(&g, &format!("what depends on {}", name.to_lowercase())),
                None
            );
            assert_eq!(anchor_content(&g, "is it safe to delete unknown"), None);
        }
        let g = anchors(&[
            (10, "IS", ConceptType::Entity),
            (11, "queue", ConceptType::Entity),
        ]);
        assert_eq!(
            anchor_content(&g, "is it safe to delete queue in IS").as_deref(),
            Some("queue")
        );
        for name in [
            "that", "with", "from", "by", "at", "how", "who", "when", "will", "i", "we",
        ] {
            let g = anchors(&[(10, name, ConceptType::Entity)]);
            assert_eq!(anchor_content(&g, &format!("what depends on {name}")), None);
        }
    }

    #[test]
    fn normalized_accents_combining_marks_and_sigma_preserve_word_boundaries() {
        let g = anchors(&[
            (10, "café", ConceptType::Entity),
            (11, "ΟΣ", ConceptType::Entity),
            (12, "x", ConceptType::Entity),
        ]);
        assert_eq!(
            anchor_content(&g, "what depends on cafe\u{301}").as_deref(),
            Some("café")
        );
        assert_eq!(
            anchor_content(&g, "what depends on ΟΣ").as_deref(),
            Some("ΟΣ")
        );
        assert_eq!(
            anchor_content(&g, "what depends on οσ").as_deref(),
            Some("ΟΣ")
        );
        assert_eq!(anchor_content(&g, "what depends on x\u{307}y"), None);
        let decomposed = anchors(&[(10, "cafe\u{301}", ConceptType::Entity)]);
        assert_eq!(
            anchor_content(&decomposed, "what depends on CAFÉ").as_deref(),
            Some("cafe\u{301}")
        );
    }

    #[test]
    fn punctuation_only_concepts_never_anchor() {
        let g = anchors(&[
            (10, "?", ConceptType::Entity),
            (11, "...", ConceptType::Resource),
        ]);
        assert_eq!(anchor_content(&g, "what depends on unknown...?"), None);
    }

    #[test]
    fn marker_overlapping_clause_cannot_hide_its_named_subject() {
        let g = anchors(&[
            (10, "safe to delete db", ConceptType::Logic),
            (11, "db", ConceptType::Entity),
        ]);
        assert_eq!(
            anchor_content(&g, "is it safe to delete db").as_deref(),
            Some("db")
        );
    }

    #[test]
    fn repetitive_anchor_spans_and_oversized_queries_refuse_in_bounded_time() {
        let names: Vec<_> = (1..=30).map(|n| vec!["x"; n].join(" ")).collect();
        let concepts: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(n, s)| (n as u64 + 10, s.as_str(), ConceptType::Entity))
            .collect();
        let g = anchors(&concepts);
        let q = format!("what depends on {}", "x ".repeat(8000));
        let start = std::time::Instant::now();
        assert_eq!(anchor_content(&g, &q), None);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "bounded repeated spans took {:?}",
            start.elapsed()
        );
        assert_eq!(
            anchor_content(&g, &format!("what depends on {}", "x ".repeat(9000))),
            None
        );
        assert!(word_bounded_spans(&q, "x").len() <= MAX_ANCHOR_SPANS + 1);
    }

    #[test]
    fn word_bounded_spans_need_word_edges_and_see_overlaps() {
        assert_eq!(word_bounded_spans("dbadmin db", "db"), vec![(8, 10)]);
        assert_eq!(word_bounded_spans("dbadmin", "db"), vec![]);
        assert_eq!(word_bounded_spans("user_db", "db"), vec![]);
        // Overlapping occurrences are all tried: a non-overlapping scan
        // would stop after (0, 5) and miss (3, 8).
        assert_eq!(
            word_bounded_spans("aa aa aa", "aa aa"),
            vec![(0, 5), (3, 8)]
        );
        // A punctuation edge is its own delimiter.
        assert_eq!(word_bounded_spans("prod.env", ".env"), vec![(4, 8)]);
        assert_eq!(word_bounded_spans("", "db"), vec![]);
        assert_eq!(word_bounded_spans("db", ""), vec![]);
    }

    /// The reported bug: a short concept embedded in a longer word anchored
    /// any structural query, admitting all its dependents.
    #[test]
    fn short_concept_inside_a_longer_word_does_not_anchor() {
        let g = anchors(&[
            (10, "db", ConceptType::Entity),
            (11, "a", ConceptType::Entity),
        ]);
        assert_eq!(anchor_content(&g, "what depends on dbadmin"), None);
        assert_eq!(
            try_structural(&g, "what depends on dbadmin", 5, 500),
            None,
            "an embedded short concept must not dispatch the traversal"
        );
        assert!(!fits_structural(&g, "what depends on dbadmin"));
        // "a" is a stop word as well as a substring of "what"/"safe": it never
        // anchors on scaffolding.
        assert_eq!(anchor_content(&g, "is it safe to delete a cache"), None);
    }

    /// A short concept that IS a whole word in the query still anchors when
    /// it is the only (hence best) match.
    #[test]
    fn short_whole_word_concept_anchors_when_it_is_the_best_match() {
        let g = anchors(&[
            (10, "db", ConceptType::Entity),
            (11, "id", ConceptType::Entity),
        ]);
        assert_eq!(
            anchor_content(&g, "what depends on db").as_deref(),
            Some("db")
        );
        assert_eq!(
            anchor_content(&g, "what depends on DB?").as_deref(),
            Some("db")
        );
        assert_eq!(
            anchor_content(&g, "blast radius of the id").as_deref(),
            Some("id")
        );
        let hit = try_structural(&g, "what depends on db", 5, 500).expect("dispatches");
        assert_eq!(content_of(&hit.hits), vec!["db consumer"]);
    }

    /// The intended longer anchor wins over a short concept that is one of
    /// its words, whatever the short one's type.
    #[test]
    fn the_longer_named_anchor_wins_over_a_word_inside_it() {
        for long_ty in [ConceptType::Entity, ConceptType::Logic] {
            let g = anchors(&[
                (10, "db", ConceptType::Entity),
                (11, "user db schema", long_ty),
                (12, "schema", ConceptType::Entity),
            ]);
            assert_eq!(
                anchor_content(&g, "what depends on user db schema").as_deref(),
                Some("user db schema"),
                "{long_ty:?}"
            );
            let hit =
                try_structural(&g, "what depends on user db schema", 5, 500).expect("dispatches");
            assert_eq!(content_of(&hit.hits), vec!["user db schema consumer"]);
            // The short concept still anchors where it stands alone.
            assert_eq!(
                anchor_content(&g, "what depends on user db schema or db").as_deref(),
                Some("user db schema"),
                "the first named subject wins over a later free-standing db"
            );
        }
        // Disjoint Entity matches: the longer (more specific) one wins.
        let g = anchors(&[
            (10, "cache", ConceptType::Entity),
            (11, "orders queue", ConceptType::Entity),
        ]);
        assert_eq!(
            anchor_content(&g, "what depends on cache and orders queue").as_deref(),
            Some("cache")
        );
    }

    #[test]
    fn multibyte_anchors_match_on_unicode_word_boundaries() {
        let g = anchors(&[
            (10, "café", ConceptType::Entity),
            (11, "Café-DB", ConceptType::Entity),
            (12, "数据", ConceptType::Entity),
            (13, "Ωmega", ConceptType::Entity),
        ]);
        // Case-insensitive across a multibyte letter; "café" is a word of the
        // longer match and drops.
        assert_eq!(
            anchor_content(&g, "what depends on CAFÉ-DB now").as_deref(),
            Some("Café-DB")
        );
        // An accented letter is a word character: no match inside "cafébar".
        assert_eq!(anchor_content(&g, "what depends on cafébar"), None);
        // CJK runs are one word: "数据" does not match inside "数据库".
        assert_eq!(anchor_content(&g, "what depends on 数据库"), None);
        assert_eq!(
            anchor_content(&g, "what depends on 数据").as_deref(),
            Some("数据")
        );
        assert_eq!(
            anchor_content(&g, "blast radius of ωMEGA?").as_deref(),
            Some("Ωmega")
        );
        assert_eq!(anchor_content(&g, "blast radius of ωmegas"), None);
    }

    #[test]
    fn punctuation_and_path_anchors_match_inside_a_query() {
        let g = anchors(&[
            (10, "src/main.rs", ConceptType::Resource),
            (11, "main.rs", ConceptType::Resource),
            (12, ".env", ConceptType::Resource),
            (13, "rs", ConceptType::Entity),
        ]);
        assert_eq!(
            anchor_content(&g, "what depends on src/main.rs?").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            anchor_content(&g, "what depends on `src/main.rs`").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            anchor_content(&g, "blast radius of main.rs").as_deref(),
            Some("main.rs")
        );
        assert_eq!(
            anchor_content(&g, "what uses .env").as_deref(),
            Some(".env")
        );
        assert_eq!(
            anchor_content(&g, "what uses config/.env").as_deref(),
            Some(".env")
        );
        // "main.rs" must not match inside "domain.rs".
        assert_eq!(
            anchor_content(&g, "what depends on domain.rs").as_deref(),
            Some("rs"),
            "only the whole-word rs matches"
        );
    }

    /// The question's own wording never anchors: a concept named like a
    /// marker phrase, or made only of function words, is skipped.
    #[test]
    fn marker_and_stop_word_concepts_do_not_anchor_on_question_scaffolding() {
        let g = anchors(&[
            (10, "blast radius", ConceptType::Entity),
            (11, "user schema", ConceptType::Entity),
            (12, "it", ConceptType::Entity),
            (13, "the", ConceptType::Entity),
        ]);
        assert_eq!(
            anchor_content(&g, "blast radius of user schema").as_deref(),
            Some("user schema")
        );
        assert_eq!(
            anchor_content(&g, "is it safe to delete the payments queue"),
            None
        );
        // Any "blast radius" in a query is itself a marker occurrence, so the
        // bare concept never anchors; a longer concept that only starts with
        // the marker words is not inside the marker span and does.
        assert_eq!(anchor_content(&g, "what depends on blast radius"), None);
        let g = anchors(&[(14, "blast radius report", ConceptType::Resource)]);
        assert_eq!(
            anchor_content(&g, "what depends on blast radius report").as_deref(),
            Some("blast radius report")
        );
    }

    #[test]
    fn instrumentation_reports_per_hit_arm_contributions() {
        use crate::test_util::capture_logs;

        let g = exhibit();
        let (logs, _guard) = capture_logs(tracing::Level::TRACE);

        // Drive the REAL general pipeline over the same graph to expose the
        // per-hit d/r/final contributions (the T9 instrumentation in
        // `assemble`), plus the dispatch classification trace.
        let mut index = crate::graph::index::InvertedIndex::new();
        for c in g.concepts() {
            index.add(c);
        }
        let phase1 = crate::recall::candidates::candidates(
            &g,
            &index,
            crate::recall::candidates::Phase1Input::default(),
            "what depends on SG-Base-VPC",
            5,
        );
        let expanded = crate::recall::expand::expand(&g, phase1.clone(), 2);
        let _blended = crate::recall::assemble::assemble(
            &g,
            &expanded,
            &phase1,
            &crate::types::ScoreTable::default(),
            &std::collections::HashMap::new(),
            &query("what depends on SG-Base-VPC"),
            crate::config::RecallWeights::default(),
            ts(0),
            crate::recall::assemble::default_token_count,
        );
        let result = try_structural(&g, "what depends on SG-Base-VPC", 5, 500)
            .expect("structural question must dispatch");
        assert!(
            content_of(&result.hits).contains(&"RDS-Lambo-Demo-DB"),
            "the dependent must be surfaced in result.hits (N3)"
        );

        let text = logs.contents();
        assert!(
            text.contains("recall dispatch: classified query arm"),
            "dispatch classification must be instrumented"
        );
        assert!(
            text.contains("recall arm"),
            "per-hit arm contributions must be instrumented, got: {text}"
        );
        // The structural dependent must be visible in the instrumented output.
        assert!(
            text.contains("RDS-Lambo-Demo-DB"),
            "the dependent must appear in instrumented output, got: {text}"
        );
    }

    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn recall_entry_dispatches_structural_query() {
        // End-to-end through Daemon::recall: a structural query short-circuits
        // to the traversal answer.
        use crate::daemon::Daemon;
        use crate::recall::cache::RecallCache;

        let g = exhibit();
        let graph = Arc::new(RwLock::new(g));
        let mut index = crate::graph::index::InvertedIndex::new();
        for c in graph.read().concepts() {
            index.add(c);
        }
        let daemon = Daemon::new(
            graph.clone(),
            crate::config::ScoringWeights::default(),
            std::time::Duration::from_millis(1000),
        )
        .with_index(Arc::new(RwLock::new(index)));

        let store = crate::fixtures::load_store("session-rest-api").unwrap();
        let mut cache = RecallCache::new();
        let res = daemon
            .recall(
                &sid(),
                query("what depends on SG-Base-VPC"),
                &store,
                None,
                crate::config::RecallWeights::default(),
                &mut cache,
            )
            .await;
        let contents: Vec<&str> = res.hits.iter().map(|h| h.content.as_str()).collect();
        assert!(
            contents.contains(&"RDS-Lambo-Demo-DB"),
            "Daemon::recall must dispatch the dependency question, got {contents:?}"
        );
    }

    /// A dedicated structural graph: a canonical `users handler` and a plain
    /// `payments handler` both depend on `app router`,
    /// so the traversal answer exercises a load-bearing annotation and a
    /// full status on structural hits.
    fn structural_exhibit() -> Graph {
        let mut g = Graph::new(sid());
        let i1 = NodeId(uuid::Uuid::from_u64_pair(1, 1));
        g.insert_interaction(Interaction {
            event_time: None,
            id: i1,
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("route the API".into()),
            previous_id: None,
            created_at: ts(0),
        })
        .unwrap();
        for (id, content) in [
            (1u64, "app router"),
            (2, "users handler"),
            (3, "payments handler"),
        ] {
            g.insert_concept(concept(id, content, ConceptType::Entity), i1)
                .unwrap();
        }
        // Walk the users handler to Canonical through the audited path.
        for (from, to) in [
            (CanonizationStatus::None, CanonizationStatus::Candidate),
            (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ] {
            g.apply_canonization_transition(crate::types::CanonizationEvent {
                id: NodeId::new(),
                session_id: sid(),
                node_id: NodeId(uuid::Uuid::from_u64_pair(2, 2)),
                from_status: from,
                to_status: to,
                blast_radius: None,
                last_demotion_time: None,
                occurred_at: ts(0),
            })
            .unwrap();
        }
        // Both handlers depend on the router: a Dependency edge's source
        // rests on its target (#36).
        g.upsert_edge(edge(1, 2, 1, EdgeType::Dependency, 5.0))
            .unwrap();
        g.upsert_edge(edge(2, 3, 1, EdgeType::Dependency, 3.0))
            .unwrap();
        g
    }

    /// The H3 structural golden — a dispatched dependency question carries
    /// one hit with a `load_bearing` annotation (its Canonical status from
    /// the same graph snapshot), one annotation-free hit, and the traversal
    /// explanation as a single response-global annotation.
    #[test]
    fn h3_structural_payload_matches_golden() {
        let g = structural_exhibit();
        let result = try_structural(&g, "what depends on app router", 5, 500)
            .expect("structural question must dispatch");

        assert_eq!(result.response_annotations.len(), 1);
        assert_eq!(
            result.response_annotations[0].kind,
            AnnotationKind::Traversal
        );
        assert_eq!(
            result.response_annotations[0].text,
            "recall: dependency question answered by graph traversal (2 dependents)"
        );
        let users = result
            .detailed
            .iter()
            .find(|h| h.content == "users handler")
            .unwrap();
        assert_eq!(users.status, Some(CanonizationStatus::Canonical));
        assert_eq!(users.annotations[0].kind, AnnotationKind::LoadBearing);
        let payments = result
            .detailed
            .iter()
            .find(|h| h.content == "payments handler")
            .unwrap();
        assert!(payments.annotations.is_empty());
        assert!(result.detailed.iter().all(|h| h.included_in_context));
        // Response-global explanations never appear as hit annotations.
        assert!(result.detailed.iter().all(|h| {
            !h.annotations
                .iter()
                .any(|a| a.kind == AnnotationKind::Traversal)
        }));

        let actual = serde_json::to_value(&result).expect("payload serializes");
        let golden_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/recall-h3-goldens.json"
        );
        let golden: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(golden_path).expect("golden fixture present"),
        )
        .expect("golden parses");
        assert_eq!(
            actual, golden["structural"],
            "structural structured payload must match the golden"
        );
    }

    // -----------------------------------------------------------------------
    // #130: the shared budget cut (`format::fit_blocks`)
    // -----------------------------------------------------------------------

    /// A 6 KB `big handler` (the strongest dependent, so ranked first) and a
    /// short `payments handler` both depend on `app router` (#36).
    fn oversized_first_dependent() -> Graph {
        let mut g = Graph::new(sid());
        let i1 = NodeId(uuid::Uuid::from_u64_pair(1, 1));
        g.insert_interaction(Interaction {
            event_time: None,
            id: i1,
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("route the API".into()),
            previous_id: None,
            created_at: ts(0),
        })
        .unwrap();
        let big = format!("big handler {}", "détail 🦀 ".repeat(500));
        for (id, content) in [
            (1u64, "app router".to_string()),
            (2, big),
            (3, "payments handler".to_string()),
        ] {
            g.insert_concept(concept(id, &content, ConceptType::Entity), i1)
                .unwrap();
        }
        // Both handlers depend on the router: a Dependency edge's source
        // rests on its target (#36).
        g.upsert_edge(edge(1, 2, 1, EdgeType::Dependency, 5.0))
            .unwrap();
        g.upsert_edge(edge(2, 3, 1, EdgeType::Dependency, 3.0))
            .unwrap();
        g
    }

    /// The blocks a result's hits render, as `fit_blocks` takes them.
    fn rendered_blocks(result: &DetailedRecall) -> Vec<format::ContextBlock> {
        result
            .detailed
            .iter()
            .zip(&result.hits)
            .map(|(d, h)| {
                let lines: Vec<String> = d.annotations.iter().map(|a| a.text.clone()).collect();
                let block = format::ContextBlock::for_hit(h, &lines);
                assert_eq!(block.text, format::render_detailed_block(d));
                block
            })
            .collect()
    }

    #[test]
    fn structural_oversized_first_dependent_is_truncated_not_dropped() {
        let g = oversized_first_dependent();
        let result = try_structural(&g, "what depends on app router", 5, 500)
            .expect("structural question must dispatch");
        assert_eq!(result.hits.len(), 2, "hits are never cut by the budget");
        assert!(result.hits[0].content.starts_with("big handler"));

        let big_id = format::short_id(result.hits[0].node_id);
        assert!(
            result.context.starts_with("big handler détail"),
            "{}",
            result.context
        );
        assert!(
            result
                .context
                .ends_with(&format!("lambo_inspect {big_id} for the full concept]")),
            "{}",
            result.context
        );
        // The label survives the cut (M1): content head, `…`, then the
        // block's own label line.
        assert!(
            result.context.contains("… [Entity] (score "),
            "{}",
            result.context
        );
        assert_eq!(result.truncation, Some(format::Truncation::WithWarnings));
        assert!(format::default_token_count(&result.context) <= 500);
        assert!(
            !result.context.contains("payments handler"),
            "a lower-ranked block never follows a truncated one"
        );
        assert!(result.detailed[0].truncated_in_context);
        assert!(!result.detailed[0].included_in_context);
        assert!(
            result.detailed[1..]
                .iter()
                .all(|h| !h.included_in_context && !h.truncated_in_context)
        );
        // The headline counts the truncated dependent the context shows.
        assert_eq!(
            result.response_annotations[0].text,
            "recall: dependency question answered by graph traversal (1 dependents)"
        );
        // Additive wire: the flag is serialized on the truncated hit only.
        let wire = serde_json::to_value(&result.detailed).unwrap();
        assert_eq!(wire[0]["truncated_in_context"], serde_json::json!(true));
        assert!(wire[1].get("truncated_in_context").is_none());

        // Budget 0 and 1 still render nothing (below the marker's size).
        for budget in [0, 1] {
            let r = try_structural(&g, "what depends on app router", 5, budget).unwrap();
            assert_eq!(r.context, "", "budget {budget}");
            assert!(r.detailed.iter().all(|h| !h.truncated_in_context));
        }
    }

    /// Both cut sites (structural dispatch and blended assembly) run the
    /// one shared `fit_blocks`: over the same concepts and every budget in a
    /// sweep that crosses all three regimes (empty, truncated first block,
    /// whole blocks), each site's context and per-hit flags are exactly
    /// `fit_blocks` over the blocks it rendered, and the two sites agree on
    /// the regime and on the id the marker names.
    #[test]
    fn assemble_and_dispatch_cut_the_budget_identically() {
        use crate::config::RecallWeights;
        use crate::recall::expand::ExpandedSet;
        use crate::types::{ScoreTable, Scored};
        use std::collections::HashMap;

        let g = oversized_first_dependent();
        let big = NodeId(uuid::Uuid::from_u64_pair(2, 2));
        let payments = NodeId(uuid::Uuid::from_u64_pair(2, 3));
        let expanded = ExpandedSet {
            required: vec![Scored::new(big, 0.0), Scored::new(payments, 0.0)],
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(big, 1.0), Scored::new(payments, 0.6)],
        };
        let hot = HashMap::new();

        let check = |site: &str, result: &DetailedRecall, budget: usize| {
            let fitted = format::fit_blocks(
                &rendered_blocks(result),
                budget,
                format::default_token_count,
            );
            assert_eq!(result.context, fitted.context, "{site} budget {budget}");
            for (i, h) in result.detailed.iter().enumerate() {
                assert_eq!(
                    h.included_in_context,
                    i < fitted.whole,
                    "{site} budget {budget}"
                );
                assert_eq!(
                    h.truncated_in_context,
                    fitted.truncation.is_some() && i == 0,
                    "{site} budget {budget}"
                );
            }
            fitted
        };

        let mut regimes = std::collections::BTreeSet::new();
        for budget in (0..40).chain((40..2_500).step_by(37)).chain([10_000]) {
            let structural = try_structural(&g, "what depends on app router", 5, budget).unwrap();
            let blended = crate::recall::assemble::assemble(
                &g,
                &expanded,
                &[],
                &scores,
                &hot,
                &RecallQuery {
                    query: "irrelevant".into(),
                    top_k: 5,
                    max_tokens: budget,
                    traversal_depth: 2,
                },
                RecallWeights::default(),
                ts(0),
                format::default_token_count,
            );
            assert_eq!(structural.hits[0].node_id, big);
            assert_eq!(blended.hits[0].node_id, big);
            let s = check("dispatch", &structural, budget);
            let b = check("assemble", &blended, budget);
            assert_eq!(s.truncation, b.truncation, "budget {budget}");
            assert_eq!(structural.truncation, s.truncation, "budget {budget}");
            assert_eq!(blended.truncation, b.truncation, "budget {budget}");
            assert_eq!(s.rendered() == 0, b.rendered() == 0, "budget {budget}");
            if s.truncation.is_some() {
                let tail = format!(
                    "lambo_inspect {} for the full concept]",
                    format::short_id(big)
                );
                assert!(s.context.ends_with(&tail) && b.context.ends_with(&tail));
            }
            regimes.insert((s.whole.min(1), s.truncation.is_some()));
        }
        assert_eq!(
            regimes.len(),
            3,
            "the sweep must cover empty, truncated and whole: {regimes:?}"
        );
    }
}
