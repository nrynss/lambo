#!/usr/bin/env python3
"""J3 round-1 N1 at the release binary against the live BGE-M3.

The P1: a transient embedder outage at attach settled the WHOLE durable-intent
backlog `failed`, permanently, because `spawn_replay` ran unconditionally and its
failure arm could not tell a content refusal from an unreachable embedder.

Three sessions, one store:

1. **Defer.** Sixteen agents x four 1024-byte concepts, immediate close — the
   same shape as `j3_live_demo.py`, leaving a real backlog of durable intents.
2. **Attach during an outage.** Same store, same session, but `llama_url` points
   at a closed port (127.0.0.1:9, `discard`), which is what a down or restarting
   llama-server looks like to the client: a connection refusal, i.e.
   `EmbedError::Unavailable`. The backlog must be **untouched** — every intent
   still unconsumed, `write_queue_replayed` 0, `write_queue_replay_owed` equal
   to the backlog, and every acked receipt, queried as a whole (not one
   sampled id — J3-R2R-9), partitions exactly into `pending_replay` (the
   deferred backlog) and `applied_after_restart` (the ones session 1 drained
   in-session), with `failed` never present.
   Before the fix this session settled all of them `failed` with nothing written.
3. **Attach healthy.** The embedder is back. Every intent the outage would have
   destroyed is applied, **with its embedding** (durability is judged at the
   embedding column, never at `applied` counts — the J3-R3-1 rule).

usage: j3_n1_outage_demo.py <release-binary> <migrations/sqlite/001_init.sql>
"""
import json, os, queue, signal, sqlite3, subprocess, sys, tempfile, threading, time

BIN = sys.argv[1]
MIGRATION = sys.argv[2]
LLAMA = "http://127.0.0.1:8080"
# Port 9 is `discard`, closed on this rig: a connect() refusal, which the BGE-M3
# adapter reports as EmbedError::Unavailable ("llama.cpp unreachable"). That is
# the transport class N1 turns on, produced by the real adapter rather than a
# test double.
DEAD = "http://127.0.0.1:9"
SESSION = "j3-n1-outage"

root = tempfile.mkdtemp(prefix="lambo-j3-n1-")
db = os.path.join(root, "outage.sqlite")


def write_cfg(name, url):
    path = os.path.join(root, name)
    with open(path, "w") as f:
        f.write(
            f'[store]\nkind = "sqlite"\npath = "{db}"\n\n'
            f'[embedder]\nkind = "bge_m3"\ndim = 1024\nllama_url = "{url}"\n'
        )
    return path


cfg_live = write_cfg("live.toml", LLAMA)
cfg_dead = write_cfg("dead.toml", DEAD)

conn = sqlite3.connect(db)
conn.executescript(open(MIGRATION).read())
conn.commit()
conn.close()


class Serve:
    def __init__(self, cfg):
        self.p = subprocess.Popen(
            [BIN, "--config", cfg, "serve", "--session", SESSION,
             "--agent", "agent-0", "--transport", "stdio"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True)
        self.q = queue.Queue()
        self.next_id = 1
        threading.Thread(target=self._pump, daemon=True).start()
        self._handshake()

    def _pump(self):
        for line in self.p.stdout:
            self.q.put(line)

    def send(self, obj):
        self.p.stdin.write(json.dumps(obj) + "\n")
        self.p.stdin.flush()

    def recv(self, rid, timeout=60):
        deadline = time.time() + timeout
        while True:
            line = self.q.get(timeout=max(0.1, deadline - time.time()))
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == rid:
                return msg

    def _handshake(self):
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                              "clientInfo": {"name": "j3-n1", "version": "1"}}})
        self.recv(rid)
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def call(self, tool, args, timeout=60):
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": "tools/call",
                   "params": {"name": tool, "arguments": args}})
        return self.recv(rid, timeout)["result"]

    def sigterm_wait(self):
        self.p.send_signal(signal.SIGTERM)
        rc = self.p.wait(timeout=30)
        assert rc == 0, f"serve exited {rc}"


def content(agent, i):
    body = f"a{agent:02d}i{i:02d} outage demo concept " + ("granular fact " * 80)
    return body[:1024]


def counts():
    c = sqlite3.connect(db)
    out = {
        "embedded": c.execute(
            "SELECT count(*) FROM concepts WHERE embedding IS NOT NULL "
            "AND content LIKE '%outage demo concept%'").fetchone()[0],
        "null_rows": c.execute(
            "SELECT count(*) FROM concepts WHERE embedding IS NULL").fetchone()[0],
        "unconsumed": c.execute(
            "SELECT count(*) FROM write_intents WHERE consumed_at IS NULL").fetchone()[0],
        "failed": c.execute(
            "SELECT count(*) FROM write_intents WHERE outcome_tag = 'failed'").fetchone()[0],
        "after_restart": c.execute(
            "SELECT count(*) FROM write_intents WHERE outcome_tag "
            "= 'applied_after_restart'").fetchone()[0],
    }
    c.close()
    return out


