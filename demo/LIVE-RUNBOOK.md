# T8.4 live-cluster runbook — the demo scenario ×2 on CockroachDB

**Why this file exists.** T8.4's "done when" is the scenario running end to end
**against the live cluster twice consecutively with identical outcomes**. That
cannot be run from the task agent's machine: it needs `LAMBO_COCKROACH_DSN` and
a provisioned cluster. Everything below is exact — commands, expected output,
and the two failure modes worth recognising. Run it on the machine that has the
cluster.

Local proof already in the repo (both green, run repeatedly):

```bash
cargo test --test t84_demo                          # MemoryStore, x2 identical
cargo test --features store-sqlite --test t84_demo  # SQLite,      x2 identical
```

---

## 0. Before you start

> ### ⚠ The cluster's schema diverges from `migrations/cockroach/001_init.sql`
>
> Recorded in `dev-diary/PHASE-8-surface.md` (T8.4 block): a
> `concepts_embedding_nonnull_idx` was **hand-created** on the cluster and is
> not in the committed migration, and the cluster carries seed data (~2833
> concepts / 2004 distinct vectors) from earlier phases.
>
> Consequences for this runbook:
>
> * `lambo provision` runs `scripts/provision.sh`, which reconciles the vector
>   index. Run it **once**, before the first demo run, and read its output — if
>   it wants to drop or recreate an index, decide deliberately rather than
>   letting the demo do it mid-take.
> * The demo writes into its **own fresh session id** and reads only that
>   session, so the pre-existing seed rows do not affect its outcome. They do
>   affect table-wide queries — the split-screen `canonization_events` query in
>   §4 is therefore scoped by `session_id`.
> * Do not "clean up" by truncating tables. Other phases' evidence lives there.

Check the DSN is present and the binary is the one you think it is:

```bash
echo "${LAMBO_COCKROACH_DSN:?set LAMBO_COCKROACH_DSN first}" | sed 's/:[^:@]*@/:***@/'
cargo build --release --features store-cockroach
./target/release/lambo --version
```

Write a `lambo.toml` next to where you will run (the DSN stays in the
environment — it is never a CLI flag):

```toml
[store]
kind = "cockroach"

[embedder]
kind = "fixture"
dim = 1024
```

`kind = "fixture"` keeps the run off the network. The demo pins its write path
to canonical matching either way, so the embedder does not affect the outcome;
use `kind = "bge"` only if you want the recall vector leg live on camera.

Provision once:

```bash
./target/release/lambo --config ./lambo.toml provision
```

Expected: `cockroach schema provisioned via .../scripts/provision.sh`, exit 0.

---

## 1. Run one

```bash
./target/release/lambo --config ./lambo.toml demo --scenario rest-api \
  | tee demo-live-1.txt
```

Expected output — this is a real local run; on the cluster only the session id,
the `capabilities` line, the timings and the two masked values differ:

