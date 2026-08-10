# Adversarial review — **P7 Embeddings** (main)

**Scope:** P7 design + implementation surface:

| Area | State |
|------|--------|
| T7.0 BGE-M3 / llama.cpp factory + client | **Implemented** (`src/embed/{mod,bge_m3}.rs`, scripts) |
| T7.1 BedrockEmbedder | **Not started** (explicit `Unavailable`) |
| T7.2 Hybrid matching | **Not started** (`src/graph/hybrid.rs` missing) |
| T7.3 Cockroach `vector_candidates` | **Not started** (spike only) |
| Docs | `notes/embeddings-portable.md`, PHASE-7, `.env.example` |
| Related | T0.3 VECTOR spike, FixtureEmbedder, Cockroach `VECTOR(1024)` |

**Date:** 2026-08-11  
**Method:** 15 hostile rounds on contracts, security, ops, demo risk, and incomplete seams.  
**Related self-reviews (prior / narrower):**

- [`adve-review-p0-p1-foundation-self.md`](adve-review-p0-p1-foundation-self.md) — P0/P1 foundation  
- [`adve-review-p7-t7-0-embedder-self.md`](adve-review-p7-t7-0-embedder-self.md) — T7.0-only remediations  

**Verdict:** **CONDITIONAL ACCEPT on T7.0; P7 phase not shippable until T7.2 + T7.3 land.**  
T7.0 is solid enough to build on. Phase exit criteria are **not** met. Highest risks are **model-space mix**, **ops brittleness of GGUF/llama.cpp**, and **hybrid/vector store still unimplemented**.

---

## Executive summary

| Question | Answer |
|----------|--------|
| Is the portable-embedder design sound? | **Yes** — trait + factory + 1024-d contract + L2 normalize |
| Is T7.0 production-ready for demo? | **Almost** — code path OK; **live smoke still pending** (weights/server ops) |
| Can we claim hybrid memory today? | **No** — T7.2 / T7.3 missing |
| Bedrock swap-in ready? | **No** — stub only; account still `NOT_AUTHORIZED` |
| Keyword-only as product? | **Rejected** by product decision; Canonical fallback only when embed fails |

---

## Round 1 — Spec vs product decision

| Finding | Sev | Status |
|---------|-----|--------|
| Spec §6.3 / §12 names Bedrock Titan as the AWS embed path; product default is now BGE-M3 | **med** | **Accept with docs:** README + PHASE-7 + portable note state Bedrock as swap-in. Judging narrative must name both Cockroach vector index **and** local BGE-M3 honestly. |
| Spec degradation “keyword-only” vs product “canonical fallback, not keyword-as-product” | low | Documented in PHASE-7; implement T7.2 carefully so logs say “canonical” not “keyword mode is fine.” |
| P7 `requires: T0.4` still in phase header while T0.4 is account-blocked | **med** | **Should fix phase metadata:** T7.0 must not hard-require T0.4; only T7.1 does. |

**Action:** update PHASE-7 `requires` to `[P1, T0.3]` at phase level; keep T0.4 on T7.1 only.

---

## Round 2 — Dimension & schema contract

| Finding | Sev | Status |
|---------|-----|--------|
| BGE-M3 dense 1024 matches Cockroach `VECTOR(1024)` and T0.3 spike | — | Pass |
| `LAMBO_EMBED_DIM` can be set ≠ 1024 while schema is fixed at 1024 | **high** | **Open:** factory accepts any `dim > 0` for BGE; store/schema will diverge. Prefer fail-fast if `dim != 1024` unless migration exists, or document single allowed dim for v0.1. |
| FixtureEmbedder hard-requires 1024; BGE path does not | **med** | Align: default fail if dim ≠ 1024 for v0.1, or tag session with embedder+dim. |
| GGUF community builds may emit unexpected dims | **med** | Client checks `vec.len() == self.dim` — good. Ops must pin HF file that is truly 1024-d dense. |

---

## Round 3 — Model-space poisoning (critical product risk)

| Finding | Sev | Status |
|---------|-----|--------|
| Same dim from BGE-M3 and Titan are **not** interchangeable | **high** | Documented in portable note; **no runtime guard**. |
| Switching `LAMBO_EMBEDDER` mid-session without re-embed corrupts hybrid | **high** | **Open (T7.2/T8):** record `embedder_kind` + model id on session or stats; refuse hybrid if mismatch. |
| Empty graph then switch backend is OK | — | Pass if documented |

---

## Round 4 — T7.0 HTTP client (code)

