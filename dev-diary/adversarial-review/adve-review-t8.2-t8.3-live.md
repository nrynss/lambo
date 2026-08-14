# Live-CockroachDB review — T8.2 + T8.3 — 2026-08-14

**Cluster:** CockroachDB Cloud `nrynss` (serverless, GCP asia-south1), CCL v26.2.5
**Commit:** `b45c102` (branch `phase/p8-surface`)
**Toolchain:** rustc/cargo 1.97.1; binary `target/debug/lambo`
**Reviewer:** live run, scripted + model-driven probes; evidence in `dev-diary/evidence/live-review-t8.2-t8.3/`

## Verdict

```text
Live-Cockroach review — T8.2 + T8.3 — 2026-08-14, cluster nrynss, b45c102
Gates:            fmt [x] clippy [x] default-suite [x] live --ignored [x] vector-proof [x]
Lease (4):        cross-process refuse [x] release-on-close [x] crash-expiry [x] fencing-window [x]
MCP (5):          durable [x] N2-control-chars [~] N1-cap+starvation [~] N3/N4-leak [x/–] shutdown-durable [x]
CLI (6):          readers-lease-free [x] write-lease-fail-closed [x] CLI<->MCP-differential [x]
Full stack (7):   model-driven derive+recall [x/–] durable-in-cockroach [x] real-BGE-recall [–]
New findings:     L82-1 / P1 / src/mcp/serve.rs:55,337 / CONFIRMED
                  L82-2 / P2 / src/cli/caps.rs:154 / CONFIRMED
                  L82-4 / P2 / src/graph/hybrid.rs:478 / CONFIRMED (behavior)
                  L82-3 / P3 (runbook defect) / Pi has no MCP / CONFIRMED
Verdict:          REQUEST CHANGES
Notes:            model-driven MCP leg completed via OMP + DeepSeek Flash (works); but the recall
                  NEVER used real BGE — the derive surface stores no embeddings (L82-4). N4 not
                  fault-injected live; one ad-hoc "edge target not found" did not reproduce.
```

`[~]` = partially passed (recorded below). `[–]` = not reproducible as scripted (blocked).

---

## Result 1 — environment & build

- DSN reachable: `psql "$LAMBO_COCKROACH_DSN" -c "SELECT version();"` → `CockroachDB CCL v26.2.5`.
- Rust builds under the live feature set; `cargo fmt --check` exit 0.

## Result 2 — schema

`./scripts/provision.sh` re-applied idempotently. `SHOW`/verify confirms all tables present:
`sessions` (739 rows), `interactions` (706), `concepts` (1137), `edges` (365),
`canonization_events` (37), `reservations` (0), **`session_leases` (0 present)**.

## Result 3 — full suite + live + vector proof

Feature gate: `--features store-cockroach,embed-bge`.

- `cargo fmt --all -- --check` — clean.
- `cargo clippy --all-targets --features store-cockroach,embed-bge -- -D warnings` — clean.
- Default suite — green (652 lib passed, 8 ignored; 5 bin; integration green; 1 doctest).
- `-- --ignored` with BGE server up — **10 passed, 0 failed** (8 store/lease/canon conformance + 2 live-calibration):
  `conformance_suite`, `build_store_returns_working_adapter`,
  `vector_beam_size_reaches_the_server_and_keeps_statement_timeout`,
  `single_writer_lease_is_enforced_across_pools`, `vector_explain_camera_proof`,
  `cockroach_three_hop_progression_matches_memory`,
  `saints_and_stats_against_live_cockroach`, `live_smoke_against_llama_server`,
  `context_embedding_separation`, `report_bge3_cosine_distribution`.
  (The two `live_calibration` tests fail — not skip — if `LAMBO_LLAMA_EMBED_URL` is unset/empty; they pass with BGE up. That hard-fail-without-server is the documented `dsn_or_skip`-style honesty, correct.)
- `LAMBO_REQUIRE_VECTOR_INDEX=1 … -- --ignored vector` — **2 passed** (camera proof + beam-size), i.e. the vector index is selected, not just present.

## Result 4 — single-writer lease (live)

- **4a cross-process:** A acquires (`agent-a@cachyos-x8664#19762`). B exits 1 with:
  `"session live-lease is already held by another writer (agent-a@cachyos-x8664#19762) —
  it acquired the single-writer lease 13s ago and is still refreshing it. … operator can force
  a takeover: DELETE FROM session_leases WHERE session_id = '<session>';"` Exactly one lease row.
