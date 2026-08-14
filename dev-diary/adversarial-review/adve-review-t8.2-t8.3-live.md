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

---

# R1 remediation (2026-08-14)

Remediation agent, branch `task/live-l82-remediation` (off `phase/p8-surface` @ `713a2ae`).
The reviewer's text above is untouched; this section appends the disposition of the two
findings this round was scoped to. **L82-3 and L82-4 are NOT addressed here** — L82-3 is the
runbook owner's, and L82-4 needs the product decision the reviewer asked for.

**Disposition summary: 2 FIXED (both scoped findings), 2 UNTOUCHED (out of scope).**

| # | P | Disposition |
|---|---|---|
| L82-1 | P1 | **FIXED** — `060cca1`; root-caused to per-row flush, fixed by bulk writes; stale lease fixed separately. Generated PostgreSQL needs live re-verification |
| L82-2 | P2 | **FIXED** — `b8c0871`; category-Cf + TAGS block refused in the shared validator |
| L82-3 | P3 | **NOT THIS ROUND** — runbook defect, owner is the runbook |
| L82-4 | P2 | **NOT THIS ROUND** — needs the product decision the finding asks for |

## L82-1 — root cause, and why it was not the grace window

The reviewer's suspicion was right. `CockroachStore::flush` and `SqliteStore::flush` both
replayed a batch as

```rust
for m in &batch.mutations {
    // one sqlx::query(..).execute(&mut *tx).await per mutation
}
```

— one statement, sequentially awaited, inside one transaction. Against a *serverless* cluster
that is one network round-trip per mutation, so a flush cost `mutations × RTT`. The measured
784-mutation tail at the reviewer's 10–30 ms is **8–24 s**, which is exactly why a 10 s
`CLOSE_GRACE` could not cover it.

Raising `CLOSE_GRACE` was rejected. The tail is bounded only by how much a client writes
between flush ticks, so *any* fixed budget loses to a large enough burst — the window would
have moved the failure, not removed it. The per-mutation cost is what changed.

**The fix (priority 1 in the brief).** New `src/store/batch.rs` plans a `MutationBatch` into
statements: node and edge upserts are bucketed **by table** and emitted as multi-row
`INSERT … ON CONFLICT`; every other variant stays one statement per mutation. Both adapters
replay the plan, and the single-row upsert helpers are deleted, so each table has one SQL
definition serving both a 1-row seed and a 256-row flush chunk.

Bucketing by table rather than by adjacent run matters: `Graph::insert_concept` emits the
concept's `UpsertNode` and its `Derives` `UpsertEdge` back to back, so a burst's log
*alternates* node/edge one for one and a run-coalescing planner would have produced runs of
length 1 and bought nothing. The reordering argument (disjoint tables, `edges` has no FK on
source/target, interactions before concepts for `concepts.origin_interaction`, and every
mutation that can observe a row is a barrier) is in the module docs.

The subtle part is deduplication, which is **required** — both engines reject a multi-row
upsert whose input rows collide on the conflict target. A collapsed concept keeps the **last**
occurrence's ordinary columns and the **first** occurrence's canonization columns, because that
is what row-by-row replay left durable under R2-1's INSERT-only rule; collapsing naively to the
last row would have regressed the status of a concept born mid-progression. Edges deduplicate
on the natural key `(source, target, edge_type)`, not on `id`, matching their conflict target.

**The stale lease (priority 2 in the brief).** Separate bug, same finding. `close_bounded`
*drops* the `close()` future on timeout, so the release on close's success paths never runs —
which is why Result 5c saw the lease row survive. `close_bounded` now runs a bounded
best-effort `Memory::release_lease_after_abandoned_close` on **both** abandon paths (deadline
and second signal). A fenced handle still does not release: that lease belongs to whoever took
the session over. The release is not a durability claim — it asserts this process is gone,
which is true, and keeping the lease does not make the lost tail less lost.

The 2 s release window is carved **out of** `CLOSE_GRACE` (now 8 s flush + 2 s release), not
added on top, so `SHUTDOWN_BUDGET` and the documented supervisor SIGKILL contract are
unchanged. A compile-time assert pins the split, alongside the two the brief flagged.

