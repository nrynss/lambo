# T6 R1 — Adversarial re-review of the launcher fixes

**Task:** T6 (13 findings: 8 known + 5 new from T5).
**Reviewed (read-only):** `scripts/aws-infra/launch_exhibit_ec2.py` (1398 ln),
`scripts/aws-infra/provision_network.py` (450 ln), `scripts/aws-infra/_common.py` (506 ln),
`dev-diary/notes/remediation-tasks.md` §T6, README (`scripts/aws-infra/README.md`), `AGENTS.md`.
**Verdict:** `REQUEST_CHANGES` — two P1 defects, one a hard `NameError` that breaks every real launch.
**Disposition:** not shippable as-is; block D1 until P1-1 and P1-2 are fixed.

## Method

Static review of the full `git diff` and of both changed files in context; then live
verification that does not require AWS:

- `python3 -m py_compile` on both changed scripts → **pass** (syntax only).
- Rendered `USER_DATA` and `LLAMA_BLOCK` via the real renderers and ran `bash -n` on both
  embedded shell blocks → **pass** (quoting/expansion intact).
- Proved at runtime that `require_subnet`/`require_sg` are **not** in the module namespace
  (`False, False`), while `require_vpc` is (`True`).
- Exercised `arch_for_instance_type` over ~18 families incl. edge cases (`mac2-m2.metal`,
  `u-6tb1.metal`, `x2gd`, `im4gn`, `bogus`) → correct and fail-closed.
- Exercised the bash blocks' target functions (`llama_tarball`, `effective_bge_model_sha256`).
- Confirmed `libgomp1` is not in the base Ubuntu cloud image and not a dependency of the
  remaining `apt-get` list (web-verified against Ubuntu manifests/package metadata).
- Grepped for stale prose, `--open-http` remnants, README drift, and em dashes.

## Verified-clean (13 findings, adversarial pass)

| Ref | Verdict |
|---|---|
| T2-P2-1 port 80 | **Fixed.** `PUBLIC_INGRESS` now `(80,…),(443,…)`; `--open-http` removed cleanly from arg, plan, and note; `_check_port_80_open` (launch:944-963) warns (not fails) if the SG lacks tcp/80. Rationale (http→https redirect + ACME HTTP-01 fallback) is real and met. No repo script invoked `--open-http`; only README (nit). SG-PublicWeb is attached only to the EC2 exhibit (+ teardown), so opening 80 affects the web tier alone — no unintended surface. `bash -n` on the shell blocks passes. |
| T2-P2-2 sha coupling | **Fixed.** `--bge-model-sha256` default `None`; `main` refuses custom URL w/o custom hash (launch:1156-1165) before any AWS call; `effective_bge_model_sha256` (launch:272-279) resolves the right hash. No repo caller relied on the old default value. |
| T2-P2-3 IAM retry | **Fixed.** `_iam_propagation_error` (launch:833-849) retries only on `InvalidIamInstanceProfileArn.Malformed` or `InvalidParameterValue` whose lowercased message contains `instance profile`/`iam instance profile`/`iaminstanceprofile`. Real config errors raise immediately; no over-retry. Correct against AWS's actual message wording. |
| T2-P2-4 stale prose | **Fixed.** No `source build`, `Must be ARM64`, or `stays ARM for cost` remains; docstring, comments, help, warn text all rewritten. |
| T2-P3-1 IP race | **Fixed.** `poll` to `running` then `_ephemeral_ip` (launch:972-989, 1219-1234) bounds the wait (120 s) and hard-fails instead of printing "at None". EIP path unaffected. Also added a `stopping/stopped` re-adopt refusal (launch:1203-1207). |
| T2-P3-2 caddy restart | **Fixed.** `Restart=always` (launch:477), unified with the other units. |
| T2-P3-3 static UIDs | **Fixed & idempotent** (`getent group`/`id -u` guards, gids 901/902/903). Only edge: collision on a *non-fresh* image → P3-1. |
| T2-P3-4 health abort | **Fixed.** Wrapper aborts after 3 consecutive `systemctl is-active` failures (launch:395-417); `LAMBO_LLAMA_SERVICE` correctly threaded through `render_user_data` (launch:733) → unit env (launch:438), empty when no local llama. |
| NEW-1 ref-keyed tarball | **Fixed.** `LLAMA_TARBALLS` re-keyed ref→arch→(name,sha); `known_llama_cpp_ref` refuses unpinned refs at parse time (type=/`ArgumentTypeError`); `llama_tarball` keys both name and hash by ref; default b10453 renders. |
| NEW-3 layout verify | **Fixed.** `LLAMA_BLOCK` tests `-d $LLAMA_DIR`, `-x $LLAMA_DIR/llama-server`, and `ls libllama.so*` before `cp -a`/`ln`, failing closed (launch:544-552). The `-x /usr/local/bin/llama-server` guard correctly re-downloads after a prior dangling symlink. |
| NEW-4 families | **Fixed.** `x2g.` dot added; lists completed and **correct** (all verified real families, correct arch); unknown family fails closed via `arch_for_instance_type`. No false accept of x86↔ARM (edge-tested). |
| NEW-5 AMI honesty | **Handled honestly.** No fake AWS verification claimed; the VERIFY-BEFORE-D1 comment (launch:177-192) is precise, actionable, gives the exact `aws ssm get-parameter` commands, and the resolved AMI is deliberately logged (not hard-pinned) with the rotation tradeoff documented. This is a legitimate D1 blocker, not a code defect. `[UNVERIFIED]` resolved in `main` via `note(f"AMI …")` (launch:1210). |

