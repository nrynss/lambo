# A — Gemini embedder (`embed-gemini`)

**Goal:** a hosted embedder so the same autobiography can be written from two machines.

**Why not BGE-M3:** it needs a local GPU under llama.cpp. The desktop has a 4070; the MacBook does
not. Two machines writing into different embedding spaces violates the `EmbeddingContract` stamped
on the session, and recall degrades silently rather than failing.

**Templates:** `embed-bedrock` for registry wiring (`src/embed/mod.rs`), `src/embed/bge_m3.rs` for
adapter shape — it is the other HTTP embedder, so its error classification and normalization carry
over almost unchanged.

---

## A1 — Registry wiring

`EmbedderKind::Gemini` and its six touch points in `src/embed/mod.rs`:

| Touch point | Note |
| --- | --- |
| enum variant | doc comment naming the feature, as `Bedrock` does |
| `feature_name()` | `"embed-gemini"` |
| `is_compiled()` | `cfg!(feature = "embed-gemini")` |
| `is_ready()` | **`false` until A3 lands**, then `true` |
| `FromStr` | `"gemini"`, plus whatever aliases; update the three "expected …" error strings |
| `Display` | `"gemini"` |

Cargo feature: `embed-gemini = ["dep:reqwest"]`. Reqwest is already an optional dep behind
`embed-bge`, so this adds no new dependency.

`is_compiled` and `is_ready` are deliberately distinct — the first is a *message* pre-check
("rebuild with `--features X`"), the second means an adapter actually exists. The registry design
note in `mod.rs` says not to simplify this away. Respect it.

**Tests that must change:** the FromStr error-string assertions and the `is_ready` test covering
"not implemented yet". Issue #3 lists the same test as part of its own landing pad — do not delete
it, extend it.

**Depends on:** nothing.

---

## A2 — Config keys

`EmbedderConfig` is `#[serde(deny_unknown_fields)]`, so Gemini's keys must be declared, not merely
read. Mirror the `llama_url` / `llama_model` pattern:

* project, location, model id
* credential source — decide between ADC and an explicit key path, and write down which

Extend `overlay_env` with matching `LAMBO_*` vars. Non-empty env wins over file; empty env leaves
the base intact. That is the existing contract for every other key, and diverging from it here
would be a surprise.

**Depends on:** A1.

---

## A3 — The adapter

`src/embed/gemini.rs`. Vertex `gemini-embedding-001`, `outputDimensionality` from `cfg.dim`.

Contracts, all inherited and all already tested for the other backends:

* **CON-7** — reject empty/whitespace input with `Unavailable`. No backend may embed the empty
  string; a blank reaching an embedder is a caller bug and every backend fails it identically.
* **CON-2** — no retry that silently alters the request. `bge_m3.rs` spells out why: a retry that
  drops or changes the model embeds in a different space, which passes the only runtime check
  (dim) and is therefore undetectable. Fail hard; the operator fixes the config.
* **Error classification** — connect-level failure is `Unavailable`, so the caller can degrade to
  canonical matching permanently and log once instead of hammering. Server rejection, unparseable
  body, or wrong width is `Backend`.
* **L2-normalize before returning**, rejecting non-finite and zero-norm vectors. Copy
  `l2_normalize_in_place`; NaN/Inf from a backend poisons every downstream distance.

Flip `is_ready()` to `true` here, not earlier.

**Depends on:** A2.

---

## A4 — Dim guard

`gemini-embedding-001` truncates to **768, 1536 or 3072** only. Reject any other configured dim at
construction, with a message naming the three, rather than sending an unsupported
`outputDimensionality` and discovering it as a `Backend` error at first embed.

**Depends on:** A3.

---

## The dim decision, recorded

`[embedder] dim` is already a TOML key (`src/embed/mod.rs:147`) and
`resolve::check_vector_compatibility` (`src/resolve.rs:39`) already refuses an embedder whose
width disagrees with the store's, with the message *"store schema is the authority; change the
embedder or the store, not a global constant."* So "the dim must match the embedder" needs no
work — it is enforced at process resolution today.

**Configurable is late-binding, not reversible.** Once the bootstrap embeds at N, changing the
TOML means a full re-embed: the store schema is the authority and the check refuses to start
against a store of a different width. Issue #3 states the same rule for a different reason —
*"session identity is the `EmbeddingContract`"*, so switching embedder is a re-embed, never a
config flip.

The choice is made once, when the embedder is chosen, before the ingest.

**Recommended default: 1536** (decided 2026-08-19). Not for the raw number — dims are not
comparable across models, and gemini-embedding-001 at 768 already outperforms BGE-M3's
1024 — but for the regret asymmetry under MRL: information is front-loaded, so choosing
too *wide* is later fixable by a local truncate-and-renormalize migration with no API
calls, while choosing too *narrow* means re-embedding the whole corpus through Vertex at
full cost. 1536 also clears pgvector's 2000-dim hnsw ceiling (B2), where 3072 does not
(it needs `halfvec` or goes unindexed — B2 refuses it loudly at init). One
incompatibility travels with the choice: the shipped Cockroach schema is a fixed
`VECTOR(1024)`, so a 1536-d session cannot land there without its own schema change —
irrelevant to Mooshik (its cloud tier is Postgres) but stated so nobody discovers it
sideways.

---

## The model field must be stamped for real (gap found 2026-08-19)

