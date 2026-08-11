# Adversarial review — **T1.4 Fixture graphs** (main)

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: OPEN                                            ║
║  Disposition: REJECT (must-fix before swarm trust)        ║
║  Opened: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T1.4 — Fixture graphs (swarm unblocker)  
**Scope:** `fixtures/*`, `scripts/gen-fixtures.py`, `src/fixtures.rs`, `MemoryStore::seed`  
**Out of scope:** full P2–P7 implementations; T1.1–T1.3 foundation (already reviewed)  
**Date:** 2026-08-11  
**Method:** hostile rounds against phase contract, spec §5.7 / §8.1 / §10, loader tests, generator, and structural claims

**Verdict:** **REJECT T1.4 as currently shipped** for production-of-record use by P2–P7.

The package **does** unblock cold-start work (JSON loads, structural stage-2/3 numbers mostly real, five files present). It does **not** meet the written done-when:

> every fixture loads through `MemoryStore` without invariant violations (spec §5.7 checked by a test)

Several goldens and graph keys will actively mis-train downstream agents if taken as truth.

---

## Executive summary

| Question | Answer |
|----------|--------|
| Are the five named fixtures present and loadable? | **Yes** |
| Does `MemoryStore::seed` + load path work? | **Yes** |
| Are §5.7 invariants enforced / tested? | **No** — edges exist, but Temporal + Derives invariants fail hard |
| Does `user schema` lawfully pass **all three** canonization stages? | **No** — Stage 1 peer gate fails; only gc_survived + S2 + S3 checked |
| Are recall goldens EXACT for `keyword_candidates`? | **No** — `"create"` also hits `user created at` |
| Do graph `canonical_key`s match the documented Porter convention? | **No** — multiple mismatches (middleware, responses, profile, created/updated) |
| Is the conflict-window seed usable against wall-clock? | **No** — timestamps are frozen in the past; handoff admits P4 must re-plant |
| Can P4/P5/P6 start offline? | **Yes, with caveats** — treat fixtures as sketches, not oracle |

---

## Round 1 — Phase done-when vs delivery

| Finding | Sev | Evidence | Status |
|---------|-----|----------|--------|
| Phase requires §5.7 checked by a test; only endpoint existence is checked | **high** | `edges_reference_existing_nodes` only; `seed` inserts snapshot with zero validation | Open |
| Test named `loads_into_memory_store_without_invariant_violations` does not check invariants | **med** | Asserts concept count + `user schema` status only | Open |
| Exit criteria checkbox still unchecked while handoff says DONE | **low** | `PHASE-1-contracts.md` exit vs T1.4 handoff | Open |
| Helper lives in `src/fixtures.rs`, not `src/store/memory.rs` as task body said | **low** | Documented in handoff; acceptable | Accept |

**§5.7 (spec) reduced list:**

1. Every non-first interaction has exactly one **Temporal** predecessor  
2. Every concept has at least one **Derives** edge  
3. No duplicate `(source, target, edge_type)`  
4. No cycles in `Causal` / `Dependency`  
5. Weights ≥ 0 and finite  

**Measured on committed graphs:**

| Graph | Temporal edges | Derives edges | Dup keys | Dep cycles | Dangling ends |
|-------|----------------|---------------|----------|------------|---------------|
| `session-rest-api` | **0** (12 interactions chained only via `previous_id`) | **0** / 20 concepts | 0 | none | 0 |
| `session-drift` | **0** (both interactions `previous_id: null`) | **0** / 9 concepts | 0 | none | 0 |

**Conclusion:** fixtures are **structurally illegal** under the same invariants T2.1 will enforce. Loading them into a law-abiding graph core should fail `assert_invariants()`.

---

## Round 2 — Canonization claims (the load-bearing demo nodes)

### 2a. `user schema` — "passes all three stages"

| Stage | Spec requirement | Fixture reality | Honest? |
|-------|------------------|-----------------|---------|
| 1 Candidate | `gc_survived >= 3` **and** composite ≥ P90 of non-Canonical peers **and** `non-Canonical peers >= 20` | `gc_survived=4` only. Concepts: 20 total, **19 non-Canonical** (`None`×18 + `Venerable`×1). Peer gate **fails** (`canonization_min_peer_count=20`). No scores. Status pre-stamped `Canonical` with no `canonization_events` | **No** |
| 2 Venerable | `interaction_span` ≥ 3 distinct, coverage ≥ 0.3, aged edges | distinct=6, coverage ≈ **0.455** (25/55 min). Handoff says ~0.42 (used 25/60) | **Yes** (coverage note wrong) |
| 3 Canonical | `blast_radius > 5` | computed exclusive orphans = **8** (matches stored `blast_radius: 8`) | **Yes** |

