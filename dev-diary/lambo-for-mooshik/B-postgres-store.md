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

## Where B is developed, and on what Postgres (operator, 2026-08-23)

**B is developed on the other machine, not this MacBook.** Recorded so that nobody starts
B0's extraction here and discovers the divergence at parity time.

**Postgres runs in a pinned container — never a host install.** The reason is independence
from system-specific bundling, not convenience: what a machine calls "Postgres" is a
packaging decision made by brew or apt, and B's correctness claims must not inherit it.
Three of B's own Done-when boxes are exposed to that decision directly:

* **pgvector is not part of Postgres.** Whether `CREATE EXTENSION vector` works at all
  depends on a separate package (`brew install pgvector`, apt's
  `postgresql-NN-pgvector`, or a source build). "Does the store initialise" would become a
  question about the host's package manager rather than about lambo.
* **Collation.** `initdb` defaults differ across distributions — glibc versus ICU — and
  that moves sort order and index behaviour. For the one workstream whose purpose is
  *parity measurement*, a cross-machine collation difference surfaces as **adapter skew**,
  which is precisely what H3 exists to detect. The bug would live in the operating system
  and be hunted in the adapter.
* **Planner configuration decides B3's `EXPLAIN` box.** Whether the planner picks the hnsw
  index depends on `work_mem`, `random_page_cost` and `effective_cache_size`, all of which
  vary by how a host packaged its defaults. A container pins the configuration beside the
  binary, so that box tests lambo's query rather than someone's `postgresql.conf`.

**Pin the tag** (`pgvector/pgvector:pgNN`, never `latest`), for the reason J2 wrote out
FNV-1a instead of using `DefaultHasher`: this is an agreement several machines and CI must
share, and a version that moves underneath it breaks the agreement silently. A container
does not *remove* packaging decisions — it moves them into one image that is identical
everywhere and changes only when someone decides to change it. That is the whole win, and
it is worth stating precisely rather than claiming containers are neutral.

**Two consequences worth taking:**

1. **It is what makes the dialect family testable at all.** The design above promises that
   future wire-compatible stores — Yugabyte, Neon, AlloyDB, Timescale — are "a dialect file
   each". None of those is a host package anywhere; every one is an image. So containers are
   not a Postgres convenience, they are the mechanism that lets the family be a tested thing
   rather than a claim.
2. **It retires the DSN-bearing-machine constraint** that has shaped this branch. H2 waited
   for the Linux box; `cockroach-live` needs secrets and is gated off on this branch. H3
   against a container needs neither, which is the concrete content of this doc's claim that
   the service-container model is *strictly better* than the Cockroach live model — parity
   testing stops requiring special hardware.

Local development and CI therefore run the **same** image, so "works locally" and "works in
CI" stop being two claims. **One caveat, stated up front:** do not quote *performance*
numbers from a container on macOS. F4's `close_ms ≈ 221 + 249.4·K` came from live
serverless Cockroach and drove a real design decision (the interactive-path K ≤ ~150
envelope); a virtualised layer would give misleading figures. Correctness tests are fine
anywhere — throughput claims name their machine or are not made.

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

## What workstream J left in B's path (2026-08-23)

B was scoped 2026-08-19. J2, J3, J4, D and C all landed afterwards and all of them wrote to
the adapter B0 extracts from, so four things now sit in B's path that its original sections
do not mention. Recorded here rather than in B0's round-1 review, because **none of these is
a B0 defect** — B0's artifact is correct on all four. They are B1-and-later work, and the
cheapest time to read them is before B1 starts.

**1. The extraction surface grew, and mostly in B's favour.** The Cockroach adapter picked up
J2's `endpoint` column on `session_leases`, J4's `lease_refusals` (with its retention `DELETE`
and `(session_id, refused_at)` index), J3's `write_intents`, and D/C's `event_time` and
`human_confirmed`. B0 moved that SQL into `pg/mod.rs`, which is right: it is plain SQL both
dialects share — the `session_leases` upsert uses `ON CONFLICT … DO UPDATE` with `excluded.`,
standard on both — so it is genuine base material rather than dialect surface. The
over-merging trap this doc warns about applies to less of the new code than its volume
suggests.

**2. B1 gets a forced decision from the type system.** `store_is_shareable`
(`src/mcp/endpoint.rs`) matches **exhaustively** on `StoreKind`, so adding `Postgres` will not
compile until someone rules on whether a Postgres-backed session publishes a session
endpoint. The answer is yes, on the same reasoning that makes Cockroach `true` — it is a
networked store another process can open — but the compiler makes it an explicit ruling
rather than a default, which is the right shape for it.

