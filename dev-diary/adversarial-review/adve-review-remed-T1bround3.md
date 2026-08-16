# Adversarial Review: Remediation T1 part 2 — Round 3 (branch `remed/T1b`)

```text
╔════════════════════════════════════════════════════════════════════╗
║  STATUS: APPROVE — Round 3 final clearance                         ║
║  Round 2:  APPROVE — 0 P1 / 0 P2 / 0 P3 / 2 nits (T1b-R2-1/-2)     ║
║  Round 3:  the two open nits are now documented at the source;      ║
║            comment-only change, no behaviour delta. Worktree is     ║
║            clean and integration-ready.                             ║
║  Verdict:  APPROVE — 0 P1 / 0 P2 / 0 P3 / 0 nits.                  ║
║            Single known non-defect: client-side IPv6 half deferred  ║
║            to T7 (T1b-R1-3), documented in `_lambo.py` + task doc.  ║
╚════════════════════════════════════════════════════════════════════╝
```

## Grounding

Final read-only clearance of the `remed/T1b` worktree (detached HEAD @
`1285dd0` — unchanged from round 2; no new commits since APPROVE). Read the
current serve-web `run()` startup block, the `_lambo.py` client diff, all four
acceptance-criteria regions in source, and re-ran the targeted test set. No
file modified; exactly one deliverable (this doc) written.

**Ran (targeted, as allowed):**
- `graph::derive::tests` — **23 passed** (incl. the round-1 refusal pins).
- `cli::tests::reader_refuses_mismatched_embedding_contract` — **pass**.
- `cli::serve_web::tests::the_module_registers_only_get_routes` (no-writer-lease
  source-grep) — **pass**.
- `cli::derive::tests::parent_of_accepts_colon_bearing_parent_ipv6_roundtrip`
  — **pass**.

---

## 1. Goal: the only change since Round-2 APPROVE is comment-only

**Confirmed.** The sole delta is a comment block added at
`src/cli/serve_web.rs:812-826`, immediately above the (unchanged)
`load_reader_graph_with_contract(...)` startup check at 827-839.

- **Comment-only / no behaviour change:** the raw `git diff` of the whole run()
  hunk shows the added rows are exclusively `//` comment lines. The code block
  (`if let Err(e) = load_reader_graph_with_contract(...)` … `return Err(e)`) is
  byte-identical to what round 2 reviewed — the round-2 doc quotes the refusal
  message *"refusing to start — the live embedder does not match this session's
  stored vectors"* verbatim, matching the current lines 835-837. HEAD is still
  `1285dd0`, so no source changed elsewhere.
- **Accurate:** the comment correctly states (a) the check is read-only
  (no writer / no lease / nothing stamped) and only refuses on a genuine
  mismatch (fresh/absent sessions have no stored contract and load fine);
  (b) the message names kind/model/dim of the mismatched model — matching
  `ensure_compatible` (`src/types/mod.rs:512`), which names **both** the
  session's writing contract and the live embedder; and (c) the all-or-nothing
  posture.
