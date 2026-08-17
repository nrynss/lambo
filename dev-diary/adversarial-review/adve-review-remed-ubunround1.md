# Adversarial review — Round 1: drop the container, build Linux natively on Ubuntu 24.04

**Reviewer:** UbunReviewR1 · **Round:** 1 · **Date:** 2026-08-17
**Target:** `.github/workflows/release.yml` (uncommitted working-tree change; detached HEAD `35ea9af`)
**Scope:** READ-ONLY. Verify (1) the container is fully removed from the build job and the Linux matrix rows, macOS/Windows untouched; (2) the "Prepare container" step is gone and the ubuntu-24.04 / ubuntu-24.04-arm host runners genuinely need nothing extra; (3) the glibc gate relaxed to ≤ 2.39 — logic correct, comment accurate; (4) the rationale comment honestly records the T12 history and the deliberate AL2023 drop; (5) the parity tests now run on the host (container flakiness gone by construction); (6) build step order, `release`, and `publish-crate` intact; (7) expressions balanced, none in comments, no bare `{{`; (8) doc drift (bookworm/container/AL2023/2.34 as current) anywhere else in the repo.
**Verification:** full `git diff`; full workflow read; `python3` PyYAML parse; expression audit; repo-wide greps for `bookworm`, `container`, `glibc`/`GLIBC`/`AL2023`/`Amazon Linux`/`2.34`. No full test suite run (workflow-only change; constraint). No source edits — this review is the only deliverable.

**Verdict: APPROVE** — the change is correct and complete; no P1/P2. Three P3 doc-drift findings (deployment procedure, remediation diary, `launch_exhibit_ec2.py` comment) and two nits. The workflow is ready to re-tag v0.2.0; the parity tests will run on the host, avoiding the container flakiness by construction. P3 fixes recommended to land with the re-tag or at the next workflow/doc touch — they do not block the workflow itself.

---

## Diff summary

`git status` → exactly one modified file. `git diff` → `.github/workflows/release.yml`, **17 insertions / 51 deletions (net −34)**, matching the stated scope.

Removed:
- Job-level `container: ${{ matrix.container }}` + its four-line "future editor must NOT fill it in" comment (`:37-40` old).
- The eleven-line "Why the Linux builds run in a container" rationale block above `strategy:` (`:49-59` old).
- Both Linux matrix rows' digest-pinned `container: debian:bookworm@sha256:813017f3…` entries (x86_64 and arm64) + the pin comment (`:60-65`, `:68-71` old).
- The "Prepare container (install build toolchain)" step + its 13-line package rationale comment (curl/ca-certificates/build-essential; `:97-118` old).
- The glibc gate's `readelf`-missing → `apt-get install binutils` fallback + its comment (`:178-181` old).

Added:
- The new native-build rationale comment above `strategy:` (`:45-51`).
- Gate renamed to "Assert max required GLIBC <= 2.39 (Linux only)" with rewritten logic comment and error message (`:148-163`).

macOS/Windows matrix rows, `release`, and `publish-crate`: byte-identical (diff touches no other hunks).

## Verification (against the review focus)

### 1. Container fully removed — VERIFIED
`python3` PyYAML parse of the working tree: `build` job keys `['name','runs-on','defaults','strategy','steps']` — **no `container` key** at job level. Matrix rows: `ubuntu-24.04`/`linux-x86_64`, `ubuntu-24.04-arm`/`linux-arm64`, `macos-14`/`macos-arm64`, `windows-latest`/`windows-x86_64` — **none carry a `container` property**, and no row references `matrix.container` anywhere (grep for `matrix.container` in release.yml: no matches). The old digest `sha256:813017f3…` appears nowhere in the working tree outside historical review docs. macOS/Windows rows never had a container and still don't — untouched.

