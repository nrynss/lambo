#!/usr/bin/env python3
"""J3 proof obligation 1 at the release binary against the live BGE-M3.

Green side of the round-3 red numbers (326/361 and 13/16 acked writes
abandoned at a clean close, measured at ed22476 by the round-3 reviewer on
this same rig): under durable intents, acked => (applied-with-embedding OR
durable intent) at a clean close, and the next serve applies the remainder.
"""
import json, os, sqlite3, subprocess, sys, tempfile, threading, time, queue, signal

BIN = sys.argv[1]
MIGRATION = sys.argv[2]
LLAMA = "http://127.0.0.1:8080"
SESSION = "j3-live-demo"

root = tempfile.mkdtemp(prefix="lambo-j3-live-")
db = os.path.join(root, "live.sqlite")
cfg = os.path.join(root, "lambo.toml")
with open(cfg, "w") as f:
    f.write(f'[store]\nkind = "sqlite"\npath = "{db}"\n\n'
            f'[embedder]\nkind = "bge_m3"\ndim = 1024\nllama_url = "{LLAMA}"\n')

# Provision the schema.
conn = sqlite3.connect(db)
conn.executescript(open(MIGRATION).read())
conn.commit(); conn.close()

class Serve:
    def __init__(self):
        self.p = subprocess.Popen(
            [BIN, "--config", cfg, "serve", "--session", SESSION,
             "--agent", "agent-0", "--transport", "stdio"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True)
        self.q = queue.Queue()
        self.next_id = 1
        t = threading.Thread(target=self._pump, daemon=True)
        t.start()
        self._handshake()

    def _pump(self):
        for line in self.p.stdout:
            self.q.put(line)

    def send(self, obj):
        self.p.stdin.write(json.dumps(obj) + "\n"); self.p.stdin.flush()

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
        rid = self.next_id; self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": "initialize",
                   "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                              "clientInfo": {"name": "j3-live", "version": "1"}}})
        self.recv(rid)
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def call(self, tool, args, timeout=60):
        rid = self.next_id; self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": "tools/call",
                   "params": {"name": tool, "arguments": args}})
        return self.recv(rid, timeout)["result"]

    def sigterm_wait(self):
        self.p.send_signal(signal.SIGTERM)
        rc = self.p.wait(timeout=30)
        assert rc == 0, f"serve exited {rc}"

def content(agent, i):
    # The discriminating tokens lead, so canonicalization cannot collide two
    # burst items onto one key.
    body = f"a{agent:02d}i{i:02d} live demo concept " + ("granular fact " * 80)
    return body[:1024]

# ---- Session 1: multi-agent burst of in-band concepts, immediate close ----
s = Serve()

# The J3-R3-1 shape first: a content the embedder refuses must be a FAILED
# receipt, never applied-with-NULL-embedding.
big = "z" * 3000
res = s.call("lambo_derive", {"agent_id": "agent-0",
                              "concepts": [{"content": big, "concept_type": "entity"}]})
receipt_big = res["structuredContent"]["receipt"]
w = s.call("lambo_stats", {"agent_id": "agent-0", "receipt": receipt_big, "wait_ms": 8000})
refusal_state = w["structuredContent"]["receipt"]["state"]
refusal_detail = w["structuredContent"]["receipt"]["detail"]

AGENTS, PER = 16, 4
receipts = {}
t0 = time.time()
for i in range(PER):
    for a in range(AGENTS):
        agent = f"agent-{a}"
        res = s.call("lambo_derive", {"agent_id": agent, "concepts": [
            {"content": content(a, i), "concept_type": "logic"}]})
        sc = res["structuredContent"]
        receipts[(agent, i)] = sc["receipt"]
burst_secs = time.time() - t0
acked = len(receipts)
t0 = time.time()
s.sigterm_wait()
close_secs = time.time() - t0

conn = sqlite3.connect(db)
embedded = conn.execute(
    "SELECT count(*) FROM concepts WHERE embedding IS NOT NULL AND content LIKE '%live demo concept%'"
).fetchone()[0]
null_embedded = conn.execute(
    "SELECT count(*) FROM concepts WHERE embedding IS NULL"
).fetchone()[0]
unconsumed = conn.execute(
    "SELECT count(*) FROM write_intents WHERE consumed_at IS NULL"
).fetchone()[0]
consumed_applied = conn.execute(
    "SELECT count(*) FROM write_intents WHERE outcome_tag = 'applied'"
).fetchone()[0]
conn.close()

print(f"refusal probe: state={refusal_state} detail={refusal_detail[:100]!r}")
print(f"session 1: acked={acked} in {burst_secs:.2f}s; close={close_secs:.2f}s")
print(f"  store: embedded={embedded} embedding_NULL_rows={null_embedded} "
      f"unconsumed_intents={unconsumed} consumed_applied={consumed_applied}")
invariant_1 = (embedded + unconsumed == acked)
print(f"  INVARIANT acked == applied-with-embedding + durable-intent: "
      f"{acked} == {embedded} + {unconsumed} -> {invariant_1}")

# ---- Session 2: replay ----
s = Serve()
deadline = time.time() + 300
while True:
    st = s.call("lambo_stats", {"agent_id": "agent-0"})["structuredContent"]
    if st.get("write_queue_replayed", 0) >= unconsumed:
        break
    assert time.time() < deadline, f"replay wedged: {st}"
    time.sleep(0.5)
replayed = st["write_queue_replayed"]

# One receipt that deferred: it must answer applied_after_restart now.
sample_state = None
for (agent, i), rid in receipts.items():
    r = s.call("lambo_stats", {"agent_id": agent, "receipt": rid})
    stt = r["structuredContent"]["receipt"]["state"]
    if stt == "applied_after_restart":
        sample_state = stt
        break
t0 = time.time()
s.sigterm_wait()
print(f"session 2: replayed={replayed}; sampled cross-restart receipt state={sample_state}; "
      f"close={time.time()-t0:.2f}s")

conn = sqlite3.connect(db)
embedded2 = conn.execute(
    "SELECT count(*) FROM concepts WHERE embedding IS NOT NULL AND content LIKE '%live demo concept%'"
).fetchone()[0]
null2 = conn.execute("SELECT count(*) FROM concepts WHERE embedding IS NULL").fetchone()[0]
unconsumed2 = conn.execute(
    "SELECT count(*) FROM write_intents WHERE consumed_at IS NULL").fetchone()[0]
after_restart = conn.execute(
    "SELECT count(*) FROM write_intents WHERE outcome_tag = 'applied_after_restart'").fetchone()[0]
failed_rows = conn.execute(
    "SELECT count(*) FROM write_intents WHERE outcome_tag = 'failed'").fetchone()[0]
conn.close()
print(f"  store after replay: embedded={embedded2} embedding_NULL_rows={null2} "
      f"unconsumed={unconsumed2} applied_after_restart={after_restart} failed={failed_rows}")
ok = (invariant_1 and embedded2 == acked and unconsumed2 == 0 and null2 == 0
      and after_restart == unconsumed and refusal_state == "failed")
print(f"OVERALL: {'PASS' if ok else 'FAIL'}")
sys.exit(0 if ok else 1)
