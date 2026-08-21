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
    /// Mint a fresh random id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Nil UUID — safe `Default` (unlike random, which would be a footgun).
    pub const fn nil() -> Self {
        Self(Uuid::nil())
    }

    /// Is this the nil id (the `Default`)?
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
    /// Wrap a session key.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the session key as a string.
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
    /// Wrap an agent name.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Borrow the agent name as a string.
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
    /// A thing the work is about: a file, a service, a schema.
    Entity,
    /// A rule of how something behaves, or a decision and its reasoning.
    Logic,
    /// A requirement the work must keep satisfying.
    Constraint,
    /// An artifact an agent produced or touched.
    Resource,
    /// Something an agent noticed. The weakest kind, and the first evicted.
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
    /// One interaction followed another in time.
    Temporal,
    /// An interaction produced this concept.
    Derives,
    /// Two concepts were written by the same interaction. Decays.
    CoOccurrence,
    /// One thing caused or produced another.
    Causal,
    /// One thing needs another to be true or present.
    Dependency,
    /// Parent to child, from `parent_of` on a derive.
    Hierarchical,
    /// Two concepts are close in embedding space. Decays.
    Semantic,
}

impl EdgeType {
    /// Whether this edge type participates in weight decay (spec §5 table).
    pub const fn decays(self) -> bool {
        matches!(self, Self::CoOccurrence | Self::Semantic)
    }
}

/// How far a concept has travelled toward becoming a canonical fact.
///
/// Concepts climb by structural evidence, never by an agent declaring one
/// important. See [`CanonizationEvent`] for the audit trail of each move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum CanonizationStatus {
    /// Ordinary memory. The starting point, and where a demotion lands.
    #[default]
    None,
    /// Enough evidence to be worth evaluating.
    Candidate,
    /// Strong evidence, not yet promoted.
    Venerable,
    /// A canonical fact for this session.
    Canonical,
}

/// Which concepts a recall is allowed to match — **and, on the write path,
/// whether a derive embeds.**
///
/// The second half was undocumented here and it is the half with the
/// availability consequence (J3 round-1 F3, found by that finding's own register
/// sweep). `crate::graph::hybrid::derive` is selected by this setting, so
/// `Hybrid` means a new concept is stored **with its embedding or not at all**:
/// an embedder that is unreachable, refusing or too slow fails the write rather
/// than applying a concept semantic recall could never find. `Canonical` is the
/// declared keyword-only mode (spec §3.2) and needs no embedder to write.
///
/// **Note the two defaults are not the same**, deliberately and confusingly: the
/// `Default` impl here is `Canonical` (the conservative choice for a
/// programmatically built value), while `Config::default().match_strategy` is
/// `Hybrid` — the product default a `lambo serve` runs under. Read the config,
/// not this attribute, when asking what a deployment does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum MatchStrategy {
    /// Recall matches canonical facts only; a derive writes keyword-only
    /// concepts and never calls the embedder.
    #[default]
    Canonical,
    /// Recall matches canonical facts and ordinary memory together; a derive
    /// embeds, and fails if it cannot.
    Hybrid,
}

// ---------------------------------------------------------------------------
// Nodes & edges
// ---------------------------------------------------------------------------

/// One turn of agent work: the unit that concepts are derived from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interaction {
    /// This interaction's node id.
    pub id: NodeId,
    /// The session this interaction belongs to.
    pub session_id: SessionId,
    /// The agent that did the work.
    pub agent_id: AgentId,
    /// The prompt text, when the caller supplied one.
    pub prompt_text: Option<String>,
    /// The previous interaction in this session, forming the temporal chain.
    pub previous_id: Option<NodeId>,
    /// When Lambo recorded this. Lambo stamps it, never the caller.
    pub created_at: DateTime<Utc>,
}

