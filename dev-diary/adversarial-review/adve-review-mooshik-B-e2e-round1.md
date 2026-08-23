# Adversarial review: mooshik B, whole workstream end to end, round 1

**Reviewer**: independent adversarial reviewer, agent_id `BE2EReview1`. Wrote nothing
under review except this file and `evidence/mooshik-b-e2e/`.
**Scope**: the composition of B0 through B4 as merged, not the individual phases. Tips
`9d9d8d7`, `e14ba49`, `97ee28c`, `e219488`, `6a4ea26`, merged at `d3bea88`. The rubric is
the **Done when** checklist of `dev-diary/lambo-for-mooshik/B-postgres-store.md`. The five
per-phase reviews (`adve-review-mooshik-B-B{0,1,2,3,4}-round{1,2}.md`) all returned APPROVE;
nothing already closed there is re-reported unless a later phase degraded it.
**Worktree**: `/home/nryn/work/lambo`, branch `lambo-for-mooshik` @ `d3bea88`, verified
before starting and verified clean after (no tracked file modified; only
`evidence/mooshik-b-e2e/`, the orchestrator's untracked E2E notes, and the pre-existing
untracked `local:/` show in `git status`).
**Live rig**: container `lambo-b-e2e`, `pgvector/pgvector:pg17`, PostgreSQL 17.11, host port
55432. The live CockroachDB cluster in `.env` was **never contacted**: every invocation ran
under `env -u LAMBO_COCKROACH_DSN -u DATABASE_URL`, and the one test that needed
`LAMBO_COCKROACH_DSN` set pointed it at a database inside this container.

**Verdict**: **REQUEST_CHANGES**. 2 P1, 2 P2, 7 P3.

The wall does not stand. Two of the bricks are fine on their own and wrong together: CI is
red on the merged tree because B3 tripped a lint in a feature combination no B phase gate
runs, and the config-vs-environment DSN precedence that E2E-1 found on the Cockroach
`provision` path is not Cockroach-specific at all. It is worse on the Postgres leg, which
B1 explicitly claimed was clean.

The good news, stated first because it is load-bearing: **the distance conversion is
genuinely verified.** Three independent mutations of the B3 row were caught, two of them
by the live harness. And the fencing token, which B0's round 1 showed has no offline
evidence anywhere, now has real live evidence on the Postgres leg running in CI on every
push. Those are the two things the brief said mattered most, and both hold.

---

## Method

1. Read `B-postgres-store.md`, `b-run/CYCLE.md`, `b-run/E2E-orchestrator-notes.md`, all ten
   per-phase reviews, `src/store/pg/{mod,dialect,postgres}.rs`, `src/store/mod.rs`,
   `src/mcp/endpoint.rs`, `src/config.rs`, the H1/H3 harness in `src/store/sqlite.rs`, and
   `.github/workflows/ci.yml`.
2. **Ran every gate myself.** No number below is copied from any implementation or review
   report. Numbers that disagree with `CYCLE.md` are flagged rather than reconciled.
3. **Four mutations**, each applied to the working tree, gates re-run, then reverted with
   `git status --short src/` confirmed empty. Full log:
   `evidence/mooshik-b-e2e/mutation-log.txt`. A pin counts as live only if it FAILS under
   the mutation; a pin that passes is itself a finding.
4. **Preferred live evidence over source reasoning wherever live evidence was possible**,
   which is the whole content of B's container decision. In particular the `EXPLAIN` box
   and the hnsw envelope were re-measured at a corpus size where the planner's choice is
   real (20,000 rows at dim 768, hnsw index 71 MB) rather than at the 0-, 9- and 22-row
   corpora the shipped tests use.
5. Exercised the shipped binary as an operator would: `provision`, `derive`, `recall`,
   `stats`, `re-embed`, and two concurrent `serve` processes, against the container.
6. Evidence artifacts under `evidence/mooshik-b-e2e/`: `explain-20k-dim768.txt`,
   `hnsw-envelope-20k.txt`, `dsn-precedence.txt`, `mutation-log.txt`, `feature-matrix.txt`.

---

## Done-when box-by-box ruling

