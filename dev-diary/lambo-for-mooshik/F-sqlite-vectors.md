# F — SQLite vector search (issue #5)

**Goal:** semantic recall on the local store.

**Why this is not optional.** Spec §3.3 makes `SqliteStore` the local single-machine backend, and
§3.1 exposes `lambo_recall(query, strategy?: "Canonical" | "Hybrid")` to the companion. But
`SqliteStore` reports `Capabilities::empty()` (`src/store/sqlite.rs:564`), so hybrid matching
degrades to keyword-only:

```
hybrid matching disabled: store lacks VECTOR_SEARCH — degrading to
MatchStrategy::Canonical (creating keyword-only concept)
```

Cockroach is the only adapter advertising `VECTOR_SEARCH`. So today, Mooshik's default local mode
— the `$0 / 0ms`, offline, always-on posture the whole §1.3 positioning table rests on — has **no
semantic recall at all**, and "memory recall" is a property of the cloud tier. That inverts the
product.

---

## Why it is smaller than it looks

* The embeddings are **already on disk**: `concepts.embedding BLOB`
  (`migrations/sqlite/001_init.sql:82`), written on every flush for round-trip parity (CON-8). The
  schema comment states the gap outright — *"no VECTOR_SEARCH … the column is never queried."*
* The session's embedding contract is already persisted in `sessions.embedding_{kind,model,dim}`.
* **No migration is required.** Only the query path is absent.
* The reference implementation is in-tree: `VectorSearchStore` (`src/memory.rs:3204`) is
  MemoryStore plus exact cosine over flushed embeddings, built so a default `cargo test` can
  assert derive → flush → vector recall with no live cluster. It is the shape SQLite needs.
* The math exists: `crate::embed::cosine` (`src/embed/math.rs:5`).

---

## F1 — The query path

Implement `vector_candidates` and `vector_candidates_checked` (`src/store/mod.rs:165`, `:182`):
load the session's non-null embeddings, score with `cosine`, sort, return top-k as
`Scored<NodeId>`.

**`vector_candidates_checked` is the one that matters.** The trait documents `vector_candidates`
as unable to bind the query's embedding contract to the durable read, and says new production code
must call the checked variant. It must compare the expected contract to the session's durable
contract *in the same transactional snapshot as every candidate query* and refuse a mismatch —
the way the Cockroach path does.

### The write side has to hold the width too (F-R1-1, completed under F-R2-1)

Read-side detection is terminal and session-wide — `select_session_vectors` returns on the first
bad row, so **one** width-mismatched concept fails the whole session's vector leg on every read,
permanently. `concepts.embedding` is a width-agnostic `BLOB`, so unlike Cockroach's `VECTOR(n)` the
adapter must hold the width by hand, and it takes **two** statements, not one:

* `enforce_concept_vector_widths` (F-R1-1) refuses a concept whose vector disagrees with the
  contract durable at its flush step, before any encoding, so a refusal leaves the transaction empty.
* `set_embedding` (F-R2-1) NULLs every vector of a **different width** when it stamps a contract.
  The gate alone was not enough: round 2 reproduced a **restamp** — `SetEmbedding{4}`,
  `Concept{4-wide}`, `SetEmbedding{3}`, `Concept{3-wide}` — through one public `flush` with no
  direct SQL, where every concept matched the contract of its own step and the batch still committed
  a 4-wide vector under a `dim = 3` contract. That is the same terminal state, reached one ordering
  over. With the wider predicate the batch is accepted and **self-heals**: the orphan is erased.

Together: *no vector whose width disagrees with the session contract survives a write through this
adapter.* The per-read check remains the defence against an externally edited database, which no
write-side rule can cover.

The quarantine keys on **width**, deliberately — a same-width `kind`/`model` relabel keeps its
vectors, because `Graph::replace_embedding_with_operator_override` (the `--allow-embedding-mismatch`
attach path) requires equal widths and explicitly permits a same-kind model rename with the vectors
intact. Cockroach keeps the narrower NULL-contract-only quarantine, and that divergence is sound
rather than an oversight: its `VECTOR(1024)` DDL means every stored vector is exactly that wide, so
a restamp cannot produce a row that decodes to an unexpected width — it fails loud at the
DDL-authority probe check instead. Recorded on `set_embedding` in `src/store/sqlite.rs`.

