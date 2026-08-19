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
//! [`try_structural`] recognizes a dependency question, resolves the entity it
//! names, and answers it **by traversal** over the structural edge types
//! (`Dependency` / `Hierarchical` / `Causal`), returning the anchor's
//! dependents ranked by how strongly the graph binds each one to it. It is a
//! *dispatch*: when it fires it produces the whole [`RecallResult`] and the
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

use std::collections::HashMap;

use crate::graph::Graph;
use crate::recall::detail::{Annotation, AnnotationKind, DetailedHit, DetailedRecall};
use crate::recall::format;
use crate::types::{CanonizationStatus, NodeId, RecallHit, Scored};

/// What kind of question a recall query is, for routing (T9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecallKind {
    /// A general / word-match question — the blended pipeline answers it.
    General,
    /// A structural / dependency question — the traversal answers it.
    Structural,
}

/// The traversal answers a dependency question over the structural edge types
/// (`Dependency` / `Causal` / `Hierarchical` — [`format::STRUCTURAL_EDGE_TYPES`],
/// spec §4.1 errata: `Derives` provenance and `Temporal` never carry
/// dependency). Membership follows the §4.1 blast-radius predicate directly:
/// a node depends on the anchor when the anchor is its **sole** structural
/// source (see [`dependents`]), so the returned set and the `blast_radius`
/// field stamped on each hit always agree.
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

