# Adversarial Review: P7 — Hybrid matching + vector_candidates (T7.2 + T7.3)

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: REMEDIATE — 1 MAJOR (hybrid key-collision) + 4 MINOR    ║
║  Reviewed: 2026-08-13                                             ║
║  Scope:    Whole-P7-worktree delta vs phase/p7-embeddings         ║
║            (4 commits: f0313dd, 68301fb, 5e947ae, f1f86ed)        ║
║            src/graph/hybrid.rs · derive.rs · mod.rs ·             ║
║            cockroach.rs (vector_candidates D1) · evidence + doc   ║
║  Reviewer: P7AdveReview (adversarial whole-worktree sweep,        ║
║            cross-task integration / single-writer TOCTOU /        ║
║            determinism / threshold law / blast radius /           ║
║            resources+honesty / spec 7.1·5·3.2·6.4·2.2·12.1)       ║
║  Verdict:  REMEDIATE — fix MAJOR-1 before merge; MINOR-1..4       ║
║            track (some are accepted-documented tradeoffs)         ║
╚══════════════════════════════════════════════════════════════════╝
```

Whole worktree delta = the two accepted task deliverables combined, so the
focus here is what the per-task reviews could not see: the seam where T7.2
consumes T7.3, the single-writer plan→commit frontier, and the store change's
effect on the pre-existing caller (recall's `gather`, T5.1).

## Non-live gates (run once, per scope)

- `cargo check --all-targets` — **clean** (finished, no errors).
- `cargo clippy --all-targets -- -D warnings` — **clean** (no warnings).
- `cargo test --all` — **428 passed; 0 failed; 1 ignored**.
- `cargo test --lib --features embed-fixture` (hybrid twin tests) — **428 passed,
  0 failed; all 7 `graph::hybrid::tests::*` pass**, including
  `near_pair_merges_with_decaying_semantic_edge`, the contract-swap, embed-failure
  and capability-absent paths. The T7.3 `vector_explain_camera_proof` is `#[ignore]`
  (documented-PENDING camera proof), so it does not run in the default suite.

I did not edit source or add tests during this review (adversarial, read/trace only).

---

## Findings

### MAJOR-1 (P1) — Hybrid `derive` hard-errors and leaves a partial write when one call carries two distinct contents that share a canonical key (divergence from sync `derive`)

- **File/construct:** `src/graph/hybrid.rs` phase-1 eager canonicalization
  (items build, ~lines 263–276) + commit write loop (~lines 421–525, specifically
  the `Fresh`/`HybridMerge` branches that unconditionally `insert_concept` a fresh
  node with the precomputed key at 451–486).
- **Why it matters / trigger:** sync `derive` resolves each content in the write
  loop (`resolve_concept` re-`canonicalize`s at write time), so two distinct
  contents that canonicalize to the same key — e.g. `"user schema"` and
  `"schema user"` → key `"schema user"` — collapse into ONE node (documented in
  derive.rs lines 39–42 and tested by `derive_collapses_contents_sharing_a_canonical_key`).
  Hybrid instead canonicalizes ALL items up front in phase 1 (both come back
  `Unmatched`, since neither exists yet) and never re-canonicalizes or collapses by
  key at commit. At commit the FIRST colliding content creates a concept with key K;
  the SECOND unconditionally calls `insert_concept` with the same key K →
  `Graph::insert_concept`'s `UNIQUE (session_id, canonical_key)` collision check
  (graph.rs ~435–452, non-Observation) returns `Err(Invariant)`, and `hybrid::derive`
  propagates it. So a single hybrid call with two colliding non-Observation contents:
  (a) hard-fails where sync `derive` succeeds, and (b) leaves partial state — the
  first node + its `Derives` edge are already in `nodes` and the mutation log before
  the error, violating derive's validate-then-mutate / no-partial-batch guarantee.
  `insert_concept` (graph.rs 411–472) inserts id1 fully (nodes + UpsertNode + Derives
  UpsertEdge) before the second insert errors, and there is no rollback on `Err`.
- **Confidence:** 0.85 (root cause confirmed by reading phase-1 vs commit; the
  collision check and the sync-collapse test are conclusive; the exact input needs a
  two-contents-same-key hybrid call, which no hybrid test currently exercises).
