# B — Postgres-family store (`pg` base, `postgres` + `cockroach` dialects)

**Goal:** the unified cross-machine store of spec §3.3, on real PostgreSQL with pgvector —
built as a shared Postgres-wire-protocol family, not as a fork of the Cockroach adapter.

**The lie to remove:** `StoreKind::from_str` (`src/store/mod.rs`) maps `"postgres"` and
`"pg"` onto `Cockroach`. The Cockroach adapter emits `VECTOR(1024)`, `CREATE VECTOR INDEX` and
`::STRING` casts that PostgreSQL does not have, so that alias has never meant what it says.

**No new dependency.** `store-cockroach` is already `["dep:sqlx", "sqlx/postgres"]` — both
databases speak the Postgres wire protocol through the *same* sqlx driver, same pool, same row
types. That is what makes a shared base cheap: it is not generic over drivers, only over dialect.

---

## Design decision, recorded (2026-08-19)

The original plan was copy-then-edit: fork the 4,900-line Cockroach adapter and change the
narrow dialect surface. **Rejected**, for three reasons:

1. **It makes Postgres a second-class citizen.** Every future SQL fix must be applied twice or
   drifts; F-R2-2 demonstrated how reliably "apply it everywhere" fails even for one struct
   field. And since B's CI gives Postgres a service container that runs on every push while
   `cockroach-live` needs secrets, the fork would soon be the better-tested copy while the
   original rots — the worst of both.
2. **The Cockroach code is battle-hardened** (it carried the hackathon and its reviews).
   Extraction transfers that pedigree to the whole family; forking walks away from it.
3. **Future wire-compatible stores** (Yugabyte, Neon, AlloyDB, Timescale) become a dialect file
   each, not a copy each.

**The shape — extract, then extend:**

```
src/store/pg/
  mod.rs        PgStore<D: Dialect> — implements GraphStore once; all shared machinery:
                upserts, fencing/lease, flush planning, session load, structural queries,
                quarantine, transaction discipline
  dialect.rs    the Dialect trait (compile-time, monomorphized; no dynamic dispatch)
  cockroach.rs  CockroachDialect + the existing include_str! DDL and width-from-DDL authority
  postgres.rs   PostgresDialect + width-templated DDL, hnsw from init
```

Naming: `pg` is the **family** (module, `PgStore`); `postgres` and `cockroach` are the
**implementations** (dialect files, config kinds, `StoreKind` variants). One collision to kill
in the module doc: the config alias `"pg"` means the PostgreSQL *implementation*; the module
`pg/` means the *family* — PostgreSQL, CockroachDB, and future wire-compatible stores.

**The over-merging trap, named so it is not walked into:** a function moves into `PgStore`
only when its SQL is **byte-identical** for both dialects. If it differs by one cast, it stays
in the dialect, even if a `bool` parameter could force it into one body — a base full of
`if cockroach` branches recreates the drift problem inside the shared code, where it is harder
to see. Two dialects is the right number to extract from: the shared subset is *discovered* by
diffing two real implementations, not speculated from one.

---

## B0 — Extraction

Move-only, then carve:

1. **Move-only commit.** `store/cockroach.rs` → `store/pg/` with zero behaviour change,
   provably: the no-default cockroach test suite, the conformance gate, and clippy all pass
   untouched. No `Dialect` trait yet.
2. **Carve commits.** Introduce `Dialect`; replace one inline Cockroach-ism per commit with a
   dialect call; suite green at every step. The live `#[ignore]`d tests re-run on a DSN-bearing
   machine at the end of the carve (machines exist; not a blocker).

**H1 is the behavioural lock for this refactor** — the cross-store parity harness
([H](H-cross-store-parity.md)) pins recall behaviour before the carve starts, in addition to
the existing conformance suite. Soft edge: H1 → B0.

**Depends on:** nothing hard; H1 soft.

---

## B1 — `StoreKind::Postgres` and the alias split

New variant, feature `store-postgres`, and the clean separation:

| Config string | Resolves to |
| --- | --- |
| `"postgres"`, `"pg"` | `PgStore<PostgresDialect>` |
| `"cockroach"`, `"crdb"` | `PgStore<CockroachDialect>` |

No string maps across the boundary in either direction.

