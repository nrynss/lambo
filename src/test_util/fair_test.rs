//! The shared canonization fair-test fixture (#36 PR 3), the single source of
//! `fixtures/session-fair-test.json`.
//!
//! The 2026-08-20 fair test named four canon-shaped rulings in advance and
//! asked that they promote and that chaff does not. This is its synthetic,
//! committed form: no rig text, every row built here from the tables below,
//! with real canonical keys (`graph::canonical::canonical_key`), the graph's
//! own `Derives`/`Temporal` writes, and deterministic ids.
//!
//! The committed JSON is regenerated from this module with
//! `LAMBO_UPDATE_GOLDEN=1 cargo test --features store-memory,fixtures --lib fair_test_fixture_is_current`,
//! and that test fails when the two disagree. #20 (supersession) extends the
//! fixture here rather than writing a second one.
//!
//! # Layout
//!
//! Session `session-fair-test`, 21 interactions over 19 days (2026-09-01 to
//! 2026-09-20; longer than the 14-day Stage 2 coverage cap, so the capped
//! denominator is exercised). Every concept is past the survival floor
//! (`gc_survived = 3`) and starts at `None`: no seeded statuses.
//!
//! | Role | Keys | Shape |
//! |---|---|---|
//! | Supported rulings | `lease_stays` (Logic), `wedge` (Constraint), `identity` (Logic), `async_ack` (Constraint) | six dependents each, from 4 to 6 other interactions spread over 11 to 13 days |
//! | Under-supported variants | `wedge_five` | five dependents with spread: passes Stage 2, fails Stage 3 (blast 5) |
//! | | `identity_burst` | six dependents from three interactions inside two hours: fails Stage 2 coverage |
//! | | `ack_single` | six dependents from one interaction: fails Stage 1 under Swarm |
//! | | `lease_self` | six `parent_of` children minted in its own interaction: no other interaction vouches for it |
//! | Chaff | `file_flush`, `file_lease`, `file_changelog` | file Resources that actions modify (`Causal`) on many days, cited by none |
//! | | `soak_note` | an Observation read 17 times (access count), cited by none |
//! | | `lease_alias` | a short Entity name cited by three actions on three days: support, no load (history #36 PR 4 does not rewrite and PR 5 only previews; see [`expected`]) |
//! | | `write_path_hub` | an Entity associated (`CoOccurrence`/`Semantic`) with every ruling |
//! | | `lease_revoke` | a newer Logic that contradicts `lease_stays`: an assertion with no support of its own |
//! | Supersession (#20) | `retry_forever` (Logic) | six dependents from six other interactions on days 1 to 8: earns Canonical |
//! | | `retry_bounded` (Logic) | contradicts `retry_forever` on day 12; four dependents from four interactions on days 14 to 19, all newer than `retry_forever`'s: supersedes it when `canonization_supersede` is on; blast 4, so it stops at Venerable itself |
//! | | `ttl_fixed` (Logic) | six dependents on days 2 to 14: earns Canonical |
//! | | `ttl_sliding` (Logic) | contradicts `ttl_fixed`; four dependents on days 6, 8, 15 and 17: Stage 2 on its own, but only two are newer than `ttl_fixed`'s day-14 evidence, so it never supersedes it (the cutoff applies before origins are counted) |
//! | Dependents | `a01`..`a32` (Resource actions), `d01`..`d10` (Logic decisions), `c01`..`c06` (children), `e01`..`e06`, `f01`..`f04`, `g01`..`g06`, `h01`..`h04` (#20's actions) | the work that cites the rows above |
//!
//! Dependence is real-shaped: an action or a later decision declares
//! `depends_on` a ruling, which the store holds as `dependent -Dependency->
//! ruling` (#36's dependents relation). No ruling has an outbound
//! `Dependency` to anything, so there are no reciprocal Dependency edges.
//! Actions also carry the `Causal` edges `record_action` writes to the files
//! they modify, and a few actions cite two rulings (shared dependents).
//!
//! # Extending it
//!
//! Add interactions to [`INTERACTIONS`] (keep them in time order: the table
//! is the temporal chain), concepts to [`CONCEPTS`], citations to
//! [`DEPENDS`], and the new rows' expected fixed-point statuses to
//! [`expected`], then regenerate. Every id is derived from a hash of its
//! row's natural key (an interaction's or concept's key; an edge's source,
//! target and type), never from a position, so a row inserted anywhere
//! leaves every existing id unchanged and only adds rows to the JSON. The
//! ids are SHA-256-derived UUIDs (version 8, "custom"): the name-based v5
//! constructor needs the `uuid` crate's `v5` feature, which this crate does
//! not enable, and a deterministic hash is all the fixture needs.

