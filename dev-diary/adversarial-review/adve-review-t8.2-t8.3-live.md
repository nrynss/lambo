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
                  L82-4 / P2 / src/graph/hybrid.rs:478 / CONFIRMED (behavior) — FIXED 2a9ee34
                       └─ adversarial review round: semantic RATIFIED; 3 REQUEST CHANGES
                          items (P2-1/P3-1/P3-2) all FIXED 5d3de66 — see §7 below
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

**DISPOSITION — FIXED (branch `task/l82-4-fresh-embeddings`, commit `2a9ee34`).**

- **Decision taken (user-approved 2026-08-14):** freshly-derived concepts SHALL persist their
  embedding, so organically-derived data becomes vector-recallable — without loosening the
  precision-bias / anti-over-merge law (MAJOR-1).
- **What changed:** exactly one arm. `src/graph/hybrid.rs`, the below-threshold
  `Resolution::Fresh` (the `hybrid.rs:478-486` this finding cites) now commits
  `embedding: Some(vec)` instead of `None`. Every arm where **no vector exists** is untouched and
  still writes `None` — capability absent, embed failure, embed timeout, a store that refuses
  `vector_candidates` after advertising the capability, and the invalid non-Concept merge target
  (P7 MINOR-2). A vector is never invented. **No store change was required:**
  `Mutation::UpsertNode` already carries the whole `Concept` and every adapter's concept upsert
  already binds `embedding` (Cockroach `UPSERT_CONCEPT_SQL` `$15::VECTOR`), which is why the fix
  is 2 files and 0 migrations.
- **Exclusion semantic chosen — THRESHOLD-PRESERVING** (recall-visible, merge bar unchanged);
  written up in full in the `hybrid.rs` module doc under "Vector persistence for fresh concepts":
  1. *A refusal is never recorded as an endorsement.* A below-threshold concept gets its vector
     but **no `Semantic` edge**. Recall expansion (spec §8) travels `Semantic` edges, so it is
     reachable only by its own similarity to the query — never
     transitively out of an unrelated concept's neighbourhood. **The over-merge this prevents:**
     A and B at cosine 0.84 must not be joined; if they were, a later recall on A would pull in B,
     and through B whatever later merges into B — collapsing distinct topics into one recall
     neighbourhood via links no single comparison ever endorsed.
     **[Corrected 2026-08-15 per P3-1, commit `5d3de66`.]** This paragraph originally also
     claimed *"P6's physical fold"* travels `Semantic` edges. **There is no physical fold** —
     canonization is a status-transition machine and `EdgeType::Semantic` appears nowhere in
     `src/canon/` (Stage 2 `interaction_span` and Stage 3 `blast_radius` count only
     `Dependency`/`Causal`/`Hierarchical`). The real canonization coupling is **scoring, not
     traversal**: a `Semantic` edge raises the concept's incident-edge count, which feeds
     `density` — the heaviest weighted dimension at **0.35** (`ScoringWeights::default`) — plus a
     small `edge_type_bonus`, and those composite scores drive Stage 1's P90 peer gate and GC's
     `MIN_CONCEPT_SCORE`. The decision is unchanged and in fact rests on firmer ground:
     canonization is *less* exposed than the original claim implied, and the case for refusing
     the edge stands on the recall-neighbourhood harm above.
  2. *The merge bar did not move.* `score >= semantic_match_threshold` (0.85), `best_candidate`'s
     finite/`[0,1]`/deterministic-tie validation, and the commit-time "target really is a Concept"
     check are all unchanged. Persisting a vector lowers nothing.
  3. *A vector minted in a call cannot drive a merge inside that call* — candidates come from the
     store, which cannot see that call's staged, un-flushed writes.
- **Deliberately NOT claimed (the honest residue):** once flushed, a fresh vector **is** a legal
  merge *target* for a later derive. `vector_candidates` is one query over
  `embedding IS NOT NULL` and cannot tell the merge leg from the recall leg apart, so a strict
  target-exclusion would need durable per-vector provenance (a new `concepts` column + migration
  in every adapter) and — because after this change *every* organic concept is fresh-persisted —
  it would exclude every organic concept and leave the merge leg permanently inert. That is a
  larger product change than this finding asks for. The threshold, not the vector's provenance,
  is the precision instrument.
