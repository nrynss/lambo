# #36 PR 5: the read-only canon dry run and the alias preview

Recorded 2026-10-13 for 0.6.0. Design of record:
[design-36-canonization.md](design-36-canonization.md), section 4, PR 5
("Read-only canon dry-run/alias preview"), "Rollout is an operator action"
and "Acceptance before declaring #36 fixed". The PR 1 to PR 4 notes
([dependents](feature-36-pr1-dependents.md),
[denominator](feature-36-pr2-denominator.md),
[admission](feature-36-pr3-admission.md),
[citations](feature-36-pr4-citations.md)) are the contracts this reads.
Branch `feat/36-pr5-dry-run` from `origin/main` e333bc7c.

This is the gate PR 3's note set: **do not re-pin the rig to a build with
PR 3 until this dry run has been reviewed.** The figures the owner needs
are under "Rig dry run" below.

## What it adds

- `lambo canon dry-run --store <file> --session <id>` and
  `lambo canon alias-preview --store <file> --session <id> [--map <file>]`,
  with `--policy swarm|solo|both`, `--at`, `--json`, `--quiet`,
  `--max-cycles`, `--top`, `--assume-survived`, `--scratch`, `--watch <id>`
  (repeatable) and, for the dry run, `--baseline <earlier.json>`.
- Library: `canon::dry_run` (the engine, the report types, `compare`,
  `render_report`, `resolve_watch`), `canon::alias` (detection, the map,
  the plan, the overlay), `canon::alias_preview`, `canon::dry_run_defaults`,
  `cli::canon`.
- `examples/canon36_admission.rs` is removed: the dry run reproduces its
  rig figures exactly (below) and reports more. `canon36_stage2_cap.rs`
  stays; it answers a different question (raw against capped denominator).
- `fixtures/canon-dry-run-fair-test.txt`: the fixture's text report, pinned.

## Decisions

### The command: `lambo canon <action>`

A subcommand group, like `lambo recall-index backfill`. Both actions are
operator tools that take a store file, not `lambo.toml`, and build no
backend; the top level holds the agent-facing read and write verbs, and a
group leaves room for later canon tools without adding top-level names.
`dry-run` and `alias-preview` say what each does and that neither applies
anything. `main` dispatches `canon` before the backend resolve, so no
config is read (the binary test points `LAMBO_CONFIG` at a missing file).

### A private copy, read-only, and an in-memory overlay

The design asks for a snapshot or overlay that works on a store copy and
never writes. The command does three things, each a guard:

1. **It never opens the input with SQLite.** The file (and its `-wal`, when
   present) is hashed and copied byte for byte into a fresh 0700 scratch
   directory (`--scratch` or the temp directory), and the copy's hash must
   equal the input's, or the run is refused as a live writer's file. Not
   opening the input means not even a `-shm` appears beside it.
2. **Only the copy is opened, read-only**, and only to `load_session` and
   `read_lease`. The evaluator then runs on an overlay: a `Graph` from the
   snapshot and a fresh `MemoryStore` filled by an unfenced `flush`.
3. **Afterwards** the scratch directory is removed, the input is hashed
   again, and the `source_unchanged` invariant records whether the hashes
   and the set of sidecars agree.

**Copied leases.** The copy's lease row is read and reported (present,
fencing token, unexpired at the clock), never taken, renewed or stolen.
The overlay store is fresh, so it has never been leased and its writes need
no token. This is the clean form of what the design's harness did by
deleting lease rows from disposable copies; nothing is deleted here.
Unconsumed write intents are counted and not replayed (a writer would
replay them; the report says so).

**SQLite only.** `--store` names a SQLite file. A Postgres or Cockroach
session can be dry-run from a SQLite copy of it; reading a live server
directly would make "never writes, works on a copy" a promise about
someone else's database. Recorded as an open question.

### The report: ids, labels and counts

