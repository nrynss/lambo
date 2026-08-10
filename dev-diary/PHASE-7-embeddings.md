# P7 — Embeddings & hybrid matching

```yaml
id:       P7
requires: [P1, T0.3, T0.4]
blocks:   nothing hard — capability-gated; keyword-only is a lawful degraded mode (spec §3.2)
parallel: high   # T7.1 ‖ T7.2 ‖ T7.3
runs-parallel-with: P2, P3, P4, P5, P6
```

**Goal:** Portable `Embedder` trait (1024-d dense) + hybrid concept matching (spec §7.1
step 6) + live `vector_candidates` — Cockroach **Distributed Vector Indexing** (spec §12.1)
doing real work: merging concepts normalization can't ("register user" / "create account").

**Default embedder:** **BGE-M3** weights from **Hugging Face**, runtime **llama.cpp**
(see [`notes/embeddings-portable.md`](notes/embeddings-portable.md)).  
**Swap-in:** Bedrock Titan V2 when account is authorized.  
**Tests:** `FixtureEmbedder` only.

**Degradation contract:** if hybrid cannot embed, fall back to `MatchStrategy::Canonical`
and log once — not "keyword as product." §12.1 still requires the vector index in use for
the demo (T7.3 + one hybrid merge minimum).

---

### T7.0 — Embedder factory + BGE-M3 / llama.cpp ★ (default path)
```yaml
requires:   T1.3
fixture-ok: yes
owns:       src/embed/mod.rs, src/embed/llama_cpp.rs (or bge_m3.rs), scripts/fetch-bge-m3.sh, scripts/run-llama-embed.sh
status:     not-started
```
- `LAMBO_EMBEDDER=bge_m3|bedrock|fixture`, `LAMBO_EMBED_DIM=1024`, `LAMBO_LLAMA_EMBED_URL`.
- Download GGUF (or convert) from HF into `models/` (gitignored); serve with llama.cpp
  `--embedding`.
- HTTP client returns L2-normalized 1024-d vectors.
- **Never mix** BGE-M3 and Titan vectors in one session without re-embed.

**Done when:** smoke against local llama.cpp returns 1024 dims; fixture path still green in CI
without models.

---

### T7.1 — `BedrockEmbedder` (optional swap-in)
```yaml
requires:   T1.3, T0.4
fixture-ok: yes   # written from the T0.4 handoff; live call behind an integration gate
owns:       src/embed/bedrock.rs
status:     not-started
```
Titan Text Embeddings V2, 1024-dim, via `aws-sdk-bedrockruntime` or Bearer API key, using
T0.4 shapes. Selected when `LAMBO_EMBEDDER=bedrock` and account is AUTHORIZED. Timeout +
typed errors; **embed failure fails the hybrid match step, never the write** — fall back to
canonical match (per-call fallback is the v0.1 shape).

**Done when:** unit tests with a mocked client pass; feature-gated live smoke returns 1024
dims when AWS allows.

---

### T7.2 — Hybrid matching (canonicalization step 6)
```yaml
requires:   T2.2, T1.3
fixture-ok: yes   # FixtureEmbedder near/far pairs (T1.3) drive all tests
owns:       src/graph/hybrid.rs
status:     not-started
```
On canonical-key miss under `MatchStrategy::Hybrid`: embed, query
`store.vector_candidates()`, accept above `semantic_match_threshold=0.85`, create a
`Semantic` edge to the matched concept (decaying, per spec §5). Below threshold or
capability absent → create new concept, keyword-only, log the fallback once per session.
Sits behind T2.2's `Unmatched` seam — do not modify `canonical.rs`.

**Done when:** with `FixtureEmbedder`, the near pair merges with a `Semantic` edge and the
far text creates a fresh concept; with a no-capability store, behavior is byte-identical to
`MatchStrategy::Canonical`.

---

### T7.3 — Live `vector_candidates` on CockroachDB ★ (hackathon requirement)
```yaml
requires:   T3.2, T0.3
fixture-ok: no
owns:       (vector paths inside src/store/cockroach.rs — same owner as T3.2; claim jointly or sequence)
status:     not-started
```
The T0.3 spike productionized: embedding column write in `flush()`, index-backed
similarity query, `Capabilities::VECTOR_SEARCH` advertised. Verify with `EXPLAIN` that the
vector index is actually used — "we used the vector index" must be true on camera.

**Done when:** integration test: two paraphrase concepts derived through the full live
stack merge via the index, and `EXPLAIN` output is captured into `dev-diary/evidence/`.

---

## Exit criteria

- [ ] BGE-M3 + llama.cpp path documented and smokeable (default)
- [ ] Bedrock path optional swap-in (same 1024-d contract)
- [ ] Hybrid merge demonstrated offline (fixtures) and live (Cockroach)
- [ ] Degraded mode proven equivalent to Canonical strategy
- [ ] `EXPLAIN` evidence of index use committed

## Handoff Log

- **2026-08-10:** Portable embeddings decision — default BGE-M3 (HF + llama.cpp), Bedrock
  Titan when authorized. Dim 1024. Details: `notes/embeddings-portable.md`.

---

## Handoff Log

> _Fill on completion._
