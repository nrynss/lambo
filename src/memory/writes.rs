//! Write operations: session metadata, `derive` / `record_action` (sync and
//! acknowledged-before-the-embedder), `demote`, `retract`, and soft locks.
//!
//! Invariants kept here:
//!
//! * every mutating method enters the writers gate first ([`super::gate`]);
//! * **the graph lock is never held across `.await`**: each method takes it,
//!   works, releases, and only then does I/O;
//! * **graph before index**: every concept write is mirrored into the inverted
//!   index through `mirror_concepts` (graph read, then index write), and
//!   `retract` removes from both under the graph write lock;
//! * **interactions are server-stamped** from the process clock
//!   (`begin_interaction_full`); no caller supplies a timestamp;
//! * the synchronous surface stays synchronous and read-your-writes; the
//!   `*_async_as` methods are the additive J3 path that validates on the call
//!   path, pins the interaction, and hands the job to the write queue, whose
//!   execution and receipt state `Memory` owns ([`Memory::pipeline`]).

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{DryRun, ImpactReport, Memory};
use crate::graph::action::{record_action as graph_record_action, Action, ActionOutcome};
use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::demote::demote as graph_demote;
use crate::graph::derive::{derive as graph_derive, DeriveOutcome, ParentOf};
use crate::graph::reserve::{release as graph_release, reserve as graph_reserve};
use crate::graph::{hybrid, Graph};
use crate::recall::format;
use crate::types::{
    AgentId, Concept, ConceptType, Interaction, LamboError, MatchStrategy, Node, NodeId,
    Reservation, StoreError,
};
use crate::writeq::{Submitted, WritePipeline};
use std::sync::Arc;

/// Bound on [`Memory::retract`]'s durable blast-radius query (R2-5).
///
/// It was the one store call on a user-facing path with no bound at all, and
/// the writers gate turned that into `close()`'s problem: `retract` holds a
/// read permit across this await, so `close()`'s step 0 waited on it — an
/// unresponsive backend made shutdown unbounded, defeating the point of the
/// `FLUSH_ATTEMPT_TIMEOUT` bound on step 4.
///
/// **The same 30s as [`hybrid::HYBRID_IO_TIMEOUT`]**, and defined from it so
/// there is one number: that constant bounds exactly this shape — the store I/O
/// of a `&self` write method that holds the gate — and `retract` earning its own
/// value would only invite the two to drift. Named for its own site because the
/// hybrid *derive* path is not the caller.
pub(super) const RETRACT_IO_TIMEOUT: Duration = hybrid::HYBRID_IO_TIMEOUT;

/// Resolve a caller-supplied string to a concept id.
///
/// Canonicalization first (so synonyms and casing work), then an exact
/// `content` match — the fallback is what makes demoted `Observation`s
/// reachable, since canonicalization's match step skips them by design.
/// The fallback picks the lowest id among equal matches so the choice is
/// deterministic rather than `HashMap`-iteration dependent.
pub(super) fn resolve_concept(graph: &Graph, target: &str) -> Result<NodeId, LamboError> {
    if let CanonicalizeResult::Matched { node, .. } = canonicalize(target, graph)? {
        return Ok(node);
    }
    let exact: Option<&Concept> = graph
        .concepts()
        .filter(|c| c.content == target)
        .min_by_key(|c| c.id.0);
    match exact {
        Some(c) => Ok(c.id),
        None => Err(LamboError::Store(StoreError::NotFound(format!(
            "no concept matching {target:?} in session {}",
            graph.session_id()
        )))),
    }
}

impl Memory {
    /// Declare the session's root goal (spec §9 drift anchor). Concepts the
    /// goal names are promoted to `Venerable` through the audited transition
    /// path, so the promotion is durable.
    pub fn set_root_goal(&self, goals: &[&str]) -> Result<(), LamboError> {
        let _writing = self.begin_write_sync()?;
        let value = serde_json::to_value(goals)
            .map_err(|e| LamboError::Config(format!("set_root_goal: {e}")))?;
        self.graph.write().set_root_goal(Some(value));
        self.daemon.wake();
        Ok(())
    }