**3. A real hazard: the endpoint hash includes store *identity*, and for Postgres that is a
DSN.** This is J2-R1-2's defect wearing Postgres clothes. There, `path = "./lambo.db"` named
a different file from every cwd, so hashing it verbatim gave two graphs one socket; the fix
was canonicalising the path before hashing. A DSN is harder: `postgres://u@host/db` and
`postgres://u@host:5432/db` are the **same database and different strings**, so they hash
differently — two serves on one machine against one database would derive *different* socket
paths, fail to find each other, and each believe it was alone. The J2 rule to carry forward
is its own: hash the store's **identity, not its spelling**. Whatever normalisation B1
chooses (default port, default database, host case, ignored parameters) belongs beside
`store_identity` with its reasoning, and needs a test that two spellings of one database
derive one endpoint. Note also J2's second reason for hashing at all: it keeps a DSN's
password out of both the filesystem and the lease row.

**4. Several machines, one shared store, one single-writer lease — ruled 2026-08-23
(operator): park and fail over.**

> **Correction in place, 2026-08-24 (B-E2E-R2-4): ruled, not built.** The ruling below is the
> operator's and stands as the decided direction; nothing in it is reworded here. What B
> **ships** is the refusal it describes replacing: the loser gets `HolderIsOnAnotherHost` and
> stops. E2E-F11 filed the gap, the round-1 remediation declined it with reasoning (see
> "Deliberately not built: park and fail over" below), and the round-2 review ruled that
> decline legitimate. Its owner is now [FUTURE.md](FUTURE.md), "Park and fail over on a lost
> lease". Before that entry existed, an operator ruling sat on the map with no
> implementation and no owner, which is the defect R2-4 filed. Read the rest of this item in
> the present tense of the ruling, not of the artifact: "the work is small" is an estimate of
> unstarted work, not a report of finished work.

This doc's goal is "the unified cross-machine store", and
the lease admits exactly one writer per session. Point two machines at one shared Postgres
and the same session and one wins; the loser cannot proxy, because `proxyable` refuses with
`HolderIsOnAnotherHost` — J2 checks the holder's host precisely so a loser never dials a
unix socket path that exists only on the holder's machine. That refusal is correct. With a
machine-local store it was nearly unreachable; with a shared store it becomes the normal
case, so B has to say what the loser does instead.

**The ruling: the losing machine's writer parks rather than refusing.** It keeps serving
**reads**, retries, and takes the lease if the holder's lapses — so whichever machine is
being worked on is the one that writes.

What makes this cheap rather than a compromise: **reads already work everywhere.** `recall`
takes no lease and reads the durable store, and `backend_flush_interval` defaults to **1
second**, so a non-holding machine trails the holder by about a second. J2 rejected
read-only attach as "strictly worse", but that judgement was about the *same-machine* case,
where proxying delivered true read-your-writes for free; across machines the alternative
costs a new transport and a shared secret, and the penalty being priced is one second.

What it gives up, stated rather than glossed: **no simultaneous writes from two machines.**
A write on one is visible on the other in ~1s, but the second machine cannot write while the
first holds. That fits one human at one machine at a time, which is Mooshik's shape.

The work is small, because the lease already does the hard part: the loser needs a
park-and-retry loop instead of a refusal, plus honest text saying writes are held elsewhere
and naming the holder. **Cross-host proxying is the declared future, not this phase** — see
[FUTURE.md](FUTURE.md), "Cross-host proxying". A parked writer is exactly the process that
would later learn to proxy, so this does not have to be undone to get there.

DOGFOOD-SETUP's promise that when B lands "the `[store]` block flips to a shared Postgres and
**nothing else changes**" is falsified by this ruling and moves with it: what else changes is
that one machine writes at a time.

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

**Recorded at B2 implementation (2026-08-23):**

* **Template at init.** `migrations/postgres/001_init.sql` carries the
  placeholder `__LAMBO_VECTOR_DIM__`. `PostgresDialect::init_sql(dim)`
  substitutes it. The file on disk is not valid SQL (applying it with psql
  fails loudly). Generate-in-code was rejected: a 300-line schema built with
  `format!` is worse to review than a file with one placeholder, and the
  inverted data flow is still honest (width goes *into* the SQL).