- **Live re-verification needed:** `SELECT (embedding IS NOT NULL) FROM concepts` on a NEW organic
  session must now be true (the pre-existing 13 rows stay NULL — this is not a backfill). Watch
  for the second-order effect: organic `Semantic` merge edges become reachable for the first time
  (previously the pool was empty), so `edges WHERE edge_type='semantic'` and P6 canonization
  cycles on organic sessions are worth eyeballing against the 0.85 BGE-M3 calibration.
  **Sharpened 2026-08-15 (P3-1 + P2-2, commit `5d3de66`):** the canonization path to watch is
  **scoring, not traversal** — a new organic `Semantic` edge raises the concept's incident-edge
  count and therefore `density` (weight 0.35), shifting Stage 1's P90 peer gate and GC's
  `MIN_CONCEPT_SCORE` pressure. So the query to run alongside the edge count is the
  **canonization-event rate and promotion mix on organic sessions before vs after**, not a search
  for folded concepts (there is no fold). And per P2-2, the run should measure the **far-class
  score distribution under production-length origin context**, not merely that
  `embedding IS NOT NULL`.
- **Tests added / changed:** `far_text_creates_fresh_concept_with_vector_but_no_merge` (renamed
  from `far_text_creates_fresh_keyword_concept`; vector assertion inverted per this decision,
  every other assertion unchanged), `first_use_empty_candidates_still_commits_contract` (same
  inversion + a new zero-Semantic-edge assertion), `persisted_fresh_vector_does_not_lower_the_merge_bar`,
  `vectors_minted_in_this_call_cannot_merge_within_it`, and
  `memory::tests::organic_derive_persists_a_vector_that_recall_finds` — derive → flush → reload →
  recall end to end against a vector-capable store double doing exact cosine over persisted
  vectors, asserting both the durable `embedding IS NOT NULL` and that the vector leg returned the
  organic concept at `>= 0.85` for a keyword-disjoint query.
- **Docs:** `docs/reference/end-to-end.mdx` and `mcp.mdx` were checked and mention neither
  embeddings nor vector recall, so there was no false claim to correct and none was added.
- **F18:** no wire schema or output field changed; `f18_tool_schemas_match_the_golden_property_set`
  and `f18_no_tool_schema_accepts_a_client_timestamp` pass unchanged.
- **Gates:** full PHASE-8 binding block green — `fmt`, all three `clippy` variants, `cargo test`
  (623 lib / 5 bin / 8 integration / 1 doctest, 0 failed), `cargo test --features store-sqlite`
  (667 lib, 0 failed), both `--no-default-features … --no-run` rows, and
  `cargo check --no-default-features`.

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

# L82-4 review round — ratification of the fresh-vector merge semantic

```
╔══════════════════════════════════════════════════════════════════╗
║  Scope:    L82-4 remediation only — 2a9ee34 + 080b4a0 on 713a2ae ║
║            (src/graph/hybrid.rs, src/memory.rs, this record)     ║
║  Reviewer: L824AdveReview (adversarial; findings only, no fixes) ║
║  Ruling:   SEMANTIC RATIFIED — threshold-preserving is SOUND     ║
║  Verdict:  REQUEST CHANGES (test-strength + doc-accuracy;        ║
║            no change to the ratified product decision)           ║
╚══════════════════════════════════════════════════════════════════╝
```

Out of scope by instruction (concurrent work on another branch): `src/mcp/serve.rs`,
`src/store/flush.rs`, `src/store/cockroach.rs`, `src/cli/caps.rs`.

## 1. RULING ON THE MERGE-TARGET SEMANTIC — **RATIFY threshold-preserving**

The strict reading (exclude fresh vectors as merge *targets*) is **refuted**; no third
semantic is required. Five independent reasons, in descending weight:

1. **A "merge" does not collapse identity, so the word overstates the risk.**
   `Resolution::HybridMerge` (`src/graph/hybrid.rs:689-753`) creates a **new, distinct**
   concept node and writes **one decaying `Semantic` edge** to the target. Nothing is
   absorbed: no node is deleted, no canonical key is aliased, no content is rewritten,
   `matched` is deliberately not touched (P7 MINOR-3). The worst case reachable through
   this leg is therefore *recall-neighbourhood adjacency*, never "two topics became one
   record". Every over-merge scenario has to be re-read with that ceiling in mind.
