# `scripts/cloudops`: the two agents, and the outage they prevent

Phase 4 of `docs/plans/multi-agent-cloudops-aws-plan.md` (revision 2). Three
scripts play the plan's §3 workflow against the stack `scripts/aws-infra/` built:
one agent records the network foundation into Lambo, a second attaches its
workloads and records what they depend on, and a third attempts to delete the
shared security group and is stopped.

**These scripts create, modify and delete nothing in AWS.** Every AWS call they
make is a `describe-*` or a `get-*`. Provisioning is Phase 3's job, and if a
resource is missing the error names the script that creates it.

Read the plan's §3 (the two tracks and the climax) and §6 (why RDS is not a Lambo
store) first. Read `scripts/aws-infra/README.md` too: these are its siblings and
they share its conventions, its `_common.py`, and its resource names.

---

## Prerequisites

| | |
|---|---|
| Python | 3.12+ |
| Packages | `boto3`. Nothing else. |
| Binary | `lambo` on PATH, or a `cargo build --release` in this repo. `--lambo-bin` overrides. |
| Store | Whatever `lambo.toml` / `LAMBO_CONFIG` selects. `--lambo-config` chooses the file; no DSN is ever passed on a command line. |
| Credentials | The ordinary boto3 chain. **No profile, account id, access key or DSN is hardcoded anywhere in this directory.** |
| AWS stack | `provision_network.py` and `provision_app_data.py` have run. |

### IAM permissions the running identity needs

Read-only, and nothing more: EC2 `Describe*` (VPCs, subnets, route tables,
internet gateways, NAT gateways, security groups, instances), RDS
`DescribeDBInstances` / `DescribeDBSubnetGroups`, Lambda `GetFunction`, IAM
`GetRole`, Secrets Manager `DescribeSecret` (**not** `GetSecretValue`; nothing
here reads the DSN), and STS `GetCallerIdentity`.

---

## Order to run things

```sh
SESSION="<the lambo session id the exhibit shows>"

# 1. The network agent records the foundation.
python3 scripts/cloudops/01_network_agent.py --session "$SESSION"

# 2. The app-data agent reads that back and attaches its workloads to it.
python3 scripts/cloudops/02_app_data_agent.py --session "$SESSION"

# 3. The network agent tries to delete the shared security group, and does not.
python3 scripts/cloudops/03_crossover_protect.py --session "$SESSION"
```

Every script takes `--dry-run`, which prints the plan and **makes no AWS API call
at all, and runs no `lambo` subprocess either**. It works with no credentials,
no session, and no built binary.

Every script is **idempotent**. Concept contents canonicalize to the same nodes
and edges reinforce rather than duplicate, so a second run of 01 and 02 reports
`0 created, N matched existing` across the board and leaves the concept count and
every blast radius unchanged.

### The writer lease

`01` and `02` use `lambo derive` and `lambo record-action`, which are **writers**:
each invocation takes the session's single-writer lease and releases it on exit.
**Stop `lambo serve` on this session before running them**, or the first call
fails with a conflict naming the current holder. The scripts turn that into a
sentence rather than a traceback.

`03` uses `recall`, `inspect` and `stats` only, which are lease-free readers. It
is safe to run beside a live `lambo serve` and beside the judge portal, and that
is the point: the guard has to work while the system is running.

---

## What each script records

### `01_network_agent.py`

Plays plan §3 Track 1. Discovers, by tag:

| Discovered | Recorded as |
|---|---|
| `VPC-Enterprise-Prod` | `entity`, the tier root |
| `Subnet-Public-1a`, `Subnet-Private-1a`, `Subnet-Private-1b`, `InternetGateway`, `RouteTable-Public`, `SG-Base-VPC`, `SG-PublicWeb` | `entity`, each a `parent_of` child of the VPC |
| the VPC's CIDR | `constraint` under the VPC |
| whether a NAT gateway exists | `constraint` under the VPC, and a loud warning if one does |
| each resource's live AWS id | `entity` under that resource, as `<Name> = <id>` |
| every ingress and egress rule on both security groups | `constraint` under its group |
| `lambo/cockroach-dsn` | `resource`, no parent; existence only, the value is never read |
| `EC2-LamboWebExhibit` | `entity` under `Subnet-Public-1a`, only if the exhibit is running |

