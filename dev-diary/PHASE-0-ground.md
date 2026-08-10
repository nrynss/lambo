# P0 — Ground & spike

```yaml
id:       P0
requires: []
blocks:   everything
parallel: partial   # T0.1 ‖ T0.2 ‖ T0.4; T0.3 needs T0.2
```

**Goal:** kill the one risk that can invalidate the whole language choice (spec §14 decision
gate), and give every later agent a repo that builds, lints, and tests in CI.

**This phase is a day, not more.** Its output is a go/no-go on Rust and a skeleton.

---

### T0.1 — Repo, license, CI, workspace
```yaml
requires:   []
fixture-ok: n/a
owns:       Cargo.toml, .gitignore, LICENSE, README.md (stub), .github/workflows/, rust-toolchain.toml, src/main.rs, src/lib.rs (stubs)
status:     done
```
Single crate `lambo`, binary + lib. MIT `LICENSE` (spec §12.4 — must be detectable in the
GitHub About section, so use the stock MIT text verbatim). `.gitignore` covers `target/`,
`.env`. CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`. Stub README
stating the single-writer deployment model in one paragraph (spec §2.2) — expanded in T9.1.
Commit `.env.example` with `LAMBO_COCKROACH_DSN`, `AWS_REGION`, AWS credential names;
never `.env`.

Module skeleton in `src/lib.rs`: `types`, `graph`, `store`, `daemon`, `recall`, `canon`,
`embed`, `mcp`, `cli` — empty `mod.rs` files so every later task's `owns` directory exists
and `Cargo.toml`/`lib.rs` churn is front-loaded here, not fought over mid-swarm.

Dependencies (spec §6.3): `tokio`, `sqlx` (postgres, sqlite, uuid, chrono, runtime-tokio),
`rmcp`, `axum`, `aws-sdk-bedrockruntime`, `rust-stemmers`, `unicode-segmentation`,
`parking_lot`, `clap`, `tracing`, `tracing-subscriber`, `serde`, `serde_json`, `uuid`,
`chrono`, `async-trait`.

**Done when:** CI is green on the initial commit and `cargo run -- --help` prints a clap
stub.

---

### T0.2 — Cluster provisioning + schema DDL applied
```yaml
requires:   []
fixture-ok: n/a
owns:       scripts/provision.sh, migrations/cockroach/
status:     done
```
`ccloud` CLI: create the cluster, capture the DSN, apply the spec §4 DDL verbatim
(including `CREATE VECTOR INDEX`). Script it — `scripts/provision.sh` is itself a
deliverable (spec §12.1, recorded for the video). Store the DSN in `.env`, never in the
script.

**Done when:** `psql`/`cockroach sql` against the cluster shows all seven tables and the
vector index, and `provision.sh` reruns idempotently (CREATE ... IF NOT EXISTS).

---

### T0.3 — Spike: `sqlx` × CockroachDB `VECTOR` ★ DECISION GATE
```yaml
requires:   T0.2
fixture-ok: n/a
owns:       spikes/vector-spike/
status:     done
```
The single most likely place to lose half a day (spec §14). In a throwaway crate:

1. Connect with `sqlx::PgPool` to the T0.2 cluster.
2. INSERT a row into `concepts` with a 1024-dim embedding. `sqlx` has no native binding for
   Cockroach's `VECTOR` — expect to encode as a string literal (`'[0.1,0.2,...]'::VECTOR`)
   or write a custom `sqlx::Type` impl. Record which worked.
3. Read it back and verify round-trip fidelity.
4. Run a vector-index query (`ORDER BY embedding <-> $1 LIMIT k`) and confirm the index is
   used (`EXPLAIN`).

**Done when:** all four steps pass, and the working encode/decode snippet + any gotchas are
written into the Handoff Log (T3.2 and T7.3 build directly on it).
**If it fails by end of session:** invoke the spec §14 fallback — the whole build flips to
Python. That decision is made here and nowhere else; escalate to the human, do not drift.

---

### T0.4 — Spike: Bedrock Titan embeddings smoke
```yaml
requires:   []
fixture-ok: n/a
owns:       spikes/bedrock-spike/
status:     blocked-account   # crate ready; InvokeModel denied until Bedrock use-case form
```
`aws-sdk-bedrockruntime`: embed one string with Titan Text Embeddings V2, confirm 1024
dims, note the request/response shapes, region, and the model id string. Also confirm the
account has model access enabled (a console-side toggle that has burned people before).
Capture cost/latency of a single call in the Handoff Log.

**Done when:** a 1024-dim vector prints, and T7.1 can be written from the Handoff Log alone.

---

## Exit criteria

- [x] CI green, license detectable, `.env.example` committed
- [x] Cluster live, schema applied, `provision.sh` idempotent
- [x] **Go/no-go on Rust recorded** (T0.3 Handoff Log) → **GO**
- [ ] Bedrock access proven end-to-end — **blocked on account use-case form** (see Handoff)

---

## Handoff Log

### T0.1 — Repo skeleton (2026-08-10)

- Single crate `lambo` bin+lib; modules: types/graph/store/daemon/recall/canon/embed/mcp/cli.
- MIT LICENSE (Narayan S S, 2026). `.env.example` committed; `.env` gitignored.
- `aws-config` with `credentials-login` feature (required for `aws login` sessions).
- `cargo fmt` / `clippy -D warnings` / `cargo test` clean; `cargo run -- --help` works.
- CI: `.github/workflows/ci.yml` (fmt, clippy, test).
- Note: `rmcp` pinned at 0.1.x in Cargo.toml (latest is v3 — upgrade carefully in P8).

### T0.2 — Schema (2026-08-10)

- Cluster: **nrynss** (serverless, GCP asia-south1, CRDB **v26.2.5**).
- DB: `lambo`. DSN in `.env` with `sslmode=verify-full&sslrootcert=system` (libpq/psql).
- Applied: all **7 tables** + **`concepts_embedding_idx`** VECTOR index.
- `SET CLUSTER SETTING feature.vector_index.enabled = true` succeeded.
- `scripts/provision.sh` splits base DDL vs VECTOR INDEX; re-runs idempotent
  (`IF NOT EXISTS` / notices for existing relations).
- Gotcha: early statement-splitter bug only created `sessions`; fixed by `run_sql_file`.

### T0.3 — sqlx × VECTOR ★ **VERDICT: GO (Rust)** (2026-08-10)

Evidence: `dev-diary/evidence/t0.3-vector-spike.txt`

| Check | Result |
|-------|--------|
| Connect `sqlx::PgPool` | OK (~340ms cold) |
| INSERT 1024-dim via text + `$n::VECTOR` | **Attempt A works** |
| Round-trip `embedding::STRING` | max abs diff **0** (eps 1e-4) |
| Similarity `ORDER BY embedding <-> $1::VECTOR LIMIT k` | top hit correct |
| EXPLAIN pure (no session filter) | **`vector search` on `concepts@concepts_embedding_idx`** |
| EXPLAIN filtered (`WHERE session_id = $1`) | **does NOT use vector index** — scans session unique index |

**Working encode/decode (use in T3.2 / T7.3):**

```rust
// write
let s = format!("[{}]", v.iter().map(f32::to_string).collect::<Vec<_>>().join(","));
sqlx::query("... embedding = $n::VECTOR ...").bind(&s)

