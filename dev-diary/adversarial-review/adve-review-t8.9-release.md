# Adversarial Review — T8.9 release (adve-review-t8.9-release)

Branch: `task/release` · Worktree: `/home/nryn/work/lambo/worktrees/task-release`
Reviewer: `ReleaseReview` · Date: 2026-08-15
Scope: UNCOMMITTED T8.9 release-implement work (findings only, no remediation).

## Files reviewed
- `.github/workflows/release.yml` (new) — 131 lines
- `scripts/install.sh` (new) — 111 lines
- `.github/release/release-notes-template.md` (new)
- `Cargo.toml` (modified: adds `ship` feature)
- `docs/reference/installation.mdx` (modified: prebuilt-install section)

## Verdict: FINDINGS
1 × P1 (release-blocking), 1 × P3 (claim mismatch / informational).

---

## Finding T8.9-P1 — `install.sh` is never published as a release asset, so the primary documented install command 404s

- **Severity:** P1
- **File:** `.github/workflows/release.yml:123-131` (release job's `gh release create`)
- **Affected docs:** `scripts/install.sh:4`, `scripts/install.sh` header docstring lines 4; `.github/release/release-notes-template.md:48`; `docs/reference/installation.mdx:27`
- **Evidence:**
  - The release job downloads artifacts via `pattern: lambo-*` / `merge-multiple` into `dist/`, then runs:
    ```
    gh release create "$GITHUB_REF_NAME" \
      --title "Lambo v$VERSION" \
      --notes-file .github/release/release-notes-template.md \
    dist/lambo-*
    ```
    Only the staged `lambo-<v>-<name>[.exe]` binaries and their `.sha256` files land in `dist/`. `scripts/install.sh` is **not** copied into `dist/` and is never attached to the release.
  - GitHub's `https://github.com/<owner>/<repo>/releases/latest/download/<file>` endpoint serves **release assets only** — not repo files. Because `install.sh` is not an asset, this URL returns **404**.
- **Impact:** Three places document the primary install path as
  `curl -fsSL https://github.com/nrynss/lambo/releases/latest/download/install.sh | sh`
  (install.sh self-doc line 4, release-notes template line 48, installation.mdx line 27). That exact pipeline — the headline way users install the product — will 404 for every release. The secondary pinned command in installation.mdx:32 uses `https://raw.githubusercontent.com/nrynss/lambo/main/scripts/install.sh`, which does resolve, but the primary channel is broken.
- **Required to fix (remediation, not done here):** attach the script in the release job, e.g. add `scripts/install.sh` to the `gh release create …` asset list (gh supports `scripts/install.sh:install.sh` or a plain path), so `/releases/latest/download/install.sh` resolves.

---

## Finding T8.9-P3 — `gh release create` omits the claimed `--verify-tag`

- **Severity:** P3 (informational; functionally benign)
- **File:** `.github/workflows/release.yml:128`
- **Evidence:** The claimed outcome specified `gh release create --verify-tag`. The actual command is `gh release create "$GITHUB_REF_NAME"` with no `--verify-tag`.
- **Assessment:** `--verify-tag` verifies the tag exists before publishing. In this workflow the tag is the trigger (`on: push: tags: ['v*']`), so `GITHUB_REF_NAME` is always a real tag; additionally the job's own assert at lines 109-114 (`GITHUB_REF_NAME == v$VERSION`) already fails a stale/mismatched tag before any publish. The omission is not a correctness defect, only a claim-vs-implementation discrepancy.

---

## Verified and sound (no findings)

**release.yml**
- Valid YAML (parsed with `yaml.safe_load`).
- Trigger: `on: push: tags: ['v*']`.
- Matrix covers exactly the 5 required targets on **native** runners (no cross/zigbuild):
  - linux-x86_64 → `ubuntu-latest`, `x86_64-unknown-linux-gnu`
  - linux-arm64 → `ubuntu-24.04-arm`, `aarch64-unknown-linux-gnu`
  - macos-arm64 → `macos-14`, `aarch64-apple-darwin`
  - macos-x86_64 → `macos-13`, `x86_64-apple-darwin`
  - windows-x86_64 → `windows-latest`, `x86_64-pc-windows-msvc`
- Build step: `cargo build --release --features "$FEATURES" --target <t>` with `FEATURES: ship`.
- Staging + checksum: `cp … target/<t>/release/lambo<ext> dist/lambo-<v>-<name><ext>` then `sha256sum … > …sha256`. `.exe` ext handled per-row (`ext` column), dry on unix, `.exe` on windows.
- Version derived from Cargo.toml via `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1` → picks the `[package] version` (line 3; only top-level match; confirmed single source of truth).
- Tag assertion (lines 109-114): fails the job when `GITHUB_REF_NAME != v$VERSION` → a stale tag cannot publish mismatched artifacts. Present and correct.
- Release job downloads all artifacts (`pattern: lambo-*`, `merge-multiple: true`) and runs `gh release create` with `--notes-file`.
- All third-party actions SHA-pinned to 40-char SHAs, **identical to ci.yml convention**: `actions/checkout@11d5960a…` (v4.4.0), `dtolnay/rust-toolchain@032958af…` (1.97.1), `Swatinem/rust-cache@6323deb1…` (v2.9.2), `actions/upload-artifact@ea165f8d…` (v4), `actions/download-artifact@d3f86a10…` (v4). Publish uses the preinstalled `gh` CLI (no third-party action), as documented.

**scripts/install.sh**
- POSIX sh; `sh -n` clean (verified).
- `set -eu`; OS detection (`Linux|Darwin`) and arch (`x86_64|amd64`, `aarch64|arm64`) with explicit unsupported-platform abort.
- Version resolution: `LAMBO_VERSION` or `releases/latest` via the GitHub API (`sed`-extracted `tag_name`); empty-result abort.
- Download binary + checksum over HTTPS (`curl -fsSL`, `-L` needed for the S3 redirect).
- SHA-256 verification: extracts first 64-hex token from the checksum file, aborts if empty/malformed, aborts on mismatch (expected vs actual printed). Documented checksum files are `sha256sum` output `<hash>  <file>`, consistent with the workflow's writer.
- Install 0755 into `$LAMBO_INSTALL_DIR` (default `~/.local/bin`), `mktemp -d` + `trap … EXIT HUP INT TERM` cleanup, cross-fs `mv` fine. All vars quoted; no injection surface. (Checksum+asset both ride TLS from the same host — the standard verified-by-transport model, not a defect.)

**`ship` feature (Cargo.toml)**
- `ship = ["store-memory", "store-cockroach", "store-sqlite", "embed-bge", "embed-fixture"]` — the exact authorized full set; **does not pull `embed-bedrock`** (commented as blocked on account authorization). Correct.

**installation.mdx prebuilt section**
- URLs are real and consistent with the repo (`nrynss/lambo` matches `git remote`), no placeholder/fake URLs.
- Style: no em dashes (`—`) or semicolons in the added prose (scanned lines 17-51).
- Primary install + pin + Windows instructions hang together with the release asset naming.

**release-notes-template.md**
- Complete: What's new / Features included / Binary checksums (table matches matrix naming incl. `.exe`) / Install / Known limits / Build-from-source / Verify. Placeholder `VERSION` + list-fill lines are explicit with a maintainer note, not silent slack.

**Versioning**
- `Cargo.toml [package] version = "0.1.0"`; `lambo --version` → `lambo 0.1.0` (verified on the release build). One source of truth; tags `v0.1.0`.

**Scope / collision**
- Exactly the 5 files changed (2 modified + 3 new). release.yml and the `ship` feature line do **not** touch store code, so no conflict with the simultaneous fencing work in the sibling worktree.

## Checks run
- `git status` / `git diff --stat` / `git diff Cargo.toml docs/reference/installation.mdx`
- `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"` → valid
- `sh -n scripts/install.sh` → clean
- `cargo check --features ship` → finished (dev), no errors
- `cargo build --release --features ship` → finished (release), no errors
- `./target/release/lambo --version` → `lambo 0.1.0`
- Style scan of added mdx lines; `git remote -v`; Cargo.toml feature/version/bin inspection; ci.yml SHA-pin comparison; github release-asset semantics reviewed.

---

## Remediation note (ReleaseRemediate · 2026-08-15)

**T8.9-P1 — FIXED.** The release job now copies `scripts/install.sh` into `dist/install.sh` and passes `dist/install.sh` to `gh release create`, so the script is attached as a release asset named `install.sh` (last path segment). `https://github.com/nrynss/lambo/releases/latest/download/install.sh` therefore resolves to a real asset. All three doc sites (install.sh self-doc, release-notes-template.md:48, installation.mdx:27) point at that same `/releases/latest/download/install.sh` URL — now serving.

**T8.9-P3 — FIXED.** `--verify-tag` added to the `gh release create` invocation, matching the claimed intent (benign but honest).

Verification after fix:
- YAML: `yaml.safe_load('.github/workflows/release.yml')` → valid; release step ends with `dist/lambo-* dist/install.sh`.
- `sh -n scripts/install.sh` → clean.
- `cargo check --features ship` → finished, no errors.
- `./target/release/lambo --version` → `lambo 0.1.0`.

---

## R2 Reverify (ReleaseReverify · 2026-08-15) — VERDICT: CLEAN

Re-verified both remediations on the release branch. No findings, no remediation performed.

**T8.9-P1 — REAL & regression-free.** `release.yml` Publish step now `cp scripts/install.sh dist/install.sh` (L132) and appends `dist/install.sh` to the `gh release create` asset list (L137). Asset lands with last path segment `install.sh` → `/releases/latest/download/install.sh` serves it. All three doc sites (install.sh:4, release-notes-template.md:48, installation.mdx:27) use that same URL; secondary pinned `raw.githubusercontent.com/nrynss/lambo/main/scripts/install.sh` (installation.mdx:36) also resolves. Repo `nrynss/lambo` matches docs.

**T8.9-P3 — REAL.** `--verify-tag` present (L136), matching claimed intent.

**Fast checks re-run:** yaml.safe_load → valid; `sh -n scripts/install.sh` → clean; `cargo check --features ship` → finished, no errors; `./target/release/lambo --version` → `lambo 0.1.0`.

**Prior sound findings still hold:** all third-party actions SHA-pinned (40-char, 6 distinct pins); tag==`v$VERSION` assert (L109-114); 5-target native matrix; install.sh shell safety (`set -eu`, sh -n clean, quoted vars, checksum verify); `ship` = store-memory/cockroach/sqlite + embed-bge/fixture, **excludes embed-bedrock**; diff scope = Cargo.toml + installation.mdx only (new files all release-implement scope).
