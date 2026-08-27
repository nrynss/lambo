# Adversarial Review: C3 — `promotion_policy` on the serve surface

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — ACCEPTED after round 4, on a judgement call         ║
║  Verdict: ACCEPTED WITHOUT AN EMPTY ROUND. Rounds 1-4 closed. R4      ║
║    found 3 (2×P2, 1×P3), none of them in shipped behaviour, so the    ║
║    "review until a round returns empty" rule was retired rather       ║
║    than satisfied — see "Why this closed at four rounds".              ║
║  Both R4 open questions were answered: the macro is justified and     ║
║    costs nothing measurable; the fallback string was NOT made         ║
║    reachable, and is now labelled for what it is.                     ║
║  Gates after R4: see "State after round 4". All green.                ║
║  Findings: R1 13 (+1 self) · R2 11 · R3 6 (+4 nits, +3 self) · R4 3   ║
║  P0/P1 open: none. Last P1 closed in R2. No P0 after R0.              ║
║  Live services: none used, none needed.                               ║
║  Opened: 2026-08-27                                                    ║
╚══════════════════════════════════════════════════════════════════════╝
```

**Task:** C3 — expose `PromotionPolicy` on the process-config surface so `lambo serve` can
select it. Follows C1 (the `PromotionScorer` seam) and C2 (the solo formula) in
[`../lambo-for-mooshik/C-solopolicy.md`](../lambo-for-mooshik/C-solopolicy.md).

**Branch reviewed:** `task/promotion-policy-serve` @ base `71334f0`, reviewed as an
uncommitted working-tree diff across all four rounds.

**Why the task existed:** C1/C2 made the policy selectable *in Rust only*. `PromotionPolicy`
was absent from `LamboFile` (which is `deny_unknown_fields`), had no env override, and `serve`
had no flag — so every `lambo serve` canonized under `Swarm`. For Mooshik's bootstrap ingester,
which spawns `lambo serve` as its writer, that means nothing can ever promote. Measured, not
theorized: 7 recurrences over 7 days with correct `event_time` and 4 canonization cycles
produced zero canonization events.

**Method:** four rounds of adversarial review, each followed by remediation, both on Opus per
the standing per-task rule. Reviewer was read-only throughout and did its mutation testing on
copies under the session scratchpad; all implementation ran in an isolated worktree, never the
main checkout. Findings were withheld from the reviewer's brief in R1 so its pass stayed
independent, then merged with the orchestrator's own before remediation.

---

## Round 0 — orchestrator triage (the prior agent died mid-review)

The implementation inherited from the dead agent **did not compile.**

| # | Defect | Severity |
|---|---|---|
| 1 | New MCP test used `CanonizationStatus` with no import — 4× `E0433`/`E0425`, lib test target would not build | P0 |
| 2 | Unused `mut` on the test's `Config` binding — CI runs `clippy --all-targets -- -D warnings` | P1 |

Both trivial, fixed in place rather than dispatched. After them: 980 passed / 0 failed.

Wiring was confirmed real on inspection: `LamboFile::load_resolved` → `resolve_backends` →
`ResolvedBackends.config` → `Memory::builder().config(..)` → the live canonization task. The
pre-existing `Config::validate` refusal of `Solo` was genuinely gone (C2 removed it).

## Round 1 — 13 findings (2×P1, 5×P2, 6×P3)

- **P1** `LAMBO_PROMOTION_POLICY=` (empty) was a hard startup error, contradicting the docs
  line it was added under, unanimous repo convention (`!v.is_empty()` in every other
  override), and — verified — turning `cargo test` **red** via `src/main.rs`'s env-hermetic bin
  test, which cleared nine `LAMBO_*` vars but not the tenth.
- **P1** The documented sample `lambo.toml` did not parse: `promotion_policy` sat after
  `[embedder]`, so `deny_unknown_fields` rejected it. `lambo.example.toml` had it right, so the
  change's own two artifacts disagreed.
- **P2** The `ResolvedBackends.config → Memory` seam on the serve path had no test. Deleting
  `.config(config)` from `serve_builder` left all 981 tests green while `serve` silently
  reverted to `Swarm`. `src/cli/mod.rs` has the precedent test — it exists because `open_writer`
  shipped that exact regression once (T1-R1-2).
- **P2** No runtime visibility of the live policy. The attach line logs `match_strategy` — the
  *other* enum-valued `Config` knob — but not this one, and `PromotionPolicy::as_str()`, whose
  docstring says it exists for exactly this, had zero callers.
- **P2** The change's own new doc sentence, "without changing any threshold", was materially
  false: measured, `Solo` reaches `Canonical` in 3 cycles having cleared **none** of the four
  gates. `api.mdx` already said the opposite, correctly, and was left untouched still asserting
  the file "only chooses the store and the embedder".
- **P2** `lambo.example.toml`'s `[daemon]` prose and `DaemonConfig`'s docstring were falsified
  by the value the change now offers (~15 mutations and zero GC sweeps, vs. the claimed ~30000).
- **P2** `serve_web`'s gate-progress payload lied under `Solo` — "0 of 4 gates met" beside a
  concept promoting next cycle.
- **P3** ×6: env parser skipped the trim and case-insensitivity both sibling enums have (`solo`
  refused, and it is the likeliest thing typed since every other file value is snake_case); the
  new test's cycle budget had one cycle of slack against background writes; the `Swarm` arm's
  docstring named the wrong mechanism; a stale `validate` comment; the bin-test clean-slate
  list; a `demo` carve-out note.

**Cleared, not a finding:** the absent `serve --promotion-policy` flag. Verified that *zero*
`serve` flags duplicate a `lambo.toml` key — every backend/config selector comes from file or
env, so omitting it is correct per repo convention rather than merely permitted by the spec.

Remediation did the parser first and built the rest on it: one `FromStr`, a `pub const ALL`, and
an `expected()` derived from `as_str()`, so the valid-set string has a single source. The file
path was routed through the same lenient parser via `deserialize_with`, refusing an
env-lenient/file-strict split. Both P1s were proven by reverting the fix and reproducing the
failure, including the exact red build. One extra found and fixed: `config.mdx`'s verbatim error
transcript, already wrong before this change and made wronger by it.

## Round 2 — 11 findings (3×P2, 8×P3)

R1's fixes were confirmed load-bearing by mutation. Two showed R1's P2 on `serve_web` was only
**nominally** closed:

- **P2** The `Solo` early-return over-omitted. `GateProgress` is the only carrier of
  `in_cooldown`/`cooldown_until`, and the cooldown is policy-*in*dependent — `canon/eval.rs`
  gates the Venerable→Canonical hop on it precisely *when* `evidence.is_none()`, i.e. exactly
  the `Solo` path. A `Solo` concept stalled 300s in cooldown got zero explanation, where the
  pre-fix payload at least carried that one true fact among four bogus gates.
- **P2** The omission was keyed on `serve-web`'s *own* resolved policy, not the writer's. In the
  deployment the new docs recommend — writer under systemd with
  `Environment=LAMBO_PROMOTION_POLICY=Solo`, operator opening `serve-web` by hand — the reader
  resolves `Swarm` and renders the original lie.
- **P2** The sentence R1's remediation *wrote* in `api.mdx` was false: it claimed
  `promotion_policy` is the "one exception" to code-only settings, but `[daemon]` already makes
  two `Config` keys file-settable. The old sentence was vaguely wrong; the rewrite made it
  precisely and checkably wrong.
- **P3** ×8: no `CHANGELOG.md` entry despite two genuine semver breaks; a **second** identical
  env clean-slate list; `[daemon]` referenced three times on a page with no `[daemon]` section;
  a comment claiming the payload carried `promotion_policy` when it did not, leaving three
  omission causes indistinguishable; `mcp.mdx` payload contract not updated; two imprecise doc
  claims disproved against the real binary; one `pub` that should be `pub(crate)`.

Remediation split the four swarm gates into a `SwarmGates` group held as `Option` with
`#[serde(flatten)]`, so `Solo` drops the gates and keeps the cooldown, `met_count()` returns
`Option` so no caller can render "0 of 4", and neither store query runs under `Solo`. No
cross-process channel was invented — none exists, so the requirement was documented instead.
Verified end-to-end on the shipped `serve-web` binary across three configurations.

