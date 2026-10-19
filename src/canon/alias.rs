//! The alias backfill preview (#36 PR 5): which citations of a short name
//! would move to the ruling the name stands for, computed on a copy and
//! never applied.
//!
//! # The defect
//!
//! Agents cited rulings by a short name long before citations by id existed
//! (#36 PR 4). `record_action`'s text rule minted each name as an `Entity`,
//! so the citations landed on the name and not on the ruling: the name gains
//! dependents and climbs, the ruling does not (design note, section 2; the
//! fair-test fixture pins it as `lease_alias`, Venerable).
//!
//! # How a redirect is expressed
//!
//! 0.6.0 has no names (#147: in-memory synonyms are not safe under S5), so a
//! redirect is expressed in **citation edges**. Every `citer -Dependency->
//! alias` edge becomes `citer -Dependency-> ruling`, which is exactly the
//! edge a citation by id writes, keeping the original edge's times (the
//! citation was made then, so the spread Stage 2 measures stays honest), and
//! the alias edge is retired. An `alias -Dependency-> ruling` edge was
//! considered and rejected: the alias would keep its dependents and still
//! climb, and the ruling would gain one dependent from the alias's own origin
//! interaction, which nobody cited.
//!
//! # What is refused
//!
//! * **Ambiguous**: a name whose leading phrase starts more than one ruling,
//!   or a map naming two rulings for one alias, or a map reference that is a
//!   shared short id. Nothing is picked.
//! * **Chains and cycles** between pairs (`a -> b`, `b -> c`, or back to
//!   `a`): redirects are direct, never transitive, like the synonyms they
//!   replace.
//! * **Self**: an alias mapped to itself.
//! * A map reference by **content**: the map names concepts by id only, so
//!   it carries no concept text.
//!
//! Per citation: a citer that already cites the ruling has its alias edge
//! retired and nothing added (idempotent); the ruling citing its own alias is
//! retired and never becomes a self-loop; a redirect that would close a
//! `Causal`/`Dependency`/`Hierarchical` cycle (spec §5.7's acyclicity, as
//! the graph's own invariant checks it) is
//! left on the alias and counted. The cycle check walks `Causal`,
//! `Dependency` and `Hierarchical` edges, as the graph's invariant does. The alias's other edges (its `parent_of`
//! children, provenance) are untouched and counted.
//!
//! Planning a second time on the result changes nothing: the alias has no
//! citations left to move (`a_second_plan_on_the_result_is_empty`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::graph::canonical::normalize_tokens;
use crate::graph::Graph;
use crate::surface::resolve::{resolve_reference, Leg, Resolved};
use crate::types::{CanonizationStatus, ConceptType, Edge, EdgeType, GraphSnapshot, Node, NodeId};

/// Where a pair came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairSource {
    /// The leading-phrase rule found it.
    Detected,
    /// An operator's reviewed map named it.
    Reviewed,
    /// The near-duplicate detector suggested it (#178).
    Similar,
}

/// One proposed alias-to-ruling pair, before planning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AliasPair {
    /// The short name the citations landed on.
    pub alias: NodeId,
    /// The ruling it stands for.
    pub ruling: NodeId,
    /// Detected or reviewed.
    pub source: PairSource,
    /// The map line it came from, for a reviewed pair.
    pub line: Option<usize>,
}

/// Why a pair or a map line was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalKind {
    /// More than one ruling for one alias, or a shared short id.
    Ambiguous,
    /// The pairs lead back to where they started.
    Cycle,
    /// A pair's ruling is another pair's alias (or the reverse).
    Chain,
    /// An alias mapped to itself.
    SelfRedirect,
    /// A map reference that names no concept.
    Unresolved,
    /// A map reference that names an interaction.
    NotAConcept,
    /// A map reference by content instead of by id.
    ByContent,
    /// A map line that is not two references.
    Malformed,
}

/// One refused pair or map line. Carries ids only, never concept text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AliasRefusal {
    /// Why.
    pub kind: RefusalKind,
    /// The alias, when it resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<NodeId>,
    /// The rulings involved (the candidates, for an ambiguity).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rulings: Vec<NodeId>,
    /// The map line, for a reviewed pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// Detected or reviewed.
    pub source: PairSource,
}

