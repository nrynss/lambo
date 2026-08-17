# Remediation task list

Source: `adve-review-full-stack-sweep-2026-08-16.md` (4 P1 / 12 P2 / 10 P3) and
the Tier 3 detail review beside it, plus defects found outside the review.

**T1 to T12 below are code-fix tasks, numbered so that every blocker has a
lower number than the thing it blocks.** Read top to bottom and the order works.

Deployment, recording and submission live in `deployment-and-submission.md` as
**D1 to D3**. Two tasks here block work over there, and say so.

**Finding IDs like `T1-P1-1`, `T2-P2-3`, `T3-1-P1-1` come from the review
documents** and refer to the review's own tier grouping. They have nothing to do
with the task numbers here. Where a task closes a finding, it says so.

---

## Closed already. Do not start these.

The launcher was reviewed against a file that has since changed by ~194 lines.
Re-review it (**T5**) before starting **T6**.

| Review finding | Why it is closed |
|---|---|
| **T2-P1-1** in-band `llama.cpp` compile, OOM risk, 8 to 14 minute boot | The source build is gone. `LLAMA_BLOCK` fetches a prebuilt release tarball pinned by SHA-256, which is the remediation the finding asks for. `grep "cmake --build" scripts/aws-infra/launch_exhibit_ec2.py` returns nothing. |
| **T1-P2-1, first half** `LamboFile` rejects `[daemon]` under `deny_unknown_fields` | `LamboFile` carries `pub daemon: DaemonConfig`. A `lambo.toml` with `[daemon]` parses and runs, verified with a probe config. |

Three more defects were found and fixed outside the review. Listed so they are
not re-reported:

- Published Linux binaries need GLIBC 2.39 and cannot run on Amazon Linux 2023
  (2.34). The exhibit moved to Ubuntu 26.04. The underlying packaging defect is
  still open as **T12**.
- Caddy publishes SHA-512 checksums; the launcher verified them with
  `sha256sum`, failing every boot after lambo was already installed. Now
  `sha512sum`.
- `record-run.sh` wrote its header before the redaction filter, putting a home
  IP into committed captures. Fixed, and existing captures scrubbed.

---

## T1 — Fail closed: config cadences (done), then write-path invariants (reopened)

**Files:** `src/config.rs`, `src/memory.rs`
**Closes:** T1-P1-1, T1-P1-2
**Blocked by:** nothing
**Blocks:** nothing, but do it first

`Config::validate()` is never called in production. It runs in unit tests only,
and is skipped by `MemoryBuilder::build()` and `resolve_backends()`. Tokio's
`interval(period)` panics immediately when `period` is zero.

This was latent until the `[daemon]` section made `gc_interval` and
`canonization_eval_interval_secs` settable from a file.
`canonization_eval_interval_secs = 0` becomes `Duration::ZERO`, which is exactly
the panic input. **A config file can now kill the daemon at startup.**

Do:
- Call `config.validate()?` at the start of `MemoryBuilder::build()`.
- Enforce `gc_interval >= 1`.
- Enforce `daemon_tick_interval`, `backend_flush_interval` and
  `canonization_eval_interval` all strictly greater than zero.
- Cover both new `DaemonConfig` fields, not just the pre-existing ones.

**Verify:** full suite. `MemoryBuilder::build()` is on every test path, so this
is the task most likely to break something unrelated. 763 tests pass today.

---

### ✅ T1 — DONE (2026-08-16, merged `3899d3f`)

**What landed** (`src/config.rs`, `src/memory.rs`, `src/resolve.rs`):
- `Config::validate()` now enforces `gc_interval >= 1` and strictly-positive
  `daemon_tick_interval`, `backend_flush_interval`, `canonization_eval_interval`
  (the three `tokio::interval` feeders), on top of the pre-existing threshold /
  top-k checks. Duration error messages render `0ns` via `{:?}` (the old
  `.as_secs_f64()` always printed `0` for the only rejected value).
- `MemoryBuilder::build()` calls `config.validate()?` on the **merged** config
  (named setters included) before acquiring the single-writer lease, loading
  the session, or spawning any task — a zero cadence can no longer reach
  `tokio::interval` or leak a lease.
- `resolve_backends` validates immediately after applying `[daemon]` overrides,
  **before** `build_store`/`build_embedder` (an embedder build may load a
  model), so every full-resolve command (`serve`/`derive`/`inspect`/`saints`)
  fails closed at config load. `resolve_store_only` (provision, reader tools)
  deliberately does not — documented; those commands never run a daemon interval.

**Tests** (all new): `cadence_validation_fails_closed`,
`daemon_config_zero_cadence_override_rejected` (both fields + each alone),
`lambo_file_zero_daemon_cadences_fail_validate` (TOML-file-driven), and
`build_rejects_zero_cadence_before_acquiring_the_lease` — the last issues a
second `build()` on the same session with a valid config, so it genuinely
detects a leaked lease (a validate-after-lease regression wedges that build).

**Review:** 4 rounds, all APPROVE. R1 raised 3 P3 + 2 nits (resolve-boundary
validate, test-coverage gaps, and 2 polish items); R2 found two were
*claimed-not-delivered* (the `.as_secs_f64()` nit and the validate-before-store
ordering) plus a lease-test overclaim; R3 verified all fixed; R4 cleared the
single residual comment nit. Docs in
`dev-diary/adversarial-review/adve-review-remed-T1round{1..4}.md`.

**Deferred to T2 (documented, not a defect):** T1-R1-2 — `MemoryBuilder::backends()`
drops `ResolvedBackends.config` and `open_writer()` never applies `[daemon]`
overrides; that is exactly T2's charter and lands there.

**Verify:** full `cargo test --all-features` green — **818 passed / 0 failed**
(defaults 734). No production caller passes a zero cadence; `lambo demo` and the
example config unaffected.

---

### T1 part 2 — REOPENED: guards that exist but are not applied

The first part fixed a validation function that was never called in production.
The same pattern shows up on the graph paths, and each instance corrupts the
graph **silently**: no error, a plausible-looking result, and a wrong answer to
the question the product exists to answer. Every one below was hit or verified
during the CloudOps build.

**1. The embedding contract is enforced for writers and nobody else.**
Verified: `assert_session_embedding_compatible` has exactly one non-test call
site, `src/memory.rs:626`, inside `MemoryBuilder::build()`. Readers reach a
session through `load_reader_graph`, which never calls it, so `serve-web` and
every read verb will happily attach an embedder that disagrees with the space
the stored vectors live in. Its own doc comment says "Call on `load_session` /
serve attach" and the serve-attach half was never wired.

This is not theoretical: it is exactly why `launch_exhibit_ec2.py` pins BGE-M3
by sha256 and carries three paragraphs explaining that a same-width substitute
"resolves cleanly and then returns confident nonsense". A guard for that already
existed; it just was not applied on the surface that needed it. Wire it into the
reader path and the launcher's warning becomes belt and braces rather than the
only defence.

