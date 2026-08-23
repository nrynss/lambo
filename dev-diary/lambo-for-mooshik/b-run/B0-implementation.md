# B0: extraction, implementation record

**Phase:** B0 (workstream B, `lambo-for-mooshik`).
**Baseline commit:** `7937de7`.
**Scope shipped:** the move, plus the `Dialect` carve with **exactly one** dialect
implemented (Cockroach). No `StoreKind::Postgres` (B1), no `PostgresDialect` (B2), no
`postgres.rs` stub.
**Tree left dirty and uncommitted**, per the run protocol.

---

## 1. Final module layout

| File | Lines | Contents |
| --- | ---: | --- |
| `src/store/pg/mod.rs` | 2886 | `PgStore<D: Dialect>`, the single `impl GraphStore`, `DialectSql`, and all shared machinery: pool, `tx_retry`, flush planning + fencing gate, session load, structural queries, quarantine, lease, write intents, statement helpers, row mappers |
| `src/store/pg/dialect.rs` | 77 | the `Dialect` trait, and nothing else |
| `src/store/pg/cockroach.rs` | 3956 | the T3.2 design log (module doc), `INIT_SQL`, `CockroachDialect`, `schema_vector_dim` / `ddl_vector_dim` (width authority), `pub type CockroachStore`, and the three Cockroach test modules (`tests`, `conformance`, `h2_cockroach_parity`) |
| | **6919** | was 6566 in one file; the growth is the trait, the `DialectSql` struct, and the doc/`B2/B3:` comments |

`src/store/mod.rs` is the only other file touched: `pub mod cockroach;` became
`pub mod pg;` plus `pub use pg::cockroach;`. **No file outside `src/store/` changed.**
In particular `src/canon/eval.rs:2638`'s `use crate::store::cockroach::CockroachStore;`
still resolves, through that re-export, and compiles under
`clippy --all-targets --features store-cockroach`.

`git status` records the move as a rename (`R src/store/cockroach.rs ->
src/store/pg/cockroach.rs`), so history follows.

---

## 2. Stage 1: the move-only change

`git mv src/store/cockroach.rs src/store/pg/cockroach.rs`, plus a new `src/store/pg/mod.rs`
that at this stage contained only a module doc and `pub mod cockroach;`.

### 2.1 Every forced edit, enumerated

**`src/store/mod.rs`** (2 edits, both inside `src/store/`):

| # | Edit | Why forced |
| --- | --- | --- |
| M1 | `pub mod cockroach;` → `pub mod pg;` | the file no longer sits at `src/store/cockroach.rs`; without this the module does not exist |
| M2 | added `pub use pg::cockroach;` | `crate::store::cockroach::CockroachStore` is used from `src/canon/eval.rs`, outside `src/store/`. Re-exporting the module keeps that path byte-identical, which is what "no caller outside `src/store/` has to change" requires. The alternative was editing `canon/eval.rs`, i.e. a caller change |

**`src/store/pg/cockroach.rs`** (14 changed lines; the stage-1 diffstat was exactly
`14 insertions(+), 14 deletions(-)`):

