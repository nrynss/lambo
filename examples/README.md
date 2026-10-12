# examples

Scripts that demonstrate something about Lambo without being part of the product
or of the provisioning path. Nothing here is imported by the binary, run by CI,
or needed to install or operate Lambo.

---

## `canon36_stage2_cap.rs` — what the Stage 2 coverage cap changes on a store

Measures, through the real SQLite queries, which concepts of one session pass
Stage 2 under the whole-extent denominator and which under the capped one
(#36 PR 2), and lists the concepts whose verdict the cap flips by node id,
type, status, origins, both coverages and blast radius. It prints no concept
text and carries no data of its own. The database is opened read-only; run it
on a copy of a store, never on a live writer's file.

```text
cargo run --features store-sqlite --example canon36_stage2_cap -- \
    <store.db> <session-id> [--min-age-secs 60] [--window-days 14] [--at <RFC3339>]
```

Pin `--at` to make two runs over the same copy comparable.

---

## `canon36_admission.rs` — what evidence-led admission does on a store

Measures #36 PR 3 on one session under both policies: how many concepts
would newly become Candidate, Venerable or Canonical on the first evaluation
(every concept's next hop through the evaluator's own gates and the real
SQLite queries, unbatched), what one real cycle with the default batch
commits, and where the evaluator settles when run to a fixed point on an
in-memory copy. Every count is split Resource versus non-Resource or by
concept type, and every concept that would turn Canonical is listed by node
id and type. It prints no concept text and carries no data of its own. The
database is opened read-only; run it on a copy of a store, never on a live
writer's file.

```text
cargo run --features store-sqlite --example canon36_admission -- \
    <store.db> <session-id> [--at <RFC3339>] [--assume-survived] [--quiet 30]
```

`--assume-survived` raises every `gc_survived` below 3 to 3 first, the state
after three GC sweeps.

---

## `drive_mcp_soak.py` — a demo artifact, and why that label matters

Connects to a running `lambo serve` over MCP and repeatedly calls `lambo_derive`
with the CloudOps topology until a concept reaches Canonical, then prints the
result.

**This script produced the Canonical status the CloudOps exhibit displays.**
Anyone reproducing that result will have used it, so it is worth being exact
about what it did and did not do.

### What it did not do

It did not lower the bar. Promotion is gated by `canon::stage{1,2,3}`, and every
threshold was enforced by the engine, not by this script:

* blast radius strictly greater than 5, counted over structural edges only
* `gc_survived >= 3`
* a score in the top decile of at least 20 non-Canonical peers
* interaction coverage of at least 0.3

`VPC-Enterprise-Prod` cleared all of them, at a real blast radius of 7 derived
from edges the agents wrote while provisioning real AWS infrastructure.

### What it did do, and the honest caveat

Stage 2 also requires **three distinct origin interactions**, and this script
supplied those by replaying the same derives. Each replay is a genuinely
distinct interaction as far as the engine is concerned. None of them is distinct
*work*.

So the structural claim is earned and the interaction count is manufactured. If
you are citing the exhibit's canonization as evidence, cite the blast radius and
the audit trail, and know that the interaction count behind Stage 2 came from a
loop rather than from two agents doing different things.

The honest version is to run the real agents repeatedly against a long-lived
writer, so the interactions arrive from actual work. That is more setup for the
same result, which is why this exists.

### Why a script is needed at all

The default cadence puts canonization out of reach of any ordinary session:

* GC sweeps every `gc_interval` **mutations**, defaulting to 10 000
* Stage 1 requires `gc_survived >= 3`

So a concept cannot be promoted until its session has taken roughly 30 000
mutations. The CloudOps session has 47 concepts and a few hundred mutations. It
would never have promoted anything, no matter how long it ran.

`lambo demo` hides this by setting `gc_interval` to 1 internally. Outside the
demo the knob is reachable through `[daemon]` in `lambo.toml`. Both are cadence,
never a threshold.

### Running it

```sh
lambo serve --session <id> --transport http --port 17701 --bind 127.0.0.1
python3 examples/drive_mcp_soak.py --session <id> --endpoint http://127.0.0.1:17701/mcp
```

`--endpoint` is required: there is no default, so the driver never loads a
long-lived writer by omission. An endpoint on port 7700, where such a writer
conventionally listens, is refused unless `--allow-production-port` is passed.

Needs a writer already running, because that is the point: a short-lived
`lambo derive` exits long before the daemon can evaluate anything.
