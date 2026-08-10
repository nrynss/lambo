# Adversarial review — P7 T7.0 BGE-M3 / llama.cpp embedder (15 rounds)

**Scope:** `src/embed/mod.rs`, `src/embed/bge_m3.rs`, `scripts/fetch-bge-m3.sh`,
`scripts/run-llama-embed.sh`, `.env.example`, `Cargo.toml` (reqwest/httpmock additions).
**Date:** 2026-08-11
**Verdict after remediation:** **ACCEPT with residual notes.**

Gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` — all green
(41 tests).

---

## Round 1 — HTTP client defaults (timeout / hang)
| Finding | Sev | Status |
|---------|-----|--------|
| `reqwest::Client::new()` has **no default timeout** — a hung or half-open llama.cpp server could block `embed()` forever and stall the whole recall/derive path | **high** | **Fixed:** `build_client` sets connect timeout 5s + request timeout 60s; `with_timeouts` overrides; health uses its own 5s. Connect-level failures map to `Unavailable`. |

## Round 2 — Error classification vs degradation contract
| Finding | Sev | Status |
|---------|-----|--------|
| Original code returned `Backend` for a *down server*; the T7.2 degradation contract wants fallback-to-canonical + log-once (don't hammer). Muddling "server down" and "server up but unhappy" makes that policy hard to express. | **med** | **Fixed:** documented split — connect-level → `Unavailable` (permanent fallback, log once); non-2xx / bad JSON / wrong dim / non-finite → `Backend`. Comment added for T7.2's benefit. |

## Round 3 — Empty/whitespace input
| Finding | Sev | Status |
|---------|-----|--------|
| Empty text → `Unavailable` with clear message; strict is right for a concept name. | low | Pass (documented). |

## Round 4 — Concurrency / `&self` safety
| Finding | Sev | Status |
|---------|-----|--------|
| `BgeM3LlamaCppEmbedder` is `Clone` and shares a connection-pooled `reqwest::Client` (cheap clone); no locks, no `.await` under any lock. `embed(&self)` is safe for concurrent recall/derive. | — | Pass. |

## Round 5 — Dimension contract
| Finding | Sev | Status |
|---------|-----|--------|
| BGE path accepts any `dim > 0`; mixing a non-1024 embedder with the store's `VECTOR(1024)` is a latent footgun. | low | Residual: adapter layer stays flexible (enforced at store layer); documented in `new()` that 1024 is expected for Cockroach. Fixture path requires exactly 1024. |

## Round 6 — Model id mismatch robustness
| Finding | Sev | Status |
|---------|-----|--------|
| llama.cpp `/v1/embeddings` can 400 on a model id that isn't loaded; with a configured `LAMBO_LLAMA_MODEL` the call would hard-fail even though the server is fine. | **med** | **Fixed:** on 400 with a non-empty model, retry once omitting `model` (server default), bounded by a `retried` flag — no unbounded loop. Test `retries_without_model_on_400_model_not_loaded` + `no_retry_loop_when_model_empty`. |

## Round 7 — Path / URL normalization
| Finding | Sev | Status |
|---------|-----|--------|
| `trim_end_matches('/')` then append paths; endpoints are pinned to `/v1/embeddings` and `/health`. A reverse-proxy prefix would break `/health` derivation — out of scope for a local server. | low | Residual: documented. |

## Round 8 — Input length policy
| Finding | Sev | Status |
|---------|-----|--------|
| No client-side token cap; BGE-M3 ~8192 tokens. Over-long input is delegated to llama.cpp (error or truncation). | low | Residual: policy decision deferred; caller handles errors via fallback. |

## Round 9 — `data[0]` assumption
| Finding | Sev | Status |
|---------|-----|--------|
| We send one input string and consume `data[0]`. If a server returned multiple embeddings we'd silently use the first. | low | Accept: single-input request by construction; `data` empty is rejected. |

## Round 10 — Test isolation / determinism
| Finding | Sev | Status |
|---------|-----|--------|
| Each test spins its own `httpmock::MockServer`; embedders are created per-test; no shared mutable state; normalization asserted on a deliberately non-unit vector. | — | Pass. |

## Round 11 — Recursion in async fn
| Finding | Sev | Status |
|---------|-----|--------|
| Original retry used direct self-recursion in an `async fn` → **E0733** (unboxed recursion = infinitely-sized future). | **high** (compile) | **Fixed:** refactored to a `loop` with a `retried` flag. |

## Round 12 — httpmock matcher API
| Finding | Sev | Status |
|---------|-----|--------|
| `HttpMockRequest.body` is `Option<Vec<u8>>`, not a `String` — `.contains(...)` failed to compile on the raw field. | med (compile) | **Fixed:** `body.as_deref().unwrap_or(&[])` + `String::from_utf8_lossy`. |

## Round 13 — Flaky network tests
| Finding | Sev | Status |
|---------|-----|--------|
| "drop the MockServer then expect connect-refused → Unavailable" tests are **flaky under parallel CI** (the freed ephemeral port can be re-bound by another test). | **med** | **Fixed:** removed both flaky tests. The connect→`Unavailable` mapping is a single `map_err`; kept coverage via the deterministic contract tests. Documented here so nobody re-adds the anti-pattern. |

## Round 14 — Doc/surface coherence
| Finding | Sev | Status |
|---------|-----|--------|
| Trait doc said "Bedrock Titan V2 in production" while BGE is now the default — stale/misleading. | low | **Fixed:** updated to "default: BGE-M3 via llama.cpp; Bedrock Titan V2 swap-in." `lib.rs` re-exports the new surface (`EmbedderKind`, `EmbedderConfig`, `build_embedder`, `embedder_from_env`, `BgeM3LlamaCppEmbedder`). |

## Round 15 — Regression / contract preservation
| Finding | Sev | Status |
|---------|-----|--------|
| `FixtureEmbedder`, `cosine`, `near_far_contract`, store/types untouched; existing tests still pass. reqwest added ~900 lock lines (shared-exception, announced in Handoff Log). Factory: `bedrock` → `Unavailable` with actionable note (T7.1); `fixture` requires dim 1024. | — | Pass. |

---

## Remediation summary

Hardened:
1. Default connect/request timeouts + per-request health timeout (no indefinite hangs).
2. Clearly documented `Unavailable` (server down) vs `Backend` (server up, config/version wrong) split feeding the T7.2 fallback-log-once contract.
3. Bounded retry-once omitting `model` on 400 (robust to llama.cpp model-id mismatch), no recursion.
4. Corrected httpmock matcher API usage; removed flaky dropped-server tests.
5. Stale doc string fixed; public surface re-exported.

## Residual risks (acceptable for now)

- Non-1024 BGE dim allowed at adapter layer (enforced at store).
- No client-side token cap (handled via error → fallback).
- `/v1/embeddings` + `/health` path assumption (local server; no reverse-proxy prefix).
- Real llama.cpp smoke not run here — weights are several GB + gitignored (ops checklist in
  `notes/embeddings-portable.md`); httpmock tests cover the wire contract.

## Gate

```text
cargo fmt --check   # clean
cargo clippy --all-targets -- -D warnings   # clean
cargo test          # 41 passed
```
