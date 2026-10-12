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
agent submits about one write per second, which is close to what one lane
could retire, so lanes backed up. The embedder's total throughput was not
the only limit; the number of serial rounds per write was.

So the design reduces **rounds per write**, and removes padding waste.

## Design

### 1. `Embedder::embed_batch` and `batch_capacity` (non-breaking)

Default methods. `embed_batch(&[&str]) -> Vec<Result<..>>`, one result per
text in order, each that text's own; the default loops over `embed`.
`batch_capacity()` defaults to 1, which tells callers gathering gains
nothing, and every write path then behaves exactly as before. Candle
overrides both (pushes every text under one queue lock so one take sees
them; capacity `MAX_BATCH` = 32). The fixture embeds batches natively
(capacity 32) so tests over it take the gathered path. The HTTP adapters
(BGE-M3 over llama-server, EmbeddingGemma 2, Gemini) are unchanged:
llama-server accepts an array input and could override later, but its
error for one bad text is an HTTP error for the whole request, so that
adapter would need per-slot isolation first. Not done here.

### 2. One round per write (`hybrid::derive_with_gather`)

Before its per-item loop, a hybrid derive sends every text it will embed
(unmatched items and `parent_of` ends, framed with `context_text`) to
`embed_gathered`, which calls `embed_batch` in rounds of the embedder's
capacity under the call's I/O deadline and keeps what comes back in the
per-call `EmbedCache` (#116). The loop is unchanged: a hit costs nothing;
a text the round did not embed is embedded alone through
`embed_or_refuse`, so the write meets **that text's own error and class**
(`Embed` permanent, `EmbedUnavailable` transient). `record_action` does
the same (`embed_action_contents_gathered`; the public
`embed_action_contents` keeps its signature).

### 3. One round per several writes (the lane prefetch, `writeq::prefetch`)

Before running a job, the lane worker predicts the texts of that job and
of up to `LANE_PREFETCH_MAX_JOBS - 1` (7) jobs queued behind it in the
same lane, against the graph as it is now (`hybrid::planned_embed_texts`,
which shares the plan's `plan_inputs`, budget and context-cap checks, and
returns nothing for a call the plan would refuse; record_action's distinct
contents). It embeds up to `LANE_PREFETCH_MAX_TEXTS` (16) of them in one
round, and hands the vectors to each job as an `EmbedSeed` read on a cache
miss. A job whose texts the seed already covers runs no round.

Peeking does not pop: `Job.payload` became `Arc<JobPayload>` so the worker
clones handles, not strings, under the lanes lock. Lanes are not joined
with each other: the candle coalescer already takes every lane's queued
round together, and joining lanes in the queue would make one lane wait
for another.

### 4. Candle: length-sorted chunks, and failure isolation

A coalesced take (up to 32) now runs as forwards of at most
`FORWARD_WIDTH` (8) texts, shortest first, where no text in a forward is
longer than twice the forward's shortest plus 64 bytes
(`length_chunks`). Each chunk's callers are answered when their chunk is
done.

Found while doing this (pre-existing, own `fix` commit): a forward that
failed or panicked failed **every** caller in the batch, as a permanent
`Backend` refusal, so a write coalesced with a bad text by timing alone
failed and its durable intent was settled `failed`. `forward_isolated`
reruns a failed batch one text at a time. A width mismatch is still
reported to every caller without a retry (a property of the model,
K2-R1-4); a panic is still caught and named (K2-R1-5).

## Invariants kept

- **Per-lane order and receipts.** The round only embeds. Every job still
  plans, validates (`plan_still_holds`) and commits on its own, in FIFO
  order, through `WriteCtx::run`, and settles its own receipt.
- **#116's commit model.** Unchanged: only the vector gather is shared,
  through the per-call cache. A seeded vector is keyed by the exact text
  sent to the embedder, as the cache is; the live embedder is the
  process's one, so a text's vector does not depend on when it was
  embedded. The seed is dropped with the cache if the session's stamp
  moves between contracts.
- **Failure isolation.** A failed slot, or a round that fails or times out
  as a whole, is only missing from the seed or cache; the job embeds that
  text alone. Nothing in a round can fail another write.
- **The I/O bound.** The lane round runs inside the running job's
  `HYBRID_IO_TIMEOUT` budget (`Gather::io_started` is the round's start),
  so a job's round plus its own I/O never exceed one budget and
  `RECEIPT_WAIT_MAX` (built on that bound) is unchanged.
- **Fairness.** A lane has at most one round in flight, of at most 16
  texts, so one busy agent puts at most 16 texts ahead of another's at the
  embedder (two 8-wide forwards, about 200 ms on Metal). The lane bound is
  unchanged.
- **Low load.** No wait or debounce is added in the queue. A lane with one
  job embeds that job's texts in one round. One cost to note: the first
  job of a backlog now waits for the whole round (up to 16 texts) before
  it commits, so its own latency rises while the jobs behind it cost no
  round; at 8 agents backlogs are rare (apply p50 93 ms).
- **Telemetry.** `write_queue_serial_items_per_sec` (the observed rate) is
  still per applied write, but a round's time is charged to the job that
  ran it, so individual samples are less even; the mean is unchanged in
  meaning.

## Tests

- `embed::trait_tests`: the default `embed_batch` is `embed` per text, in
  order, each slot its own; the fixture's batch equals its single embeds.
- `embed::candle`: `one_bad_text_fails_only_its_own_caller`,
  `a_panicking_text_fails_only_its_own_caller_and_names_the_panic`,
  `a_width_mismatch_fails_every_caller_without_a_retry`,
  `a_healthy_batch_is_one_forward`, the two `length_chunks` tests, and
  `embed_batch_matches_single_embeds_and_fails_empty_text_in_its_slot` on
  the tiny CPU artifact. Probe: `metal_batch_throughput` (ignored).
- `graph::hybrid::tests::batching` (over `test_util::BatchingEmbedder`,
  which counts rounds and single embeds, can refuse a text, fail every
  round, and hold a gate): one round for a derive's items and parent ends
  with the unbatched vectors stored; capacity 1 keeps the old path; a
  refused item fails the write as `Embed` with nothing written; a
  whole-round outage falls back to single embeds and applies; seeded
  vectors are used; a spent I/O budget refuses as `EmbedUnavailable`;
  record_action in one round and a refused path failing as `Embed`;
  `planned_embed_texts` predicting exactly what the call embeds and
  nothing it would refuse.
- `writeq::pipeline_tests::batching`: five queued two-concept derives in
  one lane cost **2 rounds and 0 single embeds** (3 rounds and 2 singles
  with the prefetch disabled; checked by breaking it), apply in lane order
  (created then matched) and store the unbatched vectors; a refused text
  fails only its own write and its neighbours apply; a whole-round outage
  leaves each write to embed alone; two lanes prefetch independently and
  keep their own order.

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