### 2. Prepare step gone; host runners need nothing extra — VERIFIED (reasoning)
Step sequence now: `actions/checkout` → **"Install Rust toolchain"** directly (no prepare step; nothing between them but two blank lines). The old prepare step existed only because `debian:bookworm`'s minimal image lacks curl (rustup bootstrap `curl https://sh.rustup.rs | sh` — exit 127), ca-certificates (TLS to sh.rustup.rs — exit 77), and build-essential (`cc` linker driver + C sources in the ship profile: libsqlite3-sys bundled sqlite, aws-lc-sys, ring). The GitHub-hosted `ubuntu-24.04` / `ubuntu-24.04-arm` runner images (actions/runner-images) preinstall the full build toolchain — curl, git, ca-certificates, gcc/`cc`, make, and binutils (`readelf`) are all part of the base image; both arches use the same image definition. Every `run:` step's host-needs check: checkout (git present), toolchain (curl + ca-certificates), add target (rustup), cache (JS action), derive version (sed/head — coreutils), build (cc + C compiler), parity (same), stage/checksum (mkdir/cp/sha256sum — coreutils), glibc gate (readelf — binutils, now unconditional, no fallback needed), upload (action). **Nothing extra is required.** [INFERENCE: runner-image package contents are from the documented actions/runner-images ubuntu-24.04 image definition — well-established; no runner was provisioned in this review.]

### 3. glibc gate ≤ 2.39 — logic VERIFIED, comment accurate
Gate (`:148-163`): `max_glibc=$(readelf -V "$BIN" | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)`; `req=$(printf '2.39\n%s' "${max_glibc#GLIBC_}" | sort -V | tail -1)`; fail iff `req != "2.39"`.
- `max == 2.39` → `sort -V` tail is `2.39` → **passes** ✓
- `max > 2.39` (e.g. 2.40) → tail is `2.40` → fails ✓
- `max < 2.39` (e.g. 2.34) → tail is `2.39` → passes (lower floor is fine) ✓
- No `GLIBC_*` symbols (static-ish binary) → `max_glibc` empty → tail is `2.39` → passes (no glibc requirement) ✓
- The pattern matches only glibc versioned symbols: `GLIBC_PRIVATE` and `GLIBCXX_*` both fail the `GLIBC_[0-9.]*` grammar, so libstdc++ can't corrupt the measurement. `sort -Vu` dedupes. Sound, and identical in shape to the ≤ 2.34 gate it replaces.
- The comment ("2.39 is the Ubuntu 24.04 runner's glibc — the floor the build guarantees… the shipped floor cannot silently creep above the build host") is accurate: a native Ubuntu 24.04 build links against glibc 2.39 and can never require a symbol newer than the glibc it linked against, so max ≤ 2.39 passes by construction — that is exactly the intended contract now. Error message ("must not exceed the Ubuntu 24.04 build host") matches.

### 4. Rationale comment honest about T12 + AL2023 — VERIFIED
`release.yml:45-51` records: (a) Linux builds natively on Ubuntu 24.04 (glibc 2.39) ✓; (b) the earlier bookworm container (T12) existed only to hold the shipped floor at 2.34 for Amazon Linux 2023 ✓ (matches the T12 review trail: container introduced for the AL2023 floor, gate added to make it structural); (c) AL2023 is deliberately dropped ✓; (d) the container's in-container process-spawning parity tests were flaky ✓ (consistent with the T12/curlm trail: the parity step was the never-fully-exercised residual, and the operator records >60 s in-container stalls vs 0.97 s host baseline); (e) the gate's remaining job is preventing silent floor creep above the build host — "the part of T12 worth keeping" ✓. No nostalgia, no invented guarantees; the comment's one soft spot is the word "floor" doing double duty (see nit N2).

### 5. Parity tests host-run — flakiness gone by construction — VERIFIED
The parity step (`:105-128`) and every other step run on the host now: no `container:` key means GitHub Actions runs `run:`/`uses:` steps directly on the runner. The parity test spawns `lambo` subprocesses over stdio via `env!("CARGO_BIN_EXE_lambo")` under the same `CARGO_TARGET_DIR`/`--target` as the build, relinking the exact staged artifact with the full ship profile — all host-side. The only in-container dependencies the container path ever had were the prepare step (removed, §2) and the gate's binutils fallback (removed, §3); nothing else referenced the container (grep). No in-container dependency remains, so the >60 s stdio-stall class is eliminated structurally.

