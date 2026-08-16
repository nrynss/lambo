# Adversarial review — remed-T8 (round 2, remediation verification)

**Scope:** `scripts/cloudops/03_crossover_protect.py` + `scripts/cloudops/_lambo.py` + `scripts/cloudops/02_app_data_agent.py` remediation in the remed-T8 worktree (detached HEAD `555698e`).
**Task:** round-2 adversarial re-review of the full current diff; verify each of the four R1 findings plus the nit is *genuinely* remediated; confirm the two original fixes still hold; no regression; py_compile + self-test; assess evidence sufficiency for D2.
**Method (read-only):** full diff review; read the current state of every changed region; reproduced the evidence's parse-unit and stderr banner through the *actual* code paths; empirically probed the drift-fail self-test under two CLI-reword scenarios; verified the Rust cross-references at `src/cli/serve_web.rs:495` and `src/daemon/drift.rs:97`; confirmed the import removal via grep; `py_compile` (PASS) and `_lambo.py` self-test (PASS). No source edited; exactly this file written.

**Verdict: APPROVE.**
**Disposition: APPROVE** — three of the four P3s and the nit are fully and genuinely remediated with no regression; the two original fixes hold; py_compile and self-test pass. One residual P3 (T8-R2-1) and two nits remain, all low-severity/optional. The committed evidence is a **synthetic** recapture, not a live run: **adequate** as a behavioral reproduction of the script logic, but a **real-live capture is still strongly recommended before D2** for a robust defense (flag for Main, not a worktree defect).

---

## Remediation-by-remediation verification

### T8-R1-1 (P3) — empty-session exit 0 + prominent warning → **REMEDIATED, verified**
- **Exit 0 kept:** `main` (03:377) returns `0 if verdict.concept_missing else 1`; `render_unprotected` runs first (03:370), then `_flag_empty_session` is called only on the concept-missing path (03:371-372). Confirmed.
- **Destructive action never issued:** the only executable path in `main` is `confirm_shared` + `run_guard` (both reader-only) then `render_*`. The destructive `aws-call` is only ever *described* via `describe_destructive_call`. No mutating AWS call exists on any path. Confirmed no regression.
- **Warning is prominent/unmissable:** `_flag_empty_session` (03:324-343) writes a 62-`!` rule + three ALL-CAPS `!! … !!` lines to **stderr** (so it survives stdout capture) with `flush=True`. I captured the banner from the real function and it matches the evidence's stderr block **byte-for-byte** (see evidence lines 165-169). Genuine.

### T8-R1-2 (P3) — hoist `EMPTY_SESSION_ERR` + drift-fail self-test → **PARTIALLY remediated (see T8-R2-1)**
- **Sentinel is module-level:** `EMPTY_SESSION_ERR = "no concept matching"` at `_lambo.py:127` (module scope). Confirmed via `hasattr`.
- **Both sides use it:** 03 imports it (03:64) and matches `EMPTY_SESSION_ERR not in str(exc)` in `run_guard` (03:255); `_lambo.py` self-test uses it (`_self_test_empty_session_sentinel`, 03:678-691). Confirmed.
- **The drift-fail self-test does NOT actually fail on a CLI reword** — see the adversarial finding T8-R2-1 below. The constant-hoist is a genuine improvement; the self-test's advertised guarantee is overstated.

### T8-R1-4 (P3) — cross-reference comment + whitelist self-test → **REMEDIATED, verified**
- **Comment accurate:** `_lambo.py:108-116` ties `STRUCTURAL_EDGE_LABELS` to Rust `is_structural` (`src/cli/serve_web.rs:495`) and `DRIFT_EDGE_TYPES` (`src/daemon/drift.rs:97`). I confirmed both sites exist and define exactly `{Dependency, Causal, Hierarchical}` (`matches!(ty, Dependency | Causal | Hierarchical)`); equals the Python tuple. Accurate.
- **Self-test covers whitelist semantics:** `_self_test_structural_whitelist` (03:637-675) mixes all seven rendered kinds plus a hop-2 decoy and asserts only the three structural dependents survive. It fails if a structural heading is dropped/renamed or an excluded kind starts counting (within the known set). Genuine. (Inherent limitation — it cannot anticipate a *future*-added kind unless the banner is updated per the comment — noted as nit T8-R2-N2.)

### T8-R1-N5 (nit) — dead import removal → **REMEDIATED, verified**
- `parse_outbound_neighbours` removed from the `02_app_data_agent.py` import (02:90). `grep` of `02_app_data_agent.py` returns **zero** matches — truly unused. Verified.

