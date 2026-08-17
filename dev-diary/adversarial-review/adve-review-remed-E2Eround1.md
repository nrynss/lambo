# End-to-end adversarial review — T1-T12 remediation surface, round 1 (final)

**Reviewer:** E2EReviewR1 (cross-task seam hunt over the integrated whole)
**Tree:** `/home/nryn/work/lambo` `main` @ `b8d38eb`
**Range:** `26c4d71..HEAD` — 29 commits, 68 files, +9258/−356
**Mode:** READ-ONLY. No source/script edited. The only artifact written is this file.
**Baseline:** Main owns the full suite; stated baseline 842 passed / 0 failed. I ran `cargo check --lib` (clean), `cargo test --lib -- <seam modules>` (217 passed / 0 failed), `py_compile` on all 7 cloudops/aws-infra scripts + `python3 scripts/cloudops/_lambo.py` self-test (all pass), and two read-only `aws ssm get-parameter` lookups (us-east-1, `AWS_PROFILE=lambo-user`).

```text
╔═══════════════════════════════════════════════════════════════════╗
║  VERDICT: APPROVE — ready to push                                 ║
║  0 P1 / 0 P2 / 3 P3 / 2 nits. Three doc/integration gaps (P3),    ║
║  none load-bearing. The seams hold.                               ║
╚═══════════════════════════════════════════════════════════════════╝
```

**Disposition:** APPROVE. The cross-task seams — the entire point of this pass — are
consistent. The three P3 findings are documentation/UI-wiring gaps, not correctness or
regression defects; none changes a number the engine compares, an AWS permission, a build
output, or a wire contract. I recommend the P3s be patched as small follow-ups (all three are
single-file, low-risk), but none blocks the push and the release this exhibition depends on is
not affected. **The whole is ready to push.**

---

## What I verified (grounding)

Compile + targeted tests: `cargo check --lib` clean; `cargo test --lib -- config:: resolve::
cli:: serve_web:: canon:: recall::` → **217 passed, 0 failed** (includes the serve-web
no-mutating-route test, the dispatch structural tests, the ResolvedBackends/non_exhaustive
sites, and the `/api/inspect` gate-progress tests). Python: `py_compile` clean on all seven
scripts; `python3 scripts/cloudops/_lambo.py` self-test prints
`IPv6 --parent-of; structural whitelist; empty-session sentinel — all pass`. AWS (read-only):
both `UBUNTU_SSM` AMI parameter paths resolve (`arm64`→`ami-0bcb…`, `amd64`→`ami-02eb…`), so
the launcher's NEW-5 resolve concern is discharged.

## The seams — results

