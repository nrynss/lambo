# K — Local-native embedder: candle in-crate, bypassing llama.cpp

**Two tracks, and the second is conditional.** K1 measures; K2 builds only if K1's numbers
clear their falsifiers. Opened 2026-08-21 (operator), after the J3 rounds established that a
large share of this branch's embedder-shaped defects are *environmental* — artefacts of the
embedder being a separate networked process — rather than logic defects.

Drafted first as an A sibling and moved out the same day: this is a different bet with a
different risk profile, not a variant of A. **A stays the hosted answer** (and needs J3's
transport-failure classification more than anything else on the branch, precisely because a
hosted embedder fails over the network by definition).

**Runs after J closes**, and after the E2E adversarial cycle. Before D, deliberately: D is
the long workstream and will be dogfooded heavily, so a rig that actually embeds what it
stores is worth having first.

---

## Why this is a workstream and not a nicety

What the current shape has cost, all measured on this branch rather than argued:

| defect | mechanism | status |
| --- | --- | --- |
| 92 of 100 dogfood concepts unembedded | embedder unreachable and/or refusing, silently | live damage, needs a backfill either way |
| J3-R3-1 (P1) | llama-server answered HTTP 500 at 1536 B input; the write applied with `embedding = NULL` and the receipt said "applied" | fixed in J3, but the *class* remains reachable |
| J3-R1 N1 (P1) | replay cannot tell an unreachable embedder from a refused input — a distinction created by making the timeout an `Err` | fixed in J3's round-1 remediation |
| F3 (P2) | on default config a dead llama fails **every** write, while spec §3.2 promises keyword-only is lawful | fixed as stated in J3 |

In-process embedding deletes the *cause* of the first, the second, and F3's teeth, and both
of N1's amplifiers: no transport means "unreachable" stops being a runtime state, there is no
30 s `HYBRID_IO_TIMEOUT` hang because compute is bounded by input length, and J2's lease
arithmetic can no longer aim an attach at a co-located server's unhealthy window — there is
no co-located server. What remains is a **startup** condition (weights missing, corrupt, or
too large), which is strictly better than a mid-life transient: deterministic at launch.

Two further wins that are not about defects:

* **The single-binary story completes.** `cargo install lambo`, first-run weight fetch, then
  offline. That is the ten-minute quickstart the adoption path needs, with llama.cpp out of
  the prerequisites.
* **J2 already makes the cost affordable.** Only the *holder* needs weights; proxies forward
  bytes and hold no graph, so they hold no model. N clients on a machine load one model, not
  N — the architecture J2 built is what makes this cheap.

**Support, verified 2026-08-21 (web research, recorded before any code):**
candle-transformers ships a native `xlm_roberta` module — `XLMRobertaModel` (the base encoder
dense BGE-M3 needs), plus MaskedLM and SequenceClassification. BGE-M3 is XLM-RoBERTa-large +
RetroMAE with 8192-token capacity, an exact architecture match. There is **no packaged bge-m3
example in candle's tree**, so K writes its own ~100–200 line load / CLS-pool / L2-normalize
path — the main correctness risk. The Metal backend is real (candle-metal-kernels; red-candle
ships Metal-accelerated embeddings downstream), but no published BGE-M3-on-Metal throughput
exists, which is why K1 exists. Fallback if hand-rolled pooling fights back: fastembed-rs has
first-class BGE-M3 including dense+sparse+ColBERT in one pass, but rides ONNX Runtime — a C++
dependency with no advertised macOS GPU path, i.e. the dependency class K exists to delete.
Fallback only, and a declared retreat rather than a silent one.

---

## K1 — Spike: the three numbers, each with its falsifier