### Exact scan, not an index

Lambo is session-scoped, so n stays small by construction. At 1024 f32 a concept vector is 4 KB:
the `cloudops-exhibit` session holds 41 concepts, and the K=12 concurrency run produced roughly
1,400 (about 5.7 MB). A full scan at that size is sub-millisecond and returns *exact* nearest
neighbours, where C-SPANN measured 0.99 recall@50 at beam 64.

`sqlite-vec` gives a real ANN index and can be statically linked via `sqlite3_auto_extension`, so
the objection is not a stray `.so` — it is a C toolchain dependency across four cross-compiled
release targets plus auto-extension registration before sqlx opens a pool. Not worth it until an
exact scan actually hurts.

**Trigger to revisit — measure `hybrid::derive`, not recall** (corrected under F-R1-3 remediation;
the original text said "recall latency", which named the *cooler* of the two paths). Recall runs
**one** scan per query. `derive` calls `vector_candidates_checked` *inside* its per-unmatched-concept
loop (`src/graph/hybrid.rs`), so a derive of `k` concepts over `n` stored vectors is **k×n** BLOB
decodes and text→`f32` parses, with no caching and no reuse between iterations. Three things
compound there and nowhere else:

* all `k` scans share **one 30s deadline** (`HYBRID_IO_TIMEOUT`, computed once per `derive` call);
* they contend for the **single pooled connection** (`max_connections(1)`), which the write-behind
  flush also needs, so a scan blocks durability;
* overrunning the deadline is **not** a degradation — it returns `Backend("hybrid vector candidate
  lookup timed out…")`, which propagates and fails the whole derive before its commit phase. This
  is a failure mode that could not occur on SQLite at all before F2.

So the scheduled measurement is: **k×n scan cost and peak RSS on the derive path**, on the
bootstrapped graph the day it first exists — plus recall latency as the secondary number.

**Named next mitigation, not done here:** hoist **one scan per `derive` call** instead of one per
unmatched concept (same pool, same session, same contract check). Deliberately deferred: the probes
come from per-concept `embed` calls interleaved with the lookups, so hoisting means splitting
`derive` into an embed-all phase and a scan-once phase, and it needs a trait method returning the
raw candidate pool rather than `Vec<Scored<NodeId>>`. That restructures `derive`'s per-concept error
handling (today each arm degrades a *single* concept on embed failure or capability miss) and the
`GraphStore` trait, which is frozen after P1. Recorded with its trigger rather than smuggled into a
doc-correction pass.

#### The assumption underneath has moved

The exact-scan argument rests on *"Lambo is session-scoped, so n stays small by construction"* —
41 concepts in the `cloudops-exhibit` session, ~1,400 in the K=12 run.

**Mooshik holds one unified session.** That is a settled product decision, not a possibility:
spec §3.3's single autobiographical memory across every machine, bootstrapped from 17,106 commits
and 8.7M words of markdown in one graph. Whatever that extracts to, it is not 1,400, and at 1536
dimensions each concept carries ~6 KB of vector.

So `n` is no longer small by construction. It is small *in the workloads Lambo has measured so
far*, which is a different claim.

This does not change the decision to start with an exact scan — it is still the right first
implementation, and the alternative costs a C toolchain across four targets for a number nobody
has measured. What it changes is **how F1 should be written**: the scan must sit behind a seam
that can be replaced without touching its callers. Concretely, keep candidate selection separable
from scoring, so swapping exact cosine for an index is one implementation change rather than a
refactor of the recall path.

The revisit trigger is now a scheduled measurement rather than a hypothetical: measure recall
latency and peak RSS on the bootstrapped graph the day it first exists, not the week someone
complains.

**Depends on:** nothing.

---

## F2 — Advertise the capability

Flip `capabilities()` to `VECTOR_SEARCH` and implement `vector_dimensions()` **in the same
change**. `check_vector_search_contract` (`src/resolve.rs:63`) refuses a store that claims the
capability without a concrete width, and a test already asserts that refusal (`resolve.rs:413`).

**The sharper trap.** `vector_candidates_checked` has a fail-closed *default* that returns
`StoreError::Capability` when the store advertises `VECTOR_SEARCH` — it exists so third-party
v0.2.0 adapters cannot silently weaken the contract. So flipping `capabilities()` before F1 is
complete does not degrade recall to keyword-only; it makes recall **error**. F1 lands first, or
the two land together. Never F2 alone.

