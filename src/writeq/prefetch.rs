//! Lane prefetch (#125): a lane worker embeds the texts of the job it is
//! about to run **and of the jobs queued behind it** in one embedder round.
//!
//! # Why rounds, not width
//!
//! Each lane has one consumer, and a job embeds what it needs before it
//! commits. Before this, a job embedded its texts one after another, so a
//! lane retired one job per two or more embedder rounds. Under load every
//! round waits for a whole forward on the embedder, and an agent writing
//! faster than its lane's rounds built a backlog (apply p90 12.3 s at 16
//! agents, #125). Measured on candle/Metal, a wider forward buys at most
//! about 1.5 times the throughput, so what a lane needs is fewer rounds per
//! job: one round now covers this job and up to [`LANE_PREFETCH_MAX_JOBS`]
//! queued after it.
//!
//! # What it keeps
//!
//! * **Per-lane order and receipts.** The round only embeds. Jobs still
//!   plan, validate (#116's `plan_still_holds`) and commit one at a time in
//!   FIFO order, each through its own `WriteCtx::run`, and each settles its
//!   own receipt.
//! * **Failure isolation.** The round is best effort. A text whose slot
//!   failed, or a round that failed or timed out as a whole, leaves those
//!   texts out of the seed; the job then embeds them alone and meets its own
//!   error, with its own class (`Embed` or `EmbedUnavailable`), on its own
//!   receipt. Nothing in a round can fail another job.
//! * **The I/O bound.** The round runs inside the running job's
//!   [`HYBRID_IO_TIMEOUT`] budget (`Gather::io_started`), so a job's round
//!   plus its own embeds never exceed one budget, and `RECEIPT_WAIT_MAX`'s
//!   reasoning is unchanged.
//! * **Fairness.** A lane has at most one round in flight, of at most
//!   [`LANE_PREFETCH_MAX_TEXTS`] texts, so one busy agent puts at most that
//!   many texts ahead of another agent's at the embedder. The lane bound
//!   ([`super::WRITE_QUEUE_LANE_MAX`]) is unchanged.
//! * **Low load.** No debounce or wait is added: a lane with one job embeds
//!   that job's texts in one round, never more slowly than one at a time.
//!
//! Across lanes, the embedder batches: the candle adapter's coalescer takes
//! every lane's round that is queued when it runs a forward. Nothing here
//! joins lanes, so no lane waits for another.
//!
//! Only an embedder that batches ([`crate::embed::Embedder::batch_capacity`]
//! above 1) is prefetched for; otherwise the lane runs exactly as before.

use std::sync::Arc;

use parking_lot::Mutex as PlMutex;

use super::admission::{Job, JobPayload, Lanes};
use super::execution::WriteCtx;
use crate::graph::action::{action_embed_texts, Action};
use crate::graph::derive::ParentOf;
use crate::graph::hybrid::{self, EmbedSeed, HYBRID_IO_TIMEOUT};
use crate::graph::Graph;
use crate::types::{AgentId, ConceptType, MatchStrategy, NodeId};

/// Most texts one lane round embeds (#125). Past four texts a candle forward
/// on Metal buys no throughput (`embed::candle`'s `FORWARD_WIDTH` note), so
/// this is not a throughput knob: it bounds how far one agent's round can
/// put its texts ahead of another agent's, and the memory a lane's seed
/// holds (16 vectors of 1,024 floats is 64 KiB). Sixteen covers eight
/// two-concept derives.
pub const LANE_PREFETCH_MAX_TEXTS: usize = 16;

/// Most jobs one lane round embeds for, the running one included (#125).
pub const LANE_PREFETCH_MAX_JOBS: usize = 8;

/// One lane worker's seed: the vectors its last round embedded.
#[derive(Default)]
pub(super) struct LanePrefetch {
    seed: EmbedSeed,
}

impl LanePrefetch {
    pub(super) fn seed(&self) -> &EmbedSeed {
        &self.seed
    }

