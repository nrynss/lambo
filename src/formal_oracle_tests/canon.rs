//! `formal/oracle/Canon.json`: the pure parts of Lean `Canon` against the
//! Rust: `gate` against `Graph::apply_canonization_transition` (every
//! `(current, from, to)`, no demotion reason), `stage1Passes`,
//! `stage2Passes` (coverage `num / den`) and `inCooldown` (milliseconds)
//! against `stage1_evidence_passes`, `stage2_evidence_passes` and
//! `in_repromotion_cooldown`. `Canon.cycle` is not covered (see
//! `formal/OracleExport/Canon.lean`).

use std::time::Duration;

use chrono::{TimeZone, Utc};

use super::{vectors, Vectors};
use crate::canon::formal_oracle::{in_repromotion_cooldown, stage1_evidence_passes};
use crate::canon::stage2_evidence_passes;
use crate::graph::Graph;
use crate::test_util::props;
use crate::types::{CanonizationEvent, CanonizationStatus, InteractionSpan, NodeId};

fn status(s: &str) -> CanonizationStatus {
    match s {
        "none" => CanonizationStatus::None,
        "candidate" => CanonizationStatus::Candidate,
        "venerable" => CanonizationStatus::Venerable,
        "canonical" => CanonizationStatus::Canonical,
        other => panic!("Canon.json: unknown status {other:?}"),
    }
}

fn name(s: CanonizationStatus) -> String {
    match s {
        CanonizationStatus::None => "none",
        CanonizationStatus::Candidate => "candidate",
        CanonizationStatus::Venerable => "venerable",
        CanonizationStatus::Canonical => "canonical",
    }
    .to_owned()
}

/// Apply `from → to` to a concept at `current`: its new status, or `None`
/// when the gate refuses.
fn gate(
    current: CanonizationStatus,
    from: CanonizationStatus,
    to: CanonizationStatus,
) -> Option<String> {
    let mut g: Graph = props::graph_of([props::concept(1, "x", "x", current)]);
    let event = CanonizationEvent {
        id: NodeId(uuid::Uuid::from_u64_pair(4, 1)),
        session_id: props::sid(),
        node_id: props::cid(1),
        from_status: from,
        to_status: to,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: props::t0(),
        reason: None,
    };
    match g.apply_canonization_transition(event) {
        Ok(()) => match g.node(props::cid(1)) {
            Some(crate::types::Node::Concept(c)) => Some(name(c.canonization_status)),
            _ => unreachable!("the concept is in the graph"),
        },
        Err(_) => None,
    }
}

fn millis(ms: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .expect("a valid instant")
}

#[test]
fn canon() {
    let v = Vectors::load(
        "Canon",
        vectors!("Canon"),
        &[
            "Lambo.Canon.gate",
            "Lambo.Canon.stage1Passes",
            "Lambo.Canon.stage2Passes",
            "Lambo.Canon.inCooldown",
        ],
    );
    v.replay("Lambo.Canon.gate", |[c, f, t]: &[String; 3]| {
        gate(status(c), status(f), status(t))
    });
    v.replay("Lambo.Canon.stage1Passes", |&(gc, dep): &(i32, usize)| {
        stage1_evidence_passes(gc, dep)
    });
    v.replay(
        "Lambo.Canon.stage2Passes",
        |&(distinct, num, den): &(u64, u64, u64)| {
            stage2_evidence_passes(InteractionSpan {
                distinct,
                coverage: num as f64 / den as f64,
            })
        },
    );
    v.replay(
        "Lambo.Canon.inCooldown",
        |&(last, cooldown, now): &(Option<i64>, u64, i64)| {
            in_repromotion_cooldown(
                last.map(millis),
                Duration::from_millis(cooldown),
                millis(now),
            )
        },
    );
}