Two discoveries worth keeping: the env clean-slate list had **five** copies, not two, and three
were in integration-test crates that `env_remove` on a *spawned* binary — so an ambient
`LAMBO_PROMOTION_POLICY=Solo` would have reached the real process. And remediation caught its
own vacuous test: its first pin set only the variables the list named, so deleting a name also
stopped it being set, and the test could never fail.

## Round 3 — 6 findings (1×P2, 5×P3)

All R2 fixes confirmed closed by 13 mutations. The restructuring was judged justified rather
than over-built, and no place was found with two sources of truth. Because `GateProgress`,
`SwarmGates` and `InspectResponse` all derive `Serialize` **only**, the `flatten`-on-`Option`
deserialization hazards are unreachable by construction.

The two substantive findings were both **vacuous safety** — something that looked pinned and
was not, each proved by mutation rather than argued:

- **P2** `RESOLVE_ENV_VARS` asserted a completeness it did not have. Its own doc says "every
  variable the resolve reads"; six were missing, including `LAMBO_POSTGRES_DSN`. Behaviour was
  unchanged from base (all five old copies carried the same nine names), so not a regression —
  but the shape was worse than what it replaced: five copies nobody trusted became one public
  const telling the next author it is exhaustive. The hazard is concrete — a harness clears the
  list, writes a `lambo.toml` with `store.kind = "postgres"` and no `dsn`, runs `provision`, and
  the DDL lands on whatever ambient cluster the env names.
