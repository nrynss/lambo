# Adversarial review — remed-T8 (round 1)

**Scope:** `scripts/cloudops/03_crossover_protect.py` + `scripts/cloudops/_lambo.py` (remed-T8 worktree, detached HEAD `555698e`).
**Task (from `dev-diary/notes/remediation-tasks.md` § T8):** closes T3-2-P2-1 (empty-session `render_unprotected`) and T3-2-P2-2 (exclude `CoOccurrence` from `parse_outbound_neighbours`). The task mandates running the script live against the real exhibit.
**Method (read-only):** full diff review; `py_compile` on both files (PASS); live-reproduced both fixes via a synthetic `inspect`-format parse check and by driving `run_guard` + `render_unprotected` + `main` exit-code path with a stubbed `Lambo`; verified the exact lambo CLI error string at `src/cli/inspect.rs`; cross-checked the whitelist against the Rust structural-edge definition and every other caller. No source edited; exactly this file written.

**Verdict: APPROVE.**
**Disposition: APPROVE** — both fixes are correct and reproduce; no P1/P2. Four P3s and two nits to address comfortably in a later round.

---

## Summary of the two fixes

### Fix 1 — empty-session `render_unprotected` (03_crossover_protect.py)
`run_guard` wraps `lam.inspect(SG_BASE_NAME, depth=1)` in `try/except InfraError`, swallowing **only** the error whose message contains `"no concept matching"`, setting `verdict.concept_missing = True`, and re-raising any other `InfraError`. `main` returns `0 if verdict.concept_missing else 1`.

**Verified:**
- The swallowed condition is *exactly* the empty-session error. lambo emits `no concept matching '{focus}' in session '{session}'` only at `src/cli/inspect.rs:304` (`Focus::Missing`); a repo-wide grep finds no other CLI path producing that phrase. A genuine failure (Cockroach refusal, timeout, malformed config) does **not** contain it and is re-raised. Confirmed both branches empirically.
- `render_unprotected()` runs and produces sane output (no crash; `blast radius 0`, no dependents).
- Exit-0-on-empty is the documented, deliberately-chosen semantic (module docstring updated in this diff). It is internally consistent: the guard **never** issues the destructive action regardless; exit 0 = "guard resolved and refused" for both the blocked and empty cases; exit 1 = "present-but-unprotected, investigate".
- **Recall path is unaffected and needs no guarding.** `lam.recall` (`src/cli/recall.rs`) raises only `Usage`-level errors (missing/overlong session/query args); on an empty session it returns empty context with exit 0. `inspect` is the only verb that fails with "no concept matching." So guarding inspect alone is complete.

### Fix 2 — exclude `CoOccurrence` from `parse_outbound_neighbours` (_lambo.py)
Replaced the provenance-exclusion filter (`counts = stripped not in PROVENANCE_EDGE_LABELS`) with a strict structural whitelist (`STRUCTURAL_EDGE_LABELS = ("Causal", "Dependency", "Hierarchical")`; `counts = stripped in STRUCTURAL_EDGE_LABELS`).

**Verified:**
- The whitelist **exactly** matches the CLI's structural edge set. Every authoritative Rust source treats precisely `{Dependency, Causal, Hierarchical}` as structural: `is_structural` (`src/cli/serve_web.rs:495-500`), `DRIFT_EDGE_TYPES` (`src/daemon/drift.rs`), blast-radius counting (`src/store/memory.rs`), the inspect page (`src/cli/serve_web.rs:449-451`). The other four types — `CoOccurrence`/`Semantic` (decaying, spec §5) and `Derives`/`Temporal` (interaction provenance) — are correctly non-structural. **No structural edge label is missing**, so no real dependent can be dropped.
- No genuine dependent is lost. A structural dependent by definition carries one of the three whitelisted edges, and the renderer emits each neighbour once under the heading of the edge that reached it first; a `Hierarchical` child renders under a `Hierarchical` heading and is kept. The old docstring's fear ("a hierarchy child frequently surfaces under `CoOccurrence` and never under `Hierarchical`") is obsolete: per `derive.rs`, `ParentOf` children get `Hierarchical` edges and do **not** join the pairwise `CoOccurrence` step, and T7's split co-derive removed the cross-tier `CoOccurrence`. Reproduced: a mixed synthetic banner yields exactly the structural dependents, never `CoOccurrence`/`Semantic`/`Derives`/`Temporal`/hop-2 rows.
- **Other callers are not broken.** `02_app_data_agent.py` imports `parse_outbound_neighbours` (line 90) but **never calls it** — `read_network_topology` (lines 310-324) checks each required node via per-name `lam.inspect` with `try/except`. `parse_blast_radius` (unchanged) is unaffected. Coherence *improves*: the parser now agrees with the `blast radius:` count on the same structural set, rather than reporting a superset.

