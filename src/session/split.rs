//! Offline split of one classified source session into new target sessions
//! (#33 D2). [`build_split`] is pure: it reads a validated
//! [`ClassificationPlan`] and the immutable source snapshot and returns
//! complete, deterministic target snapshots plus the accounting the split
//! report carries. It never touches a store; the CLI imports the result.
//!
//! The rules it applies, in the order a reader needs them:
//!
//! * **Placements.** A concept goes to its assigned session, or to every
//!   session in its `copy_into` (a shared hub). An interaction goes to its
//!   assigned session and to every session holding a concept that originated
//!   in it, so each placed concept keeps its `origin_interaction` local.
//! * **Identity.** Every row gets a new id per target, derived from the
//!   split's inputs by SHA-256 ([`target_id`]). Ids are globally unique keys
//!   in the SQL stores, so even a primary placement is remapped.
//! * **Edge ledger.** Every source edge gets exactly one category, computed
//!   from the original primary assignments, never from final placements:
//!   intra, cross-session Temporal (rebuilt), hub reattached, Derives
//!   reattached, dropped (with a named reason). Only Temporal and Derives
//!   edges may touch an interaction (the endpoint matrix
//!   `Graph::from_snapshot` enforces on the source), so no other edge type
//!   can attach to an origin replica.
//! * **Replicas.** An interaction copied outside its own session (an origin
//!   replica) is refused when it carries hard protected provenance (an
//!   excluded project's actor or repository folder): that evidence is never
//!   waivable, so it never leaves its project. A soft replica is a **stub**
//!   (same mapped id, agent, created and event times, no prompt text), except
//!   in `general` when the interaction itself passes D1's general-interaction
//!   test (every origin concept is in general). The full text stays in the
//!   interaction's own target. The rule is one function,
//!   [`replica_interaction`]. The accounting carries a replica matrix (origin
//!   session -> target session), every stub and every general replica.
//! * **Output edges.** A non-Temporal edge that is not dropped is written into
//!   every target holding both endpoints. Temporal links are not copied: each
//!   target's chain is the subsequence of the original `previous_id` chain
//!   formed by its interactions (replicas included, at their original
//!   position). A link between originally adjacent interactions keeps that
//!   edge's metadata; a new adjacency gets a documented new edge.
//! * **Session metadata.** `root_goal` is cleared (a mixed goal would leak
//!   into every target), `created_at`, the mutation epoch and the GC watermark
//!   are inherited together, `closed_at` is cleared because every target is a
//!   new writable session, and the transient GC reset flag is false.
//! * **Write intents.** Pending intents are refused (they are owed a
//!   replay). Consumed ones are receipts only, so they are not migrated:
//!   they are counted, and no payload text crosses into a target.
//! * **Refusals.** Pending write intents, a canonization event or synonym
//!   that resolves to no concept, sub-millisecond timestamps, non-finite
//!   vectors or weights, a hard-provenance replica and any target that fails
//!   [`Graph::from_snapshot`] stop the split before any write.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::plan::digest;
use super::{validate_plan, ClassificationPlan, ClassifyError, ItemKind, ItemRef};
use crate::graph::Graph;
use crate::types::{
    CanonizationEvent, Concept, ConceptType, DemotionReason, Edge, EdgeType, GcMark, GraphSnapshot,
    Interaction, NodeId, SessionId, Synonym,
};

/// Version of the split transform. It is part of every derived id and of the
/// split digest, so a changed rule can never verify against an older import.
/// Version 2: soft origin replicas are stubs (owner decision, 2026-10-13).
pub const SPLIT_ALGORITHM_VERSION: u32 = 2;

/// Weight of a Temporal link the split creates between interactions that were
/// not adjacent in the source chain.
pub const NEW_CHAIN_LINK_WEIGHT: f64 = 1.0;

/// Why a split was refused. Nothing is written on any of these.
#[derive(Debug, thiserror::Error)]
pub enum SplitError {
    #[error("{0}")]
    Plan(#[from] ClassifyError),
    #[error("split refused: {0}")]
    Refused(String),
    /// A refusal whose message lists ids (`message`), with an id-free
    /// version (`counts`, by session) for callers that must not print ids,
    /// such as `lambo session verify` with `--aggregate-only`. Displays
    /// exactly as [`SplitError::Refused`] with `message`.
    #[error("split refused: {message}")]
    Withheld { message: String, counts: String },
    #[error("split report: {0}")]
    Render(#[from] serde_json::Error),
}

fn refuse<T>(msg: impl Into<String>) -> Result<T, SplitError> {
    Err(SplitError::Refused(msg.into()))
}

/// The exclusive category of one source edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeDisposition {
    /// Both ends have the same primary session.
    Intra,
    /// A Temporal edge between sessions: not copied, the chains are rebuilt.
    CrossTemporalRebuilt,
    /// One end is a shared hub with a copy in the other end's session.
    HubReattached,
    /// A cross-session Derives edge whose interaction is replicated into the
    /// concept's session (as the origin of that concept or of another one).
    DerivesReattached,
    /// Not written anywhere; see [`DropReason`].
    Dropped,
}

/// Why a [`EdgeDisposition::Dropped`] edge was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// A concept-to-concept edge between sessions with no hub copy on either
    /// side (design decision: no cross-session edges in v1).
    CrossSessionStructural,
    /// An informational concept-to-concept edge (`Contradicts`,
    /// `SuspectedSupersedes`; [`EdgeType::is_informational`]) between
    /// sessions with no hub copy on either side. It declared no structure,
    /// and a contradiction or a suspected supersession is read within one
    /// session only, so it lapses with the split.
    CrossSessionInformational,
    /// A Derives edge whose interaction is not copied into the concept's
    /// session; the concept keeps its own origin's Derives edge.
    DerivesWithoutReplica,
}