Tests only assert S1=`gc_survived>=3`, S2, S3 — not lawful full Stage 1.

| Finding | Sev | Status |
|---------|-----|--------|
| Peer count 19 < 20 → Stage 1 cannot lawfully fire | **high** | Open — add ≥1 more non-Canonical concept (or drop one Canonical/Venerable and keep ≥20 peers) |
| Status/events pre-stamped without audit trail | **med** | Residual — ok as seed if labeled "already-canonical"; not a progression trace |
| Handoff coverage ~0.42 vs real ~0.455 | **low** | Doc drift |

### 2b. `api layer` — "Stage 2 pass, Stage 3 fail"

| Claim | Reality |
|-------|---------|
| Stage 2 | distinct=3 (create/rate/pagination), coverage ≈ 0.545 — pass |
| Stage 3 | exclusive orphans = **only `api docs`** (br=1). `caching layer` also inbound from `load testing` — correct |
| Generator comment | says `blast_radius = 2` — **stale/wrong** vs final graph |

| Finding | Sev | Status |
|---------|-----|--------|
| Stage 2/3 split is real and useful | — | Pass |
| Generator comment contradicts computed br | **low** | Open (comment hygiene) |

---

## Round 3 — Conflict-window seed

Task: "one recent write from agent-A inside the conflict window."

| Claim | Reality |
|-------|---------|
| `caching layer` origin_agent = `agent-a` | True |
| Inside `conflict_recency_window=30s` of `Utc::now` | **False** — all stamps on `2026-08-10T09:xx` (frozen past) |
| Handoff | Explicitly says P4 must re-plant against wall clock |

Spec conflict: ≥2 agents with edges / write activity inside 30s window. Fixture timestamps make **zero** activity recent. The "conflict seed" is only a label, not a runnable scenario.

| Finding | Sev | Status |
|---------|-----|--------|
| Conflict seed unusable without time rewrite | **high** (for P4) | Open — ship relative offsets + test helper that shifts to `now - N s`, or document as structure-only |
| Both Dependency writers into CACHE are agent-b concepts (API, LOAD); agent-a is node author only | **med** | Ambiguous for conflict attribution |

---

## Round 4 — Drift fixture

| Claim | Reality | Ok? |
|-------|---------|-----|
| Root goal Venerable | `launch the product` status Venerable | Yes |
| On-path ≤5 hops | steps 1–5 at hops 1–5 | Yes |
| Far concept >5 hops | `far budget concept` at hop **6** (directed chain) | Yes |
| Disconnected component (GC food) | `isolated widget` ↔ `isolated sibling`, no path from root | Yes |
| `root_goal` field | JSON **string** `"launch the product"`, not concept id / array | Med — schema is `JSONB`; consumers must not assume UUID |
| Temporal chain for GC step 3 | **No** Temporal edges; interaction 2 not linked via `previous_id` | High for GC that BFS from temporal chain |
| Drift `canonical_key`s | Raw content (`"launch the product"` keeps stopword) | Low — inconsistent with §8.1 convention |

| Finding | Sev | Status |
|---------|-----|--------|
| Hop geometry correct for directed Dependency BFS | — | Pass |
| Missing Temporal / previous_id breaks "BFS from temporal chain" GC food story | **med** | Open |
| `root_goal` shape underspecified vs concept id | **med** | Open — freeze shape for P4 |

---

## Round 5 — Mutations batch

| Claim | Reality |
|-------|---------|
| Every mutation kind present | Yes: upsert_node, upsert_edge, delete_node, delete_edge, canonization_transition |
| Applies cleanly | Yes — test green; delete cascades edges so trailing `delete_edge` is no-op |
| Valid edge semantics | **No** — `Temporal` edge from **interaction → concept** (`7001→7002`). Spec table: Temporal is **Interaction → Interaction**; Interaction → Concept is **Derives** |
| Spec §2.4 ideal order | Nodes → edges → deletes → transitions — batch matches that skeleton |

