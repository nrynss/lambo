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

**Amended 2026-08-22 (operator): K1 unblocked early.** K1 is read-only measurement touching
no J surface, so it runs concurrently with the J E2E adversarial cycle — the CUDA leg on an
NVIDIA machine, the Metal/CPU leg on this rig in an isolated worktree. The K2 gate is
unchanged: the build / retreat / shelve decision waits for K1's numbers **and** the E2E
verdict, because K2's economics read J2's proxy architecture as settled.

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

### K1 — MacBook Pro leg (spike, measured 2026-08-22). All three falsifiers clear.

**Scope: one machine, one chip — the Metal/CPU half of K1.** The CUDA leg runs separately
on an NVIDIA machine; the build / retreat / shelve call merges both legs **and** the J E2E
verdict, per the amendment above. Full capture, method, raw vectors and per-item results:
[`evidence/mooshik-k1-metal/`](../../evidence/mooshik-k1-metal/README.md), landed at
`ba558a4`. Spike crate: `spikes/k1-candle-bgem3/` (471 lines, standalone — nothing wired
into `src/`).

**Rig.** MacBook Pro, Apple M3 Pro (6P+6E cores), 18 GB, macOS 26.6.2. Rust 1.97.1,
**candle 0.11.0** (`metal` for the Metal leg, `accelerate` for the CPU leg), tokenizers
0.21.4, hf-hub 0.4.3 — exact resolved graph in the evidence directory.

**Model under test (spike side).** `BAAI/bge-m3`, revision
`5617a9f61b028005a4858fdac845db406aefb181`, via `candle-transformers`' `XLMRobertaModel`
(CLS pool, pad id 1, L2-normalize). **Correction to this doc's premise: `BAAI/bge-m3`
ships no safetensors at all** — the only full-precision artifact is the fp32
`pytorch_model.bin` (2,271,145,830 B, sha256
`b5e0ce3470abf5ef3831aa1bd5553b486803e83251590ab7ff35a117cf6aad38`), loaded through
`VarBuilder::from_pth` rather than a third-party safetensors mirror (same-artifact rule,
DOGFOOD-SETUP §1). Run as f16 and f32 on Metal, f32 on CPU. The weight-file story (keep
`from_pth` vs convert once to local fp16 safetensors, which would also remove the f16
load transient) is a K2 decision.

