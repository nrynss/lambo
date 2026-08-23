# J3 round-3 adversarial review — brief for the reviewer (REVIEW-ONLY, no edits)

You are the independent adversarial reviewer for the J3 durability redesign's round 3. You
are REVIEW-ONLY: verify and report. Do NOT implement, do NOT fix, do NOT "compare and
adopt" — the operator explicitly removed that; if something is wrong or missing, you call it
in your verdict and leave the fix to a remediation round. The only thing you may write is
your review document.

## Scope

The remediation round on branch `wt/j3`, commits `160858b..9b4c456` pushed to
`origin/wt/j3` (6 commits: `160858b` R-1 rule table, `4c290b1` R-1 bound+R-2+R-4+R-5+R-8,
`0f25f60` R-3 column preflight, `a5fd56a` R-5/R-7 source docs, `69719a9` R-2/R-6/R-7
surfaces + round-3 dispositions, `9b4c456` R-9 evidence driver). Repo root:
`/home/nryn/work/lambo`. HEAD is now `9b4c456`, working tree clean apart from a stray
untracked `local:/` file (not part of the round).

## Sources (read in this order)

1. `dev-diary/adversarial-review/adve-review-mooshik-J3-redesign-round2.md` — the round-2
   verdict and its nine findings (1 P1 / 3 P2 / 5 P3) are your checklist. Each finding has a
   prescribed remediation.
2. `dev-diary/lambo-for-mooshik/J3-durability-redesign.md` §"Prescribed design for
   J3-R2R-1" — the prescribed P1 fix (the Pāṇini rule table: no wildcard default,
   unclassified is its own named class, class decided at the adapter, a termination measure
   on the LOOP not the write's survival). Also its round-3 "as built" additions (commit
   69719a9 wrote the dispositions).
3. The remediation's own report: `agent://J3Remediation` (full JSON) — treat every claim as
   unverified until you check it at source, exactly as round 2 did.

## Verify, independently, at source

For EACH of the nine findings (J3-R2R-1..R-9), check the claimed fix at the file/line the
remediation names, not at its test or commit message. Record CLOSED / PARTIAL / not-closed
with evidence. This is a checklist, not a trust exercise.

### Priority verifications (the operator's explicit concerns)

- **The suggested algorithm is implemented** — the migration from a hard-coded
  `break after k` to a *sequential decision rule* (SPRT-family: "how many consecutive
  transient failures before blaming the embedder vs the content"). Verify in `writeq.rs`
  (commit 4c290b1): (a) a content-class rejection is the absorbing consume-and-break case
  and is NEVER counted as embedding-sickness evidence; (b) a transient streak is the
  sickness evidence, reset on any success; (c) the loop terminates with the remaining
  backlog left DURABLE (not consumed) once the threshold crosses; (d) the threshold and its
  error posture (burn bound / false-alarm tolerance) are STATED in the code or the design
  doc's as-built section, not an unexplained constant. If it is still a bare unexplained
  `k=3`, that is a PARTIAL/not-closed on R-1 — say so plainly.
- **The rule table** (`bge_m3.rs`, commit 160858b): NO `_ =>` arm may produce `Backend`;
  `unclassified` must map conservatively (durability preserved) and log the status; class
  decided at the adapter where the status is known; `is_transient` needs no re-derivation
  from a message string. Confirm the exact status→class mapping (transient vs content vs
  permanent-config vs unclassified) matches the prescribed table.
- **R-3 against real Cockroach** — verify the `store-cockroach` gate claim (557+5+2,
  `LAMBO_COCKROACH_DSN` + `LAMBO_REQUIRE_LIVE=1` from `.env`) and the live column-preflight
  test. The DSN is on this machine (reachable). You may re-run the gate. Never print or
  commit the DSN. If you cannot reproduce the live Cockroach runs, say so.
- **R-8** `write_queue_replay_blocked` — set in the liveness-gate return AND both
  breaking arms; an operator can distinguish draining from wedged.

### Gate reconciliation (do not take the remediation's gate table at face value)

Round 2 measured the sqlite gate at 1000/0/3 at `bc28ac8`. The remediation reports
**996 passed / 3 failed** (`serve_proxy_multi_client` trio) and claims pre-existing
(verified failing on clean HEAD with changes stashed). Resolve this: read the three tests,
check what they bind (ports/serves), and check whether the running environment breaks them —
note there are TWO live `serve` processes on this machine (the dogfood HTTP serve I started
on 127.0.0.1:7700, pid 118400, and an Antigravity stdio serve) that may occupy ports. Decide
whether the failures are genuinely environment/port-based and J3-independent, or a J3
regression. Evidence, not assertion.

Two more gates were NOT run by the remediation and MUST be either run or definitively
blocked by you:

- **clippy ×4** (default; `store-sqlite,fixtures`; `ship,fixtures`;
  `--no-default-features store-cockroach,embed-fixture`) — never ran (budget). Run it.
- **`bash scripts/observability/verify.sh`** — the remediation says blocked because
  `scripts/observability/warnings.py` shadows stdlib `warnings` under Python 3.14
  (`AttributeError: module 'warnings' has no attribute 'warn'`). Determine whether this is
  genuinely pre-existing/environmental (check the script and the Python) and whether it can
  be made to run without touching the repo; if it genuinely cannot pass on this machine, say
  so and note the branch has never passed a verify.sh on this rig.

Also confirm `cargo fmt --all -- --check` is clean and reconcile the fixtures gate
(912 vs round 2's 908 — the +4 should be the new tests; name them).

### Live-embedder note (R-6)

The remediation dropped the PROBE_TEXT measured numbers (allowed fallback) because it says
the BGE-M3 at 127.0.0.1:8080 is not running. Check whether it is in fact up (curl
127.0.0.1:8080/health); if it is running, note that the weaker drop-numbers option was
chosen even though re-measure-and-stamp was available — a PARTIAL-if-live, not a blocker.
Do not start it if absent.

## Output

Write your review as
`dev-diary/adversarial-review/adve-review-mooshik-J3-redesign-round3.md`, matching the
round-2 format (Method / findings table / any new findings / gate results / verdict
APPROVE | REQUEST_CHANGES, under the zero-residue rule). Verification-only edits: the repo
tree must be left clean apart from your new review doc and the pre-existing untracked
`local:/` file. Do NOT commit your review (the operator/orchestrator handles J3 doc commit
as part of the round).

## Report back (concise, evidence-first)

- Per finding J3-R2R-N: CLOSED / PARTIAL / not-closed + file:line evidence.
- The algorithm question: IS the sequential-decision termination measure implemented with a
  stated threshold + error posture? YES/NO + evidence.
- Gate reconciliation verdicts: clippy ×4 result, verify.sh outcome, the 3 serve_proxy
  failures (J3-caused vs environment), fixtures +4 name-set, fmt, store-cockroach/live.
- Any new findings (grade them).
- Overall verdict (APPROVE / REQUEST_CHANGES) with the findings that must close before
  integration.
