import sys, json
sys.path.insert(0, sys.argv[1]); import mcp_ab
c = mcp_ab.Mcp("http://127.0.0.1:17707/mcp")
qs = sorted({json.loads(l)["query"] for l in open("queries_raw.jsonl")})
ok = bad = 0
unconfirmed = []
for i, q in enumerate(qs):
    # One concept per call, so no other concept's text is in the call. The stored vector is
    # still a FRAMED vector (the concept framing of context_text), not the bare vector recall
    # embeds for the same text. See the README, "Query vectors".
    ms, body, err = c.tool("lambo_derive", concepts=[{"content": q, "concept_type": "observation"}])
    if err or not body.get("receipt"): bad += 1; continue
    _, st, st_err = c.tool("lambo_stats", receipt=body["receipt"], wait_ms=8000)
    state = (st.get("receipt") or {}).get("state")
    if not st_err and state == "applied":
        ok += 1
    else:
        # Not resubmitted: the write may still land. Reconcile before exporting vectors.
        unconfirmed.append((i, body["receipt"], "status call failed" if st_err else state))
print("queries", len(qs), "applied", ok, "refused", bad, "unconfirmed", len(unconfirmed))
for i, receipt, state in unconfirmed:
    print(f"  unconfirmed: query #{i} receipt {receipt} state {state}")