#![cfg(test)]

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};

use std::collections::HashMap;

use parking_lot::RwLock;

use crate::canon::{EvalParams, Evaluator, PromotionPolicy};
use crate::config::ScoringWeights;
use crate::daemon::score::rescore;
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, GraphSnapshot, Interaction,
    Mutation, MutationBatch, Node, NodeId, ScoreTable, SessionId,
};

/// The fixture's name under `fixtures/`.
pub const NAME: &str = "session-fair-test";

/// The session id.
pub fn session() -> SessionId {
    SessionId::from(NAME)
}

fn at(day: i64, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap()
        + ChronoDuration::days(day)
        + ChronoDuration::hours(hour as i64)
}

/// The evaluation clock: two days after the last interaction, so every row
/// is past the 60-second age floor.
pub fn now() -> DateTime<Utc> {
    at(21, 12)
}

/// Interactions in time order: key, day, hour, agent, prompt.
pub const INTERACTIONS: &[(&str, i64, u32, &str, &str)] = &[
    (
        "i01",
        0,
        9,
        "agent-a",
        "settle how the writer lease behaves",
    ),
    ("i02", 0, 14, "agent-a", "settle what a flush may block"),
    (
        "i03",
        1,
        10,
        "agent-b",
        "settle how agents identify themselves",
    ),
    ("i04", 1, 16, "agent-b", "settle what a write ack promises"),
    ("i05", 2, 10, "agent-c", "draft the related rules"),
    (
        "i06",
        3,
        11,
        "agent-c",
        "survey the repository and the soak",
    ),
    ("w05", 5, 10, "agent-a", "wire the lease heartbeat"),
    ("w06", 6, 10, "agent-b", "bound the flush"),
    ("w08", 8, 10, "agent-c", "fence the canonization writes"),
    ("w09a", 9, 10, "agent-a", "identity burst one"),
    ("w09b", 9, 11, "agent-a", "identity burst two"),
    ("w09c", 9, 12, "agent-a", "identity burst three"),
    (
        "w10",
        10,
        10,
        "agent-b",
        "rework the ack path in one sitting",
    ),
    ("w11", 11, 10, "agent-c", "shutdown and receipts"),
    ("w12", 12, 10, "agent-a", "drain the queue"),
    ("w14", 14, 10, "agent-b", "reader processes"),
    ("w15", 15, 10, "agent-c", "store timeouts"),
    ("w17", 17, 10, "agent-a", "lease expiry"),
    ("w18", 18, 10, "agent-b", "per-session queues"),
    ("w19", 19, 10, "agent-c", "attach line and ids"),
    ("w19b", 19, 15, "agent-a", "close out the review"),
];

use ConceptType::{Constraint, Entity, Logic, Observation, Resource};

