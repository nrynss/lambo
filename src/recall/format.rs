//! Recall context format (T5.3, spec §8 / v0.6.0 §9.2) — the text the demo
//! video shows on screen.
//!
//! One hit renders as a block of lines, in this order:
//!
//! ```text
//! <content> [<ConceptType>{, canonical}] (score <final>, id <short>, blast radius <n>{, also <short>})
//! <⚑ load-bearing-pillar line>        (only when the concept is Canonical)
//! <hot-list condition line(s)>        (only for re-validated hot nodes)
//! <contradicts line(s)>               (only on a contradicting companion, #19(c))
//! <reservation line>                  (only while an active reservation holds)
//! ```
//!
//! The `id <short>` token (issue #9) is an additive extension of the spec §8
//! block shape: the rendered text is the only surface some callers read, and
//! without a node id in it their only inspect focus was retyping a
//! paragraph-length concept string. It renders [`short_id`] right after the
//! score, and [`short_id`] is a valid `inspect` focus by construction (the
//! short-form id leg in `resolve_focus` resolves it), so a caller working only
//! from this text can reach the durable node-id path.
//!
//! The `also <short>` items (#181) name the members skipped as redundant
//! with the hit under `[selection] skip_redundant`: at most
//! [`crate::recall::redundancy::ALSO_MENTIONS`], then `+N more`. A hit
//! that skipped nothing renders without them.
//!
//! Blocks are joined with a blank line ([`render_context`]). The token budget
//! is applied once, by [`fit_blocks`] (submodule `fit`): the longest ranked
//! prefix of whole blocks, except that a first block too long for the budget
//! has its content cut at a UTF-8 boundary, keeps its label, and is followed
//! by a marker naming its id (#130).
//!
//! ## The ⚑ line (spec §13, verbatim)
//!
//! ```text
//! ⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.
//! ```
//!
//! The template is reproduced byte-for-byte, including the em dash and the
//! flag glyph; only the count is interpolated. This is the spec-verbatim
//! golden text (repo rule: em dashes are otherwise banned in docs; this one
//! is the exception, exactly as spec §13 quotes it).
//!
//! ## Blast radius (spec §4.1 contract + errata 2026-08-11, #36)
//!
//! [`blast_radius`] computes the Stage-3 dependent count over the in-RAM
//! graph, mirroring `GraphStore::blast_radius` (the MemoryStore / SQLite /
//! Postgres-family agreement): the number of distinct concepts that declare a
//! dependence on `node` ([`crate::types::Edge::dependence`]). Since #36 that
//! is the **sources of `Dependency` edges into `node`** (`record_action`
//! writes `action -> depends_on`, so the action is a dependent of what it
//! depends on) plus **`node`'s `Hierarchical` children**, all of them rather
//! than only those `node` would orphan. `Causal` (`action -> produced or
//! modified artifact`) is provenance and confers no load, nor do `Derives`,
//! `Temporal`, `CoOccurrence` or `Semantic`. A self-loop is not a dependent.
//! The definition is 1-hop, no recursion, and is a read-time reinterpretation
//! of the stored edges: nothing is migrated.
//!
//! No edge-age cutoff applies here: recall renders over the current graph
//! (the store query's `min_edge_age` is an adapter concern), and the
//! committed fixtures put every edge in the past by convention.
//!
//! **Fixture note (shipped numbers):** on `fixtures/session-rest-api.json`
//! the pillar "user schema" computes **14** dependents (six concepts that
//! depend on it plus its eight field children), pinned by `src/fixtures.rs`
//! (`blast_radius: Some(14)`) and `scripts/gen-fixtures.py`. The live demo
//! (`lambo demo`) builds its own graph and prints its own count.
//!
//! [`inbound_sources`] is a different relation, kept for #29's GC
//! protection (`crate::daemon::gc::resources_with_dependents`): every
//! structural (`Dependency`/`Causal`/`Hierarchical`) source of each concept,
//! whatever the edge direction means. It is not the dependents relation.
//!
//! ## The conflict line
//!
//! ```text
//! Agent A wrote to it 11 seconds ago
//! ```
//!
//! Subject is the payload's `writer` (ALGO-2) — never guessed from the
//! `agents` list, which on the shipped fixture would pick the wrong agent
//! (the newest write is agent-b's). [`agent_display`] maps the fixture's id
//! convention (`agent-a` → `A`). The age renders as a numeral, matching the
//! events channel's `"{}s ago"` convention and PHASE-8's "11-seconds-ago
//! conflict line"; spec §13's "eleven seconds ago" is demo narration, not
//! quoted screen text (unlike the ⚑ line above).

use std::collections::{HashMap, HashSet};

use crate::graph::Graph;
use crate::types::HotListPayload;
use crate::types::{AgentId, EdgeType, Node, NodeId, RecallHit, Reservation};

mod fit;
pub(crate) use fit::{fit_blocks, ContextBlock, FittedContext, Truncation};

