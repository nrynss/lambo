//! Statement planning for the write-behind flush (L82-1).
//!
//! # Why this exists
//!
//! Both SQL adapters used to replay a [`crate::types::MutationBatch`] one `sqlx::query` per
//! mutation, sequentially awaited inside one transaction. Against a local
//! SQLite file that is merely wasteful; against a *serverless* CockroachDB
//! cluster every statement is a network round-trip, so the cost of a flush was
//! `mutations × RTT`. The live T8.2/T8.3 review measured the consequence: four
//! at-cap `lambo_record_action` calls left a 784-mutation tail, SIGTERM arrived
//! before the 1 s flush interval drained it, and the final `close()` could not
//! finish inside its 10 s grace window — 784 × ~10–30 ms is 8–24 s. The tail was
//! discarded and the session's lease left stale (finding **L82-1**).
//!
//! Raising the grace window would not have fixed it: the tail is bounded only by
//! how much a client can write between flushes, so any fixed budget loses to a
//! large enough burst. The cost per mutation is what had to change.
//!
//! # What it does
//!
//! [`plan_flush`] turns a batch into an ordered list of [`FlushStep`]s, each of
//! which is **one** statement. Node and edge upserts — the entire volume of a
//! burst — are collected into per-table buckets and emitted as multi-row
//! `INSERT … ON CONFLICT` statements, so a 784-mutation tail costs a handful of
//! round-trips instead of 784. Everything else stays one statement per
//! mutation; those variants are rare and order-sensitive, and buying a few more
//! round-trips there would not be worth the reasoning.
//!
//! # Why the reordering is safe
//!
//! The graph contract (`src/graph/mod.rs`, T2.1 M2) is *replay in submission
//! order, never re-sort*. Bucketing reorders mutations, so it needs an argument:
//!
//! * **Upserts of different node kinds touch different tables.** `interactions`,
//!   `concepts` and `edges` are three tables; `edges` carries **no** foreign key
//!   on `source`/`target` (spec §4 — the graph tier owns endpoint integrity), so
//!   an edge upsert can neither observe nor constrain a concept upsert.
//! * **The one cross-table dependency is honoured by bucket order.**
//!   `concepts.origin_interaction REFERENCES interactions(id)`, so interactions
//!   are always emitted before concepts, and both after the `sessions` row that
//!   `concepts.session_id` / `interactions.session_id` reference.
//! * **Everything that *can* observe a row is a barrier.** A mutation that
//!   deletes, reads-then-writes, or updates a row another mutation may have
//!   written ([`Mutation::DeleteNode`], [`Mutation::DeleteEdge`],
//!   [`Mutation::CanonizationTransition`], [`Mutation::SetRootGoal`],
//!   [`Mutation::SetEmbedding`]) flushes every open bucket first and is then
//!   emitted alone. So no upsert ever crosses a mutation that could see it, and
//!   relative order is preserved wherever it is observable.
//! * **Within a bucket, submission order is preserved**, and duplicates are
//!   collapsed to the value row-by-row replay would have left (see
//!   [`ConceptRow`] for the one subtle case) **at the position row-by-row replay
//!   would have first written them** (see `dedupe_last_at_first_position`).
//!
//! Duplicate collapsing is not an optimisation, it is **required**: PostgreSQL
//! and CockroachDB both reject a multi-row `INSERT … ON CONFLICT DO UPDATE`
//! whose input rows collide on the conflict target ("cannot affect row a second
//! time").

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::types::{
    CanonizationStatus, Concept, Edge, EdgeType, Interaction, Mutation, Node, NodeId,
};

/// Columns bound per row of each multi-row upsert, i.e. bind parameters per row.
///
/// Both adapters emit twin-shaped statements, so one definition serves both and
/// the per-adapter bind-parameter const-asserts (R1-4) cannot drift from the
/// column lists they are meant to bound. `cockroach::upsert_placeholder_shapes_match_structs`
/// pins them against the real generated SQL.
pub const INTERACTION_COLUMNS: usize = 6;
/// See [`INTERACTION_COLUMNS`]. Includes `embedding` and `chunk_group_id`.
pub const CONCEPT_COLUMNS: usize = 16;
/// See [`INTERACTION_COLUMNS`].
pub const EDGE_COLUMNS: usize = 9;

/// Rows per multi-row statement, per table.
///
/// Each adapter picks its own: the ceiling is the backend's bind-parameter
/// limit divided by the column count ([`CONCEPT_COLUMNS`] and friends), and the
/// floor is "large enough that a realistic burst is a handful of statements".
/// Every adapter must `const _: () = assert!(rows * columns <= limit)` for each
/// bucket — see the asserts next to each `BULK_LIMITS` (R1-4).
#[derive(Clone, Copy, Debug)]
pub struct BulkLimits {
    /// Rows per `interactions` statement.
    pub interactions: usize,
    /// Rows per `concepts` statement (16 columns).
    pub concepts: usize,
    /// Rows per `edges` statement (9 columns).
    pub edges: usize,
}