    /// Map `source` onto `canonical` for canonicalization (spec §7.1 step 4).
    ///
    /// # Not durable (pinned upstream contract S5)
    ///
    /// Synonyms are **RAM-local for this handle's lifetime**. There is no
    /// `Mutation` kind for them by pinned S5 design, so no flush — not even
    /// [`Memory::close`]'s final one — writes them, and `load_session` cannot
    /// restore them: after a reattach the map is empty again.
    ///
    /// The consequence is not cosmetic. A synonym is what makes
    /// `register_user` resolve onto the existing `create_user` concept; once
    /// it is gone the same phrase **creates a duplicate concept** instead of
    /// matching, and [`Memory::retract`]'s resolution loses the alias too. A
    /// caller that needs the mapping across restarts must re-declare it on
    /// every attach (do it right after `build()`, before the first
    /// [`Memory::derive`]).
    pub fn declare_synonym(&self, source: &str, canonical: &str) -> Result<(), LamboError> {
        let _writing = self.begin_write_sync()?;
        self.graph.write().declare_synonym(source, canonical);
        Ok(())
    }

    /// Record one explicit human confirmation of a concept (C2, spec §3.2's
    /// "Human Confirmed" term — the solo score's `Human Confirmed × 4.0`).
    ///
    /// ## What "human confirmed" means operationally
    ///
    /// A **deliberate human verdict on one concept**, carried by a dedicated
    /// verb rather than inferred: no agent write path (`derive`,
    /// `record_action`, recall) reaches this counter, so agent activity cannot
    /// inflate the heaviest term in the solo formula. Each call bumps the
    /// concept's `human_confirmed` count by one and appends an `UpsertNode`
    /// mutation, so the value is durable on every store adapter. Surfacing the
    /// verb on MCP/CLI is deliberately deferred: unlike `event_time`, whose MCP
    /// wire form now has a historical-ingest consumer, `confirm_human` still
    /// needs a consumer-designed wire contract. The in-process API remains the
    /// contract future confirm tooling will call.
    ///
    /// A missing id or a non-concept node is an error — a confirmation that
    /// cannot be applied must fail loudly, never silently vanish.
    pub fn confirm_human(&self, node: NodeId) -> Result<i32, LamboError> {
        let _writing = self.begin_write_sync()?;
        let confirmed = self.graph.write().confirm_human(node)?;
        self.daemon.wake();
        Ok(confirmed)
    }

    /// Derive concepts from a fresh interaction (spec §7) — **async**.
    ///
    /// Async because `MatchStrategy::Hybrid` dispatches to
    /// [`crate::graph::hybrid::derive`], which embeds and queries the store.
    /// One shape serves both strategies rather than two divergent signatures;
    /// the `Canonical` arm does no I/O and never awaits.
    ///
    /// Mirrors every created **and** matched concept into the inverted index
    /// (`index.add` is idempotent per node id, so re-mirroring a matched
    /// concept is a cheap re-index, not a duplicate posting).
    ///
    /// A failure after the interaction was opened leaves that interaction in
    /// the graph — interactions are append-only in v0.1 (spec §9) and an empty
    /// one is harmless. `derive` itself is validate-then-mutate, so no partial
    /// concept write can survive an error.
    pub async fn derive(
        &self,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
    ) -> Result<DeriveOutcome, LamboError> {
        self.derive_as(&self.agent, concepts, parent_of).await
    }

    /// [`Memory::derive`] on behalf of `agent` (J1).
    ///
    /// The acting agent lands on the interaction this call opens and on every
    /// `Provenance` edge below it, so "who derived this" survives into the
    /// graph rather than being flattened to the handle's own id. Everything
    /// else — validation, canonicalization, the write-behind log, the
    /// single-writer lease and its fencing token — is unchanged and still
    /// process-wide: this parameter names the *writer*, not a second session.
    ///
    /// Identity is whatever the caller passed. Over MCP that is caller-asserted
    /// and unauthenticated (see `lambo_reserve`'s tool doc), which is exactly
    /// the trust level lambo's soft locks already assume.
    pub async fn derive_as(
        &self,
        agent: &AgentId,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
    ) -> Result<DeriveOutcome, LamboError> {
        self.derive_for_ingest_as(agent, None, concepts, parent_of)
            .await
    }

