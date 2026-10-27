# #181: redundancy-aware selection within top_k

Measures `[selection] skip_redundant` (rule (a), skip if redundant) against
current selection, an exact-duplicate-only control, and two offline
comparisons, MMR and facility location, on the #190 evaluation set
(`fixtures/eval/`) under BGE-M3 (candle, CPU, `BAAI/bge-m3@5617a9f6`). It
also replays the 2026-09-07 efficacy run's duplicate pairs and times recall
with the rule off and on.

## Summary

- **Duplicates.** With an exact copy and a labelled rewording injected for
  every corpus concept that has one (`dup`), the shipped rule raises the
  distinct memories in the rendered context from 1.45 to 2.18 (dev) and
  2.31 (held-out) at `top_k = 3` under the default 500-token budget, and
  facet coverage from 0.836 to 0.849 (dev) and 0.840 to 0.887 (held-out).
  At `top_k = 5` coverage goes from 0.849 to 0.879 (dev) and 0.881 to 0.897
  (held-out).
- **No redundancy, no change.** On the clean corpus and with declared
  contradictions (`contrast-declared`), every arm renders exactly what
  `current` renders, on every query.
- **False suppression comes from `Semantic` edges.** With labelled
  related-but-distinct partners and no declared contradiction (`contrast`),
  the shipped rule drops a partner pair member in 4 of 37 dev and 4 of 20
  held-out polarity windows, 1 of 15 and 1 of 9 exception windows, and 1 of
  11 and 2 of 7 quantity windows (`top_k = 3`). Every one of those skips is
  the `Semantic`-edge signal: hybrid derive merged five anchor and partner
  pairs at write time. Their texts swap two names (`warn` and `verify`,
  Opus and Fable), move which mode is the default, invert an exception, or
  give a value in words ("half of" against "equal to"), none of which the
  identifier and negation guards see. Held-out facet coverage falls from
  0.820 to 0.789. The
  `Contradicts` veto removes all of them once the contradiction is declared.
- **The exact-duplicate control gets most of the gain with no false
  suppression.** `content` (same-content signal only) reaches 2.04 / 2.03
  distinct memories and 0.849 / 0.881 coverage on `dup` at `top_k = 3`, and
  changes nothing on `contrast`. The edge and cosine signals add distinct
  memories on `dup` (2.18 / 2.31) but not coverage at `top_k = 5`, where
  `content` covers one more dev query's facets than the shipped rule (0.885
  against 0.879); the two differ in which replacement blocks and `also`
  items fit the budget, and that one query was not isolated further.
- **MMR and facility location do not beat the simple rule.** MMR at its
  dev-chosen lambda (0.9) barely moves `dup` (1.57 distinct memories);
  facility location renders the most distinct memories on `dup` (2.93) but
  lowers dev coverage there (0.830) and suppresses the most partners on
  `contrast` (six of the seven families).
- **2026-09-07 replay.** The same-content signal alone takes the two
  duplicate queries (`missing-downloads`, `record-action-embedding`) from 2
  to 3 distinct concepts in the top 3; the other ten are unchanged.
- **Latency.** In process, at `top_k = 5` (window 20): +4 to +6 µs at p50
  on a 0.135 ms recall when nothing is redundant, +8 to +18 µs (about 1%)
  on a 1.29 ms recall when every query skips.

## Method

`examples/eval_redundancy` derives four SQLite stores through `Memory`, one
concept per derive (self-framed, as one `lambo_derive` writes it):

