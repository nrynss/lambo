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
    /// Something an agent noticed. The weakest kind: GC's score cut spares it
    /// like Logic and Constraint (it still goes as an orphan or island), and
    /// it is the only kind that can later be demoted.
    Observation,
}

impl ConceptType {
    /// Relative resistance to GC eviction (higher = stickier). From v0.6.0
    /// design. Scales GC's step-2 bar for the types still under the score cut
    /// and Solo promotion's score; see [`Self::exempt_from_gc_score_cut`].
    pub const fn eviction_resistance(self) -> f64 {
        match self {
            Self::Constraint => 1.5,
            Self::Entity => 1.2,
            Self::Logic => 1.1,
            Self::Resource => 1.0,
            Self::Observation => 0.7,
        }
    }

    /// Whether GC's step-2 score cut never collects this type (issue #29).
    ///
    /// `Logic`, `Constraint` and `Observation` are exempt: they carry what an
    /// agent decided, required or noticed, and the score cut ranks by
    /// structure and age, neither of which says a note has stopped mattering.
    /// They are still collected as orphans and as disconnected components,
    /// which are structural clauses. Exhaustive on purpose: a new type has to
    /// state which side it is on.
    pub const fn exempt_from_gc_score_cut(self) -> bool {
        match self {
            Self::Logic | Self::Constraint | Self::Observation => true,
            Self::Entity | Self::Resource => false,
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

/// Which concepts a recall is allowed to match — **one of three consequences
/// this setting has, and the two others are on the write path.**
///
/// The first two consequences were the ones documented for a long time (the
/// third was found by J3-R2R-7, which is also the finding that made this
/// authority name all three):
///
/// 1. **Recall scope** — which concepts a recall may match: canonical facts
///    only (`Canonical`) or canonical and ordinary memory together (`Hybrid`).
/// 2. **Whether a derive embeds** — `crate::graph::hybrid::derive` is selected
///    by this setting, so `Hybrid` stores a new concept **with its embedding
///    or not at all**: an embedder that is unreachable, refusing or too slow
///    fails the write rather than applying a concept semantic recall could
///    never find. `Canonical` is the declared keyword-only mode (spec §3.2)
///    and needs no embedder to write.
/// 3. **The call-time validation rule set** — `Canonical` additionally runs
///    `reject_repeated_observation` and the single-`Hierarchical`-parent rule,
///    while `Hybrid` uses the laxer hybrid rule set
///    (`crate::memory::Memory::derive`, `crate::graph::derive::validate`). So
///    the opt-out a user is sent to — flip to `Canonical` — also starts
///    rejecting inputs `Hybrid` accepted (J3-R2R-7).
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

/// One turn of agent work: the unit that facts are derived from.
///
/// ## Flush time and event time (workstream D)
///
/// [`Self::created_at`] is **flush time**: when Lambo recorded the turn. It is
/// stamped by the process clock and never accepted from a caller — the
/// server-authoritative ordering that F18 protects on the CLI surface too.
///
/// [`Self::event_time`] is **event time**: the instant this turn is *about* —
/// a commit date, a transcript timestamp — supplied per fact by an ingester
/// replaying history. `None` (the default) means the turn has no about-time:
/// a live session, whose facts are about now. The resolver every time-based
/// canonization gate reads is [`Self::about_time`], never a bare field, so
/// the fallback rule lives in exactly one place; see
/// `crate::canon::event_time` for the design decisions behind it.
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
    /// When this turn was originally made, when the ingester knows it.
    ///
    /// `None` for live sessions. Serde-defaulted so existing fixture JSON —
    /// which has no `event_time` key — loads unchanged; a missing key *is*
    /// the no-event-time case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time: Option<DateTime<Utc>>,
}

impl Interaction {
    /// The instant this fact is about: its event time, or its flush time when
    /// the ingester supplied none (D's fallback rule — see
    /// `crate::canon::event_time`). Every age/coverage/separation measurement
    /// resolves timestamps through this, so a fact without event time behaves
    /// exactly as it did before D while an event-timed fact ages by its own
    /// about-time.
    pub fn about_time(&self) -> DateTime<Utc> {
        self.event_time.unwrap_or(self.created_at)
    }
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
    /// How many times a human has explicitly confirmed this concept (C2, spec
    /// §3.2's "Human Confirmed" term).
    ///
    /// The one solo-score input no existing structure carries (the other three
    /// derive from interactions, structural edges and the canonization event
    /// log — see `crate::canon::policy`), so it is persisted rather than
    /// invented: bumped only through [`crate::Memory::confirm_human`], a verb
    /// no agent write path can reach, so agent activity cannot inflate it.
    /// Serde-defaulted (like `event_time` and `chunk_group_id`) so existing
    /// fixture JSON — which has no key — loads unchanged; a missing key *is*
    /// the never-confirmed case.
    #[serde(default)]
    pub human_confirmed: i32,
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
    /// Where [`Self::embedding`] came from when it is **not** a function of
    /// [`Self::content`] (#22). `None` means the vector, if any, was embedded
    /// from this concept's own content text, which is every concept a text
    /// write path makes.
    ///
    /// `Some` marks a supplied vector (an image embedding today). Re-embed and
    /// the merge rules read it so they never replace an image vector with a
    /// vector of its caption. It holds provenance only, never image bytes.
    /// Serde-defaulted and skipped when `None`, so existing fixture JSON and
    /// every wire shape that serializes a concept are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_source: Option<EmbeddingSource>,
}

/// Provenance of a concept's supplied vector (#22, design §4.3).
///
/// Persisted as compact JSON in the nullable `concepts.embedding_source`
/// column. It never carries image bytes or base64: only the modality, which
/// side computed the vector, the MIME type when known and, for a vector the
/// server computed from bytes it saw, the hex sha256 of those bytes. All of it
/// lives on the concept row, so erasing the session erases it.
///
/// Unknown keys are refused on decode, so a value written by a newer build
/// fails loudly here instead of being dropped by the next upsert.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingSource {
    /// What the vector was computed from.
    pub modality: SourceModality,
    /// Which side computed the vector.
    pub origin: VectorOrigin,
    /// Lowercase hex sha256 of the image bytes the client submitted to the
    /// server. An embedder may send its backend a canonical form of them
    /// instead (EmbeddingGemma 2 downscales an image over 768 px a side and
    /// converts a WebP to PNG); this still names the submitted bytes. `None`
    /// for a client-submitted vector, where the server never saw the bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The source's MIME type, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<ImageMimeWire>,
}

/// The modality of a supplied vector's source. Image is the only one today.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceModality {
    /// An image embedding.
    Image,
}

