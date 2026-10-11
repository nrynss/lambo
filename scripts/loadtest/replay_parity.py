#!/usr/bin/env python3
"""Replay-parity harness: compare the write outcomes of two lambo binaries.

Usage:
  python3 scripts/loadtest/replay_parity.py --bin-a OLD/lambo --bin-b NEW/lambo \
      --snapshot BACKUP.db --config-template lambo.toml --session SESSION \
      --groups 300 --out OUT [--work WORK] [--variants 30] [--canon-wait 20] \
      [--reasons reasons.json] [--include-content] [--dry-run]

First used for 0.4.0 vs 0.3.0 (evidence/0.4.0-vs-0.3.0-load/, whose harness/
copy is the record of that run). This copy is generalized for later releases:
paths, session and protected directories are arguments, and the reasons a
difference is expected are data (see "Classification" below).

Approach (truncate-and-replay):
  1. Copy the snapshot read-only (`sqlite3 "file:SRC?mode=ro" ".backup DST"`).
  2. Pick the last N write calls ("groups") in the snapshot. One interaction row
     is one lambo_derive or lambo_record_action call. T = created_at of the
     first of them.
  3. Reconstruct each call from the store:
     - derive: interactions.prompt_text is the call's concept contents joined
       with "; " in call order (hybrid::derive_prompt), so it is split back into
       the original texts, typed from the concepts the call created (Derives
       edges from the interaction) or matched. parent_of pairs are the
       Hierarchical edges stamped with the interaction's created_at.
     - record_action: prompt_text is the action text; produces/modifies are the
       Causal edges and depends_on the Dependency edges stamped with the
       interaction's created_at (modifies is folded into produces: both write
       a Causal edge, so the outcome is the same). Ambiguous one-Resource calls
       with no edges are classified with the call ledger when given.
     - agent_id and event_time are the original interaction's.
  4. Truncate a base copy to before T: delete interactions, concepts and edges
     created at/after T (plus edges touching deleted nodes), canonization events
     at/after T (rolling each concept's status back to the earliest post-T
     from_status), and all write_intents / reservations / leases / lease
     refusals / session_stats. Edges older than T that were reinforced after T
     keep their weight; both sides start from the same bytes, so that does not
     bias the comparison.
  5. Copy the base once per binary, repoint a copy of the config at it (and add
     [daemon] canonization_eval_interval_secs / gc_max_interval_secs so GC does
     not fire on wall-clock time mid-run), run `serve --transport stdio` with a
     fresh XDG_RUNTIME_DIR, replay the calls in order, and wait on each receipt
     with lambo_stats(receipt, wait_ms=4000).
  6. Optional --variants K: K extra derives, each one concept from the replayed
     set prefixed "Recap: " (a different canonical key, near-identical meaning)
     to exercise the semantic-merge path directly.
  7. After the writes, sleep --canon-wait seconds (canonization passes run on
     the daemon timer; there is no CLI trigger), take one lambo_stats, close
     stdin, and read final counts from each copy with sqlite3.

Classification: every differing call gets one or more reason ids from fixed
detectors in classify() (receipt fields, merge targets, refusal text). Whether
a reason id is EXPECTED, and what CHANGELOG entry explains it, is data: a
catalogue mapping id -> {label, expected, ref}. The built-in catalogue,
REASONS_0_4_0, is the 0.4.0 CHANGELOG mapping used for the 0.4.0 vs 0.3.0
run. A later release passes --reasons FILE (JSON, same shape) whose entries
override or extend it; an entry may set "expected": false to retire a reason.
`state_differs` entries may carry a "detail_pattern" regex matched against the
refusal text, so a release can name why a call is now refused. A reason id
not in the catalogue, marked unexpected, or produced by no detector at all is
reported as UNEXPECTED, and the script exits 1 when any call is UNEXPECTED.

Output: <out>/parity.json and <out>/parity.txt hold aggregates: totals, store
counts, canonization counts, and differing calls grouped by reason. Concept
text is omitted unless --include-content is passed, so <out> can be committed
as evidence. Everything that holds store content (snapshot and replay copies,
reconstructed calls, per-side results, stderr logs, JSON-RPC transcripts) goes
to --work (default: a fresh temp directory, outside any checkout); never
commit that directory.

Tool-call budget: 2 per replayed call (write + receipt wait, more only while a
receipt stays pending) plus 1 final lambo_stats, per side.

Stdlib only. Opens the snapshot read-only, never writes under a --protect
directory (default ~/lambo-dogfood, the live dogfood rig), refuses a snapshot
named like a live store unless --allow-live-name, and never uses the HTTP
transport.
"""

import argparse
import collections
import json
import os
import queue
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

DEFAULT_PROTECT = Path.home() / "lambo-dogfood"
# A snapshot whose file name starts with one of these is taken to be a live
# store, not a backup (the dogfood rig's store is lambo-dev.db).
LIVE_DB_PREFIXES = ("lambo-dev.db",)
TERMINAL_STATES = {
    "applied", "failed", "applied_after_restart", "dropped", "expired",
    "restart_lost", "never_issued", "forbidden", "intent_durable",
}
WIRE_TYPE = {
    "Entity": "entity", "Logic": "logic", "Constraint": "constraint",
    "Resource": "resource", "Observation": "observation",
}


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def under(path, root):
    try:
        Path(path).resolve().relative_to(Path(root).resolve())
        return True
    except ValueError:
        return False


def ro_connect(path):
    return sqlite3.connect(f"file:{Path(path).resolve()}?mode=ro", uri=True)


def loose(text):
    return " ".join(sorted(set(re.findall(r"[a-z0-9]+", text.lower()))))


# ---------------------------------------------------------------------------
# Reconstruction
# ---------------------------------------------------------------------------

def load_ledger_calls(path):
    """(agent_id, ts, tool) for every write call line in calls.jsonl."""
    out = collections.defaultdict(list)
    if not path or not Path(path).exists():
        return out
    with open(path) as f:
        for line in f:
            if '"kind":"call"' not in line:
                continue
            try:
                d = json.loads(line)
            except ValueError:
                continue
            tool = d.get("tool")
            if tool in ("lambo_derive", "lambo_record_action"):
                out[d.get("agent_id")].append((d.get("ts", "")[:23], tool))
    for v in out.values():
        v.sort()
    return out


def ledger_kind(ledger, agent, created_at):
    """The latest write-call ledger line by this agent at or before created_at."""
    calls = ledger.get(agent) or []
    ts = created_at.replace("Z", "")[:23]
    best = None
    for cts, tool in calls:
        if cts <= ts:
            best = (cts, tool)
        else:
            break
    return best[1] if best else None


