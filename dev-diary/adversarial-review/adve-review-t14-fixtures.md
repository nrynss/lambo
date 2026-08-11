# Adversarial review — **T1.4 Fixture graphs** (main)

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT (residual notes only)               ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T1.4 — Fixture graphs (swarm unblocker)  
**Scope:** `fixtures/*`, `scripts/gen-fixtures.py`, `src/fixtures.rs`, `MemoryStore::seed`,
`MemoryStore::blast_radius` (cross-path fix required by §5.7 legality)  
**Out of scope:** full P2–P7 implementations; T1.1–T1.3 foundation (already reviewed)  
**Do not reopen T1.4** for residual notes or for T2.1/T3.x/T6 ownership items below.

**Date opened:** 2026-08-11  
**Remediation commit:** `cf83294` (+ prior remediation work in `8204066` / `d8d2dc5`)  
**Disposition re-check:** 2026-08-11 (this close)  
**Gate at close:** `cargo fmt --check`; `clippy` default + `--no-default-features` (`-D warnings`);
**55** lib tests pass (`blast_radius_ignores_provenance_derives_edges` included).

---

## Executive summary (post-remediation)

| Question | Answer |
|----------|--------|
| Are the five named fixtures present and loadable? | **Yes** |
| Does `MemoryStore::seed` + load path work? | **Yes** (+ `load_store_relative`) |
| Are §5.7 invariants present + tested? | **Yes** — Temporal + Derives + dups + weights; `satisfies_spec_57_invariants` |
| Does `user schema` lawfully clear Stage 1 peer gate + S2 + S3? | **Yes** — 21 non-Canonical peers; distinct 6; br=8 |
| Are recall goldens EXACT for `keyword_candidates`? | **Yes** — orphan renamed; phase1 set-equality tested |
| Do graph `canonical_key`s match Porter convention? | **Yes** — generated via stem table in `gen-fixtures.py` |
| Is the conflict-window seed runnable? | **Yes** — via `load_store_relative` |
| Spec §4.1 blast SQL vs §5.7 Derives? | **Resolved in code**; **spec errata applied** (see Round 11) |
| Can P4/P5/P6 start offline? | **Yes** |

**Verdict:** **ACCEPT T1.4 — CLOSED** with residual notes only.

---

## Round 0 — Original REJECT (historical)

Hostile pass against first T1.4 land found must-fix gaps: missing Temporal/Derives, Stage 1 peer
undercount, recall `create` substring collision, hand-guessed concept keys, illegal mutation
Temporal I→C, conflict timestamps frozen in the past, and a test suite that under-asserted
§5.7. Full original findings preserved in git history of this file prior to close.

---

## Must-fix closure

| # | Must-fix | Status | Evidence |
|---|-----------|--------|----------|
| 1 | §5.7 Temporal + Derives + invariant test | **Closed** | rest-api: 11 Temporal, 22 Derives; drift: 1 Temporal, 9 Derives; `satisfies_spec_57_invariants` |
| 2 | Stage 1 peer gate (≥20 non-Canonical) | **Closed** | 22 concepts, 21 non-Canonical; handoff + stage test |
| 3 | Recall phase1 EXACT via keyword path | **Closed** | `user join time` rename; `recall_goldens_phase1_exact_under_keyword` |
| 4 | Canonical keys via Porter, not hand lists | **Closed** | stem table in `gen-fixtures.py`; graph keys match cases convention |
| 5 | Mutations edge endpoint types legal | **Closed** | Temporal I→I, Derives I→C, Dependency C→C |
| 6 | Conflict seed runnable vs wall clock | **Closed** | `fixtures::load_store_relative` + `conflict_window_recent_write_via_relative_load` |
| 7 | P1 exit criteria checkbox | **Closed** | `PHASE-1-contracts.md` exit criteria all `[x]` |

---

## Round 11 — Spec-internal inconsistency (review miss → agent fix)

**Finding (agent-resolved, accepted at close):** Must-fix #1 (Derives on every concept)
collides with the **literal** §4.1 blast-radius SQL, which treats *any* inbound edge as
"dependent on another source". A mandatory Derives (interaction → concept) would then make
every concept non-orphaned and **zero out** Stage-3 blast radius for `user schema`.

| Aspect | Disposition |
|--------|-------------|
| Severity | **high** (would invalidate Stage 3 + demo) if left as literal SQL |
| Code fix | `MemoryStore::blast_radius` counts only aged inbound `{Dependency, Causal, Hierarchical}` edges whose **source is a concept** |
| Regression | `blast_radius_ignores_provenance_derives_edges` |
| Product result | `user schema` blast stays **8** with legal Derives |
| Spec | **Errata applied** to §4.1 so T3.x SQL adapters do not reintroduce the bug |

This was the right product resolution: provenance edges (`Derives` / `Temporal`) must not
participate in concept-to-concept orphan accounting. Interaction span already restricted to
structural edge types; blast radius now matches that intent.

---

## Residual notes (do not reopen T1.4)

| Item | Owner | Notes |
|------|-------|-------|
| `seed` bypasses write-path invariant enforcement | T2.1 | Seed is a test backdoor; graph core should `assert_invariants` on load |
| Cases table not yet run through live `canonicalize()` | T6 | Fixture is the contract; T6 owns the implementation + assert |
| Goldens typed as `serde_json::Value` | P5 | Fine until typed structs land |
| `root_goal` JSON shape (string vs id/array) | P4 | Freeze when daemon binds it |
| Cycle check in fixture test is shallow | T2.1 | Graphs are acyclic; production BFS belongs in graph core |
| Pre-stamped Canonical without live score path | — | Acceptable seed state; not a live progression trace |
| phase2 REQUIRED ⊆ full depth-2 expansion | P5 | Documented; do not treat as closed set |

---

## Reopen criteria

Reopen **only** if:

- Fixture regen breaks §5.7, Stage 2/3 numbers for the demo nodes, or phase1 golden equality  
- `blast_radius` again counts provenance Derives/Temporal as un-orphaners  
- `load_store_relative` removed without a replacement for conflict-window tests  

---

## Evidence commands (close gate)

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo test --lib
# structural:
#   session-rest-api: Temporal=11 Derives=22 concepts=22 non-Canonical=21
#   blast_radius(user schema)=8 with Derives present
```

---

## Cross-links

- Task / handoff: `dev-diary/PHASE-1-contracts.md` §T1.4  
- Spec: `lambo-hackathon-spec-v0.1.md` §4.1 (errata), §5.7, §8.1, §10  
- Prior foundation review: `adve-review-p0-p1-foundation-self.md` (left T1.4 open; superseded here)
