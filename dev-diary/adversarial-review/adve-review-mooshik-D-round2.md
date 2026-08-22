# Adversarial review — mooshik D (event-time clock), round 2

**Reviewer**: independent adversarial reviewer, agent_id `DReview2`. Wrote nothing under
review except this file. Mutation probes ran in a scratch clone
(`/home/nryn/work/.mut-d2r2`, deleted afterwards; tracked-changes count confirmed 0
before deletion). The reviewed tree was never modified.
**Scope**: the five remediation commits on `lambo-for-mooshik` — `ec15d9e` (D-R1-1),
`5c115cc` (D-R1-3), `4473c1a` (D-R1-2), `3a2c0d3` (D-R1-4), `1d30eee` (D-R1-5) —
against the five findings of round 1 (`adve-review-mooshik-D-round1.md`). Round 1
verdict was REQUEST_CHANGES (1 P1 / 2 P2 / 2 P3).
**Worktree**: `/home/nryn/work/lambo`, branch `lambo-for-mooshik` @ `a9b7a4a`
(verified before starting).
**Verdict**: **REQUEST_CHANGES** — all five closures **HOLD**, zero failed closures,
**one new P3** (D-R2-1: remediation-introduced doc residue). The new finding is docs-
only and touches no runtime behaviour; it fails the zero-residue bar, not the design.

## Method

1. Read the '## Round 1 closures' section, then every remediation diff in full
   (`git show ec15d9e 5c115cc 4473c1a 3a2c0d3 1d30eee`).
2. Traced the D-R1-3 seam end to end in the source: `derive_async_as` →
   `begin_interaction_full(agent, Some(prompt), event_time)` at submit
   (`memory.rs:1644`) → `submit_derive(agent, interaction, …)` carries the pre-opened
   `NodeId` (`writeq.rs:2505`) → the background job reads that node back as an
   `Interaction` and stamps every edge it creates with its `event_time`
   (`graph/derive.rs:208/233/390/464`, `hybrid.rs:520/920/960/1023`,
   `graph.rs:392/494`). `record_action_async_as` uses the identical seam
   (`memory.rs:1686-1687`). The stamp cannot miss.
3. Enumerated all callers of the widened `derive_async_as`: MCP derive passes `None`
   (`mcp/server.rs:1620`) and the three in-crate tests pass `None`
   (`memory.rs:4862/5004/5139`) — live behaviour unchanged, exactly as claimed. The
   restored prompt-builder hunk (`memory.rs:1639-1643`,
   `.collect::<Vec<_>>().join("; ")`) is byte-identical to its `f1431ce` form.
4. **Mutation-tested four closures** in the scratch clone at `a9b7a4a`, one mutation
   per claimed guard, each reverted before the next (`git checkout --`; clean tree
   verified after every cycle). A closure counts as mutation-verified only if its
   guard FAILS under the mutation.
5. Re-ran all six gates personally on `a9b7a4a` (results below).
6. New-defect hunt over the remediation diffs themselves: grep for surviving
   references to the removed `begin_interaction_as`; diffed `cargo doc --no-deps`
   warning output against the pre-remediation baseline `d74efc2`.

## Closure verdicts

| Finding | Verdict | Evidence |
| --- | --- | --- |
| D-R1-1 | **HOLDS** (mutation-verified) | `BULK_LIMITS.edges = 99` with prose rewritten to `10 × 99 = 990` (`sqlite.rs:262-274`). Mutant — edges back to 100:
  `cargo check --all-targets --features store-sqlite,fixtures` aborts with
  `error[E0080]: evaluation panicked: edges chunk exceeds SQLITE_MAX_VARIABLE_NUMBER`,
  exit 101. The const assert genuinely fires; the fix restores compilation. |
| D-R1-2 | **HOLDS** (mutation-verified, one caveat) | sqlite `event_time_survives_the_flush_load_round_trip` writes about-time
  `1999-12-31T23:59:59Z` against flush-time `2026-01-02T03:04:05Z` plus `None`
  companions, and covers BOTH positional reads (interactions `try_get(6)`, edges
  `try_get(9)`). Mutant A — edge read drifted to `try_get(8)`: test **FAILS**
  (`FAILED. 0 passed; 1 failed`, exit 101). Cockroach
  `event_time_rides_the_upsert_and_select_shape` pins column-list position, placeholder
  maxima (7/10), `DO UPDATE SET` re-stamp, and named SELECT reads. Mutant B —
  `event_time = EXCLUDED.event_time` dropped from `ON_CONFLICT_EDGE_SQL`: shape test
  **FAILS** (exit 101). Caveat below (mutation C survived — inherent limit of
  no-cluster shape asserts, covered by the open operator leg). |
| D-R1-3 | **HOLDS** (traced) | Full path traced (Method step 2): parameter lands on the interaction at submit,
  before queueing; both derive strategies inherit it for every edge; MCP + three tests
  pass `None`. `begin_interaction_as` has no remaining definition or call. The §1 doc
  now describes the real mechanism. No test pins a stamped async derive end-to-end —
  acceptable, because the seam is composed entirely of already-pinned halves
  (`begin_interaction_full` stamping + derive-edge inheritance, both tested); noted,
  not a finding. Residue from the removal → D-R2-1. |
