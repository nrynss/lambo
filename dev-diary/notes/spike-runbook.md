# Spike runbook — T0.2 / T0.3 / T0.4 (one session)

Goal of the session: **a recorded go/no-go on Rust** in PHASE-0's Handoff Log. Everything
else is secondary. Timebox: if step T0.3 isn't green by end of session, the spec §14
fallback (Python) fires — no overnight driver fights.

Suggested order: kick off T0.2 first (provisioning has wall-clock waits), run T0.4 while
waiting, then T0.3 on the live cluster.

---

## T0.2 — cluster + schema (~30 min, mostly waiting)

```bash
ccloud auth login
ccloud cluster create lambo --plan serverless   # or basic; capture the name it returns
ccloud cluster sql lambo                        # verify connectivity, note the DSN
```

- Put the DSN in `.env` as `LAMBO_COCKROACH_DSN`. Never in a script.
- Apply the spec §4 DDL. **Trap:** `CREATE VECTOR INDEX` may be gated by a cluster
  setting on some versions/plans (`SET CLUSTER SETTING feature.vector_index.enabled = true`).
  If the CREATE errors with an "experimental/disabled" message, that's the fix — record it,
  it goes in `provision.sh`.
- Script every command into `scripts/provision.sh` as you go (it's a deliverable, spec
  §12.1) with `IF NOT EXISTS` so it reruns.

## T0.4 — Bedrock smoke (~20 min, runs during provisioning)

Pre-check with the CLI before writing any Rust — it isolates account/permission problems
from code problems:

```bash
aws bedrock-runtime invoke-model \
  --model-id amazon.titan-embed-text-v2:0 \
  --body '{"inputText":"user schema","dimensions":1024,"normalize":true}' \
  --cli-binary-format raw-in-base64-out /dev/stdout | head -c 300
```

- **Trap:** model access is a per-region console toggle (Bedrock → Model access). If the
  CLI call 403s, fix it in the console first; no code will route around it.
- Then the same call via `aws-sdk-bedrockruntime` in `spikes/bedrock-spike/`. Record in the
  Handoff Log: region, model id string, request/response JSON shapes, latency of one call.
- Note `"normalize": true` (default) — normalized vectors make L2 and cosine rankings
  agree, which simplifies the distance-operator choice below.

## T0.3 — sqlx × VECTOR, the decision gate

`spikes/vector-spike/` — throwaway crate, `sqlx` (postgres, runtime-tokio, tls), connect
with `PgPool` to the T0.2 DSN.

Run the attempts in cost order; stop at the first one that passes all four checks
(insert → read-back fidelity → similarity query → EXPLAIN shows the vector index).

**Attempt A — string cast (cheapest, most likely to just work):**
```rust
// write: bind the vector as text, cast server-side
let s = format!("[{}]", v.iter().map(f32::to_string).collect::<Vec<_>>().join(","));
sqlx::query("INSERT INTO concepts (id, session_id, ..., embedding) VALUES ($1, $2, ..., $3::VECTOR)")
    .bind(id).bind(session).bind(&s).execute(&pool).await?;

// read: cast back to string server-side, parse in Rust
let s: String = sqlx::query_scalar("SELECT embedding::STRING FROM concepts WHERE id = $1")
    .bind(id).fetch_one(&pool).await?;
```
Round-trip check: parse and compare with a small epsilon (text formatting may drop
trailing precision — decide and record the tolerance).

**Attempt B — `pgvector` crate (`features = ["sqlx"]`):** `pgvector::Vector` implements
`sqlx::Type` against the type name `vector`. Cockroach speaks pgwire and its VECTOR is
pgvector-compatible, so this *may* bind directly. **Trap:** if sqlx negotiates the binary
format and Cockroach doesn't accept binary encoding for the vector type, you'll get opaque
encode/decode errors — that's your cue to fall back to A, not to debug the wire format.

**Attempt C — custom `sqlx::Type` impl** wrapping Attempt A's text encoding behind a
`LamboVec(Vec<f32>)` newtype. Only if A works but is too ugly to live in two adapters —
this is polish, not gate material.

**Similarity + index check:**
```sql
SELECT id FROM concepts
WHERE session_id = $1
ORDER BY embedding <-> $2::VECTOR
LIMIT 5;
```
- `<->` is L2, `<=>` cosine; with normalized Titan vectors either ranks the same — pick one
  and record it (T3.2/T7.3 must match the index's operator class).
- `EXPLAIN` must show a scan of `concepts_embedding_idx`, not a full scan. **Trap:** vector
  indexes typically only plan for the exact `ORDER BY embedding <op> $k LIMIT n` shape —
  extra predicates or a missing LIMIT can silently fall back to full scan. If EXPLAIN shows
  a full scan with the index present, simplify the query shape before concluding the index
  is broken.

**Recording the verdict (PHASE-0 Handoff Log):** which attempt won, the exact working
snippet, the distance operator chosen, the EXPLAIN output (also drop it in
`dev-diary/evidence/`), and any cluster settings flipped. T3.2 and T7.3 are written from
this entry alone.

**If nothing passes by end of session:** the verdict is still a deliverable. Write NO-GO
and the failure modes observed — the Python fallback decision is made in the Handoff Log,
deliberately, not by drift.

---

## After the session

- Update the status board in `dev-diary/README.md` (P0 counts, gate verdict).
- If GO: T0.1 (repo/CI skeleton) is the remaining P0 task; P1 can start immediately after —
  it needs only T0.1.
