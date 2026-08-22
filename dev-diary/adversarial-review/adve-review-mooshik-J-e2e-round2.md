# Adversarial review — workstream J E2E, round 2 (verification of the round-1 remediation)

**Reviewer**: independent E2E round-2 reviewer (Fable). Worktree branch
`worktree-agent-ae34d5414d819ef9f` (nine remediation commits `347df85..a90827f` on
`lambo-for-mooshik` @ `896b3c7`). The main checkout was not touched. Twelve temporary probe
mutations were made in the worktree (ten Rust/script mutations, two docs-gate probes), every
one reverted; `git status --porcelain` empty at review end and the post-revert sqlite suite
back at 1016/0/3.

**Verdict**: **REQUEST_CHANGES** — **no P1, no P2**. All twelve round-1 findings are honestly
closed at the artifact, and every closure mutation-tested went red exactly where claimed,
including at the release binary. The seven P3s below are all defects in the remediation's own
new material — a fused docstring pair, a demonstrated test blind spot on the round's
centerpiece wiring, two stated-as-absolute claims with real residuals, a missing artifact on
the newest degraded path, register slips in the remediation's own record, and a second
fixture vacuity beside the one the remediation itself confessed. Under the operator's
standing rule that a remediation round closes the P3s too, these gate the next pass, not the
design.

## Round-1 findings: one line each

