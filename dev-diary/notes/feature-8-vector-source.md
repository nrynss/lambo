# #8: the holder ranks the vectors its graph holds (decisions)

Base: main `3df6812` (the refactor series #24-#28 and #50 landed). Decisions
and why; the commits carry the mechanics.

## What changed

On a session holder over SQLite, recall's vector leg and hybrid derive's
semantic match no longer call the store for vectors. They rank the vectors the
in-memory graph already holds (`Concept.embedding`, decoded once at session
load and attached to each concept as it is derived) with the same scorer the
SQLite scan uses, `store::vector_source::rank_by_cosine`. Spec §2.1's RAM tier
now holds for vectors.

| piece | where |
|---|---|
| the graph-backed source, `GraphVectorSource` and `graph_vector_candidates` | `src/graph/vector_source.rs` |
| the caller-side choice, `VectorCandidates::Graph` and `VectorCandidates::for_holder` | `src/store/vector_source.rs` |
| the adapter declaration, `GraphStore::exact_vector_scan()` (default `false`) | `src/store/mod.rs` |
| SQLite opts in | `src/store/sqlite/mod.rs` |
| `Memory::vector_candidates`, `WriteCtx::vector_candidates` call `for_holder` | `src/memory/reads.rs`, `src/writeq/execution.rs` |

## Decisions

**The adapter declares the property; the holder acts on it.** `for_holder`
picks the graph only when the store advertises `VECTOR_SEARCH` *and* returns
`true` from `exact_vector_scan()`. The second bit names exactly what makes the
graph's answer the store's answer: the store's checked read is an exact cosine
over every vector it holds under the durable contract, scored by
`rank_by_cosine`. A defaulted trait method (the precedent is
`preflight_schema` and `vector_candidates_checked`, both added after P1 with
defaults) keeps every third-party adapter, every wrapper and every test double
on the store path until it opts in. A capability flag was the alternative; it
would have changed `SqliteStore::capabilities()`, which tests pin by equality
and which the bit set exposes publicly, for a property no caller outside the
holder needs.

**One constructor, two callers.** `Memory` and `WriteCtx` both call
`VectorCandidates::for_holder(store, graph)`. #27 left the choice in two places
because `WriteCtx` holds no `Memory`; the constructor makes it one rule.

**Postgres and Cockroach keep database-side search** (the scope decision #8
asked for). Their scores come from database distance arithmetic
(`distance_to_score`: Postgres `1 - d`, Cockroach `1 - d²/2`) and Cockroach can
serve from a partial ANN index, so graph-side cosine would change scores and,
under the index, candidate sets. #8's acceptance is bit-identical ranking. The
pg family also does not parse vector text in the process, which is SQLite's
cost (below). Opting in later is a one-line override plus a parity run against
live Postgres and Cockroach; measure first.

**MemoryStore stays without a vector leg: a deviation from #8's stated
scope.** The issue says graph-side ranking "applies to SQLite and Memory for
certain"; this change applies it to SQLite only. MemoryStore does not advertise
`VECTOR_SEARCH`, and `for_holder` never offers the graph as a way to switch a
vector leg on: doing so would change every MemoryStore recall and derive
(embeds, merges, the recall goldens' leg composition), which is a behaviour
change, not the cost fix #8 is. There is also no store read to remove on
MemoryStore today. Giving MemoryStore a graph-ranked vector leg is a separate
decision with its own golden review if wanted.

**Readers keep the store.** `lambo recall` and `lambo serve-web` load a reader
graph and still read vectors from the store, in the store's own contract-checked
transaction. The source is a holder concept because the holder's graph is the
freshest copy; a reader's graph is a snapshot of the same store. Switching
readers would save their second read of the vectors (one-shot CLI cost), and is
left for a measured follow-up.

**No vector matrix, no index, nothing to keep in sync.** The scan iterates
`Graph::concepts()` and borrows each `Vec<f32>` in place. The graph already
held every vector; the source adds **0 bytes per vector** at rest. Per call it
allocates one `(NodeId, &[f32], &str)` tuple (48 bytes) per embedded concept
plus `rank_by_cosine`'s scored list (40 bytes each), about 320 KB at 3,600
concepts, freed on return; the store path allocated the 45 MB of text and
15 MB of decoded `f32` per call. Because nothing sits beside the graph, every
mutation that changes an embedding (stamp, `replace_embedding_*`, `reembed_all`,
`embed_missing`, `insert_concept`, `remove_node`, GC, `from_snapshot`) is seen
by the next scan with no extra code, and #23's session erasure drops vectors by
removing their concepts. A contiguous pre-normalised matrix would make the scan
a SIMD dot product, but pre-normalising changes the float arithmetic and so the
score bits, and the measured scan is already ~3 ms (below).

**Locking.** The scan takes the graph read lock and releases it inside one
synchronous call; no `.await` while it is held. Both callers reach it from
phases that hold no graph lock (recall's gather before the pipeline's guard,
hybrid derive's gather between plan and commit). `for_holder` and `available()`
never touch the graph, so the write paths that ask `available()` under a read
guard are unaffected. The scan holds the read lock for ~3 ms at 3,600 concepts.

*Cost and scaling.* The scan is synchronous O(N·d) work on a tokio worker
under the graph read lock: about 0.83 µs per embedded concept at d = 1024
(3.0 ms / 3,600; one measured N, so the slope is inferred, not fitted).
Linear projection per call: ~30 ms at 36k concepts, ~85 ms at 100k, ~0.8 s at
1M. Hybrid derive makes up to k separate calls, one lock window each.
Consequences: parking_lot's task-fair lock bounds a writer's wait (flush drain,
derive commit, `push_front_log`) to one in-flight scan and queues new readers
behind a waiting writer, so there is no starvation but each write can be
delayed by one scan; the worker thread is blocked for the scan (no
`block_in_place`); and `HYBRID_IO_TIMEOUT` no longer bounds this leg, because
`timeout_at` polls the already-ready inner future first
(`graph/hybrid.rs:1302`), so a derive whose scan runs past its deadline
succeeds instead of timing out. Read the CHANGELOG's deadline line as "the scan
no longer consumes the store-I/O deadline", not as a bound on the scan. All of
this is strictly better than before #8, which decoded ~110 ms of text on the
worker per call. Cheap later wins if N grows: score all k derive probes in one
pass under one read guard, and move very large scans to `block_in_place` or a
blocking task.

**Same checks, same order, same errors.** `graph_vector_candidates` applies
SQLite's sequence: limit bound, `limit == 0` empty, probe is an embedding
(`ensure_is_an_embedding`, moved to the always-compiled seam module so Memory-only
builds have it), no contract empty, contract mismatch refused with SQLite's
exact `Invariant` message (recall matches on "embedding contract changed" to
annotate a keyword-only result), probe width refused. A stored vector whose
width disagrees with the contract is reported as `Backend`, as SQLite reports a
corrupt row; the graph's write gates make it unreachable. A request for a
session the graph does not hold is refused (`Invariant`), not answered empty.

**Freshness is the intended difference.** The graph is ahead of the store by
the flush lag (50-130 s measured on the Metal rig). A concept derived and not
yet flushed is now a vector candidate, for recall and as a merge target for the
next derive; one removed and not yet flushed is no longer returned. Hybrid
derive's rule that a vector minted in a call cannot drive a merge in the same
call still holds: the call stages its writes on a private clone until commit.
Its replan check (epoch) makes the graph source consistent with the commit.

*Freshness versus durability.* Merges are staged on a clone, a merge target
removed before commit forces a replan, and the flush drains the ordered log as
a prefix with retained batches put back at the front (`store/flush.rs`), so a
`Semantic` edge never lands without its unflushed target. Two residual cases
predate #8 in kind but are now reachable through a merge into an unflushed
target:

- a dead-lettered batch (STORE-4/D5) holding the target makes the later batch
  with the edge fail its foreign key and be dead-lettered too; the same class
  as canonical-match reinforcement of an unflushed concept;
- a queued derive whose receipt reported `semantic_merged = [T]` with T
  unflushed, followed by a crash: the intent is not durably consumed and
  replays against a different graph (T re-created with a new id by its own
  replayed intent, or absent if T came from a synchronous derive), so the
  client-visible outcome differs from the pre-crash receipt. Before #8 the same
  held for `created` ids.

No code change: both follow from write-behind durability, not from where the
vectors are ranked.

**`select_session_vectors` loses `_probe` and `_limit`.** An exact scan can
use neither; the seam an index plugs into is the adapter's
`VectorCandidateSource` implementation, which already carries both.

**`graph/hybrid.rs` was not split.** The change there is documentation only
(the gather phase and the merge-target rules name the graph source), so a split
would not have helped.

**Packed-f32 storage (#8 part 2) is not in this change.** After part 1 the
text codec is off the recall and derive path. It still costs at session load
(the micro-benchmark below: 282 ms to load 3,600 concepts, of which about 110 ms
is parsing) and in file size (45.6 MB of vector text against 14.7 MB as `f32`).
It needs a SQLite storage migration with a codec marker and an old-codec store
test, a separate reviewable change; proposed as its own issue in this branch's
issue updates.

## Parity evidence

`src/store/sqlite/tests/vector_graph_parity.rs` runs SQLite's checked read and
the graph source over one flushed session, loaded the way a holder loads it, and
requires the same ids in the same order with identical score **bits**, or the
same refusal:

- ties under distinct keys, and three Observations sharing one canonical key and
  one vector (only the node id separates them); concepts without vectors;
  limits 1 to past the pool and the public maximum; four probes including an
  unnormalised one;
- both committed fixture graphs, synthetic unit vectors, every third concept
  unembedded, every embedded concept's vector as a probe plus a blend;
- every refusal and empty answer: limit over the bound, limit 0 with a wrong
  contract, zero-norm / NaN / infinite probes, a renamed contract (same message),
  a wrong-width probe, no contract, unknown session, contract without vectors;
  plus a foreign session on the graph source.

Mutation-checked: dropping the probe check, or scoring a normalised probe,
turns them red. The existing recall, derive, H1 cross-store and vector e2e
tests pass unchanged; the CI-grepped
`sqlite_vector_leg_fires_on_an_organically_derived_concept` still runs the
SQLite scan, because its recording wrapper does not forward the opt-in.

Holder tests in `src/store/sqlite/tests/vector_e2e.rs`: zero store vector reads
across a derive, a reopen, a recall whose vector leg fires and a queued derive
through the write pipeline; the vector leg returns an unflushed concept and a
near paraphrase merges into it, where the store path misses both. Turning the
SQLite opt-in off turns all three red.

## Performance

Method of #8's "Reproducing", on a scratch copy only: the 3,600-concept SQLite
store #26 seeded (3,600 embedded at dim 1024, 45.6 MB of vector text, fixture
embedder), a fresh `.backup` per run with lease rows cleared, `lambo serve
--transport http` on a private port with a private `XDG_RUNTIME_DIR`, 2 warmup
recalls, 36 timed recalls, 12 one-concept and 12 three-concept derives timed
from `lambo_derive` to its receipt applied (`lambo_stats` with `wait_ms`).
Release builds (`store-sqlite,store-memory,embed-fixture`), base `3df6812`,
head this branch; six runs each in alternating order on a loaded macOS host.
Pooled over all samples:

| | base p50 | head p50 | head p95 |
|---|---|---|---|
| warm recall (n=216 each) | 219.4 ms | 4.3 ms | 4.8 ms |
| derive to applied, 1 concept (n=72) | 246.2 ms | 14.1 ms | 18.7 ms |
| derive to applied, 3 concepts (n=72) | 670.5 ms | 20.0 ms | 22.5 ms |
| serve start to first MCP answer (n=6) | 355 ms | 360 ms | |

Where a base recall went (release micro-benchmark over the same store, median
of 30): the SQLite checked read 218.6 ms, of which the `SELECT` of 45 MB of
BLOBs 100.1 ms, decoding the text to `f32` 110.3 ms (the **vector-parse share,
about 50 % of a recall**), ranking 2.5 ms. The graph source's scan: 3.0 ms. The
store scan was about 99 % of a warm recall; what is left is the ranking and
the rest of the pipeline. With a real embedder the query embed (about 17 ms on
Metal for a short query) is now the floor, which is #14's lever. The Metal rig
re-measure #8's acceptance asks for is still owed.

## For #14 and #18

- #14: `embed_query` and the recall cache are untouched. The vector leg is now
  a pure function of the graph at the moment of the scan, which a cache keyed
  on a graph generation can rely on.
- #18: an Elastic tier is a store (`TieredStore`) whose checked read is its own
  `VectorCandidateSource`; it leaves `exact_vector_scan` false, so a holder over
  it keeps calling the store and `for_holder` needs no change.
