#!/usr/bin/env python3
"""#254, second check: the candidate floor on real recall queries.

Reads the private analysis database behind `evidence/layer-floor-size/`
(read-only, through the `duckdb` CLI) and writes aggregates only to
`real-queries.json`: no query text, no concept text, no session name.

    python3 evidence/issue-254/real_queries.py --db <analysis.duckdb> \
        --project-a <session> --project-b <session>

The database needs three tables. `q` is the one
`evidence/layer-floor-size/method.sql` builds; `pool` and `e2` are that file's
steps 2 and 3 made per project, and the README beside this script
("Reproducing") gives the SQL that builds them:

    q(qid, session, v FLOAT[1024])
    pool(grp, id, v FLOAT[1024], r1, r2, r3)
    e2(qid, session, ns INTEGER[], a FLOAT[], b FLOAT[], c3 FLOAT[], araw FLOAT[])

`q` holds the logged recall queries with their vectors. `pool` holds the
stand-in layer of each project: `grp` is the session name of the project
whose queries the layer is for (it equals `q.session` and the name passed as
`--project-a` or `--project-b`), and its rows are concepts of sessions
unrelated to that project, ranked in three random orders (`r1`, `r2`, `r3`)
within the group. `e2` has one row per query of a project. `ns` holds the
layer sizes, and the four score arrays are index-aligned with it: element i
of `a`, `b` and `c3` is the query's best cosine over the first `ns[i]`
concepts of its project's layer in order `r1`, `r2` and `r3`, with the linear
correction applied; element i of `araw` is order `r1` without the correction.

For each size it reports the share of queries admitting a hit at the fixed
floor and at the candidate rule's floor (BGE-M3, from `floor_rule.py`), and
the level floor: the lowest floor, at or above the fixed one, at which these
queries themselves admit no more often than they do at 8 concepts under the
fixed floor (in-sample, so the best case for any size-only floor), with the
share it does admit and the share of the #190 set's held-out grade-2 pairs
it loses.

It also reports the null tail (`null_tail`): the share of single (query,
unrelated concept) pairs at or above each of the rule's floors, for each
project, beside the same share of the labelled dev null the rule was derived
from. That is what decides whether the rule's floors are too high for these
queries. The corrected figure applies the linear fit of
`evidence/layer-floor-size/query-vector-check.json`.
"""

import argparse
import json
import math
import pathlib
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import floor_rule  # noqa: E402

FIXED = 0.50
N0 = 12
SIZES = [8, 32, 64, 128, 256, 512, 1024, 2048]


def level_floor(best, cap):
    """The lowest floor, at or above `FIXED`, that at most `cap` of `best` clear.

    A score clears a floor it is at or above. The fixed floor is the answer
    when it already admits no more than `cap`. Otherwise more than `cap`
    scores are at or above it, and the lowest floor that keeps to the cap is
    the next float above the (cap + 1)-th highest score: any lower floor
    admits that score too. Scores tied with it are excluded with it, so at a
    tie the floor admits fewer than `cap`, never more. With `cap` 0 it is the
    next float above the highest score.
    """
    if sum(x >= FIXED for x in best) <= cap:
        return FIXED
    return math.nextafter(sorted(best, reverse=True)[cap], math.inf)


def summarise(rows, null_dev, grade2_held_out):
    """`rows`: {size: {"raw": [best...], "corrected": [[best per order]...]}}."""
    out = []
    base = {}
    for n in SIZES:
        r = rows[n]
        rule = floor_rule.rule_floor(null_dev, FIXED, N0, n)[0]
        entry = {"layer_concepts": n, "queries": len(r["raw"]), "rule_floor": round(rule, 4)}
        for name, best in (("uncorrected", r["raw"]), ("corrected", r["corrected"])):
            entry[f"admit_fixed_{name}"] = round(floor_rule.share_at_or_above(best, FIXED), 4)
            entry[f"admit_rule_{name}"] = round(floor_rule.share_at_or_above(best, rule), 4)
            if n == SIZES[0]:
                base[name] = floor_rule.share_at_or_above(best, FIXED)
            # The cap is the 8-concept share of this size's scores, rounded
            # down (every size has the same number of scores here, so it is
            # the 8-concept count itself).
            level = level_floor(best, math.floor(base[name] * len(best) + 1e-9))
            entry[f"level_floor_{name}"] = round(level, 4)
            entry[f"admit_level_floor_{name}"] = round(
                floor_rule.share_at_or_above(best, level), 4
            )
            entry[f"held_out_grade2_lost_at_level_floor_{name}"] = round(
                sum(x < level for x in grade2_held_out) / len(grade2_held_out), 4
            )
        out.append(entry)
    return out


