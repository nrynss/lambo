"""Unit tests for the #254 evidence scripts' arithmetic.

    python3 -m unittest discover -s evidence/issue-254 -p 'test_*.py'
"""

import json
import pathlib
import sys
import unittest

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import floor_rule  # noqa: E402
import real_queries  # noqa: E402


class PerPairShare(unittest.TestCase):
    def test_holds_the_best_of_n_rate(self):
        p0, n0 = 0.03, 12
        at_n0 = 1 - (1 - p0) ** n0
        for n in (32, 100, 2048):
            p_n = floor_rule.per_pair_share(p0, n0, n)
            self.assertAlmostEqual(1 - (1 - p_n) ** n, at_n0, places=12)

    def test_sits_between_the_papers_bounds(self):
        # alpha / N <= p_N <= -ln(1 - alpha) / N (Proposition 1).
        import math

        p0, n0, n = 0.03, 12, 1024
        alpha = 1 - (1 - p0) ** n0
        p_n = floor_rule.per_pair_share(p0, n0, n)
        self.assertGreaterEqual(p_n, alpha / n)
        self.assertLessEqual(p_n, -math.log(1 - alpha) / n)


class RuleFloor(unittest.TestCase):
    NULL = sorted(i / 1000 for i in range(1000))  # uniform on [0, 0.999]

    def test_is_the_fixed_floor_at_or_below_the_measured_size(self):
        for n in (1, 12, 100):
            floor, p = floor_rule.rule_floor(self.NULL, 0.9, 100, n)
            self.assertEqual(floor, 0.9)
            self.assertAlmostEqual(p, 0.1)

    def test_rises_with_the_layer_and_never_falls_below_the_fixed_floor(self):
        floors = [floor_rule.rule_floor(self.NULL, 0.9, 12, n)[0] for n in (12, 32, 128, 2048)]
        self.assertEqual(floors, sorted(floors))
        self.assertEqual(floors[0], 0.9)
        self.assertGreater(floors[-1], floors[0])

    def test_quantile_is_nearest_rank(self):
        self.assertEqual(floor_rule.quantile([1, 2, 3, 4], 0.5), 2)
        self.assertEqual(floor_rule.quantile([1, 2, 3, 4], 1.0), 4)
        self.assertEqual(floor_rule.quantile([1, 2, 3, 4], 0.0), 1)


class LevelFloor(unittest.TestCase):
    def test_admits_the_requested_share(self):
        best = [i / 100 for i in range(100)]
        floor = real_queries.level_floor(best, 0.25)
        self.assertAlmostEqual(floor_rule.share_at_or_above(best, floor), 0.25)


class Committed(unittest.TestCase):
    """The committed aggregates say what the README quotes."""

    def test_bge_rule_reproduces_the_fixed_floor_at_the_measured_size(self):
        result = json.loads((HERE / "floor-rule.json").read_text())
        bge = result["contracts"][0]
        self.assertEqual(bge["fixed_floor"], 0.5)
        first = bge["anchors"]["recorded"]["sizes"][0]
        self.assertEqual((first["layer_concepts"], first["floor"]), (12, 0.5))
        for anchor in bge["anchors"].values():
            floors = [r["floor"] for r in anchor["sizes"]]
            self.assertEqual(floors, sorted(floors))

    def test_bootstrap_intervals_contain_their_point_estimates(self):
        result = json.loads((HERE / "floor-rule.json").read_text())
        for c in result["contracts"]:
            b = c["bootstrap_common12"]
            self.assertEqual((b["unit"], b["anchor_n0"]), ("query", 12))
            by_size = {r["layer_concepts"]: r for r in c["anchors"]["common12"]["sizes"]}
            for r in b["sizes"]:
                lo, hi = r["floor"]
                self.assertLessEqual(lo, r["floor_point"])
                self.assertLessEqual(r["floor_point"], hi)
                for split in ("dev", "held_out"):
                    point = by_size[r["layer_concepts"]][split]
                    share = point["grade2_lost"] / point["grade2_pairs"]
                    lo, hi = r["grade2_lost_share"][split]
                    self.assertLessEqual(lo, share + 1e-4)
                    self.assertLessEqual(share - 1e-4, hi)

    def test_readme_figures_at_32_and_100_concepts(self):
        # The README rests its conclusion on 100 concepts and quotes dev
        # beside held-out at 32, where the two splits disagree.
        result = json.loads((HERE / "floor-rule.json").read_text())
        sizes = {r["layer_concepts"]: r for r in result["contracts"][0]["anchors"]["recorded"]["sizes"]}
        lost = lambda n, split: (sizes[n][split]["grade2_lost"], sizes[n][split]["grade2_pairs"])
        self.assertEqual((lost(12, "dev"), lost(12, "held_out")), ((25, 169), (17, 98)))
        self.assertEqual((lost(32, "dev"), lost(32, "held_out")), ((34, 169), (32, 98)))
        self.assertEqual((lost(100, "dev"), lost(100, "held_out")), ((62, 169), (44, 98)))

    def test_cross_project_tail_falls_faster_than_the_labelled_null(self):
        # Above the fixed floor, each project keeps a smaller part of its
        # share at 0.50 than the labelled dev null does, under both estimates.
        tail = json.loads((HERE / "real-queries.json").read_text())["null_tail"]["floors"]
        self.assertEqual(tail[0]["floor"], 0.5)
        for e in tail[1:]:
            labelled = e["labelled_dev"]["relative_to_fixed"]
            for p in ("A", "B"):
                for name in ("uncorrected", "corrected"):
                    self.assertLess(e[p][f"relative_to_fixed_{name}"], labelled, (e["floor"], p))

    def test_real_query_aggregates_hold_no_names(self):
        text = (HERE / "real-queries.json").read_text()
        self.assertEqual(sorted(json.loads(text)["projects"]), ["A", "B"])
        self.assertNotIn("/Users/", text)
        self.assertNotIn("session", text)


if __name__ == "__main__":
    unittest.main()
