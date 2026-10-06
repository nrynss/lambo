# Adversarial Review: issue #9 — inspect focus resolution (rendered node ids, near-matches, the bounded past-cap scan, ledgered failures)

```text
╔═══════════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — clean final pass; 2 low doc-sync findings accepted  ║
║    as recorded residue (docs/reference rows, observability facts      ║
║    contract). Nothing blocking for hand-over.                         ║
║  Verdict: R1 was NOT clean — 6 findings, all low; all remediated at   ║
║    3034685. The loop closed on the remediated tree under the all-low  ║
║    operator rule, not on an empty round. The final cold pass then     ║
║    returned clean=true.                                               ║
║  Rounds: R1 6 (6 low) → remediated · final cold pass clean (2 low     ║
║    accepted, neither behavioral)                                      ║
║  Gates at close: all deterministic gates green — see "State at close" ║
║  Live services: none used, none needed.                               ║
║  Opened: 2026-10-06                                                    ║
╚═══════════════════════════════════════════════════════════════════════╝
```

**Task:** issue #9. On the dogfood rig `lambo_inspect` failed 28.2% of the time
(22 of 78 calls), every failure the bare `no concept matching` refusal
(`src/mcp/server.rs`). Callers type an approximation of a concept they read in
recall's rendered text block, but that block carried zero node ids: the id lived
only in `structuredContent`, the text a model actually reads carried none, and at
p90 a concept is 548 B of text a model will not retype. 22% of successful inspects
already resolved through the fuzzy (substring) leg, which `MAX_INSPECT_SCAN_CONCEPTS
= 2000` (`src/cli/inspect.rs`) hard-refused past; the rig passed 8,207 concepts on
2026-10-06, so the refuse path was days from going live and would have turned that
22% into hard errors. Secondary: failed inspects were unclassifiable, because the
`note_facts` call recording depth/fuzzy ran only on the success path, so Missing,
Ambiguous and Oversized all landed in the ledger as `error_kind: "unclassified"`
with no focus string.

**Branch reviewed:** `task/issue-9-inspect-focus`, two commits over `main`:

| Commit | Role |
|---|---|
| `8d58bfd` | the change itself: the four decided changes |
| `3034685` | R1 remediation — subset-scoped suggestions past the cap plus five doc/test tightenings |

Diff at close: 10 files, +1347/−133 (`git diff main...HEAD --stat`), this record
excluded.

**Why the change looks the way it does:** the decision of record (orchestrator,
mapping the issue's five directions onto four changes; the issue left the design
call open) is:

- *Render the id on the text surface, in short form, and make the short form a
  valid focus.* The affordance and the durable handle were in different channels.
  A rendered id the resolver refuses would recreate the gap it exists to close, so
  the renderer and the resolver share one constant, `format::SHORT_ID_CHARS = 8`
  (`src/recall/format.rs:163-170`), and `resolve_focus` gained a short-form id leg
  (a pure-hex focus of 8..=32 chars resolving by id prefix). Eight hex chars is
  the compromise: at 4-7, real hex-looking words ("def", "cafe") collide with
  concept ids often enough to hijack substring foci; at 8 the collision odds on a
  2,000-concept graph are negligible. The compare is nibble against
  `Uuid::as_bytes`, allocation-free, so the leg runs past the cap too.
- *On Missing, do what Ambiguous does.* The Ambiguous arm (refuse, explain, list
  candidates with their node ids) was the issue's stated model. Near-match ranking
  is bounded by construction: token overlap with the focus, recency tiebreak, the
  issue #2 total order, capped at `MAX_INSPECT_CANDIDATES = 10`, announced as
  suggestions, never a silent match.
- *Past the cap, fall back to a bounded subset instead of refusing.* The cap's
  rationale stands: the O(total-content) lowercase pass is still refused, and the
  cap stays keyed on concept count. But an outright refusal also withdrew every
  fuzzy focus at once the moment a normally growing session crossed 2,000, and the
  2000-concept live-audit comment measured the fuzzy leg at ~0.1 us per concept,
  against a neighbouring unguarded recall scan of 19.1 MB per call. The fallback
  scans the 256 most recently created concepts plus the 256 highest live
  blast-radius concepts, deterministic, comparison-only to build; every renderer
  announces the bound, and a match outside the subset fails honestly as Oversized
  rather than pretending nothing exists.
