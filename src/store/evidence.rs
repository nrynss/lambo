//! Supersession evidence (#20): one reading of a node's dependence rows,
//! shared by every adapter and by the evaluator's apply-time revalidation.
//!
//! Each adapter returns the same raw [`DependentEvidence`] (one row per
//! dependence edge, no filtering beyond "a concept of this session other
//! than the node", fetched once per node per cycle), and its
//! [`DependentEvidence::latest_structural_evidence_at`] /
//! [`DependentEvidence::interaction_span_since`] apply the rules, so
//! MemoryStore, SQLite and the Postgres family cannot drift apart on them.
//! [`rows_from_graph`] builds the same rows from the in-RAM graph.
//!
//! ## The rules (#19(c)'s clock, Stage 2's predicate)
//!
//! * **Evidence** is one dependence edge into the node
//!   ([`crate::types::Edge::dependence`]: a `Dependency` source or a
//!   `Hierarchical` child), whose dependent is a concept of the session other
//!   than the node.
//! * **Eligible** by write time: the edge was written at or before
//!   `now - min_age` (#19(c) review M3: a backdated `event_time` does not
//!   skip the floor). A cutoff outside chrono's range makes nothing eligible.
//! * **Own origin**: a dependent written by the node's own origin
//!   interaction never supports it.
//! * **Dated** by [`declared_at`]: the about-time clamped to the write time,
//!   exactly as `daemon::contradiction` dates evidence and contradictions.
//! * [`latest_at`] is the newest such date (the supersession watermark),
//!   minus the edges the caller excludes (self-suppression).
//! * [`span_since`] is Stage 2's span over the evidence strictly after
//!   `since`: the edge **and** its dependent's origin interaction (a session
//!   interaction, itself written by the cutoff) must both be dated strictly
//!   after it. The filter runs before origins are counted and the spread is
//!   measured. Distinct origins and their spread (on the same clamped dates)
//!   over min(session extent, `window`), the extent measured exactly as
//!   Stage 2 measures it.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::graph::Graph;
#[cfg(feature = "store-memory")]
use crate::types::{Concept, GraphSnapshot, Interaction};
use crate::types::{InteractionSpan, Node, NodeId};

/// One dependence edge into a node, with its dependent's origin.
#[derive(Clone, Debug, PartialEq)]
pub struct DependentEvidenceRow {
    /// The dependence edge.
    pub edge: NodeId,
    /// The edge's write time.
    pub edge_created: DateTime<Utc>,
    /// The edge's `event_time`, when the write gave one.
    pub edge_event: Option<DateTime<Utc>>,
    /// The dependent concept.
    pub dependent: NodeId,
    /// The dependent's origin interaction.
    pub origin: NodeId,
    /// That interaction's `(created_at, event_time)` when it is an
    /// interaction of the session; `None` otherwise (a foreign or missing
    /// origin, which never counts toward the span).
    pub origin_dates: Option<(DateTime<Utc>, Option<DateTime<Utc>>)>,
}

/// One node's dependent evidence as a store returns it
/// ([`crate::store::GraphStore::dependent_evidence`]): every row, unfiltered,
/// so the rules below are applied in one place for every adapter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DependentEvidence {
    /// The node's own origin interaction (`None` when the node is not a
    /// concept of the session).
    pub own_origin: Option<NodeId>,
    /// One row per dependence edge into the node.
    pub rows: Vec<DependentEvidenceRow>,
    /// The session's temporal extent on about-times (Stage 2's denominator),
    /// `None` for a session without interactions.
    pub extent: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// About-time clamped to write time (`daemon::contradiction::declared_at`'s
/// rule, for a row).
pub(crate) fn declared(created: DateTime<Utc>, event: Option<DateTime<Utc>>) -> DateTime<Utc> {
    event.unwrap_or(created).min(created)
}

fn cutoff(now: DateTime<Utc>, min_age: Duration) -> Option<DateTime<Utc>> {
    ChronoDuration::from_std(min_age)
        .ok()
        .and_then(|age| now.checked_sub_signed(age))
}