    /// [`Memory::derive`] for a fact whose about-time the caller knows (D).
    ///
    /// This is the historical-corpus entry point: `event_time` — a commit
    /// date, a transcript timestamp — is carried on the interaction this call
    /// opens and inherited by every concept and edge derived under it. Flush
    /// time stays process-stamped exactly as in [`Memory::derive`]; F18's
    /// server-authority rule is about *observed-at* claims, not about-time,
    /// which no store-side clock could know. Canonization's age floors,
    /// coverage bar and session separation then measure the replayed history
    /// on its own timeline; see `crate::canon::event_time`.
    ///
    /// Passing `None` is exactly [`Memory::derive`]: the fallback rule makes
    /// the interaction behave as if D never happened.
    pub async fn derive_for_ingest(
        &self,
        event_time: DateTime<Utc>,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
    ) -> Result<DeriveOutcome, LamboError> {
        self.derive_for_ingest_as(&self.agent, Some(event_time), concepts, parent_of)
            .await
    }

    /// [`Memory::derive_for_ingest`] on behalf of `agent` (J1), with an
    /// explicit `Option` so an ingester can mix timestamped turns with live
    /// ones through one seam.
    pub async fn derive_for_ingest_as(
        &self,
        agent: &AgentId,
        event_time: Option<DateTime<Utc>>,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
    ) -> Result<DeriveOutcome, LamboError> {
        // Held across every await below, so a concurrent `close()` either
        // waits for this whole derive or refuses it (T81-1).
        let _writing = self.begin_write().await?;
        let prompt = hybrid::derive_prompt(concepts.iter().map(|(content, _)| *content));
        let interaction = self.begin_interaction_full(agent, Some(prompt), event_time)?;

        let outcome = match self.config.match_strategy {
            MatchStrategy::Hybrid => {
                hybrid::derive_with(
                    self.graph.clone(),
                    self.vector_candidates(),
                    self.embedder.as_ref(),
                    &self.embedding,
                    interaction,
                    agent,
                    concepts,
                    parent_of,
                    self.config.max_cooccurrence_per_derive,
                    self.config.semantic_match_threshold,
                    // The synchronous path has no durable intent to consume —
                    // the caller holds the outcome directly (J3).
                    None,
                )
                .await?
            }
            MatchStrategy::Canonical => {
                // Short critical section; the guard dies with this block, well
                // before the mirroring below. No `.await` inside it (§6.4).
                let mut g = self.graph.write();
                graph_derive(
                    &mut g,
                    interaction,
                    agent,
                    concepts,
                    parent_of,
                    self.config.max_cooccurrence_per_derive,
                )?
            }
        };

        let mut touched = outcome.created.clone();
        touched.extend(outcome.matched.iter().copied());
        self.mirror_concepts(&touched);
        self.daemon.wake();
        Ok(outcome)
    }

    /// Record an agent action (spec §7): a `Resource` concept plus `Causal` /
    /// `Dependency` edges, on a fresh interaction.
    ///
    /// Synchronous — unlike `derive` there is no hybrid twin and no I/O.
    pub fn record_action(&self, action: &Action<'_>) -> Result<ActionOutcome, LamboError> {
        self.record_action_as(&self.agent, action)
    }

    /// [`Memory::record_action`] on behalf of `agent` (J1). See
    /// [`Memory::derive_as`] for what the per-call id does and does not change.
    ///
    /// The action's own `event_time` (D) — when it carries one — stamps the
    /// interaction opened for this call, and through it every edge the call
    /// creates. Flush time is still process-stamped here, never caller-set.
    pub fn record_action_as(
        &self,
        agent: &AgentId,
        action: &Action<'_>,
    ) -> Result<ActionOutcome, LamboError> {
        let _writing = self.begin_write_sync()?;
        let interaction =
            self.begin_interaction_full(agent, Some(action.action.to_string()), action.event_time)?;
        let outcome = {
            let mut g = self.graph.write();
            graph_record_action(&mut g, interaction, agent, action)?
        };

        // The action node may be pre-existing (already indexed) — mirroring it
        // anyway is idempotent and covers the case where it is not.
        let mut touched = outcome.created.clone();
        touched.push(outcome.action_node);
        self.mirror_concepts(&touched);
        self.daemon.wake();
        Ok(outcome)
    }

