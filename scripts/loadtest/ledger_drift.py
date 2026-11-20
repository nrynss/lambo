#!/usr/bin/env python3
"""Offline recall-quality change detection over `lambo serve --ledger` files (#177).

Detects shifts in recall and derive quality streams using an online conformal
exchangeability test martingale (Vovk, Nouretdinov & Gammerman, ICML 2003):

  - **Online randomized rank p-values** (eq. 6, Theorem 1): each observation is
    ranked against all past observations in the same stream (itself included)
    using an O(log n) order-statistic B-tree that collapses ties into counts
    and breaks ties uniformly at random. Unlike a fixed reference window, the
    resulting p-values are exact i.i.d. Uniform(0, 1) under exchangeability,
    even with heavy ties (e.g. the 0.35 recency floor or discrete hit counts).
  - **Sleepy Jumper mixture** (§6, eqs. 10-11): mixes power betting functions
    f_eps(p) = eps * p^(eps - 1) across an epsilon grid together with a
    sleeping state f_1(p) = 1 and a small Markov jump rate J. This avoids
    picking one fixed epsilon and lets the martingale follow a late change
    without CUSUM-style resets, so Ville's inequality P(sup_n M_n >= C) <= 1/C
    holds strictly across the entire stream.
  - **Two-sided by default**: a 50/50 mixture of the lower-tail and upper-tail
    Sleepy Jumper martingales preserves exact Ville validity while catching
    both drops (e.g. broken vectors or re-embed regressions) and rises (e.g.
    abstention spikes or junk-burst merge spikes).
  - **Conditioned streams**: split by (session, embedding contract, query class)
    for recall streams and (session, embedding contract) for derive streams, so
    contract changes reset naturally and heterogeneous query classes do not
    masquerade as quality drift.

Alarms are **diagnostic only** — traffic-mix changes within a workstream can
also reject exchangeability. Output contains aggregates and operational events
only (no query text, concept content, agent ids, or file paths beyond the CLI
inputs), safe to paste into issues or `evidence/`.

Stdlib only.

Usage:
    python3 scripts/loadtest/ledger_drift.py calls.jsonl
    python3 scripts/loadtest/ledger_drift.py --json calls-1.jsonl calls-2.jsonl
    python3 scripts/loadtest/ledger_drift.py calls.jsonl \\
        --inject-stream top_vector_cosine --inject-after 200 --inject-delta -0.10
"""

from __future__ import annotations

import argparse
import bisect
import datetime as dt
import json
import math
import random
import statistics
import sys
from collections.abc import Iterable
from dataclasses import dataclass, field
from typing import Any

#: Line schema versions this reader knows (`LINE_VERSION` in `src/ledger.rs`).
KNOWN_VERSIONS = frozenset({1})

#: Applied completion states that contribute derive facts (J4).
APPLIED_STATES = ("applied", "applied_after_restart")

#: Default active epsilon grid for the Sleepy Jumper mixture (Vovk et al. 2003).
DEFAULT_EPSILONS: tuple[float, ...] = (0.1, 0.25, 0.4, 0.55, 0.7, 0.85, 0.92)

#: Default Markov jump rate J between sleeping and active betting states.
DEFAULT_JUMP_RATE = 0.01

#: Default prior/jump mass on the sleeping state (eps = 1.0, f_1(p) = 1).
DEFAULT_SLEEP_WEIGHT = 0.5

#: Default martingale alarm threshold C (Ville false-alarm bound <= 1 / C = 1%).
DEFAULT_THRESHOLD = 100.0

#: Default +/- window in minutes for correlating alarms with operational events.
DEFAULT_EVENT_WINDOW_MINUTES = 60.0

_FRACTION_DIGITS = 6
_MIN_UTC = dt.datetime(1, 1, 1, tzinfo=dt.timezone.utc)


class LedgerDriftError(Exception):
    """Unreadable input or invalid drift-analysis options."""


def _is_int(val: Any) -> bool:
    """True for exact integers, excluding bool (which subclasses int in Python)."""
    return isinstance(val, int) and not isinstance(val, bool)


def _finite_number(val: Any) -> float | None:
    """Convert a finite int/float (excluding bool) to float, else None."""
    if isinstance(val, (int, float)) and not isinstance(val, bool):
        try:
            f = float(val)
        except OverflowError:
            return None
        if math.isfinite(f):
            return f
    return None


def parse_ts(raw: str) -> dt.datetime:
    """Parse an RFC3339 timestamp with timezone into an aware UTC datetime.

    Normalizes trailing ``z``/``Z`` to ``+00:00`` and pads/truncates fractional
    seconds to 6 digits for Python 3.10 ``datetime.fromisoformat`` parity.
    Rejects naive timestamps (missing timezone offset) and empty fractional parts.
    """
    if not isinstance(raw, str) or not raw:
        raise ValueError(f"invalid RFC3339 timestamp: {raw!r}")

    s = raw
    if s.endswith(("Z", "z")):
        s = s[:-1] + "+00:00"

    head, sep, rest = s.partition(".")
    if sep:
        digits = ""
        for ch in rest:
            if not ("0" <= ch <= "9"):
                break
            digits += ch
        if not digits:
            raise ValueError(f"empty fractional seconds in timestamp {raw!r}")
        offset = rest[len(digits):]
        s = f"{head}.{digits[:_FRACTION_DIGITS].ljust(_FRACTION_DIGITS, '0')}{offset}"

    parsed = dt.datetime.fromisoformat(s)
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise ValueError(f"naive timestamp without timezone offset: {raw!r}")
    try:
        return parsed.astimezone(dt.timezone.utc)
    except OverflowError as exc:
        raise ValueError(f"timestamp out of range: {raw!r}") from exc


def _ts_key(record: dict[str, Any]) -> tuple[dt.datetime, str]:
    raw = record.get("ts")
    if isinstance(raw, str):
        try:
            return (parse_ts(raw), raw)
        except ValueError:
            return (_MIN_UTC, raw)
    return (_MIN_UTC, "")


def _logsumexp(values: list[float]) -> float:
    m = max(values)
    if math.isinf(m):
        return m
    return m + math.log(sum(math.exp(v - m) for v in values))


# ---------------------------------------------------------------------------
# O(log n) order-statistic B-tree multiset (stdlib only)
# ---------------------------------------------------------------------------

_BTREE_MAX_KEYS = 64


@dataclass
class _Leaf:
    keys: list[float] = field(default_factory=list)
    counts: list[int] = field(default_factory=list)
    total: int = 0


@dataclass
class _Internal:
    keys: list[float] = field(default_factory=list)
    children: list[Any] = field(default_factory=list)
    total: int = 0


class OrderStatMultiset:
    """Sorted multiset supporting O(log n) insertion with exact rank counts.

    Distinct keys are stored in a B+ tree with node fanout `_BTREE_MAX_KEYS`
    and subtree totals on every node. Equal values collapse into a single entry
    with a frequency counter, so heavy-tie streams stay tiny and every update
    stays O(log n) worst case with no recursion.
    """

    def __init__(self) -> None:
        self._root: _Leaf | _Internal = _Leaf()

    def __len__(self) -> int:
        return self._root.total

    def add_and_rank(self, value: float) -> tuple[int, int, int]:
        """Insert `value` and return `(count_lt, count_eq, total_after)`.

        `count_eq` includes the newly inserted copy (always >= 1), and
        `total_after` is the new multiset size (always >= 1).
        """
        if math.isnan(value):
            raise ValueError("OrderStatMultiset does not accept NaN")

        path: list[tuple[_Internal, int]] = []
        node: _Leaf | _Internal = self._root
        lt = 0

        while isinstance(node, _Internal):
            idx = bisect.bisect_right(node.keys, value)
            for j in range(idx):
                lt += node.children[j].total
            path.append((node, idx))
            node = node.children[idx]

        # Now `node` is a _Leaf.
        pos = bisect.bisect_left(node.keys, value)
        for j in range(pos):
            lt += node.counts[j]
        if pos < len(node.keys) and node.keys[pos] == value:
            node.counts[pos] += 1
            eq = node.counts[pos]
        else:
            node.keys.insert(pos, value)
            node.counts.insert(pos, 1)
            eq = 1
        node.total += 1

        promoted: tuple[float, _Leaf | _Internal] | None = None
        if len(node.keys) > _BTREE_MAX_KEYS:
            mid = len(node.keys) // 2
            right_keys = node.keys[mid:]
            right_counts = node.counts[mid:]
            right_total = sum(right_counts)
            right = _Leaf(right_keys, right_counts, right_total)
            del node.keys[mid:]
            del node.counts[mid:]
            node.total -= right_total
            promoted = (right.keys[0], right)

        for parent, child_idx in reversed(path):
            parent.total += 1
            if promoted is not None:
                sep, right_child = promoted
                parent.keys.insert(child_idx, sep)
                parent.children.insert(child_idx + 1, right_child)
                promoted = None
                if len(parent.keys) > _BTREE_MAX_KEYS:
                    mid = len(parent.keys) // 2
                    up_sep = parent.keys[mid]
                    r_keys = parent.keys[mid + 1:]
                    r_children = parent.children[mid + 1:]
                    r_total = sum(c.total for c in r_children)
                    r_node = _Internal(r_keys, r_children, r_total)
                    del parent.keys[mid:]
                    del parent.children[mid + 1:]
                    parent.total -= r_total
                    promoted = (up_sep, r_node)

        if promoted is not None:
            sep, right_root = promoted
            left_root = self._root
            self._root = _Internal(
                keys=[sep],
                children=[left_root, right_root],
                total=left_root.total + right_root.total,
            )

        return lt, eq, self._root.total


