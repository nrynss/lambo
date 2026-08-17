# Adversarial Review - Hardening H3, round 1

- **Reviewer:** `h3_review_r1` (independent, source read-only)
- **Scope:** implementation commit `1bfe55c` (structured recall results beside
  the verbatim block) and `93631ef` (browser evidence), against base `5eed73c`
  (post-H2-merge), plus the H3 section of
  `dev-diary/notes/hardening-tasks.md` and the implementation brief
  `/tmp/omp-local/session/h3-implementation-brief.md`
- **Worktree:** `/home/nryn/work/lambo/worktrees/hardening-h3`
  (branch `codex/hardening-h3`)
- **Verdict:** **REQUEST_CHANGES** - 1 P2, 1 P3

The single-execution seam, the typed annotation family, the additive wire
shape, the portal consumption and the goldens are all sound and verified
against the spec. The blocker is evidence quality: the four "cards view"
screenshots committed under `evidence/h3-recall-cards/` do not show the cards
view at all (the cards region sits below the 900px viewport fold), and
`cards-blended` and `cards-tiny-budget` are byte-identical, so the runbook's
claims for them (score bars, status badges, pillar styling, traversal banner,
excluded-warnings area, XSS-as-text) are not visible in the artifacts. One
P3 test-robustness issue is also recorded.

## Findings

### H3-R1-1 (P2) - The committed cards-view evidence does not show the cards view; blended and tiny-budget screenshots are byte-identical

- **Evidence:** `scripts/recording/capture-portal.mjs:105-124` screenshots each
  query with `fullPage: false` and no scrolling, but the H3 results region
  (`#lookup-results`) sits below the lookup input in `web/index.html:143-158`,
  which the 900px viewport cuts off mid-input. OCR and pixel analysis of the
  committed PNGs confirm: `cards-blended-blended-default.png`,
  `cards-structural-structural-default.png`, `cards-tiny-budget-tiny-budget-24.png`
  and `cards-xss-xss-default.png` all contain only the legend, the structure
  tree, the details panel and the typed query in the input box — no cards, no
  score bars, no status badges, no traversal banner, no excluded-warnings
  area, no rendered malicious content. The "9 depend on it" text visible in
  the blended capture is the structure tree, not a card. `md5sum` shows
  `cards-blended-blended-default.png` == `cards-tiny-budget-tiny-budget-24.png`
  (both `b5708803bcca27efda32284442cb4f73`), so the tiny-budget artifact
  cannot show the collapsed cards / excluded-warnings view the runbook
  describes. Only `verbatim-context.png` actually contains recall output (the
  verbatim fallback with the `[Entity, canonical]` marker and the ⚑ line), and
  `audit-feed.png` matches its claim.
- **Impact:** The H3 acceptance criterion "live/browser evidence under
  `evidence/` — screenshot(s) of the cards view showing score bars, status
  badges, load-bearing pillar styling, traversal banner, and the excluded-hit
  warnings area" is not met by the committed artifacts, and the runbook
  descriptions overstate what the bytes show. A reviewer relying on
  `evidence/h3-recall-cards/README.md` would wrongly conclude the cards view
  was visually verified. The DOM-level checks in the capture script
  (`#excluded-warnings` textContent, collapsed `.card-body` count, no `<img>`
  element, `window.__h3xss` never fired) are visibility-independent and can
  pass while the screenshots show nothing, so this is an evidence-capture gap,
  not evidence of an implementation failure.
- **Required remediation:** Re-capture with the results region scrolled into
  view before each screenshot (the script already does
  `#audit.scrollIntoViewIfNeeded()` at `capture-portal.mjs:181`; apply the
  same to `#lookup-results` / the cards area, or use `fullPage: true`), then
  replace the four cards PNGs and the webm with captures that genuinely show
  the claimed views, and re-verify the tiny-budget artifact depicts the
  excluded-warnings area distinctly from the blended view. Keep the captures
  unedited; re-run the runbook to prove the local serve-web flow.

### H3-R1-2 (P3) - The warning-parity test's "exactly once" count breaks on duplicate warning texts across hits

