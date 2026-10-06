//! `lambo inspect` — neighbourhood around a focus concept (reader process).
//!
//! Resolution (UUID → exact case-insensitive content → short-form node id →
//! substring; ambiguous refuses with named candidates; deterministic sort) is
//! shared with `lambo_inspect`. Caps: [`MAX_INSPECT_DEPTH`],
//! [`MAX_INSPECT_NODES`], [`MAX_INSPECT_CANDIDATES`],
//! [`MAX_INSPECT_SCAN_CONCEPTS`], [`MAX_INSPECT_BOUNDED_SCAN`].

use serde_json::json;

use super::caps::{
    check_size_cli, require_nonempty, CliError, MAX_INSPECT_CANDIDATES, MAX_INSPECT_DEPTH,
    MAX_INSPECT_NODES,
};
use super::load_reader_graph;
use crate::graph::Graph;
use crate::recall::format;
use crate::store::GraphStore;
use crate::types::{tie_break_by_key, CanonizationStatus, EdgeType, Node, NodeId};

/// Upper bound on the number of concepts `inspect`'s fuzzy (substring) leg
/// will full-scan (T8.7 residual #3 graph-size guard).
///
/// [`resolve_focus`]'s substring pass lowercases **every** concept's content —
/// O(total-content) allocation per call. The HTTP rate limit bounds the call
/// *rate*; this constant bounds the *per-call* concept set, so the combined
/// per-second work is `rate_limit×MAX_INSPECT_SCAN_CONCEPTS` no matter how large
/// the session graph grows. A graph past the cap never pays the unbounded
/// lowercase pass: the fuzzy leg falls back to the bounded subset of
/// [`MAX_INSPECT_BOUNDED_SCAN`] (issue #9), whose results are announced as
/// bounded, never silently trimmed. The exact, node-id and short-form-id legs
/// still resolve a graph past the cap. It lives here, not in `caps`, because
/// it guards this function's own iteration and the task that added it touches
/// this file.
pub(crate) const MAX_INSPECT_SCAN_CONCEPTS: usize = 2_000;

/// Size of each side of the bounded subset a past-cap graph's fuzzy leg scans
/// (issue #9): the [`MAX_INSPECT_BOUNDED_SCAN`] most recently created concepts
/// plus the [`MAX_INSPECT_BOUNDED_SCAN`] highest blast-radius concepts.
///
/// The scan cap's rationale is untouched: the O(total-content) lowercase pass
/// stays refused. But refusing outright also refused *every* fuzzy focus the
/// moment a normally growing session crossed 2,000 concepts, and the refusal
/// was silent about what would and would not be searched. Building the subset
/// allocates no concept content (reference sorts plus the fixed-size
/// blast-radius map) and the lowercase pass then runs over at most
/// `2 × MAX_INSPECT_BOUNDED_SCAN` concepts, so the
/// per-call cost stays bounded as the cap intends.
pub(crate) const MAX_INSPECT_BOUNDED_SCAN: usize = 256;

/// Proof that a fuzzy pass ran over the bounded subset a past-cap graph scans
/// rather than over every concept. `scanned` is the subset size actually
/// iterated; the cap that triggered the fallback is
/// [`MAX_INSPECT_SCAN_CONCEPTS`]. Every renderer of a `bounded` result must
/// say so; the caller is entitled to know a match outside the subset would
/// not have been found.
#[derive(Clone, Debug)]
pub(crate) struct BoundedScan {
    pub(crate) scanned: usize,
}

/// A concept `inspect` could have meant.
#[derive(Clone, Debug)]
pub(crate) struct FocusCandidate {
    pub id: NodeId,
    /// The concept's canonical key: the stable tie order ahead of the per-run
    /// node id (issue #2).
    pub canonical_key: String,
    pub content: String,
}

/// How `inspect` resolved (or refused to resolve) its `focus`.
#[derive(Debug)]
pub(crate) enum Focus {
    /// A node UUID, a short-form id, or an exact (case-insensitive) content
    /// match.
    Exact(NodeId),
    /// Exactly one substring match, usable but the caller is told. `bounded`
    /// is set when the match was found inside the bounded subset a past-cap
    /// graph scans.
    Fuzzy {
        id: NodeId,
        content: String,
        bounded: Option<BoundedScan>,
    },
    /// Several substring matches; the caller must disambiguate. `bounded`
    /// carries the same meaning as in [`Focus::Fuzzy`].
    Ambiguous {
        candidates: Vec<FocusCandidate>,
        bounded: Option<BoundedScan>,
    },
    /// Nothing matched. `near` lists the closest concepts by focus-token
    /// overlap (recency breaking ties) as *suggestions* with their node ids;
    /// every renderer must present them as suggestions, never a silent match
    /// (issue #9).
    Missing { near: Vec<FocusCandidate> },
    /// The graph exceeded [`MAX_INSPECT_SCAN_CONCEPTS`], the bounded subset
    /// ([`MAX_INSPECT_BOUNDED_SCAN`]) was scanned and matched nothing: an
    /// honest failure that still says the scan was bounded, since the concept
    /// the caller meant may exist outside the subset (issue #9). `near` lists
    /// the closest concepts within that bounded subset by focus-token overlap
    /// (recency breaking ties) as *suggestions* with their node ids; the
    /// full-graph ranking is the pass the cap refuses, so every renderer must
    /// present them as subset-scoped suggestions, never a silent match.
    Oversized {
        cap: usize,
        near: Vec<FocusCandidate>,
    },
}

