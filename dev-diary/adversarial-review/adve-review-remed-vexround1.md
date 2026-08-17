# Adversarial Review — {{VERSION}} placeholder fix (v0.2.0 release workflow failure)

- **Reviewer:** VexReviewR1 (round 1, adversarial)
- **Scope:** worktree `/home/nryn/work/worktrees/remed-vex` at `ff3ba9d` (detached), uncommitted 2-file diff
  (`.github/workflows/release.yml`, `.github/release/release-notes-template.md`)
- **Mode:** read-only; evidence gathered from the repo, the git history, and the *actual* GitHub
  failure records (repo is public: `nrynss/lambo`)

## Verdict

**REQUEST_CHANGES (P1).**

The fix removed the wrong thing. The real, evidence-backed trigger of the GitHub "Invalid workflow
file" failure was **not** the bare `{{VERSION}}` in the `sed` command (that text is inert) — it was
the **literal `` `${{` `` inside the comment at `release.yml:215`**, which GitHub's run-block
expression parser picks up as an expression start even inside a shell comment. That comment is
**unchanged** by this fix, so the fixed workflow will be rejected by GitHub's parser again. The tag
must NOT be re-pushed until `release.yml:215` is repaired and the workflow validates on GitHub.

---

## Evidence (grounding)

### The actual GitHub error (retrieved from the public repo)

Both failed workflow runs for `ff3ba9d` (`actions/runs/31991864622` and `31991466759`, event
`push`, tag `v0.2.0`; `conclusion: failure`, **zero jobs**) report the identical annotation:

```
Invalid workflow file: .github/workflows/release.yml#L1
(Line: 211, Col: 14): Unexpected symbol: '`'. Located at position 1 within expression: ` as an
# expression, so the bare braces are inert in this script.
sed "s/{{VERSION
```

Decoding: GitHub's expression scanner found `${{` on line 215 — the line whose comment reads
`` GitHub Actions only parses `${{` as an `` — and began parsing the following text as an
expression. The very first token after `${{` is a backtick, which is not a valid expression
symbol → `Unexpected symbol: '`'` at position 1. The scanner consumed the span to the next `}}`,
which at the time was the closing `}}` of `{{VERSION}}` in the `sed` line (line 217) — that is why
the sed line appears inside the reported expression. The bare `{{VERSION}}` was **inert text**; it
only supplied the closing braces that terminated the (already malformed) expression. No second
error is reported for the sed line itself, consistent with bare `{{` (no `$`) being ordinary text
to the parser.

### The fixed file still contains the hazard

`release.yml` (current worktree), render-notes step:

```
211:        run: |
212:          set -euo pipefail
213:          # The template carries a literal __LAMBO_VERSION__ placeholder (P3-2);
214:          # substitute the version derived from Cargo.toml above (already
215:          # asserted to match the tag). GitHub Actions only parses `${{` as an
216:          # expression, so the bare braces are inert in this script.
217:          sed "s/__LAMBO_VERSION__/$VERSION/g" \
218:            .github/release/release-notes-template.md > /tmp/notes-$VERSION.md
```

Line 215 still contains the contiguous sequence `${{`. There is now **no `}}` anywhere in this run
block**, so GitHub will find `${{`, tokenize the immediately following backtick, and fail again
with the same `Unexpected symbol: '`'` error. The parser does not need a closing `}}` to start
tokenizing the expression; the first token after `${{` decides it. The workflow will be rejected
exactly as before.

### Audit of every run: block / expression (complete)

- All `{{` occurrences in `release.yml` are `${{ ... }}` expressions on well-formed lines
  (`matrix.*`, `runner.os`, `secrets.*`) **except** line 215 (the comment) — the only line in any
  workflow with `${{` lacking a same-line `}}`, and the only `${{` inside a comment.
- No lone `}}` anywhere; no other `${{` in comments in `release.yml`, `ci.yml`, or `docs.yml`.
- `ci.yml:135` `run: ${{ matrix.command }}` is a valid expression (matrix-driven `run:`), fine.
- YAML hygiene: zero tab characters, zero duplicate keys (PyYAML re-parse + duplicate-key scan
  clean); the `on:` key quirk (PyYAML loads it as boolean `True`) is the well-known benign case —
  GitHub's parser handles `on:` natively. `tags: ['v*']` is quoted. No other GitHub-stricter
  hazard found: the failure was purely the expression issue above.

### Placeholder replacement checks (ran)

- Template placeholder count: **8** `__LAMBO_VERSION__` (title 1, checksums sentence 1, asset
  table 4, install block 2). The task brief said "7x 0.2.0" — the correct count is **8**.
- `sed "s/__LAMBO_VERSION__/$VERSION/g"` with `VERSION=0.2.0` on the template: **8** substitutions
  of `0.2.0`, **0** leftover `__LAMBO_VERSION__`, **0** leftover `{{VERSION}}`. Renders correctly.
- `__LAMBO_VERSION__` is safe in sed: no `&`, no `\`, no `/` (delimiter), underscores are literal
  in both BRE pattern and replacement; no `__...__` token in the template collides with it.
- Markdown: double underscores are emphasis syntax, so the *template* would render
  `LAMBO_VERSION` bold if viewed raw — cosmetic only; the template is never published and the
  rendered notes contain real versions (no underscores remain).

---

## Findings

### P1 — vex-R1-P1 — The literal `${{` in the comment at release.yml:215 is the actual trigger and is unchanged by this fix; the workflow will fail GitHub's parser again

- **Where:** `.github/workflows/release.yml:215` (render-notes run block, comment line; unchanged
  in the diff).
- **What:** GitHub's workflow-file validator scans `run:` block text for `${{` and parses the
  following text as an expression — **including inside shell comments**. The comment
  `` GitHub Actions only parses `${{` as an `` contains the contiguous sequence `${{`; the
  validator tokenizes the trailing backtick and fails with
  `Unexpected symbol: '`'. Located at position 1 within expression: ...` (the actual error on both
  failed v0.2.0 runs, retrieved from the public repo). The fix changed only the `sed` line and the
  first comment line, leaving line 215's `${{` in place; with no `}}` left in the block, the same
  error will recur. The re-pushed tag will fail again: no job will run.
- **Why it matters:** The stated root cause ("bare `{{VERSION}}` in the sed command") is wrong —
  bare `{{` without `$` is inert text to GitHub's parser. The `${{` in the comment was the only
  malformed-expression trigger, and it survives the fix.
- **Fix:** remove the contiguous `${{` sequence from the comment entirely, e.g.
  `# ... GitHub Actions evaluates only dollar-prefixed double-brace expressions, so bare braces are inert in this script.`
  (no literal `${{` anywhere), or split the characters (`$` + `{{`). Then push and confirm the
  workflow validates (a run with jobs appears); only then re-push the tag.

### P2 — vex-R1-P2 — Root-cause diagnosis was made without consulting the failure record and is incorrect

- **Where:** the fix rationale (comment lines 213-216 and the diff as a whole).
- **What:** The diagnosis attributed the failure to the sed line's `{{VERSION}}` and treated the
  comment's `${{` as proof that "bare braces are inert". The actual GitHub error (public repo,
  Actions tab / `actions/runs` API — retrievable in seconds) points at the comment's `${{` and was
  never consulted. The fix consequently removed the inert text and preserved the hazard (P1).
- **Fix:** re-derive the diagnosis from the actual validation error; after the P1 comment repair,
  validate the workflow on GitHub before re-pushing the tag.

### P3 — vex-R1-P3-1 — The updated comment is factually wrong in the way that matters

- **Where:** `.github/workflows/release.yml:215-216`.
- **What:** "GitHub Actions only parses `${{` as an expression, so the bare braces are inert in
  this script" is backwards on both counts: GitHub parses `${{` even inside comments (this very
  comment broke the workflow), and the claim's own literal `${{` is the hazard. Only the sed
  line's *bare* `{{VERSION}}` was inert.
- **Fix:** folded into P1 — rephrase without the `${{` sequence and state what GitHub actually
  parses.

### P3 — vex-R1-P3-2 — Render-count expectation in the task brief was off by one

- **Where:** template + `sed` render check.
- **What:** brief expected "7x 0.2.0"; the template carries **8** placeholders and the render
  produces 8 substitutions (title 1, checksums sentence 1, table 4, install block 2). The render
  itself is correct: 0 leftover `__LAMBO_VERSION__`, 0 leftover `{{VERSION}}`. Recount only; no
  code change.

### P3 — vex-R1-P3-3 — `__LAMBO_VERSION__` is Markdown emphasis syntax in the raw template

- **Where:** `.github/release/release-notes-template.md` (all 8 occurrences).
- **What:** double underscores render as `<strong>` if the template file is viewed on GitHub
  (e.g. `lambo-LAMBO_VERSION-linux-x86_64` bolded). Harmless operationally — the template is never
  published and sed replaces all 8 before the notes reach the release — but a single-underscore or
  brace-free token without markdown meaning (e.g. `LAMBO_VERSION_PLACEHOLDER`) would avoid
  confusing template readers. Optional.

## Nits

- **vex-R1-N1** — `(P3-2)` in the comment at release.yml:213 cross-references an internal review
  ID that is opaque to any future maintainer outside the dev-diary.
- **vex-R1-N2** — Nothing in CI validates the workflows (no actionlint in ci.yml); the failure
  shipped twice. An actionlint step (or GitHub's own validation awareness when touching run
  blocks) would have flagged line 215 before push.
- **vex-R1-N3** — `sed "s/__LAMBO_VERSION__/$VERSION/g"` is correct as written; a `|` delimiter
  or `${VERSION}` would be marginally more defensive, but there is nothing to fix.

## Summary

The fix is well-intentioned but treats the symptom: it removed the **inert** bare `{{VERSION}}`
text (sed line + comment mention) and left the **actual trigger** — the literal `` `${{` `` in the
comment at `release.yml:215` — untouched. Ground truth from the public repo's two failed v0.2.0
runs shows GitHub's validator rejected the run block with
`Unexpected symbol: '`'` at the comment's `${{`, which is precisely what the fixed file still
contains (now with no `}}` at all). All other checks pass: every other `${{ ... }}` in the
workflows is a well-formed expression, the YAML is clean (no tabs, no duplicate keys, benign
`on:`), the placeholder rename is complete and safe (8/8 substitutions, 0 leftovers), and
`__LAMBO_VERSION__` is safe in both sed and the published notes. **REQUEST_CHANGES**: repair
`release.yml:215` (remove the `${{` sequence from the comment), confirm the workflow validates on
GitHub (jobs appear), and only then re-push the tag.

{
  "verdict": "REQUEST_CHANGES",
  "findings": {
    "P1": ["vex-R1-P1: .github/workflows/release.yml:215 - literal `${{` inside the run-block comment is the actual trigger of GitHub's 'Invalid workflow file' rejection (proven by the real error on both failed v0.2.0 runs: 'Unexpected symbol: backtick' at the comment's ${{); it is unchanged by this fix, so the workflow will fail validation again and the tag must not be re-pushed. Fix: remove the contiguous ${{ sequence from the comment and re-validate on GitHub."],
    "P2": ["vex-R1-P2: root-cause diagnosis is wrong - the failure was the comment's ${{ (line 215), not the sed line's bare {{VERSION}} (which is inert text); the fix was made without consulting the retrievable GitHub failure record and preserved the hazard."],
    "P3": ["vex-R1-P3-1: comment claim 'bare braces are inert' is factually wrong - GitHub parses ${{ even inside comments, and the claim's own ${{ broke the workflow.", "vex-R1-P3-2: brief expected 7x 0.2.0; actual render is 8/8 substitutions, 0 leftovers - render is correct, recount only.", "vex-R1-P3-3: __LAMBO_VERSION__ is markdown emphasis in the raw template (cosmetic; template never published) - optional token rename."],
    "nits": ["vex-R1-N1: '(P3-2)' review-ID cross-reference is opaque to future maintainers.", "vex-R1-N2: no workflow validation in CI (no actionlint); the failure shipped twice.", "vex-R1-N3: sed delimiter style is fine as-is; nothing to fix."]
  },
  "summary": "REQUEST_CHANGES. The fix removed the inert bare {{VERSION}} text but left the actual trigger - the literal `${{` inside the comment at release.yml:215 - in place; GitHub's parser treats ${{ as an expression start even in shell comments (the exact error on both failed runs: 'Unexpected symbol: backtick' at the comment), so the fixed workflow will be rejected again and the v0.2.0 tag must not be re-pushed until line 215 is repaired and the workflow validates on GitHub. Everything else checks out: all other ${{ }} are well-formed, YAML is clean (no tabs/duplicate keys/on: issues), placeholder rename is complete (8/8 substitutions, 0 leftovers) and safe in sed/markdown."
}
