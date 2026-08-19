# lambo-for-mooshik

What Mooshik needs from Lambo, as atomic tasks with their real dependencies.

**Branch:** `lambo-for-mooshik`. **Merges to main:** after 2026-09-15, when Lambo's judging
window closes. `main` does not move before then.

**Companion docs:** `~/work/mooshik/scratch/PRODUCT_SPEC.md` is the authority on what Mooshik is.
`~/work/mooshik/scratch/hackathon.md` §7 is where these four changes were first written down.
Where this folder and the spec disagree about Mooshik, the spec wins.

This folder does not follow the `PHASE-N` convention of the rest of `dev-diary/`. That convention
belonged to a swarm building one system to a frozen spec; this is one person adding four features
to a system that already exists.

---

## Why these four

All four are Mooshik requirements, not hackathon inventions. Mooshik's autobiography spans two
machines, so it needs a hosted embedder (A) and a real shared store (B). Its canonization runs
with one human and no peer agents, so the swarm promotion policy does not apply (C). And it is
bootstrapped from a decade of history in one sitting, which breaks every time-based gate Lambo
has (D).

---

## Documents

| Doc | Covers |
| --- | --- |
| [A — Gemini embedder](A-gemini-embedder.md) | `embed-gemini`, registry wiring, config keys, the adapter, the dim guard |
| [B — Postgres-family store](B-postgres-store.md) | `pg` base extracted from the Cockroach adapter, `postgres` + `cockroach` as dialects; clean alias split, templated width, hnsw from init. Redesigned 2026-08-19 from copy-then-edit to extract-then-extend |
| [C — SoloPolicy](C-solopolicy.md) | the `PromotionScorer` seam, the solo formula, eviction resistance |
| [D — Event-time clock](D-event-clock.md) | event time vs ingest time, the gates it unblocks, the fallback |
| [F — SQLite vectors](F-sqlite-vectors.md) | issue #5, the query path, the fail-closed capability trap |
| [G — Recall calibration](G-recall-calibration.md) | `RECENT_SCORE` floor and `semantic_match_threshold` vs real-embedder score bands; found by F's BGE-M3 evidence run |
| [H — Cross-store parity](H-cross-store-parity.md) | one live parity harness: closes F's deferred Cockroach box, becomes B3's parity criterion for pgvector. Live legs need a DSN-bearing machine or post-merge CI |
| [I — Observability](I-observability.md) | serve call ledger, heartbeats, analysis kit — makes DOGFOOD's metrics measurable from artifacts. **Runs before further implementation cycles** (decided 2026-08-19): every cycle before I is dogfood data lost |

E (consumption from Mooshik) stays in this file — it is two lines of manifest, not a workstream.

[DOGFOOD.md](DOGFOOD.md) is not a workstream either: it is the proposal for running the
branch's own development on a live Lambo session (pinned binary, store outside the repo,
recall-before-workstream). Design only until someone runs it; its open questions get
answered in that file.

---

## Task graph

```
T0 ─→ everything

A1 ─→ A2 ─→ A3 ─→ A4
B0 ─→ B1 ─→ B2 ─→ B3
            B2 ─→ B4
H1 ⇢ B0               (soft: the parity harness is the extraction's behavioural lock)
D1 ─→ D2 ─→ C2
C1 ─────────↗
F1 ─→ F2
F2 ─→ G1 ─→ G2
F2 ─→ H1 ─→ H2        (H2 needs a DSN-bearing machine or post-merge CI)
I1 ─→ I2 ─→ I3        (I first among remaining starts, by decision — feeds DOGFOOD's metrics)
(H1, B3) ─→ H3
(A4, B4, F2) ─→ E1 ─→ E2
```

Nothing in A blocks anything in B. C2 is the one real join: SoloPolicy cannot be evaluated
honestly until event time exists, because its recurrence rule is defined over wall-clock
separation that a bulk ingest does not have.

F is independent of B but shares its "width comes from config" decision, so the two should be
designed together even though they can be built apart.