### 6. Step order + release + publish-crate intact — VERIFIED
Build job order: checkout (`:82`) → Install Rust toolchain (`:86`) → Add target (`:91`) → Cache cargo (`:94`) → Derive version (`:97`) → Build release binary (`:102`) → parity test (`:105`) → Stage and checksum (`:130`) → glibc gate (`:148`) → Upload binaries (`:165`) — matches the stated order exactly. The gate still runs after staging (it consumes `dist/lambo-${VERSION}-${{ matrix.name }}`) and before upload. `release` job (`:172-223`) and `publish-crate` (`:225-244`): zero diff hunks; PyYAML confirms all three jobs present. The parity Windows skip (`:111`) and the sha256sum/shasum branch are untouched.

### 7. Expressions — VERIFIED 16/16
`${{` occurs exactly 16 times (`:35, :36, :92, :103, :111, :128, :134×4, :136×2, :152, :168, :211, :243`); `}}` occurs exactly 16 times; balanced. No comment line contains `${{` or `}}` (including the "expression markers" note in the Render-release-notes comment, which is prose only). No bare `{{` (every opener is `${{`). No `matrix.container` reference remains. The two `run:` blocks the diff touched (parity skip is untouched; gate body) contain no `${{` — the gate's shell text uses `$(…)`/`${max_glibc#GLIBC_}`/`${VERSION}` only, which GitHub does not scan.

### 8. Doc drift — three P3s, see below. Historical review docs (T12round1-3, curlmround1-2, E2Eround1-2) describe the bookworm state **as of their dates** and are accurate history — not drift.

---

## Findings

### P1 — none

### P2 — none

### P3

- **ubun-R1-P3-1 — `scripts/aws-infra/launch_exhibit_ec2.py:169-173` comment asserts a build reality that no longer exists and a compatibility claim that is now false.**
  *What:* The comment says "Release builds now run inside a `debian:bookworm` container (T12), whose older toolchain keeps the shipped binary below the AL2023 glibc floor; a repo-side 'Assert max required GLIBC <= 2.34' CI gate makes that structural … **So the binary runs on AL2023 too**". After this change: builds run natively on Ubuntu 24.04, the gate is ≤ 2.39, and the shipped Linux binaries require **GLIBC 2.39 — they do NOT run on AL2023 (2.34)**.
  *Why P3:* comment-only; no behavior change to the script. But it sits directly above the `UBUNTU_SSM` AMI selection that drives the exhibit deployment, and a future operator reading "the binary runs on AL2023 too" could pick an AL2023-based instance and get a hard `GLIBC_2.39 not found` at service start — the exact failure mode the T12 work existed to prevent. This is the most load-bearing of the drift sites.
  *Fix:* rewrite the comment: release Linux builds now run natively on Ubuntu 24.04 (glibc 2.39); the AL2023 floor was deliberately dropped, so the shipped binary requires GLIBC ≤ 2.39 (gate-enforced); the Ubuntu 26.04 instance's glibc (2.41) exceeds the build environment, so the checksum-verification rationale in the final sentence stays valid unchanged. Keep the NEW-5 discharge note.

- **ubun-R1-P3-2 — `dev-diary/notes/deployment-and-submission.md:58-60` (D1 procedure) instructs building the instance binary "in a Debian bookworm container" as the current method.**
  *What:* "The binary the instance runs is built in a Debian bookworm container, because a locally built one needs a newer glibc than the instance has." The repo no longer builds in bookworm (release path is native Ubuntu 24.04), and the stated reason is stale on both ends — the instance is Ubuntu 26.04 (glibc 2.41), newer than both the local build env (≥ 2.39) and the release build env (2.39), so a host-built binary runs on it either way.
  *Why P3:* dev-diary procedure doc, not shipped code; the described manual build still yields a runnable binary, so nothing breaks — but the procedure is now disconnected from the repo's actual build story and misleads the next D1 operator.
  *Fix:* update the bullet to "the instance binary is the release artifact (built natively on Ubuntu 24.04, GLIBC 2.39) or a local `cargo build --release --features ship`; the Ubuntu 26.04 instance's glibc (2.41) is newer than the build environment, so either runs. Build, `scp`, restart: about four minutes."