# ---- Session 1: defer a real backlog -------------------------------------
s = Serve(cfg_live)
AGENTS, PER = 16, 4
receipts = {}
for i in range(PER):
    for a in range(AGENTS):
        agent = f"agent-{a}"
        res = s.call("lambo_derive", {"agent_id": agent, "concepts": [
            {"content": content(a, i), "concept_type": "logic"}]})
        receipts[(agent, i)] = res["structuredContent"]["receipt"]
acked = len(receipts)
s.sigterm_wait()
c1 = counts()
backlog = c1["unconsumed"]
print(f"session 1 (live embedder): acked={acked} "
      f"embedded={c1['embedded']} durable_intents={backlog} "
      f"NULL_rows={c1['null_rows']}")
assert backlog > 0, "the demo needs a real backlog; the embedder drained everything"

# ---- Session 2: attach while the embedder is unreachable -----------------
s = Serve(cfg_dead)
# Give the replay task every chance to misbehave: the pre-fix code needed only
# ~2 ms per intent to burn the whole backlog.
time.sleep(5)
st = s.call("lambo_stats", {"agent_id": "agent-0"})["structuredContent"]
# J3-R2R-9: the old driver sampled `next(iter(receipts))` — the FIRST write of
# session 1, the one likeliest to have drained in-session, which a later
# process answers `applied_after_restart` rather than `pending_replay`. That
# made the check order-dependent (it failed or passed on a timing accident).
# Assert over EVERY receipt, partitioned by the obtained state, which is
# deterministic: the acked set must partition exactly into `pending_replay`
# (the deferred backlog, still owed — count == store `unconsumed`) and
# `applied_after_restart` (the ones session 1 drained in-session), with
# `failed` never present.
states = {}
for (agent, i), receipt in receipts.items():
    sample = s.call("lambo_stats", {"agent_id": agent, "receipt": receipt})
    state = sample["structuredContent"]["receipt"]["state"]
    states[state] = states.get(state, 0) + 1
drained_in_session = acked - backlog
outage_ok = (
    c2["unconsumed"] == backlog
    and c2["failed"] == 0
    and st.get("write_queue_replayed") == 0
    and st.get("write_queue_replay_owed") == backlog
    and states.get("pending_replay", 0) == backlog
    and states.get("applied_after_restart", 0) == drained_in_session
    and states.get("failed", 0) == 0
)
print(f"session 2 (embedder at {DEAD}): replayed={st.get('write_queue_replayed')} "
      f"replay_owed={st.get('write_queue_replay_owed')} "
      f"receipts={{pending_replay={states.get('pending_replay', 0)}, "
      f"applied_after_restart={states.get('applied_after_restart', 0)}, "
      f"failed={states.get('failed', 0)}}}")
print(f"  store: unconsumed={c2['unconsumed']} failed_rows={c2['failed']} "
      f"embedded={c2['embedded']}")
print(f"  N1: the outage consumed NOTHING and the debt is visible -> {outage_ok}")

# ---- Session 3: the embedder is back ------------------------------------
s = Serve(cfg_live)
deadline = time.time() + 300
while True:
    st3 = s.call("lambo_stats", {"agent_id": "agent-0"})["structuredContent"]
    if st3.get("write_queue_replayed", 0) >= backlog:
        break
    assert time.time() < deadline, f"replay wedged after the outage: {st3}"
    time.sleep(0.5)
# J3-R2R-9 (same order-independence as session 2): after the replay, every
# acked receipt must answer `applied_after_restart` — the drained-in-session
# ones and the replayed-backlog ones alike.
states3 = {}
for (agent, i), receipt in receipts.items():
    sample3 = s.call("lambo_stats", {"agent_id": agent, "receipt": receipt})
    state3 = sample3["structuredContent"]["receipt"]["state"]
    states3[state3] = states3.get(state3, 0) + 1
s.sigterm_wait()
c3 = counts()
print(f"session 3 (live embedder again): replayed={st3['write_queue_replayed']} "
      f"replay_owed={st3.get('write_queue_replay_owed')} "
      f"receipts={{applied_after_restart={states3.get('applied_after_restart', 0)}, "
      f"other={dict((k, v) for k, v in states3.items() if k != 'applied_after_restart')}}}")
print(f"  store: embedded={c3['embedded']} NULL_rows={c3['null_rows']} "
      f"unconsumed={c3['unconsumed']} applied_after_restart={c3['after_restart']}")
recovered_ok = (
    c3["embedded"] == acked
    and c3["unconsumed"] == 0
    and c3["null_rows"] == 0
    and c3["after_restart"] == backlog
    and states3.get("applied_after_restart", 0) == acked
)
print(f"  every write the outage would have destroyed is applied WITH its "
      f"embedding -> {recovered_ok}")
ok = outage_ok and recovered_ok
print(f"OVERALL: {'PASS' if ok else 'FAIL'}")
sys.exit(0 if ok else 1)