## Findings

### P1

**T6-R1-1 (P1) — `require_subnet`/`require_sg` removed from the import block but still used → `NameError` on every real launch**
- File/line: `launch_exhibit_ec2.py:57-92` (import block) vs `:1179-1180` (`subnet = require_subnet(…); sg = require_sg(…)`).
- What: the `_common` import list dropped `require_sg` and `require_subnet` (the diff replaced them with `note`/`one_or_none`/`poll`), but `main` still calls both bare. Confirmed at runtime: `'require_subnet' in dir(launch_exhibit_ec2)` → `False`, `'require_sg'` → `False`, `'require_vpc'` → `True`. It is not `from _common import *`; no alias; `py_compile` cannot see it.
- Why it matters: any non-`--dry-run` run reaches `require_vpc` (imported, works) and then crashes with `NameError: name 'require_subnet' is not defined` — a raw traceback, exit 1, before any resource is touched. The script is entirely broken for its primary purpose.
- Fix: re-add `require_sg` and `require_subnet` to the `_common` import list (and remove the duplicate `note`/`one_or_none`/`project_filters` rows while there, N5).

**T6-R1-2 (P1) — `apt-get install -y libgomp1` deleted from `LLAMA_BLOCK` → llama-server cannot start on a fresh build**
- File/line: `launch_exhibit_ec2.py:520-529` (the block that replaced the install; dangling truncation at `:522`).
- What: the users refactor removed `apt-get install -y libgomp1 >/dev/null` (present pre-T6) when it inserted the `groupadd`/`useradd` block, and left the comment mid-sentence — `:520-522` now reads "…so it is explicit here or llama-server dies with `libgomp.so.1: cannot open shared" with the rest of the sentence and the install line gone.
- Why it matters: T5 verified `libggml-cpu-*.so` links `libgomp`, and `libgomp1` is **not** in the base Ubuntu cloud image nor pulled by the only remaining `apt-get` line (`tar gzip curl ca-certificates awscli`, launch:312). So on a fresh build the dynamic linker fails `llama-server` at exec with `libgomp.so.1: cannot open shared object file`; `systemctl enable --now llama-server.service` (launch:608) then fails under `set -e` and aborts the whole bootstrap (Caddy/`lambo-web` never install → NEW-2 fails the launch). This is exactly the silent/aborting boot-failure class T6 exists to remove, reintroduced by T6's own edit.
- Fix: restore `apt-get install -y libgomp1` in USER_DATA's base apt step (or the LLAMA block) and repair the comment. (If the team believes it is pulled transitively on 26.04, prove it and say so; the current state relies on an unverified, and per 24.04 evidence false, assumption.)

### P2

