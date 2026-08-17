# Adversarial Review — Round 2: workflow-fix verification (v0.2.0 release workflow failure)

- **Reviewer:** VexReviewR2 (round 2, adversarial)
- **Scope:** worktree `/home/nryn/work/worktrees/remed-vex` at `ff3ba9d` (detached, uncommitted
  2-file diff: `.github/workflows/release.yml`, `.github/release/release-notes-template.md`)
- **Prior:** `adve-review-remed-vexround1.md` (same directory) — REQUEST_CHANGES (P1: the literal
  `` `${{` `` inside the comment at `release.yml:215` was the actual GitHub-validation trigger —
  proven by the real error on both failed v0.2.0 runs: `Unexpected symbol: '`'` at the comment's
  `${{`; the sed line's bare `{{VERSION}}` was inert). Round-1 P2 (wrong root cause) and the fix
  direction were folded into that verdict.
- **Mode:** read-only; evidence gathered from the worktree via grep audits (dedicated tool,
  absolute paths rooted at the worktree), PyYAML parse, duplicate-key scan, sed render
  simulation, full re-read of all three workflows, and git (HEAD, diff, tags, `ls-remote`).

## Verdict

**APPROVE.**

The P1 trigger is gone: the comment at `release.yml:215` no longer contains `${{` (or any brace
sequence), the run block is now brace-free end to end, and a full adversarial sweep of all three
workflows finds **zero** expression markers in comments, zero bare `{{`, and every `${{` a
well-formed, same-line `${{ ... }}` (17/17, 3/3, 1/1). No remaining hazard exists that PyYAML
accepts but GitHub's stricter parser rejects. The workflow is ready to commit, push, and re-tag
`v0.2.0` (with the two operational nits below: order the push before the tag move, and the remote
tag must be force-moved).

---

## Evidence (grounding)

### 1. The P1 comment is repaired (grep-verified)

Current render-notes block (`release.yml:210-218`, read in full):

```
210:      - name: Render release notes
211:        run: |
212:          set -euo pipefail
213:          # The template carries a literal __LAMBO_VERSION__ placeholder (P3-2);
214:          # substitute the version derived from Cargo.toml above (already
215:          # asserted to match the tag). GitHub Actions parses expression
216:          # markers inside run blocks, so the placeholder is brace-free.
217:          sed "s/__LAMBO_VERSION__/$VERSION/g" \
218:            .github/release/release-notes-template.md > /tmp/notes-$VERSION.md
```

The old line 215 (`` GitHub Actions only parses `${{` as an ``) is replaced by a comment with no
`$`, no backtick, and no brace sequence of any kind. The whole run block contains no `${{`, no
`{{`, no `}}`. The diff is exactly the 4 line-pairs expected (placeholder name in the comment, the
two reworded comment lines, the `sed` pattern); nothing else in the workflow changed.

### 2. Every `${{` is a well-formed single-line expression with matching `}}`; none in comments (programmatic audit)

Python scan of all three workflows: every line's count of `${{` equals its count of `}}`, on the
same line, and no `{{`/`}}` appears after a `#` on any line:

- `release.yml`: 17 opens, 17 closes, identical line sets — 35, 36, 41, 100, 111, 119, 136,
  142×4 (`matrix.target`, `matrix.ext`, `matrix.name`, `matrix.ext`), 144×2 (`matrix.name`,
  `matrix.ext`), 166, 181, 222 (`secrets.GITHUB_TOKEN`), 254 (`secrets.CARGO_REGISTRY_TOKEN`).
- `ci.yml`: 3/3 — 134 (`matrix.name`), 135 (`matrix.command` — the `run: ${{ matrix.command }}`
  matrix-driven step, valid), 145 (`secrets.LAMBO_COCKROACH_DSN`).
- `docs.yml`: 1/1 — 47 (`steps.deployment.outputs.page_url`).

`braces after # comment: NONE` for all three files. Matches the round-1 locally-verified counts.

### 3. No bare `{{` anywhere in any workflow

`(?<!\$)\{\{` over the worktree `.github/workflows` directory: **no matches**. Repo-wide scan of
`.github` for `{{VERSION}}` / `{{[A-Z_]*}}` placeholder remnants: **no matches**. The template
uses `__LAMBO_VERSION__` exclusively; the workflow's `sed` targets `__LAMBO_VERSION__`. Consistent.

### 4. sed render is correct (simulated, `VERSION=0.2.0`)

- Template placeholders: **8** `__LAMBO_VERSION__` (title 1, checksums sentence 1, asset table 4,
  install block 2 — the install line carries 2 on one line, so 7 changed lines = 8 substitutions,
  matching the diff stat). Zero `{{VERSION}}` in the template.
- Rendered: **8** occurrences of `0.2.0`; **0** leftover `__LAMBO_VERSION__`; **0** leftover
  `{{VERSION}}`; **0** `${{`/`}}` in rendered output.
- `__LAMBO_VERSION__` is sed-safe (no `&`, `\`, or `/`; underscores literal in both BRE pattern
  and replacement) and carries no brace syntax into the run block.

### 5. Full adversarial scan for GitHub-stricter-than-PyYAML hazards (all clear)

Re-read every `on:`, `env:`, `permissions:`, `if:`, `uses:`, and `run:` block in all three
workflows; ran the tab and duplicate-key scans:

- **`on:` key** — PyYAML loads it as boolean `True` (confirmed: top-level key types include
  `bool`); this is the well-known benign quirk, GitHub parses `on:` natively. Not a hazard.
- **Tabs** — none (`\t` scan clean on all three files).
- **Duplicate keys** — none (custom loader that raises on repeats; all three files clean).
- **`if: runner.os == 'Linux'`** (`release.yml:157`) — valid implicit-expression syntax;
  `==`/quoting fine. No `${{ }}` needed or present.
- **`set -euo pipefail` `run: |` blocks** — standard block scalars; GitHub treats them as shell
  scripts and scans for `${{`; now brace-free where they were a problem.
- **Backticks in comments** — several remain (e.g. release.yml:42-45, 145-148). Inert: backticks
  were only fatal as the *first token of an expression following `${{`*; no `${{` exists in any
  comment (check 2), so no expression is ever started there.
- **Shell constructs that are not expression markers** — `$(...)`, `${VERSION}`, `${max_glibc#GLIBC_}`, `"$GITHUB_ENV"`, `::error::`, `--verify-tag`, `dist/lambo-*`: single-brace or brace-free shell text, not scanned by GitHub.
- **Scalar punctuation** — `container: debian:bookworm` (colon inside plain scalar, valid YAML,
  standard workflow pattern), `runs-on: ubuntu-24.04-arm`, `tags: ['v*']` and
  `branches: [main, master, 'phase/**']` (quoted globs), `fail-fast: false`,
  `merge-multiple: true`, `concurrency:`, `environment:` — all standard, all parse under both
  parsers.
- **SHA-pinned third-party actions** with `# v4.4.0`-style trailing comments — no brace sequence
  after any `#` (covered by check 2).
- No `${{`/`}}` anywhere in the release-notes template (it is sed-input only, never parsed by
  GitHub).

### 6. Tag readiness (git-verified)

- `HEAD` = `ff3ba9dda8c3b651377ee9096098b666cd3efeb5` (the v0.2.0 bump commit), detached; fix is
  uncommitted on top.
- `Cargo.toml` `version = "0.2.0"` — matches `v0.2.0`, so the release job's tag-vs-version assert
  will pass.
- **Remote tag `refs/tags/v0.2.0` already exists and points at `ff3ba9d`** (`git ls-remote`
  verified). The tag therefore **must not** be re-pushed until the fix commit is on top of the
  history **and** the workflow has validated; and a plain `git push origin v0.2.0` after the fix
  will not move it (the remote ref already equals the old commit — the push would be a
  no-op/rejected). The move requires `git push origin :refs/tags/v0.2.0` then
  `git push origin v0.2.0`, or `git push --force origin v0.2.0` (see nit vex-R2-N2).

---

## Findings

### P1

None. The round-1 P1 trigger (literal `${{` in the comment at `release.yml:215`) is removed; no
new expression-marker hazard exists anywhere in the workflows. This is the round-1 blocker, and it
is closed.

### P2

None. The fix now addresses the actual root cause (comment content) rather than the inert sed
placeholder, and the comment states the corrected rule ("GitHub Actions parses expression markers
inside run blocks, so the placeholder is brace-free") without containing the forbidden sequence.
No remaining GitHub-stricter-than-PyYAML hazard was found in the full scan (check 5).

### P3

- **vex-R2-P3-1 — The comment explains why the *placeholder* is brace-free, but not why *comments*
  must be brace-free (the exact failure mode that shipped twice).** Where: `release.yml:215-216`.
  The current sentence is factually correct and brace-free, but it does not warn that GitHub scans
  comment text too — a future editor could reintroduce `${{` in a comment under the (round-1,
  disproven) belief that comments are inert. One appended clause, e.g. "markers are recognized
  even inside comments, so keep this whole block brace-free," would harden the spot where the bug
  lived. Optional; the block is safe as-is.
- **vex-R2-P3-2 — `(P3-2)` internal review-ID cross-reference remains in the comment at
  `release.yml:213`.** Carried over from vex-R1-N1. Opaque to any future maintainer outside the
  dev-diary; drop the parenthetical. Cosmetic.
- **vex-R2-P3-3 — `__LAMBO_VERSION__` is Markdown emphasis syntax in the raw template.** Carried
  over from vex-R1-P3-3. Double underscores bold `LAMBO_VERSION` if the template is ever viewed
  raw; harmless operationally (template never published; sed replaces all 8 before rendering).
  Optional token rename (e.g. `LAMBO_VERSION_PLACEHOLDER`).

## Nits

- **vex-R2-N1 — No CI coverage for workflow files, and no validation step; a `${{` regression can
  ship silently again.** `ci.yml`'s path filters include `.github/workflows/ci.yml` but NOT
  `release.yml` or `docs.yml`, so a broken release workflow never triggers CI — the exact reason
  this failure shipped twice. Add `release.yml`/`docs.yml` to the path lists and an actionlint
  step (or at minimum a YAML/expression lint) to the check job; it would have flagged line 215
  before push in round 1.
- **vex-R2-N2 — Re-tag sequence and remote-tag state.** The remote `v0.2.0` tag already exists at
  `ff3ba9d` (verified). Correct order: (1) commit this fix, (2) push the branch — GitHub validates
  the workflow file at push time; confirm no "Invalid workflow file" annotation appears on the
  Actions tab, (3) only then move the tag: `git push origin :refs/tags/v0.2.0` (or
  `git push --force origin v0.2.0`) after re-creating it on the fix commit, then push the tag to
  trigger the release run. A plain `git push origin v0.2.0` will not move an existing remote tag.
- **vex-R2-N3 — sed delimiter style.** `sed "s/__LAMBO_VERSION__/$VERSION/g"` is correct as
  written; a `|` delimiter or `${VERSION}` would be marginally more defensive but there is nothing
  to fix (carried from vex-R1-N3).

## Summary

Round-1's P1 — the literal `` `${{` `` inside the comment at `release.yml:215`, which GitHub's
expression scanner picked up even in a shell comment (the exact error on both failed v0.2.0 runs,
`Unexpected symbol: '`'`) — is fixed: the comment is reworded with no brace sequence, the render
run block is brace-free end to end, and no `${{`/`{{`/`}}` remains anywhere a comment or run block
could trip the parser. Full adversarial sweep is clean: 17/17, 3/3, 1/1 `${{ ... }}` expressions,
all single-line and balanced, none in comments; zero bare `{{`; zero `{{VERSION}}` remnants
repo-wide; the template's `__LAMBO_VERSION__` (8×) renders to 8× `0.2.0` with 0 leftovers; no
tabs, no duplicate keys, benign `on:` quirk, valid `if:`/`run:` syntax; no hazard PyYAML accepts
that GitHub rejects. `Cargo.toml` is `0.2.0`, matching the tag. Remaining items are two cosmetic
P3 carryovers, one precision nit on the repaired comment, and one operational nit (order the push
before the force-moved tag — the remote tag already exists at the old commit). **APPROVE** — the
workflow is ready to commit, push, and re-tag `v0.2.0`.

{
  "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": ["vex-R2-P3-1: release.yml:215-216 - the repaired comment explains why the placeholder is brace-free but not that GitHub scans comments too (the round-1 failure mode); a clarifying clause would harden the spot where the bug lived - optional, block is safe as-is.", "vex-R2-P3-2: release.yml:213 - '(P3-2)' opaque review-ID cross-reference carried over from vex-R1-N1; drop the parenthetical.", "vex-R2-P3-3: template __LAMBO_VERSION__ is Markdown emphasis in the raw template (carried from vex-R1-P3-3; cosmetic - template never published raw)."],
    "nits": ["vex-R2-N1: ci.yml path filters omit release.yml/docs.yml and there is no actionlint/validation step, so a ${{ regression in a workflow can ship without CI noticing (the cause of the double failure); add the workflow files to the path lists and an actionlint step.", "vex-R2-N2: remote tag v0.2.0 already exists at ff3ba9d (ls-remote verified) - after committing/pushing the fix and confirming GitHub accepts the workflow (no 'Invalid workflow file' annotation), the tag must be force-moved (delete+recreate or --force); a plain push will not move it.", "vex-R2-N3: sed delimiter style fine as-is (carried from vex-R1-N3); nothing to fix."]
  },
  "summary": "APPROVE. Round-1 P1 is closed: the literal ${{ inside the comment at release.yml:215 - the proven GitHub-validation trigger ('Unexpected symbol: backtick' on both failed v0.2.0 runs) - is removed; the render run block is now brace-free end to end. Adversarial full-file audit of all three workflows is clean: 17/17, 3/3, 1/1 ${{ }} expressions all well-formed, single-line, balanced, none inside comments; zero bare {{ anywhere; zero {{VERSION}} remnants; template renders 8/8 __LAMBO_VERSION__ -> 0.2.0 with 0 leftovers; no tabs, no duplicate keys, benign on: quirk; valid if:/run:/uses: syntax; no hazard PyYAML accepts that GitHub rejects. Cargo.toml 0.2.0 matches the tag. Only carryover P3s/nits remain (comment precision, opaque (P3-2) reference, markdown-emphasis token, no workflow validation in CI, and the operational tag-move sequence). The workflow is ready to commit, push, and re-tag v0.2.0 - push the fix first, confirm the workflow validates on GitHub, then force-move the existing remote v0.2.0 tag to the fix commit."
}
