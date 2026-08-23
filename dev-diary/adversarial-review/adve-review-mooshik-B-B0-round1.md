# Adversarial review: mooshik B, phase B0 (extraction), round 1

**Reviewer**: independent adversarial reviewer, agent_id `B0Review1`. Wrote nothing under
review except this file. No commit, no push.
**Scope**: the uncommitted B0 change in `/home/nryn/work/lambo` on branch
`lambo-for-mooshik`, against `dev-diary/lambo-for-mooshik/B-postgres-store.md`
("Design decision, recorded", **B0, Extraction** and **B3, The dialect surface**) and the
implementer's claims in `b-run/B0-implementation.md`.
**Tree state**: HEAD `bd3e9ac` (one docs-only commit above the pinned pre-B0 baseline
`7937de7`), working tree dirty:

```
 M src/store/mod.rs
RM src/store/cockroach.rs -> src/store/pg/cockroach.rs
?? src/store/pg/dialect.rs
?? src/store/pg/mod.rs
```

Reviewed-state SHA-256, taken before any mutation and re-verified after every mutation
cycle and again at the end of the review:

| File | SHA-256 |
| --- | --- |
| `src/store/pg/mod.rs` | `342704f8e86a58001ca4864c7c4c4c379838f7cfa1b4658fabb926b3580ebffb` |
| `src/store/pg/dialect.rs` | `a982fbdb6bc27d1c58977c88db11050aa08eb11ddcde13b45df757a6d3aca56d` |
| `src/store/pg/cockroach.rs` | `739c49d2271d838ac65da9f6ac382d82b025e68f10c072592f31d3014d4f8e50` |
| `src/store/mod.rs` | `7af4e26f78c8fe4da3081dac356b11732415fe67f5b7954ce1503b6756708daf` |

**Verdict**: **REQUEST_CHANGES**. 0 P1, 1 P2, 5 P3.

The extraction itself is sound. I independently rebuilt the byte-identity proof and it
passes for every composed statement, and I independently reproduced the whole-body diff:
there is no behaviour change anywhere in the carve. The P2 is not about what the code
does, it is about what the tree can still prove: the implementer deleted the one test
that pinned the composed SQL, and I demonstrated by mutation that with it gone, two of
the five rows of B3's dialect table (`STRING_CAST` and `DISTANCE_OP`) have **zero**
regression pins in the entire 994-test offline suite. `DISTANCE_OP` is the row B3 itself
names "the dangerous one".

---

## Method

1. Read `b-run/CYCLE.md`, `B-postgres-store.md` (§"Design decision", §B0, §B3, §B4), and
   `b-run/B0-implementation.md` in full. Read `src/store/pg/dialect.rs` (77 lines) in
   full, and `src/store/pg/mod.rs` / `src/store/pg/cockroach.rs` at every site the
   review touches.
2. Copied all four reviewed files to a scratch directory **before** any mutation, because
   two of them are untracked and `git checkout --` cannot restore them. Every mutation
   cycle ends with `cp` from that snapshot and a `sha256sum` comparison against the table
   above. The tree was verified byte-identical to the reviewed state after every cycle and
   at the end of the review, and `git status --short` is unchanged from session start.
3. **Mechanical constant diff.** Extracted every `const …: &str` from
   `git show 7937de7:src/store/cockroach.rs` and from both post-B0 files with a parser,
   and compared them by name and by bytes.
4. **Rebuilt the byte-identity proof the implementer removed**, generating the test
   source *from* the baseline constants (so no transcription error is possible), adding it
   transiently to `pg/mod.rs`, running it, checking it is not vacuous, and removing it.
5. **Whole-body diff.** Reconstructed the pre-carve non-test body and the post-carve
   non-test body and diffed them, then read all 39 hunks. Separately diffed the test
   region (baseline `2847..6566` against `pg/cockroach.rs` `214..3956`) and read all 20
   hunks.
6. **Mutation-tested eight targets.** A pin counts as verified only if the cited test
   FAILS under the mutation. Results in Part C, including the four mutations that
   **survived**.
7. **Re-ran every gate myself**, plus three feature combinations the implementer did not
   run, plus `cargo doc` with and without `--document-private-items`, against both the
   post-B0 tree and a `git archive 7937de7` extraction built in its own target directory.
8. Hunted for over-merging by grepping the shared base for dialect conditionals, `bool`
   discriminators, `TypeId`/`Any` downcasts, and surviving Cockroach-specific text.

---

## Part A: per-claim verification