| # | Box | Ruling | Basis |
| --- | --- | --- | --- |
| 1 | B0: Cockroach byte-identical after extraction; suites and clippy unchanged; live `#[ignore]`d tests re-run green on a DSN-bearing machine | **MET (offline) / NOT VERIFIABLE HERE (live)** | `store::pg::cockroach::tests::b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` exists and passes on my run. All three cockroach suites green (counts below, all +5 vs `CYCLE.md`, see E2E-F10). The 15 `#[ignore]`d live Cockroach tests I am **forbidden to run** (production cluster); this half of the box remains on the orchestrator. |
| 2 | `kind = "postgres"` reaches a real Postgres, `"cockroach"` still reaches Cockroach, and the cross-misconfiguration fails loud at provision or first vector query | **PARTIALLY MET** | Aliases verified in `StoreKind::from_str` and live end to end. **Cross-misconfiguration, cockroach-dialect-at-PostgreSQL: now proven to fail loud.** I ran `kind = "cockroach"` with `store.dsn` at my container: `stats`, `recall` and `derive` all rc=1 with `unrecognized configuration parameter "vector_search_beam_size"` at the first connection, earlier than B1 predicted. This closes the leg E2E-1 called untested, **for every verb except `provision`**. The postgres-dialect-at-Cockroach direction stays untested (no reachable Cockroach). And E2E-F2 below undermines the box's premise: on this machine the config file does not select the database at all. |
| 3 | Schema initializes at a width taken from config, at more than one width, with hnsw present from init, and dim > 2000 handled loudly | **MET** | Live at four widths (8, 768, 1536, 2000): `concepts.embedding` is `vector(n)`, `concepts_embedding_idx` is `USING hnsw (embedding vector_cosine_ops) WHERE embedding IS NOT NULL`, `vector 0.8.6` created by lambo. `vector_dim = 3072` refused at rc=1 naming 3072, the 2000 ceiling, that 768/1536 pass, and the unimplemented halfvec hatch. The refusal fires at construction, before any I/O, so it precedes B4's live check cleanly (see "Composition seams" below). |
| 4 | An `EXPLAIN` capture proves the hnsw index is actually used by the recall query | **SUBSTANCE MET, CAPTURE VACUOUS** | The claim is true and I verified it independently at 20,000 rows / dim 768 with a real probe: natural plan is `Index Scan using concepts_embedding_idx`, 0.79 ms, 394 buffers. But the shipped capture does not establish it. See **E2E-F4** (P2). |
| 5 | Parity via H3: forced-exact lane shows zero adapter skew; hnsw lane's divergence stated as a measured envelope | **FIRST HALF MET, SECOND HALF NOT** | Forced-exact zero skew is real and mutation-verified twice. The hnsw envelope is measured on 9 and 22 vectors, where hnsw provably cannot diverge, and the forced-exact lane is not self-verifying. See **E2E-F3** (P2). |
| 6 | Fencing-token refusal and flush-replay idempotency both proven on the new dialect | **MET, and better than the Cockroach leg** | `fencing_refuses_stale_write_and_upserts_replay` passes live on my run and **fails under mutation M4** (fencing gate disabled). It also proves replayed-batch convergence (one concept after two flushes) and the NULL-only quarantine. B0's round 1 established the gate has no offline evidence; that is still true on both dialects (M4 left 794 and 987 offline tests green), but the Postgres leg's live proof runs in `postgres-live` on **every push**, where Cockroach's equivalent is secret-gated and disabled on this branch. This box is the clearest win in B. |
| 7 | `store-postgres` matrix row plus a `postgres-live` job with a service container, tag pinned, running on every push | **PARTIALLY MET** | The pin is a **digest**, not a tag and never `latest`: `pgvector/pgvector:pg17@sha256:cf134a76…`, identical to the local image. `postgres-live` carries no `if:` gate (unlike `cockroach-live`), so it runs on push to `lambo-for-mooshik`, and `.github/workflows/ci.yml` is in the path filter. `LAMBO_REQUIRE_LIVE: "1"` is set at job level and I verified the contract both ways: without a DSN and without the flag the test prints a skip notice and reports `ok`; with the flag it panics. The `LAMBO_REQUIRE_LIVE` lesson from `cockroach-live` is correctly applied. **But CI is red on the merged tree** (E2E-F1) and **no CI row lints `store-postgres` test code at all** (E2E-F8). |

---

## Findings

### E2E-F1 (P1): CI is red on the merged tree: B3 broke two existing matrix rows

Two `feature-matrix` rows in `.github/workflows/ci.yml` fail to compile at `d3bea88`:

```
$ cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings     # row "sqlite-vectors"
error: this function has too many arguments (8/7)
    --> src/store/sqlite.rs:5787:9
error: could not compile `lambo` (lib test) due to 1 previous error

$ cargo clippy --all-targets --features ship,fixtures -- -D warnings             # row "ship-fixtures"
error: this function has too many arguments (8/7)
```

B3 added `postgres_dsn: Option<&str>` to `run_fixture_grid`, taking it from 7 parameters to
8 and tripping `clippy::too_many_arguments` (default threshold 7). Confirmed against
`git show e219488^:src/store/sqlite.rs`, where the signature has exactly 7.

