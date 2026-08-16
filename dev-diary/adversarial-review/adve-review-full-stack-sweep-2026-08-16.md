# Adversarial Review: Whole-System Sweep (Tiers 1, 2, 3)

```text
╔══════════════════════════════════════════════════════════════════════╗
║  STATUS: FINDINGS — Whole-System Multi-Agent Sweep                   ║
║  Verdict: FINDINGS (4 P1 / 12 P2 / 10 P3)                            ║
║  Scope:   Tier 1 (Product binary & Daemon config)                    ║
║           Tier 2 (AWS Exhibit EC2 launch & Llama provisioning)       ║
║           Tier 3 (CloudOps agents 01-02 & Climax protection script)  ║
║  Gates:   fmt [x] clippy x3 [x] test 718 [x]                         ║
║  Opened:  2026-08-16 · Reviewed: 2026-08-16                          ║
╚══════════════════════════════════════════════════════════════════════╝
```

**Task:** Consolidated whole-system adversarial review covering Tiers 1, 2, and 3 across dedicated isolated worktrees.
**Method:** 4 independent subagents executed deep, adversarial audits against live code, type contracts, build scripts, AWS cloud-init generation, JSON-RPC MCP protocols, and fail-closed safety invariants.

---

## Executive Summary & Scorecard

| Tier | Component | Scope | Verdict | Key Landmines |
|---|---|---|---|---|
| **Tier 1** | Rust Core & Daemon | `src/config.rs`, `src/resolve.rs`, `src/mcp/serve.rs`, `src/cli/mod.rs`, `src/cli/serve_web.rs` | **2 P1, 2 P2, 1 P3** | `Config::validate()` dead in prod; Tokio runtime panic on zero interval; `gc_interval = 0` locks daemon in continuous write loop; `deny_unknown_fields` rejects `[daemon]`. |
| **Tier 2** | AWS Exhibit EC2 | `scripts/aws-infra/launch_exhibit_ec2.py` | **1 P1, 4 P2, 4 P3** | In-band `llama.cpp` source compile on 2-vCPU instances causes OOM killer aborts & 14-min boot latency; closed Port 80 in SG-PublicWeb drops HTTP->HTTPS redirects. |
| **Tier 3 (Part 1)** | CloudOps Scaffolding & Agents | `scripts/cloudops/_lambo.py`, `01_network_agent.py`, `02_app_data_agent.py` | **1 P1, 4 P2, 4 P3** | `02_app_data_agent.py` derives Lambda + DB in single call, spuriously creating `CoOccurrence` edge violating VPC isolation; Clap error details truncated by `detail[-1]`. |
| **Tier 3 (Part 2)** | Demo Climax Script | `scripts/cloudops/03_crossover_protect.py` | **0 P1, 2 P2, 1 P3** | Missing focus on empty session crashes via `InfraError` instead of rendering unprotected state (P2); non-dependent siblings leak into stranded list (P2). Mutation invariant verified CLEAN. |

---

## Consolidated Findings Catalog

### P1 Findings (Critical / Blocker)

#### T1-P1-1: `Config::validate()` is Dead Code in Production Paths, Admitting Zero-Duration Timers that Panic the Tokio Runtime
- **Where:** `src/config.rs:177-195`, `src/memory.rs:524-565`, `src/daemon/mod.rs:671`, `src/canon/task.rs:232`, `src/store/flush.rs:380`.
- **Detail:** Tokio's `tokio::time::interval(period)` panics immediately with `period must be non-zero` if `period == Duration::ZERO`. `Config::validate()` is only called in unit tests and is completely omitted during `MemoryBuilder::build()` and `resolve_backends()`.
- **Impact:** An invalid or unvalidated zero-duration duration parameter panics background tasks upon startup.
- **Remediation:** Call `config.validate()?` at the start of `MemoryBuilder::build()`, and validate that all intervals (`daemon_tick_interval`, `backend_flush_interval`, `canonization_eval_interval`) are `> Duration::ZERO`.

