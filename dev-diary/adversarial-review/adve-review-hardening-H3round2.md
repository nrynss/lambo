# Adversarial Review - Hardening H3, round 2

- **Reviewer:** `h3_review_r2` (fresh independent reviewer, source read-only
  for implementation)
- **Scope:** the remediation diff `git diff 93631ef..HEAD` (commits
  `f84fb67` capture/parity fixes, `07be5af` re-captured evidence,
  `a5eb802` disposition) on top of the round-1-verified implementation
  (`1bfe55c` + `93631ef`), against base `5eed73c` (post-H2-merge), plus the
  H3 section of `dev-diary/notes/hardening-tasks.md` and the round-1 review
  record `dev-diary/adversarial-review/adve-review-hardening-H3round1.md`
- **Worktree:** `/home/nryn/work/lambo/worktrees/hardening-h3`
  (branch `codex/hardening-h3`)
- **Verdict:** **CLEAN / APPROVE** - zero findings

Both round-1 findings are genuinely closed and verified against the artifacts
and the code: the four cards-view screenshots now show the cards view with
the claimed styling (score bars, status badge, pillar styling, traversal
banner, collapsed excluded cards, persistent excluded-warnings area,
XSS-as-text), all four `cards-*.png` md5s are distinct, and the warning-parity
test now counts per-text occurrences so it cannot false-fail on duplicate
warning texts while still failing when an annotation is missing from
`context`. The regression sweep over the round-1-verified items found nothing
regressed: the remediation touched only `scripts/recording/capture-portal.mjs`
and the `mod tests` section of `src/cli/serve_web.rs` plus evidence/docs, so
the production implementation is byte-identical to what round 1 verified, and
every round-1-verified property re-verified at HEAD below.

## Round-1 finding closure

### H3-R1-1 (P2) - The committed cards-view evidence does not show the cards view; blended and tiny-budget screenshots are byte-identical — CLOSED

The capture script now proves each query's SPECIFIC content rendered before
screenshotting, and puts the results region on camera:

- **Real-render waits.** `scripts/recording/capture-portal.mjs:111-149`
  (`RENDER_CONDITIONS`) waits for query-specific, freshly rendered content:
  the Canonical pillar card with its `.score-track` and a "depend on it"
  note (blended), the `#response-annotations` banner containing "graph
  traversal" above at least one `.card` (structural), the `#excluded-warnings`
  area populated with "Load-bearing pillar" plus "outside the context budget"
  and at least one collapsed `.card.is-excluded` (tiny-budget), and the XSS
  marker inside a real card (xss). Each condition also requires
  `#lookup-btn` to be re-enabled (request finished) and `#lookup-results`
  laid out (`getClientRects().length > 0`), so a stale render from the
  previous query riding leftover DOM text cannot satisfy it — the mechanism
  that produced the round-1 byte-identical captures.
- **Scroll before capture.** Each cards screenshot pins `#lookup-results`
  to the viewport top with `scrollIntoView({block: 'start'})`
  (capture-portal.mjs:178-180); the tiny-budget capture additionally brings
  `#excluded-warnings` fully into the viewport when the collapsed cards push
  it below the fold (lines 181-196). The 1600x900 viewport with
  `deviceScaleFactor: 2` produces the committed 3200x1800 PNGs.
- **Artifacts re-verified by OCR and pixel analysis (this reviewer, on the
  committed bytes):**

