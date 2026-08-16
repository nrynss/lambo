# Adversarial review — T12 round 2: release workflow glibc (Debian bookworm container)

**Reviewer:** T12ReviewR2 · **Round:** 2 · **Date:** 2026-08-17
**Target:** `.github/workflows/release.yml` (uncommitted working-tree change, remediation of T12-R1-P1/P2)
**Prior review:** `adve-review-remed-T12round1.md` (requested changes; reference its `## T12` / findings, below)
**Scope:** READ-ONLY. Verify the two Linux targets actually build inside a `debian:bookworm`
container (P1) and a glibc-version gate makes the AL2023 ("max GLIBC ≤ 2.34") guarantee structural
(P2). No release triggered; verification = reading the workflow + GH Actions semantics + machine
YAML parse + empirical execution of the gate pipeline against real binaries. `actionlint`/`yamllint`
still **not installed** — see "Tool notes".

---

## What the remediation changed (diff vs HEAD)

1. Added job-level `container: ${{ matrix.container }}` after `runs-on` (`:40`).
2. Linux matrix rows now carry `container: debian:bookworm` (`:64`, `:69`); macOS/Windows rows
   deliberately omit it (`:73`, `:86`).
3. x64 row host `ubuntu-latest` → `ubuntu-24.04` (`:63`).
4. New `Assert max required GLIBC <= 2.34 (Linux only)` step after `Stage and checksum` (`:155-175`).
5. Comments relocated/reworded (the in-container/blob above `strategy:`, the host-native rationale
   above `container:`).

Machine parse (`python3 yaml`): `build` job keys `['name','runs-on','container','defaults','strategy','steps']`
— job-level `container` present. Rows: `linux-x86_64` → `container=debian:bookworm`,
`linux-arm64` → `container=debian:bookworm`, `macos-arm64`/`windows-x86_64` → `container` **absent**.
`release` job: `runs-on: ubuntu-latest`, **no** `container` key.

---

## Findings

### T12-R2-P1 — P1 remediated correctly: job-level container wired, Linux steps in-container, non-Linux host-native, release containerless — VERIFIED
**File:** `.github/workflows/release.yml:35-40`

- **Wired.** `container: ${{ matrix.container }}` is now a real job-level keyword (not parked in a
  matrix row). Every `run:` step and every `uses:` action of `build` — checkout, rust-toolchain,
  Add target, rust-cache, Derive version, cargo build, parity test, Stage-and-checksum, the glibc
  gate, upload — executes **inside the job's container** for the two Linux rows. `dtolnay/rust-toolchain`
  installs rustup into the container; `cargo build` links against bookworm's glibc, not the host's
  2.39. This is exactly the mechanism round 1 found missing.
- **Non-Linux rows stay host-native — expression is valid.** The macOS/Windows matrix entries
  **omit** the `container` property (not an explicit `''`), so `${{ matrix.container }}` evaluates to
  GitHub's `NULL` for an absent matrix key and the `container:` value is effectively unset → the job
  runs directly on the `runs-on` host. This "omit the property, let the job-level expression fall to
  NULL" pattern is the one GitHub treats as containerless, and it is deployed in production
  (D. Eddelbuettel, *Thinking inside the box*, blog #054, 2025-10-27, used by rcpparmadillo /
  rcppmlpack-examples). Crucially the remediation **correctly avoids** `container: ''` in the matrix
  row (an explicit empty *string* is the documented-tricky case); here the key is absent, which is the
  safe form. No regression to macOS/Windows.
- **Release job containerless.** `release` has no `container` key, `runs-on: ubuntu-latest`; it only
  downloads artifacts and publishes (links nothing). Correct per round-1 P3 reasoning.
- **Multi-arch.** `ubuntu-24.04` (amd64) + multi-arch `debian:bookworm` amd64 variant; `ubuntu-24.04-arm`
  (arm64) + bookworm arm64 variant. Host supplies only Docker; no host rust leaks. Correct.

### T12-R2-P2 — P2 remediated correctly: glibc gate makes the ≤ 2.34 claim structural — VERIFIED
**File:** `.github/workflows/release.yml:155-175`