- **4b release/expiry:** SIGINT → row gone (0). `kill -9` → row lingers → expires after 46 s
  (TTL 45 s + 1 s poll granularity; `active_rows=0`).
- **4c fencing window:** expiry measured at 46 s (documented TTL 45 s). B acquires cleanly
  (holder flips to `agent-b`), and B's startup log shows `existing=true`; a reader stats shows
  `nodes=2 edges=1 concepts=1` — the seeded concept is replayed from Cockroach on takeover.

## Result 5 — MCP over Cockroach

- **5a durable happy path:** derive (2 concepts) + record_action (2 concepts, 2 edges) + recall +
  saints + stats all `isError:false`. Stats: `flush_lag≈518ms log_depth=0 flush_depth=15
  dead_lettered=0 degraded=false`. Store readback: `concepts=4 edges=8`; lease released on close.
  Recall returned the concepts, but **not via real BGE vectors**: `Concepts embedding IS NULL`
  for every concept in this session (and in every derive-surface session below). The recall scores
  are keyword/recency, not vector similarity. See **Finding L82-4.**
- **5b N2 control chars:** NUL (`U+0000`) → `isError:true`, `"concept.content contains a
  disallowed control character (U+0000); only tab and newline are allowed"`, 0 NUL rows in store.
  **U+202E (RTL override) → NOT refused**: `isError:false`, "derived 1 concept(s)", and the row
  lands in Cockroach (`content LIKE '%'||chr(8238)||'%'` → 1). **Finding L82-2.**
- **5c N1 fan-out cap + starvation:** 65 combined produces+modifies+depends_on → refused
  (`"must total at most 64 entries (65 given)"`). Four at-cap (64) calls each succeed
  ("65 concept(s) created, 64 edge(s) added" per call). **SIGTERM with the 784-mutation tail still
  un-flushed → `close()` times out at 10 s and discards the tail**: exit 1, stderr
  `"final flush failed — tail lost on exit, not durable … close timed out after 10s"` +
  `"Memory dropped after a close() that did not finish: 784 un-flushed mutations were discarded"`.
  Store readback: `concepts=0 edges=0`, lease row left stale. **Finding L82-1.**
- **5d N3/N4 leak:** serve with an unreachable embedder (`http://127.0.0.1:59999`) → recall
  degrades to keyword and returns the warning
  `"recall: query embedding failed (embedder unavailable: llama.cpp unreachable: error sending
  request for url <redacted-url> vector leg skipped"`. The URL is redacted (`<redacted-url>`);
  no host/port/path/DSN present in the response text, structured warnings, or server stderr.
  N3 = no leak. N4 (forced mid-session store error → class-not-driver-text) was **not** fault-injected
  live (no boundary hook); store errors are a typed `StoreError` by construction and the NUL and
  redaction probes both show class-not-detail behavior.
- **5e shutdown durability (small tail):** SIGINT → rc 0, `"session closed, tail durable"`,
  concepts durable (1), lease released. SIGTERM → identical clean exit. The load-dependent failure
  is 5c (L82-1), not the small-tail path.

## Result 6 — CLI over Cockroach

- **6a readers lease-free:** `recall`, `saints`, `inspect`, `stats` all rc 0 against a live
  session with **no** server; `session_leases` count unchanged at 0. Readers never acquire a lease.
- **6b writer fail-closed:** with `serve` holding `live-cli-6b`, `lambo derive` exits 1 naming
  `srv@cachyos-x8664#51944` + OPERATOR_OVERRIDE. After the server stops, the same derive acquires,
  writes (`derived 1 concept(s)`), and releases (lease count → 0, concept count → 1).
- **6c differential:** CLI derive "billing retries change [Entity]" → CLI `recall` returns
  `"billing retries change [Entity] (score 0.25)"`; MCP `lambo_recall` on the same session returns
  `"billing retries change [Entity] (score 0.68)"`. Identical concept text + type; the score
  differs because MCP recall runs a live daemon while CLI recall does not (documented in the
  T8.3 Handoff Log — the differential compares texts/types, not scores).

## Result 7 — full stack (BGE + LFM2.5 + Pi)

- **7a:** BGE-M3 `bge-m3-FP16.gguf` served at `127.0.0.1:8080`; LFM2.5-230M `UD-Q8_K_XL.gguf`
  served at `127.0.0.1:8081` (`--jinja -c 32768 -a lfm2.5-230m`). Both `/health` = `{"status":"ok"}`.
