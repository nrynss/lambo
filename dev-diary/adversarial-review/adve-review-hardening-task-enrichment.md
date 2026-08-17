# Adversarial Review - Hardening H2-H7 task enrichment

- **Reviewer:** `hardening_docs_review` (independent; implementation source
  read-only)
- **Date:** 2026-08-17
- **Scope:** documentation-only commits
  `760b7848a446f8aeb1133a587206a99c9b929d6c` and
  `1c941208ba194b461aa476e117c8c0f6661a4390` against the H1 CLEAN head
  `46ca7befa3777b3ba1d496d8c2bb1eceb74211fd`
- **Worktree:** `/Users/narayan/Documents/work/lambo/worktrees/hardening-h1`
- **Verdict:** **REQUEST_CHANGES** - 3 P2 / 2 P3

The enrichments substantially improve the handoffs and most of their factual
claims are accurate. H2's consumer boundary and current-status guard, H4's
description of the static branch UI, H5's package inventory, H6's server caps,
and H7's missing discovery surface all match the reviewed code. H1's completed
section is byte-identical to the CLEAN head.

The document is not clean yet. H3 presents mutually incompatible choices as
one wire contract and requires a success payload for a fail-closed error. H4
and H6 turn an explicitly out-of-branch portal commit into reconciliation work,
which exceeds the task scope. The closing order then contradicts the new task
statuses. Two route/cache claims are also broader than the implementation.

## Findings

### HENR-1 (P2) - H3 has no single satisfiable response contract

- **Evidence:** `dev-diary/notes/hardening-tasks.md:388-424` calls the shape
  "the compatible wire contract", lists `traversal` among per-hit annotation
  kinds, and then says traversal is response-global and should preferably live
  in a response-level array. The same section leaves the hit truncation policy
  and whether the portal is in scope for the implementer to decide. Those
  choices change the JSON schema, parity assertions, owned paths, and browser
  evidence; they are not implementation details under one pinned contract.
- **Impossible criterion:** `hardening-tasks.md:442-446` requires HTTP
  `context` to remain byte-identical to `lambo recall` "including H1's
  fail-closed mismatch". On this branch `cli::recall::run` calls
  `load_reader_graph_with_contract` before producing a `RecallResult`
  (`src/cli/recall.rs:70-75`), and `/api/recall` converts that error to a 502
  (`src/cli/serve_web.rs:919-940`). A mismatch deliberately has no successful
  HTTP response and no `context` field.
- **Impact:** two conforming agents can implement incompatible payloads, while
  no implementation can prove the mismatch clause as written. The portal can
  also enter scope accidentally through an unresolved product choice.
- **Required fix:** pin one schema before H3 is claimable. Hit-owned
  `load_bearing`, `conflict`, `hot`, and `reservation` annotations should stay
  on hits; put traversal and vector/query degradation in an explicitly named
  response-level field, or document another single ownership rule. Pin the
  truncation representation and whether H3 includes browser consumption. If
  those remain undecided, mark H3 `NEEDS DECISION` rather than `OPEN / blocked
  by nothing`. Split parity into (a) byte-identical successful context and (b)
  identical fail-closed mismatch semantics with no hits/context payload.

### HENR-2 (P2) - H4/H6 authorize out-of-scope upstream portal reconciliation

- **Evidence:** H4 says an agent must record whether independent
  `origin/main` commit `5ccd48f` "will be ported or reconciled" before claiming
  H3/H4 (`hardening-tasks.md:546-553`). H6 is declared blocked by portal
  "reconciliation" (`:637-638`) and later directs the implementer to reuse that
  commit (`:697-704`). The commit and its described behavior are real, and the
  paragraphs correctly label it out-of-branch, but the operational directions
  turn it into task authority. The reviewed deliverable chain deliberately
  does not contain that portal rewrite, and orchestration explicitly excluded
  reconciliation with main.