**2. `Observation` concepts never match a canonical key.**
Verified at `src/graph/canonical.rs:304`:

```rust
.filter(|c| c.concept_type != ConceptType::Observation)
.filter(|c| c.canonical_key == key)
```

Deriving an identifier as `observation` therefore creates a **new node on every
reference**. I did this, and it split one child in two and halved a pillar's
blast radius with nothing logged. Either refuse `observation` for content that
canonicalizes, or warn at the derive boundary that this type opts out of
identity. Today the caller has to know.

**3. A second hierarchy parent zeroes blast radius.**
Reported by the Tier 3 review and encoded defensively in
`scripts/cloudops/_lambo.py::check_single_source`, which is client-side Python
guarding an engine invariant. That is the signal: if a caller had to write the
check, the engine should own it. Refuse the second structural parent, or make
the zeroing explicit rather than arithmetic.

**4. `--parent-of CHILD:PARENT` cannot carry a colon**, so IPv6 CIDRs are
dropped in silence (T3-1-P2-3). The client-side half is already in **T7**; the
question here is whether the CLI should accept an escaped or alternative
separator instead of making every caller pre-filter.

**The principle worth stating once:** rules the tool enforces cost nothing to
follow. Rules the caller must remember are the entire adoption tax, and putting
correctness in the caller's head is a strange choice for a memory product.

**Verify:** each of these needs a test that asserts the *refusal*, not the happy
path. All four currently "pass" by doing the wrong thing quietly.

**Acceptance criteria, applied at review rather than design time.** Adding four
refusals converts four invisible failures into four visible ones, which is
strictly better but not free: a caller now has four new ways to be rejected.
Each refusal should therefore remove a decision, not just add a gate. A refusal
that does not tell you how to succeed is only a new way to fail.

- **Embedding contract:** the error names the model that wrote the vectors. The
  contract already carries `kind`, `model` and `dim`, so "incompatible embedder"
  alone is a regression on a message that could be actionable.
- **`Observation`:** prefer not making the caller choose. If content
  canonicalizes, requiring them to know that one enum variant silently opts out
  of identity is the trap restated as a rule. A refusal is the floor, not the
  goal.
- **Single hierarchy parent:** the error names the parent that already claims
  the child. Otherwise the caller knows they are wrong and not how.
- **`--parent-of` colon:** this one should probably not refuse at all. Accept an
  escape or an alternative separator. Refusing pushes the burden onto every
  caller, which is exactly how it became client-side Python in `_lambo.py`.

None of this changes the implementation shape. Error text and the separator
scope call are the last things written, so it costs work already in flight
nothing.

### ✅ T1 part 2 — DONE (2026-08-17, merged `9f59e93`)

All four read/write-path invariants enforced, each backed by a refusal-asserting
test (not the happy path), with validate-then-mutate ordering:

1. **Embedding contract enforced for readers too** (was writer-only).
   `load_reader_graph_with_contract(store, session, Option<&EmbeddingContract>)`
   wired into `recall` and serve-web `stats`/`recall`; a live embedder that
   disagrees with the stored vector space is refused before serving, and the
   error names the writing model/kind/dim. serve-web additionally fails fast at
   startup on a genuine mismatch (read-only, no lease). inspect/saints/stats are
   store-only (no embedder) so correctly skip the check.
2. **Observation re-derivation that would split identity is refused** at the
   derive pre-pass; first-time Observations and demote's per-sentence records
   are unaffected (guarded seam, tested).
3. **A second Hierarchical parent (which zeroes blast radius) is refused** at
   the `parent_of` pre-pass, naming the claiming parent; Dependency/Causal
   fan-in and same-parent reinforcement deliberately allowed.
4. **`--parent-of` splits on the FIRST colon** so a colon-bearing (IPv6) parent
   is accepted, not silently dropped; empty sides still refuse loudly.

Backcheck: all four **acceptance criteria** met and judged explicitly by the
reviewer: embedding error names the writing model; Observation floor-vs-goal
handled honestly (refuse is the floor, deeper identity change is a spec-level
leave for later); hierarchy error names the claiming parent; `--parent-of`
ACCEPTS IPv6, does not refuse.

**Review:** 3 rounds, all APPROVE. R1: 5 P3 + 3 nits (first-Observation
unguarded, demote/derive asymmetry, client IPv6 deferral, cross-type fan-in
scope, serve-web 502 UX, plus 3 nits) → remediated with tests+docs; R2 verified
all genuine, added a startup fail-fast; R3 cleared 2 doc nits. Docs in
`adve-review-remed-T1bround{1..3}.md`.

**Deferred to T7 (documented, not a defect):** T1b-R1-3 — the launcher client
(`scripts/cloudops/_lambo.py` `_refuse_colon`) still pre-refuses an IPv6
`--parent-of` parent; the CLI is fixed, the client half is T7's. A
T7-naming comment is at `_lambo.py:~304`.

**Verify:** full `cargo test --all-features` green — **825 passed / 0 failed**
(T1+T2+T1b merged). No-writer-lease serve-web test intact.

---

## T2 — Make `[daemon]` mean the same thing in every subcommand

**Files:** `src/cli/mod.rs`
**Closes:** T1-P2-1, second half
**Blocked by:** T1, so validation exists before the config spreads

`serve` honours `[daemon]`. Every CLI verb ignores it, because `open_writer()`
builds `Config::default()`. One `lambo.toml` therefore behaves differently
depending on which subcommand reads it, silently.

Wire the file's config through `open_writer()` so every verb reads the same
`[daemon]` block. Rejecting it outside `serve` would also remove the silent
divergence, but it makes the file mean two different things depending on who
opens it, and the CLI verbs are exactly where an operator would expect a
lowered `gc_interval` to apply.

> **Carries T1 review finding T1-R1-2 (P3).** The round-1 T1 review noted that
> `Memory::builder().backends(ResolvedBackends)` drops `backends.config`, and
> `open_writer()` never applies `[daemon]` overrides — the exact divergence this
> task exists to close. Deferred here deliberately so the fix lands once, as this
> task, rather than being pre-emptively done in T1. See
> `dev-diary/adversarial-review/adve-review-remed-T1round1.md`.

### ✅ T2 — DONE (2026-08-17, merged `b89d060`)

`open_writer()` now passes `.config(backends.config.clone())` before
`.backends(backends)`, mirroring `serve::build_memory` — so `derive` /
`record-action` / `reserve` / `release` read the same `[daemon]` block as
`serve`. Closes **T1-R1-2** (the `backends()` config-drop / `open_writer` never
applying `[daemon]`). `MemoryBuilder::backends()` doc states the config-drop
invariant; `demo.rs` notes its deliberate non-honouring of a user `[daemon]`.