| # | Line (pre-move) | Edit | Why forced |
| --- | --- | --- | --- |
| C1 | 128 | `include_str!("../../migrations/...")` → `"../../../migrations/..."` | `include_str!` is relative to the containing file; the file gained one directory of depth. Without it the build fails |
| C2 | 4 | module doc `[`super::build_store`] for [`super::StoreKind::Cockroach`]` → `[`crate::store::build_store`] for [`crate::store::StoreKind::Cockroach`]` | `super::` now names `crate::store::pg`, which has neither item: the intra-doc links break |
| C3 | 107 | `use super::batch::{...}` → `use crate::store::batch::{...}` | depth change; `super::batch` no longer exists |
| C4 | 112 | `use super::batch::{seed_concept_rows, seed_edge_rows};` → `crate::store::batch::` | same |
| C5 | 113 | `use super::lease::{...}` → `use crate::store::lease::{...}` | same |
| C6 | 114 | `use super::vector::{...}` → `use crate::store::vector::{...}` | same |
| C7 | 115 | `use super::{columns_in_ddl, ...}` → `use crate::store::{...}` | same |
| C8 | 905 | comment `the shared `super::vector` module` → `crate::store::vector` | the comment names a module path that no longer resolves from here |
| C9 | 2960 | the same module-path name in the vector-codec comment inside `mod tests` | same |
| C10 | 1156 | struct doc `[`super::build_store`]` → `[`crate::store::build_store`]` | broken intra-doc link, as C2 |
| C11 | 3215 | `kind: super::super::StoreKind::Cockroach` → `crate::store::StoreKind::Cockroach` | inside `mod tests`, `super::super::` now names `crate::store::pg` |
| C12 | 3876 | same edit, in `mod conformance` | same |
| C13 | 4274 | `super::super::build_store(...)` → `crate::store::build_store(...)` | same |
| C14 | 6439 | doc-comment run recipe `--lib store::cockroach::h2_cockroach_parity` → `store::pg::cockroach::h2_cockroach_parity` | the documented command is a test-name **filter**; after the move `store::cockroach::` matches nothing, so the recipe would silently run zero tests |

**Choice of spelling, stated because it is a choice.** C3–C7 and C11–C13 could have been
`super::super::` / `super::super::super::` instead of absolute `crate::store::`. Absolute
was chosen because stage 2 moves most of this code from `pg/cockroach.rs` (whose `super`
is `crate::store::pg`) into `pg/mod.rs` (whose `super` is `crate::store`), so relative
paths would have had to change twice. The absolute form is depth-proof, and the file
already used `crate::store::lease::…` and `crate::store::StoreKind::…` elsewhere, so it
is not a new idiom. Every one of these edits is a path adjustment; none changes a value,
a statement, or a control flow.

Nothing else was touched in stage 1.

### 2.2 The empty leaf-name diffs

Leaf name = last `::`-separated component of each `--list` entry, sorted.

```
$ diff <(leaf $SCRATCH/b-baseline/tests-cockroach.txt) <(leaf list-cockroach.txt)
$ echo $?
0
```

```
=== leaf diff default + store-cockroach ===
(empty)
=== leaf diff --no-default-features + store-cockroach ===
(empty)
```

Counts: **938** and **597**, matching the pinned baseline exactly.

The proof is in fact stronger than "leaf names match". The **full** paths differ only by
the single module rename, and nothing else:

```
=== full-path diff after rewriting store::cockroach:: -> store::pg::cockroach:: ===
(empty)      # default features + store-cockroach   (938 entries)
(empty)      # --no-default-features + store-cockroach (597 entries)
```

52 of the 938 entries carry the renamed prefix; the other 886 are untouched.

---

## 3. Stage 2: the carve

### 3.1 The `Dialect` trait, as implemented

`src/store/pg/dialect.rs`, verbatim signatures:

```rust
pub trait Dialect: Send + Sync + 'static {
    fn init_sql(dim: usize) -> Result<Cow<'static, str>, StoreError>;
    const STRING_CAST: &'static str;
    const VECTOR_CAST: &'static str;
    const DISTANCE_OP: &'static str;
    fn distance_to_score(dist: f64) -> f64;
    fn vector_dim(cfg: &StoreConfig) -> Result<usize, StoreError>;
}
```

Six items, which is exactly the five rows of B3's table (the `distance_op` /
`distance_to_score` row is two items by the spec's own naming). Nothing else.

Notes on the two signature decisions that were not forced by the table:

* **`Cow<'static, str>` from `init_sql`.** Cockroach returns `Cow::Borrowed(INIT_SQL)`,
  the exact `include_str!`, so B0 introduces no allocation and no copy on the DDL path.
  B2's templated width returns `Cow::Owned`. A plain `&'static str` would have forced B2
  to leak or to re-introduce a per-dialect type; a plain `String` would have made
  Cockroach copy 10 KB of DDL on every construction for nothing.
* **`Send + Sync + 'static` supertraits.** These are properties of the marker type, not
  behaviour. `PgStore<D>` carries `PhantomData<D>` and is handed out as
  `Box<dyn GraphStore>`, which requires `Send + Sync`. They are not extra methods.

