# Adversarial review — T12 round 3: final clearance

**Reviewer:** T12ReviewR3 · **Round:** 3 (final) · **Date:** 2026-08-17
**Target:** `.github/workflows/release.yml` (uncommitted working-tree change; remediation of T12-R1-P1/P2)
**Prior review:** `adve-review-remed-T12round2.md` (APPROVE)
**Scope:** READ-ONLY. Confirm round-2's APPROVE state is unchanged, all P3/nit findings are
informational/benign, the worktree is clean and integration-ready. Skipped the full suite; machine
YAML parse + reading only. No release triggered.

---

## Verification

**Worktree matches round-2's APPROVE state — unchanged since round 2.**
`git diff HEAD` over `.github/workflows/release.yml` is exactly **+40/−1, one file**, identical to
round-2. `git status`: only `release.yml` modified (plus the two untracked round-1/round-2 review
docs in `dev-diary/`). No new edits since round 2. Confirmed present, byte-for-byte as round-2
described:

1. **Job-level container wired** — `container: ${{ matrix.container }}` sits after `runs-on`
   (`release.yml:35-40`).
2. **Linux in `debian:bookworm`** — `python3` PyYAML parse: `linux-x86_64` and `linux-arm64` rows →
   `container=debian:bookworm`; every `run:`/`uses:` step of `build` executes in-container.
3. **glibc gate** — `Assert max required GLIBC <= 2.34 (Linux only)` step after Stage/checksum,
   before Upload; `if: runner.os == 'Linux'`; `set -euo pipefail`; binutils installed if missing
   (`release.yml:152-175`).
4. **macOS/Windows host-native** — `macos-arm64` and `windows-x86_64` rows have `container`
   **ABSENT** (safe omit-property → `NULL` form, not `''`); `release` job containerless.
5. **Comments relocated/reworded** — container/host-native rationale moved above `container:`; the
   "empirical, gate-enforced" wording above `strategy:`.

**YAML parses.** `python3` PyYAML 6.0.3: `build` keys `['name','runs-on','container','defaults',
'strategy','steps']`; rows/container as above. No syntax issue.

**Diff is release.yml-only; sibling empty.** `git diff` touches exactly one tracked file
(`release.yml`). Sibling worktree directory `/home/nryn/work/worktrees/` contains only `remed-T12`;
no sibling worktree present.

---

## Round-2 findings disposition (all closed)

- **T12-R2-P3-1** (info) — host-row correctness rests on GitHub evaluating an *absent* matrix
  property to `NULL`; real-world-confirmed (eddelbuettel production pattern), and the remediation
  correctly uses the **omit** form, guarded by the comment at `:36-39`. Benign, **no change warranted**.
- **T12-R2-P3-2** (info) — fully-static ELF with no `GLIBC_*` tokens would make the gate pass
  (correct for a static binary) but is **unreachable** for the current dynamically-linked Rust
  artifact. Benign, **no change warranted**.
- **T12-R2-P3-3** (info) — `ubuntu-latest`→`ubuntu-24.04` pin on the x64 row (from original T12, not
  remediation) is benign/deterministic and matches `ubuntu-24.04-arm`. **No change warranted**.
- **T12-R2-N1** — empty (round-1 N1/N2/N4 all resolved; N3 N/A).

**No P1/P2/P3/nit remains requiring a worktree change.** Round-2's P1/P2 are remediated and were
period-verified in round 2 (gate pipeline executed against real binaries: 2.38 fails, 2.34 passes,
2.35/2.36 fail, bare `GLIBC_` token sorts below and is harmless).

---

## One empirical caveat (not a worktree defect)

Structural correctness of the container wiring and the gate is verified by reasoning (GitHub Actions
semantics) + the machine YAML parse + round-2's empirical gate-pipeline runs. The **definitive** proof
— that the shipped binary really requires max GLIBC 2.34 and that it runs on Amazon Linux 2023 — is a
**real (draft/PR) release build on the new runner**, confirmed via `readelf --version-info` reporting
`GLIBC_2.34` and running on an AL2023 instance. That is a **D2/D3 verification step**, not a worktree
defect, and does not block integration clearance.

---

## Verdict

Ready for integration. Worktree is at round-2's APPROVE state, all round-2 findings closed as
informational/benign (no change required), no remaining P1/P2/P3/nit, YAML clean, diff release.yml-only,
sibling empty. The structural correctness is solid; only the real-run release build (D2/D3) remains as
post-integration empirical confirmation.

**Verdict: APPROVE**

```json
{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": [],
    "nits": []
  },
  "summary": "Round-3 final clearance. Worktree exactly matches round-2's APPROVE state: diff is +40/-1 across release.yml only, unchanged since round 2. Job-level `container: ${{ matrix.container }}` wired; Linux rows build fully inside debian:bookworm; macOS/Windows rows host-native via the safe omit-property NULL form; release job containerless; glibc gate (Assert max GLIBC <= 2.34) runs after staging, `set -euo pipefail`, covers both Linux arches; comments relocated/reworded. All three round-2 P3s confirmed informational/benign (host-NULL form is the correct real-world pattern; static-binary edge unreachable for the dynamic Rust artifact; ubuntu-24.04 pin benign/deterministic) — no change warranted; N1 empty. No P1/P2/P3/nit requires a worktree change. python3 YAML parses clean; diff touches only release.yml; sibling worktree empty. One empirical caveat noted: definitive proof (readelf GLIBC_2.34 + runs on AL2023) requires a real draft/PR release build on the new runner — a D2/D3 verification step, not a worktree defect. Clean-for-integration: APPROVE."
}
```