---

## Both original fixes still hold (regression scan)

- **Fix 1 — empty-session `render_unprotected`:** `run_guard` (03:252-265) wraps `lam.inspect` in `try/except InfraError`, swallows **only** the `EMPTY_SESSION_ERR` match, re-raises anything else, sets `concept_missing`, and lets `render_unprotected` run. Verified current. Empty-path safety is real: `parse_blast_radius("") == 0`, `parse_outbound_neighbours("") == []` — no crash on the empty `inspect_text`.
- **Fix 2 — exclude `CoOccurrence`:** `parse_outbound_neighbours` (03:594) uses `counts = stripped in STRUCTURAL_EDGE_LABELS`. Verified current; the EVIDENCE parse-unit reproduces **exactly** the real function output (`['RDS-Lambo-Demo-DB config block', 'rds-lambo-demo-db', 'vpc-route entry', 'SG-Base-VPC child']`).
- **Non-empty guard path unchanged:** `render_blocked` → exit 0 (blocked with dependents present); `render_unprotected` → exit 1 (present-but-unprotected); only *addition* is the conditional `_flag_empty_session` + comment. No behavior change to the present-but-unprotected or blocked paths.
- `py_compile` on all three files: **PASS**. `python3 scripts/cloudops/_lambo.py`: **PASS** (all three self-tests green, exit 0).

---

## Adversarial finding on the drift-fail self-test — T8-R2-1

The remediation claims (`_lambo.py:124-126`) that the self-test makes "a future reword of the CLI fail loudly instead of silently reverting the empty-session guard to a hard abort." **This claim is not achieved.**

The self-test anchors `EMPTY_SESSION_ERR` to a **hardcoded, duplicated copy** of the error string (`InfraError("no concept matching 'SG-Base-VPC' in session 't8-probe'")`, line 686) — it never reads the live CLI or the Rust source. I probed both scenarios empirically:

| Scenario | Constant value | Self-test | Real guard on a reworded CLI |
|---|---|---|---|
| CLI reworded, constant **updated** to new phrase | `"no matching concept"` | **FAILS** (not in pinned old string) | matches new phrase |
| CLI reworded, constant **left stale** (the R1-2 silent-revert worry) | `"no concept matching"` | **PASSES** (still in pinned old string) | guard now misses live error → aborts on empty session = the original T3-2-P2-1 bug |

So the test only detects *constant-vs-pinned-phrase* inconsistency (scenario A), **not** the regression R1-2 actually flagged — a CLI reword with a stale constant travels silently (scenario B), exactly the failure that reintroduces the P2 bug. Today the CLI phrase is unchanged, so this is a **latent, not active**, risk — hence P3, not P2.

**Recommendation (optional):** make the self-test source the real string — e.g. read `src/cli/inspect.rs` at test time and assert `EMPTY_SESSION_ERR` appears in it (or shell out to the live `lambo` binary and recognise the `Focus::Missing` error), and/or correct the comment to "pinned against a representative copy; update both if the CLI wording changes." The constant-hoist itself is the substantive improvement and stays.

---

## Evidence sufficiency for D2 (flag to Main, not a worktree defect)

`evidence/remed-t8-crossover-run.md` is **synthetic**, not a live capture: it stubs `Lambo`'s `recall`/`inspect`, invokes no AWS API and no real `lambo` binary, and uses stand-ins (`sg-0abc1234`, `vpc-0def5678`, `t8-demo-session`, `t8-empty-session-probe-9f3`). The critical empty-session error string is **quoted verbatim from `src/cli/inspect.rs` (source attribution), not captured live** (evidence lines 13-15).

**Positive (what it *is* good for):** it exercises the **real Python code paths** end-to-end (`run_guard → render_blocked/render_unprotected → main` exit mapping; `parse_outbound_neighbours`), and I verified its three outputs against the actual code **byte-exact**: the parse-unit list matches the real function, and the stderr banner matches the real `_flag_empty_session`. Redaction is clean (no real identifiers/IPs anywhere — nothing to scrub). It is an accurate and complete behavioral reproduction of both fixes and the guard's read-only/refusal semantics.

**Gap (why live is still recommended before D2):** the single most defense-critical claim — that the *real* `lambo inspect` CLI emits exactly `no concept matching …` for an empty/unpopulated session and that `run_guard` swallows only that — rests on a source-quoted string and stubbed I/O, not a live run. For a video/defense, a **real-live capture of both cases is strongly advised**:
1. **case2 (empty/unpopulated session):** run the real binary/`03` against an empty (or mistyped) session; capture the live `Focus::Missing` InfraError, the script's exit 0, and the stderr banner. This directly backs the R1-2 swallow condition with live bytes.
2. **case1 (present):** run against the real exhibit graph to show the structural whitelist on real data and the read-only refusal against the real account.

