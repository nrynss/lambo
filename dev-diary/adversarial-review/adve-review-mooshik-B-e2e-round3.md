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

**Verdict: REQUEST_CHANGES**: no P1, no P2, **1 P3**. All six round-2 closures are
genuine: every one verified at the artifact under mutation (eight mutations of my own,
eleven distinct red outcomes, each at its intended pin), the R2-1 fix verified by
running the real script eleven ways including the paths the fix could most plausibly
have broken, and the R2-2 planner pin ruled acceptable engineering with the flagged
autovacuum risk measured to be structurally impossible at fixture size. The one
finding is a residual in the R2-5 closure's own neighbourhood: the `://`-anchored
redaction leaves two operator-plausible DSN shapes printing their password under
"(passwords stripped)". **This cycle does not reach zero residue**; the decision line
is at the end of this file.

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

### B-E2E-R2-2 (P3) — H3 diagnostic vs its own probe: **closed-verified, and the planner pin is ruled acceptable engineering**

The rewritten comment (`src/store/sqlite.rs:6401-6424`) and `eprintln!`
(`sqlite.rs:6456-6470`) now state the measured mechanism — the grid seeds through
`flush()` and never `ANALYZE`s, `reltuples = -1`, the hnsw lane costed against a
fabricated estimate and taking the index over 22 real rows — and attribute the zero
envelope to `ef_search = 40 > n`, not to the plan. The hnsw lane's value is asserted
(`sqlite.rs:6447`), not narrated.

