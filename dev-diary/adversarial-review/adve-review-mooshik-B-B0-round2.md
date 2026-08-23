# Adversarial review: mooshik B, phase B0 (extraction), round 2

**Reviewer**: independent adversarial reviewer, agent_id `B0Review2`. Wrote nothing
under review except this file. No commit, no push.
**Scope**: the uncommitted B0 round-1 remediation on branch `b0-pg-extraction`
against `adve-review-mooshik-B-B0-round1.md` (0 P1 / 1 P2 / 5 P3,
REQUEST_CHANGES) and the remediator's claims in `b-run/B0-remediation.md`.
**Worktree**: `/home/nryn/work/lambo`, branch `b0-pg-extraction` @ `1368f83`
(one docs commit above `bd3e9ac`; pre-B0 baseline `7937de7`). Dirty tree at
review start is the remediator's edits plus the round-1 review closures
appendix. Ignored: `local:/` and the orchestrator briefs.
**Verdict**: **APPROVE**. All six closures hold. Zero failed closures. Zero new
findings.

Reviewed-state SHA-256, taken before any mutation and re-verified after every
mutation cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/pg/mod.rs` | `45ab510d99cea44796559c26f75d75bca375476671739910ba55a088f307b4dd` |
| `src/store/pg/dialect.rs` | `a982fbdb6bc27d1c58977c88db11050aa08eb11ddcde13b45df757a6d3aca56d` |
| `src/store/pg/cockroach.rs` | `12f775f56eda2398130391872e714c4f1cf7c55b33d51d618f56fa51c251f170` |
| `src/store/mod.rs` | `7af4e26f78c8fe4da3081dac356b11732415fe67f5b7954ce1503b6756708daf` |

`dialect.rs` and `src/store/mod.rs` are byte-identical to the round-1 reviewed
state. `pg/mod.rs` differs by the one rustdoc line (B0-R1-2). `cockroach.rs`
differs by that rustdoc line plus the standing pin (B0-R1-1).

## Method

1. Read `b-run/B0-round2-review-brief.md`, `b-run/CYCLE.md`, the round-1 review
   plus its closures appendix, `b-run/B0-remediation.md`, `B-postgres-store.md`
   (B0, B3), `b-run/B0-implementation.md`, and the K-round-2 house style.
2. Copied the four reviewed source files to `/tmp/b0-r2-review-snapshot/`
   before any mutation. Every mutation cycle ends with a restore and a
   `sha256sum` comparison against the table above.
3. Independently re-extracted the ten pre-carve SQL constants from
   `git show 7937de7:src/store/cockroach.rs` with a parser and byte-compared
   them to the pin's `PRE_*` literals. All ten MATCH. Independently compared
   the pin's `pre_keyword_candidates_sql` builder to the 7937de7 function:
   same `::STRING` literal prefix, same loop, same `strpos` clauses.
4. **Mutation-tested every closure that has an offline pin.** A closure counts
   as mutation-verified only if the cited test FAILS under the mutation.
   R1-3 through R1-6 have no test to fail: they are labelled **trace**.
5. Re-ran every CYCLE gate myself on the restored tree (results in Part C).
   Live Cockroach tests: not run. Postgres container: not started. `.env` and
   `models/`: not touched.
6. Hunted for defects the remediation introduced: vacuous pin, a test that
   asserts the constant not the composed SQL, over-broad CYCLE edits, leftover
   mutation, rustdoc of a different broken form, a split of `init_schema` /
   `connect_options` that R1-3 forbade.

Honest process note: the first `cargo test --features store-cockroach` after
the mutation cycles failed three SQL-shape tests with `$1::vector` in the
composed text. That was this reviewer's `cp -a` restore preserving a snapshot
mtime older than the last VECTOR_CAST-mutated artifact, so cargo reused the
stale incremental binary. `touch` of the two restored files and a re-run went
green (935 passed / 4 ignored / 939 listed). The pin, run exact against a
fresh compile of the unmutated tree before any mutation, was already ok. Not
a product defect.

## Part A: per-finding closure verification

| Finding | Verdict | Verification |
| --- | --- | --- |
| B0-R1-1 (P2) standing byte-identity pin | **HOLDS** | Test `store::pg::cockroach::tests::b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` (`src/store/pg/cockroach.rs:662`) is in `mod tests` next to the other SQL-shape tests. `crdb_sql()` is `DialectSql::for_dialect::<CockroachDialect>()`, not a stub. `PRE_*` bodies are byte-identical to the ten 7937de7 constants (independent parser, 169..494 bytes each). The test `assert_eq!`s all ten `DialectSql` fields, `vector_cast == "::VECTOR"`, and `keyword_candidates_sql::<CockroachDialect>(n)` for n = 1..5 against a pre-carve builder that still hard-codes `::STRING`. Mutations below all go red at `cockroach.rs:751` (`vector_candidates`). CYCLE listed counts measured **939 / 598 / 1007**. The pin is present in all three `--list` outputs. |
| B0-R1-2 (P3) two rustdoc links + private-items gate | **HOLDS** | `cockroach.rs:114` is now `[crate::store::GraphStore::init_schema]` (`GraphStore::init_schema` lives on the trait at `src/store/mod.rs:248-249`). `pg/mod.rs:940` is now `[PgStore::normalize_tokens]` (inherent method at `pg/mod.rs:1453`). CYCLE.md phase-gate list includes `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` (`CYCLE.md:85-86`). My doc run: **54** `^warning:` lines (53 issue warnings + the crate summary `generated 53 warnings`), matching the round-1 baseline counting method; **zero** naming `store/pg`, `init_schema`, `normalize_tokens`, or `CockroachStore`. Mutation: restore the two round-1 spellings → **56** warnings, the two new ones exactly `PgStore has no field or associated item named init_schema` at `cockroach.rs:114` and `no item named CockroachStore in scope` at `mod.rs:940`. Restore → 54, those two gone. |
| B0-R1-3 (P3) name the over-merged functions, do not split | **HOLDS (trace)** | `init_schema` and `connect_options` are still one function each in `PgStore`. `pg/mod.rs:2171` still executes `ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint STRING`. `pg/mod.rs:1257` still sends `("vector_search_beam_size", beam.to_string())`. `git diff` of `pg/mod.rs` is the single rustdoc line; no split landed. `B0-implementation.md` §3.4 and new §4.1 name both as known over-merged B2/B3 debt (executed DDL / session setting), not as five error strings. B0-N3 and B0-N4 moved into the §4.1 table; the §4.2 table is the three operator-facing strings. |
| B0-R1-4 (P3) B0-N2 lists both sites | **HOLDS (trace)** | `B0-implementation.md` B0-N2 row names `PgStore::new` (`src/store/pg/mod.rs:1201`) **and** `connect_options` (`src/store/pg/mod.rs:1234`). Grep finds exactly those two `"invalid Cockroach DSN: {e}"` constructions, no third. |
| B0-R1-5 (P3) fixtures row in CYCLE.md | **HOLDS (already-closed, trace)** | CYCLE.md baseline table already has `cargo test --features store-cockroach,fixtures` = 994 passed / 0 failed / 12 ignored (1006 listed), plus the note that `fixtures` compiles `mod conformance` and `mod h2_cockroach_parity`. Both listing files exist at `$SCRATCH/b-baseline/` (`tests-cockroach.txt` 938 lines, `tests-nodefault-cockroach.txt` 597 lines). No third fixtures listing was promised; none invented. Not re-opened. |
| B0-R1-6 (P3) 26 of 938, not 52 | **HOLDS (trace)** | `B0-implementation.md:105`: "26 of the 938 entries carry the renamed prefix (26 of 597, 36 of 1006)". Independent count on the baseline listings: **26** of 938 and **26** of 597 `startswith store::cockroach::`. The 36-of-1006 figure follows from the current fixtures listing (37 of 1007) minus the new pin. Conclusion (full-path diffs empty after the module-prefix rewrite) is unchanged. |

### B0-R1-1 mutations (restored after each)

Each cycle: mutate, run
`--lib store::pg::cockroach::tests::b0_composed_sql_is_byte_identical_to_the_pre_carve_constants --exact`,
restore, `sha256sum` check. Unmutated pin was **ok** before the first mutation.

| # | Mutation | Result | Panic |
| --- | --- | --- | --- |
| M5 | `STRING_CAST` `"::STRING"` → `"::TEXT"` | **RED** | `cockroach.rs:751` `vector_candidates`: left `id::TEXT` / `session_id::TEXT`, right `id::STRING` / `session_id::STRING` |
| M7 | `DISTANCE_OP` `"<->"` → `"<=>"` | **RED** | `cockroach.rs:751` `vector_candidates`: left `embedding <=> $1::VECTOR`, right `embedding <-> $1::VECTOR` |
| WS | `ORDER BY dist ASC` → `ORDER BY  dist ASC` in the `vector_candidates` template (`pg/mod.rs`) | **RED** | `cockroach.rs:751` `vector_candidates`: left `ORDER BY  dist ASC`, right `ORDER BY dist ASC` |
| VC (extra) | `VECTOR_CAST` `"::VECTOR"` → `"::vector"` | **RED** | `cockroach.rs:751` `vector_candidates`: left `$1::vector`, right `$1::VECTOR` |

M5 and M7 are the two Part C survivors the deleted proof was the only pin for.
WS shows the pin is not a substring check: one extra space fails it. VC shows
`VECTOR_CAST` is pinned by the same `assert_eq!` on composed text, not only by
the extra `sql.vector_cast == "::VECTOR"` token assert. The pin is not vacuous:
`PRE_*` are frozen raw-string literals matching 7937de7, not `format!` from
`STRING_CAST` / `DISTANCE_OP`, and `crdb_sql()` composes from the live dialect.

CYCLE counts after the pin, measured by me:

| Gate | Listed (`: test$`) | Passed / ignored |
| --- | ---: | --- |
| `cargo test --features store-cockroach` | 939 | 935 / 4 |
| `cargo test --no-default-features --features store-cockroach` | 598 | 598 / 0 |
| `cargo test --features store-cockroach,fixtures` | 1007 | 995 / 12 |

Mutation score: **5/5 attempted mutations were caught** (M5, M7, WS, VC, and
the R1-2 rustdoc pair). None of the five pins passed regardless of the fix.

## Part B: hunt for defects introduced by the remediation

No new findings. Specific attack vectors examined:

- **Vacuous pin.** Ruled out. The test does not `assert_eq!` a `PRE_*` against
  itself, does not rebuild the right-hand side from `D::STRING_CAST`, and
  `crdb_sql()` is the real `for_dialect` composer. Mutating the dialect consts
  or one space in a template fails it. Independently, `PRE_*` matches 7937de7,
  so the pin cannot have been generated from a drifted post-carve body.
- **Asserts the constant, not the behaviour.** `assert_eq!(sql.vector_cast, "::VECTOR")`
  is a token check, but every mutation that changes `STRING_CAST`,
  `VECTOR_CAST`, or `DISTANCE_OP` also fails the `vector_candidates` full-string
  compare (the statement the store actually issues). `keyword_candidates_sql`
  is compared to a builder that still hard-codes `::STRING`. Not a finding.
- **Over-broad CYCLE edits.** Status line now says remediation is on the dirty
  tree and B0 is not closed until APPROVE. New "Expected counts after B0-R1-1"
  table names the pin and 939 / 598 / 1007. New "Phase gates" list is the
  round-1 gate set plus `--document-private-items` (R1-2's fix). Fixtures-row
  closure note is accurate. No standing rule rewritten. Not over-broad.
- **Leftover mutation.** After restore: `STRING_CAST = "::STRING"`,
  `DISTANCE_OP = "<->"`, `VECTOR_CAST = "::VECTOR"`. Grep for `ORDER BY  dist`
  and leftover `<=>` / `::TEXT` in the two files: empty.
- **Broken rustdoc of a different form.** `cargo doc --document-private-items`
  stderr has no `src/store/pg` path. Mutating the two fixed links is the only
  way to make store/pg warnings reappear, and they are the same two round-1
  messages. No new private-item rot from the pin's rustdoc comment.
- **Forbidden split of `init_schema` / `connect_options`.** Did not happen.
  `git diff -- src/store/pg/mod.rs` is one rustdoc line.
- **Stale numbers in `B0-implementation.md` §5.** The original implementer's
  gate table still says 938 / 597 / 1006 "on the final tree", while §3.3 and
  §6 of the same file (and CYCLE.md, and `B0-remediation.md`) say 939 / 598 /
  1007. The layout table still lists `cockroach.rs` at 3956 lines; it is 4112
  after the pin. CYCLE.md is the protocol of record and is correct. R1-1's
  fix asked to update CYCLE.md, which happened. Not elevated: historical §5
  was not re-run as a lie, and B1 reads CYCLE.md.
- **Prefix count after the pin.** Current listings: 27 of 939, 27 of 598, 37
  of 1007. That is 26+1 / 36+1, the pin itself. R1-6 asked to correct the
  original 52 figure, not to rewrite it as a post-pin count. Left as 26 of 938.
- **Em dashes in remediator prose.** CYCLE.md and `B0-remediation.md`: zero.
  The one INIT_SQL doc line the remediator touched replaced an em dash with a
  comma (`T3.1 DDL, embedded`). Pre-existing design-log dashes in `cockroach.rs`
  / `pg/mod.rs` were not part of this remediation.

## Part C: gates rerun (my own runs, restored tree)

All rows are my runs on the hashes in the header. Nothing is copied from
`B0-remediation.md` or `CYCLE.md`.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo test --features store-cockroach` | 0 | **935 passed / 0 failed / 4 ignored**, 939 listed. Pin present. |
| `cargo test --no-default-features --features store-cockroach` | 0 | **598 passed / 0 failed / 0 ignored**, 598 listed. Pin present. |
| `cargo test --features store-cockroach,fixtures` | 0 | **995 passed / 0 failed / 12 ignored**, 1007 listed. Pin present. |
| H1 lock: `--lib h1_cross_store_parity` under `store-sqlite,store-cockroach,fixtures` | 0 | `h1_sqlite_and_memory_oracle_agree_exactly` **passed**; `git diff src/store/sqlite.rs` **empty** (0 bytes) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54** `^warning:` lines (baseline counting method); **none** naming `store/pg` / `init_schema` / `normalize_tokens` / `CockroachStore` |

`#[ignore]`d live Cockroach tests: **not run**. Postgres container: **not
started**.

No gate finding.

## Summary of closures

| ID | Grade | Status |
| --- | --- | --- |
| B0-R1-1 | P2 | **HOLDS** (mutation: M5, M7, WS, VC all red) |
| B0-R1-2 | P3 | **HOLDS** (mutation: broken links → 56 warnings, same two messages) |
| B0-R1-3 | P3 | **HOLDS (trace)** |
| B0-R1-4 | P3 | **HOLDS (trace)** |
| B0-R1-5 | P3 | **HOLDS (already-closed, trace)** |
| B0-R1-6 | P3 | **HOLDS (trace)** |

**0 P1, 0 P2, 0 P3 residue.** Round 1's P2 is now a standing offline pin on
B3's dangerous row. The extraction remains behaviour-neutral, and the tree
can prove it.

**Tree state at close**: restored to the reviewed state and verified. All four
SHA-256 values match the table in the header, and `git status --short` is
identical to session start except for this file. Nothing was committed or
pushed.

B0Review2, 2026-08-23
