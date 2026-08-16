# Adversarial review — remed-T10 (round 1)

**Scope:** round-1 adversarial review of the T10 worktree (detached HEAD; sole working-tree change is `scripts/aws-infra/provision_app_data.py`, +24/−15). T10 = diagnose the 43(0) Lambda Function URL 403, with (per the handoff) a root cause, a live fix (URL now 200), a §11-wording decision, and a provisioning-script patch. Read-only; exactly this file written.
**Method:**
- `git diff` (full) read; the T10 section of `dev-diary/notes/remediation-tasks.md` read.
- **Live re-verification (read-only, `AWS_PROFILE=lambo-user`, `us-east-1`):** `get-function-url-config`, `get-policy`, `get-function`, `get-account-settings`, and a `curl` of the public URL.
- `python3 -m py_compile scripts/aws-infra/provision_app_data.py` → OK.
- Sibling repo (`/home/nryn/work/lambo`) checked for path-safety contamination.
- AWS primary doc (`urls-auth.html`) consulted for the both-permissions requirement.

**Verdict: APPROVE.**
**Disposition: APPROVE** — diagnosis sound, live fix correct and verified, script patch correct/idempotent/durable, §11 honest.

---

## Root cause — verified sound

The T10 finding: since October 2025 a public (AuthType=NONE) Lambda Function URL requires **both** `lambda:InvokeFunctionUrl` **and** `lambda:InvokeFunction` in the resource-based policy; only the first was attached, so the URL 403d even though the policy "looked correct."

**Adversarial checks (all pass):**
1. **Documented behaviour — CONFIRMED.** AWS `urls-auth.html` states that, starting in October 2025, new function URLs require both permissions; missing either yields HTTP 403. The two documented statements are `lambda:InvokeFunctionUrl` (+ `FunctionUrlAuthType NONE` condition) and `lambda:InvokeFunction` (+ `lambda:InvokedViaFunctionUrl=true` condition). Both match the live policy exactly.
2. **Live policy — CONFIRMED.** `get-policy` now returns **both** statements:
   - `AllowPublicFunctionUrl`: `lambda:InvokeFunctionUrl`, `*`, condition `StringEquals lambda:FunctionUrlAuthType = NONE`.
   - `AllowPublicFunctionUrlInvoke`: `lambda:InvokeFunction`, `*`, condition `Bool lambda:InvokedViaFunctionUrl = true`.
3. **URL config — CONFIRMED.** `AuthType=NONE`, URL matches the claimed endpoint.
4. **Cutoff timing — CONFIRMED.** Function `LastModified` `2026-08-16T15:17:20Z`; URL created `2026-08-16`. Both after the Oct-2025 cutoff, consistent with the both-permissions requirement applying to this function URL.
5. **Account-level-block hypothesis — GENUINELY RULED OUT.** `get-account-settings` returns only `AccountLimit`/`AccountUsage` keys — no public-access-block field. `aws lambda get-public-access-block-config` fails with `ParamValidation: Invalid choice 'get-public-access-block-config'` in CLI 2.36.19 — no such operation exists. The original doc was right that it could not be "checked from here"; the ruling-out rests most decisively on the positive result: a **resource-policy-only** change flipped the URL to 200, which an account-level deny (had it existed) would not have permitted regardless of the policy. No SCP (account not in an Organization) and the Free→Paid move were already eliminated earlier. Diagnosis is sound.

## Live fix — correct, verified, reversible, non-destructive

Added resource-policy statement `AllowPublicFunctionUrlInvoke = { Effect: Allow, Principal: *, Action: lambda:InvokeFunction, Resource: <fn arn>, Condition: Bool lambda:InvokedViaFunctionUrl = true }`.

- **Verified 200.** `curl https://uwvhgfb2rothsct6pnl44edk3q0kazsl.lambda-url.us-east-1.on.aws/` → HTTP 200 with live JSON (`session: cloudops-exhibit`, `concepts: 41`, `canonical: 1`, `edges: 485`, `interactions: 72`).
- **Both statements now present** (see `get-policy` above).
- **Reversible.** The added statement is a discrete `add-permission`; `remove-permission --statement-id AllowPublicFunctionUrlInvoke` removes it cleanly. It touched only the resource policy — no change to function code, config, or URL config.
- **Least-privilege-appropriate.** The wildcard `lambda:InvokeFunction` is constrained by `lambda:InvokedViaFunctionUrl=true`, so it exposes nothing beyond the already-public URL (AuthType=NONE). The correct condition key for a public URL, matching the AWS-documented example.
- **Correctness judgment: correct.** Adding precisely the missing second permission (and only it) resolved the 403; nothing broader was granted.

