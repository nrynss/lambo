# #173: reciprocal rank fusion of recall's legs

Measures `[fusion] kind = "max"` (lambo's ranking before #173 and still the
default) against reciprocal rank fusion on the #190 evaluation set
(`fixtures/eval/`), through the whole recall pipeline, under BGE-M3 (candle,
CPU) and EmbeddingGemma 2 (768 dims, Q8_0, `lambo-eg2-v2`, llama.cpp b11517).

## Method

`examples/eval_fusion` derives the 100 corpus concepts into a SQLite session,
one concept per derive (each self-framed, as a one-concept `lambo_derive`
stores it), 5 ms apart so the recent leg's three newest interactions are the
same on every run. For each variant it reopens a copy of that session with
the variant's `Config`, waits for the daemon's first cycle (so no recall is in
cold mode), and recalls every query with `top_k = 10` and the default
traversal depth. Repeating a run gives the same ranking metrics. The top scores
can differ in the fourth decimal between runs, because the daemon's recency
term reads the wall clock.

* **Ranking:** hit@k and MRR@10 on the first grade-2 concept, nDCG@10 with
  gain `2^grade − 1`, over answerable queries, per split and per family. The
  definitions are `eval_measure`'s, except that MRR stops at rank 10.
* **No answer:** the top hit's final score for answerable and no-answer
  queries. `no-answer AUC` is the chance an answerable query's top score beats
  a no-answer query's (ties half). `no-answer past gate` is the share of
  no-answer queries at or above a split-conformal gate at α = 0.10 tuned on
  `dev`'s answerable top scores.
* **Variants:** `max`, or `rrf` with `k=`, the RRF weights `keyword=`,
  `vector=`, `recent=` (default 1, 1, 0.3, as #173 proposes) and `wd=`, the
  daemon's share of the final score (`w_query = 1 − wd`; default 0.5).
  `wd=0` ranks on query relevance alone, the quantity #173's synthetic study
  scored.

**Selection, on `dev` only.** The sweep (`*-dev.json`) is run on `dev`, and
`k` is chosen by nDCG@10 with #173's weights: 3 (flat from 1 to 3 under both
embedders; BGE-M3 best at 3, EmbeddingGemma 2 at 2, within 0.003). `held_out`
was then run once (`*-confirm.json`) with `max`, the chosen `rrf` (k = 3, now
the default `k`) and #173's `rrf:k=60`, and no choice was revisited after it.
The weights were not tuned: `recent=0` scores higher on `dev`, but on this set
the recent leg always holds the corpus's last three concepts, so that is an
artefact of the set, not evidence about the leg.

## Results
### bge-m3-candle-cpu, dev

