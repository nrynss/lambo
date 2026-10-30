#!/usr/bin/env python3
"""#175: score known-positive evidence coverage without inventing negative labels.

Consumes private frozen query labels and an operator-run JSON. Emits aggregates
only. --select chooses each propagation arm on development queries only and
writes an immutable settings file BEFORE a held-out run. Latencies are component
replay timings, never a full-recall latency/adoption claim.
"""

from __future__ import annotations
import argparse
from collections import defaultdict
import hashlib
import json
import math
from pathlib import Path
import random


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def validate(dataset):
    by_id = {}
    topics = {}
    positives = {}
    for q in dataset["queries"]:
        if q["id"] in by_id:
            raise ValueError("duplicate query ID")
        by_id[q["id"]] = q
        if q["split"] not in ("dev", "held_out"):
            raise ValueError("unknown split")
        if q["kind"] not in ("answerable", "no_answer"):
            raise ValueError("unknown query kind")
        if q["topic"] in topics and topics[q["topic"]] != q["split"]:
            raise ValueError("topic crosses tuning/held-out split")
        topics[q["topic"]] = q["split"]
        facets = {x["id"] for x in q["facets"]}
        if len(facets) != len(q["facets"]):
            raise ValueError("duplicate facet")
        if not set(q.get("graph_only_facets", [])) <= facets:
            raise ValueError("unknown graph-only facet")
        if q["kind"] == "answerable" and (not facets or not q["known_relevant"]):
            raise ValueError("answerable query lacks known answer evidence")
        if q["kind"] == "no_answer" and (facets or q["known_relevant"]):
            raise ValueError("no-answer control has positive labels")
        for r in q["known_relevant"]:
            if r["node"] in positives and positives[r["node"]] != q["split"]:
                raise ValueError("known answer record crosses split")
            positives[r["node"]] = q["split"]
            if r["grade"] not in (1, 2) or not set(r["facets"]) <= facets:
                raise ValueError("invalid grade or unknown facet")
    return by_id


def setting_key(setting):
    return json.dumps(setting, sort_keys=True, separators=(",", ":"))


def measure(q, variant, hubs=None):
    known = {r["node"]: r for r in q["known_relevant"]}
    facets = {f["id"] for f in q["facets"]}
    focus = set(q.get("graph_only_facets", []))
    rendered = [h for h in variant["hits"] if h["rendered"]]
    covered = set()
    for hit in rendered:
        if hit["id"] in known:
            covered.update(known[hit["id"]]["facets"])
    return dict(
        known_facet_coverage=len(covered) / len(facets) if facets else None,
        known_graph_only_facet_coverage=len(covered & focus) / len(focus)
        if focus
        else None,
        known_graph_only_facets_covered=len(covered & focus),
        known_graph_only_facets=len(focus),
        known_answer_hit_at_10=float(
            any(h["id"] in known for h in variant["hits"][:10])
        ),
        known_positive_rendered=sum(h["id"] in known for h in rendered),
        unjudged_rendered=sum(h["id"] not in known for h in rendered) if facets else 0,
        known_graph_only_rendered=sum(
            h["id"] in known and h["graph_only"] for h in rendered
        ),
        graph_only_rendered=sum(h["graph_only"] for h in rendered),
        hub_rendered=sum(h["id"] in hubs for h in rendered)
        if hubs is not None
        else None,
        rendered=len(rendered),
        truncated=sum(h["truncated"] for h in variant["hits"]),
        tokens=variant["context_tokens"],
        assembly_plus_propagation_us=variant["assembly_plus_propagation_us"],
    )


def mean(values):
    values = [x for x in values if x is not None]
    return sum(values) / len(values) if values else None


def quantile(values, q):
    xs = sorted(values)
    return xs[max(0, math.ceil(len(xs) * q) - 1)] if xs else None