// read
let s: String = sqlx::query_scalar("SELECT embedding::STRING FROM concepts WHERE id = $1")
    .bind(id).fetch_one(&pool).await?;
// parse "[a,b,c]" → Vec<f32>
```

**Distance operator:** `<->` (L2). With Titan `normalize: true`, L2 ranking agrees with cosine.

**DSN gotcha for sqlx+rustls:** libpq's `sslrootcert=system` is not understood — rewrite to
`/etc/ssl/certs/ca-certificates.crt` (see `rewrite_dsn_for_rustls` in spike) or use
`sslmode=require`. Keep `sslrootcert=system` in `.env` for docker/psql.

**Query-shape gotcha:** pure `ORDER BY embedding <-> $1::VECTOR LIMIT n` hits the vector
index. Adding `WHERE session_id = $1` made the planner skip it (recommends a secondary
index storing embedding). T3.2 should either:

1. vector-search globally then filter session in Rust, or
2. add a composite strategy after measuring, or
3. accept filtered top-k without vector index at small scale.

**Attempts B/C not needed** — A is clean enough for adapters.

### T0.4 — Bedrock Titan (2026-08-10) — **BLOCKED (account)**

Evidence: `dev-diary/evidence/t0.4-bedrock-blocked.txt`

- Spike crate: `spikes/bedrock-spike/` builds.
- Required SDK feature: `aws-config` **`credentials-login`** (without it: "ProfileFile
  provider could not be built... credentials-login").
- Request shape (T7.1):
  ```json
  {"inputText":"user schema","dimensions":1024,"normalize":true}
  ```
  model id: `amazon.titan-embed-text-v2:0`
- **Invoke fails** on both `ap-south-2` and `us-east-1`:
  `ValidationException: Operation not allowed`
- Root cause: `aws bedrock get-use-case-for-model-access` →
  **"You have not filled out the request form."**
- **Human action:** AWS Console → Bedrock → Model access / enable foundation models
  (complete use-case form), then re-run:
  ```bash
  cd spikes/bedrock-spike && AWS_REGION=ap-south-2 cargo run
  # or us-east-1 if Titan is only enabled there
  ```
- Until then: keyword-only path remains lawful degraded mode (spec §3.2 / P7 degradable).

### Open for human

1. Enable Bedrock model access (use-case form) → re-run T0.4 → mark done.
2. Optional: `git remote` + push so GitHub About shows MIT.