**T6-R1-3 (P2) — NEW-2 failure diagnostic claims the console tail is the bootstrap log; it is not**
- File/line: `wait_for_bootstrap` `:1036-1043` / `:1055-1060`, `_console_tail` `:1063-1073`, `USER_DATA:296` (`exec >>/var/log/lambo-bootstrap.log 2>&1`).
- What: the docstring says the console tail "is the bootstrap log, since user data redirects stdout/stderr to /var/log/lambo-bootstrap.log". But that redirect is precisely why the bootstrap body never reaches the serial console that `get_console_output` reads — the messages go to the file, not the console. On a failed/hung bootstrap the fetched tail is kernel/systemd/cloud-init meta (often near-empty on a hang), not the failing `curl`/`sha256sum`/`apt` step.
- Why: detection (status 2/2 **and** :443 probe) is sound and the non-zero exit works — the core NEW-2 requirement (never print green for a dead bootstrap) is met. But the operator is actively told they are being shown the failing command when they are not, costing real debugging time.
- Fix: fetch the actual boot log (SSM `AWS-RunShellScript` `tail -n 30 /var/log/lambo-bootstrap.log`) when an instance has the SSM agent, else temper the message to "console output (bootstrap script redirects to the boot log; it may not appear here)".

### P3

**T6-R1-4 (P3) — `_caddy_up` treats `AttributeError` as "server up"**
- File/line: `launch_exhibit_ec2.py:1027-1028`.
- What: `except (ssl.SSLError, AttributeError): return True`. The `ssl.SSLError ⇒ up` rationale (mid-ACME temp cert) is fine; `AttributeError` has no stated reason and would swallow a genuine code bug inside the probe as "healthy".
- Fix: drop `AttributeError` from the success tuple (or comment the exact case that needs it).

**T6-R1-5 (P3) — hardcoded system UIDs 901-903 assume a fresh image**
- File/line: `launch_exhibit_ec2.py:348-359` (lambo/caddy), `:523-528` (llama).
- What: `--uid 901/902/903` sit inside Debian's system range (SYS_UID_MAX 999). Idempotency is correct, and on a fresh cloud image 901-903 are free. On a reused/heterogeneous image a pre-existing unrelated account holding e.g. 902 makes `useradd` fail and `set -e` aborts the **whole** bootstrap with a bare adduser error.
- Fix: probe `getent passwd`/`group` for the chosen UIDs and emit a clear "UID 902 already taken" error, or document the reservation. (Keep static UIDs — that is the correct fix; just fail loudly.)

**T6-R1-6 (P3) — port 80 world-open deviates from plan §8 "443 only"; should be recorded as a deliberate decision**
- File/line: `provision_network.py:89-96` (`PUBLIC_INGRESS` now includes 80).
- What: deliberate and documented in code (80 for redirect + HTTP-01), and verified not to reach other stacks (SG-PublicWeb is the exhibit host + teardown only). Not a defect. But it is a posture change vs the plan; record it in the deployment doc/README so an auditor does not treat it as drift.
- Fix: doc change (fold into N1).