/// The structural edge types (spec §4.1 errata: `Derives` provenance and
/// `Temporal` never count). This is the edge set of #29's GC protection
/// ([`inbound_sources`]), the portal's structural edge filter and the
/// structural dispatch's anchor search. It is **not** the dependents relation:
/// since #36 only `Dependency` and `Hierarchical` declare dependence, each in
/// its own direction ([`crate::types::EdgeType::dependent_end`]).
pub const STRUCTURAL_EDGE_TYPES: [EdgeType; 3] = [
    EdgeType::Dependency,
    EdgeType::Causal,
    EdgeType::Hierarchical,
];

/// Every distinct concept structural source of each concept, keyed by the
/// target: `target -> [sources]` over [`STRUCTURAL_EDGE_TYPES`],
/// concept-to-concept edges only.
///
/// This is #29's GC-protection input (`crate::daemon::gc::resources_with_dependents`),
/// defined independently of the #36 dependents relation on purpose: GC keeps
/// a Resource that any structural edge reaches, or that is the sole structural
/// source of another concept, whatever the edge direction means. Do not use it
/// to count dependents; use [`dependents_of`] / [`blast_radii`].
pub fn inbound_sources(graph: &Graph) -> HashMap<NodeId, Vec<NodeId>> {
    let concept_ids: HashSet<NodeId> = graph.concepts().map(|c| c.id).collect();
    let mut sources: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for e in graph.edges() {
        if !STRUCTURAL_EDGE_TYPES.contains(&e.edge_type) {
            continue;
        }
        if !concept_ids.contains(&e.source) || !concept_ids.contains(&e.target) {
            continue;
        }
        let dst_sources = sources.entry(e.target).or_default();
        if !dst_sources.contains(&e.source) {
            dst_sources.push(e.source);
        }
    }
    sources
}

/// The concept a dependence edge names as the dependent of `node`, when the
/// edge declares one ([`crate::types::Edge::dependence`]) with `node` as the
/// pillar and both ends are concepts of `graph`.
fn dependent_via(graph: &Graph, node: NodeId, e: &crate::types::Edge) -> Option<NodeId> {
    let (pillar, dependent) = e.dependence()?;
    let is_concept = |id: NodeId| matches!(graph.node(id), Some(Node::Concept(_)));
    (pillar == node && is_concept(pillar) && is_concept(dependent)).then_some(dependent)
}

/// `node`'s dependents (#36): the distinct concepts that declare a dependence
/// on it — `Dependency` sources into it plus its `Hierarchical` children —
/// each with the strongest weight among the edges declaring it, in
/// node-id order. Routed through the adjacency index (`O(degree)`), so it is
/// cheap per node.
pub fn dependents_of(graph: &Graph, node: NodeId) -> Vec<(NodeId, f64)> {
    let mut out: HashMap<NodeId, f64> = HashMap::new();
    for e in graph.incident_edges(node) {
        if let Some(dependent) = dependent_via(graph, node, e) {
            let w = out.entry(dependent).or_insert(e.weight);
            if e.weight > *w {
                *w = e.weight;
            }
        }
    }
    let mut out: Vec<(NodeId, f64)> = out.into_iter().collect();
    out.sort_by_key(|(id, _)| id.0);
    out
}

/// Dependent count for a single `node` (spec §4.1 with the #36 errata; see
/// the module docs). Prefer [`blast_radii`] when many nodes need a count.
pub fn blast_radius(graph: &Graph, node: NodeId) -> u64 {
    dependents_of(graph, node).len() as u64
}

/// All blast radii in ONE graph pass (GPT5.6sol P2-7): a map from each node
/// with at least one dependent to its dependent count. Agrees with
/// [`blast_radius`] for every node.
pub fn blast_radii(graph: &Graph) -> HashMap<NodeId, u64> {
    let concept_ids: HashSet<NodeId> = graph.concepts().map(|c| c.id).collect();
    let mut pairs: HashSet<(NodeId, NodeId)> = HashSet::new();
    for e in graph.edges() {
        if let Some((pillar, dependent)) = e.dependence()
            && concept_ids.contains(&pillar)
            && concept_ids.contains(&dependent)
        {
            pairs.insert((pillar, dependent));
        }
    }
    let mut out: HashMap<NodeId, u64> = HashMap::new();
    for (pillar, _) in pairs {
        *out.entry(pillar).or_insert(0) += 1;
    }
    out
}

/// The `[Entity, canonical]` label: concept content + type, plus the
/// canonical marker exactly when the concept is Canonical (spec §10: only
/// canonical nodes are marked, and always marked).
pub fn concept_label(hit: &RecallHit) -> String {
    let ty = hit
        .concept_type
        .map(|t| format!("{t:?}"))
        .unwrap_or_default();
    let marker = if hit.is_canonical { ", canonical" } else { "" };
    // #129: a scoped path names its repository, quoted, since two
    // repositories' `README.md` are otherwise the same line.
    let repo = match &hit.repo_scope {
        Some(repo) => format!(", repo {repo:?}"),
        None => String::new(),
    };
    format!("{} [{ty}{repo}{marker}]", hit.content)
}

