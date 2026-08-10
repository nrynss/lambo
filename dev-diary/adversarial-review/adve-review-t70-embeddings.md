# Adversarial review — **T7.0 Embeddings** (main)

**Task:** T7.0 — Embedder factory + BGE-M3 / llama.cpp (default production path)  
**Not this review:** full P7 exit (T7.1 / T7.2 / T7.3) — tracked separately as follow-on work.

**Scope:**

| Area | State |
|------|--------|
| `src/embed/mod.rs` factory + `EmbedderKind` | Implemented |
| `src/embed/bge_m3.rs` HTTP client | Implemented |
| `src/embed/fixture.rs` | Implemented (T1.3; consumed by T7.0) |
| `scripts/fetch-bge-m3.sh`, `scripts/run-llama-embed.sh` | Implemented |
| `.env.example`, `notes/embeddings-portable.md` | Documented |
| Related contract | Cockroach `VECTOR(1024)`, T0.3 spike |

**Date:** 2026-08-11  
**Disposition check:** 2026-08-11 (findings re-verified against tree)  
**Related self-reviews:**

- [`adve-review-p7-t7-0-embedder-self.md`](adve-review-p7-t7-0-embedder-self.md) — first T7.0 pass + remediations  
- [`adve-review-p0-p1-foundation-self.md`](adve-review-p0-p1-foundation-self.md) — foundation (non-embed)  

**Verdict:** **ACCEPT T7.0** with residual ops notes.  
Unit/mocked tests green; live server smoke remains ops-dependent (weights + `llama-server`).  
T7.1–T7.3 are **out of scope for closing this review**.

---

## Executive summary

| Question | Answer |
|----------|--------|
| Is the portable-embedder design sound for T7.0? | **Yes** — trait + factory + 1024-d + L2 normalize |
| Is T7.0 code ready for downstream (T7.2/T7.3)? | **Yes** — call `embedder_from_env()` / `build_embedder` |
| Live demo smoke without ops setup? | **No** — need HF GGUF + llama.cpp running |
| Does this close P7? | **No** — only T7.0 |

---

## Round 1 — Spec vs product decision

| Finding | Sev | Status |
|---------|-----|--------|
| Spec names Bedrock; product default is BGE-M3 | med | **Accept with docs** — portable note + PHASE-7 |
| Phase hard-required T0.4 while T7.0 default is BGE | med | **Closed** — PHASE-7 `requires: [P1, T0.3]`; T0.4 only on T7.1 |

---

## Round 2 — Dimension & schema contract

| Finding | Sev | Status |
|---------|-----|--------|
| BGE-M3 1024 matches `VECTOR(1024)` | — | Pass |
| Factory/BGE accepted any dim | high | **Closed** — `BgeM3LlamaCppEmbedder::new` requires `dim == 1024` |
| Fixture requires 1024 | — | Pass |
| GGUF may emit wrong dim | med | **Residual (ops)** — client rejects len ≠ 1024; pin HF file that is truly 1024-d |

---

## Round 3 — Model-space poisoning

| Finding | Sev | Status |
|---------|-----|--------|
| BGE vs Titan vectors not interchangeable | high | **Out of T7.0 close** — documented; runtime session guard is T7.2/T8 |
| Switch embedder mid-session | high | **Deferred** — not a T7.0 code defect |

---

## Round 4 — HTTP client (T7.0 code)

| Finding | Sev | Status |
|---------|-----|--------|
| Timeouts (connect 5s / request 60s) | — | Pass |
| Connect → `Unavailable` vs HTTP → `Backend` | — | Pass |
| Empty text → `Unavailable` | — | Pass |
| L2 normalize; reject NaN/Inf/zero-norm | — | Pass |
| Dim mismatch rejected | — | Pass |
| 400 + model id → single retry without model | — | Pass |
| OpenAI `/v1/embeddings` only | low | Residual — OK for v0.1 |
| Double-normalize | low | Residual — harmless |

---

## Round 5 — Factory / env surface

| Finding | Sev | Status |
|---------|-----|--------|
| Default embedder → BgeM3 | — | Pass |
| Bedrock stub → clear `Unavailable` | — | Pass |
| Default URL `127.0.0.1:8080` | low | Residual — log at Memory build (T8) |
| `LAMBO_BGE_M3_MODEL` dual-use as path vs HTTP model id | med | **Closed** — factory uses `LAMBO_LLAMA_MODEL` only for request id; path is scripts-only (`.env.example` documents) |
| No process-start health check | med | **Deferred T8** — `check_health()` exists for callers |

---

## Round 6 — Ops scripts (HF + llama.cpp)

| Finding | Sev | Status |
|---------|-----|--------|
| Community GGUF default; no revision pin | med | **Residual (ops)** — pin revision in Handoff when demo freezes |
| curl fallback no checksum | med | Residual — prefer huggingface-cli |
| Server already up → exit 0 | — | Pass |
| Naive port parse from URL | low | Residual — local default fine |
| Missing `--embedding` | high if true | **Closed** — `run-llama-embed.sh` passes `--embedding` |
| ctx 8192 OOM risk | low | Residual — `LAMBO_LLAMA_EMBED_CTX` |

