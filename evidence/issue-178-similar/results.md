## Each labelled pair on its own (default parameters, cosine 0.85)

### dogfood-i3: 50 pairs

suggested 5: 5 duplicates, 0 distinct. **Precision 1.000**, recall 0.714.

| label | family | outcome |
|---|---|---|
| distinct | file/commit name | guard: below_jaccard 16, guard: digits_differ 20, guard: identifiers_differ 1, guard: no candidate (no shared LSH band, cosine below the floor) 5, not suggested 42 |
| distinct | related, distinct | guard: below_jaccard 1, not suggested 1 |
| duplicate | identical | suggested 5 |
| duplicate | paraphrase | guard: below_jaccard 1, guard: no candidate (no shared LSH band, cosine below the floor) 1, not suggested 2 |

### synthetic: 119 pairs

suggested 92: 89 duplicates, 3 distinct. **Precision 0.967**, recall 1.000.

| label | family | outcome |
|---|---|---|
| distinct | hash family | guard: below_jaccard 2, not suggested 2 |
| distinct | hex hash | guard: digits_differ 1, guard: no candidate (no shared LSH band, cosine below the floor) 1, not suggested 2 |
| distinct | host name | guard: digits_differ 2, not suggested 2 |
| distinct | issue number | guard: digits_differ 2, not suggested 2 |
| distinct | model version | guard: digits_differ 2, not suggested 2 |
| distinct | path | guard: digits_differ 1, guard: identifiers_differ 2, not suggested 3 |
| distinct | port | guard: digits_differ 2, not suggested 2 |
| distinct | quantity | guard: below_jaccard 1, guard: digits_differ 1, not suggested 2 |
| distinct | related, distinct | guard: below_jaccard 1, guard: no candidate (no shared LSH band, cosine below the floor) 2, not suggested 3, suggested 3 |
| distinct | rfc number | guard: digits_differ 2, not suggested 2 |
| distinct | runtime version | guard: digits_differ 2, not suggested 2 |
| distinct | version | guard: digits_differ 3, not suggested 3 |
| duplicate | deletion | suggested 30 |
| duplicate | doubled letter | suggested 30 |
| duplicate | transposition | suggested 29 |

## The synthetic texts as one store (171 concepts, text source)

61 suggestions from 244 LSH candidates: distinct (labelled) 1, distinct (unlabelled) 1, duplicate (variants of one phrase) 59. **Precision 0.967.**

- distinct (unlabelled): 'score cache is keyed by graph epoch' / 'reacll cache is keyed by graph epoch' (jaccard 0.718)
- distinct (labelled): 'the embedder truncates inputs over the token clamp' / 'the embedder refuses inputs over the token clamp' (jaccard 0.706)

guarded: {"accepted": 0, "below_jaccard": 36, "canonical": 0, "capped": 0, "contradicts": 0, "digits_differ": 22, "identifiers_differ": 2, "overlap": 121, "rejected": 0, "too_short": 0, "type_differs": 2}