**This is a behaviour change, not an addition.** Two tests assert the current mapping, and any
deployment configured `kind = "postgres"` against a CockroachDB cluster changes meaning on
upgrade. It is safe because it fails **loud, not wrong**: the Postgres dialect's
`CREATE EXTENSION vector` and `<=>` operator do not exist on Cockroach, so the misconfiguration
dies at provision or first vector query with a clear error — it can never silently mis-rank.
Record the decision in the variant's doc comment (the next person reading
`"postgres" | "pg" => Cockroach` in git history must see a choice, not a bug), update the two
tests deliberately, update the three "expected memory | cockroach | sqlite" error strings, and
note the break in the 0.3.0 changelog.

**Depends on:** B0 (the variant constructs a dialect that must exist).

---

## B2 — PostgresDialect DDL: templated width, hnsw from init

`PostgresDialect::init_sql(dim)`: pgvector schema at a width taken from config, **with the
hnsw index created in the same init** — decided 2026-08-19:

- **hnsw from day one, no later migration event.** Behavioural stability outranks early
  exactness: the system a user starts with is the system they keep. Introducing approximation
  later via migration — onto a store that by then holds important data — is the worse failure
  mode; if hnsw disappoints, that is discovered early, on unimportant data. Consequence for
  parity: the Postgres leg of H is **envelope-based from day one** (hnsw is approximate), with
  a **forced-exact lane** (`SET LOCAL enable_indexscan = off`) so adapter skew is still
  detected exactly — approximation must come from the index, never from the dialect's SQL.
- **ivfflat rejected**, recorded so it is not relitigated: ivfflat clusters at index-build
  time and wants a populated table; Lambo's tables start empty and grow incrementally, so
  ivfflat centroids would be built on nothing and recall would quietly degrade as the data
  distribution drifts. hnsw builds incrementally and does not care when the data arrived.
- **The hnsw dimension ceiling is a real trap:** pgvector's hnsw index supports at most
  **2000 dimensions** on the `vector` type. 768 and 1536 pass; **Gemini's 3072 does not** —
  and A's dim guard explicitly allows 3072. The dialect must handle this **at init, loudly**:
  either refuse dim > 2000 naming the ceiling and the `halfvec` escape hatch, or implement the
  `halfvec` path. Decide at implementation and record here; never let index creation be the
  thing that discovers it.
- Index parameters: pgvector defaults (`m=16`, `ef_construction=64`, `ef_search=40`). No
  config knobs until a workload demands them.

**The Cockroach pattern cannot be copied for the width.** Cockroach's DDL is `include_str!`'d
and `schema_vector_dim` parses the width back *out* — the schema file is the authority. A
configurable width must be substituted *into* the SQL, inverting the data flow. Two shapes,
now scoped to `PostgresDialect::init_sql` alone (Cockroach's dialect keeps its static file and
parse-out authority):

1. **Template at init** — one `001_init.sql` with a placeholder, substituted in `init_schema`.
   Simple; the file on disk is no longer valid SQL.
2. **Generate the DDL in code** — width is a parameter, SQL built in Rust. Honest; loses the
   "schema file is the contract" property.

Whichever is chosen, `vector_dimensions()` keeps a single authority (B4), and the choice gets
recorded here.

**Depends on:** B0, B1.

---

## B3 — The dialect surface

Exactly this table lives in the `Dialect` trait; everything else is shared:

| Dialect method | Cockroach | PostgreSQL + pgvector |
| --- | --- | --- |
| `init_sql(dim)` | static `include_str!`, dim asserted against parse | templated width + hnsw index (B2) |
| `string_cast` | `::STRING` | `::TEXT` |
| vector cast | `::VECTOR` | pgvector's own cast |
| `distance_op` / `distance_to_score` | `<->` is L2, score `1 − d²/2` | `<=>` is cosine distance, score `1 − d` |
| width authority | `VECTOR(n)` DDL parse | B2's config authority |

**The distance conversion is the dangerous one.** Getting it wrong does not fail — it ranks
wrongly, quietly, and looks like a model quality problem. The unit-norm `Embedder::embed`
output contract (documented under F) is what makes `1 − d²/2 ≡ cosine` hold; carry the
reasoning across, not just the formula. **H3 is this row's verification**: the H harness's
score-agreement measure catches a fumbled conversion as systematic score skew at zero
candidate divergence — B3's parity box *is* H3, not a re-specified ad-hoc check.
(H1 landed: `src/store/sqlite.rs`'s `h1_cross_store_parity` test module and its
`evidence/mooshik-h1-cross-store-parity/` report — extend `build_adapters` there for the
Postgres leg rather than writing a second harness; see H-cross-store-parity.md.)