/// Concepts: key, type, content, origin interaction.
pub const CONCEPTS: &[(&str, ConceptType, &str, &str)] = &[
    // Supported rulings.
    ("lease_stays", Logic, "a writer keeps its lease until it releases it or the lease expires; a stall is not a release", "i01"),
    ("wedge", Constraint, "no flush may wedge the write queue: every flush finishes or fails within its deadline", "i02"),
    ("identity", Logic, "agent identity is cooperative: agent_id is caller-asserted and never verified", "i03"),
    ("async_ack", Constraint, "a write ack means the write is validated and ordered, not applied", "i04"),
    // Under-supported variants.
    ("wedge_five", Constraint, "every queue drain is bounded by the flush deadline", "i05"),
    ("identity_burst", Logic, "one agent process uses one stable agent_id for its whole life", "i05"),
    ("ack_single", Constraint, "every ack carries a receipt id the caller can wait on", "i05"),
    ("lease_self", Logic, "lease state lives in the store, never in process memory", "i05"),
    // Chaff.
    ("file_flush", Resource, "src/store/flush.rs", "i06"),
    ("file_lease", Resource, "src/store/lease.rs", "i06"),
    ("file_changelog", Resource, "CHANGELOG.md", "i06"),
    ("soak_note", Observation, "flush lag climbed during the overnight soak and recovered by morning", "i06"),
    ("lease_alias", Entity, "writer lease", "i06"),
    ("write_path_hub", Entity, "write path", "i06"),
    ("lease_revoke", Logic, "a stalled writer's lease may be revoked after one missed heartbeat", "w15"),
    // Supersession (#20).
    ("retry_forever", Logic, "a failed flush retries the same batch until the store accepts it", "i02"),
    ("retry_bounded", Logic, "a failed flush retries at most three times and then gives up on the batch", "w12"),
    ("ttl_fixed", Logic, "a lease ttl is fixed when the lease is taken and never extended", "i01"),
    ("ttl_sliding", Logic, "every heartbeat extends the lease ttl", "i06"),
    // Dependents of lease_stays.
    ("a01", Resource, "renew the writer lease from the heartbeat", "w05"),
    ("d01", Logic, "a writer that stalls keeps its lease until the ttl runs out", "w05"),
    ("a02", Resource, "fence record_canonization with the lease token", "w08"),
    ("a03", Resource, "release the lease on clean shutdown", "w11"),
    ("d02", Logic, "serve-web reads the store without taking the lease", "w14"),
    ("a04", Resource, "expire leases by ttl only", "w17"),
    // Dependents of wedge.
    ("a05", Resource, "bound each flush with a deadline", "w06"),
    ("d04", Logic, "no request handler awaits a flush", "w06"),
    ("a06", Resource, "drain the write queue before shutdown", "w12"),
    ("d03", Logic, "a flush that misses its deadline fails its batch", "w12"),
    ("a07", Resource, "time out the store write instead of retrying forever", "w15"),
    ("a08", Resource, "split the write queue per session", "w18"),
    // Dependents of identity.
    ("a09", Resource, "pass agent_id through every write", "w08"),
    ("d05", Logic, "the ledger attributes work to the caller-asserted id", "w11"),
    ("a10", Resource, "share soft locks between callers with one id", "w14"),
    ("a11", Resource, "refuse an empty agent_id", "w17"),
    ("d06", Logic, "two concurrent agents use two stable ids", "w19"),
    ("a12", Resource, "name the agent id on the attach line", "w19"),
    // Dependents of async_ack (a02 and a06 cite it too).
    ("a13", Resource, "return the receipt before the write applies", "w05"),
    ("d07", Logic, "a client waits on a receipt, not on the ack", "w11"),
    ("a14", Resource, "report pending receipts in stats", "w15"),
    ("a15", Resource, "piggyback write outcomes on the next response", "w18"),
    // Dependents of wedge_five (five).
    ("a16", Resource, "cap the batch size of a flush", "w06"),
    ("a17", Resource, "log every slow flush", "w12"),
    ("a18", Resource, "alert on flush lag", "w15"),
    ("d08", Logic, "the flush deadline is the store timeout plus one tick", "w18"),
    ("a19", Resource, "test the flush deadline against a stalled store", "w18"),
    // Dependents of identity_burst (three interactions, two hours).
    ("a20", Resource, "store the agent id in the client config", "w09a"),
    ("a21", Resource, "reuse the agent id across reconnects", "w09a"),
    ("d09", Logic, "a restarted agent keeps its id", "w09b"),
    ("a22", Resource, "print the agent id at startup", "w09b"),
    ("a23", Resource, "reject a second id from one process", "w09c"),
    ("d10", Logic, "an id change means a new agent", "w09c"),
    // Dependents of ack_single (one interaction).
    ("a24", Resource, "add a receipt field to the derive ack", "w10"),
    ("a25", Resource, "add a receipt field to the action ack", "w10"),
    ("a26", Resource, "add a receipt field to the image ack", "w10"),
    ("a27", Resource, "document the receipt field", "w10"),
    ("a28", Resource, "test the receipt field round trip", "w10"),
    ("a29", Resource, "log the receipt id on apply", "w10"),
    // lease_self's children, minted in its own interaction.
    ("c01", Logic, "the lease row names its holder", "i05"),
    ("c02", Logic, "the lease row carries a fencing token", "i05"),
    ("c03", Logic, "the lease row carries an expiry", "i05"),
    ("c04", Logic, "the lease row carries the holder endpoint", "i05"),
    ("c05", Logic, "the lease row carries the acquisition time", "i05"),
    ("c06", Logic, "the lease row is keyed by session", "i05"),
    // Citations of the alias name.
    ("a30", Resource, "check the writer lease before a flush", "w06"),
    ("a31", Resource, "show the writer lease in stats", "w12"),
    ("a32", Resource, "log writer lease takeovers", "w18"),
    // Dependents of retry_forever (days 1 to 8).
    ("e01", Resource, "requeue a failed flush batch at the head of the queue", "i03"),
    ("e02", Resource, "keep a failed batch in memory until it lands", "i04"),
    ("e03", Resource, "log each flush retry with its attempt number", "i06"),
    ("e04", Resource, "retry the lease renewal write until it lands", "w05"),
    ("e05", Resource, "back off between flush retries", "w06"),
    ("e06", Resource, "retry a failed canonization record on the next flush", "w08"),
    // Dependents of retry_bounded (days 14 to 19, all newer than the above).
    ("f01", Resource, "count flush attempts per batch", "w14"),
    ("f02", Resource, "surface the store error after the third failed flush", "w15"),
    ("f03", Resource, "drop a batch the store refused three times", "w17"),
    ("f04", Resource, "report given-up batches in stats", "w19"),
    // Dependents of ttl_fixed (days 2 to 14).
    ("g01", Resource, "store the lease expiry with the lease row", "i05"),
    ("g02", Resource, "compute the lease expiry once at acquisition", "w05"),
    ("g03", Resource, "check the fixed expiry before each flush", "w06"),
    ("g04", Resource, "reject a write after the fixed expiry", "w08"),
    ("g05", Resource, "drain the queue before the fixed expiry", "w12"),
    ("g06", Resource, "show the fixed expiry to readers", "w14"),
    // Dependents of ttl_sliding (days 6 and 8, then 15 and 17).
    ("h01", Resource, "push the lease expiry forward on each heartbeat", "w06"),
    ("h02", Resource, "renew the lease ttl from the fence path", "w08"),
    ("h03", Resource, "extend the ttl when a store write times out", "w15"),
    ("h04", Resource, "test that a heartbeat moves the expiry", "w17"),
];