- **Impact:** an H3, H4, or H6 agent can import or reconcile 1,243 insertions and
  1,075 deletions of unrelated `web/*` work while believing the task document
  requires it. That defeats the branch-local ownership and independent review
  boundary.
- **Required fix:** make both paragraphs warning-only: describe the possible
  future overlap, state that `5ccd48f` is not completion evidence and must not
  be ported/reconciled without a separate explicit orchestration instruction,
  and require implementation against the claimed deliverable head. H6's graph
  notice is implementable on this branch, so replace the reconciliation
  blocker with `nothing`; inspect notices apply to any inspect consumer that
  exists on the claimed head.

### HENR-3 (P2) - The final execution order contradicts the enriched statuses

- **Evidence:** `hardening-tasks.md:795-802` still says nothing blocks anything,
  Tier 3 is opportunistic, and H1 should be done first. The same document now
  marks H1 DONE/CLEAN, marks H6 blocked by reconciliation, and marks H7
  `PARKED / NEEDS DESIGN` with an explicit do-not-claim gate.
- **Impact:** this is the section an orchestrator will use to dispatch work. It
  can cause an already completed task to be reselected and a parked task to be
  sent for implementation without its required design decisions.
- **Required fix:** rewrite the order for current status: H1 is complete; name
  the claimable independent tasks after HENR-1/HENR-2 are resolved; describe
  H6's actual branch-local dependency; and state that H7 is not an
  implementation task until its design gate is closed.

### HENR-4 (P3) - H7 overstates session-state and cache behavior for every route

- **Evidence:** `hardening-tasks.md:714-721` says every unscoped route resolves
  through the one `AppState` session. `index`, `stylesheet`, `script`, and
  `healthz` do not read session state, and `healthz` deliberately avoids the
  store (`src/cli/serve_web.rs:804-820`). The compatibility rule at
  `hardening-tasks.md:764-766` also says keep every route `no-store`, but only
  JSON responses use the `json` helper that adds that header
  (`src/cli/serve_web.rs:789-802`); static assets and health do not. The current
  cache regression checks API responses, not every route
  (`src/cli/serve_web.rs:2806-2821`).
- **Impact:** the architecture description is inaccurate and can create
  unnecessary asset/health behavior changes during H7.
- **Required fix:** say every **session-data API** resolves through the single
  state session. Preserve GET-only and bearer middleware for every route, and
  preserve `no-store` for session-memory/API responses.

### HENR-5 (P3) - H6's `truncated` guarantee omits error responses

- **Evidence:** `hardening-tasks.md:642-648` says `/api/inspect` and
  `/api/graph` "always" return a `truncated` boolean. Their successful response
  structs do, but store/load failures return `fail(...)` with only an `error`
  field (`src/cli/serve_web.rs:794-802,955-958,1021-1025`).
- **Impact:** this is a small contract overstatement that could produce a
  vacuous test expecting the flag on 502 responses or an unintended server
  shape change.
- **Required fix:** say every **successful** inspect/graph payload carries the
  boolean. Keep error response semantics out of H6.

## Verified claims and checks

- The worktree's actual pre-review head was
  `1c941208ba194b461aa476e117c8c0f6661a4390`. The handoff's expected full hash
  ended in `07f529...`; that object is not the checked-out commit, although both
  abbreviate to `1c94120`. This record uses the object verified by
  `git rev-parse HEAD`.
- `git diff 46ca7be..1c94120` changes only
  `dev-diary/notes/hardening-tasks.md`; the H1 prefix through line 238 has the
  same SHA-256 on both heads.
- H2's line/symbol references are accurate. `gate_progress` performs exactly
  the two named store queries; the current portal does not call inspect; the
  demotion/cooldown fixture is status `None`; the CLI and MCP do not consume
  the HTTP response.
- H3's provenance analysis is accurate: public `RecallHit` has only
  `is_canonical`; assembly owns typed hot/reservation data before flattening;
  structural traversal is result-global; v0.2.0 publicly exposes these Rust
  types and `cli::recall::run`; MCP serializes the basic hit fields at the named
  lines. `RecallResult.hits` can include blocks omitted from `context`.
