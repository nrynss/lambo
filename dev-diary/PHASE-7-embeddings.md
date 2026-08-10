# P7 — Embeddings & hybrid matching

```yaml
id:       P7
requires: [P1, T0.3, T0.4]
blocks:   nothing hard — capability-gated; keyword-only is a lawful degraded mode (spec §3.2)
parallel: high   # T7.1 ‖ T7.2 ‖ T7.3
runs-parallel-with: P2, P3, P4, P5, P6
```

**Goal:** Bedrock Titan v2 behind the `Embedder` trait, hybrid concept matching (spec §7.1
step 6), and live `vector_candidates` — the hackathon's **Distributed Vector Indexing**
requirement (spec §12.1) doing real work: merging concepts normalization can't
("register user" / "create account"), keeping the graph from fragmenting into islands.

**Degradation contract:** if this whole phase slips, `MatchStrategy::Hybrid` falls back to
`Canonical` and logs once. But §12.1 lists the vector index as one of the two required
CockroachDB tools — so P7 can land *last*, it cannot land *never*. T7.3 + one hybrid merge
in the demo is the minimum.

---

### T7.1 — `BedrockEmbedder`
```yaml
requires:   T1.3, T0.4
fixture-ok: yes   # written from the T0.4 handoff; live call behind an integration gate
owns:       src/embed/bedrock.rs
status:     not-started
```
Titan Text Embeddings V2, 1024-dim, via `aws-sdk-bedrockruntime`, using T0.4's recorded
request/response shapes. Timeout + typed error mapping; **an embed failure fails the
match step, never the write** — `derive()` completes keyword-only (circuit breaker was cut;
per-call fallback is the v0.1 shape).

**Done when:** unit tests with a mocked client pass; feature-gated live smoke returns 1024
dims.

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

- [ ] Hybrid merge demonstrated offline (fixtures) and live (Cockroach)
- [ ] Degraded mode proven equivalent to Canonical strategy
- [ ] `EXPLAIN` evidence of index use committed

---

## Handoff Log

> _Fill on completion._
