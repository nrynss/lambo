# #116 and #102: hybrid derive's commit (0.5.0 lane A)

Base: main `a2d4f64`. Both issues live in one function,
`derive_planned` in `src/graph/hybrid.rs`, so they were fixed together.
This note records the design, the invariants it keeps, and the
measurements. The commits carry the mechanics.

## The problem

`derive_planned` has three phases: plan under a read lock, embed and rank
candidates with no lock, and commit under the write lock.

- **#116.** The commit went ahead only if the whole-graph epoch had not
  moved since the plan. Any write by anyone in the embed window forced a
  replan, and each replan re-embedded everything. After
  `MAX_HYBRID_REPLANS` (8), the write failed with "hybrid derive could not
  commit after 8 concurrent graph changes". With 8 concurrent agents,
  165 of 497 writes failed and apply p50 was 10.4 s
  (`evidence/0.4.0-vs-0.3.0-load/`).
- **#102.** The commit staged its writes on `guard.clone()`, a copy of the
  whole session with every 1,024-wide vector, under the write lock, then
  swapped the copy in.

## Design

### 1. Embeddings survive replans (`EmbedCache`)

The cache keeps each vector the call has computed. Its key is the exact
text sent to the embedder: the content framed with the interaction text
(`context_text`). The cache lives for one call, and the call has one
embedder and one live contract, so that text is everything that determines
the vector.

The session's *stamped* contract can still change between attempts. If it
moves from one contract to another, or is cleared, the cache is emptied. A
first stamp (none to some) leaves the cache alone, because vectors this
embedder made are still in its space. A stamp that does not match the live
contract is refused at plan, before any embed.

Result: a replan calls the embedder only for text it has not embedded yet.

### 2. Validate the plan, not the epoch (`plan_still_holds`)

At commit, under the write lock, the plan is checked against what changed
since it was made:

1. **Same epoch:** it holds (the fast path; no other writer committed).
2. **Interaction** (review L3): it must still carry the prompt text,
   creation time and event time the plan read. The text frames every
   embed and the times stamp every row; a re-upsert can rewrite them.
3. **Contract:** the session's stamp must equal the one the plan saw.
4. **Canonical resolution:** `validate_graph_inputs` and `plan_inputs` are
   rerun against the live graph. The items (content, type, canonical key,
   canonical match) and the `parent_of` ends to create must be identical.
   An error here is the error a replan would return.
