# Adversarial review - mooshik L1 (hosted-tier gap fixes), round 2

**Reviewer**: independent adversarial reviewer, agent_id `L1Review2`. Wrote nothing under
review except this file.
**Scope**: the round-1 remediation, **uncommitted in the working tree** on
`lambo-for-mooshik` (9 files, +562/-51), against the ten findings of
`adve-review-mooshik-L1-round1.md` (REQUEST_CHANGES, 1 P1 / 3 P2 / 6 P3) and against the
commit it corrects, `9580873`.
**Worktree**: `/home/nryn/work/lambo` @ `9580873` plus the uncommitted remediation. The
five source files I mutated were backed up before the first mutation and checksum-verified
identical to those backups afterwards; `git status --short` at the end shows the same nine
modified files and two untracked files it showed at the start, plus this review.
**Verdict**: **APPROVE** - all ten closures verified, zero reopened, three new P3s, zero
gate drift.

## Method

1. Read all four documents end to end: `L1-gapfix-implementation.md` (post-correction),
   `adve-review-mooshik-L1-round1.md` (525 lines), `L1-remediation-round1.md`, and
   `L-gcp-hosted-postgres.md`. Then `git show 9580873` for what came before, and the full
   uncommitted `git diff` file by file.
2. Read the post-remediation sources rather than the diff alone: `src/gcp_auth.rs` module
   doc and `access_token_with_expiry`, `build_gemini_embedder` and its three-arm pin,
   `PgStore::new` / `iam_pool` / `connect_options` in `src/store/pg/mod.rs` (**unchanged**
   by the remediation, confirmed: it is not in the diffstat), the four new tests, and all
   127 lines of `scripts/cloudsql-allowlist.sh`.
3. **Ten source mutations**, each applied to the working tree, run, then restored from a
   byte-for-byte backup. Results in Part B. A closure counts as verified only if the
   claimed pin goes **red** under the mutation.
4. **One temporary probe test**, added and removed, to settle an env-precedence question
   the existing pins cannot answer (Part D, new finding L1-R2-3).
5. Re-ran all twelve required gates myself, with per-target sums, and independently
   re-measured the `0602f3b` baselines in a detached worktree (`git worktree add --detach`,
   removed afterwards, `git worktree list` back to one entry).
6. **53 runs** of the new PostgreSQL handshake pin: 25 isolated, 20 as part of the full
   `store-postgres` lib suite under `cargo test` parallelism, 8 more of the full suite under
   36 busy-loop CPU hogs on a 12-core box.
7. Exercised the allowlist script against a stub `gcloud` on `PATH` (six cases, no live GCP
   call), then mutated each shell fix away and re-ran the case it was written for.
8. Did **not** run the live Vertex test (verified in round 1, costs a real API call). Did
   **not** re-run the remediation's live `tokeninfo` / token-endpoint measurements: reading
   this host's ADC file was refused by the sandbox, so the narrowing table in
   `L1-remediation-round1.md` §P1-1 is the remediator's measurement and is not independently
   confirmed here. That limit is recorded in Part E rather than hidden.

## Part A - per-finding closure verification