- *Record the failure mode and the focus in the ledger facts.* Via `note_facts`
  on every error path, before the error returns, so the rig's telemetry can
  classify Missing / Ambiguous / Oversized without reprobing. The mechanism is
  facts, not `error_kind`: no new error_kind vocabulary was minted, and
  classification rides the new `failure` key exactly as decided.

Acceptance 4 (the post-change error rate on the rig) is post-deploy measurement by
decision and is recorded as such, not claimed.

**Method:** one adversarial round, remediation, then a final cold pass. Every
round-1 finding was re-derived by the remediating agent against the code before
its resolution was recorded; resolutions that took a fix hint's stronger option or
refuted a premise say so below. This record independently verified, at close: the
diffstat and commit roles (`git diff main...HEAD --stat`, `git show 3034685 --stat`);
the post-remediation enum shape (`Focus::Oversized { cap, near }` at
`src/cli/inspect.rs:105-108`), the tokenizer (`focus_tokens`,
`src/cli/inspect.rs:234`) and the shared ranking core (`near_matches_over`,
`src/cli/inspect.rs:258`); the three named new tests exist at
`src/cli/inspect.rs:985`, `src/recall/format.rs:581`, `src/recall/assemble.rs:1282`;
and both accepted final findings are live in the tree as stated (`docs/reference/cli.mdx:126`,
`scripts/observability/_ledger.py:41`). The full deterministic gate set is carried
from the loop's closing statement, except the targeted runs listed in "State at
close", which this record re-ran itself.

---

## The four changes at close

| Decision | Where | Announced how |
|---|---|---|
| Recall text carries `id <short>` after the score | `src/recall/format.rs` `render_block`, `short_id`; `DetailedHit.node_id` additive (`src/recall/detail.rs:70-107`), required by the portal's byte-for-byte parity re-render (`src/cli/serve_web.rs:2424-2437`) | every rendered hit block; goldens and tests synced (`fixtures/recall-context-golden.txt`, `fixtures/recall-h3-goldens.json`) |
| Missing offers near-matches with ids | `resolve_focus` full pass → `Focus::Missing { near }` (`src/cli/inspect.rs`); renderers list them under "suggestions (not matches…)" | CLI + MCP text, suggestions with node ids; no-overlap focus stays a bare refusal |
| Past-cap fuzzy scans a bounded subset | `bounded_fuzzy_pass` / `bounded_subset` (`src/cli/inspect.rs:347-404`); `BoundedScan { scanned }` rides Fuzzy/Ambiguous/Oversized | Fuzzy note, Ambiguous message and Oversized refusal all name the subset and the 2,000 cap |
| Ledger records failures | `note_facts({failure, focus})` on all three MCP error arms before the error returns (`src/mcp/server.rs:2028-2033, 2044-2049, 2086-2091`) | `failure: missing/ambiguous/oversized`, `focus` truncated to 200 chars with the explicit marker |

MCP `structuredContent` shapes are untouched: recall's hit JSON is built from the
pre-existing `RecallResult.hits` projection, and inspect's payload fields did not
move. Scope held: no `src/store/**`, no `src/embed/**`, no recall scoring or
candidate arithmetic, no NodeId minting, no schema break.

## Round 1 — 6 findings (all low), all fixed (`3034685`)

No finding contradicted the four decisions or their acceptance criteria. All six
were remediated in one commit; under the all-low operator rule the loop closed on
that remediation rather than spending an empty round.

