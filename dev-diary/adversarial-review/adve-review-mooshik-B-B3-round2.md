# Adversarial review: mooshik B, phase B3 (Postgres dialect ranking), round 2

**Reviewer**: independent adversarial reviewer, agent_id `B3Review2`. Wrote
nothing under review except this file. No commit, no push.
**Scope**: the uncommitted B3 round-1 remediation on branch `b0-pg-extraction`
against `adve-review-mooshik-B-B3-round1.md` (0 P1 / 1 P2 / 0 P3,
REQUEST_CHANGES) and the remediator's claims in `b-run/B3-remediation.md`.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `97ee28c`
(B2 closed, round 2 APPROVE). Dirty tree at review start is the B3
implementation plus the round-1 review, its closures appendix, and the
round-1 remediation. Ignored: `local:/` and the orchestrator briefs.
**Verdict**: **APPROVE**. The single closure holds. Zero failed closures.
Zero new findings.

Reviewed-state SHA-256, taken before any mutation and re-verified after every
mutation cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/mod.rs` | `f96df098de9e2a236bb6fee4b9401d481467f643303961213605a04344a782a5` |
| `src/store/pg/postgres.rs` | `61cea28713ca928ef01a893a318917bfa6fd4f7e3fdfc527c30eb78484f414ed` |
| `src/store/pg/mod.rs` | `2a35af77c041b42076cb8a63200f8dbb91c50df1a9aa2aa0d243612931c51369` |
| `src/store/pg/dialect.rs` | `01ba315130729cd89b0063e0f6ca1f86383e8f4ab4c6cfe5a9153a98772cedef` |
| `src/store/pg/cockroach.rs` | `d8333564fc1a895cc106b0647f4289f5c14dd61049ef76c52e9b0481cff3e135` |
| `src/store/sqlite.rs` | `b040b47cb13fab6228bee2cebb354a21e180ab064b7c70ad5a3051e8dae03dda` |
| `.github/workflows/ci.yml` | `d70e767d8a8d491e8dd48b41ecc260eb31122bedd9d3740753312ddcdbd18fe1` |
| `CHANGELOG.md` | `214d5a3a3b6d76acba2e9d63240df1bee9bdfaba8f51608d1735e0aa4abd0408` |

`mod.rs` (store), `dialect.rs`, `cockroach.rs`, `sqlite.rs`, `ci.yml`, and
`CHANGELOG.md` are byte-identical to the round-1 reviewed state. `postgres.rs`
and `pg/mod.rs` differ by the R1-1 helper, the exact-lane `extra_set = None`
camera-proof, and the source pin those close.

Copied those files to `/tmp/b3-r2-review-snapshot/` before any mutation.
Every mutation cycle ends with a restore and a `sha256sum` comparison against
the table above.

## Method

1. Recalled dogfood memory (`B3Review2`). Read `B3-round2-review-brief.md`,
   `B3-remediation.md`, the round-1 review plus its closures appendix,
   `B-postgres-store.md` B3, `b-run/CYCLE.md`, and house style
   `adve-review-mooshik-B-B2-round2.md`.
2. Mutation-tested the R1-1 closure. A closure holds only if the cited test
   FAILS under the mutation. Conversion pins M1/M2 were re-mutated to prove
   they were not weakened. Source-pin wiring was mutated separately.
3. Independently re-ran every CYCLE gate on the restored tree, plus the
   `store-postgres` compile/unit row and the extra `store-postgres` clippy.
   Live Postgres: started, used, and removed only the pinned
   `pgvector/pgvector:pg17` digest
   `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
   Live Cockroach tests: not run. `.env` and `models/`: not touched.
4. Hunted for: vacuous pin, H1 regression, a second H3 harness, conversion
   pin weakening, leftover `:latest` on the pgvector image, leftover
   mutation, em dashes in remediator-authored lines, CYCLE merge-once
   wording dropped, B0/B1/B2 reopened, B4 accidentally claimed.

