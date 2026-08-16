# Adversarial Review: Tier 3 (Part 1) — CloudOps Scaffolding & Agent Scripts

```text
╔══════════════════════════════════════════════════════════════════════════╗
║  STATUS: FINDINGS — Tier 3 CloudOps Scaffolding & Agent Scripts          ║
║  Verdict: FINDINGS (1 P1 / 4 P2 / 4 P3)                                   ║
║  Scope:   scripts/cloudops/_lambo.py (529 lines)                         ║
║           scripts/cloudops/01_network_agent.py (600 lines)               ║
║           scripts/cloudops/02_app_data_agent.py (591 lines)              ║
║  Tree:    main @ 2f4ca3f / branch task/cloudops-agents                      ║
║  Opened:  2026-08-16 · Reviewed: 2026-08-16                              ║
╚══════════════════════════════════════════════════════════════════════════╝
```

## Review Scope & Context

- **Task:** Tier 3 (Part 1) — CloudOps Scaffolding & Agent Scripts Review
- **Plan Reference:** `docs/plans/multi-agent-cloudops-aws-plan.md` (revision 2), §3 Track 1 & Track 2
- **Specification:** `lambo-hackathon-spec-v0.1.md` §2.2 (Single-Writer Lease), §5 (Graph Schema & Edge Types), §6 (Inspect & Recall), §7 (Action Recording)
- **Target Files:**
  - `scripts/cloudops/_lambo.py` (529 lines) — Shared CLI subprocess driver, output parsing, graph validation
  - `scripts/cloudops/01_network_agent.py` (600 lines) — Track 1 Network infrastructure discovery and graph derivation
  - `scripts/cloudops/02_app_data_agent.py` (591 lines) — Track 2 App & Data infrastructure discovery, cross-tier edge linking
- **Method:**
  - Clause-by-clause adversarial static inspection and AST validation (`py_compile`)
  - Subprocess execution, protocol framing, buffer reads, timeouts, signals, and failure handling analysis
  - Edge graph topology tracing against Lambo core (`src/graph/{derive,action,canonical}.rs`, `src/cli/inspect.rs`, `src/recall/format.rs`)
  - Cross-agent coordination, state prerequisite verification, and error propagation analysis

---

## Verdict: FINDINGS (1 × P1, 4 × P2, 4 × P3)

---

## Verified and Sound (No Findings)

- **Strict Dry-Run Isolation:** All scripts strictly adhere to Rule 2: `--dry-run` makes zero AWS API calls, instantiates no boto3 clients, and executes no Lambo subprocesses (`_plan` returns immediately).
- **Single-Writer Lease Awareness:** Clear distinction between mutating CLI verbs (`derive`, `record-action`) which acquire the session writer lease and reader verbs (`recall`, `inspect`, `stats`) which do not.
- **Single-Source Invariant Enforcement:** `check_single_source` in `_lambo.py` correctly detects and rejects multiple hierarchy parents on the same concept, protecting blast-radius calculations from silent zeroing.
- **Tag-Based AWS Inventory:** Discovery across both agents relies solely on `Project=lambo-cloudops` tags, maintaining state independence and enabling idempotent reconciliation.
- **AWS Credentials and Secrets Safety:** Zero plaintext secret reads; `SECRET_NAME` is checked for existence only and never read via `get_secret_value`.

---

## Detailed Findings

---

### Finding T3-1-P1-1 — Unintended Cross-Tier Co-Occurrence Coupling in `02_app_data_agent.py` Violates VPC Isolation Architecture

