#!/usr/bin/env python3
"""Per-agent token usage and wall time from Claude Code transcripts.

Usage: extract.py <session.jsonl> <subagents-dir> <out.jsonl>

One row per transcript (the main session and each subagent). An assistant
message can be written over several lines that repeat its usage, so usage is
counted once per message id (the last line seen for that id wins).
"""
import json
import os
import sys
from datetime import datetime, timezone

FIELDS = ("input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens")


def ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


def scan(path):
    per_msg = {}
    models = {}
    stamps = []
    first_prompt = None
    with open(path, errors="replace") as f:
        for line in f:
            try:
                d = json.loads(line)
            except ValueError:
                continue
            t = d.get("timestamp")
            if t:
                stamps.append(ts(t))
            if first_prompt is None and d.get("type") == "user":
                c = d.get("message", {}).get("content")
                if isinstance(c, str):
                    first_prompt = c[:300]
                elif isinstance(c, list):
                    for b in c:
                        if isinstance(b, dict) and b.get("type") == "text":
                            first_prompt = b["text"][:300]
                            break
            if d.get("type") != "assistant":
                continue
            m = d.get("message", {})
            u = m.get("usage")
            mid = m.get("id") or d.get("requestId")
            if not u or not mid:
                continue
            per_msg[mid] = (m.get("model"), u)
    tot = {k: 0 for k in FIELDS}
    thinking = 0
    for model, u in per_msg.values():
        for k in FIELDS:
            tot[k] += u.get(k) or 0
        thinking += (u.get("output_tokens_details") or {}).get("thinking_tokens") or 0
        models[model] = models.get(model, 0) + 1
    stamps.sort()
    # Active time: sum of gaps between consecutive records, ignoring gaps over
    # 5 minutes (idle waiting on the user or on a background job).
    active = sum(b - a for a, b in zip(stamps, stamps[1:]) if b - a <= 300)
    return {
        "messages": len(per_msg),
        **tot,
        "thinking_tokens": thinking,
        "models": models,
        "start": datetime.fromtimestamp(stamps[0], timezone.utc).isoformat() if stamps else None,
        "end": datetime.fromtimestamp(stamps[-1], timezone.utc).isoformat() if stamps else None,
        "span_s": round(stamps[-1] - stamps[0]) if stamps else 0,
        "active_s": round(active),
        "first_prompt": first_prompt,
    }


def main():
    session, subdir, out = sys.argv[1:4]
    rows = []
    r = scan(session)
    r.update(kind="main", agent="main", description="main session")
    rows.append(r)
    for name in sorted(os.listdir(subdir)):
        if not name.endswith(".jsonl"):
            continue
        aid = name[len("agent-"):-len(".jsonl")]
        meta = {}
        mp = os.path.join(subdir, f"agent-{aid}.meta.json")
        if os.path.exists(mp):
            with open(mp) as f:
                meta = json.load(f)
        r = scan(os.path.join(subdir, name))
        r.update(kind="subagent", agent=aid, description=meta.get("description"),
                 agent_type=meta.get("agentType"), depth=meta.get("spawnDepth"),
                 finished=meta.get("finished"))
        rows.append(r)
    with open(out, "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    print(f"{len(rows)} transcripts -> {out}")


if __name__ == "__main__":
    main()
