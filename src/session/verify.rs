//! Before/after recall comparison of a session split (#33 D3).
//!
//! [`verify_split`] replays a fixed query set per project twice:
//!
//! * **before**: plain recall on the mixed source session, as a holder of
//!   that session answers it today;
//! * **after**: layered recall as an agent for that project sees it once the
//!   split is served: the project's target session plus the layer stack
//!   `[serve] layers` gives it (`ServeConfig::resolve_layers` with every
//!   target pinned), merged by the same `recall::layered::merge` with the
//!   same knobs `Memory::recall_layered` passes.
//!
//! Hits are compared by **origin identity**: an "after" hit's id is mapped
//! back to its source id through the split's placements (every target id,
//! hub copies and origin replicas included), so a memory that moved, was
//! copied into a hub placement or answers from `general` counts as the same
//! memory it was before.
//!
//! The replay is offline and read-only. Each session is materialized as a
//! holder loads it (`store::load::materialize`) into an **unspawned**
//! daemon, primed once at a pinned clock (`Daemon::prime_offline`: rescore
//! and hot-list detection, no events, no GC, no access ledger). The vector
//! leg ranks in the session's own graph, as a SQLite holder's does
//! (`VectorCandidates::for_holder`). Nothing notes accesses, so one query
//! never changes the next one's answer, and the same inputs always give the
//! same report.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use super::SplitOutput;
use crate::config::{Config, LayerSpec, ServeLayers};
use crate::daemon::{Daemon, RecallPipeline};
use crate::embed::Embedder;
use crate::graph::vector_source::GraphVectorSource;
use crate::graph::Graph;
use crate::recall::cache::RecallCache;
use crate::recall::candidates;
use crate::recall::format;
use crate::recall::layered::{
    self, HitIdentity, LayeredRecall, MergeParams, TierList, TierRecall, PRIMARY_WEIGHT,
};
use crate::resolve::assert_session_embedding_compatible;
use crate::store::vector_source::VectorCandidates;
use crate::types::{EmbeddingContract, GraphSnapshot, Node, NodeId, RecallQuery, SessionId};

/// Version of the verify report's JSON shape.
pub const VERIFY_REPORT_VERSION: u32 = 1;

/// Most queries one project may list.
pub const MAX_QUERIES_PER_PROJECT: usize = 200;

/// Longest query, in bytes.
pub const MAX_QUERY_BYTES: usize = 4096;

/// Why a verification was refused. Nothing is reported on any of these.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("verify refused: {0}")]
    Refused(String),
    #[error("verify: {0}")]
    Split(#[from] super::SplitError),
}

fn refuse<T>(msg: impl Into<String>) -> Result<T, VerifyError> {
    Err(VerifyError::Refused(msg.into()))
}

/// The query set: per project (a target session of the split), the
/// queries an agent for that project would ask.
///
/// ```toml
/// [queries]
/// alpha = ["how is the store schema checked", "release checklist"]
/// beta = ["deploy steps"]
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyQueries {
    pub queries: BTreeMap<String, Vec<String>>,
}

impl VerifyQueries {
    /// Shape checks that need no split: at least one project, every project
    /// with 1 to [`MAX_QUERIES_PER_PROJECT`] distinct non-blank queries of at
    /// most [`MAX_QUERY_BYTES`].
    pub fn validate(&self) -> Result<(), VerifyError> {
        if self.queries.is_empty() {
            return refuse("the query set names no project");
        }
        for (project, queries) in &self.queries {
            if project.trim().is_empty() {
                return refuse("the query set has a blank project name");
            }
            if queries.is_empty() || queries.len() > MAX_QUERIES_PER_PROJECT {
                return refuse(format!(
                    "project {project} must list 1 to {MAX_QUERIES_PER_PROJECT} queries"
                ));
            }
            let mut seen = BTreeSet::new();
            for q in queries {
                if q.trim().is_empty() || q.len() > MAX_QUERY_BYTES {
                    return refuse(format!(
                        "project {project} has a blank query or one over {MAX_QUERY_BYTES} bytes"
                    ));
                }
                if !seen.insert(q.as_str()) {
                    return refuse(format!("project {project} lists a query twice"));
                }
            }
        }
        Ok(())
    }
}

