# Hardening tasks

Product-side work surfaced while briefing the portal redesign. These are not
submission tasks and not deployment tasks: they are things the product does
wrong, or does not do, that a person using Lambo daily would hit.

**Tasks are H1 to H7.** Numbered separately from the remediation tasks (T1 to
T12) and the deployment tasks (D1 to D3), same as those two are numbered apart
from each other.

They are split into two tiers, because the question worth answering first is
which of these are actually needed:

- **Tier 1 (H1, H2)** is correctness. The product currently gives an answer that
  is wrong or self-contradicting. Do these regardless of what happens to the UI.
- **Tier 2 (H3, H4)** is needed by decisions already taken: the portal redesign,
  and the reframing of the portal as the product's human interface rather than a
  demo page.
- **Tier 3 (H5, H6, H7)** would help. None of it is load-bearing.

Everything below was verified against the source or a running instance during
the redesign brief, not inferred.

---

## Tier 1: correctness

### H1 - A mismatched embedder silently returns meaningless answers

**Files:** `src/resolve.rs`, `src/store/sqlite.rs`, `src/store/cockroach.rs`
**Severity:** highest in this document
**Blocked by:** nothing
**Status:** **DONE / CLEAN** (2026-08-17)
**Claim history:** `claimed:h1-implementation` -> `claimed:h1-remediation-r2`
**Worktree:** `worktrees/hardening-h1` (detached from `main` at `f32af2d`)
**Owns:** `src/resolve.rs`, `src/memory.rs`, `src/graph/{graph,hybrid}.rs`,
`src/recall/candidates.rs`, `src/cli/{mod,demo,recall,serve_web}.rs`,
`src/daemon/mod.rs`, `src/mcp/serve.rs`, `src/main.rs`,
`src/store/{mod,memory,sqlite,cockroach,flush,load}.rs`,
`src/canon/{eval,task}.rs`, `tests/cli_write_lease.rs`, `web/app.js`, this H1
section, and the H1 adversarial-review/disposition records

Implementation note: writer attach refuses by default and has a deliberately
named CLI/library override. The read-only `serve-web` process follows the task
author's preference: it opens structural surfaces, exposes the mismatch in
`/api/session`, renders a prominent page banner, and keeps recall fail-closed.
Other embedder-bearing readers continue to refuse rather than return untrusted
rankings. A session with no recorded contract remains compatible for legacy
use.

**The problem.** `resolve_backends` checks that the embedder's output dimension
matches the store's vector width, and nothing else. So pointing Lambo at a
session whose vectors were written by a different 1024-dimension model resolves
cleanly, starts normally, and then ranks every query against a vector space the
stored embeddings do not share. Nothing errors. No warning is printed. Recall
just quietly stops meaning anything, while continuing to return confident,
plausible-looking results with scores.

This is worse than the gate contradiction in H2, because a contradiction is
visible and this is not. A user has no way to discover it except by noticing
that the answers are subtly useless.

`scripts/aws-infra/README.md` already documents the trap for the deployment
case, under "Embedder". Documenting it was the right call at the time. Detecting
it is the fix.

**Why it is fixable now.** The store already records the embedding contract. The
`sessions` row carries `embedding_kind`, `embedding_model` and `embedding_dim`,
written on session upsert (`src/store/sqlite.rs:401-423`) and read back
(`src/store/sqlite.rs:745-772`). `migrations/cockroach/001_init.sql` has the same
columns. The information needed to catch this is already persisted and already
loaded. It is simply never compared against the embedder that was resolved.

**What to change.** On opening a session that has a recorded embedding contract,
compare it against the resolved embedder:

- Same kind and same model: proceed silently. The normal case.
- Different model, same dimension: **this is the dangerous case.** Refuse by
  default with a message naming both models, and offer an explicit override flag
  for the person who genuinely means it (a deliberate re-embedding pass, a model
  rename). Do not make the override the default and do not make it a warning
  that scrolls past, because the whole failure mode is that nothing looks wrong.
- No recorded contract: proceed. Older sessions and stores that never wrote one
  are not errors, and the columns are explicitly nullable snapshot metadata.

Worth deciding as part of this: whether a reader (`serve-web`) should refuse or
warn. A writer refusing is clearly right. A reader refusing means the portal
will not open at all, which may be worse than opening with a prominent banner
saying its search results cannot be trusted. My preference is refuse on write,
and on read surface it hard in `/api/session` so the page can say so.

**How to verify.** Write a session with the fixture embedder, reopen it
configured for a different model at the same width, and confirm it now refuses
instead of resolving. Confirm an unset contract still opens. Confirm the
override flag works.

**Implementation handoff (awaiting adversarial review).** The implementation
is complete in the claimed worktree but is intentionally not marked done or
clean here. Writer commands and `MemoryBuilder` refuse a stored/live mismatch
by default with both contracts named. `--allow-embedding-mismatch` is
writer-only and limited to equal dimensions. With stored vectors present it
also requires the same embedder kind, so it can attest a verified model-id
rename but cannot silently bless a cross-kind migration; a real re-embedding
migration must atomically clear/rewrite its old vectors first. The replacement
contract is an ordered durable mutation. `serve-web` starts in structural-only
mode on mismatch, reports `unrecorded|compatible|mismatch` plus stored and
configured contracts in `/api/session`, renders a warning banner, and leaves
recall fail-closed. Other embedder-bearing readers still refuse. A nullable
legacy contract still opens and is stamped by the next writer.