/// One source edge, its category and the rows it produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub edge: NodeId,
    pub edge_type: EdgeType,
    pub category: EdgeDisposition,
    /// Set exactly when `category` is `Dropped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_reason: Option<DropReason>,
    /// Primary session of the edge's source end.
    pub from_session: String,
    /// Primary session of the edge's target end.
    pub to_session: String,
    /// Output rows as `(target session, target edge id)`. For a Temporal
    /// edge, the targets where its adjacency survived and it was kept.
    pub outputs: Vec<(String, NodeId)>,
}

/// Where one source interaction or concept went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub kind: ItemKind,
    pub id: NodeId,
    /// The assigned (primary) session.
    pub primary: String,
    /// Target session -> target id, including the primary.
    pub targets: BTreeMap<String, NodeId>,
}

/// An origin replica written as a stub.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubbedReplica {
    /// The source interaction.
    pub id: NodeId,
    /// Its own (primary) session, which keeps the full text.
    pub primary: String,
    /// The session the stub is in.
    pub target: String,
}

/// One low-confidence item, with the session the plan put it in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LowConfidenceItem {
    pub kind: ItemKind,
    pub id: NodeId,
    pub session: String,
}

/// Row counts of one target.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetCounts {
    pub interactions: usize,
    /// Interactions whose primary session is elsewhere (origin replicas).
    pub origin_replicas: usize,
    pub concepts: usize,
    /// Concepts whose primary session is elsewhere (hub copies).
    pub hub_copies: usize,
    pub edges: usize,
    /// Temporal links: `max(interactions - 1, 0)`.
    pub chain_links: usize,
    /// Chain links that kept an original Temporal edge.
    pub preserved_links: usize,
    /// Chain links the split created.
    pub new_links: usize,
    pub vectors: usize,
    pub canonization_events: usize,
    pub synonyms: usize,
}

/// Source-edge ledger totals and output-edge totals, kept apart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeAccounting {
    pub source_edges: usize,
    pub intra: usize,
    pub cross_temporal_rebuilt: usize,
    pub hub_reattached: usize,
    pub derives_reattached: usize,
    pub dropped: usize,
    /// Dropped edges by [`DropReason`].
    pub dropped_by_reason: BTreeMap<String, usize>,
    /// Dropped edges by edge type.
    pub dropped_by_type: BTreeMap<String, usize>,
    /// Dropped edges by `"from -> to"` primary sessions.
    pub dropped_by_sessions: BTreeMap<String, usize>,
    /// Every dropped source edge id.
    pub dropped_ids: Vec<NodeId>,
    /// Same-session Temporal source edges (inside `intra`).
    pub intra_temporal: usize,
    /// Of those, kept as a link in their own primary target.
    pub intra_temporal_preserved: usize,
    /// Of those, replaced because the target's adjacency changed.
    pub intra_temporal_replaced: usize,
    /// Every output row, all targets.
    pub output_edges: usize,
    /// Output Temporal links, all targets.
    pub output_chain_links: usize,
    pub output_preserved_links: usize,
    pub output_new_links: usize,
    /// Output non-Temporal rows, all targets.
    pub output_structural: usize,
    /// Output non-Temporal rows beyond one per materialized source edge
    /// (hub edges written into several shared placements).
    pub output_structural_extra: usize,
}

/// Every count the split report carries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitAccounting {
    pub source_interactions: usize,
    pub source_concepts: usize,
    pub source_synonyms: usize,
    pub source_canonization_events: usize,
    /// Consumed (settled) write intents: receipts only, not migrated.
    pub consumed_write_intents_dropped: usize,
    /// Volatile soft locks, never migrated.
    pub excluded_reservations: usize,
    pub target_interactions: usize,
    /// `target_interactions - source_interactions`: origin replicas.
    pub interaction_replicas: usize,
    /// Replicas an assigned (primary) concept needs: its origin interaction
    /// in its own session.
    pub replicas_for_primary_concepts: usize,
    /// Replicas only hub copies need: a copy keeps its true origin locally
    /// so its `origin_interaction` and Derives edge stay in its session.
    pub replicas_for_hub_copies_only: usize,
    /// Origin replicas by `"origin session -> target session"`.
    pub replica_matrix: BTreeMap<String, usize>,
    /// Every replica written as a stub (no prompt text).
    pub stubbed_replicas: Vec<StubbedReplica>,
    /// Origin replicas placed in the general session.
    pub general_replicas: usize,
    /// Every one of them, by source id.
    pub general_replica_ids: Vec<NodeId>,
    /// Of those, replicas that keep their text because the interaction
    /// passes D1's general-interaction test.
    pub general_full_text_replicas: Vec<NodeId>,
    /// Of those, interactions assigned to a project excluded from general:
    /// provenance of audited general rulings, listed for owner review.
    pub general_replicas_from_excluded_projects: Vec<NodeId>,
    pub target_concepts: usize,
    /// Concepts with an explicit `copy_into`.
    pub hubs: usize,
    /// Sum of hub placements.
    pub hub_placements: usize,
    /// `target_concepts - source_concepts`.
    pub hub_extra_rows: usize,
    /// Number of placements -> number of hubs.
    pub hubs_by_placements: BTreeMap<usize, usize>,
    pub edges: EdgeAccounting,
    pub target_vectors: usize,
    pub target_canonization_events: usize,
    pub target_synonyms: usize,
    pub per_target: BTreeMap<String, TargetCounts>,
}