| Finding | Verdict | Verification |
| --- | --- | --- |
| **P1-1** (P1) refresh grant dropped the caller's scope | **CLOSED-VERIFIED** | `src/gcp_auth.rs:367` sends `("scope", self.scope.clone())` on the `refresh_token` arm and nothing was added to the `jwt-bearer` arm, so the service-account path is untouched. Mutation 1 (delete the line): pin **FAILED**, `Backend("OAuth token endpoint returned 404 Not Found: Request did not match any route or mock")`. Mutation 2 (hardcode `SCOPE_CLOUD_PLATFORM` in place of `self.scope`): pin **FAILED** the same way, so the "hardcoded to one consumer passes one half" claim is true, not decoration. The pin compiles into **both** feature rows: mutation 1 was run under `--features embed-gemini`, mutation 2 under `--no-default-features --features store-postgres`, both red. The `invalid_scope` classification claim is code-true: `access_token_with_expiry` maps every non-2xx to `GoogleAuthError::Backend` (`gcp_auth.rs:381-385`), and Google answers `invalid_scope` with 400. Residue on the two **consumer call sites**, not on this fix: see new finding L1-R2-1. |
| **P2-1** (P2) embedder ignored `GCP_LAMBO_CREDENTIALS` | **CLOSED-VERIFIED** | `src/embed/mod.rs:464` is `.or_else(crate::gcp_auth::credentials_path_from_env)`. Mutation 3 (restore the `GOOGLE_APPLICATION_CREDENTIALS`-only closure): `gemini_resolves_the_shared_credential_variable` **FAILED** on arm 1 with exactly the message the record quotes. Mutation 4 (swap it for a `GCP_LAMBO_CREDENTIALS`-only closure, i.e. one hardcoded variable for another): **FAILED** on arm 2, `"GOOGLE_APPLICATION_CREDENTIALS must keep working"`. Error text verified to name all three answers (`gemini_credentials`, `GCP_LAMBO_CREDENTIALS`, `GOOGLE_APPLICATION_CREDENTIALS`) and arm 3 asserts each by name. Precedence and empty-value behaviour are **not** covered by the three arms: new findings L1-R2-2 and L1-R2-3. |
| **P2-2** (P2) allowlist empty-list guard was dead code | **CLOSED-VERIFIED** | Stub `gcloud`, remove the sole entry: prints the three status lines **then** `refusing to empty the allowlist: that locks every machine out of the store`, exit 1. Mutation 5 (drop `\|\| true` from line 112): the same case prints the three status lines and exits 1 **with no message**, reproducing the round-1 symptom exactly. `\|\| true` sits inside the command substitution and applies to the whole pipeline, so `pipefail` cannot re-arm `set -e` before the `[ -z "$desired" ]` guard; the `add` branch has no non-zero-capable member and correctly did not get one. Removing one of two entries still yields `desired:  5.6.7.8/32` and no patch. |
| **P2-3** (P2) nothing offline proved the token becomes the password | **CLOSED-VERIFIED, both halves** | Mutation 6 (`let options = Self::connect_options(&self.dsn)?;`, the reviewer's original mutation 2): `the_minted_token_is_the_connection_password_across_a_rotation` **FAILED**, `left: "" right: "tok-1"`. The empty left-hand side is the proof the pin reads the *password field* and not "some bytes arrived". Mutation 7 (rotate the pool but keep the first token as the password, via a `OnceLock` around the token): **FAILED** on the second assertion, `left: "tok-1" right: "tok-2"`, so the rotated-pool half is real and not implied by `mint2.assert_hits(1)`. Flake hunt in Part C: **53 runs, 0 failures**. Port is ephemeral (`127.0.0.1:0`); the listener is moved into the spawned task, which is `abort()`ed on the success path and dropped with the `#[tokio::test]` runtime on any panic; the `mpsc` is unbounded so the server never blocks; 28P01 is not in sqlx-postgres' transient-in-connect set (53300 / 57P03) so `acquire()` returns after exactly one connection attempt, which is what keeps the two `rx.recv()` calls in step. |
| **P3-1** (P3) baseline test counts were wrong | **CLOSED-VERIFIED** | Re-measured myself in a detached worktree at `0602f3b`, every row run to completion: `embed-bge,embed-fixture --lib` **918**; `store-postgres` **606 lib + 8 other + 2 doc = 616**; `embed-gemini` **935 lib + 11 other + 2 doc = 948**. Every number in the corrected table in `L1-gapfix-implementation.md` matches mine exactly, including the two columns round 1 did not measure. |
| **P3-2** (P3) duplicated doc-comment paragraph | **CLOSED-VERIFIED** | `grep -c "The IAM token is a password with an expiry" src/store/pg/postgres.rs` = **1** (was 2 at `9580873`, confirmed with `git show`). The surviving copy is byte-identical to the original and the third paragraph (the env-lock window) is intact. |
| **P3-3** (P3) README's `lambo.toml` sample was stale | **CLOSED-VERIFIED** | `ship` in `Cargo.toml:146-154` is exactly `store-memory, store-cockroach, store-postgres, store-sqlite, embed-bge, embed-fixture, embed-gemini`. `README.md:137` now reads `memory \| sqlite \| postgres \| cockroach` (ship's four stores) and `:140` reads `fixture \| bge_m3 \| gemini` (ship's three embedders, correctly excluding `candle` and `bedrock`). `lambo.example.toml:31` reads `bge_m3 \| gemini \| candle \| bedrock \| fixture`, which is all five `EmbedderKind` variants (`src/embed/mod.rs:147-159`), matching that file's store line, which already listed all four `StoreKind`s. Two files, two conventions, each internally consistent. |
| **P3-4** (P3) `LAMBO_POSTGRES_IAM=` silently changed meaning | **CLOSED-VERIFIED** | Mutation 8 (restore `.is_some()`): `an_empty_iam_opt_in_means_the_password_path` **FAILED** with `an empty opt-in must take the password path, not demand a credential: Backend("PostgresStore IAM auth setup: LAMBO_POSTGRES_IAM is set but GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS is unset")`. The narrowing is now stated in `CHANGELOG.md:44-46` and `L-gcp-hosted-postgres.md`. The "as everywhere else lambo overlays the environment" claim checks out: `EmbedderConfig::overlay_env` gates every value on `if !v.is_empty()` (`src/embed/mod.rs:337-352`). `with_iam_env` was generalised into `with_iam_env_value` with only the literal `"1"` becoming a parameter; all three save/restore arms are unchanged, so no round-0 pin was weakened to make this one pass. |
| **P3-5** (P3) record understated the CI blast radius | **CLOSED-VERIFIED** | Spot-checked two of the six rows myself in the `0602f3b` worktree: `cargo check --no-default-features --features store-cockroach` fails with **E0433**, `cargo check --features demo` exits **101**. The release-job row is quoted correctly: `.github/workflows/release.yml:31` is `FEATURES: ship` and `:101` is `cargo build --release --features "$FEATURES" --target ...`, so the shape the record describes (defaults on, no `store-postgres`) is the shape CI actually builds. |
| **P3-6** (P3) allowlist script nits | **CLOSED-VERIFIED, all four** | `-qxF` / `-vxF`: `--remove --ip 1.2.3.4` against a list holding `1X2.3.4/32` prints `not on the list; nothing to do`, exit 0. `need_value`: `--remove --ip` prints `--ip needs a value`, exit **2**; mutation 9 (restore `IP="${2:-}"; shift`) reproduces the round-1 symptom, exit 1 with no output. `--help`: **29 lines, 0 occurrences of `set -euo`**, and the `awk` form has no line range to go stale. `--list` on an empty allowlist prints `  (none: the allowlist is empty, so no machine can reach the instance)`. |

Mutation score: **9 of 9 attempted closure mutations were caught by the claimed pin.** No
pin is vacuous. The tenth mutation family (Part D) targets surfaces the remediation did
**not** claim to pin, and all three of those survived, which is where the new findings come
from.

## Part B - mutations performed

Every mutation was applied to the working tree, run, then restored from a byte-for-byte
backup taken before the first one. Final `md5sum` of all five files matches the backups.

| # | Mutation | Target | Outcome |
| --- | --- | --- | --- |
| 1 | delete `("scope", self.scope.clone())` (`gcp_auth.rs:367`) | `the_callers_scope_is_sent_on_the_refresh_grant`, `--features embed-gemini` | **FAILED** (404, no mock matched) |
| 2 | `("scope", SCOPE_CLOUD_PLATFORM.to_string())`, i.e. hardcoded to the embedder's constant | same pin, `--no-default-features --features store-postgres` | **FAILED** (404 on the Cloud SQL half) |
| 3 | restore the `GOOGLE_APPLICATION_CREDENTIALS`-only closure in `build_gemini_embedder` | `gemini_resolves_the_shared_credential_variable` | **FAILED** on arm 1 |
| 4 | replace it with a `GCP_LAMBO_CREDENTIALS`-only closure | same pin | **FAILED** on arm 2 |
| 5 | drop `\|\| true` from `cloudsql-allowlist.sh:112` | stub-`gcloud` remove-the-only-entry case | **regressed**: three status lines then exit 1, no message |
| 6 | `let options = Self::connect_options(&self.dsn)?;` (drop `.password(&token)`) | `the_minted_token_is_the_connection_password_across_a_rotation` | **FAILED**, `left: "" right: "tok-1"` |
| 7 | rotate the pool but reuse the first token as password (`OnceLock`) | same pin | **FAILED**, `left: "tok-1" right: "tok-2"` |
| 8 | restore `var_os(..).is_some()` in `iam_auth_requested` | `an_empty_iam_opt_in_means_the_password_path` | **FAILED**, named refusal |
| 9 | restore `--ip) IP="${2:-}"; shift ;;` | stub-`gcloud` `--remove --ip` case | **regressed**: exit 1, no message |
| 10a | `src/store/pg/mod.rs:1453` `SCOPES_CLOUD_SQL_LOGIN` -> `SCOPE_CLOUD_PLATFORM` | whole `--no-default-features --features store-postgres` row | **620 passed / 0 failed.** Not caught. L1-R2-1 |
| 10b | `src/embed/mod.rs:497` `gemini::OAUTH_SCOPE` -> `SCOPES_CLOUD_SQL_LOGIN` | whole `--features embed-gemini` row | **943 passed / 0 failed.** Not caught. L1-R2-1 |
| 10c | reverse the two arms of `credentials_path_from_env` | both the gemini and the postgres rows | **943 / 620 passed, 0 failed.** Not caught. L1-R2-2 |
| 10d | demote `cfg.gemini_credentials` below the env chain | `--features embed-gemini` row | **943 passed / 0 failed.** Not caught. L1-R2-2 |

Shell exercise of `scripts/cloudsql-allowlist.sh` against a stub `gcloud` (no live call):

| case | outcome |
| --- | --- |
| `--remove --ip X --dry-run`, X the only entry | **refusal message, exit 1** (was: silent exit 1) |
| `--remove --ip X --dry-run`, two entries | `desired:  5.6.7.8/32`, `dry run: not patching`, exit 0 |
| `--remove --ip` (no value) | `--ip needs a value`, exit **2** |
| `--list` on an empty allowlist | `  (none: the allowlist is empty, ...)`, exit 0 |
| `--remove --ip 1.2.3.4` against `1X2.3.4/32` | `not on the list; nothing to do`, exit 0 |
| `--help` | 29 lines, 0 hits for `set -euo` |

## Part C - gates rerun (my own runs, remediated tree)

`lib` / `other` / `doc` are per-target sums: one `test result:` line per target, `other` is
every non-lib binary, `doc` is `Doc-tests`.

| Gate | Remediation record | My result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | clean | **pass** (exit 0) |
| `cargo clippy --all-targets -- -D warnings` | clean | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | clean | **pass** |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | clean | **pass** |
| `cargo clippy --all-targets --no-default-features --features store-postgres -- -D warnings` | clean | **pass** |
| `cargo clippy --all-targets --no-default-features --features store-postgres,store-sqlite,fixtures -- -D warnings` | clean | **pass** |
| `cargo test --features embed-bge,embed-fixture --lib` | 918 / 0 / 1 ignored | **918 passed / 0 failed / 1 ignored** |
| `cargo test --features ship,fixtures` | 1130 + 38 + 2 = 1170, 20 ignored | **1130 lib + 38 other + 2 doc = 1170 passed / 0 failed / 20 ignored** |
| `cargo test --no-default-features --features store-postgres` | 620 + 8 + 2 = 630, 5 ignored | **620 lib + 8 other + 2 doc = 630 passed / 0 failed / 5 ignored** |
| `cargo test --no-default-features --features store-cockroach` | 619 + 8 + 2 = 629, 0 ignored | **619 lib + 8 other + 2 doc = 629 passed / 0 failed / 0 ignored** |
| `cargo test --features embed-gemini` | 943 + 11 + 2 = 956, 4 ignored | **943 lib + 11 other + 2 doc = 956 passed / 0 failed / 4 ignored** |
| `cargo check --no-default-features` | clean | **pass** |

**Zero drift.** Every headline number, every per-target sum and every ignored count matches
the remediation record exactly. This is the first round in this workstream where that is
true; round 1 found four rows whose non-lib sums were wrong.

Baselines I measured myself at `0602f3b` (detached worktree, removed afterwards), for the
record's corrected table:

| row at `0602f3b` | lib | other | doc | total |
| --- | --- | --- | --- | --- |
| `--features embed-bge,embed-fixture --lib` | **918** | - | - | 918 |
| `--no-default-features --features store-postgres` | **606** | **8** | **2** | **616** |
| `--features embed-gemini` | **935** | **11** | **2** | **948** |

Delta check against the four added tests, name by name: `the_callers_scope_is_sent_on_the_refresh_grant`
(gcp_auth, so postgres + gemini + ship), `gemini_resolves_the_shared_credential_variable`
(gemini + ship), `the_minted_token_is_the_connection_password_across_a_rotation` and
`an_empty_iam_opt_in_means_the_password_path` (postgres + ship). Predicted lib deltas from
`9580873`: bge 0, ship +4, postgres +3, cockroach 0, gemini +2. Measured: 918, 1130, 620,
619, 943, which is exactly 918+0, 1126+4, 617+3, 619+0, 941+2. No row lost a test.

`git diff -U0 -- src/ | grep '^-'` shows **zero** removed `fn` / `#[test]` / `#[tokio::test]`
/ `#[ignore]` lines, so nothing was deleted or silently ignored to make a row pass.

**Flake hunt on `the_minted_token_is_the_connection_password_across_a_rotation`:**

| mode | runs | failures |
| --- | --- | --- |
| isolated (`-- --exact`) | 25 | **0** |
| full `store-postgres` lib suite, default `cargo test` parallelism | 20 | **0** |
| full suite under 36 busy-loop CPU hogs, 12-core box | 8 | **0** |

The timing margin is `expires_in: 62` minus the 60s cache margin = 2s TTL against a 2300ms
sleep, and the sleep starts **after** the first password is received, so the real margin is
300ms plus however long the first `acquire()` took. It held under 4.5x CPU oversubscription.

## Part D - new findings

### L1-R2-1 (P3). Neither consumer's scope constant is pinned, and the remediation record calls two synthetic token sources "the Cloud SQL caller" and "the Vertex caller".

`src/store/pg/mod.rs:1453` and `src/embed/mod.rs:497`.

The P1-1 pin proves that `GoogleOAuthTokenSource` puts **whatever scope it is handed** on
the wire for both grants, which is exactly the defect round 1 found and is genuinely closed.
It does not prove that the store hands it `SCOPES_CLOUD_SQL_LOGIN` or that the embedder
hands it `SCOPE_CLOUD_PLATFORM`, because both of its token sources are constructed inside
`gcp_auth::tests` (`gcp_auth.rs:652-687`), not by `iam_pool` or `build_gemini_embedder`.
Mutations 10a and 10b swap the two call sites' constants and leave both feature rows
**entirely green** (620 and 943 passed, 0 failed).

`vertex_asks_for_the_cloud_platform_scope_only` (`src/embed/gemini.rs:309-312`) reads like
the missing pin but is not one: it asserts `OAUTH_SCOPE == SCOPE_CLOUD_PLATFORM` and
`!OAUTH_SCOPE.contains("sqlservice.login")`, which is a property of the constant, not of
what `build_gemini_embedder` passes.

**Concrete failure scenario, and it is worse after this remediation than before it.** Before
the fix, `scope` was inert on the ADC grant, so a wrong constant at either call site was
invisible on that path. Now it is a narrowing ask. A refactor that hoists the token-source
construction into a shared helper and picks one constant, or simply an autocomplete slip
between two `crate::gcp_auth::SCOPE*` names, produces: (a) at `mod.rs:1453`, a Cloud SQL
token without `sqlservice.login`, refused by Postgres as the opaque authentication noise the
module doc says this design avoids; or (b) at `embed/mod.rs:497`, a Vertex ask for
`sqlservice.login` that an ADC lacking that scope now answers `invalid_scope` at the token
endpoint, so the embedder stops working on a host where it worked before. Every CI row stays
green in both directions.

The corresponding sentence in `L1-remediation-round1.md` §P1-1 ("the **Cloud SQL caller**
must post ... and the **Vertex caller** must post ...") reads as consumer coverage the pin
does not have. Round 1's theme was prose claiming more than the code does; this is a smaller
instance of it, in the remediation record rather than in shipped docs.

Fix, cheap: assert the constant at each call site's construction, or give `iam_pool` and
`build_gemini_embedder` a shared `fn scope_for(..)` and pin that. Or correct the record's
two labels.

### L1-R2-2 (P3). The new credential pin has three arms and none of them sets both variables, so the precedence the docs now sell is unasserted.

`src/gcp_auth.rs:159-163` (the chain) and `src/embed/mod.rs:1028-1046` (the pin's arms).

The three arms are `GCP_LAMBO_CREDENTIALS` alone, `GOOGLE_APPLICATION_CREDENTIALS` alone,
and neither. There is no arm with **both** set to different files, which is the only
configuration in which precedence is observable. Mutation 10c reverses the two arms of
`credentials_path_from_env` and both the gemini row (943) and the postgres row (620) stay
green. Mutation 10d demotes `cfg.gemini_credentials` below the env chain and the gemini row
stays green too.

The precedence is not incidental: it is the stated reason the variable exists.
`gcp_auth.rs:155-157` says *"`GCP_LAMBO_CREDENTIALS` first so a deployment can point lambo
at one identity without disturbing whatever `GOOGLE_APPLICATION_CREDENTIALS` means to the
rest of the machine"*, `CHANGELOG.md:41-43` says *"resolves credentials from
`GCP_LAMBO_CREDENTIALS` **before** `GOOGLE_APPLICATION_CREDENTIALS`"*, and
`L-gcp-hosted-postgres.md` repeats it.

**Concrete failure scenario.** The deployment the variable was invented for: a developer box
with a machine-wide `GOOGLE_APPLICATION_CREDENTIALS` pointing at their personal ADC, plus
`GCP_LAMBO_CREDENTIALS` pointing at the shared service-account key. Reverse the two arms of
that three-line function, or add a third variable ahead of them, and lambo silently
authenticates to Cloud SQL **and** Vertex as the personal identity. Nothing fails, nothing
logs, and every gate is green. The code is correct today; the pin written specifically to
close P2-1 cannot see the one property that makes it worth having.

Fix: a fourth arm, both variables set to different valid credential files, asserting the
built embedder resolves the `GCP_LAMBO_CREDENTIALS` one, plus a fifth with
`gemini_credentials` set alongside both. Roughly fifteen lines in a test that already builds
the fixture.

### L1-R2-3 (P3). `GCP_LAMBO_CREDENTIALS=` (set, empty) now breaks the embedder, which is exactly the convention P3-4 was closed to establish.

`src/gcp_auth.rs:159-163`.

`credentials_path_from_env` is `var_os("GCP_LAMBO_CREDENTIALS").or_else(|| var_os("GOOGLE_APPLICATION_CREDENTIALS")).map(PathBuf::from)`. `var_os` returns `Some("")`
for an exported-but-empty value, so an empty `GCP_LAMBO_CREDENTIALS` **shadows** a perfectly
good `GOOGLE_APPLICATION_CREDENTIALS` and yields `PathBuf::from("")`.

Before this remediation the embedder never read `GCP_LAMBO_CREDENTIALS`, so this could not
reach it. I measured both sides with a temporary probe test (added, run, removed; the tree
is checksum-verified back to its backup):

```
remediated tree:  PROBE RESULT: refused -> embedder unavailable:
                  cannot read Google credentials : No such file or directory (os error 2)
9580873 lookup:   PROBE RESULT: built (empty value treated as absent)
```

Note the message: the path renders as nothing at all, between "credentials" and the colon.

**Concrete failure scenario.** An operator has `export GCP_LAMBO_CREDENTIALS=` in a `.env`
or a profile as a placeholder (exactly the scenario `L-gcp-hosted-postgres.md` names when it
explains why an empty `LAMBO_POSTGRES_IAM` must mean "absent") and a working
`GOOGLE_APPLICATION_CREDENTIALS`. On `9580873` the embedder built. Here `resolve.rs:118`
turns the `Unavailable` into `LamboError::Config` and the process refuses to start, citing a
credential file with no name. Fail-closed and loud, which is why this is a P3 and not a P2,
but the diagnosis cost is real and the direction contradicts the repo convention the same
remediation just enshrined: `EmbedderConfig::overlay_env` gates every single value on
`if !v.is_empty()` (`src/embed/mod.rs:337-352`), and P3-4 made `iam_auth_requested` do the
same, documented it in `CHANGELOG.md:44-46` and pinned it. `credentials_path_from_env` is
now the one env reader in this change that does not follow it, and the remediation widened
its blast radius from one consumer to two.

The store has the same shape (pre-existing, not introduced here): an empty
`GCP_LAMBO_CREDENTIALS` turns `iam_pool`'s crisp *"is set but ... is unset"* refusal into the
same nameless-path read error.

Fix: one line, `.filter(|v| !v.is_empty())` on each arm. It would also make P3-4's stated
convention uniform across the change instead of true of one variable.

## Part E - defects introduced by the remediation, hunted and not found

Every surface the brief named, checked:

- **Scope narrowing breaking the service-account path.** It cannot: the `scope` parameter was
  added only to the `AuthorizedUser` arm of the `match &self.creds`
  (`gcp_auth.rs:346-369`); the `ServiceAccount` arm still posts `grant_type` + `assertion`
  and carries its scope in the signed JWT claim, unchanged from `9580873`.
- **Env-precedence change altering existing behaviour.** One real change found and it is
  documented: an operator with both variables set to different files now gets the
  `GCP_LAMBO_CREDENTIALS` one in the embedder where they previously got the
  `GOOGLE_APPLICATION_CREDENTIALS` one. `CHANGELOG.md:41-43` states it. The empty-value case
  is **not** documented and is L1-R2-3.
- **`set -euo pipefail` and the new `|| true`.** The `|| true` is inside the command
  substitution and terminates a `||` list, so `set -e` is disarmed for the whole pipeline and
  the substitution's status is 0. The only pipeline member that can legitimately return
  non-zero is the `grep -vxF` being handled. The `add` branch's pipeline (`printf | sed |
  sort | paste`) has no non-zero-capable member and correctly did not get a `|| true` that
  would have masked a future one. `need_value` is called **before** the assignment and before
  either `shift`, and `[ "$2" -ge 2 ]` is correct for `$#` measured at the flag. `--help`
  exits before the `command -v gcloud` guard, so it still works without gcloud installed.
- **Test pollution of process-global env under parallelism.** Every test that touches
  `GCP_LAMBO_CREDENTIALS`, `GOOGLE_APPLICATION_CREDENTIALS` or `LAMBO_POSTGRES_IAM` now takes
  `crate::test_util::env_lock()`, including `gemini_fail_closed_without_credentials`, which
  did not before and was the one that could have raced. `env_lock` is poison-tolerant
  (`unwrap_or_else(|e| e.into_inner())`), so a panicking test cannot wedge the suite. Both new
  env-touching tests collect their results **first** and assert **after** restoring, so a
  failure cannot leave the process environment dirty for the next test. The six scratch
  directories in these modules all have distinct prefixes, so no two tests race on a path.
  Empirically: 20 full-suite parallel runs plus 8 under CPU oversubscription, 0 failures.
- **Weakening a round-0 pin to make a round-1 pin pass.** No test function was removed and
  none was `#[ignore]`d (verified by grepping the diff for removed `fn` / attribute lines,
  zero hits). The only existing test body modified is
  `gemini_fail_closed_without_credentials`, whose four assertions are byte-identical to
  `9580873`; the change only makes its premise explicit. `with_iam_env` delegates to
  `with_iam_env_value("1", ..)` with all three save/restore arms preserved verbatim.
- **Task or listener leak in the new pin.** The `TcpListener` is moved into the spawned task
  and dies with it; the task is `abort()`ed on success and dropped with the current-thread
  runtime on panic. The pin leaves `/tmp/lambo-iam-pw-<pid>/` behind (it removes the file, not
  the directory), which is the same thing its sibling
  `the_iam_pool_is_rebuilt_when_its_token_expires` has always done. Not raised.

## Part F - documents against the code

- `src/gcp_auth.rs` module doc: **accurate**. "sends it on the wire either way" is now true
  (`:367`); "asking for more is answered `invalid_scope` ... which this module classifies
  `Backend`" is code-true for any non-2xx (`:381-385`).
- `CHANGELOG.md:32-52`: **accurate**, no duplicate entries in the Unreleased section. The
  scope entry, the new embedder-credential entry and the non-empty-`LAMBO_POSTGRES_IAM`
  entry all match the code. The one gap is the empty-value asymmetry of L1-R2-3.
- `L-gcp-hosted-postgres.md`: the "four offline pins" list names four tests that all exist,
  spelled correctly, in `src/store/pg/postgres.rs`. The `an_empty_iam_opt_in_means_the_password_path`
  claim is mutation-verified above.
- `README.md:137,140` and `lambo.example.toml:10,31`: verified against `Cargo.toml`'s `ship`
  set and against both `kind` enums. Nit, not raised: the README sample still carries
  `dim = 1024` on the line under a comment that now offers `gemini`, and the A4 dim guard
  rejects 1024 for gemini. The refusal names 768 / 1536 / 3072 explicitly at startup, so the
  trap costs one error message.
- `L1-gapfix-implementation.md`: the two corrections are accurate and my independent
  measurements match them to the test. The original gates table is left standing with its
  wrong non-lib sums and a correction paragraph below it, which is a defensible way to keep a
  historical record honest rather than rewritten, and the correction says plainly which
  numbers in the table are wrong.
- `L1-remediation-round1.md`: every measurable claim I could check checks out (gate table,
  drift table, mutation outputs, flake count, stub-`gcloud` outputs, the six-row `0602f3b`
  breakage table, the `release.yml` line references, the sqlx 0.8.6 citations). The one
  overstatement is the "Cloud SQL caller / Vertex caller" labelling in §P1-1, folded into
  L1-R2-1. The §P1-1 live measurements against `oauth2.googleapis.com` I could not reproduce:
  reading this host's ADC file was refused by the sandbox. They remain the remediator's
  evidence, uncorroborated here.

## Verdict

**APPROVE** - 0 P1, 0 P2, 3 P3.

All ten round-1 findings are closed and nine of them are mutation-verified against a pin that
goes red; the tenth (P3-1) is closed by independent re-measurement that matches the record
digit for digit. The two headline fixes are real: `scope` reaches the wire on the refresh
grant and a hardcode to either consumer's constant is caught, and the PostgreSQL v3 handshake
pin proves both that the minted token **is** the connection password and that a rotated pool
carries the **new** one, over 53 runs including full-suite parallelism and 4.5x CPU
oversubscription. All twelve gates are green with zero drift from the record, which is a
first for this workstream. The shell fixes are exercised, not asserted.

What is left is residue, not a blocker. Three P3s, all of the same family: the new pins prove
the mechanism and stop one call site short of the consumer. Nothing in them is wrong in the
shipped code today, and each is fifteen lines of test or one `.filter()` away from being
unobservable. None of them should stop this landing; all three should be written down before
the next hand touches `credentials_path_from_env` or a `SCOPE*` constant.

- L1Review2, 2026-08-25
