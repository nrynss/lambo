# Adversarial Review: Remediation T1 part 2 — Round 2 (branch `remed/T1b`)

```text
╔════════════════════════════════════════════════════════════════════╗
║  STATUS: APPROVE — Round 2 of the review/remediate loop            ║
║  Round 1:  APPROVE — 0 P1 / 0 P2 / 5 P3 / 4 nits                  ║
║  Round 2:  all 5 P3 + all 4 nits remediated; the four ORIGINAL     ║
║            invariants re-verified intact; no regressions.          ║
║  Verdict:  APPROVE — 0 P1 / 0 P2 / 0 P3 / 2 nits.                 ║
║            The remediation is genuine, not cosmetic: every P3 fix   ║
║            is backed by a passing test that asserts the refusal or  ║
║            the asymmetry, and the R1-5 startup check is verified    ║
║            read-only and safe on fresh/absent sessions.             ║
╚════════════════════════════════════════════════════════════════════╝
```

## Grounding

Re-reviewed read-only in the `remed/T1b` worktree (detached HEAD @ `1285dd0`,
8-file uncommitted diff). Read the full diff of every changed file, the current
state of every changed region in context, the task doc `### T1 part 2 — REOPENED`
section of `dev-diary/notes/remediation-tasks.md`, and the round-1 review
`adve-review-remed-T1bround1.md` (the 5 P3s + 4 nits this round must close).

**Ran (targeted, as allowed):**
- `cargo check --all-features` — clean.
- `cli::tests::reader_refuses_mismatched_embedding_contract` — **pass** (now
  asserts the stored/writing dim).
- `cli::serve_web::tests::the_module_registers_only_get_routes` (no-writer-lease
  source-grep) — **pass**.
- `graph::derive::tests` (23, incl. the four new) — **pass**.
- `cli::derive::tests` (5, parent-of IP6) — **pass**.

No file was modified; exactly one deliverable (this doc) written.

---

## Per-invariant acceptance verdicts (re-checked against the remediated tree)

### #1 Embedding contract enforced for READERS (writer-only → reader too)
**Re-checked:** `load_reader_graph_with_contract(store, session, Option<&
EmbeddingContract>)` (`src/cli/mod.rs:61`); `load_reader_graph` delegates with
`None`; recall (`src/cli/recall.rs:67-75`) and serve-web `read_stats`
(`src/cli/serve_web.rs:524`) pass `Some(&backends.embedding)`; serve-web recall
flows through `cli::recall::run` so it is covered too. `ensure_compatible`
(`src/types/mod.rs:512`) still names kind + model + dim of **both** the writing
(stored) and the live/attached embedder, with actionable guidance. The reader
test now machine-enforces the "names the writing model" criterion (R1-6, below).
Refuses before serving; serve-web stays lease-free. **Verdict: PASS.**

### #2 Observation never matches a canonical key → refusal at derive boundary
**Re-checked:** `reject_repeated_observation` (`src/graph/derive.rs:558`) still
runs in the `concepts` pre-pass, before any write; the doc now states both
anti-false-refusal limits (first-Observation unguarded; Observation-over-Entity
permitted) and the demote/derive asymmetry (R1-1, R1-2). The refusal is
side-effect-free; all 23 derive tests pass, incl. the new
`derive_repeated_observation_refuses_identity_split` and
`demote_may_duplicate_observation_key_but_derive_still_refuses`. First-time
Observation still derives (created.len()==1); Observed-over-Entity reuses the
concept. **Verdict: PASS (floor, honestly placed and now pinned).**

### #3 Second structural (Hierarchy) parent zeroes blast radius → engine refuses
**Re-checked:** `reject_second_hierarchical_parent` (`src/graph/derive.rs:608`)
still sits in the `parent_of` pre-pass before any write; error names the
claiming parent (`second_parent(prev_key)`), scoped to `EdgeType::Hierarchical`
only; same-parent reinforcement returns `Ok` in both in-batch and cross-call
branches. Doc now states the cross-type scope explicitly (R1-4). New tests
`derive_second_hierarchy_parent_is_refused` (cross-call refusal naming `schema
user`, child keeps exactly one edge) and `reject_second_hierarchical_parent_
same_parent_reinforces` (R1-7, cross-call same-parent Ok in isolation) pass.
**Verdict: PASS.**