2. **Provenance is not a quality signal — the 2026-08-12 law rested on a conflation.**
   The original MAJOR-1 remediation (`PHASE-7-embeddings.md:138-143`) justified writing
   `None` as: *"a 'far' concept would have retained a vector and become a future vector
   candidate — the exact over-merge the precision bias prevents."* Being a **candidate**
   is not an over-merge; a second, independent `>= 0.85` judgment is still required before
   anything is written. A concept that scored 0.83 against some *unrelated* neighbour on
   the day it was created carries no evidence that *its own* vector is untrustworthy. The
   threshold is the precision instrument, exactly as the module doc now argues.
3. **The strict reading has a measured cost and no reachable benefit.** Because every
   organic concept takes the below-threshold arm, `embedding: None` kept the candidate
   pool permanently empty, so the semantic-merge leg was **dead code on organic data** —
   the 0-of-13 measurement in this very record. A law whose only observable effect is to
   disable the feature it guards is not a law worth preserving.
4. **The transitive chain is real, newly reachable, and bounded.** A—C at 0.86 and C—D at
   0.86 admits cos(A,D) ≈ 0.48 (arccos 0.86 ≈ 30.7°, doubled), and recall at
   `traversal_depth >= 2` will pull D into A's neighbourhood. This genuinely could not
   happen before (empty pool). It is nonetheless contained: `Semantic` is the **last**
   priority in recall expansion (`src/recall/expand.rs:63`), expansion is depth-bounded,
   `Semantic` decays and is GC-eligible (`src/daemon/gc.rs:462`), and — see finding P3-1 —
   canonization's Stage 2/Stage 3 gates exclude `Semantic` **entirely**. Chain drift costs
   recall precision at depth; it cannot promote, fold, or destroy anything.
5. **The no-`Semantic`-edge-on-refusal guarantee is airtight in every arm.** Verified by
   reading all five `Fresh` constructions plus the merge arm: capability-absent
   (`hybrid.rs:465`), embed timeout (`:490`), embed error (`:503`), store capability-miss
   after a successful embed (`:571`), below-threshold (`:554`), and the non-Concept target
   degrade (`:700-712`, still `embedding: None`, still no edge). Only the below-threshold
   arm changed; `Fresh` structurally has no target, so it cannot emit a `Semantic` edge.
   Mutation (b) below confirms two tests fire the moment a refusal endorses an edge.

**Conclusion:** the honest residue the implementer flagged (a flushed fresh vector is a
legal merge *target*) is the correct place to land. Strict target-exclusion would need
per-vector provenance in every adapter **and** would re-inert the merge leg permanently,
buying a guarantee that reason 2 shows was never load-bearing. Ratified.

**What actually deserves attention is the threshold's calibration basis, not the
provenance rule — see P2-2.**

## 2. Mutation-check of the new pins

Each mutation applied alone to a clean tree, `cargo test --lib`, then reverted via
`git checkout`. Tree left clean.

| # | Mutation | Result | Caught by |
|---|---|---|---|
| a | below-threshold arm back to `embedding: None` | **CAUGHT** — 618 passed / **5 failed** | `organic_derive_persists_a_vector_that_recall_finds`, `far_text_creates_fresh_concept_with_vector_but_no_merge`, `first_use_empty_candidates_still_commits_contract`, `persisted_fresh_vector_does_not_lower_the_merge_bar`, `vectors_minted_in_this_call_cannot_merge_within_it` |
| b | refusal emits a `Semantic` edge to the best **sub**-threshold hit | **CAUGHT** — 621 passed / **2 failed** | `far_text_creates_fresh_concept_with_vector_but_no_merge`, `persisted_fresh_vector_does_not_lower_the_merge_bar` |
| c | `best_candidate` threshold filter bypassed (`>= 0.0`) | **CAUGHT** — 621 passed / **2 failed** | `far_text_creates_fresh_concept_with_vector_but_no_merge`, **`persisted_fresh_vector_does_not_lower_the_merge_bar`** (as predicted) |
| d | staged vectors visible within the call (commit-loop sibling merge at `>= semantic_match_threshold`) | **NOT CAUGHT — 623 passed, 0 failed** | *(none)* — see P2-1 |
| d′ | same, but unconditional (`>= 0.0`) | CAUGHT — 1 failed | `vectors_minted_in_this_call_cannot_merge_within_it` |

(a), (b), (c) are properly pinned. (d) is not — this is the review's principal finding.

> **Post-remediation (commit `5d3de66`):** mutation (d) re-applied verbatim to the strengthened
> tree is now **CAUGHT — 622 passed / 1 failed**, and so is a stealth variant (d′′) that writes
> the edge without touching `outcome.semantic_merged`, which fails on the zero-`Semantic`-edge
> assertion itself. See the P2-1 disposition for the full table. (a)–(d) are all pinned.

