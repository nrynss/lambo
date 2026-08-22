# J4 live two-serve validation — evidence

Binary: `/home/nryn/work/lambo/target/release/lambo` = lambo-for-mooshik @ `0dd15b3`
(`--features store-sqlite,embed-fixture`). Scratch session `session-j4val-1787375044`,
scratch sqlite store + config + shared ledger under `/tmp/j4val-1787375044`
(now deleted). Live dogfood rig untouched, nothing integrated/committed.

## Commands run (commands, token-redacted — none carried secrets)

```
# scratch config (mirrors lambo.example.toml)
printf '[store]\nkind = "sqlite"\npath = "/tmp/<ts>/lambo.db"\n\n[embedder]\nkind = "fixture"\ndim = 1024\n' > <scratch>/lambo.toml
lambo provision --config <scratch>/lambo.toml

# Serve A — holder (http, loopback port 33195)
lambo --config <scratch>/lambo.toml serve --session session-j4val-<ts> \
  --agent omp-agent --transport http --port 33195 --ledger <scratch>/shared.jsonl &

# A acquires the lease on initialize (streamable-HTTP MCP handshake via Python urllib).
# Serve B — loser (stdio), refused -> proxies (J2)
python3 <scratch>/drive_b.py   # spawns: lambo --config ... serve --session S --agent pi-agent \
                               #   --transport stdio --ledger <scratch>/shared.jsonl
                               #   stdout/stderr piped; drives initialize + tools/call; holds B.
kill -TERM <B-pid>             # clean proxy shutdown (code 0)
# provokes proxying_stopped (designed trigger = holder death with in-flight calls)
python3 <scratch>/scenario2.py <A-pid>   # bursts 30 calls through B, SIGKILLs A mid-burst
# cleanup
rm -f /run/user/1000/lambo/session-j4val-17-*.sock; rm -rf /tmp/j4val-<ts>
```

## What the J E2E round-1 remediation superseded (2026-08-22)

**The transcripts below are untouched, deliberately** — a capture is only
evidence while it stays byte-exact, so this note records what has moved since
rather than editing the lines to match today's binary. Two changes affect how a
reader should read them:

* **JE2E-11.** The `lease` line's other-party key was `holder` on every event
  and meant three different things — the incumbent's token, the loser's token,
  and a socket path. It is now chosen by the event: `counterparty` on
  `refused` / `refused_takeover`, `dialled` on `proxying` /
  `proxying_stopped`. Every `"holder":` below is one of those two.
* **JE2E-3.** `proxying_stopped` was booked only when calls were in flight,
  which is why the run below had to kill the holder *with 30 calls in flight*
  to provoke one ("its designed trigger", as the note says). It is now booked
  on any ending of the current hub connection, so the commonest degraded shape
  — a holder dying while the proxy is idle — produces the line too, with
  `lost: 0`.

## Quoted artifact lines (shared ledger `shared.jsonl`)

Earliest (holder) set — the **pre-lease startup line** (deliverable 1, written before acquire):
```
{"agent_id":"omp-agent","kind":"startup","session":"session-j4val-1787375044","state":"acquiring","transport":"http","ts":"2026-08-22T05:04:11.042171602+00:00","v":1}
```