/// The three canonization columns, snapshotted from a concept upsert.
///
/// Carried separately from the concept because a deduplicated row does **not**
/// take them from the same occurrence as everything else — see [`ConceptRow`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CanonizationColumns {
    pub status: CanonizationStatus,
    pub blast_radius: Option<i32>,
    pub last_demotion_time: Option<DateTime<Utc>>,
}

impl<'a> ConceptRow<'a> {
    /// A row whose concept appears exactly once, so both halves come from it.
    /// Used by the snapshot-seed path, which upserts a set of distinct concepts
    /// rather than replaying a log.
    pub fn new(concept: &'a Concept) -> Self {
        Self {
            canonization: CanonizationColumns::of(concept),
            concept,
        }
    }
}

impl CanonizationColumns {
    fn of(c: &Concept) -> Self {
        Self {
            status: c.canonization_status,
            blast_radius: c.blast_radius,
            last_demotion_time: c.last_demotion_time,
        }
    }
}

/// One concept row of a multi-row upsert.
///
/// # Why the canonization columns travel separately
///
/// `UPSERT_CONCEPT_SQL` lists `canonization_status` / `blast_radius` /
/// `last_demotion_time` in its INSERT columns but **not** in its `DO UPDATE SET`
/// list (R2-1: on an existing row the canonization path is their only writer).
/// That asymmetry is what makes deduplication non-obvious.
///
/// Replaying `[UpsertNode(c@t0), UpsertNode(c@t1)]` row by row against a row
/// that does **not** yet exist inserts `c@t0` — canonization columns included —
/// and then `DO UPDATE`s every *other* column from `c@t1`. The durable row is
/// therefore "`c@t1` with `c@t0`'s canonization columns". Collapsing naively to
/// `c@t1` would durably change the status of a concept born mid-progression.
///
/// So a deduplicated row keeps the **last** occurrence's ordinary columns and
/// the **first** occurrence's canonization columns, which is exactly what
/// row-by-row replay produces. (When the row already exists, both the INSERT's
/// canonization values and the choice made here are discarded by `DO UPDATE`, so
/// the two are trivially equivalent there.)
///
/// It is emitted at the **first** occurrence's position, for a reason unrelated
/// to either half — see `dedupe_last_at_first_position` (R1-1).
#[derive(Clone, Copy, Debug)]
pub struct ConceptRow<'a> {
    /// Every column except the three below comes from here (last occurrence).
    pub concept: &'a Concept,
    /// The values a brand-new row's INSERT must carry (first occurrence).
    pub canonization: CanonizationColumns,
}

/// One planned statement.
///
/// The vector variants are already chunked to [`BulkLimits`], so
/// `plan_flush(..).len()` **is** the number of round-trips the flush will cost.
/// That is the property [`plan_flush`]'s tests pin.
#[derive(Debug)]
pub enum FlushStep<'a> {
    /// Multi-row `interactions` upsert, in submission order.
    Interactions(Vec<&'a Interaction>),
    /// Multi-row `concepts` upsert, in submission order.
    Concepts(Vec<ConceptRow<'a>>),
    /// Multi-row `edges` upsert, in submission order.
    Edges(Vec<&'a Edge>),
    /// A mutation that is not bulk-upsertable: applied on its own, in place.
    ///
    /// Never [`Mutation::UpsertNode`] or [`Mutation::UpsertEdge`] — those always
    /// arrive in one of the bucket variants.
    Single(&'a Mutation),
}

/// Plan the statements one [`crate::types::MutationBatch`] costs.
///
/// See the module docs for the ordering argument. The returned steps must be
/// executed in order, inside one transaction, after the batch's `sessions` rows
/// exist.
pub fn plan_flush<'a>(mutations: &'a [Mutation], limits: BulkLimits) -> Vec<FlushStep<'a>> {
    let mut steps = Vec::new();
    let mut buckets = Buckets::default();

    for m in mutations {
        match m {
            Mutation::UpsertNode {
                node: Node::Interaction(i),
            } => buckets.interactions.push(i),
            Mutation::UpsertNode {
                node: Node::Concept(c),
            } => buckets.concepts.push(c),
            Mutation::UpsertEdge { edge } => buckets.edges.push(edge),
            barrier => {
                buckets.drain_into(&mut steps, limits);
                steps.push(FlushStep::Single(barrier));
            }
        }
    }
    buckets.drain_into(&mut steps, limits);
    steps
}

#[derive(Default)]
struct Buckets<'a> {
    interactions: Vec<&'a Interaction>,
    concepts: Vec<&'a Concept>,
    edges: Vec<&'a Edge>,
}

