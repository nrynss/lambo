# Adversarial review — T12: release workflow glibc (Debian bookworm container)

**Reviewer:** T12ReviewR1 · **Round:** 1 · **Date:** 2026-08-17
**Target:** `.github/workflows/release.yml` (uncommitted working-tree change)
**Scope:** READ-ONLY. Verify the two Linux targets actually build inside a `debian:bookworm`
container so the shipped binaries require GLIBC ≤ 2.34 and run on Amazon Linux 2023 (GLIBC 2.34).
No tests run (workflow-only change); verification = reading the workflow + GH Actions semantics,
with a machine YAML parse. `actionlint`/`yamllint` are **not installed** — see "Tool notes".

---

## Intent of the change

Both Linux matrix rows (`linux-x86_64`, `linux-arm64`) previously built on the Ubuntu 24.04
runner's native glibc (2.39), producing binaries that died on Amazon Linux 2023 (`version
GLIBC_2.39 not found`). The change adds `container: debian:bookworm` to those two matrix rows so
the build links against an older glibc. macOS/Windows rows and the `release` job are untouched.

---

## Findings

### T12-R1-P1 — The `container:` never reaches the job: the fix is inert, all steps still run on the host
**File:** `.github/workflows/release.yml:57` and `:62` (matrix rows); job at `:34-46`

**What:** `container` is a **job-level** GitHub Actions keyword. Setting `container: debian:bookworm`
inside a `matrix.include` row only defines a `matrix.container` value; GitHub Actions does **not**
implicitly bind matrix fields to job keywords. The only keyword consumed here is `runs-on:
${{ matrix.os }}` (`:35`). There is **no** job-level `container:` key in the `build` job at all — a
machine parse of the job shows keys `['defaults','name','runs-on','steps','strategy']` and
`'container' in job == False`. The correct wiring for a per-row container is to add
`container: ${{ matrix.container }}` at job level (with macOS/Windows rows leaving it empty).

**Why it matters:** Because no container is attached, every step of the `build` job — `checkout`,
`dtolnay/rust-toolchain`, `cargo build --release`, the parity test, and staging — runs **directly on
the runner host** (`ubuntu-24.04` / `ubuntu-24.04-arm`, glibc 2.39). The Rust toolchain is installed
into the host, and `cargo build` links against the host glibc. The published Linux binaries still
require GLIBC 2.39 and still die on Amazon Linux 2023. The change has **zero effect** on the shipped
binary. This is exactly the "step runs on the host and links against 2.39" failure the review was
asked to hunt for — and it is the entire implementation.

**Fix:** add a job-level
```yaml
container: ${{ matrix.container }}
```
under `build:` (after `runs-on`), leaving the two Linux rows with `container: debian:bookworm` and
the macOS/Windows rows without it (they then run containerless, as today). Optionally confirm with
`actionlint` (once installed) that the job schema accepts the expression.

---

### T12-R1-P2 — Container bounds glibc at 2.36, not 2.34; the "2.34 binary" claim is empirical and unenforced
**File:** `.github/workflows/release.yml:47-55` (comment); also `dev-diary/notes/remediation-tasks.md:908-911`

**What:** `debian:bookworm` ships **glibc 2.36**, not 2.34. A build inside the container can require
up to `GLIBC_2.36`, never 2.34 structurally. The stated claim — "Debian bookworm's toolchain produces
a 2.34 binary" / "requires only 2.34" — is an *empirical current-state* property of the source: today
`lambo` (pure Rust; no `.cargo/config`, no `build.rs`, rustls-based TLS, no vendored sysroot) uses no
glibc symbol newer than 2.34, so the floor lands at 2.34. Nothing in CI asserts it.

**Why it matters:** This is the highest-risk item and the entire purpose of T12. If a future
dependency (or a rustc bump) references a symbol introduced at 2.35/2.36, the workflow would
"successfully" rebuild inside bookworm and republish a binary that requires GLIBC 2.35+ — silently
re-breaking AL2023 — with **no CI failure** to catch it. The margin between the 2.36 container ceiling
and the 2.34 AL2023 target is thin and unprotected.

**Fix (cheap, makes the guarantee structural):** after `Stage and checksum`, add a glibc-version gate,
e.g.
```bash
max=$(readelf -V "dist/lambo-...-${matrix.name}" | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)
# assert max_version <= 2.34
```
and/or use a lower-glibc container (`debian:bullseye`, glibc 2.31) for real margin. At minimum,
correct the comment/task wording to "bookworm caps the binary's required glibc at 2.36; the current
build's max symbol is 2.34 (verified this session); the CI gate keeps it there."

---

### T12-R1-P3 — Build isolation, host/container split, and multi-arch reasoning (would-be corrects once P1 is fixed)
**File:** `.github/workflows/release.yml:34-65`

**What/Why:** Recorded so the fix lands correctly rather than being re-brainstormed:
- `ubuntu-24.04` (x64) + `debian:bookworm` amd64 variant, and `ubuntu-24.04-arm` (arm64) +
  `debian:bookworm` arm64 variant, are both correct choices — the host only supplies Docker; the
  multi-arch image resolves to the runner's arch. No cross-compile step invokes host rust that could
  leak host glibc. Once `container: ${{ matrix.container }}` is wired (P1), all `run:` steps and all
  `uses:` actions (checkout, rust-toolchain, rust-cache, upload-artifact) execute inside the
  container, and `dtolnay/rust-toolchain` installs rustup into the container (not the host). Good.
- The `release` job (`:159`) correctly stays `runs-on: ubuntu-latest` with **no** container — it only
  downloads artifacts and publishes; it links nothing. Correct as-is.
- macOS (`macos-14`, arm64) and Windows (`windows-latest`, msvc) rows are byte-for-byte unchanged —
  no container added (correct, they must stay host-natives). Checksum/artifact flow
  (`Stage and checksum`, `Upload binaries`, `Download all binaries`, `set -euo pipefail`,
  `test -s *.sha256`) unchanged. `fail-fast: false`, `defaults.run.shell: bash`, and the version/
  parity steps are untouched. Nothing non-Linux regressed.

---

### Nits

- **T12-R1-N1** — The 8-line explanatory comment is indented *inside* the `matrix.include` list
  (between `include:` at `:46` and the first Linux row at `:56`). It is YAML-legal (comments are
  not nodes) but sits at an odd nesting; it would read better above `strategy:`/`matrix:`. Low value.
- **T12-R1-N2** — Comment wording "produces a 2.34 binary … runs everywhere" is doubly loose:
  the 2.34 outcome is empirical (P2), and "everywhere" is overbroad (MSVC/Apple binaries are
  platform-tied anyway). Re-word when fixing P2.
- **T12-R1-N3** — No em dashes in AWS/GitHub product names anywhere; N/A here — no AWS names present.
- **T12-R1-N4** — Once the job-level `container:` expression is added, the macOS/Windows rows should
  deliberately omit `container` (empty) so they stay host-native; state that in the comment so a
  future editor doesn't "fill it in".

---

## Tool notes

- `actionlint` — **not installed** (`command -v actionlint` → empty). Cannot machine-validate the
  workflow schema. Note: even with actionlint, catching P1 requires semantic reasoning — a matrix row
  carrying an unused `container:` field is legal YAML/JSON-schema, so actionlint may not flag the
  missing job-level `container: ${{ matrix.container }}`. Manual review was required.
- `yamllint` — **not installed**. Fallback: `python3 -c "import yaml"` (6.0.3) parses the file as
  well-formed YAML and confirms the job keys (above). Syntax is valid; the defect is semantic
  (unwired container), not syntactic.
- No build could be run here (workflow is CI-only). All glibc claims are reasoning over
  GH Actions semantics + the container glibc table (bookworm 2.36, bullseye 2.31, Ubuntu 24.04 2.39,
  AL2023 2.34).

---

## Summary

The change's *direction* is right and its surrounding reasoning (host-vs-container, multi-arch
resolution, keeping the release job and non-Linux rows host-native) is sound — but **the mechanism
is never engaged**: `container: debian:bookworm` is parked inside the matrix rows and never wired to
the job-level `container:` keyword, so all build steps still run on the Ubuntu host and the binaries
still require GLIBC 2.39. T12 provides no fix as written (P1). Secondary, a `debian:bookworm`
container caps required glibc at 2.36, not 2.34, so even once wired the "runs on AL2023" guarantee is
empirical and unenforced — add a glibc-version gate (or tighten to bullseye) to make it structural
(P2). macOS/Windows and the release/checksum flow are genuinely unchanged.

**Verdict: REQUEST_CHANGES**

{
  "verdict": "REQUEST_CHANGES",
  "findings": {
    "P1": [
      "T12-R1-P1 — `container: debian:bookworm` is set only inside matrix.include rows and is never wired to the job: the `build` job has no job-level `container:` key, so GitHub Actions ignores it and every step (toolchain install, cargo build, parity, stage) runs on the Ubuntu host, linking host glibc 2.39. The change has no effect; binaries still require GLIBC 2.39 and fail on AL2023. Fix: add `container: ${{ matrix.container }}` at job level."
    ],
    "P2": [
      "T12-R1-P2 — A `debian:bookworm` (glibc 2.36) container bounds required glibc at 2.36, not 2.34; the current '2.34 binary' is an empirical property of the source with no CI enforcement. A future dependency/rustc bump could silently republish a 2.35+/2.36-requiring binary and re-break AL2023 with no workflow failure. Fix: add a `readelf -V` glibc-version gate (assert max ≤ 2.34) and/or pin a lower-glibc container (debian:bullseye 2.31); correct comment wording."
    ],
    "P3": [
      "T12-R1-P3 — Once P1 is fixed, the isolation reasoning is correct: ubuntu-24.04 (amd64) and ubuntu-24.04-arm (arm64) hosts + multi-arch debian:bookworm resolve per-arch; all steps/actions run in-container; release job correctly stays containerless on ubuntu-latest; macOS/Windows and checksum/artifact flow genuinely unchanged."
    ],
    "nits": [
      "T12-R1-N1 — Long comment indented inside the matrix.include list; move above strategy/matrix.",
      "T12-R1-N2 — Comment 'produces a 2.34 binary … runs everywhere' is empirically loose/overbroad; re-word with P2 fix.",
      "T12-R1-N3 — No AWS names present; no em-dash concern.",
      "T12-R1-N4 — When adding job-level container expression, note macOS/Windows rows intentionally omit container and stay host-native."
    ]
  },
  "summary": "Right intention, inert mechanism. `container: debian:bookworm` lives only in matrix rows and is never attached to the `build` job (no job-level `container:` key), so all steps still run on the Ubuntu host and the Linux binaries still link glibc 2.39 — T12 does nothing as written (P1). Even once wired, bookworm caps glibc at 2.36, not 2.34, so the AL2023 guarantee is empirical and unprotected; add a glibc-version gate or tighten to bullseye (P2). macOS/Windows, the release job, and the checksum/artifact flow are unchanged. actionlint/yamllint not installed (noted); YAML parse is valid, defect is semantic. REQUEST_CHANGES."
}