# ---------------------------------------------------------------------------
# Online exchangeability martingale (Vovk et al., ICML 2003)
# ---------------------------------------------------------------------------

class _OneSidedSleepyJumper:
    """Single-tail Sleepy Jumper conformal test martingale in log-space.

    States are `epsilons` (each betting f_eps(p) = eps * p^(eps - 1)) plus one
    sleeping state eps = 1.0 (betting f_1(p) = 1). At every step:
      1. Capital jumps with probability J to stationary distribution pi:
           C_tilde(s) = (1 - J) * C(s) + J * pi(s) * M_{n-1}
      2. Each state bets:
           C_n(s) = C_tilde(s) * f_s(p_n)
    Because sum_s pi(s) = 1 and E[f_s(U)] = 1 for U ~ Uniform(0, 1),
    E[M_n | F_{n-1}] = M_{n-1} identically with M_0 = 1.
    """

    def __init__(
        self,
        epsilons: tuple[float, ...] = DEFAULT_EPSILONS,
        jump_rate: float = DEFAULT_JUMP_RATE,
        sleep_weight: float = DEFAULT_SLEEP_WEIGHT,
    ) -> None:
        if not epsilons:
            raise ValueError("epsilons must be non-empty")
        for eps in epsilons:
            val = _finite_number(eps)
            if val is None or not (0.0 < val < 1.0):
                raise ValueError(f"epsilon must be a finite number in (0, 1), got {eps!r}")
        jr = _finite_number(jump_rate)
        if jr is None or not (0.0 <= jr <= 1.0):
            raise ValueError(f"jump_rate must be a finite number in [0, 1], got {jump_rate!r}")
        sw = _finite_number(sleep_weight)
        if sw is None or not (0.0 <= sw < 1.0):
            raise ValueError(
                f"sleep_weight must be a finite number in [0, 1), got {sleep_weight!r}"
            )

        self.epsilons = epsilons
        self.jump_rate = jump_rate
        active_w = (1.0 - sleep_weight) / len(epsilons)
        # Index 0 is the sleeping state (eps = 1.0); indices 1.. are active epsilons.
        weights = [sleep_weight] + [active_w] * len(epsilons)
        self._log_pi = [math.log(w) if w > 0.0 else -math.inf for w in weights]
        self._log_c = list(self._log_pi)
        self._log_keep = math.log(1.0 - jump_rate) if jump_rate < 1.0 else -math.inf
        self._log_jump = math.log(jump_rate) if jump_rate > 0.0 else -math.inf
        self._log_eps = [math.log(eps) for eps in epsilons]
        self.log_m: float = 0.0

    def update(self, p_value: float) -> float:
        """Advance one step with `p_value in (0, 1]` and return `log(M_n)`."""
        if not (0.0 < p_value <= 1.0):
            raise ValueError(f"p_value must be in (0, 1], got {p_value}")
        log_p = math.log(p_value)
        prev_log_m = self.log_m
        next_log_c: list[float] = []

        for idx, (lc, lpi) in enumerate(zip(self._log_c, self._log_pi)):
            stay = lc + self._log_keep
            jump = prev_log_m + self._log_jump + lpi
            if math.isinf(stay) and stay < 0.0:
                tilde = jump
            elif math.isinf(jump) and jump < 0.0:
                tilde = stay
            else:
                tilde = _logsumexp([stay, jump])
            if idx == 0:
                # Sleeping state: f_1(p) = 1 -> log(f_1(p)) = 0
                bet = 0.0
            else:
                eps = self.epsilons[idx - 1]
                bet = self._log_eps[idx - 1] + (eps - 1.0) * log_p
            next_log_c.append(tilde + bet)

        self._log_c = next_log_c
        self.log_m = _logsumexp(next_log_c)
        return self.log_m


@dataclass
class MartingaleStep:
    index: int
    value: float
    p_low: float
    p_high: float
    log10_m: float
    log10_m_low: float | None
    log10_m_high: float | None
    dominant_side: str


class OnlineExchangeabilityMartingale:
    """Online conformal test martingale with O(log n) randomized rank p-values.

    Supports `direction` in `("two_sided", "low", "high")`:
      - `"low"`: detects downward shifts (small values -> small p_low).
      - `"high"`: detects upward shifts (large values -> small p_high).
      - `"two_sided"`: 50/50 mixture M_n = 0.5 * M_n^low + 0.5 * M_n^high,
        preserving Ville's bound exactly while detecting shifts in either tail.
    """

    def __init__(
        self,
        *,
        direction: str = "two_sided",
        epsilons: tuple[float, ...] = DEFAULT_EPSILONS,
        jump_rate: float = DEFAULT_JUMP_RATE,
        sleep_weight: float = DEFAULT_SLEEP_WEIGHT,
        seed: int = 0,
    ) -> None:
        if direction not in ("two_sided", "low", "high"):
            raise ValueError(f"unknown direction {direction!r}")
        self.direction = direction
        self._multiset = OrderStatMultiset()
        self._rng = random.Random(seed)
        self._low = (
            _OneSidedSleepyJumper(epsilons, jump_rate, sleep_weight)
            if direction in ("two_sided", "low")
            else None
        )
        self._high = (
            _OneSidedSleepyJumper(epsilons, jump_rate, sleep_weight)
            if direction in ("two_sided", "high")
            else None
        )
        self.count = 0
        self.log_m = 0.0

    def update(self, value: float) -> MartingaleStep:
        lt, eq, n = self._multiset.add_and_rank(value)
        # 1.0 - random() is uniform on (0.0, 1.0], avoiding exact 0.0.
        theta = 1.0 - self._rng.random()
        p_low = (lt + theta * eq) / n
        # Guard against rounding to 0.0 or > 1.0 at extreme n.
        p_low = min(1.0, max(1e-15, p_low))
        gt = n - lt - eq
        p_high = (gt + (1.0 - theta) * eq) / n
        p_high = min(1.0, max(1e-15, p_high))

        log_low = self._low.update(p_low) if self._low is not None else None
        log_high = self._high.update(p_high) if self._high is not None else None

        if log_low is not None and log_high is not None:
            self.log_m = _logsumexp([log_low, log_high]) - math.log(2.0)
            dominant = "low" if log_low >= log_high else "high"
        elif log_low is not None:
            self.log_m = log_low
            dominant = "low"
        else:
            assert log_high is not None
            self.log_m = log_high
            dominant = "high"

        idx = self.count
        self.count += 1
        ln10 = math.log(10.0)
        return MartingaleStep(
            index=idx,
            value=value,
            p_low=p_low,
            p_high=p_high,
            log10_m=self.log_m / ln10,
            log10_m_low=(log_low / ln10) if log_low is not None else None,
            log10_m_high=(log_high / ln10) if log_high is not None else None,
            dominant_side=dominant,
        )

    @property
    def m_value(self) -> float:
        return math.exp(self.log_m) if self.log_m < 700.0 else math.inf


# ---------------------------------------------------------------------------
# Offline changepoint dating & summary helpers (diagnostic companion)
# ---------------------------------------------------------------------------