### 1. Cross-task file interactions (files touched by multiple tasks)
- **`resolve.rs` (T1 validate + T4 #[non_exhaustive])**: `cargo check` is clean, so the
  attribute breaks nothing. All five in-crate construction sites (`resolve.rs:112`,
  `cli/mod.rs:247/608/784`, `serve_web.rs:1405`, `mcp/serve.rs:1865`) are inside the defining
  crate, where `#[non_exhaustive]` permits construction; the one exhaustive destructure
  (`cli/demo.rs:658`) uses a `..` wildcard. No T1-T3 site contradicts T4. ✓
- **`cli/mod.rs` (T1b reader-contract + T2 open_writer)**: `load_reader_graph_with_contract`
  (`cli/mod.rs:61-79`) enforces the `EmbeddingContract` on the embedder-bearing reader path;
  `open_writer` (`cli/mod.rs:100-108`) forwards `backends.config` into `Memory::builder`. I
  confirmed the **only two** `.backends()` writer construction sites in the tree
  (`cli/mod.rs`, `mcp/serve.rs:616`) both pass `.config()` before `.backends()`. No writer
  site silently drops the `[daemon]` cadence (T2 contract holds end to end). ✓
- **`serve_web.rs` (T1b embedding + T3 routes)**: all three API handlers (`read_stats`,
  `api_inspect`, `api_graph`) call `load_reader_graph_with_contract(..., Some(&state.backends
  .embedding))` — the reader-contract wiring and the T3 routes coexist with no conflict; the
  endpoint is lease-free (test `read_only_router_has_no_mutating_route` passes). ✓
- **`_lambo.py` (T7 IPv6/split + T8 whitelist)**: STRUCTURAL_EDGE_LABELS
  (`("Causal","Dependency","Hierarchical")`, `_lambo.py:117`) agrees exactly with both Rust
  authorities — `serve_web::is_structural` (`serve_web.rs:495-498`) and the blast-radius
  set `recall::format::STRUCTURAL_EDGE_TYPES` (`format.rs:73-77`). All three enumerate
  `{Dependency, Causal, Hierarchical}`. None of the four excluded kinds
  (CoOccurrence/Semantic/Derives/Temporal) is a real dependent; T8's self-test pins the exact
  three. ✓
- **`memory.rs` (T1 fail-closed + T2 forward)**: `build()` calls `config.validate()` before
  the lease (`memory.rs:570`), so a degenerate cadence can neither reach a spawned
  `tokio::interval` nor leak the lease; the lease-ordering regression test passes. ✓

### 2. Forward references
- **IPv6 `--parent-of` (T1b CLI → T7 client)**: CLI splits on the **first** colon
  (`cli/derive.rs:31-40`, parent may carry colons) and the client refuses a colon on the child
  only (`_lambo.py:444-456`). Round-trip is pinned by both `cli/derive.rs` test
  `parent_of_accepts_colon_bearing_parent_ipv6_roundtrip` and the Python self-test. ✓
- **Structural-edge contract (T3 `/api/graph`+`/api/inspect` → T8/T9)**: T8's whitelist and
  T9's dispatch both reuse the same three-edge structural closure (see seam 1). T9's
  `dependents` applies the §4.1 sole-source blast-radius predicate, so the dispatched set and
  the stamped `blast_radius` always agree with `/api/inspect`. ✓
- **T11 gate-progress (delivered via T3)**: produced and unit-tested in `/api/inspect`
  (`serve_web.rs:910-934`); bars single-sourced from the stage `MIN_*` constants
  (`gate.rs:34-36`); comparison operators match each stage (`>=`/`>`). **BUT the payload is
  not consumed by the shipped page — see E2E-R1-1 (P3).**

### 3. Documented-guarantee consistency end to end
- "no-writer-lease serve-web": holds (route set is read-only; reader contract enforced).
- "structural edges only": holds on the wire, in the CLI parser, and in recall formatting.
- "styles single-sourced": the four gate bars are single-sourced from `MIN_*`. The *structural
  kind set* is mirrored in three places (see nit N1) but every instance agrees and T8's
  self-test pins it.
- T8-R1-3 README drift (the wiki said "do not filter to structural headings") is fixed —
  `scripts/cloudops/README.md:233-245` now describes the strict whitelist and the corrected
  exit-status contract. ✓

### 4. AWS / live (T6/T10) vs release (T12)
- **launcher ↔ provision_app_data**: T10's `_add_perm` adds both `lambda:InvokeFunctionUrl`
  and `lambda:InvokeFunction` idempotently (`provision_app_data.py:409-429`,
  `ResourceConflictException`→existing). No conflict with the launcher's SG/provisioning;
  deployment doc §11 updated to the now-answering public endpoint. ✓
- **release.yml (T12)**: Linux builds run fully inside `debian:bookworm` containers with a
  `readelf` glibc ≤ 2.34 gate; macOS/Windows rows host-native; artifact staging/upload
  unchanged inside the container. The exhibit's linux-<arch> assets continue to build, and the
  gate keeps them runnable on AL2023/Ubuntu. One **doc-drift seam** where T12 and T6 collide:
  the launcher's glibc rationale comment now describes the pre-T12 world — E2E-R1-2 (P3).

### 5. Test coherency
No test depends on another task's bug; no duplicated/contradictory fixtures across tasks. The
serve-web fixture, dispatch instrument tests, resolve/config contracts, and the two Python
self-tests all independently confirm the same invariants (structural set, IPv6 round-trip,
empty-session sentinel sourcing the real `Focus::Missing` string). 217 seam tests green.

### 6. Cumulative behavior the per-task reviews couldn't see
- **T9 dispatch ↔ cascade**: a structural query that dispatches skips the gather **and** the
  blend, returning only the traversal; a structural phrasing that does not resolve falls
  through to the FULL blend (never a degraded keyword-only answer) — verified in
  `daemon/mod.rs:363-424`. The T9-R2-P3-1 skip/TOCTOU is re-validated under the final lock and
  remains accepted-by-design. Coherent.
- **Const-time comparator**: `tokens_match` (`serve_web.rs:221-237`) scans the full expected
  length with `black_box`, refuses empty/truncated/padded, no early `len` short-circuit; it
  mirrors `mcp::serve`. Sound.

---

## Findings

### P3

- **E2E-R1-1 — T11's `gate_progress` has no product consumer.** `web/app.js` never calls
  `/api/inspect` and never reads `gate_progress`/`in_cooldown`/`met_count` (grepped: 0 refs). The
  gate-progress explanation — the entire point of "surface *why* a concept is not canonical"
  — is delivered to an API the shipped judge portal never hits; only the unit tests (`serve_web.rs:2064-2303`) exercise it.
  `src/cli/serve_web.rs:910-934` (producer), `web/app.js` (non-consumer). *Why P3 not P2:* the
  T11 charter and its review explicitly scoped the deliverable as the `/api/inspect` payload,
  which is complete, correct, tested, and reachable — this is an under-wired *surface*, not a
  wrong/regressed behavior. *Fix:* render `gate_progress` in the web tree-view/inspect pane
  (consume `/api/inspect`) so a judge can actually see per-gate met/bar and the cooldown;
  otherwise drop the payload or document it as an API-only feature.

- **E2E-R1-2 — Launcher glibc/Ubuntu rationale now stale after T12.** `launch_exhibit_ec2.py:164-171`
  still says "the release workflow builds both Linux targets on Ubuntu 24.04 runners (glibc
  2.39)" and frames the Ubuntu-26.04-over-AL2023 choice on that mismatch. T12 (`release.yml`)
  now builds Linux in `debian:bookworm` with a glibc ≤ 2.34 gate, so the stated premise and the
  load-bearing reason for the OS choice are both obsolete. The adjacent NEW-5 block
  (`launch_exhibit_ec2.py:177-185`) still claims the `UBUNTU_SSM` paths "could not be run"; I
  confirmed both resolve in us-east-1, so that VERIFY-before-D1 note is also discharged-stale.
  *Why P3:* comment-only; Ubuntu 26.04 remains a fine choice and the released binary runs on
  it; no behavior changes. *Fix:* refresh the comment to reference the T12 bookworm/glibc-gate
  build and mark the SSM paths verified.