5. **Semantic-merge choices:** every plan-relevant concept write since the
   plan is read from the graph's node-write window and checked against each
   item that ranked candidates:
   - any change to a merge target (any member of the tied top tier) makes
     the plan stale;
   - a live concept with a text-sourced vector makes it stale if it scores
     `>= threshold` against an item that planned a fresh concept;
   - a live concept with a text-sourced vector makes it stale if it scores
     `>= planned score - 1e-4` against an item that planned a merge.

   Nothing else can change a choice, and it commits alongside: a concept
   below the threshold or below the planned best, an image concept (a text
   item never merges into one, #22), an edge, a deleted non-target. If the
   window no longer reaches back to the plan, the plan is stale.

   **The rescoring is capped** (review L1). It runs under the write lock,
   so the check first collects the written concepts an item could merge
   into (live, text-sourced, with a vector). If their count times the
   ranked items exceeds `MAX_PLAN_RESCORES` (8,192), the plan is treated
   as stale instead of scanned; the replan costs no embeds. A first cap of
   32,768 measured 22.5 ms, more than the clone #102 removed, so it was
   lowered (see Measurements).

   **The final attempt checks past it** (second review, L1). A large call
   (100 to 256 unmatched items) under a steady stream of unrelated writes
   could be over the cap on every attempt and fail although nothing
   conflicted. The eighth attempt therefore uses `MAX_PLAN_RESCORES_FINAL`
   (32 times the cap, about 180 ms extrapolated from the measurement);
   its check and commit are one write-lock hold, so it is the serialized
   path and fails only on a real conflict or past that ceiling.
   `plan_still_holds` answers `Holds`, `Stale` or `OverBudget`; a
   budget-driven replan is logged (target `lambo::hybrid`) with a
   process-wide count, and a call that ends over budget fails with its own
   message ("more was written ... than its plan can be rechecked against
   ...; split the call") instead of "8 concurrent graph changes".

A stale plan goes round again. That is cheap now, because of (1).

**The semantic-merge rule, decided: correctness over throughput.** A rival
committed between plan and commit that *could* change the merge choice
always forces a replan, so the result is what a call starting after the
rival would produce:

- a rival with the same canonical key is matched, never duplicated, by
  check 4;
- a rival that scores above the planned target, or ties it within the
  margin (a tie is decided by canonical key, so a tying rival can win),
  becomes the merge target after the replan;
- a removed or rewritten target is never written as a degraded
  keyword-only concept.

The 1e-4 margin covers last-bit differences between a store's score and
the graph's cosine. It can only cause an extra replan, never a wrong merge.

The check reads the graph, not the store. On a store that lags the holder,
it can therefore see a rival the store did not return. That is stricter
than the old rule, which replanned and asked the same lagging store again,
and it ends: the replan's new epoch is after the rival.

`MAX_HYBRID_REPLANS` stays 8. It now bounds only a call that keeps losing
to rivals on its own keys or merge candidates.

### The node-write window

A RAM-only ring of the last 8,192 *plan-relevant* concept writes, each
`(epoch, id)` (`Graph::nodes_written_since`). The ring answers `None` for
an epoch older than anything it has evicted. A loaded graph answers from
its load epoch.

Plan-relevant (review L2) means what a plan can read: a concept created or
deleted, or a change to its vector, vector source, canonical key or type.
`insert_concept` compares against the value it replaces; `remove_node` and
the vector rewrites (re-embed, image vector drop, backfill) always record.
Counter and status writers (GC survivor bumps, human confirmation,
canonization transitions, access bookkeeping) and an unchanged re-upsert
(a canonical re-derive) do not. Without this, any write to a hot merge
target made every in-flight plan that ranked it stale, and enough of them
could still exhaust `MAX_HYBRID_REPLANS`. A future writer that changes one
of those fields must call `note_plan_relevant_write`.

The mutation log was not usable for this, because the flush drains it.

### 3. Commit in place (`Graph::transaction`)

The commit body runs in place inside `Graph::transaction`, which keeps an
undo record. While the transaction is open, the structural write path
(`insert_interaction`, `insert_concept`, `upsert_edge`, `remove_edge`,
`remove_node`) and `stamp_embedding` record a prior value the first time
they touch each of these:

- a node;
- an edge (by id);
- an unflushed-set entry;
- an access-dirty flag;
- the reservations.

The scalars are saved when the transaction opens: epoch, log length,
temporal-chain length, embedding contract, unflushed contract.

On `Err`, or on a panic (`catch_unwind`, then the panic is passed on), all
of it is restored, including the canonical-key index and the node-write
ring. Edges are restored in two passes, so an edge key that was removed
and then recreated under a new id comes back as the original edge.

A debug build asserts that no mutation the transaction cannot undo is
logged inside one. Every other mutator (canonization transitions, counters,
vector rewrites, root goal, synonyms, reservations) is outside the covered
path.

The undo log was chosen over "validate everything, then mutate". The
commit has fallible steps that are hard to prove unreachable, such as an
embedder returning a vector of the wrong width (`insert_concept` rejects
it). The undo log keeps every such step atomic, including ones added later.

### 4. The canonical-key index

`canonicalize` scanned every node to match a key, and so did the UNIQUE
`(session_id, canonical_key)` gate in `insert_concept`. Both now use a
RAM-only index of non-Observation concepts by key, lowest id first. That
is exactly the set and order the scans used. Without the index, the commit
(and the plan recheck in (2)) would still hold the write lock for time
proportional to the session.

The index is maintained in `insert_concept`, `remove_node`, `from_snapshot`
and rollback. `assert_invariants` checks it.

## Invariants kept

- **Atomicity.** A failed derive leaves the graph, its mutation log, its
  epoch and its unflushed set exactly as they were. Tested by
  `a_commit_that_fails_midway_leaves_the_graph_untouched` (the first item
  is written and the contract stamped, then the second item fails) and by
  the graph-level `txn` tests (failure, panic, an edge key recreated under
  a new id).
- **J3 commit hook.** It runs exactly once, after the mutations, under the
  same write-lock hold, so the mutations and the intent consumption share
  one flush batch. Tested by `the_commit_hook_runs_once_after_a_replan`.
- **No lock across an await.** Unchanged; validation and the transaction
  are synchronous.
- **Derive outcomes for non-concurrent workloads are unchanged.** With no
  rival, the epoch is unchanged and the commit is the old commit. Every
  existing hybrid, merge, receipt and writeq test passes. Two tests pinned
  the defect itself and were updated (below).

## Other callers checked

- **record_action** (the writeq job and `Memory::record_action_embedded_as`)
  embeds once before the lock, then validates and mutates under one write
  hold. There is no epoch check and no clone, so it does not share the
  pattern.

  It stamps the contract before `record_action_with_embeddings` can fail.
  That is not reachable: `Memory` stamps the contract at build (outside
  re-embed mode), so the call is only a compatibility check. A test
  written to show it failed at its first assertion for that reason.
- **Image derive** goes through `hybrid::derive_with`, so it is covered by
  this change.
- **Canonical `derive`** (`derive.rs`) is synchronous: it validates, then
  mutates, under one write hold. No clone, no epoch. Its write loop now runs
  inside `Graph::transaction` as well, for the cycle refusal below.
- No other path in `src/` clones the whole `Graph` or gates a commit on the
  epoch.

## Hierarchical cycles (pre-existing, found in review)

Neither strategy refused a `Hierarchical` cycle longer than one node:
`parent_of [(a, b)]` then `[(b, a)]`, a chain `a -> b -> c` then `(c, a)`,
both directions in one call, or (hybrid) a rival writing `child -> parent`
between plan and commit. Spec §5.7 makes `Hierarchical` a DAG.

**Decision: refuse, with nothing written,** as a `StoreError::Invariant`
built by `StoreError::cycle_refusal`. The review suggested a `Config`
error; `Invariant` was kept because it is what the reflexive-pair refusals
beside it and `record_action`'s Causal/Dependency cycle refusal already
return. Both cycle producers now share the constructor: a fixed prefix plus
a kind tag (`CycleKind::Hierarchical` or `CausalOrDependency`), then the
pair for the operator's log. `StoreError::is_cycle_refusal` recognises it,
following `is_embedding_contract_refusal`, so no public enum gains a
variant.

- **What the caller is told** (second review, L-msg): a fixed sentence per
  kind, "store error: parent_of would close a Hierarchical cycle (the
  hierarchy must stay acyclic); nothing was written", or the record_action
  equivalent. Nothing the caller sent is echoed; the class stays `store
  error`. Documented on `parent_of` and on `produces`, `modifies`,
  `depends_on` in `docs/reference/mcp.mdx` and its site mirror.
- **At replay** (second review, M1): a durable intent that meets a cycle
  refusal settles `failed` on its own and the replay goes on, as for an
  `Embed` refusal (without counting it as embedder evidence). Before, it
  fell to the session-wide arm, stopped the replay and left every later
  intent `pending_replay` for good; `record_action`'s cycle refusal had the
  same hazard. Every other `Invariant` still stops the replay: it can mean
  an inconsistent graph or store.

- `derive::cycles::reject_parent_of_cycles` runs in both pre-passes (sync
  `validate`, hybrid `validate_graph_inputs`), so the J3 ack refuses what
  the graph already shows. It walks the existing `Hierarchical` edges plus
  every pair of the call, with an end the call will create named by its
  canonical key. It runs under the read lock only: hybrid's commit recheck
  (`plan_still_holds`) skips it (second review, L-cost), and the rival case
  is refused by the write-time check below.
- `reject_closing_edge` runs at every `Hierarchical` edge write, against
  the live graph (`Graph::hierarchical_path`), and is the authority. Both
  strategies' write loops run inside a transaction, so a refusal there
  undoes the call.
- An edge that already exists is a reinforcement and never counts as
  closing a cycle.
- Both walks go *up*, from the pair's parent through its ancestors looking
  for the child (`Graph::hierarchical_parents`, straight off the adjacency
  index, no allocation or sort). A hierarchy is far shallower than it is
  wide, and sync derive's one-parent rule makes ancestors a chain. Worst
  case, measured by the ignored probe
  `derive::cycles::tests::a_cycle_walk_over_a_large_hierarchy_is_timed`
  (release, M3 Pro): a pair under the leaf of a 100,000-node chain, 20 to
  30 ms per walk; a pair over its root, about 8 µs.

## Tests

- New, in `src/graph/hybrid/tests/concurrency.rs`:
  - `two_derives_racing_on_unrelated_keys_both_commit_without_a_replan`:
    both calls planned before either committed; one attempt and one embed
    each. Fails under the old epoch rule.
  - `a_rival_commit_of_the_same_canonical_key_is_matched_not_duplicated`
  - `a_rival_that_would_win_the_semantic_merge_forces_a_replan`
  - `a_weaker_rival_does_not_disturb_the_planned_merge` (the control)
  - `a_removed_merge_target_forces_a_replan`
  - `a_commit_that_fails_midway_leaves_the_graph_untouched`
  - `the_commit_hook_runs_once_after_a_replan`
  - review L2: `counter_writes_to_the_merge_target_do_not_force_a_replan`
    (GC survivor bump, human confirmation, unchanged re-upsert) and
    `a_merge_target_vector_change_forces_a_replan`
  - review L1: `plan_rescoring_is_capped_and_replans_past_the_cap` (five
    ranked items against `MAX_PLAN_RESCORES / 5` written concepts commit
    first time; one more replans)
  - review L3: `a_rewritten_interaction_forces_a_replan`
  - second review L1:
    `a_large_call_under_streaming_writes_commits_on_the_final_attempt`
  - cycles: `hybrid_refuses_a_two_cycle_across_calls_and_in_one_call`,
    `hybrid_refuses_a_two_step_cycle`,
    `a_rival_closing_the_cycle_during_the_gather_is_refused_at_commit`
- New in `src/graph/derive/cycles.rs` (sync derive): direct, in-call and
  two-step cycles refused with the graph untouched, and the write-time
  check undoing a partially written call.
- Second review: `memory::tests::replay::a_cycle_refused_at_replay_settles_failed_and_the_replay_goes_on`
  (M1; red before: the replay stopped at the first refusal),
  `surface::error` and `types` unit tests for the fixed sentences and the
  narrow marker.
- New in `src/graph/graph/tests/txn.rs`: transaction rollback (failure,
  panic, an edge recreated under a new id), commit, and the node-write
  window, including its reach.
- New in `src/graph/graph/tests/structure.rs`:
  `the_canonical_key_index_follows_inserts_retypes_and_removals`.
- Updated (they asserted the defect):
  - `intervening_graph_mutation_discards_stale_gather_and_replans` is now
    `an_intervening_unrelated_mutation_commits_without_a_replan` (1 embed,
    1 candidate read; it was 2 and 2).
  - `intervening_synonym_change_discards_stale_gather_and_replans` keeps
    its name and now expects 1 embed (the cache) and 2 candidate reads.
- Each new behavioural test was checked by breaking the code it guards:
  - dropping the target check fails the removed-target test;
  - dropping the rescore fails the stronger-rival test;
  - restoring the epoch rule fails the race test, the weaker-rival test
    and the unrelated-mutation test;
  - disabling the cache fails the synonym and two merge tests;
  - skipping the rollback fails the midway test;
  - recording every upsert in the window fails the counter-write test;
  - removing the cap fails the cap test;
  - skipping the interaction check fails the interaction test;
  - removing the write-time cycle check fails the write-time test, and
    removing both cycle checks fails all seven cycle tests.

## Measurements

The commit-phase write-lock hold was measured with the ignored probe
`graph::hybrid::tests::commit_cost`:

- release build, Apple M3 Pro;
- 1,024-dim vectors, sessions loaded through `from_snapshot`;
- a two-concept derive, median of 21 runs;
- the hold runs from taking the write lock to the commit hook's return.

To rerun it:

```text
cargo test --release --lib graph::hybrid::tests::commit_cost -- --ignored --nocapture
```

| concepts | before (clone) | after |
|---:|---:|---:|
| 10,000 | 6.8 ms | 16.5 µs |
| 50,000 | 36.2 ms | 17 µs |
| 100,000 | 168 ms (from #102) | 21 µs |

(After the review fixes; the first round measured 18, 20 and 27 µs.)

That probe measures the uncontended path. A contended commit adds
`plan_still_holds`: a few index lookups per item, the cycle walk over the
hierarchy below the call's children, and at most `MAX_PLAN_RESCORES`
cosines. Its bounded worst case is measured by the second ignored probe,
`plan_validation_worst_case_is_bounded`, at exactly the cap with every
cosine computed: 32 fresh items against 256 written concepts on a
10,000-concept session.

| cap (cosines) | median hold |
|---:|---:|
| 32,768 (first choice) | 22.5 ms |
| 8,192 (kept) | 5.6 ms |

Past the cap the plan is replanned rather than scanned, so 5.6 ms is the
most validation adds to a commit's write-lock hold at 1,024 dimensions on
attempts one to seven. The eighth (final) attempt allows 32 times that,
about 180 ms extrapolated, so that a large call under steady unrelated
writes can still commit; past it the call fails as over budget.

Not measured here: the #116 acceptance run (`mcp_concurrent.py` at 8
workers, under 1% failed writes) needs a release binary on a store copy.
That run belongs to the release milestone check, not this lane.

The plan phase still scans every concept once, under the read lock, to
collect image-sourced concepts (#22), so a writer can wait behind one pass
over the session there. This lane left that scan alone; it is a candidate
for the same index treatment.
