# Evaluation set (#190)

Labelled queries and labelled text pairs drawn from the dogfood memory. They
are the shared yardstick for #87 / #176 (the `[embedder] related_pairs`
threshold), #173 (RRF fusion), #174 (conformal abstention), #175 (PPR
expansion), #181 (redundancy) and #198 (what a refused identifier merge is,
and the precision of its suspected-supersession trigger).

**Status: v0, labelled by agents; every item that needed the owner's
judgment is settled by the owner** (see "Owner rulings" below), and none
awaits the owner now. Every item says who labelled it (`labeller`,
`labelled_on`). An item whose label needs the owner carries a `review`
note and is kept out of the export and out of every reported number until
the owner settles it.

## Files

| File | What |
|---|---|
| `corpus.json` | The retrieval universe: 100 concepts `{id, topic, type, text, origin, sources}`. `origin` is `dogfood` (23 concepts, verbatim text of a dogfood-store concept) or `authored` (77 atomic facts restated from the repo's own records, each citing its source file). A query is "no-answer" relative to this corpus. |
| `queries.json` | 368 queries `{id, query, kind, family, topic, split, relevant[{concept, grade}], facets[], source, labeller, labelled_on, review?}`. 262 `answerable` (graded relevance 2 = answers it, 1 = partial or supporting; `facets` = what a correct answer must say, for #181) and 106 `no_answer`. |
| `pairs.json` | 547 pairs `{id, a, b, label, family, relation?, concept_types?, split, source, labeller, labelled_on, concepts?, review?}`: 309 `related_distinct`, 118 `paraphrase`, 120 `unrelated`. `concept_types` is `[type of a, type of b]`, the concept type each text would be derived as (#198); every `supersedes` pair has it. |
| `related_pairs.json` | Generated export in the `[embedder] related_pairs` format (`[{"a", "b"}]`): the 173 adjudicated `related_distinct` pairs outside #198 (no `p-198-*` pair is exported). Point `related_pairs` at it to derive the merge threshold (#87). |
| `authoring/` | The sources: `t_*.py` topic modules (one `C()` per concept with its queries, rewordings, contrasts and facets), `dogfood_texts.json` (the public dogfood texts, from `extract_dogfood_texts.py`) and `build.py`, which writes the four files above deterministically. |

`tests/eval_set.rs` checks all of it in CI: the schema (required fields,
unique ids and texts, known families, splits and relations, grades 1 or 2,
every text non-empty and within `MAX_CONTENT_BYTES`), the sizes below, that
the export equals the adjudicated `related_distinct` pairs outside #198
(exactly the 173 it held before #198) and loads as the canary's
`RelatedPair`, and a hygiene guard over every file here (no home
paths, e-mail addresses, IP addresses other than loopback, keys, tokens or
redaction markers). Edit `authoring/`, then run
`python3 fixtures/eval/authoring/build.py` and commit what it writes; the
test fails if `related_pairs.json` is stale against `pairs.json`.

## Counts

Pairs by label and family (the `relation` of identifier and correction pairs
answers #198: `supersedes`, `distinct_entity` or `contradicts`):

| Label | Family | Pairs |
|---|---|---:|
| related_distinct | identifier | 202 |
| | polarity | 36 |
| | topic-neighbour | 27 |
| | quantity | 15 |
| | exception | 13 |
| | correction | 12 |
| | repository | 4 |
| paraphrase | rewording | 97 |
| | terse | 10 |
| | same-event | 8 |
| | near-verbatim | 3 |
| unrelated | cross-area | 120 |

Splits (a hash of the id, about 70/30, except the #198 pairs, whose split
is set in their module and stratified by sub-kind, about 60/40; see
"Added for #198"): pairs 362 dev / 185 held-out; queries 239 dev / 129
held-out. Related-distinct relations: 82 contradicts, 129 distinct_entity,
71 supersedes (44 dev / 27 held-out), 27 none (families that need none).

Queries by family:

| Kind | Family | Queries |
|---|---|---:|
| answerable | fact-lookup | 124 |
| | exact-identifier | 29 |
| | contrast | 36 |
| | vocabulary-mismatch | 48 |
| | rationale | 17 |
| | superseded | 6 |
| | multi-hop | 2 |
| no_answer | near-domain | 80 |
| | false-premise | 13 |
| | efficacy | 7 |
| | off-domain | 6 |
| | ledger-miss | 0 (owner step, below) |

Per consumer:

- **#87 / #176**: 173 adjudicated related-but-distinct pairs in the export,
  over `MIN_RELATED_PAIRS` (100); #190's 150-before-adjudication target is
  met. The #198 pairs below are kept out of it (owner decision,
  2026-10-27): the identifier guard refuses every one of them before any threshold
  applies, so they barely move the threshold the export derives (BGE-M3 p99
  0.98921 with them out, 0.98934 with them in) while they push the canary's
  embed of the export from 62.6 s to 87.6 s, past its 60 s timeout. The export is
  therefore the one the committed runs in `evidence/eval-set/` and
  `evidence/issue-87-calibration/` measured.
- **#173**: 262 answerable queries with graded ids; nDCG@10 is computed by
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
- **#198**: identifier and correction pairs carry `relation`. For the
  suspected-supersession trigger's precision bar (0.90 per embedder), in
  the trigger's regime (both texts the same Logic, Constraint or
  Observation type, refused by the identifier guard; `tests/eval_set.rs`
  checks both with the guard itself) and adjudicated:

  | Kind | dev | held_out | All |
  |---|---:|---:|---:|
  | `supersedes` (positives) | 43 | 27 | 70 |
  | `distinct_entity` (hard negatives) | 47 | 30 | 77 |
  | `same-event` paraphrases (hard negatives) | 5 | 3 | 8 |

  How many positives an embedder's merge threshold lets reach the trigger
  (a pair under the threshold is never a vector hit): under BGE-M3 at its
  0.85, measured (candle on CPU, `BAAI/bge-m3@5617a9f6`, self-framed), 62
  of the 70 (37 dev / 25 held-out); the 8 below it are 7 earlier
  `correction`-family pairs, reworded rather than templated, and
  `p-198-s16`. Under EmbeddingGemma 2 at its calibrated 0.93: not measured.
  The tests require at least 60 in-regime positives and 60 in-regime
  `distinct_entity` negatives, with at least 25 of each in each split. The
  41 #178 file and commit-name pairs are typed Resource, which the
  trigger's type condition excludes; so is `p-emb-06-rbd1`, a re-pin
  action. `contradicts` pairs name no newer text; whether a trigger hit on
  one counts against precision is #198's call.

  Precision is TP / (TP + FP) over the adjudicated in-regime pairs the
  trigger flags: a flagged `supersedes` pair is a true positive, and a
  flagged `distinct_entity` **or `same-event`** pair is a false positive
  (owner decision, 2026-10-27): a same-event pair is two records of one
  event, not a replacement, and a "possibly superseded" warning on it
  would mislead a reader. Report it per split, tuning on dev and quoting
  held-out.

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
- **Added for #173 (2026-10-27).** `authoring/t_more173.py` appends 58
  answerable queries to existing concepts (36 vocabulary-mismatch, 9
  rationale, 13 contrast; 40 dev / 18 held-out by the usual hash), written
  against the corpus text only, after the owner asked for more
  vocabulary-mismatch coverage (dev had 5). They carry `labelled_on`
  2026-10-27; every earlier id, text and split is unchanged. A
  vocabulary-mismatch query avoids the concept's identifiers, numbers and
  names, asking for the fact as a user who does not know them would.
  A query takes its concept's `facets` unless `Q(..., facets=[...])` gives
  its own, for a query about a different part of the concept.
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
  toward 1.0. (Two synthetic distinct pairs, `syn-neg-017` and `syn-neg-019`,
  are paraphrase duplicates by the owner's ruling below.) The 5 identical dogfood duplicates are left out too (cosine 1
  by construction).
- **Added for #198 (2026-10-27).** `authoring/t_supersede198.py` adds 144
  pairs, ids `p-198-sNN` (64 written as `supersedes`; the owner relabelled
  `s29` and `s30` `distinct_entity`), `p-198-nNN` (72 `distinct_entity`)
  and `p-198-eNN` (8 `same-event`), each id and split
  written at its call so neither depends on the order of the calls,
  labelled by `claude-opus-5-5 (agent)` (the set's other agent items carry
  build.py's default labeller, `Grok Bot (agent)`), every one authored
  from this repository's records (`CHANGELOG.md`, `DOGFOOD.md`, the
  adversarial-review files, evidence READMEs, `src/` constants, the git
  history of this README and of merges, the issue milestones) and citing
  its source. `build.py` checks each with a mirror of `same_identifiers`
  (`src/graph/canonical.rs`), and `tests/eval_set.rs` with the function
  itself: all differ in a digit run or an identifier-shaped token, so the
  #195 guard refuses all of them. The 12 earlier `supersedes` pairs gained
  `concept_types` (their anchor's corpus type), the #178 file and
  commit-name pairs `Resource`; no earlier id, text, label or split
  changed. None is from the dogfood store: its own texts are not public,
  and real store pairs reach the set only through the owner's sheet (owner
  steps below; `fold_owner_labels.py` carries a sheet row's
  `concept_types`, and refuses a `supersedes` row without them).
  - **Split.** Not the id hash: it is stratified by sub-kind, so held-out,
    where the trigger's precision is reported, gets its share of the
    hardest negatives. Within each of `supersedes`, `distinct_entity` and
    `same-event`, cut again by whether the two texts are identical once
    identifiers and digits are masked, every second and fourth of each
    five (by id) is held-out, about 40%; `build.py` fails if an adjudicated
    stratum of five or more holds under 30% or over 50% held-out.
  - **Template.** Negatives are written to share the positives' template:
    where the facts allow, the two texts differ only in identifiers and
    digits (counts in digits, one tense, an embedder by its model id,
    `BAAI/bge-m3` or `google/embeddinggemma-2`). Masked-token Jaccard is
    1.0 for 60 of the 74 #198 `distinct_entity` pairs, 57 of the 62 #198
    `supersedes` pairs and 1 of the 8 `same-event` pairs. The other 14
    negatives keep the words their facts need (a verdict, a constant's
    name, dev against held_out, with against without fusion, Mac against
    Linux, the old embedder's tense, a redesign round) and are not
    identifier-only.
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
- `relation` on identifier and correction pairs: `supersedes` (a is the newer
  value of the same fact and b the older one; a merge would hide the
  change), `contradicts` (the
  same slot with incompatible values, neither marked newer),
  `distinct_entity` (two things that can both be true).
- `supersedes` against `distinct_entity` (#198): a pair supersedes when both
  texts state the current value of one single-valued slot (a pin, a
  default, a constant, an unscoped current measurement, a count of this
  set) and only the value changed. Two texts that each name their own
  scope (a version, a review round, an issue, an embedder, a data size, a
  split) and both stay true are `distinct_entity`, however close their
  text: "on 0.6.0 recall p50 was 18.7 ms" does not supersede "on 0.7.0 it
  is 7.5 ms", while "recall p50 is 7.5 ms" supersedes "recall p50 is
  18.7 ms". Two records of one event are a `same-event` paraphrase.
- `concept_types`: the type a writer would derive each text as, by the
  #198 owner decision's types: a pin, default or calibration is Logic; a
  code constant, cap or toolchain requirement is Constraint; a measurement,
  count, verdict, release or merge is Observation; a file, commit or note
  name is Resource.
- Graded relevance: 2 if the concept answers the query on its own, 1 if it
  supports or partly answers it.
- When the label needs the owner (an ambiguous pair, or whether an answer is
  right), the item gets `review` and stays out of the export and the
  numbers.
- The 20% double-labelled overlap #190 asks for has not been done: v0 has one
  labeller. The owner's adjudication pass is the second label.

## Owner rulings

Every item that needed the owner's judgment was settled by the owner
(Narayan SS, 2026-10-25; the #198 rulings on 2026-10-27). The labels are
recorded in `authoring/owner_pairs.json` through
`scripts/eval/fold_owner_labels.py`, and `build.py` relabels each pair in
place (labeller "Narayan SS (owner)"), keeping its id, split and
`concept_types`.

| Id | Ruling |
|---|---|
| `p-x01`, `p-x02` | Two dogfood records of the same review event, each with details the other lacks (#178 had them as paraphrase duplicates): related-but-distinct, family `topic-neighbour`. |
| `p-srv-04-rbd1` | J3's resolution supersedes the J2 coupled-ceiling residual: related-but-distinct, `correction`, relation `supersedes` (a, the J3 resolution, is the newer). |
| `p-178-syn-neg-017` | Two commit hashes one hex digit apart are a typo of the same commit: paraphrase, `near-verbatim`. |
| `p-178-syn-neg-019` | `src/main.rs` against `src/mein.rs` is a typo of the same path: paraphrase, `near-verbatim`. |
| `p-con-01-rbd1`, `p-con-03-rbd1`, `p-cn-01-rbd1` | 2026-10-27: version-scoped, so `distinct_entity`, not `supersedes`: each text names its own version (Unreleased or 0.6.0 against 0.6.0 or before it) and both stay true of that version. They keep `correction` and their `concept_types`. |
| `same-event` pairs | 2026-10-27: they count against the trigger's precision; a trigger hit on one is a false positive (see #198 above). |
| `p-198-s29`, `p-198-s30` | 2026-10-27: `distinct_entity`, not `supersedes` (their review notes are gone): 17.1 MB / 3.4 ms against 47.3 MB / 161 ms for one 3,600-concept store, where each text names its own storage format (packed `f32` or text), and a store not yet provisioned on 0.7.0 still shows the text value, so both stay true. They keep their ids (with the `s`) and `concept_types`. |

## Owner steps (to complete #190 from the store)

1. On a **copy** of the dogfood store:
   `python3 scripts/eval/sample_store_pairs.py --store copy.db --session lambo-dev --out sheet.json --key key.json`
   draws 60 pairs per cosine bin ([0.55, 0.65) to [0.85, 1]) and writes the
   store's SHA-256 into the key file.
2. `python3 scripts/eval/ledger_misses.py --ledger calls.jsonl --out misses.json`
   lists every distinct recall query, weakest view first: queries that once
   returned nothing, then the lowest best cosine, then unranked ones.
3. Label both sheets. A row is kept only when its `label` is set and its
   `private` is set to `false`, after checking that both texts may be
   published; leave `private` as `null` (or set `true`) on anything unsure,
   and that row is never written. Then
   `python3 scripts/eval/fold_owner_labels.py --pairs sheet.json --queries misses.json --labeller "<name>" --date <YYYY-MM-DD>`,
   rerun `build.py` and `cargo test --test eval_set`. Private, skipped and
   unlabelled rows are never written. Folding a corrected sheet later also
   withdraws: a row now marked private, skip or unlabelled removes the
   earlier owner row for the same texts or query.
4. Settle any new review items the same way (an owner sheet folded in, or
   a relabel in the authoring module), and record the store SHA-256 here.

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
- The 41 dogfood file-name pairs are very short texts; with the #198 pairs
  (136 of the 202) they make the identifier family mostly template pairs.
- Every #198 pair is authored, and the near-identical template shape the
  trigger looks for is how they were written. Real supersessions in the
  dogfood store are mostly reworded restatements, and its guard-refused
  same-type pairs are mostly two records of one event, so the trigger's
  real-world precision and recall need the store run #198 plans.