G was not in the original four: it is the first finding out of F's real-embedder evidence
run (a correct cosine ranking erased by recall's flat `RECENT_SCORE` floor, plus the same
fixture-calibrated geometry behind `semantic_match_threshold`). It needs F's vector leg to
measure against, hence F2 → G1. C2 consumes recall behaviour, so whichever of C2 and G2
lands second re-checks the other.

---

## T0 — CI, before anything else

CI is scoped to this branch first, so nothing else lands against jobs that are wrong for it.

**Done:** `lambo-for-mooshik` added to the push trigger (the rename off `phase/**` had left the
branch with no push CI), and `cockroach-live` gated off with
`if: github.ref != 'refs/heads/lambo-for-mooshik'`.

**Why gate rather than delete.** Merging this branch after 2026-09-15 would carry a gutted
`ci.yml` *into* main, stripping main's CI unless someone remembered to restore it first. A branch
condition merges back with its behaviour intact and needs no restore step. This applies to every
CI change made here: **condition it, never delete it.**

**Why only that job.** `cockroach-live` is the only job that connects to a cluster. Branch pushes
in this repo do receive secrets, so it would have run `LAMBO_COCKROACH_DSN` against the live
cluster — and `LAMBO_REQUIRE_LIVE=1` means a missing DSN is a hard failure, not a skip, so it
cannot be left to no-op itself. Nothing on this branch changes Cockroach's live behaviour: A is a
new embedder, B is a dialect split, C and D are canonization, F is SQLite.

**Kept deliberately:**

| Job / row | Why it stays |
| --- | --- |
| `check` (fmt, clippy, `cargo test --all --features fixtures`) | The default path. Touches no adapter and catches every contract regression in C and D. |
| `sqlite`, `sqlite-minimal` | F changes SQLite directly. These are the rows most likely to catch a real regression this month. |
| `cockroach` matrix row | Hits **no database** — live tests are `#[ignore]`d. It is the compile-and-unit guard on the adapter B0 extracts the `pg` family base from, under `-D warnings`. Deleting it would let the code rot while we extract it. |
| `minimal`, `demo` | Cheap `cargo check` rows guarding feature-combination breakage, which B1's new `StoreKind` variant can cause. |

### What each workstream adds back

CI grows with the branch rather than arriving at the end. Each of these is additive, so all of
them merge to main cleanly:

| Workstream | CI it must add |
| --- | --- |
| A | `embed-gemini` matrix row (compile + unit, no network). Live Vertex calls stay `#[ignore]`d — no API key in CI. |
| B | `store-postgres` matrix row, **and** a `postgres-live` job. Use a GitHub Actions **service container** (`pgvector/pgvector`), not a provisioned cloud cluster: no secret, no cost, no cross-account setup, and it runs on every push instead of being a tier you hope someone checks. This is strictly better than the Cockroach live model and worth saying so. |
| C | Nothing new — canonization runs in `check`. |
| D | Nothing new, but the event clock is what finally lets time-dependent tests be deterministic. See issue #2. |
| F | A row proving the vector leg **fired** on SQLite rather than inferring it from rank, per issue #5's acceptance criteria. |

**At merge:** delete the two `if:` conditions and the trigger entry. That is the whole restore.

---

## A. Gemini embedder (`embed-gemini`)

**Why:** cross-machine sync means both machines write into the same embedding space or the
`EmbeddingContract` on the snapshot is violated and recall silently degrades. BGE-M3 under
llama.cpp needs a local GPU; the desktop has a 4070 and the MacBook does not. A hosted embedder is
the only correct answer for a two-machine autobiography.

**Template:** the `embed-bedrock` arm for registry wiring, `src/embed/bge_m3.rs` for adapter shape
(it is the other HTTP embedder, so its error classification and normalization carry over).

### A1 — Registry wiring
`EmbedderKind::Gemini` plus its six touch points in `src/embed/mod.rs`: `feature_name`,
`is_compiled`, `is_ready`, `FromStr` (with aliases), `Display`, and the `build_embedder` arm.
Cargo feature `embed-gemini`. **Depends on:** nothing.