| # | Claim (source) | Verdict | Verification |
| --- | --- | --- | --- |
| A1 | Stage 1 was move-only: exactly 16 forced edits, none a behaviour change (§2.1) | **HOLDS** | Every listed edit is visible in one of my two diffs and every one is a path adjustment: `include_str!` depth (C1), intra-doc links (C2, C10), `use super::` → `use crate::store::` (C3–C7), module-path names inside comments (C8, C9), `super::super::` inside test modules (C11–C13), the test-filter recipe in a doc comment (C14), and the two `src/store/mod.rs` edits (M1, M2). None changes a value, a statement or a control flow. `super::X` at the *old* location resolved to `crate::store::X` exactly, so the absolute rewrite cannot resolve to a different item. Note stated rather than graded: C3–C8 landed in `pg/mod.rs`, whose `super` is again `crate::store`, so the absolute spelling is now a style choice rather than forced. The implementer states exactly this reasoning in §2.1, so it is disclosed, not smuggled. |
| A2 | No file outside `src/store/` changed | **HOLDS** | `git status --short src/` shows only `src/store/mod.rs`, the rename, and the two new `pg/` files. `crate::store::cockroach::CockroachStore` still resolves for `src/canon/eval.rs:2638` through `pub use pg::cockroach;`, and compiles under every clippy combination in Part D. |
| A3 | The composed SQL is byte-identical to the pre-carve constants (§3.3) | **HOLDS (independently reproven)** | See Part B. All 10 composed statements, the `vector_cast` token, and `keyword_candidates_sql::<CockroachDialect>` for n = 1..5 are byte-for-byte equal to the pre-carve literals, including whitespace. Separately, a mechanical constant diff shows all **27** surviving `const …_SQL` constants byte-identical, **0** new constants, and exactly the **10** cast-bearing constants removed. |
| A4 | Zero `::STRING`, `::VECTOR`, `<->` in any SQL in `pg/mod.rs`; no `bool is_cockroach`, no `if cockroach` (§3.4) | **HOLDS, but the check is narrower than the spec's rule** | Grep confirms: no `TypeId`, no `dyn Any`, no `downcast`, no `type_name`, no dialect-conditional `bool`. Every `bool` in `pg/mod.rs` is pre-existing and orthogonal (`has_more`, `tx_retryable`, `session_exists`). But B3's rule is about *SQL identity*, not about three literals: `init_schema` and `connect_options` fail it. Graded as **B0-R1-3**. |
| A5 | The five deliberate inline Cockroach-isms are the complete list (§4) | **PARTIAL** | The five categories are the complete set of surviving Cockroach-specific text in `pg/mod.rs` (I enumerated every live occurrence). But B0-N2 names one site of `"invalid Cockroach DSN: {e}"`; there are two (`pg/mod.rs:1201` in `PgStore::new`, `pg/mod.rs:1234` in `connect_options`). Graded as **B0-R1-4**. |
| A6 | Fencing token on `flush`: refused with `StaleWrite`, never dropped | **HOLDS (diff-verified, not test-verified)** | Zero diff hunks over `batch_session_ids`, the in-transaction `SELECT current_token`, the `lease_permits_write(cur, token)` guard, the `StaleWrite` message including the issue reference, and the `?` that drops `tx`. The same is true of the second gate in `record_canonization`. **Important honesty note:** I mutated the gate to `if false && !lease_permits_write(cur, token)` and the entire 994-test `store-cockroach,fixtures` suite **passed**. So "the test passes" is not merely insufficient evidence here, it is *no* evidence: no offline test touches this invariant at all. The only evidence for A6 is the byte-identical diff, which I reproduced independently. Pre-existing gap, not introduced by B0. |
| A7 | Idempotent upsert semantics unchanged | **HOLDS** | None of `ON_CONFLICT_INTERACTION_SQL`, `ON_CONFLICT_CONCEPT_SQL`, `ON_CONFLICT_EDGE_SQL`, `INSERT_CANONIZATION_EVENT_SQL`, `ON_CONFLICT_WRITE_INTENT_SQL` carries a cast, so none moved into `DialectSql`; all five are in the 27 byte-identical constants of A3, and none has a diff hunk. |
| A8 | The documented `created_at` divergence is preserved | **HOLDS** | `COALESCE($3, now())` survives verbatim in `DialectSql::upsert_session` (proven byte-identical in Part B), `created_at = EXCLUDED.created_at` is unchanged in the flush upsert, and `UPSERT_SESSION_ROW_SQL` is one of the 27 unchanged constants. |
| A9 | The NULL-only quarantine predicate is preserved | **HOLDS** | `QUARANTINE_LEGACY_EMBEDDINGS_SQL` is in the 27 byte-identical constants, is cast-free, still fires only on `embedding_kind IS NULL AND embedding_dim IS NULL`, and its F-R2-1 divergence doc and B2 forward pointer survive verbatim (no diff hunk). |
| A10 | Transaction discipline and retry behaviour survive the carve | **HOLDS** | `tx_retry` has exactly one diff hunk and it inserts a three-line `B2/B3:` comment above an unchanged error string. `tx_retryable`, the retry bound, the backoff and the commit/rollback shape have no hunk. Every `tx_retry` call site in `flush`, `load_session`, `vector_candidates_checked` and `record_canonization` differs only by `&self.sql.*` substitutions and the `::<D>` turbofish. |
| A11 | `PgStore::new` preserves construction-time behaviour | **HOLDS** | Same order and same error strings: DSN presence, `dsn_for_rustls`, parse-validate, then the width authority. `cfg.dsn` became `cfg.dsn.as_deref()` only because `cfg` is now needed for `D::vector_dim(&cfg)`; the produced error is identical. The one new failure mode, `D::init_sql`'s width-mismatch arm, is unreachable for Cockroach (both `init_sql` and `vector_dim` read the same DDL through `ddl_vector_dim()`), which the implementer states in §6. |
| A12 | The §3.6 EXPLAIN change is strictly stronger than the constant | **HOLDS, with a scope correction** | Both sites the implementer names (`vector_explain_camera_proof`, `assert_index_backed`) now read `store.sql.vector_candidates`, which does follow a dialect change. But a **third** EXPLAIN site (`mod conformance`, baseline line 4483) still hand-spells `embedding <-> $1::VECTOR` with a literal `LIMIT 5`. It is unchanged by B0 and it is deliberately hand-spelled (the T7.3 planner-variance note requires a literal LIMIT), so it is not a B0 defect. Recording it so §3.6's "both sites" is not read as "all EXPLAIN sites". |
| A13 | H1 was not edited (§3.5) | **HOLDS** | `git diff --stat src/store/sqlite.rs` is empty and `git status --short src/store/sqlite.rs` is empty. `h1_sqlite_and_memory_oracle_agree_exactly` passes under `--features store-sqlite,store-cockroach,fixtures`. |
| A14 | The pinned baseline has a `fixtures` hole; the real baseline is 1006 / 994 / 0 / 12 | **HOLDS (independently measured)** | `mod conformance` and `mod h2_cockroach_parity` are gated `#[cfg(all(test, feature = "store-cockroach", feature = "fixtures"))]` (`cockroach.rs:1183` and `:3045`), and neither pinned CYCLE.md command enables `fixtures`, so roughly 2,750 of the file's lines were never **compiled** by the pinned gates. I extracted `7937de7` with `git archive` into a scratch tree with its own `CARGO_TARGET_DIR` and measured: **1006** listed, **994 passed / 0 failed / 12 ignored**. Exactly the implementer's numbers. Graded against the run protocol as **B0-R1-5**. |
| A15 | Evidence figure: "52 of the 938 entries carry the renamed prefix" (§2.2) | **WRONG, harmless** | It is **26** of 938 (and 26 of 597, 36 of 1006), anchored or unanchored. The conclusion it supports is unaffected: I reproduced the full-path diffs and all five are empty. Graded as **B0-R1-6**. |
| A16 | No test was added, so the pinned counts did not move (§6) | **HOLDS** | 938 / 597 / 1006 listed, and the leaf-name and full-path diffs are all empty. My own numbers in Part D. |