def _quantile(sorted_vals: list[float], q: float) -> float:
    if not sorted_vals:
        return math.nan
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    pos = q * (len(sorted_vals) - 1)
    lo = int(math.floor(pos))
    hi = int(math.ceil(pos))
    if lo == hi:
        return sorted_vals[lo]
    frac = pos - lo
    return sorted_vals[lo] + frac * (sorted_vals[hi] - sorted_vals[lo])


def summarize_values(values: list[float]) -> dict[str, Any]:
    if not values:
        return {"n": 0, "mean": None, "median": None, "p10": None, "p90": None}
    s = sorted(values)
    return {
        "n": len(s),
        "mean": round(statistics.fmean(s), 6),
        "median": round(statistics.median(s), 6),
        "p10": round(_quantile(s, 0.10), 6),
        "p90": round(_quantile(s, 0.90), 6),
    }


def estimate_changepoint(values: list[float], alarm_index: int) -> int:
    """Estimate the most likely shift index in `1..alarm_index` (inclusive).

    Maximizes the Gaussian two-segment SSE reduction (equivalently the squared
    standardized difference in segment means) across splits `[0..t)` vs
    `[t..alarm_index]`. Purely diagnostic for dating an alarm; never affects
    whether an alarm fires.
    """
    end = min(len(values), alarm_index + 1)
    if end < 4:
        return max(0, end - 1)
    prefix_sum = [0.0] * (end + 1)
    for i in range(end):
        prefix_sum[i + 1] = prefix_sum[i] + values[i]
    total = prefix_sum[end]
    best_t = 1
    best_score = -1.0
    # Require at least 1 point on each side; break ties in favor of the earliest split.
    for t in range(1, end):
        n1 = t
        n2 = end - t
        mean1 = prefix_sum[t] / n1
        mean2 = (total - prefix_sum[t]) / n2
        diff = mean1 - mean2
        # Between-group sum of squares: (n1 * n2 / (n1 + n2)) * (mean1 - mean2)^2.
        score = (n1 * n2 / end) * diff * diff
        if score > best_score:
            best_score = score
            best_t = t
    return best_t


# ---------------------------------------------------------------------------
# Ledger loading, stream extraction, and event correlation
# ---------------------------------------------------------------------------

@dataclass
class OperationalEvent:
    ts: str
    ts_dt: dt.datetime
    session: str | None
    kind: str
    detail: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        return {
            "ts": self.ts,
            "session": self.session,
            "kind": self.kind,
            "detail": self.detail,
        }


@dataclass
class Observation:
    ts: str
    ts_dt: dt.datetime
    value: float


@dataclass
class ParsedLedger:
    paths: list[str] = field(default_factory=list)
    calls: list[dict[str, Any]] = field(default_factory=list)
    heartbeats: list[dict[str, Any]] = field(default_factory=list)
    completions: list[dict[str, Any]] = field(default_factory=list)
    startups: list[dict[str, Any]] = field(default_factory=list)
    leases: list[dict[str, Any]] = field(default_factory=list)
    unknown: list[dict[str, Any]] = field(default_factory=list)
    unparseable: list[tuple[int, str]] = field(default_factory=list)
    versions: set[int] = field(default_factory=set)
    unknown_version: list[tuple[int, Any]] = field(default_factory=list)

    def provenance(self) -> dict[str, Any]:
        # sess_key -> (ts_dt, uptime_secs) from the most recent heartbeat with uptime.
        prev_uptime_by_session: dict[str, tuple[dt.datetime | None, float]] = {}
        prev_binary_by_session: dict[str, tuple[str, str]] = {}
        # sess_key -> [banked_dropped, run_max_dropped]
        dropped_by_session: dict[str, list[int]] = {}
        latest_queued_by_session: dict[str, int] = {}
        for hb in sorted(self.heartbeats, key=_ts_key):
            sess_key = _heartbeat_session_key(hb)
            ts_dt = _parse_record_ts(hb)
            uptime = _heartbeat_uptime(hb)
            binary = _heartbeat_binary(hb)
            prev_pair = prev_uptime_by_session.get(sess_key)
            prev_ts = prev_pair[0] if prev_pair is not None else None
            prev_uptime = prev_pair[1] if prev_pair is not None else None
            prev_binary = prev_binary_by_session.get(sess_key)

            restarted, _ = _is_restart_boundary(
                prev_ts, prev_uptime, prev_binary, ts_dt, uptime, binary
            )
            if uptime is not None:
                prev_uptime_by_session[sess_key] = (ts_dt, uptime)
            if binary is not None:
                prev_binary_by_session[sess_key] = binary

            if restarted and sess_key in dropped_by_session:
                state = dropped_by_session[sess_key]
                state[0] += state[1]
                state[1] = 0

            stats = hb.get("stats")
            if not isinstance(stats, dict):
                continue
            d = stats.get("ledger_dropped_lines")
            if _is_int(d) and d >= 0:
                state = dropped_by_session.get(sess_key)
                if state is None:
                    dropped_by_session[sess_key] = [0, d]
                elif d < state[1]:
                    state[0] += state[1]
                    state[1] = d
                    if uptime is None:
                        prev_uptime_by_session.pop(sess_key, None)
                elif d > state[1]:
                    state[1] = d
            q = stats.get("ledger_queued_lines")
            if _is_int(q) and q >= 0:
                latest_queued_by_session[sess_key] = q
        dropped = (
            sum(banked + run_max for banked, run_max in dropped_by_session.values())
            if dropped_by_session
            else None
        )
        queued = sum(latest_queued_by_session.values()) if latest_queued_by_session else None
        bins: dict[tuple[str, str], int] = {}
        for hb in self.heartbeats:
            key = (str(hb.get("version", "?")), str(hb.get("git_sha", "?")))
            bins[key] = bins.get(key, 0) + 1
        binaries = [
            {"version": v, "git_sha": s, "heartbeats": n}
            for (v, s), n in sorted(bins.items(), key=lambda kv: (-kv[1], kv[0]))
        ]
        return {
            "versions": sorted(self.versions),
            "known_versions": sorted(KNOWN_VERSIONS),
            "unknown_version_lines": len(self.unknown_version),
            "unparseable_lines": len(self.unparseable),
            "dropped_lines": dropped,
            "queued_lines": queued,
            "call_lines": len(self.calls),
            "heartbeat_lines": len(self.heartbeats),
            "completion_lines": len(self.completions),
            "startup_lines": len(self.startups),
            "lease_lines": len(self.leases),
            "unknown_kind_lines": len(self.unknown),
            "binaries": binaries,
        }


def load_ledger(paths: Iterable[str]) -> ParsedLedger:
    out = ParsedLedger()
    for path in paths:
        out.paths.append(path)
        try:
            fh = open(path, "rb")
        except OSError as exc:
            raise LedgerDriftError(f"cannot open ledger {path!r}: {exc}") from exc
        with fh:
            for lineno, raw_bytes in enumerate(fh, start=1):
                raw_bytes = raw_bytes.strip()
                if not raw_bytes:
                    continue
                try:
                    raw = raw_bytes.decode("utf-8")
                    rec = json.loads(raw)
                except (UnicodeDecodeError, json.JSONDecodeError):
                    preview = raw_bytes[:80].decode("utf-8", errors="replace")
                    out.unparseable.append((lineno, preview))
                    continue
                if not isinstance(rec, dict):
                    out.unparseable.append((lineno, raw[:80]))
                    continue
                version = rec.get("v")
                if _is_int(version):
                    out.versions.add(version)
                    if version not in KNOWN_VERSIONS:
                        out.unknown_version.append((lineno, version))
                        continue
                else:
                    out.unknown_version.append((lineno, version))
                    continue
                kind = rec.get("kind")
                if kind == "call":
                    out.calls.append(rec)
                elif kind == "stats":
                    out.heartbeats.append(rec)
                elif kind == "completion":
                    out.completions.append(rec)
                elif kind == "startup":
                    out.startups.append(rec)
                elif kind == "lease":
                    out.leases.append(rec)
                else:
                    out.unknown.append(rec)
    total = (
        len(out.calls)
        + len(out.heartbeats)
        + len(out.completions)
        + len(out.startups)
        + len(out.leases)
        + len(out.unknown)
    )
    if total == 0 and not out.unparseable and not out.unknown_version:
        raise LedgerDriftError(f"no ledger lines in {out.paths}")
    return out


