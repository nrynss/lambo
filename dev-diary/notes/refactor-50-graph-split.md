# #50: `graph/graph.rs` split by responsibility (decisions)

Follow-up to #27. Base: main `1729485`. Decisions and why; the commits carry
the mechanics.

## The pattern is #27's: the struct stays, the impl blocks move

`Graph` stays in `src/graph/graph.rs`, so its fields stay private to
`graph::graph`. Every other `impl Graph` block moved verbatim into a child
module. A child module is a descendant of the root, so it can read the
fields and call the root's private helpers (`append_mutation`,
`record_edge`, `normalize_weight`, `edge_endpoint_error`, `invariant`,
`not_found`, `EdgeKey`) without any of them changing visibility. Nothing
was widened. This differs from #27, where items moved *out* of the root
had to become `pub(super)` for the root to call them. Here, nothing that
moved is called by the root or a sibling except through public methods.
`legal_canonization_transition` is used only by the write gate in
`transitions.rs`, so it moved there and stayed private.

| module | holds | lines |
|---|---|---|
| `graph.rs` | module rules and layout, consts, `Graph`, `new`, the structural write path (`insert_interaction`, `insert_concept`, `upsert_edge`, `remove_node`, `remove_edge`), structural reads and neighbour queries, private helpers | 842 |
| `graph/snapshot.rs` | `from_snapshot`, `snapshot` | 279 |
| `graph/transitions.rs` | `apply_canonization_transition`, `legal_canonization_transition`, `bump_gc_survived`, `confirm_human`, `canonization_events` | 148 |
| `graph/embeddings.rs` | `stamp_embedding`, `replace_embedding_without_vectors`, `replace_embedding_with_operator_override`, `reembed_all`, `embed_missing`, `embedding` | 320 |
| `graph/mutation_log.rs` | `log_len`, `epoch`, `drain_log`, `push_front_log`, the write-intent pair, `gc_mark`, `record_gc_sweep`, `exempt_from_gc_measure`, `reanchor_gc_clock`, `anchor_gc_clock` | 194 |
| `graph/accesses.rs` | `record_accesses`, `drain_accesses`, `pending_accesses` | 112 |
| `graph/root_goal.rs` | `set_root_goal`, `root_goal`, both `root_goal_texts`, `logical_now`, synonyms, reservations | 218 |
| `graph/invariants.rs` | `assert_invariants` | 199 |

The order followed the issue table: one `refactor(graph)` commit per module.
A final `docs(graph)` commit adds the layout section to the root's module doc.
The line counts are at the branch head, after the three review fixes below
(the moves alone left the root at 820 and `snapshot.rs` at 256).

## Where the accessors went

The issue says the root keeps "read accessors" but also lists `embedding`,
`log_len`, `epoch`, `pending_accesses` and `root_goal` under the children.
The rule used: **structural reads stay in the root, and an accessor for one
responsibility's state moves with that responsibility.** Structural reads
are nodes, edges, counts, the chain, neighbours and incident edges.

- `canonization_events` (the audit trail the transition gate appends to)
  went to `transitions.rs`.
- `gc_mark` went to `mutation_log.rs` with the GC clock it reports.
- `synonyms`, `synonym`, `reservation` and `reservations` went to
  `root_goal.rs` with the writes they read back. The issue lists "synonyms,
  reservations" there, and the base file already kept them in one section
  ("session metadata, synonyms, reservations").
- `logical_now` went to `root_goal.rs` too. It sat in that section, and its
  only production caller is `set_root_goal`.

The cycle DFS (`dfs_cycle`, `cycle_neighbors`) and `edge_endpoint_error` stay
in the root even though `assert_invariants` is their main reader, as the
issue specifies. `edge_endpoint_error` is shared with `record_edge`'s write
gate. Keeping the gate's helpers in one place means the gate and the safety
net cannot drift apart.

## Paths and imports

- Every `Graph` method is still inherent, so `Graph::x` paths are unchanged.
- The one free public function that moved, `root_goal_texts`, is re-exported
  from the root (`pub use root_goal::root_goal_texts`). So
  `crate::graph::graph::root_goal_texts` and the tests' `use super::*` still
  resolve, and rustdoc still renders it at `graph/graph/fn.root_goal_texts.html`.