/// The recall knobs both sides run with, and the pinned clock.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyParams {
    /// `k`: the hits each recall returns (forced hits may add more), and
    /// the `k` of recall@k.
    pub top_k: usize,
    pub max_tokens: usize,
    pub traversal_depth: usize,
    /// The clock every session's detectors run at (stale, conflict and
    /// high-risk windows). Pinned so a rerun gives the same report.
    pub at: DateTime<Utc>,
}

/// The newest timestamp a source carries: the default clock of a replay,
/// so detectors see the store as it was when the backup was taken.
pub fn newest_row(snapshot: &GraphSnapshot) -> Option<DateTime<Utc>> {
    let mut times: Vec<DateTime<Utc>> = Vec::new();
    times.extend(snapshot.interactions.iter().map(|i| i.created_at));
    for c in &snapshot.concepts {
        times.push(c.created_at);
        times.extend(c.last_accessed);
    }
    for e in &snapshot.edges {
        times.extend([e.created_at, e.last_reinforced]);
    }
    times.into_iter().max()
}

/// One hit of one side.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyHit {
    /// 1-based position in the answer.
    pub rank: usize,
    /// The source (origin) id.
    pub origin: NodeId,
    /// The session the hit answered from: the source session before, the
    /// project or a layer after.
    pub session: String,
    /// The hit's id in `session`.
    pub node_id: NodeId,
    /// The session the split assigned the origin to.
    pub primary: String,
    /// The origin is a shared hub (placed in more than one session).
    pub hub: bool,
    pub included_in_context: bool,
    /// The hit's annotation kinds, sorted.
    pub annotations: Vec<String>,
    /// Origins of the copies layered dedupe merged into this hit (same
    /// canonical key, or holder vectors at the dedupe threshold), as
    /// `(session, origin)`. Empty before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<(String, NodeId)>,
}

/// A hit present on both sides, and how far it moved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RankShift {
    pub origin: NodeId,
    pub before: usize,
    pub after: usize,
    /// `before - after`: positive means it moved up.
    pub shift: i64,
    /// Annotation kinds the hit gained after the split.
    pub annotations_added: Vec<String>,
    /// Annotation kinds it lost.
    pub annotations_removed: Vec<String>,
}

/// One query's comparison.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryComparison {
    pub query: String,
    pub before: Vec<VerifyHit>,
    pub after: Vec<VerifyHit>,
    /// Before hits the project can still see after the split (placed in it
    /// or in one of its layers).
    pub before_in_scope: usize,
    /// In-scope before hits that are also after hits.
    pub overlap_in_scope: usize,
    /// `overlap_in_scope / before_in_scope`; `None` with nothing in scope.
    pub recall_at_k: Option<f64>,
    /// In-scope before hits missing after: real losses.
    pub lost: Vec<NodeId>,
    /// In-scope before hits missing after as hits of their own, because
    /// layered dedupe merged them into another memory's hit (they are in
    /// that hit's `also`). Not counted in `lost` or in the overlap.
    pub deduped: Vec<NodeId>,
    /// After hits that were not before hits.
    pub gained: Vec<NodeId>,
    /// Before hits placed only in sessions this project does not read:
    /// other projects' memories, correctly removed by the split.
    pub removed_foreign: Vec<NodeId>,
    /// After hits whose origin the split did not place in the session they
    /// answered from. A correct split has none.
    pub leaked: Vec<NodeId>,
    pub shifts: Vec<RankShift>,
    /// Warning lines only before, and only after.
    pub warnings_removed: Vec<String>,
    pub warnings_added: Vec<String>,
    /// Response annotation kinds only before, and only after.
    pub response_annotations_removed: Vec<String>,
    pub response_annotations_added: Vec<String>,
}

