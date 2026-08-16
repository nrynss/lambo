# T5 — Adversarial re-review of the exhibit launcher

**Task:** T5 (re-review the launcher; blocks T6).
**Reviewed (read-only):** `scripts/aws-infra/launch_exhibit_ec2.py` (1090 ln),
`scripts/aws-infra/provision_network.py` (468 ln), `scripts/aws-infra/_common.py` (706 ln).
**Verdict:** `REVIEW-COMPLETE`

**Headline result, stated early because it contradicts the task brief:**
the six T6 bullets were *expected* to contain stale findings, because the file
"changed by ~194 lines". After mapping every T6 finding to the current code,
**all eight are still LIVE** — none is STALE. The Ubuntu switch, the prebuilt
llama tarball and the SHA-512/architecture-table changes were layered *on top
of* the original findings without closing them; in several cases the new code
**introduced the same defect class again** on a different argument. The ~194
rewritten lines contain real new defects (NEW-1 … NEW-5 below), including one
high-value override/unvalidated-hash footgun that is exactly the shape of the
known T2-P2-2 finding but for `--llama-cpp-ref`.

## Method & external verification

Static analysis of all three files plus live verification of every
external-protocol assumption that the rewritten lines depend on. Where a
claim's truth depended on a real external target I checked it directly (GitHub
release metadata, a downloaded artifact, and Ubuntu package metadata) rather
than assuming:

| Check | Result |
|---|---|
| llama.cpp prebuilt asset names in `LLAMA_TARBALLS` (`llama-b10453-bin-ubuntu-{x64,arm64}.tar.gz`) | **Present** in the real b10453 release, sizes match tarballs **verified** |
| Pinned arm64 SHA-256 (`b164e72d…09c109c`) vs the real downloaded artifact | **Exact match** — the pin is genuinely the artifact's hash, not a placeholder |
| Tarball extracts to `llama-b10453/` (code does `cp -a "llama-${REF}/." /opt/llama/`) | **Confirmed** for b10453 |
| `llama-server` present + `libllama.so`/`libllama-server-impl.so` present in archive | **Confirmed** |
| `libgomp1` really needed (code installs it explicitly) | **Confirmed** — `libggml-cpu-*.so` link `libgomp` |
| lambo release assets `lambo-0.1.0-linux-{arm64,x86_64}` (+`.sha256`) | **Both exist** (x86_64 is real, so the arch table's x86 path is live) |
| Caddy `caddy_2.10.0_linux_{amd64,arm64}.tar.gz` | **Both exist** |
| Caddy SHA-512 checksums row format vs `grep " ${TGZ}\$" \| sha512sum -c -` | **Parses correctly** (128-hex + 2-space + filename; `$`-anchor disambiguates `arm64`/`armv5`) |
| `apt-get install awscli` on Ubuntu 26.04 | **Works** — `awscli` is a real Ubuntu `resolute` (26.04) package |
| `apt-get install libgomp1` on Ubuntu | Present in the distro |
| `--threads $(nproc)` inside the llama-server systemd unit | **Not a defect** — the heredoc delimiter is unquoted (`<<UNIT`, line 469), so `$(nproc)` and `${LLAMA_PORT}` expand at bootstrap time to literals; the file on disk carries real numbers |
| Ubuntu 26.04 Canonical SSM parameter paths (`UBUNTU_SSM`, resolve_ami) | **Could not verify** — AWS account credentials were expired; the 24.04 convention strongly implies the paths exist, but this is flagged (`[UNVERIFIED]`, NEW-5) rather than asserted |

### Checked and cleared (adversarial diligence, not findings)
`$(nproc)` heredoc expansion; caddy `AmbientCapabilities` + `ProtectSystem=full`
compat; model `.part`→atomic-rename + re-check-on-boot fail-closed logic
(lines 455–467); `sha256sum -c` of the lambo asset from `/tmp` (file name inside
the checksum matches the asset name); IMDSv2-only metadata options; EIP
default-true ordering (which neutralizes the ephemeral-IP race for the default
run — see T2-P3-1).

---

## T6 finding disposition (all vs the *current* code)

| T6 ref | Original (sweep doc) | Current evidence | Disposition |
|---|---|---|---|
| **T2-P2-1** | Port 80 closed by default breaks http→https redirect + ACME HTTP-01 fallback | `provision_network.py:92` `PUBLIC_INGRESS=[(443,…)]`; `:279-289` port 80 only under `--open-http`; `:432-433` "port 80 is closed"; `launch:964` final note. Caddyfile (`launch:573-591`) relies on Caddy's auto http→https redirect, which port-80 closure defeats. | **LIVE** |
| **T2-P2-2** | `--bge-model-url` overridden without `--bge-model-sha256` → boot checksum mismatch / crash loop | `launch:1036-1049` two independent args, **no cross-validation**; `render_llama_block:550-558` substitutes both independently. Help text `:1044-1047` is advisory only. | **LIVE** |
| **T2-P2-3** | IAM retry catches generic `InvalidParameterValue`, masking real config errors behind 60 s of IAM-propagation hints | `launch:770-784` `if code not in ("InvalidParameterValue","InvalidIamInstanceProfileArn.Malformed")`, 12 × 5 s, misleading hint. A genuine config error (e.g. bad subnet/instance size) is retried 12× then reported as IAM propagation. | **LIVE** |
| **T2-P2-4** | Stale prose: docstring ARM-only; "instance stays ARM for cost"; "source build space" | All still present: `:4-11` "A `t4g.large` … t4g is Graviton, so the machine is ARM64"; `:110-111` "The instance stays ARM because that is cheaper…"; `:137-138` (TOO_SMALL comment) "leaves little room for the source build"; `:145-146` (TIGHT comment) "source build"; `:886-887` main() warn "llama.cpp's source build"; `:1069` `--volume-size` help "builds from source". | **LIVE** (partially addressed — arch table + Ubuntu comment are new and correct, but the specific stale sentences remain) |
| **T2-P3-1** | Ephemeral IP race on re-adopted pending instances | `_common.py:419-433` `find_instance` includes `pending/stopping/stopped`; `launch:939-946` the `--no-eip` path reads `PublicIpAddress` of whatever state → `None` for pending/stopped, printed as "at None". Default `eip=True` masks it; only `--no-eip` bites. | **LIVE** |
| **T2-P3-2** | `caddy.service` uses `Restart=on-failure`, others `Restart=always` | `launch:380` `Restart=on-failure`; `:344` (lambo-web) and `:485` (llama-server) `Restart=always`. | **LIVE** |
| **T2-P3-3** | System users created without static UIDs | `launch:275-278` (`lambo`, `caddy`) and `:429-430` (`llama`) `useradd --system` with no `--uid`. | **LIVE** |
| **T2-P3-4** | Health check polls 300 s even after `llama-server` dies | `launch:311-321` wrapper loops `i >= 60` × `sleep 5` = 300 s with no death detection; a genuinely broken llama-server (which itself has `Restart=always`, `:485`) yields infinite ~305 s lambo-web restarts with fixed `RestartSec=5` and no backoff, and llama-server's own failure is masked. | **LIVE** |

**None of the six T6 bullets is stale.** The T5 brief's premise ("several
findings are stale") is not supported by the current tree — every one survives
verbatim. The T6 fix list remains fully applicable.

---

## NEW findings (from the ~194 never-reviewed changed lines)

### NEW-1 — P2 — `--llama-cpp-ref` override silently invalidates the pinned SHA-256 and the extraction-dir assumption
- **File/line:** `launch_exhibit_ec2.py:1050-1054` (arg), `:553-556`
  (`render_llama_block`), `:435-449` (`LLAMA_BLOCK`).
- **What:** `--llama-cpp-ref` is user-settable. It permutes the tarball URL to
  `llama-{ref}-bin-ubuntu-*`, but the SHA-256 (`LLAMA_TARBALLS[arch][1]`) and the
  extraction directory (`cp -a "llama-${LLAMA_CPP_REF}/."`, `:445`) are pinned to
  the default `b10453`. Any non-default ref therefore produces a *different*
  tarball that fails `sha256sum -c -` at `:438` → `set -euo pipefail` aborts the
  bootstrap **after** the instance is already running; the launcher has by then
  reported success (see NEW-2).
- **Why it matters:** this is the exact defect class T6 is fixing for
  `--bge-model-url` (T2-P2-2), re-created on a different argument by the very
  change that was supposed to remove the source-build fragility. Help text
  (`:1053`) only *warns*; nothing enforces.
- **Fix:** make the hash and extraction-dir *depend on ref* (a `{ref: …hash…,
  dir…}` table) and **refuse** any `--llama-cpp-ref` that is not in the table at
  ARG-PARSE time (exactly as `known_instance_type` already refuses unknown
  families). Fail fast in Python, not silently in boot.

### NEW-2 — P2 — No post-boot success/failure detection: a failed bootstrap is reported as a successful launch
- **File/line:** `launch_exhibit_ec2.py:923-951`; specifically `:937`
  (`get_waiter("instance_running")`) and `:949-951` (`"exhibit launched"`).
- **What:** the script's only wait is the EC2 state machine reaching `running`;
  it never confirms user data completed. Every new download-and-verify step
  (llama tarball, BGE model, both checksum sets) can abort the bootstrap with
  `set -e`. `RunInstances` returns success and the script prints "exhibit
  launched" even when `/etc/lambo/lambo.toml` was never written and nothing
  serves. If `--key-name` is omitted (`:1071-1077`), the operator has no SSH path
  to read `/var/log/lambo-bootstrap.log`.
- **Why it matters:** all the NEW checksum/mismatch failure modes (NEW-1,
  T2-P2-2) land *exactly* here: a silent, unverifiable, green-printed outage. The
  demo is declared "launched" while the portal is down and no prompt hints how to
  find out.
- **Fix:** after `instance_running`, poll `describe_instance_status` for both
  status checks `2/2` passed **and** probe the Caddy/`lambo` health endpoint on
  the resulting public IP (or curl `http://<ip>:443`). On failure, print the
  `get_console_output` tail (bootstrap log) and return non-zero.

### NEW-3 — P3 — llama tarball extraction hardcodes `llama-${LLAMA_CPP_REF}/` and never verifies the installed tree
- **File/line:** `LLAMA_BLOCK:439,443-448`.
- **What:** `tar -xzf` then `cp -a "llama-${LLAMA_CPP_REF}/." /opt/llama/` assumes
  the archive's top-level dir is `llama-<ref>/` and that the executable/`libllama`
  sharing-object set is as expected. Verified correct for b10453, but it is an
  unverified assumption about upstream's packaging; the hash guarantees *bytes*,
  not *layout* or *loadability*.
- **Fix:** after extraction, `test` the expected dir and key files
  (`test -x "$DIR/llama-server"` and `ls "$DIR"/libllama.so*`) before
  `cp -a`/`ln -sf`, or derive the dir from `tar -tzf | head -1`; fail closed on a
  layout mismatch instead of producing a dangling `/usr/local/bin/llama-server`.

### NEW-4 — P3 — `ARM_FAMILIES` typo: `"x2g"` lacks the trailing dot; several common families omitted
- **File/line:** `launch_exhibit_ec2.py:170-177` (`ARM_FAMILIES`/`X86_FAMILIES`).
- **What:** every entry has a trailing dot except `"x2g"` (`:172`) — inconsistent,
  and a future/unknown `x2g*` family would be silently classed as ARM. Also
  omitted: `a1.*` (Graviton Arm, a valid free/low-cost family) and several common
  x86 families — `known_instance_type` rejects them with "add the family to
  ARM_FAMILIES or X86_FAMILIES". Fail-closed, so not P1, but wrong for real
  families.
- **Fix:** `"x2g."`; complete the family lists (or document the fail-closed
  boundary as intentional and add a `--force-arch` escape hatch).

### NEW-5 — P3 — `[UNVERIFIED]` Ubuntu 26.04 SSM parameter path and unrecorded rolling AMI
- **File/line:** `launch_exhibit_ec2.py:148-162` (`UBUNTU_SSM`), `:712-720`
  (`resolve_ami`).
- **What:** `resolve_ami` calls `ssm.get_parameter` on
  `/aws/service/canonical/ubuntu/server/26.04/stable/current/{arm64,amd64}/hvm/ebs-gp3/ami-id`.
  The 24.04 convention strongly implies these exist, but I could **not** verify
  (account creds expired at review time; `aws login` produces no session in this
  sandbox). If the 26.04 parameter path differs even slightly, `get_parameter`
  404s at runtime after all other setup. Separately, `stable/current` rotates;
  the specific AMI id used is logged to stdout but never pinned/recorded for
  audit/repro.
- **Fix:** verify both parameter paths in-region (us-east-1) before T6 ships, and
  either pin the resolved `ami_id` into a recorded artifact or document the
  rotation tradeoff explicitly.

### Folded into the known list (not separate NEW findings)
- Stale **"source build" prose** in the rewritten region is a *second* instance of
  T2-P2-4: main() warn `:886-887` and `--volume-size` help `:1069` still say
  "llama.cpp's source build"/"builds from source" with the build gone. Fix them in
  the same T2-P2-4 pass as `:4-11`, `:110-111`, `:137-138`, `:145-146`.
- The **port-80 orchestration gap** (launcher never verifies port 80's SG state or
  prompts to re-provision with `--open-http`; only prints a note at `:964`) is the
  actionable half of T2-P2-1 — close it in that fix.

---

## What T6 must now close (final list)

**Known-live (all 8, unchanged from the T6 brief):**
1. T2-P2-1 port 80 / http→https / HTTP-01 (`provision_network.py:92,279-289,432-433`; `launch:573-591,964`).
2. T2-P2-2 `--bge-model-url` ↔ `--bge-model-sha256` coupling (`launch:1036-1049`).
3. T2-P2-3 IAM retry over-broad `InvalidParameterValue` (`launch:770-784`).
4. T2-P2-4 stale prose (`launch:4-11,110-111,137-138,145-146,886-887,1069`).
5. T2-P3-1 ephemeral-IP race on `--no-eip` re-adoption (`_common.py:419-433`; `launch:939-946`).
6. T2-P3-2 caddy `Restart=on-failure` vs `always` (`launch:380,344,485`).
7. T2-P3-3 system users without static UIDs (`launch:275-278,429-430`).
8. T2-P3-4 300 s health poll with no death detection (`launch:311-321`).

**New (added by this re-review):**
- NEW-1 (P2): `--llama-cpp-ref` override breaks the pinned hash + extract-dir (`launch:1050-1054,553-556,438,445`).
- NEW-2 (P2): no boot-success detection; failed bootstrap reported as success (`launch:937,949-951`).
- NEW-3 (P3): unverified tarball layout assumption (`LLAMA_BLOCK:439,443-448`).
- NEW-4 (P3): `"x2g"` missing dot + incomplete family lists (`launch:170-177`).
- NEW-5 (P3): unverified 26.04 SSM path + unrecorded AMI (`launch:148-162,712-720`).

**Recommendation:** T6 should be scoped as "the 8 known + the 5 new" (13 items).
NEW-1 and NEW-2 are the highest-value additions because they share the exact
silent-failure shape T6 already exists to kill and both live in the never-reviewed
rewritten lines.