Per policy: the start by status and type; the **first evaluation,
unbatched** (every concept's next hop through `canon::explain_admission`,
the evaluator's own predicates, with eligible counts by hop and type and
unmet counts per `hop stage/gate`); the run (cycles, stop reason, hops,
first-cycle hops); **new Candidates, Venerables and Canonicals** (concepts
that reached each status during the run, each with its Resource split and
by type); budget demotions; the end census; the **fixed point** explained
the same way; the **concepts one gate away**; the concepts with more than
five dependents that are not Canonical and which gates stop them; the
Canonicals, most dependents first; the **watched** concepts; and the
invariants.

"One gate away" is a next hop that fails exactly one gate, **for a concept
with at least one dependent (or a Venerable)**. The first version counted
every concept failing one gate, and the fixture's golden showed the list
swamped by concepts with no dependents at all failing only the dependents
floor: true, and useless to the owner.

The invariants: `fixed_point` (stopped by the quiet rule, not the cycle
limit), `one_legal_hop_per_cycle`, `canonical_within_budget`,
`promoted_canonicals_hold_stage2_and_stage3` (re-measured at the stop),
`nothing_eligible_at_the_stop` (the evaluator and the explanation agree
that the fixed point is one), `graph_invariants`, and, from the CLI,
`source_unchanged`; the alias preview adds `only_alias_citations_move`,
`plan_is_idempotent` and `overlaid_runs_hold_their_invariants`. A failed
invariant exits 1 after the report is printed.

**Baseline comparison.** The start census against the fixed point is in
every report; `--baseline` compares with an earlier `--json` report (new
and end totals before and after, Canonical ids added or removed,
invariants newly failing), so each re-pin's run can be diffed against the
last. The alias preview's comparison is its two runs.

### The stop is deterministic, and waits out a cooldown

A run stops after `max(--quiet, ceil(max(Candidates, Venerables) / batch)
+ 3)` consecutive empty cycles (the design's "ring/batch + 3"), and empty
cycles are counted **only while no concept is inside its re-promotion
cooldown**. The first version added the cooldown's remaining cycles to the
quiet count instead; the cooldown test caught it: the remainder shrinks as
the clock advances while the idle count grows, so the run stopped halfway
through the cooldown with the ruling still waiting at Venerable. Every
input is the graph and the injected clock, so two runs over one copy stop
at the same cycle with equal reports (tested for a full budget and for a
running cooldown). Reaching `--max-cycles` fails `fixed_point`.

### `--watch`

The rollout note wants each re-pin to record the nominated invariants'
actual gate failures, and the aggregate lists filter and cap what they
show. `--watch <id>` (full or 8-hex, unique prefix) reports those concepts
at the start and the stop whatever their state, with every unmet gate,
including the Stage 2 gates that stop a concept whose next hop is still
Stage 1.

### The alias preview: aliases are expressed as citation edges

Names were removed in #147, so a redirect cannot be a synonym. It is
expressed in **citation edges**: each `citer -Dependency-> alias` becomes
`citer -Dependency-> ruling`, the edge a citation by id writes, **keeping
the original edge's times** (the citation was made then, so Stage 2's
spread stays honest), and the alias edge is retired. Rejected:

- **`alias -Dependency-> ruling`** (the alias cites its ruling). The alias
  keeps its three dependents and still reaches Venerable, so the fixture
  defect does not flip, and the ruling gains a dependent from the alias's
  own origin interaction, a citation nobody made.
- **Keeping the alias edges and adding the ruling's** (both). Same
  problem: the name keeps climbing on support meant for the ruling.
- **A read-time "forward an alias's dependents" rule.** A semantic change
  to PR 1's relation in every adapter's SQL, for a history problem.

