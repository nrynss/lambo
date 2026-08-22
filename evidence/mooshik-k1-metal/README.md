# K1 — candle BGE-M3 in-process: the three numbers, Metal and CPU legs

The measurement half of [K — Local-native embedder](../../dev-diary/lambo-for-mooshik/K-candle-embedder.md).
K1 is read-only: it produces three numbers, applies each falsifier, and records a
recommendation. **Aborting after K1 is a successful outcome if the numbers say so.**

This capture covers the **Metal and CPU legs on one Apple-silicon Mac**. The CUDA leg is
measured separately on an NVIDIA machine and is not in scope here; the two get merged into
the workstream file later. Nothing in this run is wired into `src/` — no registry arm, no
feature flag, no edge from the `lambo` crate to the spike. That is K2, and K2 is gated on
these numbers.

The spike is [`spikes/k1-candle-bgem3/`](../../spikes/k1-candle-bgem3/) — a standalone
crate that loads BGE-M3 through `candle-transformers`' `XLMRobertaModel`, takes the **CLS**
token of the last hidden state, and L2-normalizes. 471 lines in total; the load / pool /
normalize path K describes is under 100 of them, and the rest is the CLI and the three
measurement harnesses.

---

## Machine shape

| | |
|---|---|
| Chip | Apple M3 Pro (arm64) |
| RAM | 18 GB |
| OS | macOS 26.6.2, build 25G83 |
| Rust | 1.97.1 (`rust-toolchain.toml`) |
| candle | **0.11.0** — `candle-core`, `candle-nn`, `candle-transformers`, `candle-metal-kernels` |
| features | `metal` (Metal leg), `accelerate` (CPU leg — macOS BLAS) |
| tokenizers / hf-hub | 0.21.4 / 0.4.3 |
| exact resolved graph | [`cargo-lock-excerpt.txt`](cargo-lock-excerpt.txt) (also `metal` 0.29.0, `objc2-metal` 0.3.2, `accelerate-src` 0.3.2) and [`cargo-tree-versions.txt`](cargo-tree-versions.txt) |

The repository gitignores `spikes/*/Cargo.lock` for every spike, so the lockfile itself is
not committed; [`cargo-lock-excerpt.txt`](cargo-lock-excerpt.txt) is the committed record of
what actually resolved, taken from that lockfile at capture time.
| Reference | llama.cpp **version 0.1.2-dev (build 10520, commit cd644c395)**, AppleClang 21.0.0 |

**Model artifact (the spike side).** `BAAI/bge-m3`, revision
`5617a9f61b028005a4858fdac845db406aefb181`, fetched by `hf-hub` into the default HF cache
**outside the repo**. Weights are never committed — standing rule.

```
pytorch_model.bin   2,271,145,830 B   sha256 b5e0ce3470abf5ef3831aa1bd5553b486803e83251590ab7ff35a117cf6aad38
tokenizer.json                        sha256 21106b6d7dab2952c1d496fb21d5dc9db75c28ed361a05f5020bbba27810dd08
```

**Model artifact (the reference side).** The DOGFOOD-SETUP GGUF, checksum-verified against
the runbook before use:

```
~/models/bge-m3-q8_0.gguf   634,553,760 B   sha256 aa473d51f451a22f0fcf39ba3330c14bed38a385712b1113440f69df4047a173
```

### The spec's "fp16 safetensors" does not exist, and that is a K2 input

K names the spike's input as "fp16 safetensors via hf-hub". **`BAAI/bge-m3` ships no
`model.safetensors` at all.** The repository's only full-precision weight file is
`pytorch_model.bin` — a PyTorch pickle, `torch_dtype: "float32"` — beside an ONNX copy.

The spike therefore loads the canonical pickle through `VarBuilder::from_pth` rather than
a third-party safetensors mirror, because "same artifact everywhere" is the rule the whole
rig rests on (DOGFOOD-SETUP §1) and a mirror is a different artifact whatever its contents.
candle reads the pickle without complaint. The costs it imposes are real but small, and
both are recorded below: the load is ~1.0–1.8 s rather than an mmap, and requesting f16
reads f32 and converts, so *peak* memory during load is higher than the resident model.
Converting once to local fp16 safetensors is a K2 option, not a K1 finding.