### #4 `--parent-of CHILD:PARENT` — accept colon / IPv6, don't refuse
**Re-checked:** `parse_parent_of` (`src/cli/derive.rs:31`) first-colon split,
child colon-free / parent free-text-with-colons; empty side still refuses;
`concept_splits_on_last_colon` intact. `parent_of_accepts_colon_bearing_parent_
ipv6_roundtrip` passes. Client-side `_lambo.py` still pre-refuses both ends, now
with a T7-naming comment (R1-3). Backward compatible. **Verdict: PASS.**

---

## Remediation verification (each of the 8, adversarially)

### R1-1 (P3) — first-Observation unguarded + Observation-over-Entity permitted: doc + test
**Genuinely delivered.** `reject_repeated_observation`'s doc explicitly lists
both limits *"(T1b-R1-1), not gaps: (a) a first Observation derive is unguarded
… (b) Observation-over-Entity is intentionally permitted"*. The test
`derive_repeated_observation_refuses_identity_split` **asserts** both behaviours,
not just documents them: (i) first Observation derives with `created.len()==1`
and a single Observation node; (ii) a re-derive of the same content as
Observation refuses with "opts out of identity" and the graph still holds exactly
one Observation (no silent split); (iii) a **fresh** Observation whose key
matches the existing Entity (`"UserSchema"` → Entity, then `"UserSchema"` as
Observation over the same key) still derives `Ok` by design and reuses the
concept (count of key "schema user" stays 1). This closes the round-1 gap
definitively.

