# Adversarial review — remed-T10 (round 2, final clearance)

**Scope:** final round-2 clearance re-review of the T10 worktree (detached HEAD; cwd `/home/nryn/work/worktrees/remed-T10`). Round-1 was APPROVE (2 P3 — both Main-isolated doc items; 4 nits). This round confirms the three targeted nits (N1, N3, N4) were remediated correctly/minimally with no provisioning-logic regression. Read-only; exactly this file written.
**Method:**
- Full `git diff scripts/aws-infra/provision_app_data.py` read (+31/−15); `ensure_lambda` read in context.
- Round-1 review (`adve-review-remed-T10round1.md`) re-read for the two P3s and the four nits.
- `python3 -m py_compile scripts/aws-infra/provision_app_data.py` → OK.
- Sibling repo (`/home/nryn/work/lambo`) checked for contamination; worktree diff scope confirmed.

**Verdict: APPROVE.**
**Disposition: APPROVE** — clean and integration-ready. All three targeted nits closed; no P1/P2/P3/nit remains in the worktree.

---

## Nits closed (verified, minimal, no regression)

### N1 — `_add_perm` return value now aggregated into the deploy note ✅
`provision_app_data.py:426-434`: the two `_add_perm(...)` calls are collected into `perm_statuses`, and a single final `note` aggregates `"{created} created, {existing} already present"`. The return value (`"created"`/`"existing"`) is now consumed as requested.
- **Correct:** the aggregated note is emitted right after both calls; semantics clear.
- **Minimal:** only the note plumbing changed; no new abstraction.
- **No provisioning-logic regression:** both statements are still emitted, byte-compatible with the resolved live state:
  - `AllowPublicFunctionUrl` → `lambda:InvokeFunctionUrl` + `FunctionUrlAuthType="NONE"`
  - `AllowPublicFunctionUrlInvoke` → `lambda:InvokeFunction` + `InvokedViaFunctionUrl=True`
- **Idempotent:** `_add_perm` still catches `ResourceConflictException` → returns `"existing"` instead of failing; distinct statement ids mean the two calls never collide. Re-provision against an already-deployed function is a clean no-op.

### N3 — statement id confirmed-named, no rename ✅
`AllowPublicFunctionUrlInvoke` retained unchanged. Em-dash-free (AGENTS.md rule satisfied), consistent and descriptive. The round-1 note explicitly recorded this as cosmetic-only with no required action; the "confirmed, no rename" disposition is honored. No action taken, correctly.

### N4 — subjective comment reworded objectively ✅
The subjective "This is the single most commonly missed step." is gone. `provision_app_data.py:404-408` now states objective, verifiable fact: "Since October 2025, public function URLs require BOTH lambda:InvokeFunctionUrl AND lambda:InvokeFunction; the helper below emits both, because omitting the second yields a 403 Forbidden on the URL even though the first statement looks correct." Factual, grounded in the determined root cause, no unverifiable claim.

---

## Pending-Main items (NOT worktree defects)

Both P3s from round 1 are integration-time doc edits owned by Main, out of scope for this worktree (neither target file is modified in the worktree):
- **T10-R1-1 (P3):** `dev-diary/notes/remediation-tasks.md` T10 section (779-804) — still "undiagnosed" + the ruled-out account-level hypothesis at :799; needs the DONE block (root cause + live fix + §11 decision) like T8/T9. **Pending-Main.**
- **T10-R1-2 (P3):** `dev-diary/notes/deployment-and-submission.md:116` — obsolete "describe as IAM-invoked" §11 guidance, superseded since the public URL returns 200. **Pending-Main.**

Round-1 **N2** (ruling-out framing should lead with the decisive positive fix rather than "no such API exists") is a doc-framing note that ties into T10-R1-1's DONE-block wording; it is likewise deferred to Main's integration-time doc update, not a worktree code defect.

---

## Worktree health

- **py_compile:** OK.
- **Diff scope:** working tree holds exactly one modified file, `scripts/aws-infra/provision_app_data.py` (+31/−15); untracked item is only the round-1 review doc (plus this round-2 doc). No other files touched.
- **Sibling empty:** `/home/nryn/work/lambo` `git status --porcelain` → empty; no path-safety contamination. The earlier mis-targeted edit was fully reverted and only the worktree carries the patch.
- **No worktree P1/P2/P3/nit remains.**

---

## Summary

The three targeted nits (N1 aggregation, N3 confirmed-name, N4 objective reword) are all remediated correctly, minimally, and without any regression to the provisioning logic — both resource-policy statements are still emitted by `_add_perm` (idempotent via `ResourceConflictException`), and the code is comment-reworded but otherwise unchanged. The two P3s and the N2 framing note are Main's integration-time doc edits (remediation-tasks.md T10 section; deployment-and-submission.md §11), not worktree defects. `py_compile` passes; the diff is confined to `provision_app_data.py`; the sibling is clean. The worktree is clean and integration-ready.

---

{ verdict: "APPROVE", findings: { P1: [], P2: [], P3: [ "pending-Main (not worktree): T10-R1-1 remediation-tasks.md T10 section 779-804 DONE block w/ root cause + live fix + §11 decision; N2 framing folds in", "pending-Main (not worktree): T10-R1-2 deployment-and-submission.md:116 obsolete IAM-invoked §11 guidance" ], nits: [ "closed N1: _add_perm return aggregated into final note", "pending-Main N2: ruling-out framing in doc DONE-block wording", "closed N3: statement id confirmed-named, no rename", "closed N4: subjective comment reworded objectively" ] }, summary: "Round-1 APPROVE confirmed at round 2. All three targeted nits (N1/N3/N4) remediated correctly, minimally, no provisioning-logic regression; both statements still emitted, idempotent. Two P3s + N2 framing are Main's integration-time doc edits, not worktree defects. py_compile OK; diff confined to provision_app_data.py; sibling clean. Worktree clean and integration-ready." }