/// Aggregates over one project's queries. Counts only, no ids or text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub queries: usize,
    /// The layer stack the after side read, nearest first.
    pub layers: Vec<String>,
    pub before_hits: usize,
    pub after_hits: usize,
    pub before_in_scope: usize,
    pub overlap_in_scope: usize,
    /// Micro recall@k: `overlap_in_scope / before_in_scope`.
    pub recall_at_k: Option<f64>,
    /// Macro recall@k: the mean over queries with something in scope.
    pub mean_recall_at_k: Option<f64>,
    pub lost: usize,
    pub deduped: usize,
    pub gained: usize,
    pub removed_foreign: usize,
    /// Removed foreign hits by the session the split assigned them to.
    pub removed_foreign_by_primary: BTreeMap<String, usize>,
    pub leaked: usize,
    /// After hits by the session they answered from.
    pub after_by_session: BTreeMap<String, usize>,
    /// After hits that are shared-hub copies.
    pub after_hub_hits: usize,
    pub common: usize,
    pub moved: usize,
    pub mean_abs_shift: Option<f64>,
    pub max_abs_shift: usize,
    /// Annotation kinds gained or lost by hits on both sides, by kind.
    pub annotations_added: BTreeMap<String, usize>,
    pub annotations_removed: BTreeMap<String, usize>,
    /// Queries whose warning lines or response annotations changed.
    pub queries_with_changed_warnings: usize,
}

/// Everything [`verify_split`] produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub report_version: u32,
    pub source_session: String,
    pub source_sha256: String,
    pub plan_sha256: String,
    pub split_sha256: String,
    pub split_algorithm_version: u32,
    pub embedding: EmbeddingContract,
    pub params: VerifyParams,
    /// The layer weight and floor of each layered project, as resolved.
    pub layer_knobs: BTreeMap<String, Vec<(String, f64, f64)>>,
    pub projects: BTreeMap<String, ProjectSummary>,
    pub queries: BTreeMap<String, Vec<QueryComparison>>,
}

/// Where each target id came from.
struct Origins {
    /// `(session, target id) -> source id`.
    by_target: HashMap<(String, NodeId), NodeId>,
    /// source id -> (primary, every session it is placed in).
    placed: HashMap<NodeId, (String, BTreeSet<String>)>,
}

impl Origins {
    fn new(split: &SplitOutput) -> Self {
        let mut by_target = HashMap::new();
        let mut placed = HashMap::new();
        for p in &split.placements {
            for (session, target) in &p.targets {
                by_target.insert((session.clone(), *target), p.id);
            }
            placed.insert(
                p.id,
                (p.primary.clone(), p.targets.keys().cloned().collect()),
            );
        }
        Self { by_target, placed }
    }
}

/// One session, materialized for an offline replay.
struct Replay {
    session: SessionId,
    graph: Arc<RwLock<Graph>>,
    daemon: Daemon,
}

impl Replay {
    fn new(
        snapshot: GraphSnapshot,
        config: &Config,
        embedding: &EmbeddingContract,
        at: DateTime<Utc>,
    ) -> Result<Self, VerifyError> {
        let name = snapshot.session_id.0.clone();
        assert_session_embedding_compatible(snapshot.embedding.as_ref(), embedding)
            .map_err(|e| VerifyError::Refused(format!("session {name}: {e}")))?;
        let loaded = crate::store::load::materialize(snapshot)
            .map_err(|e| VerifyError::Refused(format!("session {name} does not load: {e}")))?;
        let session = loaded.graph.session_id().clone();
        let graph = Arc::new(RwLock::new(loaded.graph));
        let daemon = Daemon::from_config(graph.clone(), config)
            .with_index(Arc::new(RwLock::new(loaded.index)));
        daemon.prime_offline(at);
        Ok(Self {
            session,
            graph,
            daemon,
        })
    }