- **Past-cap misses had no near-match remediation** (`src/cli/inspect.rs:151-153,
  340-344` at the time). `Missing { near }` was unreachable past the cap, so a
  caller on an oversized graph got a refusal with nothing to act on. **Fixed:**
  `Focus::Oversized` gained `near`; `bounded_fuzzy_pass`'s empty branch ranks the
  already-materialized bounded subset through the new `near_matches_over` core
  (subset only, because full-graph ranking is the same O(total-content) lowercase
  pass the cap refuses); CLI and MCP append "nearest within the bounded subset
  (suggestions, not matches; pass a node_id or name one exactly)" after the bound
  message; the ledger failure mode stays `oversized`.
  `a_past_cap_miss_suggests_only_within_the_bounded_subset`
  (`src/cli/inspect.rs:985`) pins the subset scoping by discriminating against a
  full-graph ranking; `past_the_cap_the_fuzzy_leg_scans_a_bounded_subset` and the
  MCP bounded test assert the non-empty list and the header.
- **`InspectParams::focus` doc omitted the short form** (`src/mcp/server.rs:238-240`).
  Prose-only fix naming all three accepted focus forms; the tool-schema golden
  property set passed in the gate run.
- **The single-hit short-id golden could not discriminate per-hit ids**
  (`fixtures/recall-context-golden.txt`, `fixtures/recall-h3-goldens.json` at the
  time). Took the fix hint's unit-assertion option rather than varying fixture ids
  (which would ripple through the spec §13 fixture consumers): the new
  `a_multi_hit_context_carries_each_hits_own_short_id`
  (`src/recall/format.rs:581`) renders three hits with distinct leading hex and
  asserts each block carries its own id and no other hit's, and that the nil
  uuid's short form never renders. Fixture files untouched.
- **The assemble token-budget test hard-coded byte arithmetic**
  (`src/recall/assemble.rs:1297-1311` at the time), so it would silently survive a
  render-shape change it was meant to guard. **Fixed:** the budget is now derived
  by rendering the blocks the same way the assembler does; the separator-charging
  discriminator is restored and self-adjusts
  (`budget_charges_separators_and_enforces_ranked_prefix`,
  `src/recall/assemble.rs:1282`).
- **The allocation claims in the new docs overstated** (`src/cli/inspect.rs:146-150,
  354-367` at the time): the count is allocation-free and the subset build
  allocates no concept content, but the reference sorts and the fixed-size
  blast-radius map are not zero-allocation. Both docs and the same sentence in the
  `MAX_INSPECT_BOUNDED_SCAN` doc reworded to the accurate claim, with
  `bounded_subset`'s doc naming the O(edges) HashMap `format::blast_radii` builds.
  No behavior change.
- **The near-match tokenizer was ASCII-only and let one stray character nominate
  every concept** (`src/cli/inspect.rs:222-230, 244-247` at the time). **Fixed:**
  `focus_tokens` splits on Unicode `char::is_alphanumeric` (a non-ASCII focus
  keeps its tokens whole) and drops tokens under two characters.
  `focus_tokens_drop_single_characters_and_keep_non_ascii_whole`
  (`src/cli/inspect.rs:1026`) pins the tokenizer contract and the behavior: focus
  "a x" on "alpha pad"/"beta pad" is a bare refusal where the old tokenizer
  suggested both.

## Final cold pass — clean, 2 low findings accepted

A cold pass over the accumulated branch read the full diff and the final files and
re-ran its own targeted checks (listed below). Verdict: `clean=true` — nothing
blocking for hand-over. The two findings are documentation-sync residue only,
neither affects behavior, and both were left in place deliberately:

- **(low) `docs/reference/cli.mdx:126` and `docs/reference/mcp.mdx:283`** still
  describe the pre-issue-9 focus resolution ("resolves text exactly first, then as
  a single substring match") with no short-form-id leg, no near-match suggestions
  on a miss and no bounded past-cap scan, and the recall-block examples at
  `docs/reference/cli.mdx:100-104` / `docs/reference/mcp.mdx:171-172` lack the
  `id <short>` token the renderer now emits. Both files are actively maintained
  and neither is in this branch's diffstat, so the drift was introduced by this
  workstream and not yet repaired. Verified live at record time: the old sentence
  is still there.
- **(low) `scripts/observability/_ledger.py:41` and `scripts/observability/README.md:304`**
  still list `lambo_inspect` facts as "depth, fuzzy" only; the `failure` and
  `focus` keys acceptance 3 added are undocumented, so the rig-side tooling the
  acceptance targets has no written contract for the classification keys. Nothing
  breaks: consumers ignore unknown keys, and forward compatibility is stated at
  `_ledger.py:57-59`. Verified live at record time.

## Final verdict

The four decided changes are implemented as decided and none of the rounds found a
decision-level defect. The cap's value, its keying on concept count and the full
lowercase pass it refuses are intact; the fallback is additive and announced in
every renderer. The tests bite: the enum-shape assertions, the bounded-scan
assertions and the ledger-facts assertions would fail on the old code by
construction. The scope lines held exactly (presentation-only recall change,
structuredContent untouched, no store/embed work). Gate state at close: all
deterministic gates green, carried from the loop's closing statement.

## State at close

Re-run by this record at close (cargo 1.97.1, direct toolchain binary; the system
cargo proxy is broken in this sandbox, `unknown proxy name: ZCode-3.14.4-linux-x64`):

- `cargo test --lib cli::inspect` → 8 passed, 0 failed
- `cargo test --lib recall::format` → 12 passed, 0 failed
- `cargo test --lib recall::assemble` → 16 passed, 0 failed

Carried from the loop's closing statement (round records and final pass): the
full deterministic gate set green, including the H3 goldens, the fixture-gated
context goldens, the MCP ledger end-to-end test, `binary_parity`'s ×2 demo
determinism, clippy and fmt. Not run by any round: live pg/cockroach integration,
and acceptance 4 (post-change rig error rate), which is post-deploy measurement by
decision; nothing in the diff or the commit messages claims that measurement was
taken.

## Residual risks

Deliberate, each named here or at its site:

- **Acceptance 4 is pending on deployment.** The post-change inspect error rate on
  the CUDA rig (and the Metal rig, 17 days behind on the same cap cliff) has not
  been measured; this change does not deploy. When measuring, note that past-cap
  no-match foci now book as `oversized` with subset-scoped suggestions (R1
  remediation), and that the ledger's classification keys are `failure` and
  `focus`, not `error_kind`.
- **The two accepted doc-sync findings above are real drift.** `docs/reference/cli.mdx`,
  `docs/reference/mcp.mdx` and the observability facts contract teach the
  pre-issue-9 behavior; the code has moved. Consumers are unaffected, but a reader
  of either surface gets the old contract.
- **A pure-hex content focus of 8+ chars can resolve through the id leg** when it
  uniquely matches a concept id prefix, ahead of the substring leg. Documented
  tradeoff (`src/recall/format.rs:157-162`); at 32 bits and rig scale the
  collision odds are negligible, and several matches still refuse ambiguously.
- **Below-cap Missing lowercases all content a second time** for near-match
  ranking. Runs only on the miss path, under the scan cap; same cost envelope the
  cap already accepts (`src/cli/inspect.rs:244-250`).
- **Past-cap fuzzy results are subset-scoped by design.** A matching concept
  outside the recency and blast-radius windows fails as Oversized even though it
  exists; the refusal says so, and the blast side reads live radii
  (`format::blast_radii`), not the stored field.
- **The ledger's `error_kind` on failed inspects stays `unclassified`.** Anything
  classifying by `error_kind` alone still cannot; the failure mode rides the new
  fact exactly as decided.

## If this code is revisited

- **Fix the two doc findings first.** They are the residue of record and one
  `### Fixed`-sized commit each.
- **The short-form id's 32 bits are a rig-scale assumption.** If a session grows
  two orders of magnitude past 2,000 concepts, revisit the rendered length and the
  id-leg floor together; they share `SHORT_ID_CHARS` so they cannot drift apart by
  accident, only by decision.
- **`bounded_subset`'s recency side sorts references O(n log n).** Fine at the
  cap and well beyond it, but if the fuzzy path ever runs hot on a very large
  graph, a bounded heap replaces the sort without touching the semantics.
