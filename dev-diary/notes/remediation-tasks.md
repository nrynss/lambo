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

## T1 — Validate config, and stop a text file panicking the daemon

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
- Bound the payload and set `truncated` rather than silently cutting.
- Read-only, no writer lease, same as every other route on this surface.

Also in this file: T1-P3-1, the constant-time token comparator leaking input
length through its loop count.

---

## T4 — `#[non_exhaustive]` on `ResolvedBackends`

**Files:** `src/resolve.rs`
**Closes:** T1-P2-2
**Blocked by:** nothing

Adding the `config` field already made this a breaking change for library
consumers. The attribute stops the next field being another one.

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
- Network prerequisite check uses `inspect(depth=1)`, bounded by
  `MAX_INSPECT_NODES = 64`; past 64 nodes it spuriously reports `SG-Base-VPC`
  missing. The session is already at 113 nodes, so this is live, not theoretical.
- `_peer_label` closure reallocated per security-group iteration. Keep the
  behaviour, which stops a home IP reaching the public portal, and move it to
  module scope.
- `rsplit(" [", 1)` truncates bracketed concept text.
- `resolve_lambo_binary` prefers a stale `target/release/lambo` over a debug
  build with newly enabled features.

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

Worth establishing before changing anything:
- Whether structural expansion runs at all for these queries, and if so why
  dependents do not reach the assembled block.
- Whether the flat 0.18 is a floor constant or genuine cosine agreement.
- Whether identifier-shaped content needs different treatment from prose on the
  way in, on the way out, or both.

This is the difference between the exhibit demonstrating its thesis and merely
asserting it, and it is more valuable than any remaining P2.

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

---

## T11 — Canonization is unreachable at the default cadence

**Files:** `src/config.rs`, `src/daemon/mod.rs`, `src/canon/*`
**Blocked by:** T1
**Not a review finding.** A design question rather than a defect.

GC sweeps every `gc_interval` mutations, defaulting to 10 000, and Stage 1
requires `gc_survived >= 3`. A concept therefore needs roughly 30 000 mutations
before it can be promoted. `lambo demo` only shows the state machine working
because it sets the same knob to 1 internally, and the CloudOps exhibit only
shows it because `[daemon]` was added and set low.

So on default settings, a real session runs indefinitely and promotes nothing.
Either that is the intended behaviour for very large sessions and should be
documented as such, or the default is wrong. It should not be the case that the
only two sessions which have ever canonized both did so by overriding the knob.

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