The live B (proxying) set — same session + shared ledger:
```
B refused (loser):
{"agent_id":"pi-agent","event":"refused","holder":"omp-agent@cachyos-x8664#393845","kind":"lease","session":"session-j4val-1787375044","side":"loser","ts":"2026-08-22T05:05:34.027707231+00:00","v":1}

B proxying (loser) — agent_id=pi-agent, never the literal "proxy":
{"agent_id":"pi-agent","event":"proxying","holder":"/run/user/1000/lambo/session-j4val-17-4f393189eb56b816.sock","kind":"lease","session":"session-j4val-1787375044","side":"loser","ts":"2026-08-22T05:05:34.027894340+00:00","v":1}

A refused_takeover (holder) — A learned it was contended:
{"agent_id":"omp-agent","at":"2026-08-22T05:05:34.027+00:00","event":"refused_takeover","holder":"pi-agent@cachyos-x8664#394104","kind":"lease","session":"session-j4val-1787375044","side":"holder","ts":"2026-08-22T05:05:34.248668069+00:00","v":1}

call line for the write driven through B's stdio (proxied to A):
{"admitted":true,"agent_id":"pi-agent","duration_us":70,"kind":"call","outcome":"ok","receipt":"lwr1.07cb603bf9e075a8.1a027dc2b24.1","tool":"lambo_record_action","ts":"2026-08-22T05:05:35.524148584+00:00","v":1}

completion line (deliverable — created/matched counts for the write through B):
{"agent_id":"pi-agent","created_count":3,"kind":"completion","matched_count":0,"receipt":"lwr1.07cb603bf9e075a8.1a027dc2b24.1","state":"applied","ts":"2026-08-22T05:05:35.524256333+00:00","v":1}
```

`proxying_stopped` — produced by killing the HOLDER (A) while calls were in flight
through B (its designed trigger), with `lost` count:
```
{"agent_id":"pi-agent","event":"proxying_stopped","holder":"/run/user/1000/lambo/session-j4val-17-4f393189eb56b816.sock","kind":"lease","lost":30,"session":"session-j4val-1787375044","side":"loser","ts":"2026-08-22T05:06:24.532966611+00:00","v":1}
```

`lease_refusals` row in the scratch sqlite store (J4-R1-1):
```
('session-j4val-1787375044', '2026-08-22T05:05:34.027Z', 'pi-agent@cachyos-x8664#394104', 'omp-agent@cachyos-x8664#393845')
```
(refused_by = the proxy loser pi-agent; current_holder = the incumbent omp-agent.)

## Writer-visible proxy response through B's stdio
INIT via proxy: `{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18",...,"serverInfo":{"name":"lambo","version":"0.2.2"},...}}`
CALL via proxy: `isError:false`, receipt `lwr1.07cb603bf9e075a8.1a027dc2b24.1`,
"accepted action 'deployed j4-validate service checkpoint' for background write".

## Per-deliverable verdict
| # | J4 deliverable | Evidence | Verdict |
|---|---|---|---|
| 1 | Pre-lease startup line `kind:startup state:acquiring` (A + B) | startup lines, agent=omp-agent / pi-agent | PASS |
| 2 | B refused→PROxy over stdio, does NOT exit 1 | line 3/11/17 `event:refused side:loser`; B alive, exited code 0 only on SIGTERM | PASS |
| 3 | Write driven through B's stdio succeeds | `kind:call outcome:ok` + `kind:completion state:applied` | PASS |
| 4a | A `event:refused_takeover side:holder` | present (agent=omp-agent, holder=pi-agent) | PASS |
| 4b | B `event:refused side:loser` | present (agent=pi-agent) | PASS |
| 4c | B `event:proxying` / `proxying_stopped` with agent_id=pi-agent, never "proxy" | present, `lost:30` | PASS |
| 4d | store `lease_refusals` row | present (J4-R1-1) | PASS |
| 4e | completion line with created_count/matched_count | `created_count:3 matched_count:0` | PASS |

Cleanup: both serves stopped (A SIGKILL'd, B terminated), stale unix socket removed,
scratch db/config/ledger dir `/tmp/j4val-1787375044` deleted.

## Note / truth-tell
`proxying_stopped` is NOT emitted by simply SIGTERM-ing the proxy (B). The code gates
it on the holder (A) closing the connection with calls still in flight (`lost>0`,
src/mcp/proxy.rs FromHub::Closed). The brief's "(if you stop it)" wording implied
proxy-shutdown produces it; the actual designed trigger is holder-death with in-flight
calls, which I exercised (30-call burst, SIGKILL A) and it produced the line correctly
with `agent_id:pi-agent`, `lost:30`. No artifact was missing or mis-attributed
(e.g. B never exited 1, and no line ever carried the literal agent "proxy").
