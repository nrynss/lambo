#!/usr/bin/env python3
"""Shared reader for the `lambo serve --ledger` JSONL call ledger (I1/I2).

One module so the five report generators agree on what a "call", a "write", a
"work session" and a "successful call" are. A metric computed two ways is a
metric nobody can quote.

Schema (from `src/ledger.rs`; every line carries `v`, currently 1):

    common      v, ts (RFC3339, server-stamped), kind ("call" | "stats")
    call        tool, agent_id, outcome ("ok"|"error"|"panic"),
                error_kind (only when outcome != "ok"), duration_us
    + recall    query, top_k, hit_count, hits[], canonical_marker,
                blast_radius_warning, conflict_line, hot_warning,
                reservation_warning, response_annotations[], warning_count
      hits[i]   node_id, content (truncated), score, legs{}, is_canonical,
                blast_radius, included_in_context, annotations[]
      legs{}    any of bm25 / recent / vector_cosine -> float. A key is present
                only when that phase-1 leg produced the hit. An EMPTY legs
                object means the hit was not a phase-1 candidate at all: it
                arrived through phase-2 traversal expansion.
    + derive    concepts_requested, admitted, receipt
                (J3: created / matched / semantic_merged / reinforced moved to
                the RECEIPT — the ack precedes the write and cannot know them.
                `receipt` joins the line to lambo_stats(receipt=...).)
    + record_action  admitted, receipt
                (J3: created / edges are on the receipt, same reason.)
    + reserve   op ("reserve"|"release"), granted, ttl_seconds (grants only)
    + inspect   depth, fuzzy
    + saints    canonical_count
    stats       uptime_secs, version, git_sha, stats{...the lambo_stats payload,
                including ledger_written_lines / ledger_dropped_lines /
                ledger_queued_lines}

Forward compatibility: consumers here ignore unknown keys and unknown `kind`s,
so adding a field to a line does not need a `v` bump. A field that changes
meaning or disappears does — and this reader ACTS on `v` (see `KNOWN_VERSIONS`)
rather than merely recording it, because the failure mode of not acting is a
confidently wrong number.

Two quiet dependencies on the SHAPE of `ts`, written down here because both are
invisible at their use site and both break silently:

  1. **String sort is timestamp sort** (`Ledger.sorted_calls`, `restart_times`,
     `queued_lines` — and only those three; `binaries` groups by
     `(version, git_sha)` and sorts by heartbeat count, so it never reads `ts`).
     True only while every stamp is the same fixed-offset RFC3339 form chrono's
     `to_rfc3339()` emits — same field widths, same `+00:00` suffix. A producer
     that switched to `Z`, to local offsets, or to a variable number of
     fractional digits would reorder lines lexically without erroring.
     `queued_lines` is the strongest of the three, and the reason to keep this
     list current: its sibling `dropped_lines` takes a `max()` over VALUES and
     so never cared which stamp sorts last, whereas `queued_lines` is the kit's
     first COMPLETENESS verdict decided by lexical stamp order — a wrong
     "newest" heartbeat has `header()` call a ledger with a known backlog
     complete, silently and without erroring. `load()` accepts several paths,
     so a mixed-offset multi-file read is the realistic trigger.
  2. **Prefix slicing is bucketing** (`BUCKETS` in `dedup_rate.py`: `ts[:13]` is
     the hour, `ts[:10]` the day). True only for the same fixed-width form, and
     only while the stamp is UTC — a local-offset stamp would bucket by local
     wall clock while sorting by neither.

`parse_ts` normalises the one part of the shape that legitimately varies (the
number of fractional-second digits); nothing normalises the offset, so both
dependencies above remain conditions on the producer, not guarantees.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import sys
from dataclasses import dataclass, field
from typing import Any, Iterable, Iterator

#: Line schema versions this kit knows how to read (`LINE_VERSION` in
#: `src/ledger.rs`).
#:
#: `v` is the ledger's ONLY schema promise, and a reader that records it without
#: acting on it converts a schema change into a wrong number rather than a
#: message: a hypothetical `v:2` that renamed `matched` would make every dedup
#: rate read `0.000` — the exact opposite of the truth — with no warning anywhere.
#:
#: The choice here is **warn, do not refuse** (stated in the README's choices
#: list). A mixed file is the realistic case — one ledger spanning an upgrade, the
#: thing `git_sha` exists to make visible — and refusing it would throw away the
#: v1 lines that are still perfectly readable. So unknown-version lines are read
#: as best they can be AND announced in the header, in the same register as
#: dropped lines: capitals, first, unmissable.
KNOWN_VERSIONS = frozenset({1})

#: Tools that mutate the graph. "Did a recall precede the writes?" is a question
#: about exactly these two.
WRITE_TOOLS = ("lambo_derive", "lambo_record_action")

#: The read that recall-first compliance is about.
RECALL_TOOL = "lambo_recall"

#: `RECENT_SCORE` in `src/recall/candidates.rs` — the flat score every
#: recent-interaction candidate gets. Lowered from 0.5 to 0.35 by G2 because
#: G1 measured BGE-M3's true-match band starting at 0.4599. Overridable on the
#: scripts that use it, since a future per-embedder calibration may move it.
RECENT_FLOOR = 0.35

#: `SEMANTIC_MATCH_THRESHOLD_DEFAULT` in `src/graph/hybrid.rs` — the cosine at
#: or above which a hybrid derive MERGES an incoming concept into an existing
#: one instead of creating a duplicate. G1 kept it at 0.85.
MERGE_THRESHOLD = 0.85

#: Gap after which an agent is treated as having started a new work session.
#: The ledger has no session-boundary marker — `serve` owns one lambo session
#: for its whole life, while an *agent's* working stretches come and go — so an
#: idle gap is the honest proxy. A process restart is the other boundary, and
#: that one IS observable (see `Ledger.restart_times`).
DEFAULT_SESSION_GAP_MINUTES = 30


class LedgerError(Exception):
    """A ledger that cannot be read as a ledger."""


#: Fractional-second digits `datetime.fromisoformat` accepts before Python 3.11.
_FRACTION_DIGITS = 6


def parse_ts(raw: str) -> dt.datetime:
    """RFC3339 -> aware datetime. `serve` always stamps UTC with an offset.

    The producer is chrono's `to_rfc3339()`, which uses `SecondsFormat::AutoSi`:
    it emits 0, 3, 6 or **9** fractional digits depending on the clock's
    resolution. `datetime.fromisoformat` accepts only 3 or 6 before Python 3.11,
    and nanosecond stamps are exactly what a Linux box produces — which is half
    the dogfood rig. Truncating to microseconds here (not rounding: a ledger
    timestamp is an ordering key, and truncation cannot reorder two stamps that
    string-sort in a given order) removes the Python floor this kit would
    otherwise have to declare, and keeps 3.10 boxes readable.

    Truncation is the only normalisation performed. The offset is left exactly as
    the producer wrote it — see this module's docstring for why the two callers
    that treat `ts` as a sortable, sliceable string depend on that.
    """
    head, sep, rest = raw.partition(".")
    if not sep:
        return dt.datetime.fromisoformat(raw)
    # `rest` is digits followed by the offset (`+00:00`, `-05:00` or `Z`).
    digits = ""
    for ch in rest:
        if not ch.isdigit():
            break
        digits += ch
    offset = rest[len(digits):]
    return dt.datetime.fromisoformat(f"{head}.{digits[:_FRACTION_DIGITS]}{offset}")


@dataclass
class Ledger:
    """Every line of one (or several concatenated) ledger file(s)."""

    calls: list[dict[str, Any]] = field(default_factory=list)
    heartbeats: list[dict[str, Any]] = field(default_factory=list)
    #: Lines whose `kind` this reader does not know. Reported, never silently
    #: skipped — a consumer that ignores half a file without saying so is how a
    #: measurement quietly becomes wrong.
    unknown: list[dict[str, Any]] = field(default_factory=list)
    #: Lines that did not parse as JSON at all, as (line number, text).
    unparseable: list[tuple[int, str]] = field(default_factory=list)
    #: Line `v` values seen, so a report can say which schema it read.
    versions: set[int] = field(default_factory=set)
    #: Lines whose `v` is outside `KNOWN_VERSIONS`, or absent/non-integer, as
    #: (line number, the `v` value seen). Read anyway; announced loudly. See
    #: `KNOWN_VERSIONS` for why this warns rather than refusing.
    unknown_version: list[tuple[int, Any]] = field(default_factory=list)
    paths: list[str] = field(default_factory=list)

    @property
    def all_lines(self) -> list[dict[str, Any]]:
        return self.calls + self.heartbeats + self.unknown

    def sorted_calls(self) -> list[dict[str, Any]]:
        """Calls in server-timestamp order.

        Concurrent tool calls append in completion order, and the writer batches,
        so file order is *nearly* but not exactly timestamp order. Every
        sequence question (recall-first above all) must sort first.
        """
        return sorted(self.calls, key=lambda r: r["ts"])

    def agents(self) -> list[str]:
        return sorted({c.get("agent_id", "?") for c in self.calls})

    def restart_times(self) -> list[dt.datetime]:
        """When the serve process restarted, from the heartbeat's uptime.

        A heartbeat whose `uptime_secs` is not greater than its predecessor's
        can only mean a new process. That is the one work-session boundary the
        ledger states rather than infers — and, with `git_sha`, the one that
        says *which binary* the next stretch came from.
        """
        out: list[dt.datetime] = []
        previous: int | None = None
        for hb in sorted(self.heartbeats, key=lambda r: r["ts"]):
            uptime = hb.get("uptime_secs")
            if not isinstance(uptime, int):
                continue
            if previous is not None and uptime <= previous:
                out.append(parse_ts(hb["ts"]))
            previous = uptime
        return out

    def binaries(self) -> list[tuple[str, str, int]]:
        """`(version, git_sha, heartbeat_count)`, most heartbeats first.

        More than one row means the ledger spans an upgrade — which is the point
        of stamping the sha (I2).
        """
        seen: dict[tuple[str, str], int] = {}
        for hb in self.heartbeats:
            key = (hb.get("version", "?"), hb.get("git_sha", "?"))
            seen[key] = seen.get(key, 0) + 1
        return sorted(
            ((v, s, n) for (v, s), n in seen.items()), key=lambda t: -t[2]
        )

    def queued_lines(self) -> int | None:
        """`ledger_queued_lines` from the LAST heartbeat, not the maximum.

        A gauge, not a counter — the writer's queue depth at that instant — so
        only the newest reading says anything about the state the file was left
        in. Non-zero means lines were accepted and are not on disk yet, which
        makes the tail of the file trail the traffic that produced it even when
        nothing was dropped. `None` means no heartbeat carried the key (no
        heartbeats, or a producer older than I-R2-3).

        A fully **parked** writer cannot be seen here at all: heartbeats travel
        the same channel as call lines, so a parked writer emits none of them
        (I-R3-2). That case is only visible in a live `lambo_stats` call.
        """
        for hb in sorted(self.heartbeats, key=lambda r: r["ts"], reverse=True):
            value = hb.get("stats", {}).get("ledger_queued_lines")
            if isinstance(value, int):
                return value
        return None

    def dropped_lines(self) -> int | None:
        """Highest `ledger_dropped_lines` any heartbeat reported.

        **Read this before trusting any count below it.** A non-zero value means
        the ledger is an undercount: the serve dropped lines rather than delay a
        tool call, exactly as designed. `None` means no heartbeat said (no
        heartbeats, or `--ledger-heartbeat` was off).
        """
        values = [
            hb.get("stats", {}).get("ledger_dropped_lines")
            for hb in self.heartbeats
        ]
        values = [v for v in values if isinstance(v, int)]
        return max(values) if values else None


def load(paths: Iterable[str]) -> Ledger:
    """Read one or more ledger files into a [`Ledger`]."""
    out = Ledger()
    for path in paths:
        out.paths.append(path)
        with open(path, encoding="utf-8") as fh:
            for lineno, raw in enumerate(fh, start=1):
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    record = json.loads(raw)
                except json.JSONDecodeError:
                    # A torn tail line is the one legitimate case (the process
                    # was killed mid-write). Recorded, not hidden.
                    out.unparseable.append((lineno, raw[:120]))
                    continue
                if not isinstance(record, dict):
                    out.unparseable.append((lineno, raw[:120]))
                    continue
                version = record.get("v")
                if isinstance(version, int):
                    out.versions.add(version)
                if version not in KNOWN_VERSIONS:
                    # Read it anyway (a mixed file's v1 lines are still good),
                    # but never silently: `header` says so in capitals.
                    out.unknown_version.append((lineno, version))
                kind = record.get("kind")
                if kind == "call":
                    out.calls.append(record)
                elif kind == "stats":
                    out.heartbeats.append(record)
                else:
                    out.unknown.append(record)
    if not out.all_lines and not out.unparseable:
        raise LedgerError(f"no ledger lines in {list(paths)}")
    return out


def succeeded(call: dict[str, Any]) -> bool:
    """One definition of "the call worked", used by every script here."""
    return call.get("outcome") == "ok"


def work_sessions(
    calls: list[dict[str, Any]],
    gap_minutes: float = DEFAULT_SESSION_GAP_MINUTES,
    boundaries: Iterable[dt.datetime] = (),
) -> Iterator[list[dict[str, Any]]]:
    """Split one agent's timestamp-ordered calls into work sessions.

    A new session starts when the gap since the previous call exceeds
    `gap_minutes`, **or** when a serve restart happened in between. The gap is a
    proxy and is named as one; the restart is a fact.
    """
    cuts = sorted(boundaries)
    current: list[dict[str, Any]] = []
    previous: dt.datetime | None = None
    for call in calls:
        now = parse_ts(call["ts"])
        if previous is not None:
            idle = (now - previous).total_seconds() / 60.0
            restarted = any(previous < cut <= now for cut in cuts)
            if idle > gap_minutes or restarted:
                yield current
                current = []
        current.append(call)
        previous = now
    if current:
        yield current


def base_parser(description: str) -> argparse.ArgumentParser:
    """The argument surface every script in this kit shares."""
    parser = argparse.ArgumentParser(
        description=description,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "ledger",
        nargs="+",
        help="one or more `serve --ledger` JSONL files (concatenated in order)",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit the report as JSON instead of text (for piping into duckdb/jq)",
    )
    return parser


def header(title: str, ledger: Ledger) -> list[str]:
    """The provenance block every report opens with.

    Line counts, schema version, the binaries the lines came from, and — first,
    because they decide whether the rest can be quoted — the dropped-line count
    and, when the last heartbeat reported one, the writer's outstanding queue
    depth. A backlog is not a drop, but it is the other way this file can be an
    undercount, so "the ledger is complete" is never printed over one.
    """
    out = [
        f"== {title} ==",
        f"   ledger:   {', '.join(ledger.paths)}",
        f"   lines:    {len(ledger.calls)} call, {len(ledger.heartbeats)} heartbeat"
        + (f", {len(ledger.unknown)} of unknown kind" if ledger.unknown else "")
        + (f", {len(ledger.unparseable)} UNPARSEABLE" if ledger.unparseable else ""),
        f"   schema v: {sorted(ledger.versions) or 'unstated'}",
    ]
    if ledger.unknown_version:
        seen = sorted({str(v) for _, v in ledger.unknown_version})
        out.append(
            f"   schema:   {len(ledger.unknown_version)} LINE(S) AT AN UNKNOWN SCHEMA "
            f"VERSION (saw {', '.join(seen)}; this kit knows "
            f"{sorted(KNOWN_VERSIONS)}). They were read as v"
            f"{max(KNOWN_VERSIONS)} anyway, so any field whose MEANING changed is "
            "being reported wrong. Update this kit before quoting these numbers."
        )
        out.append(
            "             first at: "
            + ", ".join(f"L{n}" for n, _ in ledger.unknown_version[:5])
        )
    binaries = ledger.binaries()
    if binaries:
        out.append("   binaries: " + "; ".join(
            f"{v} @ {s} ({n} beats)" for v, s, n in binaries
        ))
        if len(binaries) > 1:
            out.append(
                "             ^ more than one binary: this ledger spans an upgrade, "
                "so read trends across the boundary with care"
            )
    dropped = ledger.dropped_lines()
    queued = ledger.queued_lines()
    if dropped is None:
        out.append(
            "   dropped:  UNKNOWN — no heartbeat lines, so the ledger cannot say "
            "whether it dropped any. Run with --ledger-heartbeat to close this."
        )
    elif dropped:
        out.append(
            f"   dropped:  {dropped} LINES DROPPED — every count below is a "
            "LOWER BOUND. The serve dropped rather than delay a tool call."
        )
    elif queued:
        out.append("   dropped:  0 — but the ledger is not complete; see `queued`")
    else:
        out.append("   dropped:  0 — the ledger is complete")
    if queued:
        out.append(
            f"   queued:   {queued} LINE(S) STILL QUEUED at the last heartbeat — "
            "accepted by the writer and not yet on disk, so this file's tail "
            "trails the traffic and every count below is a LOWER BOUND until the "
            "writer catches up."
        )
    if ledger.unparseable:
        out.append(
            "   note:     unparseable lines were skipped: "
            + ", ".join(f"L{n}" for n, _ in ledger.unparseable[:5])
        )
    return out


def emit(
    title: str,
    ledger: Ledger,
    text: list[str],
    data: dict[str, Any],
    as_json: bool,
) -> None:
    """Print the report, in whichever form was asked for.

    The provenance the text header leads with travels in the JSON too, under
    `ledger_schema` — a `--json` consumer piping into duckdb must not be the one
    reader that cannot see an unknown schema version or a dropped-line count.
    """
    if as_json:
        provenance = {
            "versions": sorted(ledger.versions),
            "known_versions": sorted(KNOWN_VERSIONS),
            "unknown_version_lines": [
                {"line": n, "v": v} for n, v in ledger.unknown_version
            ],
            "unparseable_lines": [n for n, _ in ledger.unparseable],
            "dropped_lines": ledger.dropped_lines(),
            "queued_lines": ledger.queued_lines(),
            "call_lines": len(ledger.calls),
            "heartbeat_lines": len(ledger.heartbeats),
            "unknown_kind_lines": len(ledger.unknown),
        }
        json.dump(
            {
                "report": title,
                "ledger": ledger.paths,
                "ledger_schema": provenance,
                **data,
            },
            sys.stdout,
            indent=2,
        )
        sys.stdout.write("\n")
    else:
        print("\n".join(header(title, ledger) + [""] + text))