- H4's current renderer is a static `div` tree with one startup fetch. The
  graph payload limits and 1.5-second pulse interval are accurate. The
  repository has Playwright dependencies under `scripts/recording`. Independent
  commit `5ccd48f` really uses an overlap-prone 20-second `setInterval`, clears
  and rebuilds the tree, and contains the stated H2/H3-ready UI work.
- `cargo package --list --allow-dirty --no-verify` produced exactly 103 paths.
  It includes fourteen internal README paths in addition to root and examples;
  the four evidence README files appear through both the real path and the
  tracked symlink. The release workflow proceeds directly to `cargo publish`.
  The proposed anchored-list shell comparison is syntactically valid.
- H6's three named cap tests, values (200 / 4,096 / 16,384), current discarded
  graph flag, absent current notice, and out-of-branch hero truncation gap are
  accurate.
- H7 is correctly PARKED: `serve-web` takes one required session, `AppState`
  owns one `SessionId`, public `GraphStore` has no discovery/list method,
  `created_at` is non-portable, docs promise one session, and the AWS unit has
  one `LAMBO_SESSION`. The allowlist/auth/isolation questions justify a design
  gate.
- `node --check web/app.js` and `git diff --check 46ca7be..1c94120` passed.

## Verdict

**REQUEST_CHANGES.** Correct the H3 contract/error semantics, remove implied
authority to reconcile the independent portal commit, align the order with the
actual statuses, and narrow the route/cache and success-payload claims. Re-review
the documentation only; no hardening implementation is authorized by this
record.

## Remediation disposition (2026-08-17)

The original verdict above is preserved. Documentation-only remediation was
applied for independent re-review; this disposition does not change the
reviewer's verdict or claim that the result is clean.

- **HENR-1:** Remediated. H3 now pins one additive success schema: every ranked
  hit carries `included_in_context`, hit-owned annotations use four fixed kinds,
  and traversal/vector degradation live in `response_annotations`. H3 includes
  the portal card consumer and pins it to the included prefix. Successful
  `context` remains byte-identical to the CLI rendering; an H1 contract mismatch
  remains an error with no success fields.
- **HENR-2:** Remediated. H4 and H6 describe `5ccd48f` only as external overlap
  context, explicitly forbid porting or reconciling it on this branch, and
  require work against the claimed deliverable head. H6 is no longer blocked by
  reconciliation.
- **HENR-3:** Remediated. The dispatch order records H1 as DONE/CLEAN, sequences
  the remaining claimable H2-H6 work, and keeps H7 parked behind its reviewed
  design gate.
- **HENR-4:** Remediated. H7 now limits the single-session statement to
  session-data APIs, calls out static assets and `/healthz`, and preserves
  `no-store` only for session-memory/API responses.
- **HENR-5:** Remediated. H6 requires `truncated` only on successful inspect and
  graph payloads and explicitly preserves existing error response semantics.

## Round-2 independent re-review (2026-08-17)

- **Reviewer:** `hardening_docs_review` (independent; implementation source
  read-only)
- **Reviewed remediation:**
  `e6b77eb009df72a37ec70655eb775519e30476c8`
- **Verdict:** **REQUEST_CHANGES** - 1 P2 / 2 P3

HENR-2's main-scope boundary, HENR-3's dispatch order, HENR-4's route/cache
wording, and HENR-5's successful-payload qualification are closed. H1 remains
byte-identical to the CLEAN head. No text authorizes importing or reconciling
`origin/main` commit `5ccd48f`: both external-warning sections expressly forbid
it on this branch and require a separate future orchestration instruction.

HENR-1 is not fully closed. The response schema is much more precise, but its
portal truncation rule does not represent the existing renderer's warning
behavior. Two smaller schema/verification contradictions also remain.

### HENR-R2-1 (P2) - Excluded-hit warnings remain in agent context but disappear from the default cards view

