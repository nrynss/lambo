# Adversarial review: mooshik B, whole workstream E2E, round 2 (verification of the round-1 remediation)

> Written as an append-as-you-go log, committed after each settled finding, because
> both prior agents on this workstream lost work to mid-run deaths. This is the final
> state; nothing below is pending.

**Reviewer**: independent E2E round-2 reviewer (Fable), worktree branch
`worktree-agent-a5ce01ade532a3145` at
`/Users/narayan/Documents/work/lambo/.claude/worktrees/agent-a5ce01ade532a3145`,
reset to `origin/lambo-for-mooshik` @ `51419fe` before starting. The main checkout was
not touched. Every probe mutation is applied in this worktree and reverted; the
cleanliness section at the end records the final `git status`.

**Machine and environment, stated because two things about this run are unusual.**

1. This round ran on the MacBook, not the Linux box where B was developed. No `.env`
   exists here and neither `LAMBO_COCKROACH_DSN` nor `DATABASE_URL` is in the
   environment, verified before anything ran. The production Cockroach cluster was
   never contactable from this machine, let alone contacted.
2. The host disk filled and was cleared shortly before this run. Casualties: the default
   colima VM's data disk took a write I/O error, its ext4 journal aborted, and the
   filesystem is now read-only, so the default docker context cannot create containers
   (recovering it needs a VM reboot, which would disturb the running `docs-telemetry`
   container and was therefore not done). The pgvector container for this review runs
   on a second colima profile (`colima-b-r2`) instead: `lambo-b-e2e-r2`,
   `pgvector/pgvector:pg17`, digest `sha256:cf134a76…0f8e6f`, byte-identical to the
   digest pinned in `b-run/CYCLE.md` and in the `postgres-live` job, PostgreSQL 17.11,
   host port 55433. All gate numbers in this file were measured after the disk was
   cleared; nothing from before the incident is quoted.

**Verdict: REQUEST_CHANGES**: no P1, **1 P2, 5 P3**. The remediation is genuine: all
ten closures verified at the artifact, every falsifier fired exactly where the closure
table said it would (nine mutations, fourteen distinct red gate outcomes, all at their
intended pins), the declined F11 is ruled a legitimate decline, and one closure (F3)
is stronger than its own description. The findings below are residuals in the
remediation's own material, plus one hole in F2's `provision.sh` leg that the closure's
marker test cannot see because it pins the wrong end of the pipe: the config layer now
refuses correctly on every verb, and the script then un-resolves the DSN for the one
verb E2E-1 was originally filed about.

---

## Round-1 findings, one line each

