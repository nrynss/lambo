# Adversarial review: mooshik B, phase B3 (Postgres dialect ranking), round 1

**Reviewer**: independent adversarial reviewer, agent_id `B3Review1`. Wrote
nothing under review except this file. No commit, no push.
**Scope**: the uncommitted B3 implementation on branch `b0-pg-extraction`
against `B-postgres-store.md` B3 and H-cross-store-parity.md H3, plus the
implementer's claims in `b-run/B3-implementation.md`.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `97ee28c`
(B2 closed, round 2 APPROVE). Dirty tree at review start is the B3
implementation, including sqlite.rs H1 `build_adapters` and
`evidence/mooshik-h3-postgres-parity/`. Ignored: `local:/` and the
orchestrator briefs.
**Verdict**: **REQUEST_CHANGES**. 0 P1 / 1 P2 / 0 P3.

Reviewed-state SHA-256, taken before any mutation and re-verified after every
mutation cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/mod.rs` | `f96df098de9e2a236bb6fee4b9401d481467f643303961213605a04344a782a5` |
| `src/store/pg/postgres.rs` | `8d212086e9c3998b4bf86230b693aa8230e5cd454db3c6354ca925e222da97f9` |
| `src/store/pg/mod.rs` | `439ee6e2d057517cbcab0e839f047c4b43fa2d32927fab79737b6c5064cfb202` |
| `src/store/pg/dialect.rs` | `01ba315130729cd89b0063e0f6ca1f86383e8f4ab4c6cfe5a9153a98772cedef` |
| `src/store/pg/cockroach.rs` | `d8333564fc1a895cc106b0647f4289f5c14dd61049ef76c52e9b0481cff3e135` |
| `src/store/sqlite.rs` | `b040b47cb13fab6228bee2cebb354a21e180ab064b7c70ad5a3051e8dae03dda` |
| `.github/workflows/ci.yml` | `d70e767d8a8d491e8dd48b41ecc260eb31122bedd9d3740753312ddcdbd18fe1` |
| `CHANGELOG.md` | `214d5a3a3b6d76acba2e9d63240df1bee9bdfaba8f51608d1735e0aa4abd0408` |

`src/store/mod.rs` is byte-identical to the B2 round-2 reviewed state. Copied
the table to `/tmp/b3-r1-review-snapshot/` before any mutation. Every mutation
cycle ends with a restore and a `sha256sum` comparison against the table
above.

## Method

1. Recalled dogfood memory (`B3Review1`). Read `B3-round1-review-brief.md`,
   `B3-implementation.md`, `B-postgres-store.md` B3, H-cross-store-parity.md
   (H3 is B3's parity box), `CYCLE.md`, and house style
   `adve-review-mooshik-B-B2-round2.md`.
2. Mutation-tested every claimed pin. A pin holds only if the cited test
   FAILS under the mutation.
3. Independently re-ran every CYCLE gate on the restored tree, plus the
   `store-postgres` compile/unit row and the extra `store-postgres` clippy.
   Live Postgres: started, used, and removed only the pinned
   `pgvector/pgvector:pg17` digest
   `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
   Live Cockroach tests: not run. `.env` and `models/`: not touched.
4. Hunted for: silent mis-rank (Cockroach formula on Postgres), H1
   behaviour change, a second H3 harness, B4 accidentally claimed, SQLite
   restamp quarantine inherited, leftover `:latest` on the pgvector image,
   leftover mutation, em dashes in B3-authored added lines, B0 composed-SQL
   pin broken.

Unmutated pins were green before the first mutation
(`--no-default-features --features store-postgres` for the new dialect tests;
`--features store-cockroach` for the Cockroach formula and B0 pins;
`--features store-sqlite,store-cockroach,fixtures` for H1). After restore,
incremental compile was forced with `touch` of the restored files.

## Part A: claimed pins, mutation-tested

