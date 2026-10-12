# #19(a): explicit contradiction edge groundwork

Approved Phase0 design: issue19 comments6127049450/6127133919; issue20 comments6127052910/6127137552. Implementation based on422ed101, branch `codex/19-contradicts-edges`.

## Decisions

- Add public `EdgeType::Contradicts`, directed concept to concept. PascalCase serde/SQL naming follows every existing edge variant; the original lowercase suggestion was superseded during Phase0. Existing unconstrained TEXT/STRING edge columns and unique(source,target,edge_type) accept it without DDL changes.
- `dependent_end` explicitly returns None. It adds no dependence/support/blast, no Solo action/recurrence support and no GC Resource score-cut protection. Preserve the independently defined GC relation and structural cycle edge set. Explicit opposition does not decay and receives zero additive edge bonus; existing graph topology density still counts incident edges, as for provenance/association.
- This PR adds an edge only. No new MCP/CLI writer fields, typed resolver, detector, recall annotation or supersession; #19(b) waits #36 PR4. #20 additionally waits #36 PR2/3 and the rest of #19.
- Extending the shared synthetic #36 dependent case checks both contradiction directions across graph recall/inspect and all store harnesses instead of building a divergent case. Dedicated GC, graph endpoint/replay/snapshot and Solo tests cover the other structural sets. Offline PG codec/SQL tests cover both shared dialects; live regressions remain manual and ignored.
- Older binaries cannot decode Contradicts rows/snapshots. Exhaustive public EdgeType matches need another arm; CHANGELOG and mirrored API docs explicitly describe both compatibility limits.

## Verification and review

- Initial focused dependents case:4passed,0failed,0ignored (graph recall, inspect, MemoryStore, SQLite).
- First focused contradiction run:3passed,1failed,0ignored. GC assertion incorrectly expected an empty inbound_sources map; this function retains empty source-list entries. Fixed the assertion to check both endpoints, preserving product logic.
- First full SQLite run:2094passed,1failed,5ignored. The sole failure was the shared synthetic inspect golden: adding Q to the case makes the generic neighbourhood list show its contradiction. Updated exactly two lines in `fixtures/inspect-dependents-golden.txt`: the `Contradicts` heading and `<- concept Q [Resource]`. The dependent list, count8 and blast warning are unchanged; no blanket golden regeneration. The cloudops parser self-test passed with this delta.
- Full lane gate running; final counts and review history will be appended before handoff. No live Postgres/Cockroach services used; forbidden PG test source not read. No manifest, lockfile, workflow, version or migration changes.

## Follow-ups

- #19(b) shared resolver/writer surfaces after #36 PR4.
- #19(c) detection, revalidation, bounded companion recall and absolute layer floors.
- #20 supersession on #36 PR2/3's shared predicate/fixture; scoped general challenge workflow after #33 PR4.
