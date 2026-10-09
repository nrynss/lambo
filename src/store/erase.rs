//! Session erasure (#23): the store-agnostic half.
//!
//! [`crate::store::GraphStore::erase_session`] removes every durable row keyed
//! to one session (nodes, edges, vectors, canonization history, synonyms,
//! reservations, write intents, flush stats, lease refusals and the `sessions`
//! row with its embedding contract) for account deletion. This module holds
//! what every adapter shares: the report, the lease gate, and the tombstone.
//!
//! # The tombstone, and why erasure keeps one row
//!
//! Erasure must fence. A writer whose lease lapsed (a crashed or starved
//! `serve`) still holds a fencing token in RAM, and a flush presenting it after
//! the erase must not bring the session back. If erasure deleted the
//! `session_leases` row too, that writer's next flush would find no lease row,
//! read the session as unleased, pass the fence and recreate everything it had
//! in memory. So erasure *replaces* the lease row with a tombstone instead:
//!
//! * `holder` = [`ERASED_HOLDER`], a value no [`LeaseHolder::token`] can
//!   produce (a real token always contains `@` and `#`);
//! * `current_token` = the prior token plus one (one on a never-leased
//!   session). A release keeps the row and its token (`store::lease`, #23
//!   review H2), so the prior token is the session's high-water mark and every
//!   token minted before the erase is below the tombstone's;
//! * `expires_at` = [`tombstone_expires_at`] (year 9999), so the expiry guard
//!   every acquire carries never lets anyone take the session over;
//! * `endpoint` = NULL; `acquired_at` = the store clock at the first erase.
//!
//! Every fence also refuses a tombstoned session **whatever token is
//! presented** ([`check_fence`], #23 review H1), the tombstone's own token
//! and `None` included. The token arithmetic above already refuses every
//! pre-erase writer; the holder check is the defense in depth that does not
//! rest on it. No writer can hold the tombstone's holder value: acquire,
//! refresh and erase refuse a reserved holder
//! ([`crate::store::lease::refuse_reserved_holder`]).
//!
//! The row holds the session id and nothing else of the session's. It is what
//! makes "a later write to an erased session id is refused and does not
//! recreate it" true on every store, and it is what #32's implicit session
//! creation must respect. Reusing an erased id is a deliberate operator act:
//! hand the row back as a released lease, keeping its fencing token, so a
//! writer cut off before the erase stays below the next holder's token:
//!
//! ```sql
//! UPDATE session_leases SET holder = 'lambo:released', expires_at = acquired_at,
//!   endpoint = NULL WHERE session_id = '<session>' AND holder = 'lambo:erased';
//! ```
//!
//! Never delete the row: that restarts the session's tokens (see
//! `store::lease`, "The token outlives every holder").
//!
//! # Who may erase
//!
//! The store takes the lease inside the erase transaction, on the same terms
//! as an acquire: it proceeds when the session has no lease row, when the lease
//! has expired, when the lease is already a tombstone, or when `eraser` itself
//! holds it. A live lease held by anyone else is reported as
//! [`EraseOutcome::Held`] and nothing is touched. Erasure never preempts a live
//! writer: the operator stops it (its `close()` flushes and releases), then
//! erases. This is [`erase_gate`].
//!
//! [`LeaseHolder::token`]: crate::store::lease::LeaseHolder::token

use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;

use super::lease::LeaseInfo;
use crate::types::{SessionId, StoreError};

/// The `session_leases.holder` value of an erased session's tombstone.
///
/// Never equal to a live holder's token: [`crate::store::lease::LeaseHolder::token`]
/// is `agent@host#pid` and always carries both separators, which this does
/// not.
pub const ERASED_HOLDER: &str = "lambo:erased";

/// `true` when a lease row's holder is the erasure tombstone.
pub fn is_erased_holder(holder: &str) -> bool {
    holder == ERASED_HOLDER
}

/// `true` when a lease row is the erasure tombstone.
pub fn is_tombstone(lease: &LeaseInfo) -> bool {
    is_erased_holder(&lease.holder)
}

