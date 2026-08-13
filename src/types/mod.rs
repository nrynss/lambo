//! Core Lambo types (P1 contracts — frozen after T1.1).
//!
//! Spec: lambo-hackathon-spec-v0.1.md §§3–6, graph model §5.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// Graph node id — store issues UUIDs (no arena / generational indices in v0.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub Uuid);

impl NodeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Nil UUID — safe `Default` (unlike random, which would be a footgun).
    pub const fn nil() -> Self {
        Self(Uuid::nil())
    }

    pub fn is_nil(self) -> bool {
        self.0.is_nil()
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::nil()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Uuid> for NodeId {
    fn from(u: Uuid) -> Self {
        Self(u)
    }
}

/// Session id (string key in durable store).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Agent identity within a session (caller-supplied, unauthenticated).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for AgentId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Concept classification (spec §5). Eviction resistance & score multipliers from v0.6.0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ConceptType {
    Entity,
    Logic,
    Constraint,
    Resource,
    Observation,
}

impl ConceptType {
    /// Relative resistance to GC eviction (higher = stickier). From v0.6.0 design.
    pub const fn eviction_resistance(self) -> f64 {
        match self {
            Self::Constraint => 1.5,
            Self::Entity => 1.2,
            Self::Logic => 1.1,
            Self::Resource => 1.0,
            Self::Observation => 0.7,
        }
    }

    /// Multiplier applied to daemon composite score.
    pub const fn score_multiplier(self) -> f64 {
        match self {
            Self::Constraint => 1.15,
            Self::Entity => 1.05,
            Self::Logic => 1.05,
            Self::Resource => 1.0,
            Self::Observation => 0.9,
        }
    }
}

/// Edge types retained in v0.1 (spec §5). Seven of nine; CrossOccurrence/Fixes cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum EdgeType {
    Temporal,
    Derives,
    CoOccurrence,
    Causal,
    Dependency,
    Hierarchical,
    Semantic,
}

