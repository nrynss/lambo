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