**Depends on:** F1.

---

## Amendment to issue #5 as filed

The issue specifies `vector_dimensions() -> Some(1024)`. **Do not.** That introduces a third
hardcoded width — beside `VECTOR(1024)` in the Cockroach migration and the `1024` default in
`EmbedderConfig` — at exactly the moment B is removing the hardcoding, and Mooshik's Gemini
embedder will not be 1024 (768/1536/3072 only).

The width must come from the persisted session contract (`sessions.embedding_dim`, already
stored) or from config, matching whatever B2 settles on as the authority.

---

## Done when

- [x] SQLite advertises `VECTOR_SEARCH` and reports a width that is not hardcoded
- [x] Hybrid derive no longer logs the degradation warning on SQLite
- [x] A test proves the vector leg **fired**, rather than inferring it from rank — mirroring what
      `VectorSearchStore` does for MemoryStore
- [x] Contract mismatch is refused on the checked path
- [~] Recall parity between SQLite and Cockroach on the same seeded graph, with any divergence
      explained by ANN approximation rather than by the adapter.
      **Cluster-free half: done.** `vector_candidates_agree_with_an_exact_cosine_oracle_on_both_fixtures`
      (`src/store/sqlite.rs`) seeds both committed fixture graphs plus a stamped contract and
      synthetic unit vectors into `SqliteStore` and into an exact-cosine oracle with the shared
      ordering contract, and asserts the returned `Vec<Scored<NodeId>>` is **exactly equal** across
      4 probes × 5 limits × 2 fixtures (40 assertions) — same ids, same order, same `f64` scores.
      This supersedes the prior artifact for this box: `vector_scores_match_the_cockroach_distance_conversion`
      only *transcribed* Cockroach's `distance_to_score` into a test body and therefore proved the
      formula was copied, not that the adapters agree on candidates or ranks. It was **kept, not
      replaced** — it still runs in the `sqlite-vectors` row, and a transcription test is worth
      having as long as it is not mistaken for parity evidence (round 2's wording note: "replaces"
      read as if it had been deleted).
      **Cockroach half: explicitly awaiting the live tier.** `cockroach-live` is gated off on this
      branch (`ci.yml`, `if: github.ref != 'refs/heads/lambo-for-mooshik'`), and no live DSN was
      available for this remediation, so no run compares the two adapters' answers on one graph.
      Recording this as *not covered* rather than letting the transcription test imply it is.
      What the identity now rests on instead of an unstated assumption: `cosine` is norm-invariant
      while Cockroach's `1 − d²/2` is not, so the two agree **only for unit-norm vectors** — now a
      documented `Embedder::embed` output contract ("vectors MUST be L2-normalized"), which every
      shipped embedder already satisfies (`bge_m3` normalizes and rejects zero-norm;
      `FixtureEmbedder` emits unit vectors) and which `A-gemini-embedder.md` instructs the next one
      to. It is stated as a trait contract rather than a `CON-`numbered one because `CON-1`…`CON-9`
      are all assigned findings from `adve-review-e2e-p0-p3-fable.md`; that series has no free slot,
      and minting `CON-10` would imply a registry entry that does not exist.
- [~] `README.md` and the site's End-to-end page describe SQLite as the out-of-the-box store;
      revisit what each tier earns once semantic matching works there.
      **Site/docs End-to-end: done** (and, under F-R1-5, corrected — the walkthrough exercises the
      vector leg *mechanically*; its fixture embedder is deterministic, not semantic, so the
      semantic claim now points at `evidence/mooshik-f-sqlite-bge/` instead of implying the
      walkthrough demonstrates it).
      **README tier-positioning: DEFERRED, deliberately.** Not an oversight and not out of scope by
      accident: rewriting the README's tier narrative is an editorial call the orchestrator is
      holding during the judging window, so a remediation agent must not pre-empt it. `README.md`
      contains nothing *false* — its only vector lines are Cockroach-tier/RDS text and remain
      accurate — so the deferral costs correctness nothing; what remains undone is the positioning
      question ("what does each tier earn now that the local store matches on meaning?") and the
      config example still leading with `kind = "memory"`. Reopen with the tier narrative, not
      separately.
