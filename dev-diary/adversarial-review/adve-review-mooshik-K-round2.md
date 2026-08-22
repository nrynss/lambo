# Adversarial review — mooshik K (candle embedder, K2), round 2

**Reviewer**: independent adversarial reviewer, agent_id `K2Review2`. Wrote nothing under
review except this file.
**Scope**: the four remediation commits on `lambo-for-mooshik` — `60a8399` (adapter
findings K2-R1-1..5, 7..10), `519cc92` (re-embed lease), `a55f966` (operator docs),
`890e0ee` (closure claims) — against the eleven findings of round 1
(`adve-review-mooshik-K-round1.md`). Round 1 verdict was REQUEST_CHANGES
(1 P1 / 4 P2 / 6 P3).
**Worktree**: `/home/nryn/work/lambo`, branch `lambo-for-mooshik` @ `890e0ee`
(verified before starting).
**Verdict**: **APPROVE** — all eleven closures hold, zero failed closures, zero new
findings.

## Method

1. Read the '## Round 1 closures' section and every claimed fix, then read the full
   remediated sources: `src/embed/candle.rs` (1221 lines), `src/cli/re_embed.rs`,
   `src/cli/mod.rs::close_writer`, and the diffstats of the three code commits
   (`60a8399`: candle.rs + resolve.rs only; `519cc92`: re_embed.rs only; `a55f966`:
   README + both cli.mdx + lambo.example.toml).