    /// [`Memory::record_action`] with an embedder hop, so the concepts it
    /// creates are findable by semantic recall and not only by keyword.
    ///
    /// **Async because embedding is I/O**, which is precisely why
    /// [`Memory::record_action`] never did it: that entry point is
    /// synchronous, and a sync signature has nowhere to put a model call. The
    /// cost of the omission was measured on the dogfood session 2026-09-01 —
    /// 555 of 946 concepts with no vector, every one of them from an action —
    /// so the sync path is now the deliberate keyword-only choice rather than
    /// the default one. Callers holding a runtime should prefer this.
    ///
    /// Under [`MatchStrategy::Canonical`] this is exactly
    /// [`Memory::record_action_as`]: that strategy has no vector leg, and
    /// embedding here would stamp a contract on a session that asked for none.
    /// Under `Hybrid` on a store without `VECTOR_SEARCH` nothing is embedded
    /// either, the same degrade hybrid `derive` makes.
    ///
    /// An embedder failure fails the call with **nothing written** (J3-R3-1's
    /// rule, see [`crate::graph::action::embed_action_contents`]).
    pub async fn record_action_embedded_as(
        &self,
        agent: &AgentId,
        action: &Action<'_>,
    ) -> Result<ActionOutcome, LamboError> {
        if self.config.match_strategy == MatchStrategy::Canonical {
            return self.record_action_as(agent, action);
        }
        // Held across the embed await below, so a concurrent `close()` either
        // waits for this whole call or refuses it (T81-1), matching `derive`.
        let _writing = self.begin_write().await?;
        {
            let g = self.graph.read();
            crate::graph::action::validate(&g, action)?;
        }
        // Off-lock: real model calls. Skipped when the store cannot search
        // vectors, as hybrid `derive` skips them: the write is keyword-only.
        let embeddings = if self.vector_candidates().available() {
            crate::graph::action::embed_action_contents(self.embedder.as_ref(), action).await?
        } else {
            crate::graph::action::ActionEmbeddings::new()
        };
        let interaction =
            self.begin_interaction_full(agent, Some(action.action.to_string()), action.event_time)?;
        let outcome = {
            let mut g = self.graph.write();
            if !embeddings.is_empty() {
                g.stamp_embedding(self.embedding.clone())?;
            }
            crate::graph::action::record_action_with_embeddings(
                &mut g,
                interaction,
                agent,
                action,
                &embeddings,
            )?
        };
        let mut touched = outcome.created.clone();
        touched.push(outcome.action_node);
        self.mirror_concepts(&touched);
        self.daemon.wake();
        Ok(outcome)
    }

    /// The J3 write pipeline and its receipt store.
    ///
    /// The MCP server needs it for the two delivery surfaces the pipeline
    /// deliberately does not own — the piggyback on the next tool response and
    /// the fetch-by-id tool — and `lambo_stats` needs its counters.
    pub fn pipeline(&self) -> &Arc<WritePipeline> {
        &self.pipeline
    }

