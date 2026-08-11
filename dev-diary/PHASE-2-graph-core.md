# P2 — Graph core (write path)

```yaml
id:       P2
branch:   phase/p2-graph-core
requires: [P1]
blocks:   P8
parallel: high   # T2.1 first; then T2.2 ‖ T2.6 ‖ T2.7; T2.3/T2.4/T2.5 after T2.2
runs-parallel-with: P3, P4, P5, P6, P7
```

**Goal:** the in-RAM bipartite graph and the three write APIs — `derive()`,
`record_action()`, `demote()` — per spec §5 and §7. This is the RAM tier; nothing here
touches the network.

**Concurrency rule (spec §6.4), non-negotiable:** graph is `Arc<RwLock<Graph>>`
(`parking_lot`), and **the lock is never held across an `.await`**.

---

### T2.1 — Graph structure & invariants
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/graph/mod.rs, src/graph/graph.rs
status:     done (main, 2026-08-11)
```
Node/edge storage (`HashMap<NodeId, Node>`, adjacency in/out by edge type), the temporal
chain, mutation log emission (every mutation appends to the ordered log the flush task
drains — T3.4's input), `MutationEpoch` counter, and the spec §5.7 invariants enforced at
write time:

- every non-first interaction has exactly one `Temporal` predecessor
- every concept has ≥ 1 `Derives` edge
- no duplicate `(source, target, edge_type)`
- weights ≥ 0 and finite; NaN/Inf clamped to 0.0

Weight dynamics per v0.6.0 §5.4 as summarized in spec §5: reinforcement bumps on duplicate
edge writes; **recall does not reinforce**.

**Done when:** fixture graphs load and an `assert_invariants()` debug check passes on every
fixture and after every mutation in tests.

---

### T2.2 — Canonicalization pipeline
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/graph/canonical.rs
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p2)
```
Spec §7.1 steps 1–5 (step 6, hybrid/vector, is T7.2's — leave a seam: the pipeline returns
`Unmatched(canonical_key)` and the caller decides):

1. normalize — lowercase, split hyphens/underscores/camelCase, strip stopwords
2. stem — Porter via `rust-stemmers`
3. token-sort → canonical key
4. synonym resolution — **direct lookup only**, no transitivity
5. match against existing `canonical_key`

`declare_synonym()` lives here too.

**Done when:** every row of `fixtures/canonicalization-cases.json` passes.

---

### T2.3 — `derive()`
```yaml
requires:   T2.1, T2.2
fixture-ok: yes
owns:       src/graph/derive.rs
status:     not-started
```
Spec §7 exactly: per concept — canonicalize → within-call dedup → match-or-create →
`Derives` edge from current interaction → pairwise `CoOccurrence` capped at
`max_cooccurrence_per_derive=10` → `Hierarchical` from `parent_of` → mutation batch →
daemon notify (a channel send; daemon side is T4.x).

**Done when:** deriving the same concepts twice creates no duplicates, reinforces
`CoOccurrence`, and emits well-ordered mutations.

---

### T2.4 — `record_action()` + cycle check
```yaml
requires:   T2.1, T2.2
fixture-ok: yes
owns:       src/graph/action.rs
status:     not-started
```
`Resource` concept for the action; `Causal` edges to `produces`/`modifies`, `Dependency` to
`depends_on`; implicit node creation through the full canonicalization pipeline; **BFS cycle
check over `Causal`/`Dependency` after canonical resolution** — reject the write, not the
process.

**Done when:** a crafted A→B→A dependency is rejected with a typed error and the graph is
unchanged after rejection.

---

### T2.5 — `demote()`
```yaml
requires:   T2.1
fixture-ok: yes
owns:       src/graph/demote.rs
status:     not-started
```
Context-overflow chunks → `Observation` nodes; UAX #29 sentence segmentation
(`unicode-segmentation`); `chunk_group_id` recorded for sibling co-retrieval (T5.2 reads
it). No custom split fn (cut).

**Done when:** a multi-sentence chunk yields one Observation per sentence sharing a
`chunk_group_id`.

---

