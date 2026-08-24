# Workstream B, end-to-end remediation round 2 (2026-08-24)

**Against**: `adve-review-mooshik-B-e2e-round2.md`, REQUEST_CHANGES, 6 findings
(1 P2 / 5 P3). Round 2 verified all ten of round 1's closures as genuine, so none
of them was re-opened or re-done here.
**Result**: all six closed. All gates green, doc gate included and now standing.
**No round 3 review has run.**

## Provenance and environment, stated plainly

This round ran on the MacBook, like the round-2 review and unlike everything before
it. No `.env` exists in the worktree, and `LAMBO_COCKROACH_DSN`, `DATABASE_URL` and
`LAMBO_POSTGRES_DSN` were all unset before anything ran (verified with `printenv`).
The production Cockroach cluster was never contactable from here, let alone contacted.

The default colima VM's data disk is still ext4-read-only from the disk-full incident
the review recorded, and recovering it needs a VM reboot that would disturb the running
`docs-telemetry` container. So this round did what the review did: a second colima
profile (`br2`, 2 CPU / 4 GiB / 20 GiB) carrying one container, `lambo-b-e2e-r2r`,
`pgvector/pgvector` at digest `sha256:cf134a76...760f8e6f`, byte-identical to the pin in
this file and in the `postgres-live` job, PostgreSQL 17.11, host port 55433. The global
docker context was returned to `colima` immediately after the profile came up and every
container command was issued with an explicit `--context colima-br2`. The profile and the
container were removed at the end of this round; `colima list` shows `default` alone
again.

**One thing to record rather than gloss.** No command in this round was issued against
the default profile, but the default VM restarted partway through it (guest uptime says
about 07:45 local, while this round was running its test battery on the host). Cause not
established here. Two consequences, both checked: `docs-telemetry` came back on its own
`unless-stopped` policy with exit code 0, no OOM, and is serving (HTTP 200 on 127.0.0.1:7788,
ingest cycles completing in its log); and the read-only ext4 condition the round-2 review
worked around is **gone**, with `/var/lib/docker` now mounted `rw`. The second colima
profile was still the right call at the time it was made, and the next round may not need
one.

Work happened in the worktree `/Users/narayan/Documents/work/lambo/.claude/worktrees/agent-a23163b4d9e42e181`
on branch `worktree-agent-a23163b4d9e42e181`, reset to `origin/lambo-for-mooshik` @ `c7a822f`
before starting. The main checkout was never written. Commits are incremental, one per
finding, because two agents on this workstream have already died mid-run.

## Per-finding closure

| Finding | Closed | What changed | Reverting it breaks |
| --- | --- | --- | --- |
| **B-E2E-R2-1** (P2) `provision.sh` sources `.env` over the pushed DSN | yes | The script captures `LAMBO_COCKROACH_DSN` before `source .env` and restores it after, with the precedence rule and its reasoning written at the site: an explicitly provided environment beats an ambient dotfile. New test `provision_script_prefers_the_pushed_dsn_over_dotenv` executes the real script against a decoy `.env` and a stub `docker`, and reads the DSN off the command line the script dialled | the new test, both halves: restore the `source .env` ordering and the pushed half fails; delete the `source .env` block and the dotfile half fails |
| **B-E2E-R2-2** (P3) H3 diagnostic contradicts its own probe | yes | Comment and `eprintln!` rewritten to the measured mechanism (no `ANALYZE` in the fixture seed path, `reltuples = -1`, the hnsw lane costed against a fabricated estimate and taking the index over 22 real rows), with the zero envelope attributed to `ef_search = 40` rather than to the plan. The hnsw lane's value is now **asserted**, not narrated | `assert!(indexed("postgres-hnsw"))` in `h3_postgres_recall_parity` (live) |
| **B-E2E-R2-3** (P3) zero-norm guard misses SQLite's query path | yes | The codec's precondition is extracted as `store::vector::ensure_is_an_embedding` and called by SQLite's `vector_candidates_checked` at the same point in the sequence where the pg family encodes its probe: after the limit checks, before the store is read. Non-finite probes covered too, since that is the other half of what encoding was enforcing | `vector_candidates_refuse_a_zero_norm_probe` (offline, `store-sqlite`), and the shared branch is still pinned by `encode_refuses_a_zero_norm_embedding` |
| **B-E2E-R2-4** (P3) record contradicts itself about parking | yes | Documentation only, three places. `FUTURE.md` gains "Park and fail over on a lost lease" as the feature's **owner**; its "B ships park-and-fail-over" clause now says B records the ruling and defers the implementation; `B-postgres-store.md` item 4 carries an in-place correction (ruled, not built) pointing at the decline, and the decline section names the owner. The operator's ruling itself is not reworded | n/a, documentation. Consistency check: no document in `lambo-for-mooshik/` now claims B ships it |
| **B-E2E-R2-5** (P3) unparseable DSN prints its password | yes | `canonical_store_dsn`'s fallback is `redact_unparseable_dsn`, which drops the userinfo password before returning. Blunt on purpose (last `@` wins, over-redacting a DSN with an `@` in its path), because a string we could not parse is a string we cannot reason about | `an_unparseable_dsn_still_has_its_password_stripped`, plus the live CLI transcript below |
| **B-E2E-R2-6** (P3) two new doc warnings, doc gate dropped from the table | yes | Both links named in plain text the way the F7 fix did it, and the doc gate restored to `CYCLE.md`'s standing table in **both** feature sets with a sentence saying the row may not be dropped | `cargo doc --no-deps --document-private-items` warning count: 53 now, 54 with either link restored |
| macOS note (not a finding) | addressed at the site | The DDL arms are gated on `(( BASH_VERSINFO[0] >= 4 ))`, placed **after** the `--check` early exit so read-only inspection stays bash-3.2-clean, and refusing before the first statement rather than dying at `route_statement` after `SET CLUSTER SETTING` has gone to the cluster | verified by hand under `/bin/bash` 3.2: rc 1, nothing dialled |

