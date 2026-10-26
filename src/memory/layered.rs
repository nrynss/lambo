//! Layered recall on the `Memory` surface (#33 PR 2): one call reads the
//! caller's session and its layers (shared sessions such as `general`),
//! merged by `crate::recall::layered::merge`.
//!
//! The query is embedded once, by the caller's session, and the vector is
//! handed to every layer (their embedding contracts must equal the
//! caller's; a layer whose contract differs is unavailable). All sessions
//! recall concurrently, each on its own graph, index, scores and hot list,
//! and no session's lock is held while another's is taken. Each session
//! notes accesses (#30) only for its own hits that the merge returned.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use super::Memory;
use crate::recall::format;
use crate::recall::layered::{
    self, HitIdentity, LayeredRecall, MergeParams, TierList, TierRecall,
    DEFAULT_LAYER_MIN_SIMILARITY, DEFAULT_LAYER_WEIGHT, PRIMARY_WEIGHT,
};
use crate::types::{EmbeddingContract, LamboError, Node, RecallHit, RecallQuery, SessionId};

/// One layer of a layered recall: a session and how its hits rank against
/// the caller's. A slice of these is the layer stack, nearest tier first;
/// the order is the dedupe precedence.
#[non_exhaustive]
#[derive(Clone, Copy)]
pub struct LayerRef<'a> {
    /// The layer's session. It must not be the caller's own, and its
    /// embedding contract must equal the caller's (else it is unavailable).
    pub memory: &'a Memory,
    /// `w` in the merge's `w / rank` key; the caller's own session has 1.0.
    /// Finite and positive (0 is refused: it would rank every hit of the
    /// layer last yet still fill free slots). Defaults to [`DEFAULT_LAYER_WEIGHT`]
    /// (measured on BGE-M3 in #157).
    pub weight: f64,
    /// The layer's absolute floor: a hit outside a
    /// traversal answer anchored in this layer merges only at vector
    /// cosine `>=` this; no warning lifts it. In `[0, 1]`. Defaults to
    /// [`DEFAULT_LAYER_MIN_SIMILARITY`] (measured on BGE-M3 in #157).
    ///
    /// Left at that default (by [`LayerRef::new`], unset), the recall uses
    /// the calling session's contract's calibrated floor instead when it has
    /// one (#87; EmbeddingGemma 2 at 768 dims: 0.72), unless the caller's
    /// `Config::calibrated_layer_floor` is off or its `calibration` is `off`.
    /// A floor set with [`LayerRef::with_min_similarity`] always applies,
    /// 0.50 included; one assigned to this field directly applies unless it
    /// equals the default.
    pub min_similarity: f64,
    /// Whether [`LayerRef::with_min_similarity`] set the floor.
    min_similarity_set: bool,
}

impl<'a> LayerRef<'a> {
    /// `memory` as a layer with the default weight and floor.
    pub fn new(memory: &'a Memory) -> Self {
        Self {
            memory,
            weight: DEFAULT_LAYER_WEIGHT,
            min_similarity: DEFAULT_LAYER_MIN_SIMILARITY,
            min_similarity_set: false,
        }
    }

    /// This layer with merge weight `weight`.
    pub fn with_weight(mut self, weight: f64) -> Self {
        self.weight = weight;
        self
    }

    /// This layer with floor `min_similarity`, which then applies under
    /// every contract (no calibrated floor replaces it).
    pub fn with_min_similarity(mut self, min_similarity: f64) -> Self {
        self.min_similarity = min_similarity;
        self.min_similarity_set = true;
        self
    }

    /// The floor the user set, if any: `None` when it was left at
    /// [`DEFAULT_LAYER_MIN_SIMILARITY`] without
    /// [`LayerRef::with_min_similarity`].
    fn configured_min_similarity(&self) -> Option<f64> {
        #[allow(clippy::float_cmp)] // the exact default constant, not a computed value
        let unset = !self.min_similarity_set && self.min_similarity == DEFAULT_LAYER_MIN_SIMILARITY;
        (!unset).then_some(self.min_similarity)
    }
}

/// One tier of a stack handed to `Memory::recall_layered_slots`: a layer
/// to recall from, or one that is configured but cannot be reached now.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LayerSlot<'a> {
    /// Recall from this layer.
    Live(LayerRef<'a>),
    /// Configured, but not attached in this process right now (held by
    /// another writer, detaching, failed): the answer says it is
    /// unavailable.
    Unavailable {
        session: &'a SessionId,
        weight: f64,
        /// The floor the user set; `None` takes the calibrated default.
        min_similarity: Option<f64>,
    },
}

