# Adversarial Review — T4 `#[non_exhaustive]` on `ResolvedBackends` (Round 1)

**Reviewer:** T4ReviewR1 · **Date:** 2026-08-17 · **Commit:** dd924f8 (detached)
**Files touched:** `src/resolve.rs` (+ rationale note in `dev-diary/notes/remediation-tasks.md`)
**Scope:** The T4 change adds `#[non_exhaustive]` above `pub struct ResolvedBackends` plus a three-line doc note. No other source change.

---

## Verdict: **APPROVE**

**Disposition:** None of the findings block merge. The change is a single, correctly-placed attribute that does what T1-P2-2 asks, introduces no in-crate break, and the doc note is accurate. Findings are 0 P1, 0 P2, 0 P3, and 3 nits.

---

## What I verified (grounding)

**1. Attribute placement & attribute shape.** `#[non_exhaustive]` sits at `src/resolve.rs:16`, immediately above `pub struct ResolvedBackends {` at line 17, with only the doc comment touching lines 11–15. No intervening attribute or macro conflicts. All six fields (`store`, `embedder`, `store_cfg`, `embedder_cfg`, `embedding`, `config`) are `pub` (`resolve.rs:18–26`). Placement is correct.

**2. No new in-crate break (correctness of the non_exhaustive guarantee).** `non_exhaustive` is spec'ed to restrict *cross-crate* struct-literal construction and exhaustive pattern-matching only; within the defining crate construction and destructuring remain legal. Every construct/destruct site lives inside this crate:
- Construct: `resolve.rs:110` (`resolve_backends`), `src/cli/mod.rs:247` (`backends_on` test helper), `src/cli/mod.rs:608` (test), `src/cli/serve_web.rs:1405` (test helper), `src/mcp/serve.rs:1865` (test helper).
- Destruct: `src/cli/demo.rs:658` — interestingly already uses a non-exhaustive `..` pattern (`let ResolvedBackends { store, embedder, embedding, .. }`), so it would survive even if it were an external type.
- All other references pass it by value/ref (`memory.rs:460` `MemoryBuilder::backends`, the CLI `run` fn signatures, `main.rs:307`).

`cargo check --tests` runs **clean** on the worktree (Finished dev, 0 warnings). No in-crate break; the intent ("it should not break in-crate construction") holds.

**3. Is `#[non_exhaustive]` the right tool for this type?** Yes. `ResolvedBackends` is a **result / bundle type that callers receive, never assemble from parts**: it is produced only by `resolve_backends` / `resolve_from_config_path` and consumed by `MemoryBuilder::backends(..)` and the CLI/serve/demo commands. Although its fields are `pub` (for convenient field access on the way through the crate), no public entry point ever asks a library user to build one from scratch — the `Box<dyn GraphStore>` / `Box<dyn Embedder>` / `StoreConfig` / `EmbedderConfig` / `EmbeddingContract` collection is genuinely something only resolution can meaningfully fabricate. So `non_exhaustive` does not block a plausible public construction flow; it only closes off impl-use of field literals that no external caller is meant to exercise. This matches T1-P2-2's intent (the T4 notes: *"Adding the `config` field already made this a breaking change. The attribute stops the next field being another one."*). The one honest cost is that `non_exhaustive` ALSO removes external exhaustive-destructuring of the already-pub fields — acceptable for a produced-bundle result type, and pre-release so a negligible one-time cost.

**4. Doc note accuracy.** `resolve.rs:11–15`: *"Fully resolved Level B backends ready for Memory / CLI. `#[non_exhaustive]` is deliberate: adding a field is a breaking change for library consumers of the resolved-backend bundle (see T1-P2-2), so the attribute future-proofs the struct against the next field addition."* This is correct: adding a pub field to a pub-fields struct is a semver-breaking change for any external consumer that constructs or exhaustively destructures it, and `non_exhaustive` future-proofs that. Cross-reference to T1-P2-2 resolves in the notes. No misleading wording.

**5. Consistency with crate conventions.** This is the **first** user-defined `#[non_exhaustive]` attribute in the crate. The only other `non_exhaustive` hits are `.finish_non_exhaustive()` **SDK method calls** (mcp/daemon/memory) and comments about *other* libraries' types (`mcp/serve.rs:1023`, `mcp/server.rs:1160`). So there is no pre-existing in-crate style to match — the attribute is not inconsistent with anything, merely precedent-setting. No conflict.

---

## Findings

### P1 — none

### P2 — none

### P3 — none

### Nits

- **T4-R1-N1 (doc completeness).** `src/resolve.rs:11–15` — the note explains the *future* benefit (next field won't break consumers) but not that **applying** `#[non_exhaustive]` is itself a one-time breaking change (it stops external struct-literal construction and exhaustive destructuring of the currently-pub fields). For a pre-release result-type this is the correct deliberate tradeoff, so this is non-blocking; a clause like *"— a one-time break that buys permanence for every subsequent field"* would make the rationale provably complete. Optional.

- **T4-R1-N2 (informational, no change required).** `src/cli/demo.rs:658` already destructures with a `..` wildcard. This is incidentally the pattern that keeps in-crate destructuring robust, but since `non_exhaustive` doesn't constrain in-crate code it is not *required* here. No action.

- **T4-R1-N3 (note duplication / drift risk).** The rationale now lives in two places — the doc comment (`resolve.rs:13–15`) and the T4 task section in `dev-diary/notes/remediation-tasks.md` (under the `## T4` heading). They agree today; keep them in sync if the rationale evolves. Trivial.

---

## Summary

T4 is a clean, minimal, correct change: `#[non_exhaustive]` is placed immediately above `pub struct ResolvedBackends`, all construct/destruct sites are in-crate and continue to compile (`cargo check --tests` green, no warnings), the type is a resolve-produced result bundle whose pub fields are never meant to be assembled by external callers (so the attribute appropriately future-proofs against the next field per T1-P2-2 without over-restricting a real public construction flow), and the doc note is accurate. It is the first user-defined `#[non_exhaustive]` in the crate (no consistency concern). Three nits only, none blocking. **APPROVE.**

{
  "verdict": "APPROVE",
  "disposition": "No blocking findings; 3 nits (all optional).",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [],
    "nits": [
      "T4-R1-N1 (src/resolve.rs:11-15): doc note explains future benefit but not that applying #[non_exhaustive] is itself a one-time break for external construction/exhaustive-destructure; add clause for complete rationale.",
      "T4-R1-N2 (src/cli/demo.rs:658): destruct site already uses `..` wildcard; informative only, no change required.",
      "T4-R1-N3: rationale duplicated in doc comment and T4 task section of remediation-tasks.md; keep in sync."
    ]
  },
  "summary": "Correctly-placed #[non_exhaustive] on the resolve-produced result bundle ResolvedBackends. All construct/destruct sites in-crate; cargo check --tests passes with no warnings; no in-crate break; type is produced-by-resolve (not caller-assembled), so the attribute is the right future-proofing per T1-P2-2. Doc note accurate. 3 optional nits; APPROVE."
}