/// Which side computed a supplied vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorOrigin {
    /// The server embedded bytes it was sent.
    Server,
    /// A client computed the vector and submitted it.
    Client,
}

/// The persisted spelling of [`crate::embed::ImageMime`]: the MIME string
/// itself, so the stored JSON reads `"image/png"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageMimeWire {
    /// `image/png`.
    #[serde(rename = "image/png")]
    Png,
    /// `image/jpeg`.
    #[serde(rename = "image/jpeg")]
    Jpeg,
    /// `image/webp`.
    #[serde(rename = "image/webp")]
    Webp,
}

impl From<crate::embed::ImageMime> for ImageMimeWire {
    fn from(mime: crate::embed::ImageMime) -> Self {
        use crate::embed::ImageMime;
        match mime {
            ImageMime::Png => Self::Png,
            ImageMime::Jpeg => Self::Jpeg,
            ImageMime::Webp => Self::Webp,
        }
    }
}

impl From<ImageMimeWire> for crate::embed::ImageMime {
    fn from(mime: ImageMimeWire) -> Self {
        match mime {
            ImageMimeWire::Png => Self::Png,
            ImageMimeWire::Jpeg => Self::Jpeg,
            ImageMimeWire::Webp => Self::Webp,
        }
    }
}

impl EmbeddingSource {
    /// Refuse to persist a source whose sha256 is not 64 lowercase hex
    /// characters. [`Self::from_column`] refuses one on load, so writing it
    /// would make the whole session unloadable; refusing the write keeps the
    /// bad value out of the store. Every store's concept write calls this
    /// before [`Self::to_column`].
    pub fn check_writable(&self, concept: impl fmt::Display) -> Result<(), StoreError> {
        match &self.sha256 {
            Some(digest) if !is_lowercase_sha256_hex(digest) => {
                Err(StoreError::Invariant(format!(
                    "concept {concept}: refusing to store an embedding_source whose sha256 is not \
                 64 lowercase hex characters ({} bytes)",
                    digest.len()
                )))
            }
            _ => Ok(()),
        }
    }

    /// The compact JSON a store persists in `concepts.embedding_source`.
    pub fn to_column(&self) -> String {
        // A struct of strings and unit enums cannot fail to serialize.
        serde_json::to_string(self).expect("EmbeddingSource serializes")
    }

    /// Decode a stored `concepts.embedding_source` value. A value that does
    /// not parse is an error, never `None`: reading it as `None` would let a
    /// re-embed overwrite the supplied vector with a vector of the caption.
    /// `concept` names the row in the error, which also names the way out
    /// (upgrade, or `lambo erase-session`; #22 review L4).
    pub fn from_column(raw: &str, concept: impl fmt::Display) -> Result<Self, StoreError> {
        let source: Self = serde_json::from_str(raw).map_err(|e| {
            StoreError::Invariant(format!(
                "concept {concept}: concepts.embedding_source does not decode ({e}). \
                 A newer Lambo build probably wrote it: upgrade to that build or later \
                 to load this session, or discard the session with \
                 `lambo erase-session`, which does not need to load it"
            ))
        })?;
        // Review I1: the digest is held to its documented form on the way in,
        // so nothing downstream sees a sha256 that is not one.
        if let Some(digest) = &source.sha256
            && !is_lowercase_sha256_hex(digest)
        {
            return Err(StoreError::Invariant(format!(
                "concept {concept}: concepts.embedding_source has a malformed sha256 \
                 (expected 64 lowercase hex characters, got {} bytes). Lambo never writes \
                 a malformed one, so the row was changed outside Lambo: repair it, or \
                 discard the session with `lambo erase-session`",
                digest.len()
            )));
        }
        Ok(source)
    }
}

/// 64 lowercase hex characters: the only form [`EmbeddingSource::sha256`]
/// takes.
fn is_lowercase_sha256_hex(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
///
/// Like [`Interaction`], an edge carries flush time ([`Self::created_at`]) and
/// optional event time ([`Self::event_time`]). Edges are stamped from the
/// interaction that wrote them (`record_action`/`derive` timestamp edges with
/// that interaction's clock), so the edge inherits the writing interaction's
/// event time at creation — an edge manufactured during a historical ingest is
/// as old as what it is about, while a live edge falls back to its own flush
/// time and keeps F17's fresh-evidence guard intact.
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
    /// When the relation the edge records was originally made, if the writing
    /// interaction carried an event time. Serde-defaulted like
    /// [`Interaction::event_time`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_time: Option<DateTime<Utc>>,
}