    /// Run a round for `job` and the jobs queued behind it in `agent`'s lane,
    /// unless the seed already covers `job`. Returns when the round started
    /// (the running job's I/O budget start), or `None` when none ran.
    pub(super) async fn ahead(
        &mut self,
        ctx: &WriteCtx,
        lanes: &Arc<PlMutex<Lanes>>,
        agent: &AgentId,
        job: &Job,
    ) -> Option<tokio::time::Instant> {
        let capacity = LANE_PREFETCH_MAX_TEXTS.min(ctx.embedder.batch_capacity());
        if capacity < 2 || ctx.match_strategy != MatchStrategy::Hybrid {
            return None;
        }
        // Peek, never pop: the queued jobs stay where admission put them.
        let queued: Vec<(NodeId, Arc<JobPayload>)> = {
            let l = lanes.lock();
            l.queues
                .get(agent)
                .map(|q| {
                    q.iter()
                        .take(LANE_PREFETCH_MAX_JOBS - 1)
                        .map(|j| (j.interaction, j.payload.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let texts = {
            let g = ctx.graph.read();
            let own = predicted_texts(ctx, &g, job.interaction, &job.payload);
            if own.is_empty()
                || own.len() > capacity
                || own.iter().all(|text| self.seed.contains(text))
            {
                // Covered by the last round, nothing to embed, or more than a
                // round holds (the job gathers its own then).
                return None;
            }
            let mut texts: Vec<String> = Vec::with_capacity(capacity);
            for text in own {
                if !texts.contains(&text) {
                    texts.push(text);
                }
            }
            for (interaction, payload) in &queued {
                let mut next: Vec<String> = predicted_texts(ctx, &g, *interaction, payload);
                next.retain(|text| !texts.contains(text));
                next.dedup();
                if texts.len() + next.len() > capacity {
                    break;
                }
                texts.extend(next);
            }
            texts
        };
        if texts.len() < 2 {
            return None;
        }
        self.seed.clear();
        let io_started = tokio::time::Instant::now();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        self.seed.extend(
            hybrid::embed_gathered(ctx.embedder.as_ref(), &refs, io_started + HYBRID_IO_TIMEOUT)
                .await,
        );
        Some(io_started)
    }
}

/// The texts `payload` would embed if it ran against `g` now (a prediction;
/// see [`hybrid::planned_embed_texts`]). An image derive is not predicted.
fn predicted_texts(
    ctx: &WriteCtx,
    g: &Graph,
    interaction: NodeId,
    payload: &JobPayload,
) -> Vec<String> {
    match payload {
        JobPayload::Derive { concepts, pairs } => {
            if !ctx.derive_vector_candidates().available() {
                return Vec::new();
            }
            let borrowed: Vec<(&str, ConceptType)> =
                concepts.iter().map(|(c, t)| (c.as_str(), *t)).collect();
            let borrowed_pairs: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
                .collect();
            let parent_of = if borrowed_pairs.is_empty() {
                ParentOf::none()
            } else {
                ParentOf::from_pairs(&borrowed_pairs)
            };
            hybrid::planned_embed_texts(
                g,
                interaction,
                &borrowed,
                &parent_of,
                ctx.semantic_match_threshold,
            )
        }
        JobPayload::DeriveImage { .. } => Vec::new(),
        JobPayload::Action {
            action,
            produces,
            modifies,
            depends_on,
        } => {
            if !ctx.vector_candidates().available() {
                return Vec::new();
            }
            let p: Vec<&str> = produces.iter().map(String::as_str).collect();
            let m: Vec<&str> = modifies.iter().map(String::as_str).collect();
            let d: Vec<&str> = depends_on.iter().map(String::as_str).collect();
            action_embed_texts(&Action {
                event_time: None,
                action: action.as_str(),
                produces: &p,
                modifies: &m,
                depends_on: &d,
            })
            .into_iter()
            .map(str::to_string)
            .collect()
        }
    }
}
