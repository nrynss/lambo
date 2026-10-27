//! Before/after recall comparison of a session split (#33 D3).
//!
//! [`verify_split`] replays a fixed query set per project twice:
//!
//! * **before**: plain recall on the mixed source session, as a holder of
//!   that session answers it today;
//! * **after**: layered recall as an agent for that project sees it once the
//!   split is served over HTTP: the project's target session plus the layer
//!   stack the serving `[serve]` table gives it. The pinned sessions are
//!   worked out exactly as `lambo serve --transport http` does with no
//!   `--session` (`[serve] sessions`, with `default_session`), the stacks by
//!   `ServeConfig::resolve_layers` over them, and the merge is the same
//!   `recall::layered::merge` with the knobs `Memory::recall_layered`
//!   passes. A queried project the serve would attach on demand (so
//!   unlayered) is refused.
//!
//! Hits are compared by **origin identity**: an "after" hit's id is mapped
//! back to its source id through the split's placements (every target id,
//! hub copies and origin replicas included), so a memory that moved, was
//! copied into a hub placement or answers from `general` counts as the same
//! memory it was before.
//!
//! The replay is offline and read-only, and it measures a **SQLite
//! holder**: each session is materialized as a holder loads it
//! (`store::load::materialize`) into an **unspawned** daemon whose clock is
//! pinned, primed once (`Daemon::prime_offline`: rescore and hot-list
//! detection, no events, no GC, no access ledger), and the vector leg ranks
//! in the session's own graph, which is what `VectorCandidates::for_holder`
//! picks over SQLite (its checked read is an exact scan). Each query is
//! embedded once and the vector is shared by both sides. Nothing notes
//! accesses, so one query never changes the next one's answer, and the same
//! inputs always give the same report.
//!
//! **Disclosure.** The report names an origin id only when the session the
//! split assigned it to is a queried project or one of their layers, and is
//! not aggregate-only. Everything else (other projects' hits, and every hit
//! of an aggregate-only session such as an excluded project) appears only as
//! counts by assigned session. Warning lines are reduced to their kind
//! (`stale_session`, `conflict`, ...), never kept raw: a line can quote an
//! agent id or a contradicting concept's text.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use super::{ClassificationPlan, ItemKind, SplitOutput};
use crate::config::{Config, LayerSpec, ServeConfig, ServeLayers};
use crate::daemon::{Daemon, RecallPipeline};
use crate::embed::Embedder;
use crate::graph::vector_source::GraphVectorSource;
use crate::graph::Graph;
use crate::recall::cache::SharedRecallCache;
use crate::recall::detail::DetailedRecall;
use crate::recall::format;
use crate::recall::layered::{
    self, HitIdentity, LayeredRecall, MergeParams, TierList, TierRecall, PRIMARY_WEIGHT,
};
use crate::resolve::assert_session_embedding_compatible;
use crate::store::vector_source::VectorCandidates;
use crate::types::{EmbeddingContract, GraphSnapshot, Node, NodeId, RecallQuery, SessionId};

/// Version of the verify report's JSON shape. Version 2: disclosure rules,
/// warning kinds instead of lines, foreign-primary after hits, recall@k cut
/// at rank `k`, pinned sessions from `[serve]`.
pub const VERIFY_REPORT_VERSION: u32 = 2;

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
    /// the `k` of recall@k (both sides cut at rank `k`).
    pub top_k: usize,
    pub max_tokens: usize,
    pub traversal_depth: usize,
    /// The clock every session's detectors and hot-list re-validation run
    /// at. Pinned so a rerun gives the same report.
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