Read-only measurement. Aborting after K1 is a **successful outcome** if the numbers say so
(G3's precedent: shelve with numbers).

1. **Correctness — cosine parity.** Embed the dogfood corpus's embedded concepts plus a
   synthetic spread (multilingual, 32 B → 8 KiB) through both the spike and the rig's
   llama-server; compare pairwise.
   *Falsifier:* median self-agreement below **0.99** means the pooling is wrong — fix it or
   take the fastembed retreat; it does not by itself kill K.
   **This is the gate that matters most.** Subtly wrong pooling produces vectors that are
   *plausible but wrong*, and recall then degrades silently — `applied ≠ embedded` wearing a
   different hat. Parity is not a nice-to-have.
2. **Throughput — CPU and Metal on this rig**, by the J3 probe methodology (representative
   input sizes, warm-up discarded, serial and concurrent legs).
   Baseline: llama.cpp q8_0 measures **110–141 items/s** here.
   *Falsifier:* if candle Metal **and** candle CPU both land under ~half of that, shelve —
   the operational wins do not justify a 2× recall-latency regression on the default path.
3. **Cost — cold start, compile, binary.** Weight-load latency is the one that can bite in
   production: on the stdio path a client spawns a serve *per session*, and J2's live probe
   measured opencode abandoning a server at **~32 s**. Measure cold start with weights on
   disk and with a first-run fetch.
   *Falsifier:* none absolute (the feature is gated, so it prices an option), but a cold
   start that does not leave generous headroom under ~30 s forces lazy-load-on-first-embed
   into K2's design rather than leaving it optional.

---

## K2 — Implement, if K1 clears

**Bundled with the `re-embed` verb, deliberately — they are one act.** Switching a session's
embedder is a re-embed event (the `EmbeddingContract` forbids mixed spaces in one session),
so migrating the dogfood rig to candle needs either a fresh session — discarding ~400
concepts that are also the test corpus for G3, the canonization fair test and the
revision-rate watch — or a backfill. And the backfill is **already owed** for the 92/100
unembedded damage. Done separately that is two re-embed passes; done together it is one, and
the dogfood store comes out fully embedded, in-process, and continuous.

Task order:

1. **`lambo re-embed`** — the verb, with the embedding-coverage counter in `lambo_stats`
   (`embedded/total`) that would have made the 92/100 damage visible the day it started.
   Independently valuable; required regardless of K1's verdict.
2. **The adapter** — `src/embed/candle.rs`, feature `embed-candle`, registry arm, config
   keys. Level B shape: adapter + feature + registry arm + docs, never a core fork.
   Contracts inherited and tested elsewhere: CON-7 (refuse empty/whitespace),
   CON-2 (no retry that silently changes the request), error classification
   (`Unavailable` vs `Backend` — note K narrows what `Unavailable` can even mean),
   L2-normalize before returning, reject non-finite and zero-norm.
3. **Stamp real model identity.** `bge_m3.rs` stamps `model = NULL` today, so the store can
   refuse a kind/dim mismatch but cannot tell two quantizations apart. The candle adapter
   stamps the served artefact (name, ideally the weight hash) — the same-artifact rule
   becomes machine-checked instead of hoped for.
4. **Weight-fetch UX** — hf-hub on first use, cached; never commit weights (standing rule);
   an explicit offline path; a clear failure when weights are absent.
5. **Lazy load on first embed** if K1's cold-start number demands it (recall-only sessions
   then pay nothing).
6. **CI row** — compile plus unit, no weights, no network. Live weight-loading tests stay
   `#[ignore]`d.
7. **The migration act** — one re-embed pass over the dogfood store, folded into the rig
   re-pin, with `git_sha` and the new contract as the proof it happened.

**Out of scope, explicitly:** the sparse and ColBERT legs (dense parity first — they are
future hybrid-recall value, not this workstream's promise); GGUF/quantized loading in candle
(fp16 safetensors first, quantize later if RAM says so); and any change to the **default**
feature set — that decision waits for the numbers and is made against the migration story,
not inside it.

---

## What K does not change

* **Durability is untouched.** In-process embedding does not make a write durable — a
  `kill -9` still loses the write-behind tail. J3's intent WAL, replay, idempotency and the
  receipt state machine all stay exactly as they are.
* **A is not superseded.** Two machines sharing one embedding space is still A's case, and
  a hosted embedder remains the convenient answer there. What K removes is the claim that it
  is the *only correct* one.
* **The transport-failure work stays load-bearing** — see the A pointer: a hosted embedder
  guarantees the failure class K avoids locally.

## Done when

**K1:** the three numbers exist with their method stated; each falsifier applied honestly;
the recommendation recorded here (build / retreat to fastembed / shelve) with the evidence.

**K2 (only if K1 clears):** `kind = "candle"` builds an adapter and embeds locally;
cosine parity against the K1 gate holds in the shipped adapter, not just the spike; the
contract stamps a real model identity; `lambo re-embed` migrates a store between contracts
and repairs NULL-embedding rows, with coverage visible in `lambo_stats`; the dogfood rig
runs on it with its full history intact; CI carries the compile row; and the cold-start
number is documented where an operator wiring a stdio client will read it.
