# Live J4 validation — two real lambo serves, lease conflict, ledger artifacts

Goal: prove J4's lease-conflict artifacts end-to-end by running the real `serve` binary
(two processes) against a scratch store with two real agent ids (an "OMP" agent and a "Pi"
agent), driving a write through the losing (proxying) serve, and reading the J4 lines out of
the shared ledger. Capture the output as evidence. This is a LIVE dogfood test of the just-
integrated J4 on `lambo-for-mooshik` @ `0dd15b3`.

## Binary + setup

- Binary: `/home/nryn/work/lambo/target/release/lambo` = `lambo-for-mooshik` @ `0dd15b3`,
  built `--features store-sqlite,embed-fixture`. All lambo processes on this machine are
  already stopped; you start and stop your own.
- Use a SCRATCH SQLite store and a fresh session id (e.g. `j4val-<ts>`), never the live
  `lambo-dev`/dogfood store. Config mirrors `lambo.example.toml`: `[store] kind="sqlite"`
  at a scratch path, `[embedder] kind="fixture" dim=1024`. Delete the scratch db + ledger
  after.

## Scenario (mirror the J4 test `tests/serve_j4_lease_conflicts.rs`, but live + with named agents)

1. **Serve A — the holder** (`--agent omp-agent`): start on session S with a `--ledger`
   path shared by both serves, `--transport http` on a free loopback port. Wait for
   "listening". Note the J4 **pre-lease startup line** (`kind:startup state:acquiring`,
   agent=omp-agent) written before the acquire.
2. **Serve B — the loser** (`--agent pi-agent`): start against the SAME session S with the
   SAME `--ledger`, `--transport stdio` (J2 proxies stdio losers). With the lease held, B
   must NOT exit 1 — it becomes a proxy (J2) and records J4 artifacts.
3. Drive a real MCP write **through the proxy (serve B)**: send a `tools/call` for
   `lambo_record_action` (or `lambo_derive`) over B's stdio. It must succeed (the proxy
   forwards to the holder). This carries a completed write → a J4 **completion line**.
   Use a small Python MCP client (mirror `examples/drive_mcp_soak.py` / the MCP framing the
   serve expects; newline-delimited or Content-Length per the serve's stdio protocol).
4. **Read the shared ledger** for the J4 "from both sides" artifacts:
   - A's line: `kind:lease event:refused_takeover side:holder` (A learned it was contended)
   - B's lines: `kind:lease event:refused side:loser` (the proxy loser's refusal) AND
     `kind:lease event:proxying` — and the `proxying` / `proxying_stopped` lines must carry
     **agent_id=pi-agent, never the literal "proxy"** (J4-R1-2)
   - A store `lease_refusals` row for B's refusal (query the scratch sqlite via `lambo inspect`
     or a raw sqlite read) (J4-R1-1)
   - the **completion line** (`kind:completion` or the schema's completion kind with
     `created_count`/`matched_count`) for the write driven through B
   - B's shutdown (if you stop it) writes `proxying_stopped` with the lost/undrained count
5. Assert each artifact is present in the ledger or store; quote the actual lines in the
   report.

## Truth-telling
If the proxy path does not produce one of the claimed artifacts (e.g. B exits 1, or a line
is missing), report it as a FAILURE with the exact observed stderr/ledger — do not paper over
it. The point is to validate, and a negative result is a finding worth having.

## Cleanup
Stop both serves, delete the scratch db, config and ledger. Do NOT touch the live dogfood
rig or its files. Do NOT integrate or commit anything.

## Report back
The exact commands you ran (token redacted), the artifact lines quoted (startup / both-sides
/ proxying / completion), a pass/fail verdict per J4 deliverable, the evidence file path, and
anything that did not show up.
