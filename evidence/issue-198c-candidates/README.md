# #198 part c: replacement candidates, sized

A hybrid derive's settled outcome lists, for each concept it created, the
existing concepts it most likely replaces (`src/graph/suspected/replacement.rs`,
owner decision of 2026-10-28). This directory holds the measurement behind
the two defaults: the **floor** (the write's effective merge threshold) and
the **cap** (3 per created concept).

## Defaults

| Setting | Value | Why |
|---|---|---|
| Floor | the merge threshold the write uses: 0.85 under BGE-M3, 0.93 under EmbeddingGemma 2 at 768 dims (#87's row), else a #176 derivation or the configured `semantic_match_threshold` | All four labelled real supersessions sit at 0.857 to 0.899. Lower floors add noise on real memory faster than they add predecessors (tables below). Tying the floor to the threshold reuses #87's per-contract calibration with no second table. |
| Cap | 3 per created concept | At the floor no write on the real store had a longer list, and the true predecessor always ranked first. |
| Source | the top 8 vector hits the write already reads (`VECTOR_CANDIDATE_LIMIT`) | A search over the whole session lists the same concepts at every floor from 0.78 up. |

No setting was added: the floor follows the threshold, and a write with no
candidates is unchanged.

## Public: the #211 pairs (`public-measure.txt`, reproducible)

In regime: both texts the same `Logic`, `Constraint` or `Observation` type,
not under review, labelled `supersedes` or `distinct_entity`, with a
self-framed BGE-M3 cosine (`../issue-198-trigger/cosines-bge-m3-self.json`).
Each pair is judged by `replacement::select`, the function the write path
calls: the second text's write with the first as its one vector hit. When
the identifier guard passes and the cosine reaches 0.85 the write merges
with the hit, which is then listed marked `merged`, as the write path lists
it. A listed `supersedes` pair is a found predecessor; a listed
`distinct_entity` pair is noise.

| Floor | Dev supersedes | Dev distinct | Held-out supersedes | Held-out distinct |
|---|---:|---:|---:|---:|
| 0.80 | 39/43 | 42/47 | 25/27 | 26/30 |
| 0.82 | 39/43 | 39/47 | 25/27 | 26/30 |
| **0.85 (shipped)** | **37/43** | **34/47** | **25/27** | **23/30** |
| 0.86 | 36/43 | 33/47 | 25/27 | 20/30 |
| 0.88 | 35/43 | 31/47 | 25/27 | 20/30 |

The eight supersessions not listed at 0.85 are all reworded pairs below it
(0.66 to 0.82); none is lost to a type refusal. No in-regime pair merges
at 0.85 (each pair at or above it names different identifiers), so these
counts are the same whether the merged-with concept is listed or not; the
measurement output's last line pins that (0 merged of every listed
count). These pairs are
authored, so their `distinct_entity` pairs are near-identical templates:
they show the floor cannot separate a sibling from a predecessor by cosine,
which is why the list is handed to the writer and nothing is recorded. A
pair has one predecessor, so list length is at most 1 here; length and the
rate on writes that replaced nothing come from real memory.

`tests/replacement_measure.rs` pins the 0.85 and 0.82 rows in CI.

## Private: the dogfood store (aggregates only)

A private copy of the dogfood store (session `lambo`, BGE-M3, 2,384 embedded
concepts, 1,103 of an eligible type), read without writing, and the 100
labelled guard-refused pairs at cosine 0.85 or more behind the owner
decision (4 `supersedes`, 2 `same-event`, 93 `distinct_entity`, 1 `unsure`).
Each concept's write was replayed against the concepts written before it,
with the write path's rules: top-8 hits, merge target skipped, same type,
eligible type, no `against`/`depends_on`. No concept text, id or per-pair
row leaves the private copy; the numbers below are all that is recorded.

**These aggregates predate two later changes and were not re-run against
the private store.** The list now includes the concept a write merged with
(marked `merged`) instead of skipping it, so a write that merged can list
one more candidate than the replay counted, and the length and non-empty
rates below are lower bounds for such writes. A candidate dated after the
write is now marked `newer` rather than read as a predecessor (see the true
predecessor paragraph). The labelled supersessions are guard-refused pairs,
which never merge, so the four found at rank 1 are unaffected.

**The true predecessor.** All 4 labelled supersessions are listed, at rank
1, on the later of the two writes (cosines 0.857 to 0.899). In 2
the replacing concept was written second, so its writer sees the
predecessor. In the other 2 the store holds the replacing record first (the
older decision was recorded later), so the pair surfaces on that later
write, naming the replacing concept. Such a candidate is dated after the
write that lists it, so it is now marked `newer` (it may replace that
write, and is not one to cite in `against`) rather than listed as a
predecessor. At a floor of 0.86, 3 of 4 remain; at 0.88, 1.

**Siblings are not candidates.** 91 of the 100 labelled pairs are two
concepts of one call. The write reads the store before it commits, so a
same-call sibling is never a hit and never listed; the label sample is
therefore not where the noise comes from.

**List length and noise** (every eligible concept's write, top 8, cap 3):

| Floor | Writes with a non-empty list | Length 0 / 1 / 2 / 3 |
|---|---:|---|
| 0.78 | 293 / 1,103 (26.6%) | 810 / 161 / 61 / 71 |
| 0.80 | 196 / 1,103 (17.8%) | 907 / 132 / 36 / 28 |
| 0.82 | 116 / 1,103 (10.5%) | 987 / 93 / 14 / 9 |
| **0.85 (shipped)** | **31 / 1,103 (2.8%)** | **1,072 / 28 / 1 / 2** |
| 0.86 | 16 / 1,103 (1.5%) | 1,087 / 14 / 1 / 1 |

Most writes replace nothing, so the non-empty rate is close to the rate on
writes that replaced nothing. On the labelled sample at 0.85 the newer
concept's list has length 0 / 1 / 3 for 89 / 10 / 1 pairs, and 6 of the 93
`distinct_entity` newer concepts have a non-empty list.

A whole-session search instead of the top 8 changes one write's list at
0.75 and none from 0.78 up.

## Caveats

* **Four real positives.** The real-memory recall figure rests on 4
  labelled supersessions, all at 0.857 or more. A reworded replacement
  below the merge threshold is not listed (the public misses are that
  shape).
* **EmbeddingGemma 2 is not measured.** Its floor is #87's calibrated merge
  threshold, the quantile equivalent of BGE-M3's 0.85 on related-but-distinct
  pairs, not a measurement of candidates.
* **Replay, not history.** The private replay uses each concept's stored
  vector (embedded with its call's text, as the write was) and the concepts
  still in the store; concepts collected since, and the vectors of their
  time, are not seen.
* **Direction is marked, not judged.** The list names close neighbours;
  which one replaces which is the writer's call. A candidate dated after
  the write (`suspected::dated_at`, the trigger's clamped about-time) is
  marked `newer`, so the writer is not steered to contradict the later
  fact; a tie is not marked. The public measurement writes both texts in
  one interaction, so none of its pairs is marked.

## Re-run

```text
cargo run --example replacement_measure > evidence/issue-198c-candidates/public-measure.txt
```