> **Annotation (2026-08-22, after this capture):** the conversion was subsequently done,
> bitwise-verified, and published as
> [`rockus/bge-m3-f16-safetensors`](https://huggingface.co/rockus/bge-m3-f16-safetensors)
> (operator-owned, provenance anchored to this same canonical artifact — see "Weight
> artifact published" in [K-candle-embedder.md](../../dev-diary/lambo-for-mooshik/K-candle-embedder.md)
> and `scripts/embedder/convert_bgem3_f16.py`). Everything above is unchanged capture.

---

## Artifacts

| Artifact | What it is |
|---|---|
| [`corpus.py`](corpus.py) → [`corpus.jsonl`](corpus.jsonl) | The 82-item parity corpus, generated deterministically: the committed BGE-M3 evidence texts plus a 10-language spread at 32 B → 8 KiB. |
| [`reference_llama.py`](reference_llama.py) | The reference side. Writes back exactly what `llama-server` returned. |
| [`compare.py`](compare.py) | Pairwise cosine, per item, with the distribution and the per-group/per-language/per-size breakdown. |
| [`bench_llama.py`](bench_llama.py) | The reference's throughput on the *same* harness shape as the spike. |
| [`run.sh`](run.sh) | The whole capture end to end. |
| [`bench_repeats.sh`](bench_repeats.sh), [`coldstart_repeats.sh`](coldstart_repeats.sh), [`cost_build.sh`](cost_build.sh) | The three legs' drivers. |
| [`summarize.py`](summarize.py) → [`summary.json`](summary.json) | Every number quoted below is derived here, so the prose and the raw files cannot drift. |
| `parity-*.json` / `parity-*.csv` | Per-item results: cosine, byte size, token count, both norms. |
| `vectors-*.jsonl.gz` | The raw 1024-d vectors both sides produced, gzipped. `compare.py` reads them gzipped, so a re-verification needs no unpacking. |
| `bench-repeats-*.jsonl` | The authoritative throughput readings — three repeats per backend. Every throughput figure below is the median of these. |
| `bench-single-*.jsonl` | Single readings taken while setting the legs up, kept because two of them are data no repeat covers: the plain-CPU build (no `accelerate`) and the first, colder Metal f16 batch reading. Not used for any quoted median. |
| [`coldstart.jsonl`](coldstart.jsonl), [`cost-build.jsonl`](cost-build.jsonl) | Five fresh process launches per backend; compile time and binary size per feature configuration. |

---

## Number 1 — cosine parity. **PASS**

*The gate that matters most.* Subtly wrong pooling produces vectors that are plausible but
wrong, and recall then degrades silently.

**Method.** 82 texts through both implementations, one text per forward on the spike side
so batch padding cannot move a vector. Corpus: the nine committed evidence texts (the three
derived concepts, the same three under `hybrid::derive`'s context framing, the three recall
queries), a 10-language × 7-size synthetic spread (32 B, 128 B, 512 B, 1 KiB, 2 KiB, 4 KiB,
8 KiB), and three non-prose shapes (a Rust fragment, a mixed-script line, punctuation).
Cosine is `dot(a,b)/(|a||b|)` — the metric's own definition, applied symmetrically.

**The two sides are genuinely independent.** The reference is BGE-M3 **q8_0 through
llama.cpp**: a different tokenizer, a different graph, a different quantization, written by
different people. Nothing was applied to it — no re-normalization, no truncation, no pooling
change. The reference returns unit vectors on its own (‖v‖ ∈ [0.99999995, 1.00000005]),
which is recorded per item in the CSVs rather than asserted, so the cosine denominator is
visibly a no-op on that side rather than a silent fix-up.

| Spike side | n | median | min | p05 | max | below 0.99 | verdict |
|---|---|---|---|---|---|---|---|
| candle **Metal f16** | 82 | **0.999720** | 0.997920 | 0.999070 | 0.999868 | 0 | PASS |
| candle **Metal f32** | 82 | **0.999721** | 0.997933 | 0.999066 | 0.999868 | 0 | PASS |
| candle **CPU f32** | 82 | **0.999721** | 0.997933 | 0.999066 | 0.999868 | 0 | PASS |

Holding across every axis, not just in the median (Metal f16 shown; the other two match to
the fifth decimal):

| by group | n | median | min |
|---|---|---|---|
| committed evidence texts | 9 | 0.999826 | 0.999802 |
| synthetic spread | 70 | 0.999709 | 0.997920 |
| non-prose edge shapes | 3 | 0.999781 | 0.999711 |

| by size | 1 KiB | 2 KiB | 4 KiB | 8 KiB |
|---|---|---|---|---|
| median | 0.999713 | 0.999694 | 0.999567 | 0.999527 |
| min | 0.999065 | 0.999496 | 0.999494 | 0.998945 |

Per language, all ten medians sit between 0.999170 (es) and 0.999763 (zh); the single worst
item in the whole run is `synth-es-32` at **0.997920** — a 32-byte Spanish fragment, i.e.
the shortest inputs, where q8_0 rounding has the fewest tokens to average out over.

**Falsifier applied.** *Median self-agreement below 0.99 means the pooling is wrong.*
The median is 0.99972 and **the minimum over all 82 items on all three backends is 0.99792
— every single item clears the gate, not merely the median.** The falsifier does not fire.
The pooling is right: CLS (not mean), pad id 1 (not tokenizers' default 0), L2-normalized,
tokenizer matched. No debugging round was needed and **no fastembed retreat is recorded**.

The gate prices in that the reference is q8_0-quantized, so parity was never going to be
1.0. The residual ~0.0003 is consistent with quantization noise alone.

### The reference is the rig's own reference — cross-checked, not assumed

The full sweep needs an 8 KiB input to succeed, so it ran against a server started for this
capture on **:8099** with `-c 8192 -b 8192 -ub 8192`. The rig's own embedder server on
**:8080** was left running and unreconfigured throughout.

To show those capacity flags changed capacity and nothing else, the nine evidence texts were
also embedded by the untouched :8080 server and compared to the :8099 output:

```
llama :8080 (baseline flags) vs llama :8099 (K1 flags)
  n=9  median=1.000000  min=1.000000  max=1.000000   PASS
```

Identical to six decimal places. The reference used for the gate is the rig's reference.

> **Correction to the brief.** The K1 brief described `:8080` as "an unrelated LFM2.5-230M
> rig" to be left alone. It is not: `:8080` is running
> `llama-server --embedding -m ~/models/bge-m3-q8_0.gguf`, i.e. **the BGE-M3 dogfood
> embedder itself**, serving the exact GGUF whose sha256 DOGFOOD-SETUP §1 pins. It was still
> left undisturbed — the sweep used its own server — but the fact matters for whoever reads
> this next, because it means reference option (a) was available on this machine all along.

---

## Number 2 — throughput. **Falsifier does not fire** (Metal clears; CPU does not)

**Method — J3's probe, reproduced.** From `src/writeq.rs::probe_embedder`: warm-up embeds
discarded, then a serial leg and a `PROBE_CONCURRENCY = 4`-wide leg, at J3's two input
shapes — `PROBE_TEXT` (35 B, 12 tokens) and `PROBE_TEXT_BYTES` (1024 B, 287 tokens). Four
warm-up embeds discarded, 32 iterations, **three repeats**; every figure below is the median
of the three. Repeats matter here: the recorded baseline is itself quoted across repeats
(110, 131, 141), and J3-R1-2 was precisely a finding about warmth.

Three legs are reported per backend, because "concurrency" means something different
in-process than it does over HTTP:

- **serial** — one text per forward, the like-for-like of a single `embed()` call.
- **4-wide** — four OS threads sharing one model, the direct analogue of four concurrent
  `embed()` calls against llama-server.
- **batch-4** — four texts in one forward. *This is what an in-process adapter would
  actually do* with four outstanding requests, and it has no analogue on the HTTP path.

### 35 B / 12 tokens (J3 `PROBE_TEXT`)

| backend | serial | 4-wide | batch-4 |
|---|---:|---:|---:|
| llama.cpp q8_0 — **reference** | 75.6 | **146.2** | — |
| candle **Metal f16** | 66.9 | **69.6** | **190.5** |
| candle Metal f32 | 52.8 | 54.7 | 144.0 |
| candle CPU f32 + Accelerate | 12.4 | 30.2 | 25.9 |
| candle CPU f32, no Accelerate | 9.4 | 19.1 | 18.0 |

### 1024 B / 287 tokens (J3 `PROBE_TEXT_BYTES`)

| backend | serial | 4-wide | batch-4 |
|---|---:|---:|---:|
| llama.cpp q8_0 — **reference** | 20.2 | **20.7** | — |
| candle **Metal f16** | 11.5 | **11.6** | 10.8 |
| candle Metal f32 | 10.8 | 10.9 | 10.0 |
| candle CPU f32 + Accelerate | 1.6 | 4.6 | 1.7 |
| candle CPU f32, no Accelerate | 1.0 | 2.2 | 1.1 |

**The harness agrees with the recorded baseline.** Re-measured here, llama.cpp reads
**146.2 items/s** 4-wide at 35 B against the branch's recorded **110–141**, and **20.7**
at 1024 B against the recorded **~19–22**. The harness is therefore calibrated, and the
candle figures can be read against either the recorded band or this re-measurement.

**Falsifier applied.** *If candle Metal **and** candle CPU both land under ~half the
baseline, shelve.* Half of the recorded 110–141 band is **55.0–70.5** items/s at 35 B;
half of 19–22 is **9.5–11.0** at 1024 B.

| | 35 B, 4-wide | 1024 B, 4-wide | verdict |
|---|---|---|---|
| candle Metal f16 | 69.6 — **clears** | 11.6 — **clears** | clears at both sizes |
| candle Metal f32 | 54.7 — marginally under; batch-4 144.0 clears | 10.9 — **clears** | clears |
| candle CPU f32 | 30.2 — under | 4.6 — under | **under at both sizes** |

**The conjunction is not satisfied, so the falsifier does not fire. There is no shelve
signal for this rig.** Stated without varnish: candle Metal does not *match* llama.cpp on
the like-for-like 4-wide leg — it runs at roughly half (35 B) to somewhat over half
(1024 B) of the reference. It is the *batched* leg where candle wins outright, at 190.5
items/s against the reference's 146.2.

**CPU alone is not a viable default on this rig**, and that is a firm finding rather than a
marginal one: 30.2 items/s 4-wide at 35 B is ~21–27% of the baseline band, and 4.6 at
1024 B is ~21–24%. Enabling the `accelerate` feature is worth roughly 1.6–2.1× over a plain
CPU build and is not close to closing the gap. A CPU-only candle default would be the 2×
recall-latency regression the falsifier was written to prevent.

### The finding K2's design actually turns on

**Metal does not scale with thread concurrency, but does scale with batching.** Metal f16
reads 66.9 items/s serial and 69.6 4-wide — four threads buy 4%, because the Metal command
queue serializes the work. The same four items in *one* forward read **190.5 items/s**, a
2.7× gain over serial.

So an in-process adapter must **coalesce concurrent `embed()` calls into one batched
forward** rather than putting a mutex around the model and letting callers queue. Done that
way candle Metal beats the llama.cpp reference at 35 B (190.5 vs 146.2); done the naive way
it loses to it by half. The gain does not carry to 1024 B (batch-4 10.8 vs serial 11.5 —
the GPU is already saturated by a 287-token sequence), so the coalescing is a small-input
optimization specifically, which is the shape most recall queries have.

---

## Number 3 — cost. **Comfortable, ~15–25× headroom under the gate**

**Cold start, weights already on disk.** Five fresh process launches per backend;
`coldstart` timestamps at `main()` entry, so `→ first vector` is what a stdio client
spawning a serve per session actually waits for. Weights are in the page cache — stated
rather than hidden, since that is the steady-state case the gate is about.

| backend | weight load (median) | **→ first vector** (median) | → first vector (max of 5) | subsequent embed |
|---|---:|---:|---:|---:|
| CPU f32 | 1041 ms | **1173 ms** | 2000 ms | 77.7 ms |
| Metal f16 | 1481 ms | **1523 ms** | 1729 ms | 15.7 ms |
| Metal f32 | 1504 ms | **1629 ms** | 1886 ms | 19.0 ms |

**First-run fetch.** 2,271,145,830 B in **99.0 s** (≈23 MB/s) — [`fetch.jsonl`](fetch.jsonl).
Single measurement, network variability not characterized: one sample, one link, one day.
Once cached it is a sub-millisecond path lookup (`hub_resolve_ms` median 0.36 ms).

**Compile and binary.**

| config | compile | binary | stripped |
|---|---:|---:|---:|
| `metal,accelerate` — **from clean** | **72.0 s** | 11,148,928 B (10.6 MiB) | 8,923,968 B (8.5 MiB) |
| `metal` — warm deps | 28.9 s | 11,352,944 B | 9,092,240 B |
| `accelerate` — warm deps | 27.5 s | 10,146,768 B | 7,999,968 B |
| no features — warm deps | 27.5 s | 10,351,888 B | 8,168,240 B |

Only the first row is a from-clean build; the other three had the dependency graph warm and
are labelled as such rather than quoted as clean builds. Binary size carries hf-hub's
default features, which pull `reqwest` and `tokio` for an async API the spike never calls —
a lighter fetch path is available to K2 and would take some of that 10.6 MiB back.

**Memory.** Peak resident, from `/usr/bin/time -l`:

| backend | max RSS | peak footprint |
|---|---:|---:|
| CPU f32 | 2.68 GB | 2.62 GB |
| Metal f16 | 3.46 GB | 4.72 GB |

f16 halves the *resident* model but raises the *peak*, because the fp32 pickle is read and
then converted — the transient holds both. On this 18 GB machine neither is a problem; on a
smaller one the peak, not the model, is the number that would bite. Converting once to
local fp16 safetensors would remove the transient.

**Falsifier applied.** *No absolute falsifier, but a cold start that does not leave generous
headroom under ~30 s forces lazy-load-on-first-embed into K2's design rather than leaving it
optional.* Worst observed launch across all fifteen: **2.00 s**. Against the ~30 s at which
J2's live probe measured opencode abandoning a server, that is **~15× headroom on the worst
sample and ~20–25× on the median**. The headroom is generous. **Lazy-load-on-first-embed
stays optional** — it remains worth having so recall-only sessions pay nothing, but this
number does not force it into the design.

---

## What did not need fixing, recorded because a clean run is also a result

- **CLS vs mean pooling** — right on the first attempt; the parity number would have
  collapsed to ~0.8–0.9 had it been mean.
- **Pad id** — set from `config.pad_token_id` (1), not tokenizers' default of 0. This one is
  a live trap rather than a hypothetical: `XLMRobertaEmbeddings` derives position ids from
  `input_ids.ne(pad_token_id)`, so a wrong pad id silently shifts every position in a padded
  batch. It is commented at the call site in the spike.
- **fp16 vs fp32 accumulation** — f16 costs nothing measurable in parity (median 0.999720 vs
  0.999721) while running ~27% faster and halving resident memory. candle hardcodes the
  attention mask to F32 inside `XLMRobertaModel::forward`, which looked like it would force
  an F32 model dtype; it does not, and f16 runs clean.
- **8192-token capacity** — the 8 KiB items tokenize well inside the limit and hold parity
  (median 0.999527). No truncation divergence between the two sides at any size.

---

## Recommendation — **BUILD-WORTHY, for this rig**

All three numbers clear. Parity passes outright, with every one of 82 items above the gate
rather than just the median. Throughput's falsifier does not fire: Metal clears half the
baseline at both input sizes, and beats the reference outright when batched. Cost is not
close to the gate.

Carried forward into K2, if the NVIDIA leg agrees:

1. **Metal must be the default on Apple silicon; CPU is a fallback, not a peer.** CPU sits
   at ~21–27% of baseline. If a CPU-only candle path is ever the default, it is a regression.
2. **Coalesce concurrent embeds into one batched forward.** This is the difference between
   190.5 and 69.6 items/s at 35 B — between beating llama.cpp and losing to it by half.
3. **Build with `accelerate` on macOS** for the CPU fallback: 1.6–2.1× for one feature flag.
4. **f16 on Metal**, on this evidence: same parity, faster, half the resident weights.
5. **Lazy-load stays optional** — the cold-start number does not force it.
6. **Stamp the model identity K2 asks for.** The spike already pins revision
   `5617a9f6…b181`; the sha256 of `pytorch_model.bin` is recorded above and is the natural
   value for the contract's `model` field, which `bge_m3.rs` leaves NULL today.
7. **Decide the weight-file story.** No safetensors exists upstream; either keep
   `from_pth` or convert once to a local fp16 safetensors cache (which would also remove the
   f16 load transient).

**Scope of this recommendation.** One machine, one OS, one chip. It says nothing about the
CUDA leg, and nothing about whether candle should become the *default* feature set — K
defers that decision to the migration story, and it should stay deferred.

---

## Reproducing

```sh
# A reference server that can take an 8 KiB input, NOT the rig's own :8080:
llama-server --embedding -m ~/models/bge-m3-q8_0.gguf \
  --port 8099 --host 127.0.0.1 -c 8192 -b 8192 -ub 8192

bash evidence/mooshik-k1-metal/run.sh "$(pwd)" http://127.0.0.1:8099
```

First run fetches ~2.3 GB into the default HF cache outside the repo; later runs are offline.
The reference is q8_0 and llama.cpp is not bit-deterministic across builds, so the cosines
are expected to move in the fourth decimal place. **The claims are the verdicts** — every
item above 0.99, Metal clearing half the baseline, CPU not, and cold start an order of
magnitude under the gate — not the digits.
