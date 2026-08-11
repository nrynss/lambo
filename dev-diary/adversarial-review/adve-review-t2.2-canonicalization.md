# Adversarial Review: T2.2 — Canonicalization Pipeline

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT (2 review rounds; round-2 clean)    ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T2.2 — Canonicalization pipeline (spec §7.1 steps 1–5)
**Scope:** `src/graph/canonical.rs`, one `pub mod canonical;` line in `src/graph/mod.rs`
**Implementer:** T22Canonical (commit `e576d62`); remediation `a753f4c`
**Reviewer:** ReviewT22Canonical (round 1), Review2T22Canonical (round 2)
**Gate at close:** `cargo test graph::` = 40 passed / 0 failed; test build 0 warnings.

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | P2 | Unused test helper `fn sid()` (canonical.rs:161-163) emits a dead-code warning on every test build — would break a clippy `-D warnings` gate on test targets | **Fixed** (`a753f4c`): helper deleted + orphaned `SessionId` test import removed; test-build warning count 0 |
| R2 | P3 | Commit/handoff claim "82-word" fixture STEM table; actual 80 entries | **Fixed** (`a753f4c`): handoff corrected to 80, verified programmatic count |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified: remediation touches only canonical.rs +
phase doc; pinned contract holds (`normalize_tokens` pure: lowercase → split
`[-_ ]` + camelCase-on-original-case → 13 pinned stopwords → Porter stem;
`canonical_key` raw-trimmed synonym lookup first; `canonicalize` → Matched/Unmatched
with hybrid step 6 left to the caller; `Graph::synonym` consumed, no new storage);
12 tests green incl. the 11-row fixture acceptance iterating the real JSON;
STEM table byte-identical to `gen-fixtures.py`.

## Notable decisions recorded (handoff log)

- camelCase split runs BEFORE lowercasing — the fixture JSON is truth;
  `gen-fixtures.py`'s lowercase-first `split_tokens` is a latent script bug (do
  not "fix" the fixture to match the script).
- The pinned `impl Fn(&str) -> Option<&str>` is higher-ranked; `canonicalize`
  does the raw lookup via `Graph::synonym` through a shared `tokens_to_key`
  helper (exported signatures unchanged).
- Stopword set is 13 words (`{the,a,an,for,of,at,in,to,on,and,or,is,are}`),
  matching the frozen script.