- **Runs ON the staged binary, both Linux targets.** Step is after `Stage and checksum` (index 8 > 7)
  and before `Upload binaries` (8 < 9). `BIN="dist/lambo-${VERSION}-${{ matrix.name }}"` matches the
  staged filename `dist/lambo-${VERSION}-${{ matrix.name }}${{ matrix.ext }}` with `ext=""` on both
  Linux rows (verified: `linux-x86_64`, `linux-arm64` match exactly). `if: runner.os == 'Linux'` is
  true for both Linux rows, so the gate covers **both** arch targets via `matrix.name`.
- **`set -euo pipefail`** is the first line; a gate failure (`exit 1`) fails the build job, which the
  `release` job `needs:`, so it also blocks publication.
- **Comparison logic correct (empirically executed).** I ran the exact pipeline on a real
  high-glibc binary:
  - `/usr/bin/ls` (needs GLIBC_2.38): `max_glibc=GLIBC_2.38` → `req=2.38` → **gate fails** (correct).
  - Simulated `max=GLIBC_2.34` → `req=2.34` → **pass**.
  - Boundary `GLIBC_2.35` (a future 2.35+ symbol) → `req=2.35` → **rejected**. A future dependency /
    rustc bump that pulls a `GLIBC_2.35/2.36` symbol now fails CI instead of silently re-breaking
    AL2023. This closes round-1 P2.
- **Robust to readelf output artifacts.** `readelf -V` emits a bare `GLIBC_` token (from
  `GLIBC_PRIVATE`, matching `GLIBC_[0-9.]*` with zero digits). I confirmed `sort -V` orders `GLIBC_`
  **below** every numbered `GLIBC_<ver>`, so `tail -1` still selects the true numeric max
  (`printf 'GLIBC_\nGLIBC_2.34\n' | sort -V | tail -1` → `GLIBC_2.34`). `*${max#GLIBC_}` strip works
  for the max token.