    fn vectors(&self) -> VectorCandidates<'_> {
        VectorCandidates::Graph(GraphVectorSource::new(&self.graph))
    }

    /// `Memory::recall_text_with`: the session's own text pipeline.
    async fn own(
        &self,
        query: RecallQuery,
        vector: Option<&[f32]>,
        warning: Option<String>,
        ctx: &Ctx<'_>,
    ) -> crate::recall::detail::DetailedRecall {
        let mut cache = RecallCache::<RecallPipeline>::new();
        let mut result = self
            .daemon
            .recall_with(
                &self.session,
                query,
                self.vectors(),
                vector.map(|v| (v, ctx.embedding)),
                ctx.config.recall_weights,
                &mut cache,
            )
            .await;
        let mut warnings: Vec<String> = warning.into_iter().collect();
        warnings.append(&mut result.warnings);
        result.warnings = warnings;
        result
    }

    /// `Memory::recall_as_layer`: the daemon's `Route::Layer` pipeline.
    async fn as_layer(
        &self,
        query: RecallQuery,
        vector: Option<&[f32]>,
        ctx: &Ctx<'_>,
    ) -> crate::recall::detail::DetailedRecall {
        self.daemon
            .recall_layer_with(
                &self.session,
                query,
                self.vectors(),
                vector.map(|v| (v, ctx.embedding)),
                ctx.config.recall_weights,
            )
            .await
    }

    /// `Memory::hit_identities`.
    fn identities(&self, recall: &crate::recall::detail::DetailedRecall) -> Vec<HitIdentity> {
        let g = self.graph.read();
        recall
            .hits
            .iter()
            .map(|h| match g.node(h.node_id) {
                Some(Node::Concept(c)) => HitIdentity {
                    canonical_key: Some(c.canonical_key.clone()),
                    embedding: c.embedding.clone(),
                },
                _ => HitIdentity::default(),
            })
            .collect()
    }

    /// The query embedding, once (`Memory::embed_recall_query` without the
    /// cache): `None` and a warning on an embed failure.
    async fn embed(&self, text: &str, ctx: &Ctx<'_>) -> (Option<Vec<f32>>, Option<String>) {
        match candidates::embed_query(self.vectors(), ctx.embedder, text).await {
            Ok(v) => (v, None),
            Err(w) => (None, Some(w)),
        }
    }
}

struct Ctx<'a> {
    embedder: &'a dyn Embedder,
    embedding: &'a EmbeddingContract,
    config: &'a Config,
}

fn recall_query(text: &str, params: &VerifyParams) -> RecallQuery {
    RecallQuery {
        query: text.to_string(),
        top_k: params.top_k,
        max_tokens: params.max_tokens,
        traversal_depth: params.traversal_depth,
    }
}

/// Plain recall: `Memory::recall_detailed` without the access note.
async fn plain(replay: &Replay, text: &str, params: &VerifyParams, ctx: &Ctx<'_>) -> LayeredRecall {
    let (vector, warning) = replay.embed(text, ctx).await;
    let detail = replay
        .own(recall_query(text, params), vector.as_deref(), warning, ctx)
        .await;
    LayeredRecall::single(&replay.session, detail)
}

/// Layered recall: `Memory::recall_layered_slots` over live layers, without
/// the access note. Every layer is graph-backed here, so the primary always
/// embeds and searches (its vector source is available).
async fn layered_recall(
    primary: &Replay,
    layers: &[(&Replay, &LayerSpec)],
    text: &str,
    params: &VerifyParams,
    ctx: &Ctx<'_>,
) -> LayeredRecall {
    if layers.is_empty() {
        return plain(primary, text, params, ctx).await;
    }
    let (vector, warning) = primary.embed(text, ctx).await;
    let own = primary
        .own(recall_query(text, params), vector.as_deref(), warning, ctx)
        .await;
    let identities = primary.identities(&own);
    let mut lists = vec![TierList {
        session: primary.session.clone(),
        weight: PRIMARY_WEIGHT,
        floor: None,
        recall: Some(TierRecall {
            recall: own,
            identities,
        }),
    }];
    for (replay, spec) in layers {
        let recall = replay
            .as_layer(recall_query(text, params), vector.as_deref(), ctx)
            .await;
        let identities = replay.identities(&recall);
        lists.push(TierList {
            session: replay.session.clone(),
            weight: spec.weight,
            floor: Some(spec.min_similarity),
            recall: Some(TierRecall { recall, identities }),
        });
    }
    layered::merge(
        &lists,
        MergeParams {
            top_k: params.top_k,
            max_tokens: params.max_tokens,
            dedupe_threshold: ctx.config.semantic_match_threshold,
            vector_pairs: layered::MAX_VECTOR_DEDUPE_PAIRS,
        },
        format::default_token_count,
    )
}