| store | what is derived |
|---|---|
| `clean` | the 100 corpus concepts |
| `dup` | the corpus; for every corpus concept with a labelled paraphrase, an exact copy of its text under another type (an Observation written before a non-Observation anchor, an Entity after an Observation anchor: the canonical-key matcher skips Observations, which is how the efficacy run's Entity/Observation pairs arise), and each labelled rewording |
| `contrast` | the corpus and every labelled related-but-distinct partner of a corpus concept (polarity, exception, correction, repository, identifier, quantity, topic-neighbour) |
| `contrast-declared` | the same partners, each derived `against` its anchor |

For each of the 262 answerable queries, the **shared window** is the
session's own top 20 with the rule off (`top_k = 20`, a budget nothing
reaches). Every arm picks `k` hits from the first `min(20, 4k)` of that
window, so all arms see the same candidates, scores and canonical order (the
issue's controlled comparison):

- `current`: the first `k`.
- `content`: rule (a) with the same-content signal only (the issue's arm 2).
- `a@T`: rule (a) with every signal: same normalized content, a `Semantic`
  edge, cosine `>= T`, and the vetoes (a `Contradicts` edge either way;
  different identifiers, digits or negation count). `a@0.85` ships under
  BGE-M3, whose merge threshold is 0.85 (no #87 row).
- `mmr@L`: `L · rel(i) − (1 − L) · max_{j∈S} sim(i, j)`, `rel` the final
  score over the window's best, `sim` 1 for the same content, else the
  cosine, else 0. Lambda chosen on dev only (`summarise.py`).
- `fl`: facility location as PACMS defines it (arXiv:2606.20047, §3.1):
  `w_ij = rel(i) · max(0, cos(e_i, e_j))`, `F(S) = Σ_i max_{j∈S} w_ij` over
  the window, relevance weight on the covered item `i`, greedy.

`mmr` and `fl` are gated like the rule: when rule (a)'s detector finds no
redundant pair in the window, they return `current`. Neither is a shipped
code path.

**Rendered context.** Each picked hit renders as recall renders it (rule
(a)'s kept hits carry their `also <id>` items, which cost tokens), joined by
blank lines and cut to the budget by the longest whole-block prefix, with
recall's estimator. Budgets: 250 and 500 (the default) tokens.

**Primary metric.** Facet coverage: the share of the query's labelled
answer facets covered by the rendered context, averaged over queries. A
rendered hit covers every facet when it is a grade-2 concept of the query,
the facets holding at least 80% of their content words when it is a grade-1
concept, and the same as its anchor when it is an exact copy or labelled
rewording of one. A partner covers nothing. Also reported: distinct
memories rendered (copies of one fact count once; a partner is its own
memory).

**False suppression.** For each window where `current` renders both a
corpus concept and one of its labelled partners: the arm renders only one of
the two. Reported per family.

**Validation.** A second copy of each store is opened with the rule on; its
`top_k = 20` recall (window 20) must start with the offline replay of
`a@0.85` on the same window. It does on all 262 queries of every store,
including the 262 `dup` and 71 `contrast` queries where the rule skipped
something, so the offline replay is the shipped code's behaviour on this
data.

## Results

Generated by `summarise.py` from `runs/bge-m3-candle-cpu.json` (per-query
windows, roles and scores only, in `runs/bge-m3-candle-cpu.windows.jsonl`).

### Validation

| store | concepts | Semantic edges | Contradicts edges | queries | agree | with a skip |
|---|---:|---:|---:|---:|---:|---:|
| clean | 100 | 0 | 0 | 262 | 262 | 0 |
| dup | 304 | 143 | 0 | 262 | 262 | 262 |
| contrast | 200 | 5 | 0 | 262 | 262 | 71 |
| contrast-declared | 200 | 0 | 101 | 262 | 262 | 0 |

### Facet coverage, budget 500 tokens

dev / held-out, and in brackets distinct memories rendered per query.

| store | k | current | content | a@0.85 (shipped) | mmr@0.9 | fl |
|---|---:|---:|---:|---:|---:|---:|
| clean | 3 | 0.812 / 0.876 [2.88 / 2.88] | same | same | same | same |
| clean | 5 | 0.836 / 0.897 [3.94 / 3.85] | same | same | same | same |
| dup | 3 | 0.836 / 0.840 [1.45 / 1.45] | 0.849 / 0.881 [2.04 / 2.03] | 0.849 / 0.887 [2.18 / 2.31] | 0.836 / 0.840 [1.57 / 1.53] | 0.830 / 0.876 [2.93 / 2.92] |
| dup | 5 | 0.849 / 0.881 [2.22 / 2.21] | 0.885 / 0.897 [2.86 / 2.93] | 0.879 / 0.897 [3.07 / 3.26] | 0.861 / 0.881 [2.38 / 2.35] | 0.849 / 0.897 [4.01 / 3.94] |
| contrast | 3 | 0.741 / 0.820 [2.95 / 2.97] | 0.741 / 0.820 [2.95 / 2.97] | 0.741 / 0.789 [2.95 / 2.97] | 0.748 / 0.809 [2.95 / 2.97] | 0.729 / 0.830 [2.95 / 2.97] |
| contrast | 5 | 0.794 / 0.866 [4.64 / 4.56] | 0.794 / 0.866 [4.64 / 4.56] | 0.782 / 0.845 [4.64 / 4.55] | 0.794 / 0.856 [4.64 / 4.56] | 0.770 / 0.845 [4.62 / 4.48] |
| contrast-declared | 3 | 0.741 / 0.809 [2.95 / 2.97] | same | same | same | same |
| contrast-declared | 5 | 0.794 / 0.866 [4.63 / 4.57] | same | same | same | same |

"same": identical to `current` on every query. At 250 tokens two blocks fit,
so `k` matters less; `dup` at `k = 3` goes from 0.770 / 0.768 to 0.776 /
0.799 (shipped) with distinct memories 1.19 / 1.14 to 1.58 / 1.66, and
`contrast` from 0.669 / 0.768 to 0.663 / 0.737 (the full tables are in
`summarise.py`'s output).

### False suppression by family, `contrast`, budget 500

suppressed / windows where `current` renders both, dev | held-out.
`contrast-declared`: 0 in every cell, for every arm.

| family | k | content | a@0.85 (shipped) | mmr@0.9 | fl |
|---|---:|---:|---:|---:|---:|
| polarity | 3 | 0/37 \| 0/20 | 4/37 \| 4/20 | 1/37 \| 2/20 | 5/37 \| 4/20 |
| polarity | 5 | 0/56 \| 0/31 | 8/56 \| 7/31 | 1/56 \| 1/31 | 11/56 \| 10/31 |
| exception | 3 | 0/15 \| 0/9 | 1/15 \| 1/9 | 0/15 \| 1/9 | 1/15 \| 2/9 |
| exception | 5 | 0/22 \| 0/13 | 2/22 \| 1/13 | 0/22 \| 1/13 | 4/22 \| 4/13 |
| correction | 3 | 0/13 \| 0/7 | 0/13 \| 0/7 | 0/13 \| 1/7 | 0/13 \| 1/7 |
| correction | 5 | 0/16 \| 0/9 | 0/16 \| 0/9 | 0/16 \| 1/9 | 2/16 \| 3/9 |
| repository | 3 | 0/7 \| 0/1 | 0/7 \| 0/1 | 0/7 \| 0/1 | 2/7 \| 0/1 |
| repository | 5 | 0/11 \| 0/4 | 0/11 \| 0/4 | 0/11 \| 0/4 | 2/11 \| 2/4 |
| identifier | 3 | 0/3 \| 0/2 | 0/3 \| 0/2 | 0/3 \| 0/2 | 0/3 \| 0/2 |
| identifier | 5 | 0/4 \| 0/3 | 0/4 \| 0/3 | 0/4 \| 0/3 | 0/4 \| 0/3 |
| quantity | 3 | 0/11 \| 0/7 | 1/11 \| 2/7 | 1/11 \| 0/7 | 4/11 \| 2/7 |
| quantity | 5 | 0/19 \| 0/9 | 1/19 \| 2/9 | 0/19 \| 1/9 | 5/19 \| 4/9 |
| topic-neighbour | 3 | 0/11 \| 0/5 | 0/11 \| 0/5 | 0/11 \| 1/5 | 1/11 \| 1/5 |
| topic-neighbour | 5 | 0/18 \| 0/14 | 0/18 \| 0/14 | 0/18 \| 2/14 | 2/18 \| 4/14 |

What the shipped rule skipped, both splits: on `dup`, only copies of the
kept fact (`top_k = 3`: 245 same content, 46 `Semantic` edge, 46 cosine);
on `contrast`, only partners, all by `Semantic` edge (14 at `top_k = 3`, 20
at 5); on `contrast-declared`, nothing.

### Tuning (dev only)

- MMR lambda over 0.5 to 0.9 by mean dev facet coverage across every
  store, `k` and budget: 0.7518, 0.7522, 0.7537, 0.7559, 0.7559; 0.9 chosen
  (the tie goes to the larger lambda, nearer `current`).
- Rule (a)'s cosine threshold is not tuned: it is the session's merge
  threshold by design. The sweep (0.85, 0.90, 0.95) gives identical
  coverage on `dup` and `contrast` at budget 500: every partner skip is an
  edge, and the copies the cosine signal stops catching at 0.95 (dev,
  `top_k = 3`: 9 cosine skips against 23 at 0.85) do not change coverage.

### 2026-09-07 replay (`hitk-replay.txt`)

The 990-concept store copy of that run is not available, so the harness
was not re-run against it. `hitk_replay.py` replays the same-content signal
over the published top-20 lists (content cut to 120 characters) and the
full top-5 exports. In `missing-downloads` and `record-action-embedding`,
ranks 2 and 3 are one text as an Entity and an Observation; the full
exports confirm the two texts are byte-identical (374 and 565 characters).
At `top_k = 3` the replay keeps rank 2, skips rank 3 and shows rank 4; the
`embedder-superseded` pair at ranks 14 and 15 is outside the window of 12,
and nothing else changes. Covered@3 cannot drop under this signal: a skipped
member has the kept member's exact text, and every other member keeps or
improves its rank. The `Semantic`-edge and cosine signals could not be
replayed: the published files carry neither edges nor vectors.

### Latency (`latency.txt`)

`examples/bench_redundancy`: two in-process sessions (memory store, fixture
embedder, 1024 dims) from the same 2,000 writes, 40 queries, 25 rounds,
`top_k = 5`, alternating which setting runs first. Three runs:

| store | off p50 / p90 ms | on p50 / p90 ms | queries with a skip |
|---|---:|---:|---:|
| distinct (nothing redundant) | 0.133-0.135 / 0.141-0.142 | 0.138-0.139 / 0.146-0.148 | 0 / 40 |
| duplicated (one text in four stored twice) | 1.287-1.293 / 1.310-1.316 | 1.296-1.305 / 1.317-1.329 | 40 / 40 |

These recalls skip the vector leg (the memory store has none), so they
time assembly over a phase-1 result; an end-to-end recall over MCP is
milliseconds (#171: 7.5 ms p50), where this difference is noise.

## Caveats

- **One embedder.** BGE-M3 only. EmbeddingGemma 2 (merge threshold 0.93) was
  not run.
- **The window is the `top_k = 20` recall.** A real `top_k = 3` recall asks
  the vector leg for 3 candidates, so its window of 12 holds fewer vector
  hits than this one. The arms are compared on one window by design; the
  pipeline at `top_k = 3` was not measured separately.
- **Synthetic redundancy.** `dup` injects a copy and a rewording for every
  anchor that has one, far denser than a real store, and the rewordings
  were written by the labeller of the originals (#190's known bias). The
  partners are short single-claim edits, close to the hardest case.
- **Facet coverage is label-based.** A facet is credited from relevance
  grades (and, for grade 1, a word-overlap rule), not from reading the
  rendered text, and a partner is credited with nothing.
- **Undeclared contradictions are not protected.** The `contrast` table is
  the measure of what the `Contradicts` veto cannot see: related-but-distinct
  texts that hybrid derive merged at write time.

## Re-run

```bash
bash evidence/issue-181/run.sh                       # writes evidence/issue-181/runs-rerun/
PAPER=/path/to/lambo-paper bash evidence/issue-181/run.sh   # adds the 2026-09-07 replay
```