- **Evidence:** H3 sets `included_in_context` only for hits whose complete
  blocks fit, keeps hit-owned warnings attached to their hits, renders cards
  only for the included prefix, and displays only `response_annotations`
  separately (`hardening-tasks.md:388-424,452-456`). In the existing assembly,
  however, every hit's canonical/hot/reservation lines are copied into
  `RecallResult.warnings` before token truncation; the source explicitly says a
  block cut from context still reports its conditions
  (`src/recall/assemble.rs:263-315`). `render_recall_text` then prepends every
  such warning absent from the truncated block context
  (`src/cli/recall.rs:121-145`). A tiny budget can therefore yield a hit with
  `included_in_context: false` whose load-bearing, conflict, hot, or reservation
  warning is nevertheless present in the exact agent `context`.
- **Impact:** the pinned default cards view hides that warning: it suppresses
  the false hit's card, and the warning is correctly hit-owned so it is absent
  from `response_annotations`. This can hide the most safety-relevant text while
  the UI claims to structure the agent's answer. The sentence "a ranked hit the
  agent did not receive" is also inaccurate for this split case: the agent
  receives the warning but not the complete hit block.
- **Required fix:** pin one representation that keeps those warnings visible
  without parsing text. For example, render all ranked hit cards with an honest
  included/excluded marker, or add a visible structured overflow-warning area
  sourced from the excluded hits' typed annotations. Preserve the existing
  context and token-budget behavior. Add a tiny-budget regression where an
  excluded Canonical or conflict hit contributes a warning to `context`, and
  prove the cards view surfaces that same warning.

### HENR-R2-2 (P3) - The claimed additive HTTP shape omits two existing fields

- **Evidence:** `hardening-tasks.md:388-397` enumerates existing `context` and
  `elapsed_ms` plus new `hits` and `response_annotations`, but the current
  `RecallResponse` also has `session` and `query`
  (`src/cli/serve_web.rs:467-475`). "Additive" suggests preservation, but the
  supposedly pinned shape and acceptance criteria never say those fields
  remain.
- **Impact:** an implementer following the enumerated schema can accidentally
  remove legacy response metadata while still satisfying the written checks.
- **Required fix:** include `session` and `query` in the preserved success
  shape and assert all four existing fields remain compatible.

### HENR-R2-3 (P3) - H6's fallback test still requires a nonexistent inspect notice

- **Evidence:** H6 now correctly says inspect notices apply only to consumers
  present on the claimed head, and records that this branch has no inspect
  consumer (`hardening-tasks.md:641-642,655-661`). Its no-browser fallback still
  requires an embedded-asset contract test binding "both response flags to both
  notice elements" (`:671-679`). The current branch has only the graph consumer
  and no inspect notice; H6 explicitly excludes implementing the focus/detail
  panel.
- **Impact:** the fallback either demands a dead, vacuous inspect element or
  silently expands H6 into the parked focus UI, undoing the branch-local
  correction.
- **Required fix:** bind the graph flag to the graph notice and bind an inspect
  flag only to each inspect consumer/notice that actually exists on the claimed
  head. Do not require a placeholder inspect element.

### Round-2 checks

- H1 prefix SHA-256 remains
  `8e33ec7d93f41980d401308cacc706629e55f91c6f72cf524da3454294ad8d30`,
  identical at `46ca7be` and the remediation head.
- The remediation diff changes only the hardening task document and its
  disposition record. `git show --check e6b77eb` and
  `git diff --check 46ca7be..e6b77eb` passed.
- `node --check web/app.js` passed.
- `cargo package --list --allow-dirty --no-verify` still lists exactly 103
  paths.
- The review index remains accurate at 57 rows for 57 records; every indexed
  Markdown target resolves. It correctly remains **OPEN** while this round has
  findings, so no index edit was necessary.

**Round-2 verdict: REQUEST_CHANGES.** Close the excluded-warning presentation
gap and the two narrow schema/test contradictions, then re-review the task
document only. No implementation, integration, main reconciliation, or push is
authorized by this review.
