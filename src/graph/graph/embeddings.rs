//! The session's embedding space: stamping and replacing the
//! [`EmbeddingContract`](crate::types::EmbeddingContract), the atomic full
//! re-embed ([`Graph::reembed_all`]), the same-space backfill
//! ([`Graph::embed_missing`]) and the contract accessor.
//!
//! One session never mixes two model spaces: every path here either verifies
//! the stamped contract or rewrites the vectors and the contract in one
//! drained batch (vectors first, contract last).

use super::{invariant, Graph};
use crate::types::{LamboError, Mutation, Node, NodeId};

impl Graph {
    /// Stamp the session's embedding space on first vector work, or verify an
    /// existing stamp. Ordinary callers cannot clear or replace the contract.
    pub fn stamp_embedding(
        &mut self,
        contract: crate::types::EmbeddingContract,
    ) -> Result<(), LamboError> {
        if let Some(existing) = &self.embedding {
            existing.ensure_compatible(&contract)?;
            return Ok(());
        }
        self.embedding = Some(contract.clone());
        self.append_mutation(Mutation::SetEmbedding {
            session_id: self.session_id.clone(),
            embedding: Some(contract),
        });
        Ok(())
    }

    /// Explicit contract replacement/clear gate for an atomic re-embedding
    /// workflow. It is safe only after every vector-bearing concept has been
    /// removed or rewritten in the same staged graph transaction.
    pub fn replace_embedding_without_vectors(
        &mut self,
        contract: Option<crate::types::EmbeddingContract>,
    ) -> Result<(), LamboError> {
        if self.concepts().any(|c| c.embedding.is_some()) {
            return Err(invariant(
                "cannot clear or replace embedding contract while concept vectors remain",
            ));
        }
        if self.embedding != contract {
            self.embedding = contract.clone();
            self.append_mutation(Mutation::SetEmbedding {
                session_id: self.session_id.clone(),
                embedding: contract,
            });
        }
        Ok(())
    }

    /// Replace a same-width contract after an operator explicitly declares
    /// the stored vectors compatible with a renamed model identifier, or
    /// after a migration has already removed every old vector.
    ///
    /// This is crate-private because ordinary graph callers must never relabel
    /// an existing vector space. The only production caller is the
    /// `--allow-embedding-mismatch` writer attach path, which checks equal
    /// dimensions, permits vectors only for a same-kind identifier rename,
    /// emits a warning, and records the replacement durably. A cross-kind
    /// migration must clear/rewrite its vectors before this point.
    pub(crate) fn replace_embedding_with_operator_override(
        &mut self,
        contract: crate::types::EmbeddingContract,
    ) -> Result<(), LamboError> {
        if let Some(existing) = &self.embedding {
            if existing.dim != contract.dim {
                return Err(invariant(format!(
                    "cannot override embedding contract width {} with width {}; \
                     --allow-embedding-mismatch is only for same-width migrations",
                    existing.dim, contract.dim
                )));
            }
            if self.concepts().any(|concept| concept.embedding.is_some())
                && existing.kind != contract.kind
            {
                return Err(invariant(format!(
                    "cannot relabel {} vectors as {} while stored concept vectors remain; \
                     atomically clear/re-embed the vectors before changing embedder kind",
                    existing.kind, contract.kind
                )));
            }
        }
        if self.embedding.as_ref() != Some(&contract) {
            self.embedding = Some(contract.clone());
            self.append_mutation(Mutation::SetEmbedding {
                session_id: self.session_id.clone(),
                embedding: Some(contract),
            });
        }
        Ok(())
    }

    /// Atomic full re-embed: rewrite **every** concept vector into a new space
    /// and swap the session contract in one staged graph transaction (K2).
    ///
    /// This is the operation `lambo re-embed` runs when a session migrates to a
    /// different embedder (e.g. bge_m3 → candle): the `EmbeddingContract`
    /// forbids two model spaces in one session, so the old vectors must be
    /// replaced — not relabelled — in the same batch as the contract change.
    /// `replace_embedding_with_operator_override` refuses that (it only lets a
    /// same-kind identifier rename relabel existing vectors); this method is
    /// the sanctioned path that replaces vectors first.
    ///
    /// * `updates` maps every **text** concept id (no `embedding_source`) to
    ///   its freshly embedded vector in the target space. **Every** text
    ///   concept must appear: a concept left out keeps a vector from the old
    ///   space, which is exactly the mixed-space violation this operation
    ///   exists to end. An id that is not a concept, a vector of the wrong
    ///   width, or a non-finite vector is a hard error and the graph is left
    ///   untouched.
    /// * An **image** concept (#22, `embedding_source` set) is never in
    ///   `updates`: its vector is not a function of its text, so embedding its
    ///   caption would mislabel it. One whose vector is already missing is
    ///   left as it is. One that still carries a vector makes this refuse,
    ///   because that vector would stay in the old space; see
    ///   [`Graph::reembed_all_dropping_image_vectors`].
    /// * The contract swap allows the same width only (a re-embed never
    ///   changes dimensionality; a width change is a fresh session, not a
    ///   migration) and, like the RAM invariants everywhere, refuses a
    ///   non-different contract as a no-op.
    ///
    /// Mutations are appended in order, so the drained batch carries every
    /// `UpsertNode` (new vectors) **before** the trailing `SetEmbedding` (new
    /// contract). The store applies one flushed batch transactionally, so the
    /// durable session never holds the new contract beside old-space vectors —
    /// and on a crash mid-flush the old contract and old vectors survive
    /// together, which is consistent. Callers MUST flush the drained batch to
    /// the store; until then the RAM graph is ahead of the durable state.
    pub fn reembed_all(
        &mut self,
        updates: Vec<(NodeId, Vec<f32>)>,
        contract: crate::types::EmbeddingContract,
    ) -> Result<(), LamboError> {
        self.reembed(updates, contract, false).map(|_| ())
    }

