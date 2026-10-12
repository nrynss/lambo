# #125: batched embeds across queued writes (0.6.0 lane C)

Base: main `48d83a53` (0.5.0). Issue #125: at 16 agents (think 0.5 s)
apply p50 was 7.1 s and p90 12.3 s, against 93 ms / 146 ms at 8 agents.
This note records what was measured first, the design that followed from
it, the invariants it keeps, and how to re-measure. The commits carry the
mechanics.

## Measured first

The issue asks whether candle on Metal batches efficiently. The ignored
probe `embed::candle::tests::metal_batch_throughput` times `core.forward`
directly on derive-shaped texts (about 150 bytes: a concept framed with
its call's `"; "`-joined prompt). Release build, Apple M3 Pro, f16, with
the dogfood serve sharing the GPU (so absolute numbers are noisy; the
second run below was slower throughout):

| forward width | run 1 | run 2 |
|---:|---:|---:|
| 1 | 18.8 ms, 53 texts/s | 26.8 ms, 37 texts/s |
| 4 | 49.8 ms, 80 texts/s | 75.1 ms, 53 texts/s |
| 16 | 206 ms, 78 texts/s | 336 ms, 48 texts/s |
| 32 | 424 ms, 76 texts/s | 697 ms, 46 texts/s |
| 64 | 889 ms, 72 texts/s | 1,466 ms, 44 texts/s |
| 16 with one 910-byte text | 798 ms, 20 texts/s | 1,280 ms, 12.5 texts/s |
| the same 16 as 3 length chunks | | 312 ms, 51 texts/s |
| 16 as two 8-wide forwards | | 267 ms, 60 texts/s |
| 32 as four 8-wide forwards | | 467 ms, 69 texts/s |

```text
cargo test --release --features embed-candle-metal --lib -- --ignored --nocapture metal_batch_throughput
```

Two conclusions:

1. **Width buys little.** Throughput rises about 1.5 times from width 1 to
   width 4 and is flat after that. The candle coalescer already merged
   concurrent embeds from different lanes into one forward (2 ms debounce,
   up to 32 wide), so "batch across lanes" was already happening at the
   embedder. A queue-level batcher that only made forwards wider would buy
   nothing. (This matches the #11 note in the dogfood graph: batching one
   derive's embeds measured no gain on Metal.)
2. **Padding costs a lot.** `BatchLongest` pads every row to the longest
   text, so one long text in a coalesced batch made it four times slower.
   Real traffic mixes long derive contexts, short record_action paths and
   recall queries in one take.

## Where the latency came from

Each lane has one consumer, and a job embedded its texts one after
another (a derive: embed, candidate lookup, embed, ...; a record_action:
one embed per content string). Under load every embed round waits for a
whole forward, often a wide padded one of a few hundred ms, so a lane
retired a two-text write in two or more such waits. At 16 agents each
agent submits about one write per second, close to what one lane could
retire, so lanes backed up. This is an argument from the code and the
probe, not a measurement under load; the acceptance run below decides.

So the change cuts **rounds per write** and removes padding waste.

## Design (what landed)

### 1. `Embedder::embed_batch` and `batch_capacity` (non-breaking)

Default methods. `embed_batch(&[&str], deadline) -> Vec<Result<..>>`: one
result per text in order, each that text's own. It answers by `deadline`:
a text not embedded by then fails its own slot `Unavailable`, the texts
already done keep their vectors, and no work starts after the deadline.
The default loops over `embed`, each bounded by the deadline.
`batch_capacity()` is the most texts worth gathering into one call; a
larger call is allowed and must still answer every slot. The default, 1,
tells callers gathering gains nothing, and every write path then behaves
exactly as before.

Candle overrides both: it queues every text under one lock as **bulk**
work, waits for all slots against one shared deadline (the caller's or
`EMBED_WAIT`, whichever is sooner), keeps a load failure's own class, and
reports `FORWARD_WIDTH` (8), one forward's width. The fixture batches
natively with capacity 8. The HTTP adapters (BGE-M3 over llama-server,
EmbeddingGemma 2, Gemini) are unchanged: llama-server accepts an array
input and could override later, but its error for one bad text is an HTTP
error for the whole request, so that adapter needs per-slot isolation
first.

### 2. One round per write (`hybrid::embed_gathered`)

Before its per-item loop, a hybrid derive sends every text it will embed
(unmatched items and `parent_of` ends, framed with `context_text`) to
`embed_gathered`, which calls `embed_batch` in rounds of the embedder's
capacity, handing each round the call's deadline, and keeps what comes
back in the per-call `EmbedCache` (#116). The loop is unchanged: a hit
costs nothing; a text the round did not embed is embedded alone through
`embed_or_refuse`, so the write meets **that text's own error and class**
(`Embed` permanent, `EmbedUnavailable` transient). `record_action` does
the same; the public `embed_action_contents` keeps its signature. A round
that has not returned `BATCH_DEADLINE_GRACE` (100 ms) after the deadline
(an adapter overrunning it) is abandoned; `RECEIPT_WAIT_MAX` has 4 s of
headroom over `HYBRID_IO_TIMEOUT`, so its bound holds.

### 3. Candle: length-sorted chunks, priority, failure isolation

A coalesced take (up to 32) runs as forwards of at most `FORWARD_WIDTH`
(8) texts, shortest first, where no text in a forward is longer than twice
the forward's shortest plus 64 bytes (`length_chunks`). Each chunk's
callers are answered when their chunk is done.

Single embeds (a recall's query, a lone text) are taken before bulk
`embed_batch` texts (`take_prioritized`) and forwarded first, so a recall
does not wait behind a write's gathered texts. A request whose caller has
dropped its receiver (its deadline passed) is not forwarded
(`still_awaited`).

Found while doing this (pre-existing, own `fix` commit): a forward that
failed or panicked failed **every** caller in the batch, as a permanent
`Backend` refusal, so a write coalesced with a bad text by timing alone
failed and its durable intent was settled `failed`. Now a failed forward
is followed by a one-word canary forward (`forward_isolated`):

- canary fails too: the device or model is at fault; every caller gets
  `Unavailable` (transient), and nothing is retried per text, so a broken
  device costs two forwards per chunk, not one per text;
- canary passes: each text runs alone and only a text that fails alone
  gets `Backend`.

A width mismatch is still reported to every caller without a retry (a
property of the model, K2-R1-4); a panic is still caught and named
(K2-R1-5).

### Batched equals unbatched, within tolerance (review M2)

The ignored live test `metal_batched_matches_single_within_tolerance`
compares, on the real f16 weights on Metal, each of seven texts (11 to
910 bytes, Japanese and code included) embedded alone against
`embed_batch` and against one forward over all seven padded to the
longest. Measured: max absolute difference 0 for `embed_batch` (its
chunks here held up to four short texts padded together, so padding alone
did not move them on this run) and 3.3e-4 for the one padded forward that
included the 910-byte text. So a batched vector is **equal within
tolerance** (under 1e-3), not bit-identical in general: a derive whose
two texts land in one chunk is padded to the longer one. Stores written
before and after this change can therefore differ in the last bits of
vectors, and `replay_parity.py`'s vector-drift detector may report it;
that is expected, not a behaviour change, if the drift stays at this
scale.

```text
cargo test --release --features embed-candle-metal --lib -- --ignored --nocapture metal_batched_matches_single
```

## Removed after review: the lane prefetch (a future option)

A first version also had each write-queue lane embed the texts of the job
it was about to run **and of up to seven jobs queued behind it** (16
texts at most) in one round, seeding each later job's cache
(`writeq/prefetch.rs`, commit 58173ae6, reverted in this branch). With
`BatchingEmbedder`, five queued two-concept derives in one lane cost 2
rounds instead of 3 rounds and 2 single embeds. The adversarial review
objected:

- **H1:** the round's time was charged to the running job's 30 s I/O
  budget, so that job's own embeds and lookups could be starved by other
  jobs' texts;
- **H2:** a timeout around the whole round dropped vectors it had already
  computed;
- **M1:** wider bulk pushes made recalls queue behind other agents' write
  texts in candle's FIFO;
- **M4:** the seed outlived the call that embedded it.

The orchestrator removed it: the probe shows Metal throughput is flat past
4 texts per forward, so the measured wins are one call per write and
length chunking. H2's partial-results fix, M1's priority and the
deadline handling have since landed for the per-write path, which removes
most of the objections' mechanism, and M4 cannot arise without a
cross-call seed. If the acceptance run at 16 agents still shows lane
backlogs, the prefetch is the next thing to measure: with its own budget
(not the running job's), behind single embeds in candle's queue, and
with the seed scoped to the jobs it was embedded for.

## Review resolutions (the ones not covered above)

- **H1:** a vector-candidate lookup that times out (or is not started
  because the deadline passed) is now `EmbedUnavailable`, transient, so
  the write queue leaves the durable intent unconsumed instead of settling
  an acked write `failed`. Test:
  `a_candidate_lookup_timeout_is_transient_and_writes_nothing`.
- **H2:** no embed, lookup or round is started after the deadline (a
  `timeout_at` past its deadline still polls once); a round keeps the
  slots it finished. Tests: `a_gathered_round_keeps_the_slots_done_before_its_deadline`,
  `a_round_that_overruns_its_deadline_is_abandoned_just_past_it`, and
  `default_embed_batch_keeps_what_finished_by_the_deadline_and_starts_nothing_after`.
- **L3:** the calibration probe's representative write now embeds the way
  a derive does (one batched call when the embedder batches), so the probe
  and the observed rate measure the same unit and `probe_optimism` keeps
  its meaning: the probe's write rate over the observed write rate.
- **L5:** `batch_capacity` is defined as the most texts worth gathering
  per call; candle reports its forward width (8), not its take (32).
- **L7:** the API reference lists the shipped embedders.

## Invariants kept

- **Per-write receipts and lane order.** Untouched: batching happens
  inside one write's call.
- **#116's commit model.** Only the vector gather is shared, through the
  per-call cache, keyed by the exact text sent to the embedder.
- **Failure isolation.** A failed slot, or a round that fails or times out
  as a whole, is only missing from the cache; the write embeds that text
  alone and meets its own error with its own class.
- **The I/O bound.** One `HYBRID_IO_TIMEOUT` per call, plus at most
  `BATCH_DEADLINE_GRACE` for an adapter that overruns.
- **Low load.** No wait is added. A lone derive's two texts go out in one
  call instead of two; if their lengths are close they share one forward
  (padded to the longer, within the tolerance above).

## Tests

- `embed::trait_tests`: the default `embed_batch` is `embed` per text, in
  order, each slot its own; it keeps finished slots and starts nothing
  after the deadline; the fixture's batch equals its single embeds.
- `embed::candle`: failure isolation (one bad text; a panicking text; a
  failing device fails all transient in two forwards; a lone bad text is
  permanent; a width mismatch fails all without a retry), single-before-bulk
  takes, a closed request not forwarded, the two `length_chunks` tests, and
  `embed_batch` on the tiny CPU artifact. Live probes (ignored):
  `metal_batch_throughput`, `metal_batched_matches_single_within_tolerance`.
- `graph::hybrid::tests::batching` (over `test_util::BatchingEmbedder`):
  one round for a derive's items and parent ends with the unbatched
  vectors stored; capacity 1 keeps the old path; a refused item fails the
  write as `Embed` with nothing written; a whole-round outage falls back to
  single embeds and applies; record_action in one round and a refused
  path failing as `Embed`; the H1 and H2 tests above.

## Not measured here: the #125 acceptance run

The acceptance check is `mcp_concurrent.py` at 16 agents (think 0.5 s,
120 s) on a copy of a realistic store, with apply p90 under 2 s and 0
failed or dropped writes, plus `replay_parity.py` against 0.5.0 with
canonization off. This lane did not run it: it needs a copy of the
dogfood store (this lane may not read `~/lambo-dogfood`), a bearer token,
and the GPU the dogfood serve is using. To run it, as the owner:

```bash
# 1. a scratch copy of the store and a scratch config on port 17701
#    (never 7700); embedder kind = "candle", offline = true
cargo build --release --features ship,embed-candle-metal
lambo provision --config scratch/lambo.toml
XDG_RUNTIME_DIR=$(mktemp -d /tmp/lbc.XXXX) target/release/lambo serve \
    --config scratch/lambo.toml --session conc-125 --agent conc \
    --transport http --port 17701 --bind 127.0.0.1 \
    --ledger scratch/serve-ledger.jsonl 2> scratch/serve.stderr.log &

# 2. 8 agents (must not be slower than p50 93 ms / p90 146 ms) and
#    16 agents (target p90 < 2 s), each on a FRESH copy of the store
python3 scripts/loadtest/mcp_concurrent.py --endpoint http://127.0.0.1:17701/mcp \
    --workers 8 --think 1.0 --secs 120 --seed 1 --out conc-125-w8.json
python3 scripts/loadtest/mcp_concurrent.py --endpoint http://127.0.0.1:17701/mcp \
    --workers 16 --think 0.5 --secs 120 --seed 1 --out conc-125-w16.json

jq '{drained: .drain.drained, rate: .writes.write_failure_rate_all,
     p50: .final_stats.write_queue_apply_ms_p50,
     p90: .final_stats.write_queue_apply_ms_p90,
     max: .final_stats.write_queue_apply_ms_max}' conc-125-w16.json
```

Run the 0.5.0 release asset the same way on fresh copies for the
baseline, then `replay_parity.py` against 0.5.0 per the loadtest README.
