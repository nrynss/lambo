# T7 — Adversarial re-review, Round 2 (CloudOps agents remediation)

**Task:** Re-review the remediated T7 change set at `scripts/cloudops/` after
Round 1 (`adve-review-remed-T7round1.md` — verdict `APPROVE`, one P3 + four
nits). Three code items were remediated (T7-R1-1, T7-R1-3, T7-R1-4); two were
out-of-scope doc/integration items (R1-2 README, R1-5 em dashes — pending Main).
This round re-reads the **full** current diff, verifies each remediation is
genuine (including fail-closed under `python3 -O` and import isolation), and
re-confirms the 9 original T7 fixes hold with no regression.
**Reviewed (read-only):** full `git diff` of the three files; re-read the
changed regions (`02_app_data_agent.py:141`, `~294-324`, `~332-375`;
`01_network_agent.py` `_peer_label` + `~347-352`, `~409-417`;
`_lambo.py:596-620`); live execution of `py_compile`, the self-check (normal and
`-O`), import isolation, and offline spot-checks of the original fixes.
**Verdict:** `APPROVE`.

---

## Remediation verification (each checked adversarially)

### T7-R1-1 (P3) — `02_app_data_agent.py:141` dry-run plan text — CLEARED

`_plan` now emits:

```python
would("inspect", ", ".join(REQUIRED_FROM_NETWORK_AGENT), "by name, expecting each present")
```

which reads `inspect SG-Base-VPC, Subnet-Private-1a by name, expecting each
present` (REQUIRED_FROM_NETWORK_AGENT is exactly `(SG_BASE_NAME,
SUBNET_PRIVATE_NAME)`). Verified **accurate**: `read_network_topology`
(`02:294-324`) loops `for name in REQUIRED_FROM_NETWORK_AGENT: lam.inspect(name,
depth=1)`, treats the focus itself as always-reported, and raises `InfraError`
on an absent node — i.e. precisely "inspect … by name, expecting each present."
No `depth 1` / VPC framing survives, which was the point: a reviewer acting on
the dry-run line is told the agent uses the bounded-neighbour check no longer.
Format is consistent with the sibling `would(verb, subject, detail)` calls
(`:129`, `:130`, …). **Actually remediated, correct.**

### T7-R1-3 (nit) — `01_network_agent.py` "exactly one" / "cannot carry" — CLEARED

Both stale phrasings re-cast to the CHILD end:

- `derive_account_bindings` docstring (`:349-351`): "an ARN's colons cannot
  appear on the CHILD end of `--parent-of` (the first colon is the separator)."
- `derive_security_rules` pre-filter comment (`:411-413`): "The rule text
  becomes the CHILD end of `--parent-of`; an IPv6 CIDR renders with colons, so
  the child end cannot carry it."

Both are literally accurate — the restriction is child-end-only (the round-1
review confirmed the parent accepts a colon-bearing IPv6 CIDR via first-colon
split). Operation is unchanged. **Actually remediated, correct.**

### T7-R1-4 (nit) — `_lambo.py` self-test `assert` → fail-closed — CLEARED

`_self_test_ipv6_parent` (`_lambo.py:596-615`) replaces the two `assert`s with
explicit `if`-guarded `raise SystemExit(<message>)`:

```python
if flags != expected:
    raise SystemExit(f"self-test FAILED: _parent_of_flags returned {flags!r}, expected {expected!r}")
try:
    _parent_of_flags([("2001:db8::/64", "SG:Public")])
except InfraError:
    pass
else:
    raise SystemExit("self-test FAILED: a colon-bearing child must still be refused")
```

Verified:
- **Exit status.** `raise SystemExit("msg")` prints the message to stderr and
  exits with status **1** (non-zero) — correct for the tools-and-`$?` convention
  the script's `if __name__ == "__main__"` block relies on.
- **Fail-closed under `-O`.** These are plain `if` checks, not `assert`, so
  `python3 -O` removes nothing. `python3 -O scripts/cloudops/_lambo.py` exits 0
  only because the checks *pass*; a regression in `_parent_of_flags` would still
  raise `SystemExit` under `-O`. No path prints the success line (`:620`) until
  both checks have passed — there is no "passing-but-mis-verified" branch.
- **Isolation preserved.** Still inside `if __name__ == "__main__":` (`:618-620`);
  `import _lambo` (as `01`/`02`/`03` do) remains silent — re-verified live below.
- `NEIGHBOUR_METADATA_RE` / `_run` / `_require_executable` docstrings were
  expanded but are comment-only; no logic changed.

**Actually remediated, correct and robust.**

---

## The 9 original fixes still hold (no regression)

Re-verified against the current tree (only one spot-check needed a corrected
input — see note):