**T6-R1-7 (P3) — console tail on failure is runtime-only but may contain instance IPs/hostname**
- File/line: `wait_for_bootstrap:1055-1059`, `_console_tail:1063-1073`.
- What: on a failed launch the tail is printed to stderr at the operator's terminal. It can include the instance's private/public IPs and hostname (not the DSN — that lives only in the wrapper's env and is never echoed, so no credential leak). The repo's home-IP redaction rule is scoped to commits, so this is acceptable; just note the operator should not paste a failed-launch trace into a public issue.

**T6-R1-8 (P3) — duplicated imports in the `_common` import block**
- File/line: `launch_exhibit_ec2.py:75,77,80-81,83` — `note`, `one_or_none`, `project_filters` each appear twice.
- What/why: lint-level noise from the import rewrite.
- Fix: deduplicate (fold into T6-R1-1).

### Nits

**T6-R1-N1 — README is stale across the whole T6 change surface** (integration-time doc fix owned by Main):
`scripts/aws-infra/README.md:109,117,194` still reference `--open-http`; `:109` still says port 80 "only with `--open-http`"; `:155` says `t4g.medium` / "Amazon Linux 2023" / "24 GB"; `:209-210` says the default is `t4g.medium`; `:218-220` says llama.cpp is "built from source, because upstream publishes no linux-arm64 binary"; `:228` says `--instance-type` "must be Graviton". All contradict T6 (Ubuntu 26.04, t4g.large default, prebuilt, x86_64 supported).

**T6-R1-N2 — em dashes in user-facing/comment strings** (`launch:108,620,635`; `provision_network.py:20`).
AGENTS.md's rule ("no em dashes in **AWS resource names or descriptions**") is not violated — none of these are AWS names/descriptions. Note only: if the team applies the rule broadly to user-facing strings, these (introduced by earlier work, not T6) should be cleaned; the new SG descriptions introduced in T6 use hyphens, so the rule itself is respected.

## Summary

The T6 implementation closes **11 of 13** findings correctly and adversarially: T2-P2-1..T2-P2-4, T2-P3-1..T2-P3-4, NEW-1, NEW-3, NEW-4 are all properly fixed (each verified against the current code, with `bash -n`+`py_compile` green and the ARM/x86 family mapping edge-tested); NEW-5 is handled honestly as a documented, un-verifiable D1 blocker. But two P1 defects block shipping: **(T6-R1-1)** `require_subnet`/`require_sg` are no longer imported though still called, so every real launch dies with `NameError` (the script is unusable for its primary purpose), and **(T6-R1-2)** the deliberate `apt-get install -y libgomp1` was accidentally removed, so llama-server will not load on a fresh Ubuntu build (libgomp missing), aborting the bootstrap — the exact silent-failure class T6 exists to kill, reintroduced by its own edit, with a truncated comment left behind. There are also 2 solid P2-adjacent items (NEW-2's console-tail diagnostic overpromises; `_caddy_up` swallows `AttributeError`), several reasonable P3s, and clear README/nit cleanup for Main. Both P1s are small, mechanical fixes; once addressed the rest is straightforward.

```json
{
  "verdict": "REQUEST_CHANGES",
  "findings": {
    "P1": ["T6-R1-1 require_subnet/require_sg no longer imported but called (launch:57-92 vs 1179-1180) -> NameError on every real launch; re-add to import list", "T6-R1-2 apt-get install -y libgomp1 accidentally removed from LLAMA_BLOCK (launch:520-529) -> llama-server fails libgomp.so.1 on a fresh build and bootstrap aborts; restore install + repair truncated comment"],
    "P2": ["T6-R1-3 NEW-2 console-tail is claimed to be the bootstrap log but the USER_DATA redirect means it is not (launch:1036-1043/1055-1073; USER_DATA:296); pull /var/log/lambo-bootstrap.log via SSM or temper the message"],
    "P3": ["T6-R1-4 _caddy_up returns True on AttributeError (launch:1027-1028)", "T6-R1-5 static UIDs 901-903 can collide on a non-fresh image; fail loudly (launch:348-359,523-528)", "T6-R1-6 port 80 world-open versus plan section-8 443-only; record as deliberate (provision_network:89-96)", "T6-R1-7 failed-launch console tail prints instance IPs/hostname (runtime only, not commits; note not to paste publicly)", "T6-R1-8 duplicated note/one_or_none/project_filters imports (launch:75-83)"],
    "nits": ["T6-R1-N1 README stale: --open-http (109/117/194), Amazon Linux 2023/t4g.medium/24GB (155), default t4g.medium (209-210), built-from-source (218-220), must-be-Graviton (228) - integration doc fix by Main", "T6-R1-N2 em dashes in user-facing/comment strings (launch:108,620,635; provision:20); literal AGENTS.md names/descriptions rule not violated"]
  },
  "summary": "11/13 findings fixed correctly and verified; NEW-5 handled honestly as a documented un-verifiable D1 blocker. Shipped blocked only by two small P1s: a missing-import NameError that breaks every real launch (T6-R1-1) and an accidental removal of the verified-need libgomp1 install (T6-R1-2) - both mechanical fixes, after which REQUEST_CHANGES clears to approve-with-notes given the remaining P2/P3/nits."
}
```
