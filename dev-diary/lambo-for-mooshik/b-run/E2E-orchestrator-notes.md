# B end-to-end, orchestrator's first pass (2026-08-23)

Ran before the review agent, to answer one operator question directly: **can you
point a `lambo.toml` at a stock Postgres and have it work?**

Rig: `pgvector/pgvector:pg17`, container `lambo-b-e2e`, host port 55432,
db/user/password all `lambo`. Binary built `--features store-postgres,embed-fixture`.
Tree at `d3bea88`.

The whole config used, nothing omitted:

```toml
[store]
kind = "postgres"
dsn = "postgresql://lambo:lambo@127.0.0.1:55432/lambo?sslmode=disable"
vector_dim = 8

[embedder]
kind = "fixture"
dim = 8
```

## What worked

| Step | Result |
| --- | --- |
| `provision` | rc 0, "postgres schema provisioned (init_schema, idempotent, hnsw from init)" |
| schema | 11 tables; `concepts.embedding` is `vector(8)`, the width taken from config |
| extension | `vector 0.8.6` created by lambo, not pre-installed by hand |
| hnsw from init | `concepts_embedding_idx hnsw (embedding vector_cosine_ops) WHERE embedding IS NOT NULL` |
| `derive` (3 concepts) | rc 0, "3 created, 0 matched existing" |
| rows | all 3 present, all 3 with a non-NULL embedding |
| `recall` | rc 0, 3 hits, ranked, scores 0.31 / 0.30 / 0.17 |
| `stats` | nodes=4 edges=6 concepts=3, embedded=3/3 |
| dim 3072 | **refused loudly at provision**, rc 1, naming the 2000 ceiling, that 768 and 1536 pass, that Gemini 3072 does not, and the unimplemented halfvec hatch |
| dead DSN port | rc 1, pool timeout. The Postgres path really does read the TOML `dsn` |
| unknown TOML key | rc 1, names the key and the accepted set |

So the headline answer is yes.

## E2E-1: the cross-misconfiguration box is not met in the cockroach direction

B1's Done-when says "the cross-misconfiguration fails loud at provision or first
vector query". Tested by flipping one key on the working config above:

```toml
kind = "cockroach"   # dsn still points at the PostgreSQL container
```

Result: **rc 0, reported success, and never contacted the container.**

`lambo provision` for `kind = "cockroach"` shells out to `scripts/provision.sh`,
which reads the DSN from the environment only (`DSN="${LAMBO_COCKROACH_DSN:-}"`,
line 23). `store.dsn` from the TOML is not passed to it and not consulted. The
binary loads `.env`, so the script inherited `LAMBO_COCKROACH_DSN` from there and
provisioned **the live Cockroach cluster** instead: the verify block came back with
`owner | nryn`, `REGIONAL BY TABLE IN PRIMARY REGION`, and 344 rows in
`canonization_events`.

Every statement was idempotent and every table reported "already exists, skipping",
so nothing was created, altered or lost. It was a no-op re-provision of a cluster
that was already provisioned. But the failure mode is the one B1 says cannot
happen, and it is worse than the one B1 anticipated: not "fails loud", not even
"fails quietly", but **succeeds loudly against a database the operator did not
name**. An operator who believes they are provisioning a local container is told
"cockroach schema provisioned" while a remote production cluster is what answered.

Two things are tangled here and both want a verdict:

1. **Precedence.** `store.dsn` in the TOML is silently outranked by an environment
   variable on the cockroach provision path, which inverts the Level B rule that the
   config file selects the backend and is the single construction site. The Postgres
   path does not have this bug: the dead-port test proves it honours `store.dsn`.
2. **Reachability.** Even with the right DSN, the cockroach dialect against a
   PostgreSQL server would have to fail on `SET CLUSTER SETTING` or `VECTOR(1024)`.
   That leg was never reached, so B1's box is **untested**, not merely unmet.

Not filed as a B-phase regression: `provision.sh` predates B and is env-only by
design. Filed because B1 claims the box, the claim is checkable, and it does not
hold as written.

## E2E-2: `provision` for cockroach can reach a cluster the config never names

Follows from E2E-1 and is worth separating, because the fix is different. A verb
that shells out to a script with an inherited environment can act on a target the
operator's config did not mention. At minimum `provision` should refuse when
`store.dsn` is set and disagrees with `LAMBO_COCKROACH_DSN`, naming both, rather
than preferring the environment in silence.

## Not covered by this pass

Left for the review agent, deliberately: the `EXPLAIN` proof that hnsw is actually
chosen by the recall query, H3 forced-exact parity and the hnsw envelope, the
fencing-token refusal and flush-replay idempotency on the new dialect, the
`postgres-live` CI job, real embedder widths (768 / 1536), concurrency, and
`re-embed`. Everything above used the fixture embedder at width 8.