    /// [`Memory::derive_as`] **acknowledged before the embedder** (J3).
    ///
    /// What stays synchronous, and why each part does:
    ///
    /// * The **writers gate**, so a concurrent `close()` cannot slip between
    ///   the checks below and the enqueue.
    /// * The **validation pre-pass**, so the errors a caller can actually fix
    ///   still arrive at call time rather than on a receipt. It is **the
    ///   pre-pass the session's `match_strategy` actually uses**, and the two
    ///   are not the same set of rules:
    ///   * `Hybrid` (the default — see `config.rs`): `hybrid::validate_limits`
    ///     then `hybrid::validate_graph_inputs` and
    ///     `hybrid::validate_embed_budget`, which is deliberately the
    ///     **smaller** set. It omits the repeated-`Observation` and
    ///     single-`Hierarchical`-parent rejections, because hybrid's own write
    ///     path does not enforce them and validation that disagrees with the
    ///     write is worse than none (defect 3 in §J3 Status).
    ///   * `Canonical`: `hybrid::validate_limits` then
    ///     [`crate::graph::derive::validate`], the read-only half of the
    ///     synchronous path — the same checks against the same graph in the
    ///     same order.
    ///
    ///   Under `Hybrid`, then, five error classes move from call time to the
    ///   receipt, and all five need the embedder or the store: embedder
    ///   failure, an embedder dim/contract mismatch,
    ///   [`crate::graph::hybrid::HYBRID_IO_TIMEOUT`] expiry,
    ///   `MAX_HYBRID_REPLANS` exhaustion, and store errors from the vector
    ///   candidate check. Nothing a *caller* could act on moved, and no rule
    ///   was removed from the write path: `validate_graph_inputs` still runs
    ///   inside `derive_planned`'s phase 1. Under `Canonical` nothing moves.
    ///   (The bullet this replaces claimed "the same checks, run against the
    ///   same graph, in the same order" for every strategy, three lines above
    ///   the comment correcting it — J3-R1-4.)
    /// * The **interaction**, which pins this write's place in the `Temporal`
    ///   chain at submission time. That is why the chain cannot be corrupted by
    ///   an out-of-order drain: the drain no longer decides the order. Scoped to
    ///   *sequential* calls from one agent — for two this agent has in flight at
    ///   once, this line and the queue's own enqueue are separate critical
    ///   sections and can disagree (J3-R1-10, and see `writeq`'s §Ordering).
    ///
    /// What moves off the call path is the embedder wait — 22 to 27 ms of a
    /// warm 27 ms `derive` — not the 0.4 ms round trip, which is not worth
    /// removing.
    ///
    /// D's optional **event time** rides the same seam as on
    /// [`Memory::record_action_async_as`]: the interaction is opened
    /// synchronously at submit (before the job is queued), so the parameter is
    /// stamped straight into that interaction via `begin_interaction_full` and
    /// every edge the queued derive later creates inherits it. `None` is a live
    /// fact (fallback rule: about-time = created-at).
    ///
    /// Returns the receipt. **A refused admission is not an `Err`**: the
    /// receipt carries [`crate::writeq::ReceiptAnswer::Dropped`] and the drop
    /// is counted in `lambo_stats`. An `Err` here means the call was rejected
    /// before a receipt existed — a closed or fenced session, or input the
    /// pre-pass refused.
    pub async fn derive_async_as(
        &self,
        agent: &AgentId,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
        event_time: Option<DateTime<Utc>>,
    ) -> Result<Submitted, LamboError> {
        let _writing = self.begin_write().await?;
        // The pre-pass, on the call path. The background path re-runs its own
        // planning validation; this one exists so the common errors do not have
        // to be collected from a receipt.
        //
        // **The pre-pass must be the one the strategy actually uses.** Hybrid's
        // and the synchronous path's are different sets of rules — hybrid omits
        // the repeated-`Observation` and single-`Hierarchical`-parent
        // rejections — so running the wrong one here would refuse writes the
        // background path would have accepted, and validation that disagrees
        // with the write is worse than none.
        hybrid::validate_limits(concepts, parent_of, self.config.semantic_match_threshold)?;
        {
            let g = self.graph.read();
            match self.config.match_strategy {
                MatchStrategy::Hybrid => {
                    hybrid::validate_graph_inputs(&g, parent_of)?;
                    // The embed budget too: an over-budget call is refused
                    // here, not after the ack as a timeout at apply.
                    hybrid::validate_embed_budget(
                        &g,
                        concepts,
                        parent_of,
                        self.vector_candidates().available(),
                    )?;
                }
                MatchStrategy::Canonical => {
                    crate::graph::derive::validate(&g, concepts, parent_of)?
                }
            }
        }
        let prompt = hybrid::derive_prompt(concepts.iter().map(|(content, _)| *content));
        let interaction = self.begin_interaction_full(agent, Some(prompt), event_time)?;
        Ok(self
            .pipeline
            .submit_derive(
                agent.clone(),
                interaction,
                concepts
                    .iter()
                    .map(|(c, t)| ((*c).to_string(), *t))
                    .collect(),
                parent_of
                    .pairs()
                    .iter()
                    .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
                    .collect(),
            )
            .await)
    }