| variant | nDCG@10 | MRR@10 | hit@1 | hit@5 | hit@10 | no-answer AUC | no-answer past gate |
|---|---|---|---|---|---|---|---|
| `max` | 0.848 | 0.814 | 0.728 | 0.952 | 0.952 | 0.775 | 0.622 |
| `max:wd=0` | 0.849 | 0.817 | 0.728 | 0.952 | 0.952 | 0.773 | 0.635 |
| `rrf:k=0` | 0.874 | 0.841 | 0.752 | 0.960 | 0.968 | 0.706 | 0.770 |
| `rrf:k=1` | 0.891 | 0.863 | 0.792 | 0.960 | 0.968 | 0.697 | 0.716 |
| `rrf:k=10` | 0.835 | 0.777 | 0.632 | 0.960 | 0.976 | 0.637 | 0.730 |
| `rrf:k=100` | 0.683 | 0.582 | 0.408 | 0.824 | 0.976 | 0.560 | 0.865 |
| `rrf:k=2` | 0.891 | 0.859 | 0.784 | 0.960 | 0.984 | 0.697 | 0.784 |
| `rrf:k=20` | 0.766 | 0.682 | 0.488 | 0.952 | 0.976 | 0.602 | 0.838 |
| `rrf:k=3` | 0.893 | 0.860 | 0.784 | 0.960 | 0.984 | 0.682 | 0.730 |
| `rrf:k=3,keyword=0.5` | 0.891 | 0.860 | 0.792 | 0.952 | 0.976 | 0.684 | 0.635 |
| `rrf:k=3,recent=0` | 0.901 | 0.871 | 0.800 | 0.960 | 0.984 | 0.709 | 0.716 |
| `rrf:k=3,recent=0.6` | 0.876 | 0.840 | 0.752 | 0.952 | 0.976 | 0.656 | 0.757 |
| `rrf:k=3,vector=0.5` | 0.876 | 0.842 | 0.752 | 0.960 | 0.976 | 0.641 | 0.797 |
| `rrf:k=3,wd=0` | 0.904 | 0.874 | 0.800 | 0.976 | 0.992 | 0.731 | 0.649 |
| `rrf:k=3,wd=0.3` | 0.896 | 0.864 | 0.784 | 0.952 | 0.984 | 0.727 | 0.622 |
| `rrf:k=30` | 0.742 | 0.653 | 0.464 | 0.920 | 0.976 | 0.591 | 0.838 |
| `rrf:k=5` | 0.875 | 0.835 | 0.736 | 0.960 | 0.984 | 0.660 | 0.730 |
| `rrf:k=60` | 0.698 | 0.599 | 0.416 | 0.864 | 0.976 | 0.576 | 0.851 |
| `rrf:k=60,recent=0` | 0.712 | 0.616 | 0.440 | 0.880 | 0.976 | 0.590 | 0.865 |
| `rrf:k=60,wd=0` | 0.880 | 0.842 | 0.744 | 0.944 | 0.992 | 0.684 | 0.676 |

nDCG@10 by query family (n), dev:

| variant | exact-identifier | vocabulary-mismatch | contrast | fact-lookup | rationale |
|---|---|---|---|---|---|
| `max` | 0.923 (18) | 0.786 (5) | 0.822 (11) | 0.832 (80) | 0.988 (5) |
| `rrf:k=3` | 0.930 (18) | 1.000 (5) | 0.884 (11) | 0.877 (80) | 0.964 (5) |
| `rrf:k=60` | 0.811 (18) | 0.656 (5) | 0.796 (11) | 0.657 (80) | 0.778 (5) |

### bge-m3-candle-cpu, held_out

| variant | nDCG@10 | MRR@10 | hit@1 | hit@5 | hit@10 | no-answer AUC | no-answer past gate |
|---|---|---|---|---|---|---|---|
| `max` | 0.865 | 0.852 | 0.785 | 0.949 | 0.949 | 0.673 | 0.656 |
| `rrf` | 0.887 | 0.865 | 0.785 | 0.975 | 0.987 | 0.717 | 0.719 |
| `rrf:k=60` | 0.723 | 0.640 | 0.468 | 0.924 | 0.962 | 0.582 | 0.781 |

nDCG@10 by query family (n), held_out:

| variant | exact-identifier | vocabulary-mismatch | contrast | fact-lookup | rationale |
|---|---|---|---|---|---|
| `max` | 0.948 (11) | 0.775 (7) | 0.897 (12) | 0.853 (44) | 0.942 (3) |
| `rrf` | 0.899 (11) | 0.784 (7) | 0.859 (12) | 0.910 (44) | 0.942 (3) |
| `rrf:k=60` | 0.829 (11) | 0.623 (7) | 0.812 (12) | 0.692 (44) | 0.819 (3) |

### eg2-q8-768, dev