| Finding | Sev | Status |
|---------|-----|--------|
| Timeouts (connect 5s / request 60s) | — | Pass (T7.0-self fixed) |
| Connect → `Unavailable` vs HTTP error → `Backend` | — | Pass |
| Empty text → `Unavailable` | — | Pass |
| L2 normalize; reject NaN/Inf/zero-norm | — | Pass |
| Dim mismatch rejected | — | Pass |
| 400 + model id → single retry without model | — | Pass (no unbounded loop) |
| OpenAI-compatible `/v1/embeddings` only | low | Residual: other llama.cpp endpoints ignored — OK for v0.1 |
| Double-normalize if llama.cpp already unit-normalizes | low | Residual: harmless (still unit) |

---

## Round 5 — Factory / env surface

| Finding | Sev | Status |
|---------|-----|--------|
| Default `LAMBO_EMBEDDER` empty → BgeM3 | — | Pass |
| Bedrock returns clear `Unavailable` | — | Pass |
| Default URL `http://127.0.0.1:8080` if unset | low | Residual: silent default can surprise; log at Memory build time |
| `LAMBO_BGE_M3_MODEL` dual-use (GGUF path in script vs model id in HTTP) | **med** | **Open:** env collision risk. Scripts use path; client uses model **id**. `.env.example` should split `LAMBO_LLAMA_MODEL` (id) vs `LAMBO_BGE_M3_MODEL` (path) and stop dual-feeding both into the same field without docs. |
| No validation that server is up at process start | **med** | Residual: T8 Memory assembly should call `check_health()` once and fail loud or disable VECTOR_SEARCH |

---

## Round 6 — Ops scripts (HF + llama.cpp)

| Finding | Sev | Status |
|---------|-----|--------|
| `fetch-bge-m3.sh` default `gpustack/bge-m3-GGUF` / `bge-m3-f16.gguf` — community, not BAAI official GGUF | **med** | **Must pin revision** in Handoff for reproducibility; verify dim/quality offline once. |
| curl fallback has no checksum | **med** | Residual: prefer huggingface-cli; document SHA when locking revision |
| `run-llama-embed.sh` exits 0 if server already up — good for re-entry | — | Pass |
| Port parse from URL is naive (`URL##*:`) | low | Residual: breaks on IPv6 / userinfo; local default is fine |
| No `--embedding` flag visible in truncated script read — confirm file ends with `--embedding` | **high** if missing | **Verify on ship:** without `--embedding`, `/v1/embeddings` may 404 |
| Context `-c 8192` may OOM on small machines | low | Residual: document lower `LAMBO_LLAMA_EMBED_CTX` for laptops |

---

## Round 7 — FixtureEmbedder vs live BGE-M3

| Finding | Sev | Status |
|---------|-----|--------|
| Near-pair is **hand-seeded family**, not real semantics | **med** | T7.2 tests with Fixture prove *wiring*, not retrieval quality. Need one live BGE-M3 paraphrase smoke for demo confidence. |
| DefaultHasher not stable across rustc | low | Documented; geometry tests OK |
| Fixture rejects non-1024; good for CI | — | Pass |

---

## Round 8 — Hybrid matching (T7.2) — not implemented

| Finding | Sev | Status |
|---------|-----|--------|
| No `src/graph/hybrid.rs` | **high** | **Blocks phase exit** |
| Threshold 0.85 is cosine-oriented; store may rank by L2 `<->` | **high** | **Design debt:** after L2 normalize, order by L2 ≈ order by cosine; threshold must be applied on **cosine** (or equivalent) in hybrid code, not raw L2 distance without conversion. Spec threshold is similarity 0.85. |
| Embed failure must not fail `derive()` write | **high** | Spec/v0.1: fail hybrid match step only; create new concept. Enforce in T7.2. |
| Capability absent → Canonical byte-identical | **med** | Required done-when; needs test against MemoryStore |
| Log-once fallback | low | Easy to get wrong under multi-thread; use `Once` / atomic |

---

## Round 9 — Cockroach vector path (T7.3) — not implemented

| Finding | Sev | Status |
|---------|-----|--------|
| T0.3 proved pure `ORDER BY embedding <-> $1 LIMIT k` uses index; filtered by session_id may not | **high** | T7.3 must choose: global k then filter session, or accept planner shape; capture EXPLAIN in evidence |
| Flush must write embedding column | **high** | Not in MemoryStore path; Cockroach adapter not built |
| sqlx rustls needs `sslrootcert` rewrite (T0.3) | **med** | Must not re-lose in T3.2/T7.3 |
| Empty embedding vs NULL | low | Prefer NULL until first hybrid match |

---

## Round 10 — Security

| Finding | Sev | Status |
|---------|-----|--------|
| llama.cpp bound to 127.0.0.1 by default | — | Pass if script keeps host local |
| Binding `0.0.0.0` would expose embed API | **high** | Document: never expose embed server publicly without auth |
| HF download via curl is MITM-sensitive without pin | **med** | Prefer hub CLI + revision pin |
| No secrets in BGE path | — | Pass |
| Bedrock keys in `.env` | low | Existing secret hygiene |

