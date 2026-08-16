# Adversarial review — remed-T9 (round 3, final clearance)

**Scope:** final clearance re-review of the T9 worktree (detached HEAD `f7ef210`). Round 2 returned APPROVE (2 P3 accepted-by-design + 1 nit), with the two comment notes to be added. Read-only; exactly this file written.
**Method:** verified the only delta since round-2 APPROVE is the two comment additions; `cargo check --lib` PASS; ran the dispatch (7) and recall (62) test sets — all PASS. No source edited.

**Verdict: APPROVE.**
**Disposition: APPROVE** — clean for integration.

---

## Delta since round-2 APPROVE

**Comment-only, exactly the two requested notes, no behavior change.** `src/recall/dispatch.rs` is 663 lines (round-2 recorded 656; delta = 7 lines), and lines 142-148 are the only change — all seven lines are `//` comments, no code touched:

- **142-145 (precompute note):** accurately documents that each `dependents(graph, *id)` in the SG branch (dispatch.rs:149) is a full `inbound_sources` pass, run twice per recall (brief read + final lock), so the branch is O(SG × E) — bounded and fine at recall-graph sizes, with a precompute-if-it-ever-matters note. This records round-2 P3-2 (`T9-R2-P3-2`).
I verified the comments against the surrounding code they describe line-for-line: `sg.sort_by_key(|id| Reverse(dependents(graph, *id).len()))` at line 149 confirms the per-SG `dependents()` recomputation and the count-based (iteration-ordered) tie-break. Both notes are accurate and non-inventive.

The tracked diff (`src/daemon/mod.rs`, `src/recall/{assemble,candidates,format,mod}.rs`) is byte-identical to what round-2 reviewed; `git status` shows no file beyond those five plus the untracked `dispatch.rs` and the round-1/round-2 review docs. No other change exists.

## R3 residual closure

- **T9-R2-P3-1 (stale skip-decision TOCTOU)** — still accepted-by-design, as cleared in round 2: the `dispatch_ready` decision under the brief read and the re-validation under the final lock can only, on cross-thread graph mutation between them, skip the gather and then fail to dispatch into a blend. Re-validation under the final lock means it can never yield a wrong structural answer. Inherent to the "decide skip early, validate late" design; no residual risk. *No change required.*
- **T9-R2-P3-2 (O(SG × E) SG branch)** — now documented at the site (dispatch.rs:142-145); accepted-by-design.
- **T9-R2-N1 (iteration-order tie-break)** — now documented at the site (dispatch.rs:146-148).

No P1, P2, P3, or nit remains open in the worktree.

## Verification evidence

- `cargo check --lib` → clean (no warnings/errors).
- `cargo test --lib 'recall::dispatch::'` → 7 passed, 0 failed, including both exemplar tests (`dependency_question_returns_structural_dependent_by_traversal`, `delete_safety_question_returns_real_ranking_not_flat_floor`), the false-positive guard (`marker_bearing_non_dependency_questions_stay_general`), the refusal path, and the end-to-end entry test (`recall_entry_dispatches_structural_query`).
- `cargo test --lib 'recall::'` → 62 passed, 0 failed.

## Clean-for-integration statement

The T9 worktree is clean and integration-ready. The classifier, traversal/§4.1 membership reconciliation, dispatch gate + full-blend fall-through, gated instrumentation, ⚑ warning parity, canonical-first ordering, and the no-cache/no-hotlist decision are all as approved in round 2; the sole post-approval change is the two comment notes, which are accurate and behavior-preserving. All round-2 findings are closed (accepted-by-design and/or documented). Recommend merge.

---

{ verdict: "APPROVE", findings: { P1: [], P2: [], P3: [], nits: [] }, summary: "Since round-2 APPROVE the only change is the two comment notes at dispatch.rs:142-148 (precompute O(SG x E) note + iteration-order tie-break note); verified line-for-line accurate, comment-only, and they record round-2 P3-2 and the nit. The round-2 P3-1 TOCTOU remains accepted-by-design (re-validation under the final lock prevents any wrong structural answer). No P1/P2/P3/nit remains open. cargo check --lib clean; dispatch (7) and recall (62) tests all pass. Clean for integration." }