```
════════════════════════════════════════════════════════════════════════
  lambo demo — scenario rest-api   (spec §13: two agents, one REST API)
════════════════════════════════════════════════════════════════════════
  session      demo-rest-api-172e62ae-3f34-4b6b-af6a-7b29d10b442d   (fresh per run — P6 R3-1)
  capabilities Capabilities(VECTOR_SEARCH)
  agents       agent-a (builds the API) · agent-b (separate feature)

  Compressed for the video — intervals only, no threshold weakened:
    canonization_edge_min_age   60s     → 10ms
    canonization_eval_interval  60s     → 25ms  (frozen during the build)
    daemon_tick_interval        1s      → 5ms
    backend_flush_interval      1s      → 5ms
    gc_interval                 10000   → 1 mutation (spec default during the build)
  Untouched: min_peer_count 20, gc_survived ≥ 3, strictly > P90, distinct ≥ 3,
  coverage ≥ 0.3, blast radius > 5, conflict window 30s.

── ACT I — agent-a builds the REST API (9 interactions) ────────────────
  [ 1] derive         user schema, auth middleware, session store  → 3 created, 0 matched
  [ 2] derive         email / password hash / user id columns  (children of user schema)  → 3 created, 6 matched
  [ 3] record-action  write POST /users handler            depends on user schema  → 2 created, 3 edges
  [ 4] derive         created at / role columns, users table migration  (children)  → 3 created, 6 matched
  [ 5] record-action  write session middleware             depends on session store, user schema  → 2 created, 4 edges
  [ 6] derive         user serializer, validation rules, fixtures  (children)  → 3 created, 6 matched
  [ 7] record-action  add JWT verification                 depends on auth middleware, user schema  → 2 created, 4 edges
  [ 8] record-action  write user repository                depends on user schema  → 2 created, 3 edges
  [ 9] record-action  wire login endpoint                  depends on all three pillars  → 2 created, 7 edges
  agent-a released the single-writer lease

── ACT II — agent-b joins on a separate feature (2 interactions) ───────
  [10] derive         rate limiter, redis backend          (agent B's own feature)  → 2 created, 0 matched
  [11] record-action  add rate limiting middleware         depends on auth middleware, user schema  → 2 created, 4 edges
  agent-b released the single-writer lease

── ACT III — agent-a comes back for one last edit ──────────────────────
  [12] record-action  add oauth_id to user schema          MODIFIES user schema  → 1 created, 5 edges
  agent-a released the single-writer lease
  graph complete: 12 interactions, 27 concepts, 'user schema' blast radius 9
  GC headroom: closest to the eviction bar is 'users table migration' at 1.55× — nothing in this session is collectable

── CANONIZATION — the engine, not the script, promotes user schema ─────
  gc_survived floor 3 ≥ 3 — Stage 1's survival gate is open for every concept
  cycle   1   user schema              → Candidate   (canonization_events row written)
  cycle   2   user schema              → Venerable   (canonization_events row written)
  cycle   3   user schema              → Canonical   (canonization_events row written)
  no transitions for 3 consecutive cycles — the state machine is at its fixed point (5 events total)
  agent-a released the single-writer lease

── ACT IV — agent-b: recall("update user schema") ──────────────────────
  daemon event: Conflict on 'user schema' — contesting agents: agent-a, agent-b

  user schema [Entity, canonical] (score 2.27, blast radius 9)
  ⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.
  Agent A wrote to it 0 seconds ago
  High-risk modification: high-value node 445370df-69b2-47ac-94ab-b18b52b8b100 (Canonical, blast radius 9) modified within 30s

  add oauth_id to user schema [Resource] (score 1.46)

  user serializer [Logic] (score 0.75)

  user fixtures [Resource] (score 0.73)

  user validation rules [Constraint] (score 0.70)

  write user repository [Resource] (score 0.66)

  auth middleware [Entity] (score 0.15)
  Agent A wrote to it 0 seconds ago

  handlers/login.rs [Resource] (score 0.12)
  Agent B wrote to it 0 seconds ago

  agent-b does not make the breaking change.

── OUTCOME — the ×2 determinism bar ────────────────────────────────────
  scenario            rest-api
  interactions        12
  concepts            27
  edges               93
  canonization_events 5
    add oauth_id to user schema: None -> Candidate
    user schema: None -> Candidate
    user schema: Candidate -> Venerable
    user schema: Venerable -> Canonical  (blast radius 9)
    wire login endpoint: None -> Candidate
  canonical           1
    user schema  blast_radius=9
  statuses
    ...
    Candidate  add oauth_id to user schema
    Canonical  user schema
    Candidate  wire login endpoint
    ... (every other concept None)
  recall_warnings     5
  recall_context
    ...
```

Note the session id printed on line 4 — you need it in §3 and §4. Capture it:

```bash
SESSION_1=$(grep -m1 '  session ' demo-live-1.txt | awk '{print $2}')
echo "$SESSION_1"
```

---

## 2. Run two, and diff

```bash
./target/release/lambo --config ./lambo.toml demo --scenario rest-api \
  | tee demo-live-2.txt
SESSION_2=$(grep -m1 '  session ' demo-live-2.txt | awk '{print $2}')
```

