//! Vector candidate selection for the Postgres-wire family: the checked read
//! (contract read, the global index-backed top-k with Rust-side session
//! filter and grow-and-retry, the exact session fallback, all in one
//! transaction replayed whole by `tx_retry`), the frozen v0.2.0 unchecked
//! surface that re-enters it, the H3 forced-exact hook, and the fetch-sizing
//! rules (DECISION D1). Scores come from the database's distance through
//! [`Dialect::distance_to_score`].

use sqlx::Row;

use super::codec::{
    backend, check_embedding_dim, order_candidates, parse_node_id, session_embedding_from_parts,
};
use super::pool::tx_retry;
use super::{Dialect, PgStore};
use crate::store::vector::encode_vector;
use crate::store::{validate_vector_candidate_limit, GraphStore};
use crate::types::{EmbeddingContract, NodeId, Scored, SessionId, StoreError};

// DECISION D1 (PHASE-7-embeddings.md T7.3): the vector query is GLOBAL
// (`ORDER BY embedding <-> $1::VECTOR LIMIT $k`, no session predicate) so the
// planner uses `concepts@concepts_embedding_idx` (the session-filtered shape scans
// `concepts_session_id_canonical_key_key` and bypasses the index — evidence in
// `dev-diary/evidence/t0.3-vector-spike.txt`). Session filtering happens in Rust.
// Because the trait passes only `limit`, the global fetch `$k` must be sized
// generously enough that the caller's in-session top candidates are not crowded out
// of the global top-k by foreign-session concepts. The sizing is a documented,
// deterministic approximation of "global top-k + session filter":
//
//   - BASE: the first global fetch is `limit × VECTOR_FETCH_MULTIPLIER` (a generous
//     headroom over the session's expected concept population), floored at the
//     multiplier and CAPPED at `VECTOR_FETCH_CAP` ([`initial_fetch_k`]) — `limit` is
//     caller-supplied but validated at the public 2,048-result bound, so the cap
//     keeps even the base fetch within the
//     documented worst-case bound (T7.3 remediation).
//   - GROW-AND-RETRY: the adapter requests `k + 1`; when that lookahead exists yet
//     the first `k` rows yield fewer than `limit` in-session hits, more global rows
//     exist beyond this window, so
//     re-query with `k` doubled (cheap: index-backed top-k is O(log n + k)), up to
//     `VECTOR_FETCH_CAP` ([`next_fetch_k`]).
//   - Completeness bound: retry STOPS EARLY when the `k + 1` lookahead is absent.
//     Under an EXACT scan (non-partial index) that means the global population is
//     exhausted, so no further in-session candidate can exist. Under the PARTIAL
//     ANN index (T7.4) `vector search` visits a bounded set of neighbourhoods, so a
//     lookahead-absent page means the BEAM exhausted its visited neighbourhoods,
//     not the table — a true near neighbour can be missed (see the ANN accuracy
//     dial doc above); that miss is accepted for v0.1, not "provably complete".
//   - Cap fallback: if the global page is still full and crowded at CAP, run an
//     exact session-scoped query. That path may not use the global vector index,
//     but it preserves the GraphStore completeness contract for adversarial
//     multi-tenant distributions. Normal traffic stays on the indexed fast path.
pub(super) const VECTOR_FETCH_MULTIPLIER: usize = 10;

pub(super) const VECTOR_FETCH_GROWTH: usize = 2;

pub(super) const VECTOR_FETCH_CAP: usize = 2048;

/// Keep only rows belonging to the caller's session from one global top-k fetch.
/// Input rows arrive in L2-distance-ascending order (SQL `ORDER BY dist ASC`), so
/// the survivors keep that order: the trait's score-descending ordering contract,
/// with the issue-2 tie-break ([`order_candidates`]) deciding equal scores.
/// Pure & deterministic: unit-tested without a cluster.
pub(super) fn filter_session_rows<D: Dialect>(
    session: &SessionId,
    rows: &[(NodeId, f64, String, String)],
) -> Vec<Scored<NodeId>> {
    order_candidates(
        rows.iter()
            .filter(|(_, dist, sid, _)| sid == &session.0 && dist.is_finite())
            .map(|(id, dist, _, key)| (Scored::new(*id, D::distance_to_score(*dist)), key.clone()))
            .collect(),
    )
}