/// The tombstone's expiry: the last second of year 9999, far enough that the
/// `expires_at <= now` takeover guard can never fire, and representable in
/// every store's timestamp type (SQLite's fixed-width text included).
pub fn tombstone_expires_at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(9999, 12, 31, 23, 59, 59)
        .single()
        .expect("9999-12-31T23:59:59Z is a valid UTC instant")
}

/// The stable refusal for any durable write to an erased session.
///
/// A [`StoreError::StaleWrite`]: the erase minted a newer fencing token than
/// any writer holds, so the write is exactly a stale one, and every caller
/// already treats that class as terminal (not retried, the writer is fenced).
/// The message says why, so an operator reading a refused flush is not sent
/// looking for a second writer that does not exist.
pub fn erased_session_error(session: &str) -> StoreError {
    StoreError::StaleWrite(format!(
        "session {session} was erased (lambo erase-session): writes to an erased session are \
         refused and never recreate it"
    ))
}

/// The fence's refusal for a write presenting `token` against a lease whose
/// current token is `current`.
///
/// `holder` is the lease row's holder when the caller read it: a tombstone
/// gets [`erased_session_error`], anything else the ordinary stale-token
/// message, unchanged from before #23.
pub fn fence_refusal(
    session: &str,
    token: Option<u64>,
    current: u64,
    holder: Option<&str>,
) -> StoreError {
    if holder.is_some_and(is_erased_holder) {
        return erased_session_error(session);
    }
    StoreError::StaleWrite(format!(
        "session {session}: presented token {token:?} is stale (lease token {current}) — \
         single-writer fence (GitHub issue #1)"
    ))
}

/// The store-side fence every durable write gate runs (#1, #23): may a write
/// presenting `token` proceed against a lease row whose current token is
/// `current` and whose holder is `holder`?
///
/// An erasure tombstone refuses **whatever token is presented** (#23 review
/// H1). The tombstone is minted above every earlier token, so the token rule
/// alone already refuses a pre-erase writer now that a release keeps the
/// token (H2); the holder check is the defense in depth that does not depend
/// on any token arithmetic being right, and it is decided on the typed holder
/// value rather than on a number. Anything else is the ordinary
/// [`crate::store::lease::lease_permits_write`] rule.
pub fn check_fence(
    session: &str,
    token: Option<u64>,
    current: u64,
    holder: &str,
) -> Result<(), StoreError> {
    if is_erased_holder(holder) {
        return Err(erased_session_error(session));
    }
    if !crate::store::lease::lease_permits_write(current, token) {
        return Err(fence_refusal(session, token, current, Some(holder)));
    }
    Ok(())
}

/// What a writer's background task calls once the store has refused one of
/// its writes because the session was erased (#23 review L2). `Memory` passes
/// one that latches its lease fence and the serve wake-up with
/// [`ERASED_HOLDER`] as the winner, so the handle stops serving reads of the
/// deleted data at once instead of at its next heartbeat.
pub type ErasedLatch = std::sync::Arc<dyn Fn() + Send + Sync>;

/// `true` when a store call for `session` failed because the session is
/// erased, decided on typed data: the lease row, re-read after the failure, is
/// the tombstone (never by matching the message). Any store error qualifies,
/// not only the fence's stale-write refusal: a canonization cycle over an
/// erased session usually fails earlier, on a read that finds the session
/// gone (`SessionNotFound`). A transient error over a live session, or a
/// lease read that fails, answers `false`, and the heartbeat still catches
/// the erasure.
pub async fn refused_as_erased(
    store: &dyn crate::store::GraphStore,
    session: &SessionId,
    _err: &StoreError,
) -> bool {
    matches!(store.read_lease(session).await, Ok(Some(row)) if is_tombstone(&row))
}