def score_rows(dataset, run, split, hubs=None):
    queries = validate(dataset)
    result = defaultdict(list)
    seen = set()
    for row in run["queries"]:
        q = queries.get(row["id"])
        if q is None:
            raise ValueError("run query missing from labels")
        if row["id"] in seen:
            raise ValueError("duplicate run query")
        seen.add(row["id"])
        if q["split"] != split:
            raise ValueError("run mixes splits or uses wrong split")
        for v in row["variants"]:
            m = measure(q, v, hubs)
            result[setting_key(v["setting"])].append(
                dict(
                    id=q["id"],
                    topic=q["topic"],
                    split=q["split"],
                    family=q["family"],
                    kind=q["kind"],
                    graph_only_candidates=row["graph_only_candidates"],
                    **m,
                )
            )
    if seen != {q["id"] for q in dataset["queries"] if q["split"] == split}:
        raise ValueError("run does not contain the full labelled split")
    counts = {len(rows) for rows in result.values()}
    if counts != {len(seen)} or any(
        {r["id"] for r in rs} != seen for rs in result.values()
    ):
        raise ValueError("settings were not evaluated on identical query sets")
    return result


def aggregate(rows):
    answerable = [r for r in rows if r["kind"] == "answerable"]
    controls = [r for r in rows if r["kind"] == "no_answer"]
    return dict(
        answerable=len(answerable),
        controls=len(controls),
        topics=len({r["topic"] for r in answerable}),
        known_facet_coverage=mean(r["known_facet_coverage"] for r in answerable),
        known_graph_only_facet_coverage=mean(
            r.get("known_graph_only_facet_coverage") for r in answerable
        ),
        known_graph_only_facets_covered=sum(
            r.get("known_graph_only_facets_covered", 0) for r in answerable
        ),
        known_graph_only_facets=sum(
            r.get("known_graph_only_facets", 0) for r in answerable
        ),
        known_answer_hit_at_10=mean(r["known_answer_hit_at_10"] for r in answerable),
        known_graph_only_rendered=sum(
            r["known_graph_only_rendered"] for r in answerable
        ),
        known_positive_rendered=sum(r["known_positive_rendered"] for r in answerable),
        unjudged_rendered=sum(r["unjudged_rendered"] for r in answerable),
        graph_only_rendered=sum(r["graph_only_rendered"] for r in answerable),
        hub_share_rendered=(
            sum(r["hub_rendered"] for r in answerable)
            / sum(r["rendered"] for r in answerable)
            if answerable
            and all(r["hub_rendered"] is not None for r in answerable)
            and sum(r["rendered"] for r in answerable)
            else None
        ),
        mean_context_tokens=mean(r["tokens"] for r in rows),
        component_us=dict(
            p50=quantile([r["assembly_plus_propagation_us"] for r in rows], 0.5),
            p99=quantile([r["assembly_plus_propagation_us"] for r in rows], 0.99),
        ),
        controls_with_rendered_context=sum(
            r["rendered"] > 0 or r["truncated"] > 0 for r in controls
        ),
    )


def select(rows, family=None, primary="known_facet_coverage"):
    """Dev only. A predeclared family/metric can isolate screened missing facets.

    Overall known support and hit@10 break quality ties before lower gain does.
    """
    if any(r["split"] != "dev" for rs in rows.values() for r in rs):
        raise ValueError("selection requires development rows")
    chosen = {}
    for arm in ["damped", "degree"]:
        options = []
        for key, values in rows.items():
            if json.loads(key)["arm"] != arm:
                continue
            scoped = [r for r in values if family is None or r["family"] == family]
            if not scoped:
                raise ValueError("selection family has no rows")
            quality = aggregate(scoped)[primary]
            if quality is None or not math.isfinite(quality):
                raise ValueError("selection metric has no labelled observations")
            options.append((key, quality, aggregate(values)))
        if not options:
            raise ValueError("missing propagation arm")

        def rank(item):
            key, quality, all_queries = item
            setting = json.loads(key)
            return (
                -quality,
                -all_queries["known_facet_coverage"],
                -all_queries["known_answer_hit_at_10"],
                setting["gain"],
                abs(setting["damping"] - 0.5),
                setting["damping"],
            )

        key, _, _ = min(options, key=rank)
        chosen[arm] = json.loads(key)
    return chosen