`CockroachDialect` is a zero-sized marker; `pub type CockroachStore =
PgStore<CockroachDialect>;` keeps the public name and `new(cfg)` signature.

### 3.2 Each method, and the call sites it replaced

| Method | Replaced |
| --- | --- |
| `init_sql(dim)` | the four uses of the `INIT_SQL` constant in the store: `PgStore::new` (now stores the result in a `ddl` field), `init_schema`'s `sqlx::raw_sql(...)`, and `preflight_schema`'s `tables_in_ddl(...)` and `columns_in_ddl(...)`. Cockroach's impl asserts `dim` against the DDL parse and returns the borrowed `include_str!` |
| `STRING_CAST` | **20** literal `::STRING` occurrences across 11 statements: `vector_candidates` (2), `session_vector_candidates` (1), `upsert_session` (2), `set_embedding` (2), `select_session` (1), `select_interactions` (2), `select_concepts` (3), `select_edges` (3), `select_canonization_events` (2), `select_reservations` (1), and `keyword_candidates_sql`'s SELECT prefix (1) |
| `VECTOR_CAST` | the 3 live `::VECTOR` literals: the two vector-candidate statements' `$1::VECTOR`, and `concept_upsert_query`'s `push_unseparated("::VECTOR")` (now `push_unseparated(vector_cast)`, threaded as `DialectSql::vector_cast`) |
| `DISTANCE_OP` | the 2 live `<->` operators, in `vector_candidates` and `session_vector_candidates` |
| `distance_to_score` | the free `fn distance_to_score` and both its call sites: `filter_session_rows` (now `filter_session_rows::<D>`) and the exact-session fallback arm of `vector_candidates_checked`. The formula, the clamp, and the unit-norm reasoning moved into `CockroachDialect` verbatim, with the `d² = 2 − 2·(a·b)` derivation written out rather than asserted |
| `vector_dim(cfg)` | `PgStore::new`'s `schema_vector_dim(INIT_SQL)` parse and its exact error string. `schema_vector_dim` itself moved to `cockroach.rs`. Cockroach ignores `cfg` and says so in its doc, per B4's recorded semantics ("Cockroach reports its DDL number and ignores the key there") |

### 3.3 How the shared base gets its SQL: `DialectSql`

The 10 cast-bearing statements are written **once**, in `pg/mod.rs`, as `format!`
templates over `{s}` / `{v}` / `{op}`, and composed once per store into a `DialectSql`
value held on `PgStore`. They are not composed per query: `vector_candidates` re-issues
its statement inside the grow-and-retry loop and `load_session` issues six of them in one
transaction, so per-call composition would have put an allocation on the hot recall path
that the constants did not have. `keyword_candidates_sql::<D>` stays a per-call builder
because it already was one (its shape depends on the token count).

Three free helpers take `&DialectSql` rather than a type parameter, because what they
need is a *string* rather than a *function*: `bulk_upsert_concepts`, `apply_step`,
`apply_single`. Two take the type parameter, because what they need is the score
conversion or a cast token at build time: `filter_session_rows::<D>`,
`keyword_candidates_sql::<D>`. No dynamic dispatch and no function pointers are
introduced anywhere.

**Byte-identity of the composed SQL was proved, not assumed.** A temporary test asserted
each of the 11 composed statements plus the `vector_cast` token against the exact
pre-carve literal, copied from the baseline file:

```
test store::pg::cockroach::tests::b0_composed_sql_is_byte_identical_to_the_pre_carve_constants ... ok
```

The test was then removed (it would have changed the pinned test count, and it is a
one-shot migration assertion, not a standing invariant). A reviewer can reconstruct it
from `git show 7937de7:src/store/cockroach.rs` in about five minutes; the source
literals are unchanged in that commit.

