# #36 PR 1: dependents semantics and spec errata

Recorded 2026-10-12 for 0.6.0. Design of record:
[design-36-canonization.md](design-36-canonization.md), section 4, PR 1;
operator approval on [#36](https://github.com/nrynss/lambo/issues/36)
(2026-10-08). This note is the errata entry the frozen spec
`lambo-hackathon-spec-v0.1.md` needs; it does not edit the spec.

## The relation

One relation, `dependents(X)`, now answers every "what rests on X" question:

- the **sources of `Dependency` edges into X**: `record_action` writes
  `action -> depends_on`, so an action is a dependent of what it depends on;
- the **targets of X's `Hierarchical` edges**: X's `parent_of` children.

Nothing else counts. `Causal` (`action -> produced or modified artifact`) is
provenance and confers no load. `Derives` and `Temporal` are provenance,
`CoOccurrence` and `Semantic` are association. A self-loop is not a
dependent, a dependent must be a concept of the queried session, and an edge
counts only once it is older than the age floor (its about-time: event time,
else flush time).

The table lives in one place, `EdgeType::dependent_end` (with
`Edge::dependence` on top). Its match is exhaustive, so a new edge type has to
pick an arm before anything compiles. #19's `Contradicts` must answer `None`;
`edge_type_dependent_end_table` in `src/types/mod.rs` is the test to extend.

It is a **read-time reinterpretation**: no stored edge, column or migration
changes. `record_action` keeps writing edges in the same direction it always
did.

## Who reads it

| Reader | Where |
|---|---|
| Stage 3 blast radius | `GraphStore::blast_radius`: `MemoryStore::aged_dependents`, SQLite `DEPENDENTS_CTE`/`BLAST_RADIUS_SQL` (`src/store/sqlite/structural.rs`), Postgres/Cockroach `BLAST_RADIUS_SQL` (`src/store/pg/sql.rs`) |
| Stage 2 support | `GraphStore::interaction_span`, same dependents CTE in every adapter |
| Budget demotion (lowest blast first) | `src/canon/eval.rs`, through `store.blast_radius` |
| Recall ⚑ warning, saints, retract headline | `recall::format::{dependents_of, blast_radius, blast_radii}` |
| Structural dispatch ("what depends on X") | `recall::dispatch::dependents`, now `format::dependents_of` |
| Inspect (MCP, CLI, portal) | `surface::neighbourhood` and `surface::focus` via `blast_radii`; inspect's `dependents (N):` section and structured `dependents` list; the portal's dependents list (`cli::serve_web::projections::structural_dependents`) |
| High-risk check | `daemon::events::high_risk_at`, live `format::blast_radius` instead of the stored stamp |

**Unchanged on purpose:** #29's GC protection
(`daemon::gc::resources_with_dependents`). It keeps its own two senses over
`Dependency`/`Causal`/`Hierarchical` in either direction, read from
`recall::format::inbound_sources`, which is kept for it alone. The design
asked for that predicate to stay independently defined.

## Errata

**§4.1 Blast radius.** Was: concepts with at least one structural edge from
the node and none from any other concept (exclusive, outbound, over
`Dependency`/`Causal`/`Hierarchical`). Now: the number of the node's
dependents as defined above, all of them, not only those the node would
orphan. The 2026-08-11 errata (provenance never counts) and the
`c.id <> node` self-exclusion still hold.

**§4.1 Interaction span.** Was: distinct origin interactions of the sources
of aged structural edges into the node. Now: distinct origin interactions of
the node's aged dependents, **excluding the node's own origin interaction**,
each interaction itself older than the cutoff. Both age gates (edge and
interaction) and the F1 single-point rule are unchanged. The own-origin
exclusion is the design's "dependent origin interactions other than X's own":
a parent derived together with its children, or an action depending on a
concept minted in the same call, is one interaction vouching for itself. The
14-day denominator is PR 2, not this PR.

**§10 Stage 2 and Stage 3.** The wording "inbound
`Dependency`/`Causal`/`Hierarchical` edges" and "hypothetical removal would
orphan > 5 nodes" both read as the dependents relation. Thresholds are
unchanged: `distinct >= 3`, `coverage >= 0.3`, blast radius `> 5`.

**§13 Demo.** The ⚑ line in the live demo now reads
`⚑ Load-bearing pillar — 15 nodes depend on this. Modify with caution.` The
script is unchanged: `user schema` has nine `parent_of` children (the nine
the spec counted) and six actions whose `depends_on` names it, and the six
now count. The spec's 9 is the pre-#36 count.

**High-risk check.** `daemon::events::high_risk_at` reads the live
dependent count, with no age floor (unlike Stage 3), and only after the
node has a fresh write, so a quiet cycle costs what it did in 0.5.0. The
edge that gives a node its fifth dependent is itself a fresh write, so
reaching five fires a high-risk warning at once. In the demo, `auth
middleware` (five actions depend on it) now carries a high-risk line in
agent B's recall: nine warnings instead of eight.

**Stored stamps.** `Concept::blast_radius` and
`CanonizationEvent::blast_radius` are written at Stage 3 promotion. Rows
written by 0.5.0 hold the pre-#36 exclusive count and are not rewritten:
they record what admitted the promotion at the time. Everything that shows a
current count computes it from the graph: recall, inspect, saints, the
portal and, since this PR, the high-risk check (`is_high_value` used to read
the stamp).

## Upgrading: no migration, and Stage 2 is re-checked at Stage 3

Nothing is migrated: stored edges are read with the #36 meaning, and
stored stamps stay as written (above). The one behaviour an upgrade needs
guarded is promotion of Venerables admitted under the old rule.

A Venerable was never re-checked: Stage 3 looked at blast radius and the
cooldown only. Venerables admitted by 0.5.0 counted inbound `Causal` edges
as Stage 2 support, which #36 removes, so without a guard a 0.5.0 Venerable
whose only support was `Causal` (a cross-repo path hub such as a README, see
#129) would turn Canonical on the first cycle after the upgrade as soon as it
had more than five aged `depends_on` sources, even from one interaction, and
carry a ⚑ warning. Budget demotion cannot catch it with 0 Canonicals.

Under Swarm, `canon::eval::verdicts` now runs `stage2_passes` for each
Stage 3 window member before its blast probe, with the same age floor. The
design does not exempt Venerables from Stage 2, and its acceptance wants
Canonical rulings to be both supported and load-bearing. A root-goal
Venerable (made Venerable directly by `set_root_goal`) now also needs Stage 2
support to go Canonical under Swarm.

Solo keeps its plain probe. It admits by score band and does not consume
the verdict for admission, but `apply` stamps the audit row with the blast
measured by a passing Stage 3 verdict, so skipping the probe for Solo
promoted the same node with no measured blast on its row (review M1; the
first version of this guard claimed Solo was unchanged and was wrong). The
re-check is selected by an exhaustive match on `PromotionPolicy`; test
`solo_keeps_its_stage_3_probe_and_stamps_the_measured_blast`.
Venerables are never demoted for losing Stage 2; they stop progressing.
Tests: `a_venerable_that_no_longer_holds_stage_2_does_not_promote` (fails
without the re-check) and `a_venerable_that_holds_stage_2_and_stage_3_promotes`.

### Dry run on today's rig copy (aggregates only)

A copy of the dogfood store taken 2026-10-12 (5,005 concepts: 404
Candidate, 7 Venerable, 0 Canonical) was evaluated read-only at
2026-10-12T12:00Z with the 0.5.0 and the #36 SQLite queries (60 s floor,
300 s cooldown, Swarm thresholds). The #36 figures match the library on
411 of 411 concepts. No store contents are committed.

| | 0.5.0 | #36 |
|---|---:|---:|
| Candidates with blast > 0 / > 5 | 137 / 39 | 28 / 1 |
| Candidates passing Stage 2 | 0 | 0 |
| Venerables with blast > 0 / max | 0 / 0 | 4 / 3 |
| Venerables passing Stage 2 | 7 | 0 |
| Turn Canonical on the first evaluation, without the guard | 0 | 0 |
| Turn Canonical on the first evaluation, with the guard | n/a | 0 |

All seven Venerables are short path-like Resources or Entities whose 0.5.0
support was `Causal`; under #36 none holds Stage 2 (the closest has 3
distinct interactions at coverage 0.273, blast 3). Nothing turns Canonical
either way on today's data; the guard matters once such a hub accumulates
six `depends_on` sources.