- **7b/7c — two drivers were tried.**
  - *Pi* (`@earendil-works/pi-coding-agent` 0.84.1) cannot do this at all: its README states
    **"No MCP."** A project `.mcp.json` is ignored, so the model never receives the `lambo_*` tools
    (L82-3, runbook defect).
  - *OMP* (the Oh My Pi harness, `omp` v17.3.4) **is an MCP client** and did drive it: with the
    project `.mcp.json` (Lambo `serve` on Cockroach + BGE at 8080), **DeepSeek Flash
    (`deepseek-v4-flash` / `-0731`) autonomously issued `lambo_derive` → `lambo_recall` →
    `lambo_stats`** over the MCP wire:
      - `lambo_derive` (2 concepts): `"derived 2 concept(s): 2 created, 0 matched existing"`
      - `lambo_recall` (query "which component retries billing payments"): returned the billing
        concept first (score 1.09) then the auth concept (0.68)
      - `lambo_stats`: `nodes=3 edges=3 concepts=2 … dead_lettered=0 degraded=false`,
        `canonization_cycles=1`
  - **Durability held**: the two concepts are physically in Cockroach (`omp-live2`), session
    contract stamped `bge_m3 / dim 1024`, lease released. So the model-driven MCP machine leg and
    the write-behind durability both **work**.
  - **But the recall did NOT use real BGE vectors**: both concepts stored with `embedding IS NULL`
    (see **L82-4**). The 1.09 / 0.68 scores are keyword/recency, not vector similarity. The
    runbook's "recall used real BGE embeddings" expectation is **not met by the current derive
    surface**.


---

## Findings

### L82-1 (P1, CONFIRMED) — SIGTERM discards an un-flushed tail under burst load; violates "tail durable"

- **Where:** `src/mcp/serve.rs:55` (`CLOSE_GRACE = 10s`) and `src/mcp/serve.rs:337`
  (`"close timed out after {}s; tail lost on exit, not durable"`).
- **Repro:** start `lambo serve` (Cockroach + BGE), issue four `lambo_record_action` calls each at
  the 64-entry fan-out cap (260 concepts / 256 edges ⇒ 784 log mutations), then send SIGTERM before
  the 1 s write-behind flush drains. Result: `close()` is capped at 10 s, times out, and the serve
  path drops `Memory` without flushing — exit code 1, `"784 un-flushed mutations were discarded"`,
  0 concepts/edges reach Cockroach, and the lease row is left stale (release never ran).
- **Why it matters:** the P8 exit criteria require *"SIGTERM still flushes the tail
  (`session closed, tail durable`)"* under K concurrent clients. That guarantee holds for small
  tails (Result 5e) but fails for a realistic at-cap burst (Result 5c). The in-memory log is the
  only copy (no WAL), so this is bounded-loss-on-shutdown, not a degraded-but-retained tail.
- **Not reproduced / open question for remediation:** why 784 vector rows cannot drain in 10 s —
  the flush may be applying the batch row-by-row to a serverless cluster. Root-cause the flush
  write path (bulk vs per-row) and/or raise `CLOSE_GRACE` toward the 45 s lease TTL instead of
  discarding.

### L82-2 (P2, CONFIRMED) — U+202E (bidi RTL override) accepted; only C0/C1 control chars are blocked

- **Where:** `src/cli/caps.rs:154` — `.find(|c| c.is_control() && *c != '\n' && *c != '\t')`.
  Rust `char::is_control()` returns true only for C0 (`U+0000–U+001F`) and C1 (`U+007F–U+009F`), so
  Unicode bidi/format controls (`U+202A–U+202E`, `U+2060–U+2069`, category *Cf*) pass through.
- **Repro:** `lambo_derive` with content containing `U+202E` → `isError:false`, "derived 1
  concept(s)", and the byte lands in the `concepts.content` column (verified with
  `chr(8238)` = `U+202E`). A NUL byte in the same position IS refused.
- **Why it matters:** the error message asserts *"only tab and newline are allowed"*, overstating
  the contract; a bidi override is a real prompt-injection/spoofing vector into the model's recall
  context. It does **not** poison the flush queue (Cockroach accepts the byte, so no
  retain-and-retry), which is why this is P2 not P1. Fix: also reject `char` in category `Cf`
  (or explicitly blacklist the bidi-control ranges) in the same `check_size` validator.

### L82-4 (P2, CONFIRMED — behavior — needed product decision) — the derive surface never persists concept embeddings, so vector recall never fires for freshly-derived data