/// One complete target session, ready to import.
#[derive(Clone, Debug, PartialEq)]
pub struct SplitTarget {
    pub session: String,
    /// Interactions in chain order (head first), the insertion order.
    pub snapshot: GraphSnapshot,
    /// Digest of [`canonical_target`] of `snapshot`.
    pub sha256: String,
}

/// Everything [`build_split`] produced.
#[derive(Clone, Debug, PartialEq)]
pub struct SplitOutput {
    pub algorithm_version: u32,
    pub source_session: String,
    pub source_sha256: String,
    pub plan_sha256: String,
    /// Binds the version, source, plan and every target digest.
    pub split_sha256: String,
    pub targets: Vec<SplitTarget>,
    pub ledger: Vec<LedgerEntry>,
    pub placements: Vec<Placement>,
    pub low_confidence: Vec<LowConfidenceItem>,
    pub accounting: SplitAccounting,
    /// Whether the source carried a `root_goal` the split cleared.
    pub root_goal_cleared: bool,
}

/// A target snapshot with every collection in a fixed order, the form both
/// the digest and the post-import comparison use. Stores load rows in their
/// own order; this makes the comparison independent of it.
pub fn canonical_target(mut s: GraphSnapshot) -> GraphSnapshot {
    s.interactions.sort_by_key(|i| i.id.0);
    s.concepts.sort_by_key(|c| c.id.0);
    s.edges.sort_by_key(|e| e.id.0);
    s.synonyms
        .sort_by(|a, b| (&a.source_key, &a.canonical_key).cmp(&(&b.source_key, &b.canonical_key)));
    s.reservations.sort_by_key(|r| r.node_id.0);
    s.canonization_events.sort_by_key(|e| e.id.0);
    s.write_intents.sort_by(|a, b| a.receipt.cmp(&b.receipt));
    s
}

/// Digest of a target snapshot, in its canonical order.
pub fn target_sha256(snapshot: &GraphSnapshot) -> Result<String, SplitError> {
    Ok(digest(&canonical_target(snapshot.clone()))?)
}

/// Inputs every derived id is bound to.
struct Ids<'a> {
    source_sha: &'a str,
    plan_sha: &'a str,
}

impl Ids<'_> {
    /// The target id of `original` in `target`: the first 16 bytes of a
    /// domain-separated SHA-256 over the algorithm version, the source and
    /// plan digests, the row kind, the target session, the original id and
    /// `extra` (the older interaction of a new chain link), stamped as a
    /// version 8 (custom) UUID. Each variable-length part is length-prefixed.
    fn target_id(&self, kind: &str, original: Uuid, target: &str, extra: Option<Uuid>) -> NodeId {
        let mut h = Sha256::new();
        h.update(b"lambo-session-split\0");
        h.update(SPLIT_ALGORITHM_VERSION.to_be_bytes());
        for part in [self.source_sha, self.plan_sha, kind, target] {
            h.update((part.len() as u64).to_be_bytes());
            h.update(part.as_bytes());
        }
        h.update(original.as_bytes());
        if let Some(extra) = extra {
            h.update(extra.as_bytes());
        }
        let bytes: [u8; 16] = h.finalize()[..16]
            .try_into()
            .expect("a SHA-256 digest has 32 bytes");
        NodeId(uuid::Builder::from_custom_bytes(bytes).into_uuid())
    }
}

fn ms_aligned(t: &DateTime<Utc>) -> bool {
    t.timestamp_subsec_nanos().is_multiple_of(1_000_000)
}

/// SQLite persists millisecond UTC text, so a finer instant would not
/// round-trip and the import could never verify. Refuse instead of truncating.
fn check_round_trip(s: &GraphSnapshot) -> Result<(), SplitError> {
    let mut times: Vec<DateTime<Utc>> = Vec::new();
    times.extend(s.created_at);
    times.extend(s.gc_mark.last_gc_at);
    for i in &s.interactions {
        times.push(i.created_at);
        times.extend(i.event_time);
    }
    for c in &s.concepts {
        times.push(c.created_at);
        times.extend(c.last_accessed);
        times.extend(c.last_demotion_time);
    }
    for e in &s.edges {
        times.extend([e.created_at, e.last_reinforced]);
        times.extend(e.event_time);
    }
    for ev in &s.canonization_events {
        times.push(ev.occurred_at);
        times.extend(ev.last_demotion_time);
    }
    if times.iter().any(|t| !ms_aligned(t)) {
        return refuse(
            "the source carries sub-millisecond timestamps, which a SQLite target cannot \
             store exactly; export the source through a SQLite copy first",
        );
    }
    let too_big = |v: u64| i64::try_from(v).is_err();
    if too_big(s.mutation_epoch) || too_big(s.gc_mark.last_gc_epoch) {
        return refuse("a counter exceeds the SQLite integer range");
    }
    if s.concepts.iter().any(|c| {
        c.embedding
            .as_ref()
            .is_some_and(|v| v.iter().any(|x| !x.is_finite()))
    }) || s.edges.iter().any(|e| !e.weight.is_finite())
    {
        return refuse("the source carries a non-finite vector component or edge weight");
    }
    Ok(())
}