impl<'a> Buckets<'a> {
    /// Emit every open bucket, FK order first (`interactions` before
    /// `concepts`), and reset. Empty buckets emit nothing.
    fn drain_into(&mut self, steps: &mut Vec<FlushStep<'a>>, limits: BulkLimits) {
        let interactions =
            dedupe_last_at_first_position(std::mem::take(&mut self.interactions), |i| i.id);
        for chunk in chunks(interactions, limits.interactions) {
            steps.push(FlushStep::Interactions(chunk));
        }

        let concepts = dedupe_concepts(std::mem::take(&mut self.concepts));
        for chunk in chunks(concepts, limits.concepts) {
            steps.push(FlushStep::Concepts(chunk));
        }

        // Natural-key conflict target, matching `UPSERT_EDGE_SQL`'s
        // `ON CONFLICT (source, target, edge_type)` — deduplicating by `id`
        // would leave two rows colliding on the real target.
        let edges = dedupe_last_at_first_position(std::mem::take(&mut self.edges), |e| {
            (e.source, e.target, e.edge_type)
        });
        for chunk in chunks(edges, limits.edges) {
            steps.push(FlushStep::Edges(chunk));
        }
    }
}

/// Split into statement-sized chunks. A limit of 0 is treated as 1 rather than
/// looping forever.
fn chunks<T>(items: Vec<T>, limit: usize) -> Vec<Vec<T>> {
    let limit = limit.max(1);
    let mut out = Vec::new();
    let mut rest = items;
    while !rest.is_empty() {
        let tail = rest.split_off(limit.min(rest.len()));
        out.push(std::mem::take(&mut rest));
        rest = tail;
    }
    out
}

/// Collapse repeats of `key`: **last occurrence's values, first occurrence's
/// position**.
///
/// # Why the position is the first one (R1-1)
///
/// The obvious spelling — keep the last occurrence where it stands — is wrong,
/// and was a live defect. It *relocates* a row past everything between its first
/// and last occurrence, and `interactions.previous_id REFERENCES
/// interactions(id)` is a **self** foreign key, so a batch's interactions can
/// reference each other:
///
/// ```text
/// submitted: [ i1(prev=None), i2(prev=i1), i1(prev=None) ]
/// last-position dedupe emits: [ i2(prev=i1), i1 ]   <- i2 references a row
///                                                      that does not exist yet
/// ```
///
/// With `BULK_LIMITS.interactions == 1` each interaction is its own statement
/// and both engines check foreign keys at end-of-statement, so `i2` fails
/// immediately: `SQLITE_CONSTRAINT_FOREIGNKEY` (787) on SQLite, an equivalent FK
/// violation on CockroachDB. `StoreError::Constraint` is classified **terminal**
/// (`super::error`), so the flush loop dead-letters the *whole batch* — bounded
/// data loss, the same class of failure L82-1 was raised for.
///
/// First-occurrence position is exactly what row-by-row replay does: replay
/// *inserts* each key the first time it appears and `DO UPDATE`s it thereafter,
/// so the order in which rows come into existence is first-occurrence order.
/// Emitting there preserves reference-before-use for free.
///
/// The *values* still come from the last occurrence, because every column of the
/// three upsert statements except a concept's canonization triple is in the
/// `DO UPDATE SET` list, and the last write to those wins under replay too. For
/// `interactions` the FK-bearing column is `previous_id`, and the graph tier
/// forbids a re-upsert from moving an interaction within the temporal chain
/// (`Graph::insert_interaction`, "would move it within the chain"), so
/// `previous_id` is invariant across occurrences of one id and the choice of
/// occurrence cannot change which row is referenced.
///
/// The rule is applied to **all three** buckets rather than only to
/// `interactions`. `concepts` and `edges` have no intra-table constraint that
/// position could break today (`concepts.origin_interaction` points at the
/// *interactions* bucket, which is drained first; `edges` carries no
/// `REFERENCES` at all — DDL at `migrations/cockroach/001_init.sql:29,40,141`),
/// so for them this is a no-op in effect. It is uniform anyway so that no future
/// column can reintroduce the bug by being added to a table whose planner arm
/// happened to keep the relocating spelling.
fn dedupe_last_at_first_position<T, K: Eq + std::hash::Hash + Clone>(
    items: Vec<T>,
    key: impl Fn(&T) -> K,
) -> Vec<T> {
    let mut last: HashMap<K, usize> = HashMap::with_capacity(items.len());
    let mut first_seen: Vec<K> = Vec::with_capacity(items.len());
    for (i, it) in items.iter().enumerate() {
        let k = key(it);
        if last.insert(k.clone(), i).is_none() {
            first_seen.push(k);
        }
    }
    let mut slots: Vec<Option<T>> = items.into_iter().map(Some).collect();
    first_seen
        .into_iter()
        .map(|k| {
            slots[last[&k]]
                .take()
                .expect("one surviving row per key, taken once")
        })
        .collect()
}