---

## Part B: the byte-identity proof, rebuilt

This is the claim the tree cannot support on its own, because the implementer removed the
test that proved it. I rebuilt it rather than trusting it.

**Construction.** I generated the test source programmatically from
`git show 7937de7:src/store/cockroach.rs`, extracting each `const …: &str = r#"…"#;` body
with a parser and emitting it as a `PRE_*` constant, so the comparison cannot be weakened
by a transcription slip. The test asserts each field of
`DialectSql::for_dialect::<CockroachDialect>()` against its pre-carve constant, asserts
`vector_cast == "::VECTOR"`, and re-implements the pre-carve `keyword_candidates_sql`
builder verbatim to compare against `keyword_candidates_sql::<CockroachDialect>(n)` for
n = 1..5.

**Result.**

```
test store::pg::b0_review_byte_identity::b0_composed_sql_is_byte_identical_to_the_pre_carve_constants ... ok
```

All eleven statements plus the token, byte for byte, whitespace included. Not a sample:
every cast-bearing statement in the adapter.

**The proof is not vacuous.** I inserted one extra space into the `vector_candidates`
template (`ORDER BY  dist ASC`) and the test FAILED with the expected byte diff.

**A second, independent check.** A mechanical constant diff over all
`const …: &str` declarations in the baseline versus both post-B0 files reports:

```
constants in BASELINE but not post-B0:  the 10 cast-bearing ones (now DialectSql fields)
constants post-B0 but not in BASELINE:  (none)
byte differences among shared constants: 0 differing of 27 shared
```

**A third check.** The whole-body diff of the pre-carve non-test region against the
post-carve `pg/cockroach.rs` dialect block plus `pg/mod.rs` produces 39 hunks. I read every
one. Each is exactly one of: a cast-bearing constant removed, a path adjustment, a
`&self.sql.*` substitution, a `::<D>` turbofish, a `PgStore<D>` generalisation, a new
`B2/B3:` comment, or the `DialectSql` block itself. The test-region diff produces 20 hunks
with the same property. There is no hunk over the fencing gate, the `ON CONFLICT` targets,
`created_at`, the quarantine predicate, or `tx_retry`'s mechanism.

**Should the proof live permanently in the tree? Yes, and its absence is a finding.**
The implementer removed it because it would move the pinned test count by one. That trade is
backwards. The pinned counts are already pinned by two `--list` diffs, which is what
"the counts did not move" actually rests on, and a one-line baseline update is cheap.
Against that, Part C shows the proof was the **only** thing in the tree that could catch a
change to `STRING_CAST` or `DISTANCE_OP`. A proof that lives in a report is documentation,
not a regression test. See **B0-R1-1**.

---

## Part C: mutation testing

Every mutation was applied to the working tree, run, then reverted from the pre-mutation
snapshot with a `sha256sum` check. Eight mutations.

| # | Mutation | Result | Catching test(s) |
| --- | --- | --- | --- |
| M1 | `CockroachDialect::distance_to_score` body `(1.0 - 0.5*d*d)` → `(1.0 - d)` (the cosine-distance formula, i.e. the exact B2 confusion B3 warns about) | **CAUGHT** | `tests::distance_to_score_is_cosine` FAILED, and `tests::session_filter_keeps_only_caller_and_preserves_order` FAILED (920 passed / 2 failed) |
| M2 | `filter_session_rows::<D>` bypasses the dialect, inlining the *correct* formula | **survived (expected)** | Behaviour-identical by construction; recorded to establish the baseline for M2b |
| M2b | `filter_session_rows::<D>` bypasses the dialect with a *wrong* formula | **CAUGHT** | `tests::session_filter_keeps_only_caller_and_preserves_order` FAILED. So the `D::distance_to_score` wiring at this call site is genuinely pinned |
| M3 | The **other** `D::distance_to_score` call site (the exact-session fallback arm of `vector_candidates_checked`) given the wrong formula | **SURVIVED** | Nothing. 994 tests pass. The arm needs a live cluster; the tests that would see it are `#[ignore]`d. Pre-existing, not introduced by B0, recorded because §3.2 cites both call sites as covered |
| M4 | `CockroachDialect::vector_dim` returns `Ok(768)` instead of `ddl_vector_dim()` | **CAUGHT** | `tests::oversized_seed_embedding_dimension_fails_before_pool_use` FAILED and `store::tests::cockroach_build_behavior` FAILED. The width-from-DDL authority is pinned end to end, through `PgStore::new` into `vector_dimensions()` |
| M5 | `STRING_CAST` `"::STRING"` → `"::TEXT"` | **SURVIVED** | Nothing. 994 tests pass. See B0-R1-1 |
| M6 | `VECTOR_CAST` `"::VECTOR"` → `"::vector"` | **CAUGHT** | `tests::sql_shape_is_a_multi_row_upsert` FAILED and `tests::upsert_placeholder_shapes_match_structs` FAILED (via the `$15::VECTOR` assertions on the concept upsert) |
| M7 | `DISTANCE_OP` `"<->"` → `"<=>"` | **SURVIVED** | Nothing. 994 tests pass. This is the L2-to-cosine operator swap that, left paired with the unchanged `1 - d²/2` conversion, produces exactly the silent mis-ranking B3 calls "the dangerous one". See B0-R1-1 |
| M8 | The flush fencing gate disabled (`if false && !lease_permits_write(…)`) | **SURVIVED** | Nothing. 994 tests pass. Pre-existing: no offline test exercises the fence. Recorded so A6's evidence is not overstated |

