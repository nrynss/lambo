# Adversarial review — remed-T8 (round 3, final clearance)

**Scope:** `scripts/cloudops/03_crossover_protect.py` + `scripts/cloudops/_lambo.py` + `scripts/cloudops/02_app_data_agent.py` in the remed-T8 worktree (detached HEAD `555698e`, working tree carries the round-2 remediation).
**Task:** round-3 final-clearance re-review of the change since round-2 APPROVE (the T8-R2-1 fix); confirm the two original fixes and all R1 remediations still hold; full regression scan; no P1/P2/P3/nit remains; assess evidence sufficiency for D2.
**Method (read-only):** read the current state of every changed region; verified `_source_focus_missing_text`/`_self_test_empty_session_sentinel` against the live Rust arm at `src/cli/inspect.rs:303-306` and empirically probed all fail-closed scenarios (stale-constant, CLI-reword, missing-arm, unreadable-file) without editing source; re-confirmed original fixes and R1 remediations against current 03/02 code; `py_compile` on all three files; self-test under `python3` and `python3 -O`. No source edited; exactly this file written.

**Verdict: APPROVE.**
**Disposition: APPROVE** — the R2-1 P3 is genuinely fixed (self-test now sources the real `Focus::Missing` string and fails closed in every drift scenario), N1's comment is now accurate, N2's instruction is present, the two original fixes and all R1 remediations hold with no regression, py_compile and self-test are green both ways. The worktree is clean and integration-ready. Two items remain for **Main** (not worktree defects): the R1-3 README doc drift, and the strong recommendation of a real-live D2 capture.

---

## 1. The change since round-2 — T8-R2-1 (P3) → REMEDIATED, verified

Round-2 flagged that the drift-fail self-test anchored `EMPTY_SESSION_ERR` to a hardcoded copy of the error (only catching constant-vs-pinned-phrase drift, not a CLI reword with a stale constant). The fix replaces that with a live-source pin:

- **`_source_focus_missing_text()`** (`_lambo.py:678-707`) reads `REPO_ROOT / src / cli / inspect.rs` and returns the text of the **`Focus::Missing => Err(`** match arm. It correctly targets the error-formatting arm and **skips the unrelated `return Focus::Missing;`** in `resolve_focus` (`inspect.rs:110` — no `=> Err(`, so never matched). A bounded 12-line window is returned so any reword that drops/renames the phrase still fails the assertion. It raises `SystemExit` if the file is unreadable or the arm is absent — it **never silently vacates**.
- **`_self_test_empty_session_sentinel()`** (`_lambo.py:710-724`) asserts `EMPTY_SESSION_ERR` is present in that live text. Fails loudly via `SystemExit` otherwise.
- Wired into `__main__` (`_lambo.py:727-731`) alongside the IPv6 and structural-whitelist self-tests.

**Empirical probe (no source edited, against the real functions):**

| Scenario | Result |
|---|---|
| Baseline — constant matches live source | PASS (no raise) |
| Stale constant (`"no matching concept"`) | **SystemExit** (fails loudly) |
| CLI reworded (`inspect.rs` arm text changed) | **SystemExit** (fails loudly) |
| Missing arm (arm removed line-precisely) | **SystemExit** via real `_source_focus_missing_text` (`arm not found`) |
| Unreadable source file | **SystemExit** (`could not read … to pin`) |

The stale-constant case — the exact R1-2 silent-revert worry that reintroduces T3-2-P2-1 — now fails loudly. The drift guarantee is **achieved**, not just claimed.

- **N1 (T8-R2-N1) — now consistent:** the comment at `_lambo.py:119-126` says the sentinel "pins it to the real error string" and "a future reword of the CLI fails loudly" — this is now **true** (it sources the real Rust source at self-test time). Comment and behavior agree. Closed.
- **N2 (T8-R2-N2) — instruction present:** the structural-whitelist comment at `_lambo.py:113-116` still instructs maintainers to update the tuple **AND** the synthetic banner if a structural kind is added/renamed. Inherent, already-documented limitation; instruction present. Closed.

## 2. Original fixes + R1 remediations still hold (regression scan)

