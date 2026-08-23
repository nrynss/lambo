# Adversarial review: mooshik B, phase B2 (PostgresDialect DDL), round 2

**Reviewer**: independent adversarial reviewer, agent_id `B2Review2`. Wrote
nothing under review except this file. No commit, no push.
**Scope**: the uncommitted B2 round-1 remediation on branch `b0-pg-extraction`
against `adve-review-mooshik-B-B2-round1.md` (0 P1 / 1 P2 / 0 P3,
REQUEST_CHANGES) and the remediator's claims in `b-run/B2-remediation.md`.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `e14ba49`
(B1 closed, round 2 APPROVE). Dirty tree at review start is the B2
implementation plus the round-1 review, its closures appendix, and the
round-1 remediation. Ignored: `local:/` and the orchestrator briefs.
**Verdict**: **APPROVE**. The single closure holds. Zero failed closures.
Zero new findings.

Reviewed-state SHA-256, taken before any mutation and re-verified after every
mutation cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/mod.rs` | `f96df098de9e2a236bb6fee4b9401d481467f643303961213605a04344a782a5` |
| `src/store/pg/postgres.rs` | `dd4955a373347b19f6c27226a2795f01e141e64f2b856a6412c60af6430e90ee` |
| `src/store/pg/mod.rs` | `17c45b2ca980248983fe8a5182572f16ce1b461432ff5e18415755e50385bb5a` |
| `src/store/pg/dialect.rs` | `b3c0e8e1338e4dea431f2e3b9a8ca903b5b62d8fcb594168f6b2024fbc553b1f` |
| `src/store/pg/cockroach.rs` | `35e7e5cec9fba1872d4d6924e471fe83417b5513ead7c2bb00154087ffd3ebe6` |
| `src/cli/provision.rs` | `c25c29af597112e82bfb65da1fd4aad2f2759d79825eeefca5dd2dded0f99f6d` |
| `migrations/postgres/001_init.sql` | `c7ecd3cf18b55a9d4ee4015b59498aacc4ea86ed47436dad39a16566153aff98` |
| `.github/workflows/ci.yml` | `6000666458f85b22c6d341e186900744f10b06ece8480c7d5ec0ed3eb4e15504` |

`postgres.rs`, `pg/mod.rs`, `dialect.rs`, `cockroach.rs`, `provision.rs`,
`001_init.sql`, and `ci.yml` are byte-identical to the round-1 reviewed
state. `mod.rs` differs by the R1-1 test, the rustdoc/body-comment rewrite,
and the production copy those pin.

Copied those files to `/tmp/b2-r2-review-snapshot/` before any mutation.
Every mutation cycle ends with a restore and a `sha256sum` comparison against
the table above.

## Method

1. Recalled dogfood memory (`B2Review2`). Read `B2-round2-review-brief.md`,
   `B2-remediation.md`, the round-1 review plus its closures appendix,
   `B-postgres-store.md` B2, `CYCLE.md`, and house style
   `adve-review-mooshik-B-B1-round2.md`.
2. Mutation-tested the R1-1 closure. A closure holds only if the cited test
   FAILS under the mutation. Rustdoc is labelled **trace**.
3. Independently re-ran every CYCLE gate on the restored tree, plus the
   `store-postgres` compile/unit row and the extra `store-postgres` clippy.
   Live Cockroach tests: not run. Postgres container: not started. `.env` and
   `models/`: not touched.
4. Hunted for: vacuous pin, B3 ranking guessed, `sqlite.rs` edits, leftover
   `:latest` on the pgvector image, leftover mutation, em dashes in
   remediator-authored lines, CYCLE merge-once wording dropped, B0/B1
   reopened.

Unmutated pins were green before the first mutation
(`--no-default-features --features store-postgres` for the copy pin). After
restore, incremental compile was forced with `touch` of the restored files.

## Part A: per-finding closure verification

| Finding | Verdict | Verification |
| --- | --- | --- |
| B2-R1-1 (P2) embedder-width copy | **HOLDS** | `store::tests::postgres_copies_embedder_width_when_pin_is_absent` (`mod.rs:1228`) calls `build_store_with_vector_dim` the way production does (`resolve_backends` passing `Some(embedder_cfg.dim)`). Pin absent, param `Some(768)`: `vector_dimensions() == Some(768)` (`mod.rs:1256`). Pin `Some(1536)`, param `Some(768)`: `vector_dimensions() == Some(1536)` (`mod.rs:1271`). Production copy is still `mod.rs:961-963`. Uncompiled `StoreKind::Postgres` still fail-closes on this call. Rustdoc **HOLDS (trace)** below. |

Rustdoc **HOLDS (trace)**. Function doc `mod.rs:886-889` names SQLite **and**
Postgres as consumers (Postgres templates `vector(n)` at init; the copy fills
the pin when absent). Cockroach still parses `VECTOR(n)` out of its own DDL.
Body comment `mod.rs:923-926` says the same. `rg` for `Only adapters whose
vector column` / `Consumed only by the width-agnostic` / `only SQLite` in
`src/store/mod.rs`: no hits. No cargo pin for the words; verified by reading.

### B2-R1-1 mutations (restored after each)

Each cycle: mutate, run the cited `--lib` test under
`--no-default-features --features store-postgres`, restore from snapshot,
`sha256sum` check, `touch` so cargo cannot reuse a mutated artifact.

| # | Closure | Mutation | Cited test | Result |
| --- | --- | --- | --- | --- |
| M-copy | R1-1 | delete `let mut cfg = cfg;` and `if cfg.vector_dim.is_none() { cfg.vector_dim = vector_dim; }` | `postgres_copies_embedder_width_when_pin_is_absent` | **RED** at restored `mod.rs:1256`: left `Some(1024)`, right `Some(768)` (`absent pin must take the embedder width, not default 1024`) |
| M-pin | R1-1 | `cfg.vector_dim = Some(768)` always (param overwrites pin) | same | **RED** at restored `mod.rs:1271`: left `Some(768)`, right `Some(1536)` (`pin still outranks the embedder-width param`) |

M-copy is the hole round 1 opened. M-pin proves the second assertion is not
vacuous. Both restored.

Mutation score: **2/2 attempted closure mutations were caught.** Neither pin
passed regardless of the fix.

## Part B: hunt for defects introduced by the remediation

No new findings. Specific attack vectors examined:

- **Vacuous pin.** Ruled out for copy-delete: M-copy fails `vector_dimensions()`
  at 1024 vs 768, not a tautology. Ruled out for pin-outranks: M-pin fails at
  768 vs 1536; the first assertion stayed green under that mutation, so the two
  asserts are independent. Always assigning the already-rebound
  `cfg.vector_dim.or(param)` left the cited test **GREEN**: that is equivalent
  to the `is_none` copy after `mod.rs:927`, not a hole. The uncompiled branch
  of the test (CYCLE's `store-cockroach` row, feature off) fail-closes and
  would stay green under copy-delete; the behavioural pin lives on
  `--features store-postgres`, which is the row that compiles the copy. Not
  elevated.
- **B3 ranking guessed.** `PostgresDialect::distance_to_score` is still
  `unimplemented!` naming B3 (`postgres.rs:81-86`). `return 1.0 - dist` made
  `distance_to_score_does_not_guess_a_formula` **FAIL** (test did not panic).
  Cockroach formula `1.0 - 0.5 * dist * dist` the same. `postgres.rs` is
  byte-identical to round 1. Ranking conversion was not started.
- **H1 / sqlite.rs.** `git diff -- src/store/sqlite.rs` is 0 bytes.
  `h1_sqlite_and_memory_oracle_agree_exactly` passed under
  `store-sqlite,store-cockroach,fixtures`.
- **Leftover `:latest`.** `.github/workflows/ci.yml:278` is still
  `pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
  Never `:latest` on that image. Runner `ubuntu-latest` is the pre-existing
  GHA runner label, not the pgvector image. `ci.yml` hash matches round 1.