/// True when the global fetch's kth and lookahead distances tie. The canonical
/// key tie-break can only order rows the fetch actually returned, so a tie
/// group cut by SQL's LIMIT has an arbitrary subset — this still forces the
/// exact session query, which re-fetches the session's own rows and re-orders
/// them with the same comparator (the forcing rule is unchanged by issue #2).
/// That makes the re-fetch authoritative, not unconditionally deterministic
/// (remediation round 1 doc alignment): a tie group outgrowing the exact
/// query's own LIMIT is still cut by its SQL `id` order, and rows sharing one
/// canonical key fall through to that run-minted id — the residual per-run
/// arbitrariness no in-process tie-break can remove.
pub(super) fn has_boundary_tie(rows: &[(NodeId, f64, String, String)], k: usize) -> bool {
    k > 0
        && rows.len() > k
        && rows[k - 1].1.is_finite()
        && rows[k].1.is_finite()
        && rows[k - 1].1.total_cmp(&rows[k].1).is_eq()
}

/// DECISION D1 base global fetch size. `limit × multiplier`, floored at the
/// multiplier so a non-trivial query always pulls some headroom, and CAPPED at
/// [`VECTOR_FETCH_CAP`] — `limit` is validated at the public boundary, and the
/// cap additionally ensures the first global fetch cannot exceed the
/// documented 2048-row worst-case bound. The growth step in [`next_fetch_k`] is
/// already capped; this extends the same bound to the BASE. `limit == 0` is
/// short-circuited by the caller before reaching here.
pub(super) fn initial_fetch_k(limit: usize) -> usize {
    // VECTOR_FETCH_MULTIPLIER <= VECTOR_FETCH_CAP is guaranteed, so clamp cannot panic.
    limit
        .saturating_mul(VECTOR_FETCH_MULTIPLIER)
        .clamp(VECTOR_FETCH_MULTIPLIER, VECTOR_FETCH_CAP)
}

/// Grow-and-retry decision for the global vector fetch (DECISION D1). Given
/// whether the `k + 1` lookahead found another row, how many rows were
/// in-session, and the current `k`, return
/// the next `k` to fetch, or `None` when the current result is final.
///
/// Final when any of:
///   - at least `limit` in-session hits were surfaced (`in_session >= limit`);
///   - lookahead found no more row (`has_more == false`). Exact scan: the global
///     population is exhausted, so no further in-session candidate can exist. Partial
///     ANN index (T7.4): the beam exhausted its visited neighbourhoods, so a true
///     near neighbour may be missed — not provably complete (see the ANN dial doc);
///   - `k` is at `VECTOR_FETCH_CAP`.
///
/// Otherwise the page has more rows yet under-delivered — more global rows may hold
/// in-session candidates — so double `k` (capped) and retry.
pub(super) fn next_fetch_k(
    in_session: usize,
    has_more: bool,
    k: usize,
    limit: usize,
) -> Option<usize> {
    if in_session >= limit || !has_more || k >= VECTOR_FETCH_CAP {
        None
    } else {
        Some((k.saturating_mul(VECTOR_FETCH_GROWTH)).min(VECTOR_FETCH_CAP))
    }
}

pub(super) fn needs_session_fallback(
    in_session: usize,
    has_more: bool,
    k: usize,
    limit: usize,
) -> bool {
    in_session < limit && has_more && k >= VECTOR_FETCH_CAP
}

impl<D: Dialect> PgStore<D> {
    /// Issue [`Dialect::forced_exact_scan_sql`] on `tx` when the H3
    /// forced-exact flag is set. Shared by production search and the
    /// camera-proof EXPLAIN helper so the GUC is not a lookalike extra_set.
    pub(crate) async fn issue_forced_exact_scan(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), StoreError> {
        if self.force_exact_scan {
            if let Some(sql) = D::forced_exact_scan_sql() {
                sqlx::query(sql).execute(&mut **tx).await.map_err(backend)?;
            }
        }
        Ok(())
    }

    /// The production vector-candidates statement this store issues.
    /// Camera-proofs EXPLAIN this string, not a hand-copied lookalike.
    #[cfg(all(test, feature = "store-postgres"))]
    pub(crate) fn vector_candidates_sql(&self) -> &str {
        &self.sql.vector_candidates
    }

    /// Session-scoped fallback of [`Self::vector_candidates_sql`].
    #[cfg(all(test, feature = "store-postgres"))]
    pub(crate) fn session_vector_candidates_sql(&self) -> &str {
        &self.sql.session_vector_candidates
    }