- **Evidence:** `src/cli/serve_web.rs:2347-2357` asserts
  `context.matches(text).count() == 1` for every annotation text. Two hits
  sharing an identical warning line (e.g. two canonical hits with the same
  blast radius → two identical "⚑ Load-bearing pillar — N nodes depend on
  this…" lines, or two conflicts with the same writer and age) produce count 2
  in a spec-correct output: the included hit renders the line inside its
  block, and — per the implementation's own deviation (a), which is
  spec-correct — the excluded hit renders the same line in the header. The
  H3 losslessness property is per warning *line* ("every warning line rendered
  in `context` has exactly one typed counterpart"), not per distinct text, so
  the assertion false-fails a valid, spec-conformant output.
- **Impact:** Test-only: the parity test is brittle against any future fixture
  or seed change that produces duplicate warning texts; it would report a
  regression where the implementation is correct. No production impact.
- **Required remediation:** Count per-text occurrences against the number of
  annotations carrying that text, or assert whole-line parity instead:
  `context.lines().filter(|l| l == text).count()` compared with the count of
  annotations whose text equals `text` (each block line plus each header line
  has exactly one typed counterpart).

## Positive observations

- **Single execution, verified:** `cli::recall::run` is a thin wrapper over
  `run_detailed` (`src/cli/recall.rs:42-55`); `/api/recall` calls
  `run_detailed` and serializes `hits` / `response_annotations` from the same
  `CliRecall` whose `context` field came from the one `Daemon::recall_detailed`
  call. The parity test re-renders the payload's own structured fields through
  the shared `pub(crate) render_cli_text` and asserts byte equality with
  `context` — it genuinely proves the endpoint used the CLI rendering path on
  this execution's data (a divergent serializer would break it); the residual
  shared-renderer circularity is bounded by the byte-pinned goldens
  (`recall-context-golden.txt` at `src/daemon/mod.rs:2551` and
  `src/recall/assemble.rs:1202`) and by the equivalence analysis below.
- **Byte-parity of `render_cli_text` with the old `render_recall_text`:** I
  diffed the old base-commit renderer against the new one. For every
  pre-existing warning arrangement the header lines, their `⚑` prefixing, the
  per-hit producer order (canonical blast, hot conditions, reservation) and
  the rank order are identical; the old `context.contains(text)` skip and the
  new per-hit inclusion skip agree except in exactly two cases, both of which
  the spec mandates the new behaviour for: (a) an excluded hit whose warning
  text duplicates an included hit's (old silently dropped the excluded hit's
  line; new renders it in the header — "token exclusion … does not discard
  that hit's annotations"), and (b) a structural query whose budget cut
  excludes a canonical dependent (old structural `warnings` carried only the
  traversal line, silently losing the excluded hit's blast warning; new
  renders it). Neither case appears in any existing golden or test, and all
  golden/CLI/MCP tests pass.
- **Untyped daemon warnings claim verified:** the three untyped
  `warn_only`/no-index warnings cannot fire on the CLI/HTTP surface:
  `top_k` is clamped to `MAX_TOP_K = 100 < MAX_VECTOR_CANDIDATE_LIMIT = 2048`
  (`src/cli/caps.rs:16`, `src/store/mod.rs:83-90`); the graph is always loaded
  for the caller's session (`load_session_async` returns `Graph::new(session)`
  on `SessionNotFound`, and both Memory and SQLite key snapshots by the
  requested session, `src/store/memory.rs:525-530`, `src/store/sqlite.rs:731-755`);
  and `load_session_async` always returns an index
  (`src/store/load.rs:76-95`), so the no-index warning cannot fire either.
  The MCP path (memory.rs → `Daemon::recall` projection) is unchanged and
  still carries those warnings in `RecallResult.warnings`.
- **Provenance:** status is read from the same `&Graph` snapshot the hit was
  assembled from (`src/recall/assemble.rs:262-270`, `src/recall/dispatch.rs:291-297`),
  never from `is_canonical` or a later store read; `included_in_context` is
  marked at the budget cut (`assemble.rs:344-347`, `dispatch.rs:347-349`) and
  the tiny-`max_tokens` endpoint test proves the excluded hit's `load_bearing`
  annotation survives in both `hits` and `context`.
- **Kinds + parity:** kinds pinned at the typed producers (`load_bearing`
  from `blast_radius_warning`, `conflict`/`hot` from `HotListPayload`,
  `reservation` from `active_reservation`; `traversal` and `vector_degraded`
  response-global, each once, never attached to a hit — asserted by the
  structural and degraded-endpoint tests and the golden tests).
- **Wire contract:** additive fields only; `status` absent iff `None`
  (`skip_serializing_if` + producer `then_some`); `blast_radius` absent when
  `None`; the mismatch stays a 502 error whose body carries none of
  `hits`/`response_annotations`/`included_in_context`/`context`
  (`src/cli/serve_web.rs:3335-3353`); structural payloads carry exactly one
  `traversal` annotation.
- **Public surface:** `src/types/mod.rs`, `src/mcp/server.rs`, `src/memory.rs`
  untouched (confirmed by `git diff 5eed73c..HEAD --name-only`); `RecallHit`,
  `RecallResult` and `cli::recall::run` signatures unchanged; the
  CLI-vs-MCP parity test, the degradation-text test and the source-text guard
  still pass.
- **Goldens:** `fixtures/recall-h3-goldens.json` is real, not round-tripped:
  I re-derived the blended scores from the scenario arithmetic
  (`daemon_score × 0.5` with `RecallWeights::default()` → 0.5 / 0.45 / 0.4 /
  0.35 / 0.3 / 0.25), the blast radius 2 from the two planted dependents, and
  the traversal "(2 dependents)" from the two structural hits; both golden
  tests pin the full serialized payload byte-for-byte.
- **Portal:** zero `innerHTML` occurrences in `web/app.js`; all untrusted text
  (content, annotation text, labels) flows through the `textContent`-based
  `el()` helper; excluded cards are collapsed by default
  (`.card.is-excluded .card-body { display: none }`), the persistent
  excluded-warnings area is populated from excluded hits' typed annotations,
  labelled by owning hit and independent of the expander; `response_annotations`
  render prominently above the cards; the verbatim view survives via the
  fallback toggle. The capture script's XSS regression asserts element absence
  and non-execution (`no <img>`, marker verbatim, `window.__h3xss` never
  fires), not just textContent presence.
- **Scope:** the 23 changed files are all inside the brief's ownership list
  (daemon seam explicitly claimed); nothing outside.

## Commands and results

| Command/check | Result |
|---|---|
| `git diff --stat 5eed73c..HEAD` + full per-file diff/context trace | 23 files, +1536/-153; all production and test paths reviewed |
| `cargo test` (default: store-memory + embed-bge + embed-fixture) | pass: 717 passed / 0 failed / 3 ignored (lib 705+1, integration 12+2); incl. `h3_blended_payload_matches_golden`, `h3_structural_payload_matches_golden`, `recall_endpoint_payload_carries_typed_hits_and_warning_parity`, `recall_endpoint_tiny_budget_excludes_block_but_keeps_its_warning`, `recall_endpoint_reports_vector_degradation_as_response_annotation`, `recall_endpoint_structural_payload_carries_traversal_response_annotation`, CLI-vs-MCP parity, degradation text, source-text guard |
| `cargo test --no-default-features --features store-memory,embed-fixture` | pass: 703 passed / 0 failed |
| `cargo test --no-default-features --features store-sqlite,embed-fixture` | pass: 532 passed / 0 failed |
| `cargo test --all-features` | pass: 859 passed / 0 failed; incl. `recall_entry_reproduces_context_golden` |
| `cargo test --features fixtures recall_entry_reproduces` | pass: daemon golden test reproduces `fixtures/recall-context-golden.txt` byte-for-byte |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass |
| `cargo fmt --all -- --check` | pass |
| `git diff --check 5eed73c..HEAD` | pass |
| `node --check web/app.js`; `node --check scripts/recording/capture-portal.mjs` | pass (both) |
| `md5sum evidence/h3-recall-cards/cards-*.png` | `cards-blended` == `cards-tiny-budget` (same `b5708803…`), demonstrating H3-R1-1 |
| OCR + pixel analysis of the six evidence PNGs (tesseract, PIL) | cards-view artifacts contain no cards region; `verbatim-context.png` and `audit-feed.png` match their runbook claims |
| `printenv LAMBO_COCKROACH_DSN` | unset; live Cockroach legs ignored, not passed |

## Verdict

**REQUEST_CHANGES.** The implementation itself is spec-correct and the code
gates all pass, but the H3 acceptance criterion for live/browser evidence is
not met by the committed artifacts (H3-R1-1): the cards view is never visible
in the screenshots, two artifacts are byte-identical, and the runbook
overstates what they show. H3-R1-2 is a test-robustness over-constraint.
After remediation, re-review must verify the re-captured evidence visually
shows score bars, status badges, pillar styling, the traversal banner and the
excluded-warnings area, and that the parity test no longer false-fails on
duplicate warning texts.

## Remediation disposition

- **Remediation agent:** `H3RemediationR1`
- **Remediation commit:** `f84fb6759ebabcfa453735e790c70b858042193c`
- **Disposition:** both round-1 findings remediated; awaiting independent
  re-review. The original `REQUEST_CHANGES` verdict above is unchanged.

### H3-R1-1 (P2) - remediated

`scripts/recording/capture-portal.mjs` no longer captures a region that sits
below the 900px fold, and no longer accepts a stale render as proof of
output:

- **Real-render waits.** Each query now waits for its query-SPECIFIC content
  to be laid out and visible before capture — the Canonical pillar card with
  its score track and blast-radius note (blended), the traversal banner in
  `#response-annotations` (structural), the `#excluded-warnings` area
  populated with the typed load-bearing warning and collapsed `.is-excluded`
  cards (tiny-budget), and the XSS marker rendered inside a real card (xss).
  The old `#lookup-cards` textContent check could pass on leftover DOM text
  from the previous query while the request was still in flight — the
  mechanism that produced the byte-identical blended/tiny-budget captures.
- **Scroll before capture.** Each cards screenshot pins the results region
  (`#lookup-results`) to the viewport top with
  `scrollIntoView({block: 'start'})` before the shot (mirroring the existing
  `#audit` scroll), so the cards view — not the legend/structure tree — is on
  camera; the tiny-budget capture additionally brings `#excluded-warnings`
  into the viewport.
- **Re-captured evidence.** The runbook was re-run exactly as documented
  against a fresh local SQLite writer session (`/tmp/h3-evidence`,
  `demo --scenario rest-api --session h3-evidence`, the two `derive` seeds)
  and a local `serve-web` (port 7799), using the installed
  `chromium-1234` build. All four cards PNGs now show the H3 results region,
  and the four `cards-*.png` md5s are all distinct (previously
  `cards-blended` == `cards-tiny-budget`, both `b5708803…`).
  OCR + pixel verification of each new PNG: blended shows the pillar card
  with its `Canonical` status badge (white-on-amber chip), teal score bar,
  `Score 2.09 · 9 depend on it`, the `load_bearing` annotation
  ("⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution.")
  and the plain cards below; structural shows the traversal banner
  ("recall: dependency question answered by graph traversal (1 dependents)")
  prominently above the `RDS-Lambo-Demo-DB` card; tiny-budget shows the
  collapsed "Outside the context budget" bars and the persistent
  "Warnings from results outside the context budget" area listing the
  excluded `user schema` hit's load-bearing warning with its owning-hit
  label; xss shows "malicious markup <img src=x onerror=window.__h3xss=1>"
  rendered as text; `verbatim-context.png` shows the exact `lambo recall`
  block with the `[Entity, canonical]` marker and the ⚑ line;
  `audit-feed.png` shows the canonization feed. The runbook statements in
  `evidence/h3-recall-cards/README.md` now match what the artifacts show; no
  wording changes were needed.

### H3-R1-2 (P3) - remediated

`src/cli/serve_web.rs:2334-2371` (`recall_endpoint_payload_carries_typed_hits_and_warning_parity`)
no longer asserts each annotation text occurs exactly once in `context`.
It now asserts per-text occurrence parity: for each distinct annotation
text, the number of `context` lines equal to that text (or to its `⚑ `-
prefixed header form, which `render_cli_text`'s `push_header` produces for
non-⚑ texts rendered outside an included block) must equal the number of
annotations carrying that text. Two hits sharing an identical warning line
(e.g. two canonical hits with the same blast radius) are spec-valid and
count 2 on both sides; the losslessness property is per warning line, not
per distinct text.