/// Resolve the entity a structural query names.
///
/// Precedence:
/// 1. A concept whose `content` appears verbatim (case-insensitive substring)
///    in the query — e.g. `"what depends on SG-Base-VPC"` names
///    `"SG-Base-VPC"`. Among several, prefer Entity/Resource and the shortest
///    content (the bare identifier, not a longer clause that happens to
///    contain it).
/// 2. A query that references a security group without naming one
///    ("the shared security group") picks the SG-shaped concept with the most
///    structural dependents — the one whose deletion is dangerous. This
///    requires explicit `"security group"` language, so a structural phrasing
///    that names nothing real ("what depends on nothing-imaginary") still
///    refuses.
fn resolve_anchor(graph: &Graph, query: &str) -> Option<NodeId> {
    let lower_q = query.to_lowercase();
    let mut substring_matches: Vec<&crate::types::Concept> = graph
        .concepts()
        .filter(|c| !c.content.is_empty() && lower_q.contains(&c.content.to_lowercase()))
        .collect();
    let rank = |c: &&crate::types::Concept| {
        let ty_prio = match c.concept_type {
            crate::types::ConceptType::Entity | crate::types::ConceptType::Resource => 0,
            _ => 1,
        };
        // Shortest content first, then Entity/Resource, then id.
        (ty_prio, c.content.len(), c.id.0)
    };
    substring_matches.sort_by_key(rank);
    if let Some(c) = substring_matches.first() {
        tracing::trace!(
            target: "lambo::recall",
            anchor = %c.id,
            content = %c.content,
            "recall dispatch: resolved named anchor"
        );
        return Some(c.id);
    }

    if !lower_q.contains("security group") {
        return None;
    }
    let mut sg: Vec<NodeId> = graph
        .concepts()
        .filter(|c| {
            let upper = c.content.to_uppercase();
            upper.starts_with("SG-") || upper.contains(" SECURITY GROUP")
        })
        .map(|c| c.id)
        .collect();
    // Each `dependents(graph, *id)` is a full inbound_sources pass, run twice
    // per recall (brief read + final lock), so this branch is O(SG x E) —
    // bounded and fine at recall-graph sizes, but precompute per-SG dependent
    // counts if it ever matters.
    // Tie-break among equal-dependent SGs is not an explicit key: it falls
    // back to concept-store iteration order, deterministic for an ordered
    // store and for the exhibit, but an assumption worth naming.
    sg.sort_by_key(|id| std::cmp::Reverse(dependents(graph, *id).len()));
    if let Some(id) = sg.first() {
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
/// final graph lock inside [`try_structural`], so this is a gather-skip
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
/// Membership matches the §4.1 blast-radius predicate exactly (T9-R1-2),
/// reusing [`format::inbound_sources`] so the returned set and the
/// `blast_radius` field stamped on each hit agree: a concept `dst` depends on
/// `anchor` when `anchor` is its **sole** structural source
/// (`srcs.len() == 1 && srcs[0] == anchor`). A multi-source dependent survives
/// `anchor`'s deletion and is deliberately NOT a blast-radius dependent, so it
/// is not returned here — this is what reconciles the traversal with §4.1 and
/// keeps delete-safety answers from over-reporting risk. The anchor itself is
/// excluded by construction (`inbound_sources` tracks concepts only).
///
/// Score is the strongest structural edge weight binding `dst` to `anchor`.
/// Ordering applies spec §10 canonical-first promotion, then strength, then id
/// ascending (T9-R1-6) — the same partition `assemble` applies, so the
/// traversal answer orders identically to the blend.
pub fn dependents(graph: &Graph, anchor: NodeId) -> Vec<Scored<NodeId>> {
    let sources = format::inbound_sources(graph);
    let mut strength: HashMap<NodeId, f64> = HashMap::new();
    for (dst, srcs) in sources {
        if dst == anchor || srcs.len() != 1 || srcs[0] != anchor {
            continue;
        }
        strength.insert(dst, max_structural_strength(graph, anchor, dst));
    }
    let mut out: Vec<Scored<NodeId>> = strength
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
    out.sort_by(|a, b| {
        is_canonical(b.item)
            .cmp(&is_canonical(a.item))
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.item.0.cmp(&b.item.0))
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

/// Strongest weight of any structural edge between `a` and `b` (either
/// direction). Scans `graph.edges()`; the recall-relevant subgraph is small.
fn max_structural_strength(graph: &Graph, a: NodeId, b: NodeId) -> f64 {
    let mut best: f64 = 0.0;
    for e in graph.edges() {
        if format::STRUCTURAL_EDGE_TYPES.contains(&e.edge_type)
            && ((e.source == a && e.target == b) || (e.source == b && e.target == a))
            && e.weight > best
        {
            best = e.weight;
        }
    }
    best
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
    let mut blocks: Vec<String> = Vec::new();
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
        let block = format::render_block(&hit, &lines);
        blocks.push(block);
        hits.push(hit);
        detailed.push(detailed_hit);
    }
    if hits.is_empty() {
        return None;
    }

    // Truncate context to the token budget (whole blocks, longest prefix first).
    let mut kept: Vec<String> = Vec::new();
    for block in blocks {
        let mut provisional = kept.join("\n\n");
        if !provisional.is_empty() {
            provisional.push_str("\n\n");
        }
        provisional.push_str(&block);
        if format::default_token_count(&provisional) > max_tokens {
            break;
        }
        kept.push(block);
    }

    tracing::debug!(
        target: "lambo::recall",
        query = %query,
        hits = hits.len(),
        "recall dispatched dependency question to structural traversal"
    );
    // T9-R1-N4: headline names only the dependents the context actually
    // renders (kept whole blocks), not the pre-truncation hit set.
    let count = kept.len();
    let mut warnings = Vec::new();
    let traversal =
        format!("recall: dependency question answered by graph traversal ({count} dependents)");
    warnings.push(traversal.clone());
    // H3: included_in_context at the cut; the traversal explanation is
    // response-global — one annotation, never attached to a hit.
    for (i, d) in detailed.iter_mut().enumerate() {
        d.included_in_context = i < count;
    }
    Some(DetailedRecall {
        hits,
        context: format::render_context(&kept),
        warnings,
        // A dispatched structural query answers by traversal and skips the
        // phase-1 blend entirely (T9-R1-3), so there are no leg scores to
        // report. Empty here means "no leg ran", not "the legs were dropped".
        legs: Default::default(),
        detailed,
        response_annotations: vec![Annotation::new(AnnotationKind::Traversal, traversal)],
    })
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
            chunk_group_id: None,
        }
    }

    fn edge(id: u64, src: u64, tgt: u64, ty: EdgeType, weight: f64) -> Edge {
        Edge {
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
        let mut hot = crate::daemon::hotlist::HotList::new();
        let _blended = crate::recall::assemble::assemble(
            &g,
            &expanded,
            &phase1,
            &crate::daemon::ScoreTable::default(),
            &mut hot,
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

    /// A dedicated structural graph: `app router` is the sole structural
    /// source of a canonical `users handler` and a plain `payments handler`,
    /// so the traversal answer exercises a load-bearing annotation and a
    /// full status on structural hits.
    fn structural_exhibit() -> Graph {
        let mut g = Graph::new(sid());
        let i1 = NodeId(uuid::Uuid::from_u64_pair(1, 1));
        g.insert_interaction(Interaction {
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
        g.upsert_edge(edge(1, 1, 2, EdgeType::Dependency, 5.0))
            .unwrap();
        g.upsert_edge(edge(2, 1, 3, EdgeType::Dependency, 3.0))
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
        assert!(result.detailed.iter().all(|h| !h
            .annotations
            .iter()
            .any(|a| a.kind == AnnotationKind::Traversal)));

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
}
