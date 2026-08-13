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