/// One piece of remembered meaning, and the node type recall returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Concept {
    /// This concept's node id.
    pub id: NodeId,
    /// The session this concept belongs to.
    pub session_id: SessionId,
    /// The text as it was written, kept verbatim.
    pub content: String,
    /// The normalized form used to decide whether two concepts are the same.
    ///
    /// Stemmed, lowercased, and stripped of invisible characters, so texts that
    /// read identically collapse to one key and cannot become duplicates.
    pub canonical_key: String,
    /// How this concept is classified.
    pub concept_type: ConceptType,
    /// The interaction that produced this concept.
    pub origin_interaction: NodeId,
    /// The agent that wrote it.
    pub origin_agent: AgentId,
    /// When Lambo recorded this.
    pub created_at: DateTime<Utc>,
    /// How many times recall has returned this concept.
    pub access_count: i32,
    /// When recall last returned it, if ever.
    pub last_accessed: Option<DateTime<Utc>>,
    /// How many garbage-collection sweeps this concept has survived.
    pub gc_survived: i32,
    /// How far along the path to canonical this concept is.
    pub canonization_status: CanonizationStatus,
    /// How many concepts depend on this one. `None` until it is computed.
    pub blast_radius: Option<i32>,
    /// When this concept was last demoted from canonical, if ever.
    ///
    /// Drives the cooldown that stops a demoted concept from being re-promoted
    /// immediately.
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

/// Either kind of graph node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    /// A unit of agent work.
    Interaction(Interaction),
    /// A piece of remembered meaning.
    Concept(Concept),
}

impl Node {
    /// This node's id, whichever kind it is.
    pub fn id(&self) -> NodeId {
        match self {
            Self::Interaction(i) => i.id,
            Self::Concept(c) => c.id,
        }
    }

    /// The session this node belongs to, whichever kind it is.
    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::Interaction(i) => &i.session_id,
            Self::Concept(c) => &c.session_id,
        }
    }
}

/// A typed, weighted link between two nodes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// This edge's own id.
    pub id: NodeId,
    /// The session this edge belongs to.
    pub session_id: SessionId,
    /// The node the edge leads from.
    pub source: NodeId,
    /// The node the edge leads to.
    pub target: NodeId,
    /// What kind of relationship this edge records.
    pub edge_type: EdgeType,
    /// Current strength. Decaying edge types lose weight over time.
    pub weight: f64,
    /// How many times this edge has been observed again and strengthened.
    pub reinforcements: i32,
    /// When the edge was first written.
    pub created_at: DateTime<Utc>,
    /// When it was last strengthened.
    pub last_reinforced: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Mutations / snapshot
// ---------------------------------------------------------------------------

/// One durable change to the graph, as written to the write-behind log.
///
/// A batch of these is replayed in submission order, so a mutation may rely
/// on the effect of every mutation appended before it.
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
        /// The node to insert or update.
        node: Node,
    },
    /// Insert or update an edge.
    UpsertEdge {
        /// The edge to insert or update.
        edge: Edge,
    },
    /// Remove a node and its incident edges.
    DeleteNode {
        /// The node to remove.
        id: NodeId,
    },
    /// Remove one edge.
    DeleteEdge {
        /// The edge to remove.
        id: NodeId,
    },
    /// Record one audited canonization move.
    CanonizationTransition {
        /// The transition to record.
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
        /// The session whose goal changed.
        session_id: SessionId,
        /// The new goal, or `None` to clear it.
        goal: Option<serde_json::Value>,
    },
    /// The session's active dense embedding space changed. `None` clears it
    /// only as part of an explicit re-embedding workflow.
    ///
    /// This is ordered with concept/vector mutations so a normal write-behind
    /// flush cannot durably store vectors while losing the contract that makes
    /// those vectors interpretable after restart.
    SetEmbedding {
        /// The session whose embedding space changed.
        session_id: SessionId,
        /// The new embedding space, or `None` to clear it.
        embedding: Option<EmbeddingContract>,
    },
    /// A validated, acked background write that has not yet been applied
    /// (J3 durable intents — `dev-diary/lambo-for-mooshik/J3-durability-redesign.md`).
    ///
    /// Appended at **ack** time by the write pipeline, through this same
    /// write-behind log, so the C-series "session closed, tail durable"
    /// guarantee carries it: at a clean close, every acked write is either
    /// applied or durable as an intent — **by construction**, independent of
    /// any drain estimate being right. The next serve of the session replays
    /// unconsumed intents (idempotent per receipt id, per-lane order
    /// preserved). A `kill -9` loses unflushed intents exactly as it loses the
    /// rest of the write-behind tail; receipts stay honest (`restart_lost`).
    PutWriteIntent {
        /// The intent record.
        intent: WriteIntent,
    },
    /// The intent was applied (or attempted and failed) — appended **in the
    /// same graph-lock critical section as the applied mutations**, so one
    /// flush transaction carries both and a crash can never leave the applied
    /// write durable beside a still-unconsumed intent (the double-apply this
    /// design excludes). Adapters retain the consumed row, with its outcome,
    /// for the receipt-retention window so a restarted session can answer
    /// `applied_after_restart`; rows older than that are purged.
    ConsumeWriteIntent {
        /// The session the intent belongs to.
        session_id: SessionId,
        /// The receipt id, in its display form — the intent's primary key.
        receipt: String,
        /// What became of the write.
        outcome: WriteIntentOutcome,
    },
}

