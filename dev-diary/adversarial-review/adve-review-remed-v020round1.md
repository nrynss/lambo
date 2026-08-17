# Adversarial Review — v0.2.0 release changes (Round 1)

- **Scope**: worktree `/home/nryn/work/worktrees/remed-v020` (detached HEAD at 85b5c79), two changes: version bump `0.1.0 → 0.2.0` (Cargo.toml + Cargo.lock) and a new `publish-crate` job in `.github/workflows/release.yml` (runs `cargo publish` with `secrets.CARGO_REGISTRY_TOKEN`, after the GitHub release, mirroring the vimanam convention).
- **Mode**: READ-ONLY on source. One deliverable: this file.
- **Verdict**: **REQUEST_CHANGES** — 0× P1, 1× P2, 4× P3, 4× nit. The two named changes are individually correct (version consistency verified; the job is placed, permissioned, and env-wired correctly; no secret is leaked by the workflow). The blocker is **what the job would publish**: the crates.io tarball ships the entire repository — including the full internal `dev-diary/`, live-infrastructure identifiers in `evidence/`, and AWS provisioning tooling — with no `[package] include/exclude` in Cargo.toml. crates.io is immutable; this ships forever.
- **Disposition**: fix v020-R1-P2-1 (package scoping) before tagging; the P3s are fast follow-ons (one is a one-line manifest fix that belongs in the same commit).

## Method / evidence (all run in this worktree)