def split_prompt(prompt, known):
    """Split a derive prompt ("; "-joined contents) into segments.

    `known` are contents the call created (exact text). A known content that
    starts at a segment boundary and ends at "; " or the end wins (longest
    first); otherwise the segment runs to the next "; ".
    """
    segs = []
    pos = 0
    known_sorted = sorted(set(known), key=len, reverse=True)
    n = len(prompt)
    while pos <= n:
        hit = None
        for c in known_sorted:
            if c and prompt.startswith(c, pos):
                end = pos + len(c)
                if end == n or prompt.startswith("; ", end):
                    hit = c
                    break
        if hit is None:
            end = prompt.find("; ", pos)
            if end < 0:
                end = n
            hit = prompt[pos:end]
        segs.append(hit)
        pos += len(hit) + 2
        if pos > n:
            break
    return segs


def reconstruct(snapshot_copy, session, groups, ledger, offset=0):
    db = ro_connect(snapshot_copy)
    db.row_factory = sqlite3.Row
    inters = db.execute(
        "SELECT id, agent_id, prompt_text, created_at, event_time FROM interactions "
        "WHERE session_id=? ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?",
        (session, groups, offset),
    ).fetchall()[::-1]
    if not inters:
        raise SystemExit(f"no interactions for session {session!r} in snapshot")
    T = inters[0]["created_at"]
    concept = {}

    def get_concept(cid):
        if cid not in concept:
            r = db.execute(
                "SELECT id, content, concept_type, origin_interaction, created_at, "
                "embedding IS NOT NULL AS emb FROM concepts WHERE id=?", (cid,)).fetchone()
            concept[cid] = dict(r) if r else None
        return concept[cid]

    calls = []
    for it in inters:
        iid, ts, prompt = it["id"], it["created_at"], it["prompt_text"] or ""
        warn = []
        derived = [r[0] for r in db.execute(
            "SELECT target FROM edges WHERE source=? AND edge_type='Derives' ORDER BY rowid",
            (iid,))]
        call_set = [get_concept(c) for c in derived if get_concept(c)]
        call_ids = {c["id"] for c in call_set}
        act_edges = db.execute(
            "SELECT source, target, edge_type FROM edges WHERE created_at=? AND "
            "edge_type IN ('Causal','Dependency') ORDER BY rowid", (ts,)).fetchall()
        hier = db.execute(
            "SELECT source, target FROM edges WHERE created_at=? AND edge_type='Hierarchical' "
            "ORDER BY rowid", (ts,)).fetchall()
        kind = None
        if act_edges:
            kind = "record_action"
        elif hier:
            kind = "derive"
        else:
            lk = ledger_kind(ledger, it["agent_id"], ts)
            only_resource = (len(call_set) <= 1 and
                             all(c["concept_type"] == "Resource" for c in call_set))
            if lk == "lambo_record_action" and only_resource:
                kind = "record_action"
            elif lk == "lambo_derive" or not only_resource:
                kind = "derive"
            else:
                kind = "record_action" if not call_set else "derive"
                warn.append(f"kind ambiguous (no ledger match), guessed {kind}")

        base = {"interaction_id": iid, "created_at": ts, "agent_id": it["agent_id"],
                "event_time": it["event_time"], "kind": kind,
                "orig_created": sum(1 for c in call_set if c["origin_interaction"] == iid),
                "orig_matched": sum(1 for c in call_set if c["origin_interaction"] != iid)}
        if kind == "record_action":
            produces, depends = [], []
            sources = {e["source"] for e in act_edges}
            if len(sources) > 1:
                warn.append("action edges from several sources at this timestamp")
            for e in act_edges:
                tc = get_concept(e["target"])
                if not tc:
                    warn.append("action edge target missing")
                    continue
                (produces if e["edge_type"] == "Causal" else depends).append(tc["content"])
            args = {"agent_id": it["agent_id"], "action": prompt}
            if produces:
                args["produces"] = produces
            if depends:
                args["depends_on"] = depends
        else:
            parent_of = []
            hier_ends = set()
            for h in hier:
                p, c = get_concept(h["source"]), get_concept(h["target"])
                if p and c:
                    parent_of.append({"parent": p["content"], "child": c["content"]})
                    hier_ends.update([p["id"], c["id"]])
            created_here = [c for c in call_set if c["origin_interaction"] == iid]
            segs = split_prompt(prompt, [c["content"] for c in created_here])
            by_content = {}
            for c in call_set:
                by_content.setdefault(c["content"], c)
            used = set()
            concepts = []
            for s in segs:
                c = by_content.get(s)
                if c is None or c["id"] in used:
                    ls = loose(s)
                    c = next((x for x in call_set if x["id"] not in used
                              and x["origin_interaction"] != iid and loose(x["content"]) == ls), None)
                if c is None:
                    c = next((x for x in call_set if x["id"] not in used
                              and x["origin_interaction"] != iid
                              and x["id"] not in hier_ends), None)
                    if c is not None:
                        warn.append("segment typed by position, not text")
                if c is None:
                    warn.append("segment with no concept; typed entity")
                    concepts.append({"content": s, "concept_type": "entity"})
                    continue
                used.add(c["id"])
                concepts.append({"content": s, "concept_type": WIRE_TYPE[c["concept_type"]]})
            leftover = [c for c in call_set if c["id"] not in used and c["id"] not in hier_ends]
            if leftover:
                warn.append(f"{len(leftover)} Derives target(s) not placed in the prompt")
            if not concepts:
                warn.append("no concepts reconstructed")
            args = {"agent_id": it["agent_id"], "concepts": concepts}
            if parent_of:
                args["parent_of"] = parent_of
            base["parent_of_only_ends_created"] = sum(
                1 for c in call_set if c["id"] in hier_ends and c["id"] not in used
                and c["origin_interaction"] == iid)
        if it["event_time"]:
            args["event_time"] = it["event_time"]
        base["tool"] = "lambo_" + kind
        base["arguments"] = args
        base["warnings"] = warn
        calls.append(base)
    db.close()
    return T, calls


def make_variants(calls, k):
    out = []
    seen = set()
    for c in calls:
        if c["kind"] != "derive":
            continue
        for con in c["arguments"]["concepts"]:
            if len(out) >= k:
                return out
            if con["concept_type"] == "observation" or con["content"] in seen:
                continue
            seen.add(con["content"])
            out.append({
                "interaction_id": None, "created_at": None, "agent_id": "replay-variant",
                "event_time": None, "kind": "derive", "tool": "lambo_derive", "variant": True,
                "orig_created": None, "orig_matched": None, "warnings": [],
                "parent_of_only_ends_created": 0,
                "arguments": {"agent_id": "replay-variant", "concepts": [
                    {"content": "Recap: " + con["content"], "concept_type": con["concept_type"]}]},
            })
    return out