/// `dedupe_last_at_first_position` by concept id, keeping the **first**
/// occurrence's canonization columns. See [`ConceptRow`] for why the two halves
/// differ.
fn dedupe_concepts(items: Vec<&Concept>) -> Vec<ConceptRow<'_>> {
    let mut first_canonization: HashMap<NodeId, CanonizationColumns> =
        HashMap::with_capacity(items.len());
    for c in &items {
        first_canonization
            .entry(c.id)
            .or_insert_with(|| CanonizationColumns::of(c));
    }
    dedupe_last_at_first_position(items, |c| c.id)
        .into_iter()
        .map(|c| ConceptRow {
            concept: c,
            canonization: first_canonization[&c.id],
        })
        .collect()
}

/// Seed-path rows for `concepts`, deduplicated exactly as a flush is (R1-6).
///
/// `GraphStore::seed` takes an arbitrary [`crate::types::GraphSnapshot`]. One
/// built by `Graph::snapshot` has distinct ids by construction (the graph keys
/// its nodes by id), but the type does not *promise* that, and the pre-L82-1
/// seed path was a row-at-a-time loop that simply last-wins'd a repeat. Feeding
/// a repeat straight into a multi-row statement instead fails it outright
/// ("cannot affect row a second time"), so the bulk seed path must dedupe to
/// keep the behaviour it replaced.
pub fn seed_concept_rows(concepts: &[Concept]) -> Vec<ConceptRow<'_>> {
    dedupe_concepts(concepts.iter().collect())
}

/// Seed-path rows for `edges`, deduplicated on the natural key the SQL conflicts
/// on. See [`seed_concept_rows`] for why the seed path dedupes at all (R1-6).
pub fn seed_edge_rows(edges: &[Edge]) -> Vec<&Edge> {
    dedupe_last_at_first_position(edges.iter().collect(), |e| {
        (e.source, e.target, e.edge_type)
    })
}

/// Distinct session ids a batch writes into, in first-seen order.
///
/// Both adapters must ensure a `sessions` row exists before the rest of the
/// batch, because `interactions.session_id`, `concepts.session_id` and
/// `edges.session_id` all `REFERENCES sessions(session_id)` while the graph
/// tier creates sessions implicitly. Deletions name no session, so they are
/// skipped.
pub fn batch_session_ids(mutations: &[Mutation]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for m in mutations {
        let sid = match m {
            Mutation::UpsertNode { node } => node.session_id().as_str(),
            Mutation::UpsertEdge { edge } => edge.session_id.as_str(),
            Mutation::CanonizationTransition { event } => event.session_id.as_str(),
            Mutation::SetRootGoal { session_id, .. }
            | Mutation::SetEmbedding { session_id, .. } => session_id.as_str(),
            Mutation::DeleteNode { .. } | Mutation::DeleteEdge { .. } => continue,
        };
        if !out.contains(&sid) {
            out.push(sid);
        }
    }
    out
}

/// Statement count a planned flush costs. Handy for tests and for the
/// latency arithmetic in the module docs.
pub fn planned_statements(mutations: &[Mutation], limits: BulkLimits) -> usize {
    plan_flush(mutations, limits).len()
}