### 3.4 The over-merging trap: what was checked

After the carve, `src/store/pg/mod.rs` contains **zero** literal `::STRING`, `::VECTOR`
or `<->` in any SQL. The three remaining textual occurrences are `D::STRING_CAST` /
`D::VECTOR_CAST` / `D::DISTANCE_OP` references and one design-log comment that quotes
Cockroach's spelling as an example. There is **no** `bool is_cockroach` parameter and
**no** `if cockroach` branch anywhere in the shared base. Where a Cockroach-ism could not
be removed without widening the trait beyond B3's table, it was left inline and marked
(§4).

### 3.5 Invariants preserved, and how that is visible

A whole-body diff of the pre-carve non-test region against the post-carve
`pg/mod.rs` + `pg/cockroach.rs` dialect block produces 37 hunks, every one of which is
one of the transformations listed above. Reproduce it with:

```sh
git show 7937de7:src/store/cockroach.rs | sed -n '126,2841p' > /tmp/base-body.rs
# then diff against pg/cockroach.rs's dialect block + pg/mod.rs's body
```

The four invariants named in the brief are covered by that diff producing **no hunk at
all** over their code:

* **Fencing token on `flush`.** The `batch_session_ids` loop, the
  `SELECT current_token FROM session_leases` read inside the same transaction, the
  `lease_permits_write(cur, token)` guard and the `StoreError::StaleWrite` return are
  byte-identical, including the message text and the GitHub-issue reference. The same is
  true of the second gate in `record_canonization`. Refused, never dropped: the `?` still
  drops `tx`, rolling back.
* **Idempotent upsert semantics.** `ON CONFLICT (id) DO UPDATE SET …` for interactions
  and concepts, `ON CONFLICT (source, target, edge_type)` for edges, `ON CONFLICT (id) DO
  NOTHING` for canonization events, `ON CONFLICT (session_id, receipt)` for write
  intents: none of these statements carries a cast, so none of them moved into
  `DialectSql`, and none has a diff hunk. R2-1's "canonization columns are insert-only on
  the concept upsert" is likewise untouched.
* **The documented `created_at` divergence.** `COALESCE($3, now())` in the seed upsert
  and `created_at = EXCLUDED.created_at` in the flush upsert are unchanged; the
  `UPSERT_SESSION_ROW_SQL` bare-row insert still relies on the DDL default.
* **The NULL-only quarantine predicate.** `QUARANTINE_LEGACY_EMBEDDINGS_SQL` is
  cast-free, unchanged, and still fires only on `embedding_kind IS NULL AND embedding_dim
  IS NULL`. Its doc comment recording the deliberate divergence from SQLite (F-R2-1) and
  the reason the DDL width makes SQLite's wider rule unnecessary here is preserved
  verbatim, including its forward pointer to B2.

**H1 was not edited.** `git diff --stat src/store/sqlite.rs` is empty, and
`h1_sqlite_and_memory_oracle_agree_exactly` passes.

### 3.6 One test-side behaviour improvement worth flagging to the reviewer

`vector_explain_camera_proof` and `assert_index_backed` used to `EXPLAIN` the
`VECTOR_CANDIDATES_SQL` **constant**, deliberately, so the camera proof could not drift
from the production query (adve-review MINOR-4). There is no constant now. Rather than
re-spell the query in the test (which is precisely what MINOR-4 rejected), both sites now
read `store.sql.vector_candidates`, the statement **that store instance** issues. That
is strictly stronger than the constant was: it follows a change of dialect as well as a
change of SQL. Both tests are `#[ignore]`d live tests, so this change is compiled but
unrun here (§6).

---

## 4. Deliberately **not** in the trait: B2/B3 inputs

B0 ships one dialect, and B3's rule is that the shared subset is discovered by diffing
two real implementations rather than guessed from one. Each item below is a place where I
am confident PostgreSQL differs, and each was left inline in the shared base with a
`B2/B3:` comment at its site rather than turned into a trait method.

