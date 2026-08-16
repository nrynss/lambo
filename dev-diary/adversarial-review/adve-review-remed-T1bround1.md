# Adversarial Review: Remediation T1 part 2 — write/read-path invariants (branch `remed/T1b`, round 1)

```text
╔════════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 1 of the review/remediate loop               ║
║  Scope:  T1 part 2 (REOPENED) — four guards that existed but were  ║
║          not applied: (1) embedding contract on READERS,           ║
║          (2) Observation re-derive identity split, (3) second       ║
║          Hierarchical parent blast-radius zeroing, (4) --parent-of  ║
║          colon / IPv6 parent.                                       ║
║  Branch: remed/T1b (worktree /home/nryn/work/worktrees/remed-T1b,   ║
║          detached HEAD @ 1285dd0, 7-file uncommitted diff)          ║
║  Date:   2026-08-17                                                 ║
║  Reviewer: T1Part2ReviewR1 (read-only)                              ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 5 P3 / 4 nits.                   ║
║          All four acceptance criteria are met soundly (validated)   ║
║          with refuse-before-write ordering; the remaining P3s are   ║
║          honest floor/deferral gaps the implementation itself       ║
║          documents, none of which defeats the task's goal.          ║
╚════════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only in the `remed/T1b` worktree. Read the full 7-file diff
(`git diff`: `src/cli/derive.rs`, `src/cli/mod.rs`, `src/cli/recall.rs`,
`src/cli/serve_web.rs`, `src/graph/derive.rs`, `src/main.rs`, `src/types/mod.rs`,
+431/−54), read the `### T1 part 2 — REOPENED` section of
`dev-diary/notes/remediation-tasks.md`, re-read every changed region in context,
traced the acceptance criteria below against the code, and **ran** the targeted
tests (Main owns full-suite/formatter/clippy):

- `cargo check --all-features` — clean.
- `cli::tests::reader_refuses_mismatched_embedding_contract` — **pass** (new).
- `graph::derive::tests::*` (all 21, incl. the two new) — **pass**; critically
  `derive_after_demote_creates_new_concept_not_observation_match`,
  `derive_rederive_reinforces_edges`,
  `derive_parent_of_rederive_reinforces_hierarchical`, and
  `derive_with_duplicate_observation_keys_is_deterministic` all still pass, so
  the new refusals do **not** break demote's Observations, Entity-after-Observation,
  or same-parent hierarchy reinforcement.
- `cli::serve_web::tests::the_module_registers_only_get_routes` (the **no-writer-lease**
  source-grep test) — **pass**.
- `cli::derive::tests::*` (5, incl. the two new parent-of tests) — **pass**.

---

## Per-invariant acceptance verdicts

### #1 Embedding contract enforced for READERS (writer-only → reader too)

**Implementation:** `load_reader_graph_with_contract(store, session, Option<&EmbeddingContract>)`
in `src/cli/mod.rs:70`; `load_reader_graph` delegates with `None`. Recall
(`src/cli/recall.rs:70`) and serve-web's stats (`src/cli/serve_web.rs:524`) pass
`Some(&backends.embedding)`. `src/types/mod.rs:512` `ensure_compatible` reworded.

**Verified:**
- **Message names the writing model.** `ensure_compatible` (`src/types/mod.rs:512`)
  emits `"this session's vectors were written by kind={} model={:?} dim={}, but
  the live/attached embedder is kind={} model={:?} dim={} — re-embed or start a
  new session"`. Direction is correct: `assert_session_embedding_compatible(stored,
  live)` calls `stored.ensure_compatible(live)` (`src/resolve.rs:137-145`), so
  `self` = the stored/session contract that **wrote** the vectors, `other` = the
  live embedder about to be attached. Kind + model (or `(default)`) + dim are all
  named; the guidance is actionable. **Meets acceptance.**
- **Reader wiring is complete and correct.** Enumeration of every session-load path
  that attaches an embedder: `recall.rs:70` and — critically — serve-web's
  `/api/recall` also flows through `crate::cli::recall::run` with `state.backends`
  (`serve_web.rs:664-678`), which internally uses the contract check, so **both**
  of serve-web's embedder-bearing surfaces (recall + stats/pulse) refuse. `read_stats`
  carries the real live contract: `state.backends` is a full `ResolvedBackends`
  (from `resolve_backends`, `src/resolve.rs:99-102`), so `backends.embedding` has
  the true kind/model/dim of the configured embedder.