2. Verified dependency-level claims at the source: hf-hub 0.4.3
   `CacheRepo::get` (`src/lib.rs:141-151`) is pure filesystem — ref-path read +
   pointer-path existence check, no network call anywhere — so `cache_get` cannot
   dial out even as a side effect (the mechanism K2-R1-3's fix rests on).
3. **Mutation-tested eight of the eleven closures**: reverted/broke the specific fix
   in the working tree (transient edits, never committed; `git checkout --` revert
   after each, tracked-changes count confirmed 0 after every cycle) and ran the
   claimed regression test against the mutated code. A closure counts as
   mutation-verified only if its test FAILS under the mutation.
4. Re-ran all gates myself on the pristine tree (results below).
5. Hunted for defects introduced BY the fixes across exactly the flagged
   concurrency-sensitive surfaces: supervisor/respawn, poison-tolerant locks,
   bounded waits, offline cache path, hash-before-load ordering, identity stamping.

## Part A — per-finding closure verification

| Finding | Verdict | Verification |
| --- | --- | --- |
| K2-R1-1 (P1) identity from loaded bytes | **HOLDS** | Mutation: `stamp_identity` forced to use the compile-time `WEIGHT_SHA256[..12]` regardless of input digest → `override_identity_describes_the_loaded_artifact_not_the_default_stamp` **FAILED** (asserts the override stamp carries neither the f16 prefix nor the default shape), `identity_stamps_source_revision_and_sha_prefix` still passed (canonical stamp preserved). Wiring traced in `new`: `sha256_file(&weights_path)` runs unconditionally at `candle.rs:585` on EVERY path (hub_get / offline cache_get / weights_dir) BEFORE either stamp branch; the override branch stamps the computed digest plus effective repo@revision or `dir:` source (`candle.rs:609-628`). The constant-stamp path from round 1 is unreachable. Honest limit: the two tests pin the pure function, not `new`'s wiring (no weightless construction succeeds); the wiring itself is trace-verified line-by-line above. |
| K2-R1-2 (P2) coalescer drops beyond max_batch | **HOLDS** | Mutation: initial take unclamped (`let take = q.len()`) → `initial_take_never_exceeds_max_batch_and_leftovers_survive` **FAILED** (first batch 50-wide instead of clamped to 32). Source: `q.clear()` and the dead `notify_all` are gone; `wait_for_batch` clamps (`candle.rs:169`), `top_up` bounds by remaining capacity (`candle.rs:186-187`), leftovers stay queued and are picked up by the next loop iteration. Both halves of the round-1 defect are dead. |
| K2-R1-3 (P2) offline dials network | **HOLDS** | Mechanism verified in hf-hub 0.4.3 source first (see Method §2). Mutation: `cache_get` body replaced with `hub_get(repo, revision, filename)` (network path) and run with `HF_ENDPOINT=http://127.0.0.1:9` → `offline_weight_resolution_is_cache_only_and_loud` **FAILED** (error text lacks "not cached"/"fetch once online"). Weights, config.json, and tokenizer.json all route through `cache_get` under `offline = true` (`candle.rs:563-579`). |
| K2-R1-4 (P2) width-vs-dim check | **HOLDS** | Mutation: dim guard disabled (`if false && dim != BGE_M3_DIM`) → `wrong_dim_is_refused_at_construction_before_any_resolution` **FAILED** (construction proceeded past the guard toward resolution). Guard sits at `candle.rs:535-541`, before device and weight resolution; the second layer (coalescer refuses forward output whose width ≠ pinned `Shared::dim`, `candle.rs:781-790`) traced as defense in depth. |
| K2-R1-5 (P2) wedge on dead coalescer | **HOLDS** | Mutation: `recv_bounded`'s dropped-sender arm rewritten to return `Ok(Vec::new())` → `bounded_wait_reports_a_dropped_sender_as_a_named_error` **FAILED**. All three layers present in source: `catch_unwind(AssertUnwindSafe(..))` around the forward with the panic failing only that batch (`candle.rs:774-809`); locks via `unwrap_or_else(unpoison)` everywhere (`candle.rs:175,198,202`); supervisor respawn loop (`supervise`, `candle.rs:734-748`); bounded wait mapping stall→Unavailable / dropped-sender→named Backend error (`recv_bounded`, `candle.rs:836-852`). Honest limits, stated by the remediator and confirmed: no weightless test kills a real worker or exercises catch_unwind through a live `Shared` (needs weights) — those paths are trace-verified; the panic/drop arms themselves are mutation-verified. |
| K2-R1-6 (P3) lease held past abort | **HOLDS** | Mutation: reintroduced the pre-fix early return (`.await?` out of `run()` before `close_writer`) → `re_embed_embed_failure_still_releases_the_lease` **FAILED** with exactly the round-1 symptom: a different holder's `acquire_lease` got `Held { holder: "operator@…", token: 1 }`. Fixed code funnels success AND every mid-run abort through `close_writer(mem, out)` (`re_embed.rs:94-110`), which releases via `Memory::close` on both arms (`cli/mod.rs:115-128`). |
| K2-R1-7 (P3) coalescer never exits | **HOLDS** | Mutation: shutdown made to preempt a non-empty queue (`if !q.is_empty() && !*self.lock_shutdown()`) → `shutdown_exits_only_after_draining_queued_work` **FAILED** ("queued work drains first"). Real semantics: drain-first-then-exit (`candle.rs:165-177`), stragglers racing in at exit are failed with a named error (`drain_all` arm, `candle.rs:752-761`), `Handle::drop` fires only on the LAST handle (`Arc<Handle>`, `candle.rs:505-517`). Drop-order trace: last handle sets shutdown → worker exits after drain → supervisor joins and exits → `Arc<Shared>` (and the model) drop. |
| K2-R1-8 (P3) hash-after-load | **HOLDS (trace)** | Order in `new` is resolve paths → `sha256_file` (`candle.rs:585`) → pinned-digest refusal (`candle.rs:592-598`) → `load_core` (`candle.rs:600`). No behavioral test exists without weights (closure says so plainly); the ordering is source-visible and I re-derived it independently. Mutation impractical: any reorder compiles identically weightless and no gate observes it — recorded honestly as trace-verified. |
| K2-R1-9 (P3) accelerator misreported | **HOLDS** | Mutation: auto-CUDA-miss arm changed to `no_accelerator("an accelerator", ..)` → `auto_names_the_missing_accelerator` **FAILED**. `no_accelerator(accelerator, err)` takes the name directly (`candle.rs:395-402`); all four auto-arm sites pass "Metal"/"CUDA". |
| K2-R1-10 (P3) dead code / typo | **HOLDS** | Grep over `src/embed/candle.rs` for `_weight_file_path`, `resolve_hf`, `resolve_offline`, `hf_path`, `verify_hash`: zero hits. Exactly two helpers remain (`hub_get` fetch-or-cache :647, `cache_get` cache-only :663). Typo fixed at `src/resolve.rs:158` ("cannot hide a **swap** between two quantizations"). Dead-code absence is additionally enforced by `cargo clippy --all-targets --features embed-candle -- -D warnings` passing (re-run below). |
| K2-R1-11 (P3) operator-facing docs / cited tests | **HOLDS** | Cold-start numbers (~2–3 s warm; ~60 s / ~1.1 GB one-time fetch) present in README.md, `docs/reference/cli.mdx`, `site/src/content/docs/cli.mdx`, and `lambo.example.toml`; `scripts/docs/check-mirror-drift.sh` passes on my run. The cited ignored live test now EXISTS: `embed::candle::tests::live_weights_load_and_embed_on_cpu` (`#[ignore] #[tokio::test]`, `candle.rs:1188-1213`), so the CI-row comment's reference to ignored tests is true as written (lib suite reports it: 2 ignored). Parity-in-shipped-adapter and task-7 migration remain declared-open operator items, not defects. |

Mutation score: **8/8 attempted mutations were caught by the claimed regression
tests** — none of the eight tests passed regardless of the fix, i.e. none is a
vacuous pin. The three closures without a feasible weightless mutation (R1-1's
wiring half, R1-5's live-worker half, R1-8) are trace-verified and flagged as such
above.

## Part B — hunt for defects introduced by the fixes

No new findings. Specific attack vectors examined:

- **Supervisor/respawn**: single-worker invariant holds (supervisor joins before
  respawning — no concurrent double-drain of the queue). Lock order is uniform
  (queue lock never taken while holding shutdown lock, nor vice versa) → no lock-
  order deadlock between `push`, `request_shutdown`, `wait_for_batch`, `top_up`.
  A mid-batch worker death drops that batch's senders (callers get the named
  "dropped the request" error) while leftover queued work is picked up by the
  respawned worker — verified by the shutdown-drain semantics and the respawn
  trace. Residual edge: the supervisor's inner worker `spawn(...).expect` would
  kill the supervisor on a thread-spawn failure (OOM-class); consequence is the
  600 s bound firing rather than a hang — bounded and named, not elevated to a
  finding.
- **Poison tolerance**: `unpoison` recovers the queue mutex, shutdown mutex, and
  condvar wait; a panic while poisoned leaves at worst a partially-drained queue,
  which the next iteration drains normally. No state machine assumes lock health.
- **Bounded wait**: timeout leaves no dangling caller expectation — a late result
  sends into a closed channel and is dropped harmlessly; a timed-out request whose
  Pending is still queued is failed at shutdown or served by the respawned worker,
  never lost silently.
- **Offline cache path**: pure filesystem (verified in hf-hub source); error text
  names repo@revision and file; config/tokenizer arms share the same helper, so no
  network path remains under `offline = true`.
- **Hash-before-load**: hash now precedes VarBuilder/model construction on all
  paths; the only pre-hash work is path resolution/fetch, which is inherent (the
  bytes must exist to be hashed).
- **Identity stamping**: default-artifact predicate requires absence of
  `weights_dir` AND exact repo+revision+file match, so every override — including
  a `weights_dir` holding byte-identical shipped weights — stamps its own source
  string. Side effect: migrating default↔explicit-dir of the SAME artifact changes
  the contract string and forces an explicit re-embed. That is the conservative
  direction (refuse-and-reembed, never silent accept), consistent with task 3's
  intent; noted as behavior, not a defect.
- **`debug_assert_eq!(vectors.len(), batch.len())`** is debug-only, but a release
  mismatch cannot strand a caller silently-hanging: zip leaves extra senders
  dropped → callers get the named dropped-request error. Acceptable.
- **re_embed funneling**: `close_writer` composes op-result and close-result on
  all four arms (`cli/mod.rs:120-127`); the abort path flushes an empty log and
  releases the lease — no partial-flush window introduced.

## Part C — gates rerun (my own runs, tree @ 890e0ee, pristine)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **pass** |
| `cargo clippy --all-targets -- -D warnings` | **pass** |
| `cargo clippy --all-targets --features embed-candle -- -D warnings` | **pass** |
| `cargo test --features embed-candle` | **902 passed / 0 failed / 4 ignored** across all suites (lib suite: 890 passed / 0 failed / 2 ignored — the two being the declared `#[ignore]`d live tests) |

No gate finding.

## Operator-leg items remaining open (not review findings)

As flagged by the remediator and unchanged by this round, pending live
weights/GPU: (1) the supervision lifecycle exercised against a real dying worker;
(2) parity-in-shipped-adapter (still spike-only status from K1); (3) task 7, the
dogfood migration act itself. The code does not contradict itself on any of these.

— K2Review2, 2026-08-23
