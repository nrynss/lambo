# #181: redundancy-aware selection within top_k

Measures `[selection] skip_redundant` (rule (a), skip if redundant) against
current selection, and two offline comparisons, MMR and facility location,
on the #190 evaluation set (`fixtures/eval/`) under BGE-M3 (candle, CPU,
`BAAI/bge-m3@5617a9f6`). It also replays the 2026-09-07 efficacy run's
duplicate pairs and times recall with the rule off and on.

**What ships.** `skip_redundant = true` on its own runs the arm named
`content`: identical normalized content only (NFC, invisible codepoints
stripped, whitespace collapsed, case kept). The `Semantic`-edge and cosine
signals run only under `similar_signals = true`, off by default; their arm
is `a@0.85` (BGE-M3's merge threshold). Owner decision, 2026-10-28, on the
archived run below.

Two runs are here:

- `runs-2026-10-28/`: after every review fix in this series (the
  `similar_signals` switch, the stricter recall-side claim guard, case kept
  in the same-content signal, the per-recall verdict memo, readers on the
  writer's threshold). **The numbers in Summary and Results are this run.**
- `runs/`: the archived run, made before the review (all three signals on
  by default, the older guard, lowercased content). Its numbers are the
  basis of the owner decision and are kept in [Archived run](#archived-run-before-the-review)
  only; they predate every change above.

## Summary (`runs-2026-10-28/`)

- **The shipped default (`content`) on duplicates.** With an exact copy and
  a labelled rewording injected for every corpus concept that has one
  (`dup`), it raises the distinct memories in the rendered context from
  1.44 to 2.05 (dev) and 1.45 to 2.03 (held-out) at `top_k = 3` under the
  default 500-token budget, and facet coverage from 0.830 to 0.849 (dev)
  and 0.840 to 0.881 (held-out). At `top_k = 5` coverage goes from 0.849
  to 0.885 (dev) and 0.881 to 0.897 (held-out). Over the 8 `dup` cells
  (split × `top_k` × budget) it covers 48 more facets than `current`.
- **No false suppression.** On the clean corpus, with labelled
  related-but-distinct partners (`contrast`) and with declared
  contradictions (`contrast-declared`), `content` renders exactly what
  `current` renders, on every query.
- **`similar_signals` now adds almost nothing.** `a@0.85` skips no
  partner on `contrast`: the archived run's five merged pairs still carry
  their `Semantic` edges, and the stricter claim guard now rejects every
  one. On `dup` its edge and cosine skips at `top_k = 3` fell from 46 and
  46 (archived) to 8 and 8, mostly because the guard now reads those
  rewordings as different claims (not isolated from the other changes).
  It covers 2 fewer facets than `content` over the 8 `dup` cells (one dev
  cell at `top_k = 5`) and the same on `contrast`.
- **MMR and facility location do not beat `content`.** MMR at its
  dev-chosen lambda (0.5) renders more distinct memories on `dup` (2.40 /
  2.39 at `top_k = 3`) but lower coverage (0.824 / 0.856); facility
  location renders the most (2.93 / 2.92) at 0.830 / 0.887.
- **2026-09-07 replay.** The same-content signal (now case-kept) takes the
  two duplicate queries (`missing-downloads`, `record-action-embedding`)
  from 2 to 3 distinct concepts in the top 3; the other ten are unchanged.
  Re-run on 2026-10-28 with case kept: output identical to
  `hitk-replay.txt`.
- **Latency.** In process, at `top_k = 5` (window 20), with every signal
  on (the costlier configuration): +4 to +5 µs at p50 on a 0.14 ms recall
  when nothing is redundant, +27 to +41 µs (2 to 3%) on a 1.34 ms recall
  when every query skips (`latency-2026-10-28.txt`).

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
- `content`: rule (a) with the same-content signal only (the issue's arm
  2), **the shipped default**.
- `a@T`: rule (a) with every signal: same normalized content, a `Semantic`
  edge, cosine `>= T`, and the vetoes (a `Contradicts` edge either way; the
  recall-side claim guard, `recall::redundancy::distinct_claims`).
  `a@0.85` is what `similar_signals = true` runs under BGE-M3, whose merge
  threshold is 0.85 (no #87 row).
- `mmr@L`: `L · rel(i) − (1 − L) · max_{j∈S} sim(i, j)`, `rel` the final
  score over the window's best, `sim` 1 for the same content, else the
  cosine, else 0. Lambda chosen on dev only (`summarise.py`).
- `fl`: facility location as PACMS defines it (arXiv:2606.20047, §3.1):
  `w_ij = rel(i) · max(0, cos(e_i, e_j))`, `F(S) = Σ_i max_{j∈S} w_ij` over
  the window, relevance weight on the covered item `i`, greedy.

`mmr` and `fl` are gated like rule (a) with every signal: when its detector
at 0.85 finds no redundant pair in the window, they return `current`.
Neither is a shipped code path.

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

**Validation.** Two more copies of each store are opened with the rule on,
one under the shipped default and one with `similar_signals = true`; each
copy's `top_k = 20` recall (window 20) must start with the offline replay
of its arm (`content`, `a@0.85`) on the same window. Both do on all 262
queries of every store, including the 262 `dup` queries where the rule
skipped something, so the offline replay is the shipped code's behaviour on
this data, at window 20.

## Results (`runs-2026-10-28/`)

Generated by `summarise.py` from `runs-2026-10-28/bge-m3-candle-cpu.json`
(per-query windows, roles and scores only, in
`runs-2026-10-28/bge-m3-candle-cpu.windows.jsonl`). Every vector was
embedded fresh by the candle CPU embedder for this run (no external cache),
which moves `current` slightly from the archived run (for example `dup` at
`top_k = 3`, dev, 0.836 to 0.830): the stores were re-derived and re-ranked.

### Validation

Agree / queries (queries with a skip).

| store | concepts | Semantic edges | Contradicts edges | shipped default (`content`) | `similar_signals` (`a@0.85`) |
|---|---:|---:|---:|---:|---:|
| clean | 100 | 0 | 0 | 262/262 (0) | 262/262 (0) |
| dup | 304 | 143 | 0 | 262/262 (262) | 262/262 (262) |
| contrast | 200 | 5 | 0 | 262/262 (0) | 262/262 (0) |
| contrast-declared | 200 | 0 | 101 | 262/262 (0) | 262/262 (0) |

### Facet coverage, budget 500 tokens

dev / held-out, and in brackets distinct memories rendered per query.

| store | k | current | content (shipped default) | a@0.85 (similar_signals) | mmr@0.5 | fl |
|---|---:|---:|---:|---:|---:|---:|
| clean | 3 | 0.812 / 0.876 [2.88 / 2.88] | same | same | same | same |
| clean | 5 | 0.836 / 0.897 [3.94 / 3.85] | same | same | same | same |
| dup | 3 | 0.830 / 0.840 [1.44 / 1.45] | 0.849 / 0.881 [2.05 / 2.03] | 0.849 / 0.881 [2.07 / 2.04] | 0.824 / 0.856 [2.40 / 2.39] | 0.830 / 0.887 [2.93 / 2.92] |
| dup | 5 | 0.849 / 0.881 [2.24 / 2.19] | 0.885 / 0.897 [2.86 / 2.93] | 0.879 / 0.897 [2.92 / 3.00] | 0.885 / 0.881 [3.33 / 3.25] | 0.842 / 0.897 [3.99 / 3.93] |
| contrast | 3 | 0.741 / 0.820 [2.95 / 2.97] | same | same | same | same |
| contrast | 5 | 0.794 / 0.866 [4.63 / 4.56] | same | same | same | same |
| contrast-declared | 3 | 0.741 / 0.809 [2.95 / 2.97] | same | same | same | same |
| contrast-declared | 5 | 0.794 / 0.866 [4.63 / 4.57] | same | same | same | same |

"same": identical to `current` on every query. At 250 tokens two blocks
fit, so `k` matters less; `dup` at `k = 3` goes from 0.758 / 0.768 to
0.770 / 0.789 (`content`) with distinct memories 1.19 / 1.12 to 1.50 /
1.38, and `contrast` is unchanged for every arm (the full tables are in
`summarise.py`'s output).

### False suppression, `contrast`, budget 500

0 in every family, at both `k`, in both splits, for every arm (and 0 on
`contrast-declared`). `mmr` and `fl` are 0 because their gate, rule (a)'s
detector with every signal, finds no redundant pair in any `contrast`
window under the stricter guard, so they return `current` there.

What each arm skipped, both splits: `content` on `dup` only copies of the
kept fact (`top_k = 3`: 230, all same content); `a@0.85` on `dup` 233 same
content, 8 `Semantic` edge, 8 cosine, all copies; nothing on `contrast` or
`contrast-declared` for either.

### Tuning (dev only)

- MMR lambda over 0.5 to 0.9 by mean dev facet coverage across every
  store, `k` and budget: 0.7555, 0.7552, 0.7544, 0.7548, 0.7548; 0.5
  chosen (the archived run chose 0.9; `sim` and the gate both changed with
  case kept and the stricter guard).
- Rule (a)'s cosine threshold is not tuned: it is the session's merge
  threshold by design. The sweep (0.85, 0.90, 0.95) gives identical
  coverage on `dup` and `contrast` at budget 500.

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
replayed: the published files carry neither edges nor vectors. Re-run on
2026-10-28 with the case-kept normalization against the lambo-paper
checkout: output identical.

### Latency (`latency-2026-10-28.txt`)

`examples/bench_redundancy`: two in-process sessions (memory store, fixture
embedder, 1024 dims) from the same 2,000 writes, 40 queries, 25 rounds,
`top_k = 5`, alternating which setting runs first; "on" is
`skip_redundant` with `similar_signals`. Three runs:

| store | off p50 / p90 ms | on p50 / p90 ms | queries with a skip |
|---|---:|---:|---:|
| distinct (nothing redundant) | 0.138-0.147 / 0.149-0.164 | 0.143-0.151 / 0.155-0.169 | 0 / 40 |
| duplicated (one text in four stored twice) | 1.332-1.356 / 1.442-1.491 | 1.365-1.386 / 1.476-1.520 | 40 / 40 |

These recalls skip the vector leg (the memory store has none), so they
time assembly over a phase-1 result; an end-to-end recall over MCP is
milliseconds (#171: 7.5 ms p50), where this difference is noise. The
archived `latency.txt` (before the review fixes) measured +4 to +6 µs and
+8 to +18 µs on the same machine.

## Archived run (before the review)

`runs/bge-m3-candle-cpu.json`, `latency.txt` and the README of commit
62480a66. All three signals were on by default (`a@0.85` was the shipped
arm), the claim guard was the write path's `same_identifiers` plus a
negation count, and the same-content signal lowercased. These numbers
predate every fix in this series.

- `a@0.85` raised `dup`'s distinct memories at `top_k = 3`, budget 500,
  from 1.45 to 2.18 (dev) and 2.31 (held-out); `content` reached 2.04 /
  2.03 with coverage 0.849 / 0.881.
- On `contrast`, `a@0.85` dropped a partner in 4 of 37 dev and 4 of 20
  held-out polarity windows, 1 of 15 and 1 of 9 exception windows, and 1 of
  11 and 2 of 7 quantity windows (`top_k = 3`), every one by the
  `Semantic`-edge signal: hybrid derive had merged five anchor and partner
  pairs at write time (`p-can-02-rbd1`, `p-can-02-rbd2`, `p-can-03-rbd1`,
  `p-con-05-rbd1`, `p-pro-01-rbd1`). Held-out facet coverage fell from
  0.820 to 0.789. `content` changed nothing on `contrast`.
- **The owner decision's numbers.** Against `content`, `a@0.85` covered 3
  more facets over the 8 `dup` cells (split × `top_k` × budget: +3, +1,
  +1, −2) and 42 fewer over the 8 `contrast` cells. So `content` ships and
  the similarity signals sit behind `similar_signals`, off by default.
- On `contrast-declared` no arm suppressed anything. That was mainly the
  write path refusing to merge a fact into the one it is written `against`
  (the store has no `Semantic` edge), not recall's `Contradicts` veto,
  which could only act through the cosine signal there.

## Caveats

- **One embedder.** BGE-M3 only. EmbeddingGemma 2 (merge threshold 0.93) was
  not run.
- **The splits are not held-out topics.** Dev and held-out are a hash of
  the query id over the same stores: every topic is in both, and every
  partner pair sits in the store both splits query (the held-out pair
  `p-can-03-rbd1` caused suppressions in dev queries in the archived run).
  Rule (a) tunes nothing, so this barely matters for it, but it is not the
  held-out-topic protocol the issue asked for, and MMR's lambda is chosen
  on the same topics it is read on.
- **The gain is inflated.** `dup` adds 204 copies to 100 concepts, so every
  one of the 262 queries has something to skip; in the 2026-09-07
  efficacy run only 2 of 12 queries had a duplicate in the top 3. The
  copies and rewordings were written by the labeller of the originals
  (#190's known bias). The partners are short single-claim edits, close to
  the hardest case.
- **The coverage metric cannot see a dropped exception.** A partner covers
  no facet by design, so suppressing a needed exception or correction costs
  nothing in facet coverage; only the false-suppression counts see it.
- **Real-store sibling cosine was not measured.** Every concept here was
  derived on its own. A real multi-concept derive embeds each concept with
  the whole call's text, which raises the cosine between siblings; the
  dogfood store had 184 pairs at or above 0.85 with no `Semantic` edge, and
  under BGE-M3 35-44% of labelled related-but-distinct pairs score 0.85 or
  more. This matters only under `similar_signals`.
- **The `Contradicts` veto was barely exercised.** The declared store has
  no `Semantic` edges (the write path refuses those merges), so the veto
  could act only through the cosine signal. Its unit and merge-path tests
  cover it; this evaluation does not.
- **The replay was checked only at window 20.** The pipeline and the
  offline replay agree at `top_k = 20`; the arms at `k = 3` and `5` are
  offline replays on that window. A real `top_k = 3` recall asks the vector
  leg for 3 candidates, so its window of 12 holds fewer vector hits; the
  pipeline at `top_k = 3` was not measured separately.
- **Facet coverage is label-based.** A facet is credited from relevance
  grades (and, for grade 1, a word-overlap rule), not from reading the
  rendered text.
- **Undeclared contradictions are protected only by the text guard.** The
  `contrast` store is the measure of what the `Contradicts` veto cannot
  see. The stricter guard keeps all five merged pairs here, but it is a
  surface heuristic, and it now also keeps many genuine rewordings (it
  reads a reordered paraphrase as a different claim).

## Re-run

```bash
bash evidence/issue-181/run.sh                       # writes evidence/issue-181/runs-rerun/
PAPER=/path/to/lambo-paper bash evidence/issue-181/run.sh   # adds the 2026-09-07 replay
```

`runs-2026-10-28/` was produced with the same harness, per condition first
to fill an embedding cache (`--cache`, kept outside the repo), then in one
fully cached run of all four stores (4460 cache hits, 0 misses).