/// The payload of a [`WriteIntent`] — the validated job, exactly as acked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WriteIntentPayload {
    /// A `lambo_derive` job.
    Derive {
        /// `(content, concept_type)` pairs, in submission order.
        concepts: Vec<(String, ConceptType)>,
        /// `(parent, child)` hierarchy pairs, in submission order.
        pairs: Vec<(String, String)>,
    },
    /// A `lambo_record_action` job.
    Action {
        /// The action sentence.
        action: String,
        /// Concept contents this action produces.
        produces: Vec<String>,
        /// Concept contents this action modifies.
        modifies: Vec<String>,
        /// Concept contents this action depends on.
        depends_on: Vec<String>,
    },
}

/// A durable post-validation write intent (J3). See
/// [`Mutation::PutWriteIntent`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WriteIntent {
    /// The session the write was acked into.
    pub session_id: SessionId,
    /// The receipt id the caller holds, in display form — the primary key, and
    /// what makes replay idempotent.
    pub receipt: String,
    /// The agent whose lane the write was queued on. Replay preserves order
    /// **within** an agent's intents (the lane promise), and receipts stay
    /// agent-scoped across restart.
    pub agent: AgentId,
    /// The interaction the write hangs from. Durable by the same close that
    /// made this intent durable (the call path writes the interaction to the
    /// log before the ack).
    pub interaction: NodeId,
    /// The receipt's sequence number — strictly increasing per issuing
    /// process, so sorting by (`issued_ms`, `lane_seq`) replays one process's
    /// intents in exact admission order and multiple crashed processes'
    /// intents in wall-clock order.
    pub lane_seq: u64,
    /// The receipt's issue timestamp (epoch millis), for cross-process
    /// ordering.
    pub issued_ms: i64,
    /// The validated job.
    pub payload: WriteIntentPayload,
    /// When the intent was recorded (ack time, process clock).
    pub created_at: DateTime<Utc>,
    /// `None` while unconsumed (replay is owed); `Some` once applied or
    /// failed. Consumed rows are retained for the receipt-retention window so
    /// the answer survives the restart, then purged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<WriteIntentOutcome>,
}

/// How long a **consumed** write intent row is retained before an adapter may
/// purge it — the cross-restart receipt window: a consumed intent is the only
/// durable carrier of "your write applied after a restart", so it lives as
/// long as an in-RAM receipt would ([`crate::writeq::RECEIPT_RETENTION`] is
/// const-asserted equal to this).
///
/// **Two mechanisms, stated precisely (J3 round-1 F2 corrected the second).**
///
/// * *Purging the row* is **lazy**: it happens inside the adapters' consume
///   step, clocked by that mutation's own `consumed_at`, so no adapter needs a
///   clock. A session that goes quiet after a burst therefore **keeps** its
///   consumed rows — there is no sweeper — until its next consume. They are
///   small and bounded by the queue cap; nothing reads them.
/// * *Answering from the row* is bounded: the replay's seeding step skips a
///   consumed row older than this window, so it answers `restart_lost` rather
///   than `applied_after_restart`. Without that skip a stale row answered
///   **better** for having survived a restart than the same id would have in a
///   process that never restarted (`expired`) — the exact asymmetry the equality
///   assert above exists to forbid, pointing the other way.
///
/// This docstring used to end "and expired rows are skipped at load". No such
/// filter existed in either adapter: both load with an unfiltered
/// `SELECT … WHERE session_id = ? ORDER BY issued_ms, lane_seq`, and nothing
/// between there and the replay filtered by age. The skip is real now, and it is
/// at the replay, not at the load.
pub const WRITE_INTENT_RETENTION: std::time::Duration = std::time::Duration::from_secs(300);