def _format_contract(raw: Any) -> str | None:
    if not isinstance(raw, dict):
        return None
    kind = raw.get("kind")
    dim = raw.get("dim")
    if not isinstance(kind, str) or not kind or not _is_int(dim) or dim <= 0:
        return None
    model = raw.get("model")
    model_str = model if isinstance(model, str) and model else "(default)"
    return f"{kind}/{model_str}/{dim}"


def _heartbeat_session_key(hb: dict[str, Any]) -> str:
    top = hb.get("session")
    if isinstance(top, str) and top:
        return top
    stats = hb.get("stats")
    if isinstance(stats, dict):
        inner = stats.get("session")
        if isinstance(inner, str) and inner:
            return inner
    return "\x00unknown"


def _heartbeat_uptime(hb: dict[str, Any]) -> float | None:
    raw = hb.get("uptime_secs")
    if raw is None and isinstance(hb.get("stats"), dict):
        raw = hb["stats"].get("uptime_secs")
    val = _finite_number(raw)
    if val is not None and val >= 0.0:
        return val
    return None


_RESTART_ELAPSED_TOLERANCE_SECS = 2.0


def _parse_record_ts(record: dict[str, Any]) -> dt.datetime | None:
    ts_raw = record.get("ts")
    if not isinstance(ts_raw, str):
        return None
    try:
        return parse_ts(ts_raw)
    except ValueError:
        return None


def _heartbeat_binary(hb: dict[str, Any]) -> tuple[str, str] | None:
    version = hb.get("version")
    git_sha = hb.get("git_sha")
    if isinstance(version, str) and version and isinstance(git_sha, str) and git_sha:
        return (version, git_sha)
    return None


def _is_restart_boundary(
    prev_ts: dt.datetime | None,
    prev_uptime: float | None,
    prev_binary: tuple[str, str] | None,
    ts_dt: dt.datetime | None,
    uptime: float | None,
    binary: tuple[str, str] | None,
) -> tuple[bool, str | None]:
    """Shared process-restart boundary detector for heartbeat streams.

    Detects restarts via strict uptime reset, binary identity change, or
    wall-clock elapsed time exceeding uptime growth by more than
    ``_RESTART_ELAPSED_TOLERANCE_SECS`` (allowing for integer ``uptime_secs``
    truncation and minor scheduling skew, while catching restarts across long
    outages where the new process's first observed uptime exceeds the old one).
    """
    if prev_uptime is not None and uptime is not None and uptime < prev_uptime:
        return True, "uptime_reset"
    if prev_binary is not None and binary is not None and binary != prev_binary:
        return True, "binary_change"
    if (
        prev_ts is not None
        and ts_dt is not None
        and prev_uptime is not None
        and uptime is not None
    ):
        elapsed = (ts_dt - prev_ts).total_seconds()
        uptime_growth = uptime - prev_uptime
        if elapsed - uptime_growth > _RESTART_ELAPSED_TOLERANCE_SECS:
            return True, "uptime_gap"
    return False, None


def _session_of_heartbeat(hb: dict[str, Any]) -> str:
    key = _heartbeat_session_key(hb)
    return "default" if key == "\x00unknown" else key


def _contract_display(contract: str, contract_epoch: int) -> str:
    return f"{contract}#{contract_epoch}" if contract_epoch > 0 else contract


@dataclass(frozen=True)
class _SessionContracts:
    times: list[dt.datetime]
    contracts: list[str]
    epochs: list[int]


def _build_contract_timeline(
    heartbeats: list[dict[str, Any]],
    startups: Iterable[dict[str, Any]] = (),
    leases: Iterable[dict[str, Any]] = (),
) -> tuple[dict[str, _SessionContracts], str | None]:
    """Per-session O(log K) contract timeline with contract epochs and unattributed gaps.

    - Consecutive identical contracts share the same ``contract_epoch``.
    - Each contract change advances ``contract_epoch`` so returning to an
      earlier contract (e.g. ``A -> B -> A``) starts a fresh detector.
    - Startup lines carry no process identity, so proximity to a ``startup``
      marker is never used to prove which process served a call. Instead:
      - When ``uptime_secs`` proves the new process booted strictly after the
        prior contract heartbeat and no lease contention occurred in the
        transition window, the 1-second integer-truncation ambiguity band
        ``[uptime_start - 1s, uptime_start)`` is marked ``"unknown"``
        (unattributed) and the new contract begins at ``uptime_start``.
      - When a lease conflict occurred during that window, or when
        ``uptime_secs`` is missing/overflowed or does not prove a restart
        after the prior heartbeat, the entire ambiguous transition interval
        up to the new heartbeat is marked ``"unknown"`` (unattributed).
    """
    del startups  # startup lines carry no process identity; kept in signature for compatibility.

    leases_by_session: dict[str, list[dt.datetime]] = {}
    unscoped_leases: list[dt.datetime] = []
    for le in sorted(leases, key=_ts_key):
        le_dt = _parse_record_ts(le)
        if le_dt is None:
            continue
        sess = le.get("session")
        if isinstance(sess, str) and sess:
            leases_by_session.setdefault(sess, []).append(le_dt)
        else:
            unscoped_leases.append(le_dt)

    times_by_session: dict[str, list[dt.datetime]] = {}
    contracts_by_session: dict[str, list[str]] = {}
    epochs_by_session: dict[str, list[int]] = {}
    current_epoch_by_session: dict[str, int] = {}
    last_contract_hb_ts: dict[str, dt.datetime] = {}
    session_names: set[str] = set()

    for hb in sorted(heartbeats, key=_ts_key):
        ts_dt = _parse_record_ts(hb)
        if ts_dt is None:
            continue
        sess = _session_of_heartbeat(hb)
        session_names.add(sess)

        stats = hb.get("stats") if isinstance(hb.get("stats"), dict) else {}
        contract = _format_contract(stats.get("embedding_contract"))
        if contract is None:
            continue

        prev_hb_ts = last_contract_hb_ts.get(sess)
        times = times_by_session.setdefault(sess, [])
        contracts = contracts_by_session.setdefault(sess, [])
        epochs = epochs_by_session.setdefault(sess, [])
        if contracts and contracts[-1] == contract:
            last_contract_hb_ts[sess] = ts_dt
            continue

        if not contracts:
            current_epoch_by_session[sess] = 0
            times.append(ts_dt)
            contracts.append(contract)
            epochs.append(0)
            last_contract_hb_ts[sess] = ts_dt
            continue

        next_epoch = current_epoch_by_session[sess] + 1
        current_epoch_by_session[sess] = next_epoch

        if prev_hb_ts is not None and prev_hb_ts < ts_dt:
            after_prev = prev_hb_ts + dt.timedelta(microseconds=1)
            uptime = _heartbeat_uptime(hb)
            uptime_start: dt.datetime | None = None
            if uptime is not None and uptime <= 1e9:
                try:
                    uptime_start = ts_dt - dt.timedelta(seconds=uptime)
                except OverflowError:
                    uptime_start = None

            if uptime_start is not None and uptime_start > prev_hb_ts:
                low_bound = uptime_start - dt.timedelta(seconds=1)
                ambig_start = max(after_prev, low_bound, times[-1])
                # Only lease contention after the new process could have booted
                # (lt >= low_bound) makes [uptime_start, ts_dt) ambiguous; an
                # earlier lease refusal under the old contract does not.
                sess_leases = leases_by_session.get(sess, [])
                has_lease_conflict = any(
                    prev_hb_ts < lt <= ts_dt and lt >= low_bound for lt in sess_leases
                ) or any(
                    prev_hb_ts < lt <= ts_dt and lt >= low_bound for lt in unscoped_leases
                )
                confirmed_start = ts_dt if has_lease_conflict else min(ts_dt, uptime_start)
            else:
                ambig_start = max(after_prev, times[-1])
                confirmed_start = ts_dt

            confirmed_start = max(confirmed_start, times[-1])
            if ambig_start < confirmed_start:
                times.append(ambig_start)
                contracts.append("unknown")
                epochs.append(next_epoch)
            times.append(confirmed_start)
            contracts.append(contract)
            epochs.append(next_epoch)
        else:
            effective_ts = max(ts_dt, times[-1])
            times.append(effective_ts)
            contracts.append(contract)
            epochs.append(next_epoch)

        last_contract_hb_ts[sess] = ts_dt

    timeline = {
        sess: _SessionContracts(
            times=times_by_session[sess],
            contracts=contracts_by_session[sess],
            epochs=epochs_by_session[sess],
        )
        for sess in times_by_session
    }
    single_hb_session = next(iter(session_names)) if len(session_names) == 1 else None
    return timeline, single_hb_session


