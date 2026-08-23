# B3 round-1 remediation

**Agent:** `b3-remediator`. Branch `b0-pg-extraction`. Tree left dirty, **not
committed**. Closes the single finding in
`dev-diary/adversarial-review/adve-review-mooshik-B-B3-round1.md`. Does not
reopen B0/B1/B2, does not weaken the conversion pins, does not start B4,
does not touch `.env`, `models/`, or `src/store/sqlite.rs`. Did not add an
H3 EXPLAIN probe (optional in the finding).

Keeps the CYCLE.md merge-once line: B stays on this branch until B0-B4 are
all closed; one merge to `lambo-for-mooshik` at the end of B.

Authority: spec of record, then B-postgres-store.md, then source, then the
review file.

---

## Per-finding

### B3-R1-1 (P2): closed

`explain_vector_candidates` (`src/store/pg/postgres.rs:596`) now issues
`Dialect::forced_exact_scan_sql` when `store.forced_exact_scan()` is set,
through the same helper production search uses.

* `PgStore::issue_forced_exact_scan` (`src/store/pg/mod.rs:1265`) holds the
  SET LOCAL execute (`mod.rs:1269-1272`).
* `vector_candidates_checked` calls it after the contract read
  (`mod.rs:2754`).
* The camera-proof calls the same helper (`postgres.rs:608-611`).
* Exact-lane EXPLAIN is `explain_vector_candidates(&exact, None)`
  (`postgres.rs:654-655`). It does **not** pass
  `SET LOCAL enable_indexscan = off` as `extra_set`.
* `extra_set` remains only the inverse GUC that proves hnsw *can* be used
  (`enable_seqscan = off`).

Unit pin `explain_vector_candidates_uses_store_forced_exact_scan`
(`postgres.rs:452`) fails if either caller drops the shared helper or if
the exact lane injects the GUC as `extra_set`.

Live EXPLAIN on the restored tree, pinned digest
`sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`:

* planner's choice / `enable_seqscan = off`: `Index Scan using
  concepts_embedding_idx` / `embedding <=> '[0,0,0,0,0,0,0,0]'::vector`
* forced-exact (`with_forced_exact_scan()`, `extra_set = None`): `Seq Scan`
  + `Sort`, no `concepts_embedding_idx`

Conversion pins were re-run green and not edited:
`distance_to_score_is_one_minus_d`,
`recall_sql_pairs_cosine_operator_with_text_and_vector_casts`,
Cockroach `distance_to_score_is_cosine`, B0 composed-SQL pin.

Mutation evidence below. Reverted after the cycle.

---

## Mutations (reverted)

| ID | Mutation | Cited test | Result |
| --- | --- | --- | --- |
| M-guc | delete the SET LOCAL execute inside `issue_forced_exact_scan` (`mod.rs:1269-1272`; this is the execute that lived at `vector_candidates_checked` ~2737-2741) | `explain_recall_uses_hnsw` | **RED** at `postgres.rs:657`: forced-exact plan still names `concepts_embedding_idx` (`Index Scan using concepts_embedding_idx` / `embedding <=> …::vector`) |
| M-extra | (source pin) exact-lane `extra_set` lookalike, or either caller dropping `issue_forced_exact_scan` | `explain_vector_candidates_uses_store_forced_exact_scan` | pin is the `include_str!` asserts at `postgres.rs:452-476` |

M-guc is the hole the review opened (M6 was GREEN because `extra_set` was a
lookalike). Restored. Conversion formula bodies were not mutated this round.

---

## Gates

All rows are this remediator's runs on the restored tree after the closure.
Live Cockroach tests: not run. `.env` and `models/`: not touched.
`src/store/sqlite.rs` remediator diff: empty (the H3 harness hunk is B3
implementation, not this round). Live Postgres: pinned digest only,
container removed at the end.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **940 passed / 0 failed / 4 ignored**, **944 listed**. Lib: 928 passed / 2 ignored |
| `cargo test --no-default-features --features store-cockroach` | 0 | **603 passed / 0 failed / 0 ignored**, **603 listed**. Lib: 594 passed |
| `cargo test --features store-cockroach,fixtures` | 0 | **1000 passed / 0 failed / 12 ignored**, **1012 listed**. Lib: 987 passed / 10 ignored |
| `cargo test --no-default-features --features store-postgres` | 0 | **589 passed / 0 failed / 3 ignored**, **592 listed**. Lib: 580 passed / 3 ignored. +1 over B3 review 591: `explain_vector_candidates_uses_store_forced_exact_scan` |
| B0 composed-SQL pin `--lib` `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` | 0 | **passed** |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed** (0.07s). This remediator did not edit `sqlite.rs` |
| Live EXPLAIN on pinned digest | 0 | **passed** after restore. Forced-exact is Seq Scan + Sort. M-guc was **RED** first |
| Live fencing | 0 | **passed** `fencing_refuses_stale_write_and_upserts_replay` |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines including cargo's summary line (B0/B1 counting method); cargo reports **53** rustdoc warnings. **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` / `issue_forced_exact_scan` |

CYCLE.md merge-once wording is intact. B0/B1/B2 were not reopened. B4 was
not started. Conversion pins were not weakened.

Nothing was committed or pushed.
