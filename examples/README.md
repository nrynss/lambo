# examples

Scripts that demonstrate something about Lambo without being part of the product
or of the provisioning path. Nothing here is imported by the binary, run by CI,
or needed to install or operate Lambo.

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
lambo serve --session <id> --transport http --port 7700 --bind 127.0.0.1
python3 examples/drive_mcp_soak.py --session <id>
```

Needs a writer already running, because that is the point: a short-lived
`lambo derive` exits long before the daemon can evaluate anything.