| # | Claim | Cited test | Mutation | Result |
| --- | --- | --- | --- | --- |
| M1 | Postgres score is `1 - d`, not Cockroach `1 - d^2/2` | `distance_to_score_is_one_minus_d` | body `(1.0 - dist).clamp` to `(1.0 - 0.5 * dist * dist).clamp` | **RED** at `postgres.rs:243`: left `0.5`, right `0.0` (`d=1`) |
| M2 | Cockroach score stays `1 - d^2/2` | `distance_to_score_is_cosine` | body `(1.0 - 0.5 * dist * dist).clamp` to `(1.0 - dist).clamp` | **RED** at `cockroach.rs:448`: `(C::distance_to_score(1.0) - 0.5).abs() < 1e-12` |
| M3 | Composed recall SQL uses `<=>`, rejects `<->` | `recall_sql_pairs_cosine_operator_with_text_and_vector_casts` (also `dialect_tokens_are_not_cockroach_sql`, `distance_to_score_is_one_minus_d`) | `DISTANCE_OP` `"<=>"` to `"<->"` | **RED** at `postgres.rs:272`: `embedding <-> $1::vector`; also `:217` and `:260` |
| M4 | Composed SQL uses `::TEXT`, rejects `::STRING` | `recall_sql_pairs_...` and `dialect_tokens_are_not_cockroach_sql` | `STRING_CAST` `"::TEXT"` to `"::STRING"` | **RED** at `postgres.rs:288`: `id::STRING`; also `:210` |
| M5 | Composed SQL uses `::vector`, rejects `::VECTOR` | same | `VECTOR_CAST` `"::vector"` to `"::VECTOR"` | **RED** at `postgres.rs:280`: `$1::VECTOR`; also `:216` |
| M7 | Forced-exact GUC spelling | `distance_to_score_is_one_minus_d` | `forced_exact_scan_sql` returns `None` | **RED** at `postgres.rs:261`: left `None`, right `Some("SET LOCAL enable_indexscan = off")` |
| M6 | Forced-exact `SET LOCAL` is issued in the search transaction | claimed: `explain_recall_uses_hnsw`, `h3_postgres_recall_parity`, `with_forced_exact_scan_is_off_by_default` | delete the `if self.force_exact_scan { sqlx::query(sql).execute(...) }` block at `pg/mod.rs:2737-2741` | **GREEN**. All 12 `postgres::tests` unit tests pass. Live `explain_recall_uses_hnsw` **passes** (it injects the GUC as `extra_set`, never reads the flag). Live `h3_postgres_recall_parity` **passes** (0.95s, 240 pairs, envelope still zero). |

M1 through M5 and M7 are not vacuous. M1 fails at `d=1` (`0.5` vs `0.0`), which is the
copied-Cockroach point the test names. M2 fails the existing `d=1 -> 0.5`
assert, not only the new `assert_ne`. M3's composed-SQL dump shows the
production `vector_candidates` text, not a stub. M6 is the residue.

Mutation score: **6/6 claimed conversion and token pins were caught. 0/1 on
the forced-exact issuance claim.**

## Part B: hunt for defects introduced by B3

### B3-R1-1 (P2) forced-exact SET LOCAL is not issued on the camera-proof path

`PgStore::vector_candidates_checked` (`pg/mod.rs:2737-2741`) does issue
`Dialect::forced_exact_scan_sql` after the contract read when
`force_exact_scan` is set. Postgres returns
`SET LOCAL enable_indexscan = off`. That GUC, when actually run, disables
the hnsw index: my live EXPLAIN of the production `vector_candidates` SQL
with that SET in the same transaction is `Seq Scan` + `Sort`, no
`concepts_embedding_idx`. Natural planner choice on this digest is
`Index Scan using concepts_embedding_idx` / `Order By: (embedding <=> $1::vector)`.

The camera-proof does not go through that path.

`explain_vector_candidates` (`postgres.rs:558-581`) never reads
`store.forced_exact_scan()`. It applies an optional `extra_set` string.
`explain_recall_uses_hnsw` (`postgres.rs:608-610`) constructs
`store.with_forced_exact_scan()` and then passes
`Some("SET LOCAL enable_indexscan = off")` as `extra_set`, so the flag is
dead on the EXPLAIN helper. `with_forced_exact_scan_is_off_by_default`
(`postgres.rs:439-446`) only asserts the bool. `distance_to_score_is_one_minus_d`
pins the GUC *string* (`postgres.rs:261-264`), which is why M7 is red and M6
is not.

H3's `postgres-exact` lane does call `with_forced_exact_scan`
(`sqlite.rs:5763-5765`). H3 does **not** EXPLAIN that lane. `index_present`
is a constructor bool (`sqlite.rs:5767-5777`: hnsw `true`, exact `false`),
not a probe of the plan that served the answer. At this corpus (9 and 22
vectors) hnsw and seq scan agree bit-for-bit, so deleting the execute
leaves H3 green: both Postgres lanes use the index, scores still match
sqlite within `1.21e-7`, and the report still claims `index_present: false`
on `postgres-exact`.