/// The kind of one warning line, keeping a layer's `[<session>] ` prefix.
/// A raw line can quote an agent id, a high-risk reason or a contradicting
/// concept's text, so the report keeps only this.
pub fn warning_kind(line: &str) -> String {
    let (prefix, body) = match line.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        Some((session, body)) => (Some(session), body),
        None => (None, line),
    };
    let kind = if body.starts_with("Session inactive") {
        "stale_session"
    } else if body.starts_with("Agent ") && body.contains(" wrote to it ") {
        "conflict"
    } else if body.starts_with("High-risk modification") {
        "high_risk"
    } else if body.starts_with("Drifted ") {
        "drift"
    } else if body.starts_with("⚠ Contradicted") {
        "contradicted"
    } else if body.starts_with("⚠ Contradicts") {
        "contradicts"
    } else if body.starts_with("⚠ Rests on ") {
        "rests_on_contradicted"
    } else if body.starts_with("⚑ Load-bearing pillar") {
        "load_bearing"
    } else if body.starts_with("Reserved by ") {
        "reservation"
    } else if body.starts_with("recall: dependency question answered") {
        "traversal"
    } else if body.starts_with("recall: layer ") && body.contains(" is unavailable") {
        "layer_unavailable"
    } else if body.starts_with("recall: query embedding failed") {
        "embed_failed"
    } else if body.starts_with("recall:") && body.contains("vector") {
        "vector_degraded"
    } else if body.starts_with("recall:") {
        "recall_other"
    } else {
        "other"
    };
    match prefix {
        Some(session) => format!("[{session}] {kind}"),
        None => kind.to_owned(),
    }
}

/// One disclosed hit of one side.
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
    /// The origin is a shared hub of the plan (a concept with `copy_into`).
    pub hub: bool,
    pub included_in_context: bool,
    /// The hit's annotation kinds, sorted.
    pub annotations: Vec<String>,
    /// Disclosed origins of the copies layered dedupe merged into this hit,
    /// as `(session, origin)`. Empty before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<(String, NodeId)>,
    /// Merged copies whose origin is not disclosed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub also_undisclosed: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A disclosed hit present on both sides, and how far it moved.
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

/// One query's comparison. Origin ids appear only when disclosed (see the
/// module docs); every count covers all hits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueryComparison {
    pub query: String,
    /// Disclosed before hits.
    pub before: Vec<VerifyHit>,
    /// Undisclosed before hits, by the session the split assigned them to.
    pub before_undisclosed_by_primary: BTreeMap<String, usize>,
    /// Disclosed after hits.
    pub after: Vec<VerifyHit>,
    /// Undisclosed after hits, by assigned session.
    pub after_undisclosed_by_primary: BTreeMap<String, usize>,
    /// Before hits (any rank) the project can still see after the split
    /// (placed in it or in one of its layers), and those that are after hits.
    pub before_in_scope: usize,
    pub overlap_in_scope: usize,
    /// The same, both sides cut at rank `k`.
    pub before_in_scope_at_k: usize,
    pub overlap_at_k: usize,
    /// `overlap_at_k / before_in_scope_at_k`; `None` with nothing in scope.
    pub recall_at_k: Option<f64>,
    /// In-scope before hits missing after (any rank).
    pub lost: usize,
    /// In-scope before hits missing as hits of their own because layered
    /// dedupe merged them into another memory's hit (its `also`).
    pub deduped: usize,
    /// After hits that were not before hits.
    pub gained: usize,
    /// Disclosed origins of the three lists above.
    pub lost_ids: Vec<NodeId>,
    pub deduped_ids: Vec<NodeId>,
    pub gained_ids: Vec<NodeId>,
    /// Before hits placed only in sessions this project does not read
    /// (removed by the split), by assigned session.
    pub removed_foreign_by_primary: BTreeMap<String, usize>,
    /// After hits whose assigned session is not one this project reads: a
    /// shared hub copied in from another project, by assigned session.
    pub foreign_primary_after_by_primary: BTreeMap<String, usize>,
    /// Disclosed hits on both sides.
    pub shifts: Vec<RankShift>,
    /// Warning kinds only before, and only after (see [`warning_kind`]).
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
    pub before_in_scope_at_k: usize,
    pub overlap_at_k: usize,
    /// Micro recall@k: `overlap_at_k / before_in_scope_at_k`.
    pub recall_at_k: Option<f64>,
    /// Macro recall@k: the mean over queries with something in scope.
    pub mean_recall_at_k: Option<f64>,
    pub lost: usize,
    pub deduped: usize,
    pub gained: usize,
    pub removed_foreign: usize,
    pub removed_foreign_by_primary: BTreeMap<String, usize>,
    /// After hits assigned to a session the project does not read.
    pub foreign_primary_after: usize,
    pub foreign_primary_after_by_primary: BTreeMap<String, usize>,
    /// After hits by the session they answered from.
    pub after_by_session: BTreeMap<String, usize>,
    /// After hits that are shared-hub copies (the plan's `copy_into`).
    pub after_hub_hits: usize,
    /// Hits on both sides, all of them (disclosed or not).
    pub common: usize,
    pub moved: usize,
    pub mean_abs_shift: Option<f64>,
    pub max_abs_shift: usize,
    /// Annotation kinds gained or lost by hits on both sides, by kind.
    pub annotations_added: BTreeMap<String, usize>,
    pub annotations_removed: BTreeMap<String, usize>,
    /// Warning kinds gained or lost, by kind (one per query that changed).
    pub warnings_added: BTreeMap<String, usize>,
    pub warnings_removed: BTreeMap<String, usize>,
    /// Queries whose warning kinds or response annotations changed.
    pub queries_with_changed_warnings: usize,
}