    /// [`Graph::reembed_all`], nulling the vector of every image concept
    /// (#22) instead of refusing on it, in the same staged batch. Each one
    /// keeps its `embedding_source`: an image concept with no vector is
    /// "image vector missing" (`re-embed --missing-only` skips it, and
    /// re-deriving the same image restores it), never a text concept. Returns
    /// how many image vectors were nulled.
    pub fn reembed_all_dropping_image_vectors(
        &mut self,
        updates: Vec<(NodeId, Vec<f32>)>,
        contract: crate::types::EmbeddingContract,
    ) -> Result<usize, LamboError> {
        self.reembed(updates, contract, true)
    }

    fn reembed(
        &mut self,
        updates: Vec<(NodeId, Vec<f32>)>,
        contract: crate::types::EmbeddingContract,
        drop_image_vectors: bool,
    ) -> Result<usize, LamboError> {
        if let Some(existing) = &self.embedding {
            if existing.dim != contract.dim {
                return Err(invariant(format!(
                    "cannot re-embed session from width {} to width {}; a re-embed never \
                     changes dimensionality (start a fresh session for a different width)",
                    existing.dim, contract.dim
                )));
            }
            if *existing == contract {
                return Err(invariant(
                    "re-embed requested but the session already carries exactly this contract",
                ));
            }
        }

        let concept_ids: std::collections::HashSet<NodeId> = self
            .concepts()
            .filter(|c| c.embedding_source.is_none())
            .map(|c| c.id)
            .collect();
        let image_vectors: Vec<NodeId> = self
            .concepts()
            .filter(|c| c.embedding_source.is_some() && c.embedding.is_some())
            .map(|c| c.id)
            .collect();
        if !image_vectors.is_empty() && !drop_image_vectors {
            return Err(invariant(format!(
                "re-embed would leave {} image vector(s) in the old space: an image concept's \
                 vector cannot be recomputed from its caption. Drop them explicitly \
                 (reembed_all_dropping_image_vectors) or keep the current embedder",
                image_vectors.len()
            )));
        }
        // Duplicate ids are as fatal as missing ones: a list [a, a] over
        // concepts {a, b} has the right length but leaves `b` carrying an
        // old-space vector — exactly the mixed-space state this method exists
        // to end. Count distinct coverage, not list length.
        let mut covered: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for (id, _) in &updates {
            if !covered.insert(*id) {
                return Err(invariant(format!(
                    "re-embed updates list concept {id} twice; each concept appears exactly once"
                )));
            }
        }
        if covered.len() != concept_ids.len() {
            return Err(invariant(format!(
                "re-embed requires every concept: updates cover {} of {} concepts",
                covered.len(),
                concept_ids.len()
            )));
        }
        for (id, vector) in &updates {
            if !concept_ids.contains(id) {
                if matches!(self.nodes.get(id), Some(Node::Concept(c)) if c.embedding_source.is_some())
                {
                    return Err(invariant(format!(
                        "re-embed update targets {id}, an image concept: its vector is never \
                         replaced by a vector of its caption"
                    )));
                }
                return Err(invariant(format!(
                    "re-embed update targets {id}, which is not a concept in this session"
                )));
            }
            if vector.len() != contract.dim || vector.iter().any(|x| !x.is_finite()) {
                return Err(invariant(format!(
                    "re-embed vector for {id} is non-finite or has width {} != contract {}",
                    vector.len(),
                    contract.dim
                )));
            }
        }

        // Rewrite each concept's vector in the node map, then emit its upsert
        // mutation (the batch orders every UpsertNode before the SetEmbedding).
        for (id, vector) in updates {
            // Write through the node map, then emit the upsert from a CLONE:
            // `append_mutation` takes `&mut self`, which cannot overlap the
            // `self.nodes` borrow.
            let node = match self.nodes.get_mut(&id) {
                Some(Node::Concept(c)) => {
                    c.embedding = Some(vector);
                    Node::Concept(c.clone())
                }
                Some(Node::Interaction(_)) => {
                    return Err(invariant(format!(
                        "re-embed update targets {id}, which is an interaction, not a concept"
                    )));
                }
                None => {
                    return Err(invariant(format!(
                        "re-embed update targets {id}, which is not a node"
                    )));
                }
            };
            self.append_mutation(Mutation::UpsertNode { node });
        }
        // #22: null the image vectors (source kept), before the contract
        // swap like every other vector change of the batch.
        let dropped = image_vectors.len();
        for id in image_vectors {
            let node = match self.nodes.get_mut(&id) {
                Some(Node::Concept(c)) => {
                    c.embedding = None;
                    Node::Concept(c.clone())
                }
                _ => unreachable!("collected from this graph's concepts above"),
            };
            self.append_mutation(Mutation::UpsertNode { node });
        }
        self.embedding = Some(contract.clone());
        self.append_mutation(Mutation::SetEmbedding {
            session_id: self.session_id.clone(),
            embedding: Some(contract),
        });
        Ok(dropped)
    }