| # | Status | Mutation run, result |
|---|---|---|
| JE2E-1 | **closed-verified** | cursor advance neutered (`if false &&`) → `the_refusal_pollers_window_advances…` red at the "window advances onto OUR newest row" assertion; green after revert. Retention DELETE verified on all three adapters (`sqlite.rs:840-852`, `cockroach.rs:2256-2267`, `memory.rs:482-503`), `LEASE_REFUSAL_RETENTION` build-guarded above `LEASE_TTL` (`lease.rs:127-133`), `(session_id, refused_at)` index as separate `IF NOT EXISTS` in both `001_init.sql`. Residual → R2-5 |
| JE2E-2 | **closed-verified** | licence made always-agree → `a_superseded_holders_exit_does_not_unlink…` red: "the live holder's socket must still be there". Covers both the fenced and the release-then-rebind windows. Residual → R2-3 |
| JE2E-3 | **closed-verified at the binary** | `lost > 0` guard restored around the append → `an_idle_proxy_books_the_degraded_state…` red with the ledger ending at the `proxying` line, exactly the round-1 shape; the failing dump also shows `counterparty`/`dialled` live in the wild. Reconnect books the closing `proxying` line (`proxy.rs:1340-1358`); `Dialled::Failed` stays silent with the per-retry argument written at the arm |
| JE2E-4 | **closed-verified at behavior** (both rulings recorded, dated 2026-08-22) | fence arm made never-complete → `losing_the_lease_winds_the_serve_down…` red: "the fence must wake the wind-down, not leave it parked". Exit is non-zero via `close()`'s fenced refusal through `run_and_close`; log names the winner; ordering inherited whole (close → poller/heartbeat/hub abort → identity-licensed unlink → ledger drain last). Gaps → R2-2, R2-4 |
| JE2E-5 | **closed-verified** | applied-state guard removed from `joined_facts` → verify.sh red on exactly 5 checks, as recorded; all five consumers moved (`_ledger.py` five kinds + `completions_by_receipt` + `joined_facts`; both reports join through the one implementation; README's "missing" paragraph gone; cli.mdx row names the five kinds in both mirrors). Second vacuity → R2-7; count slip → R2-6a |
| JE2E-6 | **closed-verified** | sweep recorded in §J4's as-built (85 hits, 5 stale, including the two no finding named); all five corrections verified at HEAD (`ledger.rs` open docstring rewritten on true premises, test doc, arming comment with `Ledger::open`'s absence explained, `authorize_bind`'s "What J4 changed", `PHASE-8-surface.md:1767`) |
| JE2E-7 | **closed-verified at the two source sites** | both restated at `ELECTION_BUDGET` = 20s with the argument re-made (10.2 s measured election cited); 27-hit figure reproduces at src/+tests/ scope. Scope residual → R2-6b |
| JE2E-8 | **closed-verified** | zero unticked boxes remain; duplicate J4 box deleted with an in-place note; J5 ticked with its evidence |
| JE2E-9 | **closed-verified** | the checklist's quoted line matches `writeq.rs:2212-2217` verbatim (values substituted) and the WARN semantics match `:2219-2227`; the "Nothing reaches the ledger" sibling corrected with history preserved in italics |
| JE2E-10 | **closed-verified, all three directions** | round-1 probe input (`](/lambo/config/#http-transport)` in the reference copy) → exit 1 naming file and line while the drift half still passed (proving the old false-pass); real tree green; shared-prose mutation in the site copy still red |
| JE2E-11 | **closed-verified** | `party_key` proxying arm removed → `j4_line_builders…` red; nulls re-checked (no kit script reads `lease` lines, no fixture carries one, mdx "holder" is prose only); evidence README annotated, not rewritten, no `.jsonl` touched |
| JE2E-12 | **closed-verified** | worker `map_err` reverted to raw → `an_embedder_refusal…` red showing the exact leak ("HTTP 500: input refused" in the receipt). All three sites verified at HEAD including the durable intent row's `summary` (both consume sites hold the model-safe form, so a restart answers safe strings); `Dropped` and the fenced arm untouched with reasons; `redact_urls` unweakened |

## New findings (all P3; all in the remediation's own material)

**JE2E-R2-1 (P3) — `d5de563` fused `shutdown_signal`'s docstring into `wind_down`'s; `shutdown_signal` is now undocumented and the sweep table records a site that no longer exists.**
*Evidence*: `src/mcp/serve.rs:2213-2268` is one contiguous `///` block ending at `async fn wind_down` — it opens "Ctrl-C (and SIGTERM on unix), so `close()` still runs" and asserts "Registration is EAGER: the handlers are installed when this function is *called*", both about `shutdown_signal`, both false of `wind_down` (an `async fn` that installs nothing at call time; the eagerness lives in the `shutdown_signal()` argument evaluation at `serve.rs:1598`). `fn shutdown_signal` (`:2284`) carries no docs. The JE2E-4 half is even glued on with no blank `///` separator (`:2234-2235`). `26c906c` then edited the fused block and its diary sweep row names the corrected site "`serve.rs` `shutdown_signal` docstring" (`J-multi-client.md:2654`) — at HEAD that text documents `wind_down`.
*Failure scenario*: a later editor replaces or moves the `signal` argument; the eager-arming contract — the load-bearing R2-a mechanism — is documented on the wrong function and hover/rustdoc on `shutdown_signal` shows nothing, so nothing warns them.
*Close*: split the block — restore the Ctrl-C/eager-registration paragraphs onto `shutdown_signal`, give `wind_down` its own summary line above its two JE2E-4 sections, and fix the diary row.

**JE2E-R2-2 (P3) — the exit-on-fence *wiring* is test-blind: severing it passes the entire suite.**
*Claim tested*: the ruling's deliverable is "a fenced holder winds down"; the commit's "Pinned by" names two tests.
*Evidence*: mutating `serve.rs:1598` so the transport's shutdown future is plain `shutdown_signal()` (wind_down constructed but unused) → **1016 passed / 0 failed**, the full store-sqlite suite green. The two new tests pin `wind_down`-the-function and the fenced `run_and_close` in isolation; nothing pins that `serve()` actually feeds `wind_down` to the transport, which is the one line the ruling's behavior lives in. (Mutation reverted; suite re-run green.)
*Failure scenario*: any refactor of `serve()`'s shutdown plumbing silently restores live-forever fenced holders; every gate stays green.
*Close*: pin the composition — cheapest is a serve-level test that drives the stdio/http transport with a `Memory` whose fence is latched via `simulate_lease_loss_to` and asserts the transport future resolves (the pieces for this already exist in `close_runs`); a binary-level self-heal test would need a real 45 s TTL expiry and can be declined with the gap stated.

**JE2E-R2-3 (P3) — the unlink licence's "can never match … by construction" is stated as absolute; it has two narrow holes.**
*Evidence*: `endpoint.rs:110-115` ("a superseded endpoint's identity can never match and its owner can never delete a live successor's socket — by construction"). (a) The identity is captured by a path `stat` an instant *after* `bind` (`serve.rs:1679`); a >45 s wedge inside that instant captures the successor's inode as ours. (b) On the clean-close path, `hub.abort()` (`serve.rs:1744-1746`) drops the listener's reference to the old inode *before* `unlink_if_ours` runs; a successor that wins the released lease and binds inside that gap frees the old inode at its own unlink, and on an inode-recycling filesystem (ext4-style first-free allocation; not tmpfs/APFS, the defaults) its fresh socket can be handed the same `(dev, ino)` back — the exit then unlinks the live successor's socket through a licence that "matches".
*Why P3*: both windows are microseconds wide on non-default preconditions; the demonstrated round-1 failure modes are genuinely closed, and the default endpoint directories (tmpfs `$XDG_RUNTIME_DIR`, APFS `/tmp`) never recycle inode numbers.
*Close*: widen the identity to `(dev, ino, birth-or-ctime)` — a recreated file cannot share all three — or soften the docstring's "never … by construction" to name the filesystem and capture-instant assumptions it rests on.

**JE2E-R2-4 (P3) — a lease loss books no ledger artifact; the biggest lease event a holder can suffer is stderr-only on the idle path.**
*Claim*: §J4's bar is "lease conflicts leave an artifact"; the ruling's record says the ledger drains last "with its startup/lease/completion lines intact".
*Evidence*: `wind_down`'s fence arm emits only `tracing::warn!` (`serve.rs:2271-2280`); `close()`'s fenced branch emits only `tracing::error!` (`memory.rs:2217-2223`). Completion `failed` lines appear only if writes were in flight at the fence — and the commonest fence, like the commonest holder death (JE2E-3's own argument), is idle. An idle fenced holder's ledger simply stops.
*Failure scenario*: operator asks the shared ledger "why did this holder exit / why did its client lose memory at T" — the serve's story ends mid-air; the answer exists only in stderr the client may have swallowed. The reconstruction via the respawn's `startup` + `proxying` lines is possible but inferential — exactly the state JE2E-3 was filed against.
*Close*: append `kind:"lease", event:"lost", side:"holder"` (counterparty = winner) in the fence arm before the transport is cancelled — the ledger drains after `close()`, so the line survives by the existing ordering; `party_key`'s fallback already labels it correctly.

**JE2E-R2-5 (P3) — the RefusalCursor can permanently skip a refusal whose store stamp becomes visible below an already-advanced cursor.**
*Claim*: "onto, not past" plus per-poll dedup covers everything the store can re-deliver (`serve.rs:1274-1283`), and it does — but only rows the store *delivers*.
*Evidence*: on Cockroach, `refused_at = now()` is the transaction's read timestamp while visibility is commit-ordered (`cockroach.rs:2246-2252`): loser L1's INSERT starts at t=10.0 and commits slowly at t=10.5 (push-with-refresh keeps the evaluated `now()`); loser L2 commits fast at 10.2; a poll between the commits advances the cursor to 10.2; L1's row, stamped 10.0, is then excluded by `refused_at >= since` forever. The pre-remediation frozen window could not skip (that was its defect's flip side); the fix traded unbounded re-reads for this window. SQLite (one wall clock, ms text stamps, lossless round-trip through `ts_to_text`) is not exposed except by clock steps.
*Why P3*: the window is milliseconds per refused start; the loser's own `refused` line and the store row (1 h retention) survive; only the holder-side `refused_takeover` line is lost, and the recorder is documented best-effort.
*Close*: read from `cursor − ε` (ε ≈ 1 s / max clock offset) and dedup by `(refused_by, at)` within the overlap — the cursor type already has the shape for it — or state the residual where the docstring currently says older rows "cannot be re-logged" without saying they can also never be logged.

**JE2E-R2-6 (P3) — register slips in the remediation's own record.**
(a) The per-finding closure table says "`verify.sh` 46 → **55** ok" (`J-multi-client.md:2772`) and `d4504ee`'s message says the same; measured: **56 ok** (the `--json` step's ok counts), and the gates table in the *same section* (`:2885`) says 56. The register contradicts itself 113 lines apart — the exact JE2E-8 class.
(b) JE2E-7's sweep scoped itself to source (its 27 hits reproduce at `src/`+`tests/`; tree-wide is 79): `J-multi-client.md:669` still asserts, present tense inside §J2's as-built, "a loop allowed to run **50 seconds** by design (J2-R1-7)" — contradicting the Done-when box's own 20 s arithmetic (`:2961-2962`) — and the J2-R2 register row JE2E-7 itself convicted ("the 31.96s / 20s / 50s figures re-checked and **still true**", `:1264`) stands un-annotated, while the convicted `serve.rs` sites got in-place correction parentheticals. The family lives in more prose sites than any one of them signals — the workstream's own carried sentence, applying to the sweep that quoted it.
(c) The sweep row naming "`shutdown_signal` docstring" is R2-1's fused block at HEAD.
*Close*: fix the two numbers, annotate `:669` and `:1264` in place, re-point the sweep row.