- **Severity:** **P1**
- **File:** `scripts/cloudops/02_app_data_agent.py:350-380` (`derive_topology`)
- **Affects:** Plan §2 (Architecture), Plan §7 (Isolation), `src/graph/derive.rs:180-195` (CoOccurrence edge generation)
- **Detail:**
  In `02_app_data_agent.py`, `derive_topology` bundles `DB_SUBNET_GROUP`, `RDS_NAME`, `LAMBDA_NAME`, and `STATS_ROLE_NAME` into a single `lam.derive()` call:
  ```python
  if not skip_rds:
      root = DB_SUBNET_GROUP
      pairs.append((VPC_NAME, DB_SUBNET_GROUP))
      concepts.append(f"{RDS_NAME}:entity")
      pairs.append((SG_BASE_NAME, RDS_NAME))
  if not skip_lambda:
      if root is None:
          root = LAMBDA_NAME
      else:
          concepts.append(f"{LAMBDA_NAME}:entity")
      concepts.append(f"{STATS_ROLE_NAME}:entity")
  ...
  out = lam.derive(AGENT_APP_DATA, root, "entity", concepts=concepts, parent_of=pairs)
  ```
  In Lambo's graph engine (`src/graph/derive.rs`), **all concepts created within the same derivation interaction automatically receive pairwise `CoOccurrence` edges**.
  As a direct consequence, `derive_topology` creates `CoOccurrence` edges between `Lambda-LamboStats-API` and `RDS-Lambo-Demo-DB`, as well as between `Lambda-LamboStats-API` and `SG-Base-VPC`.
- **Impact:**
  This directly violates the foundational architectural rule of the design (Plan §2, §7): the Lambda function is strictly decoupled from the VPC, runs outside the VPC, and has no dependency or relationship with the private database or internal security groups. Creating artificial `CoOccurrence` edges causes semantic recall queries regarding the database to return the Lambda function and execution role, corrupting memory relevance and graph topology.
- **Remediation:**
  Split `derive_topology` into two separate, isolated `derive` calls:
  1. Derive the VPC-internal data tier (`DB_SUBNET_GROUP`, `RDS_NAME`).
  2. Derive the external serverless tier (`LAMBDA_NAME`, `STATS_ROLE_NAME`).

---

### Finding T3-1-P2-1 — Subprocess Error Detail Masking in `_lambo.py` (`_run`) Truncates Root-Cause Diagnostic Output

- **Severity:** **P2**
- **File:** `scripts/cloudops/_lambo.py:276-280` (`Lambo._run`)
- **Detail:**
  When a subprocess command fails, `_run` parses standard error by selecting only the final line:
  ```python
  if proc.returncode != 0:
      detail = (proc.stderr or proc.stdout or "").strip().splitlines()
      last = detail[-1] if detail else f"exit {proc.returncode}"
      raise InfraError(f"`lambo {verb}` failed: {last}", hint=_conflict_hint(proc.stderr))
  ```
  In Rust CLI applications (such as Clap-based CLIs), argument parsing errors or subcommand errors format with the actual error message on line 1, followed by multi-line usage text, ending with:
  `For more information, try '--help'.`
  Consequently, `detail[-1]` evaluates to `"For more information, try '--help'."`, completely discarding the underlying error message (e.g. `error: unexpected argument '--foo' found` or `error: invalid value for '--kind'`).
- **Impact:**
  Operators and automated agents receive completely uninformative error messages (`InfraError: `lambo derive` failed: For more information, try '--help'.`), severely hindering debugging and violating Rule 3 ("Fail with a sentence, not a traceback").
- **Remediation:**
  Capture the first non-empty line of error output (`detail[0]`) or join the full error block if short, rather than naively selecting `detail[-1]`.

---

### Finding T3-1-P2-2 — Unhandled `PermissionError` and `OSError` in `_lambo.py` (`resolve_lambo_binary` / `_run`)

- **Severity:** **P2**
- **File:** `scripts/cloudops/_lambo.py:193-214` (`resolve_lambo_binary`), `262-275` (`_run`)
- **Detail:**
  `resolve_lambo_binary` verifies only `path.is_file()`, but does not verify executable permissions via `os.access(path, os.X_OK)`.
  In `Lambo._run`, the exception handler catches only `FileNotFoundError` and `subprocess.TimeoutExpired`. If the target binary exists but lacks execute permissions (`chmod -x`) or encounters an execution permission error, `subprocess.run` raises `PermissionError` (a subclass of `OSError`).
- **Impact:**
  `PermissionError` escapes unhandled, dumping a raw Python traceback onto standard error and crashing the agent process, violating Project Rule 3.
