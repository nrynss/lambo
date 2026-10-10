//! Phase-3 scoring, hot-list force-inclusion, and assembly to `max_tokens`
//! (T5.3, spec §8) — the final read-path stage.
//!
//! `assemble` turns the phase-2 [`ExpandedSet`] into a `RecallResult`:
//! every member (required AND `chunk_group_id` siblings, "scored
//! independently") gets a final score, hot-listed members are force-included
//! after condition re-validation, and the rendered context is truncated to
//! the query's token budget.
//!
//! ## Scoring rule
//!
//! ```text
//! final_score = daemon_score × w_daemon + query_relevance × w_query
//! ```
//!
//! While a query-backed phase-1 concept is absent from the daemon score table, all
//! expanded members use `query_relevance × w_query` for this recall. Ranking
//! only the new member by its query score would promote fresh noise above an
//! established relevant concept with a low structural score. The temporary
//! query-only mode gives both the same ordering rule until the daemon scores
//! the new concept. An explicit daemon score of zero is present, not missing.
//! A daemon-only configuration (`w_query == 0`) continues to use daemon scores.
//! Once the daemon catches up, normal blended scoring resumes and ranks may
//! change. Canonical-first and hot-list force-inclusion still apply.
//!
//! * **query_relevance** — the member's phase-1 candidate score (BM25 for
//!   keyword hits, the max-merged score otherwise). Members that were never
//!   phase-1 candidates — BFS-reached concepts and force-included
//!   `chunk_group_id` siblings — score **0.0**: they were not keyword hits,
//!   so phase 1 has no relevance evidence for them; their only signal is the
//!   daemon score. Siblings are deliberately scored this way (spec §8
//!   "force-included, scored independently"), not silently dropped.
//! * **daemon_score** — the [`ScoreTable`] lookup by node id. A node missing
//!   from the table scores **0.0** in normal blended mode. If a query-backed phase-1
//!   candidate lacks an entry, the temporary query-only rule above applies
//!   to every expanded member until the next daemon score table arrives.
//! * **weights** — [`RecallWeights`], sanitized like `ScoringWeights`
//!   (ALGO-10): a non-finite or negative weight becomes `0.0`, so the final
//!   score is finite for every input.
//!
//! Hits are sorted by `final_score` descending, ties broken by canonical key
//! ascending then node id ascending (the same total order phase 1 uses),
//! **after** the canonical partition below.
//!
//! ## Canonical-first ordering (spec §10)
//!
//! Spec §10: "**Canonical nodes are:** eviction-immune; **always promoted
//! first** with `is_canonical=True`; marked `[canonical]` in recall output
//! with a blast-radius warning". The marker, the flag and the ⚑ warning
//! landed in P5; "always promoted first" had no owner in any phase plan, so a
//! Canonical concept that fell below the `top_k` cut was silently absent from
//! the read the whole tier exists to produce. Canonization decides what is
//! load-bearing; recall is where that decision is supposed to show up.
//!
//! So the sort key is `(is_canonical desc, final_score desc, canonical key
//! asc, node id asc)`: every Canonical member of the expanded set is ranked
//! ahead of every non-Canonical one, and score order applies within each
//! group. The key tie-break keeps equal-score members in one order across
//! runs (issue #2); the id stays as the final fallback because non-canonical
//! synonym duplicates share a key and interaction-adjacent members may lack
//! the concept context entirely.
//!
//! **Why ordering rather than a rank boost.** A boost is a magic constant
//! added to a formula spec §8 states exactly (`daemon_score × w_daemon +
//! query_relevance × w_query`) — it would make the reported `score` no longer
//! mean what the spec says it means, and it would not implement the rule
//! anyway: any *finite* boost can still be out-ranked by a high enough
//! non-canonical score, so "always" would hold only up to a tuning accident.
//! Partitioning is exact, keeps `score` honest, and needs no constant.
//!
//! **Why ordering rather than unbounded force-inclusion.** Hot-listed members
//! are force-included past `top_k` because a live conflict warning is
//! time-critical and rare. Canonical membership is neither: a session at its
//! `max_canonical_nodes=1000` ceiling would blow past every `top_k` and turn
//! the budget into the real output bound. Ranking first already guarantees
//! presence for the first `top_k` Canonicals, which is what "promoted first"
//! asks for; beyond that the token budget is the binding constraint and
//! honouring it is the point of phase 3.
//!
//! ## Hot-list force-include
//!
//! Before assembly the caller re-validates every expanded member that is on
//! the daemon's hot list **at the same `now` it passes here** (the `now` used
//! for everything else in the call: reservations, rendering), through
//! `HotList::revalidate_members` in `Daemon::recall_detailed`. The predicate
//! re-derives its recency window from that instant, so an entry whose window
//! elapsed between detection and this read is dropped there (XP-3), and a
//! surviving entry's payload has just been rebuilt against `now`. Assemble
//! takes the resulting payload map and renders it directly, never a cached
//! copy; it does not touch the hot list itself, so recall does not depend on
//! the daemon. Members in the map are **force-included**: they stay in the
//! hit list even beyond `top_k`, so a live conflict warning is never
//! truncated away by rank. The per-entry re-validation is a single
//! neighborhood walk (CONC-5), so a handful of hot nodes under the graph lock
//! is cheap.
//!
//! ## Assembly to `max_tokens`
//!
//! The hit list holds the first `top_k` scored members plus every
//! force-included hot member, in final-score order. Each hit renders as a
//! whole block ([`crate::recall::format`]); the context is truncated to
//! `max_tokens` by keeping the **longest score-ordered prefix** of blocks
//! whose cumulative `token_fn` count fits — equivalently, whole
//! lowest-scoring blocks are dropped from the tail. A block is never split,
//! and a dropped block still appears in `RecallResult::hits` and contributes
//! its warnings (a warning is actionable regardless of the token budget; the
//! caller renders `context` and `warnings` separately).
//!
//! The default estimator is [`default_token_count`] (`ceil(bytes / 3.5)`);
//! callers pass their own `Fn(&str) -> usize` to override.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::config::RecallWeights;
use crate::graph::reserve::active_reservation;
use crate::graph::Graph;
use crate::recall::candidates::LegScores;
use crate::recall::detail::{Annotation, AnnotationKind, DetailedHit, DetailedRecall};
use crate::recall::expand::ExpandedSet;
use crate::recall::format;
use crate::types::{
    tie_break_by_key, CanonizationStatus, HotListPayload, Node, NodeId, RecallHit, RecallQuery,
    ScoreTable, Scored,
};

/// The built-in token estimator (see [`crate::recall::format`]).
pub use crate::recall::format::default_token_count;

/// Score + assemble the expanded set into the final recall result.
///
/// `phase1` is the phase-1 candidate list (query relevance source), `scores`
/// the daemon's [`ScoreTable`], `hot_payloads` the hot-list payloads of the
/// expanded members that survived re-validation (already rebuilt at `now`;
/// every member in it is force-included), `query` carries `top_k` /
/// `max_tokens`, and `now` is the caller's clock — pass the same instant the
/// hot list was re-validated at, and that every other time-sensitive read in
/// the recall uses (reservations).
#[cfg(test)]
#[allow(clippy::too_many_arguments)] // test and direct callers supply query-backed phase-1 hits
pub(crate) fn assemble<F>(
    graph: &Graph,
    expanded: &ExpandedSet,
    phase1: &[Scored<NodeId>],
    scores: &ScoreTable,
    hot_payloads: &HashMap<NodeId, Vec<HotListPayload>>,
    query: &RecallQuery,
    weights: RecallWeights,
    now: DateTime<Utc>,
    token_fn: F,
) -> DetailedRecall
where
    F: Fn(&str) -> usize,
{
    assemble_with_legs(
        graph,
        expanded,
        phase1,
        None,
        scores,
        hot_payloads,
        query,
        weights,
        now,
        token_fn,
    )
}