**JE2E-R2-7 (P3, advisory) — the strengthened completion-join fixture has a second vacuity: `APPLIED_STATES` widened to admit `deferred` survives green.**
*Evidence*: mutating `_ledger.py:363` to `("applied", "applied_after_restart", "deferred")` → verify.sh **56 ok, ALL CHECKS PASSED** (reverted). The fixture's `deferred` line (`r-4`) carries no counts *and* is shadowed by a later terminal line, so nothing opposes the state — precisely the argument the remediation recorded about the first vacuity ("nothing lambo emits today puts counts on a non-applied line") applied one state over. Honest caveat: this mutant is production-equivalent today; so was the un-strengthened guard before the 40/40 `failed` line was planted.
*Close*: plant one receipt whose *terminal* completion is `deferred` with absurd counts, mirroring the `failed` one.

## Where the remediation held under attack (for the record)

The JE2E-2×4 composition walked clean on every other path: clean SIGTERM (identity matches → own socket removed), fence-triggered wind-down (successor bound → skip with an explaining log; successor not yet bound → own cleanup), fence during close (early check; store-side fence backstops the late latch), SIGTERM during wind-down (`close_bounded` re-arms), bind-failure-after-lease (`bound_socket = None` → nothing removed), double-release (fenced close and abandoned-close release both refuse, holder-scoped release cannot evict the winner), no forever-leak (any survivor is cleared by the next bind under the lease licence). The fenced quiesce settles in-flight jobs `failed` with the hand-written lease message, intents deliberately unconsumed for the successor's replay, completion lines written before the ledger drains — acked writes are not destroyed by the exit, and the log line says exactly what is discarded. The retention arithmetic holds: an unread-purge needs a >1 h poller stall, and any >45 s stall now fences and winds the holder down first. N4 on all new surfaces: the `counterparty`/`dialled` values and the `wind_down` line are operator-facing (JSONL/stderr); the one model-facing surface (the Failed receipt) now carries the class, mutation-proven.