    /// [`Memory::record_action_as`] acknowledged before the graph write (J3).
    ///
    /// `record_action` has no embedder hop of its own, so what asynchrony buys
    /// here is not latency but **ordering with `derive`**: both tools feed one
    /// per-agent lane, so an agent that records an action and then derives from
    /// it gets them applied in that order. Routing only `derive` through the
    /// queue would have let a later synchronous `record_action` overtake an
    /// earlier queued `derive` on the same agent's chain.
    ///
    /// The interaction — and with it the action's optional D `event_time` — is
    /// pinned at submit time exactly as in [`Memory::record_action_as`].
    ///
    /// See [`Memory::derive_async_as`] for what stays on the call path.
    pub async fn record_action_async_as(
        &self,
        agent: &AgentId,
        action: &Action<'_>,
    ) -> Result<Submitted, LamboError> {
        let _writing = self.begin_write().await?;
        {
            let g = self.graph.read();
            crate::graph::action::validate(&g, action)?;
        }
        let interaction =
            self.begin_interaction_full(agent, Some(action.action.to_string()), action.event_time)?;
        Ok(self
            .pipeline
            .submit_action(
                agent.clone(),
                interaction,
                action.action.to_string(),
                action.produces.iter().map(|s| (*s).to_string()).collect(),
                action.modifies.iter().map(|s| (*s).to_string()).collect(),
                action.depends_on.iter().map(|s| (*s).to_string()).collect(),
            )
            .await)
    }

    /// Context-overflow demotion (spec §7): one `Observation` concept per
    /// sentence of `chunk`, all sharing `chunk_group_id` for T5.2 sibling
    /// co-retrieval.
    ///
    /// The interaction opened here carries **no** `prompt_text`: the chunk is
    /// being demoted precisely because it overflowed the context window, and
    /// copying it onto the interaction node would put it straight back into
    /// recall's recent-interactions leg.
    ///
    /// An empty or whitespace-only chunk is a no-op — not even an interaction
    /// is opened.
    pub fn demote(&self, chunk: &str, chunk_group_id: &str) -> Result<Vec<NodeId>, LamboError> {
        let _writing = self.begin_write_sync()?;
        if chunk.trim().is_empty() {
            return Ok(Vec::new());
        }
        let interaction = self.begin_interaction(None)?;
        let created = {
            let mut g = self.graph.write();
            graph_demote(&mut g, interaction, &self.agent, chunk, chunk_group_id)?
        };
        // Observations are concepts: the mod.rs contract names `demote`
        // explicitly, and missing it is the classic silent-staleness bug.
        self.mirror_concepts(&created);
        self.daemon.wake();
        Ok(created)
    }