**Mutation M-R3-2** — `ANALYZE concepts` injected into `index_present` before the plan
probe: `h3_postgres_recall_parity` **RED at `src/store/sqlite.rs:6447`**, and the
failure message is the one the closure promised — it names the mechanism ("the fixture
corpus now carries real statistics … the planner costs 22 rows honestly and picks a Seq
Scan") and tells the next person to rewrite the prose rather than delete the pin. Only
the intended test went red (6 passed / 1 failed); the envelope and EXPLAIN tests were
unaffected. Reverted; live 7/0. The mutation is also the measurement that confirms the
comment: with fabricated estimates the index is taken, with real statistics it is not —
the same before/after the remediation's own temporary probe reported.

**The judgment the brief asks for: acceptable engineering, not a latent flake.** Four
measured reasons. (1) The assertion runs only in the live suite against the
digest-pinned image (CI's service container and this review's container are
byte-identical pins), so the cost model that makes the choice is frozen. (2) The one
nondeterminism the remediation conceded — "an autovacuum that beats the probe" —
cannot occur at fixture size: measured on the pinned image,
`autovacuum_analyze_threshold = 50` and `autovacuum_analyze_scale_factor = 0.1`, and a
22-row table never accumulates 50 changes, so autoanalyze is structurally unreachable
there, not merely unlikely. (3) `hnsw.ef_search` measured at its default 40 on the
image, above both corpora, so the envelope-is-zero claim is pinned by configuration,
not by plan. (4) The only path to red is a deliberate act (a digest bump, or an
`ANALYZE` added to the seed path), and M-R3-2 proves the failure message correctly
routes that person to the prose rather than the pin. A planner-dependent assertion
against an unpinned image would be a latent flake; this one is a checked claim about a
pinned artifact, the same epistemic shape as the pinned suite counts.

### B-E2E-R2-3 (P3) — zero-norm guard on SQLite's query path: **closed-verified, no over-rejection**

The precondition is extracted as `store::vector::ensure_is_an_embedding`
(`src/store/vector.rs:75-105`) and called by SQLite's `vector_candidates_checked` at
`src/store/sqlite.rs:1257` — after the limit checks, before the transaction opens,
which is where the pg family encodes its probe. The refusal criterion is
`norm_sq <= 0.0 || is_nan()` accumulated in f32, i.e. exactly the arithmetic pgvector
and Cockroach would do.

**Mutations (three, all reverted green):**

* **M-R3-3a** — the call severed from `vector_candidates_checked`:
  `vector_candidates_refuse_a_zero_norm_probe` **RED at `sqlite.rs:3472`** with
  `called unwrap_err() on an Ok value: [Scored { item: …, score: 0.0 }]` — the silent
  meaningless ranking, verbatim.
* **M-R3-3b** — the zero-norm branch disabled in the shared guard (`if false &&`):
  **both** pins RED (`encode_refuses_a_zero_norm_embedding` at `vector.rs:148` and the
  SQLite pin at `sqlite.rs:3472`) — one guard, three adapters, demonstrated.
* **M-R3-3c** — the call moved below the contract-row read: RED on the
  **unknown-session leg** (`Ok value: []` — an empty list instead of a refusal), the
  seeded leg still refusing; the pin holds the guard's *position*, exactly as the
  remediation claimed.

**Over-rejection, attacked directly** (a temporary probe test, reverted after
measurement): unit vectors at dims 1/4/768/1536 pass; `[1e-6, 0, 0, 0]` passes (also
pinned in-tree through the legacy entry point); `[1e-20, 0, 0, 0]` passes
(`norm_sq = 1e-40` is subnormal but nonzero in f32); `[f32::MIN_POSITIVE, 1.0]` passes;
negative-only components pass; the empty slice is *not* refused here (dim-0 stays
`check_embedding_dim`'s case, as documented). The only refusal beyond exact zero is a
norm whose square underflows f32 to 0.0 (`[1e-25, …]`) — which is precisely the vector
pgvector would score `NaN` and Cockroach `0.5`, so refusing it there is the fix's
stated point, not over-rejection. Both full fixture suites are green (battery below),
so nothing legitimate anywhere in H1/H3 trips it.

**Coverage boundary checked**: `rank_by_cosine` has exactly one production caller
(`sqlite.rs:1311`), now behind the guard; SQLite's legacy `vector_candidates` funnels
into the checked path (`sqlite.rs:1214`); the pg family's legacy path returns empty for
an unknown session *before* touching the probe (`pg/mod.rs:2750-2753`), which is the
same order SQLite has, so the two legacy surfaces agree with each other too. The memory
store advertises no VECTOR_SEARCH and refuses wholesale (`StoreError::Capability`), so
no silent adapter path remains.

### B-E2E-R2-5 (P3) — unparseable DSN printed its password: **closed-verified for every filed shape** (a residual neighbour is B-E2E-R3-1 below)

`canonical_store_dsn`'s fallback is now `redact_unparseable_dsn`
(`src/store/dsn.rs:82-97`): after `strip_libpq_password_token`, anything between the
first `:` of the userinfo and the **last** `@` after `://` is spliced out. All four
filed shapes (port past u16, non-numeric bracketed port, `postgre://`, port 99999 with
query) verified refused-with-redaction by the in-tree pin, and the last-`@` rule's
deliberate over-redaction confirmed by probe (`postgre://app:pw@h:26257/db?opt=x@y` →
`postgre://app@y` — ugly, safe direction). A password containing colons redacts
correctly (first-`:` split).

**Mutation M-R3-5** — fallback restored to `strip_libpq_password_token(trimmed)`:
`an_unparseable_dsn_still_has_its_password_stripped` **RED at `src/store/dsn.rs:347`**,
printing `postgres://app:S3cretHunter@127.0.0.1:70000/lambo` — the leak, verbatim.
Reverted, green.

**Live CLI probe** (debug binary, container on 55434): `store.dsn` with port 70000 +
`LAMBO_POSTGRES_DSN` at the container → rc 1, "the config file says
postgres://app@127.0.0.1:70000/lambo and LAMBO_POSTGRES_DSN says
postgres://lambo@127.0.0.1:55434/livetest (passwords stripped)". Password gone, typo
visible. The round-2 transcript's defect is closed.

### B-E2E-R2-6 (P3) — doc warnings and the dropped doc gate: **closed-verified**

Both links named in plain text (`src/store/mod.rs:885-891` and `mod.rs:944-946`), the
F7 pattern. Measured `cargo doc --no-deps --document-private-items` at
`store-cockroach,fixtures` = **53 warnings**, at the full
`store-postgres,store-cockroach,store-sqlite,fixtures` set = **53** (counted from the
"generated 53 warnings" summary; a naive `grep -c '^warning'` says 54 because it counts
the summary line itself — that is not drift). `CYCLE.md` now carries the doc row at
**both** feature sets, with the standing sentence "The doc row is standing and may not
be dropped" and the history of why.

**Mutations** — the remediation claimed "54 with either link restored", so both were
tried separately: **M-R3-6** (the `overlay_env` → `canonical_store_dsn` link restored)
→ 54, naming the private item; **M-R3-6b** (the `Dialect::DSN_ENV` link restored) →
54, "no item named `dialect` in module `pg`". Each reverted; 53 both times after.

### The macOS bash gate (round-2 note, addressed at the site): **verified**

`(( BASH_VERSINFO[0] < 4 ))` at `scripts/provision.sh:172-185`, placed after the
`--check` exit. Probes P9a/P9b above: under `/bin/bash` 3.2.57, `--check` runs clean
(rc 0, both read-only statements dialled), and the full arm refuses rc 1 with
"Nothing has been sent to the cluster" and an **empty dial log** — the refusal lands
before `SET CLUSTER SETTING`, which is the whole point. Statement order under bash 5
confirmed: the gate sits before the first `run_sql` of the DDL arm.

---

## New findings

**B-E2E-R3-1 (P3): a DSN with userinfo but no `://` — scheme dropped, or the
one-keystroke `postgres:/` typo — still prints its password under "(passwords
stripped)", because the R2-5 redaction is anchored on `://`.**

*Claim tested*: `redact_unparseable_dsn`'s doc says the canonical form must be safe to
print "on **every** input, including the ones neither parser understands"
(`src/store/dsn.rs:12-15`), and `overlay_env`'s refusal prints both canonical forms
under "(passwords stripped)" (`src/store/mod.rs:963-974`).

*Evidence*: `redact_unparseable_dsn` (`dsn.rs:82-97`) returns the input unredacted when
`find("://")` fails, and both parsers refuse scheme-less input
(`parse_postgres_url` requires the `postgres://`/`postgresql://` prefix,
`dsn.rs:129-137`; `parse_libpq_kv` requires `=`). Measured by probe (temporary test,
reverted) and then demonstrated live through the CLI against the pinned container:

```
store.dsn = "postgres:/app:S3cretHunter@127.0.0.1:26257/lambo"   # single-slash typo
LAMBO_POSTGRES_DSN = postgres://lambo:lambo@127.0.0.1:55434/livetest
→ rc 1: "the config file says postgres:/app:S3cretHunter@127.0.0.1:26257/lambo and
   LAMBO_POSTGRES_DSN says postgres://lambo@127.0.0.1:55434/livetest (passwords stripped)"
```

`app:S3cretHunter@127.0.0.1:26257/lambo` (scheme dropped entirely) leaks identically.
Both are the same one-typo class R2-5 was graded on: `postgre://` is a misspelled
scheme, `postgres:/` is a missing keystroke in the same token. A secondary consequence,
no observable leak but a false doc claim: `store_identity` (`src/mcp/endpoint.rs:667`)
promises "The password never appears in the string that is hashed", and on these two
shapes it now does (hashed and never published, so consequence-free today — but the
sentence is load-bearing for anyone auditing what can reach the lease row).

*Failure scenario*: identical to R2-5's — the refusal prints a live credential to
stderr on the line that promises it did not, and error text is the one place J2's
hashing was built to keep passwords out of. The input is one dropped keystroke away
from the shapes the closure handles.

*What closes it*: extend the splice to a scheme-less authority — when there is no
`://` but the string carries a `:` inside a `userinfo@` prefix before any `/`, drop
`:…` up to the `@` — or take the round-2 finding's own option B and return a
placeholder (`<unparseable dsn>`) for any input the redactor cannot positively make
safe. Either way, extend `an_unparseable_dsn_still_has_its_password_stripped` with the
two shapes above so the promise is pinned at its edge, and re-check the
`store_identity` sentence while there.