Verification run in `worktrees/hardening-h1`:

- `cargo test`: **695 passed, 1 ignored** in the library, plus all default
  binary, integration and doc tests passed.
- `env -u RUST_LOG cargo test --no-default-features --features
  store-sqlite,embed-fixture`: **509 passed** in the library plus all SQLite
  binary, durability, integration and doc tests passed. (`RUST_LOG=warn` in
  the parent environment suppresses the INFO synchronization line one existing
  subprocess test waits for, so this matrix deliberately unsets it.)
- `cargo test --features ship h1_` and the Cockroach contract parser test:
  all targeted H1/store tests passed under the full ship feature set.
- `cargo clippy --all-targets -- -D warnings` and `cargo clippy --all-targets
  --features ship -- -D warnings`: passed.
- `cargo check --all-targets --features ship`, `cargo fmt --all -- --check`,
  and `git diff --check`: passed.

Review focus: prove the override cannot create a mixed-vector window, challenge
whether same-kind model-id renames are sufficiently explicit, and check the
reader policy end to end. A configured model of `None` still means "server
default" in the pre-existing contract format; Lambo cannot distinguish two
different server defaults when neither has an identifier, so operators who
need detection must configure a model id.

**Round-2 remediation handoff (awaiting round-3 adversarial review).** H1's
race-free read initially changed the frozen `GraphStore::vector_candidates`
signature. That was broader than necessary and source-breaking for released
Level B adapters. The original required three-argument method is restored
exactly. H1 now adds `vector_candidates_checked` instead: its default delegates
only for stores without `VECTOR_SEARCH`, while an old vector-capable adapter
fails closed until it supplies an atomic contract check. Every production
recall and hybrid lookup uses the checked method; the old method exists solely
as the v0.2.0 compatibility surface. Cockroach overrides the checked method and
keeps the contract read, global ANN growth loop, exact-session fallback, and
commit inside one serializable transaction. The entire transaction is replayed
with the existing bounded `tx_retry` policy after SQLSTATE 40001; a contract
mismatch is an `Invariant` and returns without retry.

The expanded `Owns` list above records the complete frozen-contract ripple:
production readers/writers, both checked call sites, adapter implementations,
test wrappers, and mocks. A regression compiles an adapter that implements only
the old method, invokes that method unchanged, and proves its inherited checked
surface fails closed. Existing recall and hybrid spies make the unchecked
method panic, pinning production routing through the checked method. The
Cockroach retry seam proves a first retryable attempt is replayed and a
deterministic contract mismatch is attempted once. No live Cockroach result is
claimed because `LAMBO_COCKROACH_DSN` was not available; non-live adapter tests
and both minimal feature checks passed.

**Completion record (2026-08-17 - DONE / CLEAN).** The original problem,
decision record, implementation handoff, claim history, and ownership record
above are preserved. H1 completed the repository's implementation -> adversarial
review -> remediation cycle in this detached worktree:

1. Implementation: `1a3accf9d1349dde7ce01cc41538fa755275436c`.
2. Round-1 review, **REQUEST_CHANGES** (2 P1 / 1 P2 / 1 P3):
   `ce0c441de4172f158a4cb3719631ae96a2703dcf`.
3. Round-1 remediation and disposition:
   `c72acf5fb1abd0f909d8bc2ef15f6d579df0d2fd` ->
   `298af97a4049e6ff4e642ec7be54dcec0373bc39`.
4. Round-2 review, **REQUEST_CHANGES** (1 P2 / 1 P3):
   `14c2d52f7ac3cb734778ec13576b5ad658b0e002`.
5. Round-2 remediation and disposition:
   `7cd81943398cd4c7c8249e8605560c62074bb6a4` ->
   `de3b4b7aa862597a929dcfccf4e1f19c13d06790`.
6. Round-3 review, **CLEAN / APPROVE** (zero findings):
   `c57395eda2ce1864e4cc1542e729691ffcf27abe`.

What landed: exact kind/model/dimension checks now protect writer attach and
reader vector lookup. Writers refuse mismatches by default and release their
lease on clean startup failure. The writer-only override is an explicit
same-width attestation, not a re-embedding operation. Readers carry the query's
contract into an atomic checked candidate read; in-repo production never uses
the legacy unchecked lookup. `serve-web` keeps structural access available,
reloads compatibility for `/api/session` and `/api/pulse`, updates its warning
in both directions, and keeps recall fail-closed. Sessions with no recorded
contract remain loadable; their unattested vectors are quarantined before the
next writer stamps a contract. The released three-argument `GraphStore` method
remains source-compatible, while its additive checked surface fails closed for
a legacy vector-capable adapter until that adapter implements atomic checking.
Cockroach runs the contract read, both candidate paths, and commit in one
bounded-retry serializable transaction.

The clean round-3 reviewer independently verified:

- `cargo test --all-features
  legacy_vector_adapter_compiles_unchanged_and_checked_default_fails_closed
  -- --nocapture`: passed; the old adapter/caller compiles unchanged and the
  checked default returns `Capability`.
- `cargo test --all-features
  checked_vector_transaction_retries_backend_but_not_contract_mismatch
  -- --nocapture`: passed; the retryable attempt ran twice and the mismatch
  once.
- `env -u RUST_LOG cargo test --all-features h1_ -- --nocapture`: passed - 6
  library tests, 1 parser test, and 1 real subprocess lease test.
- `env -u RUST_LOG cargo test --no-default-features --features
  store-sqlite,embed-fixture h1_ -- --nocapture`: passed - 4 library tests, 1
  parser test, and 1 real subprocess lease test.
- `env -u RUST_LOG cargo test --all-features store::cockroach::tests::`:
  passed - 24 non-live Cockroach unit tests.
- `env -u RUST_LOG cargo test --all-features`: passed - 827 library tests, 8
  live tests ignored; every binary, integration, and doc harness passed, with 2
  calibration tests ignored.
- `cargo check --all-targets --no-default-features --features
  store-cockroach,embed-fixture`, the corresponding
  `store-sqlite,embed-fixture` check, and
  `cargo clippy --all-targets --all-features -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check f32af2d..HEAD`: passed.

Accepted residual behavior is explicit. Same-kind, same-width relabeling while
vectors exist is permitted only through the dangerous writer override and is
an operator attestation that the change is a model-id alias; using it for an
actual model migration would bless old vectors and is unsupported. Dimension
changes and cross-kind relabeling with vectors are refused atomically; a real
migration must clear/rewrite vectors before committing the replacement
contract. The frozen unchecked adapter method remains callable by external
library consumers, but Lambo production uses only the checked method. A
configured `model: None` still cannot distinguish two changed server defaults;
operators who require that detection must record a model id.

The only unclosed verification limitation is environmental, not a source
finding: `LAMBO_COCKROACH_DSN` was unset throughout review. The eight live
Cockroach legs therefore remained explicitly ignored, and no live SQLSTATE
40001 conflict was reproduced. The transaction/retry boundary and both
candidate branches were source-inspected and covered by the passing non-live
suite. H1 is CLEAN and ready for integration.

---

### H2 - `/api/inspect` ships a self-contradicting payload

**Files:** `src/cli/serve_web.rs` (`InspectResponse` at lines 517-529 and
`api_inspect` at lines 946-1015 on `46ca7be`)
**Severity:** visible correctness bug
**Blocked by:** nothing
**Status:** **OPEN**
**Owns:** `src/cli/serve_web.rs`, this H2 section, and H2
adversarial-review/disposition records

**The problem.** For a concept that has already reached Canonical, the endpoint
returns the status and the blast radius, and then, in the same object, gate
figures that say the concept does not qualify. Captured live from a real demo
session:

```
status:       Canonical
blast_radius: 9

gate_progress:
  blast_radius:          current 0.0,  bar 5.0,  not met
  distinct_interactions: current 0.0,  bar 3.0,  not met
  coverage:              current 0.0,  bar 0.3,  not met
```

**Both halves are correct, and that is why this needs deciding rather than
patching.** The gates deliberately recompute against connections older than
`canonization_edge_min_age`, because those are the numbers the promotion
evaluation itself reaches. The top-level blast radius counts live connections.
On a young session nothing is old enough to count, so the gates read zero while
the live radius reads nine. Aligning the two numbers would be wrong: the gate
figure has to stay on the aged basis or it would misrepresent what the engine
will actually do.

So the fix is not to reconcile them. It is to stop shipping the pairing.

**What to change.** At `src/cli/serve_web.rs:977-1011`, compute
`gate_progress` only when
`concept.canonization_status != CanonizationStatus::Canonical`. The field is
already `Option<GateProgress>` with `skip_serializing_if = "Option::is_none"`
(`:517-529`), so omission needs no wire variant. It also skips the
`blast_radius` and `interaction_span` store queries which exist only to build
the inapplicable gate block.

**Why omission is the right shape.** T11's charter was to surface *why a concept
is not canonical yet*. That is not a question about one that already is. Doing
it server-side rather than as a UI rule makes the HTTP contract correct for
future portal and external consumers without asking each one to suppress a
misleading field.

**Current consumers, precisely.** This is an HTTP API-contract defect. The
page at `46ca7be` does not call `/api/inspect`; `web/app.js:308-315` explicitly
records it as API-only pending the focus/detail pass. The CLI and MCP inspect
surfaces have their own implementations and do not consume this HTTP response,
so H2 must not claim to fix them. A future portal detail panel and any external
HTTP client are the affected consumers.

**Compatibility constraints.** This conditionally removes an already-optional
field. Candidate, Venerable, status-None/demoted, miss, and gate-read-failure
behaviour must remain unchanged. Clients already have to tolerate omission:
the field is skipped when `None`, and a miss omits it today
(`src/cli/serve_web.rs:2180-2196`). Key the decision on *current* status, not
`last_demotion_time` or "has ever been Canonical": budget demotion sets status
back to `None`, and a cooling concept's progress is genuinely useful
(`:2368-2411`).

**Acceptance criteria.**