/// Rows removed per kind by one [`crate::store::GraphStore::erase_session`].
///
/// One field per session-keyed table, plus `vectors` (concepts that carried an
/// embedding; those rows are also counted in `concepts`) and `leases` (1 when
/// a holder's lease row was replaced by the tombstone).
///
/// `#[non_exhaustive]` (#23 review L7): a table added to the schema adds a
/// field here (the coverage tests force it), and that must not break a
/// downstream struct literal or exhaustive destructuring. Read the fields;
/// build one with `Default` and field assignment inside this crate. The JSON
/// shape is unaffected.
///
/// ```compile_fail
/// // Outside the crate a struct literal does not compile, even with `..`.
/// let _ = lambo::store::EraseCounts { sessions: 1, ..Default::default() };
/// ```
///
/// ```
/// // Reading it does.
/// fn total(c: &lambo::store::EraseCounts) -> u64 { c.rows() + c.vectors }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct EraseCounts {
    /// The `sessions` row, with the embedding contract and GC mark (0 or 1).
    pub sessions: u64,
    pub interactions: u64,
    pub concepts: u64,
    /// Concepts that carried an embedding vector (a subset of `concepts`).
    pub vectors: u64,
    pub edges: u64,
    pub synonyms: u64,
    pub canonization_events: u64,
    pub reservations: u64,
    /// Durable write intents, consumed or not (their payloads hold concept text).
    pub write_intents: u64,
    /// The writer's published flush stats row (0 or 1).
    pub session_stats: u64,
    pub lease_refusals: u64,
    /// A holder's lease row replaced by the tombstone (0 or 1).
    pub leases: u64,
}

impl EraseCounts {
    /// Rows removed across every table. `vectors` is excluded: it counts a
    /// column of rows already counted in `concepts`.
    pub fn rows(&self) -> u64 {
        self.sessions
            + self.interactions
            + self.concepts
            + self.edges
            + self.synonyms
            + self.canonization_events
            + self.reservations
            + self.write_intents
            + self.session_stats
            + self.lease_refusals
            + self.leases
    }

    /// Add `n` rows removed from the named table. The names are the SQL
    /// adapters' table names; an unknown one is an invariant violation (an
    /// erase statement list that names a table this report has no field for).
    pub fn add_table(&mut self, table: &str, n: u64) -> Result<(), StoreError> {
        let slot = match table {
            "sessions" => &mut self.sessions,
            "interactions" => &mut self.interactions,
            "concepts" => &mut self.concepts,
            "edges" => &mut self.edges,
            "synonyms" => &mut self.synonyms,
            "canonization_events" => &mut self.canonization_events,
            "reservations" => &mut self.reservations,
            "write_intents" => &mut self.write_intents,
            "session_stats" => &mut self.session_stats,
            "lease_refusals" => &mut self.lease_refusals,
            other => {
                return Err(StoreError::Invariant(format!(
                    "erase_session: no report field for table {other}"
                )))
            }
        };
        *slot += n;
        Ok(())
    }
}

/// What a completed erase removed. Returned only after the store committed,
/// so a caller (an account-deletion fan-out) may mark the target done.
///
/// `#[non_exhaustive]` (#23 review L7), like [`EraseCounts`]; built with
/// [`EraseReport::new`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct EraseReport {
    pub session: SessionId,
    /// `true` when nothing of the session was left to remove: a repeat of a
    /// completed erase, or an id that never held data. Either way the session
    /// is now erased and tombstoned.
    pub already_absent: bool,
    pub removed: EraseCounts,
    /// The tombstone's fencing token. Every token below it is refused.
    pub fence_token: u64,
}

impl EraseReport {
    /// Build the report from the counts; `already_absent` is derived, never
    /// set by an adapter, so every store answers it the same way.
    pub fn new(session: SessionId, removed: EraseCounts, fence_token: u64) -> Self {
        Self {
            session,
            already_absent: removed.rows() == 0,
            removed,
            fence_token,
        }
    }
}

/// Outcome of [`crate::store::GraphStore::erase_session`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EraseOutcome {
    /// Committed: every session-keyed row is gone and the tombstone is in place.
    Erased(EraseReport),
    /// Refused, nothing touched: a live lease is held by someone other than
    /// the eraser. Stop that writer (its close releases the lease), then retry.
    Held {
        /// The live holder's lease row.
        current: LeaseInfo,
        /// How long it has held the lease (store clock − `acquired_at`).
        age: Duration,
    },
}