| # | Status | Falsifier run, result |
|---|---|---|
| F1 (P1) CI red on merged tree | **closed-verified** | `run_fixture_grid` now takes the `FixtureGrid` struct (`sqlite.rs:5868`) rather than 8 positional parameters. All seven clippy rows green in my battery, including the two that were red at `d3bea88`. M-LINT (below) proves the lint class still fires in exactly those rows if an 8-arg function returns. |
| F2 (P1) env DSN outranks `store.dsn` | **closed at the config layer, defective at the `provision.sh` leg** | Refusal disabled (`if false &&` at `mod.rs:960`) → `env_dsn_naming_a_different_database_is_refused_not_preferred` RED at `mod.rs:1611`; reverted, green. Cross-kind leak restored (`Postgres => Some(COCKROACH_DSN_ENV)`) → `each_kind_reads_its_own_dsn_env_var` RED (`mod.rs:1663`, left `crdb-dsn` right `pg-dsn`) AND the dialect's own drift pin `dsn_env_named_in_errors_is_the_one_config_reads` RED (`postgres.rs:648`); reverted, green. Live CLI probes (six, section below): all behaved as claimed. **But see B-E2E-R2-1: the script leg re-opens E2E-1.** |
| F3 (P2) H3 not self-verifying | **closed-verified, stronger than claimed** | M3 (`forced_exact_scan_sql() -> None`) → THREE live tests red, not the claimed two: `explain_recall_uses_hnsw` (forced-exact lane planned `Index Scan using concepts_embedding_idx`), `h3_postgres_hnsw_envelope_at_scale` (red at `sqlite.rs:6583`, same plan text), and `h3_postgres_recall_parity` (red at `sqlite.rs:6360`, "the forced-exact lane must never plan through concepts_embedding_idx"). Each died at its intended assertion, reached through a real `EXPLAIN` probe: no compile error, no panic on `None` (`issue_forced_exact_scan` no-ops). Round 1 measured this same mutation leaving the parity test GREEN; it is now red at the probe, which is exactly E2E-F3a's fix working. Offline: `distance_to_score_is_one_minus_d` RED (pins `forced_exact_scan_sql()` at `postgres.rs:488`). All reverted, all green after (7/0 live). Residual prose defect → B-E2E-R2-2. |
| F4 (P2) EXPLAIN on empty table | **closed-verified** | Corpus shrunk to 100 rows (below the measured 500-row crossover) → `explain_recall_uses_hnsw` RED at `postgres.rs:1008`, "the planner must CHOOSE concepts_embedding_idx … with no GUC helping it". Reverted, green. The test now also asserts 5 finite distances from a corpus-drawn probe, so the round-1 NaN-probe vacuity is closed too. |
| F5 (P3) width check skips reader attach | **closed-verified** | `preflight_schema()` call severed in `load_reader_graph_with_contract` (`cli/mod.rs`) → `reader_verbs_refuse_an_unprovisioned_store_by_name` RED at `cli/mod.rs:187`, failing with the exact raw-error shape the fix replaced ("no such table: sessions" instead of the actionable `lambo provision` refusal). Reverted, green. Live leg: `preflight_schema` is exercised live in `live_schema_width_refuses_a_config_that_disagrees` and `init_schema_at_two_widths_creates_hnsw` (both green on my container); the funnel-to-preflight composition is pinned offline. |
| F6 (P3) `provision --help` omits Postgres | **closed-verified** | "Postgres" deleted from the about string at `main.rs:195` → `provision_help_names_every_store_kind_it_provisions` RED at `main.rs:932` ("provision --help must name Postgres"). Reverted, green. |
| F7 (P3) private-item doc link | **closed-verified, and the same class reintroduced one module over** | Measured: `cargo doc --no-deps --document-private-items` at `store-cockroach,fixtures` = 55 warnings, at the full `store-postgres,store-cockroach,store-sqlite,fixtures` set = 55: the `pg/postgres.rs:103` warning is gone and the pg module adds zero. But the round-1 baseline was 54, and the +1 is the remediation's own F2 doc text → B-E2E-R2-6. |
| F8 (P3) no CI row lints `store-postgres` test code | **closed-verified** | Both clippy invocations present in the `postgres` matrix row of `.github/workflows/ci.yml`; the `index_present` dead-code allow is scoped `cfg_attr(not(feature = "store-sqlite"), allow(dead_code))` at `postgres.rs:353`, not blanket. Lint probe M-LINT: an 8-argument fn planted inside the H3 harness module turned BOTH rows red (`too_many_arguments`, the E2E-F1 class), so the rows genuinely lint the module that was invisible to every round-1 CI row. Reverted, both green. |
| F9 (P3) unit-norm contract unenforced | **closed-verified, coverage claim overstated** | Guard disabled (`if false &&` around the zero-norm branch in `vector.rs`) → `encode_refuses_a_zero_norm_embedding` RED at `vector.rs:119`. Reverted, green. The guard sits in the shared codec and covers every WRITE path on all three sqlx adapters, and the QUERY path on the pg family (`pg/mod.rs:2775` encodes the probe). It does not cover SQLite's query path → B-E2E-R2-3. |
| F10 (P3) CYCLE.md counts stale | **closed-verified** | Every suite count in the replaced `CYCLE.md` table reconciles exactly with my measured runs as the sum across the suite's test binaries (lib + bins + integration + doctests): 947/0/4, 1007/0/12, 934/0/7, 991/0/7, 1072/0/3, 610/0/0, live 7/0. Zero drift. Note for the next round: the numbers are per-invocation sums, not lib-only counts; the round-1 review quoted lib-only "listed" numbers, so the two tables are consistent but not directly comparable. |
| F11 (P3) park-and-fail-over unimplemented | **declined; decline ruled legitimate, record inconsistent** | See "Judging the declined finding" and B-E2E-R2-4. |
| E2E-1 / E2E-2 | merged into F2 per round 1's ruling | The `overlay_env` half is verified above. The `provision.sh` half is B-E2E-R2-1. |