| Check | Result |
|---|---|
| `git diff HEAD` (full) | 3 files: release.yml (+24/−2), Cargo.lock (1 line), Cargo.toml (1 line) |
| Cargo.toml version / features | `0.2.0`; `default = ["store-memory", "embed-bge", "embed-fixture", "fixtures"]`; ship/demo/cockroach/sqlite/bedrock opt-in |
| Cargo.lock lambo entry | `version = "0.2.0"` (L2257) — only line changed in lock; remaining `0.1.0` entries are third-party (`pin-utils`, `wasite`) |
| `python3 yaml.safe_load` of release.yml | Parses; jobs `build, release, publish-crate`; `publish-crate` `needs: release`, `permissions: {contents: read}`, token env present (note: PyYAML reads the `on:` key as bool — a check quirk only, GitHub's parser is fine) |
| `cargo publish --dry-run --allow-dirty` | **Packaged 435 files, 6.4 MiB (1.8 MiB compressed); verify build finished; upload aborted (dry run)**. `--allow-dirty` was needed only because this worktree holds the uncommitted release changes; the real job's clean tag checkout correctly omits it |
| `rustup show active-toolchain` (repo root vs /tmp) | root: `1.97.1 … overridden by '…/rust-toolchain.toml'`; /tmp: `stable (default)` → the `toolchain: stable` input is overridden by the committed `rust-toolchain.toml` |
| `grep 0.1.0` (whole tree) | Only historical docs/evidence, third-party lock entries, the release-notes **template** (live, see P3-2), and `scripts/aws-infra/launch_exhibit_ec2.py` (live, see P3-4). `src/` is clean (`lambo --version` derives from `CARGO_PKG_VERSION`) |
| credential scan of `target/package/lambo-0.2.0/` | No `AKIA*`, no private keys, no password values, no `CARGO_REGISTRY_TOKEN` value (only the workflow's `${{ secrets.* }}` reference). `.env` absent (gitignored). Root `.mcp.json` is NOT tracked. **But** real operational identifiers ship: CockroachDB cluster host `nrynss-19495.jxf.gcp-asia-south1.cockroachlabs.cloud:26257` (4 evidence files), AWS VPC/subnet IDs (`evidence/cloudops-run/`), absolute dev paths (`/home/nryn/work/lambo/...`) |

## Findings

### P1 — none

### P2-1 — crates.io tarball ships the entire repository; no `[package] include/exclude`
- **file:line**: `Cargo.toml:1-10` (no `include`/`exclude`/`publish` control) → affects the whole publish path `release.yml:227-244`.
- **what**: `cargo publish` packages every git-tracked file. Verified contents of `target/package/lambo-0.2.0/` (435 files, 6.4 MiB; of 367 tracked files, 228 — 62% — are internal/aux): `dev-diary/` (2.6 MiB — the entire internal engineering diary, including every adversarial review), `evidence/` (584 KiB — live test logs with a **real CockroachDB cluster hostname + db name**, AWS VPC/subnet IDs from `evidence/cloudops-run/`, MCP client configs with absolute local paths), `scripts/` (372 KiB — `aws-infra/` AWS provisioning + EC2 launch tooling, `cloudops/` agent scripts), `site/` (460 KiB Astro docs source incl. `package-lock.json`), `docs/`, `web/`, `demo/`, `.grok/rules`, `.github/`, `AGENTS.md`, `lambo-hackathon-spec-v0.1.md`, `.env.example`, `.gitignore`.
- **why**: crates.io is **immutable** — once published, the only remedy is a yank, never a retract. This permanently publishes (a) the maintainer's internal engineering notes and reviews, (b) live operational identifiers (cluster endpoint, AWS resource IDs) useful for reconnaissance, and (c) ~2–3× bloat (6.4 MiB vs ~2–3 MiB for `src/` + manifests + the genuinely-needed `migrations/`, `fixtures/`, `examples/`) in every `cargo install` download and in the public file browser. No credentials are exposed (verified — `.env` and the real `.mcp.json` are correctly absent), which keeps this at P2 rather than P1.
- **fix**: scope the package. Whitelist is safer than blacklist (new internal dirs can't sneak in):
  ```toml
  [package]
  # …existing fields…
  include = [
      "src/**",
      "migrations/**",   # include_str! compile input (ci.yml:21)
      "fixtures/**",     # include_str! / golden files (ci.yml:22)
      "examples/**",
      "lambo.example.toml",  # include_str! in config.rs (ci.yml:24)
      "rust-toolchain.toml", # deliberately kept: governs consumer-side cargo install toolchain
      "README.md",
      "LICENSE",
      "NOTICE",
  ]
  ```
  (Cargo.toml / Cargo.toml.orig / `.cargo_vcs_info.json` / Cargo.lock are auto-included for a bin crate.) Verify with `cargo package --list` and re-run the dry-run. Decide `tests/` deliberately (harmless either way — not shipped in the tarball today? it IS: `tests/` 104 KiB ships; exclude unless you want consumers running your tests).

### P3-1 — `toolchain: stable` is dead config; effective toolchain is the pinned 1.97.1
- **file:line**: `release.yml:236-239`.
- **what**: the job installs `stable`, but `cargo publish` runs inside the checkout where committed `rust-toolchain.toml` pins `channel = "1.97.1"` — verified: `rustup show active-toolchain` in the repo root reports `1.97.1 … overridden by rust-toolchain.toml`. The publish verify build therefore runs **1.97.1**, matching ci.yml and the release build job exactly.
- **why**: this is the *good* outcome for reproducibility (publishing is pinned, not floating) — but the config says something it does not do. If someone later deletes `rust-toolchain.toml`, this job silently switches to stable while the rest of CI keeps 1.97.1. It also contradicts the repo's stated convention (ci.yml/build job pass no `toolchain` input and rely on `rust-toolchain.toml`).
- **fix**: drop the `toolchain: stable` input to match the other two jobs (and let `rust-toolchain.toml` remain the single source of truth), or keep it and add a comment explaining the override. Either way the `# 1.97.1` pin comment (P-N1) needs aligning.

### P3-2 — release notes template hardcodes 0.1.0 and is passed to `gh release create` unmodified
- **file:line**: `release.yml:223` + `.github/release/release-notes-template.md:3,33,37-41,54`.
- **what**: the tag assertion (release.yml:192-202) reads the version dynamically, but the notes are not generated — the template is attached raw. It contains `0.1.0` in the asset table (including a `macOS x86_64` row that is deliberately no longer built — release.yml:78-86) and in the pinned install URL.
- **why**: unless the maintainer hand-edits the template for v0.2.0, the published release notes advertise `lambo-0.1.0-*` assets and a `v0.1.0` install URL — the exact class of stale-version drift the workflow otherwise guards against.
- **fix**: substitute in the workflow like the version derivation (`sed "s/0\.1\.0/$VERSION/g"` over the template into a temp file) and drop the stale macOS x86_64 row, or gate the notes file on a checked-in per-release artifact.

### P3-3 — default features include test-only `fixtures`, and `cargo install lambo` silently yields a reduced binary
- **file:line**: `Cargo.toml:54`; consequence for the new install channel claimed by `release.yml:5` and `release-notes-template.md:17-24`.
- **what**: every `#[cfg(feature = "fixtures")]` use in `src/` is inside a `#[cfg(test)]` module — the feature has zero consumer value. Separately: `cargo install lambo` builds **default** features only, so the crates.io channel yields a binary with `store-memory` + `embed-bge` + `embed-fixture` but **no sqlite/cockroach stores**, while the release notes promise "the full adapter feature set compiled into one binary … pick the store at runtime" (true only for the GitHub-release `ship` binary).
- **why**: `fixtures` in default is inert for consumers (it gates test-only code) — either intended (dev convenience) or leftover. The bigger gap is a user-facing contract: a consumer following the new `cargo install lambo` path can configure `store.kind = "sqlite"` in `lambo.toml` and get a hard "rebuild with --features" error at runtime, contradicting the notes.
- **fix**: drop `fixtures` from `default` (keep for dev/test profiles) and document the per-channel feature matrix: GitHub release binary = `ship`; `cargo install lambo` = default (lean) profile. Putting the two sqlx stores in `default` is the alternative but forces sqlx on every library consumer — document, don't default.

### P3-4 — `scripts/aws-infra/launch_exhibit_ec2.py` hardcodes `DEFAULT_LAMBO_VERSION = "0.1.0"`
- **file:line**: `scripts/aws-infra/launch_exhibit_ec2.py:94` (also README.md:164).
- **what**: the deploy script's default version will fetch `lambo-0.1.0-linux-arm64` release assets after v0.2.0 ships.
- **why**: stale default in a live provisioning tool; the release pipeline won't catch it (the script is not executed by CI).
- **fix**: bump to `0.2.0` or derive from the release; note the same sweep should catch `docs/reference/installation.mdx:34` (docs-site pin, updated by docs.yml on main) and `site/package.json` if the docs site is versioned per release.

### Nits
- **v020-R1-N1** — `release.yml:237` comment `# 1.97.1` sits beside `with: toolchain: stable`: the SHA pin is the 1.97.1-era action code, but the effective toolchain is 1.97.1 via `rust-toolchain.toml`. The pair reads as "pinned to 1.97.1 but installing stable" — align comment and input after P3-1.
- **v020-R1-N2** — package noise that disappears with the P2-1 whitelist but is worth naming: `.env.example`, `AGENTS.md`, `.grok/rules`, `.gitignore`, `docs/`, `web/`, `demo/` all ship; `evidence/` configs embed absolute dev-machine paths (`/home/nryn/work/lambo/...`). Keep `rust-toolchain.toml` in the whitelist *deliberately* — it changes consumer-side `cargo install` toolchain resolution via rustup.
- **v020-R1-N3** — `release.yml:227` job named `publish-crate` while its step name is "Publish to crates.io": consistent with the vimanam convention and with `publish-release`/`publish-crate` sibling naming — no change needed; noted only for the record that "release" (job) now means "GitHub release", which the header comment (release.yml:3-6) handles correctly.
- **v020-R1-N4** — header comment (release.yml:3-6) is accurate end-to-end (build → checksums → GitHub release → crates.io, in job order) and line 10's "no extra third-party action is needed" still holds: `publish-crate` reuses the two already-pinned actions (checkout v4.4.0, dtolnay 1.97.1-era SHA) and introduces no new third-party action — convention preserved.

## Job-correctness confirmation (the named change is sound)

- **Placement** — after `release` with `needs: release` (release.yml:228-229): binaries + GitHub release exist before publish; the tag==`v$VERSION` assertion in the release job transitively gates publish, so the published crate version always matches the tag. ✓
- **Permissions** — job-level `contents: read` (release.yml:231-232): sufficient (checkout needs read; `cargo publish` never touches the GitHub API/GITHUB_TOKEN) and the right hardening — the top-level `contents: write` needed by `gh release create` (release.yml:22-23, 211-225) applies only to the release job; the override breaks nothing. ✓
- **Token** — `CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}` (release.yml:243) is the correct crates.io env var; env-only, never echoed (static step name, `run: cargo publish`), and the workflow fires only on `v*` tags (release.yml:18-20). ✓
- **Features** — `cargo publish` correctly takes no `--features` (the top-level `FEATURES: ship` env is consumed only by build steps): the uploaded manifest is verbatim, the verify build compiles default features (dry-run passed), and downstream `cargo add lambo` picks features itself. ✓
- **Pinned-action convention** — publish-crate reuses the exact SHAs from ci.yml/build (checkout `11d5960…` # v4.4.0, dtolnay `032958a…` # 1.97.1). ✓

## Summary

Version bump is clean and consistent across Cargo.toml, Cargo.lock (lambo entry only), the packaged manifests, and the dynamically-read tag assertion; no stray 0.1.0 in `src/` or the workflow (only the release-notes template and the AWS deploy script, both flagged). The `publish-crate` job is correctly placed, least-privileged, token-safe, and toolchain-reproducible in practice (via `rust-toolchain.toml`, not via its own `stable` input). The one release-blocking defect is package scope: the crates.io tarball would permanently publish the internal diary, live infrastructure identifiers, and 2–3× bloat because `[package]` has no include/exclude. Fix that (plus the four P3s, two of which are one-line) and this is an APPROVE.

```json
{
  "verdict": "REQUEST_CHANGES",
  "findings": {
    "P1": [],
    "P2": [
      "v020-R1-P2-1: crates.io tarball ships the entire repo (435 files / 6.4 MiB incl. dev-diary/, evidence/ with live Cockroach host + AWS resource IDs, scripts/aws-infra, site/, .grok/, .github/) — Cargo.toml:1-10 has no [package] include/exclude; crates.io is immutable. Fix: include whitelist (src/**, migrations/**, fixtures/**, examples/**, lambo.example.toml, rust-toolchain.toml, README, LICENSE, NOTICE), verify with cargo package --list."
    ],
    "P3": [
      "v020-R1-P3-1: release.yml:239 'toolchain: stable' is dead config — rust-toolchain.toml (1.97.1) overrides it (verified); drop the input to match ci.yml/build, or comment the override.",
      "v020-R1-P3-2: release-notes-template.md hardcodes 0.1.0 (asset table incl. unbuilt macOS x86_64 row, install URL) and is attached raw at release.yml:223 — substitute VERSION in the workflow.",
      "v020-R1-P3-3: Cargo.toml:54 default includes test-only 'fixtures'; and 'cargo install lambo' yields a binary without sqlite/cockroach, contradicting the release-notes full-adapter claim — drop fixtures from default and document the per-channel feature matrix.",
      "v020-R1-P3-4: scripts/aws-infra/launch_exhibit_ec2.py:94 DEFAULT_LAMBO_VERSION = '0.1.0' will fetch stale assets after 0.2.0 ships."
    ],
    "nits": [
      "v020-R1-N1: release.yml:237 '# 1.97.1' comment beside 'toolchain: stable' misleads (effective toolchain is 1.97.1 via rust-toolchain.toml).",
      "v020-R1-N2: .env.example / AGENTS.md / .grok / .gitignore / docs / web / demo ship; evidence configs embed absolute dev paths — resolved by P2-1; keep rust-toolchain.toml in the whitelist deliberately.",
      "v020-R1-N3: publish-crate/publish-release naming is consistent with the vimanam convention — no change.",
      "v020-R1-N4: header comment and pinned-SHA reuse verified accurate."
    ]
  },
  "summary": "Version bump consistent (Cargo.toml/Cargo.lock/packaged manifests/tag assertion all 0.2.0, no src hardcodes). publish-crate job correct: needs: release placement, contents: read sufficient and least-privilege, CARGO_REGISTRY_TOKEN env correct and never logged, tag-only trigger, no --features (manifest published as-is, verify build on default features passed dry-run), pinned-SHA convention preserved. One P2 blocks: the immutable crates.io tarball ships the entire repository including internal dev-diary and live infrastructure identifiers — add [package] include/exclude before tagging. Four P3s (dead 'stable' input, hardcoded 0.1.0 in release notes template + AWS deploy script default, test-only 'fixtures' in default features and the cargo-install feature-matrix gap)."
}
```
