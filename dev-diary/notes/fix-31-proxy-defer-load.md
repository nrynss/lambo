# A proxy never loads the model: candle identity before load (issue #31)

**Decision (2026-10-11):** the candle embedder is built in two steps.
`resolve_backends` (still the single construction site) builds it *deferred*:
device, artifact identity, and width only. The weights reach the device in
`Embedder::prepare`, which `MemoryBuilder::build_attach` awaits once the
session's lease is won and its contract has been checked. A serve that loses
the startup election gets `Attach::Held` before that point and never loads.

Follows #13 (`idle-model-residency.md`), which made a proxy *release* the
embedder once its role was known; this removes the load itself.

## What moved

| Before | After |
|---|---|
| `CandleEmbedder::new` hashed the weights, parsed `config.json`, loaded the model onto the device and spawned the coalescer, all in `build_embedder` at resolve | `CandleEmbedder::deferred` (used by `build_embedder`) resolves the device, the artifact paths, the weight sha256 and `config.json`; `CandleEmbedder::load` parses the tokenizer, loads the model and spawns the coalescer. `new` = `deferred` + `load`, unchanged for callers |
| The weight sha256 was computed on every construction (~1.1 GB read) | `weight_sha256` serves it from a metadata-keyed cache (see below) |
| Candle width: the configured `dim` checked against the `BGE_M3_DIM` constant; a mismatched override artifact failed at its first forward | Also checked against `config.json`'s `hidden_size` at resolve (`check_hidden_size`) |
| No load step on the holder path | `Embedder::prepare` (new, defaulted no-op) awaited in `build_attach`, step 2b |
| `lambo serve-web` had a loaded model from the resolve | `serve_web::run` awaits `prepare` after the schema preflight, before it binds |

Code: `src/embed/candle.rs` (`LoadPlan`, `LoadSlot`, `deferred`, `load`,
`is_loaded`, `load_count`, `weight_sha256`, `check_hidden_size`),
`src/embed/mod.rs` (`Embedder::prepare`, the registry arm),
`src/memory/builder.rs` (step 2b), `src/cli/serve_web.rs`, comments in
`src/resolve.rs` and `src/mcp/serve/roles.rs`.

## Where the holder load happens, and why there

Inside `build_attach`'s startup future, after the session load and the
contract check (step 2), before the write pipeline is spawned:

1. **After the role is decided.** The lease acquire (step 0) is the role
   decision: `Attach::Held` returns before step 2b, so a proxy (or a refused
   HTTP serve, or a lease loser of any kind) never reaches the load. The same
   holds for every caller of `build_attach`: the one-session serve through
   `resolve_role`, `serve_pinned`, on-demand registry attaches, the CLI
   writers and `re-embed`.
2. **After the contract check.** A session whose stored contract disagrees
   with the live one is refused before anything is loaded.
3. **Before handshake and before any embed.** The attach returns only after
   the load, so `resolve_role` hands `serve` a `Memory` whose model is on the
   device before the transport starts. The write pipeline (the #32 calibration
   probe, the J3 intent replay) is spawned after the load, so nothing embeds
   first. The load is explicit, never lazy on the holder's first call, so the
   first recall's latency is unchanged.
4. **Under the J6 race.** The startup future is the one `build_attach` races
   against the early-shutdown signal, so a SIGTERM during the load abandons
   it and releases the lease, exactly as during the session load. `prepare`
   runs `load` on a blocking thread so the race is actually polled. J6's "the
   pre-arm covers no unbounded wait" stays true: the load is now one more
   bounded wait inside the raced future.

Startup timeline: the holder's total startup work is the same; the load moved
from before the election to after the lease is won, which is still before the
first byte to the client. The pre-handshake durability test runs the fixture
embedder, whose `prepare` is a no-op, so its timeline is unchanged.