## Script patch — correct, idempotent, durable

`ensure_lambda` now emits both statements via a small `_add_perm(statement_id, action, **extra)` helper (provision_app_data.py:409-428):

- **Both statements for a fresh deploy — CONFIRMED.** Calls at 426-427 emit `AllowPublicFunctionUrl` (`InvokeFunctionUrl` + `FunctionUrlAuthType=NONE`) and `AllowPublicFunctionUrlInvoke` (`InvokeFunction` + `InvokedViaFunctionUrl=True`), byte-compatible with the documented example and the resolved live state.
- **Idempotent / re-provision-safe — CONFIRMED.** `_add_perm` catches `ResourceConflictException` (the error `add-permission` raises on an existing statement id) and reports "already present" rather than failing. Distinct statement ids mean the two calls cannot collide with each other. Re-running against an already-deployed function with both statements present is a clean no-op for the permission step.
- **Durable — CONFIRMED.** The both-permissions requirement is now code, not a comment: every run (fresh or existing) emits both statements. A re-provision cannot re-break it. The inline comment (405-407) states the Oct-2025 requirement and the 403 symptom.
- **Condition correctness — CONFIRMED.** `InvokedViaFunctionUrl=True` (boto3) ⇒ `Bool lambda:InvokedViaFunctionUrl=true`, the standard condition for a public URL.
- **py_compile → OK.**
- **Path-safety incident — handled.** The worktree's working tree holds exactly the one modified file; the sibling repo (`/home/nryn/work/lambo`) is clean (`git status --porcelain` empty) and its `provision_app_data.py` still contains the original single-statement form — i.e. the initial mis-targeted edit was caught and fully reverted; only the worktree carries the patch. No contamination remains.

## §11 wording — honest (decision to claim a public endpoint is correct)

The plan's §11 (docs/plans/multi-agent-cloudops-aws-plan.md:412) reads: "**AWS Lambda** (Function URL) | A public read-only stats endpoint over the live CockroachDB session. Runs outside the VPC…" Every claim now verifies against the live endpoint:
- **public** — 200 to an unauthenticated curl; AuthType=NONE, CORS `*`.
- **read-only stats** — handler SELECT-only (per module comment), returns live counts.
- **over the live CockroachDB session** — JSON carries live `cloudops-exhibit` counters.
- **outside the VPC** — no `VpcConfig` (module comment "NO VpcConfig").

Since the URL now answers, keeping the "public endpoint" claim (rather than the alternative "IAM-invoked" downgrade that the earlier text contemplated) is the honest, correct choice. The wording matches the actual endpoint.

---

## Findings

### P1
None.

### P2
None.

### P3
- **T10-R1-1 (P3) — T10 task section not updated; asserts a now-known-false state.** `dev-diary/notes/remediation-tasks.md:779-804`. The `## T10` section still opens "returns 403, **undiagnosed**" and carries `:799` "**Untested hypothesis:** an account-level Lambda public-access block," which this work both concluded false and disproved by the working fix. Unlike T8/T9 (which each gained a `✅ … — DONE` block), T10's resolution — root cause, live fix, §11 decision — is recorded only in this review, not in the tracker. *Why:* a future reader of remediation-tasks.md would conclude T10 is still open and its block-hypothesis untested, contradicting the resolved, live-200 reality. *Fix:* mark the section done with the root cause + fix + §11 decision, and delete/qualify the untested-hypothesis line.
- **T10-R1-2 (P3) — now-obsolete §11 guidance in the deployment doc.** `dev-diary/notes/deployment-and-submission.md:116`: "…the Function URL 403s, and that is undiagnosed. §11 should describe it as **IAM-invoked** rather than claim a public endpoint that does not answer." That instruction is superseded: the public endpoint now answers (200) and §11's "public read-only stats endpoint" is truthful. *Why:* leaving it instructs a writer to misdescribe the now-public endpoint. *Fix:* update the line to reflect that the URL is live/public.