---

## New findings

**B-E2E-R2-1 (P2): `scripts/provision.sh` sources `.env` over the pushed resolved DSN, so F2's `provision.sh` leg is undone by the exact file that caused E2E-1.**
*Claim tested*: the F2 closure says "resolved DSN pushed into `provision.sh`" and the spec's new DSN-precedence section says "the Cockroach arm now pushes the resolved DSN into the child's environment rather than letting it inherit the ambient one, so the config names the cluster the DDL lands on."
*Evidence*: `provision_command` (`src/cli/provision.rs:100-107`) does set `LAMBO_COCKROACH_DSN` on the child. But `scripts/provision.sh:16-21` then runs `set -a; source .env; set +a` from the repo root before reading `DSN="${LAMBO_COCKROACH_DSN:-}"` at line 23, and a sourced assignment overwrites the inherited environment. Demonstrated live in this worktree with a stub `docker` on PATH and a probe `.env`:

```
$ LAMBO_COCKROACH_DSN='postgres://env-pushed-resolved@127.0.0.1:1/resolved' \
    bash scripts/provision.sh --check        # .env: LAMBO_COCKROACH_DSN=postgres://dotenv-overrides@…/dotenv
DOCKER-STUB dial: run --rm -i postgres:16-alpine psql postgres://dotenv-overrides@127.0.0.1:1/dotenv …
```

The script dialed the `.env` DSN, not the pushed one. (Probe `.env` deleted afterwards; it is gitignored and was never committed.)
*Failure scenario*: on the machine where B is developed, `.env` at the repo root carries the production Cockroach DSN. An operator whose shell has NOT exported that file (the binary does not load `.env`; nothing forces the shell to) writes `kind = "cockroach"` with `store.dsn` naming a local container. `overlay_env` sees no environment DSN, so there is nothing to refuse against; the resolved local DSN is pushed into the child; the script sources `.env` and the DDL lands on the production cluster while `lambo provision` reports success against a config that named a container. That is E2E-1 verbatim, surviving one door down from where it was closed. When the shell env DOES carry the production DSN the config-layer refusal fires first, which is why the remediation's live 4-way verification could not see this: every probe it ran had the variable in the process environment.
*Why the closure's own falsifier missed it*: `cockroach_provision_hands_the_resolved_dsn_to_the_script` (`provision.rs:286`) asserts on `Command::get_envs`, the sending end. Nobody ran the receiving end. A pin on the wrong end of a pipe is the same class as F3's hardcoded `index_present`.
*What closes it*: in `provision.sh`, capture `PUSHED="${LAMBO_COCKROACH_DSN:-}"` before the `source .env` block and prefer it afterwards (or source `.env` only when the variable is unset, or pass the DSN as an argument). Plus one test that executes the script head with a stub and a decoy `.env`, so the receiving end is pinned.