fn kind_name<T: Serialize>(kind: &T) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The hits of one side, mapped to their origins.
fn side_hits(
    recall: &LayeredRecall,
    origins: &Origins,
    source_session: Option<&str>,
) -> Result<Vec<VerifyHit>, VerifyError> {
    let mut out = Vec::with_capacity(recall.hits.len());
    for (i, (hit, detail)) in recall.hits.iter().zip(&recall.detail.detailed).enumerate() {
        let session = hit.session.0.clone();
        let origin = match source_session {
            // Before: the source's ids are the origins.
            Some(_) => hit.hit.node_id,
            None => *origins
                .by_target
                .get(&(session.clone(), hit.hit.node_id))
                .ok_or_else(|| {
                    VerifyError::Refused(format!(
                        "a hit in {session} has no placement in this split; the split output \
                         does not match the plan"
                    ))
                })?,
        };
        let (primary, placed) = origins.placed.get(&origin).ok_or_else(|| {
            VerifyError::Refused(
                "a source hit has no placement in this split; the plan does not match the source"
                    .into(),
            )
        })?;
        let mut also = Vec::with_capacity(hit.also.len());
        for copy in &hit.also {
            let session = copy.session.0.clone();
            let origin = *origins
                .by_target
                .get(&(session.clone(), copy.node_id))
                .ok_or_else(|| {
                    VerifyError::Refused(format!(
                        "a deduplicated copy in {session} has no placement in this split; the \
                         split output does not match the plan"
                    ))
                })?;
            also.push((session, origin));
        }
        let mut annotations: Vec<String> = detail
            .annotations
            .iter()
            .map(|a| kind_name(&a.kind))
            .collect();
        annotations.sort();
        annotations.dedup();
        out.push(VerifyHit {
            rank: i + 1,
            origin,
            session,
            node_id: hit.hit.node_id,
            primary: primary.clone(),
            hub: placed.len() > 1,
            included_in_context: hit.included_in_context,
            annotations,
            also,
        });
    }
    Ok(out)
}