- The root no longer uses `GraphSnapshot` in code, but its struct docs link
  to it and the tests reach it through their glob import. It keeps an import
  marked `#[allow(unused_imports)]` with a comment saying so. The other option
  was to rewrite the struct's doc links to full paths and add an import to
  the tests, which would have edited text that did not move. `Synonym`,
  `CanonizationStatus`,
  `MutationBatch`, `WriteIntent` and `WriteIntentOutcome` left the root's
  imports with the code that used them.
- The test files in `src/graph/graph/tests/` needed **no change**. They are
  children of the root and import `super::*`, which still sees every item
  they use.

## Checks

- **Body identity.** `bodycmp.py` (from #27) compares every fn, struct,
  const and type item in base `graph.rs` with the same item in the root plus
  the children, doc comments and attributes included. All 78 base items are
  identical at every move commit and at the head. A line-multiset comparison
  of the whole file finds every non-blank base line at the head, except the
  four section banners that were removed (their sections left the root) and
  the root's import list, which was re-wrapped.
- **No `.await`.** No method gained an `.await`. There is no `async` and no
  `.await` anywhere in the root or its children.
- **`assert_invariants`.** It is unchanged and still the only definition.
- **Replay contract.** The "every mutation appends to the log, under the
  caller's write lock" rule is unchanged: `append_mutation` stays in the
  root, and no body changed.
- **Gates.** Through the move and docs commits, every local CI row has
  identical test names, statuses and counts before and after. The review
  fixes below add exactly three tests to each row that compiles the graph
  tests, and nothing else changes. `cargo doc --no-deps` gives 0 warnings,
  with or without `--document-private-items`, both before and after the
  moves.

## Write-gate gaps fixed after review

The move commits change no behaviour. Review found three latent gaps in the
code that moved or stayed, and each is fixed in its own `fix(graph)` commit
after the moves. Under the owner's standing rule, a real latent defect is
fixed rather than recorded and left. An earlier draft of this note left the
first gap as is; that call is reversed.

- **`insert_concept` refuses a concept id that names an interaction.**
  Before, it overwrote the node. The temporal chain and its Temporal edges
  then touched a concept, and every SQL store persisted both rows
  (interactions and concepts are separate id-keyed tables). The next
  `from_snapshot` failed, so the session became permanently unloadable. With
  `c.id == derives_from`, the call returned `Err`, but only after the node
  had been overwritten and an `UpsertNode` logged: a partial mutation. The
  check now runs right after the session check, before any node write or
  log append, and returns the same invariant error as the sibling refusals.
  It refuses only an *interaction* id: re-upserting an existing concept by
  id (derive, hybrid derive, `record_action` and demote all do this) is
  unchanged. No production caller reached the gap. Test:
  `insert_concept_refuses_an_id_that_names_an_interaction`.
- **`insert_interaction` names a concept-id collision.** It was already
  refused before any mutation, but the message ("exists but is missing from
  the temporal chain") read as internal corruption. The node kind is now
  checked first, and the message names the caller error. Same error variant.
  Test: `insert_interaction_refuses_an_id_that_names_a_concept`.
- **`from_snapshot` refuses duplicate node ids.** Two concepts with one id
  used to load last-wins, which contradicts GRAPH-7 ("the loaded graph must
  equal the stored snapshot"). A duplicate interaction, or a concept that
  reuses an interaction's id, was caught only later, by the chain walk or
  an edge-endpoint check, with a message that did not name the cause. Each
  shape is now refused up front. No store produces them today. Test:
  `from_snapshot_rejects_duplicate_node_ids`.

## For #8

If #8 keeps a contiguous vector matrix inside `Graph`, its state and
maintenance belong in `embeddings.rs`. The matrix is a field on the root
struct, maintained from `stamp_embedding`, `reembed_all`, `embed_missing`
and the structural `insert_concept` / `remove_node`. A graph-backed
`VectorCandidateSource` that only reads `Graph::concepts()` needs no change
here.