    /// Blast-radius report for `target`, optionally removing it (spec §6.1,
    /// §13) — **async** because the durable radius is a store query.
    ///
    /// `target` is resolved through the canonicalization pipeline (so a synonym
    /// or a differently-cased phrase finds the same concept), falling back to
    /// an exact `content` match — which is how a demoted `Observation`, skipped
    /// by canonicalization's match step, is reachable.
    ///
    /// [`DryRun::Yes`] mutates **nothing**. [`DryRun::No`] removes the node and
    /// every incident edge from the graph and drops it from the inverted index
    /// in the same critical section, so no reader can observe the node gone
    /// from one and present in the other.
    ///
    /// The report is **measured before the removal**, under a read lock that is
    /// released for the durable-radius store query: with a concurrent writer on
    /// another task, `blast_radius` / `incident_edges` describe the graph as of
    /// the measurement, not as of the removal (an edge added in between is
    /// destroyed but uncounted). Report accuracy only — the removal itself is
    /// atomic under one write lock.
    ///
    /// ## The durable-radius query is bounded (R2-5)
    ///
    /// That store call gets `RETRACT_IO_TIMEOUT`, and a timeout **fails the
    /// whole retraction** — nothing is removed, since the await precedes every
    /// mutation. Note the asymmetry with the arm above it, which is deliberate:
    /// a store *error* is an answer, and the commonest one ("no such session
    /// yet") is what a never-flushed session gives, so it degrades to a warning
    /// and an in-RAM-only count. A store that never answers is a different
    /// animal — the report's durable half cannot be honestly filled in, and
    /// `retract` holds the writers gate across this await, so an unbounded wait
    /// here is also an unbounded `close()` (its step 0 waits for exactly this
    /// permit).
    ///
    /// **This includes a dry run** (R3-3). [`DryRun::Yes`] mutates nothing, so
    /// nothing is at stake in *proceeding* — it could have degraded to the
    /// warning path like the error arm does. It does not, for three reasons.
    /// The asymmetry above is a judgement about the **store** ("an error is an
    /// answer, a hang is not"), and what this call was going to do next cannot
    /// change what the store said. A dry run is the *preview* an operator
    /// authorises the real retraction from, so quietly returning a report whose
    /// durable half is missing is least defensible exactly when the backend is
    /// wedged. And the two calls are meant to be read together: an operator who
    /// gets `Ok` from `DryRun::Yes` and, a second later, a timeout error from
    /// `DryRun::No` has been told two different things about one store. So a
    /// dry run against an unresponsive backend **errors**, having (as always)
    /// mutated nothing.
    pub async fn retract(&self, target: &str, dry_run: DryRun) -> Result<ImpactReport, LamboError> {
        // The gate spans the store call below, so a `close()` racing a live
        // retraction waits for it rather than draining past its removal —
        // which would acknowledge a retraction that resurrects on reattach
        // (T81-1). A DryRun::Yes retract takes the gate too: whether it will
        // mutate is known here, but the store call is the same, and holding a
        // shared read permit costs concurrent writers nothing.
        let _writing = self.begin_write().await?;

        // Resolve + measure under ONE read lock; released before the store call.
        let (node, content, canonization_status, blast_radius, incident_edges) = {
            let g = self.graph.read();
            let node = resolve_concept(&g, target)?;
            let concept = match g.node(node) {
                Some(Node::Concept(c)) => c,
                _ => {
                    return Err(LamboError::Store(StoreError::NotFound(format!(
                        "retract: {target:?} did not resolve to a concept"
                    ))))
                }
            };
            (
                node,
                concept.content.clone(),
                concept.canonization_status,
                format::blast_radius(&g, node),
                g.incident_edges(node).len(),
            )
        };

        // Durable radius — no lock held (spec §6.4), and bounded (R2-5).
        let mut warnings = Vec::new();
        let durable = tokio::time::timeout(
            RETRACT_IO_TIMEOUT,
            self.store
                .blast_radius(&self.session, node, Duration::ZERO, Utc::now()),
        )
        .await;
        let durable_blast_radius = match durable {
            Ok(Ok(count)) => Some(count),
            Ok(Err(err)) => {
                // Not fatal: the graph is the primary tier and already answered.
                // A never-flushed session legitimately lands here.
                warnings.push(format!(
                    "durable blast radius unavailable ({err}); reporting the in-RAM count only"
                ));
                None
            }
            Err(_elapsed) => {
                // Fatal, unlike the error arm above — see the rustdoc: an error
                // is an answer ("no such session yet"), a hang is not, and this
                // one holds the writers gate open behind it. Fatal for a DRY
                // RUN too (R3-3): a dry run is the preview the real retraction
                // is authorised from, so it must not be the one call that
                // quietly reports less about a wedged store.
                //
                // Nothing has been mutated at this point: every graph write is
                // below, so the retraction is refused whole rather than left
                // half-done.
                return Err(LamboError::Store(StoreError::Backend(format!(
                    "retract: durable blast-radius query timed out after {RETRACT_IO_TIMEOUT:?}; \
                     nothing was removed"
                ))));
            }
        };

        let removed = if dry_run.is_dry() {
            false
        } else {
            // graph -> index, the daemon GC's order. Both guards die here.
            let mut g = self.graph.write();
            g.remove_node(node)?;
            self.index.write().remove(node);
            drop(g);
            self.daemon.wake();
            true
        };

        Ok(ImpactReport {
            target: node,
            content,
            canonization_status,
            blast_radius,
            durable_blast_radius,
            incident_edges,
            dry_run: dry_run.is_dry(),
            removed,
            warnings,
        })
    }

    /// Acquire or extend a soft lock on `node` for this handle's agent
    /// (spec §11). Cross-agent contention returns [`LamboError::SoftLock`].
    ///
    /// # Not durable (pinned upstream contract S5)
    ///
    /// Reservations live in RAM only: like synonyms they have no `Mutation`
    /// kind, so no flush — [`Memory::close`]'s final one included — persists
    /// them and no reattach restores them. A restart releases every soft lock
    /// in the session; a caller that reattaches must re-`reserve` anything it
    /// still holds, and must not read "no reservation" after a restart as
    /// "nobody else was working on this".
    pub fn reserve(&self, node: NodeId, ttl: Duration) -> Result<Reservation, LamboError> {
        self.reserve_as(&self.agent, node, ttl)
    }