If D2's demo only needs to show *script logic*, the synthetic evidence is adequate; if the defense hinges on proving live-exhibit + live-error behavior, a real capture is **needed**. R1 already recommended live transcripts; the remediation chose synthetic instead — flagging this so Main can decide before the video.

---

## Findings

### P1
None.

### P2
None.

### P3
**T8-R2-1 — R1-2's drift-fail self-test does not actually catch a CLI reword; it only catches constant-vs-pinned-phrase drift.**
`_lambo.py:686` anchors `EMPTY_SESSION_ERR` to a hardcoded copy of the error, not the live CLI. Empirically: CLI reword + stale constant (the R1-2 silent-revert case) → self-test stays green while the real guard would miss the live error and reintroduce T3-2-P2-1. Latent (not active — CLI phrasing unchanged), hence P3. Constant-hoist remediation itself is genuine. *Fix (optional):* source the real string (read `src/cli/inspect.rs`, or shell out to the binary) or correct the overstated comment.

### Nits
**T8-R2-N1 — drift-fail self-test comment overstates its guarantee.** Lines `_lambo.py:124-126` say a CLI reword "fails loudly"; it does not in the stale-constant scenario (see T8-R2-1). Re-word the comment (or fix the test).

**T8-R2-N2 — the structural-whitelist self-test cannot anticipate a future-added edge kind.** `_self_test_structural_whitelist` guards only the seven kinds present in its fixed banner; a genuinely *new* Rust structural type would not trip it unless the banner is extended (the comment at `_lambo.py:114-116` already instructs this — inherent to any such unit test, minor).

---

## Summary

All four remediations were re-reviewed against the live current code. T8-R1-1 (prominent stderr empty-session banner, exit 0 kept, destructive never issued — banner reproduced byte-exact), T8-R1-4 (accurate Rust cross-reference verified at both cited sites + whitelist self-test covering the full known set), and T8-R1-N5 (import removal confirmed unused via grep) are **genuinely and fully remediated**, with no regression to the non-empty guard path. T8-R1-2 is partially remediated: the module-level sentinel and its use on both sides are real, but the advertised drift-fail self-test does not actually catch a CLI reword (T8-R2-1). Both original fixes (empty-session `render_unprotected`; structural-whitelist `CoOccurrence` exclusion) still hold and are regression-clean; `py_compile` and the self-test pass. Evidence is a clean, behaviorally-accurate **synthetic** reproduction (byte-verified against the code) but not live — **a real-live capture is still strongly recommended before D2** for the defense (flag to Main).

```
{
  "verdict": "APPROVE",
  "findings": { "P1": [], "P2": [], "P3": ["T8-R2-1"], "nits": ["T8-R2-N1", "T8-R2-N2"] },
  "summary": "Round-2 remediation verified against the live current code. T8-R1-1 (prominent 62-rule stderr empty-session banner, exit 0 kept, destructive never issued — banner reproduced byte-exact from the real function), T8-R1-4 (Rust cross-reference confirmed accurate at src/cli/serve_web.rs:495 and src/daemon/drift.rs:97, plus a whitelist self-test covering the full known set), and T8-R1-N5 (parse_outbound_neighbours import removal confirmed truly unused via grep) are fully and genuinely remediated, with no regression to the non-empty guard path. T8-R1-2 is partially remediated: the module-level EMPTY_SESSION_ERR sentinel and its use by both 03 and the self-test are real, but the advertised drift-fail self-test does not actually catch a CLI reword — it anchors to a hardcoded copy of the error, so a reword leaving the constant stale (the exact R1-2 silent-revert scenario) travels green while the guard would miss the live error (T8-R2-1, latent/P3). Both original fixes hold (empty-session render_unprotected; structural-whitelist CoOccurrence exclusion); py_compile and the full self-test pass; the evidence parse-unit and banner reproduce the real code byte-exact. FLAG TO MAIN: the committed evidence is a SYNTHETIC recapture (stubbed Lambo, stand-in ids, empty-session error quoted from source, no live AWS/binary), not the real live exhibit — accurate as a behavioral reproduction and adequate for demonstrating script logic, but a real-live capture of both cases (especially case2's live Focus::Missing error, exit 0, and stderr banner) is still strongly recommended before D2 for a robust video/defense."
}
```