- A Canonical hit remains HTTP 200 and preserves `status`, `blast_radius`,
  `dependents` and `truncated`, but its serialized object has no
  `gate_progress` key.
- Inspecting a Canonical hit performs neither gate-only store query. Prove this
  with a counting or panicking store wrapper; JSON omission alone is vacuous.
- Candidate, Venerable and status-None hits still carry the same gate shape and
  thresholds.
- A recently demoted status-None concept still reports `in_cooldown` and
  `cooldown_until`.
- Misses and a failed gate read retain their existing omission/degradation
  behaviour.

**Verification.** Replace the Canonical gate assertions at
`src/cli/serve_web.rs:2144-2172` with exact key absence; add or extend a
non-Canonical fixture assertion; retain the cooldown regression at
`:2368-2411`; and add the query-count regression above. Run default tests,
`cargo test --no-default-features --features store-memory,embed-fixture`,
`cargo test --all-features`, both corresponding clippy/check gates,
`cargo fmt --all -- --check`, and `git diff --check`.

**Cut line.** Do not turn H2 into a transactional snapshot of concept status
plus all gate measurements. The existing read can race a writer between graph
load and gate queries; solving that broader consistency problem is not required
to stop pairing a Canonical status with an inapplicable promotion explanation.

---

## Tier 2: needed by decisions already taken

### H3 - Structured recall results beside the verbatim block

**Files:** `src/cli/serve_web.rs` (`RecallResponse` at lines 467-475 and
`api_recall` at lines 913-940), `src/cli/recall.rs`,
`src/recall/{assemble,dispatch,format}.rs`, and the portal assets
**Blocked by:** nothing
**Needed by:** the portal redesign, if it goes card-per-result
**Status:** **OPEN**
**Owns:** `src/cli/serve_web.rs`, `src/cli/recall.rs`,
`src/recall/{assemble,dispatch,format}.rs`, narrowly required internal
presentation types, `web/{app.js,index.html,app.css}`, this H3 section, and H3
adversarial-review/disposition records. Any edit to `src/types/mod.rs` or
`src/mcp/server.rs` is a compatibility ripple and must be explicitly claimed.

**The problem.** `/api/recall` returns the answer as one pre-formatted string.
The page can only render it as a text block. A design that wants one styled card
per result, with the score as a bar and the load-bearing warning as its own
element, cannot be built on top of a string.

This matters more than it sounds. The top result for a structural query is the
load-bearing pillar, promoted into the answer at score 0.00 because it did not
win on similarity at all. That is precisely the behaviour flat vector search
cannot produce, it is the strongest thing the product does, and today it renders
as one line of grey monospace identical to every line beneath it.

**What to change.** Add a structured array beside `context`, do not replace it.
`context` is documented as the agent's block verbatim, warnings and conflict
lines included, so the page can show exactly what an agent receives. That
property is worth keeping.

**The actual handoff boundary.** This is larger than serialization work at
`46ca7be`:

- `/api/recall` calls the public, string-returning `cli::recall::run` and only
  receives the flattened context (`src/cli/serve_web.rs:913-940`).
- That helper consumes `RecallResult` and returns `String`, including its own
  embedding-degradation header (`src/cli/recall.rs:106-145`).
- `RecallHit` has `is_canonical`, not the full Candidate/Venerable/Canonical
  status, and owns no annotations (`src/types/mod.rs:607-633`).
- Assembly temporarily owns the typed hit and its warning lines together, then
  flattens warning ownership into `RecallResult.warnings`
  (`src/recall/assemble.rs:235-289`).
- Structural traversal is a result-global explanation in
  `RecallResult.warnings`, not naturally a per-hit annotation
  (`src/recall/dispatch.rs:326-335`).

Do not recover this information by parsing `context`. Introduce an internal
detailed/presentation result at the point where status and typed warning
provenance still exist, and have the existing CLI string plus the HTTP payload
render from the same single recall execution. Calling recall twice would be
slower and could combine different graph/store instants.

**The compatible wire contract.** Add fields beside `context`; do not replace
or reformat it. Every successful `/api/recall` response has this additive
shape: the existing `session`, `query`, `context` and `elapsed_ms`; `hits`,
containing every ranked hit; and `response_annotations`, containing
response-global explanations. Each hit carries `content`, `concept_type`,
`status` (absent only for status `None`), `score`, `blast_radius` when present,
`included_in_context`, and `annotations` as zero or more `{kind, text}` pairs.
`included_in_context` is `true` exactly for the longest ranked prefix whose
complete rendered blocks appear in `context`; hits after the token-budget cut
remain in `hits` with `false`. Derive annotation kinds from typed producers,
never text patterns.

Hit-owned kinds are:

- `load_bearing` - the Canonical blast warning;
- `conflict` - `HotListPayload::Conflict`;
- `hot` - HighRisk, Drift or Stale;
- `reservation` - the active reservation.