    /// Fill in vectors for concepts that have **none**, without touching the
    /// session contract or any vector already stored.
    ///
    /// This is the backfill twin of [`Graph::reembed_all`], and the two are
    /// mutually exclusive by design: `reembed_all` migrates *between* spaces
    /// and therefore refuses to run when the live contract is already the
    /// stored one, which is exactly the state a backfill runs in. Without this
    /// method a session that accumulated NULL vectors inside its own current
    /// space had no repair path at all — the 2026-09-01 dogfood finding, where
    /// `lambo re-embed` correctly refused with "already carries exactly this
    /// contract" and left 555 unembedded concepts in place.
    ///
    /// * The contract must already be stamped and identical to `contract`. A
    ///   session with no contract is refused rather than stamped here: a
    ///   backfill is repair, and stamping a space from a repair path is how a
    ///   session acquires a contract nobody chose.
    /// * Every id must name a concept whose `embedding` is `None`. Overwriting
    ///   an existing vector is refused — that is a migration, and migrations go
    ///   through `reembed_all` so the contract moves with them.
    /// * An image concept (#22, `embedding_source` set) is refused: its
    ///   missing vector is an image's, and a vector of its caption would
    ///   mislabel it. Re-deriving the image restores it.
    /// * Width and finiteness are checked exactly as in `reembed_all`.
    /// * Partial coverage is fine and expected: this is the one vector
    ///   operation that does not require every concept, because the concepts it
    ///   skips are already correct.
    ///
    /// Returns how many concepts were given a vector. Callers MUST flush the
    /// drained batch; until then the RAM graph is ahead of the durable state.
    pub fn embed_missing(
        &mut self,
        updates: Vec<(NodeId, Vec<f32>)>,
        contract: &crate::types::EmbeddingContract,
    ) -> Result<usize, LamboError> {
        match &self.embedding {
            None => {
                return Err(invariant(
                    "cannot backfill embeddings in a session with no embedding contract; \
                     a contract is stamped by the first real write, never by a repair",
                ))
            }
            Some(existing) if existing != contract => {
                return Err(invariant(format!(
                    "cannot backfill embeddings from a different space: session carries \
                     kind={} dim={} but the live embedder is kind={} dim={}; that is a \
                     migration, which is `re-embed`, not a backfill",
                    existing.kind, existing.dim, contract.kind, contract.dim
                )))
            }
            Some(_) => {}
        }

        let mut covered: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
        for (id, vector) in &updates {
            if !covered.insert(*id) {
                return Err(invariant(format!(
                    "backfill updates list concept {id} twice; each concept appears at most once"
                )));
            }
            match self.nodes.get(id) {
                Some(Node::Concept(c)) => {
                    if c.embedding.is_some() {
                        return Err(invariant(format!(
                            "backfill targets {id}, which already carries a vector; \
                             replacing a vector is a migration (`re-embed`), not a backfill"
                        )));
                    }
                    if c.embedding_source.is_some() {
                        return Err(invariant(format!(
                            "backfill targets {id}, an image concept: its missing vector is \
                             an image's, never a vector of its caption"
                        )));
                    }
                }
                Some(Node::Interaction(_)) => {
                    return Err(invariant(format!(
                        "backfill targets {id}, which is an interaction, not a concept"
                    )))
                }
                None => {
                    return Err(invariant(format!(
                        "backfill targets {id}, which is not a node"
                    )))
                }
            }
            if vector.len() != contract.dim || vector.iter().any(|x| !x.is_finite()) {
                return Err(invariant(format!(
                    "backfill vector for {id} is non-finite or has width {} != contract {}",
                    vector.len(),
                    contract.dim
                )));
            }
        }

        // No `SetEmbedding` tail here: the contract is unchanged, so emitting
        // one would append a mutation that says nothing and make the batch look
        // like a migration to anything reading the log.
        let filled = updates.len();
        for (id, vector) in updates {
            let node = match self.nodes.get_mut(&id) {
                Some(Node::Concept(c)) => {
                    c.embedding = Some(vector);
                    Node::Concept(c.clone())
                }
                // Unreachable: validated above, and `self` is not shared across
                // the two loops.
                _ => unreachable!("backfill target validated as a concept above"),
            };
            self.append_mutation(Mutation::UpsertNode { node });
        }
        Ok(filled)
    }

    pub fn embedding(&self) -> Option<&crate::types::EmbeddingContract> {
        self.embedding.as_ref()
    }
}
