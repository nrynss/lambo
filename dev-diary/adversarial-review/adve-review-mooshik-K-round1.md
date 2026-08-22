# Adversarial review — mooshik K (candle embedder, K2), round 1

**Reviewer**: independent adversarial reviewer, agent_id `K2Review`. Wrote nothing under
review except this file.
**Scope**: the five commits `c9b2ecd..db555bd` on branch `k2-candle` — `8e564fa` (the
candle adapter), `b29b262` (`Graph::reembed_all`), `2db6ae5` (`lambo re-embed` +
coverage counter), `b521ae4` (CI row), `db555bd` (docs). Against §K2 of
`lambo-for-mooshik/K-candle-embedder.md` (tasks 1–7 + Done-when) and
`notes/level-b-pluggability.md`.
**Worktree**: `/tmp/lambo-k2`, branch `k2-candle`.
**Verdict**: **REQUEST_CHANGES** — one **P1**, four **P2**, six **P3**.

The load-bearing architecture claims survive the attack: the re-embed atomicity argument
is **correct** and I verified it end-to-end at the source (single write-lock acquisition
→ whole-log drain → one store transaction, both adapters); the Level B shape is intact;
the coverage counter is honest (canonical strategy reports 0/N and a test pins it); all
three claimed gates pass under my own run. What fails is narrower and fixable, but the
P1 goes to the heart of K2 task 3: **the stamped contract identity is built from compile-time
constants, not from the artifact actually loaded**, so the exact quantization swap task 3
was written to make machine-checkable is again invisible whenever an operator uses any of
the three documented weight-source overrides.

## Method

1. Read §K (all 352 lines incl. both K1 legs and the f16-artifact amendment) and
   `level-b-pluggability.md`, then the full `git diff c9b2ecd..HEAD` (18 files) and the
   complete new files `src/embed/candle.rs` (720 lines) and `src/cli/re_embed.rs`
   (472 lines).
2. Traced the atomicity claim through `src/store/batch.rs` (barrier planning),
   `src/store/flush.rs` (drain/tick semantics), `src/store/sqlite.rs::flush`
   (one BEGIN..COMMIT per batch, fencing inside the tx), `src/store/cockroach.rs::flush`,
   `src/memory.rs` (`build_attach` reembed_mode skip, `close()` final flush,
   heartbeat/TTL), and `src/cli/mod.rs::close_writer`.
3. Checked candle-core 0.11.0 / tokenizers 0.21.4 / hf-hub 0.4.3 vendored sources where
   behavior under review lives in the dependency, not the diff (Device constructor cfg
   gating; `TruncationParams` defaults; `Api::get` cache-fallback semantics).
4. Re-ran the implementer's gates myself: `cargo fmt --all -- --check` (pass),
   `cargo clippy --all-targets --features embed-candle -- -D warnings` (pass),
   `cargo test --features embed-candle` (**867 passed, 0 failed**, plus small suites).
   No gate finding.
5. Executed a faithful simulation of the coalescer's debounce/drain algorithm
   (lines 591–614 of `candle.rs`) to confirm the request-drop arithmetic (K2-R1-2).
   The simulation reproduces the code's structure exactly (`mem::take`, sleep-debounce,
   `min`-clamped tail drain, then the `q.clear()` under review).

Order of authority: §K2 is the claim; the source is what ships; where a docstring and
the code disagree, the code wins and the disagreement is a finding. Two findings below
are exactly that.

## Part A — surface-by-surface adjudication