---

## Live-run assessment (mandatory part of the task)

**Assessment: the executed run is sound and read-only, and its described outcomes genuinely demonstrate both fixes.** Three grounds:

1. **Behavior reproducible from the code.** `case1` (banner lists `RDS-Lambo-Demo-DB` + 3 structural/constraint children, **no** `CoOccurrence`/subnets/interactions) is exactly what the structural whitelist produces (reproduced on synthetic input matching the real `render_neighbourhood` format from `evidence/mcp-client-stdio/stdio-all-seven-tools.jsonl` and `docs/reference/cli.mdx`; `RDS-Lambo-Demo-DB named first among its dependents` also matches the README's stated ordering). `case2` (exit 0, `render_unprotected` ran, root cause = `lambo inspect` exit 1 "no concept matching") is exactly the `except InfraError` path, reproduced end-to-end against the exact source string.
2. **Read-only is structurally guaranteed, independent of the specific live output.** The script performs **no** AWS mutation — `main` only renders `render_blocked`/`render_unprotected`; the destructive `aws-call` is only ever *described* via `describe_destructive_call` (03:300: "no AWS resource was created, modified or deleted"; the action line is never invoked). It uses only **reader** verbs `recall`/`inspect`, which "never touch the lease" (_lambo.py:262-266, 03 docstring:44 "uses read verbs only"). The sibling binary `/home/nryn/work/lambo/target/release/lambo` is invoked as a read-only subprocess for `inspect`/`recall` — neither verb writes repo files or acquires the writer lease, so the sibling repo is not mutated.
3. **Caveat (evidence availability):** no committed evidence file in the worktree captures the raw live transcript — the probe id `t8-empty-session-probe-9f3` appears nowhere under `evidence/` (only unrelated `vector-index` text contains "9f3"). The described behavior is fully reproduced and consistent with the code and exact CLI error string, but the *exact* live bytes cannot be independently re-verified from the repo on disk. Recommend the implementer capture the two run transcripts under `evidence/` for the demo/defense.

---

## Findings

### P1
None.

### P2
None.

### P3

**T8-R1-1 — Exit-0-on-empty can mask a wrong-session-id / unpopulated-session misconfiguration.**
`03_crossover_protect.py:353` (`return 0 if verdict.concept_missing else 1`).
The empty-session condition is precisely the case `render_unprotected` attributes to "01/02 have not run against this session, **or they ran against a different session id**" (03:313-314). Previously an empty session was a loud `InfraError`/nonzero; now it is a quiet exit 0 with a single `warn` line. The safety invariant is preserved (the guard still refuses any destructive action), and this is a deliberate, documented, demo-scoped choice — but any pipeline consuming the exit code will treat a mistyped/empty session id as green success.
*Fix (optional):* keep the distinction on the exit code (e.g. reserve exit 2 for "focus absent / empty session" while keeping 0 = blocked and 1 = present-but-unprotected), or at minimum ensure the warning is prominent enough for an operator to notice a wrongly-targeted session.

**T8-R1-2 — The empty-session gate is a fragile substring match against CLI prose.**
`03_crossover_protect.py:255` (`if "no concept matching" not in str(exc)`).
Today the phrase is emitted only by `src/cli/inspect.rs:304`, so the match is precise and no real error is swallowed (verified). But it is coupled to the exact CLI wording: any future reword (e.g. "no matching concept", "unknown focus") quietly reverts the guard to aborting on empty sessions — the original T3-2-P2-1 bug — with no test catching it.
*Fix (optional):* hoist the sentinel to a module constant beside the reader-verb contract in `_lambo.py` and/or add a parse/unit check asserting `"no concept matching" in str(<the Focus::Missing InfraError>)` so a wording drift fails loudly.

**T8-R1-3 — README doc drift contradicts the fix.**
`scripts/cloudops/README.md:235-244` still instructs "**do not filter the neighbour list to the structural edge headings**" and describes `parse_outbound_neighbours` as "keeps everything except the pure provenance kinds, which makes it a superset" — the exact **opposite** of the new whitelist behavior. `README.md:150-151` ("Exit status is 0 when it blocked … and 1 when it found nothing to protect") is also stale (now exit 0 on empty session). The code docstring was updated; the README was not. A maintainer following the README could revert the fix or be misled.
*Fix:* update the README paragraphs to match the structural-whitelist behavior and the new exit-status contract.

**T8-R1-4 — Whitespace mirror of the structural-edge definition introduces a duplication seam.**
`_lambo.py:99-107` (`STRUCTURAL_EDGE_LABELS`) is a Python string-literal twin of the Rust structural set (`is_structural` at `src/cli/serve_web.rs:495-500`, `DRIFT_EDGE_TYPES`). It matches exactly today and none of the four excluded types is a real dependency, so nothing is wrong — but if a structural edge type is ever added (or an existing one renamed) in Rust, this list will silently drift out of sync and the parser would first under-/over-count before any test catches it.
*Fix (optional):* add a cross-reference comment tying the constant to the Rust `is_structural`/`DRIFT_EDGE_TYPES`, and/or extend the self-test with a synthetic banner that fails if any new heading starts (or stops) counting.

### Nits

**T8-R1-N5 — Dead import of `parse_outbound_neighbours` in `02_app_data_agent.py:90`.**
It is imported but never called (`read_network_topology` uses per-name `lam.inspect`). Pre-existing (T8 didn't touch 02), and it confirms the whitelist change breaks no caller — but the unused import should be dropped.

**T8-R1-N6 — Em-dashes.** The em-dashes introduced by T8 are confined to comments/docstrings/help text (e.g. `03_crossover_protect.py` docstring, `_lambo.py` whitelist comment); **no em-dash appears in any AWS resource name or description**, so `AGENTS.md:17-18` is not violated. Verified clean; no action.

---

## Additional review notes (checked, non-findings)

- **Recall path** — needs no guarding (see Fix 1 verification): `recall` never returns the empty-session error, so `inspect` is the only verb that fails and the fix is complete.
- **Non-empty guard path** — the `CoOccurrence` exclusion cannot change the standing outcome: a genuine blast-radius / structural dependent still blocks (exit 0, `render_blocked`); only false positives on the abort banner were removed.
- **Code hygiene** — `PROVENANCE_EDGE_LABELS` fully removed, no dangling references; `exclude_reasons`/`render_*` unaffected; `py_compile` passes.

---

```
{
  "verdict": "APPROVE",
  "findings": { "P1": [], "P2": [], "P3": ["T8-R1-1", "T8-R1-2", "T8-R1-3", "T8-R1-4"], "nits": ["T8-R1-N5", "T8-R1-N6"] },
  "summary": "Both remed-T8 fixes are correct and were reproduced end-to-end. Fix 1 (03_crossover_protect.py) wraps lam.inspect in try/except InfraError and swallows ONLY the exact 'no concept matching' empty-session error (src/cli/inspect.rs:304 is the sole emitter), re-raising all genuine failures; render_unprotected runs and produces sane output; exit 0 on concept_missing matches the updated doc. The recall step needs no guarding because recall raises only Usage-level errors and returns empty context (exit 0) on an empty session — inspect is the only failing verb, so the fix is complete. Fix 2 (_lambo.py) replaces provenance-exclusion with a strict structural whitelist (Causal/Dependency/Hierarchical) that exactly matches the Rust structural edge definition (is_structural/DRIFT_EDGE_TYPES); no structural type is missing, no genuine dependent is dropped (ParentOf children get Hierarchical edges, not CoOccurrence; T7 removed cross-tier CoOccurrence), and the only other importer (02_app_data_agent.py) never calls it, so nothing breaks. The mandatory live run is sound and read-only: the script issues no AWS mutation (the destructive action is only described, never executed) and uses only reader verbs, so neither the live session nor the sibling repo is mutated by --lambo-bin /home/nryn/work/lambo/target/release/lambo (inspect/recall are read-only subprocess verbs); both described case outcomes are reproduced from the code and exact CLI error string. Caveat: no committed evidence file captures the raw live transcript (probe t8-empty-session-probe-9f3 is absent from evidence/), so the exact live bytes are not independently re-verifiable from disk — behavior is nonetheless fully reproduced. Disposition: APPROVE, with four P3s (exit-0 may mask a wrong-session misconfiguration; substring-coupling of the empty-session gate; stale README contradicting the whitelist; string-literal mirror of the Rust structural set) and two nits (dead import in 02; em-dashes verified confined to comments, no AGENTS.md violation)."
}
```
