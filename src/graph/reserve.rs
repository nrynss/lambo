//! Spec §11 soft-lock reservation **policy** (T2.7).
//!
//! Sits on top of T2.1's storage: [`Graph::set_reservation`] /
//! [`Graph::clear_reservation`] / [`Graph::reservation`] / [`Graph::reservations`]
//! are RAM-local (no `Mutation` kind exists; they round-trip via
//! [`GraphSnapshot`] only). This module owns the *rules* — expiry, same-agent
//! extend, cross-agent deny/takeover — and deliberately adds nothing to `Graph`.
//!
//! ## Policy (spec §11, v0.6.0 §10.3.3)
//!
//! * Advisory: reservations never block writes; they are visible in other agents'
//!   recall output and expire after `ttl`.
//! * Same-agent re-reservation **extends**: `expires_at` is replaced with
//!   `now + ttl`; node and agent are unchanged.
//! * Cross-agent re-reservation of a **live** lock returns
//!   [`LamboError::Conflict`] naming the holder and expiry.
//! * Cross-agent re-reservation of an **expired** lock is a takeover: the new
//!   agent's reservation replaces the dead one.
//!
//! ## Time discipline
//!
//! Every entry point takes `now: DateTime<Utc>` explicitly — this module never
//! calls `Utc::now()`, so tests mock time deterministically. Expiry is
//! half-open: a reservation is active iff `now < expires_at` (at
//! `now == expires_at` it is expired).

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::graph::Graph;
use crate::types::{AgentId, LamboError, NodeId, Reservation, StoreError};

/// Typed failure for an unusable TTL: either it does not fit
/// [`chrono::Duration`] (whose range is ±~292,000 years in microseconds; a
/// [`Duration`] beyond that, e.g. `u64::MAX` seconds, is rejected rather than
/// silently clamped) or it fits but `now + ttl` would overflow
/// [`DateTime<Utc>`](chrono::DateTime) (whose span is ±~262,000 years) — the
/// add is checked, so this error is returned instead of a panic. Reachable
/// through [`LamboError::Other`]; downcast with
/// [`anyhow::Error::downcast_ref`](https://docs.rs/anyhow/latest/anyhow/struct.Error.html#method.downcast_ref).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ttl {ttl:?} is out of chrono's duration range")]
pub struct ReserveError {
    ttl: Duration,
}

impl From<ReserveError> for LamboError {
    fn from(e: ReserveError) -> Self {
        LamboError::Other(e.into())
    }
}

/// Acquire or extend a soft lock on `node` for `agent`.
///
/// * Missing node -> `StoreError::NotFound`.
/// * No reservation -> create, `expires_at = now + ttl`.
/// * Same agent -> extend (replace `expires_at` with `now + ttl`; node and
///   agent unchanged).
/// * Cross-agent, unexpired -> `LamboError::Conflict` naming the holder and
///   expiry; the existing reservation is left untouched.
/// * Cross-agent, expired -> takeover (new reservation for `agent`).
pub fn reserve(
    graph: &mut Graph,
    node: NodeId,
    agent: &AgentId,
    ttl: Duration,
    now: DateTime<Utc>,
) -> Result<Reservation, LamboError> {
    if graph.node(node).is_none() {
        return Err(LamboError::Store(StoreError::NotFound(format!(
            "node {node} not found"
        ))));
    }
    let ttl_std = ttl;
    let ttl = chrono::Duration::from_std(ttl_std).map_err(|_| ReserveError { ttl: ttl_std })?;
    // `now + ttl` would panic on overflow: chrono's `Add<TimeDelta>` for
    // `DateTime` unwraps `checked_add_signed`, and `from_std` alone admits
    // TTLs that fit TimeDelta (~±292k years) but overflow `DateTime<Utc>`
    // (~±262k years). Route that band through the typed error too — a
    // reservation policy must never panic on a caller-supplied TTL.
    let expires_at = now
        .checked_add_signed(ttl)
        .ok_or_else(|| ReserveError { ttl: ttl_std })?;

    // Snapshot the holder before mutating: the decision below must not hold a
    // borrow of `graph` across `set_reservation`.
    let held = graph.reservation(node).map(|r| (r.agent_id.clone(), r.expires_at));

    let reservation = match held {
        // No lock yet — create.
        None => Reservation {
            session_id: graph.session_id().clone(),
            node_id: node,
            agent_id: agent.clone(),
            expires_at,
        },
        // Same agent — extend (node + agent unchanged, expiry replaced).
        Some((holder, _)) if holder == *agent => Reservation {
            session_id: graph.session_id().clone(),
            node_id: node,
            agent_id: agent.clone(),
            expires_at,
        },
        // Cross-agent, still live — deny; existing lock untouched.
        Some((holder, expiry)) if now < expiry => {
            return Err(LamboError::Conflict(format!(
                "node {node} already reserved by {holder} until {expiry}"
            )));
        }
        // Cross-agent, expired — takeover.
        Some(_) => Reservation {
            session_id: graph.session_id().clone(),
            node_id: node,
            agent_id: agent.clone(),
            expires_at,
        },
    };

    graph.set_reservation(reservation.clone());
    Ok(reservation)
}

