#!/usr/bin/env python3
"""Synthesize `sample/calls.jsonl` — the committed fixture the five scripts run on.

**Entirely fabricated. No dogfood data, no Endor content, no real agent.** The
ledger's hygiene rule (I1) keeps real ledgers outside the repo and admits them to
`evidence/` only through the curated export path, so the committed sample cannot
be a real one. What it can be is *shaped* like one, deterministically, so every
script's happy path is verifiable in CI-less isolation and a change that breaks
a report shows up as a diff rather than a surprise on a live file.

Deliberately planted, one per script, so no report is exercised on an empty set:

  * two agents, several work sessions, one of them non-compliant (writes with no
    prior recall)                                    -> recall_first.py
  * a serve restart mid-file, visible as the heartbeat's uptime resetting AND as
    a git_sha change                                 -> recall_first.py, header
  * derives across two days, converging (day 2 matches more)  -> dedup_rate.py
  * recall hits carrying all three legs, including one where the recency floor
    masks a real cosine                              -> score_bands.py
  * a Canonical hit with a blast radius, warned over twice, one of those cut by
    the token budget                                 -> warnings.py
  * a non-zero dropped-line count on the last heartbeat, so the "counts are a
    lower bound" path in the header is exercised
  * one failed call with an error_kind, and one torn final line

`duplicates.py` reads the store, not the ledger, so `--store <path>` synthesizes
a matching SQLite fixture instead: four fabricated concepts with hand-built unit
vectors placed at known cosines, one pair above the 0.85 merge threshold (a
should-have-merged defect) and one inside G1's paraphrase band below it. Written
on demand rather than committed — a generated binary in the tree is a thing
nobody can review.

Usage:
    python3 scripts/observability/make_sample.py > scripts/observability/sample/calls.jsonl
    python3 scripts/observability/make_sample.py --store /tmp/sample.db
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import sqlite3
import sys

V = 1
BASE = dt.datetime(2026, 8, 18, 9, 0, 0, tzinfo=dt.timezone.utc)
SHA_BEFORE = "aaaa111"
SHA_AFTER = "bbbb222"

#: A fabricated concept id per fabricated concept, stable across regenerations.
IDS = {
    "pagination": "11111111-1111-4111-8111-111111111111",
    "auth": "22222222-2222-4222-8222-222222222222",
    "compat": "33333333-3333-4333-8333-333333333333",
    "retry": "44444444-4444-4444-8444-444444444444",
}


def ts(minutes: float) -> str:
    return (BASE + dt.timedelta(minutes=minutes)).isoformat()


def call(minute: float, tool: str, agent: str, /, outcome: str = "ok", **facts) -> dict:
    line = {
        "v": V,
        "ts": ts(minute),
        "kind": "call",
        "tool": tool,
        "agent_id": agent,
        "outcome": outcome,
        "duration_us": 1200 + int(minute) % 900,
    }
    if outcome != "ok":
        line["error_kind"] = facts.pop("error_kind", "unclassified")
    line.update(facts)
    return line


def hit(
    key: str,
    content: str,
    score: float,
    *,
    bm25: float | None = None,
    recent: float | None = None,
    vector: float | None = None,
    canonical: bool = False,
    blast_radius: int | None = None,
    included: bool = True,
    annotations: list[str] | None = None,
) -> dict:
    legs = {}
    if bm25 is not None:
        legs["bm25"] = bm25
    if recent is not None:
        legs["recent"] = recent
    if vector is not None:
        legs["vector_cosine"] = vector
    return {
        "node_id": IDS[key],
        "content": content,
        "score": score,
        "legs": legs,
        "is_canonical": canonical,
        "blast_radius": blast_radius,
        "included_in_context": included,
        "annotations": annotations or [],
    }


def recall(minute: float, agent: str, query: str, hits: list[dict]) -> dict:
    kinds = [k for h in hits for k in h["annotations"]]
    return call(
        minute,
        "lambo_recall",
        agent,
        query=query,
        top_k=8,
        hit_count=len(hits),
        hits=hits,
        canonical_marker=any(h["is_canonical"] for h in hits),
        blast_radius_warning="load_bearing" in kinds,
        conflict_line="conflict" in kinds,
        hot_warning="hot" in kinds,
        reservation_warning="reservation" in kinds,
        response_annotations=[],
        warning_count=len(kinds),
    )


def derive(minute: float, agent: str, created: int, matched: int, merged: int = 0) -> dict:
    return call(
        minute,
        "lambo_derive",
        agent,
        created=created,
        matched=matched,
        semantic_merged=merged,
        reinforced=0,
        concepts_requested=created + matched,
    )


def heartbeat(minute: float, uptime: int, sha: str, *, nodes: int, dropped: int = 0) -> dict:
    return {
        "v": V,
        "ts": ts(minute),
        "kind": "stats",
        "uptime_secs": uptime,
        "version": "0.2.2",
        "git_sha": sha,
        "stats": {
            "session": "sample-dogfood",
            "agent": "lambo-serve",
            "flush_lag_ms": 400,
            "log_depth": 0,
            "flush_depth": 0,
            "dead_lettered": 0,
            "degraded": False,
            "node_count": nodes,
            "edge_count": nodes * 2,
            "concept_count": nodes - 4,
            "canonical_count": 1,
            "epoch": nodes * 3,
            "daemon_cycles": uptime // 30,
            "canonization_cycles": uptime // 300,
            "canonization_failures": 0,
            "ledger_path": "/home/dogfood/lambo-dogfood/calls.jsonl",
            "ledger_written_lines": 10 + int(minute),
            "ledger_dropped_lines": dropped,
        },
    }


PAGINATION = "pagination contract: cursor-based, opaque cursors, no offsets"
AUTH = "auth middleware validates bearer tokens before any handler runs"
COMPAT = "deployment must stay backward compatible for one minor version"
RETRY = "retry HTTP requests with jitter, never a bare fixed backoff"


def lines() -> list[dict]:
    out: list[dict] = []

    # ---- session A, agent-alpha: compliant. Recall, then a write run. --------
    out.append(heartbeat(0, uptime=30, sha=SHA_BEFORE, nodes=12))
    out.append(
        recall(
            1,
            "agent-alpha",
            "how do we paginate list endpoints",
            [
                # All three legs, vector winning. A canonical hit with dependents:
                # the blast-radius warning fires and the model sees the block.
                hit(
                    "pagination",
                    PAGINATION,
                    0.6412,
                    bm25=0.5810,
                    recent=0.35,
                    vector=0.6412,
                    canonical=True,
                    blast_radius=7,
                    annotations=["load_bearing"],
                ),
                hit("retry", RETRY, 0.35, recent=0.35),
            ],
        )
    )
    out.append(derive(3, "agent-alpha", created=2, matched=0))
    out.append(derive(4, "agent-alpha", created=1, matched=1))
    out.append(
        call(
            6,
            "lambo_record_action",
            "agent-alpha",
            created=1,
            edges=3,
        )
    )
    out.append(call(7, "lambo_reserve", "agent-alpha", op="reserve", granted=True, ttl_seconds=30))
    out.append(call(9, "lambo_reserve", "agent-alpha", op="release", granted=True))

    # ---- session A, agent-beta: NOT compliant. Writes with no prior recall. --
    out.append(derive(12, "agent-beta", created=3, matched=0))
    out.append(call(13, "lambo_record_action", "agent-beta", created=2, edges=4))
    # ...then it recalls, and complies for the rest of the session.
    out.append(
        recall(
            18,
            "agent-beta",
            "what breaks if I change the token check",
            [
                hit(
                    "auth",
                    AUTH,
                    0.6142,
                    bm25=0.4402,
                    vector=0.6142,
                ),
                # THE PLANTED FLOOR MASK: a real cosine below the 0.35 recency
                # floor, so `max` kept the flat score and threw the semantic
                # signal away. score_bands.py must catch this.
                hit("compat", COMPAT, 0.35, recent=0.35, vector=0.2914),
            ],
        )
    )
    out.append(derive(20, "agent-beta", created=1, matched=2))
    out.append(call(22, "lambo_inspect", "agent-beta", depth=2, fuzzy=True))
    out.append(call(23, "lambo_saints", "agent-beta", canonical_count=1))
    out.append(heartbeat(25, uptime=1_530, sha=SHA_BEFORE, nodes=24))

    # ---- a failed call, classified. -----------------------------------------
    out.append(
        call(
            27,
            "lambo_reserve",
            "agent-gamma",
            outcome="error",
            error_kind="refused: foreign agent",
            op="reserve",
            granted=False,
        )
    )
    out.append(
        call(
            28,
            "lambo_derive",
            "agent-alpha",
            outcome="error",
            error_kind="invalid params",
        )
    )

    # ---- THE UPGRADE. New process (uptime resets) on a new sha. -------------
    out.append(heartbeat(120, uptime=30, sha=SHA_AFTER, nodes=24))

    # ---- session B (day 2, after the restart): converging vocabulary. -------
    day2 = 24 * 60 + 30
    out.append(heartbeat(day2 - 5, uptime=1_500, sha=SHA_AFTER, nodes=31))
    out.append(
        recall(
            day2,
            "agent-alpha",
            "pagination cursors",
            [
                # Warned over a second time; this time the hit fell past the
                # token budget, so lambo counted the warning and the model never
                # saw the block. warnings.py must distinguish the two.
                hit(
                    "pagination",
                    PAGINATION,
                    0.6698,
                    bm25=0.9120,
                    vector=0.6698,
                    canonical=True,
                    blast_radius=9,
                    included=False,
                    annotations=["load_bearing", "hot"],
                ),
                # An expansion-only hit: no phase-1 legs at all.
                hit("retry", RETRY, 0.4100),
            ],
        )
    )
    out.append(derive(day2 + 2, "agent-alpha", created=0, matched=3))
    out.append(derive(day2 + 4, "agent-alpha", created=1, matched=4, merged=1))
    out.append(derive(day2 + 6, "agent-beta", created=0, matched=2))
    out.append(
        recall(
            day2 + 9,
            "agent-beta",
            "backward compatible rollout",
            [hit("compat", COMPAT, 0.5276, bm25=0.3300, vector=0.5276)],
        )
    )
    out.append(derive(day2 + 11, "agent-beta", created=0, matched=2))

    # Last heartbeat reports DROPPED LINES, so every report's header must say
    # its counts are a lower bound.
    out.append(heartbeat(day2 + 15, uptime=2_400, sha=SHA_AFTER, nodes=33, dropped=4))
    return out


# ---------------------------------------------------------------------------
# The store fixture for duplicates.py
# ---------------------------------------------------------------------------

#: Only the columns `duplicates.py` reads. A real store's schema is
#: `migrations/sqlite/001_init.sql`; reproducing all of it here would couple this
#: fixture to migrations it does not exercise, and the script's own SQL is the
#: contract being tested.
STORE_DDL = """
CREATE TABLE concepts (
    id                  TEXT PRIMARY KEY,
    session_id          TEXT NOT NULL,
    content             TEXT NOT NULL,
    embedding           TEXT,
    canonization_status TEXT NOT NULL
);
"""


def unit_at_angle(degrees: float, dim: int = 16) -> list[float]:
    """A unit vector at a known angle from `e0`, so cosines are exact.

    Two components carry the angle and the rest are zero: cos(theta) between
    `unit_at_angle(0)` and `unit_at_angle(t)` is exactly cos(t). That makes the
    planted pairs' cosines a property of arithmetic rather than of a model.
    """
    theta = math.radians(degrees)
    vec = [0.0] * dim
    vec[0] = math.cos(theta)
    vec[1] = math.sin(theta)
    return vec


#: (content, angle, status). Angles chosen so:
#:   register/create  -> cos(20°) = 0.9397  ABOVE the 0.85 threshold (a defect)
#:   charge/process   -> cos(0°-45°)... see below; 0.7071  IN G1's paraphrase band
STORE_CONCEPTS = [
    ("register user account", 0.0, "None"),
    ("create user account", 20.0, "None"),
    ("charge the customer card", 90.0, "Canonical"),
    ("process the customer payment", 135.0, "None"),
]


def write_store(path: str) -> None:
    con = sqlite3.connect(path)
    try:
        con.executescript(STORE_DDL)
        for i, (content, angle, status) in enumerate(STORE_CONCEPTS):
            vec = unit_at_angle(angle)
            con.execute(
                "INSERT INTO concepts (id, session_id, content, embedding, "
                "canonization_status) VALUES (?, ?, ?, ?, ?)",
                (
                    f"{i:08d}-0000-4000-8000-000000000000",
                    "sample-dogfood",
                    content,
                    "[" + ",".join(f"{x:.10f}" for x in vec) + "]",
                    status,
                ),
            )
        # One concept with no embedding at all: the scan cannot see it, and the
        # report has to say so rather than imply it was proven distinct.
        con.execute(
            "INSERT INTO concepts (id, session_id, content, embedding, "
            "canonization_status) VALUES (?, ?, ?, NULL, ?)",
            (
                "99999999-0000-4000-8000-000000000000",
                "sample-dogfood",
                "written before the embedder came up",
                "None",
            ),
        )
        con.commit()
    finally:
        con.close()


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__ or "")
    parser.add_argument(
        "--store",
        help="write the duplicates.py SQLite fixture here instead of the ledger",
    )
    args = parser.parse_args(argv)

    if args.store:
        write_store(args.store)
        print(f"wrote {args.store}", file=sys.stderr)
        return 0

    for line in lines():
        sys.stdout.write(json.dumps(line, separators=(",", ":")) + "\n")
    # A torn final line: the process was killed mid-write. Every script must
    # report it and carry on, because a real ledger's tail can look like this.
    sys.stdout.write('{"v":1,"ts":"2026-08-19T09:46:00+00:00","kind":"call","tool":"lambo_de')
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
