# #178: labelled precision of `canon alias-preview --similar`

`labelled-pairs.json` holds 169 labelled pairs:

- **50 real pairs from the dogfood store** (`dogfood-i3`), every pair
  `evidence/mooshik-i3-live-ledger/duplicates.txt` prints in full, with the
  BGE-M3 cosine it measured: the 25 at or above the 0.85 merge threshold
  and the first 25 of the band below it. Labelled by hand. 7 are duplicates:
  5 identical texts stored twice and 2 paraphrases of one review verdict.
  43 are distinct: review files of different rounds, different source
  files, different commits, and related but different notes.
- **119 synthetic pairs.** 89 duplicates: a transposition, a deletion and a
  doubled letter in the longest word of 30 phrases from the lambo domain
  (case and separator variants are left out: the canonical key already
  unifies them). 30 distinct: every #148 / #178 identifier family (versions,
  RFC numbers, hash families, host names, issue numbers, ports, model and
  runtime versions, hex hashes, paths, quantities) and 6 related but
  different claims written to be lexically close.

The dogfood store itself is not on this machine, so the "top 100
suggestions on the dogfood copy" pass in #178's evaluation plan is the
owner's to run: `lambo canon alias-preview --store <copy> --session
lambo-dev --similar`. These pairs are the dogfood texts available here.

## Results (default parameters: Jaccard 0.7, 12 characters, cosine 0.85)

See `results.md` for the per-family table. In short:

| set | suggested | duplicates | distinct | precision | recall |
|---|---:|---:|---:|---:|---:|
| dogfood pairs, each on its own (text and vector) | 5 | 5 | 0 | **1.000** | 0.714 |
| synthetic pairs, each on its own (text) | 92 | 89 | 3 | **0.967** | 1.000 |
| synthetic texts as one 171-concept store (text) | 61 | 59 | 2 | **0.967** | n/a |

- **Zero identifier or version pairs suggested**, real or synthetic. In
  the dogfood set, the 20 review-file pairs at cosines 0.97 to 0.99 (`...J3-round1.md`
  against `...J3-round3.md`) are exactly what a vector merge at 0.85 would
  fold; the digit guard refuses every one, and the identifier guard refuses
  `src/mcp/serve.rs` against `src/mcp/server.rs`.
- **The dogfood misses** are the two paraphrases: different wording of one
  verdict (Jaccard below 0.7), whose digit runs also differ. Lexical
  similarity is not built to find those; the vector source finds them only
  when their digits match.
- **The false positives** are related claims that differ in one word:
  `score cache` / `recall cache`, `stage two` / `stage three`, `batches` /
  `drops`, `truncates` / `refuses`. Text similarity cannot tell these from a
  typo, which is why the tool suggests and never merges, and why the report
  prints both texts and `--overrides` records a rejection.

## Runtime

`similar_timing` (release build, 8 cores): 10,000 concepts, text source
0.37 s; with 1024-wide vectors on every concept, 1.74 s in all.

## Re-run

```bash
cargo test --release --features store-sqlite,fixtures --lib similar_precision_report \
    -- --ignored --nocapture | python3 evidence/issue-178-similar/report.py
cargo test --release --features store-sqlite,fixtures --lib similar_timing -- --ignored --nocapture
```