/// What `check_layers` reads of a slot.
struct SlotKnobs<'a> {
    session: &'a SessionId,
    weight: f64,
    /// The floor the user set, if any (the calibrated one is in range).
    min_similarity: Option<f64>,
}

impl LayerSlot<'_> {
    fn knobs(&self) -> SlotKnobs<'_> {
        match self {
            LayerSlot::Live(l) => SlotKnobs {
                session: &l.memory.session,
                weight: l.weight,
                min_similarity: l.configured_min_similarity(),
            },
            LayerSlot::Unavailable {
                session,
                weight,
                min_similarity,
            } => SlotKnobs {
                session,
                weight: *weight,
                min_similarity: *min_similarity,
            },
        }
    }
}

impl std::fmt::Debug for LayerRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayerRef")
            .field("session", self.memory.session())
            .field("weight", &self.weight)
            .field("min_similarity", &self.min_similarity)
            .field("min_similarity_set", &self.min_similarity_set)
            .finish()
    }
}

type LayerFuture<'a> = Pin<Box<dyn Future<Output = Result<TierRecall, String>> + Send + 'a>>;

/// How many hits' identities are copied per graph read: the copy clones
/// holder vectors, so a large `top_k` takes the lock in short sections.
const IDENTITY_CHUNK: usize = 64;