def duckdb_json(db, sql):
    """Run `sql` against `db`, read-only, and return its rows.

    `-json` is the only output switch: DuckDB 1.4 refuses `-no-agent` (a 2.0
    flag), and under 2.0 `-json` already keeps the agent-mode notice off
    stdout.
    """
    out = subprocess.run(
        ["duckdb", "-readonly", db, "-json", "-c", sql],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(out)


def add_shares(entry, base):
    """Add each population's share at or above this entry's floor, in place.

    `base` remembers each population's share at the first floor seen (the
    fixed one); `relative_to_fixed` is the share divided by it, or `None`
    when that population has no pair at or above the fixed floor.
    """
    cells = [("labelled_dev", "at_or_above")] + [
        (p, f"at_or_above_{name}") for p in ("A", "B") for name in ("uncorrected", "corrected")
    ]
    for who, key in cells:
        cell = entry[who]
        share = cell[key] / cell["pairs"]
        fixed = base.setdefault((who, key), share)
        suffix = key.removeprefix("at_or_above").lstrip("_")
        tag = f"_{suffix}" if suffix else ""
        cell[f"share{tag}"] = round(share, 6)
        cell[f"relative_to_fixed{tag}"] = round(share / fixed, 4) if fixed else None


def null_tail(db, labels, null_dev):
    """Per project, the single-pair share at or above each rule floor."""
    check = json.loads((HERE.parent / "layer-floor-size" / "query-vector-check.json").read_text())
    intercept, slope = check[0]["intercept"], check[0]["slope"]
    sizes = floor_rule.SIZES
    floors = [floor_rule.rule_floor(null_dev, FIXED, N0, n)[0] for n in sizes]
    names = ", ".join("'" + name.replace("'", "''") + "'" for name in labels)
    sql = (
        "with p as (select q.session s, array_cosine_similarity(q.v, pool.v) cos "
        f"from q join pool on pool.grp = q.session where q.session in ({names})), "
        f"f as (select unnest({floors!r})::double fl) "
        "select s, fl, count(*) pairs, sum((cos >= fl)::int) raw, "
        f"sum(({intercept} + {slope} * cos >= fl)::int) corrected "
        "from p, f group by all"
    )
    by = {(labels[r["s"]], round(float(r["fl"]), 4)): r for r in duckdb_json(db, sql)}
    out = {
        "note": (
            "Share of single (query, unrelated concept) pairs at or above each floor the "
            "rule gives (anchor 12). labelled_dev is the null the rule was derived from. "
            "relative_to_fixed is that share divided by the same population's share at the "
            "fixed floor: how much of the mass at 0.50 is still there (null when that "
            "population has no pair at or above the fixed floor). Pairs that share a "
            "query are not independent, and the correction was fitted on logged top hits, "
            "not on pairs in this tail."
        ),
        "correction": {"intercept": intercept, "slope": slope},
        "floors": [],
    }
    base = {}
    for n, floor in zip(sizes, floors):
        labelled = sum(x >= floor for x in null_dev)
        entry = {
            "rule_floor_for_layer_concepts": n,
            "floor": round(floor, 4),
            "labelled_dev": {"pairs": len(null_dev), "at_or_above": labelled},
        }
        for p in ("A", "B"):
            r = by[(p, round(floor, 4))]
            entry[p] = {
                "pairs": int(r["pairs"]),
                "at_or_above_uncorrected": int(r["raw"]),
                "at_or_above_corrected": int(r["corrected"]),
            }
        add_shares(entry, base)
        out["floors"].append(entry)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", required=True)
    ap.add_argument("--project-a", required=True)
    ap.add_argument("--project-b", required=True)
    a = ap.parse_args()
    sql = (
        "select session, unnest(ns) n, unnest(araw) raw, unnest(a) a, unnest(b) b, "
        "unnest(c3) c from e2"
    )
    logged = duckdb_json(a.db, sql)
    labels = {a.project_a: "A", a.project_b: "B"}
    rows = {p: {n: {"raw": [], "corrected": []} for n in SIZES} for p in labels.values()}
    for r in logged:
        if r["session"] in labels and r["n"] in SIZES:
            cell = rows[labels[r["session"]]][r["n"]]
            cell["raw"].append(r["raw"])
            cell["corrected"].extend([r["a"], r["b"], r["c"]])
    name, path, _, _ = floor_rule.CONTRACTS[0]
    run = floor_rule.Run(path, floor_rule.FRAMING)
    null_dev = sorted(run.cosines("dev", 0))
    grade2 = run.cosines("held_out", 2)
    result = {
        "issue": 254,
        "contract": name,
        "fixed_floor": FIXED,
        "rule_anchor_concepts": N0,
        "note": (
            "Query vectors are approximate (see evidence/layer-floor-size/README.md): "
            "uncorrected and corrected are two sensitivity estimates, not an interval. "
            "uncorrected is one random order of the stand-in layer; corrected pools three."
        ),
        "projects": {p: summarise(rows[p], null_dev, grade2) for p in ("A", "B")},
        "null_tail": null_tail(a.db, labels, null_dev),
    }
    (HERE / "real-queries.json").write_text(json.dumps(result, indent=1) + "\n")
    for p in ("A", "B"):
        print(f"\nproject {p} ({result['projects'][p][0]['queries']} queries)\n")
        print(
            "| Layer size | Fixed, uncorr. | Fixed, corr. | Rule floor | Rule, uncorr. | "
            "Rule, corr. | Level floor, uncorr. / corr. | Admitted at it | Grade-2 lost at it |"
        )
        print("|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
        for e in result["projects"][p]:
            print(
                f"| {e['layer_concepts']:,} | {100 * e['admit_fixed_uncorrected']:.1f}% | "
                f"{100 * e['admit_fixed_corrected']:.1f}% | {e['rule_floor']:.4f} | "
                f"{100 * e['admit_rule_uncorrected']:.1f}% | "
                f"{100 * e['admit_rule_corrected']:.1f}% | "
                f"{e['level_floor_uncorrected']:.3f} / {e['level_floor_corrected']:.3f} | "
                f"{100 * e['admit_level_floor_uncorrected']:.1f}% / "
                f"{100 * e['admit_level_floor_corrected']:.1f}% | "
                f"{100 * e['held_out_grade2_lost_at_level_floor_uncorrected']:.0f}% / "
                f"{100 * e['held_out_grade2_lost_at_level_floor_corrected']:.0f}% |"
            )
    print("\nnull tail: single pairs at or above each rule floor (uncorrected / corrected)\n")
    print("| Rule floor for | Floor | Labelled dev | A | B |")
    print("|---:|---:|---:|---:|---:|")
    for e in result["null_tail"]["floors"]:
        print(
            f"| {e['rule_floor_for_layer_concepts']:,} | {e['floor']:.4f} | "
            f"{100 * e['labelled_dev']['share']:.3f}% | "
            f"{100 * e['A']['share_uncorrected']:.3f}% / {100 * e['A']['share_corrected']:.3f}% | "
            f"{100 * e['B']['share_uncorrected']:.3f}% / {100 * e['B']['share_corrected']:.3f}% |"
        )


if __name__ == "__main__":
    main()
