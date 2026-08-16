# Remediation task list

Source: `adve-review-full-stack-sweep-2026-08-16.md` (4 P1 / 12 P2 / 10 P3) and
the Tier 3 detail review beside it, plus defects found outside the review.

**T1 to T14 below are tasks, numbered so that every blocker has a lower number
than the thing it blocks.** Read top to bottom and the order works.

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
  still open as **T11**.
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

---

## T3 — `/api/inspect`, and the token comparator

**Files:** `src/cli/serve_web.rs`
**Closes:** T1-P3-1
**Blocked by:** nothing
**Blocks:** T9, the portal's dependents panel showing real data

The exhibit's headline claim is that Lambo names the workloads that would break.
`/api/recall` does not do that. Measured against the live exhibit:

- `what depends on SG-Base-VPC` returns `SG-Base-VPC` itself and its own ingress
  rules. It never names `RDS-Lambo-Demo-DB`, the only thing that depends on it.
  It matches words, not structure.
- `is it safe to delete the shared security group` returns five items all scored
  `0.18`. No ranking signal, no warning.

`lambo inspect` produces the right answer. `serve-web` exposes no equivalent, so
the page cannot show the thing the submission is about.

Contract, so T9 can be built against it before this lands:

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
**Blocks:** T12

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
**Blocks:** T13, the video

- `run_guard` raises `InfraError` on an empty session because `lambo inspect`
  exits 1 with "no concept matching", so `render_unprotected()` never runs.
- `parse_outbound_neighbours` includes `CoOccurrence` siblings, so unrelated
  subnets appear as "stranded dependents" in the abort banner. Given **T7**,
  this is the one place a judge could be shown a false dependency, so it is the
  higher priority of the two.

**This script has never been executed, not once.** It is the demo's climax and
the thing the video depends on. Running it is part of the task, not a follow-up.

---

## T9 — Portal

**Files:** `web/index.html`, `web/app.css`, `web/app.js`
**Blocked by:** T3, for the dependents panel to show real data

Done and deployed: labelled header facts, live strip, trust ladder, audit trail,
plain-English stat tiles.

Open:
- Dead half-width gap beside the sidebar.
- Flat `0.18` score presentation.
- **The intro copy currently over-promises.** It says Lambo "names the workloads
  that would break and counts them", which `/api/recall` does not do. **T3** makes
  it true, so the copy stays and the dependents panel gets wired to
  `/api/inspect`. The copy is correct only once that panel is live, so the two
  land together or not at all.
- The panel is pure `web/*` and is built against a fixture payload before **T3**
  exists, so **T3** does not hold up the UI work itself.

---

## T10 — Move `drive_mcp_soak.py` to `examples/`, labelled as a demo artifact

**Status: DONE.** Moved to `examples/drive_mcp_soak.py` with `examples/README.md`
and a header on the script itself. Kept below for the reasoning.

**Not a review finding**

The script replays derives through the writer over MCP so the daemon accumulates
interactions and can run a canonization pass. It produced the current Canonical
status. It ships, in `examples/`, marked for what it is. No `examples/` directory
exists yet, so this creates one.

The label has to be specific, because a vague one is worse than none. What the
header and the `examples/README.md` must say:

- The blast radius and GC gates were met genuinely.
- Stage 2 requires three distinct origin interactions, and **this script supplied
  them by replaying the same derives**. The interactions are real interactions;
  they are not real *work*.
- It exists because the default cadence puts canonization out of reach of any
  ordinary session: GC runs every `gc_interval` mutations (10 000 by default) and
  Stage 1 needs `gc_survived >= 3`, so a concept needs 30 000 mutations before it
  can be promoted. `lambo demo` sets the same knob to 1 internally for the same
  reason.
- Anyone reproducing the exhibit's Canonical status will have used this, and
  should know that.

Also folded in here: T3-1-P3-4 proposes a persistent MCP connection in place of
per-call subprocess forks. This script is already that connection, so it is the
natural place for the pattern to live.

---

## T11 — Release workflow builds against too-new glibc

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

## T12 — Clean redeploy from scratch

**Blocked by:** T6
**Blocks:** T13, T14

The running instance was repaired partly by hand while the launcher was being
fixed. It is **not** a clean product of the current script, so "rebuildable from
the scripts alone" is a claim that cannot currently be made. A launch from zero
is the only thing that proves it.

Pipeline is a container build against Debian bookworm, `scp`, service restart,
about four minutes.

The exhibit runs x86_64 on Ubuntu 26.04 and that is the shipped path. The
launcher's arm64 branch is not exercised and is not being validated.

---

## T13 — Recording and video

**Files:** `scripts/recording/*`
**Blocked by:** T8 (the script has never run), T12 (a redeploy restarts the
service mid-capture)

Playwright drives the portal and records the browser context directly, which
sidesteps this machine's broken screen capture: `ffmpeg -f x11grab` returns a
black frame on KDE Wayland, and the xdg-desktop-portal ScreenCast request times
out. Terminal footage uses `vhs`, which renders a scripted session to video with
no compositor involved. Neither needs a working screen recorder.

---

## T14 — Docs and submission text

**Files:** `README.md`, `site/src/content/docs/hackathon.mdx`, `docs/plans/multi-agent-cloudops-aws-plan.md` §11
**Blocked by:** T12, and anything in T6 that changes the stack

- README still says `AWS services used: None yet`.
- `hackathon.mdx` still carries three `Not yet` rows.
- §11 still describes a `t4g.micro` on Graviton and a public Lambda Function
  URL. The exhibit is an `m7i-flex.large` on x86_64 running Ubuntu 26.04, and
  the Function URL returns 403.

Draft freely. Do not land until nothing else will move, or it gets rewritten
twice.

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
T9     portal          (panel on a fixture until T3)
T10    examples/ move
T11    release workflow
T14    docs            (drafting only)
```

The chains:

```
T1 ──► T2
T3 ──► T9      panel shows real data
T5 ──► T6 ──► T12 ──► T13     redeploy, then capture
T8 ──────────────────► T13
T12 ─────────────────► T14    docs land last
```

Critical path to a submission: **T5 → T6 → T12 → T13**, with **T8** joining
before T13.