- **ubun-R1-P3-3 — `dev-diary/notes/remediation-tasks.md:919-927` describes release.yml in the present tense as the bookworm build; `:967-969`'s outstanding item is framed on "the new bookworm runner".**
  *What:* The T12 diary entry opens ".github/workflows/release.yml **now builds both Linux targets inside a Debian bookworm container** (job-level `container: ${{ matrix.container }}`) … Assert max required GLIBC <= 2.34" — present-tense claims that are now false. The outstanding item at `:968` — "a draft/PR release build on the **new bookworm runner** (T12)" — needs re-framing: its underlying need (one real CI exercise of the release workflow before re-tagging) is still valid and arguably more important now that the build path changed again, but the "bookworm runner" framing is obsolete.
  *Why P3:* diary/notes doc; the history portion stays accurate, the present-tense portion drifts.
  *Fix:* append a dated note that the container was subsequently dropped (native Ubuntu 24.04 builds, ≤ 2.39 gate, AL2023 floor dropped) pointing at this review; re-word the `:968` item to "draft/PR release build of the current (native-host) release workflow before re-pushing v0.2.0".

### Nits

- **ubun-R1-N1 — `release.yml:83-84`: two consecutive blank lines left where the "Prepare container" step was removed.** Between `actions/checkout` and "Install Rust toolchain". Harmless; one blank line is the house style elsewhere. *Fix:* delete one blank line (and, while there, the now-redundant lone blank at `:37` is fine to keep as the comment separator — leave it).
- **ubun-R1-N2 — `release.yml:155-156`: the word "floor" carries two meanings in one sentence.** "2.39 is the Ubuntu 24.04 runner's glibc — the floor the build guarantees." The binary's *required* glibc is bounded below by nothing and above by 2.39 (the build host's glibc is a ceiling, not a floor); "floor" here means "minimum guaranteed compatibility surface". The following sentence ("cannot silently creep above the build host") disambiguates, but the first sentence reads ambiguously. *Fix (optional):* "2.39 is the Ubuntu 24.04 runner's glibc — the compatibility ceiling the build guarantees and the gate enforces."
- **ubun-R1-N3 (informational, no action):** the historical reviews (`adve-review-remed-T12round{1..3}.md`, `adve-review-remed-curlmround{1,2}.md`, `adve-review-remed-E2Eround{1,2}.md`) describe the bookworm build as of their dates and remain accurate history; they are the provenance for the P3-3 diary note. No edits needed.

---

## Summary