- **P3** The `ALL`-covers-every-variant pin could not fail. `assert_eq!(ALL.len(), 2)` fires
  only when a variant is added *to `ALL`*, and the round-trip loop iterates `ALL`. Adding a
  `Tribe` variant and leaving `ALL` alone passed 990/990 — while `Tribe` would have been
  unselectable from both file and env, with every refusal still reading `expected Swarm | Solo`.
- **P3** `met_count() -> Option<usize>` had zero callers and zero tests: replacing its body with
  exactly the "0 of 4 under Solo" bug the CHANGELOG says the `Option` exists to prevent passed
  990/990.
- **P3** `web/app.js` rendered self-contradictory copy under `Solo` + cooldown — "There is
  nothing to tick off" immediately followed by "the checks above: every one of them can be met".
- **P3** The new daemon-cadence docs inverted the validation scope: `recall` is refused despite
  building no session, while `saints`/`inspect`/`stats` pass. The real rule is
  `Commands::needs_embedder()`, not "any command that builds a session".
- **P3** The new render path had no coverage despite an in-file `APP_JS.contains(..)` precedent,
  and its fallback string was unreachable.

Remediation made the `ALL` pin structural via a `promotion_policy!` macro generating the enum
and `ALL` from one variant list — adding a variant now fails to **compile** (`E0004` in four
places). `RESOLVE_ENV_VARS` now references `crate::store::{COCKROACH,POSTGRES,FALLBACK}_DSN_ENV`
rather than re-quoting, so it cannot drift from `dsn_from_env_for_kind`; `DATABASE_URL` was kept
deliberately, on the reasoning that a conventional name shared with unrelated tooling is an
argument *for* clearing it in a hermetic harness. Three more docs errors of the P3-4 class were
found and fixed, including an env table that documented only `LAMBO_COCKROACH_DSN` while the
page advertises `store.kind = "postgres"`.

`src/canon/gate.rs` had no test module at all before this round.

## Round 4 — 3 findings (2×P2, 1×P3)

Run from a clean start against the committed branch, weighted at the two questions the
stopped round had been briefed on. Both are answered below; neither produced a finding, and
both produced evidence rather than argument. The three findings came from elsewhere.

### The two open questions, settled

**1. The `promotion_policy!` macro was not asked for.** Justified, and it costs nothing that
was feared. Two things were checked rather than argued:

* *Does it do the job?* Adding a third variant the only way the macro allows fails to compile
  in four places (`E0004` ×3 in the lib, ×4 counting the lib-test target), and `ALL` grows with
  it — so the variant is selectable from both surfaces the moment it compiles. The property
  R3 asked for holds.