**B-E2E-R2-2 (P3): `h3_postgres_recall_parity`'s plan-probe diagnostic contradicts its own measurement, and the comment above it states the wrong mechanism.**
*Evidence*: on the clean tree the passing run prints, in one line:
`H3 fixture-grid plan probe: postgres-hnsw index_present=true, postgres-exact index_present=false. The fixture corpora are 9 and 22 rows, below the planner's crossover, so NEITHER lane uses the index here and the envelope below is structurally zero.`
The probe says `true`; the prose in the same sentence says NEITHER. The comment at `sqlite.rs:6337-6344` ("At THIS corpus size the honest answer is `false` for both") is likewise contradicted by the value printed beside it. Mechanism: `corpus::seed` runs `ANALYZE`, but the fixture grid seeds through `flush()` and never analyzes, so the planner is costing against default estimates (`reltuples = -1`), not against "9 rows", and it picks the index scan however small the table really is. The 500-row crossover constant was measured after `ANALYZE`; the fixture grid runs before any.
*Why it still passes and why the envelope is still zero*: with `ef_search = 40 > n`, hnsw over 9 or 22 rows returns the exact answer whichever plan serves it, so the zero envelope is real; but it is real for the ef_search reason, not the stated plan reason. And the load-bearing assertion (`!indexed("postgres-exact")`) is unaffected: the forced-exact lane's GUC turns the index off regardless of estimates, which is also why M3 now kills this test.
*Failure scenario*: the next person reading the evidence trusts the sentence, concludes the hnsw lane is un-indexed at fixture size, and re-litigates E2E-F3b against a test that no longer has that defect; or worse, "fixes" the probe to match the prose.
*What closes it*: make the eprintln print what it measured and say why the envelope is zero anyway (ef_search covers the corpus), and correct the comment. One honest sentence, no behavior change.

**B-E2E-R2-3 (P3): the F9 zero-norm guard does not cover SQLite's (or the memory oracle's) query path, so the cross-adapter divergence F9 was filed about still exists for probes.**
*Claim tested*: the closure says "zero-norm refused in the shared codec, where both dialects and SQLite pass through", and the guard's own message says "Refusing to write or query it".
*Evidence*: the pg family encodes the probe (`pg/mod.rs:2775`), so a zero-norm probe is refused on Postgres and Cockroach. SQLite's `vector_candidates_checked` (`sqlite.rs:1238-1302`) never encodes the probe: it hands it to `rank_by_cosine`, and `crate::embed::cosine` (`embed/math.rs:21`) guards the denominator with `.max(1e-12)`, so a zero probe silently scores every row `0.0` and returns candidates in tie-break order. Write paths are covered on all three (`concept_binds` at `sqlite.rs:2308`, `pg/mod.rs:1640`).
*Failure scenario*: an embedder that violates its unit-norm contract at query time (the same broken embedder F9 postulated at write time) gets a loud refusal on a Postgres deployment and a silent, meaningless-but-plausible ranking on a SQLite deployment: one input, two behaviors, which is the exact sentence F9 was filed under. H3 cannot see it because its probes are unit by construction.
*What closes it*: refuse a zero-norm probe at the shared entry (`vector_candidates_checked` callers already share `check_embedding_dim`-style plumbing), or document the SQLite behavior beside the guard and drop "or query" from the shared-codec claim.