- **E2E-R1-3 — Crate-level doc example contradicts the `backends()`/config contract.** `src/lib.rs:19-24`
  shows `Memory::builder().session(..).agent(..).backends(backends).build()` with no `.config()`,
  but `memory.rs:453-459` now documents that `.backends()` deliberately drops `backends.config`
  and a writer "MUST also pass `.config()`". A reader following the primary crate example would
  silently lose `[daemon]` cadence overrides (T2 contract) with no error. *Why P3:* demo-only,
  canonical/default cadences make it harmless in practice; but it is an internal doc
  contradiction introduced by the T1/T2 forwarding work. *Fix:* add `.config(backends.config
  .clone())` to the example, or annotate it.

### Nits

- **E2E-R1-N1 — Three-place structural-edge mirror is not literally single-sourced.**
  `serve_web.rs:495-498` (`is_structural`), `recall/format.rs:73-77`
  (`STRUCTURAL_EDGE_TYPES`), and `_lambo.py:117` (`STRUCTURAL_EDGE_LABELS`) each restate
  `{Dependency, Causal, Hierarchical}`. All currently agree and the T8 self-test pins the
  Python copy, but a rename on the Rust side in two files (not one) is the drift surface. *Fix
  (optional):* one Rust const reused by both, or a code comment on the second Rust site
  pointing at the first.