The two-machine story does not need a shared embedder *instance* — the space is defined
by the function (weights + dim + normalization), so the same model artifact on both
machines produces interchangeable vectors. But "same model" must mean **same artifact
including quantization**: `bge-m3-q8_0.gguf` and a Q4 conversion are different functions.
Today that rule is convention only, because **the bge_m3 adapter stamps `model = NULL`
into the contract** (visible in `evidence/mooshik-f-sqlite-bge/`: the durable row is
`bge_m3||1024`) — the store can refuse a kind or dim mismatch across machines but cannot
tell two quantizations apart.

So: **A3 stamps a real model string** (`gemini-embedding-001`) rather than inheriting the
NULL pattern, and whoever next touches `bge_m3.rs` should make it stamp the served model
identity (name, or better the artifact hash — the rig's GGUF is
`sha256 aa473d51…a173`) so the same-artifact rule becomes machine-checked. Gemini
sidesteps the whole class — one hosted function, nothing to keep in agreement — which is
A's honest advantage now that local BGE-M3 on both machines is proven viable (this
MacBook runs it fine under Metal, contra this doc's "the MacBook does not" premise; the
hosted embedder is the *convenient* answer, no longer the only correct one).

## Done when

- [ ] `kind = "gemini"` builds an adapter and embeds against Vertex
- [ ] Empty input, non-finite output and zero-norm output are each refused
- [ ] A dim outside {768, 1536, 3072} fails at construction, naming the three
- [ ] Connect failure and server rejection produce `Unavailable` and `Backend` respectively
- [ ] `embed-gemini` matrix row in CI: compile plus unit, no network. Live Vertex calls stay
      `#[ignore]`d — there is no API key in CI

## A′ — Exploratory: candle in-crate BGE-M3, bypassing llama.cpp

**Status: EXPLORATORY SPIKE, unscheduled** (added 2026-08-21, operator idea from the J3
pause). A measurement task, not a commitment; "shelve with numbers" is a first-class
outcome, per G3's precedent. Runs post-J on idle capacity. Only local BGE-M3 and Metal
are in scope — every other planned embedder is hosted (A proper, Bedrock).

**The idea:** an `embed-candle` feature running BGE-M3 in-process via
candle-core/candle-transformers, weights fetched once by hf-hub, deleting llama.cpp from
the prerequisites. Level B was built for exactly this shape: adapter + feature +
registry arm.

**Why it earns a spike** (all met in production already):

1. It deletes two failure classes we have real scars from: *embedder unreachable* (the
   92/100 unembedded-concepts damage) and *server-side input refusals* (J3-R3-1's HTTP
   500 at 1536 B — in-process, truncation policy becomes ours, explicit and tested).
2. It completes the single-binary story: `cargo install lambo`, first-run weight fetch,
   then offline — the ten-minute quickstart the adoption path needs.
3. It competes with A for Mooshik's two-machine case: pinned weights on both machines =
   one embedding space, $0, offline — and a candle adapter would stamp real model
   identity where `bge_m3.rs` stamps NULL today.

**Support, verified 2026-08-21 (web research):** candle-transformers has a native
`xlm_roberta` module (`XLMRobertaModel` base encoder — BGE-M3 is XLM-RoBERTa-large +
RetroMAE, 8192-token capacity, exact match) plus MaskedLM/SequenceClassification. There
is **no packaged bge-m3 example in candle's tree** — the spike writes its own ~100–200
line loading/CLS-pooling/normalize path. The Metal backend is real
(candle-metal-kernels; red-candle ships Metal-accelerated embeddings downstream) but no
published BGE-M3-on-Metal throughput exists. Fallback if hand-rolled pooling fights
back: fastembed-rs has first-class BGE-M3 (dense+sparse+ColBERT in one pass — future
hybrid-recall value) but rides ONNX Runtime, a C++ dependency with no advertised macOS
GPU path — the dependency class this idea exists to delete, so fallback only.

**The three measurements, each with its falsifier:**

1. **Correctness — cosine agreement.** Embed the dogfood corpus's embedded concepts (and
   a synthetic multilingual/size spread) through the spike and through the rig's
   llama-server; compare pairwise. Falsifier: median self-agreement below ~0.99 means the
   pooling is wrong (fix or fall back), not that the idea is dead — but agreement is also
   *not* a licence to mix spaces: **runtime switch = re-embed event** regardless (the dim
   doc's law), which pairs this spike with the already-required `re-embed` backfill verb.
2. **Throughput — CPU and Metal on this rig**, measured by the J3 probe methodology
   (representative input sizes, warm-up discarded, serial and concurrent legs).
   Baseline: llama.cpp q8_0 measures 110–141 items/s here. Falsifier: if candle Metal
   AND candle CPU both land under ~half of llama.cpp's figure, shelve — the operational
   wins don't justify a 2× recall-latency regression on the default path.
3. **Cost — compile time and binary size** with `embed-candle` on vs off, stated as
   numbers. No falsifier (feature-gated, so it prices an option rather than a default),
   but the quickstart claim dies if first-run weight fetch plus model load exceeds a
   stated bound — measure cold-start too.

**Deliberately out of scope:** sparse/ColBERT legs (dense parity first), GGUF/quantized
loading in candle (fp16 safetensors first; quantize later if RAM says so), replacing the
default feature set (that is a decision for after the numbers, made against the
`EmbeddingContract` migration story).

**Done when (as an exploration):** the three numbers exist with their method stated; the
falsifiers are applied honestly; the recommendation (adopt as `embed-candle` / fall back
to fastembed / shelve) is recorded here with the evidence; and if adopt-ish, the
follow-on task list is written (adapter, feature, CI row, contract stamping, re-embed
pairing, weight-fetch UX) — not started.
