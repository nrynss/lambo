# Adversarial review: mooshik B, whole workstream E2E, round 3 (verification of the round-2 remediation)

> Written as an append-as-you-go log, committed after each settled finding, because
> three agents on this workstream have now lost work to mid-run deaths. This is the
> final state; nothing below is pending.

**Reviewer**: independent E2E round-3 reviewer (Fable), worktree branch
`worktree-agent-a71840a776f495ea1` at
`/Users/narayan/Documents/work/lambo/.claude/worktrees/agent-a71840a776f495ea1`,
reset to `origin/lambo-for-mooshik` @ `84ad616` before starting. The main checkout was
not touched. Every probe mutation was applied in this worktree and reverted; the
cleanliness section at the end records the final `git status`.

**Scope**: the six round-2 closures (B-E2E-R2-1 … R2-6) plus whatever the remediation
newly introduced (the receiving-end script test, the macOS bash-4 gate, the shared
`ensure_is_an_embedding` precondition, `redact_unparseable_dsn`). Round 1's ten
closures were verified genuine under mutation by round 2 and are **not** re-verified
here, per the cycle protocol.

**Machine and environment.** MacBook, same as rounds 2 and 2R. No `.env` exists in the
worktree or the main checkout; `LAMBO_COCKROACH_DSN`, `LAMBO_POSTGRES_DSN` and
`DATABASE_URL` were all unset before anything ran (verified with `printenv`). The
production Cockroach cluster was never contactable from here. The default colima VM is
healthy again after the operator-initiated restart during round 2 (its ext4 is `rw`),
and `docs-telemetry` runs on it untouched; this round still took its own profile
(`b-r3`, 2 CPU / 4 GiB / 20 GiB, docker context `colima-b-r3`) carrying one container,
`lambo-b-e2e-r3`, `pgvector/pgvector:pg17` at digest
`sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f` —
byte-identical to the pin in `b-run/CYCLE.md` and the `postgres-live` job — PostgreSQL
17.11, host port **55434** (55433 left alone). Every container command was issued with
an explicit `--context colima-b-r3`; the global context stayed on `colima`. Profile and
container were removed at review end. The stale `colima-b-r2` disk was left for the
operator, as instructed.

**Verdict: (recorded at the end of this file, after the last settled finding)**

---

## The six round-2 closures, verified one by one

### B-E2E-R2-1 (P2) — `provision.sh` DSN precedence: **closed-verified**

The fix (`scripts/provision.sh:34-46`): `INHERITED_DSN` captured before the
`source .env` block, restored after it when the sourced value differs, with the
precedence rule written at the site and a note on stderr that deliberately prints
neither DSN. The receiving-end test
`provision_script_prefers_the_pushed_dsn_over_dotenv` (`src/cli/provision.rs:349-445`)
copies the real script into a scratch tree, plants a decoy `.env`, puts a recording
stub `docker` first on PATH, builds the command through `provision_command` itself,
and asserts both halves.

**Verified by running the real script**, not by reading it: eleven probes against the
byte-identical script copied to a scratch tree (so the worktree never gained a `.env`),
stub `docker` recording every dial, Homebrew bash 5.3 unless stated:

| Probe | Setup | Result |
|---|---|---|
| P1 | env-only DSN, **no `.env`** (the CI / existing-deployment path) | dials the env DSN, rc 0, **no note** — the capture/restore is invisible when there is nothing to restore |
| P2 | `.env`-only, nothing pushed (ambient source of last resort) | dials the `.env` DSN, rc 0 — the dotfile path still works, as it should |
| P3 | pushed DSN + different `.env` (the E2E-1 shape) | **dials the pushed DSN**, note printed, no DSN in the note |
| P4 | pushed DSN + identical `.env` | dials it, **no note** (nothing was overridden) |
| P5 | no DSN anywhere, no `.env` | rc 1, "LAMBO_COCKROACH_DSN is not set" |
| P6 | pushed DSN + CRLF `.env` | pushed DSN restored (the `\r`-bearing sourced value differs, so the restore fires), rc 0 |
| P7 | CRLF `.env`, nothing pushed | dials the `.env` DSN; pre-existing behaviour, unchanged by the fix |
| P8 | unreadable `.env` (mode 000) + pushed DSN | rc 1, loud "Permission denied" from `set -e` — pre-existing (`source .env` predates the fix), fails loud not wrong |
| P9a | `/bin/bash` 3.2.57, `--check` | rc 0, clean — `--check` stays bash-3.2-clean |
| P9b | `/bin/bash` 3.2.57, full DDL arm | rc 1, "provisioning needs bash 4 or newer … Nothing has been sent to the cluster", **dial log empty** — the gate refuses before `SET CLUSTER SETTING` |
| P10 | full arm under bash 5, leak check | the DSN appears on **neither stdout nor stderr** (`grep -c` = 0 both); it reaches only the `psql` argv, which is how `run_sql` has always worked. Statement order confirmed: SET CLUSTER SETTING → base tables → probes → final verify (which then failed loudly against the stub's empty answer — the script's own gate doing its job) |
| P11 | pushed **empty-string** DSN + `.env` present | `.env` supplies the DSN — the `-n` guard means an empty capture is "nothing inherited", so no empty-but-intended value is ever clobbered and no unset variable is ever restored as empty. (Empty also cannot be pushed: `provision_command` sets the variable only when the config resolved `Some(dsn)`.) |