What holds without this pin: the conversion formula (M1/M2) and the
composed-SQL tokens (M3-M5). postgres-vs-sqlite score agreement would still
catch a copied Cockroach formula, because both Postgres lanes share
`distance_to_score`. What does not hold: the claim that approximation on
this dialect can only come from the index, never from the dialect SQL, and
the claim that H3's Exact lane is a forced seq scan. At a larger graph the
unissued GUC would make `postgres-exact` silently ANN.

**Closure.** `explain_vector_candidates` must apply
`D::forced_exact_scan_sql()` when `store.forced_exact_scan()` is true, and
the live EXPLAIN of the exact lane must not pass the GUC as `extra_set`.
Deleting `pg/mod.rs:2737-2741` must turn `explain_recall_uses_hnsw` red
(forced-exact plan still names `concepts_embedding_idx`). Optional but
cleaner: H3 EXPLAIN the exact store the same way, or probe `index_present`
from the plan instead of hardcoding it.

### Attack vectors examined, not elevated

- **Silent mis-rank of the conversion itself.** Ruled out. M1 and M2 go red
  at `d=1` and `d=0.5`. H3's `H3_SCORE_SKEW_EPSILON = 1e-4` is orders of
  magnitude below the `~0.375` skew a copied `1 - d^2/2` produces at cosine
  0.5. Production body is `(1.0 - dist).clamp(-1.0, 1.0)`
  (`postgres.rs:96-98`). Cockroach body is unchanged
  (`cockroach.rs:162-164`); git diff is the vice-versa `assert_ne` only.
- **H1 behaviour change.** `h1_sqlite_and_memory_oracle_agree_exactly`
  passed under `store-sqlite,store-cockroach,fixtures` (0.07s). H1 still
  calls `run_synthetic_leg(..., None)` (40 pairs, sqlite vs memory-oracle,
  bit-for-bit). All sqlite.rs hunks start at line 5361 or later, inside
  `mod h1_cross_store_parity`. Production SQLite scan/score/quarantine is
  untouched (`git diff` has no `vector_candidates` / restamp hits outside
  that module).
- **Second harness.** H3 is `h3_postgres_recall_parity` in the H1 module.
  `build_adapters` appends postgres-hnsw / postgres-exact when a DSN is
  offered. Pairwise loop, measures, and `ParityReport` v1 are the H1
  types. No new `mod h3`.
- **B4 claimed.** `GraphStore::vector_dimensions` still echoes construction
  dim (`pg/mod.rs:2266-2268`). postgres.rs module doc and `vector_dim` doc
  name B4 as not closed. CHANGELOG ranking note does not mention live-schema
  reporting.
- **SQLite restamp quarantine inherited.** No restamp/NULL-on-width-change
  in the pg diffs. Live `fencing_refuses_stale_write_and_upserts_replay`
  stamps a contract, probes a concept with `embedding: None`, and asserts
  empty hits (NULL-only).
- **`:latest` on the pgvector image.** `.github/workflows/ci.yml:278` is
  `pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
  Runner `ubuntu-latest` is the pre-existing GHA runner label. My live
  container was that digest (`docker inspect Image=` the same sha256), then
  removed.
- **B0 composed-SQL pin.**
  `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` passed on
  the restored tree.
- **Em dashes in B3-authored added lines.** `git diff -U0` of every
  B3-touched tracked file: 786 added lines, zero U+2014.
  `B3-implementation.md` and the H3 evidence README: zero. Pre-existing
  em dashes in `pg/mod.rs` / `cockroach.rs` / `sqlite.rs` / `ci.yml` were
  not introduced here.
- **CYCLE merge-once.** Intact: B stays on `b0-pg-extraction` until B0
  through B4 are all closed; one merge to `lambo-for-mooshik` at the end of
  B. CYCLE status line still says "B3 is next"; that is orchestrator-owned,
  not a B3 defect.
- **Leftover mutation.** After restore, every SHA-256 in the header matches.
  Postgres formula is `1.0 - dist`. The M6 execute block is present.
- **EXPLAIN `hnsw` name OR.** `explain_recall_uses_hnsw` at
  `postgres.rs:602-606` ORs `contains("hnsw")` with
  `contains("concepts_embedding_idx")` after already requiring the index
  name. Vacuous as a *name* pin; the index-name assert and the live plan
  (`Index Scan using concepts_embedding_idx`) are enough. Not elevated.
- **H3 evidence `store-memory` feature.** The committed report lists
  `store-memory` (a default feature). CI H3 is
  `--no-default-features --features store-postgres,store-sqlite,fixtures`.
  MemoryOracleStore uses the always-compiled `store::memory::MemoryStore`.
  Not a compile hole, not elevated.

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B3-implementation.md`. Live Cockroach tests: not run. `.env` and
`models/`: not touched. Live Postgres: pinned digest only, container
removed at the end.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **940 passed / 0 failed / 4 ignored**, **944 listed**. Lib: 928 passed / 2 ignored |
| `cargo test --no-default-features --features store-cockroach` | 0 | **603 passed / 0 failed / 0 ignored**, **603 listed**. Lib: 594 passed |
| `cargo test --features store-cockroach,fixtures` | 0 | **1000 passed / 0 failed / 12 ignored**, **1012 listed**. Lib: 987 passed / 10 ignored |
| `cargo test --no-default-features --features store-postgres` | 0 | **588 passed / 0 failed / 3 ignored**, **591 listed**. Lib: 579 passed / 3 ignored (`init_schema_at_two_widths_creates_hnsw`, `explain_recall_uses_hnsw`, `fencing_refuses_stale_write_and_upserts_replay`) |
| B0 composed-SQL pin `--lib` `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` | 0 | **passed** |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed**. sqlite.rs diff is H1/H3 harness only |
| Live EXPLAIN + fencing on pinned digest | 0 | **passed**. Natural plan: `Index Scan using concepts_embedding_idx` / `embedding <=> …::vector`. Forced-exact GUC (via `extra_set`): `Seq Scan` + `Sort`. Fencing: `StaleWrite`, upsert replay, `created_at: Some(_)`, NULL-only empty hits |
| H3 live `--lib` `h3_postgres_recall_parity` under `store-postgres,store-sqlite,fixtures` | 0 | **passed** (0.93s). 240 pairs, 4 adapters. Digest `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f` |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines (B0/B1 counting method). **none** naming `PostgresDialect` / `distance_to_score` / `store/pg` |