/// Resolve inspect's focus **deterministically**.
///
/// `Graph::concepts()` iterates a `HashMap`, so a `.find(..)` over it would
/// pick an arbitrary match — arbitrary across runs *and* within one run.
/// Every leg here collects and sorts by a total order, and the ambiguous
/// case refuses instead of guessing.
pub(crate) fn resolve_focus(g: &Graph, focus: &str) -> Focus {
    if let Some(id) = uuid::Uuid::parse_str(focus)
        .ok()
        .map(NodeId)
        .filter(|id| g.node(*id).is_some())
    {
        return Focus::Exact(id);
    }

    let mut exact: Vec<FocusCandidate> = g
        .concepts()
        .filter(|c| c.content.eq_ignore_ascii_case(focus))
        .map(focus_candidate)
        .collect();
    if !exact.is_empty() {
        // Case-insensitive duplicates are the same concept to the caller, so
        // there is nothing to disambiguate, just pick one *stably*. Canonical
        // key ahead of the id (issue #2): the id is minted per run.
        exact.sort_by(|a, b| {
            a.content.cmp(&b.content).then_with(|| {
                tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
            })
        });
        return Focus::Exact(exact[0].id);
    }

    // Short-form id leg (issue #9): recall's rendered blocks carry
    // `id <short>`, so that token is a valid focus by construction; without
    // this leg the rendered id would point at a resolution path that refuses
    // it, recreating the affordance gap the rendered id exists to close.
    if let Some(resolved) = resolve_short_id(g, focus) {
        return resolved;
    }

    // Graph-size guard for the fuzzy leg (T8.7 residual #3). The rate limit
    // bounds the request *rate*; this bounds the *per-call* concept set the
    // O(total-content) lowercase pass iterates, so per-second work cannot grow
    // with an unattended graph. A graph past the cap falls back to the bounded
    // subset scan (issue #9) instead of paying the full pass; the count itself
    // is allocation-free, and the subset build allocates no concept content
    // (reference sorts plus the fixed-size blast-radius map), so the
    // lowercase pass over every concept never runs on an oversized graph.
    if g.concepts().count() > MAX_INSPECT_SCAN_CONCEPTS {
        return bounded_fuzzy_pass(g, focus);
    }

    let needle = focus.to_lowercase();
    let mut fuzzy = fuzzy_candidates(g.concepts(), &needle);
    if fuzzy.is_empty() {
        // Issue #9: the bare "no concept matching" refusal is how 22 of 78
        // dogfood-rig inspects failed: a caller typing an approximation of a
        // concept it read got nothing to act on. Suggest instead: the closest
        // concepts by token overlap, as suggestions with their node ids.
        return Focus::Missing {
            near: near_matches(g, &focus_tokens(&needle)),
        };
    }
    // Shortest content first: the least-padded match is the closest to what was
    // asked for. Content, then canonical key, then id break ties (issue #2),
    // so the order is total and stable across runs.
    finish_fuzzy(&mut fuzzy, None)
}

/// The fuzzy pass's shared tail: total order (shortest content first, then
/// content, then the issue #2 tie order), then one match is usable-but-announced
/// and several are refused with named candidates.
fn finish_fuzzy(fuzzy: &mut Vec<FocusCandidate>, bounded: Option<BoundedScan>) -> Focus {
    fuzzy.sort_by(|a, b| {
        a.content
            .len()
            .cmp(&b.content.len())
            .then_with(|| a.content.cmp(&b.content))
            .then_with(|| {
                tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
            })
    });
    debug_assert!(!fuzzy.is_empty(), "finish_fuzzy is for non-empty matches");
    if fuzzy.len() == 1 {
        let c = fuzzy.remove(0);
        Focus::Fuzzy {
            id: c.id,
            content: c.content,
            bounded,
        }
    } else {
        Focus::Ambiguous {
            candidates: std::mem::take(fuzzy),
            bounded,
        }
    }
}

fn focus_candidate(c: &crate::types::Concept) -> FocusCandidate {
    FocusCandidate {
        id: c.id,
        canonical_key: c.canonical_key.clone(),
        content: c.content.clone(),
    }
}