| # | Site (`src/store/pg/mod.rs`) | What is Cockroach-specific | Why it is not a trait row, and what B2 must decide |
| --- | --- | --- | --- |
| B0-N1 | `preflight_schema`, both arms | `unprovisioned_store_err("cockroach", …)` and `unprovisioned_column_err("cockroach", …)` | This is a `Dialect::NAME` row, and B3's table has none. It is **operator-facing**: a misprovisioned Postgres deployment would today be told "cockroach". B2 should add the row or pass the name through from `StoreKind`. Highest-value of the five |
| B0-N2 | `PgStore::new` | `"CockroachStore requires a DSN (store.dsn or LAMBO_COCKROACH_DSN)"` and `"invalid Cockroach DSN: {e}"` | Same shape as B0-N1. Left byte-identical on purpose: two tests assert this exact text, so changing it in B0 would have been a behaviour change |
| B0-N3 | `connect_options` | `("vector_search_beam_size", …)`, plus `DEFAULT_VECTOR_BEAM_SIZE`, `LAMBO_VECTOR_BEAM_SIZE` and its `1..=2048` bound | CockroachDB's C-SPANN accuracy dial. PostgreSQL + pgvector has no such setting; its hnsw analogue is `hnsw.ef_search` with different bounds and a different measured default. An ANN-tuning row is not in B3's table. **This one interacts with B2's hnsw decision and with H3's forced-exact lane**, so it should be decided together with them, not bolted on |
| B0-N4 | `init_schema`, the second convergence `ALTER` | `ADD COLUMN IF NOT EXISTS endpoint STRING`, where `STRING` is a Cockroach type name PostgreSQL does not have | These two post-DDL convergence ALTERs are deliberately **not** part of `init_sql`: folding them in would turn `init_schema` from `raw_sql(DDL)` + two `query()` calls into one `raw_sql`, which is a behaviour change B0 must not make. B2 must decide where the convergence ALTERs live for a dialect whose schema is generated rather than shipped |
| B0-N5 | `tx_retry`'s exhaustion error | `"transaction retry exhausted (Cockroach serializable conflict)"` | Cosmetic only: PostgreSQL aborts with the same SQLSTATE 40001 under `SERIALIZABLE`, so the retry *mechanism* is genuinely shared and only the wording is wrong |

Two more, recorded as observations rather than defects:

* **`STRING_CAST = "::STRING"` is preserved rather than normalised to `::TEXT`.** Cockroach
  accepts `::TEXT` as an alias, so a single shared `::TEXT` would in fact have worked for
  both dialects and made this row unnecessary. B0 did not take that route: it is a
  behaviour change (different SQL text reaching a live cluster) with no local test that
  can see it, and B3's table asks for the row. **Flagging it explicitly because it is the
  one place where B3's table may be one row wider than it needs to be**, and B2 is where
  that can be measured against a real Postgres rather than argued.
* The T3.2 design log in `pg/cockroach.rs`'s module doc describes code that now lives in
  `pg/mod.rs`. B0 moved the code without rewriting the reviewed prose, and added a
  paragraph at the top saying so. If a future phase wants the log split, that is a
  documentation task with its own review, not a side effect of an extraction.

**Nothing in the B-postgres-store.md spec was found to be wrong.** The one place I would
have chosen differently is noted above (`::TEXT`), and it is a question for B2, not a
correction to B3.

---

## 5. Gates