/// The source chain, head first. The source graph was validated, so the
/// chain is complete and acyclic.
fn chain_order(s: &GraphSnapshot) -> Vec<Uuid> {
    let next: HashMap<Uuid, Uuid> = s
        .interactions
        .iter()
        .filter_map(|i| i.previous_id.map(|p| (p.0, i.id.0)))
        .collect();
    let mut out = Vec::with_capacity(s.interactions.len());
    let mut cur = s
        .interactions
        .iter()
        .find(|i| i.previous_id.is_none())
        .map(|i| i.id.0);
    while let Some(id) = cur {
        out.push(id);
        cur = next.get(&id).copied();
    }
    out
}

/// Resolve a synonym's alias chain to the concept it names.
fn resolve_synonym<'a>(
    start: &'a str,
    aliases: &BTreeMap<&'a str, &'a str>,
    by_key: &HashMap<&'a str, Vec<&'a Concept>>,
) -> Result<Uuid, SplitError> {
    let mut seen = BTreeSet::new();
    let mut key = start;
    while let Some(next) = aliases.get(key) {
        if !seen.insert(key) {
            return refuse("a synonym alias chain is cyclic");
        }
        key = *next;
    }
    let candidates = by_key.get(key).map(Vec::as_slice).unwrap_or_default();
    let rulings: Vec<_> = candidates
        .iter()
        .filter(|c| c.concept_type != ConceptType::Observation)
        .collect();
    match (rulings.as_slice(), candidates) {
        ([only], _) => Ok(only.id.0),
        ([], [only]) => Ok(only.id.0),
        ([], []) => refuse("a synonym resolves to no concept; it cannot be placed"),
        _ => refuse("a synonym resolves to more than one concept; it cannot be placed"),
    }
}

/// What an origin replica (an interaction copied into a session other than
/// its own) carries: the soft-replica policy, option A chosen by the owner on
/// 2026-10-13. A replica is a stub: the same agent, created and event times,
/// with no prompt text, a state chunk-ingest interactions already have. The
/// full text stays in the interaction's own target, so nothing is lost, and
/// no project's prompt is copied into another project's session. The one
/// exception is `general`: an interaction that would itself pass D1's
/// general-interaction test (it has origin concepts and all are in general)
/// keeps its text there. Hard provenance never reaches here; it is refused
/// before materialization. Returns the row and whether it is a stub.
fn replica_interaction(
    original: &Interaction,
    target: &str,
    general: &str,
    passes_general_test: bool,
) -> (Interaction, bool) {
    if target == general && passes_general_test {
        (original.clone(), false)
    } else {
        (
            Interaction {
                prompt_text: None,
                ..original.clone()
            },
            true,
        )
    }
}

/// Build every target of the split. Pure and deterministic: the same plan and
/// source always give byte-identical targets, ids and accounting.
pub fn build_split(
    plan: &ClassificationPlan,
    source: &GraphSnapshot,
) -> Result<SplitOutput, SplitError> {
    validate_plan(plan, source)?;
    build_validated(plan, source)
}