| D-R1-4 | **HOLDS** (mutation-verified) | `reinforcement_preserves_the_original_edge_event_time`
  (`graph/graph.rs:2060`): original edge about-time `ts(-100_000)` survives
  reinforcement from `ts(500)`, while weight/reinforcements/`last_reinforced` move per
  spec. Mutant — the arm copies incoming event_time
  (`existing.event_time = edge.event_time`): test **FAILS** (exit 101). Exactly the
  three-assignments-away regression round 1 feared is now lethal. |
| D-R1-5 | **HOLDS** | Docs-only as prescribed: `separated_session_count`'s doc states "No production
  caller yet, by design", names `C-solopolicy.md`, and records the mutant C2 must kill.
  Grep confirms no fake caller was introduced — only the definition, its tests, the
  re-export, and the honest doc pointer exist. Bonus: `1d30eee` also removed a
  pre-existing broken intra-doc link (`super::policy`), net-reducing rustdoc warnings
  by one. |

## New findings

### D-R2-1 (P3) — `begin_interaction_as` removal left stale references; two new rustdoc warnings introduced

**Evidence**: `5c115cc` removed `begin_interaction_as` ("clean cutover"), but:

* `src/writeq.rs:47` — module doc still carries the intra-doc link
  `[`crate::Memory::begin_interaction_as`]`, which now emits
  `warning: unresolved link to 'crate::Memory::begin_interaction_as' … the struct
  Memory has no field or associated item named begin_interaction_as`.
* `src/writeq.rs:53` and `:64` — the same removed method cited by name as the
  load-bearing ordering mechanism ("the chain position is pinned by
  `begin_interaction_as`"). The mechanism is real; the name points at nothing.
* `src/writeq.rs:4383` (test comment) and `src/mcp/server.rs:1747` — further prose
  citations of the removed method.
* `src/memory.rs:1601-1602` — `derive_async_as`'s new docstring links private
  [`Self::begin_interaction_full`], adding a
  `rustdoc::private_intra_doc_links` warning.

Diffed against baseline `d74efc2`: 44 warnings → 44 warnings, but the set changed —
one fixed (`separated_session_count` → `super::policy`, by `1d30eee`), two added
(the unresolved link above and the private-link above). All gates stay green because
no gate runs `cargo doc`; the closure's "clean cutover" claim is true of code and
callers but overstated for documentation.

**Scenario**: a reader following `writeq`'s ordering section clicks through to a
method that does not exist, and the next person to touch `Memory` internals inherits
two more warnings in an already-noisy (44-line) doc build.

**Fix**: mechanical — update the four writeq sites and the server comment to name
`begin_interaction_full` (or the public seams), and either make the
`derive_async_as` docstring reference non-linking or document-private-items-safe.
Docs-only; no behaviour change.

## Caveats and observations (not findings)

* **Mutation C survived (bind-order swap, cockroach)**: swapping the last two edge
  binds — `.push_bind(e.last_reinforced)` ↔ `.push_bind(e.event_time)`
  (`cockroach.rs:1570-1571`) — leaves ALL `--features store-cockroach` lib tests green
  (909 passed). SQL text is unchanged by a value-order swap, so a shape assert cannot
  see it; this is the exact mis-bind class D-R1-2 described. It is *not* a closure
  failure: round 1 prescribed SQL-shape assertions precisely because no live cluster
  exists, and Operator-leg item 1's live leg (seed event-timed rows, reload, run
  `interaction_span`) would catch a swapped bind. Recommendation: when the operator
  runs the live leg, include one row whose `event_time ≠ last_reinforced` so the leg
  observes bind order, not just column presence.
* The sqlite round-trip test also exercises the `None` companions staying `None` and
  `created_at` remaining distinct — both halves of the mis-bind scenario pinned, not
  just the happy value.
* An untracked `local:/` scratch directory exists in the worktree root (pre-dates this
  review, other agents' briefs); left untouched.

## Gates rerun personally at `a9b7a4a`

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **pass** |
| `cargo clippy --all-targets -- -D warnings` | **pass** |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | **pass** |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | **pass** |
| `cargo test` | **pass** — exit 0, 878 lib passed / 0 failed (+ integration suites, all `ok`), doc-tests ok |
| `cargo test --features store-sqlite,fixtures` | **pass** — exit 0, 1008 lib passed / 0 failed (+ integration suites, all `ok`), doc-tests ok |

(First test invocation piped through `tail`, which masks cargo's exit status; rerun
both suites with `set -o pipefail` and full result-line capture — every suite line
reads `ok`, zero `failed`. Round 1's process finding stands closed: the gates are
demonstrably runnable and green at the closure HEAD.)

Operator-leg item 1 (live-Cockroach parity) remains open as agreed; nothing in the
remediation code contradicts it.