/// Length of the rendered short-form node id, in hex chars (issue #9).
///
/// The rendered token must stay typeable, and `inspect`'s short-form id leg
/// only fires at this length or longer: 8 hex chars is 32 bits, so on a
/// 2,000-concept graph the chance a *content* focus of this shape also
/// uniquely names an id prefix is negligible, while shorter prefixes collide
/// with real ids often enough to hijack substring foci. The renderer and the
/// resolver share this one constant so the two cannot drift.
pub const SHORT_ID_CHARS: usize = 8;

/// The short-form node id rendered in a hit block: the first
/// [`SHORT_ID_CHARS`] hex chars of the UUID's simple form. Resolvable as an
/// `inspect` focus by the short-form id leg of `resolve_focus`.
pub fn short_id(id: NodeId) -> String {
    id.0.simple().to_string()[..SHORT_ID_CHARS].to_string()
}

/// The spec §13 load-bearing-pillar warning, verbatim except for the count.
pub fn blast_radius_warning(count: u64) -> String {
    format!("⚑ Load-bearing pillar — {count} nodes depend on this. Modify with caution.")
}

/// Human-readable agent name: strips the fixture's `agent-` prefix and
/// capitalizes (`agent-a` → `A`); any other id is used as-is (capitalized).
pub fn agent_display(id: &AgentId) -> String {
    let rest = id.as_str().strip_prefix("agent-").unwrap_or(id.as_str());
    let mut out = String::with_capacity(rest.len());
    for (i, ch) in rest.chars().enumerate() {
        if i == 0 {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The demo conflict sentence: subject is the payload's `writer` (ALGO-2).
pub fn conflict_warning(writer: &AgentId, seconds_ago: u64) -> String {
    format!(
        "Agent {} wrote to it {} seconds ago",
        agent_display(writer),
        seconds_ago
    )
}

/// One warning line per hot-list condition kind. Conflict renders the §13
/// sentence; the other three render compact, deterministic lines from their
/// payloads (the demo surface is conflict; the others must not be silent).
pub fn hot_warning(payload: &HotListPayload) -> String {
    match payload {
        HotListPayload::Conflict {
            writer,
            seconds_ago,
            ..
        } => conflict_warning(writer, *seconds_ago),
        HotListPayload::HighRisk { reason } => format!("High-risk modification: {reason}"),
        HotListPayload::Drift { hops, root } => {
            format!("Drifted {hops} hops from root goal {root}")
        }
        HotListPayload::Stale { seconds_inactive } => {
            format!("Session inactive {seconds_inactive} seconds")
        }
        HotListPayload::Contradiction { by, writer, at } => {
            contradiction_warning(*by, writer, *at, None)
        }
    }
}

/// Longest excerpt of a concept's content a contradiction line quotes, in
/// characters (#19(c)). Long enough to recognise the claim, short enough
/// that a warning line stays one line.
pub const CONTRADICTION_EXCERPT_CHARS: usize = 80;

/// `content` on one line (whitespace runs collapsed to one space), cut to
/// [`CONTRADICTION_EXCERPT_CHARS`] characters with a trailing `…` when cut.
pub fn excerpt(content: &str) -> String {
    let flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(CONTRADICTION_EXCERPT_CHARS) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// The contradiction line on a contradicted hit (#19(c)):
///
/// ```text
/// ⚠ Contradicted at 2026-09-30T12:00:00Z by agent-b: "brand x fits true to size" (id 1a2b3c4d)
/// ```
///
/// `at` is the declaring write's about-time, `writer` the agent that
/// declared it, and the id is the contradicting concept's [`short_id`],
/// a valid inspect focus. The line always names the contradicting concept,
/// so nothing is lost when its own block is cut by the token budget or a
/// layer floor. `by_content` is `None` only when the concept is not in the
/// graph being rendered, and then the quote is left out.
pub fn contradiction_warning(
    by: NodeId,
    writer: &AgentId,
    at: chrono::DateTime<chrono::Utc>,
    by_content: Option<&str>,
) -> String {
    let at = at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    match by_content {
        Some(content) => format!(
            "⚠ Contradicted at {at} by {writer}: \"{}\" (id {})",
            excerpt(content),
            short_id(by)
        ),
        None => format!("⚠ Contradicted at {at} by {writer} (id {})", short_id(by)),
    }
}

/// The companion line on a contradicting concept recall pulled in beside a
/// contradicted hit (#19(c)): which shown concept it contradicts.
///
/// ```text
/// ⚠ Contradicts "brand x runs small" (id 5e6f7a8b)
/// ```
pub fn contradicts_warning(target: NodeId, target_content: &str) -> String {
    format!(
        "⚠ Contradicts \"{}\" (id {})",
        excerpt(target_content),
        short_id(target)
    )
}

/// The most stale-citation lines one hit carries (#207). A concept that
/// cites more contradicted concepts than this shows the first ones in id
/// order and one [`rests_on_more_warning`] line for the rest, so a hit's
/// block stays bounded whatever its `depends_on` grew to.
pub const STALE_CITATION_LINES: usize = 3;

/// The stale-by-dependency line on a hit that directly cites (its own
/// `depends_on`) a contradicted concept (#207):
///
/// ```text
/// ⚠ Rests on a contradicted concept: "GET /api/v1/memo lists all memos" (id 1a2b3c4d), contradicted by "list-all removed in v0.22" (id 5e6f7a8b)
/// ```
///
/// The first quote and id are the cited concept's, the second its
/// contradicting concept's, each cut like the #19(c) lines ([`excerpt`])
/// and named by [`short_id`], a valid inspect focus.
pub fn rests_on_contradicted_warning(
    cited: NodeId,
    cited_content: &str,
    by: NodeId,
    by_content: &str,
) -> String {
    format!(
        "⚠ Rests on a contradicted concept: \"{}\" (id {}), contradicted by \"{}\" (id {})",
        excerpt(cited_content),
        short_id(cited),
        excerpt(by_content),
        short_id(by)
    )
}

/// The overflow line after [`STALE_CITATION_LINES`] stale-citation lines
/// (#207): how many more contradicted concepts the hit cites.
///
/// ```text
/// ⚠ Rests on 2 more contradicted concepts
/// ```
pub fn rests_on_more_warning(more: usize) -> String {
    let noun = if more == 1 { "concept" } else { "concepts" };
    format!("⚠ Rests on {more} more contradicted {noun}")
}

/// The suspected-supersession line on an older hit (#198), shown only when
/// the newer concept is in the same result:
///
/// ```text
/// ⚠ Possibly superseded by "the embedder listens on port 8081" (id 1a2b3c4d), not declared
/// ```
///
/// The quote and id are the suspected replacement's, cut like the #19(c)
/// lines ([`excerpt`]) and named by [`short_id`]. "Not declared" says what
/// the line is: Lambo's suspicion, not a writer's `against`.
pub fn possibly_superseded_warning(by: NodeId, by_content: &str) -> String {
    format!(
        "⚠ Possibly superseded by \"{}\" (id {}), not declared",
        excerpt(by_content),
        short_id(by)
    )
}

/// The historical supersession line on a demoted hit (#20):
///
/// ```text
/// ⤓ Superseded at 2026-09-30T12:00:00Z by "a failed flush retries at most three times" (id 1a2b3c4d)
/// ```
///
/// `at` is the demotion's time and the id is the superseding concept's
/// [`short_id`]. Unlike the live `contradiction` line it is history: it
/// stays on the concept while its latest demotion is a supersession and it
/// is not Canonical again. `by_content` is `None` when the superseding
/// concept is no longer in the graph (collected), and then the quote is
/// left out; the id still names it.
pub fn superseded_warning(
    by: NodeId,
    at: chrono::DateTime<chrono::Utc>,
    by_content: Option<&str>,
) -> String {
    let at = at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    match by_content {
        Some(content) => format!(
            "⤓ Superseded at {at} by \"{}\" (id {})",
            excerpt(content),
            short_id(by)
        ),
        None => format!("⤓ Superseded at {at} (id {})", short_id(by)),
    }
}

/// The active soft-lock line (spec §10.3.3, T2.7): agent + expiry.
pub fn reservation_warning(r: &Reservation) -> String {
    format!(
        "Reserved by {} until {}",
        r.agent_id,
        r.expires_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    )
}

/// One hit's full block: the label line (content, type marker, score, node
/// id, blast radius) followed by its warning lines. Warnings are passed in
/// already ordered: ⚑ line, hot-list lines, reservation line.
pub fn render_block(hit: &RecallHit, warnings: &[String]) -> String {
    let meta = match hit.blast_radius {
        Some(radius) => format!(
            "(score {:.2}, id {}, blast radius {radius})",
            hit.score,
            short_id(hit.node_id)
        ),
        None => format!("(score {:.2}, id {})", hit.score, short_id(hit.node_id)),
    };
    let mut lines = vec![format!("{} {meta}", concept_label(hit))];
    lines.extend(warnings.iter().cloned());
    lines.join("\n")
}

/// One hit's full block from the H3 presentation model: the label line
/// (content, type marker, score, node id, blast radius) followed by its
/// annotation warning lines. Reuses [`concept_label`] / [`render_block`] by
/// projecting the presentation hit onto a [`RecallHit`], so the CLI-rendered
/// blocks and the pipeline's own blocks cannot drift. The presentation hit
/// carries the real node id (issue #9 made the rendered id load-bearing, so
/// the old nil-id placeholder is gone).
pub(crate) fn render_detailed_block(h: &crate::recall::detail::DetailedHit) -> String {
    let hit = RecallHit {
        node_id: h.node_id,
        content: h.content.clone(),
        concept_type: h.concept_type,
        score: h.score,
        is_canonical: h.status == Some(crate::types::CanonizationStatus::Canonical),
        blast_radius: h.blast_radius,
        repo_scope: h.repo_scope.clone(),
    };
    let lines: Vec<String> = h.annotations.iter().map(|a| a.text.clone()).collect();
    ContextBlock::for_hit(&hit, &lines)
        .with_meta(&crate::recall::redundancy::also_meta(&h.skipped_redundant))
        .text
}

/// The full context: rendered blocks in final-score order, separated by one
/// blank line. No trailing newline, so the string is stable as a payload and
/// as golden text.
pub fn render_context(blocks: &[String]) -> String {
    blocks.join("\n\n")
}

/// The built-in token estimator: `ceil(byte_len / 3.5)` — the common
/// "roughly 3.5 bytes per token" heuristic for mixed prose. Pass this to
/// `crate::recall::assemble::assemble` (or any `Fn(&str) -> usize`) as the
/// `token_fn`.
pub fn default_token_count(s: &str) -> usize {
    (s.len() as f64 / 3.5).ceil() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    /// #19(c): excerpts are one line, at most
    /// `CONTRADICTION_EXCERPT_CHARS` characters plus `…`, cut on a char
    /// boundary.
    #[test]
    fn excerpts_are_one_bounded_line() {
        assert_eq!(excerpt("  a\n b\t c "), "a b c");
        let long = "é".repeat(CONTRADICTION_EXCERPT_CHARS + 5);
        let cut = excerpt(&long);
        assert_eq!(cut.chars().count(), CONTRADICTION_EXCERPT_CHARS + 1);
        assert!(cut.ends_with('…'));
        let exact = "x".repeat(CONTRADICTION_EXCERPT_CHARS);
        assert_eq!(excerpt(&exact), exact);
    }

    #[test]
    fn contradiction_lines_name_the_other_side_by_short_id() {
        let at = Utc.timestamp_opt(1_760_000_000, 0).unwrap();
        let by = NodeId(Uuid::from_u64_pair(0xabcd_ef01_0000_0000, 1));
        let line = contradiction_warning(by, &AgentId::from("agent-b"), at, Some("runs\ntrue"));
        assert_eq!(
            line,
            "⚠ Contradicted at 2025-10-09T08:53:20Z by agent-b: \"runs true\" (id abcdef01)"
        );
        assert_eq!(
            hot_warning(&HotListPayload::Contradiction {
                by,
                writer: AgentId::from("agent-b"),
                at,
            }),
            "⚠ Contradicted at 2025-10-09T08:53:20Z by agent-b (id abcdef01)"
        );
        assert_eq!(
            contradicts_warning(by, "x"),
            "⚠ Contradicts \"x\" (id abcdef01)"
        );
    }

    use crate::types::{CanonizationStatus, Concept, ConceptType, Interaction, SessionId};

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn uid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(0, u))
    }

    fn iid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, u))
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

    fn interaction(id: u64, agent: &str) -> Interaction {
        Interaction {
            event_time: None,
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from(agent),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: None,
            created_at: ts(0),
        }
    }

    /// x -> a dependency only; x -> b also sourced by y; provenance edges and
    /// an interaction-sourced structural edge must all be ignored.
    fn blast_graph() -> Graph {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, "agent-a");
        let i2 = {
            let mut i = interaction(2, "agent-b");
            i.previous_id = Some(i1.id);
            i
        };
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();
        let x = uid(10);
        let a = uid(11);
        let b = uid(12);
        let y = uid(13);
        let c = uid(14);
        let d = uid(15);
        for (id, content) in [
            (10u64, "x"),
            (11, "a"),
            (12, "b"),
            (13, "y"),
            (14, "c"),
            (15, "d"),
        ] {
            g.insert_concept(concept(id, i1.id, content), i1.id)
                .unwrap();
        }
        let dep = |s: NodeId, t: NodeId, m: i64| crate::types::Edge {
            event_time: None,
            id: uid(100 + m as u64),
            session_id: sid(),
            source: s,
            target: t,
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(m),
            last_reinforced: ts(m),
        };
        // A Dependency edge's source rests on its target (#36).
        // x depends on a: x is a's dependent.
        g.upsert_edge(dep(x, a, 1)).unwrap();
        // x and y both depend on b: b has two dependents (shared ones count).
        g.upsert_edge(dep(x, b, 2)).unwrap();
        g.upsert_edge(dep(y, b, 3)).unwrap();
        // c depends on x: x's one dependent.
        g.upsert_edge(dep(c, x, 4)).unwrap();
        // y depends on d.
        g.upsert_edge(dep(y, d, 8)).unwrap();
        // NOTE: an interaction-sourced structural edge cannot exist in a legal
        // graph (spec §5: Dependency/Causal/Hierarchical must connect Concept
        // to Concept; the write gate rejects it), so `blast_radius`'s
        // concept-endpoint filter is defensive belt-and-suspenders, not a
        // reachable case.
        // Provenance Derives (i1 -> a) and a Temporal edge: must not count.
        g.upsert_edge(crate::types::Edge {
            event_time: None,
            id: uid(106),
            session_id: sid(),
            source: i1.id,
            target: a,
            edge_type: EdgeType::Derives,
            weight: 0.9,
            reinforcements: 1,
            created_at: ts(6),
            last_reinforced: ts(6),
        })
        .unwrap();
        g.upsert_edge(crate::types::Edge {
            event_time: None,
            id: uid(107),
            session_id: sid(),
            source: i2.id,
            target: i1.id,
            edge_type: EdgeType::Temporal,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(7),
            last_reinforced: ts(7),
        })
        .unwrap();
        g
    }

    #[test]
    fn blast_radius_counts_every_dependency_source() {
        let g = blast_graph();
        assert_eq!(blast_radius(&g, uid(10)), 1, "c depends on x");
        assert_eq!(blast_radius(&g, uid(13)), 0, "nothing depends on y");
        assert_eq!(
            blast_radius(&g, uid(11)),
            1,
            "x depends on a (Derives ignored)"
        );
        assert_eq!(
            blast_radius(&g, uid(12)),
            2,
            "x and y both depend on b; a shared dependent counts for each pillar"
        );
        assert_eq!(blast_radius(&g, uid(14)), 0, "nothing depends on c");
        assert_eq!(blast_radius(&g, uid(15)), 1, "y depends on d");
        let radii = blast_radii(&g);
        for id in 10..=15u64 {
            assert_eq!(
                radii.get(&uid(id)).copied().unwrap_or(0),
                blast_radius(&g, uid(id)),
                "blast_radii agrees with blast_radius for {id}"
            );
        }
    }

    /// #36: the graph-side relation answers the shared dependents case
    /// (`crate::test_util::dependents`), and every graph-side reader agrees
    /// with it: `dependents_of`, `blast_radius`, `blast_radii` and the
    /// structural dispatch's `dependents`.
    #[test]
    fn graph_side_dependents_follow_the_dependents_case() {
        use crate::test_util::dependents as case;
        let g = case::graph();
        let radii = blast_radii(&g);
        for (name, want) in case::expected_graph() {
            let node = case::concept_id(name);
            let mut want: Vec<NodeId> = want.iter().map(|n| case::concept_id(n)).collect();
            want.sort_by_key(|id| id.0);
            let got: Vec<NodeId> = dependents_of(&g, node)
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            assert_eq!(got, want, "dependents_of({name})");
            assert_eq!(
                blast_radius(&g, node),
                want.len() as u64,
                "blast_radius({name})"
            );
            assert_eq!(
                radii.get(&node).copied().unwrap_or(0),
                want.len() as u64,
                "blast_radii({name})"
            );
            let mut dispatched: Vec<NodeId> = crate::recall::dispatch::dependents(&g, node)
                .into_iter()
                .map(|s| s.item)
                .collect();
            dispatched.sort_by_key(|id| id.0);
            assert_eq!(dispatched, want, "dispatch::dependents({name})");
        }
    }

    #[test]
    fn blast_radius_warning_line_is_byte_exact() {
        // Spec §13 verbatim, em dash included; only the count interpolates.
        assert_eq!(
            blast_radius_warning(9),
            "⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution."
        );
        assert_eq!(
            blast_radius_warning(8),
            "⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution."
        );
    }

    #[test]
    fn concept_label_marks_only_canonical() {
        let hit = |canonical: bool| RecallHit {
            repo_scope: None,
            node_id: uid(1),
            content: "user schema".into(),
            concept_type: Some(ConceptType::Entity),
            score: 0.7,
            is_canonical: canonical,
            blast_radius: None,
        };
        assert_eq!(concept_label(&hit(true)), "user schema [Entity, canonical]");
        assert_eq!(concept_label(&hit(false)), "user schema [Entity]");
    }

    #[test]
    fn conflict_warning_renders_writer_not_first_agent() {
        // ALGO-2: the naive first-listed agent is wrong on the shipped
        // fixture; the subject is the payload's writer, never `agents[0]`.
        assert_eq!(
            conflict_warning(&AgentId::from("agent-b"), 11),
            "Agent B wrote to it 11 seconds ago"
        );
        assert_eq!(
            conflict_warning(&AgentId::from("agent-a"), 11),
            "Agent A wrote to it 11 seconds ago"
        );
        // Non-prefixed ids render as-is (capitalized).
        assert_eq!(
            conflict_warning(&AgentId::from("alice"), 5),
            "Agent Alice wrote to it 5 seconds ago"
        );
    }

    #[test]
    fn hot_warning_covers_all_condition_kinds() {
        use crate::types::AgentId;
        assert_eq!(
            hot_warning(&HotListPayload::Conflict {
                agents: vec![AgentId::from("agent-a")],
                writer: AgentId::from("agent-a"),
                seconds_ago: 11,
            }),
            "Agent A wrote to it 11 seconds ago"
        );
        assert_eq!(
            hot_warning(&HotListPayload::HighRisk {
                reason: "breaking migration".into()
            }),
            "High-risk modification: breaking migration"
        );
        assert_eq!(
            hot_warning(&HotListPayload::Drift {
                hops: 3,
                root: uid(7),
            }),
            format!("Drifted 3 hops from root goal {}", uid(7))
        );
        assert_eq!(
            hot_warning(&HotListPayload::Stale {
                seconds_inactive: 300
            }),
            "Session inactive 300 seconds"
        );
    }

    #[test]
    fn reservation_warning_is_deterministic_rfc3339() {
        let r = Reservation {
            session_id: sid(),
            node_id: uid(1),
            agent_id: AgentId::from("agent-c"),
            expires_at: ts(5),
        };
        assert_eq!(
            reservation_warning(&r),
            "Reserved by agent-c until 2025-07-08T18:45:00Z"
        );
    }

    #[test]
    fn render_block_and_context_layout() {
        let hit = RecallHit {
            repo_scope: None,
            node_id: uid(1),
            content: "user schema".into(),
            concept_type: Some(ConceptType::Entity),
            score: 0.7182,
            is_canonical: true,
            blast_radius: Some(8),
        };
        let warnings = vec![
            blast_radius_warning(8),
            conflict_warning(&AgentId::from("agent-a"), 11),
        ];
        let block = render_block(&hit, &warnings);
        assert_eq!(
            block,
            "user schema [Entity, canonical] (score 0.72, id 00000000, blast radius 8)\n\
             ⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution.\n\
             Agent A wrote to it 11 seconds ago"
        );
        assert_eq!(render_context(std::slice::from_ref(&block)), "user schema [Entity, canonical] (score 0.72, id 00000000, blast radius 8)\n⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution.\nAgent A wrote to it 11 seconds ago");
        assert_eq!(render_context(&[]), "");
        assert_eq!(render_context(&["a".into(), "b".into()]), "a\n\nb");

        // Issue #9: the rendered text carries the node id per hit, right
        // after the score, so a caller working only from this surface has a
        // durable inspect focus. The token is exactly `short_id`'s output so
        // the renderer and the resolver cannot drift.
        assert!(
            block.contains(&format!("id {}", short_id(uid(1)))),
            "block must carry the node id after the score: {block}"
        );
        assert_eq!(short_id(uid(1)).len(), SHORT_ID_CHARS);
    }

    #[test]
    fn short_id_is_the_first_hex_chars_of_the_simple_form() {
        let id = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        assert_eq!(short_id(id), "f0000000");
        assert_eq!(short_id(NodeId(uuid::Uuid::nil())), "00000000");
    }

    /// The rendered id is per-hit: a multi-hit context carries as many
    /// distinct short ids as hits, and no block carries another hit's id. A
    /// regression emitting one constant id (or the nil placeholder) in every
    /// block would pass the fixture goldens, whose ids share a prefix, so
    /// this is the unit-level guard.
    #[test]
    fn a_multi_hit_context_carries_each_hits_own_short_id() {
        let ids = [
            NodeId("f0000000-0000-4000-8000-000000000001".parse().unwrap()),
            NodeId("e1000000-0000-4000-8000-000000000002".parse().unwrap()),
            NodeId("d2000000-0000-4000-8000-000000000003".parse().unwrap()),
        ];
        let blocks: Vec<String> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                render_block(
                    &RecallHit {
                        repo_scope: None,
                        node_id: *id,
                        content: format!("hit {i}"),
                        concept_type: Some(ConceptType::Entity),
                        score: 0.5,
                        is_canonical: false,
                        blast_radius: None,
                    },
                    &[],
                )
            })
            .collect();
        let context = render_context(&blocks);

        let shorts: Vec<String> = ids.iter().map(|id| short_id(*id)).collect();
        assert_eq!(
            shorts.iter().collect::<HashSet<_>>().len(),
            ids.len(),
            "the fixture ids must differ in their short form for this guard to bite"
        );
        for (i, block) in blocks.iter().enumerate() {
            assert!(
                block.contains(&format!("id {}", shorts[i])),
                "block {i} must carry its own id: {block}"
            );
            for (j, other) in shorts.iter().enumerate() {
                if i != j {
                    assert!(
                        !block.contains(other),
                        "block {i} must not carry hit {j}'s id: {block}"
                    );
                }
            }
        }
        // The nil uuid is the old placeholder's short form; it must not
        // resurface as a rendered id.
        let nil_short = short_id(NodeId(uuid::Uuid::nil()));
        assert!(!context.contains(&format!("id {nil_short}")));
    }

    #[test]
    fn default_token_count_is_ceil_bytes_over_3_5() {
        assert_eq!(default_token_count(""), 0);
        assert_eq!(default_token_count("abc"), 1); // ceil(3/3.5)
        assert_eq!(default_token_count("abcdefg"), 2); // ceil(7/3.5)
        assert_eq!(default_token_count("aaaaaaaaaaaaaaaa"), 5); // ceil(16/3.5)
    }

    /// The shipped pillar computes its dependent count from the graph
    /// (spec §4.1 contract with the #36 errata), not from the fixture's
    /// stored `blast_radius` field. Both agree on the fixture: 14.
    #[cfg(feature = "fixtures")]
    #[test]
    fn fixture_schema_user_blast_radius_computes_fourteen() {
        use crate::fixtures;
        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let g = Graph::from_snapshot(snap).expect("fixture loads");
        let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        assert_eq!(
            concept_label(&RecallHit {
                repo_scope: None,
                node_id: us,
                content: "user schema".into(),
                concept_type: Some(ConceptType::Entity),
                score: 0.0,
                is_canonical: true,
                blast_radius: None,
            }),
            "user schema [Entity, canonical]"
        );
        assert_eq!(
            blast_radius(&g, us),
            14,
            "user schema: six concepts depend on it plus its eight field children (D1..D8)"
        );
    }

    #[test]
    fn blast_radius_matches_batched_and_counts_only_declared_dependence() {
        // hub; leaf1..5 each depend on hub through a Dependency edge into it.
        // `mixed` is reached from hub only by a Semantic edge (association,
        // not dependence), and leaf1 depends on it.
        let mut g = Graph::new(sid());
        let hub = uid(100);
        let origin = interaction(99, "a");
        g.insert_interaction(origin.clone()).unwrap();
        g.insert_concept(concept_for_blast(hub, origin.id, "hub"), origin.id)
            .unwrap();
        for id in [1u64, 2, 3, 4, 5] {
            g.insert_concept(
                concept_for_blast(uid(id), origin.id, &format!("leaf{id}")),
                origin.id,
            )
            .unwrap();
        }
        // leaf1..5 each depend on hub (Dependency edge into hub).
        for id in [1u64, 2, 3, 4, 5] {
            g.upsert_edge(dep_edge(id, uid(id), hub)).unwrap();
        }
        // A Semantic edge from hub to `mixed` declares nothing; leaf1
        // depending on `mixed` makes leaf1 a dependent of `mixed` too.
        g.insert_concept(concept_for_blast(uid(6), origin.id, "mixed"), origin.id)
            .unwrap();
        g.upsert_edge(non_structural_edge(1, hub, uid(6))).unwrap();
        g.upsert_edge(dep_edge(90, uid(1), uid(6))).unwrap();

        let single = blast_radius(&g, hub);
        assert_eq!(
            single, 5,
            "the 5 leaves depend on hub; Semantic does not count"
        );
        assert_eq!(blast_radius(&g, uid(6)), 1, "leaf1 depends on mixed");

        // Batched must agree per-node.
        let radii = blast_radii(&g);
        assert_eq!(radii.get(&hub).copied().unwrap_or(0), single);
        // Every leaf also reports its own dependent count from the map.
        for id in [1u64, 2, 3, 4, 5] {
            assert_eq!(
                blast_radius(&g, uid(id)),
                radii.get(&uid(id)).copied().unwrap_or(0)
            );
        }
        // Self-loop divergence (deep-review F2): a node whose only inbound
        // structural edge is a self-loop is NOT its own dependent. The batched
        // map must agree with the single-node blast_radius (and the stores).
        let mut g = Graph::new(sid());
        let origin = interaction(97, "a");
        g.insert_interaction(origin.clone()).unwrap();
        g.insert_concept(concept_for_blast(uid(0), origin.id, "self"), origin.id)
            .unwrap();
        // Structural self-loop: uid(0) depends on uid(0) alone.
        g.upsert_edge(dep_edge(1, uid(0), uid(0))).unwrap();
        let radii = blast_radii(&g);
        let solo = blast_radius(&g, uid(0));
        assert_eq!(
            radii.get(&uid(0)).copied().unwrap_or(0),
            solo,
            "blast_radii and blast_radius agree on a structural self-loop"
        );
        assert_eq!(radii.get(&uid(0)).copied().unwrap_or(0), 0);
    }

    #[test]
    fn blast_radii_one_pass_agrees_with_per_node_over_a_larger_graph() {
        // A chain of Dependency edges 0 -> 1 -> 2 -> ... -> n: each i<n
        // depends on i+1, so every i>0 has exactly one dependent (i-1) and
        // node 0 has none. Ensure the batched map matches per-node calls.
        let mut g = Graph::new(sid());
        let n = 40;
        let origin = interaction(98, "a");
        g.insert_interaction(origin.clone()).unwrap();
        for i in 0..=n {
            g.insert_concept(
                concept_for_blast(uid(i as u64), origin.id, &format!("n{i}")),
                origin.id,
            )
            .unwrap();
        }
        for i in 1..=n {
            g.upsert_edge(dep_edge(i as u64, uid((i - 1) as u64), uid(i as u64)))
                .unwrap();
        }
        let radii = blast_radii(&g);
        for i in 0..=n {
            let node = uid(i as u64);
            let count = if i == 0 { 0 } else { 1 }; // nothing depends on 0
            assert_eq!(
                radii.get(&node).copied().unwrap_or(0),
                count,
                "node {i} dependent count from one pass"
            );
            assert_eq!(blast_radius(&g, node), count, "node {i} per-node count");
        }
    }

    fn concept_for_blast(id: NodeId, origin: NodeId, content: &str) -> crate::types::Concept {
        let mut c = concept(0, origin, content);
        // fix the id to `id`
        c.id = id;
        c.canonical_key = content.into();
        c
    }

    fn dep_edge(id: u64, src: NodeId, tgt: NodeId) -> crate::types::Edge {
        let now = ts(0);
        crate::types::Edge {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(7, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: crate::types::EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 0,
            created_at: now,
            last_reinforced: now,
        }
    }

    fn non_structural_edge(id: u64, src: NodeId, tgt: NodeId) -> crate::types::Edge {
        let now = ts(0);
        crate::types::Edge {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(8, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: crate::types::EdgeType::Semantic,
            weight: 1.0,
            reinforcements: 0,
            created_at: now,
            last_reinforced: now,
        }
    }
}
