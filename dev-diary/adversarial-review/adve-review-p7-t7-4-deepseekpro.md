# Adversarial review — T7.4 camera-proof remediation

- **Date:** 2026-08-13
- **Reviewer:** deepseekpro
- **Scope:** T7.4 — §12.1 vector-index camera-proof remediation (commits
  `073b43d`..`2b28240` on `task/p7-t7.4-camera-proof`): partial vector index in
  `migrations/cockroach/001_init.sql`, `provision.sh` index-state reconciliation,
  `vector_explain_camera_proof` test rewrite, and the opt-in ANN accuracy dial
  (`LAMBO_VECTOR_BEAM_SIZE`) in `src/store/cockroach.rs`.
- **Verdict:** ACCEPT with findings — no BLOCKER, 1 MAJOR (decision/measurement
  gap), 5 MINOR, 2 NIT.

---

## R1 remediation — 2026-08-13 — ALL FINDINGS ADDRESSED

Every finding was verified against the code before being actioned; none was
taken on faith, and none was dismissed. Status per finding:

| # | Severity | Status | Resolution |
|---|---|---|---|
| MAJOR-1 | MAJOR | **FIXED (measured)** | Recall measured on the live cluster; `DEFAULT_VECTOR_BEAM_SIZE = 64` chosen from data. Evidence: `evidence/20260813-145209-ann-recall-vs-beam.txt` |
| MINOR-2 | MINOR | **FIXED** | Migration header now states provision.sh auto-reconciles; manual drop retained only for the apply-by-hand path |
| MINOR-3 | MINOR | **FIXED** | `vector_index_state()` canonicalizes (lowercase + whitespace-collapse) and bounds the predicate window |
| MINOR-4 | MINOR | **FIXED (stronger)** | The proof now EXPLAINs the `VECTOR_CANDIDATES_SQL` constant itself, so the claim is true by construction |
| MINOR-5 | MINOR | **FIXED** | Stale "PENDING" comment replaced with the actual T7.4 root cause |
| NIT-6 | NIT | **ACCEPTED, not code** | Recorded as a T8.4 narration constraint — see below |
| NIT-7 | NIT | **FIXED** | provision.sh asserts the splitter routed a vector index when the migration declares one |

### MAJOR-1 — measured, not deferred

The reviewer correctly identified that the exact→approximate change was
unmeasured and that `SESSION_VECTOR_CANDIDATES_SQL` does **not** cover an
ANN-miss (it triggers on boundary-tie/crowd-out only). Both points confirmed.

Measurement run: exact ground truth forced via the `concepts@concepts_pkey`
hint (FULL SCAN, EXPLAIN-verified), compared against the production query shape
served by the partial index, 25 probes, two 3,000-row datasets — uniform-random
and clustered unit-norm (real-embedding geometry).

```
beam:       1     2     4     8    16    32*    64    128    256
recall@10  .19   .23   .32   .47   .70   .93    .96   .96    .86
recall@50  .07   .13   .22   .40   .64   .94    .99   .99    .97
                                        *CockroachDB default
```

1. At the server default (32), **~6-7% of true nearest neighbours are missed** —
   i.e. near-duplicates hybrid matching silently fails to merge.
2. **Higher is not monotonically better**: beam 256 was WORSE than 64 in *both*
   datasets, reproducibly. This is why the default is measured rather than
   maximised, and it invalidates "just turn it up" as guidance.
3. **Recall never reached 1.000 at any beam.** The index is approximate by
   construction; exactness is only available by giving up index use, which
   spec §12.1 requires us to demonstrate. So the residual approximation is
   **accepted for v0.1** — the reviewer's third option — but now with a number
   attached instead of a shrug.

Default raised from "unset (=32)" to **64**: recall@50 0.938 → 0.990 at a scale
where the extra work is negligible. Caveat kept in the code and the evidence:
both datasets are synthetic, so 64 is evidence-based, not a tuned optimum.

### NIT-6 — accepted as a narration constraint, not fixed in code

Correct and worth keeping. The PASSING plan reads `distribution: local`, so the
capture demonstrates *vector indexing*, not the *distributed* half of §12.1's
"Distributed Vector Indexing". Nothing in T7.4's scope can change that — it is a
property of how the demo query is planned. **Carried to T8.4/P9:** either say
"CockroachDB vector indexing" when narrating this plan, or capture a
distributed plan separately. Do not let the video claim more than the artifact
shows.

### Reviewer accuracy note

All seven findings were real. MINOR-4 was the sharpest: the "byte-for-byte"
comment was false (the test dropped `::STRING` casts). It was cosmetic in
effect — casts cannot change index selection — but the entire value of a camera
proof is that it explains the query production runs, so it was fixed by
construction rather than by rewording.

**Verification after remediation:** live suite 6/6 including
`vector_explain_camera_proof` on an unseeded, migration-only cluster;
`cargo test` 528 lib + 5 integration + 1 doc; fmt clean; clippy clean under
`store-cockroach,store-memory,fixtures`; provision.sh re-exercised. Both seed
sessions removed — the cluster is back to demo-shaped data (1008 concepts).

## Verified independently

- `cargo check` — clean.
- `cargo test --lib` — 528 passed / 0 failed / 1 ignored (live BGE smoke).
- `vector_search_beam_size` is a **real** CockroachDB session variable
  (docs: default 32, query-time; C-SPANN ANN). The ANN dial is legitimate, not
  fabricated.