/// Declared dependence: `(dependent, ruling)`, stored as
/// `dependent -Dependency-> ruling`.
pub const DEPENDS: &[(&str, &str)] = &[
    ("a01", "lease_stays"),
    ("d01", "lease_stays"),
    ("a02", "lease_stays"),
    ("a03", "lease_stays"),
    ("d02", "lease_stays"),
    ("a04", "lease_stays"),
    ("a05", "wedge"),
    ("d04", "wedge"),
    ("a06", "wedge"),
    ("d03", "wedge"),
    ("a07", "wedge"),
    ("a08", "wedge"),
    ("a09", "identity"),
    ("d05", "identity"),
    ("a10", "identity"),
    ("a11", "identity"),
    ("d06", "identity"),
    ("a12", "identity"),
    ("a02", "async_ack"),
    ("a06", "async_ack"),
    ("a13", "async_ack"),
    ("d07", "async_ack"),
    ("a14", "async_ack"),
    ("a15", "async_ack"),
    ("a16", "wedge_five"),
    ("a17", "wedge_five"),
    ("a18", "wedge_five"),
    ("d08", "wedge_five"),
    ("a19", "wedge_five"),
    ("a20", "identity_burst"),
    ("a21", "identity_burst"),
    ("d09", "identity_burst"),
    ("a22", "identity_burst"),
    ("a23", "identity_burst"),
    ("d10", "identity_burst"),
    ("a24", "ack_single"),
    ("a25", "ack_single"),
    ("a26", "ack_single"),
    ("a27", "ack_single"),
    ("a28", "ack_single"),
    ("a29", "ack_single"),
    ("a30", "lease_alias"),
    ("a31", "lease_alias"),
    ("a32", "lease_alias"),
    ("e01", "retry_forever"),
    ("e02", "retry_forever"),
    ("e03", "retry_forever"),
    ("e04", "retry_forever"),
    ("e05", "retry_forever"),
    ("e06", "retry_forever"),
    ("f01", "retry_bounded"),
    ("f02", "retry_bounded"),
    ("f03", "retry_bounded"),
    ("f04", "retry_bounded"),
    ("g01", "ttl_fixed"),
    ("g02", "ttl_fixed"),
    ("g03", "ttl_fixed"),
    ("g04", "ttl_fixed"),
    ("g05", "ttl_fixed"),
    ("g06", "ttl_fixed"),
    ("h01", "ttl_sliding"),
    ("h02", "ttl_sliding"),
    ("h03", "ttl_sliding"),
    ("h04", "ttl_sliding"),
];

