# lambo-cloudops — shared-memory safety rules for cloud-ops agents

**What this skill is.** Lambo is the shared memory layer for this multi-agent
cloud-ops environment. Agents record what they build and depend on; Lambo
derives a structural graph, promotes trustworthy facts to `Canonical` status
from structural evidence (blast radius, age-gated interactions, GC survival,
coverage), and returns recall results that carry the safety warnings an agent
needs before touching anything other agents rely on. This skill makes that
protection machine-executable: it tells any LLM agent (Claude Code, Cursor,
Antigravity, OMP) exactly when to check memory, what a blocking warning looks
like, and how to keep the graph honest while provisioning AWS infrastructure.

The rule this encodes is the product's central claim: **status is earned from
structural evidence, never declared by an agent** — and a modification that
would break a load-bearing resource is caught before it happens.

---

## 0. Session and surfaces

- Every command targets one session. Use the session id from the environment
  (`LAMBO_SESSION`) or pass `--session <SESSION>` explicitly.
- Two surfaces, same contract:
  - **CLI**: `lambo recall`, `lambo derive`, `lambo record-action`,
    `lambo reserve`, `lambo inspect`, `lambo stats`.
  - **MCP** (preferred for tool-using agents): `lambo_recall`,
    `lambo_derive`, `lambo_record_action`, `lambo_reserve`, `lambo_inspect`,
    `lambo_saints`, `lambo_stats`. Every MCP tool takes your `agent_id`.
- Writers (`derive`, `record-action`, `reserve`) hold the session's
  single-writer lease. Sequence your writes; do not run two writer processes
  against one session. Readers are read-only and may run freely.
- Never send a timestamp. The server stamps interactions and edges; a
  client-supplied timestamp would corrupt the age gates that canonization
  depends on.

---

## 1. Pre-flight recall protocol — before any destructive command

**Before executing a destructive AWS command** (`delete-*`, `terminate-*`,
`disassociate-*`, `deregister-*`, `detach-*`, `release-*`, and any command
that modifies a shared resource), you MUST check what you are about to break:

1. Run recall against the resource you intend to modify:

   ```
   lambo recall --session <SESSION> --query "<resource being modified>"
   ```

   or the MCP equivalent `lambo_recall`.

2. **Halt if the result is a load-bearing pillar.** A hit whose block contains
   either of these lines is a pillar — other infrastructure depends on it:

   ```
   ⚑ Load-bearing pillar — N nodes depend on this. Modify with caution.
   ```

   (the `N` is the live blast radius; the hit's meta line reads
   `content [Kind, status] (score X, blast radius N)`).

   A pillar modification MUST be halted. Do not proceed. Either re-scope the
   change to a resource nothing depends on, or obtain an explicit human
   override and record it in the session before acting.

3. **Also treat as blocking:**

   ```
   Agent <other> wrote to it <n> seconds ago
   ```

   (a live collision notice: another agent is working in that resource) and
   `High-risk modification` lines (a high-value node modified within the
   conflict window).

4. **Fail closed.** If recall refuses or errors — embedding-contract mismatch,
   store unreachable, vector leg refused — you MUST halt. An answer you cannot
   trust is not an answer. Lambo deliberately refuses to rank against a vector
   space it cannot vouch for; treat that refusal as a stop signal.

5. **Re-check after topology changes.** Provisioning and derivation change
   blast radii. Re-run the protocol before each destructive action, not once
   per session.

---

## 2. Provenance and derivation protocol — after provisioning

Blast radius is only as honest as the graph. If you skip recording
dependencies, the protection is blind.

1. After creating or discovering a resource (EC2, RDS, Lambda, subnet, security
   group, VPC, API), record it as a concept:

   ```
   lambo derive --session <SESSION> --agent <AGENT> --content "<resource>" --kind resource
   ```

   or `lambo_derive` (MCP).

2. Record the action with its real dependencies — what would break if you
   deleted this:

   ```
   lambo record-action --session <SESSION> --agent <AGENT> \
     --action "<verb> <resource>" \
     --produces <resource-it-creates> ... \
     --modifies <resource-it-mutates> ... \
     --depends-on <resource-it-requires> ...
   ```

   or `lambo_record_action` (MCP). Example: an RDS instance `depends-on` its
   security group and subnets; a Lambda `depends-on` the RDS it queries.
   `depends-on` edges are what give a resource its blast radius — record them
   faithfully.

3. Writes are lease-held and exclusive; sequence them. A read only sees a
   write after that write's own call has returned — derive/record before the
   recall that is meant to see it.

---

## 3. CockroachDB direct inspection — why a resource is canonical

When you need to know why a resource earned (or lost) `Canonical` status,
query the Lambo CockroachDB schema directly (CockroachDB Cloud MCP or SQL):

- `canonization_events` — `(id, session_id, node_id, from_status, to_status,
  blast_radius, last_demotion_time, occurred_at)`. The promotion/demotion
  audit trail. Canonical is earned by the daemon from structural evidence;
  no agent performs or declares it.
- `concepts` — the graph nodes (`embedding VECTOR(1024)`, plus the embedding
  kind/model/dim columns that pin which model wrote the vectors).
- `edges` — structural relationships; blast radius counts live dependents.

A resource is trustworthy (`Canonical`) when its promotion row shows the
gates being met: blast radius above the configured bar, survival across GC,
age-gated interaction counts, coverage. A `Venerable` or `Candidate` resource
is not yet load-bearing; a demoted one (`status` reset to `None`) is cooling
down after failing those gates.

---

## 4. Verifying this skill

1. Run a recall against the live session and confirm the load-bearing pillar
   line appears for the exhibit's pillar resource.
2. Query `canonization_events` and confirm the pillar's promotion row and its
   blast radius.
3. Draft the destructive command for the pillar, run the pre-flight protocol,
   and confirm you halt on the warning.

---

## Honest boundaries

- Recall is a snapshot; blast radius is live at read time.
- This skill gates the agent's actions. It cannot stop a command issued
  outside the agent harness, and it is not a substitute for AWS IAM
  authorization — it is the memory layer on top of it.
- The vector leg fails closed: a degraded embedder or a mismatched embedding
  contract refuses to rank rather than returning plausible-but-meaningless
  results. Treat refusal as a halt signal (protocol 1.4).
- Canonization gates run on aged edges; on a young session nothing is old
  enough to count, so gates can read zero while the live blast radius is
  non-zero. When in doubt, trust the live blast radius in the pillar warning,
  not an absence of gate progress.