Then four to six `record-action` calls, one per provisioning step; the secret
and the exhibit each add one when they exist.

Arguments: `--session` (**required**), `--lambo-bin`, `--lambo-config`, plus the
common `--region`, `--profile`, `--dry-run`.

### `02_app_data_agent.py`

Plays plan §3 Track 2, including its step 3 "Cross-Over Binding": it queries
Lambo before it writes.

| Discovered | Recorded as |
|---|---|
| `lambo-cloudops-db-subnets` | `entity` under `VPC-Enterprise-Prod` |
| `rds-lambo-demo-db` | `entity` under **`SG-Base-VPC`** (see the design notes) |
| its endpoint, public accessibility, class and engine | `entity` / `constraint` under the database |
| `Lambda-LamboStats-API` | `entity`, **no parent**; it is outside the VPC |
| its runtime, architecture, and that it has no `VpcConfig` | `constraint` under the function |
| `lambo-cloudops-stats-role`, `lambo-cloudops-exhibit-role` | `entity`, no parent |

The edges, which are the reason this script exists (plan §3 Track 2 step 3
verbatim):

* `app-data-agent provisioned RDS-Lambo-Demo-DB in Subnet-Private-1a`
  depends on `VPC-Enterprise-Prod`, `Subnet-Private-1a`, `SG-Base-VPC`.
* `app-data-agent deployed Lambda-LamboStats-API outside VPC-Enterprise-Prod`
  produces the function and its roles, and depends on `lambo/cockroach-dsn` and
  **nothing inside the VPC**. That absence is plan §2's argument written into the
  graph.
* `app-data-agent bound the workloads into VPC-Enterprise-Prod`, the cross-over
  step named in its own right.

Before any of that it runs `lambo inspect --focus VPC-Enterprise-Prod`, and
**refuses to write** unless `SG-Base-VPC` and `Subnet-Private-1a` are already
there. `lambo derive --parent-of` creates a missing parent rather than failing,
so running out of order would otherwise grow a second, contentless network node
and silently split the graph in two.

Arguments: `--skip-rds`, `--skip-lambda` (mirroring `provision_app_data.py`),
plus `--session` and the common four.

### `03_crossover_protect.py`

Plan §3's climax. `network-infra-agent` runs a drift cleanup and decides
`SG-Base-VPC` is idle.

1. Confirms from AWS, read-only, that the group really is shared, by reading
   which security groups `rds-lambo-demo-db` is attached to. If the account and
   the graph disagree, it says so.
2. Runs the pre-flight recall protocol (plan §4.1) with the plan's own query,
   `"tear down Subnet-Private-1a and delete SG-Base-VPC"`.
3. Runs `lambo inspect --focus SG-Base-VPC --depth 1`.
4. Blocks if either signal shows a dependency, and renders what depends on the
   group.

Exit status is 0 when it blocked (the outcome the demo wants) and when the
session is empty/unpopulated (nothing to protect — printed as a prominent
stderr banner, in case a wrong session id was supplied), and 1 when a focus is
present but unprotected.

Arguments: `--action delete-security-group|revoke-ingress`, `--query`,
`--top-k`, plus `--session` and the common four.

---

## Design notes worth not re-litigating

* **The destructive AWS call is not conditional. It does not exist.**
  `describe_destructive_call` returns lines of text and the only thing done with
  them is print them. There is no client method reference, no kwargs dict, no
  `getattr`. A demo whose safety rests on a boolean is a demo that eventually
  deletes a security group.

* **Names come from the plan verbatim**, via `scripts/aws-infra/_common.py`
  rather than re-declared here. A concept named for a resource that no longer
  matches the account is worse than no concept: the graph would confidently
  describe infrastructure that is not there. The one exception is the same one
  Phase 3 documents, the lowercase `rds-lambo-demo-db` identifier against the
  plan's `RDS-Lambo-Demo-DB` `Name` tag.