### Nits
- **T10-R1-N1 — `_add_perm` return value is discarded.** `provision_app_data.py:409-424`, calls at 426-427. It returns `"created"`/`"existing"` but no caller uses it. Either use the value (e.g. aggregate a status for the final note) or return `None`. Cosmetic.
- **T10-R1-N2 — ruling-out framing leans on a weak argument first.** The diagnosis cites "no such API exists in AWS CLI 2.36.19" as the reason the account-level-block hypothesis is false. Absence of a *guessed* API name is not, by itself, dispositive; the decisive evidence is the positive result — a resource-policy-only fix produced 200, which an account-level deny would have blocked. The fix is sound regardless; recommend stating that as the primary evidence wherever the ruling is recorded (ties into T10-R1-1).
- **T10-R1-N3 — statement id naming.** `AllowPublicFunctionUrlInvoke` is fine and em-dash-free (AGENTS.md rule satisfied — no em dashes in any new AWS name/description), but AWS's own docs name the paired id `PublicInvokeViaUrl`/`…ViaUrl`. Cosmetic only; both Sids are consistent and descriptive.
- **T10-R1-N4 — retained subjective claim.** `provision_app_data.py:408`, "This is the single most commonly missed step." inherited from the original comment; harmless, arguably still accurate, but unverifiable and not needed.

---

## Summary

The diagnosis is **sound**: the documented post-Oct-2025 requirement for public function URLs to carry **both** `lambda:InvokeFunctionUrl` and `lambda:InvokeFunction` is confirmed against AWS's own docs, the live policy now holds exactly those two statements in the documented form, the function/URL creation dates are after the cutoff, and the account-level-block hypothesis is genuinely ruled out (no such API/field exists, and a resource-policy-only change resolved it). The **live fix is correct**: adding precisely the missing second permission flipped the URL 403→200 (re-verified live this review), is reversible via `remove-permission`, and is non-destructive. The **script patch is correct, idempotent, and durable**: `_add_perm` emits the documented pair and safely absorbs `ResourceConflictException` on re-run, so a re-provision cannot re-break it; py_compile passes. The **§11 "public endpoint" wording is honest** and matches the verified live 200 endpoint. The path-safety incident (initial edit hitting the sibling) was caught and fully reverted — the sibling is clean and the worktree carries the sole change. No P1/P2; two P3 documentation-hygiene items (the T10 tracker section and the deployment doc's obsolete IAM-invoked instruction) and four nits. Recommend merge with the two P3s addressed at/around landing.

---

{ verdict: "APPROVE", findings: { P1: [], P2: [], P3: [ "T10-R1-1: remediation-tasks.md T10 section (779-804) still says 'undiagnosed' and carries the ruled-out 'Untested hypothesis: account-level block' at :799; not marked DONE — update with root cause + live fix + §11 decision.", "T10-R1-2: deployment-and-submission.md:116 still instructs §11 to describe the Lambda as IAM-invoked; now obsolete since the public URL returns 200 — update." ], nits: [ "T10-R1-N1: _add_perm return value discarded (provision_app_data.py:409-427).", "T10-R1-N2: ruling-out framing leads with 'no such API exists' rather than the decisive positive fix; state the latter as primary evidence.", "T10-R1-N3: statement id naming (AllowPublicFunctionUrlInvoke) cosmetically diverges from AWS docs' example naming; em-dash rule satisfied.", "T10-R1-N4: retained subjective 'single most commonly missed step' comment (provision_app_data.py:408)." ] }, summary: "Diagnosis sound (both-permissions requirement confirmed against AWS urls-auth.html; live policy holds both documented statements; function/URL created after the Oct-2025 cutoff; account-level-block hypothesis genuinely ruled out — no such API and a resource-policy-only fix produced the 200). Live fix correct and verified (precisely the missing lambda:InvokeFunction + InvokedViaFunctionUrl condition; 403->200 re-confirmed this review; reversible and non-destructive). Script patch correct + idempotent (ResourceConflictException-absorbed _add_perm, distinct Sids, both statements emitted on every run) + durable; py_compile passes. §11 'public endpoint' wording is honest and matches the verified live 200 endpoint. Path-safety incident was caught and fully reverted (sibling clean, worktree sole change). Two P3 documentation-hygiene items and four nits; no P1/P2. APPROVE." }