- **Remediation:**
  Check `os.access(candidate, os.X_OK)` during binary resolution, and catch `OSError` inside `_run`, raising a clean `InfraError` with an actionable hint (`chmod +x <path>`).

---

### Finding T3-1-P2-3 — Silent Omission of IPv6 Security Group Rules Due to Colon-Incompatible CLI Protocol

- **Severity:** **P2**
- **File:** `scripts/cloudops/01_network_agent.py:405-413` (`derive_security_rules`), `scripts/cloudops/_lambo.py:358-374` (`_refuse_colon`)
- **Detail:**
  The CLI syntax `--parent-of CHILD:PARENT` parses on the single colon character without escaping support. To prevent ambiguous parsing, `_lambo.py` enforces `_refuse_colon`.
  To work around this limitation, `01_network_agent.py` silently drops any security group rule containing a colon:
  ```python
  for sg_name, text in net.rules:
      if ":" in text:
          skipped("constraint", text, "contains a colon, so it cannot be a hierarchy end")
          continue
      usable.append((sg_name, text))
  ```
  Any IPv6 CIDR ingress/egress rules (e.g. `2001:db8::/64` or `::/0`) are silently discarded from the graph.
- **Impact:**
  Dual-stack or IPv6 security group rules are omitted from Lambo's graph memory. Because security group rules constitute the primary dependent children establishing `SG-Base-VPC`'s blast radius, dropping rules skews blast-radius calculations and creates a discrepancy between AWS account state and graph memory.
- **Remediation:**
  Do not use the raw rule text as a hierarchy child in `--parent-of`; instead, represent IPv6 constraints via `record-action --depends-on` or sanitize colon delimiters.

---

### Finding T3-1-P2-4 — Topology Prerequisite Verification in `02_app_data_agent.py` Vulnerable to `MAX_INSPECT_NODES` Truncation

- **Severity:** **P2**
- **File:** `scripts/cloudops/02_app_data_agent.py:302-327` (`read_network_topology`)
- **Detail:**
  `02_app_data_agent.py` checks that `01_network_agent.py` has completed by running `lam.inspect(VPC_NAME, depth=1)` and verifying that `REQUIRED_FROM_NETWORK_AGENT = (SG_BASE_NAME, SUBNET_PRIVATE_NAME)` are returned in `parse_outbound_neighbours(text)`.
  However, in `src/cli/inspect.rs`, the breadth-first neighbourhood search enforces a hard budget of `MAX_INSPECT_NODES = 64`.
  In realistic production VPCs containing more than 64 direct child concepts (subnets, route tables, endpoints, security groups), the hop 1 list truncates once the budget is exhausted.
- **Impact:**
  Because graph adjacency traversal order is non-deterministic (dependent on hash set iteration), required nodes such as `SG-Base-VPC` may be omitted from the truncated hop 1 output, causing `read_network_topology` to raise a false-positive `InfraError` and abort the agent run.
- **Remediation:**
  Verify prerequisite nodes by inspecting each required concept directly (`lam.inspect(SG_BASE_NAME, depth=0)` and `lam.inspect(SUBNET_PRIVATE_NAME, depth=0)`) rather than scanning a bounded BFS traversal of the parent VPC.

---

### Finding T3-1-P3-1 — Nested Closure Re-Allocation of `_peer_label` in `01_network_agent.py`

- **Severity:** **P3**
- **File:** `scripts/cloudops/01_network_agent.py:230-246`
- **Detail:**
  `_peer_label` is defined inside the inner body of `_rule_texts(sg_name, group, name_by_id)`. `_rule_texts` is invoked repeatedly across all security groups during discovery, recreating the closure object on each call.
- **Impact:** Unnecessary function object allocation and poor code modularity.
- **Remediation:** Hoist `_peer_label` to a top-level module function.

---

### Finding T3-1-P3-2 — Fragile Bracket Splitting in `parse_outbound_neighbours` (`_lambo.py`)