* *Was the "stable Rust cannot count variants" claim real?* Yes. Every hand-written scheme
  needs a second, independent statement of the variant count to bound match arms against;
  `std::mem::variant_count` is nightly, and every substitute (an ordinal `match`, a `next()`
  chain, a discriminant sentinel) can be satisfied by the arm the compiler forces without
  `ALL` growing — which is exactly the mutation R3 landed. The alternatives are a derive
  dependency (`strum`) or this. In-tree macro is the smaller instrument.
* *The feared costs.* Measured against the built rustdoc rather than assumed: the generated
  page carries `id="variant.Swarm"` / `id="variant.Solo"` anchors, both variants' doc prose,
  and the `ALL` associated-constant — structurally identical to the hand-written
  `MatchStrategy` page. The `[src]` link points at `policy.rs#157-185`, the
  `promotion_policy! { … }` invocation, i.e. the variant list itself, so source navigation
  lands where an author needs to be. The macro is not `#[macro_export]`ed, so it adds no page
  of its own. No cost found.

**2. A fallback string was made "reachable" by adding a payload path.** The suspicion was
right, and it is finding **R4-2** below. The `unavailable` half of the concern was *not*
borne out: that state is not invented. The base at `71334f0` already degraded a failed gate
read to `gate_progress: None`; C3 only put a label on an absence that was already being
shipped, and the `already_canonical` / `unavailable` split is a true partition of it.

### Findings

- **P2 — `RESOLVE_ENV_VARS` is still not exhaustive, and the new test cannot see it.**
  T3-P2-1 completed the list against `LamboFile::load_resolved` and stopped there.
  `resolve_backends` does not stop there: it calls `build_store` and `build_embedder`, which
  read the environment again for things no `LamboFile` field carries. Three names were
  missing — `GCP_LAMBO_CREDENTIALS` and `GOOGLE_APPLICATION_CREDENTIALS` (via
  `gcp_auth::credentials_path_from_env`, called eagerly at `src/embed/mod.rs:457` in
  `build_gemini_embedder` and again at `src/store/pg/mod.rs:1294` in `PgStore::new`), and
  `LAMBO_POSTGRES_IAM` (same line, at construction). These are worse than the DSN gap R3
  found: they choose an **identity**. A harness that clears the list, writes
  `embedder.kind = "gemini"` with no `gemini_credentials`, and believes itself hermetic
  authenticates as the developer's ambient service account and bills real Vertex calls to it.
  The docstring says "every variable the resolve reads" and the CHANGELOG says "**every**",
  in bold — the same overclaim R3 rated P2, one round after it was rated.
  R3's new test could not catch it: it asserts `RESOLVE_ENV_VARS` and the table name the same
  set, and both were written from one understanding of what a resolve reads. It catches a
  shrinking list, never an omission.
  `LAMBO_VECTOR_BEAM_SIZE` was checked and deliberately excluded — read in
  `PgStore::connect_options` on first pool use, not during the resolve. That distinction is
  now written down as the rule for the next addition.

- **P2 — the new `resolve_clean_ignores_an_ambient_promotion_policy` mutates the environment
  without holding the env lock.** `resolve_clean` takes `test_util::env_lock` itself, and it
  is a plain non-reentrant `Mutex`, so the test could not hold the guard across the call. It
  set `LAMBO_PROMOTION_POLICY=Solo`, dropped the guard, and then did two resolves, a file
  read and a file write with that value live in the process environment. Three of this
  change's own new tests assert on that exact variable while holding the lock they had every
  right to trust, in the same lib-test binary and therefore concurrently:
  `promotion_policy_env_beats_file_and_unknown_value_fails_closed` (sets `Swarm`, asserts
  `Swarm`) fails outright on the interleaving; `resolve_env_vars_clears_every_override_it_names`
  fails at step 3. A test that pins hermeticity had become the leak.

- **P3 — the unreachable fallback string was not made reachable, and the comment says it
  was.** R3 found `gateAbsenceCopy`'s policy-less line unreachable. The remediation added
  `|| d.promotion_policy` and a comment naming two producers. Neither exists: a post-C2 server
  always sets `policy` on the block *and* `promotion_policy` on the response, so the argument
  is never empty; a pre-C2 server carries neither — but serializes all four gate keys
  unconditionally, so `rendered` is 4 and `gateAbsenceCopy` is never called against it. The
  added carrier is present exactly when the first one is. The in-file `APP_JS.contains(..)`
  assertion is string-presence, so it passed either way.

