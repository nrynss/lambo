# T6 R3 — Final clearance review of the T6 remediation worktree

**Task:** Round-3 final-clearance review of the T6 worktree (`ca7860e`, detached HEAD),
read-only, before integration. Round-2 was `APPROVE` with a single nit (T6-R2-N1).
This is a verification that the worktree is clean and integration-ready.

**Reviewed (read-only):** the working-tree diff of `scripts/aws-infra/launch_exhibit_ec2.py`
and `provision_network.py`; the reworded call-site comment at `launch_exhibit_ec2.py:1266`;
rediscovered changed regions (import block, LLAMA_BLOCK, ensure_system_user, `_caddy_up`,
`wait_for_bootstrap`/`_console_tail`, NEW-5 VERIFY-BEFORE-D1 comment at 178-192);
re-rendered the real `USER_DATA` and `LLAMA_BLOCK` and `bash -n`'d both; re-checked the
regression set (port 80, `Restart=always`, imports, `--open-http`, em dashes).

**Verdict:** `APPROVE` — the single round-2 nit is closed; the worktree matches the
round-2 reviewed state with no logic regression; compile checks pass; no P1/P2/P3/nit
remains in the worktree. NEW-5 remains a documented D1 blocker (un-verifiable at review
time, precise TODO left). Clean for integration.

## R3 residual (T6-R2-N1) — closed

The only change since round-2 APPROVE is the one-line comment rewording at
`launch_exhibit_ec2.py:1266` (inside the NEW-2 block at the `wait_for_bootstrap` call site):

> `# on failure, print the console tail (console meta, NOT the boot log — the bootstrap redirects to /var/log/lambo-bootstrap.log).`

- **Comment-only:** confirmed via `git diff` — it touches no logic; immediate neighbours
  (`step("bootstrap")`, `wait_for_bootstrap(...)`, `say()`, `step("exhibit launched")`,
  the `boot log: sudo tail -f ...` note) are unchanged.
- **Accurate:** now honestly characterizes the printed tail as console meta, explicitly
  `NOT the boot log`, and points to `/var/log/lambo-bootstrap.log` — consistent with the
  R1-3-corrected docstring (1055-1060) and operator hint (1076-1078).
- **Grammatically clean:** complete, well-formed sentence; the em dash is a deliberate
  parenthetical separator in a comment, consistent with the pending-Main N2 set.

## Verification of the round-2 reviewed state (no regression)

| Check | Result |
|---|---|
| `python3 -m py_compile` (both scripts) | **pass** |
| Rendered `USER_DATA` via real renderers + `bash -n` | **pass** (rc 0) |
| Rendered `LLAMA_BLOCK` via real renderer + `bash -n` | **pass** (rc 0) |
| `require_subnet` / `require_sg` back in module namespace | **True / True** (R1-1) |
| `libgomp1` install restored in USER_DATA | present (R1-2) |
| No unsubstituted `@@...@@` placeholder in rendered USER_DATA | none |
| `Restart=always` on all three units (lambo-serve-web, caddy, llama) | **3 / 3** |
| Port-80 handling | `PUBLIC_INGRESS` = [(80,…),(443,…)]; `_check_port_80_open` defined (957) + called (1207); no `--open-http` in any `.py` |
| `--open-http` leftovers | none in scripts (README references are the pending-Main N1 doc item) |
| Em dashes | same comment/string set as R2 (launch:107,633,648,1267; provision:20) — none in AWS names/descriptions (pending-Main N2) |
| NEW-5 AMI honesty | VERIFY-BEFORE-D1 comment precise at 178-192 with exact `aws ssm get-parameter` paths, AMI logged not pinned; AMI logged not pinned |

All six round-1 remediations and the 11 originally-clean findings hold exactly as reviewed
in round-2; the only delta is the reworded comment at 1266. No logic regression.

## Findings

### P1
None.

### P2
None.

### P3
None.

### Nits
None. (T6-R2-N1 closed.)

## Pending-Main doc items (NOT in this worktree; not defects here)

- **T6-R1-6**: port-80 world-open decision record (`PUBLIC_INGRESS`) — doc change.
- **T6-R1-7**: console-tail note re instance IPs/hostname not pasted publicly — doc change.
- **T6-R1-N1**: README stale across the T6 surface (`--open-http`, Ubuntu/AL2023,
  t4g.medium, source-build, must-be-Graviton) — integration doc fix owned by Main.
- **T6-R1-N2**: em dashes in comments/user-facing strings (launch:107,633,648,1267;
  provision:20) — Main's optional broad cleanup; AGENTS.md rule not violated.

## NEW-5 — documented D1 blocker (must run before D1)

NEW-5 is an honest AMI-honesty guard that cannot be verified at review time: AWS creds
were expired when T6 landed, so `aws ssm get-parameter` could not be run against a live
plumbing account, and no AWS call is permissible here (read-only review). A precise TODO
is left at `launch_exhibit_ec2.py:178-192`: before the D1 clean redeploy, confirm in
us-east-1 that BOTH SSM paths (`…/26.04/stable/current/{arm64,amd64}/hvm/ebs-gp3/ami-id`)
return a value; if either 404s, correct `UBUNTU_SSM`. This is a legitimate pre-D1
verification step owned by Main, not a worktree defect — and it does not block integration
review, only the D1 deploy.

## Summary

The worktree is clean and integration-ready. The sole post-round-2 change is the
one-line comment rewording at `launch_exhibit_ec2.py:1266`, which is comment-only, accurate,
and grammatically clean, closing the round-2 nit T6-R2-N1. The full diff otherwise matches
the round-2 reviewed state: both P1s and the P2 remain closed, all P3s closed or pending-Main,
the 11 originally-clean findings hold with no regression, `py_compile` and `bash -n` on both
rendered shell blocks pass, `--open-http` is fully gone, `Restart=always` on all three units,
and no em dashes in AWS names/descriptions. NEW-5 remains a documented D1 blocker with a
precise TODO (178-192) that must be verified before D1. Doc items R1-6/R1-7/N1/N2 are
Main's integration-time responsibilities, not worktree defects. **APPROVE.**

```json
{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [],
    "nits": []
  },
  "summary": "Worktree is clean and integration-ready. The only change since round-2 APPROVE is the one-line comment rewording at launch_exhibit_ec2.py:1266 (now 'console meta, NOT the boot log — the bootstrap redirects to /var/log/lambo-bootstrap.log'): comment-only, accurate, grammatically clean — closes the round-2 nit T6-R2-N1. Full diff otherwise matches round-2 reviewed state with no logic regression: py_compile passes on both scripts; real-render bash -n passes on both USER_DATA and LLAMA_BLOCK; require_subnet/require_sg back in namespace; libgomp1 restored; Restart=always on all three units; port-80 handling and the 11 originally-clean findings hold; --open-http fully gone from scripts; no em dashes in AWS names/descriptions. NEW-5 remains a documented D1 blocker with a precise TODO at launch_exhibit_ec2.py:178-192 that must be verified (real aws ssm get-parameter) before D1 deploys — not verifiable at review time, owned by Main. Doc items R1-6/R1-7/N1/N2 are pending-Main integration responsibilities, not worktree defects. No P1/P2/P3/nit remains in the worktree. APPROVE."
}
```
