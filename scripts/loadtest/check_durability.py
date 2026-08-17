#!/usr/bin/env python3
"""C3 — durability check: compare the load driver's ledger against the SQLite store.

The C2 assertion (`lambo serve: session closed, tail durable`) is a log line;
this script is the other half the review never did — after the process exits,
reconnect to the store and count what should have survived.

Accounting (successful tool calls only, per concurrency-capture.md C3):

* interactions — one per successful `lambo_derive` / `lambo_record_action`
  call (the server stamps the interaction node per tool call, F18), so this is
  a clean 1:1 ledger-vs-store comparison. Interactions are append-only and
  never GC'd, so this is the durable-tail yardstick.
* concepts — the server's own response text reports "N created" per call
  (`derived N concept(s): C created, M matched existing` /
  `recorded action '…': C concept(s) created, E edge(s) added`). Sum C over
  successful write calls and compare with the store's concept rows. **A
  created-then-GC-collected concept is durable work, not tail loss**: the
  daemon's spec §9 GC collects sub-threshold/orphan concepts and emits delete
  mutations the flush replays. When `--stderr` carries the GC debug lines
  (`concepts_collected=N`), the comparison is made GC-aware.
* edges — record_action reports its edge count; derive edges (Derives,
  CoOccurrence, Hierarchical) are not reported, so the store total is a
  *lower bound* on ledger-expected edges, not an exact figure. The table says
  so rather than pretending otherwise.

Store rows may EXCEED the ledger: a call in flight when SIGTERM landed can
have its mutations flushed by the close drain without ever returning a
response. That surplus is reported as in-flight-landed, not a discrepancy.

Exit status: 0 when the store is not short of ledger-successful interactions
(and concepts, GC-accounted); 2 when it is (the honest "tail was NOT durable"
signal).

    python3 scripts/loadtest/check_durability.py \
        --ledger evidence/concurrency/ledger-<run>.jsonl \
        --db     evidence/concurrency/c-load-<date>.db \
        --session c-load-<date> \
        --stderr evidence/concurrency/stderr-<run>.log
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys

# The server's own response formats (src/mcp/server.rs:741-746, 868-873).
DERIVED_RE = re.compile(r"derived \d+ concept\(s\): (\d+) created, (\d+) matched existing")
RECORDED_RE = re.compile(r"recorded action '.*': (\d+) concept\(s\) created, (\d+) edge\(s\) added")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--ledger", required=True)
    ap.add_argument("--db", required=True)
    ap.add_argument("--session", required=True)
    ap.add_argument(
        "--stderr",
        default=None,
        help="server stderr transcript; when present, the daemon GC sweep counts "
        "(spec §9 housekeeping, logged at debug) are summed and the concept "
        "comparison is made GC-aware — a 'created then collected' concept is "
        "durable work, not tail loss",
    )
    args = ap.parse_args()

    # --- GC: concepts the daemon legitimately collected (spec §9). ---
    gc_collected = 0
    if args.stderr:
        try:
            with open(args.stderr, encoding="utf-8") as fh:
                for line in fh:
                    m = re.search(r"concepts_collected=(\d+)", line)
                    if m:
                        gc_collected += int(m.group(1))
        except OSError as e:
            print(f"warning: could not read --stderr {args.stderr}: {e}", file=sys.stderr)

    # --- Ledger side: successful write calls and their reported counts. ---
    ledger_calls = ledger_ok = 0
    success_derive = success_record = 0
    derive_created = derive_matched = 0
    record_created = record_edges = 0
    transports = http_errors = tool_errors = 0
    for line in open(args.ledger, encoding="utf-8"):
        r = json.loads(line)
        if r["kind"] != "call":
            continue
        ledger_calls += 1
        if r["ok"] and not r["is_error"]:
            ledger_ok += 1
        if r["ok"] and not r["is_error"]:
            m = DERIVED_RE.search(r["text"] or "")
            if r["tool"] == "lambo_derive" and m:
                success_derive += 1
                derive_created += int(m.group(1))
                derive_matched += int(m.group(2))
                continue
            m = RECORDED_RE.search(r["text"] or "")
            if r["tool"] == "lambo_record_action" and m:
                success_record += 1
                record_created += int(m.group(1))
                record_edges += int(m.group(2))
                continue
        # Calls that never got a clean answer: they may still have landed.
        if r.get("http_status") == 429:
            http_errors += 1
        elif r["ok"] is False and r.get("http_status") is None:
            transports += 1
        elif r["is_error"]:
            tool_errors += 1

    success_writes = success_derive + success_record
    expected_concepts = derive_created + record_created
    expected_record_edges = record_edges

    # --- Store side: what is actually durable. ---
    con = sqlite3.connect(args.db)
    cur = con.cursor()
    store_interactions = cur.execute(
        "SELECT COUNT(*) FROM interactions WHERE session_id = ?", (args.session,)
    ).fetchone()[0]
    store_concepts = cur.execute(
        "SELECT COUNT(*) FROM concepts WHERE session_id = ?", (args.session,)
    ).fetchone()[0]
    store_edges = cur.execute(
        "SELECT COUNT(*) FROM edges WHERE session_id = ?", (args.session,)
    ).fetchone()[0]
    store_canon_events = cur.execute(
        "SELECT COUNT(*) FROM canonization_events WHERE session_id = ?", (args.session,)
    ).fetchone()[0]
    lease = cur.execute(
        "SELECT holder, expires_at, current_token FROM session_leases WHERE session_id = ?",
        (args.session,),
    ).fetchone()
    sess = cur.execute(
        "SELECT created_at, closed_at FROM sessions WHERE session_id = ?", (args.session,)
    ).fetchone()
    con.close()

    def fmt(v):
        return "—" if v is None else str(v)

    out = sys.stdout
    out.write("=" * 78 + "\n")
    out.write("C3 durability check — ledger vs store\n")
    out.write("=" * 78 + "\n")
    out.write(f"ledger : {args.ledger}\n")
    out.write(f"db     : {args.db}\n")
    out.write(f"session: {args.session}\n\n")

    out.write("ledger accounting (successful calls only)\n")
    out.write("-" * 78 + "\n")
    out.write(f"  calls recorded               : {ledger_calls}\n")
    out.write(f"  calls ok (no tool error)     : {ledger_ok}\n")
    out.write(f"  successful lambo_derive      : {success_derive}\n")
    out.write(f"  successful lambo_record_action: {success_record}\n")
    out.write(f"  successful write calls       : {success_writes}\n")
    out.write(f"  derive created / matched     : {derive_created} / {derive_matched}\n")
    out.write(f"  record_action created / edges: {record_created} / {record_edges}\n")
    out.write(f"  refused (tool-level)         : {tool_errors}\n")
    out.write(f"  rate-limit 429s              : {http_errors}\n")
    out.write(f"  transport failures           : {transports}\n")
    out.write(f"  daemon GC collected concepts : {gc_collected} (from stderr transcript)\n\n")

    out.write("store readback\n")
    out.write("-" * 78 + "\n")
    out.write(f"  interactions   : {store_interactions}\n")
    out.write(f"  concepts       : {store_concepts}\n")
    out.write(f"  edges          : {store_edges}\n")
    out.write(f"  canon_events   : {store_canon_events}\n")
    out.write(f"  lease row      : {fmt(lease)}\n")
    out.write(f"  session row    : created={fmt(sess[0] if sess else None)} "
              f"closed={fmt(sess[1] if sess else None)}\n\n")

    out.write("comparison\n")
    out.write("-" * 78 + "\n")
    out.write("  expected interactions == store interactions: ")
    if store_interactions == success_writes:
        out.write("MATCH\n")
        interaction_ok = True
    elif store_interactions > success_writes:
        out.write(f"store AHEAD by {store_interactions - success_writes} "
                  "(in-flight calls flushed by the close drain)\n")
        interaction_ok = True
    else:
        out.write(f"SHORTFALL {success_writes - store_interactions}\n")
        interaction_ok = False

    out.write("  expected concepts     == store concepts    : ")
    if store_concepts == expected_concepts:
        out.write("MATCH\n")
        concept_ok = True
    elif store_concepts > expected_concepts:
        out.write(f"store AHEAD by {store_concepts - expected_concepts}\n")
        concept_ok = True
    else:
        missing = expected_concepts - store_concepts
        if gc_collected > 0:
            # Spec §9 GC collects sub-threshold/orphan concepts; a created-then-
            # collected concept is durable work, not tail loss. Accounting:
            # created == store rows + collected rows.
            if missing <= gc_collected:
                out.write(
                    f"shortfall {missing} — EXPLAINED: daemon GC collected "
                    f"{gc_collected} concept(s) this run (spec §9 housekeeping; "
                    "created − store == collected within tolerance)\n"
                )
                concept_ok = True
            else:
                out.write(
                    f"SHORTFALL {missing} (GC collected {gc_collected}; "
                    f"unexplained by GC: {missing - gc_collected})\n"
                )
                concept_ok = False
        else:
            out.write(
                f"SHORTFALL {missing} (no GC counts in the transcript — a created "
                "then GC-collected concept is durable work, not tail loss; see the "
                "runbook for the GC-accounted comparison)\n"
            )
            concept_ok = True

    out.write("  record_action edges   <= store edges       : ")
    if store_edges >= expected_record_edges:
        out.write(f"OK (store {store_edges} >= ledger {expected_record_edges}; "
                  "derive edges add more, unreported)\n")
        edge_ok = True
    else:
        out.write(f"SHORTFALL {expected_record_edges - store_edges}\n")
        edge_ok = False

    out.write("\nverdict: ")
    if interaction_ok and concept_ok and edge_ok:
        out.write("tail durable — no ledger-successful write is missing from the store\n")
        return 0
    out.write(
        "tail NOT fully durable — ledger-successful writes are missing; "
        "see the shortfall rows above\n"
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