Note `is_ready` is deliberately separate from `is_compiled` — `Bedrock` returns `false` there
because the feature exists and the adapter does not. Gemini flips to `true` only at A3.

### A2 — Config keys
Gemini needs project, location, model and credential source. `EmbedderConfig` is
`#[serde(deny_unknown_fields)]`, so the keys must be declared, not just read. Mirror the existing
`llama_url` / `llama_model` pattern and extend `overlay_env` with the matching `LAMBO_*` vars.
**Depends on:** A1.

### A3 — The adapter
`src/embed/gemini.rs`. Vertex `gemini-embedding-001`, `outputDimensionality` from config.
Contracts it must honour, all inherited and all tested elsewhere in the tree:

* **CON-7** — reject empty/whitespace input with `Unavailable`. No backend may embed the empty
  string.
* **CON-2** — no retry that silently changes the request. A retry that drops or alters the model
  embeds in a different space, which passes the only runtime check (dim) and is therefore
  undetectable.
* **Error classification** — connect-level failure is `Unavailable` (caller degrades to canonical
  matching); server rejection, malformed body, or wrong width is `Backend`.
* **L2-normalize before returning**, rejecting non-finite and zero-norm vectors.

**Depends on:** A2.

### A4 — Dim guard
`gemini-embedding-001` truncates to 768, 1536 or 3072 only. Reject any other configured dim at
construction rather than sending an unsupported `outputDimensionality`. **Depends on:** A3.

### Decided: dim stays configurable
`[embedder] dim` is already a TOML key (`src/embed/mod.rs:147`), and
`resolve::check_vector_compatibility` already hard-errors when the embedder's width disagrees
with the store's. So "the dim must match the embedder" is enforced today; nothing to build.

**But configurable is late-binding, not reversible.** Once the bootstrap embeds at N, changing the
TOML means a full re-embed, because the store schema is the authority and the compatibility check
refuses to start against a store of a different width. The choice is made once, before the ingest.

---

## B. Postgres store (`store-postgres`)

**Why:** the unified cross-machine store in spec §3.3. Today `StoreKind::from_str`
(`src/store/mod.rs:436`) maps `"postgres"` and `"pg"` onto `Cockroach`, which is not true: the
Cockroach adapter emits `VECTOR(1024)`, `CREATE VECTOR INDEX` and `::STRING` casts that Postgres
does not have.

**No new dependency.** `store-cockroach` is already `["dep:sqlx", "sqlx/postgres"]`, so sqlx's
Postgres driver compiles in today. This is a dialect split, not a second 4,900-line adapter.

### B1 — `StoreKind::Postgres`
New variant, feature `store-postgres`, and **remap the aliases**: `"postgres"` and `"pg"` stop
resolving to `Cockroach`.

**This is a behaviour change with existing tests asserting the old mapping**
(`src/store/mod.rs:636` and `:658`). Any deployment with `kind = "postgres"` pointed at a
CockroachDB cluster changes meaning silently. Decide and write down whether `"crdb"` /
`"cockroach"` remain the only Cockroach spellings. **Depends on:** nothing.

### B2 — Migration with templated width
`migrations/postgres/001_init.sql`, pgvector, `VECTOR(n)` width from config.

**The Cockroach pattern cannot be copied here.** Cockroach's DDL is `include_str!`'d as a static
and `schema_vector_dim` (`src/store/cockroach.rs:868`) parses the width back *out* of that string.
A configurable width has to be substituted *into* the SQL before execution, which inverts the
direction the data flows. This is the part of B that is design work rather than transcription.
**Depends on:** B1.

### B3 — Dialect split
`INIT_SQL`, the `::VECTOR` casts, the `::STRING` casts, and the distance operator: Cockroach `<->`
is L2 with score `1 - d²/2`; pgvector `<=>` is cosine distance, so the score is `1 - d`. Getting
this wrong does not fail, it just ranks wrongly. **Depends on:** B2.