### T2.6 — Inverted index + BM25
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/graph/index.rs
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p2)
```
In-memory inverted index over concept content, per-session `df`, BM25 scoring — recall
phase 1's keyword source (spec §8). Incremental: updated on node create/update/remove, not
rebuilt. Reuses T2.2's normalizer for tokenization (import, don't fork).

**Done when:** the phase-1 keyword expectations in `fixtures/recall-goldens.json` pass
against fixture graphs.

---

### T2.7 — Reservations
```yaml
requires:   T2.1
fixture-ok: yes
owns:       src/graph/reserve.rs
status:     done (2026-08-11, reviewed ACCEPT x2, integrated into phase/p2)
```
Spec §11 soft locks: advisory, expiring, same-agent re-reservation extends, cross-agent
returns `AlreadyReserved`. Surfaced in recall output (T5.3 reads active reservations).
**Cut-order note:** this is 4th in the cut order — keep it isolated so cutting is one
module delete.

**Done when:** extend/deny/expire paths are unit tested with mocked time.

---

## Exit criteria

- [ ] All fixture graphs constructible via public write APIs alone (a test rebuilds
      `session-rest-api` from scratch)
- [ ] Invariants hold after every test
- [ ] Mutation log ordering verified (nodes before edges referencing them)
- [ ] No `.await` inside any lock scope (grep + review)

---

## Handoff Log

### Contract change (2026-08-11, by main) — `Concept.chunk_group_id` added

Added `pub chunk_group_id: Option<String>` to `Concept` (types/mod.rs),
`#[serde(default, skip_serializing_if = "Option::is_none")]` so committed fixture
JSON loads unchanged. Rationale: spec §7/§8 and the P5 doc ("T2.5's field")
require demoted Observations to share a group id for sibling co-retrieval, but
T1.1's frozen `Concept` had no slot. Per the freeze rule: fixtures (no change —
serde default), MemoryStore (no change — wholesale Concept clone), and the four
Rust `Concept` literals (types/graph/index/memory tests) updated in the same
commit. **Dependent tasks: T2.5 (writes it), T5.2 (reads it).**


### T2.1 — Graph structure & invariants (done 2026-08-11, by main)

**What exists now:** `src/graph/mod.rs` + `src/graph/graph.rs` (`Graph`, ~1.5k LOC
with tests), re-exported as `lambo::Graph`. In-RAM bipartite graph with:

- `HashMap<NodeId, Node>` nodes; `HashMap<NodeId, Edge>` edges (id PK) + natural-key
  index `(source, target, edge_type) -> id` (schema UNIQUE); per-node out/in
  adjacency grouped by `EdgeType` for recall BFS / structural queries.