impl Memory {
    /// Recall from this session and its `layers` (nearest tier first),
    /// merged into one answer under one `max_tokens` budget (#33).
    ///
    /// With no layers this is exactly [`Memory::recall`]: same hits, same
    /// context and warnings, every hit in this session.
    ///
    /// With layers, each session's list is merged by weighted rank
    /// interleave (`weight / rank`, this session at 1.0). A layer hit
    /// merges only with vector cosine at or above the
    /// layer's floor (an unset floor is this session's contract's
    /// calibrated one, #87: see [`LayerRef::min_similarity`]), or as a
    /// dependent in the layer's traversal answer to a dependency question
    /// whose anchor is in that layer. Hits with a
    /// Conflict, Hot or Reservation warning are kept past `top_k` (never
    /// past a floor). The same memory in two tiers (equal canonical key, or
    /// holder vectors at cosine at or above this session's
    /// `semantic_match_threshold`) keeps the nearer tier's copy. A layer
    /// hit's block names its session, and a truncated layer block's marker
    /// names the scope to inspect it with. See [`crate::recall::layered`]
    /// for the full rule.
    ///
    /// A layer is left out, with a warning, when it is closed, when its
    /// embedding contract is not this session's, or when its recall
    /// panics; it never fails the call. A layer whose vector read fails is
    /// not left out: it answers from its other legs, and its warning says
    /// so with the layer's `[<session>] ` prefix. No edge is followed
    /// across sessions.
    ///
    /// Refused with [`LamboError::Config`] when a layer is this session, a
    /// session appears twice, a weight is not finite and positive, or a
    /// floor is outside `[0, 1]`; with whatever [`Memory::recall`] refuses
    /// for this session itself.
    pub async fn recall_layered(
        &self,
        layers: &[LayerRef<'_>],
        query: RecallQuery,
    ) -> Result<LayeredRecall, LamboError> {
        let slots: Vec<LayerSlot<'_>> = layers.iter().copied().map(LayerSlot::Live).collect();
        self.recall_layered_slots(&slots, query).await
    }

    /// [`Memory::recall_layered`] over a stack in which a layer may be
    /// known but not reachable (#33 PR 3): a serve's configured layer that
    /// is held by another writer, detaching or failed has no `Memory` to
    /// hand over, and is answered exactly as a closed layer is (one
    /// `layer_unavailable` annotation and warning, its hits missing).
    pub(crate) async fn recall_layered_slots(
        &self,
        layers: &[LayerSlot<'_>],
        query: RecallQuery,
    ) -> Result<LayeredRecall, LamboError> {
        self.check_layers(layers)?;
        // An unlayered call, or one this session's pipeline refuses before
        // running (a `top_k` out of range answers with a warning only), is
        // the plain recall, unchanged.
        if layers.is_empty() || crate::store::validate_vector_candidate_limit(query.top_k).is_err()
        {
            return self
                .recall_detailed(query)
                .await
                .map(|d| LayeredRecall::single(&self.session, d));
        }
        self.ensure_open()?;
        let live: Vec<&LayerRef<'_>> = layers
            .iter()
            .filter_map(|slot| match slot {
                LayerSlot::Live(layer) => Some(layer),
                LayerSlot::Unavailable { .. } => None,
            })
            .collect();

        // Embed once. This session's vector source decides, unless it
        // cannot search and a compatible layer's can: then the embed still
        // runs once, here, for the layers only. This session's own side then
        // gets no vector (as its plain recall would, so its recall cache
        // still serves), and an embed failure is reported by the layers
        // that lost their vector leg, not by this session.
        let own = self.vector_candidates();
        let own_searches = own.available();
        let source = if own_searches {
            own
        } else {
            live.iter()
                .filter(|l| l.memory.embedding == self.embedding)
                .map(|l| l.memory.vector_candidates())
                .find(|v| v.available())
                .unwrap_or(own)
        };
        let (embedding, warning) = self.embed_recall_query(&query.query, source).await;
        let vector = embedding.as_deref();
        let (own_vector, own_warning, layer_warning) = if own_searches {
            (vector, warning, None)
        } else {
            (None, None, warning)
        };

        let primary = async {
            let recall = self
                .recall_text_with(query.clone(), own_vector, own_warning)
                .await;
            let identities = self.hit_identities(&recall.hits);
            TierRecall { recall, identities }
        };
        let layer_futures: Vec<LayerFuture<'_>> = live
            .iter()
            .map(|l| {
                Box::pin(l.memory.recall_as_layer(
                    query.clone(),
                    vector,
                    &self.embedding,
                    layer_warning.as_deref(),
                )) as LayerFuture<'_>
            })
            .collect();
        let (primary, layer_results) = tokio::join!(primary, join_all(layer_futures));

        let mut lists = Vec::with_capacity(layers.len() + 1);
        lists.push(TierList {
            session: self.session.clone(),
            weight: PRIMARY_WEIGHT,
            floor: None,
            recall: Some(primary),
        });
        let mut results = layer_results.into_iter();
        for slot in layers {
            // #87: an unset floor is resolved here, under this session's
            // contract (every live layer shares it) and switches.
            let floor = crate::embed::calibration::resolve_layer_floor(
                slot.knobs().min_similarity,
                &self.config,
                &self.embedding,
            );
            let (session, weight, result) = match slot {
                LayerSlot::Live(layer) => (
                    &layer.memory.session,
                    layer.weight,
                    results
                        .next()
                        .unwrap_or_else(|| Err("no result".to_string()))
                        .and_then(|r| r),
                ),
                LayerSlot::Unavailable {
                    session, weight, ..
                } => (
                    *session,
                    *weight,
                    Err("not attached in this process".to_string()),
                ),
            };
            let recall = match result {
                Ok(t) => Some(t),
                Err(reason) => {
                    tracing::warn!(
                        target: "lambo::recall",
                        layer = %session,
                        "layered recall: layer unavailable: {reason}"
                    );
                    None
                }
            };
            lists.push(TierList {
                session: session.clone(),
                weight,
                floor: Some(floor),
                recall,
            });
        }

        let merged = layered::merge(
            &lists,
            MergeParams {
                top_k: query.top_k,
                max_tokens: query.max_tokens,
                dedupe_threshold: self.config.semantic_match_threshold,
                vector_pairs: layered::MAX_VECTOR_DEDUPE_PAIRS,
            },
            format::default_token_count,
        );

        // #30: each session counts only its own hits the merge returned.
        let own_hits = |memory: &Memory| {
            merged
                .hits
                .iter()
                .filter(|h| h.session == memory.session)
                .map(|h| h.hit.node_id)
                .collect::<Vec<_>>()
        };
        self.note_accesses(own_hits(self));
        for layer in &live {
            layer.memory.note_accesses(own_hits(layer.memory));
        }
        Ok(merged)
    }

    /// The `Config` refusals of [`Memory::recall_layered`].
    fn check_layers(&self, layers: &[LayerSlot<'_>]) -> Result<(), LamboError> {
        let mut seen = vec![&self.session];
        for slot in layers {
            let layer = slot.knobs();
            let session = layer.session;
            if session == &self.session {
                return Err(LamboError::Config(format!(
                    "layered recall: session {session} cannot be its own layer"
                )));
            }
            if seen.contains(&session) {
                return Err(LamboError::Config(format!(
                    "layered recall: layer {session} is listed twice"
                )));
            }
            seen.push(session);
            if !(layer.weight.is_finite() && layer.weight > 0.0) {
                return Err(LamboError::Config(format!(
                    "layered recall: layer {session} weight must be finite and positive, got {}",
                    layer.weight
                )));
            }
            if let Some(m) = layer
                .min_similarity
                .filter(|m| !(m.is_finite() && (0.0..=1.0).contains(m)))
            {
                return Err(LamboError::Config(format!(
                    "layered recall: layer {session} min_similarity must be finite and in [0, 1], got {m}"
                )));
            }
        }
        Ok(())
    }

    /// This session's side of another session's layered recall: the
    /// daemon's `Route::Layer` pipeline with the caller's query
    /// vector. `Err` (a reason for the log) when this session cannot serve
    /// as a layer right now: it is closed, or its embedding contract is not
    /// the caller's `contract` (checked with or without a query vector: the
    /// merge compares holder vectors across sessions, which is meaningful
    /// only in one embedding space). `embed_warning` is the caller's query
    /// embed failure when the caller embedded for its layers only; it leads
    /// this layer's warnings when this layer's vector leg could have run.
    pub(crate) async fn recall_as_layer(
        &self,
        query: RecallQuery,
        embedding: Option<&[f32]>,
        contract: &EmbeddingContract,
        embed_warning: Option<&str>,
    ) -> Result<TierRecall, String> {
        self.ensure_open().map_err(|e| e.to_string())?;
        if contract != &self.embedding {
            return Err(format!(
                "embedding contract {:?} differs from the caller's {contract:?}",
                self.embedding
            ));
        }
        let vectors = self.vector_candidates();
        let lead = embed_warning.filter(|_| vectors.available());
        let mut recall = self
            .daemon
            .recall_layer_with(
                &self.session,
                query,
                vectors,
                embedding.map(|vector| (vector, &self.embedding)),
                self.config.recall_weights,
            )
            .await;
        if let Some(line) = lead {
            recall.warnings.insert(0, line.to_string());
        }
        let identities = self.hit_identities(&recall.hits);
        Ok(TierRecall { recall, identities })
    }

    /// What dedupe compares for each hit: its canonical key and holder
    /// vector from this session's graph, read [`IDENTITY_CHUNK`] hits per
    /// lock so a large `top_k` never holds the graph lock for one long
    /// copy. A concept removed between the recall and this read has an
    /// unknown identity, which never matches.
    fn hit_identities(&self, hits: &[RecallHit]) -> Vec<HitIdentity> {
        let mut out = Vec::with_capacity(hits.len());
        for chunk in hits.chunks(IDENTITY_CHUNK) {
            let g = self.graph.read();
            out.extend(chunk.iter().map(|h| match g.node(h.node_id) {
                Some(Node::Concept(c)) => HitIdentity {
                    canonical_key: Some(c.canonical_key.clone()),
                    embedding: c.embedding.clone(),
                },
                _ => HitIdentity::default(),
            }));
        }
        out
    }
}

/// Poll every future to completion concurrently and return their outputs
/// in order (the crate has no `futures` dependency, and adding one for this
/// is not worth it). A future that panics is dropped and gives `Err`, so
/// one layer's panic leaves that layer out instead of failing the call;
/// the others keep running. The unwind-safety assertion holds because a
/// panicked future is never polled again, and the sessions' locks either
/// do not poison (`parking_lot`) or release on unwind (`tokio`).
async fn join_all<T>(
    futures: Vec<Pin<Box<dyn Future<Output = T> + Send + '_>>>,
) -> Vec<Result<T, String>> {
    let mut slots: Vec<Option<Pin<Box<dyn Future<Output = T> + Send + '_>>>> =
        futures.into_iter().map(Some).collect();
    let mut out: Vec<Option<Result<T, String>>> = slots.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (slot, done) in slots.iter_mut().zip(out.iter_mut()) {
            let Some(f) = slot else { continue };
            let polled =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.as_mut().poll(cx)));
            match polled {
                Ok(Poll::Ready(v)) => {
                    *done = Some(Ok(v));
                    *slot = None;
                }
                Ok(Poll::Pending) => pending = true,
                Err(_) => {
                    *done = Some(Err("the layer's recall panicked".to_string()));
                    *slot = None;
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    out.into_iter()
        .map(|o| o.expect("every future completed"))
        .collect()
}

/// L8: `recall_layered`'s future is `Send`, so a serve handler can spawn
/// it. Checked at compile time; never called.
#[allow(dead_code)]
fn recall_layered_is_send(
    memory: &Memory,
    layers: &[LayerRef<'_>],
    slots: &[LayerSlot<'_>],
    query: RecallQuery,
) {
    fn send<T: Send>(_: T) {}
    send(memory.recall_layered(layers, query.clone()));
    send(memory.recall_layered_slots(slots, query));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L3: a panicking layer future gives `Err` for that layer only.
    #[tokio::test]
    async fn join_all_turns_a_panic_into_that_futures_error() {
        let futures: Vec<Pin<Box<dyn Future<Output = u32> + Send>>> = vec![
            Box::pin(async { 1 }),
            Box::pin(async {
                tokio::task::yield_now().await;
                panic!("layer recall blew up")
            }),
            Box::pin(async {
                tokio::task::yield_now().await;
                3
            }),
        ];
        let out = join_all(futures).await;
        assert_eq!(out[0], Ok(1));
        assert!(out[1].is_err());
        assert_eq!(out[2], Ok(3));
    }
}
