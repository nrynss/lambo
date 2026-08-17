# Adversarial review — Round 2: remediated curl fix for the v0.2.0 Linux container build

**Reviewer:** CurlmReviewR2 · **Round:** 2 · **Date:** 2026-08-17
**Target:** `.github/workflows/release.yml` (uncommitted working-tree change, +16 lines, detached HEAD `1ae3532`)
**Prior review:** `dev-diary/adversarial-review/adve-review-remed-curlmround1.md` (REQUEST_CHANGES — P1: the container lacks a C compiler/linker)
**Scope:** READ-ONLY. Verify the P1 remediation (`build-essential` + comment + step rename), the local container smoke test (full ship build green, GLIBC_2.34) and whether it genuinely de-risks the container path, the digest-pin decision (P3-1), the glibc-gate no-op (N3), and re-audit the full diff. No source edits; the only deliverable is this review.

**Verdict: APPROVE** — the P1 gap is closed with first-hand empirical proof, the container path is de-risked for the v0.2.0 release, and the change is ready to re-tag v0.2.0. P3-1 (digest pin) is recommended but non-blocking; the two remaining P3s carry dispositions below.

---

## Grounding (every claim verified, not assumed)

### 1. Diff scope — exactly +16 lines, one new step

`git diff`: `1 file changed, 16 insertions(+)`; `git status` clean apart from the modified `release.yml` and the (untracked) round-1 review doc. The insertion is one step between `actions/checkout` and `Install Rust toolchain` (`release.yml:94-108`). Nothing else changed: container wiring (`:64-70`), glibc gate (`:172-192`), release job (`:201-252`), publish-crate (`:254-273`) all byte-identical to HEAD.

### 2. YAML parse + expression audit (python3)

- 3 jobs parse: `build` (11 steps), `release` (6), `publish-crate` (3).
- **17/17 `${{` / `}}` balanced**; zero `${{` or `}}` occurrences inside comments; no bare `{{`; the new lines 94-108 are brace-free (verified line-by-line). The new step's `if:` uses the implicit expression form (`if: runner.os == 'Linux'`, no `${{ }}`), the same pattern as the glibc gate at `:173` — the new step adds **zero** `${{` to the file, so the 17/17 count is unaffected. The repo's known `${{`-in-comment hazard (commit `1ae3532`) does not recur.

### 3. The install line and the comment (curlm-R1-P1 remediation) — verified verbatim

`release.yml:102-108`:
- Step name: **"Prepare container (install build toolchain)"** (R1-N1 done).
- `if: runner.os == 'Linux'` — `runner.os` reports the runner (host) OS, so both container rows run it; macOS/Windows rows skip it. Correct.
- Run block: `set -euo pipefail` / `apt-get update` / `apt-get install -y --no-install-recommends curl ca-certificates build-essential`. Root apt is fine (bookworm default user is uid=0, verified in R1).
- Comment (`:94-101`) explains each package, and every claim is accurate against R1's empirical results: curl → rustup bootstrap `curl https://sh.rustup.rs | sh`, "dies with exit 127" (matches run `31992650303` verbatim); ca-certificates → "curl exits 77 without it" (matches R1's empirical curl TLS failure); build-essential → "rustc's default linker driver is `cc`" + the ship profile's C sources ("sqlx bundled SQLite → libsqlite3-sys, rustls → aws-lc-sys + ring") — all three crates confirmed present in Cargo.lock at the same versions R1 audited (`libsqlite3-sys 0.30.1`, `aws-lc-sys 0.44.0`, `ring 0.17.14`).

### 4. build-essential sufficiency — re-verified empirically, first-hand, in the exact image

Ran the **exact workflow install line** in the **exact image digest** (`debian:bookworm@sha256:813017f3d62be4b5891a7acca6a01bdcd4b8513daa81b1ab99d3a50385b26931` — the local copy, same digest R1 verified; 18.9s wall):

```
ALL PRESENT: cc gcc ld make readelf curl
cc (Debian 12.2.0-14+deb12u1) 12.2.0
GNU ld (GNU Binutils for Debian) 2.40
COMPILE+LINK OK
```

- build-essential brings the complete C toolchain: `cc`/`gcc` (12.2.0), `ld` (binutils 2.40), `make`, and `readelf` (also closing R1-N3 — see §6).
- Nothing more is needed: R1's dependency audit stands unchanged (Cargo.lock is untouched by this diff) — aws-lc-sys 0.44.0's default builder is the `cc`-crate CcBuilder (cmake only for FIPS; no `cmake` binary needed), `bindgen` off by default (no `libclang`), bundled libsqlite3-sys (no `pkg-config`), ring 0.17.14 via `cc`. rustc's linux-gnu linker driver is `cc` — now present.