/// What became of a consumed [`WriteIntent`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WriteIntentOutcome {
    /// `"applied"`, `"applied_after_restart"`, or `"failed"` — the receipt
    /// answer tag the outcome renders as.
    pub tag: String,
    /// The human sentence the receipt carries (an applied summary, or the
    /// failure reason).
    pub summary: String,
    /// When the intent was consumed (process clock of the consumer). Doubles
    /// as the purge clock: adapters delete consumed rows older than the
    /// receipt-retention window.
    pub consumed_at: DateTime<Utc>,
}

/// Ordered write-behind unit (spec §2.4). Apply nodes → edges → deletions → transitions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct MutationBatch {
    /// The mutations, in the order they were appended.
    pub mutations: Vec<Mutation>,
}

impl MutationBatch {
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one mutation to the end of the batch.
    pub fn push(&mut self, m: Mutation) {
        self.mutations.push(m);
    }

    /// Does the batch carry no mutations?
    pub fn is_empty(&self) -> bool {
        self.mutations.is_empty()
    }

    /// How many mutations the batch carries.
    pub fn len(&self) -> usize {
        self.mutations.len()
    }
}

/// Full session materialization for `load_session`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct GraphSnapshot {
    /// The session this snapshot is of.
    pub session_id: SessionId,
    /// The session's root goal, when one is set.
    pub root_goal: Option<serde_json::Value>,
    /// When the session was created.
    pub created_at: Option<DateTime<Utc>>,
    /// When the session was closed, if it has been.
    pub closed_at: Option<DateTime<Utc>>,
    /// Every interaction in the session.
    pub interactions: Vec<Interaction>,
    /// Every concept in the session.
    pub concepts: Vec<Concept>,
    /// Every edge in the session.
    pub edges: Vec<Edge>,
    /// Every recorded synonym.
    pub synonyms: Vec<Synonym>,
    /// Every reservation that was still live when the snapshot was taken.
    pub reservations: Vec<Reservation>,
    /// The session's canonization audit trail.
    pub canonization_events: Vec<CanonizationEvent>,
    /// Active dense-embedding space for this session (kind/model/dim).
    /// Set on first embed; refuse swaps without re-embed (see `EmbeddingContract`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingContract>,
    /// Durable write intents (J3): unconsumed ones are owed a replay by the
    /// loading process; consumed-and-retained ones answer
    /// `applied_after_restart` lookups. Expired consumed rows are not loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_intents: Vec<WriteIntent>,
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
    ///
    /// The message always **names the model the session's vectors were written
    /// with** (kind/model/dim), so the reader knows exactly which embedder
    /// produced the stored space and why the attach is refused — not a bare
    /// "incompatible" that leaves the operator guessing which model is right.
    pub fn ensure_compatible(&self, other: &Self) -> Result<(), LamboError> {
        if self.dim != other.dim || self.kind != other.kind || self.model != other.model {
            let sm = self.model.clone().unwrap_or_else(|| "(default)".into());
            let om = other.model.clone().unwrap_or_else(|| "(default)".into());
            return Err(LamboError::Config(format!(
                "embedding contract is incompatible: this session's vectors were written by \
                 kind={} model={:?} dim={}, but the live/attached embedder is kind={} model={:?} \
                 dim={} — re-embed or start a new session before writing or reading",
                self.kind, sm, self.dim, other.kind, om, other.dim
            )));
        }
        Ok(())
    }
}

/// A recorded equivalence between two canonical keys.
///
/// Lets recall treat different wordings of one idea as the same concept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synonym {
    /// The session this synonym applies to.
    pub session_id: SessionId,
    /// The key that is being redirected.
    pub source_key: String,
    /// The key it redirects to.
    pub canonical_key: String,
}

// ---------------------------------------------------------------------------
// Daemon / recall / canonization
// ---------------------------------------------------------------------------

/// Something the background daemon noticed and wants to report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonEvent {
    /// Two or more agents wrote the same node close together in time.
    Conflict {
        /// The contested node.
        node_id: NodeId,
        /// The agents that wrote it.
        agents: Vec<AgentId>,
        /// A human-readable explanation.
        detail: String,
    },
    /// A concept has drifted far from every root goal.
    Drift {
        /// The drifting concept.
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
        /// A human-readable explanation, including the no-path case in words.
        detail: String,
    },
    /// A concept has not been touched for long enough to be worth flagging.
    Stale {
        /// The stale concept.
        node_id: NodeId,
        /// A human-readable explanation.
        detail: String,
    },
    /// A write landed on a concept many others depend on.
    HighRisk {
        /// The load-bearing concept.
        node_id: NodeId,
        /// A human-readable explanation.
        detail: String,
    },
    /// A concept moved along the canonization path.
    Canonized {
        /// The transition that occurred.
        event: CanonizationEvent,
    },
}