## Golden and expectation changes

Every committed golden that moved, and why:

- `fixtures/session-rest-api.json`: regenerated by `scripts/gen-fixtures.py`.
  The eight `user schema -> D1..D8` field edges are `Hierarchical` instead of
  `Dependency` (operator decision 4), so `user schema` has 6 + 8 = **14**
  dependents. Stored `blast_radius`: `user schema` 8 -> 14, `api layer` 1 -> 3
  (three concepts depend on it). No other row changed; `api layer -> api docs`
  and `api layer -> caching layer` stay `Dependency` (api layer depends on
  them).
- `fixtures/recall-context-golden.txt`: `blast radius 8` -> `14` and the ⚑
  count 8 -> 14. Two `0.51`-score hits swap order (`create user` now precedes
  `user status`). The cause is the score's edge-type bonus,
  `edge_type_bonus_value` in `src/daemon/score.rs`: `user status`'s field
  edge is now `Hierarchical` (0.015) instead of `Dependency` (0.02), which
  lowers its score just below `create user`'s. Hits order by score, then
  canonical key, then node id, so the new order is deterministic.
- `fixtures/recall-h3-goldens.json`: **unchanged**. The two unit tests that
  pin it plant their dependents in the new direction instead.
- Fixture anchors: `user schema` span 6 distinct over 25/55 -> **7 distinct
  over 30/55** (own origin excluded); Stage 3 stamp 8 -> 14; `api layer`
  blast 1 -> 3 (still fails Stage 3, still passes Stage 2 at 3 over 30/55).
  Asserted in `src/fixtures.rs`, `src/canon/stage2.rs`,
  `src/canon/stage3.rs`, `src/canon/eval/tests/fixture.rs`,
  `src/recall/format.rs`, `src/recall/assemble.rs`,
  `src/store/sqlite/tests/structural.rs`,
  `src/store/pg/cockroach/conformance.rs` (live) and
  `tests/rebuild_session.rs`.
