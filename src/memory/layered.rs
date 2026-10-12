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
use crate::types::{EmbeddingContract, LamboError, Node, RecallHit, RecallQuery};

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
    /// Finite and non-negative. Defaults to [`DEFAULT_LAYER_WEIGHT`]
    /// (uncalibrated until #87).
    pub weight: f64,
    /// The layer's floor: a hit without keyword evidence merges only at
    /// vector cosine `>=` this. In `[0, 1]`. Defaults to
    /// [`DEFAULT_LAYER_MIN_SIMILARITY`] (uncalibrated until #87).
    pub min_similarity: f64,
}

impl<'a> LayerRef<'a> {
    /// `memory` as a layer with the default weight and floor.
    pub fn new(memory: &'a Memory) -> Self {
        Self {
            memory,
            weight: DEFAULT_LAYER_WEIGHT,
            min_similarity: DEFAULT_LAYER_MIN_SIMILARITY,
        }
    }

    /// This layer with merge weight `weight`.
    pub fn with_weight(mut self, weight: f64) -> Self {
        self.weight = weight;
        self
    }

    /// This layer with floor `min_similarity`.
    pub fn with_min_similarity(mut self, min_similarity: f64) -> Self {
        self.min_similarity = min_similarity;
        self
    }
}

impl std::fmt::Debug for LayerRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayerRef")
            .field("session", self.memory.session())
            .field("weight", &self.weight)
            .field("min_similarity", &self.min_similarity)
            .finish()
    }
}

type LayerFuture<'a> = Pin<Box<dyn Future<Output = Result<TierRecall, String>> + Send + 'a>>;