/// Substring matches over `concepts`, lowercasing each content once.
fn fuzzy_candidates<'a>(
    concepts: impl Iterator<Item = &'a crate::types::Concept>,
    needle: &str,
) -> Vec<FocusCandidate> {
    concepts
        .filter(|c| c.content.to_lowercase().contains(needle))
        .map(focus_candidate)
        .collect()
}

/// The focus's lowercase alphanumeric tokens, deduplicated, each at least two
/// characters. The split is Unicode-aware, so a non-ASCII focus keeps its
/// tokens whole instead of shedding them as separators, and single-character
/// tokens are dropped: one stray character is contained in nearly every
/// content and would make almost every concept a candidate. A focus with no
/// surviving tokens offers no suggestions.
fn focus_tokens(needle: &str) -> Vec<&str> {
    let mut tokens: Vec<&str> = Vec::new();
    for t in needle.split(|c: char| !c.is_alphanumeric()) {
        if t.chars().count() >= 2 && !tokens.contains(&t) {
            tokens.push(t);
        }
    }
    tokens
}

/// Bounded near-match ranking for a refused focus (issue #9): per concept,
/// count how many focus tokens its content contains (the lowercase pass is
/// the same cost the fuzzy leg pays), rank by overlap then recency
/// (`created_at`, newest first), then the issue #2 total order, and keep at
/// most [`MAX_INSPECT_CANDIDATES`] suggestions. Under the scan cap it ranks
/// the whole graph; past the cap the bounded subset is ranked instead, so
/// the per-call cost stays the cost the cap already accepts.
fn near_matches(g: &Graph, tokens: &[&str]) -> Vec<FocusCandidate> {
    near_matches_over(g.concepts(), tokens)
}

/// The ranking core of [`near_matches`] over any concept source, so the
/// past-cap path can rank its bounded subset without a second full-graph
/// pass. Only the kept suggestions clone content.
fn near_matches_over<'a>(
    concepts: impl Iterator<Item = &'a crate::types::Concept>,
    tokens: &[&str],
) -> Vec<FocusCandidate> {
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u32, &crate::types::Concept)> = concepts
        .filter_map(|c| {
            let lower = c.content.to_lowercase();
            let overlap = tokens.iter().filter(|t| lower.contains(*t)).count() as u32;
            (overlap > 0).then_some((overlap, c))
        })
        .collect();
    scored.sort_by(|(oa, a), (ob, b)| {
        ob.cmp(oa)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| {
                tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
            })
    });
    scored
        .into_iter()
        .take(MAX_INSPECT_CANDIDATES)
        .map(|(_, c)| focus_candidate(c))
        .collect()
}

/// The short-form id leg (issue #9): a pure-hex focus of
/// [`format::SHORT_ID_CHARS`]..=32 chars resolves against concept ids by
/// prefix: unique to [`Focus::Exact`], several to an ambiguous refusal with
/// named candidates, none to `None` (the content legs still run). The length
/// floor is [`format::SHORT_ID_CHARS`] because that is what recall renders:
/// shorter hex foci fall through to the content legs, where they belong.
///
/// The cost is O(16) nibble compares per concept against `Uuid::as_bytes`
/// with no allocation, so the leg runs regardless of the scan cap and a
/// rendered id keeps resolving after a session crosses 2,000 concepts.
fn resolve_short_id(g: &Graph, focus: &str) -> Option<Focus> {
    let bytes = focus.as_bytes();
    if bytes.len() < format::SHORT_ID_CHARS || bytes.len() > 32 {
        return None;
    }
    if !bytes.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut matches: Vec<FocusCandidate> = g
        .concepts()
        .filter(|c| id_has_hex_prefix(c.id.0, focus))
        .map(focus_candidate)
        .collect();
    match matches.len() {
        0 => None,
        1 => Some(Focus::Exact(matches.remove(0).id)),
        _ => {
            matches.sort_by(|a, b| {
                tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
            });
            Some(Focus::Ambiguous {
                candidates: matches,
                bounded: None,
            })
        }
    }
}

/// Whether `id`'s simple hex form starts with `prefix` (already validated as
/// lowercase-or-uppercase hex of length 1..=32). Allocation-free so the
/// short-form leg stays O(concepts) compares.
fn id_has_hex_prefix(id: uuid::Uuid, prefix: &str) -> bool {
    let bytes = id.as_bytes();
    for (i, ch) in prefix.bytes().enumerate() {
        let nibble = (ch as char).to_digit(16).unwrap_or(0) as u8;
        let byte = bytes[i / 2];
        let target = if i % 2 == 0 { byte >> 4 } else { byte & 0x0f };
        if nibble != target {
            return false;
        }
    }
    true
}

