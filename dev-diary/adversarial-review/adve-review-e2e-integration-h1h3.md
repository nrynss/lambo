# Adversarial Review: E2E whole-platform integration (H1 + H2 + H3 + portal) — `e2e_review_r1`

```text
╔════════════════════════════════════════════════════════════════════════╗
║  STATUS: CONDITIONAL — 1 P2 (live-reproduced) + 7 P3; no P0/P1         ║
║  Scope:  integrated main @ c5586c5 — H1 embedding contracts (95179b8), ║
║          H2 gate suppression (5eed73c), H3 structured recall (c5586c5), ║
║          portal rebuild (5ccd48f), whole platform, process honesty,     ║
║          LIVE CockroachDB verification (user-required)                  ║
║  Source: main @ c5586c5 (2026-08-17)                                    ║
║  Reviewer: e2e_review_r1 (fresh independent reviewer) + five parallel   ║
║          scouts (H1, H2, H3, portal, honesty); every finding           ║
║          re-verified by the orchestrator against source, and E2E-1      ║
║          reproduced LIVE against the real cluster                       ║
║  Evidence: evidence/e2e-live-cockroach/ (README + live-legs +           ║
║          live-exercises, redacted — no credentials)                     ║
║  Verdict: PLATFORM HOLDS with one functional defect. The H1/H2/H3       ║
║          integration is coherent, all gates green, and the previously   ║
║          ignored live Cockroach legs EXECUTED AND PASSED (8/8 lib legs, ║
║          incl. the 65 s conformance suite, plus 2 real-embedder         ║
║          calibration tests). E2E-1: the documented                        ║
║          --allow-embedding-mismatch workflow fails on its first write   ║
║          against a vector-capable store (live-reproduced; the relabel   ║
║          is write-behind, so the first checked candidate read refuses). ║
║          7 further P3s (retry classification, two portal races, renderer ║
║          warning drop, silent gather degrade, flag-vs-reality for       ║
║          legacy sessions, stale docs). No false closures found; the     ║
║          H1 residual "no live result because DSN was unset" is CLOSED.  ║
║  Verified: 2026-08-17 — every gate re-run by this reviewer; live legs   ║
║          executed against cluster nrynss with the real BGE-M3 embedder  ║
╚════════════════════════════════════════════════════════════════════════╝
```

## Grounding

Read: the H1/H2/H3 sections and completion records in
`dev-diary/notes/hardening-tasks.md`; all six hardening review records
(`H1round1..3`, `H2round1`, `H3round1..2`); the format precedent
`adve-review-e2e-p0-p3-fable.md`; the H1/H2/H3 merge diffs
(`git diff 95179b8^1 95179b8`, `5eed73c^1 5eed73c`, `c5586c5^1 c5586c5` — 49
files, +6039/−539) plus the surrounding context of every changed production
file (store trait + cockroach/sqlite/memory adapters, `resolve.rs`,
`memory.rs`, `graph/{graph,hybrid}.rs`, `recall/{assemble,detail,dispatch,
format,candidates}.rs`, `cli/{mod,recall,serve_web,derive,demo}.rs`,
`daemon/mod.rs`, `main.rs`, `mcp/serve.rs`, `web/app.js`, `web/index.html`,
`web/app.css`, `tests/cli_write_lease.rs`, `fixtures/recall-h3-goldens.json`).

Executed by this reviewer: default test suite, both feature-combo suites,
`--all-features` with and without `LAMBO_COCKROACH_DSN` (the live legs), both
`clippy -D warnings` gates, `fmt --check`, `git diff --check`, `node --check`
on `web/app.js` and `scripts/recording/capture-portal.mjs`; live manual
exercises against the cluster (demo with real BGE-M3, CLI + HTTP recall
parity, `/api/inspect` Canonical-vs-non-Canonical, contract-mismatch
fail-closed, writer refusal + override). Credentials: `LAMBO_COCKROACH_DSN`
was loaded only via `set -a; source /home/nryn/work/lambo/.env; set +a`
inside subshells; no `.env` value is printed, logged, or committed. The DSN
name is never shown in this record or the evidence directory (the logs were
scanned for `postgres://`, `password=`, `sslrootcert`, host names — zero
hits).

