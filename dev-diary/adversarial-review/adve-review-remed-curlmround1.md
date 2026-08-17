# Adversarial review — Round 1: curl fix for the v0.2.0 Linux container build

**Reviewer:** CurlmReviewR1 · **Round:** 1 · **Date:** 2026-08-17
**Target:** `.github/workflows/release.yml` (uncommitted working-tree change, +12 lines)
**Scope:** READ-ONLY. Verify (a) the root cause — `debian:bookworm` lacks `curl`, and dtolnay/rust-toolchain bootstraps rustup with `curl https://sh.rustup.rs | sh`; (b) the fix — a Linux-gated step installing `curl ca-certificates` before the toolchain step; (c) the highest-risk follow-on — whether the Linux container build can actually complete (C compiler / linker), and whether anything else the container path assumes is missing. No source edits. Verification = GitHub run-log ground truth + empirical docker runs against the exact image + Cargo.lock / registry-source audit + YAML parse.

**Verdict: REQUEST_CHANGES** — the root cause is correct and the fix is necessary and correctly shaped, but it is **not sufficient**: after this fix the Linux rows fail deterministically at `Build release binary` because `debian:bookworm` still has no C compiler (`cc`/`gcc`) and no linker (`ld`/`make`). The release workflow stays red; v0.2.0 cannot be released with this change as written.

---

## Grounding (every claim below was verified, not assumed)

### 1. Run-log ground truth — run `31992650303`, tag v0.2.0 @ `1ae3532` (the exact failure this fix targets)

Downloaded the full run logs (gh, authed as repo owner). Verbatim failure of `build-linux-x86_64` step "Install Rust toolchain" (`4_Install Rust toolchain.txt`):

```
##[group]Run if ! command -v rustup &>/dev/null; then
  curl --proto '=https' --tlsv1.2 --retry 10 --retry-connrefused --location --silent --show-error --fail https://sh.rustup.rs | sh -s -- --default-toolchain none -y
##[endgroup]
/__w/_temp/f075719e-7f85-4552-89fe-d7b1a313239b.sh: line 2: curl: command not found
##[error]Process completed with exit code 127.
```

- Step sequence (linux-x86_64 and linux-arm64 identical): Initialize containers ✓ → checkout ✓ → **Install Rust toolchain ✗ (69 ms)** → Add target / Build release binary / parity / Stage / glibc gate all **skipped**. Job conclusion `failure`; the run was cancelled by the user afterwards.
- `exit 127` mechanics: the runner executes composite-action run steps under `bash -e -o pipefail`; `curl` fails to exec in the `curl … | sh` pipeline, so the pipeline exits 127 and the step fails. The prompt's claim ("exit 127 = command not found, already confirmed from the run log") is **verbatim correct**.
- Note for context: checkout **succeeded** despite no `git` in the container — actions/checkout v4.4.0 logged "The repository will be downloaded using the GitHub REST API / To create a local Git repository instead, add Git 2.18 or higher to the PATH" and fetched a tarball. Nothing in the current steps needs a `.git` dir (see nit N2).

### 2. dtolnay/rust-toolchain@`032958a` (pinned SHA) — bootstrap mechanism confirmed from action source

`action.yml` at the pinned SHA contains exactly the step quoted above ("Install rustup if needed": `curl --proto '=https' --tlsv1.2 --retry 10 --retry-connrefused --location --silent --show-error --fail https://sh.rustup.rs | sh -s -- --default-toolchain none -y`). The fix's comment ("bootstraps rustup with `curl https://sh.rustup.rs | sh`") is accurate.

### 3. `debian:bookworm` — empirical (docker, fresh pull, digest `sha256:813017f3d62be4b5891a7acca6a01bdcd4b8513daa81b1ab99d3a50385b26931` — the same tag the runner pulls)