- **Where:** `src/graph/hybrid.rs:478-486` — a `Resolution::Fresh` whose embedding finds no
  candidate above `semantic_match_threshold` is committed with `embedding: None`
  (`None => Resolution::Fresh { key, embedding: None }`). Only `Resolution::HybridMerge`
  (`hybrid.rs:476`, merging into an existing ≥-threshold concept) persists a vector. Every other
  `Fresh` arm (store lacks `VECTOR_SEARCH`, embed timeout/error, capability miss) also stores `None`.
- **Repro / evidence:** `SELECT (embedding IS NOT NULL) FROM concepts` → **0 of 13 concepts**
  across every derive-surface session (`live-mcp-5a/5b/5d/5d2/5d3/5e-term/5f-sigint`,
  `live-cli-6b`, `omp-live`, `omp-live2`) have a vector; all are `NULL`. The 198 vector-bearing
  concepts in the cluster come only from direct `Mutation::SetEmbedding` test/seeding batches
  (`conformance-vector-*`), not from any `derive`/`record_action` call. The BGE embedder itself
  works (health-checked, `live_smoke_against_llama_server` passes, `live_calibration` passes).
- **Consequence:** hybrid vector recall (`vector_candidates` over `concepts_embedding_idx`) returns
  nothing for freshly-derived concepts; recall degrades to keyword/recency for all organic data.
  This is why my earlier claim that 5a/7c used "real BGE embeddings" was **incorrect** — corrected
  in this record — and why the runbook's "recall used real BGE embeddings" expectation (5a, 7c)
  cannot be met by the current write surface.
- **Why P2 not P1:** the demo (T8.4) recalls via keyword + graph structure (canonical markers,
  blast radius, conflict lines) and still works; nothing crashes. But it materially breaks the
  product's vector-recall value proposition for new knowledge, and it contradicts the docs/runbook.
- **Decision needed (this is the MAJOR-1 precision-bias law made observable):** writing a fresh
  concept's own vector makes it a *future* vector candidate (the over-merge MAJOR-1 prevents). If
  vector recall is intended to serve organically-derived data, the persistence rule needs rework
  (e.g., store the fresh concept's embedding but exclude it from being a merge *source*, or revisit
  the threshold semantics). If keyword-only-until-merged/seeded is intentional, the reference docs
  and this runbook must stop claiming organic `derive` produces vector-recallable memories.

### L82-3 (P3, CONFIRMED — runbook defect, not a Lambo code defect) — §7b/§7c assume Pi has MCP

- **Where:** `dev-diary/adversarial-review/runbook-cockroach-live-t8.2-t8.3.md` §7b/§7c.
- **Repro:** Pi 0.84.1 (`@earendil-works/pi-coding-agent`) `README.md`: *"No MCP. Build CLI tools
  with READMEs … or build an extension that adds MCP support."* A project `.mcp.json` is ignored;
  the model never receives `lambo_*` tool definitions.
- **Remediation (for the runbook owner):** either (a) drive the model leg with a client that does
  speak MCP (Claude Code already proven in the T8.2 evidence), or (b) build a Pi extension that
  exposes the Lambo stdio MCP server. The two-llama-server leg (7a) and the raw-stdio probes are
  unaffected.

---

## Notes — could not reproduce / accepted

- **One-off "not found: edge `<id>` target `<id>`"** (from `src/graph/graph.rs:1311` record_edge).
  Observed once in an ad-hoc Python driver's SIGINT case (exit 1, stale lease). Did **not**
  reproduce under a clean deterministic driver (initialize → one derive → SIGHUP/SIGINT → rc 0,
  "session closed, tail durable", concept durable, lease released). Recorded as a driver artifact,
  not a finding; if it recurs it warrants its own investigation of the async-derive vs close race.
- **N4 forced store error** not reproduced: there is no boundary fault-injection hook to break a
  live store mid-session without a code change. Accepted on inspection — store failures are a
  typed `StoreError`, and the NUL + redaction probes demonstrate class-not-detail on the wire.
- **Stale lease rows from killed probes** (`live-mcp-5c`, `live-mcp-5d2`, `live-mcp-5e-int`) were
  observed mid-run and cleared; they are a downstream symptom of L82-1 (failed close skips release),
  not an independent defect.

## Evidence files

`dev-diary/evidence/live-review-t8.2-t8.3/` — `lease-test.sh` (4a/4b), `fence2.sh` (4c),
`5d-test.sh`, `cli-test.sh` (6a/6b/6c), `lambo-live.toml` + `lambo-bad-embed.toml`,
`pi.mcp.json` + `pi.lambo.cockroach.toml` (DSN redacted), and raw wire/log captures
(`result4a.b-refusal.log`, `result5d.*`, `result6b.*`, `result6c.*`). All secrets redacted and
re-scan confirmed clean.