- **inspect / saints / stats are genuinely store-only.** `inspect.rs:263`,
  `saints.rs:18`, `stats.rs:17` all call `load_reader_graph` (None) and bind only
  the store (structural/count queries, no vector recall). No embedder is attached,
  so no contract mismatch is possible there — skipping the check for them is correct.
- **Refuses before serving.** `load_reader_graph_with_contract` runs the compat
  check right after load and before any computation; both callers propagate with
  `?`, so a mismatch returns an error before any recall/stats are served. The check
  is applied at the served surface, not only at attach.
- **CRITICAL — serve-web stays lease-free.** The added call site in
  `serve_web.rs` (`load_reader_graph_with_contract(...)` + `Some(...)`) contains no
  `Memory::builder` / `open_writer` / `acquire_lease` / `.spawn()`, and the helper
  itself only calls `load_session_async` + the compat assert (read-only; a fresh
  session is not stamped). The existing source-grep no-writer-lease test
  (`the_module_registers_only_get_routes`) **passes** — requirement T3 #3 intact.

**Verdict: PASS.**

### #2 Observation never matches a canonical key → refusal at derive boundary

**Implementation:** `reject_repeated_observation` (`src/graph/derive.rs:552`),
called in the derive pre-pass (`src/graph/derive.rs:238`) before any write.