### B4 — `vector_dimensions()` from config
Report the configured width rather than a DDL parse, so `check_vector_compatibility` still has an
authority to check against. **Depends on:** B2.

---

## C. SoloPolicy

**Why:** spec §3.2. Lambo's canonization assumes independent multi-agent convergence. Mooshik has
one human and no peers, so nothing converges and nothing is ever promoted.

### C1 — `PromotionScorer` seam
A selectable scorer in canonization so the swarm policy and the solo policy both exist, chosen by
`promotion_policy` in config. There is no such seam today — `daemon::score` and the three-stage
pipeline in `src/canon/` are the only path. **Depends on:** nothing.

### C2 — SoloPolicy + eviction resistance
The score from spec §3.2 with its thresholds (10 / 6 / 3), plus the concept-type eviction
resistance multipliers (Constraint 1.5, Entity 1.2, Logic 1.1, Resource 1.0, Observation 0.7).

**Depends on:** C1 **and** D2. Its recurrence rule wants three or more distinct sessions separated
by at least 24 hours, which a bulk ingest cannot produce in ingest time. Implementing C2 against
wall-clock time and testing it against a bootstrap would produce a policy that promotes nothing
and a test that proves it works.

---

## D. Event-time clock

**Why:** the most substantial change here and the most reusable afterwards, since any
historical-corpus ingest needs it. Ten years of history arriving in ninety minutes has no temporal
separation at all: Lambo's gates measure ingest time, the venerable gate ignores edges younger
than 60 seconds, and supporting edges must span at least 0.3 of the session's temporal extent.
Every one of those reads as "everything happened at once."

### D1 — Injectable clock
The time a fact is *about* — commit date, transcript timestamp — carried alongside the time it was
flushed. **Depends on:** nothing.

### D2 — Gates read event time
The 60-second edge-age floor, the 0.3 temporal-extent rule, and session separation all move onto
the injected clock. **Depends on:** D1.

**Decision point, end of day 2, not day 4.** If D is larger than it looks, the fallback is to
shuffle the ingestion queue so temporal spread proxies for source diversity. Cheaper, weaker, and
worth measuring against the real fix. Taking the fallback changes what C2 can honestly claim.

---

## F. SQLite vector search (issue #5)

**Why this is not optional.** Spec §3.3 makes `SqliteStore` the local single-machine backend, and
the trimmed tool surface in §3.1 exposes `lambo_recall(query, strategy?: "Canonical" | "Hybrid")`.
But `SqliteStore` reports `Capabilities::empty()` (`src/store/sqlite.rs:565`), so hybrid matching
degrades to keyword-only:

```
hybrid matching disabled: store lacks VECTOR_SEARCH — degrading to
MatchStrategy::Canonical (creating keyword-only concept)
```

So Mooshik's default local mode — the `$0 / 0ms`, offline, always-on posture that the whole
"Coworking vs Commanding" table in §1.3 rests on — has **no semantic recall at all** today.
Cockroach is currently the only adapter advertising `VECTOR_SEARCH`. That makes the spec's
primary focus ("memory recall") a property of the cloud tier, which inverts the product.