/// Edge natural key, spelled out so the dedupe key and the SQL conflict target
/// cannot drift apart silently.
#[allow(dead_code)]
type EdgeKey = (NodeId, NodeId, EdgeType);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentId, CanonizationEvent, ConceptType, SessionId};
    use chrono::TimeZone;

    const LIMITS: BulkLimits = BulkLimits {
        interactions: 1,
        concepts: 256,
        edges: 512,
    };

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000 + secs, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("plan")
    }

    fn interaction(id: NodeId, previous_id: Option<NodeId>) -> Interaction {
        Interaction {
            id,
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("p".into()),
            previous_id,
            created_at: ts(0),
        }
    }

    fn concept(id: NodeId, origin: NodeId, content: &str) -> Concept {
        Concept {
            id,
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(0),
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

    fn edge(id: NodeId, source: NodeId, target: NodeId, ty: EdgeType) -> Edge {
        Edge {
            id,
            session_id: sid(),
            source,
            target,
            edge_type: ty,
            weight: 0.5,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        }
    }

    /// The shape `Memory::record_action` actually appends: `insert_concept`
    /// emits `UpsertNode` **then** the concept's `Derives` `UpsertEdge`, so the
    /// log interleaves node and edge upserts one for one. A planner that only
    /// coalesced *adjacent* same-kind runs would produce runs of length 1 here
    /// and buy nothing — which is the whole reason this bucket by table rather
    /// than by run.
    fn at_cap_record_action(concepts_per_call: usize, extra_edges: usize) -> Vec<Mutation> {
        at_cap_record_action_after(concepts_per_call, extra_edges, None).0
    }

    /// As above, chained onto `previous` the way `begin_interaction` does —
    /// which also emits the interaction's `Temporal` edge. Returns the new
    /// interaction id so a caller can build a chain.
    fn at_cap_record_action_after(
        concepts_per_call: usize,
        extra_edges: usize,
        previous: Option<NodeId>,
    ) -> (Vec<Mutation>, NodeId) {
        let mut out = Vec::new();
        let iid = NodeId::new();
        out.push(Mutation::UpsertNode {
            node: Node::Interaction(interaction(iid, previous)),
        });
        if let Some(prev) = previous {
            out.push(Mutation::UpsertEdge {
                edge: edge(NodeId::new(), iid, prev, EdgeType::Temporal),
            });
        }
        let mut ids = Vec::new();
        for n in 0..concepts_per_call {
            let cid = NodeId::new();
            ids.push(cid);
            out.push(Mutation::UpsertNode {
                node: Node::Concept(concept(cid, iid, &format!("concept {n}"))),
            });
            out.push(Mutation::UpsertEdge {
                edge: edge(NodeId::new(), cid, iid, EdgeType::Derives),
            });
        }
        for n in 0..extra_edges {
            out.push(Mutation::UpsertEdge {
                edge: edge(
                    NodeId::new(),
                    ids[n % ids.len()],
                    ids[(n + 1) % ids.len()],
                    EdgeType::Causal,
                ),
            });
        }
        (out, iid)
    }

    /// **L82-1, the pin.** The live repro — four at-cap `lambo_record_action`
    /// calls, 784 log mutations — must cost a handful of statements, not one
    /// per mutation.
    ///
    /// The budget below is the finding's arithmetic run backwards. `CLOSE_GRACE`
    /// is 10 s and a serverless CockroachDB round-trip measured 10–30 ms, so
    /// per-row replay costs 784 × 30 ms ≈ 23 s and cannot finish; 24 statements
    /// at the same 30 ms is 0.72 s, comfortably inside the window even with the
    /// transaction's own `BEGIN`/`COMMIT` and the retry wrapper on top.
    #[test]
    fn an_at_cap_burst_costs_a_handful_of_statements() {
        // Four chained at-cap calls: 4 interactions + 3 temporal edges
        // + 4 × (65 concepts + 65 Derives edges + 64 Causal edges), plus one
        // canonization transition from the daemon running alongside — the
        // live tail was 784.
        let mut mutations = Vec::new();
        let mut previous = None;
        for _ in 0..4 {
            let (calls, iid) = at_cap_record_action_after(65, 64, previous);
            mutations.extend(calls);
            previous = Some(iid);
        }
        mutations.push(Mutation::CanonizationTransition {
            event: CanonizationEvent {
                id: NodeId::new(),
                session_id: sid(),
                node_id: NodeId::new(),
                from_status: CanonizationStatus::None,
                to_status: CanonizationStatus::Candidate,
                blast_radius: Some(1),
                last_demotion_time: None,
                occurred_at: ts(1),
            },
        });
        assert_eq!(
            mutations.len(),
            784,
            "this must stay the live L82-1 repro's tail size"
        );

        let statements = planned_statements(&mutations, LIMITS);
        assert!(
            statements <= 24,
            "a {}-mutation tail planned into {statements} statements — per-row replay is what \
             made SIGTERM lose the tail (L82-1)",
            mutations.len()
        );
    }

    /// Bucketing must survive the interleaving, not just tolerate it: 130
    /// alternating node/edge upserts are 2 statements, not 130.
    #[test]
    fn interleaved_upserts_coalesce_across_the_interleaving() {
        let mutations = at_cap_record_action(65, 0);
        assert_eq!(mutations.len(), 131, "1 interaction + 65 × (node, edge)");
        // 1 interaction statement + 1 concepts statement + 1 edges statement.
        assert_eq!(planned_statements(&mutations, LIMITS), 3);
    }

    /// Interactions precede concepts in every emitted plan —
    /// `concepts.origin_interaction REFERENCES interactions(id)`.
    #[test]
    fn interactions_are_emitted_before_concepts() {
        let mutations = at_cap_record_action(3, 2);
        let steps = plan_flush(&mutations, LIMITS);
        let first_concept = steps
            .iter()
            .position(|s| matches!(s, FlushStep::Concepts(_)))
            .expect("a concepts step");
        let last_interaction = steps
            .iter()
            .rposition(|s| matches!(s, FlushStep::Interactions(_)))
            .expect("an interactions step");
        assert!(
            last_interaction < first_concept,
            "the FK direction is concepts -> interactions: {steps:?}"
        );
    }

    /// Every non-upsert variant is a barrier: buckets opened before it are
    /// emitted first, and it is emitted alone, so nothing that could observe a
    /// row is ever reordered past a write to it.
    #[test]
    fn barriers_split_the_buckets_and_keep_their_place() {
        let iid = NodeId::new();
        let c1 = NodeId::new();
        let c2 = NodeId::new();
        let mutations = vec![
            Mutation::UpsertNode {
                node: Node::Interaction(interaction(iid, None)),
            },
            Mutation::UpsertNode {
                node: Node::Concept(concept(c1, iid, "one")),
            },
            Mutation::DeleteNode { id: c1 },
            Mutation::UpsertNode {
                node: Node::Concept(concept(c2, iid, "two")),
            },
        ];
        let steps = plan_flush(&mutations, LIMITS);
        assert!(
            matches!(
                steps.as_slice(),
                [
                    FlushStep::Interactions(_),
                    FlushStep::Concepts(a),
                    FlushStep::Single(Mutation::DeleteNode { .. }),
                    FlushStep::Concepts(b),
                ] if a.len() == 1 && b.len() == 1
            ),
            "the delete must not be reordered past either concept: {steps:?}"
        );
    }

    /// Each barrier variant really is one. A concept upsert on either side of
    /// each must land in a different statement.
    ///
    /// All **five** non-upsert variants of `Mutation` are listed (R1-5). The
    /// catch-all `barrier =>` arm in [`plan_flush`] makes this correct by
    /// construction, but `CanonizationTransition` is the barrier the concept
    /// dedupe rule leans on — the first-occurrence-canonization choice in
    /// [`ConceptRow`] is only sound because a demote between two upserts of one
    /// concept cannot be collapsed across — so it is the one that most needs an
    /// executable statement of the property rather than a comment.
    #[test]
    fn every_non_upsert_variant_is_a_barrier() {
        let iid = NodeId::new();
        let cid = NodeId::new();
        for barrier in [
            Mutation::DeleteNode { id: cid },
            Mutation::DeleteEdge { id: NodeId::new() },
            Mutation::SetRootGoal {
                session_id: sid(),
                goal: None,
            },
            Mutation::SetEmbedding {
                session_id: sid(),
                embedding: None,
            },
            Mutation::CanonizationTransition {
                event: CanonizationEvent {
                    id: NodeId::new(),
                    session_id: sid(),
                    node_id: cid,
                    from_status: CanonizationStatus::None,
                    to_status: CanonizationStatus::Candidate,
                    blast_radius: Some(1),
                    last_demotion_time: None,
                    occurred_at: ts(1),
                },
            },
        ] {
            let mutations = vec![
                Mutation::UpsertNode {
                    node: Node::Concept(concept(cid, iid, "before")),
                },
                barrier.clone(),
                Mutation::UpsertNode {
                    node: Node::Concept(concept(NodeId::new(), iid, "after")),
                },
            ];
            let steps = plan_flush(&mutations, LIMITS);
            assert_eq!(
                steps.len(),
                3,
                "{barrier:?} must split the two concept upserts: {steps:?}"
            );
        }
    }

    /// A repeated concept id collapses to one row — required, because both
    /// PostgreSQL and CockroachDB refuse a multi-row upsert whose inputs collide
    /// on the conflict target.
    ///
    /// The collapsed row takes the **last** occurrence's ordinary columns and
    /// the **first** occurrence's canonization columns, which is what row-by-row
    /// replay leaves durable (see [`ConceptRow`]).
    #[test]
    fn a_repeated_concept_collapses_last_wins_but_keeps_the_first_canonization() {
        let iid = NodeId::new();
        let cid = NodeId::new();
        let mut early = concept(cid, iid, "early text");
        early.canonization_status = CanonizationStatus::Canonical;
        early.blast_radius = Some(7);
        early.last_demotion_time = Some(ts(10));
        let mut late = concept(cid, iid, "late text");
        late.canonization_status = CanonizationStatus::None;
        late.blast_radius = None;
        late.last_demotion_time = None;
        late.gc_survived = 3;

        let mutations = vec![
            Mutation::UpsertNode {
                node: Node::Concept(early),
            },
            Mutation::UpsertNode {
                node: Node::Concept(late),
            },
        ];
        let steps = plan_flush(&mutations, LIMITS);
        let [FlushStep::Concepts(rows)] = steps.as_slice() else {
            panic!("expected one concepts step, got {steps:?}");
        };
        assert_eq!(rows.len(), 1, "the duplicate id must collapse");
        assert_eq!(rows[0].concept.content, "late text");
        assert_eq!(rows[0].concept.gc_survived, 3);
        assert_eq!(
            rows[0].canonization,
            CanonizationColumns {
                status: CanonizationStatus::Canonical,
                blast_radius: Some(7),
                last_demotion_time: Some(ts(10)),
            },
            "a stale upsert behind a hop must not regress the status of a concept born \
             mid-progression (R2-1)"
        );
    }

    /// Edges deduplicate by the natural key the SQL conflicts on, not by `id`:
    /// two different ids on one `(source, target, edge_type)` are one row.
    #[test]
    fn edges_dedupe_on_the_natural_key_not_the_id() {
        let a = NodeId::new();
        let b = NodeId::new();
        let winner = NodeId::new();
        let mutations = vec![
            Mutation::UpsertEdge {
                edge: edge(NodeId::new(), a, b, EdgeType::Causal),
            },
            Mutation::UpsertEdge {
                edge: edge(winner, a, b, EdgeType::Causal),
            },
            // Different edge_type — a distinct natural key, so it survives.
            Mutation::UpsertEdge {
                edge: edge(NodeId::new(), a, b, EdgeType::Dependency),
            },
        ];
        let steps = plan_flush(&mutations, LIMITS);
        let [FlushStep::Edges(rows)] = steps.as_slice() else {
            panic!("expected one edges step, got {steps:?}");
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].id, winner,
            "last write wins, at the first occurrence's position"
        );
        assert_eq!(rows[1].edge_type, EdgeType::Dependency);
    }

    /// **R1-1, the pin.** A repeated interaction must not be relocated past a
    /// later interaction that chains onto it.
    ///
    /// `interactions.previous_id REFERENCES interactions(id)` is a self FK, so
    /// emitting `i1` *after* `i2(prev=i1)` makes `i2`'s statement fail its FK
    /// check, and `Constraint` is terminal — the whole batch is dead-lettered.
    /// The equivalent SQLite round trip is
    /// `sqlite::a_repeated_interaction_does_not_outrun_the_row_that_chains_onto_it`,
    /// which fails with `SQLITE_CONSTRAINT_FOREIGNKEY` if this rule regresses.
    #[test]
    fn a_repeated_interaction_keeps_its_first_position() {
        let i1 = NodeId::new();
        let i2 = NodeId::new();
        let mut late = interaction(i1, None);
        late.prompt_text = Some("re-upserted".into());
        let mutations = vec![
            Mutation::UpsertNode {
                node: Node::Interaction(interaction(i1, None)),
            },
            Mutation::UpsertNode {
                node: Node::Interaction(interaction(i2, Some(i1))),
            },
            Mutation::UpsertNode {
                node: Node::Interaction(late),
            },
        ];
        // `interactions: 1`, so each surviving row is its own statement.
        let steps = plan_flush(&mutations, LIMITS);
        let order: Vec<NodeId> = steps
            .iter()
            .flat_map(|s| match s {
                FlushStep::Interactions(rows) => rows.iter().map(|i| i.id).collect::<Vec<_>>(),
                other => panic!("expected interactions, got {other:?}"),
            })
            .collect();
        assert_eq!(
            order,
            vec![i1, i2],
            "the repeat must collapse at i1's FIRST position — re-emitting i1 after i2 breaks \
             `previous_id REFERENCES interactions(id)` and dead-letters the batch (R1-1)"
        );
        let [FlushStep::Interactions(first), ..] = steps.as_slice() else {
            panic!("expected an interactions step, got {steps:?}");
        };
        assert_eq!(
            first[0].prompt_text.as_deref(),
            Some("re-upserted"),
            "position is the first occurrence's, but the VALUES are still the last one's — \
             that is what row-by-row replay leaves durable"
        );
    }

    /// The same rule, one level up: relocation must not move a row past *any*
    /// later row, whichever bucket. A concept repeated on both sides of a run
    /// keeps its first slot, so a plan's row order is always first-seen order.
    #[test]
    fn dedupe_never_moves_a_row_later_in_the_plan() {
        let iid = NodeId::new();
        let a = NodeId::new();
        let b = NodeId::new();
        let mutations = vec![
            Mutation::UpsertNode {
                node: Node::Concept(concept(a, iid, "a")),
            },
            Mutation::UpsertNode {
                node: Node::Concept(concept(b, iid, "b")),
            },
            Mutation::UpsertNode {
                node: Node::Concept(concept(a, iid, "a again")),
            },
        ];
        let steps = plan_flush(&mutations, LIMITS);
        let [FlushStep::Concepts(rows)] = steps.as_slice() else {
            panic!("expected one concepts step, got {steps:?}");
        };
        assert_eq!(
            rows.iter().map(|r| r.concept.id).collect::<Vec<_>>(),
            vec![a, b],
            "first-seen order, not last-seen"
        );
        assert_eq!(rows[0].concept.content, "a again", "values still last-wins");
    }

    /// Chunking is what keeps a statement inside the backend's bind-parameter
    /// limit, so the plan must actually split at the limit.
    #[test]
    fn buckets_split_at_the_row_limit() {
        let iid = NodeId::new();
        let mutations: Vec<Mutation> = (0..10)
            .map(|n| Mutation::UpsertNode {
                node: Node::Concept(concept(NodeId::new(), iid, &format!("c{n}"))),
            })
            .collect();
        let limits = BulkLimits {
            interactions: 1,
            concepts: 3,
            edges: 3,
        };
        let steps = plan_flush(&mutations, limits);
        assert_eq!(steps.len(), 4, "10 rows at 3 per statement");
        let sizes: Vec<usize> = steps
            .iter()
            .map(|s| match s {
                FlushStep::Concepts(r) => r.len(),
                other => panic!("expected concepts, got {other:?}"),
            })
            .collect();
        assert_eq!(sizes, vec![3, 3, 3, 1]);
    }

    /// No upsert may reach [`FlushStep::Single`] — the adapters rely on that to
    /// keep the bulk path the only path for volume.
    #[test]
    fn upserts_never_reach_the_single_step() {
        let mutations = at_cap_record_action(8, 8);
        for step in plan_flush(&mutations, LIMITS) {
            if let FlushStep::Single(m) = step {
                assert!(
                    !matches!(m, Mutation::UpsertNode { .. } | Mutation::UpsertEdge { .. }),
                    "{m:?} must have been bucketed"
                );
            }
        }
    }

    /// Every mutation must appear exactly once in the plan — a planner that
    /// silently dropped a bucket would look fast and lose data.
    #[test]
    fn the_plan_covers_every_mutation_exactly_once() {
        let mut mutations = at_cap_record_action(5, 5);
        mutations.push(Mutation::DeleteEdge { id: NodeId::new() });
        mutations.extend(at_cap_record_action(5, 5));
        let planned: usize = plan_flush(&mutations, LIMITS)
            .iter()
            .map(|s| match s {
                FlushStep::Interactions(r) => r.len(),
                FlushStep::Concepts(r) => r.len(),
                FlushStep::Edges(r) => r.len(),
                FlushStep::Single(_) => 1,
            })
            .sum();
        assert_eq!(
            planned,
            mutations.len(),
            "no mutation may be dropped or duplicated by planning"
        );
    }

    /// An empty batch plans nothing (the adapters shortcut on it, but the
    /// planner must not depend on that).
    #[test]
    fn an_empty_batch_plans_no_statements() {
        assert!(plan_flush(&[], LIMITS).is_empty());
    }

    /// **R1-6.** The seed path feeds an arbitrary caller-built snapshot into the
    /// same multi-row statements, which reject colliding input rows outright.
    /// The row-at-a-time loop this replaced last-wins'd them, so the seed helpers
    /// must collapse repeats the same way a flush does.
    #[test]
    fn seed_rows_collapse_repeats_like_the_flush_path() {
        let iid = NodeId::new();
        let cid = NodeId::new();
        let mut born = concept(cid, iid, "early");
        born.canonization_status = CanonizationStatus::Canonical;
        born.blast_radius = Some(4);
        let mut later = concept(cid, iid, "late");
        later.gc_survived = 2;
        let other = concept(NodeId::new(), iid, "other");
        let other_id = other.id;
        let snapshot_concepts = [born, other, later];
        let rows = seed_concept_rows(&snapshot_concepts);
        assert_eq!(rows.len(), 2, "the repeated id must collapse");
        assert_eq!(rows[0].concept.id, cid, "first-seen order");
        assert_eq!(rows[0].concept.content, "late", "ordinary columns last-win");
        assert_eq!(
            rows[0].canonization.status,
            CanonizationStatus::Canonical,
            "canonization columns come from the first occurrence, as in a flush"
        );
        assert_eq!(rows[1].concept.id, other_id);

        let a = NodeId::new();
        let b = NodeId::new();
        let winner = NodeId::new();
        let snapshot_edges = [
            edge(NodeId::new(), a, b, EdgeType::Causal),
            edge(NodeId::new(), a, b, EdgeType::Dependency),
            edge(winner, a, b, EdgeType::Causal),
        ];
        let edges = seed_edge_rows(&snapshot_edges);
        assert_eq!(edges.len(), 2, "the repeated natural key must collapse");
        assert_eq!(
            edges[0].id, winner,
            "last write wins, at the first position"
        );
        assert_eq!(edges[1].edge_type, EdgeType::Dependency);
    }

    #[test]
    fn session_ids_are_first_seen_order_and_skip_deletions() {
        let iid = NodeId::new();
        let mut other = interaction(NodeId::new(), None);
        other.session_id = SessionId::from("other");
        let mutations = vec![
            Mutation::UpsertNode {
                node: Node::Interaction(interaction(iid, None)),
            },
            Mutation::DeleteNode { id: NodeId::new() },
            Mutation::UpsertNode {
                node: Node::Interaction(other),
            },
            Mutation::UpsertNode {
                node: Node::Concept(concept(NodeId::new(), iid, "c")),
            },
        ];
        assert_eq!(batch_session_ids(&mutations), vec!["plan", "other"]);
    }
}