## Gates (this review's runs, main @ c5586c5)

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `git diff --check` (95179b8^1..HEAD and worktree) | clean |
| `node --check web/app.js`; `node --check scripts/recording/capture-portal.mjs` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo test` (default) | **705 passed, 1 ignored** (lib) + all binary/integration/doc harnesses pass |
| `cargo test --no-default-features --features store-memory,embed-fixture` | **692 passed** (lib) + harnesses pass |
| `cargo test --no-default-features --features store-sqlite,embed-fixture` | **513 passed** (lib) + harnesses pass |
| `cargo test --all-features`, **no DSN** | **835 passed, 8 ignored** — the 8 live legs report as ignored, never skip-as-green (matches the H3 completion record's claim exactly) |
| `cargo test --all-features -- --ignored`, **DSN loaded** (LIVE) | **8 passed, 0 failed** (lib) — see live table; `tests/live_calibration.rs` 2 passed against the real local llama.cpp BGE-M3 server |

### Live legs (executed vs ignored — credentials never shown)

| Test | Status |
|---|---|
| `store::cockroach::conformance::conformance_suite` (24 live checks incl. H1's embedding-contract read + flush immunity, corrupt-contract-row error, unstamped-vector-candidates-empty-until-commit, vector write/candidates/session-scoping, structural parity vs Memory) | **EXECUTED — pass** (65 s) |
| `store::cockroach::conformance::single_writer_lease_is_enforced_across_pools` | **EXECUTED — pass** |
| `store::cockroach::conformance::vector_beam_size_reaches_the_server_and_keeps_statement_timeout` | **EXECUTED — pass** |
| `store::cockroach::conformance::build_store_returns_working_adapter` | **EXECUTED — pass** |
| `cli::saints::live::saints_and_stats_against_live_cockroach` | **EXECUTED — pass** |
| `canon::eval::tests::fixture::cockroach_three_hop_progression_matches_memory` | **EXECUTED — pass** |
| `store::cockroach::conformance::vector_explain_camera_proof` | **EXECUTED — env-gated skip by design** (needs `LAMBO_REQUIRE_VECTOR_INDEX=1`); its EXPLAIN shape is independently proven on every live run by `check_vector_explain_is_global_topk` |
| `embed::bge_m3::tests::live_smoke_against_llama_server` | **EXECUTED — env-gated skip by design** (needs `LAMBO_LLAMA_EMBED_URL`); the real embedder was exercised live instead via the CLI demo + recall below |

### Manual live exercises (redacted transcript: `evidence/e2e-live-cockroach/live-exercises.txt`)

1. **H1 full write path, real embeddings**: `lambo demo --scenario rest-api`
   on a fresh Cockroach session — 27 concepts, `user schema` promoted to
   Canonical by the engine (`canonization_events` rows durable), conflict
   event, blast radius 9, every agent released its lease.
2. **H1 checked candidate read live**: `lambo recall` on that session routes
   through `vector_candidates_checked` (global ANN growth loop, contract read,
   commit in one transaction) — canonical hit ranked at score 1.92.
3. **H1 reader fail-closed live**: renamed model id in config → CLI recall
   refused naming both contracts; `/api/session` + `/api/pulse` report
   `status: "mismatch"`, `vector_search: false`; `/api/recall` returns 502 with
   `{"error": …}` only — no `hits`, no `response_annotations`, no
   `included_in_context`, no `context`; `/api/stats` + `/api/graph` stay 200
   (structural-only mode).
4. **H1 writer refusal + override live** (isolated session): derive with
   contract A → ok; with B (same kind/width, renamed model) → refused naming
   both; with B + `--allow-embedding-mismatch` → **the first write is refused
   by the checked read** (finding E2E-1; relabel lands only at close, so a
   second run succeeds). Recall then works under B and refuses under A in the
   other direction.
5. **H2 live**: `/api/inspect?focus=user schema` (Canonical) → 200 with
   `status: Canonical`, `blast_radius: 9`, 16 dependents, **no `gate_progress`
   key**; `/api/inspect?focus=auth middleware` (status None) → `gate_progress`
   present with real bars (gc 7.0/3.0, blast 0.0/5.0, …).
6. **H3 parity live**: CLI `lambo recall` output vs `/api/recall` `context`
   for the same session/query — **byte-identical modulo the CLI's own trailing
   newline** (313-byte CLI output = 312-byte HTTP context + `\n`); the payload
   carried `hits` with `status: Canonical`, `included_in_context: true` and a
   `load_bearing` annotation.

## Prior-closure verification — no false closures

Every closure claim in the six hardening records and three completion records
was re-verified against HEAD; all hold:

- **H1 round-3 closure (7cd8194/de3b4b7, CLEAN):** the additive
  `vector_candidates_checked` trait method fails closed for a legacy
  VECTOR_SEARCH adapter (`store/mod.rs:169-196`: capability check before any
  delegation, no recursion); all production vector reads go through the
  checked method (recall `gather`, `graph::hybrid::derive`, `Memory::recall`);
  Cockroach's checked method binds contract read, global growth loop,
  exact-session fallback and commit in one `tx_retry` serializable
  transaction with mismatch mapped to non-retried `Invariant`
  (cockroach.rs:2108-2232); the legacy `vector_candidates` delegates into the
  checked transaction (2080-2106). The override is clap-scoped to the six
  writer variants (main.rs:59-60,110-111,220-221,247-248,274-275,292-293),
  rejected on readers (test `h1_embedding_mismatch_override_is_explicit_and_writer_only`),
  and gated to same-width (memory.rs:659) plus, with extant concept vectors,
  same-kind (graph.rs:849-857). Lease release on refusal is holder-scoped on
  every startup error (memory.rs:678-691) and proven by the real-subprocess
  test `h1_mismatch_refusal_releases_lease_for_immediate_cross_process_retries`.
  `tests/cli_write_lease.rs` and `tests/serve_single_writer_lease.rs` both
  passed.
- **H2 round-1 closure (2af8b86, CLEAN + P3 docs disposition cbf7f24/
  e1f554b):** the predicate keys on the concept's *current* status
  (`serve_web.rs:1015`); a Canonical hit runs neither gate-only store query —
  proven at the store surface by the `Counting` wrapper wired into the served
  router (`serve_web.rs:1636+,2725-2729`), counters zero for Canonical; the
  wire key is absent (`skip_serializing_if` + `Value::get(...).is_none()`
  assertions); CLI/MCP surfaces have zero `gate_progress` references; the
  portal's pre-existing guard (`web/app.js:578-582`) renders a key-less
  Canonical payload identically. The H2 completion record's cooldown line ref
  (`serve_web.rs:2623-2667`) was accurate at merge time; at HEAD it has
  drifted to 2944-2984 (H3 inserted ~316 lines) — docs-drift nit, see E2E-7.
- **H3 round-2 closure (f84fb67/07be5af/a5eb802, CLEAN):** single-execution
  seam (`cli::recall::run` → `run_detailed`; `/api/recall` serializes
  `context`/`hits`/`response_annotations` from one execution); status from the
  same graph snapshot (`assemble.rs:262-270`, `dispatch.rs:294-297`);
  `included_in_context` recorded at the budget cut (`i < kept`,
  `assemble.rs:344-347`); six pinned kinds attached at typed producers;
  `traversal`/`vector_degraded` response-global only; per-text warning parity
  test (`serve_web.rs:2299-2374`); mismatch 502 carries no success fields
  (`serve_web.rs:3348-3366`); MCP/`src/types/mod.rs`/`src/memory.rs` untouched
  by the H3 diff (verified `git diff --name-only`); the four cards PNGs are
  real, distinct md5s (`c1277531…`, `a8bf0ef0…`, `f6b13d5c…`, `34c3e6ce…` —
  re-run by this reviewer), `capture-2026-08-17T11-33-02-061Z.txt` matches the
  runbook (portal :7799, session `h3-evidence`, four queries), the webm is a
  valid 22.4 s VP8 video; `fixtures/recall-h3-goldens.json` is deterministic
  (fixed scenario timestamps only) and pinned by
  `h3_blended_payload_matches_golden` + `h3_structural_payload_matches_golden`,
  both passing.
- **Cited test names:** all 15 spot-checked names (H1/H2/H3 records +
  completion records) exist and pass in this review's runs
  (`legacy_vector_adapter_…`, `checked_vector_transaction_…`,
  `inspect_…`×4, `recall_endpoint_…`×3, `cli_mcp_differential_…`,
  `h1_live_contract_changes_…`, `h3_…_golden`, `h1_operator_override_…`,
  `h1_embedding_mismatch_override_…`). Cited commits (7cd8194, de3b4b7,
  2af8b86, 1bfe55c, 93631ef, f84fb67, 07be5af, a5eb802, c72acf5, 298af97,
  9712333, 6112220, 2d0b2ac) all exist with matching content.
- **Process honesty:** hardening-tasks.md H1/H2/H3 are `DONE / CLEAN` with
  claim histories and matching completion records; H4/H5/H6 `DONE` (portal
  rebuild), H7 `PARKED / NEEDS DESIGN` — dispatch order accurate; review index
  header "60 records" == 60 table rows (54 `| [` + 6 `| **[`); the completion
  records' gate counts (705/1, 692, 513, 835/8) match this review's runs
  exactly.
- **The H1 live residual is now CLOSED.** The H1 round-3 record
  (lines 76-77), the H2 record (line 123), and the H3 completion record all
  state that no live Cockroach result is claimed because `LAMBO_COCKROACH_DSN`
  was unset. On this machine the DSN is set: every previously-ignored leg
  executed and passed, the checked vector-candidate transaction ran live
  (contract read → ANN growth loop → exact-session fallback → commit), and
  live recall routed through the checked path. The one sub-claim that remains
  code-verified + unit-tested rather than live-injected is a *forced* SQLSTATE
  40001 on the checked read (no live injection hook exists); the retry seam
  itself ran live under cross-pool lease contention
  (`single_writer_lease_is_enforced_across_pools`), which aborts with 40001
  under conflict.

## Findings

Severity: P1 = high, fix next cycle; P2 = real defect, fix eventually;
P3 = latent/robustness/docs. No P0, no P1.

| ID | Sev | Area | One line |
|---|---|---|---|
| E2E-1 | P2 | H1 override | `--allow-embedding-mismatch` first write is refused on vector-capable stores: the relabel is write-behind, so the first checked candidate read compares the not-yet-durable relabel against the durable contract (LIVE-reproduced on Cockroach) |
| E2E-2 | P3 | H1 store | Cockroach checked tx classifies deterministic corrupt-contract-row parse errors (kind XOR dim) as `Backend`, which `tx_retryable` replays 5× (~500 ms backoff) — should be `Invariant` (STORE-4 principle) |
| E2E-3 | P2 | portal | `runLookup` stage-timer leak + stale stage text when a lookup is superseded while in flight (rebuild lineage 5ccd48f; in reviewed scope) |
| E2E-4 | P3 | portal | `/api/graph` 20 s interval has no in-flight guard; out-of-order responses commit stale tree; hero deps fetch unsequenced |
| E2E-5 | P3 | H3 CLI/HTTP | `render_cli_text` ignores `DetailedRecall.warnings`: warn_only paths and the missing-index warning would render empty output (unreachable today; latent) |
| E2E-6 | P3 | H1×H3 | A contract-race `Invariant` from `gather` degrades to a silent keyword-only recall (tracing-only; no client-visible signal) |
| E2E-7 | P3 | docs | Stale references after the merges: app.js:573-577 "until the server stops sending it (H2)" (it now does), serve_web.rs module doc "reuses `cli::recall::run` outright" (now `run_detailed`), hardening-tasks.md:417 cooldown line ref, dead `extra_warnings` variable |
| E2E-8 | P3 | H1 serve-web | `/api/session` + `/api/pulse` report `vector_search: true` for legacy (Unrecorded) sessions whose vectors are quarantined at load — the flag says on, the leg returns nothing |

### E2E-1 (P2) — the documented `--allow-embedding-mismatch` workflow fails on its first write against a vector-capable store

- **Location:** `src/memory.rs:658-673` (override relabel of the in-memory
  graph + queued `SetEmbedding`), `src/store/cockroach.rs:2127-2150` (checked
  read compares the *durable* contract against the expected one),
  `src/graph/hybrid.rs:515-565` (a non-Capability store error propagates),
  `src/cli/derive.rs:87-107` (`open_writer → derive → close_writer`, no flush
  between attach and the first write).
- **Reproduced LIVE** (session `e2e-live-r2`, real BGE-M3 + Cockroach):
  derive with contract A → ok; derive with B + `--allow-embedding-mismatch`
  →
  `invariant violated: vector candidate lookup refused after embedding
  contract changed: … vectors were written by kind=bge_m3 model="bge-m3-FP16.gguf"
  dim=1024, but the live/attached embedder is … "bge-m3-FP16-renamed.gguf" …
  — re-embed or start a new session`. The relabel's `SetEmbedding` mutation is
  write-behind; the first hybrid write's checked candidate read sees the
  pre-relabel durable contract (A) vs the relabeled expected contract (B) →
  `Invariant` → the derive refuses. The relabel only lands at `close()`, so
  the identical invocation succeeds on the second run. The sqlite/memory
  override tests pass only because neither store advertises `VECTOR_SEARCH`,
  so the hybrid checked read is never reached — no test covers override +
  first write on a vector-capable adapter.
- **Impact:** the H1 feature's documented escape hatch ("a verified
  same-kind, same-width model-identifier rename") cannot complete its first
  write on Cockroach; the operator gets a misleading "re-embed or start a new
  session" error that contradicts the flag's purpose. No data is corrupted
  (the refusal is fail-closed), but the workflow is broken and untested.
- **Required remediation:** when the override relabel is applied at attach,
  make the relabel durable before the writer is usable (flush the
  `SetEmbedding` mutation synchronously, or flush before the first vector
  read), then add a vector-capable adapter regression test for
  override-attach + first derive. Alternative if the two-run behavior is
  considered deliberate: document it and fix the error text; this reviewer
  found no record suggesting it is deliberate.

### E2E-2 (P3) — corrupt-contract-row parse error is replayed 5× in the checked tx

- **Location:** `src/store/cockroach.rs:593-598`
  (`session_embedding_from_parts` returns `StoreError::Backend` for a row with
  exactly one of kind/dim set) consumed at `cockroach.rs:2137-2142` inside
  `tx_retry`; `tx_retryable(Backend) == true` (cockroach.rs:715-719).
- **Evidence:** the corruption is deterministic — a parse result that cannot
  change on replay — yet the checked tx replays it 5 times with backoff
  (~500 ms) before surfacing. This is exactly the STORE-4 rule ("constraint
  violations are deterministic and are never replayed") applied to the wrong
  error class. Cosmetic-vs-correctness: still errors, never wrong data.
- **Required remediation:** return `StoreError::Invariant` for the two
  kind-XOR-dim arms (mirroring sqlite's classification), so the retry loop
  returns on the first attempt.

### E2E-3 (P2) — portal `runLookup` stage-timer leak + stale stage text

- **Location:** `web/app.js:736-758` (rebuild lineage `5ccd48f`, untouched by
  H1/H2/H3; in the reviewed portal scope).
- **Evidence:** `setInterval(…, 1300)` is created per lookup (736-739); every
  response path returns early on `seq !== state.lookupSeq` (743, 749) and
  `clearInterval(timer)` sits only in the final `.then` *after* the same guard
  (753-758). A lookup superseded while in flight (Enter while the button is
  disabled — the Enter handler at 699-706 is not gated on the in-flight flag)
  leaks its interval permanently: it keeps firing every 1.3 s, incrementing
  its own closure counter and overwriting `#lookup-stage` with the *first*
  query's stale "Ranking results…" text during the second lookup's loading.
  Each superseded lookup leaks one interval; `get()` has no timeout, so a hung
  recall makes the window arbitrarily long.
- **Required remediation:** clear the interval on every seq-mismatch path
  (or self-clear inside the interval when `seq !== state.lookupSeq`, or keep a
  module-level timer id cleared at the top of each `runLookup`).

### E2E-4 (P3) — `/api/graph` polling race and unsequenced hero deps fetch

- **Location:** `web/app.js:990-991` (`setInterval(loadGraph, 20000)` with no
  coalescing), 963-975 (`.then`/`.catch` unconditionally commit
  `state.graph`), 398-400 (renderHero fires `/api/inspect` per render with no
  sequence token).
- **Evidence:** a graph response slower than 20 s overlaps the next request;
  the last-completing (possibly older) response wins, so the tree/hero/ladder
  can re-render from stale data until the next poll, and a late failure of an
  older request hides the whole structure panel. Low probability locally
  (needs a slow path to the server), real behind a proxy (the exhibit shape).
- **Required remediation:** chain the next `loadGraph` from the previous
  completion (like `schedule()` at 954-956) or use an in-flight flag; add a
  sequence token to the hero deps fetch.

### E2E-5 (P3) — `render_cli_text` drops `DetailedRecall.warnings`

- **Location:** `src/cli/recall.rs:189-235` (renderer reads only
  `detailed`/`response_annotations`), `src/daemon/mod.rs:379-393` (warn_only
  paths), `daemon/mod.rs:507-512` (missing-index warning pushed to `warnings`
  only).
- **Evidence:** the warn_only results (top-k validation failure, session
  mismatch) carry their message in `warnings` with empty `detailed`/annotations
  — `render_cli_text` returns `""`, so the CLI/HTTP output would be empty and
  the warning lost. The missing-index warning is likewise never annotated.
  Both paths are unreachable in the current call graph (run_detailed
  pre-validates; `with_index` is always called), so this is latent — but a
  future warning producer that forgets the annotation side would silently
  vanish from both CLI and HTTP output.
- **Required remediation:** render `warnings` not covered by any annotation
  (or assert unreachability of the warn_only paths with a test).

### E2E-6 (P3) — contract-race `Invariant` from `gather` degrades silently

- **Location:** `src/daemon/mod.rs:418-424` (every gather error → warn +
  empty vector leg), `src/recall/candidates.rs:102-114` (checked read), H1's
  `Invariant` on a concurrent contract change.
- **Evidence:** if a writer changes the durable contract between the reader's
  load and its vector read, `gather` returns `Invariant`; the daemon logs a
  `tracing::warn` and continues with keyword-only hits, and the HTTP/CLI
  output carries no indication that the vector leg was refused mid-flight.
  This is fail-closed for the vector leg (no wrong rankings) and the window is
  a few milliseconds, but the degradation is invisible to the client.
- **Required remediation:** surface a client-visible response annotation
  (e.g. reuse `vector_degraded`) when the checked read refuses mid-flight.

### E2E-7 (P3) — stale references and dead code after the merges

- **Evidence:** `web/app.js:573-577` still says the gate block is "Suppressed
  here until the server stops sending it (H2 in the hardening notes)" — the
  server now does stop sending it, so the premise is inverted (the client
  guard remains harmless defense-in-depth); `serve_web.rs:37-39` module doc
  says recall "reuses `cli::recall::run` outright" — it now reuses
  `run_detailed` (functional claim still true, name stale);
  `hardening-tasks.md:417` (H2 completion record) cites the cooldown
  regression at `serve_web.rs:2623-2667`, which at HEAD is 2944-2984
  (accurate at merge time); `cli/recall.rs:121` declares `extra_warnings`,
  pushed at 134 and never read (the annotation path superseded it).
- **Required remediation:** refresh the three doc/comment sites; delete the
  dead variable.

### E2E-8 (P3) — `vector_search` flag true for legacy sessions whose vectors are quarantined

- **Location:** `serve_web.rs:406-408` (`vector_search_trusted()` =
  status != mismatch), `store/load.rs:90,101-116` (legacy vectors quarantined
  at load when the contract is unrecorded), checked read returns empty for an
  unrecorded contract (`cockroach.rs:2143-2145`).
- **Evidence:** a pre-contract (legacy) session reports
  `vector_search: true` in `/api/session` and `/api/pulse`, but its vectors
  were quarantined and the checked read returns nothing — the flag says the
  leg is on, the leg returns zero candidates. Impact: cosmetic
  misinformation on legacy data only (every current writer stamps the
  contract); fail-closed on the actual ranking.
- **Required remediation:** report `vector_search: false` (or a distinct
  "unrecorded" state) for legacy sessions.

## Positive observations

- **H1 fail-closed is real and live-proven.** Reader recall, writer attach,
  serve-web session/pulse and the store-level checked read all refuse a
  mismatch, naming both contracts; the 502 body carries no success fields;
  structural routes stay up; the override is genuinely restricted to
  same-kind same-width and is rejected on readers by clap. The
  `vector_candidates_checked` default fails closed for legacy vector adapters,
  and the Cockroach implementation is one retried serializable transaction
  with the mismatch mapped to a non-replayed `Invariant`.
- **H2 is exactly scoped.** One 15-line production change + tests; CLI and MCP
  untouched; the query-count regression proves the two gate-only store reads
  are skipped at the store surface, not just omitted from JSON.
- **H3's single-execution seam is clean.** CLI string and HTTP context are the
  same execution's output (live byte-parity proven); typed annotation kinds
  are pinned at producers; `included_in_context` at the cut; goldens
  deterministic; portal renders cards/excluded-warnings/annotations entirely
  through `textContent` (zero HTML sinks repo-wide in `web/`).
- **Lease discipline** (holder-scoped release on refusal, cross-pool
  enforcement, refresh token fencing) held live.
- **Process honesty is exemplary.** Every completion-record gate count matched
  this reviewer's independent runs exactly (705/1, 692, 513, 835/8); skips are
  loud (`#[ignore]` + `dsn_or_skip` panic under `LAMBO_REQUIRE_LIVE`); the
  live residual was disclosed in every relevant record and is now closed.
- **Defense-in-depth note (not a finding):** `web/` has zero HTML sinks, but
  there is also no CSP meta tag and no security headers from serve-web; a
  future `innerHTML` would have no backstop. Consider a `Content-Security-Policy`
  header when the portal next changes.

## Verdict

**CONDITIONAL — ship after remediating E2E-1 (P2).** The H1/H2/H3 integration
is coherent end to end, every gate is green, the live Cockroach legs all
executed and passed (8/8 lib legs + real-embedder calibration), and no false
closure or process-honesty defect was found. The single functional defect
(E2E-1) is a live-reproduced failure of the H1 override workflow on the
vector-capable store — narrow, fail-closed, and untested; the seven P3s are
robustness/docs. E2E-1 should be fixed (flush the relabel before the first
write) with a vector-adapter regression test before the next release claims
the override path; the P3s can ride the next hardening cycle.

## Remediation disposition

Pending — no remediation has been performed yet; this review's findings await
disposition.

— e2e_review_r1, 2026-08-17 (main @ c5586c5; live cluster `nrynss`)