def _lookup_contract(
    timeline: dict[str, _SessionContracts],
    session: str,
    ts_dt: dt.datetime,
) -> tuple[str, int]:
    entry = timeline.get(session)
    if entry is None or not entry.times:
        return "unknown", 0
    # Most recent contract transition at or before ts_dt; if call precedes first
    # heartbeat, fall back to the earliest contract recorded for that session.
    idx = bisect.bisect_right(entry.times, ts_dt) - 1
    if idx < 0:
        return entry.contracts[0], entry.epochs[0]
    return entry.contracts[idx], entry.epochs[idx]


def extract_operational_events(ledger: ParsedLedger) -> list[OperationalEvent]:
    """Extract restarts, binary upgrades, contract swaps, and lease/startup markers."""
    events: list[OperationalEvent] = []
    # sess -> (ts_dt, uptime_secs) from the most recent heartbeat with uptime.
    prev_uptime: dict[str, tuple[dt.datetime, float]] = {}
    prev_binary: dict[str, tuple[str, str]] = {}
    prev_contract: dict[str, str] = {}

    for hb in sorted(ledger.heartbeats, key=_ts_key):
        ts_raw = hb.get("ts")
        if not isinstance(ts_raw, str):
            continue
        try:
            ts_dt = parse_ts(ts_raw)
        except ValueError:
            continue
        sess = _session_of_heartbeat(hb)
        uptime = _heartbeat_uptime(hb)
        binary = _heartbeat_binary(hb)
        prev_pair = prev_uptime.get(sess)
        prev_ts = prev_pair[0] if prev_pair is not None else None
        prev_up = prev_pair[1] if prev_pair is not None else None
        prev_bin = prev_binary.get(sess)

        restarted, restart_reason = _is_restart_boundary(
            prev_ts, prev_up, prev_bin, ts_dt, uptime, binary
        )
        if restarted:
            detail: dict[str, Any] = {"reason": restart_reason}
            if prev_up is not None and uptime is not None:
                detail["prev_uptime_secs"] = prev_up
                detail["uptime_secs"] = uptime
            if prev_ts is not None and restart_reason == "uptime_gap":
                detail["elapsed_secs"] = round((ts_dt - prev_ts).total_seconds(), 3)
            events.append(
                OperationalEvent(
                    ts=ts_raw,
                    ts_dt=ts_dt,
                    session=sess,
                    kind="restart",
                    detail=detail,
                )
            )
        if uptime is not None:
            prev_uptime[sess] = (ts_dt, uptime)

        if binary is not None:
            if prev_bin is not None and binary != prev_bin:
                events.append(
                    OperationalEvent(
                        ts=ts_raw,
                        ts_dt=ts_dt,
                        session=sess,
                        kind="binary_change",
                        detail={
                            "from": f"{prev_bin[0]}@{prev_bin[1]}",
                            "to": f"{binary[0]}@{binary[1]}",
                        },
                    )
                )
            prev_binary[sess] = binary

        stats = hb.get("stats") if isinstance(hb.get("stats"), dict) else {}
        contract = _format_contract(stats.get("embedding_contract"))
        if contract is not None:
            if sess in prev_contract and contract != prev_contract[sess]:
                events.append(
                    OperationalEvent(
                        ts=ts_raw,
                        ts_dt=ts_dt,
                        session=sess,
                        kind="contract_change",
                        detail={"from": prev_contract[sess], "to": contract},
                    )
                )
            prev_contract[sess] = contract

    for st in sorted(ledger.startups, key=_ts_key):
        ts_raw = st.get("ts")
        if not isinstance(ts_raw, str):
            continue
        try:
            ts_dt = parse_ts(ts_raw)
        except ValueError:
            continue
        sess = st.get("session") if isinstance(st.get("session"), str) else None
        events.append(
            OperationalEvent(
                ts=ts_raw,
                ts_dt=ts_dt,
                session=sess,
                kind="startup",
                detail={"transport": st.get("transport"), "state": st.get("state")},
            )
        )

    for le in sorted(ledger.leases, key=_ts_key):
        ts_raw = le.get("ts")
        if not isinstance(ts_raw, str):
            continue
        try:
            ts_dt = parse_ts(ts_raw)
        except ValueError:
            continue
        sess = le.get("session") if isinstance(le.get("session"), str) else None
        # Keep aggregates only: omit counterparty tokens and socket paths.
        events.append(
            OperationalEvent(
                ts=ts_raw,
                ts_dt=ts_dt,
                session=sess,
                kind="lease",
                detail={"event": le.get("event"), "side": le.get("side")},
            )
        )

    events.sort(key=lambda e: (e.ts_dt, e.ts, e.kind))
    return events


def _extract_recall_leg_scores(
    call: dict[str, Any],
) -> tuple[float | None, float | None, bool, bool]:
    """Return ``(top_bm25, top_vector_cosine, exact_name_match, has_top_legs)``.

    Prefers ``#256``'s ``top_legs`` object when present; falls back to per-hit
    ``hits[].legs`` on pre-``0.13.0`` ledgers.
    """
    top_legs = call.get("top_legs")
    if isinstance(top_legs, dict):
        bm25 = _finite_number(top_legs.get("bm25"))
        vec = _finite_number(top_legs.get("vector_cosine"))
        exact = top_legs.get("exact_name_match") is True
        return bm25, vec, exact, True

    hits = call.get("hits")
    if not isinstance(hits, list):
        return None, None, False, False
    bm25_vals: list[float] = []
    vec_vals: list[float] = []
    for h in hits:
        if not isinstance(h, dict):
            continue
        legs = h.get("legs")
        if not isinstance(legs, dict):
            continue
        b = _finite_number(legs.get("bm25"))
        if b is not None:
            bm25_vals.append(b)
        v = _finite_number(legs.get("vector_cosine"))
        if v is not None:
            vec_vals.append(v)
    return (
        max(bm25_vals) if bm25_vals else None,
        max(vec_vals) if vec_vals else None,
        False,
        False,
    )


def classify_recall_query(
    call: dict[str, Any],
    top_bm25: float | None,
    top_vec: float | None,
    exact_name_match: bool,
    has_top_legs: bool,
) -> str:
    """Classify a recall call strictly from score-independent execution-mode metadata.

    Categories: ``traversal``, ``image``, ``vector``, ``exact_name``, ``hybrid``,
    ``keyword_only``, ``vector_only``, or ``other``. Never inspects query text or
    compares monitored score magnitudes (which would introduce selection bias).
    """
    annotations = call.get("response_annotations")
    if isinstance(annotations, list) and "traversal" in annotations:
        return "traversal"

    by = call.get("by")
    if by in ("image", "vector"):
        return by

    # Legacy pre-0.13.0 fallback when `response_annotations` is absent:
    # structural traversal recalls return non-empty hits whose `legs` are all empty.
    if not has_top_legs and not isinstance(annotations, list):
        hits = call.get("hits") if isinstance(call.get("hits"), list) else []
        if hits and all(isinstance(h, dict) and not (h.get("legs") or {}) for h in hits):
            return "traversal"

    if exact_name_match:
        return "exact_name"

    has_kw = top_bm25 is not None
    has_vec = top_vec is not None
    if has_kw and has_vec:
        return "hybrid"
    if has_kw:
        return "keyword_only"
    if has_vec:
        return "vector_only"
    return "other"


def _completions_by_receipt(
    completions: list[dict[str, Any]],
) -> tuple[dict[tuple[str, str], dict[str, Any]], dict[str, dict[str, Any]]]:
    by_sess_receipt: dict[tuple[str, str], dict[str, Any]] = {}
    by_unscoped_receipt: dict[str, dict[str, Any]] = {}
    for comp in sorted(completions, key=_ts_key):
        receipt = comp.get("receipt")
        if isinstance(receipt, str) and receipt:
            sess = comp.get("session")
            if isinstance(sess, str) and sess:
                by_sess_receipt[(sess, receipt)] = comp
            else:
                by_unscoped_receipt[receipt] = comp
    return by_sess_receipt, by_unscoped_receipt


