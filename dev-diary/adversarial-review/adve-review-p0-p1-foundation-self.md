# Adversarial review — P0/P1 foundation (15 rounds)

**Scope:** `src/**`, `Cargo.toml`, spikes (read-only), scripts/migrations (surface).  
**Date:** 2026-08-10  
**Verdict after remediation:** **ACCEPT with residual notes** (T1.4 fixtures still open).

Each round is a hostile stance. Findings are severity-tagged. **Fixed** items were remediated in the same session.

---

## Round 1 — Spec fidelity (types & defaults)

| Finding | Sev | Status |
|---------|-----|--------|
| Config defaults match §2.4 / §9 / §10 named values | — | Pass (asserted) |
| Seven edge types only; decay flags match §5 table | — | Pass |
| `MatchStrategy` enum default is `Canonical` but `Config::default` uses `Hybrid` | low | Intentional: enum default for bare values; product default Hybrid. Documented via Config. |
| Edge primary key in schema is UUID; typed as `NodeId` | low | Residual: rename to `EdgeId` in a later types polish if desired — same Uuid newtype. |

## Round 2 — Mutation / flush ordering (spec §2.4)

| Finding | Sev | Status |
|---------|-----|--------|
| Prior flush grouped by session and could reorder cross-session batches | **high** | **Fixed:** single ordered pass |
| Deletes applied to *all* sessions when session unknown | **high** | **Fixed:** resolve session by id scan; no-op if missing |
| No session consistency check on upsert | **med** | **Fixed:** invariant if node/edge session ≠ snapshot |

## Round 3 — Concurrency & locks (spec §6.4)

| Finding | Sev | Status |
|---------|-----|--------|
| Used `std::sync::RwLock` while `parking_lot` is a declared dep | med | **Fixed:** `parking_lot::RwLock` |
| Async trait methods held lock across `.await` | — | Pass (no await under lock) |
| Concurrent flushes panic | — | Pass (test added) |

## Round 4 — Structural queries (canonization feed)

| Finding | Sev | Status |
|---------|-----|--------|
| `blast_radius` logic redundant/confusing; risk of wrong counts | **high** | **Fixed:** clear from_node && !from_other |
| Age filter used `unwrap_or_default()` on Duration convert → silent wrong cutoff | **med** | **Fixed:** return `StoreError` |
| Uses wall-clock `Utc::now` (nondeterministic tests) | low | Residual: document; tests plant aged timestamps |
| `interaction_span` coverage not clamped | low | **Fixed:** clamp 0..=1 |

## Round 5 — Keyword retrieval

| Finding | Sev | Status |
|---------|-----|--------|
| Empty string token matches every concept via `contains("")` | **high** | **Fixed:** filter empty/whitespace tokens |
| `limit == 0` not short-circuited | low | **Fixed** |
| Sort unstable on score ties | low | **Fixed:** tie-break by uuid |

## Round 6 — Capability gating

| Finding | Sev | Status |
|---------|-----|--------|
| MemoryStore correctly denies vector path | — | Pass |
| Capabilities not exposed in test | low | **Fixed:** assert empty |

## Round 7 — Identity types footguns

| Finding | Sev | Status |
|---------|-----|--------|
| `NodeId::default()` generated a **new random UUID** | **high** | **Fixed:** nil UUID default |
| SessionId empty string is valid key | low | Residual: allow; callers should validate non-empty at API edge |

## Round 8 — Embedder determinism & geometry

| Finding | Sev | Status |
|---------|-----|--------|
| Near-pair threshold contract | — | Pass |
| `DefaultHasher` not rustc-stable forever | med | **Documented** in FixtureEmbedder |
| `cosine` zip-truncates mismatched dims | **med** | **Fixed:** require equal length |
| NaN/Inf in vectors | low | **Fixed:** return 0 |

## Round 9 — Public API surface

| Finding | Sev | Status |
|---------|-----|--------|
| `pub use types::*` glob hides API | med | **Fixed:** explicit re-exports |
| Unused heavy deps (sqlx/aws) inflate CI for empty modules | low | Residual: acceptable until P3/P7; keep for skeleton |

## Round 10 — Serde / fixtures readiness

| Finding | Sev | Status |
|---------|-----|--------|
| Node/Mutation tagged enums round-trip | — | Pass |
| Full `Config` JSON not round-tripped (`Duration` shape) | low | Residual: ok until config file format chosen |
| StoreError not Serialize | low | Residual: not needed until wire errors |

## Round 11 — Error model

| Finding | Sev | Status |
|---------|-----|--------|
| Canonization of missing concept was silent success | **med** | **Fixed:** `NotFound` |
| `LamboError` / `StoreError` thiserror coverage | — | Pass |

## Round 12 — CLI / binary

| Finding | Sev | Status |
|---------|-----|--------|
| Stub main only — no secret leakage | — | Pass |
| Bare `lambo` exits 2 | — | Pass |
| clap `debug_assert` | — | Pass |

## Round 13 — Supply chain / repo hygiene

| Finding | Sev | Status |
|---------|-----|--------|
| `.env` gitignored | — | Pass |
| Spikes `target/` may exist locally | low | Covered by `**/target` |
| MIT license present | — | Pass |
| rmcp 0.1 vs 3.x | low | Residual: upgrade carefully in P8 |

## Round 14 — Security

| Finding | Sev | Status |
|---------|-----|--------|
| No secret material in source | — | Pass |
| Agent identity unauthenticated (spec) | info | By design §11 |
| Keyword path substring match is naive (DoS on huge graphs) | low | Residual: MemoryStore is test scale |

## Round 15 — Test completeness vs claimed contracts

| Finding | Sev | Status |
|---------|-----|--------|
| No blast_radius test before | **med** | **Fixed** |
| No session isolation test | **med** | **Fixed** |
| No empty-token test | **med** | **Fixed** |
| No concurrent flush test | low | **Fixed** |
| T1.4 fixtures missing | **high** (phase exit) | **Open** — not foundation code bug |

---

## Remediation summary

Hardened:

1. Ordered, session-correct `MemoryStore::flush`
2. Correct `blast_radius` / safer age cutoffs
3. Keyword empty-token footgun removed
4. `parking_lot` locks
5. Safe `NodeId` default
6. Explicit crate re-exports
7. Cosine safety
8. Expanded unit tests (session isolation, blast radius, concurrency, …)

## Residual risks (acceptable for now)

- T1.4 fixture graphs not committed → swarm not fully unblocked  
- Fixture embedder hash stability across rustc versions  
- Edge id typed as `NodeId`  
- Production adapters (P3) not yet adversarially reviewed  

## Gate

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All must be green after this review.