Preserve, unchanged and shared in `PgStore`: the fencing token on `flush` (a write below the
session lease's `current_token` refused with `StaleWrite`, never dropped), idempotent upsert
semantics so a replayed batch converges, the documented `created_at` divergence, and the
NULL-only quarantine predicate — both dialects have DDL width enforcement, so SQLite's
stronger restamp-quarantine reasoning does not apply here; that stays written down rather than
silently inherited.

**Depends on:** B2.

---

## B4 — `vector_dimensions()` from config

Report the configured width so `check_vector_search_contract` has its capability/width pairing
(a store must not claim `VECTOR_SEARCH` without a concrete width, nor report a width without
the capability).

> **The config key now exists — consume it, do not re-decide the authority.**
> Added under **F remediation** (finding F-R1-2, orchestrator-approved 2026-08-19):
> `StoreConfig::vector_dim: Option<usize>`, i.e. the TOML key `[store] vector_dim`, serde-defaulted
> like its siblings. Semantics as shipped, which B4 inherits rather than re-litigates:
>
> * It is an **operator-asserted pre-ingest pin** — an assertion about the width this deployment's
>   vectors use, not a preference.
> * Precedence in `build_store_with_vector_dim` is `cfg.vector_dim.or(param)`, then the
>   `EmbedderConfig` default. So a pin outranks the resolved `[embedder] dim`, which outranks the
>   default.
> * `resolve_backends` **refuses to resolve** when the pin disagrees with the resolved
>   `[embedder] dim`, naming both numbers. That refusal lives at the **serving verbs' resolution
>   boundary, not in store construction** — a migration verb (a future `lambo reembed`) must still
>   be able to open a store whose sessions carry a different contract in order to rewrite them. It
>   is an **explicit comparison written inline in `resolve_backends`**, and it is deliberately
>   **kind-agnostic**: a stale pin refuses a Postgres/Cockroach/`memory` resolve too (F-R2-4).
> * `check_vector_compatibility` is **not** what performs that refusal, and a pin does not make it
>   non-vacuous. A width-agnostic store's `vector_dimensions()` echoes the embedder width with no
>   pin and echoes **the pin itself** once one is set, so the comparison is `x == x` either way; the
>   pin check runs first and returns first. Describe it as an echo for a width-agnostic store, full
>   stop (F-R2-3).
>
> Postgres, like Cockroach, will have a real `VECTOR(n)`/`vector(n)` DDL authority once B2 lands its
> configurable width. Where a DDL width exists it **outranks the pin** for *reporting* (Cockroach
> reports its DDL number and ignores the key there) — but not at the resolution boundary, where the
> kind-agnostic pin check still fires. So B4's job is to report the schema number and leave the pin's
> *reporting* role to the width-agnostic adapters, without assuming the pin is inert on a
> DDL-carrying store. What B4 should still add is the check the DDL makes possible and SQLite cannot
> have: that the *initialized* schema width matches config, verified against the live database rather
> than echoed from the same config value.

**Depends on:** B2.

---

## Done when

- [ ] B0: Cockroach behaviour byte-identical after extraction — no-default suite, conformance
      gate, and clippy unchanged across the move-only commit; live `#[ignore]`d tests re-run
      green on a DSN-bearing machine after the carve
- [ ] `kind = "postgres"` reaches a real Postgres, `"cockroach"` still reaches Cockroach, and
      the cross-misconfiguration fails loud at provision or first vector query
- [ ] Schema initializes at a width taken from config, at more than one width, **with the hnsw
      index present from init** and a dim > 2000 handled loudly per B2's recorded decision
- [ ] An `EXPLAIN` capture proves the hnsw index is actually used by the recall query (the
      camera-proof analogue Cockroach has)
- [ ] Parity via **H3**: forced-exact lane shows zero adapter skew; hnsw lane's divergence
      stated as a measured envelope
- [ ] Fencing-token refusal and flush-replay idempotency both proven on the new dialect
- [ ] `store-postgres` matrix row, plus a `postgres-live` job using a **service container**
      (`pgvector/pgvector`) rather than a provisioned cluster: no secret, no cost, and it runs
      on every push instead of being a tier someone remembers to check