def _derive_semantic_merged_rate(
    call: dict[str, Any],
    target_session: str,
    completions: tuple[dict[tuple[str, str], dict[str, Any]], dict[str, dict[str, Any]]],
) -> float | None:
    """Compute ``semantic_merged / total_concepts`` for a derive call, or None if unavailable."""
    by_sess_receipt, by_unscoped_receipt = completions
    sm = call.get("semantic_merged")
    created = call.get("created")
    matched = call.get("matched")
    if not _is_int(sm):
        receipt = call.get("receipt")
        if not isinstance(receipt, str) or not receipt:
            return None
        comp = by_sess_receipt.get((target_session, receipt))
        if comp is None:
            comp = by_unscoped_receipt.get(receipt)
        if not comp or comp.get("state") not in APPLIED_STATES:
            return None
        sm = comp.get("semantic_merged")
        if not _is_int(sm):
            return None
        created = comp.get("created_count")
        matched = comp.get("matched_count")

    if sm < 0:
        return None

    req = call.get("concepts_requested")
    if _is_int(req) and req > 0:
        if sm > req:
            return None
        return sm / req
    if _is_int(created) and _is_int(matched) and created >= 0 and matched >= 0:
        # Semantically merged concepts are already included in `created`
        # (src/graph/hybrid.rs, scripts/observability/dedup_rate.py).
        denom = created + matched
        if denom > 0 and sm <= denom:
            return sm / denom
    return None


def _target_session(call: dict[str, Any], single_hb_session: str | None) -> str:
    """Resolve the session a call operated on (preferring explicit ``scope``)."""
    scope = call.get("scope")
    if isinstance(scope, str) and scope:
        return scope
    sess = call.get("session")
    if isinstance(sess, str) and sess:
        return sess
    return single_hb_session or "default"


@dataclass(frozen=True)
class StreamKey:
    session: str
    contract: str
    query_class: str
    stream: str
    contract_epoch: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "session": self.session,
            "contract": self.contract,
            "contract_epoch": self.contract_epoch,
            "query_class": self.query_class,
            "stream": self.stream,
        }

    def label(self) -> str:
        c = _contract_display(self.contract, self.contract_epoch)
        return f"{self.session} | {c} | {self.query_class} | {self.stream}"


def extract_streams(
    ledger: ParsedLedger,
    *,
    by_query_class: bool = True,
) -> dict[StreamKey, list[Observation]]:
    """Extract time-ordered scalar streams per ``(session, contract, epoch, query_class, stream)``."""
    contract_timeline, single_hb_session = _build_contract_timeline(
        ledger.heartbeats, ledger.startups, ledger.leases
    )
    completions = _completions_by_receipt(ledger.completions)
    streams: dict[StreamKey, list[Observation]] = {}

    def push(
        session: str,
        contract: str,
        contract_epoch: int,
        query_class: str,
        stream: str,
        ts: str,
        ts_dt: dt.datetime,
        value: float,
    ) -> None:
        if math.isnan(value) or math.isinf(value):
            return
        key = StreamKey(session, contract, query_class, stream, contract_epoch)
        streams.setdefault(key, []).append(Observation(ts=ts, ts_dt=ts_dt, value=value))

    for call in sorted(ledger.calls, key=_ts_key):
        if call.get("outcome") != "ok":
            continue
        ts_raw = call.get("ts")
        if not isinstance(ts_raw, str):
            continue
        try:
            ts_dt = parse_ts(ts_raw)
        except ValueError:
            continue

        sess = _target_session(call, single_hb_session)
        contract, contract_epoch = _lookup_contract(contract_timeline, sess, ts_dt)
        tool = call.get("tool")

        if tool == "lambo_recall":
            top_bm25, top_vec, exact_name, has_top_legs = _extract_recall_leg_scores(call)
            qclass = (
                classify_recall_query(call, top_bm25, top_vec, exact_name, has_top_legs)
                if by_query_class
                else "all"
            )

            hits = call.get("hits")
            if isinstance(hits, list):
                scores = [
                    s
                    for h in hits
                    if isinstance(h, dict)
                    for s in (_finite_number(h.get("score")),)
                    if s is not None
                ]
                if scores:
                    push(
                        sess,
                        contract,
                        contract_epoch,
                        qclass,
                        "top_fused",
                        ts_raw,
                        ts_dt,
                        max(scores),
                    )

            if top_bm25 is not None:
                bm25_stream = "top_bm25" if has_top_legs else "top_bm25_legacy"
                push(sess, contract, contract_epoch, qclass, bm25_stream, ts_raw, ts_dt, top_bm25)
            if top_vec is not None:
                push(
                    sess,
                    contract,
                    contract_epoch,
                    qclass,
                    "top_vector_cosine",
                    ts_raw,
                    ts_dt,
                    top_vec,
                )

            conf = call.get("confidence")
            if isinstance(conf, dict):
                cval = _finite_number(conf.get("value"))
                basis = conf.get("basis")
                if cval is not None and isinstance(basis, str) and basis:
                    push(
                        sess,
                        contract,
                        contract_epoch,
                        qclass,
                        f"confidence:{basis}",
                        ts_raw,
                        ts_dt,
                        cval,
                    )

            if isinstance(call.get("abstained"), bool):
                push(
                    sess,
                    contract,
                    contract_epoch,
                    qclass,
                    "abstained",
                    ts_raw,
                    ts_dt,
                    1.0 if call["abstained"] else 0.0,
                )

            hit_count = call.get("hit_count")
            if _is_int(hit_count) and hit_count >= 0:
                hc = _finite_number(hit_count)
                if hc is not None:
                    push(sess, contract, contract_epoch, qclass, "hit_count", ts_raw, ts_dt, hc)

        elif tool == "lambo_derive":
            rate = _derive_semantic_merged_rate(call, sess, completions)
            if rate is not None:
                push(
                    sess,
                    contract,
                    contract_epoch,
                    "derive",
                    "semantic_merged_rate",
                    ts_raw,
                    ts_dt,
                    rate,
                )

    return streams


def _nearby_events(
    events: list[OperationalEvent],
    session: str,
    anchors: list[dt.datetime],
    window_minutes: float,
) -> list[dict[str, Any]]:
    window_secs = window_minutes * 60.0
    matched: list[dict[str, Any]] = []
    seen: set[tuple[str, str, str | None, str]] = set()
    for ev in events:
        if ev.session is not None and ev.session != session:
            continue
        deltas = [abs((ev.ts_dt - anchor).total_seconds()) for anchor in anchors]
        if min(deltas) <= window_secs:
            sig = (ev.ts, ev.kind, ev.session, json.dumps(ev.detail, sort_keys=True))
            if sig not in seen:
                seen.add(sig)
                matched.append(ev.to_dict())
    return matched


@dataclass
class ShiftInjection:
    stream: str
    after: int | dt.datetime
    delta: float


def parse_inject_after(raw: str) -> int | dt.datetime:
    try:
        idx = int(raw)
        if idx < 0:
            raise LedgerDriftError("--inject-after index must be >= 0")
        return idx
    except ValueError:
        pass
    try:
        return parse_ts(raw)
    except ValueError as exc:
        raise LedgerDriftError(
            "--inject-after must be a non-negative integer index or RFC3339 timestamp with "
            f"timezone, got {raw!r}"
        ) from exc


def _apply_injection(
    stream_name: str,
    observations: list[Observation],
    injection: ShiftInjection | None,
) -> tuple[list[Observation], int | None]:
    if injection is None or injection.stream != stream_name:
        return observations, None
    out: list[Observation] = []
    start_idx: int | None = None
    for i, obs in enumerate(observations):
        hit = (
            i >= injection.after
            if _is_int(injection.after)
            else obs.ts_dt >= injection.after
        )
        if hit:
            if start_idx is None:
                start_idx = i
            out.append(Observation(ts=obs.ts, ts_dt=obs.ts_dt, value=obs.value + injection.delta))
        else:
            out.append(obs)
    return out, start_idx


def _stream_seed(base_seed: int, key: StreamKey) -> int:
    # Stable cross-process deterministic seed per stream (avoids Python's hash randomization).
    h = 2166136261
    for byte in f"{base_seed}:{key.label()}".encode("utf-8"):
        h ^= byte
        h = (h * 16777619) & 0xFFFFFFFF
    return h