| variant | nDCG@10 | MRR@10 | hit@1 | hit@5 | hit@10 | no-answer AUC | no-answer past gate |
|---|---|---|---|---|---|---|---|
| `max` | 0.848 | 0.814 | 0.728 | 0.952 | 0.952 | 0.775 | 0.622 |
| `max:wd=0` | 0.849 | 0.817 | 0.728 | 0.952 | 0.952 | 0.773 | 0.635 |
| `rrf:k=0` | 0.885 | 0.855 | 0.776 | 0.960 | 0.976 | 0.715 | 0.649 |
| `rrf:k=1` | 0.890 | 0.859 | 0.776 | 0.968 | 0.976 | 0.719 | 0.635 |
| `rrf:k=10` | 0.823 | 0.767 | 0.616 | 0.952 | 0.976 | 0.668 | 0.743 |
| `rrf:k=100` | 0.659 | 0.549 | 0.376 | 0.824 | 0.976 | 0.595 | 0.797 |
| `rrf:k=2` | 0.892 | 0.860 | 0.776 | 0.968 | 0.976 | 0.719 | 0.635 |
| `rrf:k=20` | 0.758 | 0.675 | 0.496 | 0.944 | 0.976 | 0.642 | 0.797 |
| `rrf:k=3` | 0.889 | 0.855 | 0.768 | 0.960 | 0.976 | 0.696 | 0.622 |
| `rrf:k=3,keyword=0.5` | 0.891 | 0.859 | 0.776 | 0.960 | 0.976 | 0.713 | 0.568 |
| `rrf:k=3,recent=0` | 0.907 | 0.878 | 0.808 | 0.968 | 0.984 | 0.717 | 0.608 |
| `rrf:k=3,recent=0.6` | 0.871 | 0.832 | 0.736 | 0.960 | 0.976 | 0.682 | 0.689 |
| `rrf:k=3,vector=0.5` | 0.867 | 0.828 | 0.728 | 0.960 | 0.976 | 0.655 | 0.757 |
| `rrf:k=3,wd=0` | 0.913 | 0.881 | 0.792 | 0.984 | 0.992 | 0.705 | 0.595 |
| `rrf:k=3,wd=0.3` | 0.904 | 0.874 | 0.784 | 0.976 | 0.984 | 0.723 | 0.608 |
| `rrf:k=30` | 0.725 | 0.631 | 0.440 | 0.944 | 0.976 | 0.638 | 0.797 |
| `rrf:k=5` | 0.875 | 0.834 | 0.728 | 0.960 | 0.984 | 0.685 | 0.649 |
| `rrf:k=60` | 0.683 | 0.579 | 0.400 | 0.848 | 0.976 | 0.603 | 0.797 |
| `rrf:k=60,recent=0` | 0.691 | 0.588 | 0.408 | 0.880 | 0.984 | 0.630 | 0.811 |
| `rrf:k=60,wd=0` | 0.900 | 0.864 | 0.768 | 0.976 | 0.992 | 0.681 | 0.608 |

nDCG@10 by query family (n), dev:

| variant | exact-identifier | vocabulary-mismatch | contrast | fact-lookup | rationale |
|---|---|---|---|---|---|
| `max` | 0.923 (18) | 0.786 (5) | 0.822 (11) | 0.832 (80) | 0.988 (5) |
| `rrf:k=3` | 0.918 (18) | 0.900 (5) | 0.884 (11) | 0.882 (80) | 0.956 (5) |
| `rrf:k=60` | 0.811 (18) | 0.699 (5) | 0.714 (11) | 0.653 (80) | 0.670 (5) |

### eg2-q8-768, held_out

| variant | nDCG@10 | MRR@10 | hit@1 | hit@5 | hit@10 | no-answer AUC | no-answer past gate |
|---|---|---|---|---|---|---|---|
| `max` | 0.865 | 0.852 | 0.785 | 0.949 | 0.949 | 0.673 | 0.656 |
| `rrf` | 0.881 | 0.856 | 0.759 | 0.962 | 0.987 | 0.692 | 0.625 |
| `rrf:k=60` | 0.711 | 0.624 | 0.443 | 0.911 | 0.975 | 0.538 | 0.750 |

nDCG@10 by query family (n), held_out:

| variant | exact-identifier | vocabulary-mismatch | contrast | fact-lookup | rationale |
|---|---|---|---|---|---|
| `max` | 0.948 (11) | 0.775 (7) | 0.897 (12) | 0.853 (44) | 0.942 (3) |
| `rrf` | 0.881 (11) | 0.845 (7) | 0.872 (12) | 0.893 (44) | 0.942 (3) |
| `rrf:k=60` | 0.813 (11) | 0.606 (7) | 0.824 (12) | 0.687 (44) | 0.626 (3) |