**B-E2E-R2-4 (P3): the F11 decline is legitimate, but the tree now contradicts itself about what B does when a writer loses.**
*The judgment asked for*: declining to IMPLEMENT park-and-fail-over inside a remediation round is ruled **legitimate**. The reasoning recorded in `B-postgres-store.md` ("Deliberately not built", dated 2026-08-24) is sound: it is a writer-availability feature of real size, not a defect in anything B built, and shipping a new lease behavior with none of the review the rest of B received would be worse than deferring it. No Done-when box claims it, so the verdict does not gate on it.
*But the record is now inconsistent in two places*:
1. `FUTURE.md` ("Cross-host proxying", line 54-56) says the proxying decision was made "alongside the ruling that B **ships** park-and-fail-over instead". B ships no such thing: the loser refuses (round 1 demonstrated it live, and nothing in the remediation touched `serve`'s conflict path). A reader of FUTURE.md walks away believing parking exists.
2. `B-postgres-store.md` item 4 of "What workstream J left in B's path" still states "**The ruling: the losing machine's writer parks rather than refusing**" with "the work is small", present tense, with no pointer to the decline section 200 lines below. The two sections are both dated and both authoritative-looking; only one is true of the artifact.
Between them, park-and-fail-over now has no owner: B's spec says declined, FUTURE.md says B shipped it, and no FUTURE entry or phase carries it forward. The operator ruling of 2026-08-23 is thereby left with no implementation anywhere on the map.
*What closes it*: annotate item 4 in place pointing at the decline (the J-workstream's in-place-correction pattern), fix FUTURE.md's "B ships" clause to "B records the ruling and defers the implementation", and give park-and-fail-over an owner: its own FUTURE entry or a named phase.

**B-E2E-R2-5 (P3): a DSN the canonicaliser cannot parse is returned with its password intact, and the refusal message then prints it while asserting "passwords stripped".**
*Evidence (static, then demonstrated live below)*: `canonical_store_dsn` (`src/store/dsn.rs:49-61`) falls through to `strip_libpq_password_token(trimmed)` when `parse_postgres_url` fails. That fallback strips only whitespace-separated `password=` tokens, so a URL-shaped DSN survives verbatim, embedded `user:secret@` included. `parse_postgres_url` fails on inputs an operator can plausibly produce: a port above 65535 or a non-numeric bracketed port (`split_host_port` returns `None` at `dsn.rs:159-166`), or a typo'd scheme (`postgre://`). `overlay_env`'s refusal then interpolates both canonical forms into an error that says "(passwords stripped)" (`mod.rs:961-970`).
*Failure scenario*: `store.dsn = postgres://app:S3cret@db:70000/lambo` (fat-fingered port) plus any different `DATABASE_URL` → the refusal prints `S3cret` to stderr, in a message that claims it did not, and error text is the one place J2's hashing was built to keep passwords out of.
*What closes it*: on parse failure, strip `://user:…@` userinfo with a regex-free splice before returning, or return a placeholder (`<unparseable dsn>`) in `to_identity`-failure position; either way the refusal keeps its promise on every input.
*Live transcript*: `store.dsn = postgres://app:S3cretHunter@127.0.0.1:70000/lambo` (port past u16), `LAMBO_POSTGRES_DSN` at the container → rc 1 with: "the config file says postgres://app:S3cretHunter@127.0.0.1:70000/lambo and LAMBO_POSTGRES_DSN says postgres://lambo@127.0.0.1:55433/livetest (passwords stripped)". The password is on the line that promises it is not.

**B-E2E-R2-6 (P3): the remediation introduced two doc warnings of the class F7 closed, and dropped the doc gate from its own gate table.**
*Evidence*: `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` measures **55** warnings where round 1 measured 54. Both new sites are in the F2 doc text the remediation wrote: `src/store/mod.rs:888` carries an unresolved link (`crate::store::pg::dialect::Dialect::DSN_ENV`: "no item named `dialect` in module `pg`", the module is private and the path wrong), and `src/store/mod.rs:943` has public `overlay_env` documentation linking the private item `crate::store::dsn::canonical_store_dsn`, which is byte-for-byte the class E2E-F7 was filed about. The full-feature count is also 55 (the `pg/postgres.rs:103` warning is genuinely gone; the pg module itself is clean).
*Why it happened*: the remediation's gate table (`B-e2e-remediation-round1.md`) has no doc row. B0-R1-2 added that gate precisely because it is the only one that sees private-item doc rot, and the round that closed a doc-rot finding did not run the doc gate over its own edits.
*What closes it*: fix the two links (name them plainly, as the F7 fix itself did at `postgres.rs:102-105`), and restore the doc gate to the standing table in `CYCLE.md` so the next round cannot drop it silently.

## F2 verified live, six ways (this reviewer's own runs, debug binary, container on 55433)

1. **Conflict refuses**: file `…55433/lambo`, `LAMBO_POSTGRES_DSN=…55433/decoy` → rc 1, both canonical forms printed, `lambo:lambo` credentials stripped to `lambo@`, variable named, remedy stated. (Also note: the refusal fires in config resolution BEFORE the compiled-feature check, so even a binary without `store-postgres` refuses rather than resolving.)
2. **Cross-kind leak closed** (the exact round-1 P1 reproduction): `kind = "postgres"`, file DSN at `livetest`, `LAMBO_COCKROACH_DSN` at `decoy` → provision rc 0 against `livetest`; `decoy` measured after: **0 tables**.
3. **Identity, not spelling**: file `postgresql://…?sslmode=disable` vs env `postgres://…?connect_timeout=10&application_name=alt&sslmode=disable` (same database) → no refusal, provision rc 0.
4. **Secret path intact**: no `store.dsn`, `LAMBO_POSTGRES_DSN` supplies it → provision rc 0.
5. **Cross-form identity**: file DSN in libpq `key=value` form (`host=… port=55433 dbname=livetest user=lambo password=…`) vs env URL form of the same database → no refusal, provision rc 0. The canonicaliser folds both syntaxes to one identity.
6. **Malformed DSN**: see B-E2E-R2-5's live transcript: parse failure falls back to the raw string, password included, printed under "(passwords stripped)".

F5 verified live through the CLI as well: `stats --session` with `vector_dim = 1536` against the initialized `vector(768)` database → **rc 1**, "live schema width is vector(768) but this process constructed at dim 1536". Round 1 measured rc 0 with a normal snapshot on this exact shape.

## Judging the declined finding (F11)

Covered by B-E2E-R2-4 above: **the decline stands, the bookkeeping does not.** B does
not claim park-and-fail-over in any Done-when box, the decline is recorded under a
dated heading with real reasoning, and implementing a lease-behavior change inside a
remediation round would have been the greater sin. What does not stand is FUTURE.md
asserting B ships it and the spec's item 4 still stating the ruling as if it were
built. State changed by this round: none (documentation finding).

## The three "Still not verified" items

* **Live Cockroach leg**: still unrun, and correctly so. This machine has no
  `LAMBO_COCKROACH_DSN`, no `.env`, and the brief forbids touching the production
  cluster. What this round adds: B-E2E-R2-1 means the Cockroach `provision` path has a
  live defect on `.env`-bearing machines, so the risk on that leg is HIGHER than the
  remediation's "carries more risk after this round than before it" already conceded.
  The 15 `#[ignore]`d live Cockroach tests remain on the orchestrator.
* **`postgres-live` on a real runner**: still never exercised there. I inspected the
  job definition for runner-only failure modes: the service container is digest-pinned
  and matches my local digest byte for byte; health-check and port mapping are
  standard; `fixtures = ["store-memory"]` in Cargo.toml means the two H3 steps'
  `--no-default-features --features store-postgres,store-sqlite,fixtures` combos do
  compile the harness (I verified the feature graph, and the same combos build and run
  locally); the `--exact` multi-name filters are valid libtest usage; the grep guards
  match the `... ok` lines my local runs produce, including the two H3 grep lines with
  module-qualified paths. Nothing found that would fail only on a GitHub runner.
  Remaining risk is runtime (the envelope test's hnsw build at dim 768) and it is
  bounded: 19.1 s for the whole live set on this laptop.
* **F11**: see above.

---

## Mutations run

| # | Mutation | Expected red | Observed | Reverted |
|---|---|---|---|---|
| M3 | `PostgresDialect::forced_exact_scan_sql() -> None` | claimed: both H3 tests; plus offline `distance_to_score_is_one_minus_d` | offline pin RED (`postgres.rs:488` asserts the GUC string); live: `explain_recall_uses_hnsw` RED, `h3_postgres_hnsw_envelope_at_scale` RED, `h3_postgres_recall_parity` RED, each at its own forced-exact assertion, all through real EXPLAIN probes | yes; 7/0 live after revert |
| M-F4 | explain corpus 2000 → 100 rows | natural-plan assertion | RED at `postgres.rs:1008` | yes |
| M-F2a | overlay refusal disabled (`if false &&`) | DSN-precedence pin | `env_dsn_naming_a_different_database_is_refused_not_preferred` RED at `mod.rs:1611` | yes |
| M-F2b | `StoreKind::Postgres.dsn_env()` → `COCKROACH_DSN_ENV` | kind-ownership pins | `each_kind_reads_its_own_dsn_env_var` RED (`mod.rs:1663`) and `dsn_env_named_in_errors_is_the_one_config_reads` RED (`postgres.rs:648`) | yes |
| M-F5 | reader `preflight_schema()` severed | reader-refusal pin | `reader_verbs_refuse_an_unprovisioned_store_by_name` RED at `cli/mod.rs:187`, raw sqlite error shape visible | yes |
| M-F9 | zero-norm branch disabled | codec pin | `encode_refuses_a_zero_norm_embedding` RED at `vector.rs:119` | yes |
| M-F6 | "Postgres" dropped from the provision about string | help pin | `provision_help_names_every_store_kind_it_provisions` RED at `main.rs:932` | yes |
| M-F2c | `cmd.env` push severed in `provision_command` | marker test | `cockroach_provision_hands_the_resolved_dsn_to_the_script` RED at `provision.rs:298` ("the resolved DSN must be pushed into the child: []") | yes |
| M-LINT | 8-arg fn planted in `h1_cross_store_parity` | both new clippy rows | `clippy --features store-sqlite,fixtures` RED and `clippy --no-default-features --features store-postgres,store-sqlite,fixtures` RED, both `too_many_arguments (8/7)` | yes |

Live baseline before any mutation: `cargo test --features
store-postgres,store-sqlite,store-memory,fixtures --lib -- --ignored` with
`LAMBO_POSTGRES_DSN` at the container: **7 passed / 0 failed in 19.1 s** (6 Postgres
live tests + the bge smoke), matching the claimed 7/0.

## Platform notes (macOS leg, exercised here for the first time)

This is the first time any of B has run on macOS: B was developed on the Linux box and
CI is ubuntu. What was exercised here and held: the full offline suites (battery table
below), all 7 live tests against the pinned container (the digest is identical to the
Linux/CI pin, so the "same pinned image, one claim" property really does extend to this
machine), the CLI probes above, and both doc gates. Two portability observations, one
of them load-bearing:

1. **`scripts/provision.sh` requires bash 4+** (`${var,,}` at lines 120 and 180). macOS
   ships bash 3.2 at `/bin/bash`; the script is reached via `Command::new("bash")`,
   which resolves from PATH, so on this machine (Homebrew bash 5.3 first in PATH) it
   works. On a stock Mac without Homebrew bash, the Cockroach provision arm dies with
   "bad substitution" at the first `route_statement` call: AFTER `SET CLUSTER SETTING`
   has already been issued to the cluster, but loudly (non-zero exit propagated by
   `cli::provision::run`). Fails loud, not wrong, so a note rather than a finding; worth
   a `(( BASH_VERSINFO[0] >= 4 ))` guard at the top if Macs ever run that arm in anger.
2. The `--check` arm of the script and everything else it uses (`tr`, `mktemp`,
   `grep -E`) is BSD-clean.

The B-E2E-R2-1 demonstration also ran here: the `.env`-override behaviour is plain
bash semantics (`set -a; source .env` after inheriting the variable), identical on
both platforms; nothing about it is macOS-specific.

## Gate table: claimed vs measured

Claimed = the remediation report / post-remediation `CYCLE.md` table. Measured = this
worktree at `51419fe`, after every probe mutation was reverted, all on this machine
after the disk incident. Suite numbers are the sum across each invocation's test
binaries, which is what the claimed table records.

| Gate | Claimed | Measured |
|---|---|---|
| `cargo fmt --all -- --check` | pass | **pass** |
| `cargo clippy --all-targets -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | pass (was red) | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | pass (was red) | **pass** |
| `cargo test --features store-cockroach` | 947 / 0 / 4 | **947 / 0 / 4** (lib 934/0/2) |
| `cargo test --features store-cockroach,fixtures` | 1007 / 0 / 12 | **1007 / 0 / 12** (lib 993/0/10) |
| `cargo test --features store-postgres` | 934 / 0 / 7 | **934 / 0 / 7** (lib 921/0/5) |
| `cargo test --features store-postgres,fixtures` | 991 / 0 / 7 | **991 / 0 / 7** (lib 977/0/5) |
| `cargo test --features store-sqlite,fixtures` | 1072 / 0 / 3 | **1072 / 0 / 3** (lib 1032/0/1) |
| `cargo test --no-default-features --features store-cockroach` | 610 / 0 / 0 | **610 / 0 / 0** (lib 600/0/0) |
| live Postgres `-- --ignored` (pinned container) | 7 / 0 | **7 / 0**, 19.0 s (run three times across the review: baseline, post-M3-revert, battery; 7/0 each time) |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | **absent from the remediation's table** (round 1: 54) | **55 warnings** → B-E2E-R2-6 |
| `cargo doc … --features store-postgres,store-cockroach,store-sqlite,fixtures` | absent (round 1: 55, one in `pg/postgres.rs:103`) | **55 warnings**, the pg warning gone, the two new ones in `store/mod.rs` |
| 15 `#[ignore]`d live Cockroach tests | not run (no safe DSN) | **not run**: no DSN on this machine, production forbidden. Still on the orchestrator. |

## Summary

| Grade | Count | Findings |
|---|---:|---|
| P1 | 0 | |
| P2 | 1 | B-E2E-R2-1 (`provision.sh` sources `.env` over the pushed resolved DSN: E2E-1's verb re-opened on `.env`-bearing machines) |
| P3 | 5 | B-E2E-R2-2 (H3 diagnostic contradicts its own probe), B-E2E-R2-3 (zero-norm guard misses SQLite's query path), B-E2E-R2-4 (F11 decline recorded, FUTURE.md and spec item 4 still claim parking), B-E2E-R2-5 (unparseable DSN prints its password under "passwords stripped"), B-E2E-R2-6 (two new doc warnings of the F7 class; doc gate dropped from the gate table) |

Under the operator's standing rule that a remediation round closes the P3s too, these
gate the next pass, not the design. The load-bearing verdicts of round 1 all hold at
HEAD: the distance conversion is pinned four ways and survives no mutation I ran, the
fencing token has live evidence that runs on every push, the hnsw envelope is now
measured where hnsw actually approximates (and honestly does not bound it), the
`EXPLAIN` box now tests the planner's judgement, the config file selects the database
on every in-process verb, and an operator pointing `lambo.toml` at a stock pgvector
container gets a working store whose identity, width, and index are all checked
against the live schema. What is still not true is that the Cockroach `provision`
script obeys the resolved config on a machine with a `.env`, and that is the one
place the original P1's failure mode can still occur.

## Cleanliness

Nine probe mutations applied, every one reverted; `git status --porcelain` empty
(besides this file's own commits) verified after each revert and at review end. Final
live suite re-run green (7/0) on the reverted tree via the battery. The probe `.env`
created for the B-E2E-R2-1 demonstration was deleted the same minute (it is also
gitignored) and `git status` verified clean after. The stub `docker`, probe configs,
and battery scripts live in the session scratchpad outside the repo. The
`lambo-b-e2e-r2` container and the `colima-b-r2` profile were removed at review end;
`docs-telemetry` on the default profile was never touched (verified up before and
after). The main checkout at `/Users/narayan/Documents/work/lambo` was never written.