Unmutated pins were green before the first mutation
(`--no-default-features --features store-postgres` for the dialect tests and
live EXPLAIN; `--features store-cockroach` for the Cockroach formula and B0
pins; `--features store-sqlite,store-cockroach,fixtures` for H1). After
restore, incremental compile was forced with `touch` of the restored files.

## Part A: per-finding closure verification

| Finding | Verdict | Verification |
| --- | --- | --- |
| B3-R1-1 (P2) forced-exact issuance | **HOLDS** | `explain_vector_candidates` (`postgres.rs:608-611`) calls `store.issue_forced_exact_scan(&mut tx)`. Production search calls the same helper after the contract read (`mod.rs:2754`). Exact-lane EXPLAIN is `explain_vector_candidates(&exact, None)` (`postgres.rs:654-655`); it does not pass `SET LOCAL enable_indexscan = off` as `extra_set`. `extra_set` remains only the inverse GUC (`enable_seqscan = off`). Deleting the SET LOCAL execute inside `issue_forced_exact_scan` (`mod.rs:1269-1272`) turns live `explain_recall_uses_hnsw` **RED** at `postgres.rs:657` (forced-exact plan still names `concepts_embedding_idx`). |

Unmutated live EXPLAIN on the pinned digest, before the first mutation:

* planner's choice / `enable_seqscan = off`: `Index Scan using
  concepts_embedding_idx` / `Order By: (embedding <=> '[0,0,0,0,0,0,0,0]'::vector)`
* forced-exact (`with_forced_exact_scan()`, `extra_set = None`): `Seq Scan`
  + `Sort`, no `concepts_embedding_idx`

### B3-R1-1 mutations (restored after each)

Each cycle: mutate, run the cited test, restore from snapshot, `sha256sum`
check, `touch` so cargo cannot reuse a mutated artifact. Live rows use
`LAMBO_REQUIRE_LIVE=1` against the pinned digest only.

| # | Closure | Mutation | Cited test | Result |
| --- | --- | --- | --- | --- |
| M-guc | R1-1 | delete the SET LOCAL execute inside `issue_forced_exact_scan` (`mod.rs:1269-1272`) | `explain_recall_uses_hnsw` | **RED** at `postgres.rs:657`: forced-exact plan is still `Index Scan using concepts_embedding_idx` / `embedding <=> …::vector`. Source pin `explain_vector_candidates_uses_store_forced_exact_scan` stayed **GREEN** (include_str does not see the execute body). |
| M-extra | R1-1 | exact-lane `explain_vector_candidates(&exact, None)` to `Some("SET LOCAL enable_indexscan = off")` | `explain_vector_candidates_uses_store_forced_exact_scan` | **RED** at `postgres.rs:467`: `exact-lane EXPLAIN must not inject the GUC as extra_set` |
| M-prod | R1-1 | drop `self.issue_forced_exact_scan(&mut tx)` from `vector_candidates_checked` | same source pin | **RED** at `postgres.rs:459`: `vector_candidates_checked must issue the GUC via the shared helper` |
| M-cam | R1-1 (live) | drop `store.issue_forced_exact_scan(&mut tx)` from `explain_vector_candidates` | `explain_recall_uses_hnsw` | **RED** at `postgres.rs:653` (line shift from the delete): forced-exact plan still names `concepts_embedding_idx`. Source pin stayed **GREEN**: the needle `store.issue_forced_exact_scan(&mut tx)` also lives in the test's own `contains(...)` string, so include_str is tautological for the camera call. Live is the camera-issuance pin. Not elevated. |

M-guc is the hole round 1 opened. M-extra proves the exact lane cannot go
back to the extra_set lookalike. M-prod proves production search cannot
drop the shared helper while the camera stays green. All restored.

Conversion pins were re-mutated on the restored tree and were not edited by
the remediator (`cockroach.rs` hash matches round 1; postgres formula body
is still `(1.0 - dist).clamp(-1.0, 1.0)`):

| # | Claim | Cited test | Mutation | Result |
| --- | --- | --- | --- | --- |
| M1 | Postgres score is `1 - d` | `distance_to_score_is_one_minus_d` | body `(1.0 - dist).clamp` to `(1.0 - 0.5 * dist * dist).clamp` | **RED** at `postgres.rs:243`: left `0.5`, right `0.0` (`d=1`) |
| M2 | Cockroach score stays `1 - d^2/2` | `distance_to_score_is_cosine` | body `(1.0 - 0.5 * dist * dist).clamp` to `(1.0 - dist).clamp` | **RED** at `cockroach.rs:448`: `(C::distance_to_score(1.0) - 0.5).abs() < 1e-12` |

Mutation score: **1/1 on the R1-1 issuance mutation (M-guc). 2/2 conversion
pins still caught. extra_set lookalike and production-helper drop both
caught. Camera helper-call include_str is tautological; live EXPLAIN
catches that drop.**

## Part B: hunt for defects introduced by the remediation

No new findings. Specific attack vectors examined:

- **Vacuous pin.** Ruled out for M-guc: live EXPLAIN fails because the
  forced-exact plan still names `concepts_embedding_idx`, not a tautology.
  Ruled out for extra_set: M-extra fails the `!camera.contains(Some("SET
  LOCAL enable_indexscan = off"))` assert; the test's own escaped string
  does not match the unescaped call. Ruled out for production wiring:
  `self.issue_forced_exact_scan(&mut tx)` occurs once in `mod.rs` (the
  call at `mod.rs:2754`); dropping it turns the source pin red. The
  camera-call include_str *is* tautological (M-cam source pin green) because
  the needle is in the test. Compensated by live EXPLAIN (M-cam red). The
  last source-pin assert `camera.contains("store.forced_exact_scan()")` is
  satisfied by rustdoc, not a getter call in the helper body; the helper
  reads `self.force_exact_scan` inside `issue_forced_exact_scan`. Not
  elevated: the brief's required mutation is live, and that pin is red.
- **H1 / sqlite.rs.** sqlite.rs hash is byte-identical to round 1. This
  remediator did not edit it. `h1_sqlite_and_memory_oracle_agree_exactly`
  passed under `store-sqlite,store-cockroach,fixtures` (0.07s). All sqlite.rs
  hunks versus `97ee28c` start at line 5361 or later, inside
  `mod h1_cross_store_parity`.
- **Second harness.** No `mod h3`. H3 remains `h3_postgres_recall_parity`
  in the H1 module. Live H3 on the restored tree passed (1.00s): 240 pairs,
  4 adapters, hnsw envelope min_jaccard 1 / max_score_diff 0.
- **H3 `index_present` still a constructor bool.** Round 1 called an H3
  EXPLAIN probe optional. The remediator did not add one. Camera-proof now
  issues the GUC via the production helper, which is what round 1 required.
  Not reopened.
- **Conversion pins weakened.** `cockroach.rs` hash matches round 1. M1 and
  M2 go red at `d=1`. Production bodies restored: Postgres `1.0 - dist`,
  Cockroach `1.0 - 0.5 * dist * dist`.
- **B4 claimed.** `GraphStore::vector_dimensions` still echoes construction
  dim (`pg/mod.rs:2281-2283`). postgres.rs module doc still names B4 as not
  closed. CHANGELOG ranking note does not mention live-schema reporting.
- **Leftover `:latest`.** `.github/workflows/ci.yml:278` is still
  `pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
  The `:latest` token in that file is the comment that the image is never
  `:latest`, plus the pre-existing GHA runner label `ubuntu-latest`. `ci.yml`
  hash matches round 1. My live container was `Image=sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`,
  then removed.
- **Leftover mutation.** After restore: the SET LOCAL execute is present at
  `mod.rs:1271`. Exact-lane EXPLAIN is `None`. Postgres formula is `1.0 -
  dist`. All eight SHA-256 values match the header.
- **CYCLE merge-once.** Intact: B stays on `b0-pg-extraction` until B0
  through B4 are all closed; one merge to `lambo-for-mooshik` at the end of
  B. Status line still says "B3 is next"; that is orchestrator-owned, not a
  B3 defect. B0, B1, and B2 were not reopened. B4 was not started.
- **Em dashes in remediator-authored lines.** `git diff -U0` added lines of
  every B3-touched tracked file: zero U+2014. `B3-remediation.md`: zero.
- **Files outside the finding.** Versus the round-1 reviewed hashes, only
  `postgres.rs` and `pg/mod.rs` changed. Conversion formula bodies,
  composed-SQL tokens, H3 harness, digest-pinned `postgres-live`, and the
  B0 composed-SQL pin were not retouched.
- **B0 composed-SQL pin.**
  `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` passed on
  the restored tree.

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B3-remediation.md`. Live Cockroach tests: not run. `.env` and `models/`:
not touched. Live Postgres: pinned digest only, container removed at the
end.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **940 passed / 0 failed / 4 ignored**, **944 listed**. Lib: 928 passed / 2 ignored |
| `cargo test --no-default-features --features store-cockroach` | 0 | **603 passed / 0 failed / 0 ignored**, **603 listed**. Lib: 594 passed |
| `cargo test --features store-cockroach,fixtures` | 0 | **1000 passed / 0 failed / 12 ignored**, **1012 listed**. Lib: 987 passed / 10 ignored |
| `cargo test --no-default-features --features store-postgres` | 0 | **589 passed / 0 failed / 3 ignored**, **592 listed**. Lib: 580 passed / 3 ignored (`init_schema_at_two_widths_creates_hnsw`, `explain_recall_uses_hnsw`, `fencing_refuses_stale_write_and_upserts_replay`). +1 over B3 review 591: `explain_vector_candidates_uses_store_forced_exact_scan` |
| B0 composed-SQL pin `--lib` `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` | 0 | **passed** |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed** (0.07s). sqlite.rs hash matches round 1 |
| Live EXPLAIN + fencing on pinned digest | 0 | **passed**. Natural / `enable_seqscan = off`: `Index Scan using concepts_embedding_idx`. Forced-exact: `Seq Scan` + `Sort`. Fencing: `fencing_refuses_stale_write_and_upserts_replay` ok. M-guc was **RED** first |
| H3 live `--lib` `h3_postgres_recall_parity` under `store-postgres,store-sqlite,fixtures` | 0 | **passed** (1.00s). 240 pairs, 4 adapters. Envelope min_jaccard 1, max_score_diff 0. Digest `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f` |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines including cargo's summary line (B0/B1 counting method); cargo reports **53** rustdoc warnings. **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` / `issue_forced_exact_scan` |

No gate finding. Remediator's measured counts match mine.

## Summary of closures

| ID | Grade | Status |
| --- | --- | --- |
| B3-R1-1 | P2 | **HOLDS** (mutation: M-guc live red; M-extra and M-prod source-pin red; conversion M1/M2 still red) |

**0 P1, 0 P2, 0 P3 residue.** Round 1's P2 is now a shared
`PgStore::issue_forced_exact_scan` used by production search and the
camera-proof, plus an exact-lane EXPLAIN that does not pass the GUC as
`extra_set`. Deleting the SET LOCAL execute turns `explain_recall_uses_hnsw`
red because the forced-exact plan still names `concepts_embedding_idx`.
Conversion `1 - d` versus `1 - d^2/2`, composed-SQL tokens, H1 lock,
digest-pinned `postgres-live`, and the B0 composed-SQL pin still hold from
round 1 and were not reopened. B4 is not closed.

**Tree state at close**: restored to the reviewed state and verified. All
SHA-256 values in the header match, and `git status --short` is identical
to session start except for this file. Nothing was committed or pushed.

B3Review2, 2026-08-23