impl Edge {
    /// The instant this relation is about: its event time, or its flush time
    /// when the writing interaction had none (same fallback rule as
    /// [`Interaction::about_time`]).
    pub fn about_time(&self) -> DateTime<Utc> {
        self.event_time.unwrap_or(self.created_at)
    }
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
    /// Read-access bookkeeping for one concept (issue #30): the concept's
    /// **absolute** `access_count` and `last_accessed` as the graph holds them
    /// when the access is drained — not a delta.
    ///
    /// Applied as a narrow, **monotonic** column update — `access_count =
    /// max(stored, this)`, `last_accessed = max(stored, this)` — on an existing
    /// row only; a missing row (collected, retracted) is a no-op. It never
    /// inserts, and it touches no other column: unlike [`Mutation::UpsertNode`]
    /// it does not rewrite the embedding, so a read never re-touches the vector
    /// index. Monotonic + absolute makes it idempotent under replay (a retained
    /// batch re-sent) and safe against reordering with a concept upsert in the
    /// same batch: the graph only ever raises these two fields, so an upsert
    /// appended after an access carries values at least as high, and the
    /// planner emits accesses after the concept upserts of the same segment
    /// (`store::batch`), where the `max` can never lower what an upsert wrote.
    ///
    /// Not counted by the mutation epoch (see `Graph::record_accesses`). Not a
    /// persisted or wire format: mutations live only in the in-process
    /// write-behind log and are applied by the adapters, never serialized to a
    /// store (durable write intents carry a `WriteIntentPayload`, not a
    /// `Mutation`).
    RecordAccess {
        /// The concept's session (the fencing gate and the session row cover it
        /// like every other mutation).
        session_id: SessionId,
        /// The concept.
        id: NodeId,
        /// The concept's total access count.
        access_count: i32,
        /// The concept's latest access.
        last_accessed: DateTime<Utc>,
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
    /// An image derive (#22): one concept whose vector was supplied rather
    /// than embedded from its text.
    ///
    /// Its own variant, not a defaulted field on [`Self::Derive`] (design
    /// Q18): a build that predates it fails to decode an unconsumed intent and
    /// says so (a settled one is stored as [`WriteIntentPayload::settled`]),
    /// where a defaulted field would let it drop the vector silently and
    /// embed the caption in its place. The intent carries the **vector**,
    /// never image bytes, so a replay needs no image and nothing about the
    /// image beyond its vector and provenance is ever persisted.
    DeriveImage {
        /// `(content, concept_type)`: exactly one, the image concept, whose
        /// content is `supplied.content`.
        concepts: Vec<(String, ConceptType)>,
        /// `(parent, child)` hierarchy pairs, in submission order.
        pairs: Vec<(String, String)>,
        /// The vector, the contract it was declared under, and its source.
        supplied: SuppliedVector,
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

impl WriteIntentPayload {
    /// The payload a **settled** (consumed: applied or failed) intent keeps
    /// in the store (#22 review L1).
    ///
    /// Nothing reads a settled intent's payload: the replay seeds its receipt
    /// answer from the outcome alone and replays only unconsumed rows. So a
    /// settled [`Self::DeriveImage`] drops its vector, the one piece of user
    /// data in a payload that outlives what it produced, and becomes the empty
    /// [`Self::Derive`], [`SETTLED_IMAGE_INTENT_PAYLOAD`]. That way an
    /// `re-embed --drop-image-vectors` leaves no old-space vector behind in a
    /// retained intent row, and a build that predates `DeriveImage` can still
    /// load a session whose image intents are all settled (design R5: only an
    /// *unconsumed* image intent stops an older build). Text payloads are kept
    /// as they are.
    pub fn settled(&self) -> Option<WriteIntentPayload> {
        match self {
            Self::DeriveImage { .. } => Some(Self::Derive {
                concepts: Vec::new(),
                pairs: Vec::new(),
            }),
            Self::Derive { .. } | Self::Action { .. } => None,
        }
    }
}

impl WriteIntent {
    /// The serialized payload an adapter writes for this intent: the
    /// [`WriteIntentPayload::settled`] form when the intent already carries an
    /// outcome (a snapshot save or a replayed put of a settled row), the
    /// payload as acked otherwise.
    pub fn stored_payload(&self) -> Result<String, serde_json::Error> {
        match (&self.outcome, self.payload.settled()) {
            (Some(_), Some(settled)) => serde_json::to_string(&settled),
            _ => serde_json::to_string(&self.payload),
        }
    }
}

/// [`WriteIntentPayload::settled`]'s form of an image intent, serialized: what
/// the SQL adapters' consume writes over a settled `DeriveImage` payload.
pub const SETTLED_IMAGE_INTENT_PAYLOAD: &str = r#"{"kind":"derive","concepts":[],"pairs":[]}"#;

/// A SQL `LIKE` pattern that matches a stored `DeriveImage` payload and no
/// other: the serialized enum leads with its tag. (`_` is a `LIKE` wildcard
/// for one character, which can only widen the match to tags that do not
/// exist.)
pub const DERIVE_IMAGE_PAYLOAD_LIKE: &str = r#"{"kind":"derive_image",%"#;

/// A vector supplied for one concept instead of being embedded from its text
/// (#22, design sections 3.3 and 5.1): an image embedding the server computed
/// on the call path, or one a client submitted.
///
/// It is what an image derive carries through the write queue, the durable
/// intent and replay, so the derive core never sees image bytes. `contract`
/// is the space the vector was **declared** to be in: it was checked equal to
/// the live contract on the call path, is checked against the session stamp
/// under the commit lock, and against the live contract again on replay,
/// where a mismatch settles the intent `failed`.
///
/// Unknown keys are refused on decode, like [`EmbeddingSource`]. Its `Debug`
/// never prints the vector (user data, design section 4.5), so neither does
/// that of a [`WriteIntentPayload`] or [`WriteIntent`] carrying it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuppliedVector {
    /// The concept content the vector belongs to: the caption with its
    /// `[image:<id>]` suffix, exactly as the derive's concept list names it.
    pub content: String,
    /// The vector, L2-normalized on the call path.
    pub vector: Vec<f32>,
    /// The embedding space the vector was declared to be in.
    pub contract: EmbeddingContract,
    /// Where the vector came from; persisted as the concept's
    /// [`Concept::embedding_source`].
    pub source: EmbeddingSource,
}

impl std::fmt::Debug for SuppliedVector {
    /// The vector's length, never its values, as `ImagePayload`'s `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuppliedVector")
            .field("content", &self.content)
            .field("len", &self.vector.len())
            .field("contract", &self.contract)
            .field("source", &self.source)
            .finish()
    }
}

impl SuppliedVector {
    /// Refuse a supplied vector that cannot be written under `live`: a
    /// declared contract that is not exactly `live`, a width other than
    /// `live.dim`, a non-finite component, a zero (or overflowing) norm, or a
    /// norm more than [`SUPPLIED_UNIT_NORM_TOLERANCE`] from 1.
    ///
    /// The call path checks the raw vector's values with the same rules
    /// except the last and then normalizes it, so every `SuppliedVector` it
    /// builds is unit. The apply runs this, so a durable intent replayed
    /// under a changed live contract, or one whose stored vector was damaged
    /// or planted un-normalized, is refused rather than written as is
    /// (#22 review L5). The message names both contracts and never quotes
    /// the vector.
    pub fn check(&self, live: &EmbeddingContract) -> Result<(), String> {
        check_supplied_values(&self.vector, &self.contract, live)?;
        let norm = self
            .vector
            .iter()
            .map(|x| f64::from(*x) * f64::from(*x))
            .sum::<f64>()
            .sqrt();
        if (norm - 1.0).abs() > SUPPLIED_UNIT_NORM_TOLERANCE {
            return Err(format!(
                "supplied vector is not unit length (L2 norm {norm:.6}); the call path \
                 normalizes every supplied vector, so this one was not built by it"
            ));
        }
        Ok(())
    }
}

/// How far from 1 a [`SuppliedVector`]'s L2 norm (computed in `f64`) may be
/// at apply. Ten times the call path's own "already unit" tolerance
/// (`graph::image::normalize`), so every vector the call path produced,
/// renormalized in `f64` and stored as `f32`, passes with room to spare.
pub const SUPPLIED_UNIT_NORM_TOLERANCE: f64 = 1e-5;

/// [`SuppliedVector::check`] on its parts (all but the unit-norm rule), for
/// the call path, which checks a submitted vector before it normalizes it
/// and builds the [`SuppliedVector`].
pub(crate) fn check_supplied_values(
    values: &[f32],
    declared: &EmbeddingContract,
    live: &EmbeddingContract,
) -> Result<(), String> {
    if declared != live {
        let show = |c: &EmbeddingContract| {
            format!(
                "kind={} model={:?} dim={}",
                c.kind,
                c.model.as_deref().unwrap_or("(default)"),
                c.dim
            )
        };
        return Err(format!(
            "supplied vector's declared embedding contract ({}) is not the live embedding \
             contract ({}); a vector is accepted only into the exact space it was computed in",
            show(declared),
            show(live)
        ));
    }
    if values.len() != live.dim {
        return Err(format!(
            "supplied vector has {} components but the embedding contract's width is {}",
            values.len(),
            live.dim
        ));
    }
    if values.iter().any(|x| !x.is_finite()) {
        return Err("supplied vector has a non-finite component (NaN or infinity)".into());
    }
    let norm = values
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return Err("supplied vector has zero norm; it names no direction to search by".into());
    }
    Ok(())
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
    /// When the intent was consumed (process clock of the consumer).
    ///
    /// Doubles as the **purge** clock, but the purging is *lazy* (J3-R2R-5):
    /// adapters delete consumed rows older than the receipt-retention window
    /// only as a side-effect of the next consume, scoped to one
    /// `session_id`, so a session that goes quiet after a burst keeps its
    /// consumed rows until its next consume — there is no sweeper. See
    /// [`WRITE_INTENT_RETENTION`]'s docstring for the two-mechanism precise
    /// statement.
    pub consumed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct MutationBatch {
    /// The mutations, in the order they were appended.
    pub mutations: Vec<Mutation>,
    /// The graph's [`crate::graph::Graph::epoch`] at drain time — an absolute
    /// watermark, not a delta: every mutation this batch carries is counted
    /// **through** this value. `Graph::drain_log` stamps it; adapters persist
    /// it monotonically (`GREATEST`-style upsert) **inside the flush
    /// transaction**, so a replayed batch converges (the idempotency contract)
    /// and a crash between content and counter is impossible by construction.
    /// Issue #17: this is the value `Graph::from_snapshot` resumes, so GC's
    /// `gc_interval` measures deployment-lifetime mutations instead of
    /// per-process ones. Hand-built batches (test doubles) default to 0, a
    /// no-op against any stored value.
    #[serde(default)]
    pub mutation_epoch: u64,
    /// The graph's [`GcMark`] at drain time (issue #29) — the GC sweep
    /// watermark and the time of the last sweep, carried exactly the way
    /// [`MutationBatch::mutation_epoch`] is: an absolute value stamped by
    /// `Graph::drain_log` and persisted monotonically ([`GcMark::merge`]) in
    /// the flush transaction, so a writer restart neither resets GC's
    /// accounting (the defect that let every restart past `gc_interval`
    /// lifetime mutations sweep and bump `gc_survived`) nor resets the
    /// `gc_max_interval` clock. Hand-built batches default to the unset mark,
    /// a no-op against any stored value.
    #[serde(default, skip_serializing_if = "GcMark::is_unset")]
    pub gc_mark: GcMark,
}

/// GC's durable sweep accounting for one session (issue #29).
///
/// Persisted with the session beside `mutation_epoch` (`sessions.last_gc_epoch`
/// / `sessions.last_gc_at`) and resumed by `Graph::from_snapshot`, so the
/// daemon's sweep trigger survives a writer restart instead of starting from
/// zero in every process.
///
/// Both fields only ever move forward, independently: [`GcMark::merge`] is a
/// field-wise max, which is what the adapters' `MAX`/`GREATEST` upsert applies,
/// so a replayed batch converges and a stale stamp can never rewind either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcMark {
    /// The epoch the next `gc_interval` (and the idle floor) is measured from:
    /// the epoch after the last sweep, advanced by every deferred survivor-bump
    /// drain so GC's own writes are never credited as session mutations
    /// (NEW-2). `0` for a session that has never swept.
    #[serde(default)]
    pub last_gc_epoch: u64,
    /// When the last sweep ran — or, for a session that has never swept, when a
    /// writer first observed it without one (the daemon anchors the
    /// `gc_max_interval` clock there rather than sweeping on attach). `None`
    /// until either happens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gc_at: Option<DateTime<Utc>>,
    /// `last_gc_at` was **re-anchored** by this writer after the stored value
    /// was found in the future (a forward wall-clock jump that was later
    /// corrected; see `crate::daemon::gc::gc_clock_ahead`). The one case
    /// where `last_gc_at` may move backwards: a mark carrying it **replaces**
    /// the stored `last_gc_at` instead of max-merging with it, so the corrected
    /// anchor persists instead of the future one re-asserting itself on every
    /// restart. Writer-side only: never persisted (stored marks are always
    /// `false`), omitted from JSON when `false`. `last_gc_epoch` is never
    /// affected — it stays strictly monotonic.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last_gc_at_reset: bool,
}

