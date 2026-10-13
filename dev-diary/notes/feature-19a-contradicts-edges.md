# #19(a): explicit contradiction edge groundwork

Approved Phase 0 design: issue #19 comments 6127049450 (design) and
6127133919 (approval); issue #20 comments 6127052910 and 6127137552.
Started on base `422ed101`, branch `codex/19-contradicts-edges`, then merged
with `origin/main` at `92c0f4af` (#139, the recall anchor fix). The merge was
clean.

## Decisions

- Add public `EdgeType::Contradicts`, directed concept to concept
  (`newer -> older`). Serde and the SQL codec use the PascalCase name
  `Contradicts`, like every other edge variant; the original issue's
  lowercase suggestion was superseded in Phase 0. SQLite stores the serde
  name; Postgres and Cockroach share `store::pg::codec`. Edge columns are
  unconstrained TEXT/STRING with `UNIQUE (source, target, edge_type)` in
  every migration, so no DDL change.
- `dependent_end` returns `None`. The edge adds no dependence, Stage 2
  support, blast radius, Solo action or recurrence support, and no GC
  Resource protection. The GC's own structural set
  (`recall::format::STRUCTURAL_EDGE_TYPES`), recall traversal
  (`recall::expand::TRAVERSAL_ORDER`) and the web graph's `is_structural`
  are explicit lists and stay unchanged, so `Contradicts` is outside all of
  them.
- It does not decay, has a zero additive edge bonus, and does not count
  toward score density (`EdgeType::counts_toward_density`, both the
  concept's numerator and the session's `max_incident`). The first commit
  left density counting it; adversarial review (H1) found that being
  contradicted therefore raised the composite feeding Stage 1, recall
  `final_score` and the GC cut, so a contradicted concept became more
  promotable. Fixed in `e4b2f4b7`; the orchestrator chose exclusion.
- GC's connectivity rules are unchanged: the orphan check and reachability
  from the interaction chain count a contradiction like any edge, and it
  never decays, so it keeps both endpoints connected.
- The conflict detector counts a contradiction's writer in its agent set,
  so another agent contradicting a node can complete a two-agent conflict.
  Kept as is and pinned by
  `daemon::conflict::tests::a_contradiction_writer_joins_the_conflict_agent_set`
  (inverted by #19(b) review M1, per #19(c) owner decision 2: now
  `a_contradiction_writer_does_not_join_the_conflict_agent_set`).
- Edge only. No MCP or CLI write field, resolver, detector, recall
  annotation or supersession: #19(b) waits for #36 PR 4.
- The shared synthetic #36 dependents case gains concept `Q` with
  contradictions in both directions, so the graph recall, inspect,
  MemoryStore, SQLite and (manual, ignored) live PG/Cockroach harnesses all
  inherit the exclusion and replay checks. Dedicated tests cover the GC score
  cut, the graph endpoint matrix, natural-key replay and snapshots, and Solo
  support.
- `EdgeType` is not `#[non_exhaustive]`, so the variant is a library API
  break (CHANGELOG, Breaking). Older binaries cannot decode rows or
  snapshots that contain it.

## Spec errata

The frozen spec's §5 says "seven edge types" (v0.1 cut `CrossOccurrence`
and `Fixes` from nine). `Contradicts` is now the eighth. The spec stays
frozen; this note is the record, and the `EdgeType` rustdoc says so.

## Review findings (round 1) and resolutions

- H1 density: excluded, see above; `contradictions_leave_every_score_unchanged`.
- M2: the GC test's 5.0 bar could not fail. It now compares eviction
  scores with and without the contradictions, the default-bar outcome, and
  a bar just above the uncontradicted score.
- L3: `canon::contradicts_tests` runs daemon scores, Stage 1, Solo, and
  Stage 2/3 on MemoryStore over the dependents case with and without the
  pair; both halves fail under a density or dependent-end mutation.
  Assertions that could not fail were removed.
- L4: CHANGELOG Upgrading covers downgrade and mixed fleets.
- L5, L6: documented above and in the API page; L6 pinned by a test.
- L7: stale under-weight `Contradicts` survives GC step 1; the decay-set
  debug_assert checks both directions through `EdgeType::ALL`.
- L8: rustdoc states direction and that no product path writes it yet.

## Open for #19(c)

- Should a GC-collected contradicting source drop the warning, and should
  a contradiction keep its endpoints out of GC's orphan and island sets at
  all?
- Should a contradiction writer count toward a multi-agent conflict? The
  pinned test must change with that decision.

## Golden

`fixtures/inspect-dependents-golden.txt` gains exactly two lines: a
`Contradicts` heading and `<- concept Q [Resource]` in the hop listing.
Inspect lists each neighbour once, by the first incident edge, so the mutual
pair shows one direction. The dependents list, its count (8) and the blast
warning are unchanged. Main did not touch this golden between `422ed101`
and `92c0f4af`. The cloudops parser self-test
(`python3 scripts/cloudops/_lambo.py`) passes against it, and
`inspect_lists_the_dependents_it_counts` now asserts `Q` is not a dependent.

## Verification (after the merge)

- `cargo fmt --check`: pass.
- `cargo clippy --all-targets -- -D warnings` on `store-sqlite,fixtures`,
  `ship,fixtures`, `--no-default-features store-postgres`,
  `store-postgres,store-sqlite,fixtures`, `embed-eg2`, `recall-elastic`,
  `embed-candle`: all pass.
- `cargo test --features store-sqlite,fixtures`: 2198 passed, 0 failed,
  7 ignored (lib 2118/0/5) after the review fixes; 2192 before them.
- `cargo test --no-default-features --features store-postgres`: 1162
  passed, 0 failed, 17 ignored.
- `cargo test --no-run --no-default-features --features store-sqlite`,
  `cargo check --no-default-features`, `scripts/docs/check-mirror-drift.sh`:
  pass.
- No live Postgres or Cockroach run. No manifest, lockfile, workflow,
  version or migration change.

## Follow-ups

- #19(b): write fields on the shared strict resolver after #36 PR 4.
- #19(c): detector, revalidation, bounded companion recall, absolute layer
  floors.
- #20: supersession on #36 PR 2/3's shared predicate and fixture.