- **Severity:** **P3**
- **File:** `scripts/cloudops/_lambo.py:506-508` (`parse_outbound_neighbours`)
- **Detail:**
  `names.append(stripped[3:].rsplit(" [", 1)[0].strip())` parses concept labels under the assumption that concept content never contains `" ["`. If concept content includes bracketed text (e.g. `"[prod] rule"`), `rsplit` truncates the concept content. Furthermore, interaction nodes rendered as `-> <interaction UUID>` lack ` [` and are incorrectly appended as raw interaction strings.
- **Impact:** Risk of corrupted concept names or raw interaction IDs in parsed neighbour lists.
- **Remediation:** Strip `<interaction ...>` lines and use structured regex pattern matching for `content [Type, status]` tokens.

---

### Finding T3-1-P3-3 — Inaccurate Edge Reinforcement Documentation in `02_app_data_agent.py`

- **Severity:** **P3**
- **File:** `scripts/cloudops/02_app_data_agent.py:495-506` (`record_actions`)
- **Detail:**
  The docstring in `record_actions` claims: `"The edges repeat ones written above; upsert_edge reinforces rather than duplicating..."`
  In `src/graph/action.rs`, each `record_action` invocation generates a distinct `action_node` concept. The edges created have `source = action_node_N`. Because the source node IDs differ, separate edges are created rather than reinforcing a single edge ID.
- **Impact:** Misleading technical comment regarding underlying graph mechanics.
- **Remediation:** Clarify docstring to state that repeating target dependencies creates multiple distinct inbound edges from separate origin actions, satisfying Stage 2 distinct-interaction requirements.

---

### Finding T3-1-P3-4 — Subprocess Fork Overhead vs Persistent MCP Connection

- **Severity:** **P3**
- **File:** `scripts/cloudops/_lambo.py:222-356` (`Lambo` class)
- **Detail:**
  The `Lambo` helper executes every graph mutation by spawning a separate CLI process via `subprocess.run()`. Each call spins up a tokio runtime, connects to Cockroach/SQLite, acquires the writer lease, mutates, flushes, releases the lease, and exits.
- **Impact:** Significant process startup latency across batch operations and inability to run concurrently with a long-lived `lambo serve` process due to writer lease exclusivity.
- **Remediation:** Provide an optional MCP streamable HTTP transport adapter (mirroring `scripts/cloudops/drive_mcp_soak.py`) for live agent deployments.

---

## Disposition & Summary

| Finding ID | Severity | File | Core Defect |
|---|---|---|---|
| **T3-1-P1-1** | **P1** | `02_app_data_agent.py:350-380` | Bundled `derive` creates invalid cross-tier `CoOccurrence` edges between Lambda and RDS |
| **T3-1-P2-1** | **P2** | `_lambo.py:276-280` | `_run` takes `detail[-1]`, truncating root-cause error lines from Clap/Rust |
| **T3-1-P2-2** | **P2** | `_lambo.py:193-214, 262-275` | Missing executable permission check and unhandled `PermissionError` |
| **T3-1-P2-3** | **P2** | `01_network_agent.py:405-413` | IPv6 security group rules silently discarded due to CLI colon delimiter limitation |
| **T3-1-P2-4** | **P2** | `02_app_data_agent.py:302-327` | Topology prerequisite verification vulnerable to `MAX_INSPECT_NODES` (64) BFS truncation |
| **T3-1-P3-1** | **P3** | `01_network_agent.py:230-246` | Inner closure `_peer_label` reallocated per security group |
| **T3-1-P3-2** | **P3** | `_lambo.py:506-508` | Fragile bracket splitting in `parse_outbound_neighbours` |
| **T3-1-P3-3** | **P3** | `02_app_data_agent.py:495-506` | Inaccurate docstring regarding edge reinforcement |
| **T3-1-P3-4** | **P3** | `_lambo.py:222-356` | Subprocess fork overhead vs persistent MCP connection |

**Final Verdict:** **FINDINGS (1 P1 / 4 P2 / 4 P3)**. P1 finding (T3-1-P1-1) must be remediated to preserve strict architectural boundary isolation in Lambo's graph memory.