/// One accepted pair and what planning it does to the citations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AliasRedirect {
    /// The short name.
    pub alias: NodeId,
    /// Its type label.
    pub alias_type: ConceptType,
    /// The ruling.
    pub ruling: NodeId,
    /// Its type label.
    pub ruling_type: ConceptType,
    /// Detected or reviewed.
    pub source: PairSource,
    /// Citations re-pointed from the alias to the ruling.
    pub redirected: usize,
    /// Citers that already cited the ruling: the alias edge is retired and
    /// nothing is added.
    pub already_cited: usize,
    /// The ruling's own citations of the alias: retired, never a self-loop.
    pub self_dropped: usize,
    /// Citations left on the alias because re-pointing them would close a
    /// `Causal`/`Dependency`/`Hierarchical` cycle.
    pub cycle_refused: usize,
    /// The alias's dependents that are not citations (its `parent_of`
    /// children), left in place.
    pub other_dependents_kept: usize,
    /// The alias's stored status. Canonization never demotes for lost
    /// support, so a redirect leaves it as it is (review M3).
    pub alias_status: CanonizationStatus,
    /// How many accepted aliases name this redirect's ruling (review L4: a
    /// ruling many names point at deserves a second look).
    pub ruling_fan_in: usize,
}

impl AliasRedirect {
    /// Citations still on the alias after the plan: the ones kept because
    /// moving them would close a cycle.
    pub fn citations_left(&self) -> usize {
        self.cycle_refused
    }
}

/// Something about an accepted reviewed pair worth a second look; never a
/// refusal (review L4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    /// The alias is not an Entity (a swapped line?).
    AliasNotAnEntity,
    /// The ruling is not a Logic or Constraint (a swapped line?).
    RulingNotARuling,
}

/// A warning about one accepted pair.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AliasWarning {
    /// What.
    pub kind: WarningKind,
    /// The alias.
    pub alias: NodeId,
    /// The ruling.
    pub ruling: NodeId,
    /// The map line, for a reviewed pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

/// A whole preview plan: the accepted redirects, the refusals and the edge
/// changes that would express them. Nothing here is written anywhere.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AliasPlan {
    /// Accepted pairs, alias id ascending.
    pub redirects: Vec<AliasRedirect>,
    /// Refused pairs and map lines.
    pub refusals: Vec<AliasRefusal>,
    /// Accepted pairs worth a second look.
    pub warnings: Vec<AliasWarning>,
    /// Edge ids the overlay drops.
    pub retire: Vec<NodeId>,
    /// Edges the overlay adds.
    pub add: Vec<Edge>,
}

impl AliasPlan {
    /// Whether applying the plan would change no edge.
    pub fn is_noop(&self) -> bool {
        self.retire.is_empty() && self.add.is_empty()
    }
}

/// The tokens a leading-phrase match compares: the content with ASCII
/// punctuation other than `-` and `_` turned into spaces (so `Wedge
/// invariant:` starts with the tokens of `wedge invariant`), then the
/// canonical normalizer's own tokens (case, stopwords, stems), in order.
fn phrase_tokens(content: &str) -> Vec<String> {
    let cleaned: String = content
        .chars()
        .map(|c| {
            if c.is_ascii_punctuation() && c != '-' && c != '_' {
                ' '
            } else {
                c
            }
        })
        .collect();
    normalize_tokens(&cleaned)
}

fn is_ruling(t: ConceptType) -> bool {
    matches!(t, ConceptType::Logic | ConceptType::Constraint)
}

fn concept_type(g: &Graph, id: NodeId) -> Option<ConceptType> {
    match g.node(id) {
        Some(Node::Concept(c)) => Some(c.concept_type),
        _ => None,
    }
}

/// `citer -Dependency-> alias` edges: a concept other than the alias citing
/// it.
fn citations_of(g: &Graph, alias: NodeId) -> Vec<&Edge> {
    let mut out: Vec<&Edge> = g
        .edges()
        .filter(|e| {
            e.edge_type == EdgeType::Dependency
                && e.target == alias
                && e.source != alias
                && matches!(g.node(e.source), Some(Node::Concept(_)))
        })
        .collect();
    out.sort_by_key(|e| e.id.0);
    out
}