The `OUTCOME` block is the ×2 bar. It already has the volatile values masked
(`<s>`, `<n>`, `<node>`), so diff it directly:

```bash
diff <(sed -n '/OUTCOME/,$p' demo-live-1.txt) \
     <(sed -n '/OUTCOME/,$p' demo-live-2.txt) && echo "IDENTICAL — T8.4 x2 met"
```

**Expected: no output from `diff`, then `IDENTICAL — T8.4 x2 met`.**

The two full transcripts will differ — different session ids, different real
`(score …)` values and `Agent A wrote to it N seconds ago` readings, different
node uuids. That is by design; see `demo/README.md` for what is masked and why.
The `OUTCOME` block is the artifact to screenshot for `dev-diary/evidence/`.

If `diff` reports a difference, keep both files and attach them — a real
divergence here is a finding, not a retry.

---

## 3. Read it back with the reader CLIs (no lease taken)

```bash
./target/release/lambo --config ./lambo.toml saints  --session "$SESSION_1"
./target/release/lambo --config ./lambo.toml inspect --session "$SESSION_1" --focus "user schema" --depth 2
./target/release/lambo --config ./lambo.toml stats   --session "$SESSION_1"
./target/release/lambo --config ./lambo.toml recall  --session "$SESSION_1" --query "update user schema"
```

Expected from `saints`:

```
1 canonical memory in session 'demo-rest-api-…'
  user schema [Entity, canonical]  blast_radius=9  accesses=…  since …
```

`recall` here is a **reader** process: it does not spawn the daemon, so it
renders the canonical marker and the ⚑ line but **not** the conflict line (the
hot list is populated by a running daemon). The conflict line belongs to the
demo's own Act IV, which is why the demo does its recall in-process. Do not read
its absence here as a regression.

---

## 4. Split screen — `canonization_events` through the CockroachDB MCP server

Spec §13 step 5. The managed MCP server's console-side setup is already done
(`dev-diary/PHASE-8-surface.md`); a client restart may be needed for the config
to take effect. Ask Claude Code, connected to that server, for:

```sql
SELECT node_id, from_status, to_status, blast_radius, occurred_at
FROM canonization_events
WHERE session_id = '<SESSION_1>'
ORDER BY occurred_at ASC;
```

Scoping by `session_id` matters — the cluster carries earlier phases' rows (see
the schema-divergence warning in §0).

Expected: four rows, three of them `user schema`'s, in this order — `None →
Candidate`, `Candidate → Venerable`, `Venerable → Canonical` with
`blast_radius = 9`. That is the promotion history that produced the ⚑ warning
agent B saw, read out of the database by a second tool. Screenshot it into
`dev-diary/evidence/`.

---

## 5. Failure modes worth recognising

| Symptom | Cause | Do |
|---|---|---|
| `demo: '<concept>' sits at N× GC's eviction bar` | the script drifted below the GC headroom floor | a code change, not an ops problem — report it |
| `demo: expected 27 concepts after the script, found N` | canonicalizer collision or an eviction | report with the transcript |
| `demo: timed out … waiting for user schema → Venerable` | store not answering `interaction_span` / the flush is not landing | check the DSN, cluster health, and that `provision` ran |
| `session … is already held by another writer` | a previous run's lease has not lapsed | wait out the lease TTL, or use the operator override the message prints |
| a run takes ~60s and then fails | the host suspended mid-run | the demo's waits are wall-clock bounded and the conflict window is 30s; a laptop that sleeps mid-run invalidates that run — re-run it awake |
| `init_schema: …` | schema missing or permissions | run `lambo provision` first |

---

## 6. What to record in `dev-diary/evidence/`

1. `demo-live-1.txt`, `demo-live-2.txt` (full transcripts).
2. The `diff` command and its empty output plus `IDENTICAL — T8.4 x2 met`.
3. A screenshot of the split-screen `canonization_events` query (§4).
4. The `saints` output for `$SESSION_1`.