/// The bounded fuzzy pass for a graph past the scan cap (issue #9): scan only
/// [`bounded_subset`], then resolve exactly as the full pass would, carrying
/// [`BoundedScan`] so every renderer can say the scan was bounded. Nothing
/// matched is still [`Focus::Oversized`], an honest refusal naming the
/// bound, never a silent full-scan claim; it carries the near-match
/// suggestions ranked within the subset, so the remediation a below-cap miss
/// gets survives past the cap too.
fn bounded_fuzzy_pass(g: &Graph, focus: &str) -> Focus {
    let subset = bounded_subset(g);
    let scanned = subset.len();
    let needle = focus.to_lowercase();
    let mut fuzzy = fuzzy_candidates(subset.iter().copied(), &needle);
    if fuzzy.is_empty() {
        // The suggestions are ranked within the bounded subset ONLY: the
        // full-graph ranking is the same O(total-content) lowercase pass the
        // cap refuses, and a concept outside the subset was never scanned, so
        // suggesting it would claim knowledge the scan does not have.
        let near = near_matches_over(subset.iter().copied(), &focus_tokens(&needle));
        return Focus::Oversized {
            cap: MAX_INSPECT_SCAN_CONCEPTS,
            near,
        };
    }
    finish_fuzzy(&mut fuzzy, Some(BoundedScan { scanned }))
}

/// The bounded subset a past-cap graph's fuzzy leg scans: the
/// [`MAX_INSPECT_BOUNDED_SCAN`] most recently created concepts plus the
/// [`MAX_INSPECT_BOUNDED_SCAN`] highest blast-radius concepts (live radii, the
/// same authority the rendered blast warnings use), deduplicated. Building it
/// allocates the fixed-size blast-radius map (an O(edges) HashMap, no concept
/// content); both selections then sort references and tie-break
/// deterministically so the subset is a function of the graph.
fn bounded_subset(g: &Graph) -> Vec<&crate::types::Concept> {
    use std::collections::HashSet;

    let radii = format::blast_radii(g);
    let all: Vec<&crate::types::Concept> = g.concepts().collect();

    let mut by_recency = all.clone();
    by_recency.sort_unstable_by(|a, b| {
        b.created_at.cmp(&a.created_at).then_with(|| {
            tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
        })
    });

    let radius = |c: &crate::types::Concept| radii.get(&c.id).copied().unwrap_or(0);
    let mut by_radius = all;
    by_radius.sort_unstable_by(|a, b| {
        radius(b)
            .cmp(&radius(a))
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| {
                tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
            })
    });

    let mut seen: HashSet<NodeId> = HashSet::new();
    by_recency
        .into_iter()
        .take(MAX_INSPECT_BOUNDED_SCAN)
        .chain(by_radius.into_iter().take(MAX_INSPECT_BOUNDED_SCAN))
        .filter(|c| seen.insert(c.id))
        .collect()
}

/// Render a BFS neighbourhood around `target`. Caller holds the graph read
/// lock; this function never awaits (spec §6.4).
pub(crate) fn render_neighbourhood(
    g: &Graph,
    target: NodeId,
    depth: usize,
) -> (String, serde_json::Value) {
    use std::collections::{HashMap, HashSet};

    let radii = format::blast_radii(g);
    let label = |id: NodeId| -> String {
        match g.node(id) {
            Some(Node::Concept(c)) => {
                let canon = match c.canonization_status {
                    CanonizationStatus::Canonical => ", canonical",
                    CanonizationStatus::Venerable => ", venerable",
                    CanonizationStatus::Candidate => ", candidate",
                    CanonizationStatus::None => "",
                };
                format!("{} [{:?}{}]", c.content, c.concept_type, canon)
            }
            Some(Node::Interaction(i)) => {
                format!("<interaction {}>", i.id.0)
            }
            None => format!("<missing {}>", id.0),
        }
    };

    let mut text = String::new();
    text.push_str(&format!("focus: {}\n", label(target)));
    if let Some(r) = radii.get(&target) {
        text.push_str(&format!("blast radius: {r}\n"));
        if *r > 0 {
            text.push_str(&format!("{}\n", format::blast_radius_warning(*r)));
        }
    }
    if let Some(res) = g.reservation(target) {
        text.push_str(&format!("{}\n", format::reservation_warning(res)));
    }

    let mut seen: HashSet<NodeId> = HashSet::new();
    seen.insert(target);
    let mut frontier = vec![target];
    let mut levels: Vec<serde_json::Value> = Vec::new();
    let mut budget = MAX_INSPECT_NODES;

    for hop in 1..=depth {
        let mut next = Vec::new();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut by_type: HashMap<EdgeType, Vec<String>> = HashMap::new();
        for &node in &frontier {
            for edge in g.incident_edges(node) {
                let other = if edge.source == node {
                    edge.target
                } else {
                    edge.source
                };
                // Budget first, `seen` second: marking a node seen and *then*
                // discovering the budget is spent permanently excludes a
                // neighbour that was never rendered.
                if budget == 0 {
                    break;
                }
                if !seen.insert(other) {
                    continue;
                }
                budget -= 1;
                let dir = if edge.source == node { "->" } else { "<-" };
                by_type
                    .entry(edge.edge_type)
                    .or_default()
                    .push(format!("{dir} {}", label(other)));
                rows.push(json!({
                    "node_id": other.0.to_string(),
                    "label": label(other),
                    "edge_type": format!("{:?}", edge.edge_type),
                    "direction": dir,
                    "weight": edge.weight,
                }));
                next.push(other);
            }
        }
        if rows.is_empty() {
            break;
        }
        text.push_str(&format!("\nhop {hop}:\n"));
        let mut kinds: Vec<_> = by_type.into_iter().collect();
        kinds.sort_by_key(|(k, _)| format!("{k:?}"));
        for (kind, mut entries) in kinds {
            entries.sort();
            text.push_str(&format!("  {kind:?}\n"));
            for e in entries {
                text.push_str(&format!("    {e}\n"));
            }
        }
        levels.push(json!({ "hop": hop, "neighbours": rows }));
        frontier = next;
        if budget == 0 {
            text.push_str(&format!(
                "\n(truncated at {MAX_INSPECT_NODES} neighbours)\n"
            ));
            break;
        }
    }

    let structured = json!({
        "node_id": target.0.to_string(),
        "label": label(target),
        "blast_radius": radii.get(&target).copied().unwrap_or(0),
        "levels": levels,
    });
    (text, structured)
}