**Why it is smaller than it looks** (from issue #5): the embeddings are already written to
`concepts.embedding BLOB` on every flush for round-trip parity, the session's embedding contract
is already persisted, no migration is required, `crate::embed::cosine` already exists, and
`VectorSearchStore` (`src/memory.rs:3204`) is already the reference shape — MemoryStore plus exact
cosine over flushed embeddings. Only the query path is missing.

### F1 — `vector_candidates` / `vector_candidates_checked`
Load the session's non-null embeddings, score with `crate::embed::cosine`, sort, return top-k as
`Scored<NodeId>`. The checked path must refuse a kind/model/dim mismatch the way Cockroach does.
Exact scan, not an index: Lambo is session-scoped so n stays small by construction, and
`sqlite-vec` costs a C toolchain dependency across four cross-compiled targets. Revisit on a
measured latency number, not a guess. **Depends on:** nothing.

### F2 — Advertise the capability
Flip `capabilities()` to `VECTOR_SEARCH` **and** implement `vector_dimensions()` in the same
change: `resolve.rs:68` refuses a store that claims the capability without reporting a concrete
width, and a test already asserts that refusal (`resolve.rs:413`). **Depends on:** F1.

**Amendment to the issue as filed.** Issue #5 says `vector_dimensions() -> Some(1024)`. Do not do
that. It would introduce a *third* hardcoded width, alongside `VECTOR(1024)` in the Cockroach
migration and the 1024 default in `EmbedderConfig`, at exactly the moment B is removing the
hardcoding. SQLite's width must come from the persisted session contract
(`sessions.embedding_dim`, already stored) or from config, matching whatever B2 settles on.

---

## E. Consumption from Mooshik

### E1 — Path dep during the build
`lambo = { path = "../lambo", default-features = false, features = [...] }`.
`default-features = false` matters: the default set pulls `embed-bge`, which pulls reqwest and
assumes a local llama.cpp server. Mooshik also needs `rust-toolchain.toml` at 1.97.1 to match.
**Depends on:** A4, B4, F2.

### E2 — Rev-pinned git dep before submitting
`{ git = "https://github.com/nrynss/lambo", rev = "<sha>" }`. **Pin `rev`, never `branch`** — a
branch that moves after submission means a judge builds a different Lambo than the one in the
video. **Depends on:** E1.

**Not crates.io.** A dated 0.3.0 release during the judging window is far more visible than a
branch, and the README links crates.io. Release after 2026-09-15.

---

## Open issues: absorbed and not

| Issue | Verdict |
| --- | --- |
| **#5** SQLite vector search | **Absorbed as F.** Load-bearing, not an enhancement — without it Mooshik's default local mode has no semantic recall. Amend the hardcoded 1024. |
| **#2** Recall tie-breaks on random UUIDs | **Not absorbed, but watch D.** The tie-break itself is latent and cosmetic. The interesting line is the note that the real `binary_parity` instability was *time-derived `recency` in the daemon score varying between runs*. D1 is precisely the seam that would let recency be pinned. If D lands, re-read #2 before closing anything. |
| **#3** Bedrock embedder | **Not absorbed.** Blocked on an AWS use-case form, and A supersedes its role for Mooshik: a hosted embedder for the two-machine case. Two overlaps to respect: A1 touches the same `is_ready` / "not implemented yet" message and its test, so do not break #3's landing pad; and #3's rule — *"session identity is the `EmbeddingContract`"*, so switching embedder means re-embedding, not a config flip — is the same late-binding point A records about dim. |
| **#4** serve-web single session | **Not absorbed.** Multi-session is a product surface Mooshik does not need this month. But it surfaces a question the spec has not answered (below). |

### One question #4 raises that the spec has not settled

#4 documents that the store contract has `load_session(&SessionId)` and **no discovery operation
at all** — no adapter can answer "which sessions exist."

Spec §1.3 claims a *Global Workspace* scope of `~/work/*` with cross-repo awareness. If Mooshik
holds **one** session — a single unified autobiography, which is how §3.3 reads and how the
bootstrap ingest is designed — then discovery is never needed and #4 stays irrelevant. If it holds
**one session per project**, Mooshik needs an enumeration surface that no store can currently
provide, and #4 stops being a portal nicety and becomes a blocker.

Nothing in this month's build forces the answer, because the bootstrap produces one graph. Worth
deciding before Phase 2 rather than discovering it.

---

## What we expect to learn

Worth writing down now, so the answers are not retrofitted later:

* Whether the canonization constants (blast radius > 5, three GC sweeps, 0.3 temporal extent) mean
  anything outside agent-session scale. They were tuned for a session, not a decade.
* Single-writer throughput under bulk load. Untested at this volume, and probably the first real
  finding.
* Whether SoloPolicy scores sensibly when most facts carry no human confirmation and no action
  outcomes, which is the actual bootstrap condition.
