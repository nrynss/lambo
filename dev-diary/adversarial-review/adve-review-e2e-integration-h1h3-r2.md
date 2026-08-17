# Adversarial Review: E2E round-2 re-review of the remediation branch — `e2e_review_r2`

```text
╔════════════════════════════════════════════════════════════════════════╗
║  STATUS: CLEAN / APPROVE — all 8 round-1 findings closed with evidence, ║
║          zero new findings (no E2E-R2 findings)                          ║
║  Scope:  branch codex/e2e-remediation @ 611f0e1 (base 759d59c = main    ║
║          HEAD) — remediation commits fd92341 (code) + 611f0e1 (docs)    ║
║  Reviewer: e2e_review_r2 (fresh independent reviewer) — every closure   ║
║          re-verified against source AND by executing the gates, the     ║
║          new tests, a fail-without-the-fix scratch check, and a LIVE    ║
║          CockroachDB first-run override reproduction (DSN loaded from   ║
║          .env, credentials never shown)                                  ║
║  Verdict: APPROVE. The round-1 CONDITIONAL (E2E-1 P2 live-reproduced +  ║
║          7 P3) is fully remediated. E2E-1 is FIXED in code, proven to    ║
║          FAIL without the fix (exact refusal reproduced), proven PASS    ║
║          with it, and the documented --allow-embedding-mismatch          ║
║          workflow now succeeds on its FIRST write against the real       ║
║          cluster (reproduced live by this reviewer). All other findings  ║
║          closed as claimed. Regression sweep over H1/H2/H3/portal clean; ║
║          MCP + src/types untouched; every gate green incl. 8/8 live      ║
║          legs + real-embedder calibration.                               ║
║  Verified: 2026-08-17 — every gate re-run by this reviewer on the        ║
║          branch HEAD; live legs executed against cluster nrynss with     ║
║          the real BGE-M3 embedder (llama.cpp server on 127.0.0.1:8080)   ║
╚════════════════════════════════════════════════════════════════════════╝
```

## Grounding

Read: the round-1 record and its remediation disposition
(`dev-diary/adversarial-review/adve-review-e2e-integration-h1h3.md`, incl. the
`## Remediation disposition` section); the full remediation diff
(`git diff 759d59c..HEAD`: 8 files, +756/−34) and the surrounding context of
every changed production file (`src/memory.rs` attach path + `final_flush` +
`VectorSearchStore` test adapter + the new regression test, `src/store/
cockroach.rs` `session_embedding_from_parts` + both consumers + `tx_retryable`
+ the updated XOR test, `src/cli/recall.rs` `run_detailed` +
`render_cli_text` + three new unit tests, `src/daemon/mod.rs` `recall_detailed`
gather error path + the E2E-6 stub test, `src/cli/serve_web.rs`
`EmbeddingStatus::vector_search_trusted` + the E2E-8 wire-shape test,
`web/app.js` `runLookup`/`loadGraph`/`renderHero` + boot, `dev-diary/notes/
hardening-tasks.md` cooldown ref). Also re-read the H1/H2/H3 records and
hardening-tasks.md as regression-sweep grounding, and re-verified the portal
renders `response_annotations` in both card and verbatim views
(`web/app.js` `paintResult`/`renderResponseAnnotations`, `render_cli_text`
header loop) and the MCP recall surface (context block, untouched).

Executed by this reviewer on branch HEAD: default suite, both feature-combo
suites, `--all-features` with and without `LAMBO_COCKROACH_DSN` (the live
legs), both `clippy -D warnings` gates, `fmt --check`, `git diff --check`,
`node --check web/app.js`, all six new remediation tests by name, the H3
golden/parity/differential tests, the H1/H2 test families, the lease
integration tests, and a **scratch-worktree fail-without-the-fix check** for
E2E-1. Live manual reproduction of the E2E-1 override workflow against the
real cluster. Credentials: `LAMBO_COCKROACH_DSN` was loaded only via
`set -a; source /home/nryn/work/lambo/.env; set +a` inside subshells; no
`.env` value is printed, logged, or committed; the DSN value never appears in
this record or in any command output I relied on.