| check | result |
|---|---|
| `id` | `uid=0(root) gid=0(root)` — **root by default**; GH `container:` jobs run as the image's default user, and this image's default is root. `apt-get` needs no `sudo`. Fix's assumption ✓ |
| `curl`, `git`, `gcc`, `cc`, `make`, `ld`, `ld.gold`, `pkg-config`, `readelf` | **all MISSING** |
| `sha256sum`, `sed`, `head`, `tar`, `gzip` | present (coreutils) — Stage/checksum + Derive-version steps OK |
| `curl` (installed) **without** `ca-certificates` | `curl: (77) error setting certificate file: /etc/ssl/certs/ca-certificates.crt` → **ca-certificates is REQUIRED** for TLS to sh.rustup.rs (curl's Recommends is suppressed by `--no-install-recommends`; the explicit install is correct) |
| `curl` **with** `ca-certificates` | exit 0 — bootstrap download works |
| **exact fix command** (`apt-get install -y --no-install-recommends curl ca-certificates`) | `cc`/`gcc`/`ld`/`make`/`readelf` **STILL MISSING** → the next failure after this fix is deterministic |
| fix + `build-essential` | `cc`,`gcc`,`ld`,`make`,`readelf` all present; `cc` compile+link smoke **OK** — the completed fix is sufficient |

### 4. Dependency audit — the ship build compiles C, and rustc needs a linker

- `FEATURES: ship` (release.yml:31); `ship` = store-memory, store-cockroach, **store-sqlite**, embed-bge, embed-fixture (Cargo.toml:95).
- `store-sqlite` → `sqlx 0.8.6` feature `sqlite = ["_sqlite", "sqlx-sqlite/bundled", …]` (sqlx-0.8.6/Cargo.toml in the local registry) → **`libsqlite3-sys 0.30.1` bundled** (Cargo.lock:2326-2334; build-deps `cc`, `pkg-config`, `vcpkg`) → `build.rs` compiles sqlite3.c with the `cc` crate → needs a C compiler.
- rustls 0.23.43 (reqwest `rustls-tls` + sqlx `tls-rustls` + aws-sdk) → `aws-lc-rs 1.18.0` → `aws-lc-sys 0.44.0` (Cargo.lock:375-378) and `ring 0.17.14` (Cargo.lock:2975-2978) — C + assembler sources via the `cc` crate. I read `aws-lc-sys-0.44.0/builder/main.rs` from the local registry: the **default** build is the `CcBuilder` (cmake builder only for FIPS; `bindgen` off by default) — so a C compiler + binutils suffice; **no `cmake` binary, no `pkg-config`, no `libclang` needed**.
- rustc's default linker driver on both `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` is **`cc`** — none present → `error: linker 'cc' not found`.
- aws-config / aws-sdk-bedrockruntime / rmcp / axum / tokio / parking_lot: pure Rust. Nothing else in the container path needs a package (full step scan in the checklist below).

### 5. YAML parse + expression audit

`python3 yaml` parse: build job keys `['name','runs-on','container','defaults','strategy','steps']`; the new step sits at index 1 (after checkout, before "Install Rust toolchain"); `if: runner.os == 'Linux'`. The new run block (`set -euo pipefail` / `apt-get update` / `apt-get install -y --no-install-recommends curl ca-certificates`) contains **no `${{`** — no expression marker, and the comment above it is brace-free too (this repo has a documented history of `${{` inside a run-block comment invalidating the whole workflow — commit `1ae3532`; the new comment is clean). The `if:` uses a normal context expression (`runner.os == 'Linux'`), the same pattern as the existing glibc gate at release.yml:169. No hazard.

---

## Findings

### P1 — curlm-R1-P1 — the fix installs only curl; the container still has no C compiler or linker, so the Linux build fails at `cargo build`

**File:** `.github/workflows/release.yml:104` (the new `apt-get install` line, step 99-104), failing steps at `:122-123` (Build release binary) and `:147-148` (parity test).

**What:** The new step installs `curl ca-certificates` and nothing else. `debian:bookworm` ships **no `cc`, `gcc`, `ld`, `make`** (verified empirically on the exact image tag the runner pulls). Two independent, deterministic failures await once the toolchain step passes:

1. **Linker:** rustc's default linker driver on `*-unknown-linux-gnu` is `cc`; with no `cc`/`gcc`/`clang` on PATH, `cargo build` dies at the very first link with `error: linker 'cc' not found`.
2. **C sources:** the `ship` profile compiles C — `store-sqlite` → `sqlx` `sqlite` (= `sqlx-sqlite/bundled`) → `libsqlite3-sys 0.30.1` bundled (`build.rs` compiles sqlite3.c via the `cc` crate), plus `aws-lc-sys 0.44.0` (default `CcBuilder`) and `ring 0.17.14` from the rustls stack. Without a C compiler the libsqlite3-sys build script fails even before linking.

**Why it matters:** the change's entire purpose is to un-break the Linux build so v0.2.0 can ship. After this fix the job advances exactly one step further and fails deterministically at the first `cargo` invocation; the parity test (`cargo test --release --features ship`) recompiles the same profile and fails identically. **The release workflow remains red and the tag cannot be re-pushed to a green release.** Empirical proof: running the exact fix command in the container leaves `cc/gcc/ld/make/readelf` all missing; adding `build-essential` makes all of them present and a C compile+link smoke passes.

**Fix:** extend release.yml:104 to

```yaml
apt-get install -y --no-install-recommends curl ca-certificates build-essential
```

`build-essential` (gcc, binutils→`ld`, `make`, libc6-dev headers, g++) is precisely the toolchain the source build needs — nothing more: no `cmake` binary (aws-lc-sys's cmake builder is FIPS-only), no `pkg-config` (bundled libsqlite3-sys), no `libclang` (bindgen off by default). Update the step comment to state *why* each package is present (curl → rustup bootstrap; ca-certificates → TLS to sh.rustup.rs/static.rust-lang.org; build-essential → `cc` linker driver + C sources in libsqlite3-sys/ring/aws-lc-sys), and rename the step (nit N1). Then **run one real build of the release workflow** (P3-2) before re-tagging.

---

### P3 — curlm-R1-P3-1 — floating `debian:bookworm` tag is not digest-pinned

**File:** `.github/workflows/release.yml:65,70` (both Linux matrix rows).

**What:** `container: debian:bookworm` resolves to a mutable tag. Today it is digest `sha256:813017f3…` (the image verified in this review), but any bookworm point-release (security updates are frequent) republishes the tag, silently changing the build image — and thereby the effective glibc floor the T12 gate measures, and the toolchain the container provides.
**Why P3:** pre-existing T12 choice, adjacent to (not introduced by) this fix; no current breakage. But this fix is the moment the container's package set becomes load-bearing, so pinning is cheap insurance.
**Fix (optional, low urgency):** `container: debian:bookworm@sha256:813017f3d62be4b5891a7acca6a01bdcd4b8513daa81b1ab99d3a50385b26931` in both rows, refreshed deliberately on upgrade.

### P3 — curlm-R1-P3-2 — the container path is exercised only on tag pushes; nothing validates it before a release

**File:** `.github/workflows/release.yml:91-195` (build job); see also `dev-diary/notes/remediation-tasks.md:968` ("draft/PR release build on the new bookworm runner" — still outstanding).

**What:** no actionlint (explicitly "not installed" in the T12 reviews) and no PR/dry-run of the build job in the container; both container regressions so far (missing curl; now missing toolchain) were discoverable only by pushing a release tag. This review found the toolchain gap by reasoning + local docker; CI would have found it in minutes.
**Why P3:** process improvement, not a defect in this diff — but the recommendation is load-bearing: this review cannot run the full workflow, so **run one real build of the fixed release workflow (draft/PR or `workflow_dispatch`) and confirm both Linux rows reach "Upload binaries" green before re-pushing the v0.2.0 tag**. Optionally add actionlint to CI.

---

## Nits

- **curlm-R1-N1** — `release.yml:99` step name "Prepare container (install curl)" understates the step once P1 lands; rename to something like "Prepare container (install build toolchain)".
- **curlm-R1-N2** — checkout only works without `git` because actions/checkout v4.4.0 silently fell back to the REST-API archive (observed in the run log: "The repository will be downloaded using the GitHub REST API"). No current step needs a `.git` dir, but any future one (git-derived versioning, `git` commands in a run step) will fail confusingly. One comment line near the container wiring (or adding `git` to the apt line) would document the constraint. Informational.
- **curlm-R1-N3** — once `build-essential` is installed, the glibc gate's `command -v readelf || apt-get update && apt-get install … binutils` (release.yml:175-177) becomes a no-op fallback. That is fine — the two apt-get invocations are sequential, non-concurrent steps in the same container and cannot conflict (same pattern the gate already used) — but the gate's comment ("binutils … not guaranteed") drifts slightly; optionally note that the prepare step now provides it.

---

## Review-focus checklist

1. **Root cause correct?** ✓ Verbatim run-log: `curl: command not found` → `##[error]Process completed with exit code 127.`; mechanism (dtolnay action's `curl https://sh.rustup.rs | sh` under `bash -e -o pipefail`) confirmed from the action source at the pinned SHA; curl confirmed absent in `debian:bookworm`.
2. **Fix shape?** Placement ✓ (after checkout, before "Install Rust toolchain", release.yml:99-104). Gating ✓ (`if: runner.os == 'Linux'` targets exactly the two container rows; macOS/Windows hosts ship curl). Root-user apt ✓ (uid=0, no sudo needed; verified). `ca-certificates` ✓ (proven required: exit 77 without, exit 0 with). No expression hazard ✓ (no `${{` in the run block or the comment; `if:` is a normal context expression). No conflict with the glibc gate's binutils apt-get ✓ (separate sequential steps).
3. **Right approach vs alternatives?** ✓ In-job apt-get is the right call here: it matches the pattern the glibc gate already established (in-container apt as needed), needs no new image-maintenance surface (a custom `rust:bookworm`-style image or a GHCR image with the toolchain baked in would be cleaner/faster per-run but adds a publish+update loop for a project whose workflow style is minimal and pinned). ~10-20 s per Linux row is fine. The only real problem is that the install list is incomplete (P1).
4. **Anything else the container path needs?** Full step scan: checkout (REST-archive fallback — no git needed) ✓; toolchain (curl+ca-certificates after fix) ✓; Add target (rustup) ✓; rust-cache (JS action on host, reads the shared workspace) ✓; Derive version (sed/head — coreutils, present) ✓; **Build release binary — needs `cc`/linker/C compiler: FAILS (P1)**; parity test — same profile, same failure ✓-P1; Stage/checksum (mkdir/cp/sha256sum — present) ✓; glibc gate (readelf via its own binutils apt-get, no conflict) ✓; Upload (JS action on host) ✓. **The only missing piece beyond the fix is the C toolchain/linker.**
5. **glibc gate still works after this change?** ✓ Independent, sequential apt-get; with build-essential it becomes a no-op fallback (readelf already present); either path correct.
6. **Nits** — N1-N3 above.

---

## Disposition

**REQUEST_CHANGES.** The root-cause diagnosis and the shape of the fix (placement, gating, root apt, ca-certificates, no expression hazard) are all correct and verified — but the change does not achieve its goal: the Linux rows will fail deterministically at `cargo build` because `debian:bookworm` has no C compiler or linker. Extend the install line to include `build-essential`, update the comment/step name, and run one real build of the release workflow before re-pushing the v0.2.0 tag. With that, the workflow should build Linux in the container and the release can proceed.

```json
{
  "verdict": "REQUEST_CHANGES",
  "findings": {
    "P1": ["curlm-R1-P1: .github/workflows/release.yml:104 - the fix installs only curl ca-certificates; debian:bookworm (verified: digest 813017f3..., no cc/gcc/ld/make) still cannot build. rustc's default linux-gnu linker driver is 'cc' (error: linker 'cc' not found), and the ship profile compiles C (store-sqlite -> sqlx sqlite=bundled -> libsqlite3-sys 0.30.1; rustls -> aws-lc-sys 0.44.0 cc-builder + ring 0.17.14), so 'Build release binary' (release.yml:123) and the parity test (release.yml:147-148) fail deterministically right after the toolchain step. Fix: apt-get install -y --no-install-recommends curl ca-certificates build-essential; update the comment/step name; then run one real build of the fixed workflow before re-tagging v0.2.0."],
    "P2": [],
    "P3": ["curlm-R1-P3-1: release.yml:65,70 - floating debian:bookworm tag is not digest-pinned; bookworm point-releases silently change the build image and the glibc floor the T12 gate measures. Consider debian:bookworm@sha256:813017f3d62be4b5891a7acca6a01bdcd4b8513daa81b1ab99d3a50385b26931.", "curlm-R1-P3-2: the container path is only exercised on tag pushes (no actionlint, no PR/dry-run); both container regressions shipped once each. Before re-pushing v0.2.0, run one real (draft/PR or workflow_dispatch) build of the fixed release workflow to prove both Linux rows reach 'Upload binaries' green; optionally add actionlint."],
    "nits": ["curlm-R1-N1: release.yml:99 step name 'Prepare container (install curl)' understates the step once build-essential lands; rename to 'Prepare container (install build toolchain)'.", "curlm-R1-N2: checkout works without git only via actions/checkout's REST-API archive fallback (observed in the run log); document the constraint (no .git in container) near the container wiring.", "curlm-R1-N3: with build-essential, the glibc gate's conditional binutils install (release.yml:175-177) becomes a harmless no-op fallback; optionally refresh its comment."]
  },
  "summary": "Root cause verified end-to-end from the real failing run (31992650303 @ v0.2.0/1ae3532): dtolnay/rust-toolchain's 'Install rustup if needed' ran 'curl https://sh.rustup.rs | sh' under bash -e -o pipefail in debian:bookworm (no curl) -> 'curl: command not found' -> exit 127, matching the fix's comment verbatim. The fix is correctly placed (before the toolchain step), correctly gated (runner.os == 'Linux'; macOS/Windows hosts ship curl), runs as root (uid=0 verified), and ca-certificates is genuinely required (curl fails exit 77 without it; works with it). No expression hazard; no conflict with the glibc gate. BUT the fix is incomplete: the same container has no cc/gcc/ld/make (verified empirically with the exact fix command), rustc needs a 'cc' linker driver and the ship profile compiles C (libsqlite3-sys bundled via sqlx sqlite, aws-lc-sys cc-builder, ring), so the Linux build fails deterministically at cargo build immediately after the toolchain step. Adding build-essential makes cc/gcc/ld/make/readelf present and a compile+link smoke passes (verified). With that one-line extension (and a real run of the fixed workflow before re-tagging), the release can proceed; as written, REQUEST_CHANGES."
}
```