/// `record_action`'s provenance: `(action, file)` as `action -Causal-> file`.
pub const MODIFIES: &[(&str, &str)] = &[
    ("a01", "file_lease"),
    ("a02", "file_lease"),
    ("a03", "file_lease"),
    ("a03", "file_changelog"),
    ("a04", "file_lease"),
    ("a05", "file_flush"),
    ("a06", "file_flush"),
    ("a07", "file_flush"),
    ("a07", "file_changelog"),
    ("a08", "file_flush"),
    ("a12", "file_changelog"),
    ("a13", "file_changelog"),
    ("a16", "file_flush"),
];

/// `parent_of`: `(parent, child)` as `parent -Hierarchical-> child`.
pub const CHILDREN: &[(&str, &str)] = &[
    ("lease_self", "c01"),
    ("lease_self", "c02"),
    ("lease_self", "c03"),
    ("lease_self", "c04"),
    ("lease_self", "c05"),
    ("lease_self", "c06"),
];

/// Association, which confers nothing.
pub const ASSOCIATIONS: &[(&str, &str, EdgeType)] = &[
    ("write_path_hub", "lease_stays", EdgeType::CoOccurrence),
    ("write_path_hub", "wedge", EdgeType::CoOccurrence),
    ("write_path_hub", "identity", EdgeType::CoOccurrence),
    ("write_path_hub", "async_ack", EdgeType::CoOccurrence),
    ("write_path_hub", "lease_alias", EdgeType::Semantic),
];

/// The fixture's contradictions, `newer -Contradicts-> older` (#19), each
/// stamped two minutes into its source's interaction: an assertion with no
/// support (`lease_revoke`), one whose newer support supersedes its target
/// when `canonization_supersede` is on (`retry_bounded`, #20), and one whose
/// support is mostly older than its target's (`ttl_sliding`, #20).
pub const CONTRADICTIONS: &[(&str, &str)] = &[
    ("lease_revoke", "lease_stays"),
    ("retry_bounded", "retry_forever"),
    ("ttl_sliding", "ttl_fixed"),
];

/// Read traffic: `(concept, access_count)`, last accessed on day 19.
pub const ACCESSES: &[(&str, i32)] = &[("soak_note", 17), ("write_path_hub", 9)];

/// A stable id for `name` in the default session's namespace: the first 16
/// bytes of `SHA-256("session-fair-test/" + name)` as a version-8 UUID.
pub fn hashed_id(name: &str) -> NodeId {
    hashed_id_in(NAME, name)
}