## 3. Findings

### P2-1 (CONFIRMED) — `vectors_minted_in_this_call_cannot_merge_within_it` does not pin property 3

- **Where:** `src/graph/hybrid.rs:1771` (test), asserting `0` `Semantic` edges between two
  brand-new siblings of one derive call.
- **Evidence:** mutation (d) added an explicit within-call merge to the commit loop — for
  each `Fresh` concept carrying a vector, compare against every concept already written
  this call and write a `Semantic` edge at `>= semantic_match_threshold`. That is precisely
  the behaviour property 3 forbids. **All 623 tests passed.** Re-running the identical
  mutation with the bar dropped to `>= 0.0` (d′) *did* fail the test, isolating the cause.
- **Root cause (measured):** the test's two siblings are `NEAR_A` / `NEAR_B`, but hybrid
  embeds `context_text` (`hybrid.rs:226-231`), not the bare label, and
  `FixtureEmbedder::seed_for` (`src/embed/fixture.rs:69-80`) only maps the **exact** strings
  `"register user"` / `"create account"` into one seed family — any framed variant falls
  through to a per-string hash. Measured with a scratch probe (since reverted):

  ```
  bare   cosine("register user",                          "create account")                          = 0.99999
  framed cosine("register user — two new ideas in one turn","create account — two new ideas in one turn") = 0.01406
  prefixed cosine("Concept: register user",               "Concept: create account")                 = 0.00098
  ```

  The two staged siblings sit at **0.014**, so the zero-edge assertion holds for a reason
  unrelated to within-call visibility. It is vacuous.
- **Why it matters:** the module doc states each of the three properties is "pinned by a
  test", and the disposition above lists this test as the pin for property 3. It is not.
  The test's only load-bearing assertion is `store.vector_calls() == 2`, which counts
  queries but does not establish that they preceded the writes. Property 3 is true today
  by construction (all `vector_candidates` calls are issued in the gather phase, before
  the write lock is taken) — but it is unguarded, so a future refactor that moves the
  candidate query into the commit loop would ship silently.
- **Fix direction:** either drive the sibling case through a store double that returns the
  *other* sibling's id (making the assertion about visibility rather than similarity), or
  give `FixtureEmbedder` a family whose seed survives context framing, or assert the
  structural invariant directly (no `vector_candidates` call after the first
  `insert_concept`).

**DISPOSITION — FIXED (branch `task/l82-4-fresh-embeddings`, commit `5d3de66`).** Finding
accepted in full; the pin was vacuous exactly as measured. Took the second fix direction, at
the test-double layer rather than in `FixtureEmbedder` itself (changing the production fixture's
seed families would have moved the near/far geometry under every other test in the suite).

- **What changed.** `RecordingEmbedder::context_tolerant()` (`src/graph/hybrid.rs`) reduces
  hybrid's framed context text back to the bare concept label before delegating to
  `FixtureEmbedder` — the exact inverse of `context_text`, and the same device
  `memory::tests::ContextTolerantEmbedder` already used. It still records the *framed* text, so
  the context rule stays assertable. `RecordingEmbedder::new()` is unchanged and every other
  test in the module still uses it, so no existing geometry moved.
  `vectors_minted_in_this_call_cannot_merge_within_it` now builds its embedder with
  `context_tolerant()`, and the two staged siblings sit at **0.99999** instead of 0.014.
- **The vacuity is now impossible to reintroduce silently.** The test asserts the *precondition*
  explicitly before the zero-edge assertion: `cosine(staged[0], staged[1]) >=
  SEMANTIC_MATCH_THRESHOLD_DEFAULT`, with the message "the siblings must be a merge-eligible
  pair or this test proves nothing (P2-1)". If a future change to the embedder double or the
  fixture ever pushes the pair back below the bar, the test fails on *that* line rather than
  passing for the wrong reason. The zero-edge assertion's own message now names why it holds.