### 5. The recorded local container smoke test — genuine, and the artifact corroborates it

Main's scripts (`/tmp/container-smoke.sh` 09:37, `/tmp/ship-build.sh` 09:38, today) replicate the workflow logic exactly:

- Same install line (`curl ca-certificates build-essential`, `--no-install-recommends`).
- rustup bootstrapped through the **same mechanism as dtolnay/rust-toolchain**: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- --default-toolchain 1.97.1 -y` — so the curl+ca-certificates path the P1 fix exists for is the one actually exercised (R1 already proved exit 0 with both installed; the smoke script proves it end-to-end).
- `cargo build --release --features ship` — the full ship profile, exactly the workflow's build step (host default triple `x86_64-unknown-linux-gnu` = the `linux-x86_64` row's target, so the `--target` flag omission is immaterial).
- glibc floor measured with the **exact gate pipeline**: `readelf -V … | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1`.

The recorded claim "ship build green, GLIBC_2.34" is corroborated by a surviving artifact: the worktree's `target/release/lambo` — 23,501,448 bytes, mtime **2026-08-17 09:40:37** (inside the smoke window), max required glibc **GLIBC_2.34** via `readelf -V`. A host-built release binary on this CachyOS box (glibc 2.39+) would require a far newer floor; the 2.34 floor proves the artifact is bookworm-linked. (The "1m52s" timing is Main's record — plausible with a warm cargo cache — but the artifact, not the clock, is the evidence.)

**Does this de-risk the container path? Yes, for the purpose of this fix.** The P1 failure class — "container lacks the C toolchain" — is closed: exact image, exact install line, exact profile, exact gate pipeline, green result, GLIBC_2.34 artifact. What the local test does *not* cover (the same residuals the approved T12 baseline carries): (a) the parity step (`cargo test --release --features ship --test binary_parity`) — same compile surface, same `cc` linker, historically green; per R1 its only blocker was the toolchain gap, now closed; (b) the `linux-arm64` row — never built end-to-end in the container anywhere; the mechanism is structurally symmetric (Debian arm64 is a first-class arch; rustup/cc/ld identical), so this is a residual, not a defect; (c) the full GH workflow run still only happens on a tag push (`release.yml` triggers on `v*` tags only). See P3-2 for disposition.

### 6. glibc gate binutils fallback — confirmed harmless (R1-N3)

`readelf` is now present via build-essential (verified in §4), so the gate's `if ! command -v readelf; then apt-get … binutils; fi` branch (`:179-181`) is a dead no-op path — harmless, and exactly what R1 predicted. The two apt-get invocations are separate sequential steps in the same container; no conflict.

### 7. Digest-pin decision (curlm-R1-P3-1) — new verification this round

I verified the pin is **multi-arch-safe**, which R1 had not settled:

- `docker manifest inspect debian:bookworm@sha256:813017f3d62be4b5891a7acca6a01bdcd4b8513daa81b1ab99d3a50385b26931` → `mediaType: application/vnd.oci.image.index.v1+json` with platforms **amd64, arm64/v8, arm/v7, 386, ppc64le** — i.e. `813017f3…` is the image **index**, so both `linux-x86_64` (amd64) and `linux-arm64` (arm64) runners resolve their correct platform variant from the same pinned reference. A platform-specific digest would have broken arm64; this one does not.
- Additionally, the floating tag **today** still resolves to that same index (current-tag amd64 manifest digest `88a7d30d49e1…` == the pinned index's amd64 entry), so an unpinned run at re-tag time uses the exact smoke-tested image either way.

**Decision: PIN — recommended, non-blocking.** Change `:65` and `:70` to `container: debian:bookworm@sha256:813017f3d62be4b5891a7acca6a01bdcd4b8513daa81b1ab99d3a50385b26931`. Rationale: (1) the workflow header states the repo convention — every third-party action pinned to a full SHA — and the container tag is the one floating supply-chain input in the whole release path; (2) the pinned index is exactly the image the smoke test proved green, making the imminent release deterministic; (3) multi-arch verified. Documented caveat (see nit curlm-R2-N2): the pin freezes the **base image** only — `apt-get update` still pulls current bookworm packages, so apt-level versions float either way; the readelf gate remains the true structural enforcement of the glibc floor. Leaving it floating is also acceptable today (tag still resolves to the tested index) and does not invalidate the release; refresh the pin deliberately on bookworm upgrades, same cadence as action-SHA bumps.

---

## Findings

### P1 — none. curlm-R1-P1 is resolved and verified.

The install line now carries `build-essential` (`:108`), the comment explains why each of the three packages is present, and the toolchain gap is proven closed first-hand in the exact image (§4), with the full ship build green and GLIBC_2.34 (§5). No new P1.

### P2 — none.

### P3

- **curlm-R2-P3-1 (disposition of curlm-R1-P3-1, floating tag): DECISION — PIN, non-blocking.** Pin both Linux rows to `debian:bookworm@sha256:813017f3d62be4b5891a7acca6a01bdcd4b8513daa81b1ab99d3a50385b26931` (verified multi-arch index digest = the smoke-tested image; consistent with the repo's pin-everything convention; makes the T12 floor reproducible). May land with the re-tag (2-line change, zero risk — it equals today's tag resolution) or at the next workflow touch. If left floating, add a one-line note that bookworm point-releases re-point the tag and the readelf gate is the enforcement.
- **curlm-R2-P3-2 (disposition of curlm-R1-P3-2, container path exercised only on tag pushes): SATISFIED for the P1 purpose; GH-run residual remains.** The local container smoke test is the accepted de-risk for this fix (exact image, exact install line, exact profile, exact gate pipeline, green + GLIBC_2.34 artifact — §5). Residuals it does not cover: the parity step and the linux-arm64 row (structurally symmetric, same linker/compiler; parity's only prior blocker was the toolchain gap). `release.yml` still triggers only on tags, so the full multi-row GH run happens at the tag push; `remediation-tasks.md:968`'s "draft/PR release build on the new bookworm runner" item remains the standing process debt. Optional, non-blocking: add `workflow_dispatch` to `release.yml` and/or install actionlint so future container regressions are caught pre-tag.

### Nits

- **curlm-R2-N1 (carried from R1-N2, informational, accepted as-is):** the container has no `git`; checkout works only via actions/checkout's REST-API archive fallback. No current step needs a `.git` dir, so this stays accepted; one comment line near the container wiring (or adding `git` to the install line) would future-proof at the next touch.
- **curlm-R2-N2:** if the P3-1 pin lands, add one comment line stating the pin covers the base image only — apt package versions still float with bookworm updates (`apt-get update`). Prevents future readers from assuming the pin freezes the whole toolchain; the readelf gate is the enforcement.
- **curlm-R2-N3:** the smoke-test evidence lives in `/tmp` and the gitignored `target/`; the repo diary has no record of the curlm remediation (remediation-tasks.md has no curlm/build-essential entry; `:968` only lists the still-outstanding GH-run item). One line in remediation-tasks.md ("curlm-R1-P1 closed: build-essential + local container ship build green, GLIBC_2.34") keeps the diary accurate for the next maintainer.

---

## Review-focus checklist

1. **Install line + comment?** ✓ `build-essential` present (`:108`); comment (`:94-101`) explains curl (rustup bootstrap, exit 127), ca-certificates (TLS, exit 77), build-essential (`cc` linker driver + ship's C sources) — every clause matches R1's empirical/run-log facts verbatim.
2. **build-essential sufficiency?** ✓ No `cmake`/`pkg-config`/`libclang` needed (aws-lc-sys default CcBuilder, bindgen off, bundled sqlite; Cargo.lock unchanged). Empirically re-verified: cc/gcc/ld/make/readelf/curl all present, C compile+link OK, in the exact image.
3. **Placement/gating/expressions?** ✓ After checkout, before the toolchain step; `runner.os == 'Linux'` targets exactly the two container rows (macOS/Windows skipped); 17/17 `${{`/`}}` balanced, none in comments, no bare `{{`, new lines brace-free, new step adds zero expressions.
4. **Smoke test de-risks the container path?** ✓ Yes — artifact + scripts corroborate Main's record: exact image, exact install line, same rustup curl bootstrap, full ship profile, exact readelf gate pipeline, GLIBC_2.34. Residuals (parity step, arm64 row, GH-run-only trigger) are the approved T12 baseline, not this fix's risk.
5. **glibc-gate binutils fallback?** ✓ Harmless no-op (readelf present via build-essential); sequential steps, no conflict.
6. **Digest-pin decision?** ✓ PIN (recommended, non-blocking); multi-arch index digest verified this round; tag currently still resolves to the tested image.

---

## Disposition

**APPROVE.** All round-1 findings are resolved or dispositioned: curlm-R1-P1 is closed with first-hand empirical proof (exact install line in the exact image yields the full C toolchain; the recorded ship build is green at GLIBC_2.34 with a surviving container-linked artifact); N1 (step rename) and N3 (binutils no-op) confirmed; P3-2 is satisfied for the P1 purpose with the GH-run residual documented; P3-1 is decided (PIN, non-blocking, multi-arch verified). The diff is a clean +16 on release.yml only, no expression hazard, no regression to macOS/Windows/gate/publish jobs. **Ready to re-tag v0.2.0** — apply the P3-1 pin if desired (2 lines, zero risk, equals today's tag resolution), otherwise ship as-is; either way the tag-push workflow run is the final proving ground and both Linux rows now have everything the build needs.

```json
{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [
      "curlm-R2-P3-1 (disposition of R1-P3-1): DECISION = PIN, non-blocking. Pin both Linux rows to debian:bookworm@sha256:813017f3d62be4b5891a7acca6a01bdcd4b8513daa81b1ab99d3a50385b26931 (release.yml:65,70). Verified this round that 813017f3... is the OCI image INDEX (mediaType application/vnd.oci.image.index.v1+json; platforms amd64, arm64/v8, arm/v7, 386, ppc64le), so the pin is multi-arch-safe for both Linux rows, and the pinned image is exactly the smoke-tested one (current floating tag still resolves to the same index). Consistent with the repo's pin-everything convention and makes the T12 glibc floor reproducible. Caveat to document: the pin freezes the base image only; apt packages still float with bookworm updates (readelf gate remains the enforcement). Non-blocking: leaving floating today does not invalidate the release.",
      "curlm-R2-P3-2 (disposition of R1-P3-2): the local container smoke test (ship build green, GLIBC_2.34, artifact target/release/lambo mtime 09:40:37, readelf floor 2.34) satisfies the P1 de-risk: exact image, exact install line, same rustup curl bootstrap, exact readelf gate pipeline. Residuals not covered: the parity step (same compile surface/linker, historically green, prior blocker was the toolchain gap) and the linux-arm64 row (never container-built end-to-end; structurally symmetric). release.yml still triggers only on tag pushes; remediation-tasks.md:968 'draft/PR release build' remains outstanding. Optional non-blocking: add workflow_dispatch and/or actionlint."
    ],
    "nits": [
      "curlm-R2-N1 (carried R1-N2, informational, accepted): no git in the container; checkout relies on actions/checkout's REST-archive fallback. No current step needs .git; add a comment or git to the install line at the next touch.",
      "curlm-R2-N2: if the P3-1 pin lands, add a comment line noting the pin covers the base image only (apt packages still float; readelf gate is the enforcement).",
      "curlm-R2-N3: the smoke-test evidence lives in /tmp and gitignored target/; remediation-tasks.md has no curlm entry. One line recording 'curlm-R1-P1 closed: build-essential + local container ship build green, GLIBC_2.34' would keep the diary accurate."
    ]
  },
  "summary": "APPROVE - ready to re-tag v0.2.0. The P1 gap (no C compiler/linker in debian:bookworm) is genuinely closed: the exact workflow install line (curl ca-certificates build-essential) run in the exact image digest yields cc/gcc/ld/make/readelf all present (gcc 12.2.0, binutils 2.40) with a passing C compile+link (first-hand, 18.9s), and the recorded local container smoke test is corroborated by a surviving container-linked artifact (target/release/lambo, 23.5MB, max GLIBC_2.34 via the exact readelf gate pipeline). The comment explains each package accurately (curl/exit-127 bootstrap, ca-certificates/exit-77 TLS, build-essential cc-linker + libsqlite3-sys/aws-lc-sys/ring C sources); no cmake/pkg-config/libclang needed (Cargo.lock unchanged from the R1 audit). Step renamed (R1-N1), placed and gated correctly (runner.os == 'Linux'), 17/17 expressions balanced with none in comments and no bare {{, new lines brace-free; macOS/Windows rows, glibc gate, release and publish-crate jobs untouched (diff is release.yml +16 only). glibc gate's binutils fallback is a confirmed harmless no-op. P3-1 decision: PIN (non-blocking; digest verified multi-arch this round). P3-2: de-risked for the P1 purpose; parity step and arm64 row remain GH-run residuals as per the approved T12 baseline."
}