/// Inspect the neighbourhood around `focus` (lease-free reader).
pub async fn run(
    store: &dyn GraphStore,
    session: &str,
    focus: &str,
    depth: usize,
) -> Result<String, CliError> {
    require_nonempty("session", session)?;
    check_size_cli("session", session)?;
    require_nonempty("focus", focus)?;
    check_size_cli("focus", focus)?;
    if depth > MAX_INSPECT_DEPTH {
        return Err(CliError::Usage(format!(
            "depth must be in 0..={MAX_INSPECT_DEPTH}"
        )));
    }

    let loaded = load_reader_graph(store, session).await?;
    let g = loaded.graph.read();
    match resolve_focus(&g, focus.trim()) {
        Focus::Exact(id) => {
            let (text, _) = render_neighbourhood(&g, id, depth);
            Ok(text)
        }
        Focus::Fuzzy {
            id,
            content: matched,
            bounded,
        } => {
            // A fuzzy resolution is stated, never silent, and a bounded one
            // names the bound, so the caller knows what was not scanned.
            let note = match bounded {
                None => format!(
                    "resolved '{}' → '{}' (substring match, single candidate)",
                    focus.trim(),
                    matched
                ),
                Some(b) => format!(
                    "resolved '{}' → '{}' (substring match within the bounded scan of {} \
                     concepts; this graph is past the {}-concept full-scan cap, so concepts \
                     outside the subset were not scanned)",
                    focus.trim(),
                    matched,
                    b.scanned,
                    MAX_INSPECT_SCAN_CONCEPTS
                ),
            };
            let (text, _) = render_neighbourhood(&g, id, depth);
            Ok(format!("{note}\n{text}"))
        }
        Focus::Ambiguous {
            candidates,
            bounded,
        } => {
            let mut msg = match bounded {
                None => format!(
                    "'{}' matches {} concepts — name one exactly, or pass its node_id:",
                    focus.trim(),
                    candidates.len()
                ),
                Some(b) => format!(
                    "'{}' matches {} concepts within the bounded scan of {} concepts (this \
                     graph is past the {}-concept full-scan cap); name one exactly, or pass \
                     its node_id:",
                    focus.trim(),
                    candidates.len(),
                    b.scanned,
                    MAX_INSPECT_SCAN_CONCEPTS
                ),
            };
            for c in candidates.iter().take(MAX_INSPECT_CANDIDATES) {
                msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
            }
            if candidates.len() > MAX_INSPECT_CANDIDATES {
                msg.push_str(&format!(
                    "\n  … and {} more",
                    candidates.len() - MAX_INSPECT_CANDIDATES
                ));
            }
            Err(CliError::Usage(msg))
        }
        Focus::Oversized { cap, near } => {
            let mut msg = format!(
                "this session's graph has more than {cap} concepts; the fuzzy pass scanned only \
                 the bounded subset (the {MAX_INSPECT_BOUNDED_SCAN} most recently created plus \
                 the {MAX_INSPECT_BOUNDED_SCAN} highest blast-radius concepts) and matched \
                 nothing; pass a node_id or an exact concept instead"
            );
            if !near.is_empty() {
                // Subset-scoped suggestions (issue #9): the ranking never left
                // the bounded subset, so the renderer must say so.
                msg.push_str(
                    "\nnearest within the bounded subset (suggestions, not matches; \
                     pass a node_id or name one exactly):",
                );
                for c in near.iter().take(MAX_INSPECT_CANDIDATES) {
                    msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
                }
            }
            Err(CliError::Runtime(msg))
        }
        Focus::Missing { near } => {
            let mut msg = format!("no concept matching '{}' in session '{}'", focus, session);
            if !near.is_empty() {
                // Suggestions, never a silent match (issue #9): the caller
                // gets a nameable target with its node id.
                msg.push_str("\nsuggestions (not matches; pass a node_id or name one exactly):");
                for c in near.iter().take(MAX_INSPECT_CANDIDATES) {
                    msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
                }
            }
            Err(CliError::Runtime(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::graph::Graph;
    use crate::types::{AgentId, CanonizationStatus, Concept, ConceptType, Interaction, SessionId};

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn ts() -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    fn interaction(id: u64) -> Interaction {
        Interaction {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: None,
            created_at: ts(),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.to_string(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            chunk_group_id: None,
        }
    }

    fn graph_with_concepts(n: usize) -> (Graph, NodeId, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let mut last = iid;
        for k in 1..=n {
            let c = concept(k as u64, iid, &format!("concept-{k}"));
            last = c.id;
            g.insert_concept(c, iid).unwrap();
        }
        (g, iid, last)
    }

    /// The shared concept fixture with its id and creation time overridden,
    /// so a test can pin the short-form id leg and the recency ordering.
    fn concept_shaped(
        id: NodeId,
        origin: NodeId,
        content: &str,
        created: chrono::DateTime<Utc>,
    ) -> Concept {
        let mut c = concept(0, origin, content);
        c.id = id;
        c.created_at = created;
        c
    }

    fn dep_edge(id: u64, src: NodeId, tgt: NodeId) -> crate::types::Edge {
        crate::types::Edge {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(7, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: crate::types::EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(),
            last_reinforced: ts(),
        }
    }

    /// The fuzzy leg must refuse a graph past [`MAX_INSPECT_SCAN_CONCEPTS`]
    /// instead of running its O(total-content) lowercase pass (T8.7 #3 guard),
    /// while the exact and node-id legs still resolve.
    #[test]
    fn a_graph_past_the_scan_cap_refuses_only_the_fuzzy_leg() {
        let (g, _iid, cid) = graph_with_concepts(MAX_INSPECT_SCAN_CONCEPTS + 1);

        // Exact content match still resolves — the guard only gates the
        // substring leg's lowercase pass.
        assert!(matches!(resolve_focus(&g, "concept-7"), Focus::Exact(_)));
        // Node-id focus still resolves.
        assert!(matches!(
            resolve_focus(&g, &cid.0.to_string()),
            Focus::Exact(_)
        ));
        // A non-matching substring focus is refused before the O(total-content)
        // pass, not silently scanned.
        match resolve_focus(&g, "no-such-substring") {
            Focus::Oversized { cap, .. } => assert_eq!(cap, MAX_INSPECT_SCAN_CONCEPTS),
            other => panic!("a graph past the cap must refuse the fuzzy leg, got {other:?}"),
        }
    }

    /// A graph within the cap still resolves the fuzzy leg normally (so the
    /// guard does not fire spuriously).
    #[test]
    fn a_graph_within_the_scan_cap_still_resolves_the_fuzzy_leg() {
        let (g, _iid, _cid) = graph_with_concepts(3);
        match resolve_focus(&g, "concept-2") {
            Focus::Fuzzy { .. } | Focus::Exact(_) => {}
            other => panic!("a small graph must resolve a substring focus, got {other:?}"),
        }
    }

    /// Issue #9: a refused focus must hand the caller something to act on.
    /// The suggestions are ranked by focus-token overlap and carry real node
    /// ids; no token overlap means no suggestions, never a fake one.
    #[test]
    fn a_missing_focus_suggests_near_matches_with_their_ids() {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let codec = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        let clock = NodeId("f0000000-0000-4000-8000-000000001002".parse().unwrap());
        g.insert_concept(
            concept_shaped(codec, iid, "vector storage codec", ts()),
            iid,
        )
        .unwrap();
        g.insert_concept(
            concept_shaped(clock, iid, "vector clock ordering", ts()),
            iid,
        )
        .unwrap();
        g.insert_concept(concept(3, iid, "postgres connection pool"), iid)
            .unwrap();

        match resolve_focus(&g, "vector codec") {
            Focus::Missing { near } => {
                assert_eq!(
                    near.len(),
                    2,
                    "only concepts sharing a focus token are suggestions: {near:?}"
                );
                assert_eq!(
                    near[0].content, "vector storage codec",
                    "two shared tokens outrank one"
                );
                assert_eq!(near[0].id, codec, "suggestions carry their node id");
                assert_eq!(near[1].content, "vector clock ordering");
                assert_eq!(near[1].id, clock);
            }
            other => panic!("a missing focus must suggest, got {other:?}"),
        }

        // A focus sharing no token with any concept offers nothing: the
        // refusal stays bare rather than inventing a suggestion.
        match resolve_focus(&g, "zzz qqq") {
            Focus::Missing { near } => assert!(near.is_empty(), "{near:?}"),
            other => panic!("expected a bare Missing, got {other:?}"),
        }
    }

    /// Issue #9: when several concepts tie on token overlap, the more
    /// recently created one suggests first.
    #[test]
    fn near_match_ties_break_on_recency() {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        g.insert_concept(
            concept_shaped(NodeId(Uuid::from_u64_pair(2, 1)), iid, "widget frame", ts()),
            iid,
        )
        .unwrap();
        g.insert_concept(
            concept_shaped(
                NodeId(Uuid::from_u64_pair(2, 2)),
                iid,
                "widget mount",
                ts() + chrono::Duration::minutes(5),
            ),
            iid,
        )
        .unwrap();

        match resolve_focus(&g, "widget bracket") {
            Focus::Missing { near } => {
                assert_eq!(near.len(), 2);
                assert_eq!(
                    near[0].content, "widget mount",
                    "equal overlap ranks the newer concept first"
                );
                assert_eq!(near[1].content, "widget frame");
            }
            other => panic!("expected Missing with suggestions, got {other:?}"),
        }
    }

    /// Issue #9: recall's rendered blocks carry `id <short>`, so that token
    /// must be a valid focus by construction. At the rendered length it
    /// resolves to its node; below it the id leg does not fire; a shared
    /// short form refuses with named candidates.
    #[test]
    fn a_short_form_node_id_is_a_valid_focus() {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let id_a = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        let id_b = NodeId("f0000001-0000-4000-8000-000000001002".parse().unwrap());
        g.insert_concept(concept_shaped(id_a, iid, "alpha pad", ts()), iid)
            .unwrap();
        g.insert_concept(concept_shaped(id_b, iid, "beta pad", ts()), iid)
            .unwrap();

        match resolve_focus(&g, "f0000000") {
            Focus::Exact(id) => assert_eq!(id, id_a),
            other => panic!("the rendered short form must resolve, got {other:?}"),
        }
        // Below the rendered length there is no id leg, so an unmatched
        // 7-char hex focus is a Missing with suggestions, not a resolution.
        match resolve_focus(&g, "f000000") {
            Focus::Missing { near } => assert!(near.is_empty(), "{near:?}"),
            other => panic!("a sub-short-form hex focus must not resolve via ids, got {other:?}"),
        }
        // Exact content still wins over the id leg.
        assert!(matches!(resolve_focus(&g, "alpha pad"), Focus::Exact(id) if id == id_a));

        // Two concepts sharing their first 8 hex chars refuse ambiguously.
        let (g2, _iid, _cid) = graph_with_concepts(2);
        match resolve_focus(&g2, "00000000") {
            Focus::Ambiguous {
                candidates,
                bounded: None,
            } => {
                assert_eq!(candidates.len(), 2, "{candidates:?}");
            }
            other => panic!("a shared short form must refuse with candidates, got {other:?}"),
        }
    }

    /// Issue #9: past the scan cap the fuzzy leg scans the bounded subset
    /// (the most recent concepts plus the highest blast-radius ones) and
    /// says so. A concept inside the subset resolves fuzzily carrying the
    /// bounded proof; a concept outside it fails honestly as Oversized: the
    /// match exists, the bounded scan just never looked at it.
    #[test]
    fn past_the_cap_the_fuzzy_leg_scans_a_bounded_subset() {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i.clone()).unwrap();
        // Staggered creation times, so the recency side of the subset is
        // exactly the last MAX_INSPECT_BOUNDED_SCAN concepts created.
        for k in 1..=(MAX_INSPECT_SCAN_CONCEPTS as u64 + 1) {
            let mut c = concept(k, iid, &format!("concept-{k}"));
            c.created_at = ts() + chrono::Duration::minutes(k as i64);
            g.insert_concept(c, iid).unwrap();
        }
        // An old concept made load-bearing enters the subset through the
        // blast-radius side: concept-1 depends on it exclusively, so its live
        // radius is 1.
        let mut anchor = concept(9_999, iid, "blast anchor pad");
        anchor.created_at = ts();
        let anchor_id = anchor.id;
        g.insert_concept(anchor, iid).unwrap();
        let dependent = g
            .concepts()
            .find(|c| c.content == "concept-1")
            .map(|c| c.id)
            .unwrap();
        g.upsert_edge(dep_edge(1, anchor_id, dependent)).unwrap();

        // Inside the recency window: fuzzy, announced as bounded, with a
        // subset size the two sides bound.
        match resolve_focus(&g, "ncept-2001") {
            Focus::Fuzzy {
                content,
                bounded: Some(b),
                ..
            } => {
                assert_eq!(content, "concept-2001");
                assert!(
                    b.scanned >= MAX_INSPECT_BOUNDED_SCAN
                        && b.scanned <= 2 * MAX_INSPECT_BOUNDED_SCAN,
                    "scanned {} is not a bounded subset",
                    b.scanned
                );
            }
            other => panic!(
                "a concept inside the recency window must resolve within the bounded scan, got {other:?}"
            ),
        }
        // Inside the blast-radius side despite being old.
        match resolve_focus(&g, "anchor pad") {
            Focus::Fuzzy {
                id,
                bounded: Some(_),
                ..
            } => assert_eq!(id, anchor_id),
            other => panic!(
                "a load-bearing concept must be reachable through the blast-radius side, got {other:?}"
            ),
        }
        // A matching concept outside both sides: honest failure, not silence.
        // "oncept-42" substring-matches concept-42 and concept-420..429, all
        // old and unconnected, so a full scan would have found them. The miss
        // still carries subset-scoped suggestions: every subset concept
        // shares the "oncept" token, and the list stays at the cap.
        match resolve_focus(&g, "oncept-42") {
            Focus::Oversized { cap, near } => {
                assert_eq!(cap, MAX_INSPECT_SCAN_CONCEPTS);
                assert!(
                    !near.is_empty() && near.len() <= MAX_INSPECT_CANDIDATES,
                    "a past-cap miss must suggest within the bounded subset: {near:?}"
                );
            }
            other => panic!(
                "a concept outside the bounded subset must fail honestly as Oversized, got {other:?}"
            ),
        }
    }

    /// Issue #9 round 1: a past-cap miss keeps the near-match remediation,
    /// but ranked within the bounded subset only. "widget mount" is the
    /// newest concept, so it sits in the subset and is suggested;
    /// "widget frame" is old and unconnected, so the subset never scanned it.
    /// A full-graph ranking would suggest both (equal token overlap, recency
    /// decides), so exactly one suggestion proves the ranking stayed inside
    /// the subset the cap accepts.
    #[test]
    fn a_past_cap_miss_suggests_only_within_the_bounded_subset() {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        for k in 1..=(MAX_INSPECT_SCAN_CONCEPTS as u64 + 1) {
            let mut c = concept(k, iid, &format!("concept-{k}"));
            c.created_at = ts() + chrono::Duration::minutes(k as i64);
            g.insert_concept(c, iid).unwrap();
        }
        let mount = NodeId(Uuid::from_u64_pair(2, 9_001));
        let mut newest = concept(9_001, iid, "widget mount");
        newest.id = mount;
        newest.created_at = ts() + chrono::Duration::minutes(9_001);
        g.insert_concept(newest, iid).unwrap();
        let mut oldest = concept(9_002, iid, "widget frame");
        oldest.created_at = ts();
        g.insert_concept(oldest, iid).unwrap();

        match resolve_focus(&g, "widget bracket") {
            Focus::Oversized { cap, near } => {
                assert_eq!(cap, MAX_INSPECT_SCAN_CONCEPTS);
                assert_eq!(
                    near.len(),
                    1,
                    "only the in-subset concept may be suggested: {near:?}"
                );
                assert_eq!(near[0].id, mount);
                assert_eq!(near[0].content, "widget mount");
            }
            other => panic!(
                "a past-cap miss must still suggest within the bounded subset, got {other:?}"
            ),
        }
    }

    /// Round 1: the tokenizer drops single-character tokens and keeps
    /// non-ASCII characters inside their tokens, so one stray character
    /// cannot make every concept a candidate while a non-ASCII focus still
    /// suggests by its whole tokens.
    #[test]
    fn focus_tokens_drop_single_characters_and_keep_non_ascii_whole() {
        // The contract, at the tokenizer itself.
        assert_eq!(focus_tokens("café order"), vec!["café", "order"]);
        assert_eq!(focus_tokens("a b hi"), vec!["hi"]);
        assert!(focus_tokens("x y").is_empty());

        // Behaviorally: the single-char token "a" is inside both contents, so
        // the old tokenizer suggested everything; dropped, the refusal is
        // bare.
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        g.insert_concept(concept(1, iid, "alpha pad"), iid).unwrap();
        g.insert_concept(concept(2, iid, "beta pad"), iid).unwrap();

        match resolve_focus(&g, "a x") {
            Focus::Missing { near } => assert!(near.is_empty(), "{near:?}"),
            other => panic!("single-character tokens must not suggest, got {other:?}"),
        }

        // A whole non-ASCII token still matches its content.
        g.insert_concept(concept(3, iid, "café specials"), iid)
            .unwrap();
        match resolve_focus(&g, "café menu") {
            Focus::Missing { near } => {
                assert_eq!(near.len(), 1, "{near:?}");
                assert_eq!(near[0].content, "café specials");
            }
            other => panic!("a non-ASCII token must still suggest, got {other:?}"),
        }
    }
}