fn set_diff(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Vec<String> {
    a.difference(b).cloned().collect()
}

fn compare(
    query: &str,
    before_recall: &LayeredRecall,
    after_recall: &LayeredRecall,
    before: Vec<VerifyHit>,
    after: Vec<VerifyHit>,
    visible: &BTreeSet<String>,
    origins: &Origins,
) -> QueryComparison {
    let in_scope = |o: &NodeId| {
        origins
            .placed
            .get(o)
            .is_some_and(|(_, placed)| placed.iter().any(|s| visible.contains(s)))
    };
    let after_rank: HashMap<NodeId, &VerifyHit> =
        after.iter().rev().map(|h| (h.origin, h)).collect();
    let before_rank: HashMap<NodeId, &VerifyHit> =
        before.iter().rev().map(|h| (h.origin, h)).collect();

    let mut before_in_scope = 0;
    let mut overlap_in_scope = 0;
    let mut lost = Vec::new();
    let mut deduped = Vec::new();
    let merged: HashSet<NodeId> = after
        .iter()
        .flat_map(|h| h.also.iter().map(|(_, o)| *o))
        .collect();
    let mut removed_foreign = Vec::new();
    let mut shifts = Vec::new();
    let mut seen = HashSet::new();
    for h in &before {
        if !seen.insert(h.origin) {
            continue;
        }
        if !in_scope(&h.origin) {
            removed_foreign.push(h.origin);
            continue;
        }
        before_in_scope += 1;
        match after_rank.get(&h.origin) {
            Some(a) => {
                overlap_in_scope += 1;
                let b: BTreeSet<String> = h.annotations.iter().cloned().collect();
                let n: BTreeSet<String> = a.annotations.iter().cloned().collect();
                shifts.push(RankShift {
                    origin: h.origin,
                    before: h.rank,
                    after: a.rank,
                    shift: h.rank as i64 - a.rank as i64,
                    annotations_added: set_diff(&n, &b),
                    annotations_removed: set_diff(&b, &n),
                });
            }
            None if merged.contains(&h.origin) => deduped.push(h.origin),
            None => lost.push(h.origin),
        }
    }
    let mut gained = Vec::new();
    let mut leaked = Vec::new();
    let mut seen = HashSet::new();
    for h in &after {
        let placed_here = origins
            .placed
            .get(&h.origin)
            .is_some_and(|(_, placed)| placed.contains(&h.session));
        if !placed_here || !visible.contains(&h.session) {
            leaked.push(h.origin);
        }
        if seen.insert(h.origin) && !before_rank.contains_key(&h.origin) {
            gained.push(h.origin);
        }
    }
    let lines = |r: &LayeredRecall| r.warnings.iter().cloned().collect::<BTreeSet<String>>();
    let kinds = |r: &LayeredRecall| {
        r.detail
            .response_annotations
            .iter()
            .map(|a| kind_name(&a.kind))
            .collect::<BTreeSet<String>>()
    };
    let (wb, wa) = (lines(before_recall), lines(after_recall));
    let (kb, ka) = (kinds(before_recall), kinds(after_recall));
    QueryComparison {
        query: query.to_string(),
        before_in_scope,
        overlap_in_scope,
        recall_at_k: (before_in_scope > 0)
            .then(|| overlap_in_scope as f64 / before_in_scope as f64),
        lost,
        deduped,
        gained,
        removed_foreign,
        leaked,
        shifts,
        warnings_removed: set_diff(&wb, &wa),
        warnings_added: set_diff(&wa, &wb),
        response_annotations_removed: set_diff(&kb, &ka),
        response_annotations_added: set_diff(&ka, &kb),
        before,
        after,
    }
}

fn summarize(layers: Vec<String>, rows: &[QueryComparison]) -> ProjectSummary {
    let mut s = ProjectSummary {
        queries: rows.len(),
        layers,
        ..ProjectSummary::default()
    };
    let mut recalls = Vec::new();
    let mut abs_shifts = Vec::new();
    for q in rows {
        s.before_hits += q.before.len();
        s.after_hits += q.after.len();
        s.before_in_scope += q.before_in_scope;
        s.overlap_in_scope += q.overlap_in_scope;
        recalls.extend(q.recall_at_k);
        s.lost += q.lost.len();
        s.deduped += q.deduped.len();
        s.gained += q.gained.len();
        s.removed_foreign += q.removed_foreign.len();
        for origin in &q.removed_foreign {
            if let Some(h) = q.before.iter().find(|h| &h.origin == origin) {
                *s.removed_foreign_by_primary
                    .entry(h.primary.clone())
                    .or_default() += 1;
            }
        }
        s.leaked += q.leaked.len();
        for h in &q.after {
            *s.after_by_session.entry(h.session.clone()).or_default() += 1;
            if h.hub {
                s.after_hub_hits += 1;
            }
        }
        s.common += q.shifts.len();
        for shift in &q.shifts {
            let abs = shift.shift.unsigned_abs() as usize;
            abs_shifts.push(abs);
            if abs > 0 {
                s.moved += 1;
            }
            s.max_abs_shift = s.max_abs_shift.max(abs);
            for k in &shift.annotations_added {
                *s.annotations_added.entry(k.clone()).or_default() += 1;
            }
            for k in &shift.annotations_removed {
                *s.annotations_removed.entry(k.clone()).or_default() += 1;
            }
        }
        if !(q.warnings_added.is_empty()
            && q.warnings_removed.is_empty()
            && q.response_annotations_added.is_empty()
            && q.response_annotations_removed.is_empty())
        {
            s.queries_with_changed_warnings += 1;
        }
    }
    s.recall_at_k =
        (s.before_in_scope > 0).then(|| s.overlap_in_scope as f64 / s.before_in_scope as f64);
    s.mean_recall_at_k =
        (!recalls.is_empty()).then(|| recalls.iter().sum::<f64>() / recalls.len() as f64);
    s.mean_abs_shift = (!abs_shifts.is_empty())
        .then(|| abs_shifts.iter().sum::<usize>() as f64 / abs_shifts.len() as f64);
    s
}

/// Everything [`verify_split`] reads.
pub struct VerifyInputs<'a> {
    /// The immutable source session.
    pub source: GraphSnapshot,
    /// The split rebuilt from the source and plan ([`super::build_split`]):
    /// its placements map every target id back to its origin.
    pub split: &'a SplitOutput,
    /// Every target session as the split output stores it, already checked
    /// against `split` (receipts and target digests).
    pub targets: BTreeMap<String, GraphSnapshot>,
    /// The layer stacks a serve pinning every target would resolve.
    pub layers: &'a ServeLayers,
    pub queries: &'a VerifyQueries,
    pub params: VerifyParams,
    pub embedder: &'a dyn Embedder,
    pub embedding: &'a EmbeddingContract,
    pub config: &'a Config,
}