**Reference tested against.** The dogfood rig's own embedder artifact: BGE-M3 **q8_0
GGUF** (`~/models/bge-m3-q8_0.gguf`, 634,553,760 B, sha256
`aa473d51f451a22f0fcf39ba3330c14bed38a385712b1113440f69df4047a173` — the exact file
DOGFOOD-SETUP §1 pins), served by **llama.cpp build 10520** — genuinely independent
tokenizer, graph and quantization. The sweep ran its own server on :8099 (8 KiB capacity
flags); the rig's live :8080 server was left untouched and cross-checked against :8099 at
**cosine 1.000000** (n=9), so the gate's reference is provably the rig's reference.
(:8080 is the BGE-M3 dogfood embedder itself, not an unrelated rig — recorded here
because the spike's brief said otherwise.)

**The three numbers.**

1. **Parity: PASS outright.** Median **0.999720** (Metal f16) over 82 texts — the nine
   committed evidence texts, a 10-language × 7-size spread (32 B → 8 KiB), three non-prose
   shapes. **Every individual item clears 0.99** (worst 0.997920, a 32-byte Spanish
   fragment — where q8_0 rounding has the fewest tokens to average over). Pooling right on
   the first attempt; no debugging round; **no fastembed retreat recorded**. f16 vs f32:
   identical to the fifth decimal.
2. **Throughput: falsifier does not fire.** At J3's probe shapes, 4-wide: Metal f16
   **69.6 items/s** at 35 B and **11.6** at 1024 B — clears half of the recorded 110–141 /
   ~19–22 bands at both sizes; CPU+Accelerate 30.2 / 4.6 — under at both, so **CPU alone is
   not a viable default on this rig**. The harness re-measured llama.cpp at 146.2 / 20.7,
   inside the recorded bands — calibrated. Stated without varnish: Metal does not match
   llama.cpp like-for-like; it wins only batched (next paragraph).
3. **Cost: ~15–25× headroom.** First vector 1.2–1.5 s median, worst of fifteen launches
   **2.00 s** against the ~30 s gate — **lazy-load stays optional**. From-clean compile
   72 s; binary 10.6 MiB (8.5 stripped); first-run fetch 99 s for 2.27 GB (one sample).
   Peak RSS 2.7 GB (CPU f32) / 3.5 GB resident, 4.7 GB transient peak (Metal f16 — the
   fp32 pickle is read then converted; the transient is what bites on a small machine).

**The finding K2's design turns on: Metal scales with batching, not thread concurrency.**
Four threads buy 4% (66.9 → 69.6 items/s — the Metal command queue serializes); the same
four items in **one forward** read **190.5 items/s, beating the reference's 146.2**. An
in-process adapter must coalesce concurrent `embed()` calls into batched forwards, not
mutex the model — done naively it loses to llama.cpp by half. The gain is a small-input
effect (287-token sequences already saturate the GPU), which is the shape most recall
queries have.

**Recommendation for this rig: BUILD-WORTHY**, carried into K2 if the NVIDIA leg agrees:
Metal default on Apple silicon (CPU is a fallback, not a peer); coalesce into batched
forwards; build with `accelerate` for the CPU fallback (1.6–2.1×); f16 on Metal (same
parity, ~27% faster, half the resident weights); the spike's pinned revision + weight
sha256 are the natural values for the contract's `model` field K2 task 3 wants stamped.
Says nothing about the CUDA leg or the default-feature-set decision, which stays deferred
to the migration story.


### K1 — NVIDIA leg (spike, measured 2026-08-22, this rig). Falsifiers: parity clear, cost clear, throughput clears on GPU only.

**Rig.** Linux (CachyOS, kernel 7.1.8), Ryzen 5 3600 (6C/12T), RTX 4070 SUPER 12 GB
(shared with unrelated desktop workloads — ~3.7 GiB resident during the GPU runs), CUDA
toolkit at `/opt/cuda`. Rust 1.97.1, **candle 0.11.0** (`cuda` feature for the GPU legs),
tokenizers 0.23.x, hf-hub 1.0.0. Spike at `spikes/k1-candle/` (branch `k1-spike`); logs in
`spikes/k1-candle/data/*.log`.

**Model under test.** Identical artifact to the MacBook leg: `BAAI/bge-m3`, revision
`5617a9f61b028005a4858fdac845db406aefb181`, canonical fp32 `pytorch_model.bin`
(sha256 `b5e0ce3470abf5ef3831aa1bd5553b486803e83251590ab7ff35a117cf6aad38` — re-hashed
here and byte-identical), loaded through `VarBuilder::from_pth`; f16 is a post-load
`VarBuilder::to_dtype` cast. CLS pool, pad id 1, L2-normalize. An earlier run against a
third-party safetensors conversion (`Bylaw/BAAI-bge-m3`) produced bit-for-bit the same
agreement statistics; all recorded numbers below are from the canonical artifact.

**Deviations from the MacBook-leg method, in run order (all corrected or bounded):**

1. **Weights: third-party safetensors first.** The first pass loaded
   `model.safetensors` from `Bylaw/BAAI-bge-m3` (a conversion mirror; this spike
   pre-dated reading the MacBook leg's same-artifact correction). Parity against the
   q8_0 reference on that pass: median 0.99925, min 0.98951, ρ = 0.9995 — numerically
   identical to the canonical-`pytorch_model.bin` re-run, so the mirror is effect-equivalent,
   but the recorded gate rests on the canonical artifact only.
2. **Reference server: rig's live :8080, not a dedicated sweep server.** The MacBook leg
   ran its own :8099 and cross-checked :8080 at cosine 1.000000 (n=9); this leg tested
   against :8080 directly with `--batch-size/--ubatch-size 8192`, so no independent
   cross-check pair exists here. Mitigation: the reference artifact is the exact pinned
   GGUF file, and the throughput baseline was re-measured today through the same harness
   (99.8–205 items/s short inputs) rather than citing J3's numbers.
3. **First-run fetch sample taken on the wrong artifact.** The 57.9 s fresh-cache number
   was measured downloading the mirror's 2.27 GB safetensors before the switch to the
   canonical bin; treat it as a size-class sample (the MacBook leg measured 99 s for the
   actual `pytorch_model.bin`). Warm-start numbers are unaffected (weights already local).
4. **Corpus differs slightly:** 71 live dogfood concepts + 30 synthetic (n=101) here vs
   82 texts there; both follow the K brief (dogfood corpus + multilingual spread,
   32 B → 8 KiB). Gate margins are far wider than the corpus delta.
5. **Toolchain drift:** hf-hub 1.0.0 / tokenizers 0.23.x here vs 0.4.3 / 0.21.4 on the
   MacBook (candle 0.11.0 on both); noted because K2 pins these and the two legs must not
   silently diverge again.

**Reference tested against.** The rig's own dogfood embedder: BGE-M3 q8_0 GGUF (the file
DOGFOOD-SETUP §1 pins) via llama.cpp build 10520 on `127.0.0.1:8080`. One rig fix was
required and is a finding of its own: this llama.cpp build clamps
`n_batch = n_ubatch = 512` by default, so any input over ~512 tokens fails with HTTP 500
("input too large") — the J3-R3-1 failure class, reachable by plain input length. The
parity/bench harness needs `--batch-size 8192 --ubatch-size 8192` (long flags only;
this build rejects `-u`).

**1. Parity: PASS.** 71 live dogfood concepts + 30 synthetic (multilingual, 32 B → 8 KiB),
bs=1 fp32 CPU candle vs q8_0 llama: **median self-agreement 0.99925**, min 0.98951
(one 32 B synthetic; short strings give quantization noise nowhere to hide), 101 items.
Rank agreement (Spearman over all pairwise sims): **ρ = 0.9995** — rankings survive the
swap, so the pooling is not merely plausible-but-wrong. The f16 CUDA leg re-ran the same
sweep: median **0.99924**, ρ = 0.9995 — half precision costs nothing measurable.
Falsifier (median < 0.99 → fix pooling or retreat to fastembed): does not fire.

**2. Throughput (items/s, serial | batch16, warm-up discarded).**

| input | candle CPU f32 | candle CUDA f16 | candle CUDA f32 | llama q8_0 CPU (same harness today) |
| --- | --- | --- | --- | --- |
| 32 B | 3.7 \| 21.7 | 180–187 \| 2432–2826 | 85 \| 1608 | 99.8 |
| 256 B | 2.2 \| 5.0 | 175–182 \| 934–1020 | 156 \| 426 | 205.3 |
| 2048 B | 0.5 \| 0.5 | 76–82 \| 89–98 | 37–40 \| OOM@bs16* | 97.6 |
| 8192 B | 0.1 \| 0.1 | 10.6–11.3 \| 8–11 | OOM* | 23.7 |

\* f32 attention activations are quadratic in token count; at 2 k+ tokens per sequence a
batched fp32 forward exceeds the free VRAM on this shared GPU. Not measured on a
dedicated GPU; not the deployment shape either (f16 halves it and matches parity).

The falsifier as written ("candle Metal AND candle CPU both under half of baseline →
shelve") transfers here with CUDA in Metal's place: **candle CPU lands at ~2–4 % of the
llama baseline everywhere — the CPU-only deployment is dead**, and no MKL/BLAS feature is
plausibly worth 25×. **CUDA f16 does not fire it**: serial it tracks or beats the CPU
llama baseline at ≤256 B (180 vs 100–205) and gives back ground on long inputs (10.6 vs
23.7 at 8 KiB, on a GPU three-quarters occupied by other work); batched it wins by an
order of magnitude at exactly the short-input shape recall queries have (2400–2800 vs
~100). Same conclusion as the MacBook leg's "scales with batching" finding, independently
arrived at.

**3. Cost.** Warm start (weights on disk) process→ready: **2.79 s CPU / 1.99 s CUDA f16**,
first embed +0.19–0.36 s, steady-state embed 18 ms (CUDA f16). First-run fetch 57.9 s for
2.27 GB (safetensors sample; one-time, not per-session). Against the ~30 s stdio spawn
gate (J2: opencode gave up at ~32 s) that is 15× headroom — **lazy-load stays optional**,
agreeing with the MacBook leg. Falsifier: none absolute; fires nothing.

**Recommendation for this rig: BUILD-WORTHY, conditional on a GPU being present** — the
two legs now agree where they overlap and cover each other's platforms. Concretely for K2:
`embed-candle` selects Metal on Apple silicon, CUDA elsewhere, and **refuses to resolve
(hard error, Level B rule) when neither accelerator exists unless the operator explicitly
pins `device = "cpu"`** knowing they get ~3 % of the llama-server path's throughput; the
bge_m3/llama.cpp backend stays the default on hostless-CPU deployments pending the
migration story. Coalesce into batched forwards (both GPUs say so); f16 default on GPU
(same parity, half the weights); stamp revision + weight sha256 as the contract identity
(already pinned identically on both rigs). Cold-start number for operator docs: ~2–3 s
warm, ~60 s once for the weight fetch.

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