fn eligible<'a>(
    rows: &'a DependentEvidence,
    min_age: Duration,
    now: DateTime<Utc>,
) -> impl Iterator<Item = &'a DependentEvidenceRow> + 'a {
    let cutoff = cutoff(now, min_age);
    rows.rows.iter().filter(move |r| {
        cutoff.is_some_and(|c| r.edge_created <= c) && Some(r.origin) != rows.own_origin
    })
}

/// The newest declaration date of the node's eligible evidence, ignoring
/// the edges in `exclude`; `None` with no eligible evidence.
pub(crate) fn latest_at(
    rows: &DependentEvidence,
    min_age: Duration,
    now: DateTime<Utc>,
    exclude: &[NodeId],
) -> Option<DateTime<Utc>> {
    eligible(rows, min_age, now)
        .filter(|r| !exclude.contains(&r.edge))
        .map(|r| declared(r.edge_created, r.edge_event))
        .max()
}

/// The evidence [`span_since`] counts: eligible rows whose edge and whose
/// origin interaction (a session interaction written by the cutoff) are both
/// dated strictly after `since`, as `(row, origin date)`.
fn counted<'a>(
    rows: &'a DependentEvidence,
    min_age: Duration,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> impl Iterator<Item = (&'a DependentEvidenceRow, DateTime<Utc>)> + 'a {
    let cutoff = cutoff(now, min_age);
    eligible(rows, min_age, now).filter_map(move |r| {
        if declared(r.edge_created, r.edge_event) <= since {
            return None;
        }
        let (created, event) = r.origin_dates?;
        if cutoff.is_none_or(|c| created > c) {
            return None;
        }
        let at = declared(created, event);
        (at > since).then_some((r, at))
    })
}

/// The newest declaration date among the evidence [`span_since`] counts
/// after `since` (#20 review L2: sources rank by what was counted).
pub(crate) fn newest_counted(
    rows: &DependentEvidence,
    min_age: Duration,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    counted(rows, min_age, now, since)
        .map(|(r, _)| declared(r.edge_created, r.edge_event))
        .max()
}

/// Stage 2's span over the node's eligible evidence strictly after `since`
/// (see the module docs). The window must already be validated.
pub(crate) fn span_since(
    rows: &DependentEvidence,
    min_age: Duration,
    now: DateTime<Utc>,
    window: Duration,
    since: DateTime<Utc>,
) -> InteractionSpan {
    let mut origins: HashMap<NodeId, DateTime<Utc>> = HashMap::new();
    for (r, at) in counted(rows, min_age, now, since) {
        origins.insert(r.origin, at);
    }
    let distinct = origins.len() as u64;
    let coverage = match (origins.values().min(), origins.values().max()) {
        (Some(&lo), Some(&hi)) => {
            let (sess_lo, sess_hi) = rows.extent.unwrap_or((lo, hi));
            let sess_span = (sess_hi - sess_lo).num_milliseconds().max(0);
            let span = (hi - lo).num_milliseconds().max(0);
            super::span::coverage(span, sess_span, window)
        }
        _ => 0.0,
    };
    InteractionSpan { distinct, coverage }
}

impl DependentEvidence {
    /// The supersession watermark: the newest declaration date of the
    /// node's eligible evidence, ignoring the dependence edges in `exclude`
    /// (#19(c) self-suppression); `None` with no eligible evidence. `now` is
    /// the caller's clock.
    pub fn latest_structural_evidence_at(
        &self,
        min_age: Duration,
        now: DateTime<Utc>,
        exclude: &[NodeId],
    ) -> Option<DateTime<Utc>> {
        latest_at(self, min_age, now, exclude)
    }

    /// Stage 2's span over the eligible evidence strictly after `since`,
    /// against min(session extent, `window`); feed it to
    /// [`crate::canon::stage2_evidence_passes`]. `window` is validated.
    pub fn interaction_span_since(
        &self,
        min_age: Duration,
        now: DateTime<Utc>,
        window: Duration,
        since: DateTime<Utc>,
    ) -> Result<InteractionSpan, crate::types::StoreError> {
        super::validate_coverage_window(window)?;
        Ok(span_since(self, min_age, now, window, since))
    }

    /// The newest declaration date among the evidence
    /// [`Self::interaction_span_since`] counts after `since`.
    pub fn newest_counted_at(
        &self,
        min_age: Duration,
        now: DateTime<Utc>,
        since: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        newest_counted(self, min_age, now, since)
    }
}

/// The same rows read from the in-RAM graph (the evaluator's apply-time
/// revalidation).
pub(crate) fn rows_from_graph(graph: &Graph, node: NodeId) -> DependentEvidence {
    let session = graph.session_id();
    let own_origin = match graph.node(node) {
        Some(Node::Concept(c)) if &c.session_id == session => Some(c.origin_interaction),
        _ => None,
    };
    let mut rows = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    for e in graph.incident_edges(node) {
        let Some((pillar, dependent)) = e.dependence() else {
            continue;
        };
        if pillar != node || dependent == node || e.session_id != *session || !seen.insert(e.id) {
            continue;
        }
        let Some(Node::Concept(dep)) = graph.node(dependent) else {
            continue;
        };
        if &dep.session_id != session {
            continue;
        }
        let origin_dates = match graph.node(dep.origin_interaction) {
            Some(Node::Interaction(i)) if &i.session_id == session => {
                Some((i.created_at, i.event_time))
            }
            _ => None,
        };
        rows.push(DependentEvidenceRow {
            edge: e.id,
            edge_created: e.created_at,
            edge_event: e.event_time,
            dependent,
            origin: dep.origin_interaction,
            origin_dates,
        });
    }
    let mut extent: Option<(DateTime<Utc>, DateTime<Utc>)> = None;
    for i in graph.interactions() {
        let at = i.about_time();
        extent = Some(match extent {
            None => (at, at),
            Some((lo, hi)) => (lo.min(at), hi.max(at)),
        });
    }
    DependentEvidence {
        own_origin,
        rows,
        extent,
    }
}

/// The same rows read from one session's snapshot (MemoryStore). The
/// snapshot may hold rows a legal graph cannot (a self-loop, a foreign
/// concept's edge), so the session checks are explicit here too.
#[cfg(feature = "store-memory")]
pub(crate) fn rows_from_snapshot(snap: &GraphSnapshot, node: NodeId) -> DependentEvidence {
    let session = &snap.session_id;
    let concepts: HashMap<NodeId, &Concept> = snap
        .concepts
        .iter()
        .filter(|c| &c.session_id == session)
        .map(|c| (c.id, c))
        .collect();
    let interactions: HashMap<NodeId, &Interaction> = snap
        .interactions
        .iter()
        .filter(|i| &i.session_id == session)
        .map(|i| (i.id, i))
        .collect();
    let own_origin = concepts.get(&node).map(|c| c.origin_interaction);
    let mut seen: HashSet<NodeId> = HashSet::new();
    let rows = snap
        .edges
        .iter()
        .filter(|e| &e.session_id == session)
        .filter_map(|e| {
            let (pillar, dependent) = e.dependence()?;
            if pillar != node || dependent == node || !seen.insert(e.id) {
                return None;
            }
            let dep = concepts.get(&dependent)?;
            Some(DependentEvidenceRow {
                edge: e.id,
                edge_created: e.created_at,
                edge_event: e.event_time,
                dependent,
                origin: dep.origin_interaction,
                origin_dates: interactions
                    .get(&dep.origin_interaction)
                    .map(|i| (i.created_at, i.event_time)),
            })
        })
        .collect();
    let extent = snap.interactions.iter().map(|i| i.about_time()).fold(
        None,
        |acc: Option<(DateTime<Utc>, DateTime<Utc>)>, at| {
            Some(acc.map_or((at, at), |(lo, hi)| (lo.min(at), hi.max(at))))
        },
    );
    DependentEvidence {
        own_origin,
        rows,
        extent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(min: i64) -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 1, 12, 0, 0).unwrap()
            + ChronoDuration::minutes(min)
    }

    fn id(n: u64) -> NodeId {
        NodeId(uuid::Uuid::from_u64_pair(0x20, n))
    }

    fn row(edge: u64, at: i64, origin: u64, origin_at: i64) -> DependentEvidenceRow {
        DependentEvidenceRow {
            edge: id(edge),
            edge_created: t(at),
            edge_event: None,
            dependent: id(100 + edge),
            origin: id(1000 + origin),
            origin_dates: Some((t(origin_at), None)),
        }
    }

    fn rows(rs: Vec<DependentEvidenceRow>) -> DependentEvidence {
        DependentEvidence {
            own_origin: Some(id(1000)),
            rows: rs,
            extent: Some((t(0), t(100))),
        }
    }

    const MIN: Duration = Duration::from_secs(60);
    const WINDOW: Duration = crate::store::DEFAULT_COVERAGE_WINDOW;

    /// The graph-side reading agrees with the shared case's store answers.
    #[test]
    fn the_graph_reading_answers_the_shared_case() {
        crate::test_util::supersession::check_graph();
    }

    #[test]
    fn the_watermark_is_the_newest_eligible_declaration() {
        let mut late = row(3, 50, 3, 50);
        late.edge_event = Some(t(90)); // future about-time: clamped to the write
        let mut backdated = row(4, 99, 4, 99); // written too late for the floor
        backdated.edge_event = Some(t(10));
        let r = rows(vec![row(1, 20, 1, 20), row(2, 40, 0, 40), late, backdated]);
        // Row 2 is own-origin; row 4 was written after now - 60s.
        assert_eq!(
            latest_at(&r, MIN, t(99) + ChronoDuration::seconds(30), &[]),
            Some(t(50))
        );
        assert_eq!(
            latest_at(&r, MIN, t(99) + ChronoDuration::seconds(30), &[id(3)]),
            Some(t(20)),
            "an excluded edge does not count"
        );
        assert_eq!(latest_at(&r, Duration::MAX, t(99), &[]), None);
        assert_eq!(latest_at(&rows(vec![]), MIN, t(99), &[]), None);
    }

    #[test]
    fn the_span_filters_after_the_cutoff_before_counting() {
        let r = rows(vec![
            row(1, 10, 1, 10),
            row(2, 40, 2, 40),
            row(3, 70, 3, 70),
            row(4, 90, 4, 90),
            row(5, 95, 0, 95), // own origin
        ]);
        let now = t(100);
        let span = span_since(&r, MIN, now, WINDOW, t(30));
        assert_eq!(span.distinct, 3, "rows 2, 3 and 4");
        assert!((span.coverage - 0.5).abs() < 1e-9, "50 of 100 minutes");
        let span = span_since(&r, MIN, now, WINDOW, t(40));
        assert_eq!(span.distinct, 2, "equality with the cutoff is refused");
    }

    #[test]
    fn both_the_edge_and_its_origin_must_be_newer() {
        // A new edge from an old concept: its origin predates the cutoff.
        let old_origin = row(1, 80, 1, 10);
        // A backdated new edge whose origin is new.
        let mut backdated = row(2, 80, 2, 80);
        backdated.edge_event = Some(t(20));
        // A foreign origin.
        let mut foreign = row(3, 80, 3, 80);
        foreign.origin_dates = None;
        // An origin written after the floor.
        let young = row(4, 80, 4, 100);
        let r = rows(vec![
            old_origin,
            backdated,
            foreign,
            young,
            row(5, 80, 5, 80),
        ]);
        let span = span_since(&r, MIN, t(100), WINDOW, t(50));
        assert_eq!(span.distinct, 1, "only row 5");
        assert_eq!(span.coverage, 0.0);
    }
}