/// What to recall, and how much of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallQuery {
    /// The text to recall against.
    pub query: String,
    /// How many hits to return at most.
    pub top_k: usize,
    /// The token budget for the rendered context block.
    pub max_tokens: usize,
    /// How many hops to expand through the graph around each hit.
    pub traversal_depth: usize,
}

/// One concept recall matched, with its score and its warnings' inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallHit {
    /// The matched concept.
    pub node_id: NodeId,
    /// Its text.
    pub content: String,
    /// Its classification, when the hit is a concept.
    pub concept_type: Option<ConceptType>,
    /// Its relevance to the query. Higher is more relevant.
    pub score: f64,
    /// Is this a canonical fact?
    pub is_canonical: bool,
    /// How many concepts depend on this one, when that is known.
    pub blast_radius: Option<u64>,
}

/// Everything one recall produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallResult {
    /// The individual hits, most relevant first.
    pub hits: Vec<RecallHit>,
    /// The rendered context block, ready to hand to a model.
    pub context: String,
    /// Anything the caller should know, such as a leg of the search that was skipped.
    pub warnings: Vec<String>,
}

/// A value paired with a score, used wherever candidates are ranked.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scored<T> {
    /// The scored value.
    pub item: T,
    /// Its score. Higher is better.
    pub score: f64,
}

impl<T> Scored<T> {
    /// Pair a value with its score.
    pub fn new(item: T, score: f64) -> Self {
        Self { item, score }
    }
}

/// Stage 2 structural evidence (spec §4.1 / §10).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct InteractionSpan {
    /// How many distinct interactions mention the concept.
    pub distinct: u64,
    /// What fraction of the session's interactions those are, from `0.0` to `1.0`.
    pub coverage: f64,
}

/// One audited canonization transition (spec §10 / §13 — the demo queries this
/// table on camera).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonizationEvent {
    /// This event's own id.
    pub id: NodeId,
    /// The session the transition happened in.
    pub session_id: SessionId,
    /// The concept that moved.
    pub node_id: NodeId,
    /// The status it moved from.
    pub from_status: CanonizationStatus,
    /// The status it moved to.
    pub to_status: CanonizationStatus,
    /// The concept's blast radius at the time, when it was known.
    pub blast_radius: Option<i32>,
    /// The concept's new `last_demotion_time` when this event is a demotion
    /// (`Canonical -> None`); `None` for every non-demotion transition, which
    /// must leave the concept's value untouched (adve-review COH-3). Spec §10:
    /// "Demotion sets `last_demotion_time`" — T6.3 cooldown / T6.4 read it.
    /// Serde-defaulted and skipped when absent so existing fixture JSON loads
    /// unchanged (verified: no committed fixture carries a demotion event).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_demotion_time: Option<DateTime<Utc>>,
    /// When the transition happened.
    pub occurred_at: DateTime<Utc>,
}

/// An advisory soft lock on one node, so two agents do not edit it at once.
///
/// Advisory and held in the writer's memory, so it does not survive a restart
/// and nothing enforces it against an agent that ignores it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    /// The session the reservation is in.
    pub session_id: SessionId,
    /// The reserved node.
    pub node_id: NodeId,
    /// The agent holding it.
    pub agent_id: AgentId,
    /// When the reservation lapses.
    pub expires_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure from the durable store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// No such session in the store.
    #[error("session not found: {0}")]
    SessionNotFound(String),
    /// The row or node asked for is not there.
    #[error("not found: {0}")]
    NotFound(String),
    /// This store does not offer the feature asked of it, such as vector search.
    #[error("capability not supported: {0}")]
    Capability(String),
    /// The store returned data that breaks an invariant Lambo relies on.
    #[error("invariant violated: {0}")]
    Invariant(String),
    /// The driver or the backend itself failed.
    #[error("backend: {0}")]
    Backend(String),
    /// Deterministic constraint violation (Postgres SQLSTATE 23xxx /
    /// SQLite `SQLITE_CONSTRAINT`), carrying the SQLSTATE / extended code.
    /// Terminal: replaying the batch can never succeed (STORE-4 / D5), so the
    /// flush loop dead-letters it instead of retrying.
    #[error("constraint violated ({0})")]
    Constraint(String),
    /// A write whose fencing token is stale (GitHub issue #1). The lease moved
    /// to a newer holder since this writer acquired it, so the store refuses
    /// the write: a fenced holder must not overwrite the new holder's rows.
    /// An honest, explicit refusal — never a silent drop.
    #[error("stale write (fencing token): {0}")]
    StaleWrite(String),
    /// Any other failure, kept whole.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl StoreError {
    /// STORE-4: retryability for the flush loop. A constraint violation is
    /// deterministic — retrying it can never succeed and only blocks the
    /// queue (head-of-line) — so it is non-retryable and dead-lettered
    /// (D5: drop-after-log, visible in `FlushStats::dead_lettered`). A
    /// `StaleWrite` is also non-retryable: the holder's token will never catch
    /// up (it lost the lease), so retrying cannot succeed — on detection the
    /// flush loop treats it like a constraint (bail out of the retry ladder).
    /// Every other error keeps the existing retry / retain semantics.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, StoreError::Constraint(_) | StoreError::StaleWrite(_))
    }
}