For reference, the vector leg alone (#202's bare cosines, self-framed,
`evidence/eval-set/runs/bge-m3-candle-cpu.json`) scores nDCG@10 0.893 on
`dev` and 0.850 on `held_out` under BGE-M3.

## What it shows

* **`max` is the keyword leg.** `max` gives identical numbers under BGE-M3 and
  EmbeddingGemma 2, on both splits, because BM25 above 1 outranks every
  cosine: the vector leg only orders hits no keyword matched. On `dev` the
  pipeline under `max` (0.848) is below its own vector leg (0.893).
* **RRF at k = 3 gains, but less than #173's bar.** nDCG@10 +0.045 (BGE-M3)
  and +0.041 (EmbeddingGemma 2) on `dev`; +0.022 and +0.016 on `held_out`.
  #173 asked for +0.05 and no class worse by more than 0.02.
* **Some classes get worse.** On `held_out`, exact-identifier queries fall
  from 0.948 to 0.899 (BGE-M3) and 0.881 (EmbeddingGemma 2), contrast queries
  from 0.897 to 0.859 and 0.872; on `dev`, rationale (5 queries) falls from
  0.988 to 0.964 and 0.956. Vocabulary-mismatch and fact-lookup queries gain
  the most (on `dev`, 0.786 to 1.000 and 0.900; fact-lookup 0.832 to 0.877
  and 0.882). The class counts are small (5 to 18 per split), so one query
  moves a class by 0.05 or more.
* **No-answer separation does not move consistently.** The AUC falls on `dev`
  (0.775 to 0.682 and 0.696) and rises on `held_out` (0.673 to 0.717 and
  0.692). Under every variant the α = 0.10 gate passes most no-answer queries
  (57% to 87%), so neither fusion gives a usable abstention score by itself;
  that is #174's problem, and #173's option B (calibrated per-leg
  probabilities) is the candidate for it.
* **k = 60 fails inside the daemon blend.** #173's study scored relevance
  alone. Normalised RRF at k = 60 puts a leg's first and tenth hit within about
  0.06 of each other (0.435 against 0.379 for a hit in one leg, all three
  legs active), and the daemon's half of the score differs between concepts
  by more than that, so the daemon decides the order: nDCG@10 0.698 (BGE-M3)
  and 0.683 (EmbeddingGemma 2) on `dev`. With the daemon's share removed
  (`wd=0`) k = 60 scores 0.880 and 0.900, close to the study. A small k keeps
  the gap between ranks large enough to matter. On this fresh store the daemon
  has little to go on (no accesses, one interaction per concept); a used
  store's scores spread more, which would likely make a large k worse still.

## Decision

The default stays `kind = "max"`: RRF is opt-in (`[fusion] kind = "rrf"`),
with `k = 3` and #173's weights as its defaults. The gain does not meet
#173's bar and two query classes lose. Both fused values and each hit's leg
ranks are recorded on every `--ledger` recall line (`fusion` on the hit), so
the switch can be evaluated on live traffic before a default changes.

## Re-run on the 368-query set (2026-10-27)

After the first run the owner asked for more labelled queries, mostly
vocabulary-mismatch (`dev` had 5). `fixtures/eval/authoring/t_more173.py`
adds 58 answerable queries (36 vocabulary-mismatch, 9 rationale, 13
contrast), appended so no earlier id or split moves: `dev` now has 165
answerable queries (30 vocabulary-mismatch), `held_out` 97 (18). The new
queries were written from the corpus text before this re-run, not from its
results. Same harness; `dev` swept first (k = 2, 3, 5, 10, 60 and
`k=3,wd=0`), then `held_out` once with the same three variants as before.
Files: `runs-v2/` (`run-v2.sh` repeats it into `runs-v2-rerun/`); tables from
`python3 summarise.py runs-v2`. Two `dev` queries were corrected after the
first pass and `dev` was re-run: `q-prf-01-3` guessed a release season the
corpus does not state, and `q-mg-03-3` asked why where the concept states
only the rule (it is now a contrast query). `held_out` was not re-run, as
none of its queries changed. The `-confirm` files' `split/dev` metrics
therefore predate those corrections: read `dev` from the `-dev` files.

