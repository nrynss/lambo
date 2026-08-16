# Adversarial Review — T4 `#[non_exhaustive]` on `ResolvedBackends` (Round 2, Final Clearance)

**Reviewer:** T4ReviewR2 · **Date:** 2026-08-17 · **Worktree:** `remed-T4` (detached @ dd924f8)

---

## Verdict: **APPROVE**

**Disposition:** The one outstanding nit from round-1 (T4-R1-N1) is resolved by the doc-clause addition. The worktree is clean and integration-ready. Findings: 0 P1, 0 P2, 0 P3, 0 nits.

---

## Round-2 delta verified (grounding)

**1. N1 doc-clause is the only change since round-1 APPROVE, and it is comment-only.**
`git status`/`git diff` on the worktree shows a single modified file: `src/resolve.rs`. The round-1 note (3 lines, `resolve.rs:11-15` in that review) has been extended by one clause inside the `///` doc comment:

> *"…the attribute future-proofs the struct against the next field addition — a one-time break now (callers can no longer literal-construct or exhaustively destructure it) that buys permanence for every field after."*

No code, attribute placement, or field data changed — the addition is entirely within the doc-comment text (verified above the `#[non_exhaustive]`/`pub struct ResolvedBackends`).

**2. The clause is accurate.** `#[non_exhaustive]` is spec'ed to restrict **cross-crate** struct-literal construction and exhaustive pattern-destructuring only; within the defining crate construction/destructuring remain legal (all construct/sites here are in-crate, so no in-crate break). The wording "callers can no longer literal-construct or exhaustively destructure it" precisely describes that external-consumer constraint, and "a one-time break now … that buys permanence for every field after" correctly captures the tradeoff (only next field-addition is future-proofed; applying the attribute is itself the one-time cost). This is exactly the clause T4-R1-N1 requested. Grammar is clean.

**3. `cargo check --tests` passes.** Ran clean on the worktree: `Finished dev profile … in 2.25s`, no warnings.

**4. Worktree diff scope.** `git diff --stat HEAD` = `src/resolve.rs | 7 +++`. The only non-source artifact is the round-1 review md (untracked) plus this round-2 doc. No P1/P2/P3/nit remains in the worktree.

**5. N2 / N3 — informational, no action in this worktree.**
- **T4-R1-N2:** `src/cli/demo.rs:658` already uses a `..` wildcard in its destruct — informative; `non_exhaustive` doesn't constrain in-crate code, so no change required.
- **T4-R1-N3:** rationale appears in both `src/resolve.rs` doc and the T4 section of `dev-diary/notes/remediation-tasks.md` — acknowledged in both reviews; doc-sync with `remediation-tasks.md` is a **Main documentation edit at integration time**, intentionally not performed here (read-only cargo-integrity scope).

---

## Findings

- **P1:** none
- **P2:** none
- **P3:** none
- **nits:** none

---

## Summary

The only change since round-1 APPROVE is the T4-R1-N1 doc-clause: an accurate, grammatically clean, comment-only addition inside the `///` rationale for `#[non_exhaustive]` on `ResolvedBackends`. It correctly states that applying the attribute is a one-time break (external literal-construction / exhaustive-destructure) that buys permanence for every subsequent field, matching Rust's cross-crate semantics and leaving in-crate construction intact. `cargo check --tests` passes with no warnings; the worktree diff remains isolated to `src/resolve.rs`. N1 closed; N2 and N3 are informational (N2 no action, N3 doc-sync is Main's integration edit). Nothing blocks merge. **APPROVE.**

{
  "verdict": "APPROVE",
  "disposition": "Clean for integration. N1 closed by doc-clause; N2/N3 informational (no in-tree action).",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [],
    "nits": []
  },
  "summary": "Round-2 final clearance: only change since round-1 APPROVE is the T4-R1-N1 clause added to the src/resolve.rs doc note — accurate (non_exhaustive stops external literal-construct/exhaustive-destructure, one-time break buys permanence for futures fields), grammatically clean, comment-only. cargo check --tests passes (no warnings). Worktree diff still isolated to src/resolve.rs (+attribute+note). N2 informational (demo.rs:658 `..` wildcard), N3 doc-sync deferred to Main's integration edit. No P1/P2/P3/nit remains. APPROVE."
}