#### T1-P1-2: `gc_interval = 0` is Unvalidated and Triggers Continuous GC Scans on Every 1s Tick
- **Where:** `src/config.rs:115`, `src/daemon/mod.rs:865`.
- **Detail:** In `daemon/mod.rs:865`, periodic GC evaluates `epoch.saturating_sub(cs.last_gc_epoch) >= params.gc_interval`. When `gc_interval == 0`, this condition evaluates to `true` continuously on every 1s daemon tick even when 0 mutations occurred.
- **Impact:** Continuous graph read/write lock contention and CPU starvation while idle.
- **Remediation:** Enforce `gc_interval >= 1` in `Config::validate()`.

#### T2-P1-1: In-Band `llama.cpp` Source Compilation at Boot on 2-vCPU Instances Risks OOM Killer Aborts and Drastic Boot Latency
- **Where:** `scripts/aws-infra/launch_exhibit_ec2.py:347-362` (`LLAMA_BLOCK`).
- **Detail:** Cloud-init invokes `cmake --build build --config Release --target llama-server -j "$(nproc)"` during instance bootstrap. On a 2-vCPU Graviton `t4g.medium` (4 GiB RAM), compiling matrix/template C++ files saturates memory and triggers Linux OOM killer termination (`cc1plus: fatal error: Killed`). Furthermore, compilation takes 8–14 minutes, far exceeding the documented 2–4 minute estimate.
- **Impact:** Boot failure, hung exhibit provisioning, operator timeout.
- **Remediation:** Package and fetch pre-built `llama-server` release tarballs verified by SHA-256, or enforce `t4g.large` minimum with realistic bootstrap timeout.

#### T3-1-P1-1: Cross-Tier Co-Occurrence Coupling in `02_app_data_agent.py` Violates VPC Isolation Architecture
- **Where:** `scripts/cloudops/02_app_data_agent.py:350-380`.
- **Detail:** `derive_topology` derives `DB_SUBNET_GROUP`, `RDS_NAME`, `LAMBDA_NAME`, and `STATS_ROLE_NAME` inside a single `lam.derive()` call. Under `src/graph/derive.rs`, all concepts co-derived within one interaction receive pairwise `CoOccurrence` edges.
- **Impact:** Synthesizes an artificial graph edge linking serverless Lambda to the private RDS database, directly contradicting the architectural assertion that Lambda is isolated from private VPC subnets.
- **Remediation:** Split `derive_topology` into two separate `derive` calls (one for VPC database resources, one for the external Lambda tier).

---

### P2 Findings (High / Robustness / Contract Violations)

1. **T1-P2-1 (`src/config.rs:207-213`, `src/cli/mod.rs:63-75`)**: `LamboFile` fails closed with `unknown field daemon` on `[daemon]` due to `#[serde(deny_unknown_fields)]`. Meanwhile, CLI verbs silently ignore `[daemon]` settings because `open_writer()` always uses `Config::default()`.
2. **T1-P2-2 (`src/resolve.rs:12-19`)**: `ResolvedBackends` lacks `#[non_exhaustive]`, creating a breaking API surface for library consumers when fields are added.
3. **T2-P2-1 (`scripts/aws-infra/launch_exhibit_ec2.py:480-499`)**: Port 80 is closed by default in `SG-PublicWeb`, breaking plain HTTP-to-HTTPS browser redirects and ACME HTTP-01 fallbacks for judges accessing `http://<fqdn>`.
4. **T2-P2-2 (`scripts/aws-infra/launch_exhibit_ec2.py:939-951`)**: `--bge-model-url` can be overridden without passing `--bge-model-sha256`, causing cloud-init boot checksum mismatch and instant daemon crash-loop.
5. **T2-P2-3 (`scripts/aws-infra/launch_exhibit_ec2.py:672-687`)**: EC2 IAM retry loop catches generic `InvalidParameterValue`, obscuring real configuration errors for 60 seconds with misleading IAM propagation hints.
6. **T2-P2-4 (`scripts/aws-infra/launch_exhibit_ec2.py:10-13, 123-127`)**: Docstrings and sizing comments claim ARM is chosen for cost rather than model portability and cite "source build space" despite packaging evolution.
7. **T3-1-P2-1 (`scripts/cloudops/_lambo.py:276-280`)**: `_run` selects `detail[-1]` on CLI failure, discarding Clap's primary error message and printing only `"For more information, try '--help'."`.
8. **T3-1-P2-2 (`scripts/cloudops/_lambo.py:193-214`)**: Missing executable permissions check on resolved binary path, causing unhandled `PermissionError` traceback.
9. **T3-1-P2-3 (`scripts/cloudops/01_network_agent.py:405-413`)**: IPv6 CIDRs (`2001:db8::/64`) are silently dropped because CLI `--parent-of CHILD:PARENT` cannot carry colons (`_refuse_colon`).
10. **T3-1-P2-4 (`scripts/cloudops/02_app_data_agent.py:302-327`)**: Network prerequisite verification uses `lam.inspect(VPC_NAME, depth=1)` which is bounded by `MAX_INSPECT_NODES = 64`. On graphs with >64 nodes, BFS truncation spuriously omits `SG-Base-VPC`.
11. **T3-2-P2-1 (`scripts/cloudops/03_crossover_protect.py:249`)**: `run_guard` crashes with `InfraError` on unpopulated/empty sessions because `lambo inspect` returns exit code 1 (`no concept matching 'SG-Base-VPC'`), preventing `render_unprotected()` from running cleanly.
12. **T3-2-P2-2 (`scripts/cloudops/_lambo.py:461-513`)**: `parse_outbound_neighbours` includes `CoOccurrence` siblings, displaying network subnets as "stranded dependents" during security group deletion abort banners.