| Artifact (md5) | OCR/pixel verification |
|---|---|
| `cards-blended-blended-default.png` (`c1277531…`) | "update user schema"; pillar card `user schema Entity`, `Score 2.09 · 9 depend on it`, `[load_bearing] Load-bearing pillar — 9 nodes depend on this. Modify with caution.`; below it `add oauth_id to user schema Resource \| Candidate` `Score 1.36`, `user serializer`, `user fixtures`, `user validation rules` with scores. Pixel scan: teal accent (score-track fills, `oklch(0.5 0.13 210)` dark theme ≈ sRGB(0,117,140)) 5051 sampled px and amber warn (Canonical status chip + pillar border, ≈ sRGB(164,95,0)) 5399 sampled px — score bars and the status badge are genuinely on camera. |
| `cards-structural-structural-default.png` (`a8bf0ef2…`) | `[traversal] recall: dependency question answered by graph traversal (1 dependents)` rendered prominently above the `RDS-Lambo-Demo-DB Entity — Score 0.50` card; accent score-bar pixels present (3593 sampled). |
| `cards-tiny-budget-tiny-budget-24.png` (`f6b13d5c…`) | Four collapsed `Outside the context budget` bars; the persistent `Warnings from results outside the context budget` title, the "complete blocks did not fit the token budget" note, and the owning hit `user schema` with `[load_bearing] Load-bearing pillar — 9 nodes depend on this. Modify with caution.` Pixel scan: amber warn pixels (2429 sampled, the load-bearing annotation border) plus accent pixels. Distinct from blended: different md5 and different content. |
| `cards-xss-xss-default.png` (`34c3e6ce…`) | `malicious markup <img src=x onerror=window.__h3xss=1> Observation` rendered as text inside a card (no `<img>` element; the capture script asserts `#lookup-cards img` count 0, verbatim marker text, and `window.__h3xss` never fired — script exits non-zero otherwise). Pillar card with badge/score bar also visible (amber 5399, accent 4045 sampled px). |
| `verbatim-context.png` (`f7cd9a7b…`) | Verbatim `lambo recall` block with the `[Entity, canonical]` marker and the ⚑ line (fallback toggle view). |
| `audit-feed.png` (`983d4aac…`) | Canonization feed; amber audit-blast figures present (419 sampled px). |

- **md5s distinct:** `cards-blended` `c1277531…`, `cards-structural`
  `a8bf0ef2…`, `cards-tiny-budget` `f6b13d5c…`, `cards-xss` `34c3e6ce…` —
  no two cards artifacts share an md5 (round 1: blended == tiny-budget, both
  `b5708803…`).
- **Webm replaced:** the round-1 `dd5b4e4b…webm` is deleted; the committed
  `eb16e79d…webm` is a valid VP8 1600x900 video, 22.4 s, matching the
  four-query + verbatim capture flow.
- **Runbook matches artifacts:** every `evidence/h3-recall-cards/README.md`
  statement for the four cards PNGs, `verbatim-context.png`,
  `audit-feed.png`, the `*.webm` and `capture-<utc>.txt` rows was checked
  against the OCR/pixel results above and holds. `capture-2026-08-17T11-33-02-061Z.txt`
  records `portal: http://127.0.0.1:7799`, `session: h3-evidence`, the four
  query labels — a local serve-web capture as the runbook claims.
- **Tiny-budget distinctness:** the tiny-budget artifact shows the collapsed
  excluded cards AND the persistent excluded-warnings area with the owning
  hit, which the blended artifact does not contain; the two captures can no
  longer be confused.

### H3-R1-2 (P3) - The warning-parity test's "exactly once" count breaks on duplicate warning texts across hits — CLOSED

`src/cli/serve_web.rs:2347-2370` (`recall_endpoint_payload_carries_typed_hits_and_warning_parity`)
now asserts per-text occurrence parity instead of a hard "exactly once":

```rust
let expected = ann_texts.iter().filter(|t| *t == text).count();
let prefixed = format!("⚑ {text}");
let actual = context
    .lines()
    .filter(|l| *l == text.as_str() || *l == prefixed.as_str())
    .count();
assert_eq!(actual, expected, ...);
```