### Remediation

- The three names are in `RESOLVE_ENV_VARS`, and each is pinned at its **real read site**
  rather than by re-reading the variable: the two credential names observe
  `gcp_auth::credentials_path_from_env()` under `embed-gemini` / `store-postgres`, and
  `LAMBO_POSTGRES_IAM` observes `store::pg::iam_auth_requested()` under `store-postgres` —
  both CI rows. A name whose read site is not compiled in a given row is `resolved: None`,
  which is a stated gap rather than a fabricated observation. `LAMBO_POSTGRES_IAM` gets an
  unconditional `store::POSTGRES_IAM_ENV`, asserted equal to the feature-gated const
  `PgStore::new` reads, so it cannot drift the way the DSN names could not. Proved by
  mutation: deleting one name from the const fails **both** halves — the set-equality test,
  and step 3, whose message is the hazard sentence itself.
- `resolve_clean` split into itself plus `resolve_clean_locked(cfg, &guard)`, so "I already
  hold the lock" is expressible and the whole set → resolve → restore sequence stays in one
  critical section. The scratch file is written before the guard is taken.
- The app.js comment and the serve_web assertion now say what is true: the fallback is a
  defensive default for a malformed or truncated payload, not a shape any Lambo release
  produces. Behaviour unchanged — the R4 brief's own principle is that changing behaviour to
  justify a string is backwards, and that applies to a second attempt as much as the first.

### Checked and cleared, not findings

- The `.config(config)` seam pin is load-bearing: deleting it from `serve_builder` fails
  `serve_builder_forwards_the_resolved_promotion_policy` (both the policy assertion, on the
  `Solo` iteration, and the `gc_interval` control).
- Every documented transcript reproduces byte-for-byte against the built binary: the
  top-level unknown-key refusal, the misplaced-key-under-`[store]` refusal, and the
  `LAMBO_PROMOTION_POLICY=Bogus` demo failure.
- The refusal-scope claim holds on the real binary: a bad `promotion_policy` stops
  `provision`, `saints`, `inspect` and `stats`, while `gc_interval = 0` passes all four and
  stops `recall`.
- `lambo.example.toml` parses.
- `scripts/docs/check-mirror-drift.sh` passes; the `docs/` and `site/` copies agree.
- `GateProgress::met_count` still has no production caller — the page counts rendered rows in
  JS. It is public library surface with tests, which is a defensible answer to R3's finding;
  not re-raised.
- `lambo stats` (the CLI verb) does not report the policy while `lambo_stats` (the MCP tool)
  does. Correct: the CLI verb is a lease-free reader whose own resolution is not the writer's,
  which is the same trap `serve-web` carries a warning about. Nothing claims otherwise.

## State after round 4

Every row below was run after R4's remediation, not carried over from the pause.

| Gate | Result |
|---|---|
| `cargo test --all --features fixtures` | 1008 passed / 0 failed |
| `cargo test --features store-sqlite,fixtures` | 1114 passed / 0 failed |
| `cargo test --no-default-features --features store-sqlite` | 661 passed / 0 failed |
| `cargo test --no-default-features --features store-postgres` | 643 passed / 0 failed |
| `cargo test --features embed-gemini` | 978 passed / 0 failed |
| `cargo test --no-default-features --features store-cockroach` | 638 passed / 0 failed |
| `cargo fmt --check` | clean |
| `cargo clippy --all-targets` (7 rows) | 0 warnings |
| `LAMBO_PROMOTION_POLICY=` / `=Solo` full suite | 1008 / 0 both |
| `node --check web/app.js` | OK |
| `scripts/docs/check-mirror-drift.sh` | passed |

The `store-postgres` and `embed-gemini` rows matter more after R4 than before it: they are the
two that actually execute R4-1's observations (`store::pg::iam_auth_requested` and
`gcp_auth::credentials_path_from_env`). On every other row those table entries are
`resolved: None` and the loop skips them, which is stated rather than hidden.