---

### P3 Findings (Medium / Hygiene / Performance)

1. **T1-P3-1 (`src/cli/serve_web.rs:200-211`, `src/mcp/serve.rs:273-284`)**: Constant-time token comparator leaks input length via loop iteration count.
2. **T2-P3-1 (`scripts/aws-infra/launch_exhibit_ec2.py:826-849`)**: Ephemeral public IP race condition on re-adopted pending instances.
3. **T2-P3-2 (`scripts/aws-infra/launch_exhibit_ec2.py:277, 313, 396`)**: Inconsistent systemd restart policies (`caddy.service` uses `Restart=on-failure` while others use `Restart=always`).
4. **T2-P3-3 (`scripts/aws-infra/launch_exhibit_ec2.py:208-211, 349-350`)**: System users created without deterministic static UIDs/GIDs.
5. **T2-P3-4 (`scripts/aws-infra/launch_exhibit_ec2.py:245-253`)**: Health check in `lambo-serve-web` polls for 300s even if `llama-server` has died.
6. **T3-1-P3-1 (`scripts/cloudops/01_network_agent.py:230-246`)**: Inner closure `_peer_label` reallocated on every security group iteration.
7. **T3-1-P3-2 (`scripts/cloudops/_lambo.py:506-508`)**: `rsplit(" [", 1)` truncates bracketed concept text.
8. **T3-1-P3-3 (`scripts/cloudops/02_app_data_agent.py:495-506`)**: Inaccurate docstring regarding edge reinforcement semantics.
9. **T3-1-P3-4 (`scripts/cloudops/_lambo.py:222-356`)**: O(N) Subprocess fork overhead vs persistent MCP connection.
10. **T3-2-P3-1 (`scripts/cloudops/_lambo.py:184-214`)**: `resolve_lambo_binary` prioritizes stale `target/release/lambo` over debug builds with newly enabled features.

---

## Action Plan & Remediation Order

1. **Immediate Tier 1 Fixes (Rust Binary Safety)**:
   - Wire `Config::validate()` into `MemoryBuilder::build()` and enforce `gc_interval >= 1` and non-zero timer durations.
   - Add `#[non_exhaustive]` to `ResolvedBackends`.
   - Update `LamboFile` or document that `[daemon]` is process-level.

2. **Immediate Tier 2 Fixes (AWS Launch Script)**:
   - Ensure `SG-PublicWeb` opens port 80 for HTTP->HTTPS redirection.
   - Cross-validate `--bge-model-url` and `--bge-model-sha256`.
   - Update boot time guidance or supply pre-built llama release assets.

3. **Immediate Tier 3 Fixes (CloudOps & Demo Climax)**:
   - In `02_app_data_agent.py`, split `derive_topology` into two `derive()` calls to eliminate spurious Lambda <-> RDS `CoOccurrence` edges.
   - In `03_crossover_protect.py`, catch `InfraError` / missing concept during `run_guard` so fresh sessions exit 1 with `render_unprotected()`.
   - In `_lambo.py`, fix `_run` error string selection so Clap error lines are preserved.
