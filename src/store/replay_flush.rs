//! Recovery for a flush that rejected a batch containing startup replay.
//! The ordinary flush still submits one batch. Only a deterministic rejection
//! triggers isolation, and every replay's effects and consume stay together.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::StoreError;
use crate::types::{GcMark, Mutation, MutationBatch, WriteIntentOutcome};

#[derive(Clone, Copy)]
pub(crate) struct Checkpoint {
    len: usize,
    epoch: u64,
    gc: GcMark,
}

impl Checkpoint {
    pub(crate) fn capture(graph: &Graph) -> Self {
        Self {
            len: graph.log_len(),
            epoch: graph.epoch(),
            gc: graph.gc_mark(),
        }
    }
}

// The replay task registers the receipt answer after the graph commit, while
// the flusher can confirm durability as soon as that commit releases the lock.
type Callback = Box<dyn FnOnce(Option<WriteIntentOutcome>) + Send + Sync>;

#[derive(Default)]
pub(crate) struct Completion(Mutex<CompletionState>);

#[derive(Default)]
enum CompletionState {
    #[default]
    Empty,
    Callback(Callback),
    Finished(Option<WriteIntentOutcome>),
}

impl Completion {
    pub(crate) fn on_finish(&self, callback: Callback) {
        let mut state = self.0.lock();
        if let CompletionState::Finished(outcome) = std::mem::take(&mut *state) {
            drop(state);
            callback(outcome);
        } else {
            *state = CompletionState::Callback(callback);
        }
    }

    fn finish(&self, outcome: Option<WriteIntentOutcome>) {
        let mut state = self.0.lock();
        if let CompletionState::Callback(callback) = std::mem::take(&mut *state) {
            drop(state);
            callback(outcome);
        } else {
            *state = CompletionState::Finished(outcome);
        }
    }
}

#[derive(Clone)]
struct Group {
    start: Checkpoint,
    end: Checkpoint,
    len: usize,
    completion: Arc<Completion>,
    // The effects were rejected. Only the failure consume is left to flush.
    failure: bool,
}

#[derive(Clone, Default)]
pub(crate) struct ReplayFlush {
    // Keyed by the consume at the end of each span. Span lengths remain
    // meaningful across drains, concatenation, and close-time requeue.
    groups: HashMap<String, Group>,
    deferred: bool,
}

impl std::fmt::Debug for ReplayFlush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayFlush")
            .field("groups", &self.groups.len())
            .field("deferred", &self.deferred)
            .finish()
    }
}

impl ReplayFlush {
    pub(crate) fn has_pending_groups(&self) -> bool {
        !self.groups.is_empty()
    }

    pub(crate) fn deferred(&self) -> bool {
        self.deferred
    }

    fn first<'a>(
        &self,
        batch: &'a MutationBatch,
        include_failure: bool,
    ) -> Option<(usize, &'a str, Group)> {
        if self.groups.is_empty() {
            return None;
        }
        batch
            .mutations
            .iter()
            .enumerate()
            .find_map(|(i, mutation)| {
                let Mutation::ConsumeWriteIntent { receipt, .. } = mutation else {
                    return None;
                };
                let group = self.groups.get(receipt)?;
                if !include_failure && group.failure {
                    return None;
                }
                Some((i + 1 - group.len, receipt.as_str(), group.clone()))
            })
    }
}