Applying a reviewed redirect later would be exactly the plan's
`DeleteEdge`/`UpsertEdge` mutations; this PR does not apply anything and
adds no apply path (that is the owner's rollout decision).

**Detection** is the design's leading-phrase rule: an Entity that at least
one concept cites, whose normalized tokens (ASCII punctuation other than
`-` and `_` made spaces, then the canonical normalizer's case, stopword and
stem handling) are a strict prefix of exactly one Logic or Constraint
concept's. More than one ruling is **refused as ambiguous**, never picked.
**A reviewed map** (`--map`: one `alias-ref ruling-ref` per line, full or
8-hex ids, `#` comments) replaces detection, so the owner previews only
the pairs they would sign. A map reference by content is refused (the map
then holds no concept text), as are malformed lines, unresolved or shared
short ids and interaction ids. Pairs that chain (`a -> b`, `b -> c`) or
loop are refused, like the direct-lookup-only synonyms they replace; so is
a self pair. Per citation: a citer that already cites the ruling has its
alias edge retired and nothing added (idempotent); the ruling citing its
own alias is retired, never written as a self-loop; a move that would
close a `Causal`/`Dependency` cycle (spec §5.7) is left on the alias and
counted. The alias's `parent_of` children are left and counted. Planning
again on the result changes nothing (`plan_is_idempotent`).

**The fixture defect flips.** With the reviewed map `lease_alias ->
lease_stays`, the three citations move, `lease_alias` ends at None instead
of Venerable under both policies, `lease_stays` stays Canonical with nine
dependents, and nothing else changes. Detection proposes nothing on the
fixture: "writer lease" is not the opening phrase of any ruling ("a writer
keeps its lease ..."), which is why the map exists. The fixture's expected
row stays Venerable: the fixture is history, and nothing was applied.

## Tests

Design list:

| Requirement | Test |
|---|---|
| Input database unchanged (hash) | `the_input_is_unchanged_its_lease_untouched_and_the_copy_removed` (file with a live lease: hash, sidecars, lease row and canonization events unchanged; scratch emptied), `a_store_with_an_uncheckpointed_wal_is_read_in_full` (WAL backup with its data in the `-wal`), `the_alias_preview_reads_a_map_and_flips_the_alias_on_the_copy`, `the_snapshot_is_never_modified`, and both binary tests |
| Independent oracle parity | `the_dry_run_agrees_with_an_independent_oracle`: every concept's status equals the evaluator's own fixed-point loop (`fair_test::fixed_point`) on a real SQLite store instead of the overlay's MemoryStore, under both policies; Swarm's Stage 1 eligible count equals a recount from the raw edges (survival, dependents from two other interactions). On the rig, the dry run reproduces PR 3's example figures exactly |
| Deterministic stopping for budget and cooldown | `a_full_budget_stops_deterministically` (budget 2: two runs equal, two Canonicals, the other two named one gate away on `canonical_budget`), `a_running_cooldown_is_waited_out_before_the_run_stops` (a 3,600 s cooldown with a 3-cycle quiet floor: the ruling still reaches Canonical; fails if empty cycles count during a cooldown), `the_cycle_limit_fails_the_fixed_point_invariant` |
| Isolated ports and runtime paths | `tests/cli_canon_dry_run.rs`: the binary with a fresh `XDG_RUNTIME_DIR` and `TMPDIR` (its default scratch parent), both empty after it exits, `LAMBO_CONFIG` pointing nowhere; nothing beside the input. The command builds no server, so it binds nothing |
| Fixture and aggregate regression | `the_dry_run_reaches_the_fair_test_fixed_point_under_both_policies` (`fair_test::assert_expected`, four Canonical rulings, no Resource), `the_fixture_report_is_pinned` (`fixtures/canon-dry-run-fair-test.txt`; regenerate with `LAMBO_UPDATE_GOLDEN=1 cargo test --features store-memory,fixtures --lib canon::dry_run::tests::the_fixture_report_is_pinned`), `a_report_compares_with_its_baseline` |
| Never persist proposals | The overlay is dropped; the source is never opened writable; the input's canonization events are re-read and empty after the run |
| Backfill: ambiguity, cycles, idempotency | `the_leading_phrase_rule_pairs_one_ruling_and_refuses_two`, `ambiguous_chained_cyclic_and_self_pairs_are_refused_and_nothing_moves`, `each_citation_is_moved_kept_or_dropped_on_its_own_merits`, `a_second_plan_on_the_result_is_empty`, `apply_leaves_its_input_alone_and_is_deterministic`, `a_map_names_concepts_by_id_only`, `other_dependents_of_an_alias_are_counted_and_kept`, `the_alias_preview_refuses_and_applies_nothing_for_a_bad_map` |
| The `lease_alias` defect flips | `the_alias_preview_flips_the_fixture_alias`, `a_reviewed_map_moves_the_fixture_alias_citations_to_the_ruling`, the CLI and binary alias tests |
| No concept text | `reports_carry_ids_and_labels_never_concept_text` (text and JSON of both commands, every fixture content and prompt) |

Also `the_leading_phrase_rule_proposes_nothing_on_the_fixture`,
`watched_concepts_are_reported_by_id_at_the_start_and_the_stop`,
`bad_inputs_are_usage_errors_and_leave_no_copy`,
`canon_actions_parse_and_need_no_backend`.

**Mutation checks** (each restored from a copy): counting empty cycles
during a cooldown fails the cooldown test; dropping the cycle check from
the plan fails `each_citation_is_moved_kept_or_dropped_on_its_own_merits`.

No private store enters CI: the tests use the committed fixture only.

## Gate (local)

Each command with a fresh `XDG_RUNTIME_DIR`: `cargo fmt --check`; `cargo
clippy --all-targets -- -D warnings` on `store-sqlite,fixtures`,
`ship,fixtures`, `--no-default-features store-postgres`,
`store-postgres,store-sqlite,fixtures` (with and without default
features), `embed-eg2`, `recall-elastic` and `embed-candle`; `cargo test
--features store-sqlite,fixtures` (2,350 passed, 0 failed, 7 ignored over
all targets; the library 2,267 / 0 / 5; main's counts are these less the
28 new tests); `cargo test --no-run
--no-default-features --features store-sqlite`; `cargo check
--no-default-features`; `cargo check --features demo`;
`scripts/docs/check-mirror-drift.sh`. All pass. New: 25 library tests, 1
binary-crate test, 2 integration tests; no existing test changed or was
removed. No `Cargo.toml`, `Cargo.lock`, workflow or
`src/store/pg/postgres/tests.rs` change; no store SQL changed.

## Rig dry run (aggregates only)

A fresh copy of `rig-backup-2026-10-12-0726.db` in the lane's scratch
directory (SHA-256 `1c9d3f5d…c43d`, the same file PR 3 measured; identical
before and after every run, the backup's own hash unchanged, the copy
deleted afterwards), session `lambo-dev`: 5,005 concepts, 1,786
interactions, 12,195 edges, evaluated at `--at 2026-10-12T12:00:00Z` with
the release build. The copied lease row is present (token 3) and expired
at that clock; no unconsumed intents. Starting statuses: 404 Candidate
(164 Resource, 156 Constraint, 71 Logic, 13 Entity), 7 Venerable (6
Resource, 1 Entity), 0 Canonical. Every invariant held under both
policies; each run stopped at a fixed point after 35 cycles.

| | Swarm | Solo |
|---|---:|---:|
| New Candidates | **63** | **218** |
| of which Resource / other | 42 / 21 (20 Entity, 1 Constraint) | 140 / 78 (68 Entity, 8 Constraint, 2 Logic) |
| New Venerables | **2** (both Resource) | **2** (both Resource) |
| New Canonicals | **0** | **0** |
| Demoted | 0 | 0 |
| End: Candidate / Venerable / Canonical | 465 / 9 / 0 | 620 / 9 / 0 |

These equal PR 3's figures exactly (63 / 42, 218 / 140, 2, 0), measured
then with the example this command replaces. With `--assume-survived` (433
survival counts raised) every number is identical: dependence binds, not
survival.

**Where the first evaluation stops** (unmet gates, a concept counted under
each gate it fails). Swarm: None -> Candidate `dependent_interactions`
4,531, `gc_survived` 433; Candidate -> Venerable `coverage` 404,
`distinct_interactions` 397; Venerable -> Canonical `blast_radius` 7,
Stage 2 again 6 (the pre-#36 Venerables). Solo: None -> Candidate
`solo_score` 4,376; Candidate -> Venerable `solo_score` 393, `coverage`
404, `distinct_interactions` 397; Venerable -> Canonical `blast_radius` 7
and `solo_score` 7.

**One gate away at the fixed point** (concepts with dependents):

| Hop and the one gate that fails | Swarm | Solo |
|---|---:|---:|
| Candidate -> Venerable, `coverage` | 26 (13 Resource, 12 Entity, 1 Constraint) | 26 (same) |
| Candidate -> Venerable, `distinct_interactions` | 3 (Resource) | 3 (Resource) |
| Venerable -> Canonical, `blast_radius` | 3 (Resource) | none |
| None -> Candidate, `dependent_interactions` | 763 (646 Entity, 103 Resource, 9 Constraint, 5 Logic) | |
| None -> Candidate, `solo_score` | | 668 (655 Entity, 7 Resource, 3 Constraint, 3 Logic) |

**More than five dependents and not Canonical:** two Entities, `b8e32a22`
(10 dependents from 9 other interactions, coverage 0.119) and `8f3c281d`
(7 from 6, coverage 0.065), both Candidate under both policies and
stopped only by Stage 2 coverage. As PR 3 recorded, they are one spread
gate from Canonical with the load-bearing warning: a few more days of
citations would take them there. The next concepts one gate away
include six with 5 dependents (`637fc803` and `fb6019d1` Resources, `8122b15a`, `86942bea`,
`9611f8e2`, `cc170959` Entities), all stopped on coverage.

**The four nominated rulings (watched):**

| Ruling | Swarm at the stop | Solo at the stop | Stops on |
|---|---|---|---|
| lease-stays `0235df4b` | Candidate (stored), 1 dependent | Candidate | Stage 2 distinct and coverage |
| wedge `3e01bb4b` | None, 1 dependent | Candidate | Stage 1 dependents (Swarm); Stage 2 |
| cooperative identity `a0205e60` | None, 1 dependent | Candidate | Stage 1 dependents (Swarm); Stage 2 |
| async-ack `8baad4e0` | Candidate, 3 dependents from 3 interactions | Candidate | Stage 2 coverage 0.008 |

None has more than five dependents; none is close.

### Alias preview (detected pairs, nothing applied)

- **33 pairs accepted** (28 Entity -> Logic, 5 Entity -> Constraint),
  **45 citations moved** (25 names carry one citation, 4 two, 4 three); 0
  already cited, 0 self, 0 kept for a cycle; 5 `parent_of` children of
  names left in place. 45 edges retired, 45 added.
- **21 names refused as ambiguous**, opening 2 to 9 rulings each (83
  candidate rulings in all). No chain, cycle or self pair among detected
  pairs. Among the refused: `cc170959`, one of the 5-dependent Entities
  one gate away above (two candidate rulings).
- The plan invariants hold: only alias citations move, and a second plan
  on the result is empty.

Effect, with the redirects against without:

| | Swarm | Solo |
|---|---:|---:|
| New Candidates | 63 -> 65 | 218 -> 242 |
| New Venerables / Canonicals | 2 -> 2 / 0 -> 0 | 2 -> 2 / 0 -> 0 |
| Concepts whose stop status differs | 10: 4 names Candidate -> None; 6 rulings (4 Logic, 2 Constraint) None -> Candidate | 28: 2 names Candidate -> None; 26 rulings (22 Logic, 4 Constraint) None -> Candidate |

Of the nominated rulings: the **wedge name `5da57fe5`** is detected; its 3
citations move to the **wedge ruling `3e01bb4b`**, which goes None ->
Candidate under Swarm with 4 dependents from 4 interactions and stops on
coverage 0.004. The **identity name `cc5edae8`** is detected; its 2
citations move to **`a0205e60`**, which goes None -> Candidate under Swarm
(3 dependents, coverage 0.006). Lease-stays and async-ack have no
detected name. **No redirect produces a Venerable or a Canonical**, so
applying all 33 pairs would not change what the rig canonizes today; the
redirects matter for what later citations add to.

Note that a name keeps a status it already holds: canonization never
demotes for lost support (only the budget demotes), so an alias that is
already a stored Candidate or Venerable would keep it after a real
backfill. Under Solo a name stays a Candidate on its score band even with
its citations moved.

## Rollout checklist (the owner's; this PR applies nothing)

1. **Preview on a fresh copy.** Take a new backup of the writer's store,
   copy it, and run `lambo canon dry-run` and `lambo canon alias-preview`
   on the copy with `--at` pinned at or after the backup's time, `--json` saved, and `--watch` on the
   nominated rulings. Compare with the previous re-pin's report via
   `--baseline`. Every invariant must hold (exit 0), `source_unchanged`
   included.
2. **Review the ambiguities.** For each refused name, decide by hand which
   ruling, if any, it stands for; leave out any you cannot sign. Review
   every detected pair the same way: detection is a proposal, not a
   decision. Write the pairs you accept as a map of ids and re-run
   `alias-preview --map` to see exactly what those pairs change.
3. **Collect actual explicit citations.** Ask agents to cite the rulings
   they rely on by id (`depends_on` on `lambo_derive`, ids in
   `record_action`'s `depends_on`, PR 4) and let real citations
   accumulate. Do not apply the design simulation's redirect pairs or any
   fabricated citation to the writer, and never manufacture citations for a
   target.
4. **Confirm only the rulings you would sign.** Before re-pinning, read
   the dry run's new Canonicals and the concepts one gate away (and the
   load-bearing list) by id, with `lambo inspect`, and confirm that each
   would be a ruling you sign; an artifact or name there (as `b8e32a22`
   and `8f3c281d` today) is a reason to wait or to review its citations,
   not to lower a threshold. Record the four nominated rulings' gate
   failures from `--watch` with the re-pin.

## Open questions for the owner

1. **Applying a reviewed backfill.** The plan's edge changes are exactly
   `DeleteEdge` plus `UpsertEdge`; no apply path exists. Applying needs a
   writer-side operator command (under the lease, audited) and your
   decision that re-pointing history is acceptable.
2. **Stored statuses of redirected names** are kept (no demotion for lost
   support). If a backfill should also clear a name's status, that is a
   demotion rule #36 does not have.
3. **Postgres and Cockroach sessions** are dry-run from a SQLite copy only.
4. **Detection recall.** The leading-phrase rule found 33 + 21 names on
   this copy (the design counted 31 names on an older one) and misses
   names that are not an opening phrase (the fixture's "writer lease");
   the map covers those.

## Review round (2026-10-13)

Adversarial review: 2 High, 4 Medium, 8 Low. Each fixed; commits
`28c1dab3` (H1, and the plan data for M3/L4), `0450c382` (engine: M1,
M2, H2's invariant, M3, L2, L3, L4 output, L7), `49d7f23c` (M4 tests),
`6e67cbdf` (CLI: H2's exit, L1, L2, L5, L6, help for M1/L7/L8),
`f544f1bd` (CLI docs). Then `origin/main` (#144) merged cleanly; the
CHANGELOG keeps one Unreleased section with no duplicate bullet.

- **H1** The cycle check walked `Causal`/`Dependency` only, while the
  graph's invariant also refuses a cycle through `Hierarchical`: a
  ruling's `parent_of` child citing its alias was moved into
  `child -> ruling -> child` and the whole preview aborted. The check
  now walks all three; the move is kept on the alias and counted
  (`a_rulings_child_citing_its_alias_is_kept_as_a_cycle`;
  mutation-checked).
- **H2** A reviewed map whose lines were all refused exited 0. Any
  refused reviewed line now fails `every_reviewed_line_accepted`
  (exit 1); detected-pair refusals stay information (two tests, one per
  layer; mutation-checked).
- **M1** A `--at` earlier than the data silently left rows out.
  **Decision: an invariant failure (`clock_after_the_data`, exit 1), not
  a usage refusal (exit 2).** The clock is well-formed input; what is
  wrong is its relation to the data, which is only known after the
  copy is loaded, and the report should still show what was measured
  and how much was left out (`rows_after_clock`: interactions, edges,
  demotions). On the rig copy, `--at 2026-10-10T00:00Z` fails it with
  133 interactions and 739 edges left out. Help and both cli.mdx
  copies say to pin `--at` at or after the newest row.
- **M2** `both_runs_hold_their_invariants` replaces the overlaid-only
  check, and flips are computed only when both runs held every
  invariant.
- **M3** Each redirect carries the alias's stored status; the report
  prints it and counts the aliases that keep a status with no citations
  left. On the rig: 3 (all Candidate, `6a3b4ca3`, `73df9d9a`,
  `77626e89`).
- **M4** The SQLite oracle waits 200 quiet cycles; a test runs batches
  of 1 and 2 so the queues exceed a batch (the fair-test fixture is the
  largest committed one; small batches lengthen its queues without
  inventing data); and a test drives the real `CanonizationTask` with an
  injected clock and `Config::scoring`, woken cycle by cycle until 100
  cycles change nothing, and compares the fixed point.
- **L1** The run executes on its own task beside SIGINT/SIGTERM
  (`tokio::signal`, no new crate); the scratch copy is removed before
  the error returns, and a failed removal is reported
  (`scratch_removed`, the error, or stderr on drop).
- **L2** Every Canonical id is stored (`canonical_ids`) and compared; a
  baseline of another session or report version (now 2) is refused
  (exit 2) before the run.
- **L3** Graph and store invariant errors keep their ids and withhold the
  rest (`redact`); a non-id `--watch` argument is not echoed.
- **L4** Reviewed pairs whose alias is not an Entity or whose ruling is
  not a Logic/Constraint are accepted with a warning; fan-in per ruling
  is printed (none above 1 on the rig).
- **L5** The fingerprint is streamed; a hot `-journal` is refused; the
  copy is opened with `SqliteConnectOptions::filename`, so `?`/`%` in a
  path are literal (tested).
- **L6** The input is re-hashed after the copy behind a test hook (an
  input written mid-copy is refused); the scratch directory is tested
  0700; the `source_unchanged` failure reads "input changed during the
  run (a live writer?)".
- **L7** The weights are a `DryRunParams` field defaulting to
  `Config::scoring`; `lambo.toml` cannot set them, so the CLI uses the
  defaults every writer runs. Documented, with "the dry run runs no GC;
  use `--assume-survived` to see what Swarm admits once survival no
  longer binds".
- **L8** `--quiet` help: a running cooldown pauses (resets) the count,
  it does not add cycles to it.

**Rig re-run after the fixes** (fresh copy, same flags and clock, hash
unchanged, copy deleted): **no number changed.** Both policies' dry-run
reports, the `--assume-survived` run, the alias preview's 33 pairs, 45
moved citations, 21 ambiguous refusals, deltas and flips are identical,
field for field, to the figures above; the new invariants
(`clock_after_the_data`, `scratch_removed`,
`both_runs_hold_their_invariants`) all hold. The only new information is
M3's: three detected names are stored Candidates and would stay
Candidate with no citations left (rollout step 2 should decide whether
that matters).

**Gate after the review round and the merge**, each command with a fresh
`XDG_RUNTIME_DIR`: `cargo fmt --check`; clippy `-D warnings` on
`store-sqlite,fixtures`, `ship,fixtures`, `--no-default-features
store-postgres`, `store-postgres,store-sqlite,fixtures` (with and
without default features), `embed-eg2`, `recall-elastic`,
`embed-candle`; `cargo test --features store-sqlite,fixtures` (2,366
passed, 0 failed, 7 ignored; the library 2,283 / 0 / 5: the 16 new
review tests over the earlier 2,350); `cargo test --no-run
--no-default-features --features store-sqlite`; `cargo check
--no-default-features`; `cargo check --features demo`;
`scripts/docs/check-mirror-drift.sh`. All pass. No live Postgres or
Cockroach run.