---

## Round 11 — Concurrency & hot path

| Finding | Sev | Status |
|---------|-----|--------|
| `BgeM3LlamaCppEmbedder` Clone + pooled client; safe concurrent embed | — | Pass |
| Hybrid will call embed under derive — latency ~ms–100ms | **med** | Must not hold graph `RwLock` across embed await (spec §6.4) — T7.2 / graph owner |
| 60s request timeout is long for hot path | low | Acceptable for cold model; consider 10–15s for demo snappiness |

---

## Round 12 — Demo / hackathon judging

| Finding | Sev | Status |
|---------|-----|--------|
| §12.1 Cockroach vector index still required | **high** | T7.3 + EXPLAIN evidence mandatory |
| AWS Bedrock optional if honest about BGE-M3 | **med** | Keep Bedrock spike/story as “intended / blocked account”; show BGE-M3 working |
| Demo machine must have GGUF + llama-server pre-warmed | **high** | Ops checklist; cold start kills video timing |
| Paraphrase pair for live demo must be validated on **actual BGE-M3**, not Fixture | **high** | One offline script: embed “register user” vs “create account” vs FAR |

---

## Round 13 — Dependency & supply chain

| Finding | Sev | Status |
|---------|-----|--------|
| `reqwest` + rustls consistent with sqlx | — | Pass |
| `httpmock` dev-only | — | Pass |
| Community GGUF repo is a supply-chain trust decision | **med** | Pin revision; consider converting from `BAAI/bge-m3` yourself for trust |
| llama.cpp binary version skew vs API | low | OpenAI-compatible surface mitigates |

---

## Round 14 — Test completeness

| Finding | Sev | Status |
|---------|-----|--------|
| T7.0 unit tests with httpmock — solid | — | Pass (see T7.0-self) |
| No live integration test gated by env | **med** | Add `#[ignore]` or feature `live-embed` smoke |
| No T7.2 / T7.3 tests | **high** | Phase incomplete |
| Factory tests cover parse / bedrock stub / fixture | — | Pass |

---

## Round 15 — Incompleteness honesty & phase gate

| Finding | Sev | Status |
|---------|-----|--------|
| PHASE-7 marks T7.0 `done` but live smoke “pending” | **med** | Either demote T7.0 to `done-unit` or complete ops smoke and attach evidence |
| Duplicate “Handoff Log” headers in PHASE-7 | low | Doc cleanup |
| Phase still lists `requires: T0.4` at top | **med** | Fix requires graph |
| Cannot ship P7 without T7.2+T7.3 | **high** | Correct phase status: **in progress**, not complete |

---

## Priority remediation backlog (ordered)

1. **T7.2** hybrid matching with cosine ≥ 0.85, no lock across embed await, Canonical fallback on `Unavailable`/`Backend`, Fixture near/far tests.  
2. **T7.3** Cockroach embed write + vector candidates + EXPLAIN evidence (session filter strategy explicit).  
3. **Dim / model identity guard** — refuse hybrid if session embedder_id ≠ process config.  
4. **Pin HF GGUF revision** + one live BGE paraphrase smoke in evidence.  
5. **Clarify env:** `LAMBO_BGE_M3_MODEL` = filesystem path; `LAMBO_LLAMA_MODEL` = request model id only.  
6. **Confirm `run-llama-embed.sh` passes `--embedding`.**  
7. **T7.1** when AWS unlocks — optional for demo if BGE path is primary.  
8. Fix PHASE-7 requires / status honesty.

---

## What is already good (do not thrash)

- Portable `Embedder` trait and factory  
- T7.0 client: timeouts, normalize, dim check, error split, model-id retry  
- 1024-d alignment with Cockroach  
- Docs: `embeddings-portable.md`, Bedrock blocker separate  
- FixtureEmbedder for CI  

---

## Gate commands

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
# optional live:
# ./scripts/fetch-bge-m3.sh && ./scripts/run-llama-embed.sh --check
# LAMBO_EMBEDDER=bge_m3 cargo test -- --ignored  # when live tests exist
```

---

## Verdict table

| Deliverable | Verdict |
|-------------|---------|
| T7.0 code | **ACCEPT** (with residual ops/env notes) |
| T7.1 | **REJECT until implemented + AWS auth** |
| T7.2 | **REJECT until implemented** |
| T7.3 | **REJECT until implemented + EXPLAIN evidence** |
| **P7 overall** | **NOT DONE** — continue after T7.2/T7.3; T7.0 is a solid foundation |

---

## File index

| File | Role |
|------|------|
| **`adve-review-p7-embeddings.md`** | **Main adversarial review (this file)** |
| `adve-review-p7-t7-0-embedder-self.md` | Prior T7.0-only 15-round review + remediations |
| `adve-review-p0-p1-foundation-self.md` | Prior foundation review (non-P7) |