The legitimate env-only path (P1) and the `.env`-as-last-resort path (P2) both survive
the fix, which is where a capture/restore most plausibly breaks something. The restore
re-exports a value that was already in the exported environment, so nothing new leaks
into later children (P10).

**Mutations** (each applied, run, reverted, re-run green):

* **M-R3-1a** — `scripts/provision.sh` reverted wholesale to its pre-fix version at
  `c7a822f` (the strongest form of "would the test pass vacuously if the script were
  reverted"). `provision_script_prefers_the_pushed_dsn_over_dotenv` **RED** at
  `src/cli/provision.rs:427` ("the script must dial the DSN `lambo provision` pushed"),
  and the captured argv in the failure output shows the script dialling
  `postgres://dotenv-decoy@127.0.0.1:1/dotenv` — the round-2 defect verbatim.
* **M-R3-1b** — only the `source .env` block deleted. The same test **RED** at
  `src/cli/provision.rs:416`, dotfile half: the script exited 1 with
  "LAMBO_COCKROACH_DSN is not set", which is exactly the "secret lives only in `.env`"
  path CI-adjacent deployments depend on. So the test pins both directions, not just
  the fix.
* Reverted; `provision_script_prefers_the_pushed_dsn_over_dotenv` green (1 passed).

**The new test machinery, judged for hermeticity** (the remediation itself flagged it
as new risk for the CI rows): the script is *copied* into the scratch tree, so `ROOT`
resolves there and the developer's real `.env` — which does not exist on this machine,
but would on the Linux box — is never read; the stub `docker` is found first on a
prefixed PATH, so a real docker is shadowed rather than required; the dotfile half
`env_remove`s the runner's own `LAMBO_COCKROACH_DSN` so a DSN in the test runner's
environment cannot stand in for a pushed one; the scratch root is `temp_dir()` +
pid + nanos, so parallel test binaries cannot collide; cwd is irrelevant because the
script `cd`s to its own `ROOT` and every path in the test is absolute. `#[cfg(unix)]`
for the stub's exec bit — the `postgres-live` ubuntu rows have bash and a unix fs, and
the suite passed in all six local feature combinations (battery below). Not vacuous:
M-R3-1a proves it detects the reverted script.

### B-E2E-R2-4 (P3) — the park-and-fail-over bookkeeping: **closed-verified**

Checked all three legs the closure claims, against the tree rather than the report:

1. **Nothing anywhere still claims B ships it.** Swept the whole repo (not just
   `lambo-for-mooshik/`) for `park`-family claims: `FUTURE.md:87-90` now reads
   "the losing writer parks and fails over **instead of refusing**. That ruling was
   made for B, and B **records** it rather than implementing it"; the old "B ships
   park-and-fail-over" clause is gone. The B1/B2/B3 implementation briefs say
   "Park-and-fail-over … is a B-wide ruling already recorded in FUTURE.md: do **not**
   build it" and list it under "Do not re-litigate" — consistent. Every other `park`
   hit in the repo (`docs/reference/mcp.mdx:328`, `docs/reference/cli.mdx:259`,
   `scripts/observability/README.md:366-374`, `J-multi-client.md`) is the *ledger
   writer* parking on a blocked `open` — a different mechanism, correctly worded, no
   lease claim anywhere near it.
2. **The operator's ruling is unaltered.** `B-postgres-store.md:166-179` inserts the
   correction as a dated blockquote *above* the ruling; the ruling's own words
   ("The ruling: the losing machine's writer parks rather than refusing", the 1-second
   trail arithmetic, "the work is small") are byte-identical to the pre-remediation
   text (checked against `c7a822f`). The blockquote reframes the tense
   ("an estimate of unstarted work, not a report of finished work") without rewording
   the ruling itself — the J-workstream in-place-correction pattern, applied correctly.
3. **The FUTURE entry is an owner, not a restatement.** `FUTURE.md:52-81` ("Park and
   fail over on a lost lease") carries the trigger (two machines, one shared store),
   the decided shape (park-and-retry loop, honest text naming the holder), what B
   actually ships today (the `HolderIsOnAnotherHost` refusal), the decline's reasoning
   and its ratification, sequencing ("its own phase when one is scheduled"), and the
   sentence "this entry is its owner". That is the same ownership shape the
   Cross-host-proxying entry already has, and it is exactly what round 2's
   "what closes it" asked for ("its own FUTURE entry or a named phase"). The spec's
   decline section (`B-postgres-store.md:452-458`) names the entry back, so the two
   documents point at each other instead of contradicting each other.

Documentation-only; no mutation applies. Checked by reading the corrected tree and by
sweeping for the claim-family, which is the sweep J's handoff guidance prescribes.