No gate finding. Implementer's measured CYCLE counts match mine.

## Summary of findings

| ID | Grade | Status |
| --- | --- | --- |
| B3-R1-1 | P2 | **OPEN**. Forced-exact GUC string is pinned (M7 red). Forced-exact issuance in `vector_candidates_checked` is not (M6 green on unit tests, live EXPLAIN, and H3 at fixture size). |

**0 P1, 1 P2, 0 P3 residue.** The dangerous conversion row is real and
mutation-proven: copying either formula onto the other dialect goes red, and
composed SQL rejects the other dialect's tokens. H3 extends H1
`build_adapters` rather than inventing a second harness. H1 still exact-agrees.
B4 is not closed. SQLite restamp quarantine was not inherited. The pgvector
image is digest-pinned. What does not close B3 is the Exact lane: H3 and the
EXPLAIN helper can stay green while the search transaction never runs
`SET LOCAL enable_indexscan = off`.

**Tree state at close**: restored to the reviewed state and verified. All
SHA-256 values in the header match, and `git status --short` is identical
to session start except for this file. Nothing was committed or pushed.

B3Review1, 2026-08-23

---

## Closures (round-1 remediation, 2026-08-23)

Remediator: `b3-remediator`. Work on `b0-pg-extraction`, tree left dirty, not
committed. This section records what closed; it does **not** change the
**REQUEST_CHANGES** verdict or the Findings table above.

| ID | Grade | Status | What |
| --- | --- | --- | --- |
| B3-R1-1 | P2 | **closed** | `explain_vector_candidates` (`postgres.rs:596`) issues `D::forced_exact_scan_sql` when `store.forced_exact_scan()` is set, via `PgStore::issue_forced_exact_scan` (`mod.rs:1265`, execute at `mod.rs:1269-1272`). `vector_candidates_checked` calls the same helper after the contract read (`mod.rs:2754`). Exact-lane EXPLAIN is `explain_vector_candidates(&exact, None)` (`postgres.rs:654-655`); it does not pass the GUC as `extra_set`. Deleting the execute (`M-guc`) turns `explain_recall_uses_hnsw` **RED** at `postgres.rs:657` (forced-exact plan still names `concepts_embedding_idx`). Source pin `explain_vector_candidates_uses_store_forced_exact_scan` (`postgres.rs:452`). Conversion pins not edited. Reverted. |

Gates re-run by the remediator: fmt, three clippy `-D warnings` rows (default,
store-cockroach, store-postgres), the four test commands (944 / 603 / 1012 /
592 listed; store-postgres +1 for the source pin), H1 lock green with no
sqlite.rs remediator hunk, live EXPLAIN + fencing on the pinned digest, and
`cargo doc --document-private-items` at 54 warnings. Detail in
`dev-diary/lambo-for-mooshik/b-run/B3-remediation.md`.