Response-global kinds are `traversal` for the structural-dispatch explanation
and `vector_degraded` when query embedding fails and the vector leg is skipped.
Neither is attached to an arbitrary hit or duplicated across hits. The
`response_annotations` text is also rendered through the existing CLI renderer
into `context`, preserving the warning/header order already visible to agents.
The structured payload is lossless for warnings: every warning line rendered
in `context` has exactly one typed counterpart, either in its owning hit's
ordered `annotations` or, when it has no hit owner, in ordered
`response_annotations`. Token exclusion changes only the complete hit block;
it does not discard that hit's annotations. Preserve the producer order within
both arrays so clients can present the same warnings without parsing text.

Lambo 0.2.0 has already published the public `RecallHit`, `RecallResult` and
`cli::recall::run` surfaces. Adding required fields to their public struct
literals or changing `run`'s return type is source-breaking. Prefer private or
`pub(crate)` presentation types and preserve those public shapes and the MCP
wire fields unless a separate compatibility decision explicitly permits a
break. MCP already serializes basic hits at `src/mcp/server.rs:652-674`; if H3
shares that presenter, its additive changes and text/structured parity must be
reviewed rather than changed incidentally.

**Portal boundary.** H3 includes consuming this schema in the portal: render
one normal card for each hit whose `included_in_context` is true, keep the
verbatim `context` view available, and show `response_annotations` prominently
in the cards view. Cards for `included_in_context: false` may be hidden or
collapsed by default because their complete blocks are outside the context
budget. Their annotations must nevertheless remain visible in a persistent
"warnings from results outside the context budget" area, sourced directly from
those hits' typed annotations and labelled with the owning hit. Expanding an
excluded card may reveal its content but must not be required to discover its
safety warnings. Untrusted concept and annotation text must continue through
`textContent`, never `innerHTML`.

**One thing to get right.** The annotations are a family, not a single warning
type. At least three kinds appear, and they differ in purpose:

```
⚑ Load-bearing pillar. 7 nodes depend on this. Modify with caution.
⚑ recall: dependency question answered by graph traversal (5 dependents)
Agent A wrote to it 12 seconds ago
```

The first is a caution, the second explains how the answer was found, the third
is a live collision notice that another agent is already working in that file.
The structured payload should carry the kind, not just the text, so a client can
treat them differently instead of pattern-matching on the string.

**Acceptance criteria.**

- Existing callers see the same public Rust signatures and MCP fields. Existing
  HTTP `session`, `query`, `context` and `elapsed_ms` fields remain compatible,
  and old consumers can continue reading only those fields.
- On every successful recall, HTTP `context` is byte-identical to the output of
  `lambo recall`, including Canonical markers, warnings, traversal explanation
  and visible vector-leg degradation. An H1 embedding-contract mismatch remains
  an error response with no hits, `response_annotations` or `context` payload.
- Candidate and Venerable status comes from the same graph snapshot as the hit;
  it is not reconstructed from `is_canonical` or a later store read.
- Every hit-owned annotation is attached before flattening and uses the pinned
  kind. Response-global annotations remain prominent in both card and verbatim
  views, and never appear as hit-owned annotations.
- Every warning line in `context` has one structured typed counterpart. Every
  ranked hit carries the correct `included_in_context` value; excluded-hit
  annotations remain visible even when their cards are hidden or collapsed.
- The browser renders all returned text as text, not markup.

**Non-vacuous verification.** Golden a blended result containing a Canonical
load-bearing hit, a Candidate or Venerable, a conflict, a non-conflict hot
condition, a reservation and an annotation-free hit. Golden a dispatched
structural query separately. Exercise a deliberately tiny `max_tokens`, a
failing embedder, and an embedding-contract mismatch. The tiny-budget golden
must exclude a Canonical or conflict hit's complete block while retaining its
warning in `context`, and prove the same typed warning remains visible in the
excluded-hit warning area. Assert successful HTTP context equals the
established CLI renderer without running recall twice, and assert warning
parity between that context and the combined ordered hit/response annotations.
Assert the mismatch remains an error without success fields. Add a
malicious-content/XSS regression and live/browser evidence under `evidence/`.
Run default, minimal `store-memory,embed-fixture`, minimal
`store-sqlite,embed-fixture`, `--all-features`, fmt, clippy and diff-check
gates.

**Cut lines.** H3 does not redesign recall ranking, change token budgeting,
reinterpret scores, or alter MCP merely to make two serializers look alike.
It preserves the verbatim agent contract and adds a presentation model beside
it.

---

### H4 - The structure tree is fetched once and never refreshes

**Files:** `web/app.js` (`loadGraph` at lines 320-347 and the one startup call
at lines 451-455 on `46ca7be`)
**Blocked by:** nothing
**Needed by:** the portal being a product surface rather than a demo page
**Status:** **OPEN**
**Owns:** `web/app.js`, narrowly necessary portal test/evidence files, this H4
section, and H4 adversarial-review/disposition records

**The problem.** `loadGraph()` is called once at startup, with the comment
"structure is static for the session, fetched once". That is true for a finished
exhibit session and false for the thing the portal is now meant to be. A
developer watching their agents work is watching a session that is actively
being written to. New concepts appear, dependencies form, blast radii change,
and the tree silently keeps showing the shape the session had when the tab was
opened.

Nothing looks broken. The counts tick up in the tiles while the tree beneath
them stays frozen, which is arguably worse than not updating anything.