* **What counts toward a blast radius (#36).** A node's dependents are its
  `--parent-of` children and every action that names it in `--depends-on`.
  `--produces` and `--modifies` record provenance and count for nothing, so the
  scripts use them only for concepts with no hierarchy parent. Containment is
  expressed once, with `--parent-of`; Lambo refuses a second parent for any
  concept at derive time, because the hierarchy is a tree.

* **The database hangs off the security group, not the subnet.** Both are true
  in AWS and only one can be the hierarchy edge. Membership of `SG-Base-VPC` is
  the relationship the climax is about, so that is the one that becomes
  containment; the subnet placement is recorded as an action dependency instead.
  This is why `RDS-Lambo-Demo-DB` appears by name in the abort output.

* **No concept is ever derived as an `observation`.** `canonicalize`
  (`src/graph/canonical.rs`) excludes `ConceptType::Observation` from key
  matching, so an observation can never be matched to an existing node: every
  re-run creates another copy, and naming one as a `--parent-of` end creates a
  third in the same call. Facts that read like observations are recorded as
  `entity` when they identify something and `constraint` when they state how a
  resource is configured.

* **The CHILD end of a concept carries no colons.** `--parent-of CHILD:PARENT`
  splits on the first colon, so the child must be colon-free but a parent may
  carry colons (an IPv6 CIDR works as a parent end). That is why the secret is
  recorded by name and not by ARN, why an IPv6 security group rule is skipped
  with a line saying so rather than mangled into a concept, and why an IPv6
  CIDR can appear as a parent but never as a child.

* **`--dry-run` constructs no client and spawns no subprocess.** Same contract as
  the sibling directory, extended to `lambo`, which is what makes the plan output
  usable as a review artifact on a machine with nothing installed.

### Two guard signals, and why the obvious one is not enough

`recall` renders the spec §13 `⚑ Load-bearing pillar` line **only** for a concept
the daemon has already promoted to `Canonical` (`src/recall/assemble.rs`).
Promotion is earned: at least twenty non-canonical peers in the session, three
surviving GC sweeps, structural edges older than sixty seconds, three distinct
origin interactions, and a blast radius above five. A `lambo derive` process
lives for about a second, so on a session written only by these scripts the
warning is usually not there yet. It arrives once `lambo serve` has held the
session for a few minutes.

`lambo inspect` has no such gate: it prints `blast radius: N`, the same `⚑`
line and a `dependents (N):` section naming them, for any focus, whatever its
status. So `03` reads both and blocks on either.
The recall line is the one the plan puts on screen; the inspect line is the one
that works from the first run.

Since #36 the counts are the dependents: `VPC-Enterprise-Prod` counts its
subnets, groups, facts and bindings as children plus every action that names it
in `--depends-on`; `SG-Base-VPC` counts its rule constraints, its binding and
`RDS-Lambo-Demo-DB` as children plus the RDS action that depends on it. The
0.5.0 figures recorded for this stack (9 or 7 for the VPC, 5 for the group)
used the older exclusive count and have not been re-measured.

### Reading `inspect` output back

`03` parses `lambo inspect`, because neither `recall` nor `inspect` has a JSON
mode on the CLI today. `parse_dependents` reads the `dependents (N):` section
that `inspect` prints under the `blast radius:` line: exactly the concepts the
count is made of, each tagged `Dependency` (it depends on the focus) or
`Hierarchical` (it is the focus's child). The hop listing below it names
neighbours in both directions, parents and producers included, and is not
read. The MCP tool's structured result carries the same list as `dependents`.

## Files

```
scripts/cloudops/
  _lambo.py                 the lambo CLI as a subprocess, graph-shape invariants,
                            and the re-exports from ../aws-infra/_common.py
  01_network_agent.py       network-infra-agent: discover and derive the network tier
  02_app_data_agent.py      app-data-agent: query Lambo, then record the cross-tier edges
  03_crossover_protect.py   the guard, and the call it does not make
```