/// Everything [`verify_split`] produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub report_version: u32,
    /// What the replay measures (see the module docs).
    pub measures: String,
    pub source_session: String,
    pub source_sha256: String,
    pub plan_sha256: String,
    pub split_sha256: String,
    pub split_algorithm_version: u32,
    pub embedding: EmbeddingContract,
    pub params: VerifyParams,
    /// The sessions the serving `[serve]` table pins, and its default.
    pub pinned: Vec<String>,
    pub default_session: String,
    /// Sessions that appear only as counts.
    pub aggregate_only: Vec<String>,
    /// The layer weight and floor of each layered project, as resolved.
    pub layer_knobs: BTreeMap<String, Vec<(String, f64, f64)>>,
    pub projects: BTreeMap<String, ProjectSummary>,
    pub queries: BTreeMap<String, Vec<QueryComparison>>,
}

/// What the report's `measures` field and the summary say.
pub const MEASURES: &str = "SQLite holder behaviour: holder-graph vector ranking, first-cycle \
scores and hot list at the pinned clock, no access history; a serve on another store kind or \
with a [recall] tier can rank differently";

/// Where each target id came from.
struct Origins {
    /// `(session, target id) -> source id`.
    by_target: HashMap<(String, NodeId), NodeId>,
    /// source id -> (primary, every session it is placed in).
    placed: HashMap<NodeId, (String, BTreeSet<String>)>,
    /// The plan's shared hubs: concepts with a `copy_into`.
    hubs: HashSet<NodeId>,
}

impl Origins {
    fn new(split: &SplitOutput, plan: &ClassificationPlan) -> Self {
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
        let hubs = plan
            .assignments
            .iter()
            .filter(|a| a.kind == ItemKind::Concept && !a.copy_into.is_empty())
            .map(|a| a.id)
            .collect();
        Self {
            by_target,
            placed,
            hubs,
        }
    }

    fn primary(&self, origin: &NodeId) -> &str {
        self.placed.get(origin).map_or("", |(p, _)| p.as_str())
    }