- **Fix + test:** collapse colliding keys at commit — after writing a fresh /
  merged concept, record its key→node in a per-call map and, for a later item whose
  (precomputed) key already resolved this call, follow the same skip-and-record path
  as `CanonicalMatch` (push the existing node into `outcome.matched`, no second
  write) rather than creating a duplicate. Simplest faithful mirror of sync derive:
  at commit, re-`canonicalize` each item under the write lock so the second content
  resolves `Matched` to the just-created node. Add a regression test mirroring
  derive's `..._collapses_contents_sharing_a_canonical_key` but through
  `hybrid::derive` (two same-key contents in one call, SpyStore returning an
  above-threshold hit): assert one concept, both in `matched`, `outcome.created.len()==1`,
  `graph.assert_invariants()` passes, and no `Err`.

---

### MINOR-1 (P3) — `HybridMerge` "degrade to keyword-only" branch keeps the embedding (contradicts MAJOR-1 precision law)

- **File/construct:** `src/graph/hybrid.rs` commit `HybridMerge` branch (468–518):
  the concept is created with `embedding: Some(embedding.clone())` at ~482 BEFORE the
  candidate-target validation `if let Some(Node::Concept(_)) = g.node(*target)` at
  492. When the target is not a Concept, only the `Semantic` edge is skipped; the
  concept keeps its vector. The module doc/comment claims "degrade to keyword-only",
  but the embedding is retained — a vector persisted for a concept that was refused a
  merge, exactly what the precision bias (MAJOR-1 law) forbids.
- **Reachability:** today `vector_candidates` returns only persisted Concept ids in
  the session and single-writer means no interleaving can demote a node to a
  non-Concept between gather and commit, so it is defensive-only (unreachable in
  practice). Reported for consistency of the law, not as an active exploit.
- **Confidence:** 0.5 (behavior mismatch is certain; reachability is near-zero).
- **Fix + test:** if the target is invalid, rebuild the concept with
  `embedding: None` (or strip the embedding) before returning, so the retained-vector
  branch actually matches the "keyword-only" comment; add a unit test that seeds the
  store with a bogus non-concept `NodeId` hit and asserts `embedding().is_none()` on
  the written concept.

### MINOR-2 (P3) — Global-then-filter can under-return (or return fewer than the old per-session-exact query) for recall under global crowding

- **File/construct:** `src/store/cockroach.rs` `vector_candidates` grow-and-retry
  loop (1459–1491) + `next_fetch_k` (cap 2048).
- **Why it matters:** the old SQL filtered in-session FIRST (`WHERE session_id=$1`
  then `ORDER BY dist LIMIT limit`) and was unconditionally complete for the session.
  The new D1 global-fetch + Rust filter is exact only while the session's candidates
  are inside the fetched global top-k; when the global page stays FULL up to the
  `VECTOR_FETCH_CAP` and the session's relevant concepts rank beyond position 2048
  (large/crowded cluster), it returns fewer (possibly zero) in-session hits than the
  old query — including for recall's `gather` (limit = `top_k`), which previously
  always got the session's own top-k. Bounded (≤ ~sum of doubling up to 2048 rows,
  up to ~8 round-trips) and documented as the DECISION D1 approximation, so not a
  silent/dangerous drop; on the hot recall path an empty/crowded session also pays
  the grow-retry cost before returning few or no rows.
- **Confidence:** 0.7 (logic is provable; severity is P3 because it is the accepted,
  documented tradeoff and the common case — non-full page — is exact).
- **Fix + test (optional hardening, not a blocker):** document on the trait that
  results are complete only within the global CAP, or make recall's caller aware it
  may receive fewer than `limit`. A unit test for `next_fetch_k` already covers the
  cap termination.

### MINOR-3 (P3) — §12.1 "vector index in use" is not camera-proven on the target infra (PENDING is honest but the spec item is unmet)

- **File/construct:** `dev-diary/evidence/20260812-235945-vector-index.txt` +
  `#[ignore] vector_explain_camera_proof` + T7.3 handoff "status: done # EXPLAIN
  vector-search camera-proof is PENDING".