/// Production assembly also receives phase-1 leg provenance. A recent-only
/// unscored hit is not evidence of a fresh match and cannot switch the whole
/// result to query-only ordering. `None` is for direct callers that already
/// supplied query-backed phase-1 hits.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_with_legs<F>(
    graph: &Graph,
    expanded: &ExpandedSet,
    phase1: &[Scored<NodeId>],
    legs: Option<&HashMap<NodeId, LegScores>>,
    scores: &ScoreTable,
    hot_payloads: &HashMap<NodeId, Vec<HotListPayload>>,
    query: &RecallQuery,
    weights: RecallWeights,
    now: DateTime<Utc>,
    token_fn: F,
) -> DetailedRecall
where
    F: Fn(&str) -> usize,
{
    let (w_daemon, w_query) = (sane_weight(weights.w_daemon), sane_weight(weights.w_query));
    let relevance: HashMap<NodeId, f64> = phase1.iter().map(|s| (s.item, s.score)).collect();
    let daemon: HashMap<NodeId, f64> = scores.ranked.iter().map(|s| (s.item, s.score)).collect();
    // Only a real query-backed phase-1 concept can put recall in cold mode.
    // The recent-only leg is a flat floor, not evidence of a new match. BFS
    // members, siblings and stale vector ids do not trigger it either.
    // Presence matters: an explicit daemon zero has already been scored.
    let cold = w_query > 0.0
        && phase1.iter().any(|candidate| {
            let query_backed = legs.is_none_or(|legs| {
                legs.get(&candidate.item)
                    .is_some_and(|leg| leg.keyword.is_some() || leg.vector.is_some())
            });
            query_backed
                && matches!(graph.node(candidate.item), Some(Node::Concept(_)))
                && !daemon.contains_key(&candidate.item)
        });

    // Score every member independently; sort by final score desc, id asc.
    let mut members: Vec<Scored<NodeId>> = expanded
        .required
        .iter()
        .chain(expanded.siblings.iter())
        .cloned()
        .collect();
    for s in &mut members {
        // d and r are multiplied unguarded, so sanitize them like the weights
        // (module doc: final score is finite for every input - a non-finite
        // store-provided relevance must not poison the total order, GPT5.6sol
        // P1-4 / deep-review F3).
        let d = sane_weight(daemon.get(&s.item).copied().unwrap_or(0.0));
        let r = sane_weight(relevance.get(&s.item).copied().unwrap_or(0.0));
        let daemon_part = if cold { 0.0 } else { d * w_daemon };
        s.score = daemon_part + r * w_query;
        // T9 instrumentation (default-invisible trace): attribute the final
        // score to its arms. A member with `r == 0` was never a phase-1
        // candidate — it reached the assembled block purely by structural
        // expansion (BFS over structural edges / chunk-group siblings), so it
        // contributes `d*w_daemon` (the structural arm) and nothing to the
        // query arm. A member with `r > 0` carries a query-relevance arm
        // (lexical keyword BM25 / vector similarity / recent flat score).
        // The arm/content formatting and the `graph.node` lookup run only when
        // a trace subscriber is present (T9-R1-4) - never per-member with
        // tracing disabled.
        if tracing::enabled!(target: "lambo::recall", tracing::Level::TRACE) {
            let arm = if r > 0.0 {
                "lexical/vector"
            } else {
                "structural"
            };
            let content = match graph.node(s.item) {
                Some(Node::Concept(c)) => c.content.as_str(),
                _ => "<non-concept>",
            };
            tracing::trace!(
                target: "lambo::recall",
                phase = "assemble",
                node = %s.item,
                content = %content,
                arm = arm,
                daemon = d,
                relevance = r,
                w_daemon = w_daemon,
                w_query = w_query,
                cold = cold,
                contrib_daemon = daemon_part,
                contrib_query = r * w_query,
                final = s.score,
                "recall arm {arm}: {content} daemon={d}*{w_daemon} relevance={r}*{w_query} final={}",
                s.score
            );
        }
    }
    // Spec §10 "always promoted first": Canonical members are partitioned
    // ahead of the rest, score order applies inside each group. See the
    // module docs for why this is a partition and not a score boost. Ties
    // fall to canonical key asc, then node id asc: the id alone is minted per
    // run, so it must not decide equal-score order (issue #2). Like
    // `is_canonical`, the key lookup runs inside the comparator; members that
    // are not graph concepts (stale durable-vector ids) have no key and fall
    // straight to the id.
    let is_canonical = |id: NodeId| {
        matches!(
            graph.node(id),
            Some(Node::Concept(c)) if c.canonization_status == CanonizationStatus::Canonical
        )
    };
    let key = |id: NodeId| match graph.node(id) {
        Some(Node::Concept(c)) => Some(c.canonical_key.as_str()),
        _ => None,
    };
    members.sort_by(|a, b| {
        is_canonical(b.item)
            .cmp(&is_canonical(a.item))
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| tie_break_by_key(key(a.item), &a.item, key(b.item), &b.item))
    });

    // top_k normal members (counted by VALID emitted hits — a graph-missing
    // member such as a stale durable-vector id must not consume a top_k slot,
    // GPT5.6sol P2-5), plus every force-included hot member, in score order.
    // Blast radii for every canonical hit, computed ONCE (P2-7).
    let radii = format::blast_radii(graph);
    let mut hits: Vec<RecallHit> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut detailed: Vec<DetailedHit> = Vec::new();
    let mut hit_blocks: Vec<String> = Vec::new(); // block per emitted hit, score order
    let mut emitted = 0usize; // valid non-forced hits accepted toward top_k
    for s in members {
        let forced = hot_payloads.contains_key(&s.item);
        let Some(Node::Concept(c)) = graph.node(s.item) else {
            continue; // graph-missing (e.g. stale vector id): skip, no slot
        };
        if !forced && emitted >= query.top_k {
            continue; // keep scanning: a force-included hot member may sort lower
        }

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
        // H3: the full status comes from the SAME graph snapshot the hit was
        // assembled from (never reconstructed from `is_canonical` or a later
        // store read), and each typed warning is attached here, where its
        // producer is still known, BEFORE the flat `warnings` extend below.
        // Status `None` is carried as absent (the wire contract).
        let mut detailed_hit = DetailedHit::new(
            &hit,
            (c.canonization_status != CanonizationStatus::None).then_some(c.canonization_status),
        );

        // Warning lines for this hit: ⚑ (canonical), hot-list conditions,
        // then the active reservation (soft lock). Each line carries its
        // pinned kind alongside the rendered text.
        let mut lines: Vec<String> = Vec::new();
        if canonical {
            let text = format::blast_radius_warning(hit.blast_radius.unwrap_or_default());
            detailed_hit
                .annotations
                .push(Annotation::new(AnnotationKind::LoadBearing, text.clone()));
            lines.push(text);
        }
        if let Some(payloads) = hot_payloads.get(&c.id) {
            for p in payloads {
                let kind = match p {
                    HotListPayload::Conflict { .. } => AnnotationKind::Conflict,
                    HotListPayload::HighRisk { .. }
                    | HotListPayload::Drift { .. }
                    | HotListPayload::Stale { .. } => AnnotationKind::Hot,
                };
                let text = format::hot_warning(p);
                detailed_hit
                    .annotations
                    .push(Annotation::new(kind, text.clone()));
                lines.push(text);
            }
        }
        if let Some(r) = active_reservation(graph, c.id, now) {
            let text = format::reservation_warning(r);
            detailed_hit
                .annotations
                .push(Annotation::new(AnnotationKind::Reservation, text.clone()));
            lines.push(text);
        }

        // Warning lines reflect the included hit set, independent of the token
        // budget (see module docs); a block truncated from the context still
        // reports its conditions.
        let block = format::render_block(&hit, &lines);
        warnings.extend(lines);
        hit_blocks.push(block);
        if !forced {
            emitted += 1;
        }
        hits.push(hit);
        detailed.push(detailed_hit);
    }

    // Context: ranked-prefix over the hit blocks in score order, stopping at
    // the first block that does not fit. The measured token count is of the
    // ACTUAL joined context (separators `\n\n` included), so the rendered
    // output is within budget; a lower-ranked block never follows a skipped
    // one (GPT5.6sol P1-4). Checked arithmetic keeps overflow a no-op stop.
    let mut blocks: Vec<String> = Vec::new();
    for block in hit_blocks {
        let mut provisional = blocks.join("\n\n");
        if !provisional.is_empty() {
            provisional.push('\n');
            provisional.push('\n');
        }
        provisional.push_str(&block);
        let tokens = token_fn(&provisional);
        if tokens.checked_add(1).is_none() || tokens > query.max_tokens {
            break;
        }
        blocks.push(block);
    }

    // H3: `included_in_context` is recorded AT the token-budget cut — true
    // exactly for the longest ranked prefix of hits whose complete rendered
    // blocks appear in the context. Every hit stays in `hits`; later ones
    // report `false` (their annotations remain, token exclusion never
    // discards them).
    let kept = blocks.len();
    for (i, d) in detailed.iter_mut().enumerate() {
        d.included_in_context = i < kept;
    }

    DetailedRecall {
        hits,
        context: format::render_context(&blocks),
        warnings,
        // I1: attached by the caller, which owns the phase-1 result this
        // assembly ran over. Assembly itself sees only the expanded member
        // list, so inventing legs here would mean guessing.
        legs: Default::default(),
        detailed,
        response_annotations: Vec::new(),
    }
}

