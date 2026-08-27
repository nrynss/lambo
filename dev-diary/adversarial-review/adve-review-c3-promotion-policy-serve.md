# Adversarial Review: C3 — `promotion_policy` on the serve surface

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: IN PROGRESS — paused after round 3 remediation              ║
║  Verdict: NOT YET CLEAN. Rounds 1-3 closed; round 4 was stopped      ║
║    in its reading phase and produced NO findings. The change has      ║
║    never been through a review round that returned empty.             ║
║  Gates at pause: 1008 passed / 0 failed / 3 ignored (fixtures),       ║
║    1114 (store-sqlite,fixtures), 661 (no-default,store-sqlite);       ║
║    fmt clean; clippy 0 in all five CI feature rows.                   ║
║  Findings: R1 13 (+1 self-found) · R2 11 · R3 6 (+4 nits, +3 self)    ║
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

## Round 4 — stopped, no findings

Stopped in its reading phase before producing any findings, so it contributes **nothing** to the
verdict. It was briefed to attack two things specifically, and both remain open questions:

1. **The `promotion_policy!` macro was not asked for.** The finding asked only that adding a
   variant cannot compile-or-pass. A macro-generated public enum is a heavier instrument and may
   cost rustdoc output, IDE navigation, and readability for every future reader. Its stated
   justification — that any exhaustive-`match` scheme needs a second, independent count of
   variants to bound arms against, which stable Rust cannot provide — is plausible but
   unverified, and worth testing before this lands.
2. **A fallback string was made "reachable" by adding a payload path.** Changing behaviour to
   justify keeping a string is backwards. Needs checking that it did not invent a state no
   server produces, or disturb the `unavailable`/`already_canonical` semantics R3 fixed one
   finding earlier.

---

## State at pause

| Gate | Result |
|---|---|
| `cargo test --all --features fixtures` | 1008 passed / 0 failed / 3 ignored |
| `cargo test --features store-sqlite,fixtures` | 1114 passed / 0 failed |
| `cargo test --no-default-features --features store-sqlite` | 661 passed / 0 failed |
| `cargo fmt --check` | clean |
| `cargo clippy` (all five CI feature rows) | 0 warnings |
| `LAMBO_PROMOTION_POLICY=` / `=Solo` full suite | 1008 / 0 / 3 both |
| `node --check web/app.js` | OK |

Diff growth across rounds: **433 → 965 → 1698 → 2324** insertions, **7 → 16 → 25 → 26** files.
Tests added: **+28** on the default row.

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

## Resuming

The work is committed on `task/promotion-policy-serve`, not merged. Next step is a round-4
review from a clean start, weighted at the two open questions above rather than re-covering
rounds 1-3 — every finding from those was confirmed closed by mutation testing, except where
this doc says otherwise.

Two carried-over items, neither belonging to this task:

- **`tests/t84_demo.rs::scenario_is_identical_twice_on_the_memory_store` is a pre-existing
  flake** — the demo's own 60s wall-clock deadline starving under CPU contention, reproduced
  ~1-in-6 under 24 CPU hogs *with this change reverted*. Worth a separate hardening task.
- **`lambo provision` does not validate daemon cadence.** `gc_interval = 0` passes under
  `provision`/`resolve_store_only` and fails under `serve`/`demo`. A bad `promotion_policy`, by
  contrast, is refused by every verb because the parse sits in `LamboFile::load_resolved`.