- The committed PASSING evidence is honest: plain `EXPLAIN` plan shows
  `vector search` on `concepts@concepts_embedding_idx (partial index)`, no
  `FULL SCAN`, with the operator-spelling table (spaced vs hyphenated).
- The provision.sh splitter correctly ignores `;` inside `--` comments
  (`in_comment` resets per line, L196-223); the migration header's literal
  `DROP INDEX …;` inside a comment does not tear a statement.

---

## MAJOR-1 — Exact → approximate semantic change is real, unmeasured, and the exact fallback does not cover it

**File/construct:** `src/store/cockroach.rs` `vector_candidates` + ANN dial doc (L563-591).

Before T7.4, `vector_candidates` planned a FULL SCAN = **exact** top-k. After the
partial index it plans `vector search` = **approximate** ANN (C-SPANN), so a true
near-neighbour in an unvisited neighbourhood can be silently missed. The code
acknowledges this ("a true near neighbour … can be missed … silent quality loss …
hybrid matching fails to merge a genuine near-duplicate") and defers the recall
measurement: *"that measurement is a T7.4-review item"* — which is this review.

The correctness fallback `SESSION_VECTOR_CANDIDATES_SQL` (L179-185, from `67b3f67`,
not this task) triggers only on **boundary-tie / crowd-out** (`needs_session_fallback`,
L1739), **not** on ANN-miss — a neighbour the beam never visited is simply absent,
with no fallback. A genuine `>=0.85` near-duplicate can therefore be missed and
re-created as a new concept, silently.

- For the **demo** (T8.4: ~12 interactions, few concepts, 4 distinct vectors) ANN ==
  exact — not demo-blocking.
- For any **claim** of recall quality at scale: no measurement exists to justify the
  unset default beam size 32.

**Fix / decision:** measure recall vs beam on a dense table, or extend the exact
fallback to a recall-confidence path, or state the approximation as accepted for v0.1.

## MINOR-2 — Migration header contradicts provision.sh (stale manual-step doc)

**File/construct:** `migrations/cockroach/001_init.sql` L94-105.

The header says upgrading a pre-T7.4 cluster is a "one-time manual step"
(`DROP INDEX …; ./scripts/provision.sh`), but `d335130` made `provision.sh` **auto-drop**
the legacy index (`vector_index_state` -> `legacy` -> `DROP`, L242-247). Harmless if
followed literally, but it misleads operators into a redundant manual drop.

**Fix:** update the header to say provision.sh auto-reconciles legacy -> partial.

## MINOR-3 — `vector_index_state()` is a fragile line-match

**File/construct:** `scripts/provision.sh` L90-104.

`grep -i 'concepts_embedding_idx'` (case-insensitive) then `[[ == *"WHERE embedding IS
NOT NULL"* ]]` (**case-sensitive**), assuming the `WHERE` clause lands on the **same
line** as the index name. Works today (verified "vector index verified PARTIAL"), but if
CRDB ever wraps the predicate onto a second line — or changes casing — a correctly-partial
index is misclassified `legacy`, triggering a destructive drop + ~85-96s rebuild on every
provision run.

**Fix:** match on a canonicalized (lowercased, whitespace-collapsed) `SHOW CREATE TABLE`,
or assert the exact `vector_l2_ops) WHERE embedding IS NOT NULL` shape the evidence pins.

## MINOR-4 — Camera-proof "byte-for-byte" overclaim

**File/construct:** `src/store/cockroach.rs` L4025-4027 vs `VECTOR_CANDIDATES_SQL` L167-174.

The comment claims the EXPLAINed query is "byte-for-byte" `VECTOR_CANDIDATES_SQL`, but the
test drops the `id::STRING AS id, session_id::STRING AS session_id` casts (bare `id,
session_id` in the test). Substance unaffected (output casts cannot change index choice),
but the comment's whole value is "this is the exact query production runs".

**Fix:** comment to "shape-identical modulo output casts", or use the verbatim constant.

## MINOR-5 — Stale comment on `check_vector_explain_is_global_topk`

**File/construct:** `src/store/cockroach.rs` L2957-2958.

Still reads "PENDING where the optimizer scans a small table" — false post-T7.4 (the plan
is now `vector search` on the partial index; table size was never the cause). Already
self-flagged in the handoff as out-of-`owns`; one-line fix.

## NIT-6 — `distribution: local` vs "Distributed Vector Indexing"

**File/construct:** `dev-diary/evidence/20260813-134333-vector-index-camera-proof-PASSING.txt`.

The PASSING plan is `distribution: local` (single-node) with ~883 rows. The index is used
(`vector search`), but nothing in the capture demonstrates the *distributed* half of the
§12.1 "Distributed Vector Indexing" claim. Demo-narration nuance for T8.4, not a defect.

## NIT-7 — Final verification gate is silently skipped when `VINDEX_SQL` is empty

**File/construct:** `scripts/provision.sh` L271.

The fail-loud gate is wrapped in `if [[ -s "$VINDEX_SQL" ]]`. If a future migration reformat
ever causes the splitter to route the vector index elsewhere, provision would skip **both**
the index create and the verification — the exact silent full-scan it is meant to prevent.
The splitter's fail-fast (dollar-quote/block-comment, L128) lowers probability, but the gate
itself has a silent-skip hole.

**Fix:** fail if the migration contains a `CREATE VECTOR INDEX` statement but `VINDEX_SQL`
came out empty (assert the routing produced a non-empty vector file).