- **E2E-R1-N2 — T9's dispatch skip is accepted-by-design (informational).** The
  "decide-skip-early, validate-late" gather-skip can, on cross-thread graph mutation, skip the
  gather and then fall through to a blend — never a wrong structural answer, documented at
  `dispatch.rs:142-148` and `daemon/mod.rs:363-424`. Cleared by T9 round 2/3; flagged here only
  for the E2E record. No action.

---

## Summary

The remediation surface integrates cleanly. Every cross-task seam I probed — the shared files
(`resolve.rs`, `cli/mod.rs`, `serve_web.rs`, `_lambo.py`), the forward contracts (IPv6
`--parent-of`, structural-edge semantics, T2 config forwarding, T11 gate-progress, T12 glibc
floor), the AWS/live changes (T6 launcher ↔ T10 Lambda), and test coherency — is consistent:
`cargo check` and 217 seam tests pass, all Python compiles and self-tests pass, and the Ubuntu
AMI parameters resolve live. The only genuinely NEW cross-task seams surfaced by this whole-
surface pass are the three P3s above (one UI-wiring gap for the T11 payload, one stale launcher
comment after T12, one crate-doc example contradicting the T2 forwarding contract) plus two
nits. None is load-bearing; none affects engine numbers, AWS permissions, the build/release,
or a wire contract. Per the task's own acceptance criteria those are P3, not P2. **APPROVE —
no REQUEST_CHANGES.** I recommend E2E-R1-1/2/3 be patched as small follow-ups at Main's
discretion, but the integrated `main` at `b8d38eb` is ready to push.

```json
{ "verdict": "APPROVE",
  "findings": {
    "P1": [],
    "P2": [],
    "P3": ["E2E-R1-1: gate_progress payload produced/tested but not consumed by web/app.js (surface gap; fix: render it or document API-only)",
           "E2E-R1-2: launch_exhibit_ec2.py:164-185 stale glibc/Ubuntu rationale + unverified-AMI note after T12 (fix: refresh comment; SSM paths verified usable)",
           "E2E-R1-3: src/lib.rs:19-24 example omits .config(), contradicting memory.rs backends()/config() contract (fix: add .config to example)"],
    "nits": ["E2E-R1-N1: structural-kind set mirrored across serve_web.rs/recall/format.rs/_lambo.py (agrees today; T8 selftest pins Python copy)",
             "E2E-R1-N2: T9 dispatch skip accepted-by-design (informational; cleared by T9 round 2/3)"] },
  "summary": "APPROVE. Integrated T1-T12 at b8d38eb is ready to push. All cross-task seams hold: shared-file edits coexist (resolve.rs non_exhaustive breaks nothing; cli open_writer + reader-contract + T3 routes agree; _lambo structural whitelist matches both Rust authorities; memory config forwarding holds at both writer sites); forward contracts honored (IPv6 first-colon parent-of round-trips; structural closure consistent across T3/T8/T9; T11 gate-progress single-sourced from stage MIN_*); doc guarantees hold (no-writer-lease serve-web, structural-only edges, T8 README drift fixed); T6/T10 AWS changes don't conflict with T12's bookworm+glibc-gate release. cargo check clean; 217 seam tests pass; all Python self-tests pass; Ubuntu AMI params resolve. Three P3s + two nits, all non-load-bearing doc/UI gaps, none changes engine/aws/build/wire behavior. No P1/P2." }
```