impl EdgeType {
    /// Whether this edge type participates in weight decay (spec §5 table).
    pub const fn decays(self) -> bool {
        matches!(self, Self::CoOccurrence | Self::Semantic)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum CanonizationStatus {
    #[default]
    None,
    Candidate,
    Venerable,
    Canonical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum MatchStrategy {
    #[default]
    Canonical,
    Hybrid,
}

// ---------------------------------------------------------------------------
// Nodes & edges
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    pub id: NodeId,
    pub session_id: SessionId,
    pub agent_id: AgentId,
    pub prompt_text: Option<String>,
    pub previous_id: Option<NodeId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Concept {
    pub id: NodeId,
    pub session_id: SessionId,
    pub content: String,
    pub canonical_key: String,
    pub concept_type: ConceptType,
    pub origin_interaction: NodeId,
    pub origin_agent: AgentId,
    pub created_at: DateTime<Utc>,
    pub access_count: i32,
    pub last_accessed: Option<DateTime<Utc>>,
    pub gc_survived: i32,
    pub canonization_status: CanonizationStatus,
    pub blast_radius: Option<i32>,
    pub last_demotion_time: Option<DateTime<Utc>>,
    /// Dense embedding when present (width = session [`EmbeddingContract::dim`]).
    pub embedding: Option<Vec<f32>>,
    /// Demoted-chunk group id (T2.5): Observations from one context-overflow
    /// chunk share this id for sibling co-retrieval (spec §7, §8; read by T5.2).
    /// Added post-T1.1 per the P5 doc ("T2.5's field") — serde-defaulted so
    /// existing fixture JSON loads unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_group_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Interaction(Interaction),
    Concept(Concept),
}

impl Node {
    pub fn id(&self) -> NodeId {
        match self {
            Self::Interaction(i) => i.id,
            Self::Concept(c) => c.id,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::Interaction(i) => &i.session_id,
            Self::Concept(c) => &c.session_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: NodeId,
    pub session_id: SessionId,
    pub source: NodeId,
    pub target: NodeId,
    pub edge_type: EdgeType,
    pub weight: f64,
    pub reinforcements: i32,
    pub created_at: DateTime<Utc>,
    pub last_reinforced: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Mutations / snapshot
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Mutation {
    /// Insert or update a node.
    ///
    /// **R2-1 — canonization columns are not this variant's to write.** On an
    /// **existing** concept row every adapter leaves `canonization_status`,
    /// `blast_radius` and `last_demotion_time` exactly as they are; only
    /// [`Mutation::CanonizationTransition`] and
    /// [`crate::store::GraphStore::record_canonization`] move them (the
    /// initial INSERT of a brand-new row still carries the node's values, so
    /// a concept born mid-progression persists correctly).
    ///
    /// Single-writer, because the alternative is a lost update with no
    /// repair: this variant carries a **snapshot of the concept taken when
    /// the mutation was appended**, and appenders that care nothing about
    /// canonization append it — `bump_gc_survived` (T4.5 GC) is the common
    /// one. A GC bump at `T`, a hop at `T+1`, and the flush at `T+2` puts
    /// `[UpsertNode(stale), CanonizationTransition(hop)]` in one batch; the
    /// transition is already recorded (the evaluator writes it immediately
    /// via `record_canonization`), so its replay is a documented no-op, and
    /// the stale upsert's write of the three columns would stand as the
    /// durable state. Status regresses; worse, a demoted node reloads
    /// `Canonical` with `last_demotion_time` erased — the re-promotion
    /// cooldown gone (COH-3, "cooldown survives restart").
    ///
    /// Excluding the columns here rather than making the transition's UPDATE
    /// monotonic is what makes the replay no-op's premise ("the effect is
    /// already in the row") **true**: with one writer, nothing else can take
    /// it back out. A monotonic UPDATE would repair only the batches that
    /// happen to carry the transition behind the stale upsert, and leave the
    /// row wrong (durably, across a crash) whenever the two land in different
    /// flushes.
    UpsertNode {
        node: Node,
    },
    UpsertEdge {
        edge: Edge,
    },
    DeleteNode {
        id: NodeId,
    },
    DeleteEdge {
        id: NodeId,
    },
    CanonizationTransition {
        event: CanonizationEvent,
    },
    /// The session's `root_goal` changed (XP-8). `None` clears it.
    ///
    /// Session-level metadata otherwise reaches a store only through the
    /// full-snapshot `seed` path, so before this variant a reload replayed an
    /// **empty** goal: drift detection silently stopped (no goal nodes → no
    /// hits) and GC's root-goal exclusion emptied, leaving auto-`Venerable` as
    /// the only surviving protection. Both SQL schemas already carry
    /// `sessions.root_goal`, so applying this is an `UPDATE` of a column that
    /// exists — the JSON encoding matches `seed`'s exactly.
    SetRootGoal {
        session_id: SessionId,
        goal: Option<serde_json::Value>,
    },
    /// The session's active dense embedding space changed. `None` clears it
    /// only as part of an explicit re-embedding workflow.
    ///
    /// This is ordered with concept/vector mutations so a normal write-behind
    /// flush cannot durably store vectors while losing the contract that makes
    /// those vectors interpretable after restart.
    SetEmbedding {
        session_id: SessionId,
        embedding: Option<EmbeddingContract>,
    },
}

/// Ordered write-behind unit (spec §2.4). Apply nodes → edges → deletions → transitions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct MutationBatch {
    pub mutations: Vec<Mutation>,
}

impl MutationBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, m: Mutation) {
        self.mutations.push(m);
    }

    pub fn is_empty(&self) -> bool {
        self.mutations.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mutations.len()
    }
}

/// Full session materialization for `load_session`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct GraphSnapshot {
    pub session_id: SessionId,
    pub root_goal: Option<serde_json::Value>,
    pub created_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub interactions: Vec<Interaction>,
    pub concepts: Vec<Concept>,
    pub edges: Vec<Edge>,
    pub synonyms: Vec<Synonym>,
    pub reservations: Vec<Reservation>,
    pub canonization_events: Vec<CanonizationEvent>,
    /// Active dense-embedding space for this session (kind/model/dim).
    /// Set on first embed; refuse swaps without re-embed (see `EmbeddingContract`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingContract>,
}

/// Identity of the dense embedding space used in a session.
///
/// Same dim does **not** mean interchangeable models. Stamp this on
/// [`GraphSnapshot::embedding`] and call [`EmbeddingContract::ensure_compatible`]
/// before any hybrid/vector write when a contract already exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingContract {
    /// Embedder kind string (`bge_m3`, `bedrock`, `fixture`, …).
    pub kind: String,
    /// Optional model id / GGUF name (empty = server default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Dense vector width this session's stored embeddings use.
    pub dim: usize,
}

impl EmbeddingContract {
    /// Error if `other` would mix embedding spaces (kind, model, or dim differ).
    pub fn ensure_compatible(&self, other: &Self) -> Result<(), LamboError> {
        if self.dim != other.dim {
            return Err(LamboError::Config(format!(
                "session embedding dim {} != live embedder dim {} (re-embed or new session)",
                self.dim, other.dim
            )));
        }
        if self.kind != other.kind {
            return Err(LamboError::Config(format!(
                "session embedder kind {:?} != live {:?} — vectors are not interchangeable \
                 (re-embed or new session)",
                self.kind, other.kind
            )));
        }
        let a = self.model.as_deref().unwrap_or("");
        let b = other.model.as_deref().unwrap_or("");
        if a != b {
            return Err(LamboError::Config(format!(
                "session embedder model {a:?} != live {b:?} — re-embed or new session"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synonym {
    pub session_id: SessionId,
    pub source_key: String,
    pub canonical_key: String,
}

// ---------------------------------------------------------------------------
// Daemon / recall / canonization
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonEvent {
    Conflict {
        node_id: NodeId,
        agents: Vec<AgentId>,
        detail: String,
    },
    Drift {
        node_id: NodeId,
        /// Shortest-path hop count to the nearest root goal — **or the no-path
        /// sentinel** `4294967295`
        /// ([`crate::daemon::drift::DRIFT_HOPS_NO_PATH_EVENT`], ALGO-5/NEW-5).
        /// `u32` is frozen by spec §6.1 and has no unreachable encoding, so a
        /// concept with no traversable route to any goal — the maximally drifted
        /// case — reports the sentinel. This enum derives `Serialize`: a JSON
        /// consumer sees the literal `4294967295` and must read it as "no path",
        /// never as a distance. `detail` says so in words.
        hops: u32,
        detail: String,
    },
    Stale {
        node_id: NodeId,
        detail: String,
    },
    HighRisk {
        node_id: NodeId,
        detail: String,
    },
    Canonized {
        event: CanonizationEvent,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallQuery {
    pub query: String,
    pub top_k: usize,
    pub max_tokens: usize,
    pub traversal_depth: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallHit {
    pub node_id: NodeId,
    pub content: String,
    pub concept_type: Option<ConceptType>,
    pub score: f64,
    pub is_canonical: bool,
    pub blast_radius: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallResult {
    pub hits: Vec<RecallHit>,
    pub context: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scored<T> {
    pub item: T,
    pub score: f64,
}

impl<T> Scored<T> {
    pub fn new(item: T, score: f64) -> Self {
        Self { item, score }
    }
}

/// Stage 2 structural evidence (spec §4.1 / §10).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct InteractionSpan {
    pub distinct: u64,
    pub coverage: f64,
}

/// One audited canonization transition (spec §10 / §13 — the demo queries this
/// table on camera).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonizationEvent {
    pub id: NodeId,
    pub session_id: SessionId,
    pub node_id: NodeId,
    pub from_status: CanonizationStatus,
    pub to_status: CanonizationStatus,
    pub blast_radius: Option<i32>,
    /// The concept's new `last_demotion_time` when this event is a demotion
    /// (`Canonical -> None`); `None` for every non-demotion transition, which
    /// must leave the concept's value untouched (adve-review COH-3). Spec §10:
    /// "Demotion sets `last_demotion_time`" — T6.3 cooldown / T6.4 read it.
    /// Serde-defaulted and skipped when absent so existing fixture JSON loads
    /// unchanged (verified: no committed fixture carries a demotion event).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_demotion_time: Option<DateTime<Utc>>,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub session_id: SessionId,
    pub node_id: NodeId,
    pub agent_id: AgentId,
    pub expires_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("capability not supported: {0}")]
    Capability(String),
    #[error("invariant violated: {0}")]
    Invariant(String),
    #[error("backend: {0}")]
    Backend(String),
    /// Deterministic constraint violation (Postgres SQLSTATE 23xxx /
    /// SQLite `SQLITE_CONSTRAINT`), carrying the SQLSTATE / extended code.
    /// Terminal: replaying the batch can never succeed (STORE-4 / D5), so the
    /// flush loop dead-letters it instead of retrying.
    #[error("constraint violated ({0})")]
    Constraint(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl StoreError {
    /// STORE-4: retryability for the flush loop. A constraint violation is
    /// deterministic — retrying it can never succeed and only blocks the
    /// queue (head-of-line) — so it is non-retryable and dead-lettered
    /// (D5: drop-after-log, visible in `FlushStats::dead_lettered`). Every
    /// other error keeps the existing retry / retain semantics.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, StoreError::Constraint(_))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LamboError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("embed: {0}")]
    Embed(String),
    #[error("config: {0}")]
    Config(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn concept_type_json_roundtrip() {
        for ct in [
            ConceptType::Entity,
            ConceptType::Logic,
            ConceptType::Constraint,
            ConceptType::Resource,
            ConceptType::Observation,
        ] {
            let s = serde_json::to_string(&ct).unwrap();
            let back: ConceptType = serde_json::from_str(&s).unwrap();
            assert_eq!(ct, back);
        }
    }

    #[test]
    fn edge_type_decay_table() {
        assert!(!EdgeType::Temporal.decays());
        assert!(!EdgeType::Derives.decays());
        assert!(EdgeType::CoOccurrence.decays());
        assert!(!EdgeType::Causal.decays());
        assert!(!EdgeType::Dependency.decays());
        assert!(!EdgeType::Hierarchical.decays());
        assert!(EdgeType::Semantic.decays());
    }

    #[test]
    fn node_json_roundtrip() {
        let id = NodeId::new();
        let sid = SessionId::from("s1");
        let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        let c = Concept {
            id,
            session_id: sid.clone(),
            content: "user schema".into(),
            canonical_key: "schema user".into(),
            concept_type: ConceptType::Entity,
            origin_interaction: NodeId::new(),
            origin_agent: AgentId::from("agent-A"),
            created_at: ts,
            access_count: 0,
            last_accessed: None,
            gc_survived: 3,
            canonization_status: CanonizationStatus::Candidate,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        };
        let node = Node::Concept(c.clone());
        let s = serde_json::to_string(&node).unwrap();
        let back: Node = serde_json::from_str(&s).unwrap();
        assert_eq!(node, back);
        assert_eq!(back.id(), id);
    }

    #[test]
    fn mutation_batch_json_roundtrip() {
        let batch = MutationBatch {
            mutations: vec![Mutation::DeleteNode { id: NodeId::new() }],
        };
        let s = serde_json::to_string(&batch).unwrap();
        let back: MutationBatch = serde_json::from_str(&s).unwrap();
        assert_eq!(batch, back);
    }

    #[test]
    fn node_id_default_is_nil_not_random() {
        assert!(NodeId::default().is_nil());
        assert_ne!(NodeId::new(), NodeId::new());
    }

    #[test]
    fn all_edge_types_serde() {
        for et in [
            EdgeType::Temporal,
            EdgeType::Derives,
            EdgeType::CoOccurrence,
            EdgeType::Causal,
            EdgeType::Dependency,
            EdgeType::Hierarchical,
            EdgeType::Semantic,
        ] {
            let s = serde_json::to_string(&et).unwrap();
            let back: EdgeType = serde_json::from_str(&s).unwrap();
            assert_eq!(et, back);
        }
    }
}