---

## Round 7 — FixtureEmbedder vs live BGE

| Finding | Sev | Status |
|---------|-----|--------|
| Fixture near-pair is synthetic | med | Accept for unit tests; live calibration noted in PHASE-7 handoff (~0.78 bare names) |
| DefaultHasher stability | low | Documented |
| Fixture dim lock 1024 | — | Pass |
| Live `#[ignore]` smoke | med | **Partial** — ignored live tests exist; optional for T7.0 close |

---

## Round 8–9 — Hybrid / Cockroach (not T7.0)

| Finding | Sev | Status |
|---------|-----|--------|
| No `hybrid.rs` / no Cockroach adapter | high for **P7** | **Out of scope for T7.0 close** — does not reopen T7.0 |

---

## Round 10 — Security

| Finding | Sev | Status |
|---------|-----|--------|
| Default bind 127.0.0.1 | — | Pass (script default) |
| Public bind without auth | high | Residual ops — never expose embed server |
| HF curl MITM | med | Residual — prefer hub CLI + pin |
| No secrets on BGE path | — | Pass |

---

## Round 11 — Concurrency & hot path

| Finding | Sev | Status |
|---------|-----|--------|
| Clone + pooled client; concurrent-safe | — | Pass |
| 60s timeout long for hot path | low | Residual — acceptable for cold model |
| Lock across embed await | med | **T7.2 concern** — not T7.0 client bug |

---

## Round 12 — Demo ops (T7.0 surface)

| Finding | Sev | Status |
|---------|-----|--------|
| GGUF + llama-server pre-warm required | high for demo | Residual ops checklist |
| Live paraphrase on real BGE | high for hybrid quality | Partial — calibration in PHASE-7; not a T7.0 unit fail |

---

## Round 13 — Supply chain

| Finding | Sev | Status |
|---------|-----|--------|
| reqwest rustls | — | Pass |
| httpmock dev-only | — | Pass |
| Community GGUF trust | med | Residual — pin revision |
| llama.cpp API skew | low | OpenAI-compatible mitigates |

---

## Round 14 — Tests (T7.0)

| Finding | Sev | Status |
|---------|-----|--------|
| httpmock unit suite | — | Pass (`cargo test embed` — 22 passed, 1 ignored) |
| Factory parse / bedrock stub / fixture | — | Pass |
| No T7.2/T7.3 tests | — | N/A for T7.0 close |

---

## Round 15 — Task honesty

| Finding | Sev | Status |
|---------|-----|--------|
| T7.0 marked done with live smoke pending | med | **Accept residual** — unit done; ops smoke optional evidence |
| PHASE-7 requires graph | med | **Closed** — no T0.4 on phase for default path |
| This review titled “P7” overstated scope | med | **Closed** — renamed to **T7.0** (this file) |

---

## Disposition summary (checked 2026-08-11)

| Category | Count (approx) |
|----------|----------------|
| Pass / closed | Client correctness, dim fail-fast, env split, `--embedding`, phase requires, timeouts, normalize |
| Residual ops | HF revision pin, curl checksum, live smoke evidence, pre-warm demo |
| Deferred to later tasks | Model-mix guard (T7.2/T8), health at Memory build (T8), hybrid (T7.2), Cockroach vectors (T7.3), Bedrock (T7.1) |

**Close-out decision for T7.0:**  
**ACCEPT — residual ops only.** No further code blockers identified for this task.

---

## Priority residuals (do not reopen T7.0)

1. Pin HF GGUF repo + revision (+ optional SHA) in Handoff when demo freezes.  
2. One live smoke evidence file under `dev-diary/evidence/` when convenient.  
3. Downstream: T7.2 hybrid, T7.3 vector store, T7.1 Bedrock, session embedder id.

---

## What is good (do not thrash)

- `Embedder` trait + factory  
- Timeouts, error classification, L2 normalize, dim check, model-id retry  
- Scripts for fetch + server with `--embedding`  
- Fixture path for CI  
- Docs: portable embeddings, env example  

---

## Gate commands

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test embed
# optional live:
# ./scripts/fetch-bge-m3.sh && ./scripts/run-llama-embed.sh --check
```

---

## Verdict table

| Deliverable | Verdict |
|-------------|---------|
| **T7.0 code + scripts** | **ACCEPT** (residual ops) |
| T7.1 / T7.2 / T7.3 | Not covered by this close-out |

---

## File index

| File | Role |
|------|------|
| **`adve-review-t70-embeddings.md`** | **Main T7.0 adversarial review (this file)** |
| `adve-review-p7-t7-0-embedder-self.md` | Earlier T7.0-only review + remediations |
| `adve-review-p0-p1-foundation-self.md` | Foundation review (non-T7.0) |