**Why every phase gate missed it.** `CYCLE.md`'s standing gate list is
`clippy --all-targets`, `clippy --features store-cockroach`, and three cockroach test rows.
None of them compiles `src/store/sqlite.rs`'s test module, which is where the H1/H3 harness
lives. `--features store-postgres,store-cockroach,fixtures` is clean (0 errors); it takes
`store-sqlite` to see it. B3's own review ran B3's own gates and they were green.

This is exactly the class the E2E round exists for, and it is the same shape as F-R2-2 and
B0-R1-5 (a feature combination that CI lints but the phase gates do not). The `ship-fixtures`
row's own comment argues at length for why that row must exist; the row exists and is red.

**Fix**: `#[allow(clippy::too_many_arguments)]` with a one-line reason, or bundle the grid's
inputs into a struct. Then add `cargo clippy --all-targets --features store-sqlite,fixtures`
to the standing phase-gate list so this cannot recur.

### E2E-F2 (P1): `LAMBO_COCKROACH_DSN` silently outranks `store.dsn` on the Postgres path, and `LAMBO_POSTGRES_DSN` is inert

`StoreConfig::dsn_from_env` (`src/store/mod.rs:828`) reads **only** `LAMBO_COCKROACH_DSN`
(then `DATABASE_URL`), and `overlay_env` applies the result **kind-agnostically**, after the
TOML has been read (`src/config.rs:342-352`, env wins over file). Meanwhile
`PostgresDialect::DSN_ENV = "LAMBO_POSTGRES_DSN"` is named in the operator-facing missing-DSN
error and in the dialect docs, and **nothing in config resolution ever reads it**.

Demonstrated live, every target inside my own container
(`evidence/mooshik-b-e2e/dsn-precedence.txt`):

```
config: kind = "postgres", store.dsn -> .../widthtest   (1 concept)

$ env -u LAMBO_COCKROACH_DSN lambo --config w768.toml stats --session s1
session 's1' (reader snapshot)
nodes=2 edges=1 concepts=1 canonical=0

$ LAMBO_COCKROACH_DSN=.../scale768 lambo --config w768.toml stats --session s1
lambo stats: invariant violated: concept 80d8c750-… has no Derives edge; …
                                  ^ answered from scale768, the database the config never names
```

and, with no `store.dsn` at all:

```
$ LAMBO_POSTGRES_DSN=.../widthtest lambo --config nodsn.toml provision
lambo provision: … PostgresStore requires a DSN (store.dsn or LAMBO_POSTGRES_DSN)

$ LAMBO_COCKROACH_DSN=.../widthtest lambo --config nodsn.toml provision
postgres schema provisioned (init_schema, idempotent, hnsw from init)
```

**Why this is a P1 and not a duplicate of E2E-1.** E2E-1 filed the precedence inversion as a
`provision.sh` artifact on the Cockroach path and explicitly exonerated Postgres: *"The
Postgres path does not have this bug: the dead-port test proves it honours `store.dsn`."*
That conclusion is wrong. The dead-port test only proves `store.dsn` is honoured when no
`LAMBO_COCKROACH_DSN` is present in the environment. The bug is not in `provision.sh`; it is
in `StoreConfig::overlay_env`, it predates B, and B1 **widened its blast radius** by adding a
second kind that the one env var it consults does not name.

The concrete hazard on this machine: `.env` carries `LAMBO_COCKROACH_DSN` for a production
CockroachDB cluster, and the binary loads `.env`. A `kind = "postgres"` config on this
machine therefore points at that cluster regardless of what its TOML says, on **every verb**,
not just `provision`. And since B1, `lambo provision` for `kind = "postgres"` runs
`init_schema` in-process rather than shelling to the script, so the misdirected verb is now
one that issues DDL. It would die on `CREATE EXTENSION vector` rather than succeed, so this
cannot silently mis-rank; but "issues DDL against a production cluster the operator did not
name" is the failure mode B1's box says cannot happen, and it is the one E2E-1 was written
about.

E2E-2's proposed fix (refuse when `store.dsn` and the env DSN disagree, naming both) is the
right shape, and it should be applied in `overlay_env` rather than in the `provision` verb,
so it covers both dialects and every verb at once. `LAMBO_POSTGRES_DSN` should either be read
for `StoreKind::Postgres` or stop being named in the error.

### E2E-F3 (P2): H3's forced-exact lane is not self-verifying, and its hnsw envelope is measured where hnsw cannot diverge

Two defects in one box, because the fix for one does not fix the other.

**(a) The forced-exact lane is not self-verifying.** Mutation M3 replaced
`PostgresDialect::forced_exact_scan_sql()` with `None`, so the "exact" lane forces nothing
and is byte-identical to the hnsw lane. Result:

```
offline  FAILED  store::pg::postgres::tests::distance_to_score_is_one_minus_d
live     FAILED  store::pg::postgres::tests::explain_recall_uses_hnsw
live H3  PASSED  store::sqlite::tests::h1_cross_store_parity::h3_postgres_recall_parity
```

`h3_postgres_recall_parity` is green with two identical lanes. It reports "zero adapter skew"
and "zero hnsw envelope" from a comparison of a lane against itself, and its check that
`postgres-exact` is `!index_present` reads a **hardcoded `false` in the adapter tuple**
(`sqlite.rs:5773`), not a probe of the plan. B3's spec says H3 *is* the parity box; the box
survives its own lane being switched off.

**(b) The envelope is measured where hnsw cannot diverge.** The H3 corpus is the two committed
fixture graphs at dim 8: **9 and 22 concepts**. `evidence/mooshik-h3-postgres-parity/README.md`
is honest about this ("hnsw envelope vs forced-exact: zero divergence at this corpus size
(9 and 22 vectors) … at this n it agrees with the seq scan"), but honesty about a
measurement does not make it the measurement the box asks for. I reproduced H3 exactly:
240 pairs, `min_jaccard=1, max_score_diff=0, max_displacement_ids=0`.

I then measured the same quantity at a corpus where the approximation is real
(`evidence/mooshik-b-e2e/hnsw-envelope-20k.txt`): 20,000 rows, dim 768, pgvector defaults,
probes drawn from the corpus so each probe's own row is the exact rank-1 answer.

| k | mean \|hnsw top-k ∩ exact top-k\| / k | note |
| ---: | ---: | --- |
| 5 | 1.000 | planner chose Seq Scan for **both** lanes: not an hnsw measurement |
| 10 | 1.000 | same |
| 20 | **0.000** | planner switched to the hnsw Index Scan here |
| 40 | **0.000** | |
| 60 | 0.333 | |

At k=20 the hnsw lane's top-20 shares **nothing** with the exact top-20 and does not contain
the probe's own stored row, whose cosine distance is 0.

Stated honestly: a uniformly random high-dimensional corpus is the worst case for hnsw (all
pairwise cosines cluster near zero, so ranks past the self-match are near-ties), and a real
BGE-M3 corpus is clustered and will do considerably better. **The number is not the finding.**
The finding is that B chose hnsw-from-day-one on the explicit reasoning that *"if hnsw
disappoints, that is discovered early, on unimportant data"*, and then built the only
instrument capable of discovering that at a corpus size where it structurally cannot fire.
An operator reading `evidence/mooshik-h3-postgres-parity/README.md` today would conclude the
hnsw envelope is zero. It is not zero at any n where the index is used.

**Fix**: seed one H3 fixture at a few thousand synthetic vectors (the harness already
generates them), assert the plan actually used the index in the hnsw lane rather than
hardcoding `index_present`, and print the resulting envelope as a real range. Making the
`postgres-exact` adapter's `index_present` come from an `EXPLAIN` probe closes (a) too.

### E2E-F4 (P2): the `EXPLAIN` box's capture is taken on an empty table with a degenerate probe

`explain_recall_uses_hnsw` (`postgres.rs:697`) calls `unique_live_store(…, 8)`, which creates
a database and runs `init_schema`, and then EXPLAINs immediately. **The table has zero rows.**
H3's camera proof does the same. The probe is `vec![0.0; dim]`, and on pgvector a zero vector
gives `NaN` for `<=>` on every row:

```
lambo=# SELECT '[0,0,0]'::vector <=> '[1,0,0]'::vector, '[0,0,0]'::vector <-> '[1,0,0]'::vector;
 NaN | 1
```

The load-bearing assertion is on the plan taken with `SET LOCAL enable_seqscan = off`. That
GUC is a cost penalty applied to the *alternative*, so it does not test the planner's
judgement at all: it tests that the index is usable. A natural plan that had degraded to a
seq scan would still pass this test. The B spec's own argument for the container was that
this box must test lambo's query rather than someone's `postgresql.conf`; as written it tests
neither, because there is no data for the planner to have an opinion about.

The claim itself survives. I verified it at 20,000 rows / dim 768 with a real probe
(`evidence/mooshik-b-e2e/explain-20k-dim768.txt`):

```
Limit  (cost=1066.31..1140.88 rows=40) (actual time=0.406..0.745 rows=40 loops=1)
  ->  Index Scan using concepts_embedding_idx on concepts
        Order By: (embedding <=> ((InitPlan 1).col1)::vector)
Execution Time: 0.793 ms
```

and the forced-exact lane genuinely forces exactness at that scale, with
`enable_indexscan = off` alone and no `enable_seqscan` interaction needed (the hnsw AM does
not support index-only scans, so there is no third path to close):

```
  ->  Sort  (Sort Method: top-N heapsort)
        ->  Seq Scan on concepts (actual rows=20000)
Execution Time: 158.8 ms
```

So this is a vacuous capture of a true claim, not a false claim. Graded P2 rather than P1 for
that reason. **Fix**: seed the camera-proof store before EXPLAINing, use a non-zero probe, and
assert the **natural** plan names `concepts_embedding_idx`. The `enable_seqscan = off` lane
can stay as a diagnostic, but it must not be the assertion.

### E2E-F5 (P3): B4's live width check does not run on the reader attach path

With a config at `vector_dim = 1536` against an initialized `vector(768)` schema:

| verb | rc | behaviour |
| --- | ---: | --- |
| `provision` | 1 | `postgres: live schema width is vector(768) but this process constructed at dim 1536 …` |
| `derive` | 1 | same message, nothing written |
| `re-embed` | 1 | reaches the store and applies its own guards |
| `recall` (populated) | 1 | refused, but by the **session embedding-contract** check, not by B4's |
| `stats --session` | **0** | prints a normal reader snapshot |

B4's box asks for the *initialized* schema width to be checked against config **against the
live database**, and on the provision and write paths it is, with a message naming both
numbers. `stats` opens the store and never calls `preflight_schema`, so the check does not
fire. No vector is mis-read (`stats` reads no vectors, and `recall` is fail-closed via the
contract), so this is a completeness gap rather than a correctness bug. Worth closing so the
guarantee is "on attach" rather than "on attach, on some verbs".

### E2E-F6 (P3): `provision --help` never mentions Postgres

```
provision   Provision / migrate the durable store schema (SQLite init_schema, Cockroach via scripts/provision.sh)
```

`kind = "postgres"` is a shipped, tested provision path as of B1/B2, and this is precisely the
verb E2E-1 and E2E-F2 are about, so the operator most in need of the sentence is the one
reading it.

### E2E-F7 (P3): one new private-item doc-link warning in the pg module

`cargo doc --no-deps --document-private-items --features store-postgres,store-cockroach,store-sqlite,fixtures`
emits 55 warnings against 54 for the cockroach-only gate. The one that is new:

```
warning: public documentation for `vector_dim` links to private item `DEFAULT_POSTGRES_VECTOR_DIM`
   --> src/store/pg/postgres.rs:103:11
```

B0-R1-2 added this gate specifically because it is the only one that sees private-item doc
rot. It saw some.

### E2E-F8 (P3): no CI row lints `store-postgres` test code

`check` runs `clippy --all-targets` on default features (no postgres). The `postgres`
matrix row is `cargo test --no-default-features --features store-postgres`, not clippy. The
`ship` feature is `store-memory, store-cockroach, store-sqlite, embed-bge, embed-fixture`
and does **not** include `store-postgres`, so `ship-fixtures` does not cover it either. The
`postgres-live` job sets `RUSTFLAGS: -D warnings`, which catches rustc lints but not clippy
lints. Consequence, already visible:

```
$ cargo clippy --all-targets --no-default-features --features store-postgres,store-sqlite,fixtures -- -D warnings
error: this function has too many arguments (8/7)   --> src/store/sqlite.rs:5787:9
error: useless use of `vec!`                        --> src/store/sqlite.rs:6209:39
```

The second error is inside `h3_postgres_recall_parity` itself and is invisible to every
CI row.

### E2E-F9 (P3): the unit-norm output contract is unenforced, and only the Postgres dialect turns its violation into `NaN`

`Embedder::embed`'s documented output contract is unit norm (`src/embed/mod.rs:104`).
`encode_vector` rejects non-finite components but a zero vector is finite and passes. On
pgvector, `<=>` against a zero vector is `NaN`; on Cockroach, `<->` is a finite `1`. So one
contract-violating row scores `NaN` on Postgres and `0.5` on Cockroach for the same data,
and `distance_to_score(NaN)` propagates `NaN` into ranking. H3's synthetic vectors are unit
by construction, so the harness cannot see this.

The `PostgresDialect::distance_to_score` doc comment is **correct** where it says the `1 - d`
identity does not need unit norm (pgvector divides by the norms), and the B3 spec's claim
that unit norm is what makes the *Cockroach* L2 conversion equal cosine is also correct. The
gap is the degenerate case neither covers. A cheap `norm > 0` check in `encode_vector`, or a
named refusal, closes it.

### E2E-F10 (P3): `CYCLE.md`'s expected listed counts are stale by exactly +5

| Gate | `CYCLE.md` expects | I measured |
| --- | ---: | ---: |
| `cargo test --features store-cockroach` | 939 listed (935/4) | **944 listed (940 passed / 4 ignored)** |
| `cargo test --no-default-features --features store-cockroach` | 598 listed | **603 listed (603 passed / 0 ignored)** |
| `cargo test --features store-cockroach,fixtures` | 1007 listed (995/12) | **1012 listed (1000 passed / 12 ignored)** |

Consistent +5 from B1–B4's additions, all green. Benign, but a standing gate whose expected
number is wrong stops being a gate.

### E2E-F11 (P3): the "park and fail over" ruling is not implemented

`B-postgres-store.md` records an operator ruling dated 2026-08-23: *"the losing machine's
writer parks rather than refusing … keeps serving reads, retries, and takes the lease if the
holder's lapses"*, and says DOGFOOD-SETUP's promise moves with it. Live, on Postgres:

```
$ lambo serve --session s1 --agent a2   # while a1 holds
lambo serve: conflict: session s1 is already held by another writer (a1@cachyos-x8664#450170)
             — it acquired the single-writer lease 0s ago and is still refreshing it.
             Refusing to open a second writer.
```

The loser refuses; it does not park. This is not one of the Done-when boxes, so it does not
gate the verdict, but the doc says "B has to say what the loser does instead" and what B says
is the pre-ruling answer. Either implement it or record explicitly that it moved out of B.

---

## Rulings on the orchestrator's two findings

**E2E-1: upheld as a defect; one half of its attribution is wrong; one half of its "untested"
is now tested.**

* Its core observation stands and is serious: `lambo provision` for `kind = "cockroach"`
  shells to `scripts/provision.sh`, which reads the DSN from the environment only, so the
  verb acted on a cluster the config never named and reported success.
* **Its exoneration of the Postgres path is false.** "The Postgres path does not have this
  bug: the dead-port test proves it honours `store.dsn`" holds only in an environment with no
  `LAMBO_COCKROACH_DSN`. See E2E-F2, demonstrated live. The precedence inversion lives in
  `StoreConfig::overlay_env`, not in `provision.sh`, and it hits Postgres on every verb.
  I would regrade the precedence half from a `provision.sh` note to a **P1**.
* **Its "reachability" half is now partly closed.** I ran the cockroach dialect against a real
  PostgreSQL server (my container, never the cluster) and every store-opening verb fails loud
  at connection setup: `unrecognized configuration parameter "vector_search_beam_size"`. That
  is B2's `apply_connect_options` firing, and it is earlier and louder than B1 predicted. So
  B1's box is met for `stats`, `recall`, `derive` and every other verb that constructs a
  store; only `provision` escapes, because it never constructs one.
* Its judgement not to file this as a B-phase regression was right for `provision.sh` and
  wrong for `overlay_env`, which B1 made materially more dangerous by adding a kind the env
  overlay does not know about.

**E2E-2: correctly filed, correctly separated, and it should be broadened.** The proposed
remedy (refuse when `store.dsn` and the environment DSN disagree, naming both) is right, but
placing it in the `provision` verb would fix one third of the problem. It belongs in
`overlay_env`, where it covers both dialects and every verb, and it is the same fix E2E-F2
needs. Merge them.

---

## Defects introduced by composition

These exist only because of how the phases meet. Each phase is correct alone.

1. **E2E-F1.** B3's 8th parameter is fine in B3's feature set. It is a compile error in
   `store-sqlite,fixtures` and `ship,fixtures`, which no B phase gate runs and which CI does.
   The gate list in `CYCLE.md` was inherited from B0, when the extraction touched only
   cockroach code; from B3 onward the workstream writes to `src/store/sqlite.rs`, and the gate
   list never grew to match. Note also that `CYCLE.md`'s standing H1 lock ("`git diff
   src/store/sqlite.rs` empty") became unrunnable at B3 by design, and nothing replaced it.
2. **E2E-F2.** B1 added `StoreKind::Postgres` to a config layer whose only DSN environment
   variable is named after the other dialect. B1's own review checked the alias split (which
   is correct) but not the env overlay that sits above it.
3. **E2E-F8 / E2E-F1 together.** B2 added the `postgres` CI row as `cargo test`, and B3 added
   `postgres-live` as `cargo test`. Neither added a clippy row, and the pre-existing clippy
   rows do not carry `store-postgres`. So the feature has test coverage and zero lint
   coverage, which is how a clippy error reached `main`-bound code.

**Seams I checked that are clean**, recorded so the next round does not re-walk them:

* **B4's live width check does run on the path B1's alias split routes through.** `build_store`
  → `StoreKind::Postgres` → `PgStore<PostgresDialect>` → `init_schema`/`preflight_schema` →
  `assert_live_schema_width`. Verified live end to end through the CLI on `provision` and
  `derive`.
* **B2's dim ceiling fires before B4's width check, and the two messages are coherent.**
  `vector_dim = 3072` against an initialized `vector(768)` database refuses at construction
  with the ceiling message (before any I/O); `vector_dim = 2000` against the same database
  gets past construction and refuses at init with the live-width message. Right order, right
  message each time, no interleaving.
* **Feature combinations compile.** `store-postgres` alone, with `store-cockroach`, with
  `store-sqlite`, with `fixtures`, on top of default features, and `--no-default-features`
  bare: all clean under `clippy --all-targets -- -D warnings`. The only red combination is
  the one containing `store-sqlite,fixtures`, i.e. E2E-F1, which is not a Postgres problem.
  Full matrix in `evidence/mooshik-b-e2e/feature-matrix.txt`.
* **`re-embed` reaches the Postgres store** and applies its guards (refused a same-contract
  migration with `re-embed requested but the session already carries exactly this contract`).
* **`serve` holds a lease correctly on Postgres.** Live: `session_leases` row with
  `current_token = 1`, `endpoint = /run/user/1000/lambo/s1-ecbb7bdafd611aa8.sock`, and a
  second `serve` on the same session refused by name with the holder identified. Fencing token
  minted, endpoint bound, `store_is_shareable(Postgres) = true` ruled explicitly in the
  exhaustive match with a test that reads the arm text.
* **DSN identity, not spelling.** The J2-R1-2-in-Postgres-clothes hazard is closed and I
  verified it live rather than from the changelog: two spellings of one database
  (`postgresql://…?sslmode=disable` vs `postgres://…?connect_timeout=10&application_name=alt&sslmode=disable`)
  derive the **same** endpoint `s1-ecbb7bdafd611aa8.sock`. `canonical_store_dsn` normalises
  scheme, host case, default port, default database, and drops connection-only query
  parameters, with the reasoning written beside it.
* **The three "expected memory | cockroach | sqlite" strings** were not patched three times;
  they were consolidated into one const,
  `STORE_KIND_EXPECTED = "memory | cockroach | postgres | sqlite"` (`src/store/mod.rs:642`),
  used by both `from_str` arms. No stale list survives anywhere in `src/`, `docs/`,
  `README.md` or `lambo.example.toml`.
* **The 0.3.0 changelog notes the break** under `### Breaking`, with the reasoning (fails at
  `CREATE EXTENSION vector` or first `<=>` rather than silently ranking with Cockroach SQL).

---

## Mutation results

A pin is live only if it FAILS under the mutation. Full transcript:
`evidence/mooshik-b-e2e/mutation-log.txt`.

| # | Mutation | Offline suites | Live | Verdict |
| --- | --- | --- | --- | --- |
| M1 | `distance_to_score`: `1 - d` → `1 - d²/2` (Cockroach's L2 formula on pgvector cosine) | **FAILED** `distance_to_score_is_one_minus_d` (left 0.5, right 0.0) | **FAILED** H3: `systematic score skew … max diff 0.009432170552267527 > 0.0001` | Live pin, caught twice |
| M2 | `DISTANCE_OP`: `<=>` → `<->` (Cockroach operator, conversion left at `1 - d`) | **FAILED** ×3: `dialect_tokens_are_not_cockroach_sql`, `distance_to_score_is_one_minus_d`, `recall_sql_pairs_cosine_operator_with_text_and_vector_casts` | **FAILED** H3, at the EXPLAIN assertion: the plan degraded to `Seq Scan` because `concepts_embedding_idx` is `vector_cosine_ops` and cannot serve `<->` | Live pin. Note the live catch comes from the index opclass, not from score agreement |
| M3 | `forced_exact_scan_sql()` → `None` | **FAILED** `distance_to_score_is_one_minus_d` | `explain_recall_uses_hnsw` **FAILED**; **`h3_postgres_recall_parity` PASSED** | **Vacuous within H3** → E2E-F3(a) |
| M4 | fencing gate disabled (`if false && !lease_permits_write(…)`) | **PASSED** 794/794 (postgres+sqlite+fixtures) and 987/987 (cockroach+fixtures) | **FAILED** `fencing_refuses_stale_write_and_upserts_replay`: "stale token must be StaleWrite, got Ok(())" | No offline evidence on either dialect (B0-R1 still true); live Postgres evidence is real and runs on every push |

On the brief's question about whether a self-consistently wrong operator/conversion pairing
could slip through: `<->` paired with `1 - d²/2` is mathematically *correct* on unit-norm
input, so it is not a defect to catch, and M2 shows the mismatched pairing dies on the index
opclass before ranking is even reachable. The unit-norm assumption the Cockroach equivalence
rests on is **not enforced anywhere** (E2E-F9), but the Postgres path does not depend on it,
and the dialect's doc comment says so correctly.

---

## Gates re-run, my numbers

Every row measured by me on `d3bea88`.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **pass** |
| `cargo clippy --all-targets -- -D warnings` | **pass** (0 errors) |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | **pass** (0 errors) |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | **FAIL**, E2E-F1 |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | **FAIL**, E2E-F1 |
| `cargo clippy --all-targets --no-default-features --features store-postgres` | pass |
| `cargo clippy --all-targets --no-default-features --features store-postgres,store-cockroach` | pass |
| `cargo clippy --all-targets --no-default-features --features store-postgres,store-sqlite` | pass |
| `cargo clippy --all-targets --no-default-features --features store-postgres,fixtures` | pass |
| `cargo clippy --all-targets --no-default-features` | pass |
| `cargo clippy --all-targets --features store-postgres` | pass |
| `cargo clippy --all-targets --no-default-features --features store-postgres,store-sqlite,fixtures` | **FAIL** (2 errors), E2E-F1 plus E2E-F8 |
| `cargo test --features store-cockroach` | 940 passed / 0 failed / 4 ignored (**944 listed**; CYCLE says 939) |
| `cargo test --no-default-features --features store-cockroach` | 603 passed / 0 failed / 0 ignored (**603 listed**; CYCLE says 598) |
| `cargo test --features store-cockroach,fixtures` | 1000 passed / 0 failed / 12 ignored (**1012 listed**; CYCLE says 1007) |
| `cargo test --no-default-features --features store-postgres` (the CI row) | 588 passed / 0 failed / 4 ignored |
| `cargo test --no-default-features --features store-postgres,store-sqlite,fixtures --lib` | 794 passed / 0 failed / 5 ignored |
| H1 lock: `cargo test --features store-sqlite,fixtures --lib h1_cross_store_parity` | **pass** (1 passed) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 54 warnings (baseline) |
| `cargo doc … --features store-postgres,store-cockroach,store-sqlite,fixtures` | **55** warnings, the new one in `pg/postgres.rs:103`, E2E-F7 |
| **live** `init_schema_at_two_widths_creates_hnsw` | **pass** |
| **live** `live_schema_width_refuses_a_config_that_disagrees` | **pass** |
| **live** `explain_recall_uses_hnsw` | **pass** (but see E2E-F4) |
| **live** `fencing_refuses_stale_write_and_upserts_replay` | **pass**, mutation-verified |
| **live** `h3_postgres_recall_parity` | **pass**: 240 pairs, 4 adapters, forced-exact skew cells=80, hnsw envelope `min_jaccard=1 max_score_diff=0 max_displacement_ids=0` (see E2E-F3) |
| `LAMBO_REQUIRE_LIVE` contract: no DSN, no flag | reports skip notice then `ok` (so the CI `grep -q '… ok'` alone would not catch a skip) |
| `LAMBO_REQUIRE_LIVE` contract: no DSN, flag set | **FAILED** as intended: the `cockroach-live` lesson is applied |
| 15 `#[ignore]`d live Cockroach tests | **NOT RUN**: production cluster, forbidden by the brief |

---

## Summary

| Grade | Count | Findings |
| --- | ---: | --- |
| P1 | 2 | E2E-F1 (CI red on the merged tree), E2E-F2 (env DSN outranks `store.dsn` on the Postgres path; `LAMBO_POSTGRES_DSN` inert) |
| P2 | 2 | E2E-F3 (H3 forced-exact lane not self-verifying; envelope measured where hnsw cannot diverge), E2E-F4 (`EXPLAIN` capture on an empty table with a NaN-producing probe) |
| P3 | 7 | E2E-F5, F6, F7, F8, F9, F10, F11 |

Neither P1 is a Postgres-adapter defect. Both are seams: one between B3 and a lint gate the
workstream never ran, one between B1's new store kind and a config layer that predates it.
That is the expected shape for a whole-workstream round, and it is why the round exists.

The adapter itself is in good condition. The extraction held, the dialect surface is the
table the spec specified and nothing more, the distance conversion is the dangerous row and
it is genuinely pinned, the fencing token has better evidence on Postgres than it has ever
had on Cockroach, and an operator really can point a `lambo.toml` at a stock pgvector
container and have it work. What is not yet true is that the tests prove what their boxes
claim at a scale where the claim is interesting, and that the config file is the thing that
selects the database.
