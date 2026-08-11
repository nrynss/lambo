# Adversarial Review: T2.6 — Inverted Index + BM25

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT (2 review rounds; round-2 clean)    ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T2.6 — Inverted index + BM25 (spec §8 phase-1 keyword source)
**Scope:** `src/graph/index.rs`, one `pub mod index;` line in `src/graph/mod.rs`
**Implementer:** T26Index (commit `982938e`); remediation `5f75f0b`
**Reviewer:** ReviewT26Index (round 1), Review2T26Index (round 2)
**Gate at close:** `cargo test graph::` = 37 passed / 0 failed (goldens exact);
integration seam swap verified (see below).

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | INFO | Handoff claims index.rs "~490 LOC"; actual 386 | **Fixed** (`5f75f0b`): handoff corrected to 386, `wc -l` confirmed |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified: remediation doc-only (module byte-identical);
pinned API present (`new`/`from_snapshot`/`add` idempotent per id/`remove`/`search`);
BM25 `k1=1.2` `b=0.75`; strictly positive scores only (BM25+ `idf = ln(1 + (N-df+0.5)/(df+0.5))`);
deterministic tie-break score-desc then inner `Uuid` asc; both fixture goldens exact
("pagination" -> [1008], "create" -> [1002]); 37 passed / 0 failed.

## Integration note (tokenizer seam, resolved by Main on phase/p2)

The local seam tokenizer was replaced by `crate::graph::canonical::normalize_tokens`
(import, don't fork). **One reconciliation:** the canonical tokenizer does not split
acronym runs (`APIKey` -> `["apikey"]`, per the fixture lower→Upper camelCase
convention), where the seam copy split them (`api`,`key`). Canonical behavior wins;
`tokenize_matches_canonical_contract` now pins `APIKey` -> `["apikey"]` with an
explanatory comment. Goldens unaffected (no acronyms in fixtures). Full suite
after swap: 156 passed / 0 failed.

## Notable decisions recorded (handoff log)

- Search is OR over query terms; per-term BM25 contributions sum; `df` per-session
  by construction (one index per session).
- `add` re-add with changed content atomically replaces postings; `remove` of an
  unknown id is a no-op; interactions are never indexed.
