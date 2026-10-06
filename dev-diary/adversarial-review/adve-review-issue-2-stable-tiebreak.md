# Adversarial Review: issue #2 — the stable tie-break (`canonical_key` ahead of the node id)

```text
╔═══════════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — final findings remediated at bd77160                ║
║  Verdict: the final pass (R5) was NOT clean — 2 findings, both in     ║
║    daemon/drift.rs. Both are remediated in bd77160 (one by naming     ║
║    the contract, one by refutation with an executed test), and the    ║
║    loop closed on that commit, not on a subsequent empty round.       ║
║  Rounds: R1 5 · R2 7 (3 medium, 4 low) · R3 4 (4 low) · R4 CLEAN ·    ║
║    R5 2 (1 medium, 1 low) → remediated                                ║
║  Gates at close: all deterministic gates green — see "State at close" ║
║  Live services: none used, none needed.                               ║
║  Opened: 2026-10-06                                                    ║
╚═══════════════════════════════════════════════════════════════════════╝
```

**Task:** issue #2 — every score-ordered list in the system broke an exact tie on the
raw `NodeId`. Ids are minted per run (`Uuid::new_v4()`), so such a tie was deterministic
*inside* a run and arbitrary *across* runs: the same session recalled in a different
order, synonym duplicates surfacing by coin flip seeded at process start.

**Branch reviewed:** `task/issue-2-stable-tiebreak`, five commits over `main`:

| Commit | Role |
|---|---|
| `faaf017` | the change itself: the tie chain, its sites, its residuals |
| `7e80219` | R1 remediation — total tie-break chains |
| `fafb424` | R2 remediation — chains the round-1 sweep missed |
| `5333fb4` | R3 remediation — last doc residuals and the web event cursor |
| `bd77160` | R5 (final) remediation — the drift output contract and goal attribution |

Diff at close: 19 files, +1340/−355 (`git diff main...HEAD --stat`), this record
excluded.

**Why the change looks the way it does:** the decision of record is that the tie chain
everywhere is **score first, then the concept's `canonical_key` ascending, then
`NodeId` ascending as the final fallback that keeps the order total**, stated once in
`types::tie_break_by_key` (`src/types/mod.rs:903`). Three properties forced the shape:

- *The key is not a total separator.* Non-canonical synonym duplicates share one key,
  and interactions carry none — so the id fallback stays, but only *inside* each
  presence class.
- *Key presence is itself compared.* The obvious comparator (compare keys when both
  are `Some`, fall through to the id otherwise) is **intransitive** on mixed input:
  with `a=(Some("b"), id1)`, `b=(None, id2)`, `c=(Some("a"), id3)` it yields `a<b`,
  `b<c`, `a>c`, and sort results depended on input order. R1 made the comparator
  total — a keyless node sorts after every keyed node (SQL `NULLS LAST` on ascending)
  — and pinned it with a six-permutation transitivity test
  (`types::tests::tie_break_is_transitive_on_mixed_key_presence`, which failed against
  the old form).
- *Where the concept is not in scope, the residual is named, not hidden.* The async
  gather phase of hybrid holds no graph, so the pick moved to where the graph lives
  rather than forcing keys into gather. Interactions carry no key at all, so the
  recent-interactions leg keeps its bare-id tie and says so
  (`src/recall/candidates.rs:335-341`).