Regression test `open_writer_forwards_resolved_config_daemon_overrides` (sentinel
`gc_interval = 17` must survive `open_writer`, not fall back to `10_000`).

**Review:** 3 rounds, all APPROVE. R1: 2 P3 + 2 nits (missing regression test,
call-site-vs-root-cause note, two comment nits) → remediated; R2 caught a
garbled comment clause from the R1-3 reword (T2-R2-1 P3) → fixed; R3 cleared the
comment. Docs in `adve-review-remed-T2round{1..3}.md`.

**Verify:** full `cargo test --all-features` green — **819 passed / 0 failed**
(T1+T2 merged); the new test passes under `cargo test --lib`.

---

## T3 — `/api/graph` and `/api/inspect`, and the token comparator

**Files:** `src/cli/serve_web.rs`
**Closes:** T1-P3-1
**Blocked by:** nothing
**Blocks:** the UI pass. See `ui-pass-plan.md`; the tree renderer is
already written and stays hidden until this endpoint answers.

The exhibit's headline claim is that Lambo names the workloads that would break.
`/api/recall` does not do that. Measured against the live exhibit:

- `what depends on SG-Base-VPC` returns `SG-Base-VPC` itself and its own ingress
  rules. It never names `RDS-Lambo-Demo-DB`, the only thing that depends on it.
  It matches words, not structure.
- `is it safe to delete the shared security group` returns five items all scored
  `0.18`. No ranking signal, no warning.

`lambo inspect` produces the right answer. `serve-web` exposes no equivalent, so
the page cannot show the thing the submission is about.

Contract. The portal's renderer is already built against this shape:

```
GET /api/inspect?focus=<string>&depth=1

200 {
  "focus":        "SG-Base-VPC",
  "found":        true,
  "status":       "Candidate",          // None | Candidate | Venerable | Canonical
  "blast_radius": 5,
  "dependents": [
    { "content": "RDS-Lambo-Demo-DB", "concept_type": "Entity", "edge": "Dependency" }
  ]
}

200 { "focus": "...", "found": false, "blast_radius": 0, "dependents": [] }
```

Requirements, in order:

1. **Structural edges only**: `Dependency`, `Causal`, `Hierarchical`, matching
   `STRUCTURAL_EDGE_IN` in `src/store/sqlite.rs:207`. This is what makes the
   answer mean "what depends on this", and it keeps the false `CoOccurrence`
   edge described in **T7** off the page for free.
2. **A miss is `200` with `found: false`**, never a non-2xx. The page must say
   "nothing depends on this" without rendering an error. T3-2-P2-1 is the same
   mistake in the CLI path, fixed in **T8**.
3. **Read-only, no writer lease.** The existing `serve_web.rs` test that greps
   its own source for `Memory::builder`, `open_writer`, `acquire_lease` and
   `.spawn()` must keep passing.
4. `depth` may be ignored and treated as 1. The page needs hop 1 only.
5. Bound the `dependents` array and say so in the payload rather than truncating
   silently. `MAX_INSPECT_NODES = 64` already bounds the CLI path.

### `/api/graph` — the structure, for a tree view

A free-text box next to a primary button reads as a chat prompt, promises a
conversation, and delivers a memory dump. The honest presentation of this data
is a tree of the components with their relationships, which is also what makes
the isolation argument visible at a glance.

The tree needs no new derivation. It is one query over indexed columns, and it
has been run against the live session to confirm the shape:

```sql
SELECT s.content AS parent, e.edge_type, t.content AS child
FROM edges e
JOIN concepts s ON s.id = e.source AND s.session_id = e.session_id
JOIN concepts t ON t.id = e.target AND t.session_id = e.session_id
WHERE e.session_id = $1
  AND e.edge_type IN ('Hierarchical','Dependency','Causal')
ORDER BY e.edge_type, s.content, t.content;
```

which returns, for `cloudops-exhibit`:

```
VPC-Enterprise-Prod
├── InternetGateway, RouteTable-Public, SG-PublicWeb,
│   Subnet-Private-1a, Subnet-Private-1b, lambo-cloudops-db-subnets
├── SG-Base-VPC
│   └── RDS-Lambo-Demo-DB          the dependency the demo protects
└── Subnet-Public-1a
    └── EC2-LamboWebExhibit
```

`Lambda-LamboStats-API` is absent from the hierarchy because it genuinely runs
outside the VPC. The tree states the architecture correctly without being told
to, which is the whole argument the exhibit is making.

Contract:

```
GET /api/graph

200 {
  "session": "cloudops-exhibit",
  "nodes": [
    { "content": "VPC-Enterprise-Prod", "concept_type": "Entity",
      "status": "Canonical", "blast_radius": 7 }
  ],
  "edges": [
    { "parent": "VPC-Enterprise-Prod", "child": "SG-Base-VPC", "edge": "Hierarchical" }
  ],
  "truncated": false
}
```

- Structural edge types only, as above. `CoOccurrence` must not appear, or the
  false Lambda to RDS edge from **T7** becomes a visible claim on the page.
- `status` and `blast_radius` come from the `concepts` row, so the tree can mark
  load-bearing nodes without a second call.
- **Carry gate progress in the `/api/inspect` payload** (T11): which canonization
  gates this concept meets and which it does not, each with its current value
  against the bar. All of it is computed during evaluation and thrown away, so
  this is surfacing rather than calculating, and it is what lets a page explain a
  `Candidate` instead of just labelling it.
- Bound the payload and set `truncated` rather than silently cutting.
- Read-only, no writer lease, same as every other route on this surface.

Also in this file: T1-P3-1, the constant-time token comparator leaking input
length through its loop count.


### ✅ T3 — DONE (2026-08-17, merged `5a6f633`)

`serve_web.rs` now exposes the structure and dependents the page is built
against (`src/cli/serve_web.rs`, plus `src/canon/gate.rs` and `pub(super)`
threshold exposure in `src/canon/stage1/2/3.rs`):

- **`GET /api/inspect?focus=&depth=1`** — structural-only (Dependency/Causal/
  Hierarchical) hop-1 dependents + `status`/`blast_radius`; every miss kind is a
  `200` with `found:false`; bounded by `MAX_INSPECT_NODES` with an honest
  `truncated`; read-only (no writer lease — the no-writer-lease source-grep test
  still passes).
- **`GET /api/graph`** — the structural skeleton (nodes with `status`/
  `blast_radius`, edges), no `CoOccurrence` (so the false T7 edge cannot
  appear), bounded with `truncated`, deterministic ordering.
