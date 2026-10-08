//! Vector candidate selection for the SQLite adapter (F1/F2, issue #5): the
//! checked read that binds the session's durable embedding contract and the
//! candidate scan into one transaction, the frozen v0.2.0 unchecked surface
//! that re-enters it, and the scan itself. Width authority and the
//! selection/scoring seam are described in the adapter's module doc
//! ("Vector search").

use sqlx::Row;

use super::codec::{db_err, node_id, session_embedding_from_parts};
use super::SqliteStore;
use crate::store::vector::decode_vector_blob;
use crate::store::{validate_vector_candidate_limit, GraphStore};
use crate::types::{tie_break_by_key, EmbeddingContract, NodeId, Scored, SessionId, StoreError};

/// One decoded stored vector, before scoring, with the concept's canonical key
/// riding along (the issue-2 tie-break consumes it on exact score ties).
pub(super) type VectorCandidate = (NodeId, Vec<f32>, String);

/// **Candidate selection** — the swappable half of the vector query path (F1).
///
/// Today: every non-null `concepts.embedding` in the session, decoded. `probe` and
/// `limit` are part of the signature although an exact scan cannot use them, so that an
/// ANN index (see the module doc's "The scan is a seam") replaces this function's body
/// without touching [`rank_by_cosine`], `vector_candidates_checked`, or any caller.
///
/// Runs on the caller's transaction: the contract read that authorised this scan and the
/// scan itself must observe one snapshot.
///
/// **Width is checked, not truncated.** The BLOB holds the shared `[x,y,z]` text codec
/// (CON-8), so a row whose decoded element count disagrees with the session contract is
/// a corrupt row — returned as [`StoreError::Backend`]. `cosine` refuses length
/// mismatches by scoring 0.0, which would silently rank a corrupt concept last instead of
/// reporting it.
pub(super) async fn select_session_vectors(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
    _probe: &[f32],
    _limit: usize,
    dim: usize,
) -> Result<Vec<VectorCandidate>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, canonical_key, embedding FROM concepts \
         WHERE session_id = ? AND embedding IS NOT NULL ORDER BY id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("vector_candidates: session vectors", e))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row
            .try_get(0)
            .map_err(|e| db_err("vector_candidates: concept id", e))?;
        let key: String = row
            .try_get(1)
            .map_err(|e| db_err("vector_candidates: concept canonical key", e))?;
        let blob: Vec<u8> = row
            .try_get(2)
            .map_err(|e| db_err("vector_candidates: concept embedding", e))?;
        let vector = decode_vector_blob(&id, &blob)?;
        if vector.len() != dim {
            return Err(StoreError::Backend(format!(
                "concepts.embedding for {id} decodes to {} dimensions but session {} \
                 declares {dim}",
                vector.len(),
                session.0
            )));
        }
        out.push((node_id(&id, "concept id")?, vector, key));
    }
    Ok(out)
}

/// **Candidate scoring**, the fixed half. Exact cosine, best first; ties
/// broken by canonical key ascending, then the smaller node id
/// ([`tie_break_by_key`], issue #2), so the answer is deterministic across
/// runs as well as within one (MemoryStore / Cockroach parity). Stays exact
/// whatever [`select_session_vectors`] becomes: an approximate index would
/// prune the pool, never the ranking.
pub(super) fn rank_by_cosine(
    probe: &[f32],
    candidates: Vec<VectorCandidate>,
    limit: usize,
) -> Vec<Scored<NodeId>> {
    let mut scored: Vec<(Scored<NodeId>, String)> = candidates
        .into_iter()
        .map(|(id, vector, key)| {
            (
                Scored::new(id, f64::from(crate::embed::cosine(probe, &vector))),
                key,
            )
        })
        .collect();
    scored.sort_by(|(a, a_key), (b, b_key)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
    });
    let mut scored: Vec<Scored<NodeId>> = scored.into_iter().map(|(s, _)| s).collect();
    scored.truncate(limit);
    scored
}