| | `max` | `rrf` (k = 3) BGE-M3 | `rrf` (k = 3) EmbeddingGemma 2 |
|---|---|---|---|
| `dev` nDCG@10 | 0.790 | 0.854 (+0.064) | 0.849 (+0.059) |
| `dev` hit@1 | 0.679 | 0.746 | 0.721 |
| `held_out` nDCG@10 | 0.840 | 0.872 (+0.032) | 0.863 (+0.023) |
| `held_out` hit@1 | 0.742 | 0.773 | 0.742 |
| `dev` vocabulary-mismatch (30) | 0.446 | 0.657 | 0.641 |
| `held_out` vocabulary-mismatch (18) | 0.672 | 0.724 | 0.744 |
| `held_out` exact-identifier (11) | 0.948 | 0.899 | 0.881 |
| `held_out` contrast (14) | 0.912 | 0.879 | 0.864 |
| no-answer AUC, `dev` / `held_out` | 0.772 / 0.669 | 0.672 / 0.694 | 0.699 / 0.681 |

What holds and what moves:

* **The direction holds, the size grows.** The new vocabulary-mismatch
  queries are where `max` is weakest (0.446 on `dev`: BM25 has little to
  match and the vector leg only orders what it did not), so RRF's `dev` gain
  rises above #173's +0.05. On `held_out` it stays below it (+0.032, +0.023).
* **k = 3 still holds**: k = 2 and k = 3 are within 0.003 of each other on
  both embedders (k = 2 is the higher on EmbeddingGemma 2), and k = 5, 10
  and 60 are lower on both. `wd=0` is again a little higher on `dev`; it is not
  tried on `held_out`.
* **The same classes lose on `held_out`**: exact-identifier and contrast,
  both by more than 0.02; none of those exact-identifier queries and 2 of the
  14 contrast queries are new.
  Rationale now gains on `held_out` (0.916 to 0.978).
* **No-answer AUC** still falls on `dev` and rises a little on `held_out`.

The decision stands: the `dev` gain now clears the bar, but `held_out` does
not and two classes lose there, so the default stays `max`.

## Not measured

* How often the top hit comes from each leg: `Memory::recall` does not expose
  the legs. The ledger now records them for live traffic.
* Latency. RRF sorts each leg once more (L is about 4 × `top_k` keyword hits
  plus `top_k` vector hits), negligible beside the vector scan.
* Layered recall: a layer's hits keep their own session's phase 1 and the
  layered merge is unchanged (its floor, #204, reads the vector cosine and is
  independent of fusion).

## Reproduce

`run.sh` repeats the `runs/` variants and `run-v2.sh` the `runs-v2/` ones,
both on the current `fixtures/eval/`. They write to `runs-rerun/` and
`runs-v2-rerun/` (or `$OUT`) and refuse to overwrite the archived `runs/`
and `runs-v2/`. `runs/` was produced on the 310-query set (before
`t_more173.py`), so `run.sh` now gives the 368-query numbers instead.

```
CACHE_BGE=bge.jsonl CACHE_EG2=eg2.jsonl bash evidence/issue-173-fusion/run.sh
```

`run.sh` builds `examples/eval_fusion`, runs the `dev` sweep and the held-out
confirmation under both embedders (EmbeddingGemma 2 from a llama.cpp server at
`LAMBO_LLAMA_EMBED_URL`), and prints the tables above with `summarise.py`. The
caches are `eval_measure`'s embedding cache format and optional: misses are
embedded and appended. Each `runs/*.json` holds the metrics per variant, and
`runs/*.queries.jsonl` each query's top three concept ids and top score per
variant: ids, families and numbers only, no concept or query text, path, URL
or credential.