The change is exactly what it claims: container removed from the build job and both Linux matrix rows (digest pin gone with it), macOS/Windows untouched, prepare step deleted with nothing extra needed on the ubuntu-24.04/ubuntu-24.04-arm hosts (curl, ca-certificates, build-essential/gcc, git, binutils/readelf are all base-image components), glibc gate relaxed to ≤ 2.39 with verified-correct `sort -V` logic (max 2.39 passes, > 2.39 fails, lower passes) and an honest comment, a rationale that truthfully records the T12-for-AL2023 history and the deliberate AL2023 drop, host-run parity tests (flakiness eliminated by construction — no in-container dependency remains), intact step order with the gate still after staging, untouched `release`/`publish-crate` jobs, and a clean 16/16 balanced expression audit with no comment-embedded or bare braces. No P1/P2. The three P3s are doc drift (launcher comment with a now-false "runs on AL2023" claim, the D1 deploy procedure, and the remediation diary's present-tense T12 description) — all fixable without touching the workflow; none blocks the release. **APPROVE: ready to re-tag v0.2.0.** At re-tag, the Linux binaries will be built natively on Ubuntu 24.04 (require GLIBC ≤ 2.39, verified by the gate) and the parity tests will run on the host at ~1 s instead of stalling in the container. Land the P3 fixes with the re-tag or at the next touch; recommend one real (draft/PR) run of the release workflow before re-pushing the tag, as the changed build path has not been exercised end-to-end in CI since the container removal.

```json
{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [
      "ubun-R1-P3-1: scripts/aws-infra/launch_exhibit_ec2.py:169-173 - comment claims release builds run in a debian:bookworm container with a <= 2.34 gate and that 'the binary runs on AL2023 too'; after this change builds are native Ubuntu 24.04 (glibc 2.39), the gate is <= 2.39, and the shipped binary does NOT run on AL2023 (GLIBC_2.39 not found). Comment-only, but sits above the UBUNTU_SSM instance choice, so an operator could pick an AL2023 instance and hit the exact failure T12 existed to prevent. Fix: rewrite to native Ubuntu 24.04 build, AL2023 deliberately dropped, <= 2.39 gate; final 'newer than the build environment' sentence stays valid (instance glibc 2.41).",
      "ubun-R1-P3-2: dev-diary/notes/deployment-and-submission.md:58-60 - D1 procedure says 'The binary the instance runs is built in a Debian bookworm container, because a locally built one needs a newer glibc than the instance has'; no longer true (native Ubuntu 24.04 release builds; instance Ubuntu 26.04 glibc 2.41 is newer than build env 2.39, so host-built binaries run either way). Fix: point the procedure at the release artifact / local ship build and drop the bookworm rationale.",
      "ubun-R1-P3-3: dev-diary/notes/remediation-tasks.md:919-927,967-969 - present-tense T12 entry ('release.yml now builds both Linux targets inside a Debian bookworm container … Assert max required GLIBC <= 2.34') is now false; outstanding item ':968' is framed on 'the new bookworm runner'. Fix: dated note that the container was dropped (native builds, <= 2.39 gate) pointing at this review; re-frame ':968' as a draft/PR run of the current native-host release workflow before re-tagging v0.2.0."
    ],
    "nits": [
      "ubun-R1-N1: release.yml:83-84 - two consecutive blank lines left where the 'Prepare container' step was removed; delete one.",
      "ubun-R1-N2: release.yml:155-156 - 'floor' used for the build-host glibc ceiling; suggest 'compatibility ceiling the build guarantees and the gate enforces'.",
      "ubun-R1-N3 (informational): historical reviews (T12round1-3, curlmround1-2, E2Eround1-2) describe the bookworm state as-of-their-date and are accurate history; no action."
    ]
  },
  "summary": "APPROVE - ready to re-tag v0.2.0. Verified adversarially: container fully removed (no job-level key, no matrix-row container, no digest pin, macOS/Windows untouched); prepare step gone and ubuntu-24.04/arm hosts need nothing extra (curl, ca-certificates, build-essential/cc, git, binutils/readelf are base-image); glibc gate <= 2.39 logic correct (max 2.39 passes, >2.39 fails, lower passes, no-symbol passes) with an accurate comment; rationale honestly records T12-for-AL2023 and the deliberate AL2023 drop; parity tests host-run with no remaining in-container dependency, so the >60s stdio-stall flakiness is gone by construction; step order intact with the gate still post-stage; release and publish-crate jobs untouched; expressions 16/16 balanced, none in comments, no bare {{. Three P3 doc-drift sites (launch_exhibit_ec2.py comment now falsely claims 'runs on AL2023', deployment-and-submission.md D1 procedure, remediation-tasks.md present-tense T12 entry + bookworm-runner outstanding item) - non-blocking, recommended to land with the re-tag; recommend one draft/PR run of the native-host release workflow before re-pushing the tag."
}
```