/// Finite, non-negative weight, else `0.0` (mirrors ALGO-10 sanitization).
fn sane_weight(w: f64) -> f64 {
    if w.is_finite() && w >= 0.0 {
        w
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    use crate::daemon::hotlist::{Condition, HotList, HotListEntry};
    use crate::types::{AgentId, Concept, ConceptType, Interaction, Reservation, SessionId};

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    /// Concept ids (pair 2, mirroring the candidates.rs test convention).
    fn uid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, u))
    }

    /// Interaction ids (pair 1): disjoint from concept ids.
    fn iid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, u))
    }

    fn interaction(id: u64) -> Interaction {
        Interaction {
            event_time: None,
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: None,
            created_at: ts(0),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: uid(id),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
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

    /// Graph with concepts c1..=cn and one interaction.
    fn graph_with(n: u64) -> Graph {
        let mut g = Graph::new(sid());
        let i1 = interaction(1);
        g.insert_interaction(i1.clone()).unwrap();
        for id in 1..=n {
            g.insert_concept(concept(id, i1.id, &format!("concept {id}")), i1.id)
                .unwrap();
        }
        g
    }

    /// What `Daemon::recall_detailed` hands assembly: the expanded members'
    /// hot-list payloads, re-validated at `now` (a test-only daemon use).
    fn revalidated(
        hot: &mut HotList,
        g: &Graph,
        expanded: &ExpandedSet,
        now: DateTime<Utc>,
    ) -> HashMap<NodeId, Vec<HotListPayload>> {
        hot.revalidate_members(
            g,
            expanded
                .required
                .iter()
                .chain(expanded.siblings.iter())
                .map(|s| s.item),
            now,
        )
    }

    fn ids_of(result: &DetailedRecall) -> Vec<NodeId> {
        result.hits.iter().map(|h| h.node_id).collect()
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn query(top_k: usize, max_tokens: usize) -> RecallQuery {
        RecallQuery {
            query: "irrelevant".into(),
            top_k,
            max_tokens,
            traversal_depth: 2,
        }
    }

    // -----------------------------------------------------------------------
    // Scoring
    // -----------------------------------------------------------------------

    #[test]
    fn final_score_mixes_daemon_and_relevance_with_planted_weights() {
        let g = graph_with(6);
        // Expanded set: c1/c2 are phase-1 members; c3 is BFS-reached (no
        // phase-1 evidence); c4/c5/c6 are chunk siblings.
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: vec![
                Scored::new(uid(4), 0.0),
                Scored::new(uid(5), 0.0),
                Scored::new(uid(6), 0.0),
            ],
        };
        let phase1 = vec![
            Scored::new(uid(1), 1.0),
            Scored::new(uid(2), 0.5),
            Scored::new(uid(5), 0.2),
        ];
        // c2 has an EXPLICIT daemon score of 0.0 (scored, so the blend
        // applies; #79's cold mode is for a phase-1 concept with no entry, see
        // the next test); c6 has a daemon score but no phase-1 evidence
        // (-> relevance 0.0).
        let scores = ScoreTable {
            epoch: 7,
            ranked: vec![
                Scored::new(uid(1), 0.8),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.2),
                Scored::new(uid(4), 0.4),
                Scored::new(uid(5), 0.6),
                Scored::new(uid(6), 0.6),
            ],
        };
        let weights = RecallWeights {
            w_daemon: 0.25,
            w_query: 0.75,
        };

        let hot = HashMap::new();
        let result = assemble(
            &g,
            &expanded,
            &phase1,
            &scores,
            &hot,
            &query(10, 10_000),
            weights,
            ts(0),
            default_token_count,
        );

        // c1: 0.8×0.25 + 1.0×0.75 = 0.95
        // c2: 0.0×0.25 + 0.5×0.75 = 0.375 (explicit daemon zero)
        // c3: 0.2×0.25 + 0.0×0.75 = 0.05  (BFS member, no relevance)
        // c4: 0.4×0.25 + 0.0×0.75 = 0.10  (sibling, no relevance)
        // c5: 0.6×0.25 + 0.2×0.75 = 0.30
        // c6: 0.6×0.25 + 0.0×0.75 = 0.15
        let want_order = vec![uid(1), uid(2), uid(5), uid(6), uid(4), uid(3)];
        // Score desc; the six planted finals are all distinct, so the tie
        // arms below the score (canonical-first, then canonical key, then id)
        // do not fire in this test — the chain itself is pinned by
        // `final_score_ties_break_by_canonical_key_ahead_of_node_id`.
        assert_eq!(ids_of(&result), want_order);
        let score = |id: NodeId| {
            result
                .hits
                .iter()
                .find(|h| h.node_id == id)
                .expect("hit present")
                .score
        };
        assert!(approx(score(uid(1)), 0.95));
        assert!(approx(score(uid(2)), 0.375));
        assert!(approx(score(uid(5)), 0.30));
        assert!(approx(score(uid(6)), 0.15));
        assert!(approx(score(uid(4)), 0.10));
        assert!(approx(score(uid(3)), 0.05));
        // The id-asc tie-break is exercised in the dedicated test below.
        assert!(result.warnings.is_empty());
    }

    /// #79: the same planted fixture with c2 MISSING from the daemon table
    /// (it was derived after the table was computed). Before #79 a missing
    /// entry scored 0.0 inside the blend and this produced the previous
    /// test's order. Now one unscored query-backed phase-1 concept puts the
    /// whole recall in cold mode: every member scores `r × w_query`, so the
    /// structural-only members (c3 BFS, c4/c6 siblings) tie at zero and fall
    /// back to canonical key order. This is the legacy golden that #79
    /// changes, documented in the CHANGELOG and the fix-79 note.
    #[test]
    fn a_missing_phase1_daemon_score_puts_the_planted_fixture_in_cold_mode() {
        let g = graph_with(6);
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: vec![
                Scored::new(uid(4), 0.0),
                Scored::new(uid(5), 0.0),
                Scored::new(uid(6), 0.0),
            ],
        };
        let phase1 = vec![
            Scored::new(uid(1), 1.0),
            Scored::new(uid(2), 0.5),
            Scored::new(uid(5), 0.2),
        ];
        let scores = ScoreTable {
            epoch: 7,
            ranked: vec![
                Scored::new(uid(1), 0.8),
                Scored::new(uid(3), 0.2),
                Scored::new(uid(4), 0.4),
                Scored::new(uid(5), 0.6),
                Scored::new(uid(6), 0.6),
            ],
        };
        let result = assemble(
            &g,
            &expanded,
            &phase1,
            &scores,
            &HashMap::new(),
            &query(10, 10_000),
            RecallWeights {
                w_daemon: 0.25,
                w_query: 0.75,
            },
            ts(0),
            default_token_count,
        );
        // Before #79: c1,c2,c5,c6,c4,c3 at 0.95/0.375/0.30/0.15/0.10/0.05.
        // c1: 1.0×0.75 = 0.75; c2: 0.5×0.75 = 0.375; c5: 0.2×0.75 = 0.15;
        // c3, c4, c6: 0.0, tied, canonical key order.
        assert_eq!(
            ids_of(&result),
            vec![uid(1), uid(2), uid(5), uid(3), uid(4), uid(6)]
        );
        let score = |id: NodeId| {
            result
                .hits
                .iter()
                .find(|h| h.node_id == id)
                .expect("hit present")
                .score
        };
        assert!(approx(score(uid(1)), 0.75));
        assert!(approx(score(uid(2)), 0.375));
        assert!(approx(score(uid(5)), 0.15));
        assert!(approx(score(uid(3)), 0.0));
        assert!(approx(score(uid(4)), 0.0));
        assert!(approx(score(uid(6)), 0.0));
        assert!(result.warnings.is_empty());
    }

    // -----------------------------------------------------------------------
    // Canonical-first ordering (spec §10 "always promoted first")
    // -----------------------------------------------------------------------

    /// Mark `ids` Canonical through the write path (the state machine forbids
    /// skipping stages, so each concept walks None -> Candidate -> Venerable
    /// -> Canonical).
    fn canonize(g: &mut Graph, ids: &[u64]) {
        use crate::types::CanonizationEvent;
        for &id in ids {
            for (from, to) in [
                (CanonizationStatus::None, CanonizationStatus::Candidate),
                (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
                (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
            ] {
                g.apply_canonization_transition(CanonizationEvent {
                    id: NodeId::new(),
                    session_id: sid(),
                    node_id: uid(id),
                    from_status: from,
                    to_status: to,
                    blast_radius: None,
                    last_demotion_time: None,
                    occurred_at: ts(0),
                })
                .unwrap();
            }
        }
    }

    /// Spec §10: a Canonical node ranks ahead of every non-Canonical one,
    /// whatever the scores say. Here c1 is Canonical with the *lowest* daemon
    /// score in the set, so pure score order would put it last.
    #[test]
    fn canonical_members_rank_ahead_of_higher_scoring_peers() {
        let mut g = graph_with(4);
        canonize(&mut g, &[1]);
        let expanded = ExpandedSet {
            required: (1..=4).map(|i| Scored::new(uid(i), 0.0)).collect(),
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 0.1),
                Scored::new(uid(2), 0.9),
                Scored::new(uid(3), 0.8),
                Scored::new(uid(4), 0.7),
            ],
        };
        let hot = HashMap::new();
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot,
            &query(10, 10_000),
            RecallWeights {
                w_daemon: 1.0,
                w_query: 0.0,
            },
            ts(0),
            default_token_count,
        );
        assert_eq!(
            ids_of(&result),
            vec![uid(1), uid(2), uid(3), uid(4)],
            "the Canonical node is promoted first; the rest stay score-ordered"
        );
        assert!(result.hits[0].is_canonical);
        assert!(
            approx(result.hits[0].score, 0.1),
            "ordering must not rewrite the spec §8 score: {}",
            result.hits[0].score
        );
    }

    /// The failure the rule exists to prevent: a Canonical concept below the
    /// `top_k` cut was silently absent from the read. Four members, `top_k =
    /// 2`, and the Canonical one scores worst — it must still be emitted (and
    /// carry its `[canonical]` marker into the rendered context).
    #[test]
    fn canonical_member_below_the_top_k_cut_is_still_returned() {
        let mut g = graph_with(4);
        canonize(&mut g, &[4]);
        let expanded = ExpandedSet {
            required: (1..=4).map(|i| Scored::new(uid(i), 0.0)).collect(),
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 0.9),
                Scored::new(uid(2), 0.8),
                Scored::new(uid(3), 0.7),
                Scored::new(uid(4), 0.1),
            ],
        };
        let hot = HashMap::new();
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot,
            &query(2, 10_000),
            RecallWeights {
                w_daemon: 1.0,
                w_query: 0.0,
            },
            ts(0),
            default_token_count,
        );
        assert_eq!(
            ids_of(&result),
            vec![uid(4), uid(1)],
            "spec §10: the Canonical node is promoted first, so top_k=2 keeps \
             it and drops the lowest-scoring non-Canonical instead"
        );
        assert!(result.context.contains("[Entity, canonical]"));
    }

    /// Several Canonicals keep score order **among themselves**, and the
    /// non-Canonical tail keeps its own — the partition is one level of the
    /// sort key, not a replacement for it.
    #[test]
    fn canonical_group_is_score_ordered_internally() {
        let mut g = graph_with(4);
        canonize(&mut g, &[1, 3]);
        let expanded = ExpandedSet {
            required: (1..=4).map(|i| Scored::new(uid(i), 0.0)).collect(),
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 0.2),
                Scored::new(uid(2), 0.9),
                Scored::new(uid(3), 0.5),
                Scored::new(uid(4), 0.4),
            ],
        };
        let hot = HashMap::new();
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot,
            &query(10, 10_000),
            RecallWeights {
                w_daemon: 1.0,
                w_query: 0.0,
            },
            ts(0),
            default_token_count,
        );
        assert_eq!(
            ids_of(&result),
            vec![uid(3), uid(1), uid(2), uid(4)],
            "canonicals first (0.5 then 0.2), then the rest (0.9 then 0.4)"
        );
    }

    #[test]
    fn final_score_ties_break_by_canonical_key_ahead_of_node_id() {
        // Issue #2: equal finals order by canonical key asc, id asc only
        // behind that. The graph's keys are chosen so the key order
        // contradicts the id order ("alpha" rides the LARGER id, c2), so the
        // old id-first chain fails this test.
        let mut g = Graph::new(sid());
        let i1 = interaction(1);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_concept(concept(1, i1.id, "beta"), i1.id).unwrap();
        g.insert_concept(concept(2, i1.id, "alpha"), i1.id).unwrap();
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(2), 0.0), Scored::new(uid(1), 0.0)],
            siblings: Vec::new(),
        };
        // Both nodes: daemon 0.8, no relevance -> identical finals regardless
        // of weights; input order is deliberately reversed (c2 first).
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(2), 0.8), Scored::new(uid(1), 0.8)],
        };
        let hot = HashMap::new();
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot,
            &query(10, 10_000),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(
            ids_of(&result),
            vec![uid(2), uid(1)],
            "equal finals tie on canonical key (alpha < beta), not the per-run id"
        );
    }

    #[test]
    fn non_finite_and_negative_weights_sanitize_to_zero() {
        let g = graph_with(1);
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.0)],
            siblings: Vec::new(),
        };
        let phase1 = vec![Scored::new(uid(1), 1.0)];
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(1), 1.0)],
        };
        let hot = HashMap::new();
        // NaN and negative weights must not poison the final score (ALGO-10).
        let result = assemble(
            &g,
            &expanded,
            &phase1,
            &scores,
            &hot,
            &query(10, 10_000),
            RecallWeights {
                w_daemon: f64::NAN,
                w_query: -1.0,
            },
            ts(0),
            default_token_count,
        );
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].score, 0.0);
    }

    #[test]
    fn non_finite_daemon_or_relevance_inputs_sanitize_to_zero() {
        // Deep-review F3: the "final score finite for every input" contract is
        // honored for the INPUTS too, not just the weights. A non-finite
        // store-provided relevance (or daemon score) must sanitize to 0.0 and
        // never rank first via total_cmp.
        let g = graph_with(2);
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.0), Scored::new(uid(2), 0.0)],
            siblings: Vec::new(),
        };
        // uid(1): non-finite daemon AND relevance; uid(2): sane.
        let phase1 = vec![Scored::new(uid(1), f64::NAN), Scored::new(uid(2), 1.0)];
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(1), f64::NAN), Scored::new(uid(2), 1.0)],
        };
        let result = assemble(
            &g,
            &expanded,
            &phase1,
            &scores,
            &HashMap::new(),
            &query(10, 10_000),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        // uid(1) sanitizes to 0.0; uid(2) ranks above it.
        assert_eq!(result.hits.len(), 2);
        assert_eq!(
            result.hits[0].node_id,
            uid(2),
            "non-finite input must not rank first"
        );
        assert_eq!(result.hits[1].node_id, uid(1));
        assert!(
            result.hits[1].score.is_finite(),
            "sanitized score is finite"
        );
        assert_eq!(result.hits[1].score, 0.0);
    }

    // -----------------------------------------------------------------------
    // Hot-list force-include
    // -----------------------------------------------------------------------

    /// Conflict predicate shaped like T4.3's: the recency window is derived
    /// from the caller's `now`, so a lapsed window evicts the entry and a
    /// live one rebuilds `seconds_ago` at read time (XP-3).
    fn conflict_entry(
        node: NodeId,
        writer: AgentId,
        agents: Vec<AgentId>,
        write_at: DateTime<Utc>,
        window_secs: i64,
    ) -> HotListEntry {
        HotListEntry::new(
            node,
            Condition::Conflict,
            HotListPayload::Conflict {
                agents: agents.clone(),
                writer: writer.clone(),
                seconds_ago: 999, // stale sentinel: must be rebuilt before render
            },
            move |_, now| {
                let secs = (now - write_at).num_seconds();
                if (0..=window_secs).contains(&secs) {
                    Some(HotListPayload::Conflict {
                        agents: agents.clone(),
                        writer: writer.clone(),
                        seconds_ago: secs as u64,
                    })
                } else {
                    None
                }
            },
        )
    }

    #[test]
    fn hot_force_include_keeps_live_and_drops_lapsed() {
        let g = graph_with(3);
        // Daemon-only scores -> order c3 > c2 > c1; top_k=1 keeps only c3.
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(3), 1.0),
                Scored::new(uid(2), 0.5),
                Scored::new(uid(1), 0.1),
            ],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: Vec::new(),
        };
        let now = ts(60);
        let writer = AgentId::from("agent-a");
        let agents = vec![AgentId::from("agent-a"), AgentId::from("agent-b")];
        let mut hot = HotList::new();
        // c1: live (write 11s before now, 30s window) -> force-included.
        let _ = hot.insert(conflict_entry(
            uid(1),
            writer.clone(),
            agents.clone(),
            now - chrono::Duration::seconds(11),
            30,
        ));
        // c2: lapsed (write 31s before now) -> dropped by re-validation.
        let _ = hot.insert(conflict_entry(
            uid(2),
            writer,
            agents,
            now - chrono::Duration::seconds(31),
            30,
        ));

        let hot_payloads = revalidated(&mut hot, &g, &expanded, now);
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot_payloads,
            &query(1, 10_000),
            RecallWeights::default(),
            now,
            default_token_count,
        );

        // c3 (rank 1) + force-included c1; c2 is neither in top_k nor hot.
        assert_eq!(ids_of(&result), vec![uid(3), uid(1)]);
        assert_eq!(
            result.warnings,
            vec!["Agent A wrote to it 11 seconds ago".to_string()],
            "rebuilt payload, not the stale 999 sentinel"
        );
        assert!(
            result.context.contains("concept 1"),
            "force-included block rendered"
        );
        assert!(!result.context.contains("concept 2"), "lapsed entry absent");
        // That the lapsed entry also left the list, and the live one stayed
        // with its read-time payload, is the hot list's own contract since
        // #25 moved re-validation into the daemon:
        // `daemon::hotlist::tests::revalidate_members_keeps_live_and_drops_lapsed`.
    }

    // -----------------------------------------------------------------------
    // Assembly: top_k, max_tokens, no split blocks
    // -----------------------------------------------------------------------

    #[test]
    fn top_k_and_max_tokens_drop_whole_lowest_blocks() {
        let g = graph_with(4);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 1.0),
                Scored::new(uid(2), 0.8),
                Scored::new(uid(3), 0.6),
                Scored::new(uid(4), 0.4),
            ],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
                Scored::new(uid(4), 0.0),
            ],
            siblings: Vec::new(),
        };
        let hot = HashMap::new();

        // top_k=3: c4 is excluded from hits entirely.
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot,
            &query(3, 10_000),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(ids_of(&result), vec![uid(1), uid(2), uid(3)]);
        assert_eq!(result.hits.len(), 3, "top_k respected");

        // Rebuild the rendered blocks the way assemble should, then find the
        // largest score-ordered prefix that fits the budget: the context must
        // equal that prefix (whole blocks only, highest-scoring kept).
        let blocks: Vec<String> = result
            .hits
            .iter()
            .map(|h| crate::recall::format::render_block(h, &[]))
            .collect();
        let mut budget = 0usize;
        let mut kept = 0usize;
        for b in &blocks {
            if budget + default_token_count(b) <= 200 {
                budget += default_token_count(b);
                kept += 1;
            } else {
                break;
            }
        }
        let expected = blocks[..kept].join("\n\n");
        let result2 = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot,
            &query(3, 200),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(result2.context, expected, "longest whole-block prefix");
        assert_eq!(result2.hits.len(), 3, "truncation never removes hits");
        assert!(
            !result2.context.contains("concept 4"),
            "c4 was cut by top_k before rendering"
        );
    }

    #[test]
    fn custom_token_fn_drives_truncation() {
        let g = graph_with(3);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 1.0),
                Scored::new(uid(2), 0.8),
                Scored::new(uid(3), 0.6),
            ],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: Vec::new(),
        };
        let hot = HashMap::new();

        // token_fn = byte length; budget = exactly block 1's bytes -> only
        // the highest-scoring block survives (whole-block rule).
        let hit1 = RecallHit {
            node_id: uid(1),
            content: "concept 1".into(),
            concept_type: Some(ConceptType::Entity),
            score: 1.0 * 0.5,
            is_canonical: false,
            blast_radius: None,
        };
        let block1 = crate::recall::format::render_block(&hit1, &[]);
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot,
            &query(10, block1.len()),
            RecallWeights::default(),
            ts(0),
            |s| s.len(),
        );
        assert_eq!(result.hits.len(), 3, "hits unaffected by token budget");
        assert_eq!(result.context, block1, "custom estimator honored");
        assert!(!result.context.contains("concept 2"));
    }

    #[test]
    fn max_tokens_zero_yields_empty_context_but_full_hits() {
        let g = graph_with(2);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(1), 1.0), Scored::new(uid(2), 0.8)],
        };
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.0), Scored::new(uid(2), 0.0)],
            siblings: Vec::new(),
        };
        let hot = HashMap::new();
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot,
            &query(10, 0),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(result.context, "");
        assert_eq!(result.hits.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Reservations
    // -----------------------------------------------------------------------

    #[test]
    fn reservation_rendered_when_active_and_absent_when_expired() {
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.0)],
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(1), 1.0)],
        };
        let now = ts(60);
        let hot = HashMap::new();

        let run = |expires_at: DateTime<Utc>| {
            let mut g = graph_with(1);
            g.set_reservation(Reservation {
                session_id: sid(),
                node_id: uid(1),
                agent_id: AgentId::from("agent-c"),
                expires_at,
            });
            assemble(
                &g,
                &expanded,
                &[],
                &scores,
                &hot,
                &query(10, 10_000),
                RecallWeights::default(),
                now,
                default_token_count,
            )
        };

        let live = run(now + chrono::Duration::seconds(60));
        assert_eq!(
            live.warnings,
            vec!["Reserved by agent-c until 2025-07-08T19:41:00Z".to_string()]
        );
        assert!(
            live.context.contains("Reserved by agent-c"),
            "active line shown"
        );

        let gone = run(now - chrono::Duration::seconds(60));
        assert!(
            gone.warnings.is_empty(),
            "expired reservation renders nothing"
        );
        assert!(!gone.context.contains("Reserved by"));
    }

    // -----------------------------------------------------------------------
    // THE GOLDEN: the demo query against the shipped fixture (spec §13).
    //
    // Scenario construction (deterministic):
    // * graph + inverted index: `fixtures/session-rest-api.json`, the spec §13
    //   demo world in miniature;
    // * phase 1: the real union over that index (`limit = top_k`);
    // * phase 2: the real BFS expansion (`traversal_depth = 2`);
    // * daemon scores: the MERGED daemon scorer (`daemon::score::rescore`)
    //   over the fixture — session-relative, wall-clock-free, so the table is
    //   stable;
    // * hot list: built directly with a T4.3-shaped Conflict predicate on
    //   `user schema` whose recency window derives from the caller's `now`;
    // * `now`: a fixed instant (base + 60 min), so the conflict's rebuilt
    //   `seconds_ago` is exactly 11 at read time.
    // The golden file is the byte-for-byte expected context block. G2's 0.35
    // recent floor is reflected there: it no longer masks a stronger
    // non-recent candidate during phase-1 ordering.
    #[cfg(feature = "fixtures")]
    #[test]
    fn golden_update_user_schema_demo_context_block() {
        use crate::config::ScoringWeights;
        use crate::daemon::score::rescore;
        use crate::fixtures;
        use crate::graph::index::InvertedIndex;
        use crate::recall::candidates::{candidates, Phase1Input};
        use crate::recall::expand::expand;

        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let graph = Graph::from_snapshot(snap.clone()).unwrap();
        let index = InvertedIndex::from_snapshot(&snap);

        let query = RecallQuery {
            query: "update user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        };
        let phase1 = candidates(
            &graph,
            &index,
            Phase1Input::default(),
            &query.query,
            query.top_k,
        );
        let expanded = expand(&graph, phase1.clone(), query.traversal_depth);
        let scores = ScoreTable {
            epoch: graph.epoch(),
            ranked: rescore(&graph, &ScoringWeights::default()),
        };

        let now = ts(60);
        let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        let mut hot = HotList::new();
        let _ = hot.insert(conflict_entry(
            us,
            AgentId::from("agent-a"),
            vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
            now - chrono::Duration::seconds(11),
            30,
        ));

        let hot_payloads = revalidated(&mut hot, &graph, &expanded, now);
        let result = assemble(
            &graph,
            &expanded,
            &phase1,
            &scores,
            &hot_payloads,
            &query,
            RecallWeights::default(),
            now,
            default_token_count,
        );

        let golden_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/recall-context-golden.txt"
        );
        let golden = std::fs::read_to_string(golden_path).expect("golden context fixture present");
        assert_eq!(
            result.context, golden,
            "the demo context block is the product: it must render byte-for-byte"
        );

        // The three demo features, explicitly:
        assert!(
            result.context.contains("user schema [Entity, canonical]"),
            "canonical marker on the demo pillar"
        );
        assert!(
            result
                .context
                .contains("⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution."),
            "load-bearing warning with the graph-computed count"
        );
        assert!(
            result
                .context
                .contains("Agent A wrote to it 11 seconds ago"),
            "conflict line: writer, never the first-listed agent"
        );
        assert_eq!(result.hits[0].node_id, us, "demo pillar ranks first");
        assert!(result.hits[0].is_canonical);
        assert_eq!(result.hits[0].blast_radius, Some(8));
        // The accumulated warnings: the ⚑ line then the conflict line, in hit
        // order (user schema is the only warned hit in this result).
        assert_eq!(
            result.warnings,
            vec![
                "⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution.".to_string(),
                "Agent A wrote to it 11 seconds ago".to_string(),
            ]
        );
    }

    // P1-4 (GPT5.6sol): the token budget charges the separator AND stops at the
    // first non-fitting block (ranked-prefix) — a lower-ranked short block must
    // not follow a skipped higher-ranked one. Provable with a tiny token_fn
    // that returns the byte length, and blocks whose sizes straddle a budget.
    #[test]
    fn budget_charges_separators_and_enforces_ranked_prefix() {
        let g = graph_with(3);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 1.0),
                Scored::new(uid(2), 0.5),
                Scored::new(uid(3), 0.1),
            ],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0),
            ],
            siblings: Vec::new(),
        };
        // token_fn = byte length. The discriminating budget is derived from
        // blocks the same shape renders (the custom_token_fn pattern), so the
        // test self-adjusts when the render changes: block1+block2 fits with
        // one byte to spare only while the join separator is free, and stops
        // fitting once the "\n\n" join is charged. Under a separator-free
        // accumulator the pre-P1-4 bug rendered two blocks here; the fixed
        // code must stop at block1 (ranked-prefix: block2, though it might
        // fit alone, must NOT appear after block1).
        let hit1 = RecallHit {
            node_id: uid(1),
            content: "concept 1".into(),
            concept_type: Some(ConceptType::Entity),
            score: 1.0 * 0.5,
            is_canonical: false,
            blast_radius: None,
        };
        let hit2 = RecallHit {
            node_id: uid(2),
            content: "concept 2".into(),
            concept_type: Some(ConceptType::Entity),
            score: 0.5 * 0.5,
            is_canonical: false,
            blast_radius: None,
        };
        let block1 = crate::recall::format::render_block(&hit1, &[]);
        let block2 = crate::recall::format::render_block(&hit2, &[]);
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &HashMap::new(),
            &query(3, block1.len() + block2.len() + 1),
            RecallWeights::default(),
            ts(60),
            byte_len,
        );
        let contexts = result.context.split("\n\n").collect::<Vec<_>>();
        assert_eq!(
            contexts.len(),
            1,
            "ranked-prefix: only the first block fits"
        );
        assert!(result.context.starts_with("concept 1"), "first block kept");

        // With a large budget everything fits in rank order.
        let all = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &HashMap::new(),
            &query(3, 10_000),
            RecallWeights::default(),
            ts(60),
            byte_len,
        );
        assert_eq!(all.context.split("\n\n").count(), 3);
    }

    // Deep-review F1 / GPT5.6sol P1-4: the context is a STRICT ranked prefix.
    // A lower-ranked block that would fit alone must NOT appear after a
    // higher-ranked block that does not fit. Sizes are computed at runtime so
    // the test is robust to render changes.
    #[test]
    fn ranked_prefix_stops_at_first_nonfit_block() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1);
        g.insert_interaction(i1.clone()).unwrap();
        // Long, highest-scoring block (does not fit); short, lower-scoring block
        // (fits alone). Correct code: long blocks everything -> empty context.
        g.insert_concept(concept(1, i1.id, &"A".repeat(60)), i1.id)
            .unwrap();
        g.insert_concept(concept(2, i1.id, "B"), i1.id).unwrap();
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![Scored::new(uid(1), 1.0), Scored::new(uid(2), 0.1)],
        };
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.0), Scored::new(uid(2), 0.0)],
            siblings: Vec::new(),
        };
        let long = format!("{} [Entity] (score 0.50)", "A".repeat(60));
        let short = "B [Entity] (score 0.05)".to_owned();
        assert!(long.len() > short.len());
        // Budget fits the short block alone but not the long block.
        let budget = short.len() + 2;
        assert!(budget < long.len(), "budget must exclude the long block");
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &HashMap::new(),
            &RecallQuery {
                query: "irrelevant".into(),
                top_k: 2,
                max_tokens: budget,
                traversal_depth: 2,
            },
            RecallWeights::default(),
            ts(60),
            byte_len,
        );
        assert_eq!(
            result.context, "",
            "a non-fitting top block yields empty context, never a lower block"
        );
    }

    // P2-5 (GPT5.6sol): a graph-missing member within top_k (e.g. a stale durable
    // vector id) must NOT consume a top_k slot — the next valid member fills it.
    #[test]
    fn stale_graph_missing_member_does_not_consume_top_k_slot() {
        // expanded.required includes uid(9) which is NOT in the graph (stale
        // vector id), ahead of the valid c1. top_k=1 must yield [c1], not [].
        let g = graph_with(3);
        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![],
        };
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(9), 0.9), // graph-missing / stale
                Scored::new(uid(1), 0.1),
            ],
            siblings: Vec::new(),
        };
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &HashMap::new(),
            &query(1, 10_000),
            RecallWeights::default(),
            ts(60),
            byte_len,
        );
        assert_eq!(ids_of(&result), vec![uid(1)], "valid member fills the slot");
    }

    fn byte_len(s: &str) -> usize {
        s.len()
    }

    // -----------------------------------------------------------------------
    // H3 — structured recall payload golden (blended pipeline)
    // -----------------------------------------------------------------------

    /// Walk `id` through the audited transition path and stop at `to` (the
    /// state machine forbids skipping stages, so partial promotions must be
    /// walked hop by hop).
    fn promote_to(g: &mut Graph, id: u64, to: CanonizationStatus) {
        use crate::types::CanonizationEvent;
        for (from, target) in [
            (CanonizationStatus::None, CanonizationStatus::Candidate),
            (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ] {
            g.apply_canonization_transition(CanonizationEvent {
                id: NodeId::new(),
                session_id: sid(),
                node_id: uid(id),
                from_status: from,
                to_status: target,
                blast_radius: None,
                last_demotion_time: None,
                occurred_at: ts(0),
            })
            .unwrap();
            if target == to {
                break;
            }
        }
    }

    fn dep_edge(src: u64, dst: u64) -> crate::types::Edge {
        crate::types::Edge {
            event_time: None,
            id: NodeId::new(),
            session_id: sid(),
            source: uid(src),
            target: uid(dst),
            edge_type: crate::types::EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        }
    }

    /// The H3 blended golden — one result carrying the full annotation family:
    /// a Canonical load-bearing hit, a Candidate, a conflict, a non-conflict
    /// hot condition, a reservation, and an annotation-free hit. The payload
    /// is pinned byte-for-byte in `fixtures/recall-h3-goldens.json`.
    #[test]
    fn h3_blended_payload_matches_golden() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1);
        g.insert_interaction(i1.clone()).unwrap();
        let contents = [
            (1u64, "user schema"),
            (2, "auth middleware"),
            (3, "rate limiter"),
            (4, "caching layer"),
            (5, "logging config"),
            (6, "ping endpoint"),
        ];
        for (id, content) in contents {
            g.insert_concept(concept(id, i1.id, content), i1.id)
                .unwrap();
        }
        promote_to(&mut g, 1, CanonizationStatus::Canonical);
        promote_to(&mut g, 2, CanonizationStatus::Candidate);

        // Two structural dependents for the canonical pillar (blast radius 2);
        // they are never candidates, so they never become hits.
        for id in [7u64, 8] {
            g.insert_concept(concept(id, i1.id, &format!("dep {id}")), i1.id)
                .unwrap();
            g.upsert_edge(dep_edge(1, id)).unwrap();
        }

        // A soft lock another agent holds on the reservation hit.
        g.set_reservation(Reservation {
            session_id: sid(),
            node_id: uid(5),
            agent_id: AgentId::from("agent-c"),
            expires_at: ts(60) + chrono::Duration::seconds(3600),
        });

        // Hot entries, re-validated at the pinned `now`: a conflict on uid(3)
        // (written 11s ago, the spec §13 sentence) and a HighRisk condition on
        // uid(4) (the non-conflict hot kind).
        let mut hot = HotList::new();
        let agents = vec![AgentId::from("agent-a"), AgentId::from("agent-b")];
        let writer = AgentId::from("agent-a");
        let write_at = ts(60) - chrono::Duration::seconds(11);
        let conflict_agents = agents.clone();
        let conflict_writer = writer.clone();
        let _ = hot.insert(HotListEntry::new(
            uid(3),
            Condition::Conflict,
            HotListPayload::Conflict {
                agents: agents.clone(),
                writer: writer.clone(),
                seconds_ago: 999, // stale sentinel: revalidate must rebuild
            },
            move |_, now| {
                let secs = (now - write_at).num_seconds();
                if (0..=30).contains(&secs) {
                    Some(HotListPayload::Conflict {
                        agents: conflict_agents.clone(),
                        writer: conflict_writer.clone(),
                        seconds_ago: secs as u64,
                    })
                } else {
                    None
                }
            },
        ));
        let high_risk = HotListPayload::HighRisk {
            reason: "8 dependents share this pillar".into(),
        };
        let _ = hot.insert(HotListEntry::new(
            uid(4),
            Condition::HighRiskModification,
            high_risk.clone(),
            move |_, _| Some(high_risk.clone()),
        ));

        let scores = ScoreTable {
            epoch: 0,
            ranked: vec![
                Scored::new(uid(1), 1.0),
                Scored::new(uid(2), 0.9),
                Scored::new(uid(3), 0.8),
                Scored::new(uid(4), 0.7),
                Scored::new(uid(5), 0.6),
                Scored::new(uid(6), 0.5),
            ],
        };
        let expanded = ExpandedSet {
            required: (1u64..=6).map(|i| Scored::new(uid(i), 0.0)).collect(),
            siblings: Vec::new(),
        };
        let now = ts(60);
        let hot_payloads = revalidated(&mut hot, &g, &expanded, now);
        let result = assemble(
            &g,
            &expanded,
            &[],
            &scores,
            &hot_payloads,
            &query(10, 10_000),
            RecallWeights::default(),
            now,
            default_token_count,
        );

        // The six required kinds, from typed producers:
        let hit = |content: &str| {
            result
                .detailed
                .iter()
                .find(|h| h.content == content)
                .unwrap_or_else(|| panic!("hit {content} present"))
        };
        assert_eq!(
            hit("user schema").status,
            Some(CanonizationStatus::Canonical)
        );
        assert_eq!(
            hit("user schema").annotations[0].kind,
            AnnotationKind::LoadBearing
        );
        assert_eq!(
            hit("auth middleware").status,
            Some(CanonizationStatus::Candidate)
        );
        assert_eq!(
            hit("rate limiter").annotations[0].kind,
            AnnotationKind::Conflict
        );
        assert_eq!(
            hit("caching layer").annotations[0].kind,
            AnnotationKind::Hot
        );
        assert_eq!(
            hit("logging config").annotations[0].kind,
            AnnotationKind::Reservation
        );
        assert!(hit("ping endpoint").annotations.is_empty());
        assert!(
            result.detailed.iter().all(|h| h.included_in_context),
            "a 10k budget includes every block"
        );
        // Status came from the graph snapshot, never `is_canonical`: the
        // Candidate is not canonical yet still carries its full status.
        assert!(!result.hits[1].is_canonical);

        // The wire shape is the golden.
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
            actual, golden["blended"],
            "blended structured payload must match the golden"
        );
    }

    #[test]
    fn cold_start_uses_one_query_scale_for_old_and_new_candidates() {
        let g = graph_with(4);
        let phase1 = vec![
            Scored::new(uid(1), 0.60), // old noise, high daemon score
            Scored::new(uid(2), 0.75), // established relevant, low daemon score
            Scored::new(uid(3), 0.80), // fresh relevant text/image
            Scored::new(uid(4), 0.68), // fresh irrelevant image
        ];
        let expanded = ExpandedSet {
            required: phase1.clone(),
            siblings: Vec::new(),
        };
        let old_scores = ScoreTable {
            epoch: 1,
            ranked: vec![Scored::new(uid(1), 0.90), Scored::new(uid(2), 0.20)],
        };
        let run = |scores: &ScoreTable, weights| {
            assemble(
                &g,
                &expanded,
                &phase1,
                scores,
                &HashMap::new(),
                &query(4, 10_000),
                weights,
                ts(0),
                default_token_count,
            )
        };
        let cold = run(&old_scores, RecallWeights::default());
        assert_eq!(ids_of(&cold), vec![uid(3), uid(2), uid(4), uid(1)]);
        let cold_scores: Vec<f64> = cold.hits.iter().map(|hit| hit.score).collect();
        for (actual, expected) in cold_scores.iter().zip([0.40, 0.375, 0.34, 0.30]) {
            assert!(approx(*actual, expected));
        }

        // Keep the configured query weight as a common factor. The daemon
        // share is withheld until it is available for every phase-1 concept.
        let weighted = run(
            &old_scores,
            RecallWeights {
                w_daemon: 0.25,
                w_query: 0.75,
            },
        );
        assert_eq!(ids_of(&weighted), ids_of(&cold));
        for (hit, expected) in weighted.hits.iter().zip([0.60, 0.5625, 0.51, 0.45]) {
            assert!(approx(hit.score, expected));
        }

        // A later daemon table restores the ordinary mix for every member.
        // The resulting rank change is accepted and documented.
        let settled_scores = ScoreTable {
            epoch: 2,
            ranked: vec![
                Scored::new(uid(1), 0.90),
                Scored::new(uid(2), 0.20),
                Scored::new(uid(3), 0.10),
                Scored::new(uid(4), 0.60),
            ],
        };
        let settled = run(&settled_scores, RecallWeights::default());
        assert_eq!(ids_of(&settled), vec![uid(1), uid(4), uid(2), uid(3)]);
        assert!(approx(settled.hits[0].score, 0.75));
        assert!(approx(settled.hits[1].score, 0.64));
    }

    #[test]
    fn explicit_daemon_zero_and_structural_absence_do_not_start_cold_mode() {
        let g = graph_with(3);
        let phase1 = vec![Scored::new(uid(1), 0.60), Scored::new(uid(2), 0.80)];
        let expanded = ExpandedSet {
            required: vec![
                Scored::new(uid(1), 0.0),
                Scored::new(uid(2), 0.0),
                Scored::new(uid(3), 0.0), // BFS-only, absent from score table
            ],
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 1,
            ranked: vec![Scored::new(uid(1), 0.90), Scored::new(uid(2), 0.0)],
        };
        let result = assemble(
            &g,
            &expanded,
            &phase1,
            &scores,
            &HashMap::new(),
            &query(3, 10_000),
            RecallWeights::default(),
            ts(0),
            default_token_count,
        );
        assert_eq!(ids_of(&result), vec![uid(1), uid(2), uid(3)]);
        assert!(approx(result.hits[0].score, 0.75));
        assert!(approx(result.hits[1].score, 0.40));
        assert!(approx(result.hits[2].score, 0.0));
    }

    #[test]
    fn recent_only_unscored_hit_does_not_switch_query_order() {
        let g = graph_with(2);
        let phase1 = vec![Scored::new(uid(1), 0.80), Scored::new(uid(2), 0.35)];
        let expanded = ExpandedSet {
            required: phase1.clone(),
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 1,
            ranked: vec![Scored::new(uid(1), 0.80)],
        };
        let mut legs = HashMap::new();
        legs.insert(
            uid(1),
            LegScores {
                keyword: Some(0.80),
                ..LegScores::default()
            },
        );
        legs.insert(
            uid(2),
            LegScores {
                recent: Some(0.35),
                ..LegScores::default()
            },
        );
        let run = |legs: &HashMap<NodeId, LegScores>| {
            assemble_with_legs(
                &g,
                &expanded,
                &phase1,
                Some(legs),
                &scores,
                &HashMap::new(),
                &query(2, 10_000),
                RecallWeights::default(),
                ts(0),
                default_token_count,
            )
        };
        let ordinary = run(&legs);
        assert_eq!(ids_of(&ordinary), vec![uid(1), uid(2)]);
        assert!(approx(ordinary.hits[0].score, 0.80));
        assert!(approx(ordinary.hits[1].score, 0.175));

        // A vector score equal to RECENT_SCORE is still query evidence: the
        // leg's presence, not its numeric value, controls cold mode.
        legs.get_mut(&uid(2)).unwrap().vector = Some(0.35);
        let cold = run(&legs);
        assert_eq!(ids_of(&cold), vec![uid(1), uid(2)]);
        assert!(approx(cold.hits[0].score, 0.40));
        assert!(approx(cold.hits[1].score, 0.175));
    }

    #[test]
    fn daemon_only_weight_does_not_switch_to_query_order() {
        let g = graph_with(2);
        let phase1 = vec![Scored::new(uid(1), 0.60), Scored::new(uid(2), 0.80)];
        let expanded = ExpandedSet {
            required: phase1.clone(),
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 1,
            ranked: vec![Scored::new(uid(1), 0.90)],
        };
        let result = assemble(
            &g,
            &expanded,
            &phase1,
            &scores,
            &HashMap::new(),
            &query(2, 10_000),
            RecallWeights {
                w_daemon: 1.0,
                w_query: 0.0,
            },
            ts(0),
            default_token_count,
        );
        assert_eq!(ids_of(&result), vec![uid(1), uid(2)]);
        assert!(approx(result.hits[0].score, 0.90));
        assert!(approx(result.hits[1].score, 0.0));
    }

    #[test]
    fn stale_vector_id_absent_from_the_graph_does_not_start_cold_mode() {
        // The vector leg can return an id the graph no longer holds (a store
        // row whose concept was removed); `rank` keeps it in phase 1. It is
        // unscored by construction, but it is not a fresh concept, so it
        // must not withhold the daemon share from everything else.
        let g = graph_with(1);
        let stale = uid(9);
        let phase1 = vec![Scored::new(stale, 0.90), Scored::new(uid(1), 0.80)];
        let expanded = ExpandedSet {
            required: vec![Scored::new(uid(1), 0.80)],
            siblings: Vec::new(),
        };
        let scores = ScoreTable {
            epoch: 1,
            ranked: vec![Scored::new(uid(1), 0.60)],
        };
        let mut legs = HashMap::new();
        legs.insert(
            stale,
            LegScores {
                vector: Some(0.90),
                ..LegScores::default()
            },
        );
        legs.insert(
            uid(1),
            LegScores {
                vector: Some(0.80),
                ..LegScores::default()
            },
        );
        for legs in [None, Some(&legs)] {
            let result = assemble_with_legs(
                &g,
                &expanded,
                &phase1,
                legs,
                &scores,
                &HashMap::new(),
                &query(2, 10_000),
                RecallWeights::default(),
                ts(0),
                default_token_count,
            );
            assert_eq!(ids_of(&result), vec![uid(1)]);
            // Blended: 0.5 × 0.60 + 0.5 × 0.80, not cold 0.5 × 0.80.
            assert!(approx(result.hits[0].score, 0.70));
        }
    }
}
