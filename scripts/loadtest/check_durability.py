#!/usr/bin/env python3
"""C3 — durability check: compare the load driver's ledger against the SQLite store.

The C2 assertion (`lambo serve: session closed, tail durable`) is a log line;
this script is the other half the review never did — after the process exits,
reconnect to the store and count what should have survived.

Since J3, `lambo_derive` and `lambo_record_action` are acknowledged *before*
the write is applied. The ack carries a receipt id; the write's outcome
arrives later, piggybacked on the same agent's next tool response
(`write receipts (your earlier writes, now settled):` followed by
`- <receipt>: <answer>` lines) or as the answer to `lambo_stats` with that
receipt (`receipt <receipt>: <answer>`). The accounting therefore joins acks
to settled receipts. Wording matched (see `src/mcp/server.rs` `derive_impl`,
`record_action_impl`, `attach_receipts`, `stats_impl`, and `src/writeq.rs`
`ReceiptAnswer::describe`, `derive_sentence`, `action_sentence`):

* derive ack       — `accepted N concept(s) for background write; ...`
* action ack       — `accepted action '<action>' for background write; ...`
* refused at ack   — `N concept(s) were NOT written: ...` /
                     `action '<action>' was NOT recorded: ...`
* receipt line     — `receipt <id>: <answer>` (ack and `lambo_stats`) and
                     `- <id>: <answer>` (settled-receipt piggyback)
* applied answers  — `applied — derived N concept(s): C created[ (E embedded)], M matched existing`
                     `applied — recorded action: C concept(s) created, E edge(s)`
                     and the same sentences after `applied after a restart — `

Accounting (successful tool calls only, per concurrency-capture.md C3):

* interactions — one per acknowledged write call, whether the ack admitted
  the write or refused it at the queue: the interaction that pins the write's
  place in the agent's chain is opened inside the acking call, before
  admission (`Memory::derive_async_as`, `record_action_async_as`). This stays
  a clean 1:1 ledger-vs-store comparison and the durable-tail yardstick.
* concepts / edges — known only for receipts the ledger saw settle as
  applied. Sum `created` over those and compare with the store's concept rows;
  sum record_action edges and compare (lower bound) with the store's edge
  rows. **A created-then-GC-collected concept is durable work, not tail
  loss**: the daemon's spec §9 GC collects sub-threshold/orphan concepts (and
  their edges). When `--stderr` carries the GC debug lines
  (`concepts_collected=N`, `edges_removed=N`), the comparison is GC-aware.
  Pass `--gc-logged` as well when the transcript is known to have been
  captured with `lambo::daemon::gc` at debug (capture_sigterm.sh's default):
  zero sweep lines then means no GC ran, so a concept shortfall is loss, not
  an unverifiable gap.
* receipts the ledger never saw settle (in flight at SIGTERM, recorded as a
  durable intent, or simply never piggybacked because the agent made no
  further call) are reported as their own row. Their concepts are not
  counted either way, so the concept comparison is exact for settled-applied
  writes only and the store may legitimately be AHEAD of it.

Store rows may EXCEED the ledger: a call in flight when SIGTERM landed can
have its mutations flushed by the close drain without ever returning a
response. That surplus is reported as in-flight-landed, not a discrepancy.

Exit status (every code but 0 means "do not claim durability"):
  0 — the store is not short of ledger-acknowledged interactions, nor of
      settled-applied concepts/edges (GC-accounted), and at least one
      acknowledged write was checked;
  2 — it is short (the honest "tail was NOT durable" signal), including a
      concept shortfall that GC does not explain when `--gc-logged` says
      every GC sweep is in the transcript;
  3 — the ledger has successful write calls whose response text this script
      does not recognise. The server's wording drifted; fix the parser rather
      than trust a comparison built on zero counted writes;
  4 — NOTHING TO VERIFY: the ledger holds no acknowledged write (driver
      crashed early, wrong --ledger, every call refused). A check that checked
      nothing must not pass; pass `--allow-empty` only when an empty run is the
      expected outcome (then it exits 0 and says so);
  5 — UNVERIFIABLE concepts: interactions and edges are fine but the store is
      short of settled-applied concepts and GC could legitimately explain it,
      while the transcript does not show that GC activity was logged. Re-run
      with `--stderr` captured at `lambo::daemon::gc=debug` plus `--gc-logged`;
  6 — an input could not be read (ledger or store missing/unreadable, not a
      lambo store, a ledger line that is not JSON or not a JSON object). The store is opened read-only and
      is never created or modified;
  64 — command-line usage error.

Limitation (concepts compare by count, not by id): a settled receipt reports
only how many concepts it created (`C created`), never their ids (see
`derive_sentence` / `action_sentence` in `src/writeq.rs`), and the ledger
carries no concept ids either, so "every settled-applied concept id is in the
store" cannot be checked. The concept check is a lower bound on the store
total: concepts from unsettled writes that did land can offset settled ones
that were lost. When the store is AHEAD while unsettled receipts exist, the
output says so and the verdict is qualified; it still exits 0, because the
surplus is also the normal in-flight-landed case. Interactions are 1:1 per
acknowledged write and are not affected.

    python3 scripts/loadtest/check_durability.py \\
        --ledger evidence/concurrency/ledger-<run>.jsonl \\
        --db     evidence/concurrency/c-load-<date>.db \\
        --session c-load-<date> \\
        --stderr evidence/concurrency/stderr-<run>.log
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import sqlite3
import sys

WRITE_TOOLS = ("lambo_derive", "lambo_record_action")

# Ack forms (src/mcp/server.rs, derive_impl / record_action_impl). Anchored at
# the start of the response text: the ack is the first content block.
DERIVE_ACK_RE = re.compile(r"accepted (\d+) concept\(s\) for background write\b")
ACTION_ACK_RE = re.compile(r"accepted action '.*?' for background write\b", re.S)
DERIVE_REFUSED_RE = re.compile(r"(\d+) concept\(s\) were NOT written: ")
ACTION_REFUSED_RE = re.compile(r"action '.*?' was NOT recorded: ", re.S)

# Receipt ids (src/writeq.rs, `impl Display for ReceiptId`):
# `lwr1.<epoch:016x>.<issued_ms:x>.<seq:x>`.
RECEIPT_ID = r"lwr1\.[0-9a-f]{16}\.[0-9a-f]+\.[0-9a-f]+"
# The ack's own receipt line.
ACK_RECEIPT_RE = re.compile(rf"^receipt ({RECEIPT_ID}): ", re.M)
# Every receipt answer the ledger can carry: the ack's line and lambo_stats'
# (`receipt <id>: ...`), and the settled-receipt piggyback (`- <id>: ...`).
RECEIPT_LINE_RE = re.compile(rf"^(?:receipt |- )({RECEIPT_ID}): (.*)$", re.M)

# Applied sentences (src/writeq.rs, derive_sentence / action_sentence).
DERIVE_SENTENCE_RE = re.compile(
    r"^derived (\d+) concept\(s\): (\d+) created(?: \((\d+) embedded\))?, (\d+) matched existing"
)
ACTION_SENTENCE_RE = re.compile(r"^recorded action: (\d+) concept\(s\) created, (\d+) edge\(s\)")

# ReceiptAnswer::describe prefixes -> ReceiptAnswer::tag. Order matters:
# "applied after a restart" before "applied".
ANSWER_PREFIXES = (
    ("applied after a restart — ", "applied_after_restart"),
    ("applied — ", "applied"),
    ("pending", "pending"),
    ("FAILED, nothing was written", "failed"),
    ("not applied before this session closed", "intent_durable"),
    ("DROPPED before it was attempted", "dropped"),
    ("expired", "expired"),
    ("restart-lost", "restart_lost"),
    ("never issued", "never_issued"),
    ("held by another agent", "forbidden"),
)
UNSETTLED = ("pending", "unknown")


def classify(answer: str) -> tuple[str, str]:
    """Return (tag, remainder after the prefix) for one receipt answer."""
    for prefix, tag in ANSWER_PREFIXES:
        if answer.startswith(prefix):
            return tag, answer[len(prefix):]
    return "unknown", answer


EXIT_OK = 0
EXIT_SHORTFALL = 2
EXIT_PARSER_DRIFT = 3
EXIT_NOTHING_TO_CHECK = 4
EXIT_UNVERIFIABLE = 5
EXIT_BAD_INPUT = 6
EXIT_USAGE = 64


class _Parser(argparse.ArgumentParser):
    """argparse exits 2 on a usage error, which would read as SHORTFALL."""

    def error(self, message: str):
        self.print_usage(sys.stderr)
        self.exit(EXIT_USAGE, f"{self.prog}: error: {message}\n")


def main() -> int:
    ap = _Parser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ledger", required=True)
    ap.add_argument("--db", required=True)
    ap.add_argument("--session", required=True)
    ap.add_argument(
        "--stderr",
        default=None,
        help="server stderr transcript; when present, the daemon GC sweep counts "
        "(spec §9 housekeeping, logged at debug) are summed and the concept and "
        "edge comparisons are made GC-aware — a 'created then collected' concept "
        "is durable work, not tail loss",
    )
    ap.add_argument(
        "--gc-logged",
        action="store_true",
        help="the --stderr transcript was captured with lambo::daemon::gc at debug, "
        "so every GC sweep is in it; a concept shortfall GC does not explain then "
        "fails the check instead of being reported as unverified",
    )
    ap.add_argument(
        "--allow-empty",
        action="store_true",
        help="an empty run (no acknowledged write in the ledger) is the expected "
        "outcome: exit 0 instead of 4",
    )
    args = ap.parse_args()
    if args.gc_logged and not args.stderr:
        ap.error("--gc-logged needs --stderr")

    # --- GC: concepts/edges the daemon legitimately collected (spec §9). ---
    gc_collected = gc_edges_removed = gc_sweeps = 0
    if args.stderr:
        try:
            with open(args.stderr, encoding="utf-8") as fh:
                for line in fh:
                    m = re.search(r"concepts_collected=(\d+)", line)
                    if m:
                        gc_sweeps += 1
                        gc_collected += int(m.group(1))
                        e = re.search(r"edges_removed=(\d+)", line)
                        if e:
                            gc_edges_removed += int(e.group(1))
        except OSError as e:
            print(f"warning: could not read --stderr {args.stderr}: {e}", file=sys.stderr)

    # --- Ledger side: acked writes, and every receipt answer seen. ---
    ledger_calls = ledger_ok = 0
    acked_derive = acked_record = 0
    refused_at_ack = 0
    unrecognised: list[str] = []
    transports = http_errors = tool_errors = 0
    # receipt id -> tool, for receipts issued by an admitting ack
    issued: dict[str, str] = {}
    # receipt id -> (tag, remainder); a settled answer is never overwritten by
    # an unsettled one (the ack's own `pending` line can follow a settled one
    # in ledger order when workers interleave).
    answers: dict[str, tuple[str, str]] = {}

    try:
        with open(args.ledger, encoding="utf-8") as fh:
            ledger_lines = fh.readlines()
    except OSError as e:
        print(f"error: cannot read --ledger {args.ledger}: {e}", file=sys.stderr)
        return EXIT_BAD_INPUT

    for lineno, line in enumerate(ledger_lines, 1):
        if not line.strip():
            continue
        try:
            r = json.loads(line)
        except ValueError as e:
            print(f"error: --ledger {args.ledger} line {lineno} is not JSON: {e}", file=sys.stderr)
            return EXIT_BAD_INPUT
        if not isinstance(r, dict):
            # Valid JSON that is not an object (null, [], 7, "x"): the same
            # class of damage as a non-JSON line, so the same bad-input code.
            print(f"error: --ledger {args.ledger} line {lineno} is JSON but not an "
                  f"object ({type(r).__name__}), not a ledger record", file=sys.stderr)
            return EXIT_BAD_INPUT
        if r.get("kind") != "call":
            continue
        ledger_calls += 1
        text = r.get("text") or ""

        for m in RECEIPT_LINE_RE.finditer(text):
            rid, answer = m.group(1), m.group(2)
            tag, rest = classify(answer.strip())
            if (
                tag in ("applied", "applied_after_restart")
                and m.end() == len(text)
                and not (DERIVE_SENTENCE_RE.match(rest) or ACTION_SENTENCE_RE.match(rest))
            ):
                # The drivers truncate response text (mcp_load.py at 4000
                # chars, mcp_agentic.py at 500), so the last line can be cut
                # mid-sentence. That is a lost answer, not a format change.
                tag = "unknown"
            prev = answers.get(rid)
            if prev is None or prev[0] in UNSETTLED or tag not in UNSETTLED:
                answers[rid] = (tag, rest)

        ok = bool(r.get("ok")) and not r.get("is_error")
        if ok:
            ledger_ok += 1
        if ok and r.get("tool") in WRITE_TOOLS:
            tool = r["tool"]
            if tool == "lambo_derive":
                admitted = DERIVE_ACK_RE.match(text)
                refused = DERIVE_REFUSED_RE.match(text)
            else:
                admitted = ACTION_ACK_RE.match(text)
                refused = ACTION_REFUSED_RE.match(text)
            if admitted:
                if tool == "lambo_derive":
                    acked_derive += 1
                else:
                    acked_record += 1
                m = ACK_RECEIPT_RE.search(text)
                if m:
                    issued[m.group(1)] = tool
                else:
                    unrecognised.append(f"{tool} ack without a receipt line: {text[:160]!r}")
            elif refused:
                refused_at_ack += 1
            else:
                unrecognised.append(f"{tool}: {text[:160]!r}")
            continue
        if ok:
            continue
        # Calls that never got a clean answer: they may still have landed.
        if r.get("http_status") == 429:
            http_errors += 1
        elif r.get("ok") is False and r.get("http_status") is None:
            transports += 1
        elif r.get("is_error"):
            tool_errors += 1

    # --- Join issued receipts to their outcomes. ---
    outcome_counts: dict[str, int] = {}
    derive_created = derive_matched = 0
    record_created = record_edges = 0
    for rid, tool in issued.items():
        tag, rest = answers.get(rid, ("pending", ""))
        outcome_counts[tag] = outcome_counts.get(tag, 0) + 1
        if tag not in ("applied", "applied_after_restart"):
            continue
        # `applied after a restart — <sentence> (confirmed from ...)`
        if tool == "lambo_derive":
            m = DERIVE_SENTENCE_RE.match(rest)
            if not m:
                unrecognised.append(f"applied derive receipt {rid}: {rest[:160]!r}")
                continue
            derive_created += int(m.group(2))
            derive_matched += int(m.group(4))
        else:
            m = ACTION_SENTENCE_RE.match(rest)
            if not m:
                unrecognised.append(f"applied action receipt {rid}: {rest[:160]!r}")
                continue
            record_created += int(m.group(1))
            record_edges += int(m.group(2))

    acked_writes = acked_derive + acked_record
    expected_interactions = acked_writes + refused_at_ack
    settled_applied = outcome_counts.get("applied", 0) + outcome_counts.get("applied_after_restart", 0)
    unsettled = sum(outcome_counts.get(t, 0) for t in UNSETTLED) + outcome_counts.get(
        "intent_durable", 0
    )
    expected_concepts = derive_created + record_created
    expected_record_edges = record_edges

    # --- Store side: what is actually durable. ---
    if not os.path.isfile(args.db):
        print(f"error: --db {args.db} does not exist (the store is never created "
              "by this check)", file=sys.stderr)
        return EXIT_BAD_INPUT
    try:
        # Read-only URI: never create the file, never write to a store.
        con = sqlite3.connect(f"{pathlib.Path(args.db).resolve().as_uri()}?mode=ro", uri=True)
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
    except sqlite3.Error as e:
        print(f"error: cannot read --db {args.db} as a lambo SQLite store: {e}", file=sys.stderr)
        return EXIT_BAD_INPUT

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
    out.write(f"  calls recorded                : {ledger_calls}\n")
    out.write(f"  calls ok (no tool error)      : {ledger_ok}\n")
    out.write(f"  acked lambo_derive            : {acked_derive}\n")
    out.write(f"  acked lambo_record_action     : {acked_record}\n")
    out.write(f"  refused at ack (queue full)   : {refused_at_ack}\n")
    out.write(f"  expected interactions         : {expected_interactions}\n")
    out.write(f"  unrecognised write responses  : {len(unrecognised)}\n")
    out.write(f"  refused (tool-level)          : {tool_errors}\n")
    out.write(f"  rate-limit 429s               : {http_errors}\n")
    out.write(f"  transport failures            : {transports}\n\n")

    out.write("receipt outcomes (admitted writes, as last seen in the ledger)\n")
    out.write("-" * 78 + "\n")
    for tag in ("applied", "applied_after_restart", "failed", "dropped", "intent_durable",
                "expired", "restart_lost", "never_issued", "forbidden", "pending", "unknown"):
        n = outcome_counts.get(tag, 0)
        if n or tag in ("applied", "failed", "pending"):
            label = "pending (never seen settled)" if tag == "pending" else tag
            out.write(f"  {label:<30}: {n}\n")
    out.write(f"  settled applied               : {settled_applied}\n")
    out.write(f"  UNSETTLED (not counted below) : {unsettled}\n")
    out.write(f"  applied derive created / matched : {derive_created} / {derive_matched}\n")
    out.write(f"  applied record_action created / edges: {record_created} / {record_edges}\n")
    if args.stderr:
        out.write(f"  daemon GC collected concepts  : {gc_collected} "
                  f"(edges removed {gc_edges_removed}, {gc_sweeps} sweep line(s) in stderr)\n")
    else:
        out.write("  daemon GC collected concepts  : unknown (no --stderr transcript)\n")
    out.write("\n")

    out.write("store readback\n")
    out.write("-" * 78 + "\n")
    out.write(f"  interactions   : {store_interactions}\n")
    out.write(f"  concepts       : {store_concepts}\n")
    out.write(f"  edges          : {store_edges}\n")
    out.write(f"  canon_events   : {store_canon_events}\n")
    out.write(f"  lease row      : {fmt(lease)}\n")
    out.write(f"  session row    : created={fmt(sess[0] if sess else None)} "
              f"closed={fmt(sess[1] if sess else None)}\n\n")

    if unrecognised:
        out.write("UNRECOGNISED write responses (first 5)\n")
        out.write("-" * 78 + "\n")
        for u in unrecognised[:5]:
            out.write(f"  {u}\n")
        out.write(
            "\nverdict: ledger format not recognised — the server's ack or receipt "
            "wording no longer matches this script, so no comparison is trustworthy. "
            "Update the parser (see the module docstring for the wording it expects).\n"
        )
        return EXIT_PARSER_DRIFT

    out.write("comparison\n")
    out.write("-" * 78 + "\n")
    out.write("  expected interactions == store interactions: ")
    if store_interactions == expected_interactions:
        out.write("MATCH\n")
        interaction_ok = True
    elif store_interactions > expected_interactions:
        out.write(f"store AHEAD by {store_interactions - expected_interactions} "
                  "(in-flight calls flushed by the close drain)\n")
        interaction_ok = True
    else:
        out.write(f"SHORTFALL {expected_interactions - store_interactions}\n")
        interaction_ok = False

    out.write("  settled-applied concepts <= store concepts : ")
    concepts_verified = True
    concept_unverifiable = False
    lower_bound_only = False
    if store_concepts == expected_concepts:
        out.write("MATCH\n")
        concept_ok = True
    elif store_concepts > expected_concepts:
        out.write(f"store AHEAD by {store_concepts - expected_concepts}"
                  + (f" ({unsettled} unsettled receipt(s) not counted)" if unsettled else "")
                  + "\n")
        if unsettled:
            lower_bound_only = True
        concept_ok = True
    else:
        missing = expected_concepts - store_concepts
        if gc_collected > 0 or args.gc_logged:
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
                f"shortfall {missing} UNVERIFIABLE (no GC counts in the transcript and "
                "--gc-logged not given — GC may or may not explain it; re-run with "
                "--stderr captured at lambo::daemon::gc=debug and --gc-logged)\n"
            )
            concept_ok = True
            concepts_verified = False
            concept_unverifiable = True

    out.write("  settled record_action edges <= store edges : ")
    if store_edges + gc_edges_removed >= expected_record_edges:
        out.write(f"OK (store {store_edges} + GC-removed {gc_edges_removed} >= ledger "
                  f"{expected_record_edges}; derive edges add more, unreported)\n")
        edge_ok = True
    else:
        out.write(f"SHORTFALL {expected_record_edges - store_edges - gc_edges_removed}\n")
        edge_ok = False

    if unsettled:
        out.write(
            f"  note: {unsettled} admitted write(s) never settled in the ledger; their "
            "interactions are counted above, their concepts are not\n"
        )

    out.write("\nverdict: ")
    if not (interaction_ok and edge_ok and concept_ok):
        out.write(
            "tail NOT fully durable — ledger-acknowledged writes are missing; "
            "see the shortfall rows above\n"
        )
        return EXIT_SHORTFALL
    if acked_writes + refused_at_ack == 0:
        if args.allow_empty:
            out.write("nothing to check — the ledger holds no acknowledged write call "
                      "(--allow-empty: expected)\n")
            return EXIT_OK
        out.write("NOTHING VERIFIED — the ledger holds no acknowledged write call, so there "
                  "was nothing to compare; this is not a pass (--allow-empty if an empty "
                  "run is expected)\n")
        return EXIT_NOTHING_TO_CHECK
    if concept_unverifiable:
        out.write("UNVERIFIABLE — interactions and edges are durable, but the concept "
                  "shortfall above could be GC and the transcript does not show GC was "
                  "logged; not a pass\n")
        return EXIT_UNVERIFIABLE
    out.write("tail durable — no ledger-acknowledged write is missing from the store\n")
    if lower_bound_only:
        out.write(
            "  caveat: concepts are compared by count (receipts carry no concept ids) and "
            f"{unsettled} unsettled receipt(s) exist, so the store being AHEAD could hide a "
            "loss of settled concepts; interactions are exact\n"
        )
    return EXIT_OK


if __name__ == "__main__":
    raise SystemExit(main())