* **dim > 2000: refuse, naming the ceiling and the `halfvec` hatch.**
  halfvec is not implemented in B2 (it would change the stored type, the
  operator class, and B3's cast/distance rows). 768 and 1536 pass; 2000
  passes; 2001 and Gemini 3072 are refused at `init_sql` / `vector_dim`,
  never at `CREATE INDEX`.
* **Over-merge split.** `init_schema` still runs `raw_sql(ddl)` then N
  `query()` calls; the statements come from `Dialect::post_init_statements`
  (Cockroach: `endpoint STRING` + `current_token INT`; Postgres:
  `endpoint TEXT` + `current_token BIGINT`). `connect_options` applies
  shared `statement_timeout` then `Dialect::apply_connect_options`
  (Cockroach: `vector_search_beam_size`; Postgres: identity, pgvector
  `hnsw.ef_search` stays at default 40). No ANN knobs on Postgres.
* **Width source for substitution, not B4.** `Dialect::vector_dim` reads
  `[store] vector_dim`, else the embedder width copied in by
  `build_store_with_vector_dim`, else 1024. `GraphStore::vector_dimensions`
  still echoes the construction dim. Live-schema reporting is B4.

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

## DSN precedence: the config file wins, or nothing runs (E2E-F2, 2026-08-24)

Found by the whole-workstream end-to-end review, and it was live, not theoretical.
With `store.dsn` naming one database, setting `LAMBO_COCKROACH_DSN` to another made
`provision` write the schema to the second and report success. On the machine where
B is developed, `.env` carries that variable for the production Cockroach cluster,
and since B1 the Postgres `provision` issues DDL in process.

Three separate defects were tangled together:

1. `StoreConfig::dsn_from_env` read only `LAMBO_COCKROACH_DSN` and `DATABASE_URL`,
   and `overlay_env` applied the result **kind-agnostically** after the TOML. So the
   Cockroach variable silently steered a `kind = "postgres"` deployment.
2. `LAMBO_POSTGRES_DSN` existed as `PostgresDialect::DSN_ENV` and was named in error
   messages, but nothing in the config layer ever read it. An operator following the
   error text got no effect. A variable the errors tell you to set and the config
   ignores is worse than one that does not exist.
3. `scripts/provision.sh` reads `DSN="${LAMBO_COCKROACH_DSN:-}"` and was never handed
   `store.dsn`, so the Cockroach `provision` arm could act on a cluster the config
   never named.

**The ruling.** The config file is the single construction site, per Level B. The
environment may **supply** a DSN the file omits, and may **not** silently replace one
the file states. When both are present and name different databases, `overlay_env`
refuses and prints both, passwords stripped. Each kind reads its own variable:
`postgres` reads `LAMBO_POSTGRES_DSN`, `cockroach` reads `LAMBO_COCKROACH_DSN`, and
`DATABASE_URL` remains the shared fallback.

**Why refuse rather than let the file win.** Precedent, and it is the one already in
this document: B4's `vector_dim` pin refuses to resolve when the pin disagrees with
the embedder width rather than picking a winner. Same shape of mistake, same answer.
Picking the file silently would fix the data-loss direction and leave the operator
holding a wrong belief about their own deployment, which is how the bug survived in
the first place.

**Comparison is on identity, not spelling.** `postgres://` and `postgresql://`, an
omitted port against an explicit `5432`, and host casing are all the same database, so
the DSN canonicaliser that J2 built for session socket identity moved out of
`mcp/endpoint.rs` into `store/dsn.rs` and now serves both callers. Password stripping
is what makes the canonical form safe to put in an error message. That promise holds on
inputs the canonicaliser cannot parse as well: B-E2E-R2-5 found that a DSN with a
fat-fingered port fell through to the raw string and put a live password in a refusal
that said "(passwords stripped)", so the fallback now drops the userinfo password too.

**The `provision.sh` leg, both ends.** The Cockroach arm pushes the resolved DSN into
the child's environment rather than letting it inherit the ambient one, so the config
names the cluster the DDL lands on. The "no `store.dsn`, environment supplies it" path
that CI depends on is untouched.

Pushing it is only half the pipe, which is what B-E2E-R2-1 found: the script then ran
`set -a; source .env; set +a` before reading `LAMBO_COCKROACH_DSN`, and a sourced
assignment overwrites the inherited environment, so on a machine whose `.env` carries a
production DSN the pushed value was discarded one door down and the failure this section
exists to close reappeared intact. The script now captures the inherited DSN before
sourcing and restores it after: **an explicitly provided environment beats an ambient
dotfile**, the same precedence the config layer applies to file against environment. The
lesson recorded with it: the round-1 pin was on `Command::get_envs`, the sending end, and
a pin on one end of a pipe is not a pin on the pipe. The receiving end now has its own
test, which executes the script against a decoy `.env`.

## Deliberately not built: park and fail over (E2E-F11, 2026-08-24)

The end-to-end review filed the unimplemented "park and fail over" ruling as P3.
Remediation declined it, and the declination is recorded here rather than left as a
silently open box: it is a writer-availability feature of real size, not a defect in
anything B built, and implementing it inside a remediation round would have shipped a
substantial new behaviour with none of the review that every other part of B received.
It belongs in its own phase or in [FUTURE](FUTURE.md), scoped deliberately. Nothing in
B depends on it, and no Done-when box below claims it.

**Owner, assigned 2026-08-24 (B-E2E-R2-4).** The round-2 review ruled this decline
legitimate and then found the bookkeeping around it wrong: FUTURE.md asserted that B
*ships* park-and-fail-over, item 4 above still stated the ruling in the present tense,
and between them the feature had no owner anywhere. Both documents are corrected, and the
owner is [FUTURE.md](FUTURE.md), "Park and fail over on a lost lease". B's behaviour on a
lost lease is unchanged by this round: the loser refuses.

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
      (`pgvector/pgvector`, **tag pinned — never `latest`**) rather than a provisioned
      cluster: no secret, no cost, and it runs on every push instead of being a tier someone
      remembers to check. The **same pinned image** is what local development runs (see
      "Where B is developed" above), so this job and a developer's machine are not two
      claims — and the pin is what keeps that true across a version bump