## Gates (all re-run in the worktree at the committed tree)

| Gate | Claimed | Measured |
|---|---|---|
| `cargo fmt --all -- --check` | clean | clean |
| `cargo test --all --features fixtures` | 918 / 0 / 3 | **918 / 0 / 3** |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 1016 / 0 / 3 | **1016 / 0 / 3** |
| `cargo clippy --all-targets --features store-sqlite,embed-fixture,fixtures -- -D warnings` | clean | clean |
| `scripts/observability/verify.sh` | 55 (table) / 56 (gates row) | **56 ok, ALL CHECKS PASSED** (→ R2-6a) |
| `scripts/docs/check-mirror-drift.sh` | green; red both probe directions | **green; exit 1 on the round-1 probe input naming file+line; exit 1 on shared-prose drift** |

K-workstream material brought in by the rebase (`spikes/`, `evidence/`, `scripts/embedder/`, K doc): no gate tripped over it; not reviewed.

## Cleanliness

All twelve probe mutations reverted (`_ledger.py` ×2, `docs/reference/mcp.mdx`, `site/.../mcp.mdx`, `serve.rs` ×2, `endpoint.rs`, `memory.rs`, `ledger.rs`, `writeq.rs`, `proxy.rs`, plus scratchpad copies outside the repo). Final `git status --porcelain` empty; the post-revert sqlite suite re-ran **1016 / 0 / 3**. The round-1 review's `r2-shape.stderr` debris is absent from this worktree (it lived in the main checkout and has since been deleted there).