/// The leading-phrase rule (design note, section 2): an `Entity` that at
/// least one concept cites, whose phrase tokens are a strict prefix of a
/// `Logic` or `Constraint` concept's. One ruling is a pair; more than one is
/// an ambiguity, refused.
pub fn detect(g: &Graph) -> (Vec<AliasPair>, Vec<AliasRefusal>) {
    let mut by_first: HashMap<String, Vec<(NodeId, Vec<String>)>> = HashMap::new();
    for c in g.concepts().filter(|c| is_ruling(c.concept_type)) {
        let tokens = phrase_tokens(&c.content);
        if let Some(first) = tokens.first() {
            by_first
                .entry(first.clone())
                .or_default()
                .push((c.id, tokens));
        }
    }
    let mut aliases: Vec<_> = g
        .concepts()
        .filter(|c| c.concept_type == ConceptType::Entity)
        .collect();
    aliases.sort_by_key(|c| c.id.0);
    let mut pairs = Vec::new();
    let mut refusals = Vec::new();
    for alias in aliases {
        let tokens = phrase_tokens(&alias.content);
        let Some(first) = tokens.first() else {
            continue;
        };
        let Some(rulings) = by_first.get(first) else {
            continue;
        };
        let mut found: Vec<NodeId> = rulings
            .iter()
            .filter(|(id, t)| {
                *id != alias.id && t.len() > tokens.len() && t[..tokens.len()] == tokens[..]
            })
            .map(|(id, _)| *id)
            .collect();
        if found.is_empty() || citations_of(g, alias.id).is_empty() {
            continue;
        }
        found.sort_by_key(|id| id.0);
        if found.len() == 1 {
            pairs.push(AliasPair {
                alias: alias.id,
                ruling: found[0],
                source: PairSource::Detected,
                line: None,
            });
        } else {
            refusals.push(AliasRefusal {
                kind: RefusalKind::Ambiguous,
                alias: Some(alias.id),
                rulings: found,
                line: None,
                source: PairSource::Detected,
            });
        }
    }
    (pairs, refusals)
}

/// Parse a reviewed map: one `alias-ref ruling-ref` pair per line, each a
/// full node id or an 8-hex short id, `#` starting a comment. A reference by
/// content is refused (the map names concepts by id, so it holds no concept
/// text), and so is anything that does not resolve to exactly one concept.
pub fn parse_map(g: &Graph, text: &str) -> (Vec<AliasPair>, Vec<AliasRefusal>) {
    let mut pairs = Vec::new();
    let mut refusals = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let body = raw.split('#').next().unwrap_or("").trim();
        if body.is_empty() {
            continue;
        }
        let refs: Vec<&str> = body.split_whitespace().collect();
        let refuse = |kind, alias, rulings| AliasRefusal {
            kind,
            alias,
            rulings,
            line: Some(line),
            source: PairSource::Reviewed,
        };
        if refs.len() != 2 {
            refusals.push(refuse(RefusalKind::Malformed, None, Vec::new()));
            continue;
        }
        let resolve = |r: &str| match resolve_reference(g, r) {
            Resolved::Concept {
                node,
                leg: Leg::NodeId | Leg::ShortId,
            } => Ok(node),
            Resolved::Concept { leg: Leg::Key, .. } => Err((RefusalKind::ByContent, Vec::new())),
            Resolved::Missing => Err((RefusalKind::Unresolved, Vec::new())),
            Resolved::Ambiguous(ids) => Err((RefusalKind::Ambiguous, ids)),
            Resolved::NotAConcept(_) => Err((RefusalKind::NotAConcept, Vec::new())),
        };
        match (resolve(refs[0]), resolve(refs[1])) {
            (Ok(alias), Ok(ruling)) => pairs.push(AliasPair {
                alias,
                ruling,
                source: PairSource::Reviewed,
                line: Some(line),
            }),
            (Err((kind, ids)), _) => refusals.push(refuse(kind, None, ids)),
            (Ok(alias), Err((kind, ids))) => refusals.push(refuse(kind, Some(alias), ids)),
        }
    }
    (pairs, refusals)
}