impl GcMark {
    /// True for the never-swept, never-anchored mark (the serde default).
    pub fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    /// Merge an older mark (`self`) with a newer one (`other`): field-wise max,
    /// except that a newer mark carrying [`GcMark::last_gc_at_reset`] supplies
    /// `last_gc_at` outright. This is the flush loop's carry: the newer stamp
    /// comes from the same writer's graph, which re-anchored deliberately, so
    /// its time is the truth even when it is earlier — and even when the older
    /// mark carries the flag too (a second re-anchor in one process: the
    /// pending mark still holds the first jump's future sweep time, which a
    /// max-merge would keep). The reset flag is sticky (`a || b`): once a
    /// writer re-anchored, every later stamp of its graph is derived from the
    /// corrected anchor and must keep replacing the stored future one until it
    /// has landed. `last_gc_epoch` is always the strict max.
    /// Whether this mark's re-anchor may replace `against`'s `last_gc_at`:
    /// it must carry the reset flag **and** be at least as current as the mark
    /// it replaces (`last_gc_epoch >=`). A re-anchor never changes the epoch,
    /// so a writer's own re-anchor always qualifies; a replayed or stale reset
    /// from before a later sweep (a lower epoch) does not, and falls back to
    /// the ordinary max-merge, so it cannot rewind a newer sweep's time.
    pub fn reset_is_current_for(&self, against: &Self) -> bool {
        self.last_gc_at_reset && self.last_gc_epoch >= against.last_gc_epoch
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            last_gc_epoch: self.last_gc_epoch.max(other.last_gc_epoch),
            last_gc_at: if other.reset_is_current_for(&self) {
                other.last_gc_at.or(self.last_gc_at)
            } else {
                match (self.last_gc_at, other.last_gc_at) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                }
            },
            last_gc_at_reset: self.last_gc_at_reset || other.last_gc_at_reset,
        }
    }

    /// The store-side merge every adapter applies on flush: `self` is the
    /// stored mark, `incoming` the batch's. `last_gc_epoch` is the max;
    /// `last_gc_at` is the max unless `incoming` carries
    /// [`GcMark::last_gc_at_reset`] and is at least as current as the stored
    /// mark ([`GcMark::reset_is_current_for`]), in which case it replaces the
    /// stored value (a `None` never erases one). The result is a stored mark, so its
    /// reset flag is always `false`. The SQL adapters implement exactly this
    /// in their session upsert.
    pub fn apply_to_stored(self, incoming: Self) -> Self {
        Self {
            last_gc_epoch: self.last_gc_epoch.max(incoming.last_gc_epoch),
            last_gc_at: if incoming.reset_is_current_for(&self) {
                incoming.last_gc_at.or(self.last_gc_at)
            } else {
                match (self.last_gc_at, incoming.last_gc_at) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                }
            },
            last_gc_at_reset: false,
        }
    }
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
    /// The graph's mutation epoch when this snapshot was taken.
    /// [`crate::graph::Graph::from_snapshot`] resumes it, so the epoch is a
    /// deployment-lifetime counter rather than a per-process one (issue #17):
    /// GC's `gc_interval` and the recall cache's epoch key keep their meaning
    /// across a writer restart. `0` on snapshots written before this field
    /// existed (serde default) — those sessions resume the accounting from
    /// zero and accumulate forward.
    #[serde(default)]
    pub mutation_epoch: u64,
    /// GC's sweep accounting when this snapshot was taken (issue #29).
    /// [`crate::graph::Graph::from_snapshot`] resumes it, so a writer restart
    /// does not reset `last_gc_epoch` (and with it, sweep once more and bump
    /// every `gc_survived`) or the `gc_max_interval` clock. Unset on snapshots
    /// written before the field existed.
    #[serde(default, skip_serializing_if = "GcMark::is_unset")]
    pub gc_mark: GcMark,
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