    fn placed_in_any(&self, origin: &NodeId, sessions: &BTreeSet<String>) -> bool {
        self.placed
            .get(origin)
            .is_some_and(|(_, placed)| placed.iter().any(|s| sessions.contains(s)))
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
        // One pinned clock for detection and for recall's read-time
        // re-validation of hot-list entries (stale seconds, high-risk and
        // conflict windows), so a rerun renders the same warnings.
        let daemon = Daemon::from_config(graph.clone(), config)
            .with_index(Arc::new(RwLock::new(loaded.index)))
            .with_clock(Arc::new(move || at));
        daemon.prime_offline();
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
    async fn own(&self, query: RecallQuery, vector: &[f32], ctx: &Ctx<'_>) -> DetailedRecall {
        let cache = SharedRecallCache::<RecallPipeline>::new();
        self.daemon
            .recall_with(
                &self.session,
                query,
                self.vectors(),
                Some((vector, ctx.embedding)),
                ctx.config.recall_weights,
                &cache,
            )
            .await
    }

    /// `Memory::recall_as_layer`: the daemon's `Route::Layer` pipeline.
    async fn as_layer(&self, query: RecallQuery, vector: &[f32], ctx: &Ctx<'_>) -> DetailedRecall {
        self.daemon
            .recall_layer_with(
                &self.session,
                query,
                self.vectors(),
                Some((vector, ctx.embedding)),
                ctx.config.recall_weights,
            )
            .await
    }

    /// `Memory::hit_identities`.
    fn identities(&self, recall: &DetailedRecall) -> Vec<HitIdentity> {
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
}

struct Ctx<'a> {
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

/// Plain recall: `Memory::recall_detailed` without the access note, with
/// the query vector already in hand.
async fn plain(
    replay: &Replay,
    text: &str,
    vector: &[f32],
    params: &VerifyParams,
    ctx: &Ctx<'_>,
) -> LayeredRecall {
    let detail = replay.own(recall_query(text, params), vector, ctx).await;
    LayeredRecall::single(&replay.session, detail)
}

/// Layered recall: `Memory::recall_layered_slots` over live layers, without
/// the access note. Every session is graph-backed (SQLite holder), so the
/// primary searches vectors and every layer gets the same query vector.
async fn layered_recall(
    primary: &Replay,
    layers: &[(&Replay, &LayerSpec)],
    text: &str,
    vector: &[f32],
    params: &VerifyParams,
    ctx: &Ctx<'_>,
) -> LayeredRecall {
    if layers.is_empty() {
        return plain(primary, text, vector, params, ctx).await;
    }
    let own = primary.own(recall_query(text, params), vector, ctx).await;
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
            .as_layer(recall_query(text, params), vector, ctx)
            .await;
        let identities = replay.identities(&recall);
        lists.push(TierList {
            session: replay.session.clone(),
            weight: spec.weight,
            floor: Some(crate::embed::calibration::resolve_layer_floor(
                spec.configured_min_similarity(),
                ctx.config,
                ctx.embedding,
            )),
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

/// One hit of one side, before disclosure.
struct Hit {
    rank: usize,
    origin: NodeId,
    session: String,
    node_id: NodeId,
    included_in_context: bool,
    annotations: Vec<String>,
    also: Vec<(String, NodeId)>,
}

/// The hits of one side, mapped to their origins. An after hit (or a
/// merged copy) that the split did not place in the session it answered
/// from means the output and the placements disagree, and refuses.
fn side_hits(
    recall: &LayeredRecall,
    origins: &Origins,
    before: bool,
) -> Result<Vec<Hit>, VerifyError> {
    let origin_of = |session: &str, id: NodeId| -> Result<NodeId, VerifyError> {
        if before {
            return origins
                .placed
                .contains_key(&id)
                .then_some(id)
                .ok_or_else(|| {
                    VerifyError::Refused(
                        "a source hit has no placement in this split; the plan does not match \
                         the source"
                            .into(),
                    )
                });
        }
        origins
            .by_target
            .get(&(session.to_string(), id))
            .copied()
            .ok_or_else(|| {
                VerifyError::Refused(format!(
                    "a hit in {session} has no placement in this split; the split output does \
                     not match the plan"
                ))
            })
    };
    let mut out = Vec::with_capacity(recall.hits.len());
    for (i, (hit, detail)) in recall.hits.iter().zip(&recall.detail.detailed).enumerate() {
        let session = hit.session.0.clone();
        let origin = origin_of(&session, hit.hit.node_id)?;
        let mut also = Vec::with_capacity(hit.also.len());
        for copy in &hit.also {
            let s = copy.session.0.clone();
            let o = origin_of(&s, copy.node_id)?;
            also.push((s, o));
        }
        let mut annotations: Vec<String> = detail
            .annotations
            .iter()
            .map(|a| kind_name(&a.kind))
            .collect();
        annotations.sort();
        annotations.dedup();
        out.push(Hit {
            rank: i + 1,
            origin,
            session,
            node_id: hit.hit.node_id,
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

/// What one query's comparison may name, and which sessions the project
/// reads.
struct Scope<'a> {
    visible: &'a BTreeSet<String>,
    disclosed: &'a BTreeSet<String>,
    k: usize,
}

fn compare(
    query: &str,
    before_recall: &LayeredRecall,
    after_recall: &LayeredRecall,
    before: &[Hit],
    after: &[Hit],
    scope: &Scope<'_>,
    origins: &Origins,
) -> QueryComparison {
    let disclosed = |o: &NodeId| scope.disclosed.contains(origins.primary(o));
    let in_scope = |o: &NodeId| origins.placed_in_any(o, scope.visible);
    let first_rank = |hits: &[Hit]| {
        let mut m: HashMap<NodeId, usize> = HashMap::new();
        for h in hits {
            m.entry(h.origin).or_insert(h.rank);
        }
        m
    };
    let (before_rank, after_rank) = (first_rank(before), first_rank(after));
    let merged: HashSet<NodeId> = after
        .iter()
        .flat_map(|h| h.also.iter().map(|(_, o)| *o))
        .collect();

    let mut q = QueryComparison {
        query: query.to_string(),
        before: Vec::new(),
        before_undisclosed_by_primary: BTreeMap::new(),
        after: Vec::new(),
        after_undisclosed_by_primary: BTreeMap::new(),
        before_in_scope: 0,
        overlap_in_scope: 0,
        before_in_scope_at_k: 0,
        overlap_at_k: 0,
        recall_at_k: None,
        lost: 0,
        deduped: 0,
        gained: 0,
        lost_ids: Vec::new(),
        deduped_ids: Vec::new(),
        gained_ids: Vec::new(),
        removed_foreign_by_primary: BTreeMap::new(),
        foreign_primary_after_by_primary: BTreeMap::new(),
        shifts: Vec::new(),
        warnings_removed: Vec::new(),
        warnings_added: Vec::new(),
        response_annotations_removed: Vec::new(),
        response_annotations_added: Vec::new(),
    };
    let disclose = |h: &Hit| VerifyHit {
        rank: h.rank,
        origin: h.origin,
        session: h.session.clone(),
        node_id: h.node_id,
        primary: origins.primary(&h.origin).to_owned(),
        hub: origins.hubs.contains(&h.origin),
        included_in_context: h.included_in_context,
        annotations: h.annotations.clone(),
        also: h
            .also
            .iter()
            .filter(|(_, o)| disclosed(o))
            .cloned()
            .collect(),
        also_undisclosed: h.also.iter().filter(|(_, o)| !disclosed(o)).count(),
    };

    let mut seen = HashSet::new();
    for h in before {
        let primary = origins.primary(&h.origin).to_owned();
        if disclosed(&h.origin) {
            q.before.push(disclose(h));
        } else {
            *q.before_undisclosed_by_primary
                .entry(primary.clone())
                .or_default() += 1;
        }
        if !seen.insert(h.origin) {
            continue;
        }
        if !in_scope(&h.origin) {
            *q.removed_foreign_by_primary.entry(primary).or_default() += 1;
            continue;
        }
        q.before_in_scope += 1;
        if h.rank <= scope.k {
            q.before_in_scope_at_k += 1;
            if after_rank.get(&h.origin).is_some_and(|r| *r <= scope.k) {
                q.overlap_at_k += 1;
            }
        }
        match after_rank.get(&h.origin) {
            Some(&a) => {
                q.overlap_in_scope += 1;
                let after_hit = after.iter().find(|x| x.rank == a).expect("ranked hit");
                if disclosed(&h.origin) {
                    let b: BTreeSet<String> = h.annotations.iter().cloned().collect();
                    let n: BTreeSet<String> = after_hit.annotations.iter().cloned().collect();
                    q.shifts.push(RankShift {
                        origin: h.origin,
                        before: h.rank,
                        after: a,
                        shift: h.rank as i64 - a as i64,
                        annotations_added: set_diff(&n, &b),
                        annotations_removed: set_diff(&b, &n),
                    });
                }
            }
            None if merged.contains(&h.origin) => {
                q.deduped += 1;
                if disclosed(&h.origin) {
                    q.deduped_ids.push(h.origin);
                }
            }
            None => {
                q.lost += 1;
                if disclosed(&h.origin) {
                    q.lost_ids.push(h.origin);
                }
            }
        }
    }
    let mut seen = HashSet::new();
    for h in after {
        let primary = origins.primary(&h.origin).to_owned();
        if disclosed(&h.origin) {
            q.after.push(disclose(h));
        } else {
            *q.after_undisclosed_by_primary
                .entry(primary.clone())
                .or_default() += 1;
        }
        if !scope.visible.contains(&primary) {
            *q.foreign_primary_after_by_primary
                .entry(primary)
                .or_default() += 1;
        }
        if seen.insert(h.origin) && !before_rank.contains_key(&h.origin) {
            q.gained += 1;
            if disclosed(&h.origin) {
                q.gained_ids.push(h.origin);
            }
        }
    }
    q.recall_at_k =
        (q.before_in_scope_at_k > 0).then(|| q.overlap_at_k as f64 / q.before_in_scope_at_k as f64);
    let kinds = |r: &LayeredRecall| -> BTreeSet<String> {
        r.warnings.iter().map(|w| warning_kind(w)).collect()
    };
    let responses = |r: &LayeredRecall| -> BTreeSet<String> {
        r.detail
            .response_annotations
            .iter()
            .map(|a| kind_name(&a.kind))
            .collect()
    };
    let (wb, wa) = (kinds(before_recall), kinds(after_recall));
    let (kb, ka) = (responses(before_recall), responses(after_recall));
    q.warnings_removed = set_diff(&wb, &wa);
    q.warnings_added = set_diff(&wa, &wb);
    q.response_annotations_removed = set_diff(&kb, &ka);
    q.response_annotations_added = set_diff(&ka, &kb);
    q
}

/// Aggregates over one project's queries. `common_all` and `shifts_all`
/// cover every hit on both sides, disclosed or not.
fn summarize(
    layers: Vec<String>,
    rows: &[QueryComparison],
    all_shifts: &[(usize, usize)],
    after_hub_hits: usize,
    after_by_session: BTreeMap<String, usize>,
    before_hits: usize,
    after_hits: usize,
) -> ProjectSummary {
    let mut s = ProjectSummary {
        queries: rows.len(),
        layers,
        before_hits,
        after_hits,
        after_hub_hits,
        after_by_session,
        ..ProjectSummary::default()
    };
    let mut recalls = Vec::new();
    for q in rows {
        s.before_in_scope += q.before_in_scope;
        s.overlap_in_scope += q.overlap_in_scope;
        s.before_in_scope_at_k += q.before_in_scope_at_k;
        s.overlap_at_k += q.overlap_at_k;
        recalls.extend(q.recall_at_k);
        s.lost += q.lost;
        s.deduped += q.deduped;
        s.gained += q.gained;
        for (k, v) in &q.removed_foreign_by_primary {
            s.removed_foreign += v;
            *s.removed_foreign_by_primary.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &q.foreign_primary_after_by_primary {
            s.foreign_primary_after += v;
            *s.foreign_primary_after_by_primary
                .entry(k.clone())
                .or_default() += v;
        }
        for shift in &q.shifts {
            for k in &shift.annotations_added {
                *s.annotations_added.entry(k.clone()).or_default() += 1;
            }
            for k in &shift.annotations_removed {
                *s.annotations_removed.entry(k.clone()).or_default() += 1;
            }
        }
        for k in &q.warnings_added {
            *s.warnings_added.entry(k.clone()).or_default() += 1;
        }
        for k in &q.warnings_removed {
            *s.warnings_removed.entry(k.clone()).or_default() += 1;
        }
        if !(q.warnings_added.is_empty()
            && q.warnings_removed.is_empty()
            && q.response_annotations_added.is_empty()
            && q.response_annotations_removed.is_empty())
        {
            s.queries_with_changed_warnings += 1;
        }
    }
    s.common = all_shifts.len();
    for (b, a) in all_shifts {
        let abs = b.abs_diff(*a);
        if abs > 0 {
            s.moved += 1;
        }
        s.max_abs_shift = s.max_abs_shift.max(abs);
    }
    s.recall_at_k =
        (s.before_in_scope_at_k > 0).then(|| s.overlap_at_k as f64 / s.before_in_scope_at_k as f64);
    s.mean_recall_at_k =
        (!recalls.is_empty()).then(|| recalls.iter().sum::<f64>() / recalls.len() as f64);
    s.mean_abs_shift = (!all_shifts.is_empty()).then(|| {
        all_shifts
            .iter()
            .map(|(b, a)| b.abs_diff(*a))
            .sum::<usize>() as f64
            / all_shifts.len() as f64
    });
    s
}

/// Everything [`verify_split`] reads.
pub struct VerifyInputs<'a> {
    /// The immutable source session.
    pub source: GraphSnapshot,
    /// The plan the split was made from.
    pub plan: &'a ClassificationPlan,
    /// The split rebuilt from the source and plan ([`super::build_split`]):
    /// its placements map every target id back to its origin.
    pub split: &'a SplitOutput,
    /// Every target session as the split output stores it, already checked
    /// against `split` (receipts and target digests).
    pub targets: BTreeMap<String, GraphSnapshot>,
    /// The serving `[serve]` table: its pinned sessions, default and layers.
    pub serve: &'a ServeConfig,
    pub queries: &'a VerifyQueries,
    /// Sessions that appear only as counts and may not be queried or read
    /// as a layer (an excluded project such as an employer's docs).
    pub aggregate_only: &'a BTreeSet<String>,
    pub params: VerifyParams,
    pub embedder: &'a dyn Embedder,
    pub embedding: &'a EmbeddingContract,
    pub config: &'a Config,
}

/// The pinned sessions an HTTP serve of `serve` runs with no `--session`,
/// its default, and the layer stacks it resolves.
fn serving(serve: &ServeConfig) -> Result<(Vec<String>, String, ServeLayers), VerifyError> {
    let pinned = crate::mcp::serve::pin_sessions(&[], serve, crate::mcp::serve::Transport::Http)
        .map_err(|e| VerifyError::Refused(format!("the serving [serve] table: {e}")))?;
    let layers = serve
        .resolve_layers(&pinned.sessions)
        .map_err(|e| VerifyError::Refused(format!("the serving [serve] table: {e}")))?;
    Ok((pinned.sessions, pinned.default, layers))
}

/// Run the before/after comparison. Deterministic: the same inputs give
/// the same report. Refused when the source, plan and split disagree, when
/// a target is missing or does not match the split, when a session's
/// embedding contract is not the embedder's, when a queried project is not
/// a pinned split target or reads an aggregate-only or missing layer, when
/// a query does not embed, or when a hit cannot be mapped to its origin.
pub async fn verify_split(inputs: VerifyInputs<'_>) -> Result<VerifyReport, VerifyError> {
    let VerifyInputs {
        source,
        plan,
        split,
        targets,
        serve,
        queries,
        aggregate_only,
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
    let source_sha = super::plan::source_binding(&source)
        .map_err(|e| VerifyError::Refused(e.to_string()))?
        .sha256;
    if source_sha != split.source_sha256
        || plan.source.sha256 != split.source_sha256
        || plan.plan_sha256 != split.plan_sha256
    {
        return refuse("the source, plan and split digests disagree");
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
    for name in aggregate_only {
        if !targets.contains_key(name) {
            return refuse(format!(
                "aggregate-only session {name} is not a target of this split"
            ));
        }
    }
    let (pinned, default_session, layers) = serving(serve)?;
    let mut disclosed: BTreeSet<String> = BTreeSet::new();
    for project in queries.queries.keys() {
        if aggregate_only.contains(project) {
            return refuse(format!(
                "{project} is aggregate-only: it appears only as counts and cannot be queried"
            ));
        }
        if !targets.contains_key(project) {
            return refuse(format!(
                "the query set names {project}, which is not a target of this split (the split \
                 wrote no rows for it)"
            ));
        }
        if !pinned.contains(project) {
            return refuse(format!(
                "{project} is not pinned by the serving [serve] table, so a serve would attach \
                 it on demand, unlayered; pin it in [serve] sessions or drop it from the query \
                 set"
            ));
        }
        disclosed.insert(project.clone());
        for spec in layers.stack(project) {
            if aggregate_only.contains(&spec.session) {
                return refuse(format!(
                    "{project} reads aggregate-only {} as a layer",
                    spec.session
                ));
            }
            if !targets.contains_key(&spec.session) {
                return refuse(format!(
                    "{project} is layered with {}, which is not a target of this split (the \
                     split wrote no rows for it)",
                    spec.session
                ));
            }
            disclosed.insert(spec.session.clone());
        }
    }

    let origins = Origins::new(split, plan);
    let ctx = Ctx { embedding, config };
    let at = params.at;
    let before_replay = Replay::new(source, config, embedding, at)?;
    let mut replays: BTreeMap<String, Replay> = BTreeMap::new();
    for (name, snapshot) in targets {
        if disclosed.contains(&name) {
            let replay = Replay::new(snapshot, config, embedding, at)?;
            replays.insert(name, replay);
        }
    }

    let mut projects = BTreeMap::new();
    let mut per_query = BTreeMap::new();
    let mut layer_knobs = BTreeMap::new();
    for (project, list) in &queries.queries {
        let stack = layers.stack(project);
        let stack_replays: Vec<(&Replay, &LayerSpec)> = stack
            .iter()
            .map(|spec| (&replays[&spec.session], spec))
            .collect();
        if !stack.is_empty() {
            layer_knobs.insert(
                project.clone(),
                stack
                    .iter()
                    .map(|l| {
                        let floor = crate::embed::calibration::resolve_layer_floor(
                            l.configured_min_similarity(),
                            config,
                            embedding,
                        );
                        (l.session.clone(), l.weight, floor)
                    })
                    .collect::<Vec<_>>(),
            );
        }
        let primary = &replays[project];
        let mut visible: BTreeSet<String> = BTreeSet::from([project.clone()]);
        visible.extend(stack.iter().map(|l| l.session.clone()));
        let scope = Scope {
            visible: &visible,
            disclosed: &disclosed,
            k: params.top_k,
        };
        let mut rows = Vec::with_capacity(list.len());
        let mut all_shifts = Vec::new();
        let (mut hub_hits, mut by_session) = (0, BTreeMap::new());
        let (mut before_hits, mut after_hits) = (0, 0);
        for (n, text) in list.iter().enumerate() {
            // Embedded once, shared by both sides. A failure would turn one
            // side keyword-only and make the comparison meaningless.
            let vector = embedder.embed_query(text).await.map_err(|e| {
                VerifyError::Refused(format!(
                    "query {} of {project} did not embed ({e}); nothing is reported",
                    n + 1
                ))
            })?;
            let before_recall = plain(&before_replay, text, &vector, &params, &ctx).await;
            let after_recall =
                layered_recall(primary, &stack_replays, text, &vector, &params, &ctx).await;
            let before = side_hits(&before_recall, &origins, true)?;
            let after = side_hits(&after_recall, &origins, false)?;
            before_hits += before.len();
            after_hits += after.len();
            for h in &after {
                *by_session.entry(h.session.clone()).or_insert(0) += 1;
                if origins.hubs.contains(&h.origin) {
                    hub_hits += 1;
                }
            }
            let mut seen = HashSet::new();
            for h in &before {
                if seen.insert(h.origin)
                    && let Some(a) = after.iter().find(|a| a.origin == h.origin)
                {
                    all_shifts.push((h.rank, a.rank));
                }
            }
            rows.push(compare(
                text,
                &before_recall,
                &after_recall,
                &before,
                &after,
                &scope,
                &origins,
            ));
        }
        let layer_names = stack.iter().map(|l| l.session.clone()).collect();
        projects.insert(
            project.clone(),
            summarize(
                layer_names,
                &rows,
                &all_shifts,
                hub_hits,
                by_session,
                before_hits,
                after_hits,
            ),
        );
        per_query.insert(project.clone(), rows);
    }
    Ok(VerifyReport {
        report_version: VERIFY_REPORT_VERSION,
        measures: MEASURES.to_owned(),
        source_session: split.source_session.clone(),
        source_sha256: split.source_sha256.clone(),
        plan_sha256: split.plan_sha256.clone(),
        split_sha256: split.split_sha256.clone(),
        split_algorithm_version: split.algorithm_version,
        embedding: embedding.clone(),
        params,
        pinned,
        default_session,
        aggregate_only: aggregate_only.iter().cloned().collect(),
        layer_knobs,
        projects,
        queries: per_query,
    })
}

#[cfg(all(test, feature = "embed-fixture"))]
#[path = "verify_tests.rs"]
mod tests;