impl Memory {
    /// Recall from this session and its `layers` (nearest tier first),
    /// merged into one answer under one `max_tokens` budget (#33).
    ///
    /// With no layers this is exactly [`Memory::recall`]: same hits, same
    /// context and warnings, every hit in this session.
    ///
    /// With layers, each session's list is merged by weighted rank
    /// interleave (`weight / rank`, this session at 1.0); a layer hit
    /// merges only with keyword evidence or vector cosine at or above the
    /// layer's floor; hits with a Conflict, Hot or Reservation warning are
    /// always kept; the same memory in two tiers (equal canonical key, or
    /// holder vectors at cosine at or above this session's
    /// `semantic_match_threshold`) keeps the nearer tier's copy. `top_k`
    /// bounds the merged non-forced hits. A layer hit's block names its
    /// session, and a truncated layer block's marker names the scope to
    /// inspect it with. See [`crate::recall::layered`] for the full rule.
    ///
    /// A layer that is closed, has a different embedding contract, or whose
    /// recall fails is left out with a warning; it never fails the call.
    /// No edge is followed across sessions.
    ///
    /// Refused with [`LamboError::Config`] when a layer is this session, a
    /// session appears twice, or a weight or floor is out of range; with
    /// whatever [`Memory::recall`] refuses for this session itself.
    pub async fn recall_layered(
        &self,
        layers: &[LayerRef<'_>],
        query: RecallQuery,
    ) -> Result<LayeredRecall, LamboError> {
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
        self.check_layers(layers)?;
        self.ensure_open()?;

        // Embed once. The caller's vector source decides, unless it cannot
        // search and a layer's can: then the embed still runs once, here.
        let own = self.vector_candidates();
        let source = if own.available() {
            own
        } else {
            layers
                .iter()
                .filter(|l| l.memory.embedding == self.embedding)
                .map(|l| l.memory.vector_candidates())
                .find(|v| v.available())
                .unwrap_or(own)
        };
        let (embedding, warning) = self.embed_recall_query(&query.query, source).await;
        let vector = embedding.as_deref();

        let primary = async {
            let recall = self.recall_text_with(query.clone(), vector, warning).await;
            let identities = self.hit_identities(&recall.hits);
            TierRecall { recall, identities }
        };
        let layer_futures: Vec<LayerFuture<'_>> = layers
            .iter()
            .map(|l| {
                Box::pin(
                    l.memory
                        .recall_as_layer(query.clone(), vector, &self.embedding),
                ) as LayerFuture<'_>
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
        for (layer, result) in layers.iter().zip(layer_results) {
            let recall = match result {
                Ok(t) => Some(t),
                Err(reason) => {
                    tracing::warn!(
                        target: "lambo::recall",
                        layer = %layer.memory.session,
                        "layered recall: layer unavailable: {reason}"
                    );
                    None
                }
            };
            lists.push(TierList {
                session: layer.memory.session.clone(),
                weight: layer.weight,
                floor: Some(layer.min_similarity),
                recall,
            });
        }

        let merged = layered::merge(
            &lists,
            MergeParams {
                top_k: query.top_k,
                max_tokens: query.max_tokens,
                dedupe_threshold: self.config.semantic_match_threshold,
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
        for layer in layers {
            layer.memory.note_accesses(own_hits(layer.memory));
        }
        Ok(merged)
    }

    /// The `Config` refusals of [`Memory::recall_layered`].
    fn check_layers(&self, layers: &[LayerRef<'_>]) -> Result<(), LamboError> {
        let mut seen = vec![&self.session];
        for layer in layers {
            let session = &layer.memory.session;
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
            if !(layer.weight.is_finite() && layer.weight >= 0.0) {
                return Err(LamboError::Config(format!(
                    "layered recall: layer {session} weight must be finite and non-negative, got {}",
                    layer.weight
                )));
            }
            if !(layer.min_similarity.is_finite() && (0.0..=1.0).contains(&layer.min_similarity)) {
                return Err(LamboError::Config(format!(
                    "layered recall: layer {session} min_similarity must be finite and in [0, 1], got {}",
                    layer.min_similarity
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
    /// only in one embedding space).
    pub(crate) async fn recall_as_layer(
        &self,
        query: RecallQuery,
        embedding: Option<&[f32]>,
        contract: &EmbeddingContract,
    ) -> Result<TierRecall, String> {
        self.ensure_open().map_err(|e| e.to_string())?;
        if contract != &self.embedding {
            return Err(format!(
                "embedding contract {:?} differs from the caller's {contract:?}",
                self.embedding
            ));
        }
        let recall = self
            .daemon
            .recall_layer_with(
                &self.session,
                query,
                self.vector_candidates(),
                embedding.map(|vector| (vector, &self.embedding)),
                self.config.recall_weights,
            )
            .await;
        let identities = self.hit_identities(&recall.hits);
        Ok(TierRecall { recall, identities })
    }

    /// What dedupe compares for each hit: its canonical key and holder
    /// vector from this session's graph (one brief read).
    fn hit_identities(&self, hits: &[RecallHit]) -> Vec<HitIdentity> {
        let g = self.graph.read();
        hits.iter()
            .map(|h| match g.node(h.node_id) {
                Some(Node::Concept(c)) => HitIdentity {
                    canonical_key: Some(c.canonical_key.clone()),
                    embedding: c.embedding.clone(),
                },
                _ => HitIdentity::default(),
            })
            .collect()
    }
}

/// Poll every future to completion concurrently and return their outputs
/// in order (the crate has no `futures` dependency, and adding one for this
/// is not worth it).
async fn join_all<T>(futures: Vec<Pin<Box<dyn Future<Output = T> + Send + '_>>>) -> Vec<T> {
    let mut slots: Vec<Option<Pin<Box<dyn Future<Output = T> + Send + '_>>>> =
        futures.into_iter().map(Some).collect();
    let mut out: Vec<Option<T>> = slots.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (slot, done) in slots.iter_mut().zip(out.iter_mut()) {
            if let Some(f) = slot {
                match f.as_mut().poll(cx) {
                    Poll::Ready(v) => {
                        *done = Some(v);
                        *slot = None;
                    }
                    Poll::Pending => pending = true,
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