**Mutation score: 4 of 8 caught.** The four survivors split into two pre-existing gaps
(M3, M8: both need a cluster) and two that the deleted byte-identity proof would have
caught (M5, M7). I verified the latter directly: with `DISTANCE_OP` set to `<=>`, the
composed `vector_candidates` no longer matches its pre-carve constant, so the
reconstructed test fails immediately.

---

## Part D: gates, re-run by me

All rows are my own runs on the reviewed tree, hashes verified immediately before the run.
Nothing is copied from `B0-implementation.md` or from `CYCLE.md`.

| Gate | rc | Result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | 0 | **pass** |
| `cargo clippy --all-targets -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | 0 | **pass** |
| `cargo clippy --all-targets --no-default-features --features store-cockroach -- -D warnings` | 0 | **pass** (extra, mine) |
| `cargo clippy --all-targets --no-default-features --features store-cockroach,fixtures -- -D warnings` | 0 | **pass** (extra, mine) |
| `cargo test --features store-cockroach` | 0 | **934 passed / 0 failed / 4 ignored**, 938 listed. Matches the pinned baseline exactly |
| `cargo test --no-default-features --features store-cockroach` | 0 | **597 passed / 0 failed / 0 ignored**, 597 listed. Matches |
| `cargo test --features store-cockroach,fixtures` | 0 | **994 passed / 0 failed / 12 ignored**, 1006 listed. Matches the baseline I measured myself from `git archive 7937de7` |
| leaf-name diff vs `b-baseline/tests-cockroach.txt` | 0 | **empty** |
| leaf-name diff vs `b-baseline/tests-nodefault-cockroach.txt` | 0 | **empty** |
| full-path diff, `store::cockroach::` → `store::pg::cockroach::`, all three listings | 0 | **empty** (938, 597, 1006) |
| H1 lock: `--lib h1_cross_store_parity` under `store-sqlite,store-cockroach,fixtures` | 0 | `h1_sqlite_and_memory_oracle_agree_exactly` passed; `git diff src/store/sqlite.rs` **empty** |
| `cargo doc --no-deps --features store-cockroach,fixtures` | 0 | 42 warnings, **none** naming `store/pg`. Identical count to the `7937de7` baseline |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 0 | **54 (baseline) → 56 (post-B0)**. Two new broken links, both in the moved code. See **B0-R1-2** |
| `#[ignore]`d live Cockroach tests | n/a | **7**, matching §6 |

**The baseline hole is real, and I measured it independently.** `CYCLE.md`'s two pinned
commands never enable `fixtures`, so `mod conformance` (~1,850 lines) and
`mod h2_cockroach_parity` (~900 lines) were never compiled by the pinned baseline. The
adapter's largest region was outside the gate that was supposed to be protecting it during
its own extraction. The implementer found this and closed it before I did, which is to
their credit; the run protocol should absorb the fix so B1 through B4 do not inherit the
hole. See **B0-R1-5**.

---

## Part E: findings

### B0-R1-1 (P2): the byte-identity proof was deleted, and two of B3's five dialect rows now have no pin at all

**Where**: `b-run/B0-implementation.md` §3.3; the absent test.

**What**: §3.3 says the composed SQL's byte-identity "was proved, not assumed", then says
the test "was then removed (it would have changed the pinned test count, and it is a
one-shot migration assertion, not a standing invariant)".

**Why it is wrong**: it is not a one-shot migration assertion. Part C shows, by mutation,
that with it gone:

* `CockroachDialect::STRING_CAST` can be changed from `"::STRING"` to `"::TEXT"` and all
  994 offline tests pass.
* `CockroachDialect::DISTANCE_OP` can be changed from `"<->"` to `"<=>"` and all 994
  offline tests pass.

`DISTANCE_OP` is the one B3 singles out: "The distance conversion is the dangerous one.
Getting it wrong does not fail: it ranks wrongly, quietly, and looks like a model quality
problem." B0's carve has just split that row into two items in two files, `DISTANCE_OP`
in the dialect and `distance_to_score` next to it, which must agree, and B2 is about to add
a second dialect where the correct pairing genuinely differs (`<=>` with `1 - d`). This is
the moment the pin is worth the most, and it is the moment it was removed.

