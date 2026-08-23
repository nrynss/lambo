# I3 — the five reports and `duckdb`, run against a *real* dogfood ledger

The artifact I's last two Done-when boxes were held open for. Everything here was
produced on 2026-08-23 from the live `lambo-dev` ledger and store, immediately after the
K2 migration act re-pinned the rig to `lambo-21e4cf8` — not from
`scripts/observability/sample/`, which is disclosed-fabricated and which every prior run
of these scripts used.

## Provenance

| | |
|---|---|
| Ledger | `~/lambo-dogfood/calls.jsonl` — 654 lines: 158 `call`, 493 `stats`, 3 `startup`. **0 unparseable** |
| Store | `~/lambo-dogfood/lambo-dev.db` — 513 concepts, 513 embedded (dim 1024), 0 without |
| Binaries in the file | `0.2.2 @ 0f672f1` (299 beats), `@ 19f51d3` (191), `@ 21e4cf8` (3) |
| `ledger_dropped_lines` | **0** — every count here is exact, not a lower bound |
| Kit | `scripts/observability/` at this commit, invoked exactly as its README §"The five reports" documents |

Reproduce (paths are the operator's; nothing here reads the repo):

```sh
cd scripts/observability
python3 recall_first.py  ~/lambo-dogfood/calls.jsonl
python3 dedup_rate.py    --bucket hour --store ~/lambo-dogfood/lambo-dev.db ~/lambo-dogfood/calls.jsonl
python3 score_bands.py   ~/lambo-dogfood/calls.jsonl
python3 blast_radius.py  --repo <lambo checkout> --window-minutes 120 ~/lambo-dogfood/calls.jsonl
python3 duplicates.py    --store ~/lambo-dogfood/lambo-dev.db --ledger ~/lambo-dogfood/calls.jsonl
```

## Hygiene — what was curated, and what was checked

I1's rule keeps real ledgers **outside** the repo and admits them to `evidence/` only
through a curated export. What is exported here is the **reports**, never
`calls.jsonl` itself — the ledger carries recall queries (to 2000 chars) and concept text
(to 200), and none of it is in this directory.

Two deliberate curations:

* **`duckdb.txt` runs recipes 1 and 3 only.** Recipe 2 ("slowest recalls, with the query
  that caused them") is the one recipe that prints query text verbatim; it was not run.
* **`duplicates.txt` echoes concept text by design** — a near-duplicate report that hid
  its pairs would be useless. Scanned before export: zero Endor-internal content, zero
  credentials. The four hits on a `token` grep are the words "512-token clamp" and
  "tokenizer" in ordinary technical prose. The concepts in this store are lambo's own
  development history, the same material as `dev-diary/`.

## `duckdb` over a real ledger — the box's actual wording

The box asked for `duckdb` run over a real day's ledger with the torn tail stripped.
**No tail needed stripping**: unlike the fabricated sample (which ends in a deliberate
torn line, and which `duckdb` refuses outright), the live ledger parsed 654/654. Both
recipes ran unmodified, and recipe 3 reaches *nested* fields (`stats.node_count`,
`stats.ledger_dropped_lines`), which is the flattening property the box is really about —
`read_json_auto` consumed the shape without a schema hint.

## What the reports actually found

Recorded here because an evidence capture that only says "the scripts ran" is the claim
this box was unchecked for in the first place.

1. **I2's property is visible on real data.** `recall_first.txt` and `dedup_rate.txt`
   both print three binaries in one file — `0f672f1 → 19f51d3 → 21e4cf8` — so "an upgrade
   shows as a `git_sha` change in the same file" is now demonstrated across two real
   upgrades, not asserted from a fixture. Both reports also warn, unprompted, that trends
   crossing that boundary should be read with care.

2. **184 concept pairs sit at or above the 0.85 merge threshold, and `semantic_merged`
   is 0.** The report's own words: "a high pair count above with a zero `semantic_merged`
   means the vector merge never fired at all." This is the unembedded period's second
   casualty and it was invisible until today. The store ran at ~7% embedding coverage
   (36/513) until the K2 migration, and `duplicates.py`'s own caveat names the mechanism:
   *a pair cannot merge if the older concept had no embedding when the newer one was
   written.* So the embedding gap did not only disable semantic recall — it disabled
   **deduplication**, and the graph now carries 184 pairs that hybrid derive should have
   collapsed. Now that coverage is 513/513 the merge path can fire for new writes; the
   184 historical pairs are pre-existing damage that no future write repairs.
   **This is a finding for G/C, not a defect in the kit** — the kit is what made it
   visible, on its first real run.

3. **Recall-first compliance is bimodal and the reason is structural.** Every
   short-lived agent id scores 100%, while `claude-orchestrator` scores 0.0% across 22
   work sessions with 37 derive-without-recall calls. That is the long-lived
   orchestrator session writing what it already knows rather than recalling first —
   worth reading against the protocol before treating it as a compliance failure.

4. **`canonical_count` is 0 across every heartbeat**, consistent with the store: with 513
   concepts and 1 session, nothing has met a promotion bar. This is the measurement C and
   D exist to change, and it is now on record from the live rig rather than inferred.
