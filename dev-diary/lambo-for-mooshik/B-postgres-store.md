# B — Postgres store (`store-postgres`)

**Goal:** the unified cross-machine store of spec §3.3, on real PostgreSQL with pgvector.

**The lie to remove:** `StoreKind::from_str` (`src/store/mod.rs:436`) maps `"postgres"` and
`"pg"` onto `Cockroach`. The Cockroach adapter emits `VECTOR(1024)`, `CREATE VECTOR INDEX` and
`::STRING` casts that PostgreSQL does not have, so that alias has never meant what it says.

**No new dependency.** `store-cockroach` is already `["dep:sqlx", "sqlx/postgres"]` — sqlx's
Postgres driver compiles in today. This is a dialect split, not a second 4,900-line adapter.

---

## B1 — `StoreKind::Postgres`

New variant, feature `store-postgres`, and remap the aliases so `"postgres"` / `"pg"` stop
resolving to Cockroach.

**This is a behaviour change, not an addition.** Two tests assert the current mapping
(`src/store/mod.rs:636` and `:658`), and any deployment configured `kind = "postgres"` against a
CockroachDB cluster changes meaning silently on upgrade.

Decide and write down: do `"cockroach"` and `"crdb"` become the only Cockroach spellings? The
answer belongs in the variant's doc comment, because the next person to read
`"postgres" | "pg" => Cockroach` in git history will assume it was a bug rather than a decision.

Update the three "expected memory | cockroach | sqlite" error strings.

**Depends on:** nothing.

---

## B2 — Migration with templated width

`migrations/postgres/001_init.sql`, pgvector, `VECTOR(n)` width from config.

**The Cockroach pattern cannot be copied.** Cockroach's DDL is `include_str!`'d as a static and
`schema_vector_dim` (`src/store/cockroach.rs:868`) parses the width back *out* of that string —
the schema file is the authority and the code reads it. A configurable width has to be substituted
*into* the SQL before execution, which inverts the direction the data flows.

This is the only genuine design work in B. Two shapes to choose between:

1. **Template at init.** Keep one `001_init.sql` with a placeholder, substitute from config in
   `init_schema`. Simple; means the file on disk is no longer valid SQL, which breaks reading it
   as documentation and any tooling that lints migrations.
2. **Generate the DDL in code.** The width is a parameter, the SQL is built in Rust. Honest about
   what is happening; loses the "the schema file is the contract" property the Cockroach adapter
   deliberately has.

Whichever is chosen, `vector_dimensions()` must still have a single authority (B4), and the
choice should be recorded here rather than inferred from the code later.

**Depends on:** B1.

---

## B3 — Dialect split

The Cockroach-specific surface is narrow. Each item is a place the two dialects diverge:

| Cockroach | PostgreSQL + pgvector |
| --- | --- |
| `INIT_SQL` with `CREATE VECTOR INDEX` | pgvector index (choose ivfflat or hnsw, and say why) |
| `::VECTOR` casts | pgvector's own cast |
| `::STRING` casts | `::TEXT` |
| `<->` is L2, score `1 - d²/2` | `<=>` is cosine distance, score `1 - d` |
| `VECTOR(n)` DDL width parse | B2's authority |

**The distance operator is the dangerous one.** Getting it wrong does not fail — it ranks wrongly,
quietly, and looks like a model quality problem. The Cockroach adapter L2-normalizes before
storing precisely so `<->` rankings stay coherent with cosine; carry that reasoning across rather
than the formula alone.

Preserve, unchanged, everything the trait requires of any adapter: the fencing token on `flush`
(a write below the session lease's `current_token` must be refused with `StaleWrite`, never
dropped), idempotent upsert semantics on every mutation kind so a replayed batch converges, and
the documented `created_at` divergence.

**Depends on:** B2.

---

## B4 — `vector_dimensions()` from config

Report the configured width so `check_vector_compatibility` still has an authority to check
against. `check_vector_search_contract` (`src/resolve.rs:63`) additionally refuses a store that
claims `VECTOR_SEARCH` without a concrete width, or reports a width without the capability — the
two halves are one contract.

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
> have:
> that the *initialized* schema width matches config, verified against the live database rather
> than echoed from the same config value.

**Depends on:** B2.

---

## Done when

- [ ] `kind = "postgres"` reaches a real Postgres, and `"cockroach"` still reaches Cockroach
- [ ] Schema initializes at a width taken from config, at more than one width
- [ ] Ranking parity with Cockroach on the same seeded graph, with `<=>` scoring verified rather
      than assumed
- [ ] Fencing-token refusal and flush-replay idempotency both proven on the new adapter
- [ ] `store-postgres` matrix row, plus a `postgres-live` job using a **service container**
      (`pgvector/pgvector`) rather than a provisioned cluster: no secret, no cost, and it runs on
      every push instead of being a tier someone remembers to check