- **Mutation re-check** (reviewer's mutation (d), re-applied verbatim to the commit loop: for
  each `Fresh` concept carrying a vector, compare against every concept already written this
  call and write a `Semantic` edge at `>= semantic_match_threshold`):

  | Tree | `cargo test --lib` | Caught by |
  |---|---|---|
  | (d) on the **old** test (reviewer's run) | 623 passed / **0 failed** | *(none — vacuous)* |
  | (d) on the strengthened test | 622 passed / **1 failed** | `vectors_minted_in_this_call_cannot_merge_within_it` (`out.semantic_merged.is_empty()`) |
  | (d′′) same, **stealth** — edge written, `semantic_merged` deliberately not touched | 622 passed / **1 failed** | same test, now on the **zero-`Semantic`-edge assertion itself** (`left: 1, right: 0`, message reporting sibling cosine 0.999993622303009) |
  | mutation reverted | **623 passed / 0 failed** | — |

  The stealth variant is the one that matters: it proves the zero-edge assertion — the assertion
  the review showed never bit — is now load-bearing on its own, not merely riding on
  `semantic_merged`. Tree left clean (`git status` clean, `git diff` on the mutation empty).
- **Lib count unchanged at 623** — this strengthens an existing test rather than adding one.
- **Not done, and why:** the third fix direction (assert no `vector_candidates` call after the
  first `insert_concept`) would pin the *mechanism*; the above pins the *property*. The property
  assertion is the one the module doc claims, and it now fails under a faithful violation from
  any mechanism, including ones that do not route through `vector_candidates` at all — which the
  ordering assertion would have missed. Recorded as a deliberate choice, not an oversight.

### P2-2 (PLAUSIBLE) — the 0.85 bar was calibrated on *short-sentence* context; production admits a 16 KB origin prompt, and L82-4 is what makes the mismatch live

- **Where:** `src/graph/hybrid.rs:226-231` (`context_text` = `"{content} — {origin}"`),
  `MAX_HYBRID_CONTEXT_BYTES = 16 * 1024` (`:153`).
- **The calibration actually on file** (`PHASE-7-embeddings.md:572-583`): bare labels do
  **not** separate (near `[0.567, 0.868]` vs far `[0.429, 0.855]`, overlapping); with
  concepts *"inside short sentences"* near = `[0.867, 0.931]` and far = `[0.750, 0.825]`,
  and 0.85 sits in that gap. Note the far-class **ceiling is already 0.825** — the whole
  safety margin is 0.042 wide, and it was measured on short sentences.
- **What production does instead:** concatenates a 2-3 word concept label with the entire
  interaction prompt, up to 16 KB. As the origin text grows, it dominates the embedded
  string, and cosine increasingly measures *prompt overlap* rather than concept relatedness.
- **Concrete scenario.** Interaction 1 prompt: *"fix the checkout flow — the payment gateway
  times out and the receipt emails aren't sending."* → derive yields `payment gateway
  timeout` and `receipt email delivery`. Both persist vectors; property 3 correctly keeps
  them from merging **within** the call. Interaction 2 prompt is a near-identical follow-up
  → derive yields `gateway retry budget`, whose embedded context is ~the same text as both
  stored vectors. `best_candidate` takes the max over the pool and can write a `Semantic`
  edge to `receipt email delivery` — an unrelated topic — on the strength of shared
  conversational framing. Pre-L82-4 this could not fire (empty pool).
- **Confidence / why PLAUSIBLE not CONFIRMED:** the geometry argument is sound but the
  magnitude is BGE-M3-specific and unmeasured. I could not measure it here: the only local
  endpoint (`127.0.0.1:8080`) serves LFM2.5-230M, completion-only — `/v1/embeddings`
  returns `501 "This server does not support embeddings"`. `FixtureEmbedder` is hash-seeded
  and models no semantics at all.
- **Fix direction (smaller than provenance columns):** bound the origin context used for
  the *merge* comparison (first N bytes / first sentence) so the label is not drowned, or
  re-calibrate on realistic context lengths. This is the natural home for the disposition's
  already-scheduled live re-verification — it should measure the **far-class distribution
  under production-length context**, not just that `embedding IS NOT NULL`.

### P2-3 (PLAUSIBLE) — L82-4 materially increases flush-path cost, aggravating the still-open L82-1

- Every organic concept now carries 1024 × f32 (~4 KB) through the in-memory mutation log
  and into `UPSERT_CONCEPT_SQL`'s `$15::VECTOR`. More importantly,
  `concepts_embedding_idx` is a **partial** vector index `WHERE embedding IS NOT NULL`
  (T7.4), so before this change **no organic row ever entered the vector index**; now every
  one does, paying C-SPANN insert cost per row on the flush path.
- L82-1 (P1, open) measured 784 mutations failing to drain inside the 10 s `CLOSE_GRACE`
  with all-`NULL` embeddings. This change strictly increases the per-concept cost of that
  same drain.
- The disposition's "live re-verification needed" paragraph anticipates the `Semantic`-edge
  second-order effect but not this one. Flagged as an interaction only — L82-1's files are
  out of my review scope.

### P3-1 (CONFIRMED) — the load-bearing safety argument cites a mechanism that does not exist

- **Where:** `src/graph/hybrid.rs:90` — *"Recall expansion (spec §8) and P6 canonization's
  physical fold both travel `Semantic` edges"*; repeated at `hybrid.rs:1659` (*"nor P6's
  physical fold"*) and in the disposition above.
- **There is no physical fold.** `grep -ci 'merge|fold|collapse'` over `src/canon/stage1.rs`,
  `stage2.rs`, `stage3.rs`, `eval.rs`, `task.rs`, `mod.rs` returns **0** for every file, and
  `EdgeType::Semantic` appears nowhere in `src/canon/`. Canonization is a status-transition
  machine (None → Candidate → Venerable → Canonical + demotion, `eval.rs:558-...`). It never
  folds concepts. Its structural gates explicitly exclude `Semantic`: Stage 3 `blast_radius`
  counts only `{Dependency, Causal, Hierarchical}` (`src/store/memory.rs:551-555`;
  `BLAST_RADIUS_SQL`, `src/store/cockroach.rs:439`), and Stage 2 `interaction_span` likewise.
- **The real coupling is missed.** A `Semantic` edge *does* reach canonization — through
  scoring, not traversal. It increments the concept's incident-edge count, which feeds
  **`density`, the heaviest weighted dimension at 0.35** (`src/daemon/score.rs:29-30`), plus
  `edge_type_bonus` +0.01 (`score.rs:66`). Those composite scores drive Stage 1's P90 peer
  gate (`src/canon/stage1.rs:56-85`) and GC's `MIN_CONCEPT_SCORE`. So newly-reachable
  organic `Semantic` edges shift promotion and GC pressure — via density, not edge-walking.
- **Effect on the ruling: none.** The truth is *more* favourable than the claim (canonization
  is even less exposed than argued). But a user-approved product decision should not be
  recorded on a false premise, and the actual coupling path deserves to be the thing the
  live re-verification watches.

**DISPOSITION — FIXED (branch `task/l82-4-fresh-embeddings`, commit `5d3de66`).** Finding
accepted; the citation was false and is corrected in all three places, with the density coupling
named in each. Independently re-verified before writing: `rg 'Semantic' src/canon/*.rs` returns
zero matches; `ScoringWeights::default` (`src/config.rs:34-43`) has `density: 0.35` against
`recency: 0.25`, `frequency: 0.20`, `session_activity: 0.20`; `PEER_PERCENTILE = 0.90` in
`src/canon/stage1.rs`; `MIN_CONCEPT_SCORE = 0.12` in `src/daemon/gc.rs`.

1. **`src/graph/hybrid.rs` module doc, property 1.** The fold clause is struck from the
   sentence ("Recall expansion (spec §8) travels `Semantic` edges, so without one…"), and a new
   sub-paragraph states positively that there is no physical fold, that canonization is a
   status-transition machine, that `EdgeType::Semantic` appears nowhere in `src/canon/`, and
   that the real coupling is `density` (0.35, heaviest weighted dimension) plus
   `edge_type_bonus` feeding Stage 1's P90 gate and GC's `MIN_CONCEPT_SCORE` — "scoring, not
   traversal". It records that the earlier draft made the fold claim, so a future reader meets
   the correction rather than silently inheriting a cleaned-up doc.
2. **The far-text test comment** (`far_text_creates_fresh_concept_with_vector_but_no_merge`)
   drops "(nor P6's physical fold)" and instead says the refusal keeps C1's `density` — the
   dimension that actually feeds Stage 1 and GC — from being inflated by a link no comparison
   endorsed.
3. **The L82-4 disposition paragraph above** is corrected in place with a dated
   `[Corrected 2026-08-15 per P3-1]` marker rather than a silent rewrite, since it records a
   user-approved product decision.

**The ratified conclusion is not weakened anywhere.** In all three places the correction is
stated as what it is — the exposure is *smaller* than the original claim, and the argument for
refusing the edge rests on the recall-neighbourhood harm, which the false citation was never
load-bearing for. The reviewer's point that the density path "deserves to be the thing the live
re-verification watches" is carried into the P2-2/live-re-verification note, not just the doc.

### P3-2 (CONFIRMED) — the end-to-end test validates plumbing in a regime P7 explicitly found unusable

- `ContextTolerantEmbedder` (`src/memory.rs:2760-2775`) strips `"Concept: "` and everything
  after `" — "`, reducing the text to the bare label so `FixtureEmbedder`'s family seed
  applies. That is the **bare-label** regime PHASE-7 measured and rejected: *"Bare
  concept-name embedding does NOT separate the classes… NO single threshold works on bare
  labels"* (`PHASE-7-embeddings.md:572-577`).
- `organic_derive_persists_a_vector_that_recall_finds` (`src/memory.rs:4685`) is therefore
  strong evidence for **persist → flush → reload → vector-recall wiring** (which is what
  L82-4 is about, and the wrapper's own doc comment is honest about why it exists) but is
  **no** evidence about production similarity behaviour. Worth one sentence beside the test
  so a later reader does not cite it as calibration evidence.

**DISPOSITION — FIXED (branch `task/l82-4-fresh-embeddings`, commit `5d3de66`).** Finding
accepted. A "What this test does NOT establish (L82-4 review, P3-2)" paragraph now sits in the
doc comment of `organic_derive_persists_a_vector_that_recall_finds` (`src/memory.rs`): it is
evidence for the persist → flush → reload → vector-recall **wiring**, not for embedding-space
similarity quality; the `>= 0.85` it asserts is measured in the **bare-label regime PHASE-7's
calibration explicitly rejected**, quoting *"NO single threshold works on bare labels"* with the
`PHASE-7-embeddings.md:572-577` citation; and it ends with the instruction the finding asks for —
do not cite this test as calibration evidence for the 0.85 bar under production-length context.
No assertion changed; the test's behaviour is identical.

## 4. Regression sweep

- **`None` arms intact.** All five no-vector arms still write `embedding: None` and return
  `Ok`: capability-absent (`hybrid.rs:465`, byte-identical to `MatchStrategy::Canonical`,
  pinned by `no_capability_is_byte_identical_to_canonical:1820`), embed timeout (`:490`),
  embed error (`:503`, pinned by `embed_failure_degrades_to_fresh_concept:1943` which also
  asserts the contract stamp stays `None` — P7 MINOR-2 preserved), store capability-miss
  after a successful embed (`:571`), non-Concept merge target (`:700-712`, pinned by
  `hybrid_refused_merge_target_is_keyword_only:2216`). A vector is never invented.
- **Assertion removals.** `git diff 713a2ae..080b4a0 -- src | grep '^-.*assert'` yields
  **exactly 2** lines, and both are the documented L82-4 inversions — the `is_none()` in
  `first_use_empty_candidates_still_commits_contract` (replaced by a *stronger*
  `is_some_and(len == 1024)` **plus a new** zero-`Semantic`-edge assertion) and the
  `con.embedding.is_none()` in the renamed far-text test (replaced by
  `assert_eq!(…, Some(1024))`, every other assertion retained). No silent weakening.
- **F18 goldens.** `f18_tool_schemas_match_the_golden_property_set` and
  `f18_no_tool_schema_accepts_a_client_timestamp` both pass.
- **No wire-visible embedding.** `RecallHit` (`src/types/mod.rs:482-489`) has no embedding
  field; MCP and `lambo inspect` build responses from explicit `json!` literals, never by
  serializing `Concept`. **Latent note (not a finding on this branch):** `Concept` derives
  `Serialize` with `pub embedding: Option<Vec<f32>>` and **no** `skip_serializing_if`
  (`types/mod.rs:219-220`), so any future path that serializes a whole `Concept` would now
  emit 1024 floats per concept where it previously emitted `null`. Nothing does today.
- **Blast radius / capability scope.** Only Cockroach advertises `VECTOR_SEARCH`
  (`cockroach.rs:1508`); `MemoryStore` and SQLite return `Capabilities::empty()`, so in
  production L82-4 changes behaviour on Cockroach sessions only — consistent with the
  disposition. SQLite persists `Concept.embedding` for round-trip parity but never queries it.

## 5. Gates — full PHASE-8 binding block, all green

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --features store-sqlite -- -D warnings` | clean |
| `cargo test` | **623** lib / 5 bin / 3 integration (8 binaries, 2 ignored) / 1 doctest — **0 failed**, 1 ignored |
| `cargo test --features store-sqlite` | **667** lib / 5 bin / 8 integration / 1 doctest — **0 failed**, 1 ignored |
| `cargo test --no-default-features --features store-sqlite --no-run` | builds clean |
| `cargo test --no-default-features --features store-cockroach --no-run` | builds clean |
| `cargo check --no-default-features` | clean |

Counts match the disposition's claim exactly (623 / 667).

## 6. Verdict

**The product decision is RATIFIED.** Threshold-preserving is the sound semantic; the strict
target-exclusion reading is refuted on the merits (reason 2), not merely on cost. No finding
below asks for the semantic to change.

**REQUEST CHANGES** on three items, none of which touch the ratified behaviour:

1. **P2-1** — property 3 is unpinned; the test that claims to pin it passes under a faithful
   violating mutation. Fix the test (or the claim).
2. **P3-1** — the recorded safety argument cites a canonization "physical fold" that does not
   exist, and misses the real density/score coupling. Correct the module doc, the far-text
   test comment, and the disposition paragraph above.
3. **P3-2** — one sentence beside the E2E test noting it proves wiring, not similarity.

**P2-2** (calibration basis vs 16 KB production context) and **P2-3** (flush cost vs the open
L82-1) are for the phase owner to schedule, not for this branch to fix. P2-2 in particular
should be folded into the live re-verification the disposition already plans: measure the
**far-class score distribution under production-length context**, not merely that
`embedding IS NOT NULL`.

— L824AdveReview, 2026-08-14

## 7. Remediation record — all three REQUEST CHANGES items closed

```
╔══════════════════════════════════════════════════════════════════╗
║  Round:    L82-4 review remediation — 2026-08-15                 ║
║  Branch:   task/l82-4-fresh-embeddings                           ║
║  Commits:  5d3de66 (test + doc comments) + this review record    ║
║  P2-1  FIXED — pin is real; mutation (d) now CAUGHT 622/1        ║
║  P3-1  FIXED — fold claim struck in all 3 places; density named  ║
║  P3-2  FIXED — wiring-not-similarity sentence added              ║
║  Ratified semantic: UNCHANGED (no production code was touched)   ║
╚══════════════════════════════════════════════════════════════════╝
```

**Nothing in `src/` outside test code and doc comments changed.** `git diff 080b4a0..5d3de66 --
src` touches exactly two files: `src/graph/hybrid.rs` (module doc, the `RecordingEmbedder`
test double, and the property-3 test) and `src/memory.rs` (one test doc comment). The
below-threshold `Resolution::Fresh` arm — the whole of L82-4 — is byte-identical.

**No assertion was removed or weakened.** `git diff 080b4a0..5d3de66 -- src | grep '^-.*assert'`
yields **one** line: `assert!(c.embedding.is_some(), "both fresh concepts persist vectors")`,
re-added on the `+` side verbatim with a trailing semicolon, because the block now also pushes
the vector into `staged` for the new similarity assertion. Net assertion change is **+1** — the
merge-eligibility precondition (`cosine(staged[0], staged[1]) >=
SEMANTIC_MATCH_THRESHOLD_DEFAULT`) — plus a strengthened failure message on the existing
zero-`Semantic`-edge `assert_eq!`, whose expression is unchanged. Both are documented in the
P2-1 disposition.

**P2-2 and P2-3 are not addressed here**, per the reviewer's own instruction that they are the
phase owner's to schedule. P2-2's substance has been folded into the live-re-verification bullet
of the L82-4 disposition (far-class distribution under production-length context), together with
the P3-1 correction to what that run should watch for on the canonization side.

### Gates — full PHASE-8 binding block, re-run on `5d3de66`, all green

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features store-cockroach,store-memory,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --features store-sqlite -- -D warnings` | clean |
| `cargo test` | **623** lib / 5 bin / 7 integration (8 binaries) / 1 doctest — **0 failed**, 3 ignored |
| `cargo test --features store-sqlite` | **667** lib / 5 bin / 9 integration / 1 doctest — **0 failed**, 3 ignored |
| `cargo test --no-default-features --features store-sqlite --no-run` | builds clean |
| `cargo test --no-default-features --features store-cockroach --no-run` | builds clean |
| `cargo check --no-default-features` | clean |

Lib counts unchanged at **623 / 667** — P2-1 strengthened an existing test rather than adding
one, which is the honest outcome: the suite is the same size and now pins one more thing.

— L82-4 remediation, 2026-08-15