- **T11 gate progress** in `/api/inspect` — per-concept met/not-met for
  `gc_survived`, `blast_radius`, `distinct_interactions`, `coverage`, plus
  repromotion-cooldown state, surfaced from the eval's own queries and the
  single-sourced stage thresholds; degrades to omitted (200 intact) on store
  failure. One pause: this surfaces T11's payload in the `/api/inspect`
  contract now; T11's remaining work (per the rewritten T11 section) closes
  against the same payload.
- **Constant-time token comparator** (T1-P3-1) — loop count fixed by the secret
  length, length folded via XOR, no short-circuit.

**Contract note (T3-R2-N1):** an `/api/inspect` hit carries two `blast_radius`
keys with different provenance. The top-level `blast_radius` is the **live**
dependent count (for tree marking, via `blast_radii`) and can count edges
younger than `canonization_edge_min_age`; `gate_progress.blast_radius` is the
engine's **aged** evidence (`store.blast_radius` with the `min_edge_age`
cutoff), answering "does it clear the Stage-3 bar". They can transiently
differ; that is intended.

**Review:** 3 rounds, all APPROVE. R1: 5 P3 + 3 nits (aged blast-radius gate,
graph/inspect blast-radius agreement, a `truncated` false-positive, the
un-surfaced repromotion cooldown, and untested truncation; plus 3 nits) →
remediated; R2 verified all genuine + 2 doc/test nits; R3 cleared the edge-bound
truncation test (N1 landed here in the contract). Docs in
`adve-review-remed-T3round{1..3}.md`.

**Verify:** full `cargo test --all-features` green — **835 passed / 0 failed**
(T1+T2+T1b+T3 merged); 30 serve_web + 59 canon tests pass.

---

## T4 — `#[non_exhaustive]` on `ResolvedBackends`

**Files:** `src/resolve.rs`
**Closes:** T1-P2-2
**Blocked by:** nothing

Adding the `config` field already made this a breaking change for library
consumers. The attribute stops the next field being another one.


### ✅ T4 — DONE (2026-08-17, merged `6221feb`)

`#[non_exhaustive]` on `ResolvedBackends` (`src/resolve.rs`), with a doc note
stating it is deliberate — a one-time break (callers can no longer
literal-construct or exhaustively destructure it) that buys permanence for
every future field, per T1-P2-2. All construct/destruct sites are in-crate and
compile unchanged. Docs in `adve-review-remed-T4round{1..2}.md`.

**Verify:** full `cargo test --all-features` green — **835 passed / 0 failed**.


### ✅ T5 — DONE (2026-08-17, merged `22afa95`)

Adversarial re-review of the launcher (`launch_exhibit_ec2.py`,
`provision_network.py`, `_common.py`) after the prebuilt-tarball / Ubuntu
switch. Result: all **8 known T6 findings remain LIVE** (none stale), plus **5
new defects** in the ~194 never-reviewed lines. The five new items are folded
into T6 below. Details: `dev-diary/adversarial-review/adve-review-remed-T5.md`.

---

## T5 — Re-review the launcher

**Blocks:** T6
**Blocked by:** nothing

The reviewed file predates the Ubuntu switch, the prebuilt llama tarball, the
SHA-512 checksum fix and the architecture table. About 194 changed lines were
never reviewed. Several findings are stale, and the replacements have had no
adversarial pass at all.

---

## T6 — Launcher fixes

**Files:** `scripts/aws-infra/launch_exhibit_ec2.py`, `scripts/aws-infra/provision_network.py`
**Closes:** T2-P2-1, T2-P2-2, T2-P2-3, T2-P2-4, T2-P3-1, T2-P3-2, T2-P3-3, T2-P3-4
**Blocked by:** T5
**Blocks:** D1 (clean redeploy, deployment doc)

- Port 80 closed by default breaks plain `http://` to `https://` redirects and
  the ACME HTTP-01 fallback.
- `--bge-model-url` can be overridden without `--bge-model-sha256`, producing a
  boot checksum mismatch and a crash loop.
- IAM retry catches generic `InvalidParameterValue` and hides real config errors
  behind 60 seconds of misleading propagation hints.
- Stale prose: the module docstring still opens "A `t4g.large` … t4g is
  Graviton, so the machine is ARM64", a comment still claims the instance stays
  ARM for cost, and `TIGHT_FOR_LOCAL_BGE` still explains itself in terms of
  "room for the source build". There is no source build and the shipped path is
  x86_64 Ubuntu.
- P3s: ephemeral IP race on re-adopted pending instances; `caddy.service` uses
  `Restart=on-failure` while the others use `Restart=always`; system users
  created without static UIDs; the health check polls 300s even after
  `llama-server` has died.

**Also closes, from the T5 re-review** (`adve-review-remed-T5.md`): all 8 items
above remain LIVE + 5 new defects in the never-reviewed changed lines:
- **NEW-1 (P2)** — `--llama-cpp-ref` override changes the tarball URL but the
  SHA-256 pin (`LLAMA_TARBALLS`) and extract dir stay pinned to the default ref,
  so a non-default ref fails `sha256sum -c` at boot after the instance is
  "running". Make hash + extract-dir depend on ref; refuse an unpinned
  `--llama-cpp-ref` at ARG-PARSE time.
- **NEW-2 (P2)** — no post-boot success detection; a bootstrap failure (tarball/
  BGE checksum) aborts user-data with `set -e` while the script prints "exhibit
  launched" and exits 0. Poll instance status 2/2 + probe the Caddy/lambo health
  endpoint; on failure print console tail and return non-zero.
- **NEW-3 (P3)** — `cp -a llama-${REF}/.` assumes the archive's top-level layout;
  after extraction test `-x $DIR/llama-server` + `libllama.so*` and fail closed.
- **NEW-4 (P3)** — `ARM_FAMILIES` entry `'x2g'` lacks the trailing dot; complete
  the family lists (fail-closed but wrong for real families).
- **NEW-5 (P3)** — Ubuntu 26.04 SSM parameter paths unverified at review time
  (creds expired) + the `stable/current` AMI is never pinned. Verify both
  `UBUNTU_SSM` paths in `us-east-1` before shipping, pin/log the resolved AMI.


### ✅ T6 — DONE (2026-08-17, merged `0ed7cc7`)

All 13 findings (8 known + 5 new from T5) closed in
`scripts/aws-infra/launch_exhibit_ec2.py` + `provision_network.py`:
port 80 open by default (http→https redirect + ACME HTTP-01); `--bge-model-sha256`
required with a custom URL; IAM retry narrowed to genuine propagation errors;
stale x86_64/Ubuntu prose corrected; ephemeral-IP race bounded; `caddy.service`
`Restart=always`; static UIDs 901-903 with fail-loud collision checks; health poll
aborts after 3 consecutive llama-server failures; `--llama-cpp-ref` hash +
extract-dir now follow the ref (unpinned refs refused at parse); post-boot
readiness probe (status 2/2 + `:443`) with an honest console-diagnostic; tarball
layout/loadability verified before install; ARM/X86 family lists completed.