- **No false-fail on duplicates:** `expected` counts every annotation carrying
  the text (both hits' identical lines); `actual` counts every context line
  equal to the text or its `⚑ `-prefixed header form. Each annotation renders
  exactly once — inside its included block as the verbatim line
  (`render_block` joins annotation texts as their own lines,
  `src/recall/format.rs:209-217`), or as a header line when the block was
  excluded (`push_header` in `src/cli/recall.rs:199-208` adds `⚑ ` only for
  texts without `⚑`). Two hits sharing a warning text (two canonical hits
  with the same blast radius, two conflicts with the same writer and age)
  therefore count 2 on both sides of the assertion; an included hit's block
  line is never duplicated into the header, so no line is double-counted.
- **Still fails when an annotation is missing from context:** a dropped
  rendering makes `actual < expected`, failing the `assert_eq!`. The only
  theoretical cross-match (annotation text `X` missing while a different
  annotation's text equals `⚑ X`) cannot arise from the typed producers
  (`blast_radius_warning`, `hot_warning`, `reservation_warning`) or the
  fixed `t85-recall-h3` seed.
- **Verified by execution:** the parity test passes at HEAD; the
  tiny-budget test (`recall_endpoint_tiny_budget_excludes_block_but_keeps_its_warning`)
  still proves the excluded hit's typed warning survives in `context`, so the
  per-text counting direction (annotations → context) is exercised against a
  real exclusion.

## Regression sweep over round-1-verified items (re-verified at HEAD)

- **Single-execution seam:** `cli::recall::run` remains a thin wrapper over
  `pub(crate) run_detailed` (`src/cli/recall.rs:42-55`); `/api/recall`
  (`src/cli/serve_web.rs:926-954`) calls `run_detailed` once and serializes
  `context`/`hits`/`response_annotations` from the same `CliRecall`. The
  verbatim test (`recall_endpoint_returns_the_context_block_verbatim`,
  `serve_web.rs:2254-2267`) still re-renders the payload's own structured
  fields through the shared `pub(crate) render_cli_text` and asserts byte
  equality — no second recall as an oracle; the shared-renderer circularity
  remains bounded by the byte-pinned goldens.
- **Status from the same snapshot:** `assemble.rs:262-270` reads
  `c.canonization_status` from the graph node the hit was assembled from
  (`None` → absent via `then_some`); `dispatch.rs:291-297` mirrors it. The
  blended test still asserts `Canonical`/`Candidate` on the seeded hits.
- **`included_in_context` prefix semantics:** recorded AT the budget cut —
  `assemble.rs:339-347` (`i < kept`), `dispatch.rs:344-349`; every hit stays
  in `hits` with `false` after the cut, and the tiny-budget endpoint test
  still asserts the excluded top hit's `included_in_context == false` while
  its `load_bearing` annotation survives.
- **Kinds pinned at producers:** `detail.rs` carries exactly the six pinned
  kinds; `LoadBearing`/`Conflict`/`Hot`/`Reservation` are attached in
  `assemble.rs:276-303` where the producers exist, `LoadBearing` +
  response-global `Traversal` in `dispatch.rs:291-306,355`; the structural
  golden test still asserts `Traversal` is never attached to a hit and
  appears once. `vector_degraded` remains a CLI-side response-global
  annotation (`recall.rs:121-141`).
- **Additive wire contract:** `RecallResponse` adds `hits` +
  `response_annotations` beside the existing fields; `status` and
  `blast_radius` are `skip_serializing_if`-absent when `None`
  (`detail.rs:69-88`); `response_annotations` is always present (empty
  array), as round 1 established.
- **Mismatch stays error:** `h1_live_contract_changes_update_session_pulse_and_keep_recall_fail_closed`
  still asserts the 502 body carries neither `"hits"` nor
  `"response_annotations"` nor `"included_in_context"` nor `"context":`
  (`serve_web.rs:3348-3366`).
- **Public surfaces unchanged:** `git diff 5eed73c..HEAD --name-only` shows no
  change to `src/types/mod.rs`, `src/mcp/server.rs`, or `src/memory.rs`;
  `RecallHit`/`RecallResult`/`cli::recall::run` signatures untouched; the
  CLI-vs-MCP differential test (`cli_mcp_differential_derive_record_recall`)
  passes.
- **Portal textContent-only:** zero `innerHTML`/`insertAdjacentHTML`/
  `outerHTML`/`document.write` occurrences in `web/`; all untrusted text
  flows through the `textContent`-based `el()` helper (`web/app.js:86-89`).
- **Goldens real:** `fixtures/recall-h3-goldens.json` re-derived from the
  scenario arithmetic (daemon scores × 0.5 with `RecallWeights::default()` →
  0.5/0.45/0.4/0.35/0.3/0.25; blast radius 2 from the two planted
  dependents; `(2 dependents)` from the two structural hits); both golden
  tests pin the serialized payload byte-for-byte and pass.
- **Evidence drives local serve-web:** capture metadata names the local
  portal/session; the runbook's provisioning steps are consistent with the
  captured session (SQLite writer session `h3-evidence`, `rest-api` demo
  scenario, the `SG-Base-VPC`/`malicious markup` derives).

## Commands and results

All commands ran in `worktrees/hardening-h3` at HEAD `a5eb802`
(`LAMBO_COCKROACH_DSN` unset — live Cockroach legs ignored, not passed).

| Command/check | Result |
|---|---|
| Targeted: `recall_endpoint_payload_carries_typed_hits_and_warning_parity`, `recall_endpoint_tiny_budget_excludes_block_but_keeps_its_warning`, `recall_endpoint_structural_payload_carries_traversal_response_annotation`, `recall_endpoint_reports_vector_degradation_as_response_annotation`, `h1_live_contract_changes_update_session_pulse_and_keep_recall_fail_closed`, `h3_blended_payload_matches_golden`, `h3_structural_payload_matches_golden`, `cli_mcp_differential_derive_record_recall`, `recall_entry_reproduces_context_golden` (with `--features fixtures`) | all pass (1 passed each; the daemon context golden reproduces `fixtures/recall-context-golden.txt` byte-for-byte) |
| `cargo test` (default: store-memory + embed-bge + embed-fixture) | pass: lib 705 passed / 1 ignored; binary 6 passed; integration 2 passed; doc 2 passed |
| `cargo test --no-default-features --features store-memory,embed-fixture` | pass: 692 + 5 + 2 + 2 + 2, 0 failed |
| `cargo test --no-default-features --features store-sqlite,embed-fixture` | pass: 513 + 5 + 4 + 1 + 2 + 2 + 1 + 1 + 1 + 2, 0 failed |
| `cargo test --all-features` | pass: lib 835 passed / 8 ignored; binary 6; integration 4 + 1; doc 2 (ignored) — incl. `recall_entry_reproduces_context_golden` |
| `cargo clippy --all-targets -- -D warnings` | pass (exit 0, 0 warnings) |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass (exit 0, 0 warnings) |
| `cargo fmt --all -- --check` | pass |
| `git diff --check 5eed73c..HEAD` | pass (working tree clean) |
| `node --check web/app.js`; `node --check scripts/recording/capture-portal.mjs` | pass (both) |
| `md5sum evidence/h3-recall-cards/cards-*.png` | all four distinct (`c1277531…`, `a8bf0ef2…`, `f6b13d5c…`, `34c3e6ce…`) |
| OCR (tesseract) + pixel analysis (PIL) of the six evidence PNGs | blended/structural/tiny-budget/xss show the claimed cards views, banner, collapsed bars, warnings area, XSS-as-text; verbatim and audit match their runbook rows |
| `ffprobe evidence/h3-recall-cards/eb16e79d….webm` | VP8, 1600x900, 22.4 s (valid replacement for the deleted round-1 webm) |
| `printenv LAMBO_COCKROACH_DSN` | unset; live Cockroach legs ignored, not passed |

## Verdict

**CLEAN / APPROVE** — zero findings. H3-R1-1 is closed: the re-captured
evidence genuinely shows the cards view with score bars, the Canonical status
badge, load-bearing pillar styling, the traversal banner, collapsed excluded
cards, the persistent excluded-warnings area with the owning hit, the XSS
marker rendered as text, the verbatim context view and the audit feed; all
four cards md5s are distinct and the runbook statements match the artifacts.
H3-R1-2 is closed: the parity assertion counts per-text occurrences and
cannot false-fail on duplicate warning texts, while still failing when an
annotation is missing from `context`. The regression sweep found nothing
regressed in the round-1-verified implementation, and all gates pass.
