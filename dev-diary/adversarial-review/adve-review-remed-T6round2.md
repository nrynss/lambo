# T6 R2 — Adversarial re-review of the remediated launcher fixes

**Task:** Round-2 review of the T6 remediation worktree (`ca7860e`, detached HEAD).
Round 1 was `REQUEST_CHANGES` (2 P1 + 1 P2 + 5 P3 + 2 nits); the code findings were remediated.
**Reviewed (read-only):** full `git diff` of `scripts/aws-infra/launch_exhibit_ec2.py` and
`provision_network.py`; changed regions re-read in context (import block 57-92; ensure_system_user
314-338 + uses 373/374/541; LLAMA_BLOCK 514-621; `_caddy_up`/`wait_for_bootstrap`/`_console_tail`
1023-1099; main 1204-1273); `_common.py` exhibit role policy.
**Verdict:** `APPROVE` — all 6 code remediations are genuine and verified live; no P1/P2/P3 remain;
NEW-5 remains a documented D1 blocker; doc-only items are pending-Main, not worktree defects.
One new nit (stale inline comment) left for the final cleanup pass.

## Method

Static review of the full diff and changed regions, then live verification that needs no AWS:

- `python3 -m py_compile` on both changed scripts → **pass**.
- Rendered `USER_DATA` and `LLAMA_BLOCK` via the real renderers, ran `bash -n` on both embedded
  shell blocks → **pass** (quoting/heredoc expansion intact).
- Proved at runtime that `require_subnet`/`require_sg` are back in the module namespace
  (`True, True`), alongside `require_vpc`/`poll`.
- Exercised `ensure_system_user` against each branch (idempotent no-op, clean create, GID
  collision, UID collision) with stubbed system state, under `set -e`.
- Confirmed the R1-3 tempering decision against real evidence: the exhibit IAM role grants only
  `secretsmanager:GetSecretValue`/`DescribeSecret` (no `ssm:SendCommand`, no SSM managed-instance
  core), and USER_DATA installs no SSM agent.
- Regression scan of the 11 originally-clean findings and for `--open-http`/`open_http` leftovers,
  `Restart=` values, and em dashes in AWS resource names/descriptions.

## Verification of the six remediations

| Ref | R1 finding | Result |
|---|---|---|
| **R1-1 (P1)** | `require_subnet`/`require_sg` unused+unused-import? — actually dropped from import but still called → `NameError` on every real launch | **Verified fixed.** Import block (57-91) now lists `poll, project_filters, require_boto3, require_secret_arn, require_sg, require_subnet, require_vpc` — each once, sorted/deduped. Runtime: `'require_subnet' in dir(m)` → **True**, `'require_sg'` → **True**. `main` calls both bare (1204-1206) with no stray re-import. NameError gone. |
| **R1-2 (P1)** | `apt-get install -y libgomp1` deleted → llama-server fails `libgomp.so.1` on a fresh build | **Verified fixed.** `apt-get install -y libgomp1 >/dev/null` restored at LLAMA_BLOCK:540, inside USER_DATA, before `systemctl enable --now llama-server.service` (621). The comment (535-539) is now a complete, accurate sentence ("…libgomp1 is not in the base Ubuntu cloud image, so it is installed explicitly here…"). Exactly one install line; rendered block passes `bash -n`. |
| **R1-3 (P2)** | Console tail misrepresented as the bootstrap log | **Verified fixed.** Docstring (1055-1060) and the operator `hint` (1076-1078) both state plainly the tail is kernel/systemd/cloud-init meta, **not** the failing step, and point to pulling `/var/log/lambo-bootstrap.log` from the host. `_console_tail` (1090-1099) is best-effort: wrapped in `try/except ClientError`, returns a placeholder on error, never raises into the launch path. **Tempering choice is sound** given the evidence: the exhibit role policy (launch:798-808) grants only `secretsmanager:GetSecretValue`/`DescribeSecret` — no `ssm:SendCommand` and no managed-instance core — and USER_DATA never installs the SSM agent, so an honest "no SSM path here" message is correct, not a shortcut. One **stale inline comment** at :1266 still says "(bootstrap log) tail" — nit, see T6-R2-N1. |
| **R1-4 (P3)** | `_caddy_up` returned `True` on `AttributeError` | **Verified fixed.** `_caddy_up` (1023-1043) now returns `True` only on `ssl.SSLError`, with a rationale comment ("a mid-ACME temporary cert can upset the probe; still a live Caddy"); `AttributeError` is out of the success tuple, and genuine probe bugs surface as `False` via `(ConnectionRefusedError, TimeoutError, OSError)`. |
| **R1-5 (P3)** | Static UIDs 901-903 silently collide on a non-fresh image | **Verified fixed.** `ensure_system_user` (320-338) probes `getent group` for the GID and `getent passwd` for the UID, and `exit 1`s with a named conflict message before `useradd`. Confirmed live under `set -e`: idempotent path (existing user) → rc 0, no calls; clean create → rc 0, correct `groupadd --system --gid` + `useradd --system --uid --gid`; UID collision → rc 1, "cannot create system user 'newyu': UID 902 is already taken by 'uid902'"; GID collision → rc 1, "cannot create system group…". Applied to all three users (lambo 901 @373, caddy 902 @374, llama 903 @541 inside LLAMA_BLOCK). |
| **R1-8 (P3)** | Duplicated `note`/`one_or_none`/`project_filters` imports | **Verified fixed** — import block fully deduped/sorted (folded into R1-1). |

## Regression — 11 originally-clean findings still hold