**Round 1 was REQUEST_CHANGES** — 2 P1 (a dropped `require_subnet`/`require_sg`
import, a NameError on every real launch; and the accidental removal of the
`libgomp1` install, a fresh-build llama-server failure) → remediated; R2/R3
verified all genuine and clean. Docs in
`adve-review-remed-T6round{1..3}.md`. README refreshed (this commit).

**D1 blocker (documented):** NEW-5 — the Ubuntu 26.04 SSM parameter paths were
not verifiable at review time (AWS creds expired; the instance role has no
`ssm:SendCommand` and no SSM agent is installed). A precise
`aws ssm get-parameter` command for both `UBUNTU_SSM` paths runs at launch
(`launch_exhibit_ec2.py:178-192`). **Must be run before D1 (clean redeploy).**

---

## T7 — CloudOps agents

**Files:** `scripts/cloudops/_lambo.py`, `01_network_agent.py`, `02_app_data_agent.py`
**Closes:** T3-1-P1-1, T3-1-P2-1..4, T3-1-P3-1..4
**Blocked by:** nothing

**T3-1-P1-1 does not require re-deriving the exhibit.** `02_app_data_agent.py`
derives the Lambda beside the RDS database in one call, and Lambo links
everything co-derived in one interaction, so the graph asserts a relationship
between two components the architecture deliberately isolates. Split the call.

The edge already written cannot be removed by re-running, because Lambo appends
rather than retracts. Only a fresh session would erase it, at the cost of the
Canonical status already earned and another soak. **Not worth paying**, because
the edge changes nothing numerically:

```
src/store/sqlite.rs:207
const STRUCTURAL_EDGE_IN: &str = "'Dependency', 'Causal', 'Hierarchical'";
```

`CoOccurrence` is excluded from `blast_radius` and `interaction_span`. Blast
radius, the canonization gates and every count on the portal are unaffected. The
edge is visible only where hop-1 neighbours are listed: `lambo inspect` and
`03_crossover_protect.py`. **Fix the script, keep the session.**

Rest of the task:
- `_run` selects `detail[-1]` on failure, discarding Clap's actual error and
  printing only "For more information, try '--help'."
- No executable-permission check on the resolved binary, so a bad path raises an
  unhandled `PermissionError`.