/// [`hashed_id`] in session `sid`'s namespace. The session name seeds the
/// hash, so a fixture built under another session id (the live tests' fresh
/// `session-fair-test-<uuid>`) shares no node or edge id with the default's
/// rows or with another run's: on a store whose ids are unique across
/// sessions, a second run cannot upsert into the first one's rows.
pub fn hashed_id_in(sid: &str, name: &str) -> NodeId {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{sid}/{name}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    NodeId(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

fn interaction_id_in(sid: &str, key: &str) -> NodeId {
    assert!(
        INTERACTIONS.iter().any(|(k, ..)| *k == key),
        "no interaction {key}"
    );
    hashed_id_in(sid, &format!("interaction/{key}"))
}

/// The id of the concept keyed `key` in the default session.
pub fn concept_id(key: &str) -> NodeId {
    concept_id_in(NAME, key)
}

/// The id of the concept keyed `key` in the fixture built under `sid`.
pub fn concept_id_in(sid: &str, key: &str) -> NodeId {
    assert!(CONCEPTS.iter().any(|(k, ..)| *k == key), "no concept {key}");
    hashed_id_in(sid, &format!("concept/{key}"))
}

fn interaction_time(key: &str) -> DateTime<Utc> {
    let (_, day, hour, ..) = INTERACTIONS
        .iter()
        .find(|(k, ..)| *k == key)
        .expect("interaction");
    at(*day, *hour)
}

fn origin_of(key: &str) -> &'static str {
    CONCEPTS
        .iter()
        .find(|(k, ..)| *k == key)
        .map(|(.., origin)| *origin)
        .expect("concept")
}

fn edge(
    sid: &SessionId,
    source: NodeId,
    target: NodeId,
    edge_type: EdgeType,
    when: DateTime<Utc>,
) -> Edge {
    Edge {
        event_time: None,
        id: NodeId::new(),
        session_id: sid.clone(),
        source,
        target,
        edge_type,
        weight: 1.0,
        reinforcements: 1,
        created_at: when,
        last_reinforced: when,
    }
}

/// The fixture as a graph, built through the graph's own write path.
/// `with_contradiction = false` leaves out every `Contradicts` edge.
pub fn build(with_contradiction: bool) -> Graph {
    build_in(NAME, with_contradiction)
}

/// [`build`] under session id `sid`, with every id seeded by `sid` (see
/// [`hashed_id_in`]).
pub fn build_in(sid: &str, with_contradiction: bool) -> Graph {
    let session_id = SessionId::from(sid);
    let mut g = Graph::new(session_id.clone());
    let mut previous = None;
    for (key, _, _, agent, prompt) in INTERACTIONS {
        let i = Interaction {
            event_time: None,
            id: interaction_id_in(sid, key),
            session_id: session_id.clone(),
            agent_id: AgentId::from(*agent),
            prompt_text: Some((*prompt).to_string()),
            previous_id: previous,
            created_at: interaction_time(key),
        };
        previous = Some(i.id);
        g.insert_interaction(i).unwrap();
    }
    let accessed = |key: &str| ACCESSES.iter().find(|(k, _)| *k == key).map(|(_, n)| *n);
    for (key, concept_type, content, origin) in CONCEPTS {
        let born = interaction_time(origin) + ChronoDuration::minutes(1);
        let agent = INTERACTIONS
            .iter()
            .find(|(k, ..)| k == origin)
            .map(|(.., agent, _)| *agent)
            .expect("origin");
        let c = Concept {
            id: concept_id_in(sid, key),
            session_id: session_id.clone(),
            content: (*content).to_string(),
            canonical_key: crate::graph::canonical::canonical_key(content, |_| None),
            concept_type: *concept_type,
            origin_interaction: interaction_id_in(sid, origin),
            origin_agent: AgentId::from(agent),
            created_at: born,
            access_count: accessed(key).unwrap_or(0),
            last_accessed: accessed(key).map(|_| at(19, 16)),
            gc_survived: 3,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        };
        g.insert_concept(c, interaction_id_in(sid, origin)).unwrap();
    }
    // Every concept-to-concept edge is written two minutes into its
    // source's interaction, the way record_action or derive stamps it.
    let stamped = |source: &str| interaction_time(origin_of(source)) + ChronoDuration::minutes(2);
    for (dependent, ruling) in DEPENDS {
        g.upsert_edge(edge(
            &session_id,
            concept_id_in(sid, dependent),
            concept_id_in(sid, ruling),
            EdgeType::Dependency,
            stamped(dependent),
        ))
        .unwrap();
    }
    for (action, file) in MODIFIES {
        g.upsert_edge(edge(
            &session_id,
            concept_id_in(sid, action),
            concept_id_in(sid, file),
            EdgeType::Causal,
            stamped(action),
        ))
        .unwrap();
    }
    for (parent, child) in CHILDREN {
        g.upsert_edge(edge(
            &session_id,
            concept_id_in(sid, parent),
            concept_id_in(sid, child),
            EdgeType::Hierarchical,
            stamped(parent),
        ))
        .unwrap();
    }
    for (a, b, ty) in ASSOCIATIONS {
        g.upsert_edge(edge(
            &session_id,
            concept_id_in(sid, a),
            concept_id_in(sid, b),
            *ty,
            stamped(a),
        ))
        .unwrap();
    }
    if with_contradiction {
        for (newer, older) in CONTRADICTIONS {
            g.upsert_edge(edge(
                &session_id,
                concept_id_in(sid, newer),
                concept_id_in(sid, older),
                EdgeType::Contradicts,
                stamped(newer),
            ))
            .unwrap();
        }
    }
    g
}

/// [`build`] as a deterministic snapshot: edge ids derived from each edge's
/// natural key, every list in a stable order, no process-local stamps.
pub fn snapshot(with_contradiction: bool) -> GraphSnapshot {
    snapshot_in(NAME, with_contradiction)
}

/// [`snapshot`] under session id `sid` (ids seeded by `sid`). The live
/// tests use a fresh `session-fair-test-<uuid>` per run, because an erased
/// session id is tombstoned and refuses every later flush.
pub fn snapshot_in(sid: &str, with_contradiction: bool) -> GraphSnapshot {
    let mut snap = build_in(sid, with_contradiction).snapshot();
    snap.created_at = Some(interaction_time("i01"));
    snap.mutation_epoch = 0;
    snap.edges.sort_by(|a, b| {
        (a.source.0, a.target.0, format!("{:?}", a.edge_type)).cmp(&(
            b.source.0,
            b.target.0,
            format!("{:?}", b.edge_type),
        ))
    });
    for e in snap.edges.iter_mut() {
        e.id = hashed_id_in(
            sid,
            &format!("edge/{}/{}/{:?}", e.source.0, e.target.0, e.edge_type),
        );
    }
    snap
}

/// Every concept's status at the evaluator's fixed point under `policy`,
/// keyed by concept key. Concepts not listed end at `None`.
pub fn expected(policy: PromotionPolicy) -> Vec<(&'static str, CanonizationStatus)> {
    use CanonizationStatus::{Candidate, Canonical, Venerable};
    let mut out = vec![
        // Each supported ruling: six dependents, 4 to 6 other interactions,
        // 11 to 13 days of spread over a 14-day denominator.
        ("lease_stays", Canonical),
        ("wedge", Canonical),
        ("identity", Canonical),
        ("async_ack", Canonical),
        // Five dependents: Stage 3 needs more than five.
        ("wedge_five", Venerable),
        // Three interactions inside two hours: Stage 2 coverage fails.
        ("identity_burst", Candidate),
        // KNOWN DEFECT, pinned on purpose: the short name "writer lease"
        // reaches Venerable because three actions cited the name instead of
        // the `lease_stays` ruling it stands for (design note section 2, the
        // name-resolution defect). #36 PR 4 lets new work cite the ruling
        // itself by id, but it redirects nothing already written, and a
        // citation by the alias's text still lands on the alias (0.5.0's text
        // rule; 0.6.0 has no names). The three `Dependency` edges here are
        // such history. PR 4 re-ran the fixture and no expected status
        // moved. #36 PR 5's alias preview shows the flip on an overlay
        // (`canon::dry_run::tests::the_alias_preview_flips_the_fixture_alias`:
        // with the three citations moved to `lease_stays`, this row ends at
        // None and the ruling carries nine dependents), but it applies
        // nothing, and this fixture is history: the row stays Venerable here
        // until a backfill is actually applied, if one ever is.
        ("lease_alias", Venerable),
        // #20, with supersession off (the default): both beliefs earn
        // Canonical and their contradictors stop at Venerable (blast 4).
        ("retry_forever", Canonical),
        ("retry_bounded", Venerable),
        ("ttl_fixed", Canonical),
        ("ttl_sliding", Venerable),
    ];
    match policy {
        PromotionPolicy::Swarm => {}
        // Solo's band admits this one as a Candidate (one other
        // interaction's dependents on a Constraint); Stage 2 stops it there.
        // The file rows stay None: since #36 PR 3 a `Causal` touch is not
        // recurrence, so files modified on many days no longer score.
        PromotionPolicy::Solo => out.push(("ack_single", Candidate)),
    }
    out
}

/// [`expected`] with `canonization_supersede` on (#20): `retry_forever` is
/// held at Venerable (its contradictor's support is all newer than its
/// evidence, so promoting it would supersede it at once); `ttl_fixed` and
/// `lease_stays` keep Canonical (older support, an assertion alone); no
/// contradictor is promoted by it. From a fixed point reached with
/// supersession off, turning it on demotes `retry_forever` and ends here too.
pub fn expected_superseding(policy: PromotionPolicy) -> Vec<(&'static str, CanonizationStatus)> {
    expected(policy)
        .into_iter()
        .map(|(key, status)| match key {
            "retry_forever" => (key, CanonizationStatus::Venerable),
            _ => (key, status),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Running the fair test (shared by the offline tests in
// `canon::fair_test_tests` and the live Postgres/Cockroach check in
// `store::pg::fair_test_live`).
// ---------------------------------------------------------------------------

/// The snapshot as a store batch.
pub fn batch_of(snap: &GraphSnapshot) -> MutationBatch {
    let mut batch = MutationBatch::new();
    for i in &snap.interactions {
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(i.clone()),
        });
    }
    for c in &snap.concepts {
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(c.clone()),
        });
    }
    for e in &snap.edges {
        batch.push(Mutation::UpsertEdge { edge: e.clone() });
    }
    batch
}

/// Run the evaluator from `snap` until `quiet` consecutive cycles commit
/// nothing, rescoring before every cycle like the daemon; returns the final
/// graph. The clock advances 61 s a cycle.
pub async fn fixed_point(
    store: &dyn GraphStore,
    snap: GraphSnapshot,
    params: &EvalParams,
    quiet: usize,
) -> Graph {
    fixed_point_from(
        store,
        Graph::from_snapshot(snap).unwrap(),
        params,
        quiet,
        now(),
    )
    .await
    .0
}

/// [`fixed_point`] from a graph and a clock, returning the clock it stopped
/// at too, so a second run (#20: supersession turned on after a fixed point
/// reached without it) continues forward in time.
pub async fn fixed_point_from(
    store: &dyn GraphStore,
    graph: Graph,
    params: &EvalParams,
    quiet: usize,
    start: DateTime<Utc>,
) -> (Graph, DateTime<Utc>) {
    let graph = RwLock::new(graph);
    let (tx, _rx) = crate::daemon::events::event_channel();
    let mut ev = Evaluator::new();
    let mut now = start;
    let mut idle = 0;
    for _ in 0..500 {
        let scores = {
            let g = graph.read();
            ScoreTable {
                epoch: g.epoch(),
                ranked: rescore(&g, &ScoringWeights::default()),
            }
        };
        let out = ev
            .eval_cycle(&graph, store, &scores, &tx, params, now, None)
            .await
            .unwrap();
        now += chrono::Duration::seconds(61);
        if out.is_empty() {
            idle += 1;
            if idle >= quiet {
                return (graph.into_inner(), now);
            }
        } else {
            idle = 0;
        }
    }
    panic!("no fixed point within 500 cycles");
}

/// Every concept's status in `g`, keyed by concept key; ids are resolved in
/// `g`'s own session, so a graph built with [`build_in`] works too.
pub fn statuses(g: &Graph) -> HashMap<&'static str, CanonizationStatus> {
    let sid = g.session_id().0.clone();
    CONCEPTS
        .iter()
        .map(|(key, ..)| {
            let status = match g.node(concept_id_in(&sid, key)) {
                Some(Node::Concept(c)) => c.canonization_status,
                _ => panic!("{key} is a concept"),
            };
            (*key, status)
        })
        .collect()
}