### R1-2 (P3) — demote/derive Observation asymmetry: cross-reference + seam test
**Genuinely delivered.** The doc cross-reference is present
*"(T1b-R1-2) `demote` is NOT subject to this refusal … this guard is derive-only
(caller-declared identity)"*. The seam test `demote_may_duplicate_observation_
key_but_derive_still_refuses` pins the asymmetry directly: `demote` twice on the
same key succeeds and leaves **two** same-key Observations (count==2 — demote's
duplicates are not refused), then a **derive** of that same key as an
Observation refuses with "opts out of identity" and the two demote Observations
are untouched (graph unchanged). The asymmetry the round-1 review flagged is now
documented *at* the seam and machine-enforced.

### R1-3 (P3) — client-side IPv6 deferral to T7: comment
**Genuinely delivered.** `scripts/cloudops/_lambo.py:304-308` now has a comment
naming **T7** and stating the exact relaxation needed ("relax the PARENT-side
`_refuse_colon` (and the pre-filter) to colon-free-child-only; keep child
colon-refusal"). No client logic changed (correct — the half is intentionally
T7's, and the launcher only ever emits single-colon args so nothing regresses).
This is exactly the tracked-deferral comment the round-1 fix called for.

### R1-4 (P3) — cross-type second structural source, documented as deliberate
**Genuinely delivered.** `reject_second_hierarchical_parent`'s doc now states:
*"(T1b-R1-4): a child given one Hierarchical + one Dependency/Causal parent — a
second structural source of a *different* type — still zeroes its blast radius
silently and is out of this guard's scope by design, because Dependency/Causal
fan-in IS the designed multi-source case (record-action ...). Refusing those
would be wrong, not a gap."* The doc claim "engine owns the blast-radius
zeroing" is now accurately bounded. No code change (correct — scoping is
deliberate).

### R1-5 (P3) — serve-web startup fail-fast on embedder mismatch: **CRITICAL, read-only & safe**
**Genuinely delivered, and verified read-only + safe.** `run()`
(`src/cli/serve_web.rs:812-831`) now runs
`load_reader_graph_with_contract(backends.store.as_ref(), &args.session,
Some(&backends.embedding))` before `AppState` construction; on `Err` it prints a
loud startup message ("refusing to start — the live embedder does not match this
session's stored vectors") naming the models and returns `Err`, so serve-web
refuses to start on a real mismatch instead of serving 502s.
- **Read-only:** the helper is `load_session_async` + `assert_session_embedding_
  compatible` only — no `Memory::builder`, no `open_writer`, no `acquire_lease`,
  no `.spawn()`, no stamp. `load_session_async` (`src/store/load.rs:76`) returns
  an empty graph for a missing session and never writes; a reader never stamps.
- **Fresh/absent session safe:** `assert_session_embedding_compatible(None, _)`
  returns `Ok` (`src/resolve.rs:141-144`), because a fresh/absent session has no
  stored contract. So the check does **not** block a legitimately-fresh/absent
  session — only a genuine mismatch refuses. An existing-but-empty session with
  a stamped contract behaves identically to the read paths (refuse only on a
  true disagreement).
- **No-writer-lease source-grep test still passes** (verified) — the added code
  and its comment contain none of the banned tokens.
- **Order/interaction:** the check runs after `authorize_bind_web` (so a
  non-loopback-without-token config error still wins first) and before
  `AppState` (so no borrow/move conflict — `backends` is borrowed then moved).
  It does a one-time session load at startup that is redundant with the
  per-request load in `read_stats`/`api_recall` — negligible one-time cost, no
  correctness impact.

Residual behaviour note (see T1b-R2-1 nit): the startup gate is **all-or-nothing**
— it also blocks the purely store-only surfaces (`/api/session`, `/api/events`,
`/api/inspect`, `/api/graph`) that never need an embedder and would have served
fine under the old per-endpoint 502. This is the requested fail-fast, correct for
a genuine mismatch, but stricter than the per-endpoint behaviour it replaces.

### R1-6 (nit) — reader test machine-enforces "names the writing model"
**Genuinely delivered.** `reader_refuses_mismatched_embedding_contract` now
asserts `msg.contains("dim=1024")` in addition to "incompatible". In the
disagreeing case the stored (writing) contract is dim 1024 and the live is dim
512, so `dim=1024` uniquely identifies the **stored/writing** contract in the
message — the acceptance-#1 criterion is now machine-enforced, not just true in
the source. Passed.

### R1-7 (nit) — focused unit test for re-derive-same-parent Ok path
**Genuinely delivered.** `reject_second_hierarchical_parent_same_parent_reinforces`
calls the helper directly with an empty `pending` map against a graph that
already has a Hierarchical edge from the same parent; the cross-call branch finds
no *different* parent, returns `Ok`, and `pending` still records the containment.
It pins the reinforce branch in isolation (the cross-call *refusal* for a
different parent remains covered by `derive_second_hierarchy_parent_is_refused`).
Passed.

### R1-8 (nit) — `load_reader_graph_with_contract` doc trimmed
**Genuinely delivered.** The doc (`src/cli/mod.rs:54-60`) is now one terse
paragraph: what `Some` refuses vs `None` skips, plus the load-bearing note that a
fresh session is **not** stamped here (the writer owns stamping). The verbose
restatement of the function body is gone.

---

## Regression scan of the full 8-file diff

- **Invariants intact:** all four original guard behaviours still hold and are
  re-tested; all 21 pre-existing derive tests (incl. demote-over-Observation,
  Entity-after-Observation, same-parent hierarchy reinforcement,
  duplicate-key-observation determinism) still pass alongside the 4 new ones.
- **No-writer-lease discipline intact:** serve-web diff adds only
  `load_reader_graph_with_contract(... Some(...))` at startup/per-endpoint and a
  comment — no lease, no writer, no spawn; the source-grep test passes.
- **Reader wiring consistent:** recall + serve-web `read_stats` + serve-web
  `api_recall` all carry the real `backends.embedding`; store-only readers
  (`inspect`/`saints`/`stats`) still attach `None` and are untouched.
- **Parent-of split rules independent & documented:** first-colon (parent-of) vs
  last-colon (concept) both retained; IPv6 round-trip test passes.
- **`ensure_compatible` message change** (single combined error, merged from the
  three per-field errors) is unchanged in wording this round, is still
  `LamboError::Config`, and nothing asserts the old wording.
- `src/cli/derive.rs` changes are the round-1 first-colon split (unchanged this
  round); no further edits.

---

## Findings

### P3
None. All five round-1 P3s (T1b-R1-1 … T1b-R1-5) are remediated genuinely, each
with a passing test asserting the refusal/asymmetry or with the documented
deferral. Specifically: first-Observation/over-Entity now documented *and*
tested (R1-1); the demote/derive seam asymmetry documented *and* tested (R1-2);
the IPv6 client half has an explicit T7-naming comment (R1-3, intentional
deferral); the cross-type structural-fan-in scope is documented as deliberate
(R1-4); and serve-web now fails-fast at startup on a real mismatch, verified
read-only and safe on fresh/absent sessions (R1-5).

### Nits
#### T1b-R2-1 (nit) — R1-5 startup gate is all-or-nothing: it also blocks the store-only surfaces
- **Where:** `src/cli/serve_web.rs:812-831` (`run`).
- **What:** The new startup check refuses the **whole** server on a contract
  mismatch, including `/api/session`, `/api/events`, `/api/inspect`, `/api/graph`
  — surfaces that never attach an embedder and would have rendered fine under the
  old per-endpoint 502. This is the exact fail-fast the task requested and is
  correct for a genuine mismatch (loud, names the models), but it is stricter
  than the per-endpoint failure it replaces: an operator who only wants the
  structural read of a mismatched session can no longer start serve-web at all.
- **Why it matters:** Minor operational trade-off, not a correctness defect. The
  chosen posture is defensible (a mismatched session is a poisoned session and
  fail-closed is the product's stance), and the round-1 fix explicitly offered
  startup-refusal as the recommended option.
- **Fix (optional):** If ever needed, gate only the embedder-bearing endpoints
  (stats/pulse/recall) or add an explicit `--no-embed-check` escape; leave as-is
  otherwise.

#### T1b-R2-2 (nit) — R1-5 startup check's one-time session load is redundant with the per-request load
- **Where:** `src/cli/serve_web.rs:812-831` vs `read_stats`/`api_recall`
  per-request loads through the same helper.
- **What:** The startup gate loads the session once (then discards it), and the
  first request loads it again. Pure belt-and-braces; one read at startup on a
  possibly-remote store.
- **Why it matters:** Negligible; the startup check's value (fail-before-bind,
  one loud error) outweighs one extra read. Not actionable.

---

## Summary

Round 2 fully closes the round-1 remediation list. All five P3s and all four
nits are genuinely addressed, each verified against the current source and a
passing test where one was promised: R1-1 and R1-2 are now documented **and**
machine-enforced at the seam (first-Observation unguarded / Observation-over-
Entity permitted / demote-vs-derive asymmetry), R1-3 adds the T7 naming comment
at `_lambo.py`, R1-4 bounds the "engine owns blast-radius zeroing" claim to
in-Hierarchical fan-in, R1-5 adds a read-only startup fail-fast to serve-web that
is verified safe on fresh/absent sessions and preserves the lease-free
no-writer-lease test, and R1-6/7/8 pin the message-names-model criterion, the
same-parent reinforce `Ok` path, and trim the helper doc. The four original
invariants (reader embedding contract, Observation identity refusal,
second-Hierarchical-parent refusal, first-colon parent-of with IPv6) all still
hold with refuse-before-write ordering and no regressions: `cargo check
--all-features` clean; all 23 graph-derive tests, 5 cli-derive tests, the reader
contract test and the no-writer-lease source-grep test pass. No P1/P2/P3 remain;
two honest nits (the all-or-nothing startup gate, the redundant startup read)
are within the requested spec. **APPROVE.**

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": [], "nits": ["T1b-R2-1", "T1b-R2-2"] }, "summary": "Round 2 verifies all 8 round-1 remediations are genuine and all four original invariants hold with no regressions. R1-1/2 (first-Observation unguarded + Observation-over-Entity permitted; demote/derive Observation asymmetry) are now documented and pinned by passing seam tests; R1-3 adds the T7-naming comment at _lambo.py; R1-4 bounds the blast-radius-ownership claim to in-Hierarchical fan-in; R1-5 adds a serve-web startup fail-fast verified read-only (no writer/lease/stamp) and safe on fresh/absent sessions, with the no-writer-lease source-grep test still passing; R1-6/7/8 machine-enforce the message-names-writing-model criterion, pin the same-parent reinforce Ok path, and trim the helper doc. cargo check clean; all 23 graph-derive + 5 cli-derive + reader-contract + no-writer-lease tests pass. No P1/P2/P3; two honest nits (all-or-nothing startup gate T1b-R2-1, redundant startup read T1b-R2-2) that are within the requested spec. APPROVE." }
```