- **Why it matters:** the captured plan is a global `top-k` over `scan concepts`
  (via the `concepts_key_non_obs_idx` partial index — a concept_type fast-path, NOT
  the embedding index) and contains no `vector search` node and no
  `concepts_embedding_idx` reference. The evidence honestly labels the camera-proof
  PENDING and explains the optimizer's cost choice. That is not a fabrication — but
  it means the headline deliverable claim (DECISION D1 item 3 / spec §12.1 "we used
  the vector index", on camera) is not demonstrated on the demo cluster, and the
  gated test meant to prove it is ignored in the default suite. Note too that the
  production query binds a PARAMETERIZED `LIMIT $2`, while the captured EXPLAIN uses a
  LITERAL `LIMIT 5`; the remediation explicitly allows the parameterized shape to
  fall back to `limit`+`sort`, i.e. the on-cluster proof of the production query is
  even weaker than the literal-limit capture.
- **Confidence:** 0.7 (honesty is good; conformance gap is real).
- **Disposition (decision for the integrator before ship):** if §12.1 is a hard demo
  gate, either (a) land the favorable-deployment capture (freshly ANALYZEd, >~1k
  DISTINCT embeddings or single-region) and un-ignore `vector_explain_camera_proof`,
  or (b) if a forced path is unacceptable, formally downgrade §12.1's claim from
  "vector index in use" to "global top-k over the embedding-computed distance" on the
  demo cluster. Do not ship claiming "we used the vector index" on camera without one
  of these.

### MINOR-4 (NIT) — `note_fallback_logged` module-global set grows without bound

- **File/construct:** `src/graph/hybrid.rs` `note_fallback_logged` (118–125): a
  process-lifetime `HashSet<SessionId>` that never prunes; long-running/multi-session
  daemons accrue one entry per session id. Trivial memory, cross-session keyed as
  intended (T8.1 not yet present). Cosmetic; no fix required this cycle.

---

## Cross-cutting checks that came back clean (no finding)

- **Cross-task threshold/order contract (T7.2 consumes T7.3):** Cockroach returns
  L2-ascending = score-descending (`distance_to_score` = `1 - d²/2`, the metric the
  0.85 threshold is written against); hybrid defensively picks `max_by(score)`
  among hits ≥ threshold, so ordering does not matter to it. Consistent.
- **Session-scoping:** hybrid passes its own `session_id` and T7.3 filters
  foreign-session rows; the new `check_vector_candidates_are_session_scoped` covers
  a closer-foreign concept. Correct.
- **Single-writer plan→commit TOCTOU (spec §2.2):** the only values crossing the
  await frontier are the phase-1 `canonicalize` keys/matches and candidates from the
  pre-commit store; with one writer per session the results cannot be invalidated
  between lock acquisitions, and the candidate merge target is id-based so a
  demotion between plan and commit cannot corrupt the edge endpoints. Lock is never
  held across `.await` (gather is lock-free). Sound.
- **Cycle invariants:** `Semantic` is Concept→Concept, self-loop rejected, and the
  graph's §5.7 cycle invariants target `Hierarchical` only; a `Semantic` similarity
  cycle is not an invariant violation (recall expansion revisits are handled). Not a
  bug.
- **Blast radius on recall `gather` (T5.1):** results remain session-scoped,
  score-descending, and complete in the common (non-full-page) case; ordering is
  irrelevant to recall because it re-merges and re-sorts by score anyway. Only the
  documented-pessimal crowding case differs (MINOR-2). Capability gating unchanged:
  SQLite/Memory/`resolve.rs` still return `StoreError::Capability`; `SessionNotFound`
  and `limit==0 -> []` guards retained.
- **Resource bounds:** `initial_fetch_k` clamps the base at 2048; growth is capped at
  2048; loop terminates (page-not-full, sufficient hits, or cap) in ≤ ~8 iterations;
  no unbounded loop, no unbounded allocation beyond ≤2048 scored rows.
- **Secrets/honesty:** no DSN/password/secret material in any source, doc, or
  evidence diff; the vector-index evidence honestly shows a scan plan and labels the
  camera-proof PENDING (MINOR-3 is a conformance-gap call, not a fabrication).

## Verdict

**REMEDIATE.** Blocking: **MAJOR-1** only — the hybrid twin must not hard-fail (and
partially write) on a single call whose distinct contents share a canonical key; it
must mirror sync `derive`'s collapse. MINOR-1..4 are the only other deltas found;
MINOR-2/3 are accepted-documented tradeoffs to confirm at ship, MINOR-1 is a
defensive-law consistency fix, MINOR-4 is cosmetic. Airlift the non-live gates above
(clean) and re-run the hybrid suite after the MAJOR-1 fix lands.

— P7AdveReview, 2026-08-13
