# Adversarial Review: P5 Recall - GPT5.6sol

- **Status:** REQUEST CHANGES
- **Target:** `phase/p5-recall` at `bf0ccf3`
- **Base:** `73aa894`
- **Reviewer:** GPT-5.6-sol
- **Date:** 2026-08-12

## Executive summary

P5 is not ready to merge. The implementation passes its existing automated checks, but adversarial inspection and focused regression tests exposed four P1 correctness defects and four P2 robustness or scalability defects. The most serious issues allow recall to return results assembled from a different graph epoch than the cache key, reuse cached results across materially different vector-search states, replace all keyword matches with unrelated recent concepts, and exceed the caller's token budget.

The existing P5 review's claims that cache linearization is sound and that the branch is completely clean should be withdrawn until the defects below are fixed and covered by regression tests.

## Findings

### [P1] Cache hits are not linearized to the graph epoch in the key

**Location:** `src/daemon/mod.rs:325-333`, `src/daemon/mod.rs:388-390`

The daemon briefly reads the graph to obtain the epoch, releases that guard, checks the cache, and later reacquires the graph for assembly. A mutation between those operations lets a cache entry for epoch E be assembled against graph E+1. The miss path has the same class of gap because it releases the graph after candidate computation and reacquires it for final assembly.

This violates the cache's central consistency invariant: the epoch in the key does not identify the graph snapshot used to build the returned context.

**Required remediation:** Hold one graph read guard from epoch validation or candidate computation through assembly after the asynchronous work completes, or revalidate the epoch and retry before returning. Add a deterministic concurrency regression test that mutates the graph between cache lookup and assembly.

### [P1] The cache key omits vector-source state

**Location:** `src/daemon/mod.rs:326-343`, `src/daemon/mod.rs:375-380`

The key includes query hash, `top_k`, depth, and graph epoch, but the pipeline also depends on whether an embedding was produced, transient vector-store success or failure, and durable write-behind progress. None of those state changes necessarily advances the graph epoch. A result computed with `Some(embedding)` can therefore be served to a later `None` request, a transient vector-store failure can be cached until an unrelated graph mutation, and newly flushed vectors can remain invisible behind an older cache entry.

**Required remediation:** Do not cache vector-dependent results until the key includes a reliable vector-store generation and embedding-contract identity. At minimum, bypass caching whenever vector search participates or fails. Add tests for `Some -> None`, `None -> Some`, store failure -> recovery, and write-behind flush completion without a graph mutation.

### [P1] The recent-concept leg can evict every keyword match

**Location:** `src/recall/candidates.rs:123-147`

Keyword search is capped at `limit`, recent concepts are assigned a flat `0.5` score, and the combined set is sorted and truncated to the same `limit`. This makes phase-one truncation a cross-leg competition even though the later pipeline expects multiple candidate sources. A focused reproduction with 100 exact keyword matches, 12 unrelated concepts from the latest three interactions, and `top_k = 5` returned five unrelated recent concepts and no keyword matches.

**Required remediation:** Preserve per-leg quotas or over-fetch each leg, then apply the final `top_k` only after scoring and expansion. Add a regression test proving a bounded recent set cannot erase all strong lexical matches.

### [P1] Context assembly can exceed the token budget and break ranked-prefix semantics

**Location:** `src/recall/assemble.rs:193-198`

Assembly counts each block independently but joins accepted blocks with `\n\n`, so separator tokens are never charged. It also continues after a block does not fit, allowing a lower-ranked short block to appear after a skipped higher-ranked block, despite the documented ranked-prefix behavior. Finally, `budget_used + block_tokens` is unchecked arithmetic.

Focused reproductions confirmed both an over-budget rendered context and inclusion of a lower-ranked block after a higher-ranked block was rejected.

**Required remediation:** Measure the complete provisional rendered context with the configured token function, stop at the first block that does not fit, and use checked or subtraction-based budget arithmetic. Add exact-boundary, separator-cost, ranked-prefix, and large-token-count tests.

### [P2] A stale durable vector ID consumes a `top_k` slot

**Location:** `src/recall/assemble.rs:153-160`

Vector members are enumerated and rejected by position before graph membership is checked. During normal write-behind lag, the durable store can still return an ID that has already been deleted from the in-memory graph. With `top_k = 1`, one stale rank-one ID produced zero valid vector hits even though a valid rank-two member was available.

**Required remediation:** Validate graph membership before truncating, or count emitted valid hits rather than raw vector-store positions. Add a stale-rank-one regression test.

### [P2] A missing keyword index discards otherwise valid recall legs

**Location:** `src/daemon/mod.rs:364-370`

When the keyword index is absent, the daemon returns an empty candidate set. This also discards already gathered vector candidates and recent concepts, although only lexical lookup requires the index.

**Required remediation:** Assemble the independent legs with an empty keyword result, or make the keyword leg explicitly optional. Test recall with no index but valid vector and recency candidates.

### [P2] Blast-radius formatting performs repeated full-graph scans under locks

**Location:** `src/recall/format.rs:84-109`

Formatting a canonical hit scans all concepts against all edges, and this work is repeated per canonical hit while the graph read lock and hot-state write lock are held. The graph capacity is elastic and `max_concept_nodes` is advisory, so the cost is not safely bounded by the demo defaults. The effective path can approach `O(hits * concepts * edges)` and extends lock hold time.

**Required remediation:** Maintain adjacency indexes or batch the blast-radius calculation in one graph pass before formatting. Add a scale test that exercises multiple canonical hits on a large graph.

### [P2] Session identity is not validated across recall sources

**Location:** `src/daemon/mod.rs:316-321`

The `session` argument selects the vector-store namespace, while keyword, recent, and expansion candidates come from the daemon's in-memory graph. No equality check prevents a caller from supplying a different session, producing a response that mixes graph A with vector session B.

**Required remediation:** Derive the vector namespace from the graph's authoritative session identity, or reject mismatches before launching recall. Add a cross-session regression test.

## Verification performed

The following checks passed on the P5 branch:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test recall::` - 44 passed
- `cargo test daemon::tests::recall_` - 4 passed
- `cargo test --all -- --skip embed::bge_m3` - 405 passed, 2 ignored, 12 filtered
- Minimal all-target builds for `store-memory`, `store-sqlite`, and `store-cockroach`
- `cargo check --all-targets --features demo`

Tests that require a locally running BGE embedder were intentionally not run. Four temporary adversarial regression tests reproduced the recent-candidate eviction, budget overflow, ranked-prefix, and stale-vector-ID failures; those temporary test edits were not retained.

## Integration and hygiene notes

- A merge-tree check against current `main` reports a conflict in `dev-diary/README.md`. Resolution must preserve both main's P4 live Cockroach verification and P5's 4/4 completion record.
- The pre-existing `dev-diary/adversarial-review/adve-review-p5-recall.md:43` contains trailing whitespace, so the full branch range fails `git diff --check` independently of this review file.

## Verdict

**REQUEST CHANGES.** Fix all four P1 findings and add the specified regression coverage before merge. The P2 findings should also be resolved in P5 because they affect normal write-behind behavior, source isolation, degraded operation, and graph-scale safety rather than optional polish.
