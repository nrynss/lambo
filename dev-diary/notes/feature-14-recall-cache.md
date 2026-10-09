# #14: a repeated recall reuses its query vector (decisions)

Base: main `b0e8207` (#8 part 1, #23 and #48 landed). Decisions and why; the
commits carry the mechanics.

## What changed

`Memory::recall_detailed` used to call the embedder on every recall, so an
identical repeated recall paid the full query embed. It now gets the query
vector through `recall::query_cache::embed_query_cached`, which looks the exact
query text up in the session's `QueryEmbeddingCache` first and embeds only on a
miss. Everything downstream (the vector leg, the pipeline, the recall cache,
assembly, the access note) runs exactly as before; only the source of the query
vector changed.

| piece | where |
|---|---|
| the LRU, `QueryEmbeddingCache`, and `embed_query_cached` | `src/recall/query_cache.rs` |
| the per-session field `Memory::query_embeddings` | `src/memory.rs`, built in `src/memory/builder.rs` |
| the call site, after `ensure_open` | `src/memory/reads.rs` (`Memory::recall_detailed`) |
| tests on both vector sources | `src/memory/tests/query_cache.rs` |

## Decisions

**The query-embedding LRU, not the reorder.** The issue offered two fixes:
move the recall-cache check ahead of the embed, or cache query embeddings by
(text, contract). Only the LRU shipped. The reorder alone skips nothing on a
vector-capable store: `Daemon::recall_with` never caches or serves a
vector-dependent pipeline (P1-2, `can_cache = embedding.is_none()`), and every
SQLite session with an embedder is vector-dependent. Making vector pipelines
cacheable would need the vector gather (which runs before the graph lock) and
the cache key's epoch (read under it) to describe one graph snapshot, plus a
rule for store sources whose answers the epoch cannot see (write-behind lag,
Postgres). That is real complexity for what is left after the LRU: the
in-graph scan (about 3 ms at 3,600 concepts, #8) on read-only repeats only.
The LRU removes the embed, which since #8 is most of a warm recall, and it
hits even when a write lands between two identical recalls, which on a live
rig is the common case. Measured below: a repeated recall is 4.1 ms with the
LRU; a reorder could at most take the scan out of that.

**Key: the exact query text; the contract is checked per entry; no epoch.** A
query vector is a function of the text and the embedder, and of nothing in the
graph, so the epoch has no business in this key (it is what made the issue's
hit rate "near zero on a live rig"). The text is matched exactly, with no
trimming or case folding: real embedders see the raw string, and the fixture's
normalisation is a fixture property. The full text is the key, not a hash, so
a collision cannot serve another query's vector. The embedding contract is
stored with each entry and compared on lookup; a mismatch is a miss and the
next insert replaces it. Within one `Memory` the contract cannot change (it is
fixed at build; `lambo re-embed` migrates a session through its own attach, and
the serving process reattaches with a fresh `Memory` and an empty cache), so
the check is a guard for any future holder whose contract can change rather
than a path exercised today. It is pinned by a unit test.

**Per session, inside `Memory`** (#32 decision 13). Two sessions in one process
share the embedder instance but never this cache, so one session cannot learn
from reply timing that another ran the same query. Pinned by
`sessions_do_not_share_query_embeddings`; a mutation to a process-wide static
turns it red.

**After `ensure_open`** (#23). The lookup happens where the embed did, after
`ensure_open`, so a closed handle or one fenced by an erase refuses the recall
before the cache is read. Pinned for both (`simulate_lease_loss_to(ERASED_HOLDER)`
for the erase). The cache goes with the handle; there is nothing process-wide to
scrub on erase.

**Failed embeds are not cached; cached vectors survive an outage.** A failure
returns the warning line as before (the `vector_degraded` path is untouched)
and leaves no entry, so the next recall tries again. A vector cached before an
embedder outage keeps being served, so a repeated query keeps its vector leg
through the outage. That is the one observable difference from no cache, and it
is in the cached recall's favour: the vector is the one the embedder returned
for that text under that contract. Pinned on both sources and stated in the
CHANGELOG.

**Determinism.** "Identical results with and without the cache" assumes the
embedder returns the same vector for the same text, which the fixture does and
the shipped embedders do in practice. For an embedder that is not
deterministic, the cache returns one of its outputs and pins it until eviction;
without the cache each recall would have drawn a fresh one.

**Concurrency.** The cache sits behind a `parking_lot` mutex taken twice per
miss (lookup, insert) and once per hit, never across the embed's `.await`, so
concurrent recalls on one session do not wait on each other's embeds. Two
concurrent misses for the same text both embed and the second insert replaces
the first with an equal vector; a single-flight would save one embed in a rare
race and is not worth a second await point. The recall cache's tokio mutex is
still taken afterwards, unchanged.

**Bounded memory.** 128 entries and 1 MiB per session, whichever binds first,
least recently used out (a monotonic tick, the same scheme as `RecallCache`).
Each entry is charged its query bytes, `4 * dim` for the vector, the
contract's `kind` and `model` strings, and a fixed 160 bytes for the map slot
and headers. At 1,024 dimensions a short query costs about 4.2 KiB, so the
entry cap binds at about 540 KiB per session; a 16 KiB query (the MCP argument
cap) costs about 20 KiB, so about 50 of those fit in the 1 MiB budget. An entry
larger than the whole budget is not cached. The bounds are module constants,
not config: no knob was asked for, and `[serve]` config parsing is #32's.

**`mutation_epoch` stays in the recall-cache key** (acceptance 3). The pipeline
is a function of the graph, so a write must invalidate it. The epoch already is
the "per-session write generation that read-only recalls do not advance" the
issue asked about: since #30, read accesses append to the write-behind log
without bumping it. What made the epoch coupling expensive was that the query
embed sat behind it, and the LRU removes that coupling.

**`lambo recall` is unchanged.** It is one recall per process; it still calls
`candidates::embed_query` directly.

## Tests

`src/memory/tests/query_cache.rs`, each on both vector sources a holder can
have: the store's checked read (`VectorSearchStore::new`) and the holder's own
graph (`VectorSearchStore::graph_ranked`, which declares `exact_vector_scan`,
so `for_holder` picks `VectorCandidates::Graph` as it does on SQLite). The
"uncached" twin of a recall is the same query on the same handle after
`clear_query_embeddings`, compared on hits (id, score, content), per-leg
provenance and warnings.

| case | test |
|---|---|
| identical repeat skips the embed, vector leg still fires (acceptance 1) | `an_identical_repeated_recall_skips_the_embed` |
| a derive between recalls: no embed, the new concept is seen, equals uncached | `a_write_between_recalls_reuses_the_vector_and_sees_the_write` |
| a retraction between recalls: the concept is gone, equals uncached | `a_retraction_between_recalls_matches_an_uncached_recall` |
| different `top_k` and depth share one embed, each equals its uncached answer | `different_recall_parameters_share_one_embed` |
| two texts alternate, each answered with its own vector | `distinct_queries_are_answered_with_their_own_vectors` |
| failed embed not cached; cached vector survives an outage | `a_failed_embed_is_not_cached` |
| closed and erased handles refuse before the cache | `a_closed_or_erased_session_never_answers_from_the_cache` |
| 8 concurrent identical recalls agree; afterwards a hit | `concurrent_identical_recalls_agree` |
| per session (#32 decision 13) | `sessions_do_not_share_query_embeddings` |
| no vector leg: no embed, no entry | `no_vector_leg_means_no_embed_and_no_entry` |

The LRU's own tests (`recall::query_cache::tests`, 6) cover exact-text keys, the
contract check, the entry cap, the byte budget and its accounting on replace
and clear, the oversized entry, and the default bounds at BGE width.

GC is not tested separately: a GC sweep is a graph mutation like the derive and
the retraction above, and the query vector does not read the graph. A contract
change is covered by the unit test and, at the `Memory` level, by construction
(a new contract means a new `Memory`).

Mutation checks (each turned the named tests red, then restored):

- cache always misses: 8 of 10 `Memory` tests red (all but the closed/erased and
  no-vector-leg tests, which do not depend on a hit);
- `ensure_open` dropped from `recall_detailed`:
  `a_closed_or_erased_session_never_answers_from_the_cache` red;
- lookup ignores the text (returns any entry): `distinct_queries_...` red;
- a process-wide static cache instead of the field: `sessions_do_not_share_...`
  red.

## Performance

A/B on this Mac (Apple M3 Pro, macOS, Metal), release builds with
`store-sqlite,embed-candle-metal` (default features besides), base `b0e8207`,
head `994ba39` (the behaviour commit; later commits are tests and docs). The
real embedder: candle BGE-M3 f16 from the local hf-hub cache (`offline`,
`weights_dir`), on Metal. Store: a scratch copy of the 3,600-concept SQLite
session #8 measured on (`perf-ab-26`), re-embedded once with candle
(`lambo re-embed`, 97 s), a fresh `.backup` per run with lease rows cleared.
`lambo serve --transport http` on a private port (7741) with a private
`XDG_RUNTIME_DIR`, keep-warm off (`LAMBO_EMBED_KEEP_WARM_SECS=0`) so no probe
lands inside a timed call. Per run: 3 warmup recalls; **repeat**: one untimed
recall of a fixed query, then 30 timed recalls of it; **novel**: 30 timed recalls
of distinct queries; **write**: one untimed recall of a second fixed query, then
15 times (a one-concept derive, waited to applied, then a timed recall of it).
Four runs each, alternating order. Times are client-side MCP round trips over
loopback.

| | base p50 | head p50 | head p95 | run p50s, base | run p50s, head |
|---|---|---|---|---|---|
| repeat (n=120) | 22.26 ms | 4.05 ms | 4.37 ms | 21.90-22.80 | 3.84-4.18 |
| novel (n=120) | 21.89 ms | 21.83 ms | 25.62 ms | 21.54-22.37 | 21.58-22.99 |
| repeat after a write (n=60) | 21.52 ms | 3.55 ms | 4.45 ms | 21.18-23.25 | 3.51-3.62 |

The novel recall is unchanged (a miss still embeds), so the query embed for
these 8-10 word queries is about 18 ms of a 22 ms warm recall, and a repeated
recall comes in under a novel one by about 17.8 ms, the embed cost
(acceptance 2). The write column is the case the issue cared about: with a
derive applied between every pair, base never hit and head always does.

Answers: the repeat phase's 30 outputs were compared between base and head run
1 (same seed store, same query order). They differ only in the
`Session inactive N seconds` warning (wall-clock since the store's last
interaction, so it differs by run start time, and it drops out of base's later
answers once the daemon applies the first accesses; head's 30 recalls finish
before that). Hits, ranks, scores and ids are identical.

Caveats:

- One host, one store size (3,600), one query length class. The live dogfood
  writer was running on the same Mac (its own Metal embedder, idle).
- This is not "the isolated Metal probe" of the issue's measurement (837
  concepts, the dogfood store); it is a scratch store on the same class of
  machine. The live store was not touched.
- The 3.6-4.1 ms that remains is the MCP round trip, the in-graph scan (about
  3 ms, #8) and the pipeline; the reorder would only reach the scan.

Harness and raw samples: the run's scratch directory (`ab14.py`, `raw-*.json`,
`pooled.txt`); not committed.

## Not done, and why

- The recall-cache reorder (above).
- Config knobs for the bounds: none asked for; #32 owns `[serve]` parsing.
- Clearing the cache on `close()`: a closed handle refuses every read before the
  cache, and the cache is dropped with the `Memory`; the recall cache is treated
  the same way.
