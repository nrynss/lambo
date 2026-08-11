```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — all findings dispositioned             ║
║  Branch: phase/p2-graph-core                             ║
║  Date:   2026-08-11                                       ║
║  Reviewer: grok (adversarial, branch-level, independent) ║
║  Disposition: G1-G3 FIXED, G4/G5/G6 noted + carried to   ║
║               phase docs, G7 documented                  ║
╚══════════════════════════════════════════════════════════╝
```

## Close record — dispositions

| # | Severity | Finding | Disposition | Evidence |
|---|----------|---------|-------------|----------|
| G1 | Minor | `InvertedIndex::search` double-counts duplicate query terms | **FIXED** — query terms deduped (sort+dedup) before scoring; query-term frequency is 1, document tf unchanged; doc updated | `index.rs` `search`; test `duplicate_query_terms_count_once` (asserts exact score equality `search("user user") == search("user")`) |
| G2 | Minor | BM25 `avgdl` = 0 → NaN when corpus is all zero-token docs | **FIXED** — `avg_dl` guards `total_tokens == 0` (falls back to 1.0); no NaN path | `index.rs` `search` guard; test `zero_token_corpus_stays_empty_and_sane` |
| G3 | Minor | `demote` `chunk_group_id` accepts empty string — collapses the T5.2 sibling boundary | **FIXED** — `chunk_group_id.trim().is_empty()` rejected up front with typed `Invariant`; P5 note added | `demote.rs` step-1 validation; test `demote_rejects_empty_chunk_group_id` (nothing written on rejection) |
| G4 | Suggestion | `insert_concept` UNIQUE scan is O(N) | **Noted** — scaling comment on the check + P3/P4 phase-doc notes (don't benchmark without a key index) | `graph.rs` comment; PHASE-3/PHASE-4 sections |
| G5 | Suggestion | derive pre-pass triple-canonicalizes | **Noted** — correctness holds (pure read-only pass); flagged for P3 profiling; not worth risking the F1/F2-reviewed resolution logic in a polish commit | disposition only |
| G6 | Suggestion | Multi-hop Hierarchical cycles writable via derive | **Noted** — spec-allowed; `assert_invariants` dfs (incl. Hierarchical) remains the safety net; **P4 GC must not assume Hierarchical acyclicity** (carried to PHASE-4); cycle message text updated to name all three types | `graph.rs` cycle message; PHASE-4-daemon.md section |
| G7 | Info | Observation ↔ Entity key sharing intentional | **Documented** — one-line note on `insert_concept` ("Observation keys may shadow entity keys") + P5 disambiguation note (match by `concept_type`, never key uniqueness alone) | `graph.rs` comment; PHASE-5-recall.md section |

**Also done per integrator instruction:** cross-phase obligations written into the
upcoming phase docs — PHASE-3-stores.md (partial UNIQUE DDL, reservations durability,
InvertedIndex owner wiring, reinforcements convention, scaling), PHASE-4-daemon.md
(Hierarchical acyclicity caveat, scaling), PHASE-5-recall.md (concept_type
disambiguation, query-term semantics, non-empty chunk_group_id, CoOccurrence bias).

**Verified at close:** `cargo test` = 196 lib + 2 main + 2 p2_integration + 1 rebuild,
0 failed; `--no-default-features` 142+1; clippy `-D warnings` clean on both; `fmt` clean.
Committed on `phase/p2-graph-core` (no merge to `main` per integrator instruction).

## Original findings

Preserved below verbatim; dispositions recorded above.

---

# Adversarial Review: P2 — Graph Core (branch `phase/p2-graph-core`) — grok

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — all findings dispositioned             ║
║  Branch: phase/p2-graph-core @ (post-disposition)        ║
║  Spec:   lambo-hackathon-spec-v0.1.md (frozen, errata    ║
║          2026-08-11 partial UNIQUE)                      ║
║  Phase:  dev-diary/PHASE-2-graph-core.md (T2.1–T2.7)     ║
║  Date:   2026-08-11                                       ║
║  Reviewer: grok (adversarial, branch-level, independent) ║
║  Prior:  muse-spark CLOSED (M1-M4 FIXED, verified)        ║
║  Verdict: ACCEPT after G1-G3 fixes; G4-G7 noted          ║
╚══════════════════════════════════════════════════════════╝
```

## Grounding

Read spec §§2.1,2.4,4,5.7,6.4,7,8,11 and phase doc P2 (handoff log through `9df5414`). Diff `main...HEAD` = 23 files, 4969 ins (6 graph modules + types + stores + 2 integration tests). Verified gates: `cargo test` 193 lib + 2 p2_integration + 1 rebuild = 196+ total passed, `--no-default-features` 139 passed, `clippy -D warnings` clean, `fmt --check` clean. Prior muse-spark dispositions re-checked and hold; this review does not re-litigate them — it audits what they scoped out and what `9df5414` introduced.

## Re-verified Fixes (not re-opened)

| Claim | Evidence | Still holds? |
|-------|----------|--------------|
| M1/M2 partial UNIQUE | `graph.rs:397-421` `insert_concept` collision guard (non-Observation only) + `graph.rs:907-921` `assert_invariants` scan + `lambo-hackathon-spec-v0.1.md:275-283` errata | **Yes** — tested by `insert_concept_enforces_partial_canonical_key_uniqueness` and `assert_invariants_rejects_duplicate_canonical_keys_on_load` |
| M3 owner contract | `mod.rs:38-47` + `p2_integration.rs:47-97` `inverted_index_manual_sync_contract` | **Yes** — documented, tested |
| M4 validate-then-mutate | `derive.rs:72-78,198-223` pre-pass + defense-in-depth loop checks `306-341` | **Yes** — no partial write possible for reachable errors |
| S1 demote clock | `demote.rs:88-117` `interaction.created_at` stamps | **Yes** — `Utc` import dropped |
| S2-S5 docs | `canonical.rs:88-90`, `derive.rs:262-266`, `reserve.rs:8-13` | **Yes** |

## Findings

| # | Sev | Title | Location | Impact |
|---|-----|-------|----------|--------|
| G1 | **Minor** | `InvertedIndex::search` double-counts duplicate query terms | `index.rs:121-134` | Scoring bias on repeated-term queries |
| G2 | **Minor** | BM25 `avgdl` division becomes `NaN` when all docs are zero-token | `index.rs:118-132` | Harmless today (filtered to empty results) but mathematically unsound |
| G3 | **Minor** | `demote` `chunk_group_id` unchecked (empty string allowed) | `demote.rs:81,125` | Breaks P5 sibling co-retrieval contract silently |
| G4 | Suggestion | `insert_concept` UNIQUE scan is `O(N)` with no index | `graph.rs:404-412` | Acceptable for v0.1 session sizes; note scaling cliff |
| G5 | Suggestion | `derive` pre-pass re-canonicalizes — 3× work per `ParentOf` string | `derive.rs:211-222` + `315-336` | CPU waste, not correctness; fix is trivial cache |
| G6 | Suggestion | `derive` can still create multi-hop `Hierarchical` cycles | `graph.rs:926-934` vs spec §5.7 | Allowed per reduced invariants; broader dfs is only a safety net |
| G7 | Info | Non-Observation ↔ Observation key sharing is intentionally allowed | `graph.rs:403-408` + `907-913` filter | Matches partial index errata; worth making explicit for P5 recall |

### G1 — Duplicate query terms inflate BM25 (Minor)

**Evidence:** `index.rs:114` `let terms = normalize_tokens(query)` retains duplicates (stemmed tokens are not deduped — same as `add` for tf). `index.rs:121` `for term in terms` iterates the vec as-is, and `128-133` adds `idf * tf_component` per iteration. Query `"user user"` therefore scores each matching doc twice for the same term.

Typical BM25 implementations count duplicate query terms once or weight by `qtf`, but spec §8 does not define query-term frequency. Current behavior is deterministic and OR-sum is documented, yet the double-count is untested and will surprise recall tuning. For v0.1 the dedup-free query path keeps `add`/`search` symmetry, but the bias is `k × score` for `k` repetitions.

**Recommendation:** Either dedup query terms (`HashSet` before loop, summing once per unique term) or document that duplicate query tokens intentionally boost (tf-in-query). Add one regression: `search("user user", 10) == search("user", 10)` or `2×` if intentional.

### G2 — Zero-token corpus makes `avgdl` zero (Minor)

**Evidence:** `index.rs:118-119` `let avg_dl = total_tokens as f64 / n`. `index.rs:58-68` `add` increments `total_docs` even when `tokens.is_empty()` (stopword-only / empty content). Session with `N` zero-token concepts ⇒ `total_tokens=0`, `avg_dl=0`. Then `index.rs:132` `dl / avg_dl` ⇒ `0.0/0.0 = NaN`, so `tf_component = NaN`, `scores` entries become `NaN`, filtered by `s > 0.0` → empty results. No panic, no wrong results, just NaN propagation.

Latent because all 193 tests and goldens have non-empty concepts, but a session that `demote`s only stopwords (e.g., `"the and is"`) could hit it. P5 recall expects deterministic scoring.

**Recommendation:** Guard: `let avg_dl = if total_tokens==0 { 1.0 } else { total_tokens as f64 / n }` or `if avg_dl < 1e-9 { avg_dl = 1.0 }`. One test with `Concept { content: "the and is" }` indexed should assert `search("the",10).is_empty()` without NaN.

### G3 — `chunk_group_id` is `&str` with no empty check (Minor)

**Evidence:** `demote.rs:81` `chunk_group_id: &str` stored verbatim `125` `Some(chunk_group_id.to_string())`. No empty guard, no whitespace trim. `mod.rs:38-47` and `demote.rs:14-16` say the id is the T5.2 sibling co-retrieval key (`chunk_group_id` siblings force-included in recall phase 2, spec §8). Empty id ⇒ all single-sentence demotes share `Some("")`, collapsing the co-retrieval boundary; P5 would force-include unrelated Observations.

Spec §7 does not define validation, but the field is load-bearing for recall. Existing tests use non-empty ids, so the hole is uncovered.

**Recommendation:** `if chunk_group_id.trim().is_empty() { return Err(Invariant("chunk_group_id must be non-empty")) }` or at least document "caller must supply non-empty". Add one negative test mirroring `derive`'s `NotFound` validation.

### G4 — `insert_concept` linear scan for UNIQUE (Suggestion)

**Evidence:** `graph.rs:404-412` `self.nodes.iter().find_map` scans all concepts per insertion, filtering on `canonical_key == c.canonical_key && concept_type != Observation`. `canonicalize` itself scans `concepts()` for match (`canonical.rs:120-123`). So `derive` with `k` concepts does `O(k·N)` work. For v0.1 sessions (fixtures ~10-20 concepts) this is trivial; for a long session with 10k concepts it becomes a 10k×10k cliff.

Prior reviews consciously traded the `HashMap<(SessionId, canonical_key), NodeId>` index for simplicity. Acceptable cut — but record it as a scaling note alongside the `mod.rs:44-47` owner contract so P4 GC does not benchmark the wrong thing.

### G5 — Derive pre-pass triple canonicalizes (Suggestion)

**Evidence:** `derive.rs:211-215` pre-pass calls `canonicalize(parent)` and `canonicalize(child)` to get `parent_key`/`child_key`. Later `derive.rs:315-336` `resolve_concept` calls `canonicalize` again on the same strings (and `derive.rs:241-251` does so again for `concepts`). Same input string is normalized + stemmed + sorted 2-3 times.

Correctness holds (pure read-only pass, graph unchanged between passes in `9df5414`), just waste. Fix is to cache `&str → key` in a `HashMap<&str, String>` populated in pre-pass and threaded into `resolve_concept`. Not urgent — flagged for P3 when profiling shows `canonical_key` hot.

### G6 — Multi-hop Hierarchical cycles still writable via `derive` (Suggestion)

**Evidence:** Spec §5.7 reduced invariants list omits Hierarchical cycles; `graph.rs:926-934` `dfs_cycle` deliberately includes `Hierarchical` as broader safety net (adve-review T2.1 I1). `derive` only rejects self-loop `Hierarchical` (`derive.rs:204-222` raw + resolved reflexive), `record_action`'s `closing_edge` explicitly excludes Hierarchical (`action.rs:299-300`). So `derive` can write `A parent_of B` then `B parent_of C` then `C parent_of A` across three calls — graph remains structurally consistent until `assert_invariants` trips `"Causal/Dependency cycle detected through …"` (which now includes Hierarchical despite the message text). Behavior matches spec's "enforced at write time by BFS" scope (Causal/Dependency only) but diverges from the module doc's "Hierarchical is a DAG constraint by definition" (`graph.rs:1006-1010`).

No fix required for v0.1; call out that daemon P4 need not assume Hierarchical acyclicity from write-time checks alone — keep the `assert_invariants` safety net.

### G7 — Observation ↔ Entity key sharing is intentional (Info)

**Evidence:** `graph.rs:403` `if c.concept_type != Observation` gates the collision check; `907-913` filter does same for invariants. Result: an Entity `"user schema"` (`"schema user"`) and an Observation `"user schema"` can coexist sharing the key. The DDL errata `lambo-hackathon-spec-v0.1.md:275-283` says partial unique constrains non-Observations only — so DDL and RAM are aligned. Informational because P5 recall that matches by `canonical_key` will find both; scoring must disambiguate by `concept_type` modifier (spec §5) rather than key uniqueness. Worth one line in `graph.rs:397-402` doc: "Observation keys may shadow entity keys."

---

## Coverage Check — What This Review Probed That Prior Didn't

- Tokenizer fork removal verified: `index.rs:18` imports `canonical::normalize_tokens` (not forked) — matches T2.6 handoff.
- Lock discipline re-checked: `Graph` is `&mut self` only, zero `async`/`await` in module (grep `.await` → none), `parking_lot` usage confined to future `Memory` (outside P2) — spec §6.4 holds.
- Mutation log ordering: `derive` re-upsert via `insert_concept` preserves node-before-edge (`graph.rs:422-438`); `demote` per-sentence `insert_concept` same; `action` `resolve` then `insert_concept` loop then `upsert_edge` loop — chronological `drain_log` contract (`mod.rs:13-28`) holds, proven by `p2_integration.rs:102-193` chronological check.
- Reserve durability nuance scout noted is already documented at `reserve.rs:10-13` (snapshot-only, log never carries reservations) — not re-filed.
- No `TODO`/`todo!`/`unimplemented!` left; `unwrap()` confined to tests/helpers (`graph.rs:700-711` `unwrap_or_default` on option maps is safe).

## Gate

`cargo test` — 193 lib + 2 p2_integration + 1 rebuild = 196 passed, 0 failed, 1 ignored; `--no-default-features` 139 passed; `clippy -D warnings` both profiles clean; `fmt` clean (re-checked 2026-08-11). Prior `muse-spark` gate re-verified — no regression.

## Recommendation

Keep `STATUS: ACCEPT WITH MINOR FINDINGS`. File the three Minors (G1-G3) as P3/P5 tickets or fix in a one-commit polish; G4-G7 are note-and-keep. No branch blocker — `phase/p2-graph-core` is merge-ready from a P2 correctness standpoint once the ticket trail exists.

## Checklist (for integrator)

- [ ] G1 dedup-or-document duplicate query terms (add one regression)
- [ ] G2 `avgdl` zero guard + stopword-only test
- [ ] G3 `chunk_group_id` non-empty validation (or doc + test)
- [ ] G4-G7 acknowledged as suggestions/info (no code required pre-merge)