def analyze_ledger(
    ledger: ParsedLedger,
    *,
    threshold: float = DEFAULT_THRESHOLD,
    direction: str = "two_sided",
    by_query_class: bool = True,
    min_points: int = 10,
    seed: int = 0,
    jump_rate: float = DEFAULT_JUMP_RATE,
    sleep_weight: float = DEFAULT_SLEEP_WEIGHT,
    epsilons: tuple[float, ...] = DEFAULT_EPSILONS,
    event_window_minutes: float = DEFAULT_EVENT_WINDOW_MINUTES,
    injection: ShiftInjection | None = None,
) -> dict[str, Any]:
    """Run online exchangeability drift detection on every extracted ledger stream."""
    th = _finite_number(threshold)
    if th is None or th <= 1.0:
        raise LedgerDriftError(f"threshold must be a finite number > 1.0, got {threshold!r}")
    threshold = th
    if direction not in ("two_sided", "low", "high"):
        raise LedgerDriftError(
            f"direction must be one of 'two_sided', 'low', 'high', got {direction!r}"
        )
    if not _is_int(min_points) or min_points < 1:
        raise LedgerDriftError(f"min_points must be an integer >= 1, got {min_points!r}")
    if not _is_int(seed):
        raise LedgerDriftError(f"seed must be an integer, got {seed!r}")
    jr = _finite_number(jump_rate)
    if jr is None or not (0.0 <= jr <= 1.0):
        raise LedgerDriftError(f"jump_rate must be a finite number in [0, 1], got {jump_rate!r}")
    jump_rate = jr
    sw = _finite_number(sleep_weight)
    if sw is None or not (0.0 <= sw < 1.0):
        raise LedgerDriftError(
            f"sleep_weight must be a finite number in [0, 1), got {sleep_weight!r}"
        )
    sleep_weight = sw
    if isinstance(epsilons, (str, bytes)) or not isinstance(epsilons, Iterable):
        raise LedgerDriftError("epsilons must be a non-empty sequence of numbers in (0, 1)")
    eps_list: list[float] = []
    for eps in epsilons:
        val = _finite_number(eps)
        if val is None or not (0.0 < val < 1.0):
            raise LedgerDriftError(
                f"each epsilon must be a finite number in (0, 1), got {eps!r}"
            )
        eps_list.append(val)
    if not eps_list:
        raise LedgerDriftError("epsilons must be non-empty")
    epsilons = tuple(eps_list)
    ew = _finite_number(event_window_minutes)
    if ew is None or ew < 0.0:
        raise LedgerDriftError(
            f"event_window_minutes must be finite and >= 0, got {event_window_minutes!r}"
        )
    event_window_minutes = ew
    normalized_injection: ShiftInjection | None = None
    inject_after_utc: dt.datetime | None = None
    if injection is not None:
        if not isinstance(injection.stream, str) or not injection.stream:
            raise LedgerDriftError("injection.stream must be a non-empty string")
        delta = _finite_number(injection.delta)
        if delta is None:
            raise LedgerDriftError(
                f"injection.delta must be a finite number, got {injection.delta!r}"
            )
        after = injection.after
        if _is_int(after):
            if after < 0:
                raise LedgerDriftError(f"injection.after index must be >= 0, got {after!r}")
            normalized_injection = ShiftInjection(
                stream=injection.stream, after=after, delta=delta
            )
        elif isinstance(after, dt.datetime):
            if after.tzinfo is None or after.utcoffset() is None:
                raise LedgerDriftError(
                    f"injection.after datetime must be timezone-aware, got {after!r}"
                )
            try:
                inject_after_utc = after.astimezone(dt.timezone.utc)
            except OverflowError as exc:
                raise LedgerDriftError(
                    f"injection.after datetime out of UTC range: {after!r}"
                ) from exc
            normalized_injection = ShiftInjection(
                stream=injection.stream, after=inject_after_utc, delta=delta
            )
        else:
            raise LedgerDriftError(
                f"injection.after must be a non-negative int or aware datetime, got {after!r}"
            )

    log10_threshold = math.log10(threshold)
    events = extract_operational_events(ledger)
    streams = extract_streams(ledger, by_query_class=by_query_class)

    stream_reports: list[dict[str, Any]] = []
    alarms: list[dict[str, Any]] = []

    for key in sorted(
        streams.keys(),
        key=lambda k: (k.session, k.contract_epoch, k.contract, k.query_class, k.stream),
    ):
        raw_obs = streams[key]
        obs, inject_start = _apply_injection(key.stream, raw_obs, normalized_injection)
        values = [o.value for o in obs]

        if len(obs) < min_points:
            stream_reports.append(
                {
                    **key.to_dict(),
                    "n": len(obs),
                    "evaluated": False,
                    "reason": f"fewer than {min_points} observations",
                    "summary": summarize_values(values),
                    "peak_log10_m": None,
                    "alarm": False,
                }
            )
            continue

        mart = OnlineExchangeabilityMartingale(
            direction=direction,
            epsilons=epsilons,
            jump_rate=jump_rate,
            sleep_weight=sleep_weight,
            seed=_stream_seed(seed, key),
        )

        first_alarm: MartingaleStep | None = None
        peak_step: MartingaleStep | None = None

        for o in obs:
            step = mart.update(o.value)
            if peak_step is None or step.log10_m > peak_step.log10_m:
                peak_step = step
            if first_alarm is None and step.log10_m >= log10_threshold:
                first_alarm = step

        assert peak_step is not None
        entry: dict[str, Any] = {
            **key.to_dict(),
            "n": len(obs),
            "evaluated": True,
            "summary": summarize_values(values),
            "peak_log10_m": round(peak_step.log10_m, 4),
            "peak_index": peak_step.index,
            "peak_ts": obs[peak_step.index].ts,
            "alarm": first_alarm is not None,
        }
        if inject_start is not None:
            entry["injected_at_index"] = inject_start
            entry["injected_at_ts"] = obs[inject_start].ts
            entry["injected_delta"] = normalized_injection.delta if normalized_injection else None
        stream_reports.append(entry)

        if first_alarm is not None:
            cp_idx = estimate_changepoint(values, first_alarm.index)
            before_vals = values[:cp_idx]
            after_vals = values[cp_idx:]
            alarm_ts = obs[first_alarm.index].ts
            cp_ts = obs[cp_idx].ts
            nearby = _nearby_events(
                events,
                key.session,
                [obs[first_alarm.index].ts_dt, obs[cp_idx].ts_dt],
                event_window_minutes,
            )
            alarm_rec: dict[str, Any] = {
                **key.to_dict(),
                "alarm_index": first_alarm.index,
                "alarm_ts": alarm_ts,
                "alarm_log10_m": round(first_alarm.log10_m, 4),
                "alarm_m": (
                    round(10.0 ** first_alarm.log10_m, 3)
                    if first_alarm.log10_m < 12.0
                    else None
                ),
                "peak_log10_m": round(peak_step.log10_m, 4),
                "peak_index": peak_step.index,
                "peak_ts": obs[peak_step.index].ts,
                "direction": first_alarm.dominant_side,
                "estimated_change_index": cp_idx,
                "estimated_change_ts": cp_ts,
                "before": summarize_values(before_vals),
                "after": summarize_values(after_vals),
                "nearby_events": nearby,
            }
            if inject_start is not None:
                alarm_rec["injected_at_index"] = inject_start
                alarm_rec["injected_delta"] = (
                    normalized_injection.delta if normalized_injection else None
                )
                alarm_rec["detection_delay"] = (
                    first_alarm.index - inject_start
                    if first_alarm.index >= inject_start
                    else None
                )
            alarms.append(alarm_rec)

    return {
        "report": "ledger_drift",
        "ledger": ledger.paths,
        "ledger_schema": ledger.provenance(),
        "config": {
            "method": "online_sleepy_jumper_conformal_martingale",
            "threshold": threshold,
            "ville_false_alarm_bound": 1.0 / threshold,
            "direction": direction,
            "by_query_class": by_query_class,
            "min_points": min_points,
            "jump_rate": jump_rate,
            "sleep_weight": sleep_weight,
            "epsilons": list(epsilons),
            "seed": seed,
            "event_window_minutes": event_window_minutes,
            "injection": (
                {
                    "stream": normalized_injection.stream,
                    "after": (
                        inject_after_utc.isoformat()
                        if inject_after_utc is not None
                        else normalized_injection.after
                    ),
                    "delta": normalized_injection.delta,
                }
                if normalized_injection is not None
                else None
            ),
        },
        "note": (
            "Alarms are diagnostic only: a shift in query/workstream mix can also "
            "reject exchangeability."
        ),
        "operational_events": [ev.to_dict() for ev in events],
        "streams": stream_reports,
        "alarms": alarms,
    }