Diff growth across rounds: **433 → 965 → 1698 → 2324 → 2544** insertions, **7 → 16 → 25 → 26
→ 29** files (excluding this review record).
Tests added: **+28** on the default row; R4 added one (`store-postgres` row) and re-shaped two
rather than adding count.

## Acceptance, against the brief

- [x] `promotion_policy` on `LamboFile`, `LAMBO_PROMOTION_POLICY` env override, env beats file.
- [x] Default stays `Swarm`; unset config does not move existing behaviour.
- [x] Threaded onto the `Config` the serve path builds — and pinned at the seam, which was the
      one untested link when review started.
- [x] Unknown value fails closed at startup naming both the bad value and the valid set, on the
      file path and the env path.
- [x] Both arms asserted: a single writer deriving one Constraint across 7 event-times ≥24h
      apart reaches `Canonical` under `Solo` and nothing under `Swarm`. The `Swarm` arm is a
      live negative control — verified it cannot pass by timing out.
- [x] `cargo test` green, no live services.
- [ ] **No `serve --promotion-policy` flag** — deliberately not implemented; the spec called it
      optional and repo convention says no `serve` flag should duplicate a file key.

## Why this closed at four rounds

Merged into `lambo-for-mooshik` at `ad32954` (fast-forward from `71334f0`) after round 4,
**without** the empty round the earlier status banner demanded. That rule was retired
deliberately, and the reasoning belongs here rather than in a commit message.

The findings decayed in *kind*, not merely in severity:

| Round | Findings | Where they landed |
|---|---|---|
| R1 | 2×P1, 5×P2, 6×P3 | shipped behaviour — a startup error, an unparseable sample, a payload that lied under `Solo` |
| R2 | 3×P2, 8×P3 | payload and doc correctness |
| R3 | 1×P2, 5×P3 | vacuous safety, mostly tests and docs |
| R4 | 2×P2, 1×P3 | **entirely test infrastructure and one comment** |

Nothing R4 found touched what the binary does. The product surface has not moved since R2.
"Review until a round returns empty" is a stopping rule that can regress without limit on a
2900-line diff, because a motivated reviewer will always find a P3; convergence on
nothing-that-matters is the better signal, and that is what rounds 3 and 4 show.

Two residuals were checked before closing rather than assumed:

- **`RESOLVE_ENV_VARS` is `pub`, and R4's own remediation added three names to it** —
  including `GOOGLE_APPLICATION_CREDENTIALS`. A downstream harness that iterates the const to
  clear it would start clearing its own Vertex credentials. Checked against the actual
  consumer: Mooshik does not reference `RESOLVE_ENV_VARS` anywhere. Its credential use is a
  live-gated test in `src/memory/ops.rs` that skips when the variables are unset, plus
  `ingester/deploy/entrypoint.sh`, which *exports* them. No breakage.
- **Whether the addition is a semver break.** It is not: `RESOLVE_ENV_VARS` is itself new in
  the same Unreleased 0.3.0 section, so it ships with all nineteen names on its first
  release and no prior list exists to depend on. The CHANGELOG's "Added" placement is
  correct.

## If this code is revisited

Not a review backlog — nothing here blocks anything. These are the two places a future
author should look first, because they are the newest and least settled:

- **`resolved: None` rows in `resolve.rs`'s override table.** They are honest today (a read
  site that is not compiled cannot be watched), but they are also exactly where an omission
  would next hide. Derive the list from read sites, never from the table.
- **The `&MutexGuard` proof-of-lock token** on `resolve_clean_locked`. A convention this repo
  did not previously have. If a second one appears, make it the pattern or drop it; and note
  that R4 fixed the one env-lock leak it found without sweeping the tree for others.

Two carried-over items, neither belonging to this task:

- **`tests/t84_demo.rs::scenario_is_identical_twice_on_the_memory_store` is a pre-existing
  flake** — the demo's own 60s wall-clock deadline starving under CPU contention, reproduced
  ~1-in-6 under 24 CPU hogs *with this change reverted*. Worth a separate hardening task.
- **`lambo provision` does not validate daemon cadence.** `gc_interval = 0` passes under
  `provision`/`resolve_store_only` and fails under `serve`/`demo`. A bad `promotion_policy`, by
  contrast, is refused by every verb because the parse sits in `LamboFile::load_resolved`.