The stated cost of keeping it is that the pinned test counts move by one. That cost is
mispriced: what actually carries the "nothing moved" claim is the two `--list` diffs, which
I reproduced and which are empty, and updating three baseline integers is a one-line edit.

**Fix**: land the proof permanently, in `pg/cockroach.rs`'s `mod tests` where the other
SQL-shape tests live, and update the pinned counts in `CYCLE.md` to 939 / 598 / 1007 with a
note saying which test was added and why. An equivalent pin is acceptable if it is at least
as strong: it must fail on a single whitespace change to any composed statement, and on a
change to any of `STRING_CAST`, `VECTOR_CAST`, `DISTANCE_OP`.

---

### B0-R1-2 (P3): the move broke two intra-doc links, and the doc gate the report cites cannot see them

**Where**: `src/store/pg/cockroach.rs:114`, `src/store/pg/mod.rs:940`.

**What**:

1. `cockroach.rs:114`: `/// T3.1 DDL, embedded and executed verbatim by [`PgStore::init_schema`].`
   rustdoc: "the struct `PgStore` has no field or associated item named `init_schema`".
   `init_schema` is a `GraphStore` trait method, and it resolved at `7937de7` because
   `CockroachStore` was a concrete struct; it does not resolve against the generic
   `PgStore`. This link was **edited by B0**, from `CockroachStore::init_schema`, and the
   edit is what broke it.
2. `mod.rs:940`: `/// [`CockroachStore::normalize_tokens`].` rustdoc: "no item named
   `CockroachStore` in scope". The doc comment moved out of the module where that name was
   in scope.

**Why it matters more than a typo**: §5's gate table records
`cargo doc --no-deps --features store-cockroach,fixtures` as showing "no warning naming
`src/store/pg`". That is true and I reproduced it, but only because both items are private
and plain `cargo doc` does not check private items' links. Under
`--document-private-items` the repo goes from 54 warnings at `7937de7` to 56 post-B0, and
the two new ones are exactly these. The cited gate is structurally incapable of seeing the
class of defect the move most easily introduces.

**Fix**: spell them `[`GraphStore::init_schema`]` (or drop the link) and
`[`cockroach::CockroachStore::normalize_tokens`]` (or `[`PgStore::normalize_tokens`]` if
that is where the method now lives). Consider adding the `--document-private-items` run to
the phase's gate list, since it is the only gate that can see doc rot in private code.

---

### B0-R1-3 (P3): `init_schema` and `connect_options` are over-merged by B3's own literal rule, and §3.4's check is narrower than the rule it cites

**Where**: `src/store/pg/mod.rs:2171` (inside `init_schema`), `src/store/pg/mod.rs:1257`
(inside `connect_options`).

**What**: B3's hard rule is "a function moves into `PgStore` only when its SQL is
**byte-identical** for both dialects". Two functions in the shared base fail it:

* `init_schema` executes
  `ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint STRING`. `STRING` is a
  Cockroach type name PostgreSQL does not have.
* `connect_options` sends `("vector_search_beam_size", …)` on every connection. That is
  CockroachDB's C-SPANN dial; PostgreSQL has no such setting.

§3.4 reports the over-merging check as "zero literal `::STRING`, `::VECTOR` or `<->` in any
SQL" plus "no `bool is_cockroach`, no `if cockroach`". I re-ran all of that and it holds:
no `TypeId`, no `dyn Any`, no `downcast`, no dialect-conditional `bool`. But those three
literals are not the rule; SQL identity is, and these two functions are the counterexamples
the narrower check walks past.

**Why P3 and not P2**: both are named in §4 (B0-N3, B0-N4), both carry a `B2/B3:` comment
at the site, and neither changes B0's behaviour, since B0 ships one dialect. The spec's own
reasoning also argues for leaving them: the shared subset is discovered by diffing two real
implementations, and `init_sql` cannot absorb the convergence ALTERs without turning
`init_schema` from three statements into one `raw_sql`, which would be a behaviour change
B0 must not make. That reasoning is correct and I am not asking for the split now.

**Fix**: change how they are described, not what they do. §4 presents all five items as a
flat list of "Cockroach-isms left inline", which reads as five error strings. Two of them
are functions in the shared base that the spec's hard rule says do not belong there. Say
that plainly, so B2 inherits "`init_schema` and `connect_options` are known over-merged and
owed a split" rather than "five strings to reword".