- **T1b-R2-1 documented:** lines 819-823 state the gate is *"intentionally
  all-or-nothing"* — refusing to start rather than serving only the structural,
  embedder-free surfaces — and give the escape hatch ("gate only the
  stats/pulse/recall endpoints instead").
- **T1b-R2-2 documented:** lines 824-826 state the one-time load is *"deliberate
  redundancy: it fails before the server binds … even though the first request
  would reload the session"*.
- **Grammatically clean:** both sentences are well-formed and unambiguous
  (no splice, no dangling modifier, consistent em-dash/parenthesis style).

The two open nits from round 2 are now recorded at the source, exactly as the
round-2 fix requested. **Goal met.**

## 2. Full 8-file diff — still matches Round-2 reviewed state

HEAD unchanged (`1285dd0`); the uncommitted working-tree diff is the same
8 files round 2 approved. The serve-web delta is the comment block above only
(no logic touched); `scripts/cloudops/_lambo.py` is unchanged (same T7 deferral
comment); the other six files (`src/cli/derive.rs`, `src/cli/mod.rs`,
`src/cli/recall.rs`, `src/graph/derive.rs`, `src/main.rs`, `src/types/mod.rs`)
have no round-3 edits. No logic regression; the only additions since round 2 are
the comment-only lines. **Consistent.**

## 3. Four acceptance criteria — re-confirmed intact

1. **Reader embedding-contract enforcement naming the writing model.**
   `load_reader_graph_with_contract` (`src/cli/mod.rs:61`) still enforces via
   `assert_session_embedding_compatible`; `ensure_compatible`
   (`src/types/mod.rs:512`) names kind/model/dim of **both** the stored
   (writing) and live embedder. Test asserts `dim=1024` (the stored/writing
   dim). **PASS.**
2. **Observation refusal at derive boundary.**
   `reject_repeated_observation` (`src/graph/derive.rs:566`) still refuses a
   second same-key Observation with "opts out of identity" before any write;
   first-Observation / Observation-over-Entity limits documented and pinned by
   `derive_repeated_observation_refuses_identity_split`. **PASS.**
3. **Second-Hierarchical-parent refusal naming the claiming parent.**
   `reject_second_hierarchical_parent` (`src/graph/derive.rs:608`) still sits in
   the parent_of pre-pass, scoped to `EdgeType::Hierarchical`, error names the
   existing parent (`"already has Hierarchical parent '{prev_key}'"`);
   same-parent reinforce Ok in both branches. **PASS.**
4. **`--parent-of` first-colon / IPv6 accept + empty-side refuse.**
   `parse_parent_of` (`src/cli/derive.rs:31`) first-colon split, child
   colon-free, parent free-text-with-colons, empty side refused; IPv6 round-trip
   test passes. **PASS.**

The no-writer-lease serve-web source-grep test still passes (comment-only, as
required). **All four hold.**

## 4. Residual findings

No P1 / P2 / P3 / nit remain. T1b-R2-1 and T1b-R2-2 are closed as documented
(non-actionable by design, now recorded at the source). The single known
non-defect is the **T1b-R1-3 client-side IPv6 half → T7 deferral** (the CLI
engine half is done; `_lambo.py` still pre-refuses both ends, noted with an
explicit T7-naming comment at `scripts/cloudops/_lambo.py:304-308` and tracked
in `dev-diary/notes/remediation-tasks.md`). This is an intentional, documented
deferral — not a defect.

---

## Summary

Round 3 is a final-clearance pass over an unchanged, already-approved tree: the
only delta since round-2 APPROVE is the comment-only addition at
`src/cli/serve_web.rs:812-826` that documents the two round-2 nits (all-or-
nothing fail-fast posture; deliberate one-time-load redundancy) at the source.
The comment is accurate against the surrounding code (read-only check, refuses
only on a genuine mismatch, names kind/model/dim, fresh/absent sessions load
fine) and grammatically clean. The full 8-file diff otherwise matches the
round-2 reviewed state (HEAD unchanged, no logic regression), all four
acceptance criteria still hold, the no-writer-lease test still passes, and no
P1/P2/P3/nit is open — the sole known non-defect is the T1b-R1-3 client IPv6
half, intentionally deferred to T7 and documented in both `_lambo.py` and the
task doc. The worktree is **clean and integration-ready**. **APPROVE.**

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": [], "nits": [] }, "summary": "Round-3 final clearance: the only change since round-2 APPROVE is a comment-only block at src/cli/serve_web.rs:812-826 documenting the two round-2 nits (all-or-nothing fail-fast posture T1b-R2-1; deliberate one-time-load redundancy T1b-R2-2) at the source. Verified comment-only (code block byte-identical to round 2, HEAD still 1285dd0, refusal message quoted in round-2 doc matches current source), accurate against ensure_compatible/assert_session_embedding_compatible, and grammatically clean. Full 8-file diff otherwise matches the round-2 reviewed state with no logic regression. All four acceptance criteria re-confirmed intact (reader contract refusal naming writing model; Observation refusal; second-Hierarchical-parent refusal naming claiming parent; --parent-of first-colon/IPv6 accept + empty-side refuse); graph::derive::tests 23 pass, reader-contract, no-writer-lease source-grep, and IPv6 round-trip tests all pass. No P1/P2/P3/nit open; single known non-defect is the T1b-R1-3 client-side IPv6 half, intentionally deferred to T7 and documented in _lambo.py and the task doc. The worktree is clean and integration-ready. APPROVE." }
```