## B-E2E-R2-1, the design decision

Two things were wrong, and only one of them was the script.

**The precedence rule.** `lambo provision` resolves `store.dsn` and pushes it into the
child; the script then sourced `.env` over it. The fix could have been "source `.env`
only when the variable is unset", or "pass the DSN as an argument", or what was chosen:
capture before sourcing, restore after. That keeps `.env` sourced for everything else it
carries, keeps the bare `./scripts/provision.sh` path working exactly as before, and
states one rule that already holds one layer up. The config layer's Level B rule is that
the file is the single construction site and the environment may supply but not silently
replace; the script's rule is the same shape one door down: an explicit environment beats
an ambient dotfile, and when the two differ the script says so on stderr without printing
either DSN.

**The test gap, which is the reason this survived a P1 closure.** The round-1 pin asserted
on `Command::get_envs`, the sending end. Nobody ran the receiving end, and a pin on one
end of a pipe is not a pin on the pipe. The new test copies the real script into a scratch
tree, plants a decoy `.env` beside it, puts a recording stub `docker` first on PATH, and
builds the command through `provision_command` itself, so one test now covers both ends.
It asserts the dotfile half as well, because "the secret lives only in `.env`" is a path
CI and existing deployments depend on and the obvious over-correction would break it.

## Live probes (this round's own runs, debug binary, container on 55433)

1. **The R2-1 demonstration, re-run with the fix.** Stub `docker` on PATH, probe `.env`
   carrying `postgres://dotenv-prod@prod.example:26257/prod`, pushed DSN
   `postgres://pushed@127.0.0.1:1/resolved`. `--check` dialled
   `psql postgres://pushed@127.0.0.1:1/resolved`, and so did the full DDL arm at every one
   of its four statements. The review's transcript dialled the `.env` value.
2. **The bash-3.2 arm.** `/bin/bash` (3.2.57) on the full arm: rc 1, "provisioning needs
   bash 4 or newer ... Nothing has been sent to the cluster", printed **before**
   `SET CLUSTER SETTING`. `--check` under the same shell still works.
3. **The R2-5 refusal, re-run with the fix.** `store.dsn = postgres://app:S3cretHunter@127.0.0.1:70000/lambo`
   (port past u16), `LAMBO_POSTGRES_DSN` at the container: rc 1, and the message now reads
   "the config file says postgres://app@127.0.0.1:70000/lambo and LAMBO_POSTGRES_DSN says
   postgres://lambo@127.0.0.1:55433/livetest (passwords stripped)". The password is gone
   and the typo the operator has to fix is still visible.
4. **The R2-2 mechanism, measured rather than inferred.** A temporary probe inside
   `resolve_index_present` printed the hnsw lane's plan before and after an
   `ANALYZE concepts` on the same 22-row fixture corpus:
   `Index Scan using concepts_embedding_idx (rows=209)` before, `Seq Scan (rows=22)`
   after, with `pg_class.reltuples` going -1 to 22. The probe was reverted; what it
   established is now the comment.

## Mutations run