- **Leftover mutation.** After restore: the copy is still gated on
  `cfg.vector_dim.is_none()`, rustdoc still names Postgres, `distance_to_score`
  still panics with `B3`. All eight SHA-256 values match the header.
- **CYCLE merge-once.** Intact: B stays on `b0-pg-extraction` until B0
  through B4 are all closed; one merge to `lambo-for-mooshik` at the end of
  B. B0 and B1 were not reopened. B3 ranking was not started.
- **Em dashes in remediator-authored lines.** `git diff -U0` added lines of
  every B2-touched tracked file: zero U+2014. `B2-remediation.md`: zero.
- **Files outside the finding.** Only `src/store/mod.rs` changed versus the
  round-1 reviewed hashes. Template-at-init, hnsw, ceiling, STRING/TEXT,
  beam_size, provision `init_schema`, and the B0 composed-SQL pin were not
  retouched.

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B2-remediation.md`. Live Cockroach tests: not run. Postgres container: not
started. `.env` and `models/`: not touched.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **940 passed / 0 failed / 4 ignored**, **944 listed**. Lib: 928 passed / 2 ignored. +1 over B2 review 943: `postgres_copies_embedder_width_when_pin_is_absent` |
| `cargo test --no-default-features --features store-cockroach` | 0 | **603 passed / 0 failed / 0 ignored**, **603 listed**. +1 over 602 |
| `cargo test --features store-cockroach,fixtures` | 0 | **1000 passed / 0 failed / 12 ignored**, **1012 listed**. +1 over 1011 |
| `cargo test --no-default-features --features store-postgres` | 0 | **586 passed / 0 failed / 1 ignored**, **587 listed**. Lib: 577 passed / 1 ignored. +1 over 586. Ignored remains `init_schema_at_two_widths_creates_hnsw` |
| H1 lock: `--lib` `h1_sqlite_and_memory_oracle_agree_exactly` under `store-sqlite,store-cockroach,fixtures` | 0 | **passed**; `git diff src/store/sqlite.rs` **empty** (0 bytes) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines including cargo's summary line (B0/B1 counting method); cargo reports **53** rustdoc warnings. **none** naming `store/pg` / `postgres.rs` / `PostgresDialect` |

No gate finding. Remediator's measured counts match mine.

## Summary of closures

| ID | Grade | Status |
| --- | --- | --- |
| B2-R1-1 | P2 | **HOLDS** (mutation: M-copy and M-pin both red; rustdoc HOLDS (trace)) |

**0 P1, 0 P2, 0 P3 residue.** Round 1's P2 is now a pin on
`build_store_with_vector_dim` that the copy-delete mutation turns red, plus
a pin-outranks assert the overwrite mutation turns red. Template-at-init,
hnsw from init (not ivfflat), dim > 2000 refuse naming halfvec, B3 ranking
left unimplemented, B0 composed-SQL pin, H1 lock, digest-pinned
`postgres-live`, and provision calling `init_schema` still hold from round 1
and were not reopened.

**Tree state at close**: restored to the reviewed state and verified. All
SHA-256 values in the header match, and `git status --short` is identical
to session start except for this file. Nothing was committed or pushed.

B2Review2, 2026-08-23