def paired_topic_interval(base, other, seed=175, draws=2000):
    """Paired topic-cluster bootstrap; not 80 independent seed cases."""
    baseline = {r["id"]: r for r in base if r["kind"] == "answerable"}
    groups = defaultdict(list)
    for row in other:
        if row["kind"] == "answerable":
            groups[row["topic"]].append(
                row["known_facet_coverage"]
                - baseline[row["id"]]["known_facet_coverage"]
            )
    topics = sorted(groups)
    rng = random.Random(seed)
    samples = []
    for _ in range(draws):
        values = [v for _ in topics for v in groups[rng.choice(topics)]]
        samples.append(mean(values))
    return dict(
        mean=mean(v for vs in groups.values() for v in vs),
        bootstrap_topic_clusters=len(topics),
        draws=draws,
        low=quantile(samples, 0.025),
        high=quantile(samples, 0.975),
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--labels", required=True, type=Path)
    ap.add_argument("--run", required=True, type=Path)
    ap.add_argument("--split", choices=["dev", "held_out"], required=True)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--select", type=Path)
    ap.add_argument(
        "--selection-family", help="predeclared development selection family"
    )
    ap.add_argument(
        "--selection-metric",
        choices=["known_facet_coverage", "known_graph_only_facet_coverage"],
        default="known_facet_coverage",
    )
    ap.add_argument("--hubs", type=Path, help="private census hub key; same snapshot")
    a = ap.parse_args()
    if a.select and a.split != "dev":
        ap.error("never select settings on held-out outcomes")
    labels = json.loads(a.labels.read_text())
    run = json.loads(a.run.read_text())
    if run.get("dataset_sha256") != digest(a.labels):
        raise ValueError("run labels hash differs from supplied labels")
    if run.get("snapshot_sha256") != labels.get("snapshot_sha256"):
        raise ValueError("run snapshot differs from labels")
    hubs = None
    if a.hubs:
        key = json.loads(a.hubs.read_text())
        if key["snapshot_sha256"] != labels["snapshot_sha256"]:
            raise ValueError("hub key snapshot differs")
        hubs = set(key["hubs"])
    rows = score_rows(labels, run, a.split, hubs)
    baseline_key = next(k for k in rows if json.loads(k)["arm"] == "zero")
    summaries = []
    for k, rs in rows.items():
        summaries.append(
            dict(
                setting=json.loads(k),
                **aggregate(rs),
                known_coverage_delta_vs_zero=paired_topic_interval(
                    rows[baseline_key], rs
                ),
                by_family={
                    f: aggregate([r for r in rs if r["family"] == f])
                    for f in sorted({r["family"] for r in rs})
                },
            )
        )
    report = dict(
        split=a.split,
        labels_sha256=digest(a.labels),
        run_sha256=digest(a.run),
        metric_scope="Known-positive evidence coverage only. Unjudged is not irrelevant. "
        "No precision, relevance recall, factual-answer or adoption guarantee.",
        hub_key_sha256=digest(a.hubs) if a.hubs else None,
        timing_scope="one debug replay timing per query/setting for assembly plus propagation; "
        "excludes embedding and phase1; not end-to-end recall latency",
        variants=summaries,
    )
    if a.select:
        selection = dict(
            selected=select(rows, a.selection_family, a.selection_metric),
            selection_family=a.selection_family,
            selection_metric=a.selection_metric,
            selection_split="dev",
            criterion="max primary metric on declared dev family; ties: overall known-facet coverage, "
            "overall known-answer hit@10, lower gain, damping nearest .5",
            labels_sha256=digest(a.labels),
            run_sha256=digest(a.run),
        )
        with a.select.open("x") as f:
            json.dump(selection, f, indent=2)
            f.write("\n")
        report["selection"] = selection
    with a.out.open("x") as f:
        json.dump(report, f, indent=2, allow_nan=False)
        f.write("\n")
    print(
        json.dumps(
            dict(
                split=a.split,
                settings=len(rows),
                queries=len(next(iter(rows.values()))),
                selected=report.get("selection", {}).get("selected"),
            )
        )
    )


if __name__ == "__main__":
    main()