- **Fix 1 — empty-session `render_unprotected`:** `run_guard` (`03:252-265`) wraps `lam.inspect` in `try/except InfraError`, swallows **only** the `EMPTY_SESSION_ERR` match, re-raises everything else, sets `concept_missing` (03:263); `render_unprotected` runs (03:370); exit `0 if verdict.concept_missing else 1` (03:377). Verified current.
- **Fix 2 — exclude `CoOccurrence`:** `parse_outbound_neighbours` uses `stripped in STRUCTURAL_EDGE_LABELS` = `("Causal", "Dependency", "Hierarchical")`. Verified current.
- **R1-1 — prominent stderr banner + exit 0 kept:** `_flag_empty_session` (`03:324-343`) writes a 62-`!` rule + three ALL-CAPS lines to **stderr** with `flush=True`, called only on the concept-missing path (03:371-372); exit stays 0 (a warning, not an error). Destructive `aws-call` is only ever *described* (`describe_destructive_call`); no mutating call exists on any path. No regression.
- **R1-4 — cross-reference + whitelist self-test:** comment at `_lambo.py:108-116` ties `STRUCTURAL_EDGE_LABELS` to Rust `is_structural` (`src/cli/serve_web.rs`) / `DRIFT_EDGE_TYPES` (`src/daemon/drift.rs`) — accurate; `_self_test_structural_whitelist` covers the full known set. Verified current.
- **R1-N5 — dead import removal:** `parse_outbound_neighbours` removed from the 02 import (`02_app_data_agent.py`); grep returns **zero** matches in 02. Verified.

**Compile + self-test:** `python3 -m py_compile` on all three files **PASS**; `python3 scripts/cloudops/_lambo.py` and `python3 -O scripts/cloudops/_lambo.py` both **PASS** (all three self-tests green, exit 0).

## 3. Findings

No P1, P2, P3, or nit remains in the worktree.

- **P1:** none. **P2:** none. **P3:** none. **Nits:** none.

## 4. Pending-Main integration items (not worktree defects)

1. **T8-R1-3 — README doc drift (doc-only).** `scripts/cloudops/README.md:235-243` still instructs "do not filter the neighbour list to the structural edge headings" / "superset" — the opposite of the structural whitelist; `README.md:150-151` ("1 when it found nothing to protect") is stale against the new exit-0-on-empty-session contract. Code is correct; the README was never synced. **Main must fix this before integration** (it is a docs defect, not a runtime one).
2. **Real-live D2 capture (strongly recommended).** `evidence/remed-t8-crossover-run.md` is a **synthetic** recapture (stubbed `recall`/`inspect`, stand-in ids, error string quoted from source, not captured live). It accurately reproduces the script logic — adequate for script-logic replay — but **not live-exhibit proof**. Before D2, strongly recommended: a real capture of both cases, especially **case2** (live `Focus::Missing` error + script exit 0 + stderr banner on an empty/mistyped session), and **case1** (structural whitelist + read-only refusal against the real exhibit graph/account). **Flagging clearly for Main to decide before the video.**

---

```
{
  "verdict": "APPROVE",
  "findings": { "P1": [], "P2": [], "P3": [], "nits": [] },
  "summary": "Round-3 final clearance: the T8-R2-1 P3 is genuinely remediated — _self_test_empty_session_sentinel now sources the real Focus::Missing error text from src/cli/inspect.rs via _source_focus_missing_text (targeting the error arm and skipping the unrelated resolve_focus return), asserts EMPTY_SESSION_ERR is present, and fails closed (SystemExit) under stale-constant, CLI-reword, missing-arm, and unreadable-source scenarios — all empirically verified against the real functions. N1's comment is now consistent with the (now true) fail-loudly claim; N2's update-tuple-AND-banner instruction is present. The two original fixes (empty-session render_unprotected; CoOccurrence exclusion via STRUCTURAL_EDGE_LABELS) and all R1 remediations (R1-1 prominent stderr banner + exit 0 kept + destructive never issued; R1-4 Rust cross-reference + whitelist self-test; R1-N5 import removal) still hold with no regression. py_compile passes on all three scripts; the _lambo.py self-test is green under both python3 and python3 -O. No P1/P2/P3/nit remains in the worktree. Worktree is clean and integration-ready. Two items are flagged for Main, not the worktree: (1) the T8-R1-3 README doc drift (scripts/cloudops/README.md:235-243 and 150-151 still contradict the whitelist/exit-status behavior) must be fixed at integration; (2) a real-live capture of both D2 cases — especially case2's live Focus::Missing error + exit 0 + stderr banner — is strongly recommended before the video, since the committed evidence is a synthetic recapture adequate for script-logic reproduction but not live-exhibit proof."
}
```