| Surface | Verdict | Evidence |
| --- | --- | --- |
| Metal/Apple compile path | **HELD** | candle-core 0.11.0 defines `Device::new_cuda`/`new_metal` **unconditionally** (`device.rs:233,258`; stub backends return runtime `Err` without the Cargo feature), so the ungated calls in `resolve_device`'s auto arm compile everywhere — including plain `--features embed-candle` on Linux CI and a metal-less macOS build. Layer features coherently flip the backend features (`embed-candle-metal = ["embed-candle", "candle-core/metal", …]`). Device order (macOS→Metal, else→CUDA), hard-error-unless-pinned rule, and f16-GPU/f32-CPU dtype split all match spec §K2. Two cosmetic message defects: K2-R1-9 |
| Re-embed atomicity & replay | **HELD — the headline claim is true** | `reembed_all` appends all UpsertNodes + trailing SetEmbedding inside ONE `graph.write()` acquisition (`re_embed.rs:119-126`); `FlushTask::cycle` drains the whole log under the same write lock (`flush.rs:474-479`) and never splits a pending batch across flushes (the `max_batch` gate at `flush.rs:521` only decides *whether*, and the tick path flushes `pending` whole); `SqliteStore::flush` wraps plan steps in one transaction with the fencing-token check inside it (`sqlite.rs:941-1016`); Cockroach mirrors begin/commit (`cockroach.rs:2357-2374`). So the durable session moves old-consistent → new-consistent in one commit; a crash rolls back to old contract + old vectors together. Refusals are atomic and tested (`k2_reembed_all_refusals_are_atomic_and_named` asserts snapshot equality + empty drained log); duplicate-id, partial-coverage, width-change, identical-contract all refused; batch ordering pinned by `k2_reembed_all_rewrites_vectors_then_swaps_contract_in_order` and by batch.rs's `every_non_upsert_variant_is_a_barrier` (SetEmbedding flushes every open bucket first) |
| reembed_mode bypass | **HELD, one wart** | `pub(crate)`; grep confirms the sole caller is `re_embed.rs:75`. Backends flow from one resolve; the verb's direct builder construction is forced (open_writer cannot express reembed_mode) and passes `ResolvedBackends` fields through unchanged. On a failure between attach and rewrite the durable state is untouched and the OLD contract stays stamped durably (correct no-op) — but the lease is held to TTL, see K2-R1-6 |
| Batch coalescer | **BROKEN under burst** | K2-R1-2 (request drops beyond max_batch; MAX_BATCH not enforced on the initial take), K2-R1-5 (thread-death wedge), K2-R1-7 (thread never exits). Lone-call latency bound (2 ms debounce) and lock-free-await shape are otherwise right |
| Contract identity | **FALSIFIED** | K2-R1-1. bge_m3's own stamping IS unchanged byte-for-byte (resolve.rs `else` branch is the pre-K2 expression verbatim) — that half holds |
| 512-token clamp class | **HELD** | `with_truncation(TruncationParams { max_length: 8192, .. })` is applied via the checked `Tokenizer::with_truncation` (Result mapped, `candle.rs:337-342`). tokenizers 0.21.4 defaults: `TruncationDirection::Right` (head kept — correct for content prefixes) and `LongestFirst`. Padding pad_id is the model's pad token, not tokenizers' 0, with the position-id rationale documented (`candle.rs:343-353`) — this is the K1 "plausible but wrong" class handled correctly |
| Coverage counter | **HELD** | `Memory.stats()` counts `embedding.is_some()` under the read lock (`memory.rs:1978`); `lambo stats` prints `embedded=N/M` from the durable reader snapshot (`stats.rs:24-27`); `lambo_stats` exposes `embedded_concepts`/`total_concepts` with the key-contract test updated (`server.rs:5679-5683`); canonical-strategy honesty pinned by `k2_stats_payload_reports_embedding_coverage` asserting **0/2** on two applied concepts |
| Level B conformance | **HELD** | `deny_unknown_fields` intact on EmbedderConfig/LamboFile/StoreConfig; unknown/uncompiled kind hard-errors preserved (`is_compiled` message pre-check + cfg'd registry arms, both present for Candle); single construction site respected; CI row is genuinely weightless (every candle.rs test is pure — see K2-R1-11 for a wording nit) |

## Part B — findings

### P1

**K2-R1-1 — The stamped identity describes the DEFAULT artifact, not the loaded one;
the documented weight-source overrides silently revert to the default stamp.**
Evidence: `src/embed/candle.rs:489-493` builds `identity` unconditionally from the
constants `SOURCE_REPO`/`SOURCE_REVISION`/`weight_file`/`&WEIGHT_SHA256[..12]`;
`verify_hash` runs only under `repo == WEIGHT_REPO && weight_file == DEFAULT_WEIGHT_FILE`
(`candle.rs:476-478`); the overrides are accepted three lines earlier (`candle.rs:405-413`)
and documented in `lambo.example.toml` (`weights_file = "model.safetensors"` alongside a
commented `repo = "BAAI/bge-m3"`, `weights_dir = …`) and in `CandleOpts` ("pytorch_model.bin"
is a named option, `candle.rs:85`).
Scenario (traced): operator sets `[embedder] kind="candle" weights_file="pytorch_model.bin"`
(the canonical fp32 artifact both K1 legs measured). Load succeeds via `from_pth`, hash
verification is skipped by design, and the session contract is stamped
`BAAI/bge-m3@5617… model.safetensors sha256:68440cc1b73b` — describing the f16
safetensors that was NOT loaded. Migrating the same session back to (or between) any other
artifact produces a byte-identical contract, so `ensure_compatible` passes and the
quantization swap is invisible — precisely the failure K2 task 3 declares it is ending
("a kind/dim match can no longer hide a swap between two quantizations"), and precisely
what the implementer's own K2 notes claim ("the contract's model field carries the loaded
artifact's identity"). It also means the fp32-vs-f16 distinction is unstamped on the very
migration path the dogfood rig would take if it ever ran from_pth.
Fix: derive the identity from what was actually resolved — the effective repo@revision +
loaded filename, plus the sha256 prefix of the bytes actually hashed (compute the hash
unconditionally, or stamp `sha256:<computed>` for non-default sources); refuse to stamp a
constant for an artifact whose bytes were never verified.

### P2

**K2-R1-2 — The coalescer DESTROYS queued requests beyond max_batch during the debounce
window, and MAX_BATCH is not enforced on the initial take.**
Evidence: `src/embed/candle.rs:604-613`. After `q.drain(..additional)` (additional =
min(q.len(), MAX_BATCH − first_batch.len())), `q.clear()` removes **everything still in
the queue** — every surviving `Pending`'s oneshot sender is dropped and its caller gets
`EmbedError::Backend("candle coalescer dropped the request")` (`candle.rs:656`). The
inline comment says "keep leftovers"; the code does the opposite, and the
`if !q.is_empty() { notify_all() }` at `:611-613` is dead (q was just cleared).
Reproduction (simulated, algorithm transcribed faithfully from 591–614): 1 caller wakes
the coalescer; 50 more arrive during the 2 ms debounce → additional = min(50, 31) = 31,
**19 requests dropped**, 32 forwarded. Second defect, same block: the initial
`std::mem::take(&mut *q)` at `:596` bypasses the cap entirely — a 40-deep queue at wake
runs ONE forward of width 40 against the "hard cap on one forward's batch width"
(`:72-73`), which also unbounds worst-case forward memory (40 × up-to-8192-token
sequences).
Impact: loud failures, not corruption — but random write failures under exactly the
concurrent load this adapter exists to serve, on a clean path, by construction.
Fix: loop the drain (`while` until queue empty, batching by max_batch), delete the
`clear()`, and clamp the initial take to max_batch (leave the remainder for the next
iteration).

**K2-R1-3 — `offline = true` dials the network on a cache miss; the documented offline
contract is unenforced for the weight file.**
Evidence: `resolve_offline` (`candle.rs:545-547`) is byte-identical to `hub_get`
(`:527-539`) and delegates to hf-hub's sync `ApiRepo::get`, which falls through to
`download()` when the cache misses (`hf-hub-0.4.3/src/api/sync.rs:709-715`: cache hit →
return, miss → network). Only `config.json`/`tokenizer.json` get a real existence check
(`candle.rs:443-468`). The module doc promises "`offline = true` … **never touches the
network** and fails loudly if the weights are absent" and `lambo.example.toml` repeats
it; the spec's task 4 requires "an explicit offline path".
Scenario: internet-connected rig, `offline=true`, empty hub cache → the adapter silently
downloads 1.1 GB despite the flag. Airgapped rig, uncached → fails with a raw fetch
error instead of the intended named "not cached (fetch once online)" message that the
config/tokenizer paths produce.
Fix: use `CacheRepo::get` (cache-only) or a pre-check like the config/tokenizer arms for
the weight file too; collapse the three identical helpers while there.

**K2-R1-4 — The candle adapter omits the output-width check its sibling treats as an
invariant, so a wrong `dim` config resolves and corrupts downstream instead of failing
at construction.**
Evidence: `CandleEmbedder::forward` always emits BGE-M3's fixed 1024-wide vectors;
`Embedder::embed` returns them with no length check against `self.dim`
(`candle.rs:644-657`). The sibling adapter refuses `vec.len() != self.dim`
(`bge_m3.rs:299-303`) with a comment stating the exact hazard: "same dim passes the only
runtime check, so the mix would be undetectable" (`bge_m3.rs:175-176`).
Scenario (traced): `[embedder] kind="candle" dim=768` (copied from another model).
`resolve_backends` passes (MemoryStore reports `vector_dimensions()=None`; nothing pins
1024), the contract stamps `dim=768`, and every stored vector is 1024 wide. The lie
surfaces late and sideways: `reembed_all` later refuses every update ("width 1024 !=
contract 768"), vector-capable stores reject the column width, and hybrid scoring mixes
widths under a consistent-looking contract.
Fix: hard-error in `CandleEmbedder::new` unless `dim == 1024` (the model's architectural
fact), mirroring bge_m3's runtime guard as defense in depth.

**K2-R1-5 — A dead coalescer thread wedges every future `embed()` forever; there is no
restart and no await timeout.** *(trigger speculative; consequence certain)*
Evidence: `spawn_coalescer` spawns one bare thread with no supervision (`candle.rs:580-585`);
`coalesce_loop` unwraps mutex/condvar locks (`:592,594,604`) — poisoned on any panic —
and runs `shared.core.forward` outside `catch_unwind`; `embed()` awaits the oneshot with
no timeout (`:655`). If the thread dies (a candle panic — candle is Result-mostly but not
panic-free — or a poisoned-lock unwrap), nothing ever drains the queue again: every
subsequent `embed()` pushes and blocks indefinitely, hanging MCP tool calls and the
derive pipeline with no error, no timeout, no log.
Fix: `catch_unwind` around the forward (fail the batch, keep looping), respawn-on-death,
and/or a bounded wait on `rx` that maps timeout to `Unavailable`.

### P3

**K2-R1-6 — Mid-run abort skips `close_writer`, so the lease is held to TTL, and the
docstring claiming otherwise is false.**
`re_embed.rs:102-107` (`?` on embed failure) and `:123` (`?` on `reembed_all`) return
before `close_writer(mem, out)` at `:142`; `Memory::drop` stops the heartbeat but cannot
release the lease (`memory.rs:2768-2803`), so the session is unwritable for the remaining
TTL (~30 s+) after a failed migration. Durable state is untouched and the old contract
stays stamped durably — the no-op itself is correct. But `re_embed.rs:98-99` asserts
"any embed failure aborts here … and close_writer still releases the lease": close_writer
never runs on that path. Either release on the error path or fix the comment.

**K2-R1-7 — The coalescer thread never exits; dropping the last handle leaks the thread
and the ~1–2 GB resident model until process end.**
The condvar wait (`candle.rs:593-594`) holds `Arc<Shared>` forever; there is no shutdown
signal. Harmless for one-shot verbs and single-serve lifetimes; a real cost for anything
that constructs embedders repeatedly (tests do not, today — which is also why this is
invisible).

**K2-R1-8 — Hash verification happens AFTER the full model load.**
`load_core` parses/builds the 391-tensor model (`candle.rs:470`) and only then does
`verify_hash` reject (`:476-478`). A tampered/corrupt default artifact costs seconds of
mmap+pickle work before the guaranteed refusal; hash-then-load is both cheaper and the
safe order (refuse before weights influence anything).

**K2-R1-9 — `no_accelerator` misreports which accelerator failed.**
The macOS auto-Metal arm passes `macos=false` (`candle.rs:226`) and the auto-CUDA arm
passes `has_cuda=false` (`:236`), so a genuine `Device::new_metal`/`new_cuda` failure on
a machine WITH the feature compiled prints "an accelerator is unavailable" instead of
naming Metal/CUDA. Guidance (pin `device="cpu"`) is unaffected; purely diagnostic quality.

**K2-R1-10 — Dead/duplicated code and a prose typo in the stamping site.**
`_weight_file_path` is bound and never used (`candle.rs:414`);
`hf_path`/`hub_get`/`resolve_hf`/`resolve_offline` are four names over two identical
bodies (`:512-547`); `resolve.rs:157` reads "cannot hide a between two quantizations".

**K2-R1-11 — Done-when gaps: the cold-start number is documented nowhere operator-facing,
and the notes cite `#[ignore]`d live tests that do not exist.**
Spec §K2 Done-when: "the cold-start number is documented where an operator wiring a stdio
client will read it." The commit range touches neither README.md nor docs/reference/cli.mdx;
`lambo.example.toml`'s new candle block carries config keys but no numbers; only the
dev-diary notes (which an operator wiring stdio does not read) sit near the K1 numbers.
Separately, the CI row comment and the K2 notes both say live weight-loading tests "stay
`#[ignore]`d" — `grep '#\[ignore\]' src/embed/candle.rs` finds none: there are no live
tests at all yet, ignored or otherwise (the parity-in-shipped-adapter Done-when item is
accordingly untested on this branch, matching K1's spike-only status). Say so plainly
rather than implying their presence. (Task 7, the migration act itself, is honestly
declared not-yet-run in the notes — recorded here as open, not a defect.)

## Closures

*(left for round 2)*

— K2Review, 2026-08-23