/// The stable tie-break for score-ordered candidate lists (issue #2): on an
/// exact score tie, order by **canonical key ascending**, with a keyless node
/// sorting after every keyed node (SQL's `NULLS LAST` on an ascending order),
/// and **node id ascending** as the final fallback inside each class. Node ids
/// are minted per run (`Uuid::new_v4`), so an id-first tie-break is
/// deterministic within a run but arbitrary across runs; the canonical key is
/// persisted and stable for the same logical concept. The key is not a total
/// separator (non-canonical synonym duplicates share one, and interactions
/// carry none), which is why the id fallback stays — but key *presence* is
/// itself compared. Ignoring a present key whenever the other side lacked one
/// made the order intransitive on mixed input (a keyed node could sort both
/// behind and ahead of a keyless one depending on the third element), so the
/// result of a sort depended on input order; remediation round 1 made the
/// comparator total: the same pair now always orders the same way.
pub fn tie_break_by_key(
    a_key: Option<&str>,
    a: &NodeId,
    b_key: Option<&str>,
    b: &NodeId,
) -> std::cmp::Ordering {
    match (a_key, b_key) {
        (Some(a_key), Some(b_key)) => a_key.cmp(b_key),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
    .then_with(|| a.0.cmp(&b.0))
}

/// The daemon's score table — epoch of the graph state it was computed from,
/// plus the score-descending ranked list of concept scores.
///
/// Daemon-owned: the rescore loop replaces it wholesale each cycle. T4.2+
/// reads it; never mutated from outside. Defined here rather than in
/// [`crate::daemon`] because recall and canonization read it too (#25);
/// `daemon::ScoreTable` re-exports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScoreTable {
    /// [`crate::graph::Graph::epoch`] the scores were computed from.
    pub epoch: u64,
    /// Score-descending (id-ascending tie-break) concept scores.
    pub ranked: Vec<Scored<NodeId>>,
}