| # | Mutation | Expected red | Observed | Reverted |
|---|---|---|---|---|
| M-R2-1a | `INHERITED_DSN` restore block deleted from `provision.sh` (the pre-fix ordering) | the new receiving-end test | `provision_script_prefers_the_pushed_dsn_over_dotenv` RED: the script dialled `postgres://dotenv-decoy@127.0.0.1:1/dotenv` | yes, green after |
| M-R2-1b | `source .env` block deleted | the same test's dotfile half | RED at `provision.rs:406`, script exited 1 with "LAMBO_COCKROACH_DSN is not set" | yes, green after |
| M-R2-2 | `ANALYZE concepts` run before the plan probe | the new hnsw index_present assertion | `h3_postgres_recall_parity` RED at `sqlite.rs:6454`, with the message telling the reader to rewrite the prose rather than delete the pin | yes, live 7/0 after |
| M-R2-3a | `ensure_is_an_embedding` call deleted from SQLite's `vector_candidates_checked` | the new probe pin | `vector_candidates_refuse_a_zero_norm_probe` RED: the call succeeded and returned ranked candidates, which is the defect verbatim | yes, green after |
| M-R2-3b | the same call moved below the contract read | the pin's unknown-session leg | RED: the seeded leg still refused, the unknown session returned an empty list | yes, green after |
| M-R2-3c | zero-norm branch disabled in the shared guard (`if false &&`) | codec pin **and** the new SQLite pin | BOTH RED (`encode_refuses_a_zero_norm_embedding`, `vector_candidates_refuse_a_zero_norm_probe`), which is what "one guard, three adapters" is supposed to mean | yes, green after |
| M-R2-5 | fallback restored to `strip_libpq_password_token(trimmed)` | the redaction pin | `an_unparseable_dsn_still_has_its_password_stripped` RED at `dsn.rs:347`, printing `postgres://app:S3cretHunter@127.0.0.1:70000/lambo` | yes, green after |
| M-R2-6 | one private-item doc link restored in `store/mod.rs` | the doc gate | `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` went 53 to **54**, naming `overlay_env` again | yes, 53 after |

Eight mutations, eleven distinct red outcomes, every one at its intended pin. B-E2E-R2-4
is documentation with no code behind it and was checked by re-reading the corrected
documents instead: no file under `lambo-for-mooshik/` now says B ships park-and-fail-over,
and both the spec's item 4 and its decline section name the FUTURE entry that owns it.

## Gates, all run on the finished tree

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | pass |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | pass |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | pass |
| `cargo test --features store-cockroach` | 950 / 0 / 4 (round 2 review measured 947) |
| `cargo test --features store-cockroach,fixtures` | 1010 / 0 / 12 (was 1007) |
| `cargo test --features store-postgres` | 937 / 0 / 7 (was 934) |
| `cargo test --features store-postgres,fixtures` | 994 / 0 / 7 (was 991) |
| `cargo test --features store-sqlite,fixtures` | 1076 / 0 / 3 (was 1072) |
| `cargo test --no-default-features --features store-cockroach` | 613 / 0 / 0 (was 610) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | **53 warnings** (was 55) |
| `cargo doc ... --features store-postgres,store-cockroach,store-sqlite,fixtures` | **53 warnings** (was 55) |
| live Postgres `-- --ignored` against the pinned container | 7 / 0 in 20.2 s |

Suite numbers are the sum across each invocation's test binaries. The +3 everywhere is
this round's three always-compiled tests; `store-sqlite,fixtures` is +4 because the
zero-norm probe pin needs that adapter. `CYCLE.md`'s standing table carries the same
numbers, with the doc row restored.

## Still not verified

* **The live Cockroach leg.** Still unrun, and this round touched `scripts/provision.sh`
  again, so the risk on that leg is not lower than the review left it. What is different
  is that the script's DSN handling now has an executing test rather than a pin on the
  sending end, and that the bash-4 gate turns the one macOS failure mode into a refusal
  before the first statement. The 15 `#[ignore]`d live Cockroach tests remain on the
  orchestrator, and nothing here was run against a real Cockroach cluster.
* **`postgres-live` on a real runner.** Still never exercised there. The new
  `provision_script_prefers_the_pushed_dsn_over_dotenv` is `#[cfg(unix)]` and spawns
  `bash` plus a stub executable, which is new behaviour for the CI matrix rows; it passes
  locally in every one of the six combinations, and ubuntu runners have `bash`, but no
  push has proved it.
* **The R2-2 assertion is planner-dependent.** `assert!(indexed("postgres-hnsw"))` pins a
  choice pgvector's costing makes at `reltuples = -1` on the pinned digest. That is the
  point (it makes the printed sentence a checked claim), but a digest bump or an
  autovacuum that beats the probe would turn it red. The message says so and tells the
  next person to rewrite the prose rather than delete the pin.
* **Park and fail over is still unimplemented.** R2-4 closed the bookkeeping, not the
  feature. B's behaviour on a lost lease is unchanged: the loser refuses. The ruling now
  has an owner in `FUTURE.md` and no phase claims it.
* **No round 3 review.** Six closures are self-reported by the party that made them, with
  eight mutations as evidence. Round 2 is what caught round 1's `provision.sh` hole, so
  this is a real gap rather than a formality.
