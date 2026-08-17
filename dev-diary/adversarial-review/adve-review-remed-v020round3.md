# Adversarial Review — v0.2.0 release changes (Round 3)

- **Scope**: final clearance of the remediated worktree `/home/nryn/work/worktrees/remed-v020` (detached HEAD at `85b5c79`, same base as round 2). READ-ONLY on source; the only write is this doc. Targeted checks only; Main runs the full suite.
- **Verdict**: **APPROVE** — 0× P1, 0× P2, 0× P3, 0× nits. The worktree is clean and ready to integrate and tag v0.2.0.
- **Disposition**: v020-R2-P3-1 and all four round-2 nits are verified closed with no regressions. All release-path version pins sit at 0.2.0; the packaged crate (freshly regenerated) carries no stale version strings; both installation docs agree on version, URLs, and platform list. No P1/P2/P3/nit remains.

## Method / evidence (all run in this worktree, same HEAD as round 2)

| Check | Result |
|---|---|
| `git status` | 11 modified files (identical set to round 2's 8 + the 3 remediation targets: `site/src/content/docs/installation.mdx`, `scripts/aws-infra/README.md`, `scripts/install.sh`) + untracked round-1/round-2 review docs (expected, same convention as this doc). No other changes. |
| v020-R2-P3-1 — site copy | `site/src/content/docs/installation.mdx:34` `LAMBO_VERSION=0.2.0` ✓; `:12` platform list no longer includes macOS x86_64 ✓; both install URLs correct. Fix applied in place (no blind re-copy) — the site's documented adaptations (MdxSteps/MdxWarning imports, `/lambo/…` link prefixes, Cursor-client prose) are preserved; `git show HEAD:…` proves that prose predates the remediation. |
| v020-R2-P3-1 — reference URLs | `docs/reference/installation.mdx:17` → `https://github.com/nrynss/lambo/releases/latest/download/install.sh` ✓; `:32` → `https://raw.githubusercontent.com/nrynss/lambo/main/scripts/install.sh` ✓ (both previously broken: `…/releaseslatest/…` and `…/mainscripts/install.sh`). |
| Docs consistency | Discriminating patterns identical in both installation docs: `LAMBO_VERSION=0.2.0`, `github.com/nrynss/lambo/releases/latest/download/install.sh`, `raw.githubusercontent.com/nrynss/lambo/main/scripts/install.sh`; **zero** `macOS x86` in either (or in README, workflows, install.sh). Residual reference↔site differences are only the pre-existing site adaptations listed above. |
| v020-R2-P3-1 — aws-infra | `scripts/aws-infra/README.md:164` → `(default \`0.2.0\`)` ✓; `launch_exhibit_ec2.py:38` docstring → `--lambo-version 0.2.0` ✓ and `:94` `DEFAULT_LAMBO_VERSION = "0.2.0"` ✓. |
| v020-R2-N1 | `scripts/install.sh:12` comment → `e.g. "0.2.0"` ✓. |
| v020-R2-N3 — pycache | Stale `0.1.0` bytecode gone. `scripts/aws-infra/__pycache__/` exists but is **fresh** (08:56, post-fix): `launch_exhibit_ec2.cpython-314.pyc` strings show only `0.2.0`; gitignored (`.gitignore:29`), untracked, not shipped. Regeneration on import is normal Python behavior; the round-2 concern (pre-fix embedded 0.1.0) is resolved. |
| 0.1.0 sweep (release path) | Clean. No 0.1.0 in src/ (except the fixture), `.github/` (workflows + notes template), README.md, install.sh, or either installation doc. Cargo.toml `0.2.0`, Cargo.lock lambo entry `0.2.0`; tag assertion dynamic (`release.yml` derives `VERSION` from Cargo.toml and hard-fails if `GITHUB_REF_NAME != v$VERSION`). |
| Fresh crate scan | `cargo package --allow-dirty --no-verify` → **103 files, 3.0 MiB (768.5 KiB compressed)** — identical surface to round 2. Extracted the fresh `.crate` (mtime = this run) and scanned: only `src/cli/provision.rs:140` (test fixture), `scripts/aws-infra/README.md:103` (`10.0.1.0/24` CIDR), and Cargo.lock dependency versions (`pin-utils 0.1.0`, `wasite 0.1.0`). All non-version, no action — confirmed. Packaged `aws-infra/README.md:164` reads `0.2.0`. |
| 0.1.0 elsewhere (classified, all no-action) | `site/package.json`+lock (npm metadata — round-2 accepted skip); `spikes/*/Cargo.toml` (standalone crates, no `[workspace]`, excluded from whitelist); `docs/plans/*.md` (historical planning prose, not shipped/deployed); `docs/reference/end-to-end.mdx:82` + `site/…/end-to-end.mdx:86` (`"clientInfo":{"name":"probe","version":"0.1.0"}` — example JSON-RPC initialize payload, non-version; pre-existing, reviewed in round 2). None is a lambo version pin in the release path. |
| 11-file diff vs round-2 | The 8 round-2 files (Cargo.toml whitelist + default-features, ci.yml fixtures matrix rows, release.yml toolchain/notes-render/publish-crate, template `{{VERSION}}`, README cargo-install note, lock) are unchanged in content; `docs/reference/installation.mdx` + `launch_exhibit_ec2.py` now carry the amended remediation; the 3 new files are exactly the remediation targets. YAML parse of ci.yml + release.yml: OK. |
| v020-R2-N4 | N4a (reference broken URLs) **closed** by the repair above. N4b (ci.yml path filters omit `web/**`) remains as documented: pre-existing, out-of-scope, non-blocking (round-2 classified it; not a finding then or now). |

## Findings

### P1 — none

### P2 — none

### P3 — none

### Nits — none

## Residual / observations (non-blocking, no action)

- `target/package/lambo-0.2.0/` unpacked dir is a stale round-2 inspection leftover (mtime 08:46, pre-fix copy). It is gitignored build output and is **not** the shipped artifact — the freshly regenerated `.crate` is authoritative and clean; the next verify-mode `cargo package` (which CI publish-crate runs) refreshes the dir.
- Reference vs site installation docs drift on the verified-clients prose (Cursor content exists only in the site copy, added pre-remediation). Both pages describe the same install procedure and agree on every version/URL/platform fact; cosmetic, out of remediation scope.

## Clean-for-integration statement

The v0.2.0 worktree is clean and ready to integrate and tag **v0.2.0**. The only uncommitted writes are the three review docs (this one plus round-1/round-2, per the established convention); no source changes remain. Version is consistent at 0.2.0 across Cargo.toml, Cargo.lock, the dynamic tag assertion, the notes template, install.sh, both installation docs, and the aws-infra tree; the packaged crate contains no stale version strings; both previously broken reference URLs are repaired; and the docs site page (what docs.yml actually deploys) is fixed in place with its documented adaptations intact.

```json
{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [],
    "nits": []
  },
  "summary": "Round-3 final clearance: v020-R2-P3-1 verified complete and genuine — site copy fixed in place (0.2.0, macOS x86_64 dropped, correct URLs), both reference broken URLs repaired (releases/latest + /lambo/main/scripts), aws-infra README:164 and launch docstring :38 at 0.2.0, install.sh:12 comment updated, stale 0.1.0 pyc replaced by fresh gitignored 0.2.0 bytecode. Discriminating patterns consistent across both installation docs (version, both URLs, platform list; no macOS x86 anywhere in the release path). Freshly regenerated crate (103 files, 3.0 MiB) scans clean — only the provision.rs:140 test fixture, the 10.0.1.0/24 CIDR prose, and Cargo.lock dependency versions carry 0.1.0, all non-version, no action. Cargo.toml/Cargo.lock at 0.2.0, tag assertion dynamic. Remaining 0.1.0 hits (npm metadata, spikes crates, plans prose, MCP probe example payload) classified non-release-path/non-version. The 8 round-2 files are unchanged and consistent with round-2's reviewed state; both workflows YAML-parse; round-2 N1/N3/N4a closed, N4b documented pre-existing. No P1/P2/P3/nit remains. Worktree is ready to integrate and tag v0.2.0."
}
```