# ---------------------------------------------------------------------------
# Store preparation
# ---------------------------------------------------------------------------

def sqlite_backup(src, dst, src_readonly=True):
    src_arg = f"file:{Path(src).resolve()}?mode=ro" if src_readonly else str(src)
    subprocess.run(["sqlite3", src_arg, f".backup '{dst}'"], check=True)


def truncate(db_path, T):
    db = sqlite3.connect(db_path)
    db.execute("PRAGMA foreign_keys=OFF")
    cur = db.cursor()
    cur.execute("CREATE TEMP TABLE dead(id TEXT PRIMARY KEY)")
    cur.execute("INSERT OR IGNORE INTO dead SELECT id FROM interactions WHERE created_at >= ?", (T,))
    cur.execute("INSERT OR IGNORE INTO dead SELECT id FROM concepts WHERE created_at >= ? "
                "OR origin_interaction IN (SELECT id FROM dead)", (T,))
    stats = {}
    stats["edges"] = cur.execute(
        "DELETE FROM edges WHERE created_at >= ? OR source IN (SELECT id FROM dead) "
        "OR target IN (SELECT id FROM dead)", (T,)).rowcount
    stats["concepts"] = cur.execute(
        "DELETE FROM concepts WHERE id IN (SELECT id FROM dead)").rowcount
    stats["interactions"] = cur.execute(
        "DELETE FROM interactions WHERE id IN (SELECT id FROM dead)").rowcount
    # Roll canonization status back to what it was at T.
    rolled = 0
    for node, frm in cur.execute(
            "SELECT node_id, from_status FROM canonization_events e WHERE occurred_at >= ? "
            "AND occurred_at = (SELECT min(occurred_at) FROM canonization_events e2 "
            "WHERE e2.node_id = e.node_id AND e2.occurred_at >= ?)", (T, T)).fetchall():
        rolled += db.execute("UPDATE concepts SET canonization_status=? WHERE id=?",
                             (frm, node)).rowcount
    stats["canon_status_rolled_back"] = rolled
    stats["canonization_events"] = cur.execute(
        "DELETE FROM canonization_events WHERE occurred_at >= ?", (T,)).rowcount
    for t in ("write_intents", "reservations", "session_leases", "lease_refusals",
              "session_stats"):
        try:
            stats[t] = cur.execute(f"DELETE FROM {t}").rowcount
        except sqlite3.OperationalError:
            pass
    db.commit()
    db.execute("VACUUM")
    db.close()
    return stats


def write_config(template, db_path, cfg_out, canon_secs, gc_secs, protect=()):
    text = Path(template).read_text()
    lines, section, done = [], None, False
    for ln in text.splitlines():
        m = re.match(r"\s*\[\[?([A-Za-z0-9_.]+)\]?\]\s*$", ln)
        if m:
            section = m.group(1)
        if section == "store" and re.match(r"\s*path\s*=", ln):
            ln = f'path = "{db_path}"'
            done = True
        lines.append(ln)
    if not done:
        raise SystemExit("config template has no [store] path")
    if re.search(r"^\s*\[daemon\]", text, re.M):
        log("config template already has [daemon]; leaving it as is")
    else:
        lines += ["", "# replay_parity: deterministic daemon cadence for the replay copy",
                  "[daemon]", f"canonization_eval_interval_secs = {canon_secs}",
                  f"gc_max_interval_secs = {gc_secs}"]
    out = "\n".join(lines) + "\n"
    store_sec = out.split("[store]", 1)[1].split("\n[", 1)[0]
    for root in protect:
        if str(Path(root).expanduser().resolve()) in store_sec or under(db_path, root):
            raise SystemExit(f"refusing: rewritten [store] points under protected {root}")
    Path(cfg_out).write_text(out)


def store_counts(db_path):
    db = ro_connect(db_path)
    q = lambda s, *a: db.execute(s, a).fetchall()
    out = {
        "concepts": q("SELECT count(*) FROM concepts")[0][0],
        "embedded": q("SELECT count(*) FROM concepts WHERE embedding IS NOT NULL")[0][0],
        "unembedded": q("SELECT count(*) FROM concepts WHERE embedding IS NULL")[0][0],
        "interactions": q("SELECT count(*) FROM interactions")[0][0],
        "concepts_by_type": dict(q("SELECT concept_type, count(*) FROM concepts GROUP BY 1")),
        "edges_by_type": dict(q("SELECT edge_type, count(*) FROM edges GROUP BY 1")),
        "canonization_status": dict(q(
            "SELECT canonization_status, count(*) FROM concepts GROUP BY 1")),
    }
    out["edges"] = sum(out["edges_by_type"].values())
    db.close()
    return out


def replay_facts(db_path, T):
    """Content-level facts about everything written at/after T in one copy."""
    db = ro_connect(db_path)
    created = collections.Counter(
        (c, t) for c, t in db.execute(
            "SELECT content, concept_type FROM concepts WHERE created_at >= ?", (T,)))
    sem = collections.Counter(db.execute(
        "SELECT s.content, t.content FROM edges e JOIN concepts s ON s.id=e.source "
        "JOIN concepts t ON t.id=e.target WHERE e.edge_type='Semantic' AND e.created_at >= ?",
        (T,)).fetchall())
    unemb = sorted(r[0] for r in db.execute(
        "SELECT content FROM concepts WHERE created_at >= ? AND embedding IS NULL", (T,)))
    hier_unemb = db.execute(
        "SELECT count(DISTINCT c.id) FROM concepts c JOIN edges e ON (e.source=c.id OR "
        "e.target=c.id) AND e.edge_type='Hierarchical' WHERE c.embedding IS NULL").fetchone()[0]
    canon_ev = dict(((f"{a}->{b}"), n) for a, b, n in db.execute(
        "SELECT from_status, to_status, count(*) FROM canonization_events "
        "WHERE occurred_at >= ? GROUP BY 1, 2", (T,)))
    db.close()
    return {"created": created, "semantic": sem, "unembedded_new": unemb,
            "hierarchical_ends_unembedded": hier_unemb, "canonization_events": canon_ev}


def id_contents(db_path, ids):
    if not ids:
        return {}
    db = ro_connect(db_path)
    out = {}
    for i in ids:
        r = db.execute("SELECT content, created_at FROM concepts WHERE id=?", (i,)).fetchone()
        out[i] = r
    db.close()
    return out


def semantic_targets(db_path, src_ids):
    if not src_ids:
        return []
    db = ro_connect(db_path)
    res = []
    for i in src_ids:
        for s, t, tc in db.execute(
                "SELECT s.content, t.content, t.created_at FROM edges e JOIN concepts s "
                "ON s.id=e.source JOIN concepts t ON t.id=e.target "
                "WHERE e.edge_type='Semantic' AND e.source=?", (i,)):
            res.append({"from": s, "to": t, "to_created_at": tc})
    db.close()
    return res