/// Plan `pairs` against `g`: refuse self, ambiguous, chained and cyclic
/// pairs, then work out, citation by citation, which edges the overlay would
/// retire and add. `refusals` (from [`detect`] or [`parse_map`]) are carried
/// into the plan.
pub fn plan(g: &Graph, pairs: &[AliasPair], refusals: Vec<AliasRefusal>) -> AliasPlan {
    let mut out = AliasPlan {
        refusals,
        ..AliasPlan::default()
    };

    // Self, and identical pairs collapsed (a map may repeat a line).
    let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
    let mut kept: Vec<AliasPair> = Vec::new();
    for p in pairs {
        if p.alias == p.ruling {
            out.refusals.push(AliasRefusal {
                kind: RefusalKind::SelfRedirect,
                alias: Some(p.alias),
                rulings: vec![p.ruling],
                line: p.line,
                source: p.source,
            });
        } else if seen.insert((p.alias, p.ruling)) {
            kept.push(*p);
        }
    }

    // One alias, several rulings: ambiguous, every line refused.
    let mut rulings_of: BTreeMap<uuid::Uuid, BTreeSet<uuid::Uuid>> = BTreeMap::new();
    for p in &kept {
        rulings_of.entry(p.alias.0).or_default().insert(p.ruling.0);
    }
    let ambiguous: HashSet<NodeId> = rulings_of
        .iter()
        .filter(|(_, r)| r.len() > 1)
        .map(|(a, _)| NodeId(*a))
        .collect();
    for p in kept.iter().filter(|p| ambiguous.contains(&p.alias)) {
        out.refusals.push(AliasRefusal {
            kind: RefusalKind::Ambiguous,
            alias: Some(p.alias),
            rulings: rulings_of[&p.alias.0].iter().map(|u| NodeId(*u)).collect(),
            line: p.line,
            source: p.source,
        });
    }
    kept.retain(|p| !ambiguous.contains(&p.alias));

    // Chains and cycles: a node may be an alias or a ruling, not both.
    let next: HashMap<NodeId, NodeId> = kept.iter().map(|p| (p.alias, p.ruling)).collect();
    let rulings: HashSet<NodeId> = kept.iter().map(|p| p.ruling).collect();
    let linked = |p: &AliasPair| next.contains_key(&p.ruling) || rulings.contains(&p.alias);
    let closes = |p: &AliasPair| {
        let mut at = p.ruling;
        for _ in 0..=next.len() {
            match next.get(&at) {
                Some(&n) if n == p.alias => return true,
                Some(&n) => at = n,
                None => return false,
            }
        }
        false
    };
    for p in kept.iter().filter(|p| linked(p)) {
        out.refusals.push(AliasRefusal {
            kind: if closes(p) {
                RefusalKind::Cycle
            } else {
                RefusalKind::Chain
            },
            alias: Some(p.alias),
            rulings: vec![p.ruling],
            line: p.line,
            source: p.source,
        });
    }
    kept.retain(|p| !linked(p));
    kept.sort_by_key(|p| p.alias.0);

    // Citation by citation, against the graph as the plan changes it. The
    // cycle check walks `Causal`, `Dependency` and `Hierarchical` edges: the
    // graph's own invariant refuses a cycle through any of the three (review
    // H1: a ruling's `parent_of` child citing its alias would otherwise be
    // moved into a cycle and abort the whole preview).
    let mut structural: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    let mut dependency: HashSet<(NodeId, NodeId)> = HashSet::new();
    for e in g.edges() {
        if matches!(
            e.edge_type,
            EdgeType::Causal | EdgeType::Dependency | EdgeType::Hierarchical
        ) {
            structural.entry(e.source).or_default().push(e.target);
        }
        if e.edge_type == EdgeType::Dependency {
            dependency.insert((e.source, e.target));
        }
    }
    for p in kept {
        let (Some(alias_type), Some(ruling_type)) =
            (concept_type(g, p.alias), concept_type(g, p.ruling))
        else {
            continue;
        };
        if p.source == PairSource::Reviewed {
            for (bad, kind) in [
                (
                    alias_type != ConceptType::Entity,
                    WarningKind::AliasNotAnEntity,
                ),
                (!is_ruling(ruling_type), WarningKind::RulingNotARuling),
            ] {
                if bad {
                    out.warnings.push(AliasWarning {
                        kind,
                        alias: p.alias,
                        ruling: p.ruling,
                        line: p.line,
                    });
                }
            }
        }
        let alias_status = match g.node(p.alias) {
            Some(Node::Concept(c)) => c.canonization_status,
            _ => CanonizationStatus::None,
        };
        let mut r = AliasRedirect {
            alias_status,
            ruling_fan_in: 0,
            alias: p.alias,
            alias_type,
            ruling: p.ruling,
            ruling_type,
            source: p.source,
            redirected: 0,
            already_cited: 0,
            self_dropped: 0,
            cycle_refused: 0,
            other_dependents_kept: g
                .edges()
                .filter(|e| e.edge_type != EdgeType::Dependency)
                .filter_map(|e| e.dependence())
                .filter(|(pillar, _)| *pillar == p.alias)
                .count(),
        };
        for e in citations_of(g, p.alias) {
            let citer = e.source;
            if citer == p.ruling {
                r.self_dropped += 1;
            } else if dependency.contains(&(citer, p.ruling)) {
                r.already_cited += 1;
            } else if reaches(&structural, p.ruling, citer) {
                r.cycle_refused += 1;
                continue;
            } else {
                r.redirected += 1;
                let mut moved = e.clone();
                moved.id = redirect_edge_id(e.id, p.ruling);
                moved.target = p.ruling;
                out.add.push(moved);
                structural.entry(citer).or_default().push(p.ruling);
                dependency.insert((citer, p.ruling));
            }
            out.retire.push(e.id);
            dependency.remove(&(citer, p.alias));
            if let Some(targets) = structural.get_mut(&citer)
                && let Some(i) = targets.iter().position(|t| *t == p.alias)
            {
                targets.swap_remove(i);
            }
        }
        out.redirects.push(r);
    }
    let mut fan_in: HashMap<NodeId, usize> = HashMap::new();
    for r in &out.redirects {
        *fan_in.entry(r.ruling).or_default() += 1;
    }
    for r in out.redirects.iter_mut() {
        r.ruling_fan_in = fan_in[&r.ruling];
    }
    out
}

