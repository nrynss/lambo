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

**Files:** `src/cli/serve_web.rs` (around line 909)
**Severity:** visible correctness bug
**Blocked by:** nothing

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

**What to change.** At `src/cli/serve_web.rs:909`, guard the `gate_progress`
computation on the concept not already being Canonical. The field is already
`Option<GateProgress>` with `skip_serializing_if = "Option::is_none"`, so
omitting it needs no shape change and no new variant. As a small bonus it also
skips two store queries per inspect call on Canonical concepts.

**Why omission is the right shape.** T11's charter was to surface *why a concept
is not canonical yet*. That is not a question about one that already is. Doing
it server-side rather than as a UI rule means every consumer is correct by
default, including the CLI, the MCP surface, and anything built later, instead
of each client having to know to suppress it.

**What it touches.** Two tests assert on the gate block:
`src/cli/serve_web.rs:2092` and `:2330`. Both will need their fixture concept to
be non-Canonical, or a new assertion that a Canonical concept omits the block.
`:2121` already asserts omission on a miss, so the omission path is established
and tested.

**Open question for whoever picks this up.** A demoted concept can be
re-promoted, and for one sitting in the cooldown the gates are genuinely
informative. Confirm the guard keys on current status rather than on "has ever
been Canonical".

**How to verify.** Inspect a Canonical concept and confirm no gate block.
Inspect a Candidate and confirm the block is unchanged. Inspect a miss and
confirm existing behaviour holds.

---

## Tier 2: needed by decisions already taken

### H3 - Structured recall results beside the verbatim block

**Files:** `src/cli/serve_web.rs` (`RecallResponse`, around line 411),
`src/recall/format.rs`
**Blocked by:** nothing
**Needed by:** the portal redesign, if it goes card-per-result

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

The data is already structured upstream: `render_block`
(`src/recall/format.rs:209`) is handed hits that already carry the label, the
score, the blast radius and the warnings, and flattens them. This is
serialization work, not rearchitecture.

**The contract is already pinned.** The design brief carries the agreed shape,
so the redesign is being drawn against it and this task implements to match
rather than defining it. Per result: `content`, `concept_type`, `status`
(absent when the concept has no rung), `score`, `blast_radius` when present, and
`annotations` as zero or more `{kind, text}` pairs. Kinds are `load_bearing`,
`conflict`, `hot`, `reservation` and `traversal`, matching the existing
`blast_radius_warning` / `conflict_warning` / `hot_warning` /
`reservation_warning` producers in `src/recall/format.rs` plus the traversal
note. If implementation needs to deviate, say so before it lands, because a
design is being built on it.

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

**How to verify.** Golden the structured array against the same fixtures the
context block is goldened on, and assert the two stay consistent.

---

### H4 - The structure tree is fetched once and never refreshes

**Files:** `web/app.js` (around line 430)
**Blocked by:** nothing
**Needed by:** the portal being a product surface rather than a demo page

**The problem.** `loadGraph()` is called once at startup, with the comment
"structure is static for the session, fetched once". That is true for a finished
exhibit session and false for the thing the portal is now meant to be. A
developer watching their agents work is watching a session that is actively
being written to. New concepts appear, dependencies form, blast radii change,
and the tree silently keeps showing the shape the session had when the tab was
opened.

Nothing looks broken. The counts tick up in the tiles while the tree beneath
them stays frozen, which is arguably worse than not updating anything.

**What to change.** Refresh the graph too. It does not need the 1.5 second
cadence of the counts, and it should not have it: the payload is bounded at 4096
nodes and 16384 edges (`MAX_GRAPH_NODES` / `MAX_GRAPH_EDGES` in
`src/cli/serve_web.rs:137-141`), so a large session is a substantial response.
Something like every 15 to 30 seconds, or driven off the status-change feed
noticing something moved, would be proportionate.

Whatever the cadence, the redraw must not collapse expanded nodes or move things
under the cursor, which is the usual way a live-updating tree becomes unusable.

---

## Tier 3: would help

### H5 - Anchor the crates.io include patterns

**Files:** `Cargo.toml`

Cargo `include` patterns are gitignore-style, so the unanchored `README.md`
entry matches at every depth and shipped 14 internal README files in the 0.2.0
crate. Scanned at release time: no live identifiers, no credentials, nothing
resolvable. It is internal process documentation, not a leak.

Fix is three characters in three places: `/README.md`, `/LICENSE`, `/NOTICE`.
Not worth a release on its own. Fold it into whatever forces the next patch.

### H6 - Confirm truncation is actually surfaced

**Files:** `web/app.js`

`/api/graph` and `/api/inspect` both report a `truncated` flag when they hit
their bounds, which is the honest design. Worth confirming the page actually
renders it rather than silently showing a partial tree as though it were whole.
This may already be handled; it was not checked during the brief. If it is not,
it belongs with the redesign rather than as separate work.

### H7 - One session per process

**Files:** `src/cli/serve_web.rs`

`serve-web` takes a single `--session`, so a developer with three sessions runs
three processes on three ports. Fine today, and genuinely out of scope for now.
Recording it because it is the kind of limitation that stops being tolerable
quickly once people use the portal daily, and because the redesign's navigation
will implicitly assume one session forever unless someone says otherwise.

---

## Order

Nothing here blocks anything else here. H1 and H2 are independent, H3 and H4 are
independent, and Tier 3 is opportunistic.

If only one thing gets done, do **H1**: it is the only item on this list where
the product gives a confidently wrong answer with no visible sign anything is
wrong.
