import sys, json
sys.path.insert(0, sys.argv[1]); import mcp_ab
c = mcp_ab.Mcp("http://127.0.0.1:17707/mcp")
qs = sorted({json.loads(l)["query"] for l in open("queries_raw.jsonl")})
ok = bad = 0
for i, q in enumerate(qs):
    # one concept per call; prefix-free so the vector is the query text's own
    ms, body, err = c.tool("lambo_derive", concepts=[{"content": q, "concept_type": "observation"}])
    if err or not body.get("receipt"): bad += 1; continue
    _, st, _ = c.tool("lambo_stats", receipt=body["receipt"], wait_ms=8000)
    ok += (st.get("receipt") or {}).get("state") == "applied"
print("queries", len(qs), "applied", ok, "refused", bad)