/// The lease row as the erase transaction read it, under its lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorLease<'a> {
    pub holder: &'a str,
    /// `expires_at > now` on the store's clock.
    pub live: bool,
}

/// The erase transaction's decision on the lease it read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EraseGate {
    /// Take the session: write the tombstone and delete. `replaces_lease` is
    /// whether a holder's row (not an earlier tombstone) is being replaced.
    Proceed { replaces_lease: bool },
    /// A live lease held by someone else: touch nothing.
    Refuse,
}

/// Shared by every adapter so the rule cannot drift: proceed on no row, an
/// earlier tombstone, a released row, the eraser's own lease, or an expired
/// lease; refuse a live lease held by anyone else. A released row (holder
/// [`crate::store::lease::RELEASED_HOLDER`]) is no holder's lease, so
/// replacing it is not counted in [`EraseCounts::leases`].
pub fn erase_gate(prior: Option<PriorLease<'_>>, eraser: &str) -> EraseGate {
    match prior {
        None => EraseGate::Proceed {
            replaces_lease: false,
        },
        Some(p)
            if is_erased_holder(p.holder) || crate::store::lease::is_released_holder(p.holder) =>
        {
            EraseGate::Proceed {
                replaces_lease: false,
            }
        }
        Some(p) if p.live && p.holder != eraser => EraseGate::Refuse,
        Some(_) => EraseGate::Proceed {
            replaces_lease: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::lease::LeaseHolder;
    use crate::types::AgentId;

    #[test]
    fn no_real_holder_token_can_equal_the_tombstone() {
        let h = LeaseHolder {
            agent: AgentId::new(ERASED_HOLDER),
            pid: 0,
            host: ERASED_HOLDER.into(),
            endpoint: None,
        };
        assert!(!is_erased_holder(&h.token()));
        assert!(!ERASED_HOLDER.contains('@') && !ERASED_HOLDER.contains('#'));
    }

    #[test]
    fn the_gate_refuses_only_a_live_lease_held_by_someone_else() {
        let me = "eraser@h#1";
        let live = |holder| Some(PriorLease { holder, live: true });
        let lapsed = |holder| {
            Some(PriorLease {
                holder,
                live: false,
            })
        };
        assert_eq!(
            erase_gate(None, me),
            EraseGate::Proceed {
                replaces_lease: false
            }
        );
        assert_eq!(erase_gate(live("serve@h#2"), me), EraseGate::Refuse);
        assert_eq!(
            erase_gate(lapsed("serve@h#2"), me),
            EraseGate::Proceed {
                replaces_lease: true
            }
        );
        assert_eq!(
            erase_gate(live(me), me),
            EraseGate::Proceed {
                replaces_lease: true
            }
        );
        // A released row is nobody's lease: proceed, and count no lease.
        assert_eq!(
            erase_gate(lapsed(crate::store::lease::RELEASED_HOLDER), me),
            EraseGate::Proceed {
                replaces_lease: false
            }
        );
        // A tombstone is live forever by construction and must not refuse the
        // repeat that a resumable fan-out depends on.
        assert_eq!(
            erase_gate(live(ERASED_HOLDER), me),
            EraseGate::Proceed {
                replaces_lease: false
            }
        );
    }

    #[test]
    fn already_absent_is_derived_from_the_counts_not_set_by_an_adapter() {
        let sid = SessionId::new("s");
        assert!(EraseReport::new(sid.clone(), EraseCounts::default(), 1).already_absent);
        let some = EraseCounts {
            session_stats: 1,
            ..Default::default()
        };
        assert!(!EraseReport::new(sid.clone(), some, 1).already_absent);
        // vectors alone is not a row: it is a column of `concepts`.
        let vectors_only = EraseCounts {
            vectors: 3,
            ..Default::default()
        };
        assert!(EraseReport::new(sid, vectors_only, 1).already_absent);
    }

    #[test]
    fn every_counted_table_has_a_field_and_an_unknown_one_is_refused() {
        let mut c = EraseCounts::default();
        for t in [
            "sessions",
            "interactions",
            "concepts",
            "edges",
            "synonyms",
            "canonization_events",
            "reservations",
            "write_intents",
            "session_stats",
            "lease_refusals",
        ] {
            c.add_table(t, 1).unwrap();
        }
        assert_eq!(c.rows(), 10);
        assert!(matches!(
            c.add_table("brand_new_table", 1),
            Err(StoreError::Invariant(_))
        ));
    }

    #[test]
    fn a_tombstone_refusal_says_erased_and_an_ordinary_one_is_unchanged() {
        let erased = fence_refusal("s", Some(3), 4, Some(ERASED_HOLDER)).to_string();
        assert!(erased.contains("was erased"), "{erased}");
        let stale = fence_refusal("s", Some(3), 4, Some("other@h#1")).to_string();
        assert_eq!(
            stale,
            "stale write (fencing token): session s: presented token Some(3) is stale \
             (lease token 4) — single-writer fence (GitHub issue #1)"
        );
        assert_eq!(fence_refusal("s", None, 4, None).to_string(), {
            "stale write (fencing token): session s: presented token None is stale \
             (lease token 4) — single-writer fence (GitHub issue #1)"
        });
    }

    #[test]
    fn the_fence_refuses_a_tombstone_whatever_the_token() {
        for token in [None, Some(1), Some(7), Some(8), Some(u64::MAX)] {
            let err = check_fence("s", token, 7, ERASED_HOLDER).expect_err("erased");
            assert!(err.to_string().contains("was erased"), "{token:?}: {err}");
        }
        assert!(check_fence("s", Some(7), 7, "w@h#1").is_ok());
        assert!(check_fence("s", Some(8), 7, "w@h#1").is_ok());
        assert!(matches!(
            check_fence("s", Some(6), 7, "w@h#1"),
            Err(StoreError::StaleWrite(m)) if m.contains("is stale")
        ));
        assert!(check_fence("s", None, 7, "w@h#1").is_err());
        assert!(check_fence("s", None, 0, "w@h#1").is_ok());
    }

    #[test]
    fn the_tombstone_expiry_is_year_9999() {
        assert_eq!(
            tombstone_expires_at().to_rfc3339(),
            "9999-12-31T23:59:59+00:00"
        );
    }
}

/// Called by an adapter's erase transaction after each step, with the step's
/// name (`"vectors"` or a table name). Production passes [`no_fault`]; the
/// crash-midway tests pass one that fails at step N, which drops the
/// transaction exactly as a crash or a lost connection would.
pub type EraseStepHook<'a> = &'a (dyn Fn(&str) -> Result<(), StoreError> + Send + Sync);

/// The production [`EraseStepHook`]: every step proceeds.
pub fn no_fault(_step: &str) -> Result<(), StoreError> {
    Ok(())
}

/// Shared fixtures for every adapter's erase tests: one batch that writes a
/// row into every table a `Mutation` can reach, the counts it should produce,
/// and fault hooks. Rows with no `Mutation` kind (synonyms, reservations) and
/// the lease-side tables are written by each adapter's test, since that is
/// adapter-specific.
#[cfg(test)]
// The adapter tests that call these helpers are compiled with SQLite, the
// in-memory store or Postgres; a Cockroach-only test build has none of them.
#[cfg_attr(
    not(any(
        feature = "store-sqlite",
        feature = "store-memory",
        feature = "store-postgres"
    )),
    allow(dead_code)
)]
pub(crate) mod testkit {
    #[cfg(any(
        feature = "store-sqlite",
        feature = "store-postgres",
        all(feature = "store-cockroach", feature = "fixtures")
    ))]
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Utc;

    use super::EraseCounts;
    use crate::types::{
        AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType,
        EmbeddingContract, Interaction, Mutation, MutationBatch, Node, NodeId, SessionId,
        WriteIntent, WriteIntentPayload,
    };

    /// The contract the planted vectors are written under.
    pub(crate) fn contract(dim: usize) -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: Some("erase-test".into()),
            dim,
        }
    }

    fn concept(
        sid: &SessionId,
        id: NodeId,
        origin: NodeId,
        content: &str,
        embedding: Option<Vec<f32>>,
    ) -> Mutation {
        // #22: a vectored concept carries a supplied-vector source (with its
        // image digest), so the erase census covers that column too. It lives
        // on the concept row, so no table or count changes.
        let embedding_source = embedding
            .as_ref()
            .map(|_| crate::store::embedding_source_testkit::server_source());
        Mutation::UpsertNode {
            node: Node::Concept(Concept {
                id,
                session_id: sid.clone(),
                content: content.into(),
                canonical_key: content.to_lowercase(),
                concept_type: ConceptType::Entity,
                origin_interaction: origin,
                origin_agent: AgentId::new("erase-test"),
                created_at: Utc::now(),
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding,
                human_confirmed: 0,
                embedding_source,
                chunk_group_id: None,
            }),
        }
    }

    fn derives(sid: &SessionId, source: NodeId, target: NodeId) -> Mutation {
        let ts = Utc::now();
        Mutation::UpsertEdge {
            edge: Edge {
                id: NodeId::new(),
                session_id: sid.clone(),
                source,
                target,
                edge_type: EdgeType::Derives,
                weight: 1.0,
                reinforcements: 0,
                created_at: ts,
                last_reinforced: ts,
                event_time: None,
            },
        }
    }

    /// A batch that writes the session row (with contract and root goal), two
    /// chained interactions, two concepts (one carrying a `dim`-wide vector
    /// and its #22 `embedding_source`), two edges, a canonization transition,
    /// a read access and a durable write intent. Concept text is unique per
    /// call, so two sessions planted in one store never collide on a
    /// canonical key.
    pub(crate) fn planted_batch(sid: &SessionId, dim: usize) -> MutationBatch {
        let ts = Utc::now();
        let (i1, i2, c1, c2) = (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
        let interaction = |id, previous_id| Mutation::UpsertNode {
            node: Node::Interaction(Interaction {
                event_time: None,
                id,
                session_id: sid.clone(),
                agent_id: AgentId::new("erase-test"),
                prompt_text: Some("personal history".into()),
                previous_id,
                created_at: ts,
            }),
        };
        let vector: Vec<f32> = (0..dim).map(|i| ((i % 7) as f32) + 1.0).collect();
        MutationBatch {
            mutation_epoch: 3,
            gc_mark: Default::default(),
            mutations: vec![
                Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(contract(dim)),
                },
                Mutation::SetRootGoal {
                    session_id: sid.clone(),
                    goal: Some(serde_json::json!({"goal": "dress well"})),
                },
                interaction(i1, None),
                interaction(i2, Some(i1)),
                concept(sid, c1, i1, &format!("likes linen {c1}"), Some(vector)),
                concept(sid, c2, i2, &format!("size m {c2}"), None),
                derives(sid, i1, c1),
                derives(sid, i2, c2),
                Mutation::CanonizationTransition {
                    event: CanonizationEvent {
                        id: NodeId::new(),
                        session_id: sid.clone(),
                        node_id: c1,
                        from_status: CanonizationStatus::None,
                        to_status: CanonizationStatus::Candidate,
                        blast_radius: Some(1),
                        last_demotion_time: None,
                        occurred_at: ts,
                    },
                },
                Mutation::RecordAccess {
                    session_id: sid.clone(),
                    id: c1,
                    access_count: 2,
                    last_accessed: ts,
                },
                Mutation::PutWriteIntent {
                    intent: WriteIntent {
                        session_id: sid.clone(),
                        receipt: format!("erase-test-{c1}"),
                        agent: AgentId::new("erase-test"),
                        interaction: i2,
                        lane_seq: 1,
                        issued_ms: ts.timestamp_millis(),
                        payload: WriteIntentPayload::Derive {
                            concepts: vec![("prefers navy".into(), ConceptType::Entity)],
                            pairs: vec![],
                        },
                        created_at: ts,
                        outcome: None,
                    },
                },
            ],
        }
    }

    /// What erasing one [`planted_batch`] removes from the mutation-reachable
    /// tables. An adapter test adds the rows it planted by other means.
    pub(crate) fn planted_counts() -> EraseCounts {
        EraseCounts {
            sessions: 1,
            interactions: 2,
            concepts: 2,
            vectors: 1,
            edges: 2,
            canonization_events: 1,
            write_intents: 1,
            ..Default::default()
        }
    }

    /// #23 review L4: edges in another session that point at the erased
    /// session's nodes go with them, like `DeleteNode` removes a node's
    /// incident edges in every session, and are counted in `edges`. The other
    /// session's own rows stay. `a` (erased) and `b` must be fresh ids.
    pub(crate) async fn check_erase_removes_cross_session_edges(
        store: &dyn crate::store::GraphStore,
        a: &SessionId,
        b: &SessionId,
    ) {
        let (ia, ca, ib1, ib2) = (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
        let interaction = |sid: &SessionId, id: NodeId| Mutation::UpsertNode {
            node: Node::Interaction(Interaction {
                event_time: None,
                id,
                session_id: sid.clone(),
                agent_id: AgentId::new("erase-test"),
                prompt_text: Some("x".into()),
                previous_id: None,
                created_at: Utc::now(),
            }),
        };
        let batch = |mutations| MutationBatch {
            mutations,
            ..Default::default()
        };
        store
            .flush(
                &batch(vec![
                    interaction(a, ia),
                    concept(a, ca, ia, &format!("erased fact {ca}"), None),
                ]),
                None,
            )
            .await
            .expect("plant a");
        store
            .flush(
                &batch(vec![
                    interaction(b, ib1),
                    interaction(b, ib2),
                    derives(b, ib1, ib2),
                    // B's edge onto A's concept: B's row, A's node.
                    derives(b, ib1, ca),
                ]),
                None,
            )
            .await
            .expect("plant b");

        let report = match store
            .erase_session(a, &crate::store::lease::testkit::holder("eraser", 1))
            .await
            .expect("erase")
        {
            super::EraseOutcome::Erased(r) => r,
            held => panic!("nothing holds a: {held:?}"),
        };
        assert_eq!(report.removed.edges, 1, "the cross-session edge is counted");
        assert_eq!(report.removed.concepts, 1);
        assert_eq!(report.removed.interactions, 1);

        let kept = store.load_session(b).await.expect("b is untouched");
        assert_eq!(kept.interactions.len(), 2, "b's nodes stay");
        assert_eq!(
            kept.edges
                .iter()
                .map(|e| (e.source, e.target))
                .collect::<Vec<_>>(),
            vec![(ib1, ib2)],
            "only b's edge onto the erased node goes"
        );
    }

    /// The #23 review's H1 scenario, on any store, plus the defense in depth
    /// behind it.
    ///
    /// `serve` X holds token t1 and its lease lapses; a CLI verb Y takes the
    /// session over, writes and closes cleanly (a release); the operator
    /// erases. Zombie X's flush and canonization with t1 must be refused with
    /// the erased error and recreate nothing. Before the H2 fix the release
    /// deleted the row, the tombstone was minted at token 1 and X's write
    /// passed. Defense in depth: every fence refuses a tombstoned session
    /// whatever token is presented, including the tombstone's own token and
    /// one above it, so no token arithmetic can let a write through.
    ///
    /// `sid` must be a fresh id (the pg legs share a cluster).
    pub(crate) async fn check_erase_after_release_fences(
        store: &dyn crate::store::GraphStore,
        sid: &SessionId,
    ) {
        use std::time::Duration;

        use crate::store::lease::testkit::{holder, interaction_batch};
        use crate::store::lease::LeaseOutcome;
        use crate::types::StoreError;

        let x = holder("serve-x", 1);
        let y = holder("derive-y", 2);
        let LeaseOutcome::Acquired(xl) = store
            .acquire_lease(sid, &x, Duration::from_secs(1))
            .await
            .expect("acquire x")
        else {
            panic!("x takes the fresh session");
        };
        store
            .flush(&interaction_batch(sid, "x before"), Some(xl.token))
            .await
            .expect("x writes");
        tokio::time::sleep(Duration::from_millis(1_300)).await;
        let LeaseOutcome::Acquired(yl) = store
            .acquire_lease(sid, &y, Duration::from_secs(60))
            .await
            .expect("acquire y")
        else {
            panic!("y takes the lapsed session over");
        };
        store
            .flush(&interaction_batch(sid, "y"), Some(yl.token))
            .await
            .expect("y writes");
        store
            .release_lease(sid, &y)
            .await
            .expect("y closes cleanly");

        let report = match store
            .erase_session(sid, &holder("lambo-erase-session", 3))
            .await
            .expect("erase")
        {
            super::EraseOutcome::Erased(r) => r,
            held => panic!("nothing live holds the session: {held:?}"),
        };
        assert!(!report.already_absent);
        assert!(
            report.fence_token > yl.token && yl.token > xl.token,
            "the tombstone is minted above every earlier token: x {} y {} tomb {}",
            xl.token,
            yl.token,
            report.fence_token
        );

        let erased = |res: Result<(), StoreError>, what: &str| match res {
            Err(StoreError::StaleWrite(m)) if m.contains("was erased") => {}
            other => panic!("{what} must be refused as erased, got {other:?}"),
        };
        for token in [
            Some(xl.token),
            Some(yl.token),
            None,
            Some(report.fence_token),
            Some(report.fence_token + 1),
            Some(u64::MAX >> 1),
        ] {
            erased(
                store
                    .flush(&interaction_batch(sid, "zombie x"), token)
                    .await,
                &format!("a flush presenting {token:?}"),
            );
            erased(
                store
                    .record_canonization(
                        &CanonizationEvent {
                            id: NodeId::new(),
                            session_id: sid.clone(),
                            node_id: NodeId::new(),
                            from_status: CanonizationStatus::None,
                            to_status: CanonizationStatus::Candidate,
                            blast_radius: Some(1),
                            last_demotion_time: None,
                            occurred_at: Utc::now(),
                        },
                        token,
                    )
                    .await,
                &format!("a canonization presenting {token:?}"),
            );
        }
        assert!(
            matches!(
                store.load_session(sid).await,
                Err(StoreError::SessionNotFound(_))
            ),
            "nothing of the erased session may come back"
        );
    }

    /// A fault hook that fails the `n`th step it sees (0-based) and every
    /// later one, like a connection lost mid-erase. Only the SQL adapters
    /// have steps: `MemoryStore` erases in one critical section.
    #[cfg(any(
        feature = "store-sqlite",
        feature = "store-postgres",
        all(feature = "store-cockroach", feature = "fixtures")
    ))]
    pub(crate) struct FailAt {
        pub(crate) n: usize,
        seen: AtomicUsize,
    }

    #[cfg(any(
        feature = "store-sqlite",
        feature = "store-postgres",
        all(feature = "store-cockroach", feature = "fixtures")
    ))]
    impl FailAt {
        pub(crate) fn new(n: usize) -> Self {
            Self {
                n,
                seen: AtomicUsize::new(0),
            }
        }

        pub(crate) fn step(&self, step: &str) -> Result<(), crate::types::StoreError> {
            // `Invariant`, not `Backend`: the pg family's `tx_retry` replays
            // a `Backend` error, and a crash test must fail exactly once.
            if self.seen.fetch_add(1, Ordering::SeqCst) >= self.n {
                return Err(crate::types::StoreError::Invariant(format!(
                    "injected failure after erase step {step}"
                )));
            }
            Ok(())
        }

        /// How many steps ran (to bound a loop over every failure point).
        pub(crate) fn seen(&self) -> usize {
            self.seen.load(Ordering::SeqCst)
        }
    }
}