| Finding | Sev | Status |
|---------|-----|--------|
| Illegal Temporal endpoint types | **high** | Open — use Derives (I→C) + Temporal (I→I) correctly |
| Redundant delete_edge after delete_node | **low** | Accept as intentional cascade probe |
| No session field on MutationBatch | — | Matches type (`mutations` only) |

---

## Round 6 — Recall goldens (P5 trap)

Phase: `phase1_candidates` are **EXACT**.

`MemoryStore::keyword_candidates` matches substring on `content` **or** `canonical_key`.

| Query | Golden phase1 EXACT | Actual keyword hits |
|-------|---------------------|---------------------|
| `pagination` | `{pagination}` | `{pagination}` only |
| `create` | `{create user}` | **`{create user, user created at}`** (`"created".contains("create")`) |

Also: `canonical_key` `"created user"` contains `"create"` / `"creat"` depending on tokenization — even worse if keys are fixed to proper Porter (`creat user`).

Phase2 "REQUIRED members" for create/pagination match 1-hop undirected neighbors of the intended candidate — fine **as a subset**, but phase1 EXACT is false.

| Finding | Sev | Status |
|---------|-----|--------|
| `create` golden is wrong under MemoryStore keyword semantics | **high** | Open — rename orphan (`user birth timestamp`) or change query to `"create user"` / multi-token |
| No test runs goldens through `keyword_candidates` | **high** | Open |
| phase2 listed as full expansion risk | **low** | Handoff already softens to REQUIRED members — ok |

Depth-2 expansion from `create user` actually reaches almost the entire connected component — P5 must not treat phase2 list as closed set (handoff correct).

---

## Round 7 — Canonicalization cases & graph keys (P6 trap)

### 7a. `canonicalization-cases.json`

| Check | Result |
|-------|--------|
| Categories cover hyphen/camel/stopword/stem/synonym/near-pair | Yes |
| Near-pair A/B distinct keys | Asserted in test — Pass |
| Keys match rust-stemmers Porter | Spot-checked cases table: **Pass** for listed rows |
| Cases **executed** against a shared normalize() | **No** — parse-only tests. T6 can diverge silently |

### 7b. Concept keys inside `session-rest-api` vs same convention

Using Porter (`rust-stemmers` English) + stopword drop + sort (same convention as handoff):

| content | stored key | computed key | Match |
|---------|------------|--------------|-------|
| auth middleware | `auth middleware` | `auth middlewar` | **No** |
| error responses | `error response` | `error respons` | **No** |
| user profile | `profile user` | `profil user` | **No** |
| user created at | `created user` | `creat user` | **No** |
| user updated at | `updated user` | `updat user` | **No** |
| (most others) | … | … | Yes |

Handoff itself warns: "do NOT hand-guess; re-probe if adding cases" — then the **main graph** hand-guesses.

| Finding | Sev | Status |
|---------|-----|--------|
| Graph keys disagree with cases convention / Porter | **high** | Open — generate keys in `gen-fixtures.py` via the same stemmer or a Rust probe |
| Cases not enforced by code under test | **med** | Open — T6 must own `canonicalize()` + assert fixture table |
| Synonym table only has `register_user→create_user` | — | Pass for demo |

---

## Round 8 — Loader API & seed safety

| Finding | Sev | Status |
|---------|-----|--------|
| `load_snapshot` / `load_store` / batch / goldens / cases | — | Pass (usable) |
| `seed` overwrites session, no merge, no invariants | **med** | Residual for T2.1 — seed is a backdoor past write-path checks |
| Feature gate `fixtures` default-on | — | Pass; `--no-default-features` lean |
| Path via `CARGO_MANIFEST_DIR` | — | Pass for unit tests; brittle if fixtures moved out of package root |
| Typed goldens as `serde_json::Value` not structs | **low** | Residual — weak compile-time contract for P5/P6 |

---

## Round 9 — Generator quality