| Ref | Verdict |
|---|---|
| T2-P2-1 port 80 | Holds. `PUBLIC_INGRESS = [(80,…),(443,…)]`; `--open-http` fully removed from both scripts (no `open_http`/`open-http` cs any .py); `_check_port_80_open` (957) defined and called at 1207. |
| T2-P2-2 sha coupling | Holds. `--bge-model-sha256 default=None`; `main` refuses custom URL without custom hash; `effective_bge_model_sha256` resolves default. |
| T2-P2-3 IAM retry | Holds. `_iam_propagation_error` narrows retry to IAM-shaped errors only. |
| T2-P2-4 stale prose | Holds. Source-build/ARM-only prose rewritten (module docstring, `--instance-type`/`--volume-size` help). |
| T2-P3-1 IP race | Holds. `poll`→running then `_ephemeral_ip` (120s, hard fail); stopped/stopping re-adopt refused. |
| T2-P3-2 caddy restart | Holds. `Restart=always` on lambo-web (456), caddy (492), llama (608). |
| T2-P3-3 static UIDs | Holds (strengthened into ensure_system_user; fully re-verified above). |
| T2-P3-4 health abort | Holds. Wrapper aborts after 3 consecutive inactive checks. |
| NEW-1 ref-keyed tarball | Holds. `LLAMA_TARBALLS` keyed ref→arch; `known_llama_cpp_ref` refuses unpinned refs. |
| NEW-3 layout verify | Holds. `-d $LLAMA_DIR`, `-x …/llama-server`, `ls libllama.so*` before cp/ln. |
| NEW-4 families | Holds. Families complete, `x2g.`+dot fixed. |
| NEW-5 AMI honesty | Holds (unchanged). VERIFY-BEFORE-D1 comment (176-194) precise, gives exact `ssm get-parameter` commands; AMI logged not pinned. Documented D1 blocker, not a code defect. |

**Security / info-leak:** no `--open-http` anywhere; SG descriptions use hyphens (AGENTS.md :17
"no em dashes in AWS resource names or descriptions" — nothing in a name/description uses an em
dash). The four em dashes found (launch:107,633,648; provision:20) are comments/user-facing
strings, none in AWS names/descriptions — same set as R1-N2, pending-Main.

## Findings

### P1
None. (R1-1, R1-2 verified closed.)

### P2
None. (R1-3 verified closed; tempering decision corroborated by the role policy + no SSM agent.)

### P3
None. (R1-4, R1-5, R1-6, R1-7, R1-8 all closed or pending-Main.)

### Nits

**T6-R2-N1 — stale inline comment at the `wait_for_bootstrap` call site.**
`launch_exhibit_ec2.py:1266` — the internal comment "…on failure, print the console (bootstrap
log) tail." still describes the printed tail as the bootstrap log, which is exactly the claim
R1-3 corrected everywhere else (docstring + operator hint are honest). It is an internal code
comment only — the operator-facing hint is correct — but it sits next to the corrected docstring
and re-introduces the same mischaracterization if anyone reads only the call site. Fix: change
"(bootstrap log) tail" to "(console meta) tail" or drop the parenthetical. One-line comment edit.

## Pending-Main doc items (NOT in this worktree; not defects here)

- **T6-R1-6** port-80 world-open decision record (provision_network `PUBLIC_INGRESS`) — doc change.
- **T6-R1-7** console-tail note re instance IPs/hostname not being pasted publicly — doc change.
- **T6-R1-N1** README stale across the T6 surface (`--open-http`, Ubuntu/AL2023, t4g.medium, source-build, must-be-Graviton) — integration doc fix owned by Main.
- **T6-R1-N2** em dashes in comments/user-facing strings (launch:107,633,648; provision:20) — Main's optional broad cleanup; AGENTS.md rule not violated.

## Summary

All six code remediations from Round 1 are genuine and verified end-to-end (live import namespace,
`py_compile`, `bash -n` on both rendered shell blocks, and a branch-level harness over
`ensure_system_user` under `set -e`). The two P1s that blocked shipping are closed: imports are
restored/deduped (NameError gone, proven at runtime) and the `libgomp1` install is back with a
complete comment. The P2 console-tail tempering is sound and backed by real evidence (role policy
grants no ssm, no SSM agent installed). All five P3s are closed or pending-Main. The 11 originally-
clean findings hold with no regression; `--open-http` is fully gone; `Restart=always` on all three
units; no em dashes in AWS names/descriptions. NEW-5 remains a documented, legitimate D1 blocker.
The only round-2 wrinkle is one stale inline comment (T6-R2-N1), a nit that does not block
approval. Doc items R1-6/R1-7/N1/N2 are integration-time responsibilities of Main, not defects in
this worktree. **APPROVE.**

```json
{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [],
    "nits": ["T6-R2-N1 stale inline comment at launch:1266 still calls the console tail the '(bootstrap log) tail', contradicting the R1-3-corrected docstring/hint; operator-facing hint is already honest, one-line comment fix"]
  },
  "summary": "All 6 Round-1 code remediations verified genuine live (import namespace True/True for require_subnet/require_sg, py_compile green, bash -n on both rendered shell blocks, ensure_system_user branch-tested fail-loud+idempotent under set -e). Both P1s closed; P2 console-tail tempering sound given IAM role policy grants no ssm:SendCommand and no SSM agent is installed; all P3s closed or pending-Main; 11 originally-clean findings hold with no regression; --open-http fully removed; no em dashes in AWS names/descriptions. NEW-5 remains a documented D1 blocker (VERIFY-BEFORE-D1). Doc items R1-6/R1-7/N1/N2 are Main's integration-time changes, not worktree defects. One new nit (stale inline comment at launch:1266). APPROVE."
}
```
