# #157: general-layer admission sweep

The measurement behind the layer defaults for BGE-M3 (`[[serve.layer]]` `weight = 0.5`, `min_similarity = 0.50`) and behind the rule that an ordinary layer hit needs absolute vector cosine at or above the floor, so a keyword match does not bypass it. It is a record of one measurement on 2026-10-13, not a universal calibration: per-embedder floors come from `[embedder] calibration` (#87). The current behaviour is documented in `docs/reference/config.mdx` ("Layers").

## Setup

Measured from main `0ff27103` on an isolated scratch copy of a fresh SQLite online backup of the dogfood rig, which `lambo provision` migrated (the live rig was not touched). Embedder: BGE-M3 through candle, 1024 dimensions, Hugging Face revision `5617a9f61b028005a4858fdac845db406aefb181`. `lambo session verify` measured SQLite-holder behaviour with `top_k = 5`, `max_tokens = 500`, `traversal_depth = 2` and the fixed clock `2026-10-13T12:41:49.477Z`. The general layer contains 12 concepts.

The 37 project queries cover six consuming sessions. As positive probes, six general-rule queries were also asked from each of those sessions (36 probes, repeated across sessions rather than independent topics). A matching rule is the first origin the unlayered general answer returns for the same query: a retrieval reference, not an independent human relevance judgement. The split used provisional labels for 795 records added since the reviewed set (787 records across the plan are low confidence), so this is not a reviewed cut-over plan. Every verify run used `--aggregate-only endor`; no private query strings, concept contents or ids are published.

## Files

- `sweep.csv`: the full 30-configuration sweep (keyword-bypass grid: weights 0.50, 0.33, 0.25, 0.20 by floors 0.40 to 0.80; absolute-vector grid: weights 0.50, 0.33 by floors 0.40 to 0.60).
- `projects.csv`: baseline and selected per-project counts.

## Result

| Admission | Weight | Floor | Project recall@5 | Queries with general at k=5 | General share at k=5 | Matching rule at k=5 |
|---|---:|---:|---:|---:|---:|---:|
| Keyword bypass (old default) | .50 | .40 | .87 | 36/37 | 19.46% | 35/36 |
| Keyword bypass | .50 | .50 | .89 | 26/37 | 14.05% | 35/36 |
| Absolute vector (selected) | .50 | .50 | .91 | 3/37 | 1.62% | 35/36 |
| Absolute vector | .50 | .55 | .91 | 0/37 | 0% | 17/36 |

Project recall@5 is the verifier's retention of in-scope pre-split hits, not a human-judged score. Under keyword bypass, floors at or above 0.50 plateaued at 26/37 queries because keyword evidence passed even a 0.80 floor. The absolute 0.50 floor cut general intrusion to 3 of 37 queries while keeping 35 of 36 positive probes; 0.55 removed every ordinary general hit but lost about half the rule probes. The 500-token budget hid 14 of 36 matching-rule probes in both the old baseline and the selected setting, and the selection does not change that.