| Finding | Sev | Status |
|---------|-----|--------|
| Deterministic IDs (`f0000000-…`) | — | Pass |
| Regeneratable single script | — | Pass |
| Script does not validate §5.7, stems, or goldens against store | **high** | Open — generator should emit Temporal+Derives and self-check |
| Comment/doc drift (api br=2, coverage 0.42) | **low** | Open |
| UUID note in handoff (don't use low-u64 arithmetic) | — | Good; goldens use full strings |

---

## Round 10 — Downstream blast radius (who gets hurt)

| Track | Risk if they trust T1.4 blindly |
|-------|----------------------------------|
| **P2 graph core** | `assert_invariants()` fails on day one; or invariants get weakened to match bad fixtures |
| **P4 daemon** | Conflict never fires on frozen timestamps; drift root_goal shape ambiguous |
| **P5 recall** | Phase1 golden for `create` fails against real keyword path |
| **P6 canonization** | Stage1 peer gate; key mismatches break match/dedup demos |
| **P3 SQL adapters** | Conformance vs MemoryStore ok; illegal graphs may still "pass" seed |

---

## Must-fix before ACCEPT

1. **§5.7 legality**  
   - Emit Temporal chain for all non-first interactions.  
   - Emit Derives from `origin_interaction` → every concept.  
   - Test: full invariant suite on every session fixture after `seed` / load.

2. **Lawful Stage 1**  
   - Ensure ≥20 non-Canonical peers in `session-rest-api` (or document intentional pre-canonical seed and stop claiming "lawfully passes Stage 1").

3. **Recall golden honesty**  
   - Fix `create` collision with `user created at` (rename content or tighten query).  
   - Test: load goldens → `keyword_candidates` → set equality with `phase1_candidates`.

4. **Canonical keys**  
   - Recompute all concept keys with Porter (shared with cases).  
   - Prefer generating keys in the script from content, not hand lists.

5. **Mutations Temporal**  
   - Interaction→Concept must be `Derives`; add a true Interaction→Interaction `Temporal` if needed.

6. **Conflict seed**  
   - Either relative-time helper for tests, or remove "inside conflict window" claim from done semantics.

7. **Process**  
   - Flip P1 exit criteria checkbox only after the above; keep handoff surprises accurate.

---

## Acceptable residual (after must-fix)

| Item | Notes |
|------|-------|
| Pre-stamped Canonical without live score path | Fine if labeled seed state |
| phase2 REQUIRED ⊆ full expansion | Already documented |
| `root_goal` JSON shape | Freeze in types when P4 lands |
| Goldens as `Value` | Upgrade when P5 owns typed structs |
| Fixed historical timestamps for age filters | Good for min_age=60s tests once conflict uses a shift helper |

---

## What already passes (credit)

- Five required fixture files committed + regenerator.  
- `fixtures` module + `MemoryStore::seed` load path works offline.  
- Stage 2/3 geometry for `user schema` / `api layer` is intentionally constructed and **computed** correctly by MemoryStore.  
- Drift hop=6 + disconnected component geometry is clear.  
- Mutation batch exercises all five ops and flush apply path.  
- Canonicalization **cases table** near/far distinctness and Porter stems (for the table rows) look intentional.  
- UUID stability and handoff warnings about Porter / ID arithmetic are the right kind of operational notes.

---

## Suggested disposition criteria

| Disposition | When |
|-------------|------|
| **REJECT** (current) | Any of must-fix 1–5 still true |
| **ACCEPT with residuals** | Must-fix closed; conflict time helper may remain residual if documented |
| **Reopen** | Fixture regen changes Stage 2/3 numbers, goldens, or §5.7 without updating this review |

---

## Evidence commands (repro)

```bash
# Unit tests (currently green — they under-assert)
cargo test --lib fixtures::

# §5.7 surface scan
python3 - <<'PY'
import json
from pathlib import Path
for name in ["session-rest-api","session-drift"]:
    s=json.loads(Path(f"fixtures/{name}.json").read_text())
    et=[e["edge_type"] for e in s["edges"]]
    print(name, "Temporal", et.count("Temporal"), "Derives", et.count("Derives"),
          "concepts", len(s["concepts"]))
PY

# Keyword golden collision
# "create" hits both create user and user created at under MemoryStore rules
```

---

## Cross-links

- Task contract: `dev-diary/PHASE-1-contracts.md` §T1.4  
- Spec: `lambo-hackathon-spec-v0.1.md` §5.7, §8.1, §9 conflict/drift, §10 stages  
- Prior foundation review: `adve-review-p0-p1-foundation-self.md` (left T1.4 open)