def format_text(report: dict[str, Any]) -> str:
    prov = report["ledger_schema"]
    cfg = report["config"]
    lines = [
        "== Recall quality drift (online Sleepy Jumper exchangeability martingale) ==",
        f"   ledger:    {', '.join(report['ledger'])}",
        (
            f"   lines:     {prov['call_lines']} call, {prov['heartbeat_lines']} heartbeat, "
            f"{prov['completion_lines']} completion, {prov['startup_lines']} startup, "
            f"{prov['lease_lines']} lease"
            + (f", {prov['unknown_kind_lines']} unknown" if prov["unknown_kind_lines"] else "")
            + (f", {prov['unparseable_lines']} UNPARSEABLE" if prov["unparseable_lines"] else "")
        ),
        f"   schema v:  {prov['versions'] or 'unstated'}",
    ]
    if prov["unknown_version_lines"]:
        lines.append(
            f"   schema:    WARNING {prov['unknown_version_lines']} line(s) at unknown schema version"
        )
    if prov["binaries"]:
        bins = "; ".join(
            f"{b['version']}@{b['git_sha']} ({b['heartbeats']} beats)"
            for b in prov["binaries"]
        )
        lines.append(f"   binaries:  {bins}")
    dropped = prov["dropped_lines"]
    queued = prov["queued_lines"]
    if dropped is None:
        lines.append("   dropped:   UNKNOWN (no heartbeats)")
    elif dropped > 0:
        lines.append(f"   dropped:   {dropped} LINES DROPPED (streams are incomplete)")
    elif queued:
        lines.append(f"   dropped:   0 (but {queued} line(s) still queued at last heartbeat)")
    else:
        lines.append("   dropped:   0")
    bound = cfg["ville_false_alarm_bound"]
    bound_str = f"{bound:.2e}" if 0 < bound < 1e-4 else f"{bound:.4g}"
    lines.append(
        f"   detector:  threshold M >= {cfg['threshold']:g} "
        f"(Ville bound <= {bound_str} per stream), "
        f"direction={cfg['direction']}, query_class={cfg['by_query_class']}, "
        f"seed={cfg['seed']}"
    )
    if cfg["injection"] is not None:
        inj = cfg["injection"]
        lines.append(
            f"   injection: stream={inj['stream']} after={inj['after']} delta={inj['delta']:+g}"
        )
    lines.append(f"   note:      {report['note']}")
    lines.append("")

    alarms = report["alarms"]
    if not alarms:
        lines.append("Alarms: none")
    else:
        lines.append(f"Alarms ({len(alarms)}):")
        for a in alarms:
            ac = _contract_display(a["contract"], a.get("contract_epoch", 0))
            lines.append(
                f"  - [{a['direction']}] {a['session']} | {ac} | "
                f"{a['query_class']} | {a['stream']}"
            )
            lines.append(
                f"      alarm at index={a['alarm_index']} ts={a['alarm_ts']} "
                f"log10(M)={a['alarm_log10_m']:.2f} (peak log10(M)={a['peak_log10_m']:.2f} "
                f"at index={a['peak_index']})"
            )
            lines.append(
                f"      est. change at index={a['estimated_change_index']} "
                f"ts={a['estimated_change_ts']}"
            )
            b, af = a["before"], a["after"]
            lines.append(
                f"      before (n={b['n']}): median={b['median']} mean={b['mean']} "
                f"p10={b['p10']} p90={b['p90']}"
            )
            lines.append(
                f"      after  (n={af['n']}): median={af['median']} mean={af['mean']} "
                f"p10={af['p10']} p90={af['p90']}"
            )
            if "detection_delay" in a:
                if a["detection_delay"] is None:
                    lines.append(
                        "      injection delay: n/a (alarm preceded injection "
                        f"at index={a['injected_at_index']}, delta={a['injected_delta']:+g})"
                    )
                else:
                    lines.append(
                        f"      injection delay: {a['detection_delay']} observation(s) "
                        f"(injected at index={a['injected_at_index']}, delta={a['injected_delta']:+g})"
                    )
            if a["nearby_events"]:
                ev_str = "; ".join(
                    f"{ev['kind']}@{ev['ts']}({ev['detail']})"
                    for ev in a["nearby_events"]
                )
                lines.append(f"      nearby events: {ev_str}")
            else:
                lines.append("      nearby events: none")

    lines.append("")
    lines.append(f"Streams ({len(report['streams'])}):")
    for s in report["streams"]:
        sc = _contract_display(s["contract"], s.get("contract_epoch", 0))
        if not s["evaluated"]:
            lines.append(
                f"  {s['session']} | {sc} | {s['query_class']} | {s['stream']}: "
                f"n={s['n']} (skipped: {s['reason']})"
            )
        else:
            flag = " ALARM" if s["alarm"] else ""
            sm = s["summary"]
            lines.append(
                f"  {s['session']} | {sc} | {s['query_class']} | {s['stream']}: "
                f"n={s['n']} median={sm['median']} mean={sm['mean']} "
                f"peak_log10_m={s['peak_log10_m']:.2f}{flag}"
            )
    return "\n".join(lines)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="Offline recall-quality change detection (online Sleepy Jumper martingale, #177).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument(
        "ledger",
        nargs="+",
        help="one or more `lambo serve --ledger` JSONL files (concatenated in order)",
    )
    p.add_argument(
        "--threshold",
        type=float,
        default=DEFAULT_THRESHOLD,
        help=f"martingale alarm threshold C (Ville bound <= 1/C; default {DEFAULT_THRESHOLD:g})",
    )
    p.add_argument(
        "--direction",
        choices=("two_sided", "low", "high"),
        default="two_sided",
        help="tail to test (default two_sided 50/50 mixture)",
    )
    p.add_argument(
        "--no-query-class",
        action="store_true",
        help="pool all recall query classes per (session, contract) instead of conditioning",
    )
    p.add_argument(
        "--min-points",
        type=int,
        default=10,
        help="minimum observations in a stream before evaluating it (default 10)",
    )
    p.add_argument(
        "--seed",
        type=int,
        default=0,
        help="RNG seed for randomized tie-breaking p-values (default 0)",
    )
    p.add_argument(
        "--jump-rate",
        type=float,
        default=DEFAULT_JUMP_RATE,
        help=f"Sleepy Jumper Markov jump probability J (default {DEFAULT_JUMP_RATE})",
    )
    p.add_argument(
        "--event-window-minutes",
        type=float,
        default=DEFAULT_EVENT_WINDOW_MINUTES,
        help=f"+/- window in minutes for correlating operational events (default {DEFAULT_EVENT_WINDOW_MINUTES:g})",
    )
    p.add_argument(
        "--inject-stream",
        default=None,
        help="optional stream name to inject a synthetic shift into (e.g. top_vector_cosine)",
    )
    p.add_argument(
        "--inject-after",
        default=None,
        help="0-based stream index or RFC3339 timestamp where the synthetic shift begins",
    )
    p.add_argument(
        "--inject-delta",
        type=float,
        default=None,
        help="additive shift applied at/after --inject-after (e.g. -0.10 to drop scores)",
    )
    p.add_argument(
        "--json",
        action="store_true",
        help="emit the report as JSON instead of text",
    )
    return p


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    inj_parts = (args.inject_stream, args.inject_after, args.inject_delta)
    if any(x is not None for x in inj_parts) and not all(x is not None for x in inj_parts):
        parser.error("--inject-stream, --inject-after, and --inject-delta must be supplied together")

    try:
        injection = (
            ShiftInjection(
                stream=args.inject_stream,
                after=parse_inject_after(args.inject_after),
                delta=args.inject_delta,
            )
            if args.inject_stream is not None
            else None
        )
        ledger = load_ledger(args.ledger)
        report = analyze_ledger(
            ledger,
            threshold=args.threshold,
            direction=args.direction,
            by_query_class=not args.no_query_class,
            min_points=args.min_points,
            seed=args.seed,
            jump_rate=args.jump_rate,
            event_window_minutes=args.event_window_minutes,
            injection=injection,
        )
    except (LedgerDriftError, ValueError) as exc:
        print(f"ledger_drift: {exc}", file=sys.stderr)
        return 2

    if args.json:
        json.dump(report, sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        print(format_text(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