## Gates (this review's runs, branch HEAD 611f0e1)

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `git diff --check` (759d59c..HEAD) | clean |
| `node --check web/app.js` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo test` (default) | **711 passed, 1 ignored** (lib) + all harnesses pass |
| `cargo test --no-default-features --features store-memory,embed-fixture` | **698 passed** (lib) + harnesses pass |
| `cargo test --no-default-features --features store-sqlite,embed-fixture` | **517 passed** (lib) + harnesses pass |
| `cargo test --all-features`, **no DSN** | **841 passed, 8 ignored** — the 8 live legs report as ignored |
| `cargo test --all-features -- --ignored`, **DSN loaded** (LIVE) | **8 passed, 0 failed** (52.31 s incl. the conformance suite) + `tests/live_calibration.rs` 2 passed against the real local llama.cpp BGE-M3 server |

Every count matches the disposition's claims exactly (711/1, 698, 517,
841/8, 8 live + 2 calibration) — no gate-count drift between the remediator's
run and this independent run.

## Prior-finding closure — every round-1 finding re-verified

### E2E-1 (P2) — CLOSED, three independent forms of evidence

1. **Code**: `Memory::build`'s `Mismatch`+override arm now drains the relabel
   (`graph.drain_log()`) and calls `final_flush(store, &relabel,
   Some(lease_token))` synchronously before the writer is usable
   (`src/memory.rs:688-698`); `final_flush` is the same armored
   (timeout-bounded, panic-contained) one-shot flush `close()` uses, the
   mutation stays ordered in the log, and a failed relabel flush refuses the
   attach — the startup-error path releases the freshly acquired
   holder-scoped lease. `replace_embedding_with_operator_override` queues a
   `Mutation::SetEmbedding` (graph.rs:859-865), so `drain_log` captures it.
2. **Fail-without-the-fix (empirical)**: in a scratch worktree at HEAD with
   the eager flush disabled (`let _relabel = graph.drain_log();`),
   `h1_override_relabel_is_durable_before_the_first_hybrid_write` FAILED with
   the exact round-1 refusal: `invariant violated: vector candidate lookup
   refused after embedding contract changed: vectors were written by
   kind=fixture model=Some("fixture-model-v1") dim=1024, but the
   live/attached embedder is … "fixture-model-renamed" …`. With the fix the
   same test PASSES. The `VectorSearchStore` test adapter now advertises
   `VECTOR_SEARCH` and enforces the durable-contract check like Cockroach
   (unstamped → empty pool; durable ≠ expected → `Invariant`), closing the
   round-1 gap that no test covered override + first write on a
   vector-capable adapter.
3. **LIVE reproduction (this reviewer, real cluster + real BGE-M3)**: fresh
   session `e2e-r2-1786970900`; derive with contract A → `derived 1
   concept(s): 1 created` (exit 0); derive with the renamed model id (B) →
   refused naming both contracts (fail-closed intact); derive with B +
   `--allow-embedding-mismatch` → **succeeded on the FIRST run** (exit 0),
   where the round-1 reviewer reproduced `invariant violated: vector
   candidate lookup refused after embedding contract changed: …` on the
   identical invocation. Post-override: recall under B works with real
   semantic scores (`verified model alias` 1.04 / `live contract stamp A`
   0.25); recall under A refuses naming both contracts in the other
   direction (unchanged fail-closed behavior).

### E2E-2 (P3) — CLOSED

Both kind-XOR-dim arms of `session_embedding_from_parts`
(`src/store/cockroach.rs:602-607`) now classify as `StoreError::Invariant`
(kind without dim; dim without kind); negative-dim and well-formed rows are
unchanged. `tx_retryable` replays only `Backend` (cockroach.rs:724-730), so
`tx_retry` returns on the first attempt. Both consumers — the load path
(`load_session`, cockroach.rs:1967 inside `tx_retry`) and the checked vector
read (cockroach.rs:2146) — share the helper, so the load path is fixed by the
same change. The updated test
`session_embedding_xor_corruption_errors_not_silent_none` asserts `Invariant`
for both arms (passed); the `tx_retryable_is_structured_not_substring` and
`tx_retry`-invariant tests still pass.

### E2E-3 (P2) — CLOSED

`web/app.js runLookup`: the stage interval is now a module-level
`lookupTimer` cleared at the TOP of every `runLookup` (line 752 — the Enter
handler is not gated on the in-flight flag, so a superseded lookup's timer
dies immediately), self-clears inside the interval when its `seq` is stale
(763-766, so it can never overwrite `#lookup-stage` with the first query's
text), and is cleared on the success path (786). Every interval the audit
found is either chained (`poll`/`schedule`, `scheduleGraph`) or a one-shot
setTimeout; no interval can leak and no superseded lookup writes stage text.
No timer path remains that increments a stale closure counter.

### E2E-4 (P3) — CLOSED

`loadGraph` is scheduled from the previous request's completion
(`scheduleGraph`, app.js:1014-1016: `setTimeout(function () {
loadGraph().then(scheduleGraph); }, 20000)`), replacing `setInterval(loadGraph,
20000)`; `loadGraph`'s internal `.catch` resolves, so a failed poll still
schedules the next one. Requests never overlap and a slow older response
cannot commit stale state over a newer one. The hero deps `/api/inspect`
fetch carries a `state.heroSeq` sequence token (app.js:404-412) — a stale
pillar's response is a no-op. `scheduleGraph` is called exactly once, after
the first graph load at boot (app.js:1030-1036).