/// Any failure from the Lambo API.
#[derive(Debug, thiserror::Error)]
pub enum LamboError {
    /// The durable store failed. See [`StoreError`].
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Producing an embedding failed **for this input** — the embedder answered
    /// and the answer was unusable, so a later attempt gets the same answer.
    #[error("embed: {0}")]
    Embed(String),
    /// Producing an embedding failed because the embedder **could not be
    /// reached or did not answer in time** — a transport failure or a timeout.
    ///
    /// Split out of [`LamboError::Embed`] by J3 round-1 N1 for the same reason
    /// [`LamboError::SoftLock`] was split out of [`LamboError::Conflict`] by
    /// J1-R2-2: a class a decision turns on must be a **type**, not a substring
    /// of a message. The decision here is the durable-intent replay's, and it is
    /// irreversible in both directions — settle an acked write `failed`, or
    /// leave it durable for the next process. Before the split, the replay arm
    /// could only see `LamboError::Embed(String)` and so treated a dead
    /// llama.cpp exactly like a poison record, destroying the whole backlog on
    /// one transient outage.
    ///
    /// Produced only where the cause is known: `graph::hybrid::derive`'s embed
    /// timeout arm, and its embed-error arm when
    /// [`crate::embed::EmbedError::is_transient`] says so.
    ///
    /// `Display` is deliberately identical to [`LamboError::Embed`]'s: to an
    /// operator and to the ledger this is the same *class* of failure
    /// (`error_kind` stays `"embedding error"`), and the receipt text for a
    /// refused write is unchanged. The split is about what the replay may
    /// conclude, not about renaming the failure.
    #[error("embed: {0}")]
    EmbedUnavailable(String),
    /// The configuration is unusable.
    #[error("config: {0}")]
    Config(String),
    /// Another writer holds this session — the T8.6 single-writer lease, at
    /// build time or lost mid-flight ([`crate::Memory`]'s lease-lost fence).
    ///
    /// Its message carries session-private state and an operator-only override,
    /// so it is **not** model-facing: `mcp::server`'s N4 policy flattens it to a
    /// class. The §11 soft-lock refusal that *is* safe to render is
    /// [`LamboError::SoftLock`], a separate variant for exactly that reason
    /// (J1-R2-2).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Another agent holds this node's §11 soft lock, or this agent does not
    /// hold the lock it tried to release.
    ///
    /// Split out of [`LamboError::Conflict`] by J1-R2-2 so that "a soft-lock
    /// refusal" is a *type*, not a string or a guess about which of a variant's
    /// producers ran. Produced by `graph::reserve::{reserve, release}` and
    /// nowhere else: those two messages are built from a node id the caller
    /// just sent, the holder's `agent_id` and an expiry — all three already
    /// model-facing, since `recall` renders the last two into the context
    /// block — which is what lets `mcp::server` render this variant intact
    /// while every other error class is flattened.
    ///
    /// `Display` is deliberately identical to [`LamboError::Conflict`]'s: this
    /// is the same *class* to an operator and to the ledger (`error_kind` stays
    /// `"conflict"`), and the CLI keeps the text its subprocess tests match. The
    /// split is about who may read the detail, not about renaming the failure.
    #[error("conflict: {0}")]
    SoftLock(String),
    /// Any other failure, kept whole.
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