**Current branch reality.** The renderer at `web/app.js:349-370` is a static
tree of `div`s. It has no collapse/expand control, selected node, keyboard focus
or saved interaction state, so the earlier instruction to preserve expanded
nodes describes a future interactive portal rather than this branch. H4 must
still avoid tearing down an unchanged tree and disrupting page scroll or any
focus the surrounding page owns. If the interactive redesign is integrated
first, re-audit and additionally preserve expansion, selection and focused
control by stable identity.

**Refresh contract.** Refresh the graph every 15 to 30 seconds, independently
of the 1.5-second `/api/pulse` loop. The payload is bounded at 4,096 nodes and
16,384 edges (`src/cli/serve_web.rs:133-141`), so it must not ride the fast
poll. Use completion-driven scheduling or an in-flight/sequence guard: graph
requests must not overlap, and an older response must never overwrite a newer
one. Retry after an initial failure. On a later transient failure, retain the
last known-good tree rather than making a valid structure disappear. Compare a
stable payload identity and do not replace the DOM when nothing changed.

H1 made `/api/pulse` reconcile the embedding mismatch banner in both
directions (`web/app.js:286-303`). H4 must leave that scheduling and warning
path intact.

**Acceptance criteria.**

- A node, structural edge, status change and blast-radius change written after
  page load appear without a browser reload within the documented refresh
  bound.
- Graph requests do not run at the 1.5-second pulse cadence, never overlap, and
  cannot apply out of order.
- An initially unavailable graph is retried and appears when the endpoint
  recovers. A transient failure after success leaves the last good tree visible.
- An unchanged response causes no tree DOM replacement; scroll and current
  focus remain stable. If interactive tree controls exist by implementation
  time, expansion, selection and keyboard focus also survive a changed graph.
- H1's mismatch banner still transitions compatible -> mismatch -> compatible
  through the existing pulse without duplicating banners.

**Verification and live evidence.** A source grep proving a timer exists is not
enough. Use a deterministic browser test with successive `/api/graph` payloads,
a delayed response and an injected failure to pin refresh, non-overlap,
out-of-order protection and last-good retention. The repository already has
Playwright tooling under `scripts/recording`; avoid adding a second browser
stack. Run `node --check web/app.js`, the embedded-asset HTTP test, minimal
`store-sqlite,embed-fixture` and all-features Rust gates, fmt, clippy and
diff-check. Capture a real writer + `serve-web` session under `evidence/` using
SQLite or Cockroach: MemoryStore is process-local and cannot prove
cross-process refresh.

**Cut lines.** H4 does not change `/api/graph`, move graph polling into the fast
pulse, implement the focus/detail panel, or absorb H6's truncation UI unless
the claim is explicitly widened.

**External warning only.** Independent `origin/main` commit `5ccd48f` contains
overlapping portal work, but it is not in this deliverable chain and is not
completion evidence. This branch must not port or reconcile that commit. H3
and H4 must be implemented against their claimed deliverable head. A separate
future orchestration instruction would be required before importing any of the
external work; if that ever happens, its interval and full DOM rebuild would
still need H4's overlap, stale-response and interaction-state audit.

---

## Tier 3: would help

### H5 - Anchor the crates.io include patterns

**Files:** `Cargo.toml`
**Blocked by:** nothing
**Status:** **OPEN**
**Severity:** low today; future disclosure risk
**Release policy:** land before the next crates.io release, but do not cut a
release for this alone
**Owns:** `Cargo.toml`, this H5 section, and H5
adversarial-review/disposition records

**The verified package surface.** `Cargo.toml:16-27` intends to be a whitelist,
but its `README.md`, `LICENSE` and `NOTICE` entries are unanchored
gitignore-style patterns. On the reviewed H1 head, `cargo package --list
--allow-dirty --no-verify` lists 103 files and matches `README.md` at every
depth. Fourteen internal READMEs enter solely through that pattern: the demo,
dev-diary, adversarial-review, evidence, cloud-operations and site notes. The
tracked `dev-diary/evidence -> ../evidence` symlink makes the four evidence
READMEs appear under both paths. `examples/README.md` is different: it is
intentionally included by `examples/**` at `Cargo.toml:21` and must remain.

The 0.2.0 release-time scan found no live identifier, credential or resolvable
secret in the accidental files. This is packaging hygiene rather than an
incident, but leaving the broad match in place means a future internal README
can be published without any change to `Cargo.toml` or the release workflow.
The release job currently proceeds directly to `cargo publish`
(`.github/workflows/release.yml:239-242`) and has no file-set assertion.

**The change.** Anchor exactly three entries:

- `README.md` -> `/README.md`;
- `LICENSE` -> `/LICENSE`;
- `NOTICE` -> `/NOTICE`.

**Acceptance criteria.** Root `README.md`, `LICENSE` and `NOTICE` remain in the
crate. `examples/README.md` remains through the explicit examples subtree. No
path under `demo/`, `dev-diary/`, `evidence/`, `scripts/` or `site/` is
packaged. A verifying package build succeeds, proving that the source,
`include_str!` web assets, migrations, fixtures and example config remain
complete. The change has no Rust API or runtime effect.

**Verification.** Record the whole list rather than counting files, because a
legitimate source file added later should not fail an unrelated fixed-count
assertion:

```bash
cargo package --list --allow-dirty --no-verify \
  | tee /tmp/lambo-package-files.txt

diff -u \
  <(printf '%s\n' LICENSE NOTICE README.md examples/README.md | sort) \
  <(rg '(^|/)(README\.md|LICENSE|NOTICE)$' \
      /tmp/lambo-package-files.txt | sort)

! rg '^(demo|dev-diary|evidence|scripts|site)/' \
  /tmp/lambo-package-files.txt

cargo package --allow-dirty
git diff --check
```

`cargo package --list` is the non-vacuous acceptance check for the include
semantics. Adding a permanent release-workflow assertion is optional follow-up,
not required for this three-character manifest fix.

**Cut lines and risks.** H5 does not bump the version, tag or publish a release;
redesign the release workflow; remove examples; or change production code. The
main regression risks are accidentally dropping the root legal/readme files or
an embedded runtime input, which is why both the exact list comparison and the
verifying `cargo package` run are mandatory. Consumers cannot rely on internal
process notes that were never intended as crate API.

### H6 - Confirm truncation is actually surfaced

**Files:** `web/app.js`, `web/index.html`, optionally `web/app.css`, and
`src/cli/serve_web.rs` for narrowly scoped portal regression tests
**Status:** **OPEN** on the reviewed H1 branch
**Severity:** low-frequency visible correctness bug
**Blocked by:** nothing; inspect notices apply to every inspect consumer that
exists on the claimed deliverable head
**Owns:** the files above, this H6 section, and H6
adversarial-review/disposition records

**The server half is already complete.** Every successful `/api/inspect` and
`/api/graph` payload carries a `truncated` boolean
(`src/cli/serve_web.rs:517-529`). Error payloads retain their existing error
shape and are outside H6. The bounds are 200 unique structural Concept
neighbours (`src/cli/caps.rs:38`), 4,096 graph nodes and 16,384 structural edges
(`src/cli/serve_web.rs:137-145`). Existing tests drive all three branches past
their caps and assert both the flag and the bounded length
(`src/cli/serve_web.rs:2283-2344`). Do not change those contracts to solve H6.

**The browser half is missing on this branch.** `loadGraph` fetches the response
at `web/app.js:320-347`, renders its nodes and edges and discards
`g.truncated`. `web/index.html:84-94` has no partial-result notice. The current
portal does not call `/api/inspect` at all; `web/app.js:308-315` explicitly
parks the focus/detail panel. A page can therefore present a bounded tree as the
whole tree, and a later details UI can repeat the same bug if it consumes the
flag in only one location.

**Required UI contract.** A truncated graph shows persistent, accessible text
adjacent to the tree saying the structure is partial. Every browser consumer of
a truncated inspect response must surface the same fact; a hero, summary or
button must not describe the capped array length as "all N". The notice hides
when `truncated` is false and clears when a newer untruncated response replaces
the old one. It must be conveyed in text, not by colour alone. Server-controlled
concept text continues through `textContent`, never `innerHTML`.

**Acceptance criteria.** Exercise `truncated: true` and `false` for the graph,
and for every UI location that consumes `/api/inspect`. Prove the visible,
accessible sentence appears and clears; checking only that the JSON flag or a
DOM id exists is insufficient. Existing endpoint cap tests remain green. If no
browser harness is available at implementation time, add a focused
embedded-asset contract test binding the graph flag to the graph notice and
record a manual runtime/browser capture. Only a change that adds or edits an
`/api/inspect` browser consumer must also bind that consumer's inspect flag to
its notice; do not add a placeholder inspect notice to this branch. Prefer the
repository's existing Playwright tooling under `scripts/recording` over a
second browser stack.

Targeted Rust checks already available are:

```bash
cargo test inspect_truncates_and_reports_at_the_dependents_bound
cargo test graph_truncates_and_reports_at_the_nodes_bound
cargo test graph_truncates_and_reports_at_the_edges_bound
node --check web/app.js
cargo fmt --all -- --check
git diff --check
```

Add and run a client regression with the equivalent of
`embedded_page_surfaces_graph_truncation_flag`. If the claimed change adds or
edits an inspect consumer, add the corresponding non-vacuous inspect-notice
regression too. Run the normal minimal `store-sqlite,embed-fixture` and
all-features gates.

**Cut lines and risks.** H6 does not add pagination or "load more", raise or
remove safety caps, change `/api/graph` or `/api/inspect`, absorb H4's refresh
cadence, or implement H3's structured recall. The bounds protect response size;
surfacing them must not weaken that denial-of-service control. A later graph
refresh must also clear stale warnings, but implementing refresh remains H4.

**External warning only.** Independent `origin/main` commit `5ccd48f` contains
overlapping graph/details notices, but it is not part of this deliverable chain
and cannot make H6 done here. This branch must not port or reconcile it. Work
only from the consumers present on the claimed deliverable head. If a separate
future orchestration instruction imports that commit, re-audit every inspect
consumer: its hero path was observed discarding `d.truncated` and presenting a
capped array length as the apparent total.

### H7 - One session per process

