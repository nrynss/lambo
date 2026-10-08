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
//!   session), so every token minted before the erase is stale and an
//!   unleased write (`None`) is refused too (`lease_permits_write`);
//! * `expires_at` = [`tombstone_expires_at`] (year 9999), so the expiry guard
//!   every acquire carries never lets anyone take the session over;
//! * `endpoint` = NULL; `acquired_at` = the store clock at the first erase.
//!
//! The row holds the session id and nothing else of the session's. It is what
//! makes "a later write to an erased session id is refused and does not
//! recreate it" true on every store, and it is what #32's implicit session
//! creation must respect. Reusing an erased id is a deliberate operator act:
//! delete the tombstone row (the same statement as
//! [`crate::store::lease::OPERATOR_OVERRIDE`]).
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

/// Rows removed per kind by one [`crate::store::GraphStore::erase_session`].
///
/// One field per session-keyed table, plus `vectors` (concepts that carried an
/// embedding; those rows are also counted in `concepts`) and `leases` (1 when
/// a holder's lease row was replaced by the tombstone).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
/// earlier tombstone, the eraser's own lease, or an expired lease; refuse a
/// live lease held by anyone else.
pub fn erase_gate(prior: Option<PriorLease<'_>>, eraser: &str) -> EraseGate {
    match prior {
        None => EraseGate::Proceed {
            replaces_lease: false,
        },
        Some(p) if is_erased_holder(p.holder) => EraseGate::Proceed {
            replaces_lease: false,
        },
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
    fn the_tombstone_expiry_is_year_9999() {
        assert_eq!(
            tombstone_expires_at().to_rfc3339(),
            "9999-12-31T23:59:59+00:00"
        );
    }
}