### E2E-5 (P3) — CLOSED

`render_cli_text` now renders `DetailedRecall.warnings` that no annotation
already carries — the skip set is exactly the annotation-rendered set
(response annotations + every hit's annotations, by text), with the same ⚑
header treatment, appended after the response-global annotations
(`src/cli/recall.rs:228-248`). A warnings-only producer (the daemon's
`warn_only` refusal paths, the missing-index note) can no longer render empty
output on CLI or HTTP — `api_recall`'s `context` is the same
`render_cli_text` output (serve_web.rs:956). Reachable-path output is
byte-unchanged: every reachable warning is annotation-covered and the two
`warn_only` producers and the missing-index path are unreachable in the
current call graph (`run_detailed` pre-validates top-k/max-tokens/traversal
and passes the graph's own session; `load_session_async` always returns an
`InvertedIndex`, never `None`) — confirmed by the H3 goldens
(`h3_blended_payload_matches_golden`, `h3_structural_payload_matches_golden`),
the warning-parity test
(`recall_endpoint_payload_carries_typed_hits_and_warning_parity`,
`recall_endpoint_tiny_budget_excludes_block_but_keeps_its_warning`) and the
differential test (`cli_mcp_differential_derive_record_recall`), all green.
The three new unit tests
(`a_warnings_only_detailed_result_renders_non_empty`,
`a_warning_covered_by_an_annotation_is_not_duplicated`,
`an_unannotated_warning_renders_beside_context_blocks`) pass.

### E2E-6 (P3) — CLOSED

When `candidates::gather` is refused mid-flight by the checked read's
contract-race `Invariant` (message contains `embedding contract changed`),
`recall_detailed` attaches exactly one `vector_degraded` response annotation
with the distinct text `recall: vector leg refused because the embedding
contract changed mid-query; results are keyword-only`
(`src/daemon/mod.rs:438-445`), appended in producer order after the
pipeline's own response annotations (542). Ranking stays fail-closed: the
vector leg is still empty (`Phase1Input::default()`), only the explanation is
new. No duplication with the CLI-side embed-failure `vector_degraded` by
construction: that path has no query embedding, so `gather` returns early
before reaching the store (candidates.rs:105-111), and the only other
response annotation on the blended path (`traversal`) comes from a dispatched
structural query that skips `gather` entirely. The annotation reaches both
views: `render_cli_text` pushes `VectorDegraded` into the ⚑ header (verbatim
view / HTTP context) and the portal renders every response annotation as a
typed box in the card view (`renderResponseAnnotations`). Test
`gather_contract_race_annotates_vector_degraded` passes (asserts exactly one
annotation, correct text, no others).

### E2E-7 (P3) — CLOSED

- `web/app.js` gate-block comment rewritten — the server has suppressed
  `gate_progress` for Canonical since H2; the client guard is now described
  as defense-in-depth for older payloads (app.js:589-596).
- `serve_web.rs` module doc now names `cli::recall::run_detailed` outright
  with the H3 single-execution seam (serve_web.rs:37-39).
- `hardening-tasks.md` H2 completion record's cooldown ref updated from the
  stale `2623-2667` to `2944-2984` with the drift explanation — verified
  accurate at HEAD: `inspect_surfaces_a_cooling_concepts_repromotion_cooldown`
  sits at serve_web.rs:2948-2984.
- The dead `extra_warnings` variable is deleted (`grep extra_warnings`:
  zero hits); the typed `vector_degraded` annotation superseded it.

### E2E-8 (P3) — CLOSED

`EmbeddingStatus::vector_search_trusted()` is now `status == "compatible"`
(serve_web.rs:414-416): `unrecorded` (legacy sessions whose vectors were
quarantined at load, where the checked read returns an empty pool) and
`mismatch` both report `vector_search: false` in `/api/session`
(serve_web.rs:857-864) and `/api/pulse` (911-912). The `status` field keeps
its `unrecorded|compatible|mismatch` semantics; the H1 banner
(`applyEmbeddingStatus`, app.js:203-211) keys on `status === "mismatch"`
only, and the pre-existing banner test
(`h1_live_contract_changes_update_session_pulse_and_keep_recall_fail_closed`)
still asserts that behavior. The new test
`h1_legacy_unrecorded_sessions_report_vector_search_false` proves the wire
shape on a `VECTOR_SEARCH`-advertising store (the `VectorSearch` wrapper,
serve_web.rs:1532-1540): unrecorded → `vector_search: false` on both session
and pulse with `status: "unrecorded"`, then stamped compatible → `true`.

## Regression sweep (round-1-verified platform untouched)

- **H1 fail-closed**: writer refusal + holder-scoped lease release on the
  startup-error path (verified live above and by
  `h1_mismatch_refusal_releases_lease_for_immediate_cross_process_retries`,
  `derive_succeeds_with_no_serve_and_fails_closed_while_serve_holds`,
  `a_second_process_on_one_session_is_refused_by_the_lease` — all pass);
  checked reads (the live conformance suite re-executed: 8/8 legs incl.
  `conformance_suite`, `single_writer_lease_is_enforced_across_pools`);
  serve-web structural mode (`/api/stats` + `/api/graph` stay 200 on
  mismatch — asserted inside `h1_live_contract_changes_…`, passed); banner
  intact (above).
- **H2 gate suppression**: `inspect_canonical_hit_runs_neither_gate_only_store_query`
  (store-surface proof) and `inspect_keeps_the_gate_block_for_every_non_canonical_status`
  pass; H2's production diff untouched by remediation (not in `git diff
  --name-only 759d59c..HEAD`).
- **H3 structured recall**: single-execution seam (`run_detailed` → context /
  hits / response_annotations from one run) intact; goldens deterministic and
  green; kinds + `included_in_context` pinned at producers (assemble.rs
  unchanged); mismatch 502 carries no success fields (asserted in the
  contract-change test); per-text warning parity green.
- **Portal**: `web/app.js` still 100% textContent (33 uses, zero
  `innerHTML`/`outerHTML`/`insertAdjacentHTML`/`document.write`); the only
  portal diff is the four E2E-3/E2E-4/E2E-7 changes above; `node --check`
  clean.
- **MCP / types / public surfaces**: `git diff --name-only 759d59c..HEAD` =
  only `src/cli/{recall,serve_web}.rs`, `src/daemon/mod.rs`, `src/memory.rs`,
  `src/store/cockroach.rs`, `web/app.js` + two docs files. `mcp/` and
  `src/types/` untouched; all changed functions are private or `pub(crate)`
  — no public API change. The E2E-6 annotation's consuming side is verified
  (renderer + portal + MCP context block all render response annotations; no
  silent drop).
- **Lease discipline**: cross-pool enforcement and refresh-token fencing
  re-verified live in this run's conformance legs.

## Findings

No findings this round. Severity table:

| ID | Sev | Status |
|---|---|---|
| E2E-1 | P2 | CLOSED — code fix + fail-without-fix test + LIVE first-run success reproduced |
| E2E-2 | P3 | CLOSED — `Invariant` both XOR arms, both consumers, tests updated |
| E2E-3 | P2 | CLOSED — module-level timer, top-of-runLookup clear, self-clean on stale seq |
| E2E-4 | P3 | CLOSED — completion-chained graph poll + hero deps sequence token |
| E2E-5 | P3 | CLOSED — unannotated warnings render; reachable path byte-unchanged |
| E2E-6 | P3 | CLOSED — one `vector_degraded` annotation, no duplication, both views |
| E2E-7 | P3 | CLOSED — three doc/comment sites refreshed, dead var deleted |
| E2E-8 | P3 | CLOSED — `vector_search` false for unrecorded/mismatch; banner intact |

## Positive observations

- The E2E-1 fix is proportionate and honest: it reuses the existing armored
  one-shot flush rather than adding a new durability mechanism, refuses
  attach (with lease release) when the relabel cannot be made durable, and
  its regression test genuinely reproduces the round-1 refusal when the fix
  is disabled (verified empirically in a scratch worktree).
- The six new tests + the updated XOR test are all behavior-pinned (exact
  refusal text, exact annotation text, wire shapes), not plumbing.
- Gate-count integrity held across the remediation: every number in the
  disposition matched this reviewer's independent runs exactly.
- No scope creep: the remediation touched exactly the six production files
  the eight findings named, plus the two docs files; MCP, `src/types/`, and
  the public API are untouched; the portal stayed textContent-only.

## Verdict

**CLEAN / APPROVE.** All eight round-1 findings (E2E-1 P2 + seven P3s) are
closed with source-level, test-level, and — for E2E-1 — live evidence;
regression sweep over the H1/H2/H3/portal platform is clean; MCP and public
surfaces untouched; every gate green including 8/8 live Cockroach legs and
the real-embedder calibration, with the documented
`--allow-embedding-mismatch` workflow now succeeding on its first write
against the real cluster. No new findings (no E2E-R2 findings). The branch
is ready to merge to main.

— e2e_review_r2, 2026-08-17 (branch `codex/e2e-remediation` @ 611f0e1, base
main `759d59c`; live cluster `nrynss`; remediation commits `fd92341`
code + `611f0e1` docs)