/// Whether `to` is reachable from `from` over `Causal`/`Dependency`/
/// `Hierarchical` edges.
fn reaches(adj: &HashMap<NodeId, Vec<NodeId>>, from: NodeId, to: NodeId) -> bool {
    let mut queue = VecDeque::from([from]);
    let mut seen = HashSet::from([from]);
    while let Some(n) = queue.pop_front() {
        if n == to {
            return true;
        }
        for &m in adj.get(&n).into_iter().flatten() {
            if seen.insert(m) {
                queue.push_back(m);
            }
        }
    }
    false
}

/// A deterministic id for the re-pointed edge, so two previews of one copy
/// agree: SHA-256 of the old edge id and the ruling, as a version-8 UUID.
fn redirect_edge_id(old: NodeId, ruling: NodeId) -> NodeId {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("lambo/alias-redirect/{}/{}", old.0, ruling.0).as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    NodeId(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

/// `snap` with `plan`'s edge changes applied: the overlay a preview
/// evaluates. The input is not modified.
pub fn apply(snap: &GraphSnapshot, plan: &AliasPlan) -> GraphSnapshot {
    let retire: HashSet<NodeId> = plan.retire.iter().copied().collect();
    let mut out = snap.clone();
    out.edges.retain(|e| !retire.contains(&e.id));
    out.edges.extend(plan.add.iter().cloned());
    out
}

#[cfg(all(test, feature = "store-memory", feature = "fixtures"))]
#[path = "alias_tests.rs"]
mod tests;