**On the implementer's own ranking**: they nominate B0-N1
(`unprovisioned_store_err("cockroach", …)` in `preflight_schema`) as highest-value. I
disagree, and I do not think any of the five has to move in B0. B0-N1 is an operator-facing
string that is *correct today* and would need a `Dialect::NAME` row that B3's table does
not have, which is precisely the speculative widening B3 warns against; it also has an
easier non-trait fix in B1 (carry `StoreConfig::kind` on `PgStore`). B0-N4 is the one with
teeth, because it is executed DDL rather than a message, and it is the one whose fix is a
genuine design question about where a generated schema's convergence ALTERs live.
B0-N5 is genuinely cosmetic and B0-N2 is genuinely test-pinned, so leaving both is right.

---

### B0-R1-4 (P3): B0-N2 names one site of `"invalid Cockroach DSN"`; there are two

**Where**: `src/store/pg/mod.rs:1201` (`PgStore::new`) and `src/store/pg/mod.rs:1234`
(`connect_options`).

**What**: §4's B0-N2 row attributes the string to `PgStore::new`. The identical string is
constructed a second time in `connect_options`, a different function on a different code
path (per-connection option building, not construction). B2 acting on the row as written
would fix one and leave the other.

**Fix**: add the second site to the B0-N2 row.

---

### B0-R1-5 (P3): `CYCLE.md`'s pinned baseline never compiled the larger half of the adapter; fix the protocol, not just this phase

**Where**: `dev-diary/lambo-for-mooshik/b-run/CYCLE.md`, "Baseline, pinned before B0".

**What**: both pinned test commands omit `fixtures`, and `mod conformance` and
`mod h2_cockroach_parity` are gated `#[cfg(all(test, feature = "store-cockroach", feature = "fixtures"))]`.
Roughly 2,750 of `cockroach.rs`'s 6,566 lines were outside the baseline. Independently
measured on a `git archive 7937de7` extraction: the missing gate is
`cargo test --features store-cockroach,fixtures` = **1006 listed, 994 passed / 0 failed /
12 ignored**, which the post-B0 tree matches exactly on all four numbers and on the full
test-path listing.

**Why it is graded**: the implementer found and closed this, which is the right outcome for
B0. But `CYCLE.md` is the document B1 through B4 will read, and it still pins two commands
that cannot see `mod conformance`. A protocol whose baseline does not compile the code
under refactor is a hole that recurs every phase.

**Fix**: add the `fixtures` row (and its two listing files) to `CYCLE.md`'s pinned baseline,
with the counts above, and a one-line note that `fixtures` is what compiles the conformance
and H2 modules.

---

### B0-R1-6 (P3): the "52 of the 938 entries" figure is wrong

**Where**: `b-run/B0-implementation.md` §2.2.

**What**: "52 of the 938 entries carry the renamed prefix; the other 886 are untouched."
The actual count is **26** of 938 (26 of 597, 36 of 1006), anchored or unanchored. 52
appears to be 26 + 26 summed across the two commands.

**Why it is worth a line**: the number is offered as a measure of how much of the listing
the rename touched, i.e. as evidence. The conclusion it supports is fine: I reproduced all
five listing diffs and every one is empty. Correct the figure so the evidence section is
uniformly checkable.

---

## Part F: hunt for defects introduced by the carve

I looked for the failure modes a passing suite hides. Two hits, both already graded above
(B0-R1-2). Everything else came back clean, and here is what I actually tried.

* **Path substitution changing meaning.** Every `super::X` → `crate::store::X` edit was made
  at a location whose `super` *was* `crate::store`, so the two paths named the same item by
  construction. There is no site where the substitution could resolve differently. Checked
  each of C3–C13 against the diffs.
* **Feature-gate coverage.** The `pg` module's cfgs are `#[cfg(feature = "fixtures")]`,
  `#[cfg(any(test, feature = "fixtures"))]`, `#[cfg(test)]`,
  `#[cfg(feature = "store-sqlite")]` and `#[cfg(all(test, store-cockroach, fixtures))]`. The
  interesting one is `DialectSql::upsert_session`, gated `any(test, fixtures)` while its
  only non-test consumer, `seed`, is gated `fixtures` alone: under `test` without `fixtures`
  the field exists and is read only by `mod tests`, and under `fixtures` without `test` it is
  read only by `seed`. Both directions compile clean. I ran six clippy combinations
  including two the implementer did not (`--no-default-features --features store-cockroach`
  and `…,fixtures`); all pass with `-D warnings`, so no arm is dead in any combination.
