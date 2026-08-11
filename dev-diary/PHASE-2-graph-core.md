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
status:     done (2026-08-11, reviewed ACCEPT, integrated into phase/p2)
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
status:     done (2026-08-11, reviewed ACCEPT, integrated into phase/p2)
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
status:     done (2026-08-11, reviewed ACCEPT, integrated into phase/p2)
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

### T2.3 — `derive()` (done 2026-08-11, by T23Derive)

**What exists now:** `src/graph/derive.rs` (new, ~750 LOC incl. tests) + one
additive `pub mod derive;` line in `src/graph/mod.rs`. No other files touched
(in particular `graph.rs`, `canonical.rs` untouched). Pinned API as spec'd:
`ParentOf<'a>` (`none()` / `from_pairs(&[(&str, &str)])`), `DeriveOutcome {
created: Vec<NodeId>, matched: Vec<NodeId>, reinforced: usize }`,
`derive(graph, interaction, agent, concepts, parent_of, max_cooccurrence_per_derive)
-> Result<DeriveOutcome, LamboError>`. Module constants `COOCCURRENCE_WEIGHT =
0.5`, `HIERARCHICAL_WEIGHT = 0.5` (pub, doc-commented); `PARENT_OF_CONCEPT_TYPE
= Entity`. Daemon notify seam documented in the module doc (deferred to T4 — no
stubs, no channel types).

**Decisions the next agent must not re-derive (review these):**