/// Run the before/after comparison. Deterministic: the same inputs give
/// the same report. Refused when a target is missing or does not match the
/// split, when a session's embedding contract is not the embedder's, when
/// the query set names a project the split did not produce, or when a hit
/// cannot be mapped back to its origin.
pub async fn verify_split(inputs: VerifyInputs<'_>) -> Result<VerifyReport, VerifyError> {
    let VerifyInputs {
        source,
        split,
        targets,
        layers,
        queries,
        params,
        embedder,
        embedding,
        config,
    } = inputs;
    queries.validate()?;
    if params.top_k == 0 || crate::store::validate_vector_candidate_limit(params.top_k).is_err() {
        return refuse("top_k is out of range");
    }
    if source.session_id.0 != split.source_session {
        return refuse("the source is not the split's source session");
    }
    let expected: BTreeMap<&str, &str> = split
        .targets
        .iter()
        .map(|t| (t.session.as_str(), t.sha256.as_str()))
        .collect();
    let given: BTreeSet<&str> = targets.keys().map(String::as_str).collect();
    if given != expected.keys().copied().collect() {
        return refuse("the split output's sessions are not the split's targets");
    }
    for (name, snapshot) in &targets {
        if super::target_sha256(snapshot)? != expected[name.as_str()] {
            return refuse(format!(
                "target {name} does not match the split of this source and plan"
            ));
        }
    }
    for project in queries.queries.keys() {
        if !targets.contains_key(project) {
            return refuse(format!(
                "the query set names {project}, which is not a target of this split"
            ));
        }
    }
    let origins = Origins::new(split);
    let ctx = Ctx {
        embedder,
        embedding,
        config,
    };
    let at = params.at;
    let before_replay = Replay::new(source, config, embedding, at)?;

    // Materialize only what the query set reads: its projects and their layers.
    let mut needed: BTreeSet<&str> = BTreeSet::new();
    for project in queries.queries.keys() {
        needed.insert(project);
        needed.extend(layers.stack(project).iter().map(|l| l.session.as_str()));
    }
    let mut replays: BTreeMap<String, Replay> = BTreeMap::new();
    for (name, snapshot) in targets {
        if needed.contains(name.as_str()) {
            let replay = Replay::new(snapshot, config, embedding, at)?;
            replays.insert(name, replay);
        }
    }

    let mut projects = BTreeMap::new();
    let mut per_query = BTreeMap::new();
    let mut layer_knobs = BTreeMap::new();
    for (project, list) in &queries.queries {
        let stack = layers.stack(project);
        let mut stack_replays = Vec::with_capacity(stack.len());
        for spec in stack {
            let replay = replays.get(&spec.session).ok_or_else(|| {
                VerifyError::Refused(format!(
                    "{project} is layered with {}, which is not a target of this split",
                    spec.session
                ))
            })?;
            stack_replays.push((replay, spec));
        }
        if !stack.is_empty() {
            layer_knobs.insert(
                project.clone(),
                stack
                    .iter()
                    .map(|l| (l.session.clone(), l.weight, l.min_similarity))
                    .collect::<Vec<_>>(),
            );
        }
        let primary = &replays[project];
        let mut visible: BTreeSet<String> = BTreeSet::from([project.clone()]);
        visible.extend(stack.iter().map(|l| l.session.clone()));
        let mut rows = Vec::with_capacity(list.len());
        for text in list {
            let before_recall = plain(&before_replay, text, &params, &ctx).await;
            let after_recall = layered_recall(primary, &stack_replays, text, &params, &ctx).await;
            let before = side_hits(&before_recall, &origins, Some(&split.source_session))?;
            let after = side_hits(&after_recall, &origins, None)?;
            rows.push(compare(
                text,
                &before_recall,
                &after_recall,
                before,
                after,
                &visible,
                &origins,
            ));
        }
        let layer_names = stack.iter().map(|l| l.session.clone()).collect();
        projects.insert(project.clone(), summarize(layer_names, &rows));
        per_query.insert(project.clone(), rows);
    }
    Ok(VerifyReport {
        report_version: VERIFY_REPORT_VERSION,
        source_session: split.source_session.clone(),
        source_sha256: split.source_sha256.clone(),
        plan_sha256: split.plan_sha256.clone(),
        split_sha256: split.split_sha256.clone(),
        split_algorithm_version: split.algorithm_version,
        embedding: embedding.clone(),
        params,
        layer_knobs,
        projects,
        queries: per_query,
    })
}

#[cfg(all(test, feature = "embed-fixture"))]
#[path = "verify_tests.rs"]
mod tests;