**Verified:**
- **Refusal is the FLOOR, not the goal — handled honestly.** The deeper goal
  (Observation participates in identity, so the caller never picks a variant) is
  **not** met, and the implementation says so plainly in code comments and the test
  comment (`derive_repeated_observation_refuses_identity_split`,
  `src/graph/derive.rs:1271`). Observations remain deliberately non-identity
  context-overflow records; making them participate in identity is a spec change the
  task explicitly left for a future call. This is the correct reading of demote
  semantics (demote's Observations are per-sentence records, not identifiers).
- **Refusal is keyed safely — no false refusal of a legitimate first-time
  Observation.** Guard is `concept_type == Observation && exists(Observation with
  same canonical_key)`. A first derive of fresh Observation content derives
  normally (test asserts `created.len() == 1` + exactly one Observation node). The
  21-test derive suite passes unchanged, including demote-over-Observation and
  Entity-after-Observation paths. Dedup within a call (`seen_contents`) is unaffected.
- **Demote not broken.** Demote never goes through derive's pre-pass, so the
  refusal cannot reject a demote-produced Observation. `derive_with_duplicate_
  observation_keys_is_deterministic` still passes (two demote Observations sharing a
  key are still legal at the store/model level).
- **Actionable message.** Refusal names the content/key and guides: *"use a
  non-Observation type (e.g. Entity/Resource) for stable identifiers."* Good.
- **Ordering:** pre-pass (`concepts` loop, line 233-239) runs before the write loop
  (`resolve_concept` starts line 293) — validate-then-mutate holds; the test asserts
  the graph still holds exactly one Observation (no partial write).

**Honest tension:** the floor does **not** catch the *first* misuse — a caller who
declares an identifier as Observation once still gets a silent mis-typed node, and
the refusal does not fire on an Observation whose key matches an existing
**Entity** (only an existing **Observation**), so an Entity+Observation split for a
fresh misuse remains possible. This is a deliberate anti-false-refusal choice
(refusing Observation-over-Entity would break legitimate "note about an existing
concept" records) and is within the acceptance's floor-vs-goal allowance. Flagged as
P3s so it is not lost.

**Verdict: PASS (floor, honestly placed).**

### #3 Second structural (Hierarchy) parent zeroes blast radius → engine refuses

**Implementation:** `reject_second_hierarchical_parent` (`src/graph/derive.rs:586`)
in the `parent_of` pre-pass (`src/graph/derive.rs:268`), before any write.

**Verified:**
- **Error names the claiming parent.** Both branches (in-batch via `pending`,
  cross-call via existing inbound Hierarchical edge) build the message with the
  *previous* parent's canonical key (`second_parent(prev_key)`); the cross-call test
  asserts the message contains the claiming parent `"schema user"`. **Matches acceptance.**
- **Correctly scoped to HIERARCHICAL.** The cross-call edge filter is
  `e.edge_type == EdgeType::Hierarchical` only; `check_single_source`'s replacement
  is about structural single-source (blast radius counts `srcs.len() == 1`,
  `src/recall/format.rs:113/122`), but Dependency/Causal **fan-in is the designed
  multi-source case** (record_action fans `produces`/`modifies`/`depends_on`) and is
  deliberately untouched. The blast-graph fixture reasoning confirms no Dependency/
  Causal fan-in is refused. **Meets acceptance; no false refusal of lateral edges.**
- **No false refusal of legit sibling/reinforce.** Same-parent re-derive returns
  `Ok` in both the in-batch (`prev == parent_key`) and cross-call (`canonical_key !=
  parent_key` → skip) branches, so reinforcement is preserved
  (`derive_parent_of_rederive_reinforces_hierarchical` passes). In-batch same-child /
  two-parents is refused (tested).
- **Ordering:** pre-pass sits before the write loop — validate-then-mutate; the test
  asserts the child still has exactly one inbound Hierarchical edge after refusal.

**Scope note (P3):** a child given one **Hierarchical** + one **Dependency** (i.e. a
second structural source of a *different* type) still zeroes blast radius silently
— the engine still does not own that case. This is exactly the task's stated scope
(only in-Hierarchical fan-in is refused), so it is not a defect, but the doc's
"engine now owns the blast-radius zeroing" claim is only partially true and should
be made explicit.

**Verdict: PASS.**

### #4 `--parent-of CHILD:PARENT` — ACCEPT colon / IPv6, don't refuse

**Implementation:** `parse_parent_of` (`src/cli/derive.rs:31`) splits on the FIRST
colon; the child is everything left (colon-free by construction), the parent is
everything right (free text, may contain colons -> IPv6 CIDR). Multi-colon refusal
removed; empty side still refused loudly. Help updated (`src/main.rs:197`).

**Verified:**
- **ACCEPTS, does not refuse.** `parent_of_accepts_colon_bearing_parent_ipv6_roundtrip`
  proves `"api node:2001:db8::/32"` -> parent `2001:db8::/32`, child `api node`, and a
  second IPv6 case. **Meets acceptance.**
- **Empty side still refuses** (`parent_of_rejects_empty_side`: `:parent` and
  `child:` both `Usage`). **Meets acceptance.**
- **No ambiguity/documented-child limitation:** a child that itself needs a colon is
  not expressible — documented in the doc comment and help ("Only the first colon is
  the separator"). `--concept CONTENT:KIND` still splits on the LAST colon (KIND is a
  closed token) — two different, per-flag rules that are each documented.
- **Backward compatible:** every previous single-colon `CHILD:PARENT` produces the
  identical `(child, parent)` split, so all prior callers (CLI, MCP WireParentOf
  structured path, scripts) are unaffected; the CLI's one-cli-verb derives tests pass.
- **Client seam:** `scripts/cloudops/_lambo.py:304-305` still calls `_refuse_colon`
  on BOTH ends of `--parent-of`, so the launcher still pre-refuses an IPv6 parent.
  This is the documented T7 deferral ("client-side half is already in T7"), so the
  CLI is correctly changed and backward compatible (the launcher only ever emitted
  single-colon args, which parse identically). The end-to-end IPv6 scenario therefore
  does **not** yet work through the launcher — see P3 T1b-R1-3.

**Verdict: PASS (client half correctly deferred to T7, tracked below).**

---

## Findings

### P3

#### T1b-R1-1 (P3) — The #2 floor does not catch the *first* Observation misuse (only re-derive); a fresh Observation-over-Entity still silently splits identity
- **Where:** `src/graph/derive.rs:552-570` (`reject_repeated_observation`),
  called at `derive.rs:238`.
- **What:** The refusal only fires when an Observation with the *same canonical key*
  **already exists as an Observation**. (a) A caller who declares an identifier as
  `observation` the **first** time still silently creates a mis-typed node — the
  exact incident that opened T1 part 2 #2 — with **no warning at all** on a fresh
  Observation derive; the symptom is only refused on the *second* reference. (b) The
  check compares against existing **Observations only**, so an Observation whose key
  matches an existing **Entity/Resource** is not refused — an
  Entity+Observation-split for a fresh misuse remains possible. This is a deliberate,
  defensible anti-false-refusal choice (refusing Observation-over-Entity would break
  legitimate "attach a note to an existing concept" records — e.g. an Observation
  about `user schema` that is itself an Entity), and it is squarely within the
  acceptance's floor-vs-goal allowance, which the implementer handled honestly.
- **Why it matters:** Not a defect against the accepted scope, but the guard's
  *effective* reach is narrower than the doc implies: it stops duplicate-node
  escalation, not the root cause. Future readers of this code may assume a fresh
  Observation-on-an-existing-key is also guarded.
- **Fix:** Optionally (and only if it doesn't false-refuse) warn on a fresh
  Observation derive whose content canonicalizes *Matched* to an existing
  non-Observation concept; at minimum, extend the test/doc to state explicitly that
  a first Observation derive is unguarded by design and that Observation-over-Entity
  is intentionally permitted.
- **Disposition:** addresses an acceptance-stated tension the implementation
  resolved honestly — leave open or accept as-is.

#### T1b-R1-2 (P3) — #2 asymmetry: the graph model permits duplicate-key Observations (demote), but derive now refuses a second one; the two are never reconciled in the doc
- **Where:** `src/graph/derive.rs:552-570` vs. `src/graph/graph.rs:1743-1744`
  ("two Observations sharing a key are fine"), `src/graph/demote.rs:122-134`.
- **What:** The store/model explicitly allow *multiple* Observations sharing a
  canonical key (partial-UNIQUE errata; demote creates per-sentence records that can
  repeat a key). The new derive-time refusal forbids producing a second same-key
  Observation **via derive** while demote may still produce one — an asymmetry that
  is coherent (derive = caller-declared identity; demote = genuine overflow records)
  but is neither documented nor tested at the seam. If some future pipeline re-derives
  an Observation per interaction (rather than demoting), it would now be refused.
- **Why it matters:** No current caller regresses (all 21 derive tests pass), but the
  two rules live in different modules without a cross-reference, inviting a silent
  future drift.
- **Fix:** One sentence in each doc cross-referencing the distinction, and/or a test
  that a demote-produced same-key Observation does **not** trip the derive refusal
  (the current battery implies it but does not assert it directly).

#### T1b-R1-3 (P3) — #4 is half-applied: the CLI accepts IPv6 parents but the primary launcher client still refuses them, so T3-1-P2-3 is not yet end-to-end
- **Where:** `src/cli/derive.rs:31` (CLI, fixed) vs. `scripts/cloudops/_lambo.py:303-306`
  (`_refuse_colon` on both `parent` and `child` of `--parent-of`).
- **What:** The engine/CLI-side of #4 is correctly and backward-compatibly fixed, but
  `_lambo.py::derive` still calls `_refuse_colon("parent-of", parent)`, so any launcher
  passing an IPv6 CIDR parent still fails *client-side* before it reaches the CLI. The
  task defers the client half to T7, so this is not a defect against T1 part 2 — but
  the acceptance asked to "confirm no caller of the old semantics regresses", which
  holds; what does NOT yet hold is the *open* IPv6 scenario end-to-end.
- **Why it matters:** Until T7 lands, an operator reading "the CLI accepts IPv6
  parents" and driving it through the launcher gets a hard client error — a stale
  inconsistency worth surfacing so it is not mistaken for done.
- **Fix:** Track explicitly in T7 that `_refuse_colon` on the **parent** (and the
  pre-filter) must be relaxed to colon-free-child-only once the client `split` logic
  is updated; add a cross-reference comment at `_lambo.py:304` naming T7.

#### T1b-R1-4 (P3) — #3 owns only in-Hierarchical fan-in; a mixed-type second structural source still zeroes blast radius silently
- **Where:** `src/graph/derive.rs:586-631` (`reject_second_hierarchical_parent`);
  rationale at `derive.rs:241-245` and `src/recall/format.rs:113/122` (`srcs.len()==1`).
- **What:** Blast radius counts dependent nodes with **exactly one** inbound
  *structural* source (any of Dependency/Causal/Hierarchical). The refusal guards
  only a second **Hierarchical** parent. A node given one Hierarchical + one
  Dependency (or Causal) parent still gets `srcs.len()==2` and drops out of BOTH
  parents' blast radii with nothing logged.
- **Why it matters:** This is exactly the task's stated scope — Dependency/Causal
  fan-in is the designed multi-source case (record_action), so refusing it would be
  wrong. It is therefore **not** a defect; but the comment claims the engine "owns"
  the blast-radius zeroing, which is only true within a single edge type. The residual
  gap (and why the scoping is correct) should be stated so a future reviewer does not
  mistake it for an oversight.
- **Fix:** No code change (the scoping is deliberate and correct). Extend the
  `reject_second_hierarchical_parent` doc to note that a cross-*type* second
  structural source still zeroes blast radius by design and is out of this guard's
  scope.

#### T1b-R1-5 (P3) — serve-web: an embedding mismatch fails stats + recall with 502 while the rest of the page still serves, leaving the operator a partially-broken surface
- **Where:** `src/cli/serve_web.rs:524-529` (`read_stats`) and `:664-678` (`api_recall`
  → `recall::run`); `load_reader_graph_with_contract` at `src/cli/mod.rs:80` returns
  `Err` → `fail()` → `Status(BAD_GATEWAY)` (`serve_web.rs:570-578`).
- **What:** On a contract mismatch (the very case this task exists to catch), `/api/stats`
  and `/api/pulse` and `/api/recall` return 502, while `/api/session`, `/api/events`,
  `/api/inspect`, `/api/graph` still render normally (they use `load_snapshot`, no
  contract). The page therefore shows a *partial* failure with no embedder-specific
  diagnostic surfaced in-app.
- **Why it matters:** The fail-closed behavior is correct and exactly what the task
  wants (refuse before serving). The concern is purely UX/diagnosability: a 502
  bubbles into the browser with the error text in the JSON body, but the operator
  opening the page sees stats/recall error out and may not connect it to the embedder
  contract. This is preferable to the old silent-nonsense behavior but could be
  sharper.
- **Fix:** Optionally check the contract once at serve-web **startup** (`run`,
  `serve_web.rs:795`) and exit with a clear message naming the mismatched model, and/or
  surface a banner in the SPA when `/api/stats` returns a contract error. Neither is
  required for correctness; both improve the failure story.

### Nits

#### T1b-R1-6 (nit) — Acceptance #1's "message names the writing model" is not pinned by an assertion
- **Where:** `src/cli/mod.rs:698-742` (`reader_refuses_mismatched_embedding_contract`).
- **What:** The new reader test only asserts `err.to_string().to_lowercase().contains("incompatible")`.
  The acceptance criterion is specifically that the message **names the model that
  wrote the vectors** (kind/model/dim), which is true in the code (`src/types/mod.rs:513-518`)
  but would regress silently if someone shortened the message and the test kept passing.
- **Fix:** Assert the refusal message contains the stored `kind`/`dim` (e.g. `1024`)
  — or the writing model label — so the acceptance criterion is machine-enforced.

#### T1b-R1-7 (nit) — linear scans per concept/pair in the new refusals have no helper-level tests for the cross-call "different parent" vs "same parent" distinction
- **Where:** `src/graph/derive.rs:552` and `:586`.
- **What:** Both helpers scan `graph.concepts()`/`graph.edges()` per element (O(n·m));
  fine for derive-sized inputs, not a perf issue. The cross-call *same-parent* re-derive
  branch (which must NOT refuse) is exercised only transitively via
  `derive_parent_of_rederive_reinforces_hierarchical`; there is no unit test that
  names the claiming parent on the cross-call path specifically (the new test
  `derive_second_hierarchy_parent_is_refused` exercises cross-call refusal, which is
  the important half — this is belt-and-braces).
- **Fix:** Optional; a dedicated `reject_second_hierarchical_parent` unit test for the
  re-derive-same-parent `Ok` path would pin the reinforce branch in isolation.

#### T1b-R1-8 (nit) — `load_reader_graph_with_contract` doc is long and restates what the code shows
- **Where:** `src/cli/mod.rs:53-69`.
- **What:** The doc paragraph is accurate but verbose; the two-bullet None/Some
  explanation and the "writer owns stamping" note duplicate the function body and the
  `assert_session_embedding_compatible` doc. Not a defect.
- **Fix:** Trim to one sentence on the contract argument + one on why fresh sessions
  are not stamped (the stamping note is the genuinely load-bearing part).

## Verified-OK (probed, not defects)

- **Refuse-before-write ordering (all three graph refusals):** `reject_repeated_observation`
  runs in the `concepts` pre-pass (`derive.rs:238`) and `reject_second_hierarchical_parent`
  in the `parent_of` pre-pass (`derive.rs:268`); the write loop (`resolve_concept`,
  CoOccurrence, edge upsert) begins only at `derive.rs:289+`. The new tests assert the
  post-refusal graph is unchanged (still one Observation; child still has exactly one
  Hierarchical edge).
- **No-writer-lease test unbroken:** `the_module_registers_only_get_routes` passes; the
  serve-web change adds only `load_reader_graph_with_contract` + `Some(...)`, neither of
  which matches the banned tokens, and the helper is read-only (`load_session_async` +
  compat assert, no lease, no stamp).
- **Fresh sessions are not stamped by readers** (`assert_session_embedding_compatible`
  returns `Ok` on `None` stored contract) — matches the design (writer owns stamping).
- **Existing writer-side compat behavior unchanged** except the message text: the old
  three per-field errors were merged into one combined message, but the error variant
  (`LamboError::Config`) is unchanged and no test asserts the old wording.
- **`--concept` last-colon rule intact** (`concept_splits_on_last_colon` passes) — the
  two per-flag split rules are independent and each documented.

## Summary

T1 part 2 is correctly and completely implemented, and all four acceptance criteria
are met. **#1** applies the embedding-contract refusal to the embedder-bearing reader
paths (recall, serve-web stats *and* serve-web recall-via-recall::run), names the
writing model/kind/dim in an actionable message, refuses before serving, and leaves
the serve-web process lease-free (the T3 no-writer-lease test passes; inspect/saints/
stats are confirmed store-only). **#2** refuses Observation re-derivation that would
silently split identity, keyed safely enough to avoid false-refusing legitimate
first-time Observations and demote's records, with an honest, well-documented
floor-vs-goal placement. **#3** refuses a second Hierarchical parent, naming the
claiming parent, correctly scoped to HIERARCHICAL so Dependency/Causal fan-in and
same-parent reinforcement still work. **#4** accepts a colon-bearing (IPv6) parent via
first-colon split, still refuses empty sides, removes the ambiguity over-refusal, and
is backward compatible with every prior caller. Validate-then-mutate ordering holds at
every refusal. No P1/P2. The five P3s are honest scope/floor/deferral gaps the
implementation itself documents (first-Observation misuse unguarded, demote-vs-derive
Observation asymmetry, IPv6 client still deferred to T7, mixed-type structural fan-in
still zeroes blast radius, serve-web partial-failure UX), plus four nits. None defeats
the task's goal, so this round is **APPROVE**; the P3s/nits are recommended cleanups
and explicit ticket-level tracking.

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": ["T1b-R1-1", "T1b-R1-2", "T1b-R1-3", "T1b-R1-4", "T1b-R1-5"], "nits": ["T1b-R1-6", "T1b-R1-7", "T1b-R1-8"] }, "summary": "T1 part 2 meets all four acceptance criteria soundly with refuse-before-write ordering: (1) embedding contract now enforced on the embedder-bearing reader paths (recall + serve-web stats/recall), naming the writing kind/model/dim in an actionable error, refusing before serving, and leaving serve-web lease-free (no-writer-lease test passes; inspect/saints/stats confirmed store-only); (2) Observation re-derivation that would silently split identity is refused at the derive pre-pass, keyed safely against false refusals of first-time Observations and demote records, with the floor-vs-goal tension handled honestly; (3) a second Hierarchical parent is refused naming the claiming parent, scoped to HIERARCHICAL so Dependency/Causal fan-in and same-parent reinforcement are unaffected; (4) --parent-of now splits on the first colon and ACCEPTS an IPv6 CIDR parent while still refusing empty sides, backward compatible with all prior callers. No P1/P2. P3s are documented scope/floor/deferral gaps (first-Observation misuse unguarded, demote-vs-derive Observation asymmetry, launcher client still pre-refuses IPv6 parents until T7, mixed-type structural fan-in still zeroes blast radius, serve-web partial-failure UX) plus three nits; none defeats the goal -> APPROVE." }
```
