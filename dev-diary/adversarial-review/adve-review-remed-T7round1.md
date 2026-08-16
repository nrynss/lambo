# T7 — Adversarial re-review, Round 1 (CloudOps agents)

**Task:** T7 (the 9 CloudOps-agent fixes; closes T3-1-P1-1, T3-1-P2-1..4, T3-1-P3-1..4
plus the T1b-R1-3 IPv6 deferral).
**Reviewed (read-only):** `scripts/cloudops/_lambo.py` (616 ln),
`scripts/cloudops/01_network_agent.py` (602 ln),
`scripts/cloudops/02_app_data_agent.py` (595 ln). Cross-checked against the lambo
Rust source (`src/graph/derive.rs`, `src/cli/inspect.rs`, `src/types/mod.rs`,
`src/cli/serve_web.rs`).
**Verdict:** `APPROVE`.
**Disposition:** no P1, no P2. The 9 fixes are all correct, verified by reading
plus live offline execution of every behavior that can be exercised without a
running lambo binary / live AWS session. One out-of-scope P3 doc drift plus
clear nits; nothing blocks.

This is a `scripts/cloudops/`-only, code-only change set. `git status` shows the
working tree differs from HEAD only in the three Python scripts and the two doc
files named below; the split-derive and the `__main__` self-test are provably
code-only (nothing launches a subprocess or a live session at import/parse time,
and the agents never invoke `_lambo.py` as `__main__` — see the critical section).

---

## Finding disposition (the 9 items, each verified adversarially)

| T7 item | Evidence (static + live) | Disposition |
|---|---|---|
| **T3-1-P1-1** split the co-derive | `02:347-383` two `lam.derive` calls. CoOccurrence semantics confirmed at `src/graph/derive.rs:312-345` + `:66-68`: the pairwise CoOccurrence step iterates **only the call's `concepts` argument** — the interaction root and `ParentOf` ends never join it. | **FIXED, correct** (details below) |
| **T3-1-P2-1** `_run` surfaces the real error | `_lambo.py:307-322` `next(...)` picks the first non-empty, non-`For more information` line; fallback `exit {rc}`. Executed against realistic Clap output. | **FIXED, correct** |
| **T3-1-P2-2** executable check | `_lambo.py:198-209` `os.access(path, os.X_OK)` → `InfraError` naming the path; applied to explicit/PATH/repo candidates (`:222-238`). Executed with a real non-executable file. | **FIXED, correct** |
| **T1b-R1-3** IPv6 parent deferral | `_lambo.py:398-434` `_refuse_colon` now guards the CHILD only; `_parent_of_flags` round-trips `SG-PublicWeb:2001:db8::/64`. Self-test at `:596-616`. Import-safety + round-trip executed live. | **FIXED, correct** (critical section below) |
| **T3-1-P2-4** network prereq no longer trivially truncates | `02:294-324` inspects each `REQUIRED_FROM_NETWORK_AGENT` node **by name** (focus always reported; absent → `InfraError`); never reads the bounded neighbour list. | **FIXED, correct** (one stale plan-text nit below) |
| **T3-1-P3-1** `_peer_label` to module scope | `01:221-239` module function; `/32`/`/128` → `"the operator address"`, else identity. Behavior kept. | **FIXED, correct** |
| **T3-1-P3-2** `rsplit(" [",1)` truncation | `_lambo.py:114-117` anchored `NEIGHBOUR_METADATA_RE`; used at `:573`. Verified against the real renderer (`src/cli/inspect.rs:142-158`) with internal-bracket and non-metadata-trailing inputs. | **FIXED, correct** |
| **T3-1-P3-3** stale-release preference | `_lambo.py:235-238` `max(existing, key=st_mtime)`. Executed with release-older and debug-older to confirm whichever is newer wins, and that a newest-but-non-executable candidate raises `InfraError`. | **FIXED, correct** |

---

## T3-1-P1-1 — the split genuinely removes the cross-tier edge (verified in source)