/// Release the soft lock on `node` held by `agent`.
///
/// * Owner -> clears the reservation.
/// * Non-owner -> `LamboError::Conflict`.
/// * No reservation -> `StoreError::NotFound`.
pub fn release(graph: &mut Graph, node: NodeId, agent: &AgentId) -> Result<(), LamboError> {
    let held = graph.reservation(node).map(|r| r.agent_id.clone());
    match held {
        None => Err(LamboError::Store(StoreError::NotFound(format!(
            "no reservation on node {node}"
        )))),
        Some(holder) if holder == *agent => {
            graph.clear_reservation(node);
            Ok(())
        }
        Some(holder) => Err(LamboError::Conflict(format!(
            "node {node} is reserved by {holder}, not by {agent}"
        ))),
    }
}

/// The live reservation on `node`, if any — `None` once it has expired.
/// Active iff `now < expires_at`.
pub fn active_reservation<'a>(
    graph: &'a Graph,
    node: NodeId,
    now: DateTime<Utc>,
) -> Option<&'a Reservation> {
    graph.reservation(node).filter(|r| now < r.expires_at)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::types::{Interaction, SessionId};

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn agent(name: &str) -> AgentId {
        AgentId::from(name)
    }

    fn uid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(0, u))
    }

    /// Fresh graph with one interaction node; returns (graph, node id).
    fn graph_with_node() -> (Graph, NodeId) {
        let mut g = Graph::new(sid());
        let i = Interaction {
            id: uid(1),
            session_id: sid(),
            agent_id: agent("agent-a"),
            prompt_text: Some("prompt".into()),
            previous_id: None,
            created_at: ts(0),
        };
        let id = i.id;
        g.insert_interaction(i).unwrap();
        (g, id)
    }

    #[test]
    fn fresh_reserve_creates_with_expiry() {
        let (mut g, n) = graph_with_node();
        let r = reserve(&mut g, n, &agent("alice"), Duration::from_secs(30), ts(0)).unwrap();
        assert_eq!(r.node_id, n);
        assert_eq!(r.agent_id, agent("alice"));
        assert_eq!(r.session_id, sid());
        assert_eq!(r.expires_at, ts(0) + chrono::Duration::seconds(30));
        assert_eq!(g.reservations().len(), 1);
        assert_eq!(g.reservation(n), Some(&r));
    }

    #[test]
    fn same_agent_extends_advancing_expiry() {
        let (mut g, n) = graph_with_node();
        let first = reserve(&mut g, n, &agent("alice"), Duration::from_secs(30), ts(0)).unwrap();
        assert_eq!(first.expires_at, ts(0) + chrono::Duration::seconds(30));

        // Re-reserve 10 min later with a longer ttl: expiry advances, agent and
        // node unchanged, still exactly one reservation.
        let second =
            reserve(&mut g, n, &agent("alice"), Duration::from_secs(60), ts(10)).unwrap();
        assert_eq!(second.expires_at, ts(10) + chrono::Duration::seconds(60));
        assert_eq!(second.agent_id, agent("alice"));
        assert_eq!(second.node_id, n);
        assert_eq!(g.reservations().len(), 1);
        assert_eq!(g.reservation(n), Some(&second));
    }

    #[test]
    fn cross_agent_denied_while_live() {
        let (mut g, n) = graph_with_node();
        let original = reserve(&mut g, n, &agent("alice"), Duration::from_secs(60), ts(0)).unwrap();

        // 30 s into a 60 s lock: still live, bob must be denied.
        let err = reserve(
            &mut g,
            n,
            &agent("bob"),
            Duration::from_secs(60),
            ts(0) + chrono::Duration::seconds(30),
        )
        .unwrap_err();
        match err {
            LamboError::Conflict(msg) => {
                assert!(msg.contains("alice"), "message should name holder: {msg}");
                assert!(msg.contains(&original.expires_at.to_string()), "message should name expiry: {msg}");
                assert!(msg.contains("bob") || msg.contains("reserved"), "message should be about the node: {msg}");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        // Existing reservation untouched by the denial.
        assert_eq!(g.reservation(n), Some(&original));
        assert_eq!(g.reservations().len(), 1);
    }

    #[test]
    fn cross_agent_takeover_after_expiry() {
        let (mut g, n) = graph_with_node();
        reserve(&mut g, n, &agent("alice"), Duration::from_secs(30), ts(0)).unwrap();

        // 31 min later the 30 s lock is long dead; bob takes over.
        let r = reserve(&mut g, n, &agent("bob"), Duration::from_secs(120), ts(31)).unwrap();
        assert_eq!(r.agent_id, agent("bob"));
        assert_eq!(r.expires_at, ts(31) + chrono::Duration::seconds(120));
        assert_eq!(g.reservations().len(), 1);
        assert_eq!(g.reservation(n), Some(&r));
    }

    #[test]
    fn release_by_owner_clears() {
        let (mut g, n) = graph_with_node();
        reserve(&mut g, n, &agent("alice"), Duration::from_secs(60), ts(0)).unwrap();

        release(&mut g, n, &agent("alice")).unwrap();
        assert_eq!(g.reservation(n), None);
        assert!(g.reservations().is_empty());
    }

    #[test]
    fn release_by_non_owner_errors() {
        let (mut g, n) = graph_with_node();
        let original = reserve(&mut g, n, &agent("alice"), Duration::from_secs(60), ts(0)).unwrap();

        let err = release(&mut g, n, &agent("bob")).unwrap_err();
        match err {
            LamboError::Conflict(msg) => assert!(msg.contains("alice") && msg.contains("bob"), "message should name both agents: {msg}"),
            other => panic!("expected Conflict, got {other:?}"),
        }

        // Non-owner release must not clear the lock.
        assert_eq!(g.reservation(n), Some(&original));
    }

    #[test]
    fn release_of_absent_reservation_errors() {
        let (mut g, n) = graph_with_node();
        let err = release(&mut g, n, &agent("alice")).unwrap_err();
        match err {
            LamboError::Store(StoreError::NotFound(msg)) => assert!(msg.contains("reservation"), "message should mention reservation: {msg}"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn expired_reservation_invisible_to_active_reservation() {
        let (mut g, n) = graph_with_node();
        reserve(&mut g, n, &agent("alice"), Duration::from_secs(30), ts(0)).unwrap();

        // Live while now < expires_at.
        assert!(active_reservation(&g, n, ts(0) + chrono::Duration::seconds(29)).is_some());
        // Exactly at expiry the lock is dead (half-open interval).
        assert!(active_reservation(&g, n, ts(0) + chrono::Duration::seconds(30)).is_none());
        // And stays dead.
        assert!(active_reservation(&g, n, ts(31)).is_none());
    }

    #[test]
    fn reserve_on_missing_node_errors() {
        let (mut g, _) = graph_with_node();
        let missing = uid(999);
        let err = reserve(&mut g, missing, &agent("alice"), Duration::from_secs(30), ts(0)).unwrap_err();
        match err {
            LamboError::Store(StoreError::NotFound(msg)) => {
                assert!(msg.contains(&missing.to_string()), "message should name the node: {msg}")
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert_eq!(g.reservation(missing), None);
    }

    #[test]
    fn out_of_range_ttl_is_typed_error() {
        let (mut g, n) = graph_with_node();
        let err = reserve(&mut g, n, &agent("alice"), Duration::from_secs(u64::MAX), ts(0)).unwrap_err();
        match err {
            LamboError::Other(e) => assert!(
                e.downcast_ref::<ReserveError>().is_some(),
                "expected typed ReserveError, got {e:?}"
            ),
            other => panic!("expected Other(ReserveError), got {other:?}"),
        }
        // Nothing was stored.
        assert_eq!(g.reservation(n), None);
    }

    #[test]
    fn takeover_at_exact_expiry_boundary() {
        let (mut g, n) = graph_with_node();
        reserve(&mut g, n, &agent("alice"), Duration::from_secs(60), ts(0)).unwrap();

        // At exactly `now == expires_at` the lock is dead (half-open interval):
        // the cross-agent attempt is a takeover, not a deny.
        let r = reserve(&mut g, n, &agent("bob"), Duration::from_secs(60), ts(1)).unwrap();
        assert_eq!(r.agent_id, agent("bob"));
        assert_eq!(g.reservations().len(), 1);
        assert_eq!(g.reservation(n), Some(&r));
    }

    #[test]
    fn same_agent_extends_still_live_lock() {
        let (mut g, n) = graph_with_node();
        reserve(&mut g, n, &agent("alice"), Duration::from_secs(120), ts(0)).unwrap();

        // 30 s into a 120 s lock it is still live: the same agent extends,
        // expiry advances from the new `now`, still exactly one reservation.
        let r = reserve(
            &mut g,
            n,
            &agent("alice"),
            Duration::from_secs(60),
            ts(0) + chrono::Duration::seconds(30),
        )
        .unwrap();
        assert_eq!(r.agent_id, agent("alice"));
        assert_eq!(r.node_id, n);
        assert_eq!(r.expires_at, ts(0) + chrono::Duration::seconds(90));
        assert_eq!(g.reservations().len(), 1);
        assert_eq!(g.reservation(n), Some(&r));
    }

    #[test]
    fn active_reservation_missing_node_is_none() {
        let (g, _) = graph_with_node();
        // No reservation on the node -> None, and no panic.
        assert_eq!(active_reservation(&g, uid(999), ts(0)), None);
    }

    #[test]
    fn release_then_re_reserve_by_other_agent() {
        let (mut g, n) = graph_with_node();
        reserve(&mut g, n, &agent("alice"), Duration::from_secs(60), ts(0)).unwrap();
        release(&mut g, n, &agent("alice")).unwrap();
        assert_eq!(g.reservation(n), None);

        // The slot is free: another agent can reserve immediately, with no
        // takeover/expiry dance required.
        let r = reserve(&mut g, n, &agent("bob"), Duration::from_secs(60), ts(0)).unwrap();
        assert_eq!(r.agent_id, agent("bob"));
        assert_eq!(g.reservations().len(), 1);
        assert_eq!(g.reservation(n), Some(&r));
    }

    #[test]
    fn release_of_expired_lock_is_identity_only() {
        let (mut g, n) = graph_with_node();
        reserve(&mut g, n, &agent("alice"), Duration::from_secs(30), ts(0)).unwrap();

        // The 30 s lock is long dead at ts(31), but release is decided on
        // identity alone: a non-owner is still denied...
        let err = release(&mut g, n, &agent("bob")).unwrap_err();
        match err {
            LamboError::Conflict(msg) => {
                assert!(msg.contains("alice") && msg.contains("bob"), "message should name both agents: {msg}")
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert!(g.reservation(n).is_some(), "denied release must not clear the lock");

        // ...and the original owner can still clean up the expired lock.
        release(&mut g, n, &agent("alice")).unwrap();
        assert_eq!(g.reservation(n), None);
    }

    #[test]
    fn ttl_overflowing_datetime_is_typed_error_not_panic() {
        let (mut g, n) = graph_with_node();
        // ~260k years: inside chrono::Duration's ±~292k-year range (so from_std
        // accepts it) but beyond DateTime<Utc>'s ±262,143-year span, where the
        // plain `now + ttl` add would panic. Must come back as the typed error.
        let ttl = Duration::from_secs(8_210_000_000_000);
        let err = reserve(&mut g, n, &agent("alice"), ttl, ts(0)).unwrap_err();
        match err {
            LamboError::Other(e) => assert!(
                e.downcast_ref::<ReserveError>().is_some(),
                "expected typed ReserveError, got {e:?}"
            ),
            other => panic!("expected Other(ReserveError), got {other:?}"),
        }
        // Nothing was stored.
        assert_eq!(g.reservation(n), None);
    }
}