| T7 item | Round-2 evidence | Status |
|---|---|---|
| **T3-1-P1-1** split co-derive | `02:356-383` — separate RDS and Lambda `lam.derive` calls; DB subnet group stays with RDS, roles with Lambda; exhibit-only `elif` preserved. | INTACT |
| **T3-1-P2-1** `_run` surfaces real error | `_lambo.py:307-322` — `next(...)` picks first non-empty, non-`For more information` line; `exit N` fallback. | INTACT |
| **T3-1-P2-2** executable check | `_lambo.py:198-209` `_require_executable`; spot-checked nonexistent→InfraError, non-exec→InfraError. | INTACT |
| **T1b-R1-3** IPv6 parent deferral | `_parent_of_flags` round-trips `SG-PublicWeb:2001:db8::/64`; child colon refused — live. | INTACT |
| **T3-1-P2-4** by-name prereq | `02:294-324` inspects each `REQUIRED_FROM_NETWORK_AGENT` by name, focus always reported, absent→InfraError. | INTACT |
| **T3-1-P3-1** `_peer_label` module scope | `01:221-239` module function, `/32`/`/128`→"the operator address". | INTACT |
| **T3-1-P3-2** anchored metadata regex | `NEIGHBOUR_METADATA_RE` (`:114-117`); end-to-end — inner-bracket kept, non-metadata trailing bracket kept, metadata stripped, `Derives`/`Temporal` provenance excluded per `PROVENANCE_EDGE_LABELS`. | INTACT |
| **T3-1-P3-3** stale-release preference | `mtime`-max; both directions live. | INTACT |

> **Self-correction note:** an initial spot-check fed `parse_outbound_neighbours`
> provenance rows under a `CoOccurrence` heading and got them included; that was
> a test-artifact, not a defect — `PROVENANCE_EDGE_LABELS = ("Derives",
> "Temporal")` and the real `inspect` renderer emits `<interaction …>` rows under
> those headings, where they are correctly excluded (confirmed live).

---

## Regression scan of the full diff

- `02_app_data_agent.py`, `01_network_agent.py`: changes are remediation
  rewording, `_peer_label` relocation (same logic, module scope), and
  comment/docstring expansion. No removal or weakening of any functional line.
- `_lambo.py`: additions only — `os`/`re` imports, `_require_executable`,
  `NEIGHBOUR_METADATA_RE`, `_run` message pick, `_parent_of_flags` docstring,
  `parse_outbound_neighbours` regex trace, and the hardened self-test. No
  functional deletion.
- `py_compile` passes on all three files; self-check exits 0 both with and
  without `-O`; `import _lambo` is silent and the round-trip + child-refusal
  re-exercised successfully under import.

## Isolation

`if __name__ == "__main__":` at `_lambo.py:618-620` is the only live entry; no
wrapper runs `_lambo.py` as main (grep-confirmed in round 1), so the self-test
never blocks the agent scripts.

---

## Pending-Main doc items (not worktree defects — noted, not findings)

- **R1-2 — `scripts/cloudops/README.md:197-202`** still reads "`--parent-of
  CHILD:PARENT` takes exactly one colon … an ARN, a URL or an IPv6 CIDR cannot
  be a hierarchy end." This is now stale *for the parent side* (an IPv6 CIDR can
  be a parent). It is outside the three-file change set; Main should fold it
  into the integration docs pass, e.g. "the child end must be colon-free; a
  parent may carry colons (IPv6 CIDR)."
- **R1-5 — AGENTS.md em dashes**: cleared in Round 1 (prose-only, no AWS
  resource identifier). No action.

---

## Findings

### P1
None.

### P2
None.

### P3
None. (T7-R1-1, the sole Round-1 P3, is genuinely remediated.)

### Nits
None new. (R1-3, R1-4 remediated in code; R1-2 is a pending-Main doc item; R1-5
cleared.)

---

## Summary

The three code remediations are genuine and adversarially verified: the dry-run
plan line now names the by-name inspection exactly as the real
`read_network_topology` behaves; the two `01` colon comments are re-cast to the
child end and are literally accurate; and the self-test's `assert`s are replaced
with explicit `if`-guarded `raise SystemExit`, which is fail-closed under
`python3 -O` (plain `if`, not `assert`), exits non-zero, and leaves `__main__`
isolation intact — importing `_lambo.py` stays silent. All 9 original T7 fixes
still hold under live spot-checks, the split-derive, by-name prereq, exec check,
error surfacing, anchored metadata regex, and mtime preference included; `py_compile`
passes and the self-check exits 0 both normally and under `-O`. The only
outstanding item is the Round-1 README colon rule, which is a pending-Main doc
item outside this change set, plus the already-cleared R1-5 em dashes. No P1/P2,
no remaining P3, no remaining nits → **APPROVE**.

```json
{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [],
    "nits": []
  },
  "summary": "All three Round-1 code remediations verified genuine: T7-R1-1 plan text now says 'inspect SG-Base-VPC, Subnet-Private-1a by name, expecting each present', matching the real by-name read_network_topology; T7-R1-3 both 01 comments re-cast to the CHILD end and accurate; T7-R1-4 self-test replaces assert with explicit if-guarded raise SystemExit, fail-closed under -O (plain if, non-zero exit), __main__ isolation preserved (import silent). All 9 original T7 fixes re-verified intact with no regression; py_compile passes; self-check exits 0 with and without -O. README colon rule (R1-2) and AGENTS.md em dashes (R1-5) are pending-Main doc items, not worktree defects. No P1/P2/P3/nits. APPROVE."
}
```