impl SqliteStore {
    pub(super) async fn legacy_vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        // Frozen v0.2.0 compatibility surface (Cockroach parity). It cannot attest
        // which contract produced `embedding`, so production code never calls it;
        // re-entering the checked path with the session's currently stored contract
        // preserves the legacy result shape while keeping the contract/vector snapshot
        // race closed inside this adapter.
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let stored = match self.load_session(session).await {
            Ok(snapshot) => snapshot.embedding,
            Err(StoreError::SessionNotFound(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let Some(stored) = stored else {
            return Ok(Vec::new());
        };
        self.vector_candidates_checked(session, embedding, &stored, limit)
            .await
    }

    /// Exact cosine over the session's flushed embeddings (F1, issue #5).
    ///
    /// **One transaction covers the contract read and the candidate read.** The race
    /// it closes is **cross-process**, not in-process (F-R1-9): within one process the
    /// single pooled connection (`max_connections(1)`) is a mutex, so a same-process
    /// flush cannot land between two statements of a transaction that already holds
    /// the only connection. What the transaction closes is a concurrent *writer
    /// process* on the same file database — `lambo serve` writing while `lambo recall`
    /// or `serve-web` reads, the documented topology — where WAL snapshot isolation
    /// guarantees both statements observe one snapshot and a commit in between is
    /// invisible until this transaction ends. (It follows that a future
    /// `max_connections(n) > 1` would make the in-process interleave real as well;
    /// the transaction already covers it, but the reason would change.) The refusal is
    /// `StoreError::Invariant`, matching Cockroach and the `VectorSearchStore`
    /// reference so callers classify it identically on all three.
    ///
    /// **An empty answer is not an error.** An unknown session, a session with no
    /// durable contract yet, and a session whose concepts carry no vectors all return
    /// an empty candidate list — the shape a vector-capable store returns before its
    /// first embedding lands. Only a corrupt row or a contract change is an error.
    pub(super) async fn checked_vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        // B-E2E-R2-3: the probe has to be an embedding on this adapter too.
        // The pg family gets this for free by encoding the probe before it
        // binds it; SQLite hands the probe straight to `rank_by_cosine`, whose
        // `cosine` clamps the denominator, so a zero-norm probe used to score
        // every row 0.0 and return candidates in tie-break order while the
        // same call refused loudly on Postgres. Placed here, before the store
        // is read, because that is where the pg family encodes: same input,
        // same error, same point in the sequence.
        crate::store::vector::ensure_is_an_embedding(embedding)?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| db_err("begin vector candidate transaction", e))?;

        let row = sqlx::query(
            "SELECT embedding_kind, embedding_model, embedding_dim \
             FROM sessions WHERE session_id = ?",
        )
        .bind(&session.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| db_err("vector_candidates: session contract", e))?;
        let Some(row) = row else {
            return Ok(Vec::new());
        };
        let stored = session_embedding_from_parts(
            row.try_get(0)
                .map_err(|e| db_err("vector_candidates: session contract", e))?,
            row.try_get(1)
                .map_err(|e| db_err("vector_candidates: session contract", e))?,
            row.try_get(2)
                .map_err(|e| db_err("vector_candidates: session contract", e))?,
            &session.0,
        )?;
        let Some(stored) = stored else {
            return Ok(Vec::new());
        };
        stored.ensure_compatible(expected_contract).map_err(|err| {
            StoreError::Invariant(format!(
                "vector candidate lookup refused after embedding contract changed: {err}"
            ))
        })?;
        // The probe is the caller's; the contract it claims is now known to be the
        // durable one, so a probe of a different width is a caller bug rather than a
        // store state. `cosine` would silently score it 0.0 on every row.
        if embedding.len() != stored.dim {
            return Err(StoreError::Invariant(format!(
                "query embedding has {} dimensions but session {} stores vectors of {} \
                 (the session's durable embedding contract is the authority here, not \
                 the process-wide vector_dimensions())",
                embedding.len(),
                session.0,
                stored.dim
            )));
        }

        let candidates =
            select_session_vectors(&mut *tx, session, embedding, limit, stored.dim).await?;
        tx.commit()
            .await
            .map_err(|e| db_err("commit vector candidate transaction", e))?;
        Ok(rank_by_cosine(embedding, candidates, limit))
    }
}