**Method:** five rounds of adversarial review, each followed by remediation. Every
finding below was re-derived by the remediating agent against the code — reproduced,
bite-tested by temporarily reverting the fix where a test could discriminate, or
refuted with an executed check — before its resolution was recorded. The round
summaries name what each round did *not* run (full suite, fmt, clippy were mostly
carried on the orchestrator's word; R5 re-ran the full `cargo test` itself), and this
record preserves those caveats rather than laundering them.

---

## The audited site table

State at close. "Chain" is what orders the site's ties; every row is either on the
canonical-key chain or a named, deliberate residual.

| Site | Chain at close | Round | Note |
|---|---|---|---|
| `types::tie_break_by_key` (`src/types/mod.rs:903`) | the rule, stated once | R1 | presence-partitioned, then key bytes, then id; transitivity test pins it |
| `recall::candidates::rank` (`src/recall/candidates.rs:275`) | score → key → id | R0 | hot-path: key lookups sit inside `then_with` tie arms only |
| `recall::candidates` recent-interactions leg (`candidates.rs:335-341`) | bare id, **accepted residual** | R0 | interactions carry no canonical key; documented at site |
| `recall::assemble` member ordering (`src/recall/assemble.rs:127`) | score → key → id | R0 | `final_score_ties_break_by_canonical_key_ahead_of_node_id` pins the chain; assert message corrected in R2 |
| `graph::index::InvertedIndex::search` (`src/graph/index.rs:124`) | score → key → id | R0 | keys ride on the index as `doc_keys`; `remove` now drops the entry too (R1) so the map mirrors the postings' lifecycle |
| `daemon::score::rescore` (`src/daemon/score.rs:292`) | score → key → id | R0, R2 | `partial_cmp().unwrap_or(Equal)` → `total_cmp` (R2) closed a NaN intransitivity hole; unreachable from today's producers, not bite-tested — no producer makes a NaN |
| `recall::dispatch` SG dependents (`src/recall/dispatch.rs`) | count desc → key → id | R0, R2 | count-only sort leaned on per-process `HashMap` order (revert flaked 5 of 8 runs); counts now precomputed |
| `recall::dispatch::resolve_anchor` (`src/recall/dispatch.rs:107`) | type → len → content → key → id | R2 | sibling `inspect.rs::resolve_focus` got the chain in R0; this one was missed |
| `canon::eval` stage-3 window and budget demotion (`src/canon/eval.rs`) | score → key → id | R0 | keys threaded through `CyclePlan` because verdicts holds no graph; module bullets corrected in R2 |
| `cli::saints` blast-radius comparators (`src/cli/saints.rs`, `src/memory.rs`) | radius → created_at → key → id | R0 | `CanonicalMemory` gained a `canonical_key` field (not `Serialize` — no API shape change); comment fixed in R3 |
| `cli::inspect` focus legs (`src/cli/inspect.rs`) | score → key → id | R0 | |
| `graph::hybrid` merge pick (`src/graph/hybrid.rs:215` `top_tier`, commit phase) | score tier → validated Concept → key → id | R1 | `faaf017` had left `best_candidate` id-only; the pick moved to the commit phase where the `Graph` lives; a tier with any non-Concept refuses the merge outright |
| `daemon::drift` goal attribution (`root_goal_nodes` `drift.rs:137`, `drift_at` `drift.rs:228`) | key → id (seed order) | R1 | governs `DriftHit::goal` and `drift_at`; hop distances are seed-order-independent; stability across id mintings proven in R5 remediation |
| `daemon::drift` `detect()` output (`drift.rs:213`) | node id — **documented presentation contract** | R5 | hits carry no score, so there is no tie to break; named as an accepted residual like `recent_concepts` (bd77160) |
| `daemon::events` `session_last_activity` (`src/daemon/events.rs:294`) | instant → key → id | R2 | strict-less min under the total order, so fold iteration order cannot leak; same-instant ties are routine (a live c2/c3 tie exists in the suite) |
| `daemon::hotlist` comparator (`src/daemon/hotlist.rs`) | seq (already total); id arm named unreachable | R2 | doc fix: no two live entries share a `seq`, the id arm is a formality kept for totality |
| `store::memory` keyword candidates | key → id | R0 | |
| `store::pg` keyword candidates, vector fetch (`order_candidates` `pg/mod.rs:933`) | key → id (Rust re-order) | R0 | both vector SELECTs gained `canonical_key`; the B0 byte-identical SQL pin re-pinned by hand with that one column |
| `store::pg` `has_boundary_tie` (`pg/mod.rs:969`) | forcing rule unchanged; doc states the **bounded** guarantee | R1 | still forces the exact session query, which is authoritative — `ORDER BY dist ASC, id ASC LIMIT $3` (`pg/mod.rs:590`) is cut by the run-minted id when a tie group outgrows its LIMIT |
| `store::pg::cockroach` (`src/store/pg/cockroach.rs`) | key → id | R0 | untouched by the comparator change — its `order_candidates` copy passes `Some()` keys; clippy under `store-cockroach` clean |
| `store::sqlite` (`rank_by_cosine` `sqlite.rs:1602`) | key → id, ordered in SQL | R0 | `BINARY` collation makes SQL byte order equal to Rust `str` order; `canonical_key` is `NOT NULL` in all three schemas, so the SQL `ASC` can never hit the Rust keyless-sorts-last arm |
| `cli::serve_web::events_from` (`src/cli/serve_web.rs:728`) | occurred_at → key → id | R3 | behavioral: the seq cursor was per-run arbitrary over eval's same-instant events; id residual confined to events whose node is absent from the snapshot |

Swept and cleared across rounds: all 70 id-ordering grep hits in `src/` re-classified
by R3 (demo and web render sorts are content-keyed; cli test-module hits; the eval
ring; the documented id-primary output sorts in `conflict.rs` and `events.rs` accepted
in R1's scope); a stale-phrase grep (`smallest id wins` / `tie by id` / `ties by id`)
returns zero hits in `src/` at close; the store audit reads (`ORDER BY occurred_at,
id`) chased through their consumers — the one order-sensitive consumer (the web feed's
cursor) re-sorts in Rust, snapshot replay applies each event to its own node, and the
demo audit trail re-sorts by content.

---

## Round 1 — 5 findings, all fixed (`7e80219`)

The reviewer's verdict message was dropped by the message channel three times
(escalation `dwfq-34436e38-1`); the five findings were carried verbatim from that
escalation, and the operator cancelled the re-derivation review of the unchanged
commit rather than re-run it.

- **`src/types/mod.rs:898-910` — the id-fallback comparator was intransitive.**
  Reproduced by reading and by a revert test (the triple above). Fix: key presence is
  part of the order (keyless after keyed, `NULLS LAST`), then key bytes, then id
  inside each class; the old doc's "it keeps the order total" claim was the false
  part and was rewritten. All store call sites pass `Some()`, so only the defensive
  graph-lookup closures can observe the `None` arm.
- **`src/graph/index.rs` — `remove()` leaked the `doc_keys` tie-break entry.** With a
  correction to the finding: postings were already removed on the remove path; the
  stale artifact was the tie-break key (a re-added id would overwrite it, so search
  never consulted a stale key — but the map outlived its postings). Fix:
  `InvertedIndex::remove` drops the entry; `remove_drops_the_tie_break_key` pins it.
- **`src/graph/hybrid.rs` — `best_candidate` picked by raw id.** `faaf017` had
  explicitly left it id-only because the async gather phase holds no graph; that
  constraint is real, so the fix moved the pick rather than forcing keys into gather:
  `best_candidate` became `top_tier` (the whole tier tied at the top valid score),
  `HybridMerge` carries targets, and the commit phase — where the `Graph` lives —
  validates every member as a Concept and picks by key then id. A tier containing a
  bogus non-Concept refuses the merge outright, now independent of which member
  run-minted ids would have favoured.
  `tied_merge_candidates_pick_smallest_canonical_key` seeds ids opposite to keys.
- **`src/daemon/drift.rs` — the raw-id tie governed goal attribution.** It also
  governs `detect()`'s goal attribution via `root_goal_nodes`' seed order, and
  `drift_at`'s doc promises the two agree, so both moved together: seeds sort by key
  then id, `drift_at`'s frontier pick is a min_by over the same chain. This overrides
  `faaf017`'s swept-and-left note. Hop distances are seed-order-independent, so only
  `DriftHit.goal` changes.
- **`src/store/pg/mod.rs` — the exact-query doc overclaimed.** "The exact session
  query remains the only deterministic answer" was false twice over: its SQL is
  `ORDER BY dist ASC, id ASC LIMIT $3`, so a tie group outgrowing its own LIMIT is
  still cut by the run-minted id, and issue #2 changed the exact query too. The doc
  now states the bounded guarantee: `has_boundary_tie` still forces the exact query
  (forcing rule unchanged), which is authoritative — not unconditionally
  deterministic.

## Round 2 — 7 findings (3 medium, 4 low), all fixed (`fafb424`)

The reviewer verified rather than trusted: read the full `git diff main` (16 files),
re-derived the transitivity claim with a standalone `rustc` replication of the old
comparator in `/tmp` (4 of 6 permutations sort differently — the R1 test
discriminates), and ran the new tests itself. Cleared along the way: hot-path claims
(lookups inside `then_with` tie arms only), scope claims (no `src/embed/**`, no deps),
schema safety (`canonical_key` `NOT NULL` in all three migrations, `BINARY` collation
in sqlite), goldens set-compared, `binary_parity` pins no ordering.

- **(medium) `src/canon/eval.rs:15-16,19-20`** — module-head bullets still promised
  the pre-issue-2 NodeId tie-break the code no longer implements. Doc-only; behavior
  already correct.
- **(medium) `src/daemon/events.rs:299`** — `session_last_activity` anchored the
  stale-session event on run-minted ids among same-instant ties. Proven routine: the
  existing `detect_stale_is_a_session_property_not_a_per_concept_one` is a live
  same-instant tie. Fix: the `at == *t` arm orders by `tie_break_by_key`; the existing
  test's anchor moved `c2` → `c3` and was updated honestly;
  `stale_anchor_ties_break_on_canonical_key_ahead_of_id` added and revert-tested.
- **(medium) `src/recall/dispatch.rs:117-121`** — `resolve_anchor`'s rank ended in the
  raw id while its sibling `inspect.rs` got the full chain in R1. Fix: `(type, len,
  content, key, id)`. The new test's first fixture draft was itself wrong (`beta` is 4
  bytes, so the length step legitimately decided) — caught by a stage-instrumented
  probe before commit; no production change resulted from the detour.
- **(medium) `src/recall/dispatch.rs:143-151`** — the SG prose-anchor sort was
  count-only over a `HashMap` iteration, never run-stable. Fix: per-SG dependent
  counts precomputed into `(count, id)` pairs, then key chain. Bite check: under a
  revert, the new test failed 5 of 8 consecutive process runs — direct empirical proof
  of the cross-process nondeterminism.
- **(low) `src/daemon/hotlist.rs:232`** — the id arm presented itself as a live
  tie-break; `seq` is unique among live entries, so it never decides. Arm kept for
  totality, comment now says it is an unreachable formality.
- **(low) `src/daemon/score.rs:300-303`** — `partial_cmp().unwrap_or(Equal)` left a
  NaN cycle contradicting the totality doc claim (valid construction by the reviewer;
  unreachable from today's producers). Fix: `total_cmp`, matching the other sites.
  Not NaN-bite-tested — no producer makes a NaN; stated as not run.
- **(low) `src/recall/assemble.rs:540`** — assert message named the pre-issue-2
  contract; the six planted finals are distinct so no tie arm fires. Reworded to point
  at the real chain test.

## Round 3 — 4 findings (all low), all fixed (`5333fb4`)

No behavioral defect in the R2 remediation itself — all seven fixes correct, in scope,
tests discriminating. R3 re-classified all 70 id-ordering grep hits and swept for
stale "tie … id" phrases; what surfaced was doc-level residue plus one pre-existing
site with the same shape R2 fixed.

- **`src/daemon/events.rs:1051`** — the golden-fixture comment taught the old
  smallest-id rule; keys and ids agree in that fixture, so only the comment was wrong.
  Reworded, pointing at the discriminating `stale_anchor` test. Comment-only.
- **`src/graph/index.rs:430`** — the golden assertion's parenthetical said "tie by id"
  while `search` has ordered ties by key then id since R1. Comment-only; the assertion
  was already green under the real chain because the fixture's tie groups order
  identically under both.
- **`src/cli/saints.rs:190-193`** — the comment omitted the canonical-key step R1
  inserted between `created_at` and id in both comparators. Comment-only.
- **`src/cli/serve_web.rs:734`** — behavioral, taking the fixHint's stronger option:
  `events_from` ordered the event feed by `(occurred_at, id)` while eval stamps every
  event in a cycle with the same `now` and mints ids per run, so the seq cursor was
  per-run arbitrary — the same class R2 fixed at `session_last_activity`. `events_from`
  now builds a canonical-key map beside the content map and orders
  `(occurred_at, key, id)`; the lookup runs only on exact ties and the id residual is
  confined to events whose node is absent from the snapshot, both stated in the fn
  doc. `canon_event_feed_ties_break_on_node_canonical_key_ahead_of_id` added and
  revert-tested.

## Round 4 — CLEAN

Full cold pass over the accumulated branch (`git show 5333fb4` plus tree state; 10
chain tests re-run green). All four R3 findings confirmed remediated, the web event
feed fix verified sound (comparator total, lookups inside `then_with`, the new test
discriminating by construction), and the re-sweeps — stale-phrase grep, the
`drift.rs:61` seed-order note, `candidates.rs:776` accepted residual, the store audit
reads chased through their consumers — found nothing new. Zero findings, zero
resolutions. This is the round the earlier stopping rule wanted.

## Round 5 — final pass, 2 findings, both remediated (`bd77160`)

A final weighted pass over the whole diff (`git diff main`, 19 files, 4 commits at
that point) found the workstream's own bar — "every verified tie-break site stable" —
unmet in one file, plus one doc overclaim. It also re-ran, itself: full `cargo test`
(960 lib + integration + doc tests, 0 failed), `cargo fmt --check`, clippy (default
features), and one mechanical revert bite-test in a throwaway clone (deleted after).
Not run by that pass: clippy with `store-cockroach`, live pg/cockroach tests,
`binary_parity` (0 tests selected under default features).

- **(medium) `daemon::drift::detect()` output sorts on the raw id** (`hits.sort_by_key`
  at `drift.rs:213`) with no residual note, while
  `hits_are_deterministic_and_id_sorted` actively pinned id order as the contract —
  a missed sweep site rather than a recorded decision, 40 lines from the goal
  attribution the branch did fix. **Resolution (bd77160): the sort stays** — hits
  carry no score, so there is no tie here to break — and is now named as a
  presentation contract at the sort site, in the module determinism bullet (mirroring
  `recall::candidates::recent_concepts`), and in the test, renamed
  `hits_are_deterministic_in_the_documented_id_order`, which states it pins the
  within-run order, not cross-run stability.
- **(low) the goal-attribution doc dropped the "in practice" hedge** the reviewer
  believed the BFS mechanism still needed for >1-hop ties (same-level ordering rests
  on id-ascending neighbor lists). **Resolution (bd77160): refuted with an executed
  check.** The FIFO queue with the stable canonical-key seed order keeps each seed's
  BFS family contiguous at every level, so minted ids only reorder nodes inside one
  family and never decide a same-level race between families.
  `goal_attribution_is_stable_across_id_mintings` builds the exact corner twice — a
  deep node 2 hops from both goals via one shared discoverer, and one via two
  same-level discoverers from different families — under two opposite id mintings and
  asserts the smaller-key goal wins in both. The module bullet now states that
  stability with its mechanism; the hedge would have documented a flip that cannot
  happen.

## Final verdict

The verdict of record at review close was `clean=false` on the two findings above —
against the ask's bar, `drift.rs:213` was an in-scope site the four prior rounds did
not sweep. Both findings are remediated in `bd77160` (one by naming the contract, one
by refutation), and the loop closed there. As in the C3 close, the stopping rule
"review until a round returns empty" was not satisfied by an empty round: it was
satisfied by the last round's findings being remediated without a sixth pass. The
decay across rounds supports that: 5 behavioral → 7 (3 behavioral, 4 doc/test) →
4 doc-level + 1 behavioral → 0 → 2 (1 contract-naming, 1 refuted-by-test), and the
R5 pair touched one file.

What the review verified rather than assumed, cumulatively: the comparator of record
is total and its transitivity test discriminates (standalone replication of the old
comparator); every new tie test seeds key order opposite to id order by construction,
and at least three were revert-bite-tested (index via simulated pick, session anchor,
SG anchor with a measured 5-of-8 flake rate); the chains hold where the concept is in
scope and name their residuals where not; doc contracts match code at all named
sites; `canonical_key` is `NOT NULL` in all three schemas so the SQL `ASC` orderings
cannot hit the Rust keyless-sorts-last arm; goldens are set-compared and
`binary_parity` pins no ordering; no `src/embed/**` file, no `Cargo.toml`/deps, and
`types/mod.rs` gains only the comparator plus tests.

## State at close

Carried from the closing state (all deterministic gates green); the final remediation
commit records its own row: "fmt clean; full suite green (961 lib + integration + doc
tests); clippy clean with and without store-cockroach." R5 independently re-ran the
full suite, fmt and default-feature clippy before its findings; the earlier rounds'
full-suite claims were carried on the orchestrator's word, which R5's re-run then
subsumed. Not run by any round: live pg/cockroach integration,
`binary_parity` under default features (0 tests selected there).

## Residual risks

Deliberate, each named at its site rather than in a private decision:

- **Id-primary presentation contracts are per-run arbitrary across fresh runs.** Drift
  `detect()` output, conflict and high-risk output lists, the hotlist formality arm.
  Documented as presentation, not stability; anything consuming them as
  cross-run-stable order will observe reordering.
- **The recent-interactions leg ties on bare id** (`candidates.rs:335-341`) because
  interactions carry no canonical key. If interactions ever gain a stable key, this is
  the next site.
- **`has_boundary_tie`'s guarantee is bounded.** The exact session query is forced
  when a tie group may straddle the LIMIT, and it is authoritative — but its own SQL
  ends `dist ASC, id ASC`, so a tie group outgrowing the LIMIT is cut by the
  run-minted id. The doc states this; it is not fixed, because the key cannot recover
  rows a LIMIT never returned.
- **`events_from`'s id residual** for events whose node is absent from the snapshot —
  the only way the key lookup misses.
- **The rescore NaN path is closed by construction (`total_cmp`) but was never
  bite-tested** — no producer makes a NaN (`score()` ends in `finite_or_zero`). A
  future producer that can would be running through untested territory.
- **Not executed by any round:** live pg/cockroach integration and `binary_parity`
  under default features. The store-side chains there are verified by the SQL pins
  (`b0` byte-identical re-pinned by hand) and reading, not by a live run.

## If this code is revisited

- **The drift `detect()` presentation contract is the pattern to copy, not to
  "fix".** A list with no score has no tie to break; converting it to a key chain
  would be motion. Name the contract, pin the within-run order, move on.
- **`RESOLVE`-style completeness does not exist for id-ordering sweeps.** Three of
  five rounds' findings were sites a previous round's sweep had itself just missed
  (`resolve_anchor` after `inspect`, the drift output after drift attribution). A
  grep for `sort_by.*\.0\b` plus a stale-phrase sweep (`tie by id`, `smallest id`)
  is the closest thing to a check; neither proves a negative.