    pub(super) async fn legacy_vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        // Frozen v0.2.0 compatibility surface. It cannot attest which contract
        // produced `embedding`, so production code never calls it. Reusing the
        // checked transaction with the currently stored contract preserves the
        // legacy result shape while still preventing a contract/vector snapshot
        // race inside this adapter.
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        check_embedding_dim(embedding, self.vector_dim)?;
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
        check_embedding_dim(embedding, self.vector_dim)?;
        let pool = &self.pool().await?;
        let probe = encode_vector(embedding)?;
        // One retried serializable read transaction binds contract validation
        // to every candidate statement. Cockroach may abort this hot read with
        // SQLSTATE 40001 while a writer replaces the contract and vectors, so
        // every retry must replay the contract read, global growth loop, exact
        // fallback, and commit as one unit.
        tx_retry(|| async {
            let mut tx = pool.begin().await.map_err(backend)?;
            let Some(contract_row) = sqlx::query(&self.sql.select_session)
                .bind(session.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?
            else {
                return Ok(Vec::new());
            };
            let stored = session_embedding_from_parts(
                contract_row.try_get("embedding_kind").map_err(backend)?,
                contract_row.try_get("embedding_model").map_err(backend)?,
                contract_row.try_get("embedding_dim").map_err(backend)?,
                session.as_str(),
            )?;
            let Some(stored) = stored else {
                return Ok(Vec::new());
            };
            stored.ensure_compatible(expected_contract).map_err(|err| {
                StoreError::Invariant(format!(
                    "vector candidate lookup refused after embedding contract changed: {err}"
                ))
            })?;

            // H3 forced-exact: after the contract/PK read so that lookup still
            // uses its index, before the vector query so hnsw cannot serve it.
            // Shared with the camera-proof EXPLAIN helper: do not re-issue the
            // GUC as extra_set (B3-R1-1).
            self.issue_forced_exact_scan(&mut tx).await?;

            // DECISION D1: GLOBAL index-backed top-k (`concepts@concepts_embedding_idx`),
            // then Rust-side session filter. `k` starts generous (limit × multiplier) and
            // grows via [`next_fetch_k`] when a full page still under-delivers in-session
            // hits — bounding under-return while never reading outside the global top-k.
            let mut k = initial_fetch_k(limit);
            loop {
                let fetch = k
                    .checked_add(1)
                    .ok_or_else(|| StoreError::Invariant("vector fetch window overflow".into()))?;
                let fetch = i64::try_from(fetch).map_err(|_| {
                    StoreError::Invariant("vector fetch window does not fit i64".into())
                })?;
                let rows = sqlx::query(&self.sql.vector_candidates)
                    .bind(&probe)
                    .bind(fetch)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(backend)?;

                // (id, dist, session_id, canonical_key): session_id selected so
                // foreign rows can be dropped, canonical_key so equal distances
                // order stably across runs (issue #2).
                let parsed = rows
                    .iter()
                    .map(|r| {
                        let id: String = r.try_get("id").map_err(backend)?;
                        let dist: f64 = r.try_get("dist").map_err(backend)?;
                        let sid: String = r.try_get("session_id").map_err(backend)?;
                        let key: String = r.try_get("canonical_key").map_err(backend)?;
                        Ok((parse_node_id(&id)?, dist, sid, key))
                    })
                    .collect::<Result<Vec<_>, StoreError>>()?;

                // Fetch one lookahead row. If it ties the kth boundary distance,
                // SQL's arbitrary subset of that tie group cannot be made
                // deterministic in Rust; switch to the exact session query.
                let boundary_tie = has_boundary_tie(&parsed, k);
                let has_more = parsed.len() > k;
                let page_len = parsed.len().min(k);
                let mut in_session = filter_session_rows::<D>(session, &parsed[..page_len]);
                if boundary_tie || needs_session_fallback(in_session.len(), has_more, k, limit) {
                    let exact_limit = i64::try_from(limit).map_err(|_| {
                        StoreError::Invariant("vector candidate limit does not fit i64".into())
                    })?;
                    let fallback_rows = sqlx::query(&self.sql.session_vector_candidates)
                        .bind(&probe)
                        .bind(session.as_str())
                        .bind(exact_limit)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(backend)?;
                    let hits = order_candidates(
                        fallback_rows
                            .iter()
                            .map(|row| {
                                let id: String = row.try_get("id").map_err(backend)?;
                                let dist: f64 = row.try_get("dist").map_err(backend)?;
                                let key: String = row.try_get("canonical_key").map_err(backend)?;
                                let score = D::distance_to_score(dist);
                                if !score.is_finite() {
                                    return Err(StoreError::Backend(format!(
                                        "non-finite vector distance for concept {id}"
                                    )));
                                }
                                Ok((Scored::new(parse_node_id(&id)?, score), key))
                            })
                            .collect::<Result<Vec<_>, StoreError>>()?,
                    );
                    tx.commit().await.map_err(backend)?;
                    return Ok(hits);
                }
                match next_fetch_k(in_session.len(), has_more, k, limit) {
                    None => {
                        // Query returns rows in dist-asc (= score-desc); filter preserves that
                        // order (filter_session_rows). Truncate to the requested limit.
                        in_session.truncate(limit);
                        tx.commit().await.map_err(backend)?;
                        return Ok(in_session);
                    }
                    Some(next) => {
                        k = next;
                    }
                }
            }
        })
        .await
    }
}