/// Per-condition payload — what recall (T5.3) renders and what a re-validation
/// predicate reads. The conflict payload carries everything the demo sentence
/// needs: the agent(s) involved and how long ago the write happened
/// ("Agent A wrote to it eleven seconds ago").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotListPayload {
    /// Conflicting multi-agent write on [`crate::daemon::hotlist::HotListEntry::node`].
    Conflict {
        /// Agents with edges to the node (≥2), the conflicting writers.
        agents: Vec<AgentId>,
        /// The agent that made the **most recent qualifying write** — the
        /// subject of the §13 sentence "Agent A wrote to it eleven seconds
        /// ago" (ALGO-2). Without it the renderer can only guess from
        /// `agents`, and on the shipped fixture the naive guess (first
        /// alphabetically) is wrong: the newest write is agent-b's.
        writer: AgentId,
        /// Age of `writer`'s write, in seconds, **as of the last
        /// re-validation** — refreshed by [`crate::daemon::hotlist::HotList::revalidate`], so a
        /// rendered value is the age at read time (XP-3).
        seconds_ago: u64,
    },
    /// A high-risk modification touched the node.
    HighRisk { reason: String },
    /// The node drifted from any root goal.
    Drift {
        /// Unweighted shortest-path hop count to the nearest root goal, or
        /// [`crate::daemon::drift::DRIFT_HOPS_NO_PATH`] when there is no path.
        hops: u64,
        /// The root goal node the path terminates at (nil when no path).
        root: NodeId,
    },
    /// The node's session has been inactive this long, in seconds.
    Stale { seconds_inactive: u64 },
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

    /// Whether this is a checked vector read refusing its probe because the
    /// session's embedding space is not the caller's: the durable contract
    /// changed under the read (the E2E-6 race), or the probe's width is not
    /// the session's.
    ///
    /// Every store and the holder's graph source word these two refusals
    /// alike (the parity tests compare them), so the prefixes identify them.
    /// Used only to word a model-facing message: neither refusal carries a
    /// detail N4 hides, and both are fixed by re-reading the contract and
    /// retrying, which a bare "store error" does not say (#22 PR 6 review
    /// Low 1). The class stays `store error`.
    pub fn is_embedding_contract_refusal(&self) -> bool {
        matches!(self, StoreError::Invariant(m)
            if m.starts_with("vector candidate lookup refused after embedding contract changed")
                || m.starts_with("query embedding has "))
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
    /// An image derive (#22) was refused because a **text** concept already
    /// holds the image's canonical key, its caption plus `[image:<id>]`.
    ///
    /// The field is the caller's own image id and nothing else, which is
    /// what lets every surface render this variant to the caller intact
    /// (`crate::surface::error`) while every other `Embed` refusal is
    /// flattened to a class: the fix is the caller's (choose another image
    /// id), and a bare "embedding error" told it nothing. Produced only by
    /// `graph::hybrid`'s commit-lock check, the same J1-R2-2 rule as
    /// [`LamboError::SoftLock`]: the exception is a type, so it opens for
    /// this producer and no other.
    ///
    /// It is a fact about this input, like [`LamboError::Embed`]: a replayed
    /// intent that meets it settles `failed` instead of blocking the replay.
    #[error("image id taken: a text concept in this session already holds the caption with image id {0:?}; derive the image with another image id")]
    ImageIdTaken(String),
    /// Any other failure, kept whole.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// #22 review L5: `{:?}` of a supplied vector, or of an intent carrying
    /// one, shows its length and never a component.
    #[test]
    fn a_supplied_vector_debug_never_prints_the_vector() {
        let supplied = SuppliedVector {
            content: "red [image:a1]".into(),
            vector: vec![0.123_456_7, -0.987_654_3],
            contract: EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 2,
            },
            source: EmbeddingSource {
                modality: SourceModality::Image,
                origin: VectorOrigin::Client,
                sha256: None,
                mime: None,
            },
        };
        let payload = WriteIntentPayload::DeriveImage {
            concepts: vec![],
            pairs: vec![],
            supplied: supplied.clone(),
        };
        for shown in [format!("{supplied:?}"), format!("{payload:#?}")] {
            assert!(shown.contains("len: 2"), "{shown}");
            assert!(
                !shown.contains("0.123") && !shown.contains("987"),
                "{shown}"
            );
        }
    }

    /// #22 review L1: the SQL adapters' consume matches a stored image
    /// payload with [`DERIVE_IMAGE_PAYLOAD_LIKE`] and writes
    /// [`SETTLED_IMAGE_INTENT_PAYLOAD`] over it, both as constants. Pin them
    /// to what serde actually writes and reads.
    #[test]
    fn the_settled_image_payload_constants_match_the_serialized_forms() {
        let image = WriteIntentPayload::DeriveImage {
            concepts: vec![("red [image:a1]".into(), ConceptType::Resource)],
            pairs: vec![],
            supplied: SuppliedVector {
                content: "red [image:a1]".into(),
                vector: vec![0.6, 0.8],
                contract: EmbeddingContract {
                    kind: "fixture".into(),
                    model: None,
                    dim: 2,
                },
                source: EmbeddingSource {
                    modality: SourceModality::Image,
                    origin: VectorOrigin::Client,
                    sha256: None,
                    mime: None,
                },
            },
        };
        let like_prefix = DERIVE_IMAGE_PAYLOAD_LIKE.trim_end_matches('%');
        assert!(serde_json::to_string(&image)
            .unwrap()
            .starts_with(like_prefix));
        for text in [
            WriteIntentPayload::Derive {
                concepts: vec![("x".into(), ConceptType::Entity)],
                pairs: vec![],
            },
            WriteIntentPayload::Action {
                action: "a".into(),
                produces: vec![],
                modifies: vec![],
                depends_on: vec![],
            },
        ] {
            assert!(!serde_json::to_string(&text)
                .unwrap()
                .starts_with(like_prefix));
            assert_eq!(text.settled(), None, "text payloads are kept");
        }
        let settled = image.settled().expect("an image payload settles");
        assert_eq!(
            serde_json::to_string(&settled).unwrap(),
            SETTLED_IMAGE_INTENT_PAYLOAD
        );
        let back: WriteIntentPayload = serde_json::from_str(SETTLED_IMAGE_INTENT_PAYLOAD).unwrap();
        assert_eq!(back, settled);
    }

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
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        };
        let node = Node::Concept(c.clone());
        let s = serde_json::to_string(&node).unwrap();
        let back: Node = serde_json::from_str(&s).unwrap();
        assert_eq!(node, back);
        assert_eq!(back.id(), id);
    }

    /// Issue #29: a re-anchored mark is the one way `last_gc_at` moves back.
    /// In the flush carry (older `merge` newer) the newer reset stamp supplies
    /// the time and the flag sticks, so later stamps of the same writer keep
    /// replacing the stored future value; at the store
    /// (`apply_to_stored`) the reset replaces the time, never erases it with
    /// `None`, never rewinds the epoch, and the stored result carries no flag.
    /// The flag is omitted from JSON when false.
    #[test]
    fn gc_mark_reset_replaces_the_time_once_and_only_the_time() {
        let t =
            |d: i64| Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::days(d);
        let stored_future = GcMark {
            last_gc_epoch: 50,
            last_gc_at: Some(t(400)),
            last_gc_at_reset: false,
        };
        // A re-anchor keeps the writer's current epoch (it is not a sweep).
        let reset = GcMark {
            last_gc_epoch: 50,
            last_gc_at: Some(t(2)),
            last_gc_at_reset: true,
        };
        let carried = stored_future.merge(reset);
        assert_eq!(carried.last_gc_at, Some(t(2)));
        assert_eq!(carried.last_gc_epoch, 50);
        assert!(carried.last_gc_at_reset);
        // A later stamp from the same writer (still flagged) moves forward.
        let later = GcMark {
            last_gc_epoch: 60,
            last_gc_at: Some(t(3)),
            last_gc_at_reset: true,
        };
        assert_eq!(carried.merge(later).last_gc_at, Some(t(3)));
        // A second re-anchor: the carried mark is already flagged and holds the
        // first jump's later future sweep time; the newer flagged stamp still
        // supplies the time (a max-merge would keep the future one), and the
        // epoch stays a strict max.
        let second_future = GcMark {
            last_gc_epoch: 70,
            last_gc_at: Some(t(900)),
            last_gc_at_reset: true,
        };
        let second_reanchor = GcMark {
            last_gc_epoch: 70,
            last_gc_at: Some(t(5)),
            last_gc_at_reset: true,
        };
        let twice = carried.merge(second_future).merge(second_reanchor);
        assert_eq!(twice.last_gc_at, Some(t(5)));
        assert_eq!(twice.last_gc_epoch, 70, "last_gc_epoch is a strict max");
        assert!(twice.last_gc_at_reset);
        // An unflagged newer stamp is max-merged as before.
        assert_eq!(
            carried
                .merge(GcMark {
                    last_gc_at_reset: false,
                    ..later
                })
                .last_gc_at,
            Some(t(3))
        );

        let applied = stored_future.apply_to_stored(reset);
        assert_eq!(
            applied,
            GcMark {
                last_gc_epoch: 50,
                last_gc_at: Some(t(2)),
                last_gc_at_reset: false,
            }
        );
        let no_time = GcMark {
            last_gc_at: None,
            ..reset
        };
        assert_eq!(
            stored_future.apply_to_stored(no_time).last_gc_at,
            Some(t(400))
        );
        // Unflagged: exactly the old max-merge.
        let plain = GcMark {
            last_gc_at_reset: false,
            ..reset
        };
        assert_eq!(
            stored_future.apply_to_stored(plain),
            stored_future.merge(plain)
        );

        // A stale reset (replayed from before a later sweep: lower epoch) never
        // rewinds that sweep's time, in the carry or at the store; the epoch
        // stays a strict max and the time falls back to the max-merge.
        let later_sweep = GcMark {
            last_gc_epoch: 80,
            last_gc_at: Some(t(10)),
            last_gc_at_reset: false,
        };
        let stale_reset = GcMark {
            last_gc_epoch: 70,
            last_gc_at: Some(t(5)),
            last_gc_at_reset: true,
        };
        assert!(!stale_reset.reset_is_current_for(&later_sweep));
        assert_eq!(
            later_sweep.apply_to_stored(stale_reset),
            GcMark {
                last_gc_epoch: 80,
                last_gc_at: Some(t(10)),
                last_gc_at_reset: false,
            }
        );
        assert_eq!(later_sweep.merge(stale_reset).last_gc_at, Some(t(10)));
        assert_eq!(later_sweep.merge(stale_reset).last_gc_epoch, 80);

        let json = serde_json::to_string(&stored_future).unwrap();
        assert!(!json.contains("last_gc_at_reset"), "{json}");
        let back: GcMark = serde_json::from_str(&serde_json::to_string(&reset).unwrap()).unwrap();
        assert_eq!(back, reset);
    }

    /// Issue #29: the GC mark merges field-wise (each field only moves
    /// forward), the unset mark is the identity, and old JSON without the
    /// field still parses (batches and snapshots written before #29).
    #[test]
    fn gc_mark_merge_is_fieldwise_max_and_serde_defaults() {
        use chrono::TimeZone;
        let t = |h| Utc.with_ymd_and_hms(2026, 10, 7, h, 0, 0).unwrap();
        let a = GcMark {
            last_gc_epoch: 10,
            last_gc_at: Some(t(9)),
            last_gc_at_reset: false,
        };
        let b = GcMark {
            last_gc_epoch: 4,
            last_gc_at: Some(t(11)),
            last_gc_at_reset: false,
        };
        assert_eq!(
            a.merge(b),
            GcMark {
                last_gc_epoch: 10,
                last_gc_at: Some(t(11)),
                last_gc_at_reset: false,
            }
        );
        assert_eq!(a.merge(b), b.merge(a), "commutative");
        assert_eq!(a.merge(GcMark::default()), a);
        assert_eq!(GcMark::default().merge(a), a);
        assert!(GcMark::default().is_unset());
        assert!(!a.is_unset());

        let old_batch: MutationBatch =
            serde_json::from_str(r#"{"mutations":[],"mutation_epoch":3}"#).unwrap();
        assert!(old_batch.gc_mark.is_unset());
        let with = MutationBatch {
            gc_mark: a,
            ..MutationBatch::default()
        };
        let back: MutationBatch =
            serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
        assert_eq!(back.gc_mark, a);
        // The unset mark is not serialized, so pre-#29 golden JSON is
        // byte-identical.
        assert!(!serde_json::to_string(&MutationBatch::default())
            .unwrap()
            .contains("gc_mark"));
    }

    #[test]
    fn mutation_batch_json_roundtrip() {
        let batch = MutationBatch {
            mutation_epoch: 0,
            gc_mark: Default::default(),
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

    /// Issue-2 remediation round 1: the old comparator compared keys only when
    /// BOTH were `Some` and fell through to the id otherwise, so a keyed node
    /// ordered ahead of a keyless one on one pairing and behind it on another
    /// (a < b < c with a > c) and sort output depended on input order. The
    /// total order sorts the same permutation every way.
    #[test]
    fn tie_break_is_transitive_on_mixed_key_presence() {
        // (key, id): the keyed nodes' ids order them OPPOSITE to their keys, so
        // only the key can decide a keyed pair, and the keyless node's id sits
        // between them — the exact shape that used to break transitivity.
        let a = (Some("b"), NodeId(uuid::Uuid::from_u64_pair(0, 1)));
        let b = (None, NodeId(uuid::Uuid::from_u64_pair(0, 2)));
        let c = (Some("a"), NodeId(uuid::Uuid::from_u64_pair(0, 3)));
        let cmp = |x: (Option<&str>, NodeId), y: (Option<&str>, NodeId)| {
            tie_break_by_key(x.0, &x.1, y.0, &y.1)
        };
        // Totality: antisymmetric, and the sorted result is one fixed sequence.
        assert_eq!(cmp(a, b), cmp(b, a).reverse());
        assert_eq!(cmp(b, c), cmp(c, b).reverse());
        assert_eq!(cmp(a, c), cmp(c, a).reverse());
        // Keyed nodes sort by key ahead of the keyless one, ids never deciding
        // across the presence partition.
        assert_eq!(cmp(c, a), std::cmp::Ordering::Less);
        assert_eq!(cmp(c, b), std::cmp::Ordering::Less);
        assert_eq!(cmp(a, b), std::cmp::Ordering::Less);
        let expected = [c, a, b];
        for input in [
            [a, b, c],
            [a, c, b],
            [b, a, c],
            [b, c, a],
            [c, a, b],
            [c, b, a],
        ] {
            let mut items = input;
            items.sort_by(|x, y| cmp(*x, *y));
            assert_eq!(items, expected, "input order {input:?} must not leak");
        }
    }

    #[test]
    fn tie_break_key_then_id_with_keyless_last() {
        let id = |n: u64| NodeId(uuid::Uuid::from_u64_pair(7, n));
        // Equal keys: id decides.
        assert_eq!(
            tie_break_by_key(Some("k"), &id(1), Some("k"), &id(2)),
            std::cmp::Ordering::Less
        );
        // Keys decide ahead of ids.
        assert_eq!(
            tie_break_by_key(Some("z"), &id(1), Some("a"), &id(2)),
            std::cmp::Ordering::Greater
        );
        // Two keyless nodes: id decides.
        assert_eq!(
            tie_break_by_key(None, &id(2), None, &id(1)),
            std::cmp::Ordering::Greater
        );
    }

    /// #25 moved `ScoreTable` and `HotListPayload` here from the daemon; the
    /// old public paths must keep naming the same types (a re-export, not a
    /// copy), so this only compiles while they do.
    #[test]
    fn moved_read_side_types_resolve_at_their_old_paths() {
        let table: crate::daemon::ScoreTable = ScoreTable::default();
        let table: crate::ScoreTable = table;
        assert_eq!(table, ScoreTable::default());
        let payload: crate::daemon::hotlist::HotListPayload = HotListPayload::Stale {
            seconds_inactive: 7,
        };
        let payload: HotListPayload = payload;
        assert_eq!(
            payload,
            HotListPayload::Stale {
                seconds_inactive: 7
            }
        );
    }

    /// #22 PR 2: the column value is compact JSON with a pinned spelling, it
    /// round-trips, and the digest and MIME are omitted when absent.
    #[test]
    fn embedding_source_column_spelling_is_pinned_and_round_trips() {
        let server = EmbeddingSource {
            modality: SourceModality::Image,
            origin: VectorOrigin::Server,
            sha256: Some("ab".repeat(32)),
            mime: Some(ImageMimeWire::Png),
        };
        let raw = server.to_column();
        assert_eq!(
            raw,
            format!(
                r#"{{"modality":"image","origin":"server","sha256":"{}","mime":"image/png"}}"#,
                "ab".repeat(32)
            )
        );
        assert_eq!(EmbeddingSource::from_column(&raw, "c1").unwrap(), server);

        let client = EmbeddingSource {
            modality: SourceModality::Image,
            origin: VectorOrigin::Client,
            sha256: None,
            mime: None,
        };
        assert_eq!(
            client.to_column(),
            r#"{"modality":"image","origin":"client"}"#
        );
        assert_eq!(
            EmbeddingSource::from_column(&client.to_column(), "c2").unwrap(),
            client
        );
    }

    /// A stored value this build cannot read is an invariant error, never a
    /// silent `None`: `None` would let a re-embed replace the supplied vector
    /// with a vector of the caption. Unknown keys and unknown variants (a
    /// newer build's value) are refused the same way.
    #[test]
    fn embedding_source_refuses_a_value_it_cannot_read() {
        for raw in [
            "",
            "not json",
            r#"{"modality":"image"}"#,
            r#"{"modality":"audio","origin":"server"}"#,
            r#"{"modality":"image","origin":"server","mime":"image/gif"}"#,
            r#"{"modality":"image","origin":"client","frame":3}"#,
        ] {
            let err = EmbeddingSource::from_column(raw, "c3").expect_err(raw);
            assert!(matches!(err, StoreError::Invariant(_)), "{raw}: {err:?}");
            assert!(err.to_string().contains("embedding_source"), "{err}");
            assert!(err.to_string().contains("concept c3"), "{err}");
            // Review L4: the refusal says how to get the session back.
            assert!(err.to_string().contains("upgrade"), "{err}");
            assert!(err.to_string().contains("lambo erase-session"), "{err}");
        }
    }

    /// Review I1: the stored digest is decoded as strictly as the rest of the
    /// value. Anything but 64 lowercase hex characters is refused like an
    /// unreadable value; a well-formed one loads.
    #[test]
    fn embedding_source_refuses_a_malformed_sha256() {
        let raw = |digest: &str| {
            format!(r#"{{"modality":"image","origin":"server","sha256":"{digest}"}}"#)
        };
        let good = "0123456789abcdef".repeat(4);
        assert_eq!(
            EmbeddingSource::from_column(&raw(&good), "c4")
                .unwrap()
                .sha256
                .as_deref(),
            Some(good.as_str())
        );
        for digest in [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            "AB".repeat(32),
            "zz".repeat(32),
            format!("{}é", "a".repeat(62)),
        ] {
            let err = EmbeddingSource::from_column(&raw(&digest), "c4").expect_err(&digest);
            assert!(matches!(err, StoreError::Invariant(_)), "{digest}: {err:?}");
            assert!(err.to_string().contains("concept c4"), "{err}");
            assert!(err.to_string().contains("sha256"), "{err}");
        }
    }

    /// Fixture JSON has no `embedding_source` key: it loads as `None`, and a
    /// `None` concept serializes without the key, so every golden that
    /// carries a concept is byte-identical. A `Some` survives the node JSON.
    #[test]
    fn concept_embedding_source_is_serde_default_and_skipped_when_none() {
        let ts = Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap();
        let mut c = Concept {
            id: NodeId::new(),
            session_id: SessionId::from("s1"),
            content: "outfit for onam [image:abc]".into(),
            canonical_key: "abc image onam outfit".into(),
            concept_type: ConceptType::Entity,
            origin_interaction: NodeId::new(),
            origin_agent: AgentId::from("agent-A"),
            created_at: ts,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: Some(vec![0.6, 0.8]),
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        };
        let json = serde_json::to_value(&c).unwrap();
        assert!(json.get("embedding_source").is_none(), "{json}");
        let mut legacy = json.clone();
        legacy.as_object_mut().unwrap().remove("embedding_source");
        let back: Concept = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.embedding_source, None);

        c.embedding_source = Some(EmbeddingSource {
            modality: SourceModality::Image,
            origin: VectorOrigin::Client,
            sha256: None,
            mime: Some(ImageMimeWire::Webp),
        });
        let node = Node::Concept(c);
        let back: Node = serde_json::from_str(&serde_json::to_string(&node).unwrap()).unwrap();
        assert_eq!(back, node);
    }

    #[test]
    fn image_mime_wire_converts_both_ways() {
        use crate::embed::ImageMime;
        for mime in [ImageMime::Png, ImageMime::Jpeg, ImageMime::Webp] {
            let wire = ImageMimeWire::from(mime);
            assert_eq!(ImageMime::from(wire), mime);
            assert_eq!(
                serde_json::to_value(wire).unwrap(),
                serde_json::Value::from(mime.as_str())
            );
        }
    }
}