- **binutils install correct.** bookworm base omits `readelf`, so `if ! command -v readelf; then
  apt-get update && apt-get install -y --no-install-recommends binutils; fi` installs it. Steps run
  as root inside the job container (default, no `USER`). Under `set -e` the then-body failure exits
  the job (correct: don't proceed without readelf). arm64 repo resolves identically.

### T12-R2-P3
- **T12-R2-P3-1 (informational) — `container: ${{ matrix.container }}` empty→host behavior is
  real-world-confirmed but not machine-verifiable here.** Correctness of the non-Linux rows rests on
  GitHub evaluating an *absent* matrix property to `NULL` (not an empty string). Confirmed via the
  deployed eddelbuettel pattern and, importantly, the remediation uses the **omit** form (safe) not
  `container: ''` (the documented-tricky form). `actionlint` is still not installed, and even if it
  were, it would not resolve this semantic (matrix context). A future editor must keep `container`
  absent (not `''`) on the host rows — the comment at `:36-39` already says this. Not a defect; noted
  so the assumption is explicit.
- **T12-R2-P3-2 (informational) — fully-static-binary edge of the gate.** If a future build produced
  an ELF with **no** `GLIBC_*` tokens, `max_glibc` would be empty → `req=2.34` → gate passes. That is
  correct *for that case* (a statically-linked binary needs no glibc floor and does run on AL2023),
  and unreachable for the current dynamically-linked Rust artifact. Note only.
- **T12-R2-P3-3 (informational) — `ubuntu-latest`→`ubuntu-24.04` pin.** Part of the original T12
  change (not this remediation); both rows now pin explicit Ubuntu 24.04 (host glibc 2.39), matching
  the existing `ubuntu-24.04-arm` row. Benign, more deterministic; no regression.

### Nits
- **T12-R2-N1** — None substantive. All round-1 nits are resolved:
  - N1 (comment inside `matrix.include`) → moved above `strategy:`/`matrix:` (`:48-58`).
  - N2 (loose "produces a 2.34 binary … everywhere") → reworded to "the 2.34 floor is an empirical
    property of the current source — the 'Assert max required GLIBC <= 2.34' gate below makes it
    structural" (`:53-56`). Accurate.
  - N4 (note macOS/Windows omit container intentionally) → added at `:36-39` ("a future editor must
    NOT 'fill it in'").
  - N3 (em dashes) → N/A, no AWS names.

---

## Regression / unchanged-flow checks

- **macOS (`macos-14`, arm64) and Windows (`windows-latest`, msvc) rows**: byte-for-byte unchanged;
  no `container` added; stay host-native. ✅
- **Checksum/artifact flow**: `Stage and checksum`, `Upload binaries`, `Download all binaries`
  (`release`), `set -euo pipefail`, `sha256sum`/`shasum` branch, `test -s *.sha256` — all unchanged. ✅
- **`release` job** containerless, `needs: build`, tag-version assert unchanged. ✅
- **In-container toolchain resolution** for Linux: checkout, rust-toolchain (rustup into container),
  rust-cache, cargo build/parity all execute within `debian:bookworm`; the `<target>/release/lambo`
  consumed by Stage is the bookworm-linked artifact the gate then checks. Coherent end-to-end. ✅

---

## Tool notes

- `actionlint` — **not installed** (`command -v actionlint` → empty). `yamllint` — **not installed**.
  Semantic verification of the container wiring (empty matrix.container → host) and the gate logic was
  done manually + empirically as described; today's INSTALL-STATE note for the project stands: install
  `actionlint` to catch future workflow-schema drift (it would not have caught round-1's inert P1,
  which was a semantic issue).
- Machine verification performed: `python3` PyYAML 6.0.3 parse (structure/keys above); gate pipeline
  executed verbatim against `/usr/bin/ls` (needs GLIBC_2.38 → gate fails) and simulated
  `GLIBC_2.34`/`GLIBC_2.35`/bare-token cases.
- The glibc-version numbers are from public knowledge of the Debian/AL2023 tables (bookworm 2.36,
  Ubuntu 24.04 2.39, AL2023 2.34); no container build was run here (workflow is CI-only).

---

## Summary

Both round-1 P1/P2 findings are genuinely remediated and correct. The job-level
`container: ${{ matrix.container }}` is wired so both Linux rows build entirely inside
`debian:bookworm` (toolchain, cargo, parity, staging, gate, upload), the macOS/Windows rows stay
host-native via the safe omit-the-property NULL form (real-world-confirmed, no regression), and the
`release` job is containerless. The new glibc gate runs after staging on the exact shipped
`dist/lambo-*-{linux-x86_64,linux-arm64}` binary, uses `set -euo pipefail`, installs binutils
correctly, and — verified by running the exact pipeline — rejects any max symbol above 2.34
(2.35/2.36 fail, 2.34 passes, bare `GLIBC_` token sorts below and is harmless). It closes the round-1
"empirical, unenforced" gap: a future 2.35+ symbol now fails CI instead of silently re-breaking
AL2023. All round-1 nits (N1/N2/N4) are resolved; multi-arch + checksum/artifact + macOS/Windows
flows are unchanged. No actionable P3/nit remains. Genuinely clean.

**Verdict: APPROVE**

{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [
      "T12-R2-P3-1 — `container: ${{ matrix.container }}` host-row correctness relies on GitHub evaluating an ABSENT matrix property to NULL (not ''); real-world-confirmed (eddelbuettel production pattern) and the remediation correctly uses the omit form rather than `container: ''`. Comment at :36-39 already guards it. Not materially verifiable here (actionlint absent); noted explicitly.",
      "T12-R2-P3-2 — Fully-static-binary edge: no GLIBC_* tokens -> gate passes (correct: static needs no glibc floor). Unreachable for the current dynamic Rust artifact. Informational.",
      "T12-R2-P3-3 — `ubuntu-latest`->`ubuntu-24.04` pin on x64 row (from original T12, not remediation): benign, deterministic, matches ubuntu-24.04-arm. No regression."
    ],
    "nits": [
      "T12-R2-N1 — No substantive nits. Round-1 N1/N2/N4 all resolved (comment moved above strategy/matrix; wording now empirical+gate-accurate; host-native omit rationale documented). N3 N/A."
    ]
  },
  "summary": "Both round-1 P1/P2 are genuinely fixed and verified. Job-level `container: ${{ matrix.container }}` works: Linux rows build fully inside debian:bookworm (all steps/actions in-container, toolchain into container), macOS/Windows rows stay host-native via the safe omit-property NULL form (regression-free, real-world-confirmed), release job containerless, multi-arch correct. The new glibc gate runs after staging on the exact per-arch shipped binary, `set -euo pipefail`, installs binutils, and the comparison logic was run empirically: 2.34 passes, 2.35/2.36 fail, bare GLIBC_ token sorts below and is harmless — a future 2.35+ symbol now fails CI instead of silently re-breaking AL2023. Checksum/artifact + macOS/Windows flows unchanged; all round-1 nits addressed. actionlint/yamllint still not installed (noted). APPROVE."
}
