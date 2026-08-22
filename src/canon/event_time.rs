//! Event time — the clock canonization measures (workstream D).
//!
//! Every time-based gate used to read **flush time**: when Lambo recorded a
//! fact. Ten years of history arriving in ninety minutes has no temporal
//! structure in that clock, so Stage 2's `min_age` floor, the 0.3
//! temporal-extent coverage bar, SoloPolicy's 24-hour session separation and
//! `blast_radius`'s `min_edge_age` all read a bootstrap as "everything
//! happened at once". D adds a second, per-fact instant — **event time**, the
//! time a fact is *about* — and moves those gates onto it.
//!
//! The three design questions the spec posed are answered here, in one place,
//! because they are one decision chain:
//!
//! ## 1. Where event time enters: per fact, on the write call
//!
//! On `derive` / `record_action`, not as session-level ingest metadata. A
//! fact's about-time is a property of the fact (its originating interaction),
//! not of the session: one ingest stream can mix transcript turns that carry
//! timestamps with turns that carry none, and a session-level stamp would
//! fabricate about-times for the latter. Concretely:
//!
//! * [`crate::types::Interaction::event_time`] — supplied per write through
//!   [`crate::memory::Memory::derive_for_ingest`] (and its `_as` twin), which
//!   threads the value into the interaction the call opens.
//! * [`crate::types::Edge::event_time`] — not caller-supplied; stamped from
//!   the writing interaction's `about_time` at creation (`record_action` /
//!   `derive` already timestamp edges with their interaction's clock). An edge
//!   is about what the turn that wrote it is about.
//!
//! The async pipeline needs no new surface: `derive_async_as` /
//! `record_action_async_as` open their interaction synchronously at submit
//! time, so an ingester that wants queued writes stamped can open them through
//! the sync seam first. Surfacing event time on MCP/CLI is deliberately left
//! to C2's ingest tooling: F18 bans CLI flags whose names look like client
//! wall-clock timestamps (`timestamp`, `now`, `when`) because flush time is
//! server-authoritative. Event time is a different concept — about-time, not
//! observed-at — and if it is ever surfaced it must be named for that concept
//! (`event_time` / `at`), never with a banned token; this does not weaken F18,
//! whose rule guards the server's own stamping authority, which is untouched.
//!
//! ## 2. The fallback rule, and mixing
//!
//! **Fallback:** a fact with no event time is measured on its own flush time
//! ([`crate::types::Interaction::about_time`] /
//! [`crate::types::Edge::about_time`]). No session-level or corpus-level
//! default is promoted onto such a fact: a live Mooshik session has no commit
//! date, and borrowing another fact's would corrupt its age in either
//! direction. A no-event-time fact therefore behaves exactly as it did before
//! D — which is also why every pre-D fixture and test keeps passing.
//!
//! **Mixing:** both clocks are absolute UTC instants, so a session holding
//! both kinds resolves each fact independently and measures on the union
//! timeline. The consequence is deliberate and conservative: ages stay honest
//! per fact (a 2016 commit-dated fact is old; a just-flushed live fact is
//! fresh — the F17 fresh-evidence guard survives mixing by construction,
//! because fallback edges still age on flush time), while coverage gets
//! *harder*, never easier: mixing a decade-old extent with today's points
//! widens the denominator without widening the support. Nothing promotes a
//! mixed session onto one clock retroactively.
//!
//! ## 3. Persisted, not derived at evaluation
//!
//! Event time is caller-supplied knowledge; there is nothing to recompute it
//! from at load time, so "derived" was never actually available — the real
//! alternative was dropping it at close and losing provenance across restart.
//! It is persisted: nullable columns on `interactions` and `edges`
//! (`migrations/{sqlite,cockroach}/001_init.sql`), serde-defaulted on the node
//! structs so fixture JSON loads unchanged, and round-tripped by every
//! adapter's upsert/load pair. Evaluation reads resolved instants straight off
//! loaded rows via `about_time()` / `COALESCE(event_time, created_at)`; there
//! is no recompute step to drift.
//!
//! ## What the eval passes, and what the store resolves
//!
//! Gate predicates keep taking `now` (F8 discipline — the adapter has no clock
//! of its own) and compute the cutoff `now - min_age` exactly as before. What
//! changes is which stored instant the predicate compares against that cutoff:
//! the **resolved** about-time, not the bare `created_at`. The anchor stays
//! the flush clock's `now`; an event-timed fact older than the anchor simply
//! clears any age floor immediately, which is the truth about a decade-old
//! fact. An event time in the future of the anchor makes the row invisible
//! until wall time catches up — conservative, and never special-cased.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Count how many sessions in `starts` are separated by at least `gap`.
///
/// This is D2's deliverable to C2: the solo score's recurrence term wants
/// "three or more sessions separated by ≥ 24 hours", measured on **event
/// time** — a bulk ingest of a decade has that separation in commit dates and
/// none whatsoever in flush time, so the count must resolve session starts
/// through [`crate::types::Interaction::about_time`] (the earliest interaction
/// of each session) before calling this.
///
/// The rule is greedy and order-free: sort ascending, then count a start only
/// when it sits `gap` or more after the last *counted* one. Dense clusters
/// contribute one session each; stragglers further than `gap` out extend the
/// chain. Duplicate instants collapse naturally (a zero gap only counts once).
///
/// C2 consumes this from [`super::policy`]'s scorer once the solo formula
/// lands; [`SoloScorer`](super::policy::SoloScorer) still refuses until then.
pub fn separated_session_count(starts: &[DateTime<Utc>], gap: Duration) -> usize {
    let gap = chrono::Duration::from_std(gap).unwrap_or(chrono::Duration::MAX);
    let mut sorted = starts.to_vec();
    sorted.sort();
    let mut count = 0;
    let mut anchor: Option<DateTime<Utc>> = None;
    for t in sorted {
        match anchor {
            Some(a) if t - a < gap => {}
            _ => {
                count += 1;
                anchor = Some(t);
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{GraphStore, MemoryStore};
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, Interaction,
        MutationBatch, Node,
    };
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000 + secs, 0).unwrap()
    }

    /// Greedy counting: clusters collapse, stragglers extend.
    ///
    /// Mutation: `>=` → `>` in the gap test (a boundary-equal start drops),
    /// or sorting dropped, or counting every start regardless of gap.
    #[test]
    fn separation_counts_clusters_once_and_stragglers_each() {
        // Cluster A: three starts within one hour -> one counted session.
        // Then +24h exactly (boundary counts, >=), then +24h+2h again.
        let starts = vec![ts(0), ts(600), ts(3600), ts(86_400), ts(172_800 + 7_200)];
        assert_eq!(
            separated_session_count(&starts, Duration::from_secs(86_400)),
            3,
            "cluster of 3 + boundary-straddler + next-day straggler"
        );
        assert_eq!(separated_session_count(&[], Duration::ZERO), 0);
        assert_eq!(separated_session_count(&[ts(5)], Duration::ZERO), 1);
        // Zero-gap duplicates collapse to one.
        assert_eq!(
            separated_session_count(&[ts(0), ts(0), ts(0)], Duration::ZERO),
            1
        );
    }

    // -----------------------------------------------------------------------
    // Shared fixture: a historical corpus seeded two ways.
    //
    // Five interactions ABOUT years 2015..2019 (event times spread wide),
    // flushed back-to-back inside one minute. Under event time the supported
    // interactions span most of the corpus; under pure ingest time the same
    // facts cluster into a sliver of the extent.
    // -----------------------------------------------------------------------

    /// Flush base: everything lands within [0, 60) seconds.
    const FLUSH_SPREAD: i64 = 60;

    fn interaction(
        id: u64,
        flushed_at_secs: i64,
        event_time: Option<DateTime<Utc>>,
    ) -> Interaction {
        Interaction {
            id: crate::types::NodeId(uuid::Uuid::from_u64_pair(1, id)),
            session_id: crate::types::SessionId::from("history"),
            agent_id: AgentId::from("ingester"),
            prompt_text: Some(format!("turn {id}")),
            previous_id: None,
            created_at: ts(flushed_at_secs),
            event_time,
        }
    }

    fn concept(id: u64, origin: u64, at: DateTime<Utc>) -> Concept {
        Concept {
            id: crate::types::NodeId(uuid::Uuid::from_u64_pair(2, id)),
            session_id: crate::types::SessionId::from("history"),
            content: format!("c{id}"),
            canonical_key: format!("c{id}"),
            concept_type: ConceptType::Entity,
            origin_interaction: crate::types::NodeId(uuid::Uuid::from_u64_pair(1, origin)),
            origin_agent: AgentId::from("ingester"),
            created_at: at,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }
    fn edge(
        id: u64,
        src: u64,
        tgt: u64,
        ty: EdgeType,
        at: DateTime<Utc>,
        event_time: Option<DateTime<Utc>>,
    ) -> Edge {
        Edge {
            id: crate::types::NodeId(uuid::Uuid::from_u64_pair(3, id)),
            session_id: crate::types::SessionId::from("history"),
            source: crate::types::NodeId(uuid::Uuid::from_u64_pair(2, src)),
            target: crate::types::NodeId(uuid::Uuid::from_u64_pair(2, tgt)),
            edge_type: ty,
            weight: 1.0,
            reinforcements: 1,
            created_at: at,
            last_reinforced: at,
            event_time,
        }
    }

    /// Seed interactions/concepts/edges into a `MemoryStore`.
    async fn seed(
        interactions: &[Interaction],
        concepts: &[Concept],
        edges: &[Edge],
    ) -> MemoryStore {
        let store = MemoryStore::new();
        let mut batch = MutationBatch::new();
        for i in interactions {
            batch.push(crate::types::Mutation::UpsertNode {
                node: Node::Interaction(i.clone()),
            });
        }
        for c in concepts {
            batch.push(crate::types::Mutation::UpsertNode {
                node: Node::Concept(c.clone()),
            });
        }
        for e in edges {
            batch.push(crate::types::Mutation::UpsertEdge { edge: e.clone() });
        }
        store.flush(&batch, None).await.unwrap();
        store
    }

    /// Done-when box 2, measured: the SAME corpus canonizes differently
    /// under event time than under ingest time, with the numbers asserted.
    ///
    /// Corpus: six turns about 2015..2020 (one per year), ingested
    /// back-to-back within one minute of flush time — exactly the bootstrap
    /// shape the spec opens with. Concept 10 is supported by the turns about
    /// 2016/2018/2020.
    ///
    /// * Event time: every support is years past the 60s age floor at the
    ///   flush instant, and the supports span 4 of the extent's 5 years →
    ///   `distinct = 3`, `coverage = 0.8` → Stage 2 PASSES.
    /// * Ingest time (same rows, event times stripped): the flush stamps are
    ///   scale-free — supports at 12s/36s/60s of a 60s extent would cover
    ///   0.8 too — so the failure is purely AGE: nothing is 60s old yet,
    ///   giving `distinct = 0`, `coverage = 0.0` → Stage 2 FAILS.
    ///
    /// Both halves assert numbers, and the outcome pair (pass, fail) at the
    /// same pinned instant is the measured difference.
    ///
    /// Mutation: resolve about-time as `created_at` unconditionally (drop the
    /// fallback rule) or ignore event time entirely — either collapses the
    /// event half onto the ingest half's numbers and the differing-outcome
    /// assertion goes red.
    #[tokio::test]
    async fn a_seeded_historical_corpus_canonizes_differently_under_event_time() {
        let year = |y: i32| chrono::TimeZone::with_ymd_and_hms(&Utc, y, 6, 15, 12, 0, 0).unwrap();
        // Turn n (1-based) is ABOUT year 2015+(n-1), flushed at (n-1)*12s.
        let n_turns = 6u64;
        let event_times: Vec<_> = (1..=n_turns).map(|n| year(2014 + n as i32)).collect();
        let flushes: Vec<i64> = (1..=n_turns).map(|n| (n as i64 - 1) * 12).collect();

        let build = |with_event_time: bool| {
            let interactions: Vec<Interaction> = (1..=n_turns)
                .map(|n| {
                    interaction(
                        n,
                        flushes[(n - 1) as usize],
                        with_event_time.then(|| event_times[(n - 1) as usize]),
                    )
                })
                .collect();
            let concepts = vec![
                concept(10, 1, ts(0)),
                concept(1, 1, ts(0)),
                concept(2, 2, ts(12)),
                concept(3, 3, ts(24)),
                concept(4, 4, ts(36)),
                concept(5, 5, ts(48)),
                concept(6, 6, ts(60)),
            ];
            // Supports from turns 2, 4, 6; each edge carries its writing
            // interaction's about-time in event mode (what record_action
            // would stamp), falls back to flush time otherwise.
            let support_edge = |id, src, ty, turn: usize| {
                edge(
                    id,
                    src,
                    10,
                    ty,
                    ts(flushes[turn]),
                    with_event_time.then(|| event_times[turn]),
                )
            };
            let edges = vec![
                support_edge(1, 2, EdgeType::Dependency, 1),
                support_edge(2, 4, EdgeType::Causal, 3),
                support_edge(3, 6, EdgeType::Hierarchical, 5),
            ];
            (interactions, concepts, edges)
        };

        let sid = crate::types::SessionId::from("history");
        let hub = crate::types::NodeId(uuid::Uuid::from_u64_pair(2, 10));
        let min_age = Duration::from_secs(60);
        // The eval instant: right after the ingest burst lands.
        let now = ts(FLUSH_SPREAD);

        // --- Event time ---------------------------------------------------
        let (ix, cs, es) = build(true);
        let store = seed(&ix, &cs, &es).await;
        let span = store
            .interaction_span(&sid, hub, min_age, now)
            .await
            .unwrap();
        assert_eq!(
            span.distinct, 3,
            "all supports clear 60s on their about-times"
        );
        assert!(
            (span.coverage - 0.8).abs() < 1e-9,
            "supports about 2016/2018/2020 span 4 of the 5-year extent: coverage={}",
            span.coverage
        );
        assert!(
            super::super::stage2_passes(&store, &sid, hub, min_age, now)
                .await
                .unwrap(),
            "event-timed corpus clears age + coverage at the flush instant"
        );

        // --- Ingest time: same rows, event times stripped ------------------
        let (ix, cs, es) = build(false);
        let store = seed(&ix, &cs, &es).await;
        let span = store
            .interaction_span(&sid, hub, min_age, now)
            .await
            .unwrap();
        assert_eq!(span.distinct, 0, "every support is under 60s of flush age");
        assert_eq!(span.coverage, 0.0, "no aged supports -> no coverage");
        assert!(
            !super::super::stage2_passes(&store, &sid, hub, min_age, now)
                .await
                .unwrap(),
            "the same corpus fails Stage 2 on ingest time at the same instant"
        );
    }

    /// Done-when box 3, the mixing half of the fallback rule: a session
    /// holding event-timed and fallback facts measures each fact on its own
    /// clock. The event-timed target's origin is ancient; the support edge
    /// written by a live turn (`event_time: None`) stays flush-fresh and is
    /// cut — mixing never lends one fact's age to another.
    #[tokio::test]
    async fn mixed_session_ages_each_fact_on_its_own_clock() {
        // Turn 1 is about 2015; turn 2 is live (no event time).
        let ix = vec![
            interaction(1, 0, Some(year_helper())),
            interaction(2, FLUSH_SPREAD, None),
        ];
        let cs = vec![
            concept(10, 1, ts(0)),
            concept(1, 1, ts(0)),
            concept(2, 2, ts(FLUSH_SPREAD)),
        ];
        // Support from the LIVE concept, edge written by the live turn.
        let es = vec![edge(1, 2, 10, EdgeType::Dependency, ts(FLUSH_SPREAD), None)];
        let store = seed(&ix, &cs, &es).await;
        let sid = crate::types::SessionId::from("history");
        let hub = crate::types::NodeId(uuid::Uuid::from_u64_pair(2, 10));

        let span = store
            .interaction_span(&sid, hub, Duration::from_secs(60), ts(FLUSH_SPREAD))
            .await
            .unwrap();
        assert_eq!(
            span.distinct, 0,
            "a fallback-aged edge must stay cut in a mixed session"
        );
        assert!(
            !super::super::stage2_passes(
                &store,
                &sid,
                hub,
                Duration::from_secs(60),
                ts(FLUSH_SPREAD)
            )
            .await
            .unwrap(),
            "mixing must not lend the event-timed origin's age to a live edge"
        );
    }

    fn year_helper() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2015, 6, 15, 12, 0, 0).unwrap()
    }

    /// Done-when box 4: the gate path runs on injected instants — repeated
    /// evaluations at one pinned `now` return identical answers, and moving
    /// the pin moves the answer. No wall clock behind the test's back.
    #[tokio::test]
    async fn pinned_now_is_deterministic_and_the_pin_moves_the_answer() {
        // A LIVE session (fallback domain): supports flushed at 0/20/40s,
        // a fourth turn at 60s widening the extent.
        let ix = vec![
            interaction(1, 0, None),
            interaction(2, 20, None),
            interaction(3, 40, None),
            interaction(4, FLUSH_SPREAD, None),
        ];
        let cs = vec![
            concept(10, 1, ts(0)),
            concept(1, 1, ts(0)),
            concept(2, 2, ts(20)),
            concept(3, 3, ts(40)),
            concept(4, 4, ts(FLUSH_SPREAD)),
        ];
        let es = vec![
            edge(1, 1, 10, EdgeType::Dependency, ts(0), None),
            edge(2, 2, 10, EdgeType::Dependency, ts(20), None),
            edge(3, 3, 10, EdgeType::Hierarchical, ts(40), None),
        ];
        let store = seed(&ix, &cs, &es).await;
        let sid = crate::types::SessionId::from("history");
        let hub = crate::types::NodeId(uuid::Uuid::from_u64_pair(2, 10));
        let min_age = Duration::from_secs(60);

        // Pin at t=120s: cutoff t=60s keeps every support; coverage 40/60.
        let late = ts(120);
        assert!(
            super::super::stage2_passes(&store, &sid, hub, min_age, late)
                .await
                .unwrap()
                && super::super::stage2_passes(&store, &sid, hub, min_age, late)
                    .await
                    .unwrap(),
            "same pin, identical answer"
        );

        // Pin at t=90s: cutoff t=30s cuts the third support → distinct 2.
        assert!(
            !super::super::stage2_passes(&store, &sid, hub, min_age, ts(90))
                .await
                .unwrap(),
            "an earlier pin must re-age the corpus"
        );
    }
}