/// [`build_split`] after [`validate_plan`]. Separate so tests can reach the
/// guards that a valid plan never triggers; it assumes complete assignments.
pub(super) fn build_validated(
    plan: &ClassificationPlan,
    source: &GraphSnapshot,
) -> Result<SplitOutput, SplitError> {
    let pending: Vec<&str> = source
        .write_intents
        .iter()
        .filter(|w| w.outcome.is_none())
        .map(|w| w.receipt.as_str())
        .collect();
    if !pending.is_empty() {
        let shown = pending
            .iter()
            .take(20)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        return Err(SplitError::Withheld {
            message: format!(
                "{} pending write intent(s) are owed a replay ({shown}); serve the source once \
                 so they settle, take a new backup and reclassify",
                pending.len()
            ),
            counts: format!(
                "{} pending write intent(s) in session {} are owed a replay; serve the source \
                 once so they settle, take a new backup and reclassify",
                pending.len(),
                source.session_id
            ),
        });
    }
    if source.created_at.is_none() {
        return refuse("the source session has no created_at to inherit");
    }
    check_round_trip(source)?;
    Graph::from_snapshot(source.clone())
        .map_err(|e| SplitError::Refused(format!("the source graph is invalid: {e}")))?;

    let ids = Ids {
        source_sha: &plan.source.sha256,
        plan_sha: &plan.plan_sha256,
    };
    let general = plan.rules.general_session.as_str();
    let interactions: HashMap<Uuid, &Interaction> =
        source.interactions.iter().map(|i| (i.id.0, i)).collect();
    let concepts: HashMap<Uuid, &Concept> = source.concepts.iter().map(|c| (c.id.0, c)).collect();

    // Primary sessions and placements.
    let mut primary: HashMap<Uuid, &str> = HashMap::new();
    let mut places: HashMap<Uuid, BTreeSet<&str>> = HashMap::new();
    let mut hubs: HashSet<Uuid> = HashSet::new();
    for a in &plan.assignments {
        primary.insert(a.id.0, a.session.as_str());
        let set: BTreeSet<&str> = if a.copy_into.is_empty() {
            BTreeSet::from([a.session.as_str()])
        } else {
            let set: BTreeSet<&str> = a.copy_into.iter().map(String::as_str).collect();
            if a.kind != ItemKind::Concept
                || set.len() != a.copy_into.len()
                || !set.contains(a.session.as_str())
                || set.contains(general)
            {
                return refuse(
                    "a hub placement is duplicated, omits its primary, names general or is \
                     not a concept",
                );
            }
            hubs.insert(a.id.0);
            set
        };
        places.insert(a.id.0, set);
    }
    for c in &source.concepts {
        let concept_places = places[&c.id.0].clone();
        places
            .get_mut(&c.origin_interaction.0)
            .expect("validate_plan covers every interaction")
            .extend(concept_places);
    }
    let targets: BTreeSet<&str> = places.values().flatten().copied().collect();
    let placed = |node: Uuid, t: &str| places.get(&node).is_some_and(|p| p.contains(t));

    // Hard protected provenance (an excluded project's actor or repository
    // folder) is never waivable, so such an interaction never leaves its own
    // session, not even as an origin replica. D1 already refuses it in
    // general; this re-derives the barrier from the source for every target.
    let (protected, _) = super::classify::protected_source_items(source, &plan.rules);
    let mut hard_replicas: Vec<String> = Vec::new();
    let mut hard_pairs: Vec<String> = Vec::new();
    for i in &source.interactions {
        if !protected.contains(&i.id.0) {
            continue;
        }
        for t in &places[&i.id.0] {
            if *t != primary[&i.id.0] {
                hard_replicas.push(format!("{} ({} -> {t})", i.id, primary[&i.id.0]));
                hard_pairs.push(format!("{} -> {t}", primary[&i.id.0]));
            }
        }
    }
    if !hard_replicas.is_empty() {
        hard_replicas.sort();
        let mut pairs: BTreeMap<String, usize> = BTreeMap::new();
        for pair in &hard_pairs {
            *pairs.entry(pair.clone()).or_default() += 1;
        }
        let by_pair = pairs
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(SplitError::Withheld {
            message: format!(
                "{} origin replica(s) would copy an interaction with hard protected project \
                 provenance out of its session: {}; place the concepts that originated there \
                 in that session, or drop their hub copies",
                hard_replicas.len(),
                hard_replicas
                    .iter()
                    .take(20)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            counts: format!(
                "{} origin replica(s) would copy an interaction with hard protected project \
                 provenance out of its session ({by_pair}); place the concepts that originated \
                 there in that session, or drop their hub copies",
                hard_replicas.len()
            ),
        });
    }

    // The exclusive source-edge ledger, from primary assignments only.
    let mut ledger: Vec<LedgerEntry> = Vec::with_capacity(source.edges.len());
    let mut sorted_edges: Vec<&Edge> = source.edges.iter().collect();
    sorted_edges.sort_by_key(|e| e.id.0);
    for e in &sorted_edges {
        let (s, t) = (e.source.0, e.target.0);
        let (ps, pt) = (primary[&s], primary[&t]);
        let (category, drop_reason) = if ps == pt {
            (EdgeDisposition::Intra, None)
        } else if e.edge_type == EdgeType::Temporal {
            (EdgeDisposition::CrossTemporalRebuilt, None)
        } else if (hubs.contains(&s) && placed(s, pt)) || (hubs.contains(&t) && placed(t, ps)) {
            (EdgeDisposition::HubReattached, None)
        } else if e.edge_type == EdgeType::Derives {
            // The source graph was validated: a Derives edge runs from an
            // interaction to a concept, and no other type touches one.
            if placed(s, pt) {
                (EdgeDisposition::DerivesReattached, None)
            } else {
                (
                    EdgeDisposition::Dropped,
                    Some(DropReason::DerivesWithoutReplica),
                )
            }
        } else if e.edge_type.is_informational() {
            (
                EdgeDisposition::Dropped,
                Some(DropReason::CrossSessionInformational),
            )
        } else {
            (
                EdgeDisposition::Dropped,
                Some(DropReason::CrossSessionStructural),
            )
        };
        ledger.push(LedgerEntry {
            edge: e.id,
            edge_type: e.edge_type,
            category,
            drop_reason,
            from_session: ps.to_owned(),
            to_session: pt.to_owned(),
            outputs: Vec::new(),
        });
    }
    let ledger_pos: HashMap<Uuid, usize> = ledger
        .iter()
        .enumerate()
        .map(|(n, l)| (l.edge.0, n))
        .collect();

    // Lookups the materialization needs.
    let chain = chain_order(source);
    let temporal_link: HashMap<(Uuid, Uuid), &Edge> = source
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Temporal)
        .map(|e| ((e.source.0, e.target.0), e))
        .collect();
    let mut by_key: HashMap<&str, Vec<&Concept>> = HashMap::new();
    for c in &source.concepts {
        by_key.entry(c.canonical_key.as_str()).or_default().push(c);
    }
    let aliases: BTreeMap<&str, &str> = source
        .synonyms
        .iter()
        .map(|s| (s.source_key.as_str(), s.canonical_key.as_str()))
        .collect();
    if aliases.len() != source.synonyms.len() {
        return refuse("two synonyms share a source key");
    }
    // Fixed orders, so the output never depends on the source's row order.
    let mut synonyms: Vec<&Synonym> = source.synonyms.iter().collect();
    synonyms.sort_by(|a, b| a.source_key.cmp(&b.source_key));
    let mut events: Vec<&CanonizationEvent> = source.canonization_events.iter().collect();
    events.sort_by_key(|e| (e.occurred_at, e.id.0));
    let mut synonym_concept: Vec<(&Synonym, Uuid)> = Vec::with_capacity(synonyms.len());
    for s in synonyms {
        synonym_concept.push((s, resolve_synonym(&s.source_key, &aliases, &by_key)?));
    }
    for ev in &source.canonization_events {
        if !concepts.contains_key(&ev.node_id.0) {
            return refuse("a canonization event names no source concept");
        }
    }

    // D1's general-interaction test: origin concepts exist and all are
    // assigned to general.
    let mut origin_children: HashMap<Uuid, Vec<&str>> = HashMap::new();
    for c in &source.concepts {
        origin_children
            .entry(c.origin_interaction.0)
            .or_default()
            .push(primary[&c.id.0]);
    }
    let all_general_origins: HashSet<Uuid> = origin_children
        .iter()
        .filter(|(_, sessions)| sessions.iter().all(|s| *s == general))
        .map(|(i, _)| *i)
        .collect();

    let mut out_targets = Vec::with_capacity(targets.len());
    let mut accounting = SplitAccounting::default();
    for &t in &targets {
        let sid = SessionId::from(t);
        let map = |kind: &str, original: Uuid| ids.target_id(kind, original, t, None);
        let node_map = |original: Uuid| {
            let kind = if interactions.contains_key(&original) {
                "interaction"
            } else {
                "concept"
            };
            map(kind, original)
        };
        let mut counts = TargetCounts::default();

        // Interactions: the original chain's subsequence.
        let local_chain: Vec<Uuid> = chain.iter().copied().filter(|i| placed(*i, t)).collect();
        let mut snap_interactions = Vec::with_capacity(local_chain.len());
        let mut snap_edges: Vec<Edge> = Vec::new();
        for (n, &orig) in local_chain.iter().enumerate() {
            let src = interactions[&orig];
            let prev = n.checked_sub(1).map(|p| local_chain[p]);
            let row = if primary[&orig] == t {
                Interaction::clone(src)
            } else {
                counts.origin_replicas += 1;
                *accounting
                    .replica_matrix
                    .entry(format!("{} -> {t}", primary[&orig]))
                    .or_default() += 1;
                let (row, stub) =
                    replica_interaction(src, t, general, all_general_origins.contains(&orig));
                if stub {
                    accounting.stubbed_replicas.push(StubbedReplica {
                        id: src.id,
                        primary: primary[&orig].to_owned(),
                        target: t.to_owned(),
                    });
                } else {
                    accounting.general_full_text_replicas.push(src.id);
                }
                row
            };
            snap_interactions.push(Interaction {
                id: map("interaction", orig),
                session_id: sid.clone(),
                previous_id: prev.map(|p| map("interaction", p)),
                ..row
            });
            let Some(prev) = prev else { continue };
            counts.chain_links += 1;
            if let Some(kept) = temporal_link.get(&(orig, prev)) {
                let id = map("edge", kept.id.0);
                snap_edges.push(Edge {
                    id,
                    session_id: sid.clone(),
                    source: map("interaction", orig),
                    target: map("interaction", prev),
                    ..(*kept).clone()
                });
                ledger[ledger_pos[&kept.id.0]]
                    .outputs
                    .push((t.to_owned(), id));
                counts.preserved_links += 1;
            } else {
                snap_edges.push(Edge {
                    id: ids.target_id("chain", orig, t, Some(prev)),
                    session_id: sid.clone(),
                    source: map("interaction", orig),
                    target: map("interaction", prev),
                    edge_type: EdgeType::Temporal,
                    weight: NEW_CHAIN_LINK_WEIGHT,
                    reinforcements: 0,
                    created_at: src.created_at,
                    last_reinforced: src.created_at,
                    event_time: src.event_time,
                });
                counts.new_links += 1;
            }
        }

        // Concepts, verbatim but for id, session and origin.
        let mut snap_concepts = Vec::new();
        let mut sorted_concepts: Vec<&Concept> = source.concepts.iter().collect();
        sorted_concepts.sort_by_key(|c| c.id.0);
        for c in sorted_concepts.into_iter().filter(|c| placed(c.id.0, t)) {
            if primary[&c.id.0] != t {
                counts.hub_copies += 1;
            }
            counts.vectors += usize::from(c.embedding.is_some());
            snap_concepts.push(Concept {
                id: map("concept", c.id.0),
                session_id: sid.clone(),
                origin_interaction: map("interaction", c.origin_interaction.0),
                ..(*c).clone()
            });
        }

        // Structural edges into every target holding both endpoints.
        for e in sorted_edges
            .iter()
            .filter(|e| e.edge_type != EdgeType::Temporal)
        {
            let entry = &mut ledger[ledger_pos[&e.id.0]];
            if entry.category == EdgeDisposition::Dropped
                || !placed(e.source.0, t)
                || !placed(e.target.0, t)
            {
                continue;
            }
            let id = map("edge", e.id.0);
            snap_edges.push(Edge {
                id,
                session_id: sid.clone(),
                source: node_map(e.source.0),
                target: node_map(e.target.0),
                ..(*e).clone()
            });
            entry.outputs.push((t.to_owned(), id));
        }

        // Canonization audit follows every placement of its concept.
        let mut snap_events = Vec::new();
        for ev in events.iter().filter(|ev| placed(ev.node_id.0, t)) {
            // #20: a supersession's source is a concept id too, remapped the
            // same way, so it names the target's copy when it is placed here
            // (and, like a collected source, names no row when it is not),
            // never the source session's id.
            let reason = match &ev.reason {
                Some(DemotionReason::Superseded { by }) => Some(DemotionReason::Superseded {
                    by: map("concept", by.0),
                }),
                Some(DemotionReason::Unknown {
                    reason,
                    superseded_by,
                }) => Some(DemotionReason::Unknown {
                    reason: reason.clone(),
                    superseded_by: superseded_by.map(|by| map("concept", by.0)),
                }),
                other => other.clone(),
            };
            snap_events.push(CanonizationEvent {
                id: map("canonization_event", ev.id.0),
                session_id: sid.clone(),
                node_id: map("concept", ev.node_id.0),
                reason,
                ..(*ev).clone()
            });
        }

        // Synonyms follow the concept they resolve to.
        let snap_synonyms: Vec<Synonym> = synonym_concept
            .iter()
            .filter(|(_, c)| placed(*c, t))
            .map(|(s, _)| Synonym {
                session_id: sid.clone(),
                ..(*s).clone()
            })
            .collect();

        let snapshot = GraphSnapshot {
            session_id: sid.clone(),
            root_goal: None,
            created_at: source.created_at,
            closed_at: None,
            interactions: snap_interactions,
            concepts: snap_concepts,
            edges: snap_edges,
            synonyms: snap_synonyms,
            reservations: Vec::new(),
            canonization_events: snap_events,
            embedding: source.embedding.clone(),
            // Consumed intents are receipts, not memory: they are counted
            // and dropped, so no payload text crosses sessions.
            write_intents: Vec::new(),
            mutation_epoch: source.mutation_epoch,
            gc_mark: GcMark {
                last_gc_at_reset: false,
                ..source.gc_mark
            },
        };
        Graph::from_snapshot(snapshot.clone()).map_err(|e| {
            SplitError::Refused(format!("target {t} would be an invalid graph: {e}"))
        })?;
        counts.interactions = snapshot.interactions.len();
        counts.concepts = snapshot.concepts.len();
        counts.edges = snapshot.edges.len();
        counts.canonization_events = snapshot.canonization_events.len();
        counts.synonyms = snapshot.synonyms.len();
        accounting.per_target.insert(t.to_owned(), counts);
        out_targets.push(SplitTarget {
            session: t.to_owned(),
            sha256: target_sha256(&snapshot)?,
            snapshot,
        });
    }

    // Every output id must be globally unique: SQL keys are not per session.
    let mut seen = HashSet::new();
    for target in &out_targets {
        let s = &target.snapshot;
        let all = s
            .interactions
            .iter()
            .map(|i| i.id.0)
            .chain(s.concepts.iter().map(|c| c.id.0))
            .chain(s.edges.iter().map(|e| e.id.0))
            .chain(s.canonization_events.iter().map(|e| e.id.0));
        for id in all {
            if !seen.insert(id) {
                return refuse("two output rows derived the same id");
            }
        }
    }

    // Placements (the replica maps).
    let mut placements: Vec<Placement> = plan
        .assignments
        .iter()
        .map(|a| Placement {
            kind: a.kind,
            id: a.id,
            primary: a.session.clone(),
            targets: places[&a.id.0]
                .iter()
                .map(|t| {
                    let kind = match a.kind {
                        ItemKind::Interaction => "interaction",
                        ItemKind::Concept => "concept",
                    };
                    ((*t).to_owned(), ids.target_id(kind, a.id.0, t, None))
                })
                .collect(),
        })
        .collect();
    placements.sort_by_key(|p| ItemRef {
        kind: p.kind,
        id: p.id,
    });

    // Accounting.
    let a = &mut accounting;
    a.source_interactions = source.interactions.len();
    a.source_concepts = source.concepts.len();
    a.source_synonyms = source.synonyms.len();
    a.source_canonization_events = source.canonization_events.len();
    a.consumed_write_intents_dropped = source.write_intents.len();
    a.excluded_reservations = source.reservations.len();
    for c in a.per_target.values() {
        a.target_interactions += c.interactions;
        a.target_concepts += c.concepts;
        a.target_vectors += c.vectors;
        a.target_canonization_events += c.canonization_events;
        a.target_synonyms += c.synonyms;
        a.edges.output_edges += c.edges;
        a.edges.output_chain_links += c.chain_links;
        a.edges.output_preserved_links += c.preserved_links;
        a.edges.output_new_links += c.new_links;
    }
    a.interaction_replicas = a.target_interactions - a.source_interactions;
    let for_primaries: HashSet<(Uuid, &str)> = source
        .concepts
        .iter()
        .filter(|c| primary[&c.id.0] != primary[&c.origin_interaction.0])
        .map(|c| (c.origin_interaction.0, primary[&c.id.0]))
        .collect();
    a.replicas_for_primary_concepts = for_primaries.len();
    a.replicas_for_hub_copies_only = a.interaction_replicas - for_primaries.len();
    // Soft replicas into the shared layer of an interaction assigned to a
    // project excluded from general are surfaced for owner review (hard
    // provenance was refused above).
    let excluded: BTreeSet<&str> = plan
        .rules
        .projects
        .iter()
        .filter(|p| p.exclude_from_general)
        .map(|p| p.session.as_str())
        .collect();
    for i in &source.interactions {
        if primary[&i.id.0] == general || !placed(i.id.0, general) {
            continue;
        }
        a.general_replicas += 1;
        a.general_replica_ids.push(i.id);
        if excluded.contains(primary[&i.id.0]) {
            a.general_replicas_from_excluded_projects.push(i.id);
        }
    }
    a.general_replicas_from_excluded_projects
        .sort_by_key(|id| id.0);
    a.general_replica_ids.sort_by_key(|id| id.0);
    a.general_full_text_replicas.sort_by_key(|id| id.0);
    a.stubbed_replicas
        .sort_by(|x, y| (x.id.0, &x.target).cmp(&(y.id.0, &y.target)));
    a.hub_extra_rows = a.target_concepts - a.source_concepts;
    for h in &hubs {
        let n = places[h].len();
        a.hubs += 1;
        a.hub_placements += n;
        *a.hubs_by_placements.entry(n).or_default() += 1;
    }
    let e = &mut a.edges;
    e.source_edges = ledger.len();
    e.output_structural = e.output_edges - e.output_chain_links;
    for l in &ledger {
        match l.category {
            EdgeDisposition::Intra => e.intra += 1,
            EdgeDisposition::CrossTemporalRebuilt => e.cross_temporal_rebuilt += 1,
            EdgeDisposition::HubReattached => e.hub_reattached += 1,
            EdgeDisposition::DerivesReattached => e.derives_reattached += 1,
            EdgeDisposition::Dropped => {
                e.dropped += 1;
                *e.dropped_by_reason
                    .entry(
                        serde_json::to_value(l.drop_reason)?
                            .as_str()
                            .unwrap_or("unknown")
                            .to_owned(),
                    )
                    .or_default() += 1;
                *e.dropped_by_type
                    .entry(format!("{:?}", l.edge_type))
                    .or_default() += 1;
                *e.dropped_by_sessions
                    .entry(format!("{} -> {}", l.from_session, l.to_session))
                    .or_default() += 1;
                e.dropped_ids.push(l.edge);
            }
        }
        if l.edge_type == EdgeType::Temporal {
            if l.category == EdgeDisposition::Intra {
                e.intra_temporal += 1;
                if l.outputs.iter().any(|(t, _)| t == &l.from_session) {
                    e.intra_temporal_preserved += 1;
                } else {
                    e.intra_temporal_replaced += 1;
                }
            }
        } else if l.category != EdgeDisposition::Dropped {
            if l.outputs.is_empty() {
                return refuse("a kept source edge produced no output row");
            }
            e.output_structural_extra += l.outputs.len() - 1;
        }
    }
    if e.intra + e.cross_temporal_rebuilt + e.hub_reattached + e.derives_reattached + e.dropped
        != e.source_edges
    {
        return refuse("the edge ledger does not account for every source edge");
    }

    let low_confidence = plan
        .low_confidence
        .iter()
        .map(|r| LowConfidenceItem {
            kind: r.kind,
            id: r.id,
            session: primary[&r.id.0].to_owned(),
        })
        .collect();
    #[derive(Serialize)]
    struct SplitIdentity<'a> {
        algorithm_version: u32,
        source_sha256: &'a str,
        plan_sha256: &'a str,
        targets: Vec<(&'a str, &'a str)>,
    }
    let split_sha256 = digest(&SplitIdentity {
        algorithm_version: SPLIT_ALGORITHM_VERSION,
        source_sha256: &plan.source.sha256,
        plan_sha256: &plan.plan_sha256,
        targets: out_targets
            .iter()
            .map(|t| (t.session.as_str(), t.sha256.as_str()))
            .collect(),
    })?;
    Ok(SplitOutput {
        algorithm_version: SPLIT_ALGORITHM_VERSION,
        source_session: source.session_id.0.clone(),
        source_sha256: plan.source.sha256.clone(),
        plan_sha256: plan.plan_sha256.clone(),
        split_sha256,
        targets: out_targets,
        ledger,
        placements,
        low_confidence,
        accounting,
        root_goal_cleared: source.root_goal.is_some(),
    })
}