**Status:** **PARKED / NEEDS DESIGN**
**Severity:** low current product pressure; high architectural and security
risk if implemented casually
**Do not claim for implementation until:** selection, discovery, URL and auth
decisions below are recorded

`serve-web` takes exactly one required `--session` (`src/main.rs:68-74`), and
`AppState` owns one `SessionId` (`src/cli/serve_web.rs:302-311`). Every
session-data API resolves through that state (`src/cli/serve_web.rs:1135-1158`);
static assets and `/healthz` are not session-data APIs, and `/healthz`
deliberately avoids the store. The public store contract has `load_session(id)`
but no session discovery/list operation
(`src/store/mod.rs:101-146`), the CLI guide promises one session
(`docs/reference/cli.mdx:272-287`), and the AWS service carries one
`LAMBO_SESSION` (`scripts/aws-infra/launch_exhibit_ec2.py:434-456`). This is not
a local navigation tweak.

**Recommended first scope: an explicit configured allowlist, not store-wide
discovery.** Let the operator name one or more sessions while preserving today's
single-session invocation, and expose only those names. This avoids adding a
required `GraphStore::list_sessions` method that would source-break published
Level B adapters. It also avoids turning an authenticated or accidentally
reachable portal into an implicit session-name enumeration endpoint.

**Decisions required before implementation.** Record all of these in a short
design note:

1. Is selection a repeatable `--session`, a config allowlist, or store-wide
   discovery? If discovery wins, define an additive fail-closed adapter surface
   and capability rather than a new required trait method.
2. Is the selected session encoded in a namespaced path or query parameter?
   Selection must be request-local and deep-linkable, never one mutable global
   "current session" shared by browsers.
3. Does one bearer authorize every configured session, or is per-session auth
   required? Are session names themselves sensitive, and may a client list them
   or only open a name it already knows?
4. What stable metadata and ordering may the selector show? `created_at` is not
   portable across stores (`src/store/mod.rs:140-144`), so the design cannot
   assume it exists.
5. Do today's unscoped APIs remain aliases in single-session mode? Define 404
   and authorization behaviour for unknown, unconfigured and oversized session
   ids without leaking whether an unconfigured session exists.
6. What request/concurrency bounds prevent a client from multiplying full graph
   loads and embedding work across allowed sessions?

H1 compatibility, vector-search trust, freshness and warning state are all
session-specific. Switching sessions must recompute them independently; one
mismatched session must remain structural-only without disabling recall for a
compatible session or blessing its vectors.

**Likely ownership once approved.** `src/main.rs`,
`src/cli/serve_web.rs`, `web/{index.html,app.js,app.css}` and
`tests/binary_parity.rs`; the reference CLI/config docs and mirrored site pages;
and the AWS launcher only if that deployment opts into multiple sessions.
Store adapters and migrations enter scope only if store-wide discovery is
chosen. The eventual claim must enumerate the exact paths rather than treating
this list as blanket ownership.

**Compatibility and security constraints.** Preserve the existing one-session
command and response behaviour. Keep every route GET-only and under the
existing bearer middleware; preserve `no-store` for session-memory/API
responses. Static assets and `/healthz` need not acquire session-scoped cache
semantics. Encode untrusted session and concept text as data, not markup. Never
allow a session selection to escape the configured
allowlist, reveal whether another session exists, or reuse another client's
cursor/freshness/embedding state. One bearer covering multiple sessions is a
material expansion of read authority and must be explicit. Arbitrary switching
also amplifies graph-load and recall work, so bounded concurrency is part of the
design, not an optional hardening pass.

**Eventual acceptance criteria for the recommended allowlist design.** One
session behaves exactly as today. Two configured durable sessions are
selectable and deep-linkable on one port; concurrent clients can select
different sessions without affecting each other. Counts, events, graph,
inspect, recall, freshness and H1 warnings never cross the boundary. Unknown or
unallowed sessions fail without existence disclosure. A mismatch in one
session does not disable compatible recall in the other. The read-only route
sweep, bearer checks, cache headers and existing AWS one-session systemd
configuration remain valid. Integration tests cover two-session isolation,
concurrent selection, invalid ids, unauthorized requests and backward
compatibility.

**Explicit non-goals.** H7 does not alter `lambo serve` or the
single-writer-per-session model; add writes, session creation/deletion or
cross-store aggregation; perform model migration/re-embedding; mandate
store-wide discovery; or broaden the public AWS deployment automatically. It
remains parked because choosing among those product and authority boundaries is
the work that must happen before code.

---

## Order

H1 is **DONE / CLEAN** and must not be selected again. H2 is the next
correctness task. H3 and H4 are then independently claimable against the
deliverable head; both touch the portal, so integrate one clean task before
claiming the other to keep ownership and review boundaries unambiguous. H5 and
H6 remain open and independent, with H6 applying to the browser consumers that
exist when it is claimed. H7 is **PARKED / NEEDS DESIGN** and is not an
implementation task until its selection, discovery, URL and authorization
decisions are recorded and reviewed.

Current dispatch order is therefore H2, H3, H4, H5, H6. Complete each task's
implementation -> adversarial review -> remediation loop before integrating it
and moving to the next. Re-audit line references and consumers at claim time,
because earlier integrations can move or expand the named surfaces.
