# #116 and #102: hybrid derive's commit (0.4.1 lane A)

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
2. **Contract:** the session's stamp must equal the one the plan saw.
3. **Canonical resolution:** `validate_graph_inputs` and `plan_inputs` are
   rerun against the live graph. The items (content, type, canonical key,
   canonical match) and the `parent_of` ends to create must be identical.
   An error here is the error a replan would return.
4. **Semantic-merge choices:** every concept upserted or deleted since the
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

A stale plan goes round again. That is cheap now, because of (1).

**The semantic-merge rule, decided: correctness over throughput.** A rival
committed between plan and commit that *could* change the merge choice
always forces a replan, so the result is what a call starting after the
rival would produce:

- a rival with the same canonical key is matched, never duplicated, by
  check 3;
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

`append_mutation` records `(epoch, id)` for every `UpsertNode` and
`DeleteNode` in a RAM-only ring of the last 8,192 node writes
(`Graph::nodes_written_since`). The ring answers `None` for an epoch older
than anything it has evicted. A loaded graph answers from its load epoch.

The mutation log was not usable for this, because the flush drains it.
The ring sees every concept write: derive, record_action, demote, retract,
GC, canonization, vector rewrites. It does not see access bookkeeping,
which goes around the log and changes nothing a plan reads.

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
  mutates, under one write hold. No clone, no epoch.
- No other path in `src/` clones the whole `Graph` or gates a commit on the
  epoch.

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
  - skipping the rollback fails the midway test.

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
| 10,000 | 6.8 ms | 18 µs |
| 50,000 | 36.2 ms | 20 µs |
| 100,000 | 168 ms (from #102) | 27 µs |

The probe measures the uncontended path. A contended commit adds
`plan_still_holds`, which costs the same order as one plan: a few index
lookups per item, plus one cosine per item for each concept written since
the plan.

Not measured here: the #116 acceptance run (`mcp_concurrent.py` at 8
workers, under 1% failed writes) needs a release binary on a store copy.
That run belongs to the 0.4.1 milestone check, not this lane.

The plan phase still scans every concept once, under the read lock, to
collect image-sourced concepts (#22), so a writer can wait behind one pass
over the session there. This lane left that scan alone; it is a candidate
for the same index treatment.