- **Derives "ensure" is realized by `insert_concept`, not a separate
  `upsert_edge`.** Created concepts get node + Derives (w=0.9, re=1) from
  `insert_concept`; an extra step-4 `upsert_edge` would immediately reinforce a
  fresh edge (0.9→1.9) and break fixture-compatible weights (the P2 rebuild
  test compares snapshots). Matched concepts are **re-upserted** via
  `insert_concept` — the ONLY public write path that emits the required
  `UpsertNode`, so the §2.4 drained-batch ordering contract ("every UpsertEdge's
  endpoints were UpsertNode'd earlier in the same batch") holds when derive
  writes edges to pre-existing concepts. The re-upsert is idempotent (stored
  `Concept` cloned as-is; status/gc_survived preserved) and its structural
  Derives write creates (new interaction) or reinforces (same interaction
  re-derive) the edge — this is the "re-derives reinforce per T2.1 semantics"
  path. Every node *written earlier in the same call* — created, or matched
  and re-upserted (canonical-key collision onto a pre-existing concept) — is
  tracked in `written_this_call` and NOT re-upserted, so one call can never
  self-reinforce.
- **`reinforced` counting:** `edge_between` before each write; a hit = the
  write is a duplicate natural key = +1 (Derives via insert_concept included).
  Verified: re-derive of 2 concepts from the same interaction ⇒ reinforced == 3
  (2 Derives + 1 CoOccurrence); parent_of re-derive ⇒ 3 (2 Derives ends + 1
  Hierarchical).
- **Within-call dedup:** `concepts` deduped by raw content (first occurrence
  wins, incl. its ConceptType); `parent_of` pairs deduped by the **resolved**
  `(parent_node, child_node)` pair (raw-string dedup would let two pairs whose
  ends canonicalize to the same node pair write a duplicate Hierarchical edge).
  Different contents collapsing onto one canonical key are handled by the
  matcher (every colliding content resolves Matched to the same node and is
  recorded in `outcome.matched`) — a node is never written twice in one call.
- **CoOccurrence** is pairwise over the `concepts` argument only (created +
  matched nodes, call order, direction earlier→later), capped at
  `max_cooccurrence_per_derive` **edges written per call** (create or reinforce
  both count). `ParentOf` contents do NOT join CoOccurrence.
- **`ParentOf` contents** resolve through the same canonicalize/create path;
  a brand-new one is created as a concept with `PARENT_OF_CONCEPT_TYPE` (Entity
  — derive's caller supplies types only for `concepts`) and its own Derives
  edge, and appears in `outcome.created`/`matched` alongside `concepts` nodes.
  **Reflexive pairs are rejected** with `StoreError::Invariant` — a
  Hierarchical self-loop is a cycle (§5.7 / adve-review T2.1 M1) and would trip
  `assert_invariants`. Raw-content-equal pairs are rejected before any write;
  key-collision pairs after resolution (concepts may already exist — derive is
  not transactional, though it can never leave the graph invariant-violating).
- **Timestamps:** derive takes no clock; all stamps derive from the
  interaction's `created_at` (deterministic, rebuild-friendly). Derives edges
  follow `insert_concept`'s convention (edge stamped with the concept's
  `created_at`).
- **Errors:** missing interaction → `NotFound`; interaction id naming a
  non-Interaction node → `NotFound` (pinned contract says both); reflexive
  parent_of → `Invariant`. `derive` returns `Ok(empty outcome)` for an empty
  call (no-op, no mutations).

**Verification:** 15 new unit tests (creation, within-call dedup, match-reuse
across interactions, CoOccurrence cap 2-of-4, parent_of creation, re-derive
reinforcement incl. Hierarchical, missing/non-interaction errors, reflexive
rejection, canonical-key-collision collapse without a CoOccurrence self-loop,
empty no-op, drained-batch ordering, plus two round-1 review regressions:
key collision onto a pre-existing concept must not self-reinforce the fresh
Derives edge, and colliding parent_of pairs must write one Hierarchical edge).
`cargo test graph::` (default features):
80 passed / 0 failed (65 pre-existing + 15 new), 0 warnings; clean
`cargo build`. Every test ends with `assert_invariants()` (Derives/Temporal
§5.7 coverage included). No fixtures read, so no `#[cfg(feature =
"fixtures")]` gating needed.

### T2.4 — `record_action()` + cycle check (done 2026-08-11, by T24Action)

**What exists now:** `src/graph/action.rs` (new, ~640 LOC incl. tests) + one
additive `pub mod action;` in `src/graph/mod.rs`. No other files touched — in
particular `src/graph/graph.rs` is untouched (`upsert_edge` still stores what
it's given; this module is the spec §5.7 write-time cycle gate).

Pinned API exactly as spec'd: `Action<'a>` (action/produces/modifies/depends_on),
`ActionOutcome { action_node, created, edges }`,
`record_action(graph, interaction, agent, &Action) -> Result<ActionOutcome, LamboError>`.
Flow per spec §7: interaction must be an `Interaction` node (else
`StoreError::NotFound`) → resolve ALL contents via `canonicalize` read-only
(action/produces/modifies → `Resource`, depends_on → `Entity`; unmatched become
planned concepts, `canonical_key` from `Unmatched.key`) → plan edges (source =
action node; `Causal` to produces/modifies, `Dependency` to depends_on,
deduped by natural key) → BFS cycle check → validate-then-mutate
(`insert_concept(origin_interaction = interaction)` then `upsert_edge`).

**Decisions the next agent must not re-derive:**
- **Cycle check = one BFS per planned edge over `Causal`/`Dependency`
  out-neighbors of graph ∪ planned edges**: for each planned `a -> b`, if `b`
  reaches `a`, reject `StoreError::Invariant("{ty:?} edge {a} -> {b} would
  create a cycle")`. The edge under test is never traversed (it is an incoming
  edge of `b`; reaching `a` terminates), so including all planned edges in the
  search is exact. Self-loops (`a == b`) are rejected up front (an action
  producing/depending on itself). `Hierarchical` is excluded — write-time
  acyclicity of that type is not in the pinned §5.7 contract.
- **In a single call all planned edges originate at the action node**, so the
  only *planned-vs-planned* cycle possible in one call is the self-loop; the
  "other planned edges" arm of the BFS is still implemented generally (cheap,
  mandated by the task, and future-proof if the edge set ever widens).
- **Matched concepts are reused as-is** (no type change, no re-Derives): the
  schema `UNIQUE (session_id, canonical_key)` makes re-creating a matched
  action concept illegal, and canonicalization exists precisely so repeated
  phrases collapse to one node. Re-recording an action therefore yields the
  same `action_node`, `created = []`, `edges = 0`, and the existing edges
  reinforce (`reinforcements += 1`).
- **`created`/`edges` count *new* writes only**: `created` = planned concepts
  actually inserted (encounter order: action, produces, modifies, depends_on);
  `edges` = deduped planned edges whose natural key did not pre-exist
  (re-recorded edges reinforce and are not counted).
- **Timestamps = the interaction's `created_at`** for both created concepts and
  edges. The pinned signature has no clock param; `Utc::now()` would break
  deterministic snapshots. A future `Memory` wrapper may override.
- **Within-call dedup by canonical key** (first encounter wins, including the
  type when the same phrase appears as both Resource and Entity — produces
  before depends_on in encounter order).
- **Edge weights:** `CAUSAL_WEIGHT` / `DEPENDENCY_WEIGHT` = 0.5, the
  module-owned structural default (same initial value as the other
  module-created structural edges; `Derives`/`Temporal` are Graph-owned at
  0.9/1.0). Fixture `Dependency` weights are story-specific hand-set values in
  `gen-fixtures.py`, not a convention to mirror.
- **Watch the stopwords when crafting tests**: `"a"` is in `STOPWORDS`, so
  `canonicalize("a")` → key `""` — seeded concepts must use non-stopword
  content ("b", "c", …) or explicit canonical keys.

**Verification:** 10 new unit tests, all green under `cargo test graph::`
(default features): happy path (Resource action node + Derives from
interaction + Causal to produces/modifies + Dependency to depends_on, correct
direction and weights, outcome fields, invariants), implicit creation +
matched-concept reuse, missing/non-interaction → `NotFound`, A→B→A dependency
rejection with **snapshot + log_len + epoch equality** (byte-identical),
3-hop chain (a→b→c→a) rejection, self-referential planned edges (produces
self / depends_on self) rejection with graph unchanged, cycle closing
pre-existing graph edges (upsert_edge-seeded) rejection, mutation ordering
(nodes before edges in `drain_log`; batch = 4 node upserts + 5 edge upserts,
every edge endpoint upserted earlier in the batch), within-call dedup
(`produces ["x","x"]` + `depends_on ["x"]` → 2 edges, 1 concept), and
re-record = reinforcement (no new nodes/edges, `reinforcements` bumps).
`cargo test graph::` (default features): 75 passed / 0 failed (65
pre-existing + 10 new), 0 warnings. No fixtures read, so no
`#[cfg(feature = "fixtures")]` gating needed.

### T2.5 — `demote()` (done 2026-08-11, by T25Demote)

**What exists now:** `src/graph/demote.rs` (new, ~480 LOC incl. tests) + one
additive `pub mod demote;` in `src/graph/mod.rs`. No other files touched —
`graph.rs`, `canonical.rs`, `types`, fixtures, `Cargo.toml` all untouched.

Pinned API exactly: `demote(graph, interaction, agent, chunk, chunk_group_id) ->
Result<Vec<NodeId>, LamboError>` returning the created Observation ids in
sentence order. Flow per spec §7: validate `interaction` (missing OR
non-interaction node -> `StoreError::NotFound`; validated up front so the typed
error, not `insert_concept`'s `Invariant`, is what callers see) -> UAX #29
segmentation via `UnicodeSegmentation::split_sentence_bounds`
(`unicode-segmentation`, spec §6.3 pinned crate; no custom split fn) -> per
sentence: trim, skip empty/whitespace-only, create one `Observation` concept
(`content` = trimmed sentence, `canonical_key` from T2.2, `origin_interaction`
= interaction, `origin_agent` = agent, `chunk_group_id = Some(...)`) ->
`graph.insert_concept(...)` (auto `Derives` edge from the interaction at the
Graph-owned 0.9; this module creates no edges itself, so no weight constants).
Empty/whitespace-only chunk -> `Ok(vec![])`, zero mutations. No dedup across
sentences (observations are new by construction; identical sentences both land).

**Decisions the next agent must not re-derive:**

- **`canonical::canonical_key(sentence, graph.synonym)` does not compile.**
  `canonical_key` pins `impl Fn(&str) -> Option<&str>` = `for<'a> Fn(&'a str) ->
  Option<&'a str>` (HRTB); a closure borrowing the graph returns a borrow of the
  graph, not of the input, so it cannot satisfy the bound — the same T2.2
  constraint that forced `canonicalize` to do its own raw lookup. `demote` does
  the raw synonym lookup itself (`graph.synonym(raw.trim()).unwrap_or(raw)`)
  and calls `canonical_key(effective, no_synonym)` with a never-matching `fn`
  item (`fn(&str) -> Option<&str>` coerces to the HRTB fn-pointer bound). Semantics
  are identical to the pinned call: direct lookup only (no chains), and the
  extra trim on the mapped value cannot change tokens (`normalize_tokens` splits
  on all whitespace).
- **`last_demotion_time` is left `None`.** T2.5 creates brand-new Observations
  (`canonization_status: None`); the P6 doc's "demotion sets
  `last_demotion_time`" is the canonization-daemon budget demotion (P6), a
  different operation on existing canonical nodes. If a reviewer disagrees this
  is where the field belongs, it's a one-field change.
- **`created_at` is `Utc::now()`** — the pinned signature has no clock param
  (unlike T2.7's explicit `now`), so tests never assert timestamps.
- **UAX #29 surprises (probed against the vendored crate source, all pinned in
  tests):** `unicode-segmentation` 1.13.3 implements the default UAX #29 rules
  with NO locale abbreviation lists — SB6 in this crate is the *numeric* rule
  (no boundary between a terminator and a following digit: `"Pi is 3.14. Next."`
  splits after `"3.14."`, not inside it), SB7 is the acronym rule
  (`UpperLower ATerm × Upper`: `"U.S.A. is big."` keeps `U.S.A.` whole), SB8
  places no boundary before a Lowercase start (`"U.S.A. is big."` is therefore
  ONE sentence), and `"Dr. Smith left."` splits after `Dr.` into two sentences —
  no abbreviation data, and `Upper` after the space fails SB7. This is the
  crate's (and thus UAX #29 default's) contract;
  `uax29_numeric_acronym_and_abbreviation_rules` asserts all four cases.
- **`split_sentence_bounds` folds the SB1 leading break into the first segment**
  (the iterator consumes break 0 as the segment start), so a chunk is never
  split on a leading empty segment; segments DO include trailing whitespace
  (SB7's space), which is exactly why trim+skip-empties is the contract. A
  whitespace-only chunk yields one whitespace segment -> skipped -> `Ok(vec![])`.
- **First interaction emits no `Temporal` edge** (no predecessor), so after
  `fresh_graph` the log holds 1 mutation, not 2 — irrelevant to demote but easy
  to trip on in tests.
- **No lock, no async** — module is synchronous and pure, like all P2 modules;
  mutations flow only through `Graph`'s write API (mutation log untouched
  directly). `chunk_group_id` is stored verbatim (no empty-string validation —
  pinned API takes `&str` as-is).

**Verification:** 10 new unit tests under `graph::demote::tests`
(multi-sentence split + shared group id + Derives edges + contents/types/keys,
single sentence + contraction + no-terminator chunk, empty chunk noop,
whitespace-only chunk noop, missing interaction -> `NotFound`, non-interaction
node -> `NotFound`, duplicate sentences not deduped, trailing punctuation +
newline, UAX #29 numeric/acronym/abbreviation rules, synonym-before-
normalization canonical key (`register_user` -> `create_user`), mutation-log
ordering node-before-edge per sentence with returned ids == upsert order). Every
test ends with `assert_invariants()`. `cargo test graph::` (default features):
75 passed / 0 failed (65 pre-existing + 10 new), 0 warnings; full
`cargo test --lib`: 166 passed / 1 ignored (live-calibration). No
fmt/clippy/project-wide suites run (per constraints).