impl Graph {
    pub(crate) fn ensure_replay_writable(&self) -> Result<(), crate::types::LamboError> {
        if self.replay_flush.deferred {
            return Err(crate::types::LamboError::Config(
                "replay was rejected; restart required before writing".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn consume_replayed_write(
        &mut self,
        receipt: String,
        outcome: WriteIntentOutcome,
        start: Checkpoint,
        completion: Arc<Completion>,
    ) {
        let end = Checkpoint::capture(self);
        self.replay_flush.groups.insert(
            receipt.clone(),
            Group {
                start,
                end,
                len: end.len - start.len + 1,
                completion,
                failure: false,
            },
        );
        self.consume_write_intent(receipt, outcome);
    }
}

fn finish(
    graph: &mut Graph,
    mutations: &[Mutation],
    durable: bool,
) -> Vec<(Arc<Completion>, Option<WriteIntentOutcome>)> {
    mutations
        .iter()
        .filter_map(|mutation| {
            let Mutation::ConsumeWriteIntent {
                receipt, outcome, ..
            } = mutation
            else {
                return None;
            };
            let group = graph.replay_flush.groups.remove(receipt)?;
            Some((group.completion, durable.then(|| outcome.clone())))
        })
        .collect()
}

fn notify(completions: Vec<(Arc<Completion>, Option<WriteIntentOutcome>)>) {
    for (completion, outcome) in completions {
        completion.finish(outcome);
    }
}

pub(crate) fn committed(graph: &RwLock<Graph>, batch: &MutationBatch) {
    let completions = {
        let mut graph = graph.write();
        graph.mark_durable_through(batch.mutation_epoch);
        finish(&mut graph, &batch.mutations, true)
    };
    notify(completions);
}

pub(crate) fn discarded(graph: &RwLock<Graph>, mutations: &[Mutation], dead_letter: bool) {
    let completions = {
        let mut graph = graph.write();
        graph.pin_unflushed(mutations);
        // A replay group lost to fencing or degradation leaves speculative
        // effects in RAM. Latch before removing the group, otherwise an
        // observer could mistake an empty group set for a durable replay.
        if mutations.iter().any(|mutation| {
            matches!(mutation,
            Mutation::ConsumeWriteIntent { receipt, .. }
                if graph.replay_flush.groups.contains_key(receipt))
        }) {
            graph.replay_flush.deferred = true;
        }
        if dead_letter {
            graph.note_dead_lettered_intents(mutations);
        }
        finish(&mut graph, mutations, false)
    };
    notify(completions);
}

/// A replay already inside its commit can append while recovery is running.
/// Once we know part of the replay graph was rejected, discard that
/// speculative suffix too. The durable intents are still owed on restart.
pub(crate) fn defer_suffix(graph: &RwLock<Graph>, batch: &mut MutationBatch) {
    let deferred = graph.read().replay_flush.deferred;
    if !deferred {
        return;
    }
    // Only the explicitly registered failure consume is independent of the
    // poisoned graph. All other effects, including in-flight ordinary writes,
    // must remain unflushed until a fresh attach reconstructs state.
    let mut keep = Vec::new();
    let mut drop = Vec::new();
    let mut through = None;
    {
        let graph = graph.read();
        for mutation in batch.mutations.drain(..) {
            let safe = matches!(&mutation, Mutation::ConsumeWriteIntent { receipt, .. }
                if graph.replay_flush.groups.get(receipt).is_some_and(|g| g.failure));
            if safe {
                if let Mutation::ConsumeWriteIntent { receipt, .. } = &mutation {
                    through = graph.replay_flush.groups.get(receipt).map(|g| g.end);
                }
                keep.push(mutation);
            } else {
                drop.push(mutation);
            }
        }
    }
    discarded(graph, &drop, true);
    batch.mutations = keep;
    if let Some(through) = through {
        batch.mutation_epoch = through.epoch;
        batch.gc_mark = through.gc;
    }
}

pub(crate) fn has_replay(graph: &RwLock<Graph>, batch: &MutationBatch) -> bool {
    graph.read().replay_flush.first(batch, true).is_some()
}

/// Retry by chronological spans. After each confirmed commit, remove that
/// prefix from custody before another await. Once a replay span is proven bad,
/// pin its effects and leave the suffix owed: even a later write whose values
/// the store would accept may have matched or reinforced the rejected state.
pub(crate) async fn recover(
    graph: &RwLock<Graph>,
    batch: &mut MutationBatch,
    store: &dyn GraphStore,
    token: Option<u64>,
    fence: Option<&AtomicBool>,
    mut dead_lettered: impl FnMut(),
) -> Result<(), StoreError> {
    while !batch.is_empty() {
        let first = graph.read().replay_flush.first(batch, true);
        let (len, through) = match &first {
            Some((0, _, group)) => (group.len, group.end),
            Some((start, _, group)) => (*start, group.start),
            None => (
                batch.len(),
                Checkpoint {
                    len: 0,
                    epoch: batch.mutation_epoch,
                    gc: batch.gc_mark,
                },
            ),
        };
        let last = len == batch.len();
        let chunk = MutationBatch {
            mutations: batch.mutations[..len].to_vec(),
            mutation_epoch: if last {
                batch.mutation_epoch
            } else {
                through.epoch
            },
            gc_mark: if last { batch.gc_mark } else { through.gc },
        };
        if fence.is_some_and(|f| f.load(Ordering::Acquire)) {
            return Err(StoreError::StaleWrite(
                "lease lost during replay flush".into(),
            ));
        }
        match super::flush::flush_attempt(store, &chunk, token).await {
            Ok(()) => {
                committed(graph, &chunk);
                batch.mutations.drain(..len);
            }
            Err(StoreError::Constraint(code))
                if first.as_ref().is_some_and(|(start, _, group)| {
                    *start == 0 && !group.failure && group.len > 1
                }) =>
            {
                let (_, receipt, group) = first.expect("the rejected span is a replay");
                let receipt = receipt.to_owned();
                let Mutation::ConsumeWriteIntent {
                    session_id,
                    outcome,
                    ..
                } = chunk
                    .mutations
                    .last()
                    .expect("a replay span ends with a consume")
                else {
                    unreachable!("a replay span ends with a consume");
                };
                let summary = crate::writeq::replay_flush_failure(&outcome.summary, &code);
                // Install the failed consume in custody before trying to flush
                // it. A timeout or cancelled close must never restore the
                // already-rejected effects alongside that consume.
                {
                    let mut graph = graph.write();
                    graph.replay_flush.groups.remove(&receipt);
                    graph.replay_flush.deferred = true;
                }
                discarded(graph, &batch.mutations, true);
                graph.write().replay_flush.groups.insert(
                    receipt.clone(),
                    Group {
                        len: 1,
                        end: group.start,
                        failure: true,
                        ..group.clone()
                    },
                );
                batch.mutations.clear();
                batch.mutations.push(Mutation::ConsumeWriteIntent {
                    session_id: session_id.clone(),
                    receipt: receipt.clone(),
                    outcome: WriteIntentOutcome {
                        tag: "failed".into(),
                        summary,
                        consumed_at: outcome.consumed_at,
                    },
                });
                batch.mutation_epoch = group.start.epoch;
                batch.gc_mark = group.start.gc;
                dead_lettered();
                tracing::warn!(session = %session_id, receipt, constraint = %code,
                    "FlushDeadLettered: the store rejected an isolated replay write; later replay                      intents stay owed for the next serve");
            }
            Err(StoreError::Constraint(code)) => {
                // An ordinary prefix or bare replay consume was rejected.
                // Its effects may underpin every remaining replay; none may
                // acquire a terminal receipt based on this RAM state.
                graph.write().replay_flush.deferred = true;
                discarded(graph, &batch.mutations, true);
                batch.mutations.clear();
                dead_lettered();
                tracing::warn!(constraint = %code, "replay isolation rejected a prefix; remaining intents stay owed");
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}