- Temporal chain (`Vec<NodeId>`) built by construction in `insert_interaction`;
  `MutationEpoch` (bumps per appended mutation); ordered write-behind
  `mutation_log` drained by `drain_log()` (T3.4's input).
- Write APIs: `insert_interaction` (auto `Temporal` edge), `insert_concept` (auto
  `Derives` edge from origin interaction — §5.7 enforced at write time),
  `upsert_edge` (natural-key dedup + reinforcement), `remove_node` (emits incident
  `DeleteEdge`s before `DeleteNode`), `remove_edge`,
  `apply_canonization_transition`, `declare_synonym`, reservations, root_goal,
  embedding contract.
- `from_snapshot` (validates every invariant, seeds without log entries) +
  `snapshot()` (deterministic order: interactions in chain order, concepts/edges by
  id, synonyms by source_key — **round-trip exact for both fixtures**).
- `assert_invariants()` collects ALL §5.7 violations into one error (session
  consistency, endpoints, natural-key uniqueness, finite weights, chain,
  Derives coverage, Causal/Dependency acyclicity).

**Decisions the next agent must not re-derive:**
- **Reinforcement constants are ours** (v0.6.0 §5.4 constants not in-repo):
  `REINFORCE_BUMP = 1.0`, `MAX_EDGE_WEIGHT = 10.0`. Duplicate natural-key write:
  weight bumps (capped), `reinforcements += 1`, `last_reinforced` = write time, id +
  `created_at` preserved. Recall never reinforces (read path never calls edge
  writes).
- **Temporal edge direction: source = newer, target = previous** (points back in
  time, matching `scripts/gen-fixtures.py`). The predecessor invariant is therefore
  an **out**-edge check, not an in-edge check.
- Structural edge defaults mirror fixtures: `Temporal` w=1.0, `Derives` w=0.9.
- NaN/±Inf edge weights clamp to 0.0; negatives rejected (`Invariant`).
- `Causal`/`Dependency` cycle **rejection** is T2.4's BFS; `upsert_edge` stores what
  it's given and `assert_invariants` detects cycles.
- Synonyms + reservations are RAM-local (no `Mutation` kind exists); they round-trip
  through `GraphSnapshot` only. Reservations storage is a `Vec` preserving order.
- `Graph` owns no lock — `Arc<RwLock<Graph>>` + "never hold across `.await`" is the
  T2.3+ `Memory` owner's job (spec §6.4).
- Chain construction rejects forks/cycles/missing covers; re-upserting an
  interaction must keep its chain position.

**Verification:** 22 new tests, all green; full suite 113 passed / 1 ignored
(live-calibration needs llama-server). Both fixture snapshots load with
`assert_invariants` passing and `snapshot()` round-tripping exactly.

### T2.1 adve-review remediation (2026-08-11, by main — commit 7f3b6a3)

Adversarial review (`dev-diary/adversarial-review/adve-review-t2.1-graph-structure.md`)
CLOSED as ACCEPT. All 11 findings remediated; full dispositions in the review file.
Additional decisions downstream agents must know:

- **`assert_invariants` treats `Hierarchical` as a DAG constraint** (M1) — the
  safety net is broader than spec §5.7's write-time contract (Causal/Dependency,
  enforced by T2.4's BFS). A Hierarchical cycle is now a reported violation.
- **`drain_log` batches are chronological, never re-sorted** (M2) — §2.4's phase
  grouping holds within a single logical write only. A node upsert may legally
  follow a `DeleteNode` in one batch (create→delete→create). T3.4 must replay
  in order.
- **`remove_node` rejects interactions** (S2) — interactions are append-only in
  v0.1 (spec §9 compaction is cut). GC (T4.5) may only remove concepts.
- **Neighbor accessors are deterministic** (S5 fallout): `out_neighbors`,
  `in_neighbors`, `*_typed`, `incident_edges` return id-ascending order. The S5
  round-trip test caught a real HashSet-iteration-order flake.
- **`reinforcements` starts at 1** (I2): creation counts as the first write.
  Store adapters (T3.2) must match this convention.
- **CanonizationStatus naming** (I1): spec §9 stage numbers map to
  None→Candidate→Venerable→Canonical (frozen by T1.1). P6 must map, not rename.

Gate at close: `cargo fmt --check`; clippy `-D warnings` (default + no-default);
119 lib tests × 3 consecutive runs.

### T2.2 — Canonicalization pipeline (done 2026-08-11, by T22Canonical)

**What exists now:** `src/graph/canonical.rs` (+ one additive `pub mod canonical;`
line in `src/graph/mod.rs`). Spec §7.1 steps 1–5, exactly the pinned exports:

- `normalize_tokens(&str) -> Vec<String>` — camelCase-boundary split (on the
  ORIGINAL case) → lowercase → split `[-_ ]` + whitespace → drop stopwords
  (13-word set pinned to `gen-fixtures.py` `STOPWORDS`) → Porter stem via
  `rust-stemmers` (`Algorithm::English`, lazy `LazyLock` static). NO sort/synonym/
  join. Pure — no `Graph` dep, so T2.6 imports it for the recall index.
- `canonical_key(&str, impl Fn(&str) -> Option<&str>) -> String` — RAW-input
  synonym lookup first (trimmed, as-is) → normalize → sort → join `" "`.
- `canonicalize(&str, &Graph) -> Result<CanonicalizeResult, LamboError>` with
  `Matched { key, node }` / `Unmatched { key }`. Step 6 (hybrid/vector, `Semantic`
  edges) deliberately left to the caller (T2.3/T7.2 seam).

**Decisions the next agent must not re-derive:**
- **camelCase split runs BEFORE lowercasing.** `gen-fixtures.py`'s `split_tokens`
  lowercases first, which yields `"userschema"` for `UserSchema` and `"cached"`
  for `cached` — both contradicting the checked-in `canonicalization-cases.json`
  (frozen truth). The script has a latent ordering bug; do NOT "fix" the fixture
  to match the script. All 11 fixture rows pass with case-boundary-first splitting
  + real Porter.
- **`canonicalize` does its own raw lookup** via `Graph::synonym` instead of
  calling `canonical_key` with a graph-closure: the pinned `impl Fn(&str) ->
  Option<&str>` callback is higher-ranked (`for<'a> Fn(&'a str) -> Option<&'a
  str>`), which a closure borrowing the graph cannot satisfy (return borrows from
  the graph, not the input). Same trimmed-raw-then-normalize semantics, shared
  `tokens_to_key` helper. Callers of `canonical_key` with a static table should
  pass a **fn item**, not a closure returning `Option<&str>` — closures don't
  generalize to the HRTB bound (compile error otherwise).
- **No new synonym storage** — T2.1's `Graph::synonym`/`declare_synonym` are
  consumed as-is; the phase doc's "`declare_synonym()` lives here too" is stale
  (it already lives on `Graph`).
- Match step scans `graph.concepts()` for `canonical_key` (Graph has no
  canonical-key index); schema `UNIQUE (session_id, canonical_key)` makes at most
  one match in a well-formed graph. `canonicalize`'s `Result` wrapper is part of
  the pinned contract; the step itself cannot fail.
- Stopword-only or empty input → empty token vec → key `""`; no special-casing.

**Verification:** 12 new tests, all green under `cargo test graph::` (default
features): 11 tokenizer/key unit tests + the fixture acceptance test iterating all
11 rows of `fixtures/canonicalization-cases.json` (gated
`#[cfg(feature = "fixtures")]`, synonym table mirroring the snapshot:
`register_user` → `create_user`). STEM-table unit test (80 words from
`gen-fixtures.py`) confirms rust-stemmers matches the probe-verified fixture
table verbatim. `graph::` scope: 40 passed / 0 failed. Only touched
`src/graph/canonical.rs` + `src/graph/mod.rs`; `src/graph/graph.rs` untouched.

### T2.6 — Inverted index + BM25 (done 2026-08-11, by T26Index)
**What exists now:** `src/graph/index.rs` (`InvertedIndex`, 386 LOC incl. tests),
registered via `pub mod index;` in `src/graph/mod.rs` (only line touched there).
Pinned API as spec'd: `new`, `add(&Concept)` (idempotent per node id — re-add
atomically replaces that concept's postings; a concept never appears twice),
`remove(NodeId)`, `from_snapshot(&GraphSnapshot)` (bulk = repeated `add`, never a
separate rebuild path), `search(query, limit) -> Vec<Scored<NodeId>>`. BM25
`k1 = 1.2`, `b = 0.75` (consts `BM25_K1`/`BM25_B` in-code, spec-pinned). Owns no
lock, synchronous, pure; no error paths (API returns `()`/`Vec`, so no
`LamboError`/`StoreError` needed).

**Tokenization — SEAM USED and RESOLVED AT INTEGRATION (2026-08-11).** The local
private `fn tokenize` built against the pinned T2.2 contract (lowercase -> split
`[-_ ]` + camelCase incl. acronym-run split -> drop stopwords -> Porter stem,
duplicates retained for tf) has been **removed**: `src/graph/index.rs` now
imports `crate::graph::canonical::normalize_tokens` (T2.2), the one tokenizer —
import, don't fork. **One reconciliation:** the canonical tokenizer does NOT
split acronym runs (`APIKey` -> `["apikey"]`), following the fixture convention
(lower->Upper camelCase boundaries only, matching `gen-fixtures.py`'s regex);
the seam copy split them (`api`,`key`). Canonical behavior wins; the contract
test `tokenize_matches_canonical_contract` now pins `APIKey` -> `["apikey"]`
with a comment explaining why. Goldens unaffected (no acronyms in fixtures).
Did NOT touch `canonical.rs` or `graph.rs`.

**Decisions the next agent must not re-derive:**
- **Search semantics:** OR over query terms (a concept matching any term is a
  candidate; per-term BM25 contributions sum). `idf = ln(1 + (N - df + 0.5)/(df +
  0.5))` (BM25+ smoothing) so every matching term contributes a strictly positive
  score — no zero-score padding, matching the goldens' "positive score only" note.
  `df` is per-session by construction: one index per session; `df` = number of
  indexed concepts containing the term. Tie-break by `NodeId` (inner `Uuid`) —
  `NodeId` itself derives no `Ord`; sort uses `.0`.
- **Length bookkeeping:** every `add` increments `total_docs`/`total_tokens`; a
  zero-token concept (stopword-only/empty content) is a document but can never
  match (no postings) — avgdl stays well-defined.
- **Interactions are not indexed** (concepts only, per spec §8 / task contract);
  `from_snapshot` iterates `snap.concepts` only.
- `remove` is a no-op for unknown ids (matches `HashMap::remove` semantics).

**Verification:** 9 new unit tests (`cargo test graph::` — 37 passed, 0 failed,
92 filtered out; default features so `fixtures` gate is active). Both golden cases
pass with EXACT id sets and order via `from_snapshot(load_snapshot("session-rest-api"))`
+ `search(query, top_k)`: "pagination" -> `[1008]`, "create" -> `[1002]` (asserted
as id vectors, not floats, per goldens note). Fixture tests gated
`#[cfg(feature = "fixtures")]`. No fmt/clippy/project-wide suites run (per
constraints).

### T2.7 — Reservations policy (done 2026-08-11, by worker)

**What exists now:** `src/graph/reserve.rs` (new, ~360 LOC incl. tests) +
one additive `pub mod reserve;` in `src/graph/mod.rs`. No other files touched —
in particular **`src/graph/graph.rs` is untouched** (T2.1's RAM-local
`set_reservation`/`clear_reservation`/`reservation`/`reservations` storage is
reused as-is; no new storage, no `Mutation` kind).

Policy functions (all take `now: DateTime<Utc>` explicitly — never `Utc::now()`,
time is mocked in tests):

- `reserve(graph, node, agent, ttl, now) -> Result<Reservation, LamboError>`:
  missing node -> `StoreError::NotFound`; no lock -> create
  (`expires_at = now + ttl`); same agent -> extend (expiry replaced, node +
  agent unchanged); cross-agent live -> `LamboError::Conflict` naming holder +
  expiry (`"node {n} already reserved by {holder} until {expiry}"`); cross-agent
  expired (`now >= expires_at`) -> takeover.
- `release(graph, node, agent) -> Result<(), LamboError>`: owner clears;
  non-owner -> `Conflict` (lock untouched); no reservation -> `NotFound`.
- `active_reservation(graph, node, now) -> Option<&Reservation>`: `None` when
  expired. **Expiry is half-open: active iff `now < expires_at`** (at
  `now == expires_at` the lock is dead).

**Decisions the next agent must not re-derive:**

- **Expiry boundary is half-open** — `now < expires_at` is live, `now >=
  expires_at` is expired (chosen so a `ttl` fully elapsed at the instant of
  expiry; matches `active_reservation` and the takeover trigger).
- **`release` ignores expiry** — owner/non-owner/no-lock are decided on agent
  identity alone, per the pinned contract; an expired lock is still
  owner-releasable (harmless cleanup) and non-owner-release still conflicts.
- **TTL conversion is a typed error**: `std::time::Duration` ->
  `chrono::Duration` via `chrono::Duration::from_std`; out-of-range (e.g.
  `u64::MAX` seconds) yields `pub struct ReserveError` (thiserror), surfaced as
  `LamboError::Other` and downcastable via `anyhow::Error::downcast_ref`.
  Not silently clamped, not a bare string. The expiry computation is
  **checked** (`DateTime::checked_add_signed`), so a TTL that passes
  `from_std` (fits `TimeDelta`, ±~292k years) but would overflow
  `DateTime<Utc>` (±~262k years) also returns `ReserveError` — the policy
  never panics on a caller-supplied TTL (round-1 review finding P2).
- **Borrow discipline**: the policy decision snapshots `(agent_id, expires_at)`
  by value before calling `set_reservation`, so no `&Graph` borrow is live
  across the mutation.
- **`set_reservation` replaces by `node_id`** — create/extend/takeover all
  funnel through it; the deny path never mutates, so the existing lock is
  untouched by construction.

**Verification:** 16 new unit tests (mocked time via
`Utc.timestamp_opt(1_752_000_000, 0)` + minute offsets): fresh reserve expiry,
same-agent extend (single reservation, agent unchanged, expiry advances),
same-agent extend on a **still-live** lock (expiry advances from new `now`),
cross-agent deny while live (typed `Conflict`, message names holder + expiry,
lock untouched), cross-agent takeover after expiry, takeover at exactly
`now == expires_at` (boundary: half-open, treated as expired), owner release
clears, non-owner release errors, absent-reservation release errors, release
of an **expired** lock is identity-only (owner cleans up, non-owner still
conflicts), release-then-re-reserve lifecycle (freed slot usable by another
agent immediately), expired invisible to `active_reservation` (incl. the
`now == expires_at` boundary), `active_reservation` on a missing node is
`None`, missing-node error, out-of-range TTL typed error, and a TTL that
passes `from_std` but would overflow `DateTime<Utc>` (`8.21e12` s, ~260k
years) returns the typed error instead of panicking (round-1 review finding
P2). `cargo test graph::` (default features): 44 passed / 0 failed (28
pre-existing + 16 new), 0 warnings. No fixtures read, so no
`#[cfg(feature = "fixtures")]` gating needed.