* **Visibility.** `pub mod pg` plus `pub use pg::cockroach` preserves
  `crate::store::cockroach::*` byte for byte and adds `crate::store::pg::*`. Since
  `src/lib.rs:95` is `pub mod store`, three items are newly public crate API:
  `pg::PgStore`, `pg::Dialect`, `pg::cockroach::CockroachDialect`. All three are required by
  the design (a generic store must be nameable and its bound must be public), the crate is
  pre-1.0 at 0.2.2, and `DialectSql` and the `sql`/`ddl`/`dialect` fields all stayed private.
  Not a finding; recorded because it is a real widening and B1's changelog entry should
  mention it alongside the `StoreKind` break.
* **Dead code clippy can no longer see.** Nothing: `-D warnings` passes in six feature
  combinations, and `crdb_sql` (the one new `#[cfg(test)]` helper) is used in both
  `--features store-cockroach` and `--no-default-features --features store-cockroach`.
* **Test weakening.** The three unit tests that used to assert against
  `SESSION_VECTOR_CANDIDATES_SQL`, `UPSERT_SESSION_SQL`, `SELECT_CONCEPTS_SQL` and
  `SELECT_INTERACTIONS_SQL`/`SELECT_EDGES_SQL` now assert against `crdb_sql()`, which
  composes from the real dialect. Same assertions, same substrings, now reading the string
  the store actually issues. Strictly not weaker.
* **Allocation on the hot path.** `DialectSql` is composed once in `PgStore::new` and held
  on the store; `vector_candidates` takes `&self.sql.vector_candidates` inside its
  grow-and-retry loop and `load_session` takes six references in one transaction. No
  per-query composition was introduced. `keyword_candidates_sql::<D>` was already a
  per-call builder; its one new allocation is a `format!` for a prefix that was previously a
  `push_str` of a literal, on a path the code's own comment calls the non-primary one. Not
  a finding, and the implementer declines to claim a benchmark, correctly.
* **The `#[cfg(test)] use super::*;` glob** in `cockroach.rs:111-112` is unusual (it makes
  the family's items visible to the test modules without re-listing them) but it is
  test-only, it does not shadow the unconditional `use super::{Dialect, PgStore};`, and it
  compiles clean under `-D warnings` in every combination.
* **The T3.2 design log.** I diffed the module doc: the log itself is byte-identical, and
  the only edits are the two link rewrites, the reflow of line 1, the deletion of the now-false
  clause "this module owns every statement", and the added paragraph saying where the code
  went. Honest, and the deleted clause was a correction rather than a loss.

---

## Summary of findings

| ID | Grade | Finding |
| --- | --- | --- |
| B0-R1-1 | **P2** | The byte-identity proof was deleted; `STRING_CAST` and `DISTANCE_OP` now have zero regression pins (mutation-proven) |
| B0-R1-2 | P3 | Two intra-doc links broken by the move, invisible to the doc gate §5 cites |
| B0-R1-3 | P3 | `init_schema` and `connect_options` are over-merged by B3's literal rule; §3.4's check is narrower than the rule |
| B0-R1-4 | P3 | §4's B0-N2 names one of two `"invalid Cockroach DSN"` sites |
| B0-R1-5 | P3 | `CYCLE.md`'s pinned baseline omits `fixtures` and never compiled `mod conformance` / `mod h2_cockroach_parity` |
| B0-R1-6 | P3 | The "52 of the 938 entries" figure is 26 |

**0 P1.** I tried hard to find one. What I attempted, and failed to break: the composed SQL
(rebuilt the proof from the baseline constants; byte-identical including whitespace, for
every statement, not a sample), the 27 surviving constants (mechanical diff, zero
differences), the fencing gate, the `ON CONFLICT` targets, `created_at`, the quarantine
predicate and `tx_retry` (whole-body diff, zero hunks over all of them), the width authority
(mutation caught by two tests), the distance conversion and its `filter_session_rows`
wiring (two mutations, both caught), the vector cast (mutation caught by two tests), path
resolution across the move, feature-gate coverage in six combinations, visibility, dead
code and doc links. The extraction is behaviour-neutral. What it no longer is, is *provably*
behaviour-neutral from inside the tree, and that is B0-R1-1.

**Tree state at close**: restored to the reviewed state and verified. All four SHA-256
values match the table in the header, and `git status --short` is identical to session
start. Nothing was committed or pushed. The only file I wrote is this one.

B0Review1, 2026-08-23