**Multi-session (#32).** One process shares one `Arc<dyn Embedder>`. Every
clone of a `CandleEmbedder` shares one `LoadSlot`; `load` takes its `init`
mutex, re-checks, and sets a `OnceLock`, so the first attach loads and every
later attach's `prepare` returns at once. The calibration's `Weak` key and a
proxy's release are unchanged.

**Direct users of `ResolvedBackends.embedder`.** The one-shot `lambo recall`
CLI and library callers embed without a `Memory`. `CandleEmbedder::embed`
calls `prepare` itself when the model is not loaded, so they keep working;
the load lands on their first embed, which is when the resolve-time load used
to be paid for anyway. This fallback is never reached on the serve path.

## The dimension check: still resolve-time, still store-authoritative

AGENTS.md: "Vector dim is store-authoritative ... Compatibility is checked at
resolve time." What the resolve compares, and where each input now comes from:

* `embedder.dimensions()` for candle is the configured `dim`, which
  `deferred` refuses unless it equals `BGE_M3_DIM` (1024, K2-R1-4, unchanged)
  **and** `config.json`'s `hidden_size` (new). `hidden_size` is the width of
  the CLS vector the forward emits, so the declared width is the width the
  model will produce, read from the same artifact the identity hashes.
* The checks in `resolve_backends` are untouched: `embed_dim ==
  embedder_cfg.dim`, the kind-agnostic `store.vector_dim` pin comparison, and
  `check_vector_compatibility(store.vector_dimensions(), embed_dim)`. None of
  them needed a loaded model before (the candle `dimensions()` was already the
  configured value); they still run before `resolve_backends` returns.
* Defense in depth stays: the coalescer still refuses any forward output whose
  width differs from the contract width (K2-R1-4).

So the store remains the authority, the comparison is still made at resolve,
and it is stricter than before: an override artifact with a different hidden
size used to resolve and then fail every batch; it is now refused at resolve.

Contract identity is likewise resolve-time: `EmbeddingContract.model` is the
candle identity string (source + sha256 prefix), available from `deferred`,
and the session-contract refusal in `build_attach` still runs on it, now
before the load.

## The identity cache

`weight_sha256(path, cache_dir)`. Key (`file_fingerprint`): canonical path
(an hf-hub snapshot link resolves to its blob), size, mtime and ctime in
nanoseconds, device and inode. Entry: one JSON file per key under
`$XDG_CACHE_HOME/lambo/candle-identity/` (else `$HOME/.cache/lambo/...`,
beside hf-hub's own `~/.cache/huggingface`), named by the key's sha256 prefix,
holding the key and the digest; written by temp file and rename.

* A rewrite, replacement, truncation or `touch -r` restore changes at least
  one key field (a restored mtime still moves ctime), so the entry misses and
  the bytes are hashed again; the default-artifact check against
  `WEIGHT_SHA256` then runs on the fresh digest, as before.
* A file whose fingerprint changes while it is hashed is not cached.
* An unreadable, corrupt or foreign entry is a miss; a failed write is logged
  at debug and only costs the next start a hash. No cache directory: hash
  every time (the old behaviour).
* Trust (review L4): the directory is created 0700 and entries are written
  0600. An entry is trusted only if both the directory and the entry are
  owned by the effective uid and are not group- or world-writable (checked
  with `symlink_metadata`, so a symlinked entry is a miss too); anything else
  is a miss and the bytes are hashed. So only a process that could already
  rewrite the weights can plant a digest.
* **Trade-off, accepted:** it replaces "hash the bytes on every start"
  (K2-R1-8) with "hash the bytes whenever their metadata changes", which is
  what the issue asked for. A cache hit does not read the bytes, so silent
  on-disk corruption that leaves the metadata unchanged (bit rot) is no
  longer caught at start. A badly rotted safetensors file still fails to
  load; a quietly rotted one embeds under the old identity, as it would have
  between two starts before #31.
* The directory is not a config key and `CandleOpts` is unchanged from 0.4.0
  (review L5): tests reach a scratch directory through the crate-internal
  `CandleEmbedder::deferred_in`.

## The load checks the artifact again (review M2)

The identity is stamped at resolve, but the load can run much later (a
pinned session's background retry, an on-demand attach, hours or days
after the start). `deferred` records the fingerprint of the weights (taken
before they are hashed), `config.json` and `tokenizer.json` in the
`LoadPlan`; `load` re-takes each and refuses on any difference with
`EmbedError::Backend` ("changed after this process resolved its identity";
restart to re-resolve). So the bytes on the device are always the bytes the
stamped contract describes; a file that changed while it was being hashed
also fails this check.

## Load order and error classes (review L2, L3)

`load_core` parses and configures the tokenizer first, then maps the
weights and builds the model, so a corrupt `tokenizer.json` is refused before
~1.1 GB is read. Classes:

* `EmbedError::Backend` (permanent): a changed artifact, a tokenizer that does
  not parse, an unsupported weight-file extension, a panicked load task.
  `build_attach` maps it to `LamboError::Config`, so a pinned session goes to
  `Slot::Failed` until the serve is restarted.
* `EmbedError::Unavailable` (transient): mapping the weights or building the
  model on the device (memory pressure, a busy or failing device).
  `build_attach` maps it to `LamboError::EmbedUnavailable`, which the pinned
  retry treats like an unreachable embedder: the slot stays held-elsewhere and
  is retried every `PINNED_RETRY`.

Either way the failed attach releases its lease. A one-session serve exits on
either, as it did when the resolve failed.

## The lease heartbeat starts at the acquire (review L1)

Before #31 the heartbeat was spawned after the startup load. With the model
load in the startup, acquire-to-first-refresh would have included it. The
heartbeat now starts immediately after the acquire, under a guard that aborts
it if the attach does not complete (an error, a shutdown abandon, the caller
dropping the future); the `Memory` takes it over on success. A lease lost
during the startup latches the fence, which the flush task and the write gate
then inherit.

## Keep-warm never loads (review M1)

A multi-session serve arms keep-warm whether or not it holds a session, and
the candle `embed` loads an unloaded model as a fallback. So a pinned serve
whose sessions were all held elsewhere, or an on-demand serve with nothing
attached, would have loaded ~1.4 GB at its first touch. `Embedder::is_prepared`
(new, default `true`; candle returns `is_loaded`) lets `keep_warm_loop` skip
a touch while nothing is loaded. Once a session attaches, its attach loads the
model and the touches begin.

## Other embedder kinds

Checked for heavy construction; none needs deferral:

* `bge_m3` / `openai`: builds a reqwest client and reads the bearer variable.
* `embeddinggemma2`: the same HTTP client; its `/props` check is already lazy.
* `gemini`: reads the service-account JSON and builds a client; the token is
  minted on the first embed.
* `fixture`: in-memory.

`prepare` is on the trait, so a future heavy adapter defers the same way:
cheap identity in its registry arm, the heavy part in `prepare`.

## Invariants

1. A serve that holds no lease never loads candle weights: it never reaches
   `prepare` in an attach, and keep-warm skips its touches while the
   embedder is not prepared (the only other embed a serve makes on its own).
   Tested for the proxy (`a_proxy_never_loads_and_the_holder_loads_before_it_serves`)
   and for a multi-session serve whose sessions are all held elsewhere, with
   keep-warm on (`a_serve_holding_no_session_never_loads_its_model_for_keep_warm`).
2. A holder's `prepare` completes before `build_attach` returns, before the
   write pipeline starts, and so before any handshake, recall, probe or
   replay.
3. Contract identity and width are known at resolve without the load; every
   resolve-time refusal fires before any weights reach a device.
4. One load per embedder per process, shared by every session (#32).
5. `candle_identity` / `weights_in_unified_memory` downcast to
   `CandleEmbedder` itself; there is no wrapper type to see through.
   `on_metal` is known before the load because the device is resolved first,
   so `keep_warm_interval()` is unchanged.

## Tests

* `embed::candle::tests::deferred_stamps_identity_and_width_without_loading_the_model`
  and `embed_without_prepare_loads_once_then_embeds` (load count, clone
  sharing, idempotence, the direct-use fallback).
* `embed::candle::tests::a_config_hidden_size_other_than_dim_is_refused_at_construction`.
* `embed::candle::tests::the_identity_cache_serves_an_unchanged_file_and_misses_a_changed_one`
  and `a_changed_weight_file_changes_the_stamped_identity`.
* `resolve::tests::candle_deferred::*`: the contract is stamped at resolve
  without loading; a store pinned to another width is refused at resolve (and
  an artifact whose `config.json` disagrees); a store whose session was
  written by other weights is refused before the load, a lease loser never
  loads, and a holder has loaded when its attach returns.
* `mcp::serve::tests::proxy_releases_the_model::a_proxy_never_loads_and_the_holder_loads_before_it_serves`:
  through the real `serve_builder` + `resolve_role` election with a
  load-counting embedder: the proxy loads 0 times, the holder exactly once
  and before the session is handed to the transport, and none of the
  holder's embeds (a real derive and recall, review L6) came before the load.
* `mcp::serve::tests::registry::pinned_serve::a_serve_holding_no_session_never_loads_its_model_for_keep_warm`
  (review M1): the real `serve_pinned`, every session held elsewhere,
  keep-warm every second: 0 loads over 3.5 s; once a session is released and
  re-attached in the background, exactly one load, by the attach.
* `embed::keep_warm::tests::an_unprepared_embedder_is_not_touched_until_it_is_prepared`.
* `embed::candle::tests::a_weight_file_changed_after_resolve_is_refused_at_load`
  (review M2; also `config.json` and `tokenizer.json`).
* `embed::candle::tests::a_corrupt_tokenizer_fails_the_load_before_the_weights`
  (review L3).
* `embed::candle::tests::the_identity_cache_trusts_only_private_entries`
  (review L4).
* `mcp::serve::tests::registry::a_transient_model_load_failure_is_retried_and_a_permanent_one_is_not`
  (review L2).

The candle tests run in CI's `embed-candle` row with no network and no cached
weights: `embed::candle::test_artifact` writes a tiny real artifact (1024
hidden, one layer, four-word vocabulary, ~17 MB of random f32 weights) that
loads and embeds on CPU.

## Measurement (Metal, M3 Pro 18 GB, 2026-10-11)

`embed::candle::tests::metal_proxy_footprint` (ignored; needs the cached
published f16 weights, never downloads):
`cargo test --features embed-candle-metal --lib -- --ignored --nocapture metal_proxy_footprint`.
It reads its own `ri_phys_footprint` (`proc_pid_rusage`) around a deferred
construct and the load. Debug build, n = 1 per row:

| | footprint | time |
|---|---|---|
| test process baseline | 4.2 MB | |
| deferred (what a proxy holds), first run, digest not cached | 7.7 MB | 28.7 s (the sha256 in an unoptimized build) |
| deferred, second process, digest cached | 8.0 MB | 61 ms (mostly Metal device creation); a further construct in the same process 0.5 ms |
| loaded (what a holder holds) | 1424-1427 MB | load 1.3-2.4 s |

So a proxy's embedder adds ~4 MB (the Metal device handle) instead of
~1.4 GB. For scale, `shasum -a 256` over the same 1.14 GB file takes 2.6 s,
which is roughly what every serve start used to spend hashing before the
cache. Not measured: the live writer (untouched) and a release build's
uncached hash.

## Not done

* A proxy still creates a Metal device (to keep the K "no accelerator is a
  hard error at resolve" rule and to know `on_metal`). ~4 MB; not worth
  deferring.
* The tokenizer is parsed at load (first, before the weights), so a corrupt
  `tokenizer.json` fails the holder's attach (lease released, serve exits, or
  a pinned slot is marked failed) rather than the resolve. Parsing it at
  resolve would put its vocabulary in every proxy's memory.