**Tests.** 12 planner tests (the 784-mutation repro plans into ≤ 24 statements; barriers;
dedupe; chunking; every mutation covered exactly once). `sql_shape_is_a_multi_row_upsert`
asserts the *generated* PostgreSQL — 48 placeholders for 3 rows, a `::VECTOR` cast on each
row's embedding placeholder, exactly one `ON CONFLICT` — because that text is the half no
local test can put in front of a cluster. Two SQLite tests execute the dedupe and the chunking
against a real SQL engine, and both were verified to **fail** when the first-canonization rule
is broken. Five tests in `memory`, including a burst-drain contrast that reproduces the
timeout under the old per-mutation cost model and passes under the new one, and the lease
release driven through `serve`'s real bounded-close body.

**Needs live re-verification on the cluster** (cannot be reached from this machine):

1. `cargo test --features store-cockroach,embed-bge -- --ignored` — the conformance suite is
   the flush→load round-trip over the rewritten statements.
2. Result 5c's repro: four at-cap `lambo_record_action` calls then SIGTERM. Expect
   `session closed, tail durable`, rc 0, concepts/edges durable, `session_leases` row gone.
3. The stale-lease half specifically: force a close timeout (e.g. an unreachable cluster mid
   shutdown) and confirm the lease row is released or, failing that, the new
   "will lapse at LEASE_TTL instead" warning appears.

## L82-2 — what is refused now

`check_size` (the shared CLI+MCP validator) keeps its C0/C1 control check and adds
`DISALLOWED_FORMAT_RANGES`: Unicode general category **Cf** as of Unicode 16, plus the whole
`U+E0000–U+E007F` TAGS block (unassigned holes included — a superset costs nothing and survives
future assignments). Concretely: `U+00AD`, `U+061C`, `U+180E`, `U+200B`, `U+200E–U+200F`,
`U+202A–U+202E`, `U+2060–U+2064`, `U+2066–U+206F`, `U+FEFF`, `U+FFF9–U+FFFB`, `U+110BD`,
`U+110CD`, `U+13430–U+1343F`, `U+1BCA0–U+1BCA3`, `U+1D173–U+1D17A`, `U+E0000–U+E007F`.

Two deliberate exceptions, both documented at the table:

* **`U+200C` ZWNJ and `U+200D` ZWJ are allowed.** They are orthographically required in Persian
  and several Indic scripts and are the glue in emoji ZWJ sequences; refusing them would reject
  legitimate concept text. Neither can reorder or conceal a visible character — they only join
  or separate adjacent glyphs — so the threat model (bidi spoofing, invisible smuggling) is
  still fully covered.
* **Arabic number-formatting signs** (`U+0600–U+0605`, `U+06DD`, `U+070F`, `U+0890–U+0891`,
  `U+08E2`) are Cf but are ordinary Arabic text with no direction or concealment capability.
  `U+061C` ARABIC LETTER MARK *is* refused — it is a bidi control.

The overstating single message is split in two, each claiming only what its own check enforces:
the control message may say "tab and newline are the only control characters allowed" because
for *control* characters that is now exactly true; the format message names its own exceptions
instead. Tab and newline remain allowed.

Tests: three unit cases in `caps` (ten refused codepoints by class, the allowances, and
message-vs-check agreement) and five new over-the-wire cases on the existing MCP N2 test —
`U+202E`, `U+200B`, `U+2066`, `U+FEFF` and a TAGS character now return `isError:true` through
`lambo_derive`, which is the live repro pinned at the boundary it escaped through.

**Live re-verification:** re-run Result 5b's `U+202E` probe; expect `isError:true` and
`SELECT count(*) FROM concepts WHERE content LIKE '%'||chr(8238)||'%'` = 0 for the session.

## Gates

Full binding block from `dev-diary/PHASE-8-surface.md`, all green:

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --features store-sqlite -- -D warnings` | clean |
| `cargo test` | 640 lib + 5 bin + integration, 0 failed, 1 ignored |
| `cargo test --features store-sqlite` | 686 lib + 5 bin + integration, 0 failed, 1 ignored |
| `cargo test --no-default-features --features store-sqlite --no-run` | builds |
| `cargo test --no-default-features --features store-cockroach --no-run` | builds |
| `cargo check --no-default-features` | clean |

+23 tests, **0 removed**, no existing test weakened (verified: the diff against
`phase/p8-surface` removes no `#[test]` / `#[tokio::test]` attribute).