# ---------------------------------------------------------------------------
# MCP stdio client
# ---------------------------------------------------------------------------

class Serve:
    def __init__(self, binary, cfg, workdir, session, runtime_parent):
        # A fresh, short XDG_RUNTIME_DIR per serve (a unix socket path must
        # stay short, hence a short parent), removed on close.
        self.rt = tempfile.mkdtemp(prefix="lbrp.", dir=runtime_parent)
        env = dict(os.environ, XDG_RUNTIME_DIR=self.rt)
        env.pop("LAMBO_CONFIG", None)
        self.errf = open(Path(workdir) / "serve.stderr.log", "w")
        self.transcript = open(Path(workdir) / "rpc.jsonl", "w")
        self.p = subprocess.Popen(
            [binary, "serve", "--transport", "stdio", "--config", str(cfg),
             "--session", session, "--agent", "replay"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.errf,
            env=env, cwd=workdir, text=True, bufsize=1)
        self.q = queue.Queue()
        self.next_id = 1
        self.tool_calls = 0
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        for line in self.p.stdout:
            line = line.strip()
            if not line:
                continue
            self.transcript.write(json.dumps({"dir": "in", "t": time.time(), "line": line}) + "\n")
            try:
                self.q.put(json.loads(line))
            except ValueError:
                pass
        self.q.put(None)

    def send(self, obj):
        s = json.dumps(obj)
        self.transcript.write(json.dumps({"dir": "out", "t": time.time(), "line": s}) + "\n")
        self.p.stdin.write(s + "\n")
        self.p.stdin.flush()

    def request(self, method, params, timeout):
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        deadline = time.time() + timeout
        while True:
            left = deadline - time.time()
            if left <= 0:
                raise TimeoutError(f"{method} timed out after {timeout}s")
            try:
                msg = self.q.get(timeout=left)
            except queue.Empty:
                continue
            if msg is None:
                raise RuntimeError(f"serve exited during {method} (rc={self.p.poll()})")
            if msg.get("id") == rid:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg["result"]

    def initialize(self, timeout):
        r = self.request("initialize", {
            "protocolVersion": "2025-03-26", "capabilities": {},
            "clientInfo": {"name": "replay-parity", "version": "1"}}, timeout)
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return r

    def tool(self, name, args, timeout=180):
        self.tool_calls += 1
        r = self.request("tools/call", {"name": name, "arguments": args}, timeout)
        text = "\n".join(c.get("text", "") for c in r.get("content", []) if c.get("type") == "text")
        return {"is_error": bool(r.get("isError")), "structured": r.get("structuredContent"),
                "text": text}

    def close(self, timeout):
        try:
            self.p.stdin.close()
        except OSError:
            pass
        try:
            rc = self.p.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            log("serve did not exit after stdin close; terminating")
            self.p.terminate()
            try:
                rc = self.p.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.p.kill()
                rc = self.p.wait()
        self.errf.close()
        self.transcript.close()
        shutil.rmtree(self.rt, ignore_errors=True)
        return rc


RECEIPT_RE = re.compile(r"receipt (lwr\S+?):")


def receipt_of(ack):
    st = ack.get("structured") or {}
    if isinstance(st.get("receipt"), str):
        return st["receipt"]
    m = RECEIPT_RE.search(ack.get("text", ""))
    return m.group(1) if m else None


def wait_receipt(serve, agent, receipt, wait_ms, budget_s):
    t0 = time.time()
    polls = 0
    while True:
        polls += 1
        r = serve.tool("lambo_stats", {"agent_id": agent, "receipt": receipt, "wait_ms": wait_ms},
                       timeout=wait_ms / 1000 + 120)
        st = (r.get("structured") or {}).get("receipt")
        if st is None:
            m = re.search(r"receipt \S+: (\S+)", r.get("text", ""))
            st = {"state": m.group(1).rstrip(" —-,") if m else "unknown", "detail": r.get("text")}
        if st.get("state") in TERMINAL_STATES or time.time() - t0 > budget_s:
            st["_polls"] = polls
            st["_wait_s"] = round(time.time() - t0, 3)
            return st


def run_side(label, binary, base_db, template, workdir, calls, T, a):
    workdir.mkdir(parents=True, exist_ok=True)
    db = workdir / "replay.db"
    for suffix in ("", "-wal", "-shm"):
        p = Path(str(db) + suffix)
        if p.exists():
            p.unlink()
    sqlite_backup(base_db, db, src_readonly=False)
    cfg = workdir / "lambo.toml"
    write_config(template, db.resolve(), cfg, a.canon_eval_secs, a.gc_max_interval_secs,
                 a.protect)
    # Each binary migrates its own copy (0.4.0 adds concepts.embedding_source and
    # refuses to serve an unmigrated store). provision builds no embedder.
    pv = subprocess.run([str(binary), "provision", "--config", str(cfg)],
                        capture_output=True, text=True, cwd=workdir,
                        env={k: v for k, v in os.environ.items() if k != "LAMBO_CONFIG"})
    (workdir / "provision.log").write_text(pv.stdout + pv.stderr)
    if pv.returncode != 0:
        raise SystemExit(f"[{label}] provision failed: {(pv.stdout + pv.stderr)[-500:]}")
    log(f"[{label}] starting {binary} on {db}")
    s = Serve(binary, cfg, workdir, a.session, a.runtime_parent)
    results = []
    try:
        t_init = time.time()
        init = s.initialize(a.startup_timeout)
        log(f"[{label}] initialized in {time.time() - t_init:.1f}s "
            f"({(init.get('serverInfo') or {}).get('version')})")
        for i, c in enumerate(calls):
            rec = {"idx": i}
            t0 = time.time()
            try:
                ack = s.tool(c["tool"], c["arguments"])
            except Exception as e:  # transport failure: stop this side
                rec.update(state="transport_error", detail=str(e))
                results.append(rec)
                raise
            rec["ack_ms"] = round((time.time() - t0) * 1000, 1)
            if ack["is_error"]:
                rec.update(state="refused", detail=ack["text"][:2000])
                results.append(rec)
                log(f"[{label}] {i + 1}/{len(calls)} {c['tool']} refused: {ack['text'][:160]}")
                continue
            receipt = receipt_of(ack)
            if not receipt:
                rec.update(state="no_receipt", detail=ack["text"][:2000])
                results.append(rec)
                continue
            r = wait_receipt(s, c["arguments"]["agent_id"], receipt, a.wait_ms, a.receipt_budget)
            rec["receipt"] = r
            rec["state"] = r.get("state")
            rec["applied_ms"] = round((time.time() - t0) * 1000, 1)
            created = list(r.get("created") or [])
            matched = list(r.get("matched") or [])
            rec["created_ids"], rec["matched_ids"] = created, matched
            results.append(rec)
            if (i + 1) % 10 == 0 or i + 1 == len(calls):
                log(f"[{label}] {i + 1}/{len(calls)} replayed (last: {rec['state']}, "
                    f"{rec['applied_ms']:.0f} ms)")
        if a.canon_wait > 0:
            log(f"[{label}] waiting {a.canon_wait}s for canonization passes")
            time.sleep(a.canon_wait)
        final = s.tool("lambo_stats", {"agent_id": "replay"}) if not a.no_final_stats else None
    finally:
        rc = s.close(a.shutdown_timeout)
        log(f"[{label}] serve exited rc={rc}; {s.tool_calls} tool calls")
    # Resolve receipt ids to content now that the store is flushed and closed.
    all_ids = [i for r in results for i in r.get("created_ids", []) + r.get("matched_ids", [])]
    contents = id_contents(db, all_ids)
    for r in results:
        r["created"] = sorted((contents.get(i) or ("<missing>",))[0] for i in r.get("created_ids", []))
        r["matched"] = sorted((contents.get(i) or ("<missing>",))[0] for i in r.get("matched_ids", []))
        r["semantic_merges"] = semantic_targets(db, r.get("created_ids", []))
    st = (final or {}).get("structured") or {}
    final_stats = {k: st.get(k) for k in (
        "concept_count", "edge_count", "embedded_concepts", "canonical_count",
        "canonization_cycles", "canonization_failures", "write_queue_applied",
        "write_queue_failed", "write_queue_apply_ms_p50", "write_queue_apply_ms_p90",
        "write_queue_apply_ms_max", "embedding_contract") if k in st}
    return {"binary": str(binary), "db": str(db), "exit_code": rc, "tool_calls": s.tool_calls,
            "results": results, "final_stats_tool": final_stats,
            "store": store_counts(db), "facts": replay_facts(db, T)}


# ---------------------------------------------------------------------------
# Comparison
# ---------------------------------------------------------------------------

FIELDS = ("state", "created_count", "matched_count", "semantic_merged", "reinforced",
          "edges", "embedded")


def summary_of(r):
    rc = r.get("receipt") or {}
    out = {"state": r.get("state")}
    for k in FIELDS[1:]:
        out[k] = rc.get(k)
    return out


# Reason catalogue: reason id -> {label, expected, ref[, kind, detail_pattern]}.
#
# THIS BUILT-IN CATALOGUE IS 0.4.0-SPECIFIC: it is the 0.4.0 CHANGELOG mapping
# used for the 0.4.0 vs 0.3.0 run (evidence/0.4.0-vs-0.3.0-load/). A later
# release passes --reasons FILE with its own entries (see load_catalogue). A
# reason id that is absent here, or present with "expected": false, is
# reported as UNEXPECTED.
REASONS_0_4_0 = {
    "state_differs:image_suffix": {
        "kind": "state_differs", "detail_pattern": r"(?i)image",
        "label": "derived text carries an [image:<id>]-shaped token, refused by design",
        "expected": True, "ref": "0.4.0 CHANGELOG, #22 PR 4"},
    "state_differs:refusal_limits": {
        "kind": "state_differs",
        "detail_pattern": r"(?i)(context|MAX_HYBRID_EMBEDS|too many|too large|exceeds|MiB)",
        "label": "refusal rules changed (#74 context cap / MAX_HYBRID_EMBEDS=256 / 4 MiB cap)",
        "expected": True, "ref": "0.4.0 CHANGELOG, #74"},
    "receipt_gains_embedded": {
        "label": "record_action receipt gains `embedded` (receipt only, not the store)",
        "expected": True, "ref": "0.4.0 CHANGELOG"},
    "parent_of_ends_embedded": {
        "label": "parent_of ends are embedded", "expected": True, "ref": "0.4.0 CHANGELOG, #16 §2"},
    "parent_of_embedded_differs": {
        "label": "embedded differs on a parent_of call (likely parent_of ends embedded)",
        "expected": True, "ref": "0.4.0 CHANGELOG, #16 §2"},
    "merge_into_replayed_concept": {
        "label": "semantic merge into a concept written during the replay: the SQLite holder "
                 "now ranks unflushed concepts in RAM",
        "expected": True, "ref": "0.4.0 CHANGELOG, #8 (#60 same idea on Postgres)"},
    "merge_differs_pre_t": {
        "label": "semantic merge differs against pre-T memory: exact-cosine ranking / candidate "
                 "source change, or embedding drift near the 0.85 bar",
        "expected": True, "ref": "0.4.0 CHANGELOG, #8/#60"},
    "canonical_key_invisible": {
        "label": "canonical-key change for invisible codepoints",
        "expected": True, "ref": "0.4.0 CHANGELOG, #25"},
    "reinforced_cascade": {
        "label": "reinforced differs: edge state diverged after an earlier differing call",
        "expected": True, "ref": "consequence of an earlier difference"},
}
BUILTIN_CATALOGUES = {"0.4.0": REASONS_0_4_0}

# Detector ids that are never in a catalogue: always UNEXPECTED.
UNEXPECTED_IDS = {
    "state_differs": "write outcome differs, refusal text matches no catalogued reason",
    "canonical_match_differs": "canonical match differs without a merge change",
    "action_edges_differ": "record_action edge count differs",
    "unclassified": "differs, no detector fired",
}


def load_catalogue(path=None):
    """(catalogue dict, description). No path: the built-in 0.4.0 catalogue.

    A file is JSON: {"base": "0.4.0" | null, "reasons": {id: {...}}}. With a
    base, its entries are laid over that built-in catalogue (override or
    extend; set "expected": false to retire one); without, they replace it.
    """
    if not path:
        return dict(REASONS_0_4_0), "builtin 0.4.0"
    d = json.loads(Path(path).read_text())
    if not isinstance(d, dict) or not isinstance(d.get("reasons"), dict):
        raise SystemExit(f"{path}: expected an object with a 'reasons' object")
    base = d.get("base")
    if base is not None and base not in BUILTIN_CATALOGUES:
        raise SystemExit(f"{path}: unknown base {base!r} (have {sorted(BUILTIN_CATALOGUES)})")
    cat = dict(BUILTIN_CATALOGUES[base]) if base else {}
    for rid, entry in d["reasons"].items():
        if not isinstance(entry, dict) or "label" not in entry:
            raise SystemExit(f"{path}: reason {rid!r} needs an object with a 'label'")
        if entry.get("detail_pattern"):
            re.compile(entry["detail_pattern"])
        cat[rid] = {**cat.get(rid, {}), **entry}
    return cat, f"{path} (base {base})" if base else str(path)


def state_reason(catalogue, detail):
    """The catalogue id of a state difference whose refusal text is `detail`."""
    for rid in sorted(catalogue):
        e = catalogue[rid]
        if (e.get("kind") == "state_differs" and e.get("detail_pattern")
                and re.search(e["detail_pattern"], detail or "")):
            return rid
    return "state_differs"


INVISIBLE_RE = re.compile("[​-‏⁠-⁤﻿­͏]")


def classify(call, sa, sb, ra, rb, T, catalogue):
    """Reason ids for one differing call (detectors only; no expectations)."""
    if sa["state"] != sb["state"]:
        if sb["state"] != "applied":
            detail = rb.get("detail") or (rb.get("receipt") or {}).get("detail") or ""
        else:
            detail = ra.get("detail") or (ra.get("receipt") or {}).get("detail") or ""
        return [state_reason(catalogue, detail)]
    ids = []
    ends = call.get("parent_of_only_ends_created") or 0
    if sa["embedded"] != sb["embedded"]:
        if sa["embedded"] is None and call["kind"] == "record_action":
            ids.append("receipt_gains_embedded")
        elif (sa["embedded"] is not None and sb["embedded"] is not None and ends
              and sb["embedded"] - sa["embedded"]
              == (sb.get("created_count") or 0) - (sa.get("created_count") or 0) + ends):
            ids.append("parent_of_ends_embedded")
        elif call.get("arguments", {}).get("parent_of"):
            ids.append("parent_of_embedded_differs")
    merge_diff = (sa["semantic_merged"] != sb["semantic_merged"]
                  or sorted(map(json.dumps, ra.get("semantic_merges", []))) !=
                  sorted(map(json.dumps, rb.get("semantic_merges", []))))
    if merge_diff:
        fresh = [m for m in ra.get("semantic_merges", []) + rb.get("semantic_merges", [])
                 if (m.get("to_created_at") or "") >= T]
        ids.append("merge_into_replayed_concept" if fresh else "merge_differs_pre_t")
    if sa["matched_count"] != sb["matched_count"] or ra.get("matched") != rb.get("matched"):
        if INVISIBLE_RE.search(json.dumps(call.get("arguments", {}), ensure_ascii=False)):
            ids.append("canonical_key_invisible")
        elif not merge_diff:
            ids.append("canonical_match_differs")
    if sa["reinforced"] != sb["reinforced"] and not ids:
        ids.append("reinforced_cascade")
    if sa["edges"] != sb["edges"] and call["kind"] == "record_action" and not ids:
        ids.append("action_edges_differ")
    if not ids:
        ids.append("unclassified")
    return ids


def resolve_reason(rid, catalogue):
    """{id, label, expected, ref} for a detector id under a catalogue."""
    e = catalogue.get(rid)
    if e is None or rid in UNEXPECTED_IDS:
        return {"id": rid, "label": UNEXPECTED_IDS.get(rid, "not in the reason catalogue"),
                "expected": False, "ref": None}
    return {"id": rid, "label": e["label"], "expected": bool(e.get("expected", True)),
            "ref": e.get("ref")}


def group_reasons(differing):
    """Aggregate resolved reasons over differing rows: id -> counts and label."""
    out = {}
    for d in differing:
        for r in d["reasons"]:
            g = out.setdefault(r["id"], {"n": 0, "label": r["label"],
                                         "expected": r["expected"], "ref": r["ref"]})
            g["n"] += 1
    return dict(sorted(out.items(), key=lambda kv: (-kv[1]["n"], kv[0])))


def counter_diff(ca, cb, keyfmt):
    only_a = ca - cb
    only_b = cb - ca
    return {"only_a": [keyfmt(k) for k in sorted(only_a.elements())],
            "only_b": [keyfmt(k) for k in sorted(only_b.elements())]}


def compare(calls, A, B, T, base_counts, catalogue):
    per_call, differing = [], []
    totals = {s: collections.Counter() for s in ("a", "b")}
    for i, c in enumerate(calls):
        ra = A["results"][i] if i < len(A["results"]) else {"state": "not_run"}
        rb = B["results"][i] if i < len(B["results"]) else {"state": "not_run"}
        sa, sb = summary_of(ra), summary_of(rb)
        for side, s in (("a", sa), ("b", sb)):
            totals[side]["calls"] += 1
            totals[side][f"state:{s['state']}"] += 1
            for k in FIELDS[1:]:
                if isinstance(s.get(k), int):
                    totals[side][k] += s[k]
        same = (sa == sb and ra.get("created") == rb.get("created")
                and ra.get("matched") == rb.get("matched")
                and sorted(map(json.dumps, ra.get("semantic_merges", []))) ==
                sorted(map(json.dumps, rb.get("semantic_merges", []))))
        row = {"idx": i, "kind": c["kind"], "agent_id": c["agent_id"],
               "interaction_id": c.get("interaction_id"), "variant": bool(c.get("variant")),
               "orig_created": c.get("orig_created"), "orig_matched": c.get("orig_matched"),
               "a": sa, "b": sb, "same": same,
               "ack_ms": [ra.get("ack_ms"), rb.get("ack_ms")],
               "applied_ms": [ra.get("applied_ms"), rb.get("applied_ms")]}
        if not same:
            row["reasons"] = [resolve_reason(r, catalogue)
                              for r in classify(c, sa, sb, ra, rb, T, catalogue)]
            row["unexpected"] = any(not r["expected"] for r in row["reasons"])
            row["detail"] = {  # store content: stripped unless --include-content
                "created_only_a": sorted(set(ra.get("created", [])) - set(rb.get("created", []))),
                "created_only_b": sorted(set(rb.get("created", [])) - set(ra.get("created", []))),
                "matched_a": ra.get("matched"), "matched_b": rb.get("matched"),
                "merges_a": ra.get("semantic_merges"), "merges_b": rb.get("semantic_merges"),
                "refusal_a": ra.get("detail") if sa["state"] != "applied" else None,
                "refusal_b": rb.get("detail") if sb["state"] != "applied" else None,
            }
            differing.append(row)
        per_call.append(row)
    fa, fb = A["facts"], B["facts"]
    return {
        "per_call": per_call,
        "differing": differing,
        "reasons": group_reasons(differing),
        "unexpected_calls": sum(1 for d in differing if d["unexpected"]),
        "totals": {"a": dict(totals["a"]), "b": dict(totals["b"])},
        "store": {"base": base_counts, "a": A["store"], "b": B["store"]},
        "final_stats_tool": {"a": A["final_stats_tool"], "b": B["final_stats_tool"]},
        "content_diff": {
            "concepts_created": counter_diff(fa["created"], fb["created"],
                                             lambda k: {"content": k[0], "type": k[1]}),
            "semantic_edges": counter_diff(fa["semantic"], fb["semantic"],
                                           lambda k: {"from": k[0], "to": k[1]}),
            "unembedded_new": {"a": len(fa["unembedded_new"]), "b": len(fb["unembedded_new"])},
            "hierarchical_ends_unembedded": {"a": fa["hierarchical_ends_unembedded"],
                                             "b": fb["hierarchical_ends_unembedded"]},
        },
        "canonization": {
            "status_counts": {"base": base_counts["canonization_status"],
                              "a": A["store"]["canonization_status"],
                              "b": B["store"]["canonization_status"]},
            "events_since_T": {"a": fa["canonization_events"], "b": fb["canonization_events"]},
        },
    }


def strip_content(cmp_):
    """A copy of compare()'s result with every piece of store text removed:
    per-call detail dropped, content-diff lists reduced to counts."""
    out = dict(cmp_)
    rows = []
    for r in cmp_["per_call"]:
        r = {k: v for k, v in r.items() if k != "detail"}
        rows.append(r)
    out["per_call"] = rows
    out["differing"] = [r for r in rows if not r["same"]]
    cd = dict(cmp_["content_diff"])
    for k in ("concepts_created", "semantic_edges"):
        cd[k] = {side: len(v) for side, v in cmp_["content_diff"][k].items()}
    out["content_diff"] = cd
    return out


def _n(x):
    return x if isinstance(x, int) else len(x)


def render_text(meta, cmp_):
    L = []
    L.append(f"replay parity  A={meta['bin_a']}  B={meta['bin_b']}")
    L.append(f"snapshot {meta['snapshot']}  session={meta['session']}  T={meta['T']}  "
             f"calls={meta['n_calls']} (derive={meta['n_derive']}, "
             f"record_action={meta['n_action']}, variants={meta['n_variants']}, "
             f"reconstruction warnings={meta['n_warn']})")
    L.append(f"reason catalogue: {meta['catalogue']}")
    L.append("")
    L.append("totals (receipt sums)      A          B")
    keys = sorted(set(cmp_["totals"]["a"]) | set(cmp_["totals"]["b"]))
    for k in keys:
        L.append(f"  {k:<24}{cmp_['totals']['a'].get(k, 0):>6}     {cmp_['totals']['b'].get(k, 0):>6}")
    L.append("")
    sa, sb, s0 = cmp_["store"]["a"], cmp_["store"]["b"], cmp_["store"]["base"]
    L.append("final store                base       A          B")
    for k in ("concepts", "embedded", "unembedded", "interactions", "edges"):
        L.append(f"  {k:<24}{s0[k]:>6}     {sa[k]:>6}     {sb[k]:>6}")
    for et in sorted(set(sa["edges_by_type"]) | set(sb["edges_by_type"])):
        L.append(f"  edges.{et:<18}{s0['edges_by_type'].get(et, 0):>6}     "
                 f"{sa['edges_by_type'].get(et, 0):>6}     {sb['edges_by_type'].get(et, 0):>6}")
    for st in sorted(set(sa["canonization_status"]) | set(sb["canonization_status"])
                     | set(s0["canonization_status"])):
        L.append(f"  canon.{st:<18}{s0['canonization_status'].get(st, 0):>6}     "
                 f"{sa['canonization_status'].get(st, 0):>6}     {sb['canonization_status'].get(st, 0):>6}")
    ev = cmp_["canonization"]["events_since_T"]
    L.append(f"  canonization events since T: A={ev['a']}  B={ev['b']}")
    cd = cmp_["content_diff"]
    L.append(f"  created-concept content diff: only A={_n(cd['concepts_created']['only_a'])} "
             f"only B={_n(cd['concepts_created']['only_b'])}; semantic edges only A="
             f"{_n(cd['semantic_edges']['only_a'])} only B={_n(cd['semantic_edges']['only_b'])}")
    L.append(f"  hierarchical ends without vector: A={cd['hierarchical_ends_unembedded']['a']} "
             f"B={cd['hierarchical_ends_unembedded']['b']}")
    L.append("")
    L.append(f"differing calls: {len(cmp_['differing'])} of {len(cmp_['per_call'])}; "
             f"UNEXPECTED: {cmp_['unexpected_calls']}")
    for rid, g in cmp_["reasons"].items():
        tag = "" if g["expected"] else "UNEXPECTED "
        ref = f" [{g['ref']}]" if g.get("ref") else ""
        L.append(f"  {g['n']:>4}  {tag}{rid}: {g['label']}{ref}")
    L.append("")
    for d in cmp_["differing"]:
        diffs = {k: (d["a"][k], d["b"][k]) for k in FIELDS if d["a"][k] != d["b"][k]}
        L.append(f"  #{d['idx']:<4}{d['kind']:<14}{str(d['agent_id'])[:22]:<23}{diffs}")
        for r in d["reasons"]:
            L.append(f"        -> {'' if r['expected'] else 'UNEXPECTED '}{r['id']}")
    return "\n".join(L) + "\n"


def check_paths(out, work, snapshot, protect, allow_live_name):
    """Refuse unsafe paths (SystemExit). The snapshot is only ever opened
    read-only, so it may live under a protected directory; outputs may not."""
    for label, p in (("--out", out), ("--work", work)):
        for root in protect:
            if under(p, root):
                raise SystemExit(f"refusing: {label} {p} is under protected {root}")
    if not allow_live_name and any(snapshot.name.startswith(x) for x in LIVE_DB_PREFIXES):
        raise SystemExit(f"refusing: --snapshot {snapshot.name} is named like a live store; "
                         "pass a backup, or --allow-live-name if it is one")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin-a", required=True, help="baseline lambo binary")
    ap.add_argument("--bin-b", required=True, help="candidate lambo binary")
    ap.add_argument("--snapshot", required=True,
                    help="a backup of the store (SQLite); opened read-only")
    ap.add_argument("--config-template", required=True,
                    help="a lambo.toml whose [store] path is rewritten per side")
    ap.add_argument("--session", required=True, help="session id whose writes are replayed")
    ap.add_argument("--groups", type=int, required=True,
                    help="replay the last N write calls (interactions) of the snapshot")
    ap.add_argument("--out", required=True,
                    help="directory for parity.json / parity.txt (aggregates)")
    ap.add_argument("--work", help="directory for store copies, reconstructed calls, "
                    "per-side results and transcripts, all holding store content "
                    "(default: a fresh temp directory; never commit it)")
    ap.add_argument("--protect", action="append", metavar="PATH",
                    help=f"directory nothing may be written under; repeatable "
                         f"(default {DEFAULT_PROTECT}; passing any replaces the default)")
    ap.add_argument("--allow-live-name", action="store_true",
                    help=f"accept a snapshot named like a live store ({', '.join(LIVE_DB_PREFIXES)}*)")
    ap.add_argument("--reasons", help="JSON reason catalogue (see the header); "
                    "default: the built-in 0.4.0 CHANGELOG mapping")
    ap.add_argument("--include-content", action="store_true",
                    help="keep concept text and refusal text in parity.json/parity.txt "
                         "(default: omitted, so --out can be committed)")
    ap.add_argument("--offset", type=int, default=0,
                    help="skip the newest K write calls (they are truncated away, not replayed)")
    ap.add_argument("--variants", type=int, default=0,
                    help="extra 'Recap: <concept>' derives after the replay (semantic-merge probe)")
    ap.add_argument("--ledger",
                    help="read-only serve call ledger (calls.jsonl), used only to classify "
                         "ambiguous calls")
    ap.add_argument("--runtime-parent", default="/tmp",
                    help="parent of each serve's fresh XDG_RUNTIME_DIR (keep it short: "
                         "unix socket paths are length-limited)")
    ap.add_argument("--wait-ms", type=int, default=4000)
    ap.add_argument("--receipt-budget", type=float, default=120.0)
    ap.add_argument("--startup-timeout", type=float, default=600.0)
    ap.add_argument("--shutdown-timeout", type=float, default=180.0)
    ap.add_argument("--canon-wait", type=float, default=20.0,
                    help="seconds to idle after the writes so the daemon's canonization runs")
    ap.add_argument("--canon-eval-secs", type=int, default=5)
    ap.add_argument("--gc-max-interval-secs", type=int, default=315360000)
    ap.add_argument("--no-final-stats", action="store_true")
    ap.add_argument("--only", choices=("a", "b"), help="run one side only (debugging)")
    ap.add_argument("--dry-run", action="store_true",
                    help="reconstruct and truncate only; do not start any binary")
    a = ap.parse_args(argv)

    a.protect = [Path(p).expanduser().resolve() for p in (a.protect or [DEFAULT_PROTECT])]
    out = Path(a.out).expanduser().resolve()
    work = (Path(a.work).expanduser().resolve() if a.work
            else Path(tempfile.mkdtemp(prefix="replay-parity-work.")).resolve())
    snapshot = Path(a.snapshot).expanduser().resolve()
    template = Path(a.config_template).expanduser().resolve()
    bins = {"a": Path(a.bin_a).expanduser().resolve(), "b": Path(a.bin_b).expanduser().resolve()}
    check_paths(out, work, snapshot, a.protect, a.allow_live_name)
    if not snapshot.is_file():
        raise SystemExit(f"no such snapshot: {snapshot}")
    if not a.dry_run:
        for b in bins.values():
            if not os.access(b, os.X_OK):
                raise SystemExit(f"not executable: {b}")
    catalogue, cat_desc = load_catalogue(a.reasons)
    out.mkdir(parents=True, exist_ok=True)
    work.mkdir(parents=True, exist_ok=True)
    log(f"work directory (holds store content, never commit it): {work}")

    src_copy = work / "snapshot-copy.db"
    for suffix in ("", "-wal", "-shm"):
        p = Path(str(src_copy) + suffix)
        if p.exists():
            p.unlink()
    log(f"copying snapshot read-only -> {src_copy}")
    sqlite_backup(snapshot, src_copy, src_readonly=True)

    ledger = load_ledger_calls(a.ledger)
    T, calls = reconstruct(src_copy, a.session, a.groups, ledger, a.offset)
    variants = make_variants(calls, a.variants)
    all_calls = calls + variants
    (work / "calls.json").write_text(json.dumps(all_calls, indent=1))
    n_warn = sum(1 for c in calls if c["warnings"])
    log(f"T={T}: {len(calls)} calls reconstructed "
        f"({sum(c['kind'] == 'derive' for c in calls)} derive, "
        f"{sum(c['kind'] == 'record_action' for c in calls)} record_action), "
        f"{n_warn} with warnings, +{len(variants)} variants")

    base = work / "base-truncated.db"
    shutil.copyfile(src_copy, base)
    tstats = truncate(base, T)
    log(f"truncated base: removed {tstats}")
    base_counts = store_counts(base)
    meta = {"bin_a": str(bins["a"]), "bin_b": str(bins["b"]), "snapshot": str(snapshot),
            "session": a.session, "T": T, "n_calls": len(all_calls),
            "n_derive": sum(c["kind"] == "derive" for c in calls),
            "n_action": sum(c["kind"] == "record_action" for c in calls),
            "n_variants": len(variants), "n_warn": n_warn, "truncation": tstats,
            "catalogue": cat_desc, "content_included": a.include_content,
            "reconstruction_warnings": [
                {"idx": i, "interaction_id": c["interaction_id"], "warnings": c["warnings"]}
                for i, c in enumerate(calls) if c["warnings"]]}
    if a.dry_run:
        meta["base_counts"] = base_counts
        (out / "parity.json").write_text(json.dumps({"meta": meta}, indent=1))
        log(f"dry run: wrote {work / 'calls.json'} and {out / 'parity.json'}")
        return 0

    sides = {}
    for label in ("a", "b"):
        if a.only and a.only != label:
            continue
        sides[label] = run_side(label.upper(), bins[label], base, template,
                                work / f"side-{label}", all_calls, T, a)
        dump = dict(sides[label])
        dump["facts"] = {k: ([{"key": list(kk), "n": n} for kk, n in v.items()]
                             if isinstance(v, collections.Counter) else v)
                         for k, v in sides[label]["facts"].items()}
        (work / f"side-{label}.json").write_text(json.dumps(dump, indent=1, default=list))
    if len(sides) < 2:
        log("one side only; no comparison")
        return 0
    cmp_ = compare(all_calls, sides["a"], sides["b"], T, base_counts, catalogue)
    if not a.include_content:
        cmp_ = strip_content(cmp_)
    meta["tool_calls"] = {k: v["tool_calls"] for k, v in sides.items()}
    meta["exit_codes"] = {k: v["exit_code"] for k, v in sides.items()}
    (out / "parity.json").write_text(json.dumps({"meta": meta, **cmp_}, indent=1, default=list))
    text = render_text(meta, cmp_)
    (out / "parity.txt").write_text(text)
    print(text)
    log(f"wrote {out / 'parity.json'} and {out / 'parity.txt'} (work files in {work})")
    if cmp_["unexpected_calls"]:
        log(f"{cmp_['unexpected_calls']} differing call(s) are UNEXPECTED")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