Every row below was run by me in `/home/nryn/work/lambo` on the final tree. Nothing is
quoted from the baseline document without having been re-run.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **pass** |
| `cargo clippy --all-targets -- -D warnings` | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | **pass** |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | **pass** (extra) |
| `cargo clippy --all-targets --features store-sqlite,store-cockroach,fixtures -- -D warnings` | **pass** (extra) |
| `cargo test --features store-cockroach -- --list` | **938** entries; sorted-leaf-name diff vs `tests-cockroach.txt` **empty**; full-path diff empty after rewriting the one module prefix |
| `cargo test --no-default-features --features store-cockroach -- --list` | **597** entries; sorted-leaf-name diff vs `tests-nodefault-cockroach.txt` **empty**; full-path diff empty after the same rewrite |
| `cargo test --features store-cockroach` | **934 passed / 0 failed / 4 ignored** |
| `cargo test --no-default-features --features store-cockroach` | **597 passed / 0 failed / 0 ignored** |
| `cargo test --features store-cockroach,fixtures -- --list` | **1006** entries; leaf and full-path diffs **empty** against a baseline computed from `git archive 7937de7` (extra, see below) |
| `cargo test --features store-cockroach,fixtures` | **994 passed / 0 failed / 12 ignored**, identical to the same command on the pristine baseline tree (extra) |
| `cargo test --features store-sqlite,store-cockroach,fixtures` | **1089 passed / 0 failed / 12 ignored** (extra) |
| H1 lock: `cargo test --features store-sqlite,store-cockroach,fixtures --lib h1_cross_store_parity` | `h1_sqlite_and_memory_oracle_agree_exactly` **passed**; `git diff --stat src/store/sqlite.rs` **empty** |
| `cargo doc --no-deps --features store-cockroach,fixtures` | **no warning naming `src/store/pg`** (the repo's pre-existing private-link warnings elsewhere are unchanged) |

**Why the three `fixtures` rows were added, and why they matter.** `mod conformance`
(~1850 lines) and `mod h2_cockroach_parity` (~900 lines) are gated on
`#[cfg(feature = "fixtures")]`, and `fixtures` is in neither the default feature set nor
the two pinned baseline commands. **The pinned baseline therefore never compiles more
than half of `cockroach.rs`.** I extracted commit `7937de7` with `git archive` into a
scratch directory, built it against its own target dir, and captured the missing
baseline: 1006 listed, 994 passed / 0 failed / 12 ignored. The post-B0 tree matches on
all three numbers and on the full test-path listing. This is recorded as a gap in the
run's pinned baseline, not as a criticism of it.

---

## 6. Not verified

* **The 7 `#[ignore]`d live Cockroach tests are unrun.** They are `#[ignore]`d in
  `src/store/pg/cockroach.rs` and require `LAMBO_COCKROACH_DSN`, which is not present here;
  `cockroach-live` is gated off on this branch. They compile (all three test-module bodies
  are built under the `fixtures` gates above) and their bodies are unchanged apart from the
  path adjustments in §2.1 and the two `EXPLAIN` sites in §3.6. Per the spec, they re-run on
  a DSN-bearing machine at the end of the carve. Until then, **nothing in this report is
  evidence that the extracted adapter still works against a real cluster**, only that it
  emits byte-identical SQL and passes every offline gate.
* Specifically unverified by consequence: `init_schema` idempotency, the column preflight
  against a live `information_schema`, the vector `EXPLAIN` camera proof (which now reads
  `store.sql.vector_candidates`; the change is mechanical but *unrun*), the H2 parity
  harness, and every conformance check that needs a cluster.
* **`Dialect::init_sql`'s mismatch arm is unreachable for Cockroach and therefore
  untested.** `vector_dim` and `init_sql` both read the same DDL, so `dim != ddl_dim`
  cannot fire today. It becomes meaningful in B2, where the width is configurable, and it
  is where a wrong resolution should die.
* **No test was added for the `Dialect` trait itself.** That is deliberate: adding one
  would have moved the pinned test counts, and B0's central claim is that the counts and
  leaf names did not move. Every one of the 938 tests now runs through
  `PgStore<CockroachDialect>`, so the dialect is exercised, but there is no test that would
  catch a *second* dialect being wired wrong. That test belongs in B2, where a second
  dialect exists to compare against.
* Performance is not measured. `PgStore::new` now composes ten strings at construction
  (once per process); the per-query paths allocate no more than before, and
  `keyword_candidates_sql` allocates exactly as much as before. No benchmark was run to
  confirm this, and none is claimed.