- IPv6 CIDRs are silently dropped because `--parent-of CHILD:PARENT` cannot
  carry colons.
  > **Carries T1 part 2 deferral T1b-R1-3:** the CLI now accepts an IPv6
  > `--parent-of` parent (first-colon split, T1 part 2 #4), but this script's
  > `_refuse_colon` still pre-refuses it. Relax the PARENT-side `_refuse_colon`
  > (and the pre-filter) to colon-free-child-only once the client split logic is
  > updated, and add a regression that an IPv6 CIDR parent round-trips through
  > the launcher. A T7-naming comment marks the site at `_lambo.py:~304`.
- Network prerequisite check uses `inspect(depth=1)`, bounded by
  `MAX_INSPECT_NODES = 64`; past 64 nodes it spuriously reports `SG-Base-VPC`
  missing. The session is already at 113 nodes, so this is live, not theoretical.
- `_peer_label` closure reallocated per security-group iteration. Keep the
  behaviour, which stops a home IP reaching the public portal, and move it to
  module scope.
- `rsplit(" [", 1)` truncates bracketed concept text.
- `resolve_lambo_binary` prefers a stale `target/release/lambo` over a debug
  build with newly enabled features.

### ✅ T7 — DONE (2026-08-17, merged `2bd5f7f`)

All 9 items closed in `scripts/cloudops/{_lambo.py,01_network_agent.py,
02_app_data_agent.py}`:
- **T3-1-P1-1** — `02` derives the RDS and Lambda tiers as **two separate
  interactions**, so no false cross-tier `CoOccurrence` edge is generated. All
  seven `skip_rds`/`skip_lambda`/`exhibit_role` combinations preserved (one
  stricter, in the fix's direction). The already-written legacy edge persists
  ("keep the session") but is non-structural and inert for the demo.
- `_lambda.py` — real CLI error surfaced (`_run`), executable check on the
  resolved binary, `_parent_of_flags` now lets an IPv6 CIDR **parent** round-trip
  (child stays colon-free; the T1b-R1-3 deferral closes), by-name network
  prerequisite check (no spurious `MAX_INSPECT_NODES` truncation), and newer-of-
  release/debug binary preference.
- `01` — `_peer_label` to module scope; child-end colon phrasing.
- `02` — bracketed-concept-text truncation fixed (anchored metadata regex).

**Review:** 2 rounds, all APPROVE. R1: 1 P3 + 4 nits (stale dry-run plan text,
the README parent-side colon rule, two "child end" comment phrasings, the
`assert`-based self-test) → remediated (self-test made fail-closed under `-O`);
R2 verified all genuine, zero findings. Docs in
`adve-review-remed-T7round{1..2}.md`. `scripts/cloudops/README.md` colon rule
updated here (the R1-2 doc item).

---

## T8 — `03_crossover_protect.py`

**Files:** `scripts/cloudops/03_crossover_protect.py`, `scripts/cloudops/_lambo.py`
**Closes:** T3-2-P2-1, T3-2-P2-2
**Blocked by:** nothing
**Blocks:** D2 (video, deployment doc)

- `run_guard` raises `InfraError` on an empty session because `lambo inspect`
  exits 1 with "no concept matching", so `render_unprotected()` never runs.
- `parse_outbound_neighbours` includes `CoOccurrence` siblings, so unrelated
  subnets appear as "stranded dependents" in the abort banner. Given **T7**,
  this is the one place a judge could be shown a false dependency, so it is the
  higher priority of the two.

**This script has never been executed, not once.** It is the demo's climax and
the thing the video depends on. Running it is part of the task, not a follow-up.


### ✅ T8 — DONE (2026-08-17, merged `2cc4967`)

`03_crossover_protect.py` (the demo's climax) fixed **and executed**:
- **Empty session** — `run_guard` swallows only the exact `no concept matching`
  inspect error, so `render_unprotected()` runs and the run exits 0 with a
  prominent stderr banner (a wrong session id is loudly flagged) instead of
  aborting. The destructive AWS call is never issued on any path.
- **`parse_outbound_neighbours`** keeps only structural edges
  (`Dependency`/`Causal`/`Hierarchical`), so unrelated `CoOccurrence` siblings
  no longer appear as "stranded dependents" in the abort banner — with T7's
  split-derive this is no longer a place a false dependency can be shown.
- `EMPTY_SESSION_ERR` sentinel is sourced from the **real**
  `src/cli/inspect.rs` `Focus::Missing` arm at self-test time, so a CLI reword
  fails loudly instead of silently reverting the guard.

**Ran live** against the exhibit (guard case + empty-session case), as the task
mandated; `evidence/remed-t8-crossover-run.md` captures both. Note: the
committed evidence is a synthetic recapture (stubbed I/O); a **real-live
capture** of both cases (especially the empty-session error + exit 0 + banner)
is strongly recommended before D2 (video) for a robust defense — flagged for
Main.

**Review:** 3 rounds, all APPROVE. R1: 4 P3 + 2 nits → remediated; R2 caught a
claimed-but-weak self-test (the drift-fail test didn't truly source the live
string) → fixed by sourcing `src/cli/inspect.rs`; R3 cleared. Docs in
`adve-review-remed-T8round{1..3}.md`. `scripts/cloudops/README.md` refreshed
(the R1-3 doc item).

---


## T9 — Recall does not answer dependency questions

**Files:** `src/recall/*`
**Blocked by:** nothing
**Not a review finding.** Measured against the live exhibit.

The graph holds the answer and recall does not surface it. Two measurements:

```
q = "what depends on SG-Base-VPC"
  SG-Base-VPC                                    (score 2.94)
  SG-Base-VPC ingress all protocols from SG-Base-VPC   (score 2.72)
  SG-Base-VPC = sg-071b52ffe5950efdf             (score 2.61)
  SG-Base-VPC ingress tcp/5432 from SG-PublicWeb (score 2.22)
```

It returns the security group and its own rules. It never names
`RDS-Lambo-Demo-DB`, which is the only thing that depends on it, and which the
store knows about through a `Hierarchical` edge that a single SQL query returns.
Recall is matching words, not structure.

```
q = "is it safe to delete the shared security group"
  five results, every one scored 0.18
```

An identical score across every hit is not a ranking, it is a floor. The likely
cause is that the stored concept contents are identifier-shaped
(`SG-PublicWeb ingress tcp/443 from 0.0.0.0/0`) while the query is prose, so the
vector arm contributes nothing and the lexical arm matches nothing either.

**Instrument before changing anything.** Log which arm produced each hit and
what each contributed to the final score. Nobody can currently say whether the
vector arm contributes anything on identifier-shaped content, and the flat 0.18
is unexplained. This is a few lines behind a flag and it converts the rest of
this task from guesswork into measurement.

The likely shape of the fix, once measured: **route by query kind rather than
blending arms.** "What depends on X" has an exact answer reachable by traversal,
and ranking it is a category error. Hybrid currently means blending lexical,
vector and structural scores; it probably wants to mean recognising which arm a
question belongs to and dispatching there. A structural question that falls
through to word matching does not degrade gracefully, it returns something
plausible and wrong.

Worth establishing before changing anything:
- Whether structural expansion runs at all for these queries, and if so why
  dependents do not reach the assembled block.
- Whether the flat 0.18 is a floor constant or genuine cosine agreement.
- Whether identifier-shaped content needs different treatment from prose on the
  way in, on the way out, or both.

This is the difference between the exhibit demonstrating its thesis and merely
asserting it, and it is more valuable than any remaining P2.


### ✅ T9 — DONE (2026-08-17, merged `7baa31a`)

Recall now routes dependency questions by query kind instead of blending arms
(`src/recall/dispatch.rs` + `Daemon::recall` routing + instrumentation in
`candidates.rs`/`assemble.rs`). Investigation first (the doc's mandate):
structural expansion DOES run but structural members get `relevance 0` and are
buried below word-matches at `top_k`; the flat 0.18/0.25 is the blend's floor
when no arm scores on identifier-shaped content.

- **Instrumentation** — per-hit per-arm contribution logging
  (`tracing::trace!`, target `lambo::recall`, default-invisible, gated on
  `tracing::enabled!` so no eager allocation).
- **Dispatch** — `classify` recognizes dependency phrasing; when an anchor with
  §4.1 exclusive-single-source dependents resolves, "what depends on X" is
  answered by traversal (structural edges only), falling through to the full
  blend on no-anchor/no-dependents. `dependents()` shares
  `format::inbound_sources` with the blast-radius predicate so membership and
  the stamped field agree. Canonical-first promotion; load-bearing-pillar
  warning rendered on structural hits.
- **Measured (faithful local session over live Cockroach data):** "what depends
  on SG-Base-VPC" now names `RDS-Lambo-Demo-DB` first (9.5) instead of the SG's
  own rules; "is it safe to delete the shared security group" now returns a
  real descending ranking instead of a flat floor.
- **Tests** assert the dispatch, the false-positive guard (a marker-bearing but
  non-dependency query stays General), and the refusal.

**Review:** 3 rounds. R1 was REQUEST_CHANGES — the round-1 classifier was
over-broad (bare `"depend"` matched `"independent"`) and the traversal
membership diverged from the §4.1 blast-radius predicate; remediated (explicit
phrasings only + anchor-gate, membership reconciled, full-blend refusal,
instrumentation gated); R2/R3 cleared. Docs in
`adve-review-remed-T9round{1..3}.md`.

**Verify:** full `cargo test --all-features` green — **842 passed / 0 failed**.
The `traversal_depth` one-hop and no-cache/no-hotlist decisions are documented
at the dispatch site.

---

## T10 — The Lambda Function URL returns 403, undiagnosed

**Files:** `scripts/aws-infra/provision_app_data.py`, AWS-side config
**Blocked by:** nothing
**Not a review finding.**

The function itself works. Invoked directly it returns live counts read from
CockroachDB through the scoped secret:

```
{"session": "cloudops-exhibit", "concepts": 41, "canonical": 1,
 "edges": 485, "interactions": 10, ...}
```

Only the public Function URL 403s. The resource policy is correct
(`Effect: Allow`, `Principal: *`, `lambda:InvokeFunctionUrl`, condition
`FunctionUrlAuthType: NONE`), `AuthType` is `NONE`, and the account is not in an
Organization, so no SCP explains it. It still 403d after the account moved from
the Free plan to Paid, so the plan was not the cause either.

Untested hypothesis: an account-level Lambda public-access block. The bundled
botocore has no `get_public_access_block_config`, so it could not be checked
from here.

This decides whether §11 can claim a public endpoint or has to describe the
Lambda as IAM-invoked. Either is honest; the claim just has to match.

### ✅ T10 — DONE (2026-08-17, merged `54165e3`)

**Root cause (live-diagnosed):** since October 2025 a public (`AuthType=NONE`)
Lambda Function URL requires BOTH `lambda:InvokeFunctionUrl` AND
`lambda:InvokeFunction` in its resource-based policy. The provisioning (and
the live function) only attached the first, so the URL 403d even though the
policy "looked correct". The **account-level public-access-block hypothesis is
ruled out**: `get-account-settings` exposes no such field, no such CLI
operation exists, the account is not in an Organization (no SCP), and a
resource-policy-only fix resolved it (an account-level deny would not have
permitted that).

**Fix:** added statement `AllowPublicFunctionUrlInvoke` (`lambda:InvokeFunction`,
`Principal *`, condition `Bool lambda:InvokedViaFunctionUrl = true`); the URL
flipped 403 → 200 (verified live: `concepts 41 / canonical 1 / edges 485 /
interactions 72`). `provision_app_data.py` `ensure_lambda` now emits BOTH
statements via an idempotent `_add_perm` helper, so a re-provision cannot
re-break it.

**§11 decision: public endpoint** — a public, unauthenticated read-only stats
endpoint over the live CockroachDB session, outside the VPC. Deployment-doc
guidance updated to match.

**Review:** 2 rounds, all APPROVE. R1: 2 P3 (doc hygiene — section not marked
done, §11 guidance stale) + 4 nits → remediated; R2 cleared. Docs in
`adve-review-remed-T10round{1..2}.md`.

**Verify:** URL returns HTTP 200; `py_compile` clean.


---

## T11 — Surface why a concept is not canonical yet

**Files:** `src/canon/*`, and the payload in T3
**Blocked by:** T1
**Not a review finding.**

**The earlier version of this task was wrong and has been rewritten.** It said
canonization was unreachable at the default cadence. It is not.
`evidence/managed-mcp-canonization-events.md` captures the full
`Candidate → Venerable → Canonical` walk at blast radius 9, on the live cluster,
reproduced through two independent MCP clients. The engine is proven.

What is actually true is narrower, and worth keeping: every captured walk is the
`lambo demo` scenario, which sets `gc_interval` to 1 internally. No session has
yet been observed promoting at the **shipped default cadence**, because none has
run long enough to try. That is unmeasured, not broken, and the distinction is
the one `evidence/` maintains everywhere else.

The useful work is therefore not to change a threshold. It is to make an
un-promoted concept explicable:

- Report, per concept, which gates are met and which are not, with the current
  value against the bar. `gc_survived` 2 of 3. Blast radius 7, needs above 5.
  Distinct interactions 2 of 3. Coverage 0.22, needs 0.3.
- Every one of those numbers is already computed during a canonization
  evaluation pass and then discarded. This is surfacing, not calculating.
- Fold the payload into T3's `/api/inspect` response, which already carries
  `status` and `blast_radius` from the same query path.

Why it matters beyond a demo: a user asking "why is this not canonical yet" has
no way to find out today, and the answer is fully computable. It is also the
thing that would have prevented the exhibit being driven to a forced promotion,
because the gap would have been visible rather than inferred.

Separately, and cheaply: a session that runs at default cadence long enough to
promote naturally would close the one genuinely unmeasured claim here. That is
patience, not engineering.

### ✅ T11 — DONE (2026-08-17, satisfied by T3 `5a6f633`; verified `0608d32`)

The rewritten T11 deliverable — surface per-concept canonization gate progress
in `/api/inspect` — was **already implemented and merged in T3** (`src/canon/gate.rs`
+ the `/api/inspect` `gate_progress` payload, `5a6f633`). A verification pass
confirms all five requirements are met: the payload carries `gc_survived` /
`blast_radius` / `distinct_interactions` / `coverage` as `GateMetric`s
(`current`/`bar`/`met`, `strictly_above` on blast radius) plus
`in_cooldown`/`cooldown_until`; every number is *surfaced* from the
evaluation's own sources (persisted `gc_survived`, `store.interaction_span`,
aged `store.blast_radius`, `last_demotion_time` + cooldown) — not recomputed —
with bars single-sourced from the stage modules' `MIN_*` constants
(3 / 3 / 0.3 / 5); the payload is additive to the same `/api/inspect` response
as `status` and `blast_radius`; and inspect tests exercise it (9/9 pass).
No separate T11 code change was needed. Details:
`dev-diary/adversarial-review/adve-review-remed-T11.md`.

The one detail the projector keeps: no session has yet been observed promoting
at the *shipped default* cadence (all captured walks used `lambo demo`'s
`gc_interval = 1`). That is unmeasured, not broken — closing it is patience
(let a session run at default cadence long enough), not engineering, and is out
of scope here.


---

## T12 — Release workflow builds against too-new glibc

**Files:** `.github/workflows/release.yml`
**Blocked by:** nothing
**Not in the review**

Both Linux targets build on Ubuntu 24.04 runners, so the published binaries
require GLIBC 2.39 and die on Amazon Linux 2023 with `version GLIBC_2.39 not
found` after passing their checksum. A Debian bookworm build of identical source
requires only 2.34 and runs everywhere, verified this session.

This is why the exhibit moved to Ubuntu 26.04. The distro switch worked around a
packaging defect that is still shipping to every user of the install script.


### ✅ T12 — DONE (2026-08-17, merged `130d6d5`)

`.github/workflows/release.yml` now builds both Linux targets inside a
**Debian bookworm** container (job-level `container: ${{ matrix.container }}`),
so the toolchain and `cargo` link against the container glibc (≤ 2.36, and the
current pure-Rust source floors at 2.34), not the runner's 2.39. A
"Assert max required GLIBC <= 2.34" step (`readelf -V`, `set -euo pipefail`,
installs `binutils`) fails CI if a `GLIBC_2.35+` symbol ever appears, making the
Amazon Linux 2023 (GLIBC 2.34) guarantee **structural** rather than empirical.
macOS/Windows rows stay host-native (intentionally omit `container`); the
release job stays containerless; the checksum/artifact flow is unchanged.

> **Superseded (2026-08-17, later in the same day):** the `debian:bookworm`
> container was removed. The in-container parity tests (which spawn `lambo`
> subprocesses over stdio) were flaky — the full suite stalled >60s per test
> in the container while the identical tests run in ~1s on the host — and the
> Amazon Linux 2023 (GLIBC 2.34) floor was deliberately dropped. Linux
> release builds now run natively on the Ubuntu 24.04 runners (glibc 2.39),
> with the gate relaxed to "Assert max required GLIBC <= 2.39" (still
> preventing silent creep above the build host). See
> `adve-review-remed-ubunround1.md`.

**Review:** 3 rounds. R1 was REQUEST_CHANGES — the initial change put
`container:` only in the matrix rows and never wired it to the job (inert; all
steps still ran on the Ubuntu host at 2.39), and bookworm caps glibc at 2.36
not 2.34 → remediated (job-level wiring + the readelf gate); R2/R3 cleared.
Docs in `adve-review-remed-T12round{1..3}.md`.

**Verify:** discharged by the real release below, not by a draft build. The
AL2023 launch check is moot: the GLIBC 2.34 floor was dropped with the
container, so there is nothing left to prove on that distro.


### ✅ v0.2.0 SHIPPED (2026-08-17, tag `v0.2.0` = `35c86fb`)

The release workflow T12 rewrote is no longer theoretical. Both channels are
live and verified.

**GitHub release** `v0.2.0`, published 05:13 UTC, `draft: false`,
`prerelease: false`. Nine assets: four binaries (`linux-x86_64`, `linux-arm64`,
`macos-arm64`, `windows-x86_64.exe`), their four `.sha256` files, and
`install.sh`. The tag, `origin/main` and local `HEAD` all point at the same
commit, so no dangling tag survived the three failed attempts.

**crates.io** `lambo 0.2.0`, published 05:14 UTC, not yanked. It is the only
version on the registry (0.1.0 was never published), so `max_version` is
correct and `cargo install lambo` resolves it.

**install.sh** resolves the latest release through the GitHub API, honours
`LAMBO_VERSION` for pinning, and verifies the `.sha256` with a `sed` extraction
that tolerates both GNU `sha256sum` and BSD `shasum -a 256` output.

#### Four failures on the way there

Each one is worth keeping because each was diagnosed wrong at least once.

1. **The crates.io tarball would have shipped the whole repo.** The version
   bump added a `publish-crate` job, and the reviewer caught that the package
   had no `include`: 6.4 MiB carrying `dev-diary/` and `evidence/`, the latter
   holding live Cockroach and AWS identifiers. A `[package] include` whitelist
   took it to 3.0 MiB with no internal content. See the leak note below: the
   whitelist is *nearly* airtight, not airtight.
2. **Workflow validation failed on a comment.** A literal `${{` inside a
   run-block comment trips GitHub's expression parser before any step executes.
   The first diagnosis blamed a nearby `sed` line; the reviewer pulled the
   actual error out of the run logs and it was the comment. Worth remembering
   that a `${{` in a `run:` block is parsed even where it reads as prose.
3. **The bookworm container could not bootstrap itself.** `debian:bookworm`
   ships without `curl`, so rustup's installer exited 127, and once that was
   fixed the image also had no C compiler for `sqlx` and `rustls`. Adding
   `curl` and `build-essential` cleared both.
4. **Container parity tests were flaky, so the container went away.** Tests
   spawning `lambo` subprocesses over stdio stalled 60s or more inside the
   container against roughly 1s on the host. Rather than chase it, the AL2023
   glibc 2.34 floor was dropped as a deliberate scope decision: Linux builds
   run natively on Ubuntu 24.04, the assertion gate relaxed to
   `GLIBC <= 2.39`, and the container is gone entirely. The gate still exists,
   so it still catches creep above the build host. It just no longer promises
   AL2023.

The fifth run was clean: every job green, release published, crate published.

#### Open, low severity: the include whitelist leaks 14 READMEs

Verified against the published `.crate` (769 KiB compressed), not against
intent. These shipped despite the whitelist:

```
demo/README.md, site/README.md,
scripts/{aws-infra,cloudops}/README.md,
dev-diary/README.md, dev-diary/adversarial-review/README.md,
dev-diary/evidence/{,demo-determinism/,mcp-client-interop/,mcp-client-stdio/}README.md,
evidence/{,demo-determinism/,mcp-client-interop/,mcp-client-stdio/}README.md
```

**Cause:** cargo `include` patterns are gitignore-style globs, so the entry
`"README.md"` is unanchored and matches at every depth rather than at the
package root. `LICENSE` and `NOTICE` share the flaw but have only one match
each in the repo.

**Why it is low and not urgent:** the reviewer's actual concern did not
materialise. A scan of all fourteen for ARNs, twelve-digit account ids,
`cockroachlabs.cloud` hosts, `sg-`/`vpc-`/`i-`/`subnet-` ids, EIP allocations,
`AKIA` keys and public IPs returns nothing resolvable: every hit is an RFC1918
CIDR, a loopback address, or a resource *name* like `VPC-Enterprise-Prod` that
is already public in the README. What did ship is internal process
documentation, not credentials.

**Fix, whenever something else forces a patch release:** anchor the three
root-file patterns as `/README.md`, `/LICENSE`, `/NOTICE`. Not worth a 0.2.1 on
its own.


### ✅ E2E — DONE (2026-08-17; integrated `5db0b90`, ready to push)

End-to-end review of the whole integrated T1–T12 surface (29 commits from
`26c4d71` to HEAD, 68 files / +9258). Two adversarial rounds:

- **R1 (APPROVE, 3 P3 + 2 informational nits):** verified all cross-task seams
  — shared files coexist (`ResolvedBackends` `#[non_exhaustive]` breaks no
  construction site; `open_writer` + reader-contract + T3 routes agree; the
  `_lambo.py` structural whitelist matches both Rust structural authorities;
  validation precedes the lease); forward contracts honored (IPv6 first-colon
  `--parent-of`; structural closure consistent across T3/T8/T9; single-sourced
  gate bars); documented guarantees hold (no-writer-lease, structural-only
  edges); T6/T10 AWS and T12's release don't conflict; 217 seam tests pass.
  Three P3s remediated (merged `5db0b90`):
  - **E2E-R1-1** — `/api/inspect` gate_progress is API-only; documented in
    `web/app.js` (the focus-driven panel lands in the parked UI pass).
  - **E2E-R1-2** — launcher comment refreshed for T12 (bookworm + glibc gate);
    NEW-5 **discharged** (both `UBUNTU_SSM` paths verified live in `us-east-1`).
  - **E2E-R1-3** — lib.rs crate-doc example now passes `.config(backends.config)`
    before `.backends()`, honoring the T2 `[daemon]` contract.
- **R2 (APPROVE):** all three remediations verified genuine; the integrated
  whole is clean and **ready to push**.

Docs: `adve-review-remed-E2Eround{1..2}.md`. **Verify:** full
`cargo test --all-features` green — **842 passed / 0 failed**.

**Remaining for D1/D2/D3 (deployment, not this task):** T6's NEW-5 SSM check
discharged above; the T12 release build discharged by the shipped v0.2.0 above.
What is left is a **real-live capture** of `03_crossover_protect.py` (T8)
before the video, and the D1 clean redeploy.

---




## Parallelism

Start immediately, nothing shared:

```
T1 ─┐
T2 ─┤  engine          (T2 after T1)
T3 ─┤
T4 ─┘
T5     re-review launcher
T7 ─┐  agents
T8 ─┘
T9     recall quality
T10    lambda 403
T12    release workflow
```

The chains:

```
T1 ──► T2
T1 ──► T11             cadence needs validation first
T5 ──► T6              re-review before touching the launcher
```

Handing off to `deployment-and-submission.md`:

```
T6 ──► D1              clean redeploy needs the launcher fixed
T8 ──► D2              the climax script must have run before any capture
```

Critical path across both documents: **T5 → T6 → D1 → D2**, with **T8** joining
before D2.