pub fn expected_map(policy: PromotionPolicy) -> HashMap<&'static str, CanonizationStatus> {
    map_of(expected(policy))
}

fn map_of(
    listed: Vec<(&'static str, CanonizationStatus)>,
) -> HashMap<&'static str, CanonizationStatus> {
    let listed: HashMap<_, _> = listed.into_iter().collect();
    CONCEPTS
        .iter()
        .map(|(key, ..)| {
            (
                *key,
                listed.get(key).copied().unwrap_or(CanonizationStatus::None),
            )
        })
        .collect()
}

pub fn assert_expected(
    got: &HashMap<&'static str, CanonizationStatus>,
    policy: PromotionPolicy,
    on: &str,
) {
    assert_statuses(got, &expected_map(policy), policy, on);
}

/// [`assert_expected`] against [`expected_superseding`] (#20).
pub fn assert_expected_superseding(
    got: &HashMap<&'static str, CanonizationStatus>,
    policy: PromotionPolicy,
    on: &str,
) {
    assert_statuses(got, &map_of(expected_superseding(policy)), policy, on);
}

fn assert_statuses(
    got: &HashMap<&'static str, CanonizationStatus>,
    want: &HashMap<&'static str, CanonizationStatus>,
    policy: PromotionPolicy,
    on: &str,
) {
    let mut wrong: Vec<String> = CONCEPTS
        .iter()
        .filter(|(key, ..)| got[key] != want[key])
        .map(|(key, ..)| format!("{key}: got {:?}, want {:?}", got[key], want[key]))
        .collect();
    wrong.sort();
    assert!(wrong.is_empty(), "{policy:?} on {on}: {wrong:#?}");
}