    /// [`Memory::reserve`] on behalf of `agent` (J1) — the call that makes soft
    /// locks work for more than one client of one process.
    ///
    /// Contention is now genuine: two distinct ids reserving one node produce a
    /// [`LamboError::SoftLock`] for the second, and [`Memory::release_as`]
    /// refuses an id that does not hold the lock. Two callers passing the *same*
    /// id share one lock and can release each other's — cooperative by design,
    /// and the MCP layer says so in the tool description. Nothing here
    /// authenticates `agent`; a soft lock never did.
    pub fn reserve_as(
        &self,
        agent: &AgentId,
        node: NodeId,
        ttl: Duration,
    ) -> Result<Reservation, LamboError> {
        let _writing = self.begin_write_sync()?;
        let mut g = self.graph.write();
        graph_reserve(&mut g, node, agent, ttl, Utc::now())
    }

    /// Release this agent's soft lock on `node` — the pair of
    /// [`Memory::reserve`]. A non-owner gets [`LamboError::SoftLock`].
    pub fn release(&self, node: NodeId) -> Result<(), LamboError> {
        self.release_as(&self.agent, node)
    }

    /// [`Memory::release`] on behalf of `agent` (J1). A caller that does not
    /// hold the lock under this id gets [`LamboError::SoftLock`] and the lock
    /// stands — which is what stops one client dropping another's lock.
    ///
    /// The gate comes first, so a fenced handle fails here with a
    /// [`LamboError::Conflict`] — a *different* variant, deliberately, because
    /// its message is operator-only and `mcp::server` must be able to tell the
    /// two apart without reading either (J1-R2-2).
    pub fn release_as(&self, agent: &AgentId, node: NodeId) -> Result<(), LamboError> {
        let _writing = self.begin_write_sync()?;
        let mut g = self.graph.write();
        graph_release(&mut g, node, agent)
    }

    /// Open a fresh interaction at the tail of the temporal chain.
    ///
    /// `created_at` is stamped **here**, from the process clock — never from a
    /// caller (P6 review F18: every concept and edge below this interaction
    /// inherits the timestamp, and backdating by 61s would neuter the
    /// `canonization_edge_min_age` inflation guard). `self.clock` *is* that
    /// process clock: [`Utc::now`] everywhere except `lambo demo`, which pins
    /// it at construction (see [`MemoryBuilder::clock`](super::MemoryBuilder::clock)).
    ///
    /// Reading the chain tail and inserting happen under one write lock, so two
    /// concurrent writers cannot both claim the same predecessor.
    pub(super) fn begin_interaction(&self, prompt: Option<String>) -> Result<NodeId, LamboError> {
        self.begin_interaction_full(&self.agent, prompt, None)
    }

    /// The one interaction-opening seam, with D's optional **event time**.
    ///
    /// F18's rule guards flush time only: `created_at` remains process-stamped
    /// no matter what arrives here. `event_time` is a different concept — the
    /// instant the fact is *about* (a commit date, transcript timestamp), not
    /// an observation claim about the present — and it is stored verbatim as
    /// [`Interaction::event_time`] (`None` = live fact, fallback rule). Every
    /// edge the write creates inherits this interaction's about-time at
    /// creation, so one parameter stamps the whole turn.
    pub(super) fn begin_interaction_full(
        &self,
        agent: &AgentId,
        prompt: Option<String>,
        event_time: Option<DateTime<Utc>>,
    ) -> Result<NodeId, LamboError> {
        let id = NodeId::new();
        let created_at = (self.clock)();
        let mut g = self.graph.write();
        let previous_id = g.temporal_chain().last().copied();
        g.insert_interaction(Interaction {
            id,
            session_id: self.session.clone(),
            agent_id: agent.clone(),
            prompt_text: prompt,
            previous_id,
            created_at,
            event_time,
        })?;
        Ok(id)
    }

    /// Mirror concept writes into the inverted index (the `src/graph/mod.rs`
    /// contract).
    ///
    /// The body lives in [`crate::writeq::mirror_concepts`] — where the lock
    /// order that makes it safe is documented — because J3's background workers
    /// hold `Arc` clones of the graph and index rather than a `Memory`, and two
    /// copies of a lock-order rule is two chances to get it wrong.
    pub(super) fn mirror_concepts(&self, ids: &[NodeId]) {
        crate::writeq::mirror_concepts(&self.graph, &self.index, ids);
    }
}
