# Evaluation set (#190)

Labelled queries and labelled text pairs drawn from the dogfood memory. They
are the shared yardstick for #87 / #176 (the `[embedder] related_pairs`
threshold), #173 (RRF fusion), #174 (conformal abstention), #175 (PPR
expansion), #181 (redundancy) and #198 (what a refused identifier merge is).

**Status: v0, labelled by an agent, pending owner adjudication.** Every item
says who labelled it (`labeller`, `labelled_on`). Items whose label needs the
owner's judgment carry a `review` note; they are kept out of the export and
out of every reported number until the owner settles them (see "Awaiting
owner review" below).

## Files

| File | What |
|---|---|
| `corpus.json` | The retrieval universe: 100 concepts `{id, topic, type, text, origin, sources}`. `origin` is `dogfood` (23 concepts, verbatim text of a dogfood-store concept) or `authored` (77 atomic facts restated from the repo's own records, each citing its source file). A query is "no-answer" relative to this corpus. |
| `queries.json` | 310 queries `{id, query, kind, family, topic, split, relevant[{concept, grade}], facets[], source, labeller, labelled_on, review?}`. 204 `answerable` (graded relevance 2 = answers it, 1 = partial or supporting; `facets` = what a correct answer must say, for #181) and 106 `no_answer`. |
| `pairs.json` | 403 pairs `{id, a, b, label, family, relation?, split, source, labeller, labelled_on, concepts?, review?}`: 173 `related_distinct`, 110 `paraphrase`, 120 `unrelated`. |
| `related_pairs.json` | Generated export in the `[embedder] related_pairs` format (`[{"a", "b"}]`): the 170 adjudicated `related_distinct` pairs. Point `related_pairs` at it to derive the merge threshold (#87). |
| `authoring/` | The sources: `t_*.py` topic modules (one `C()` per concept with its queries, rewordings, contrasts and facets), `dogfood_texts.json` (the public dogfood texts, from `extract_dogfood_texts.py`) and `build.py`, which writes the four files above deterministically. |

`tests/eval_set.rs` checks all of it in CI: the schema (required fields,
unique ids and texts, known families, splits and relations, grades 1 or 2,
every text non-empty and within `MAX_CONTENT_BYTES`), the sizes below, that
the export equals the adjudicated `related_distinct` pairs and loads as the
canary's `RelatedPair`, and a hygiene guard over every file here (no home
paths, e-mail addresses, IP addresses other than loopback, keys, tokens or
redaction markers). Edit `authoring/`, then run
`python3 fixtures/eval/authoring/build.py` and commit what it writes; the
test fails if `related_pairs.json` is stale against `pairs.json`.

## Counts

Pairs by label and family (the `relation` of identifier and correction pairs
answers #198: `supersedes`, `distinct_entity` or `contradicts`):

| Label | Family | Pairs |
|---|---|---:|
| related_distinct | identifier | 68 |
| | polarity | 36 |
| | topic-neighbour | 25 |
| | quantity | 15 |
| | exception | 13 |
| | correction | 12 |
| | repository | 4 |
| paraphrase | rewording | 97 |
| | terse | 10 |
| | same-event | 2 |
| | near-verbatim | 1 |
| unrelated | cross-area | 120 |

Splits (a hash of the id, about 70/30): pairs 276 dev / 127 held-out;
queries 199 dev / 111 held-out. Related-distinct relations: 84 contradicts,
52 distinct_entity, 12 supersedes, 25 none (families that need none).

Queries by family:

| Kind | Family | Queries |
|---|---|---:|
| answerable | fact-lookup | 124 |
| | exact-identifier | 29 |
| | contrast | 23 |
| | vocabulary-mismatch | 12 |
| | rationale | 8 |
| | superseded | 6 |
| | multi-hop | 2 |
| no_answer | near-domain | 80 |
| | false-premise | 13 |
| | efficacy | 7 |
| | off-domain | 6 |
| | ledger-miss | 0 (owner step, below) |

Per consumer:

- **#87 / #176**: 170 adjudicated related-but-distinct pairs in the export,
  over `MIN_RELATED_PAIRS` (100); #190's 150-before-adjudication target is
  met with 173.
- **#173**: 204 answerable queries with graded ids; nDCG@10 is computed by
  the measurement example.
- **#174**: 106 no-answer queries (32 in held-out); #174 wants at least 100
  per contract, so a held-out-only evaluation is short until the owner adds
  ledger misses.
- **#175**: the corpus is text, not a graph: it has no edges, so the
  multi-hop, low-degree-behind-hub and sparse/no-expansion families cannot
  be evaluated against it as PPR needs. The two `multi-hop` queries mark
  answers that need two concepts. A graph-structured slice needs the store
  copy (owner step).
- **#181**: the related-distinct families are #181's contrast families
  (polarity, repository, correction = old config vs correction, exception =
  rule plus exception, identifier); every answerable query carries `facets`.
- **#198**: identifier and correction pairs carry `relation`.

## Provenance

- **Dogfood text.** `authoring/dogfood_texts.json` holds the 64 dogfood-store
  texts that were already public: the `dogfood-i3` rows of
  `evidence/issue-178-similar/labelled-pairs.json`, the texts printed in
  `evidence/mooshik-i3-live-ledger/duplicates.txt`, and the full recall
  exports of the published, redacted copy of the 2026-09-07 runs in
  nrynss/lambo-paper at 696283e (`evaluation/results/2026-09-07/lambo-2026-09-07/exports/lambo/*.full.json`,
  lambo run only). That copy of the store was taken 2026-09-07T07:03Z, held
  990 concepts and was served on `lambo-e11fb06` with candle BGE-M3 on Metal
  (the paper's README). The six lambo-run efficacy questions seed the
  queries; their rubrics are the facets.
- **Authored text.** Concepts, rewordings and contrasts restate facts from
  this repository's records (`CHANGELOG.md`,
  `dev-diary/lambo-for-mooshik/DOGFOOD-FINDINGS.md` and `K-candle-embedder.md`,
  `evidence/mooshik-g-recall-calibration/`, source constants) and the paper
  suite's rubrics. Each concept names its source. Contrast partners are
  deliberately false or different claims; they are pair texts, never corpus
  concepts.
- **#178.** The 42 dogfood file/commit-name pairs (41 after one duplicate of
  a synthetic pair) and the 30 synthetic distinct pairs are reused, with
  family and relation added. The 89 synthetic typo duplicates are left out: a
  one-letter typo is not a paraphrase and would pull the paraphrase quantiles
  toward 1.0. The 5 identical dogfood duplicates are left out too (cosine 1
  by construction).
- **Unrelated pairs** are drawn with a fixed seed across four topic areas,
  then read: nine draws judged related after all are rejected in `build.py`.
- **Not yet from the store itself.** The store and its ledger live on the
  owner's machine and were not available to build this. There is therefore
  no store snapshot SHA-256 here, no cosine-stratified sample of store pairs
  and no real ledger misses. The owner steps below add them.

## Hygiene

Only text already public in nrynss/lambo or in the published, redacted
lambo-paper results is used, plus authored text. The dogfood store also
holds concepts from other projects and material the paper redacted; none of
that is used: the vimanam run's exports and every text carrying a redaction
marker are dropped by the extractor, the paper's `calls.jsonl` (it names a
host) is not read, and no ledger file is read. Two retained dogfood texts
name paths under the operator's home by `~` (`~/lambo-dogfood/...`, and one
operator script path); both were already public. `tests/eval_set.rs`
enforces the guard above on every file here.

## Labelling protocol

- Pairs are labelled against the two texts only, without cosines.
  `sample_store_pairs.py` keeps the cosines in a separate key file for that
  reason.
- `related_distinct`: both texts are about the same subject and a merge would
  lose or corrupt a fact (a different identifier, the opposite claim, an
  exception, an older value). `paraphrase`: the same facts, reworded; a merge
  loses nothing. `unrelated`: different subjects.
- Related-distinct families: `identifier` (a different name, path, hash,
  version, flag or model: the #198 case), `quantity` (a different number for
  the same setting or measurement), `polarity` (the opposite or a false claim
  about the same thing), `exception` (a rule against the rule with its
  exception dropped or inverted), `correction` (an older or mistaken value
  against its correction), `repository` (the same path in two repositories),
  `topic-neighbour` (a different fact on the same subject).
- `relation` on identifier and correction pairs: `supersedes` (b is the newer
  value of the same fact, a merge would hide the change), `contradicts` (the
  same slot with incompatible values, neither marked newer),
  `distinct_entity` (two things that can both be true).
- Graded relevance: 2 if the concept answers the query on its own, 1 if it
  supports or partly answers it.
- When the label needs the owner (an ambiguous pair, or whether an answer is
  right), the item gets `review` and stays out of the export and the
  numbers.
- The 20% double-labelled overlap #190 asks for has not been done: v0 has one
  labeller. The owner's adjudication pass is the second label.

## Awaiting owner review

| Id | Question |
|---|---|
| `p-x01`, `p-x02` | Two dogfood records of the same review event, each with details the other lacks. #178 labelled them paraphrase duplicates; is a merge (which keeps one text) acceptable? |
| `p-srv-04-rbd1` | J3's resolution of the J2 coupled-ceiling residual against the residual itself: does it supersede it or only narrow it? (#178 said related-but-distinct.) |
| `p-178-syn-neg-017` | Two commit hashes one hex digit apart: a typo of one commit (duplicate) or two commits? |
| `p-178-syn-neg-019` | `src/main.rs` against `src/mein.rs`: a typo or a distinct path? |

## Owner steps (to complete #190 from the store)

1. On a **copy** of the dogfood store:
   `python3 scripts/eval/sample_store_pairs.py --store copy.db --session lambo-dev --out sheet.json --key key.json`
   draws 60 pairs per cosine bin ([0.55, 0.65) to [0.85, 1]) and writes the
   store's SHA-256 into the key file.
2. `python3 scripts/eval/ledger_misses.py --ledger calls.jsonl --out misses.json`
   lists every distinct recall query, weakest best cosine first.
3. Label both sheets. A row is kept only when its `label` is set and its
   `private` is set to `false`, after checking that both texts may be
   published; leave `private` as `null` (or set `true`) on anything unsure,
   and that row is never written. Then
   `python3 scripts/eval/fold_owner_labels.py --pairs sheet.json --queries misses.json --labeller "<name>" --date <YYYY-MM-DD>`,
   rerun `build.py` and `cargo test --test eval_set`. Private, skipped and
   unlabelled rows are never written.
4. Settle the review items above (drop the `review` note or relabel in the
   authoring module), and record the store SHA-256 here.

## Measuring

`examples/eval_measure/` measures the set under the configured embedder; see
`evidence/eval-set/README.md` for committed runs and how to add one.

## Known biases

- Most pairs are anchored on corpus concepts, and 77 of 100 concepts are
  authored restatements: rewordings were written by the same labeller as the
  originals, so paraphrase cosines are likely higher than for paraphrases
  agents produce independently.
- Contrast partners are short single-claim edits of their anchor, which is
  close to the hardest case for an embedder; real related-but-distinct pairs
  in a store differ more.
- Relevance was first labelled from the text alone, then pooled once
  against BGE-M3 (candle, CPU): for each answerable query whose top-1 was
  not a grade-2 concept, the top three were read, and 13 queries gained a
  concept that does answer or support them (two corpus concepts record the
  same review event twice). Labels were added only where the text answers;
  none was removed to suit the ranking. That pooling favours BGE-M3 slightly
  when another embedder is compared against it.
- The topics are the lambo project only; the corpus holds no other project's
  text, so cross-project confusions (the shared-layer case) are not covered.
- 106 no-answer queries are all hand-written or efficacy-derived; none comes
  from a real ledger yet.
- The 41 dogfood file-name pairs are very short texts; they dominate the
  identifier family.