The false edge is `RDS-Lambo-Demo-DB ↔ Lambda-LamboStats-API` (and the roles):
it came from co-deriving both tiers in one interaction. `src/graph/derive.rs`
defines CoOccurrence as *pairwise among the interaction's `concepts` list only*
(`:312-345`, and explicitly `:66-68`: "`ParentOf` contents do **not** join the
pairwise `CoOccurrence` step"). So:

- **RDS call** (`02:357-367`): `derive(root=DB_SUBNET_GROUP, concepts=[RDS])`,
  `parent_of=[(VPC,db-subnet-group), (SG-Base-VPC,RDS)]`. `call_nodes = [RDS]` —
  a single concept produces **no** CoOccurrence pair. The interaction root
  (`DB_SUBNET_GROUP`) never joins CoOccurrence, so no RDS↔subnet-group edge was
  ever generated even before the split; none is lost same-tier.
- **Lambda call** (`02:369-375`): `derive(root=LAMBDA, concepts=[STATS(+EXHIBIT)])`.
  CoOccurrence confined to the lambda tier (`STATS↔EXHIBIT` when both present).

The cross-tier RDS↔Lambda CoOccurrence can therefore no longer be created or
reinforced. The pre-existing edge in the live session persists (the documented
"keep the session" tradeoff), but it is non-structural: `src/store/sqlite.rs:207`
excludes `CoOccurrence` from blast radius / interaction span, `src/cli/serve_web.rs:942-943`
ships only `Dependency/Causal/Hierarchical` on `/api/graph`, and
`03_crossover_protect.py`'s blast-radius count reads structural dependents. So the
legacy edge is inert for the demo. **Downstream (03 / demo) is unaffected.**

**All 7 `skip_rds`/`skip_lambda`/`exhibit_role` combinations were compared
against the pre-T7 single call and are preserved or improved** (notably the old
`skip_rds=False, skip_lambda=True, exhibit=True` case co-derived RDS with the
exhibit role in one interaction; the split now separates them — a strict
improvement in exactly the direction the fix intends). `check_single_source` is
still applied to the RDS pairs; the lambda call has no parents.

**Code-only confirmed:** the split is inside `derive_topology`, reached only from
`main` (`02:555`). Nothing runs a derive, re-derives, or relaunches the exhibit at
import/parse time. The `T7 (deferred …)` comment was removed from `_parent_of_flags`.

---

## T1b-R1-3 — IPv6 parent round-trip, child refusal, and `__main__` isolation (critical)

`_parent_of_flags([("2001:db8::/64","SG-PublicWeb")])` →
`["--parent-of", "SG-PublicWeb:2001:db8::/64"]`, and `("2001:db8::/64","SG:Public")`
raises `InfraError`. This matches `src/cli/derive.rs`'s first-colon split; the CLI
accepts a colon-bearing parent and the child stays colon-free (a colon in the child
would be read as the delimiter).

**`if __name__ == "__main__"` isolation — the critical check — was executed:**

- `python3 scripts/cloudops/_lambo.py` prints only the self-test line and exits 0.
- `import _lambo …` (as `01`/`02`/`03` do) is **silent** — no self-test, no side
  effect, no blocked import; the functions re-exercised successfully afterwards.
- The block at `_lambo.py:614-616` is guarded by `__main__`; `_lambo.py` is only
  ever imported by the agent scripts (verified by grep — no wrapper runs it as
  main). **Real tools are never blocked.**

**01 stays colon-clean on the CHILD ends:** `01_network_agent.py:400-432`
(`derive_security_rules`) builds hierarchy edges where each rule text is the
*child*; the `":" in text` pre-filter (`:410-415`) still skips IPv6 rules, which is
correct because a child may still not carry a colon. Its sibling
`derive_account_bindings` (`:340-366`) and `derive_vpc_invariants` likewise never
place a colon-bearing concept on the child side. No IPv6 parent is currently
derived by any live call — the deferral only *unlocks* the capability, correctly.

---

## Line-level verification notes (adversarial diligence)

- **`_run` (`P2-1`)**: executed with (1) a realistic Clap stderr →
  `failed: error: unexpected argument '--foo' found` (the real message, not the
  usage/tip/trailer); (2) leading-blank error → still picked the error; (3) a
  trailer-only stderr and (4) empty stderr → sane `exit N` fallback. A line is
  skipped only if empty or it *contains* `"For more information"`, so a genuine
  error line is never discarded.
- **`_require_executable` (`P2-2`)**: a mode-`0644` real file raises a named
  `InfraError`; `+x` accepted. All resolves funnel through it (`:222-238`); the
  `is_file()` pre-checks mean a directory is never passed to `X_OK`. Linux-only is
  fine (the comment says nothing else is supported).
- **`NEIGHBOUR_METADATA_RE` (`P3-2`)**: lists exactly the five `ConceptType`
  variants (`Entity|Logic|Constraint|Resource|Observation`, confirmed at
  `src/types/mod.rs:125-136`) plus the three real render statuses
  (`canonical|venerable|candidate`, confirmed at `src/cli/inspect.rs:145-150`),
  anchored with `$`. Exercised with (a) `[Entity]`, `[Entity, canonical]`,
  `[Entity, venerable]`, `[Constraint]` → stripped; (b) content **with an internal
  bracket** `report [Q3.1] details [Constraint]` → `report [Q3.1] details`
  (the original `rsplit` bug, now fixed); (c) a content *ending* in a non-type
  bracket `rule list [3 items]` → **left intact** (no over-strip). `parse_outbound_neighbours`
  run end-to-end against a full `render_neighbourhood`-shaped block (with the real
  no-colon `  Hierarchical`/`  CoOccurrence`/`  Derives` headings) returns exactly
  the concept dependents; `<interaction …>` provenance rows are correctly excluded
  and `Semantic` siblings kept.
- **`resolve_lambo_binary` (`P3-3`)**: with release mtime=1/debug mtime=2 → debug
  wins; flipped → release wins (truly "newer of the two", not "debug always");
  newest-but-non-executable → `InfraError`. Explicit `--lambo-bin` is verified
  `is_file()` then exec-gated (`:222-226`).
- **`_peer_label` (`P3-1`)**: module scope, single allocation, semantics identical.
- **No dead code, no unused imports** introduced by the change set (`os`, `re`,
  `_require_executable`, `_self_test_ipv6_parent` are all used/reached).

---

## Findings

### P1
None.

### P2
None.

### P3
- **T7-R1-1 — `02_app_data_agent.py:141`: dry-run plan text still describes the
  old bounded-neighbour check.** `_plan` emits `would("inspect", VPC_NAME, "depth
  1, expecting " + …)` — i.e. "inspect the VPC and read its hop-1 neighbours".
  The real `read_network_topology` (`:294-324`) no longer inspects the VPC at all;
  it inspects each required node **by name** (`SG-Base-VPC`,
  `Subnet-Private-1a`). The README markets the dry-run plan as "usable as a review
  artifact on a machine with nothing installed" (`README.md:204-206`), so a
  reviewer acting on this line would be told the agent still uses the very
  bounded-neighbour approach that caused the T3-1-P2-4 bug.
  *Fix:* reword to "inspect SG-Base-VPC and Subnet-Private-1a by name, expecting
  each present" (no `depth 1`/VPC framing).

### Nits
- **T7-R1-2 — `scripts/cloudops/README.md:197-202`: the "no colons" rule is now
  stale for the PARENT side.** It reads "`--parent-of CHILD:PARENT` takes exactly
  one colon and refuses more as ambiguous, so an ARN, a URL **or an IPv6 CIDR
  cannot be a hierarchy end**." After T7 an IPv6 CIDR *can* be a parent end; the
  restriction now applies to the child end only. Being an out-of-scope doc (outside
  the 3-file change set; Main may fold into integration), this is a nit, not a
  finding — but it actively contradicts the new capability, so fold it into the
  same docs pass, e.g. "the child end must be colon-free; a parent may carry colons
  (IPv6 CIDR) — so an ARN/URL/IPv6 can never be a *child*, but can be a *parent*."
- **T7-R1-3 — `01_network_agent.py:346-350` and `:410-415`: stale "exactly one" /
  "cannot carry" phrasing.** `derive_account_bindings`' docstring ("an ARN's colons
  cannot cross `--parent-of …`, which takes exactly one") and the rule pre-filter's
  comment ("which `--parent-of` cannot carry") are operationally accurate (both are
  about CHILD ends, still refused), but the blanket "exactly one"/"cannot carry"
  framing is no longer literal. Re-cast to "child end".
- **T7-R1-4 — `_lambo.py:596-611`: the self-test uses `assert`, a no-op under
  `python3 -O`.** `python3 -O scripts/cloudops/_lambo.py` would print the success
  line while asserting nothing. Harmless to the agents (they never run it), and a
  correct manual regression check; but a self-check that can silently vacate could
  be made fail-closed with explicit `if`-based checks.
- **T7-R1-5 — Checked, cleared: em dashes in `AGENTS.md`.** All four are in prose
  (the H1, a bullet, and two parentheticals) — none appears in an AWS resource
  name/identifier. The `_lambo.py` "exactly one place" em-dash convention refers to
  the byte-pinned §13 warning text; ordinary prose typography in `AGENTS.md` is
  unrelated. No AWS-name em dash exists.

---

## Summary

The T7 change set is clean. All nine fixes were verified against the actual lambo
source semantics and exercised live where the behavior is offline-testable: the
split-derive provably severs the cross-tier `CoOccurrence` while preserving
(and, in one flag combination, improving) same-tier grouping and all seven
`skip_*`/`exhibit` combinations; the `__main__` self-test is correctly isolated and
importing `_lambo.py` is side-effect free; the IPv6 parent round-trips while the
child stays colon-refused and 01's child-side pre-filter stays colon-clean; the
error-surface, executable check, by-name prerequisite inspection, module-scope
`_peer_label`, anchored metadata-strip regex, and mtime-based binary preference all
behave exactly as specified and stand up to adversarial input. The only finding is
a P3 stale dry-run description in `02`, plus clear doc nits (the README's now-wrong
"IPv6 cannot be a hierarchy end" being the most consequential, as expected and
scoped to integration). No P1/P2 → **APPROVE**, REQUEST_CHANGES not required.

```json
{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": ["T7-R1-1 (02_app_data_agent.py:141 stale dry-run plan text)"],
    "nits": [
      "T7-R1-2 (README.md colon rule stale for parent side)",
      "T7-R1-3 (01 comments 'exactly one'/'cannot carry' phrasing)",
      "T7-R1-4 (_lambo.py self-test uses assert, no-op under -O)",
      "T7-R1-5 (AGENTS.md em dashes — prose only, cleared)"
    ]
  },
  "summary": "All 9 T7 fixes verified correct against lambo source and offline execution; split-derive removes the cross-tier CoOccurrence while preserving same-tier grouping and all skip/flag combos; __main__ self-test isolated (import side-effect-free); IPv6 parent round-trips and child stays colon-refused; error-surface, exec-check, by-name prereq, module-scope _peer_label, anchored metadata regex, and mtime binary preference all correct. No P1/P2; one P3 stale dry-run description and clear doc nits. APPROVE."
}
```