- Demo: `EXPECT_BLAST_RADIUS` 9 -> 15 and `EXPECT_BLAST_WARNING` 9 -> 15
  (`src/cli/demo.rs`, `tests/binary_parity.rs`); docs `docs/demo.md` and
  `site/src/content/docs/demo.mdx`.
- The second-`parent_of` refusal text (`src/graph/derive.rs`) no longer
  claims a second parent zeroes blast radius; the refusal itself is
  unchanged.
- Demo: agent B's recall carries 9 warnings (was 8): `auth middleware`, with
  five actions depending on it, is high-value under the live high-risk
  check (`tests/t84_demo.rs`).
- `lambo_inspect` tool description (and `tool_schemas.golden.json`) mention
  the dependents it lists.

The dated evidence pages (`docs/reference/evidence.mdx` and its site mirror)
quote captures with the old count and are left as captured.

## Tests

- `src/test_util/dependents.rs`: one shared case with every edge kind,
  a shared dependent, an own-origin child, a foreign origin, a foreign
  concept, a young edge, a young interaction, a self-loop, a dependent
  reached by both kinds of edge (counted once), and an edge and interaction
  aged only by their `event_time`, with expected store answers at two ages
  and expected graph-side dependents.
- It is asserted on MemoryStore
  (`blast_radius_and_span_follow_the_dependents_case`), SQLite against both
  the expectations and MemoryStore
  (`structural_queries_follow_the_dependents_case_like_memory`), the
  graph-side functions and the structural dispatch
  (`graph_side_dependents_follow_the_dependents_case`) and the portal's
  inspect list (`inspect_dependents_follow_the_dependents_case`).
- Postgres/Cockroach: the SQL is text-checked in
  `pg/cockroach/tests/sql_shapes.rs`. `pg::dependents_live` runs the shared
  case live with MemoryStore parity: `postgres_answers_the_dependents_case`
  (`#[ignore]`d) and a leg in Cockroach's `conformance_suite`. Neither was
  run here (no live database). By the owner's decision the Postgres check
  stays manual and is not part of CI. Run it by hand with
  `LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::dependents_live -- --ignored`; the Cockroach leg with
  `LAMBO_COCKROACH_DSN=<dsn> cargo test --features store-cockroach,fixtures --lib conformance_suite -- --ignored`.
- Inspect: `inspect_lists_the_dependents_it_counts`
  (`src/surface/neighbourhood.rs`) and the portal's
  `inspect_dependents_follow_the_dependents_case`.

## #129 part 1

#129 asks for the ⚑ warning on Venerable hits, counted inbound. The inbound
count falls out of this PR: the warning, inspect and blast radius now count
what depends on a node, so an action-built hub with recorded `depends_on`
edges gets a non-zero count. Two parts do not fall out and remain open on
#129:

- **Statuses.** The warning still renders only on Canonical hits, as #36
  keeps it. Carrying it on Venerable is a separate decision.
- **Causal.** #129 counts inbound `Causal` edges as dependents; #36 decided
  they confer no load. On the lambo-dev Venerables in #129 (README files,
  ledger files), the inbound edges that are `Causal` (`produces`/`modifies`)
  no longer count.

Part 2 (repo-scoped Resource identity) is unrelated to this PR.
