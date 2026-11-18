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
    FIXED = real_queries.FIXED

    def admitted(self, best, floor):
        return sum(x >= floor for x in best)

    def test_is_the_lowest_floor_that_keeps_to_the_cap(self):
        best = [0.40, 0.52, 0.55, 0.61, 0.70]
        floor = real_queries.level_floor(best, 2)
        self.assertEqual(self.admitted(best, floor), 2)
        # Just above the third-highest score, not at the second-highest:
        # nothing lower keeps to the cap.
        self.assertGreater(floor, 0.55)
        self.assertLess(floor, 0.5500001)
        self.assertEqual(self.admitted(best, 0.55), 3)

    def test_is_the_fixed_floor_when_that_already_keeps_to_the_cap(self):
        best = [0.40, 0.45, 0.52, 0.61]
        self.assertEqual(real_queries.level_floor(best, 2), self.FIXED)
        self.assertEqual(real_queries.level_floor(best, 4), self.FIXED)
        # Nothing at or above the fixed floor: it holds for a zero cap too.
        self.assertEqual(real_queries.level_floor([0.1, 0.2], 0), self.FIXED)
        self.assertEqual(real_queries.level_floor([], 0), self.FIXED)

    def test_zero_cap_admits_nothing(self):
        best = [0.40, 0.52, 0.70, 0.70]
        floor = real_queries.level_floor(best, 0)
        self.assertEqual(self.admitted(best, floor), 0)
        self.assertGreater(floor, 0.70)
        self.assertLess(floor, 0.7000001)

    def test_tie_at_the_cutoff_admits_fewer_never_more(self):
        # Three scores tie across a cap of 2: no floor admits exactly two, and
        # the kth-highest score (0.60) would admit three.
        best = [0.52, 0.60, 0.60, 0.60, 0.40]
        self.assertEqual(self.admitted(best, 0.60), 3)
        floor = real_queries.level_floor(best, 2)
        self.assertEqual(self.admitted(best, floor), 0)
        self.assertGreater(floor, 0.60)
        # A tie below the cutoff is not affected.
        best = [0.52, 0.52, 0.60, 0.70]
        floor = real_queries.level_floor(best, 2)
        self.assertEqual(self.admitted(best, floor), 2)
        self.assertLess(floor, 0.5200001)

    def test_never_falls_below_the_fixed_floor(self):
        best = [0.30, 0.35, 0.40, 0.45]
        self.assertEqual(real_queries.level_floor(best, 1), self.FIXED)

    def test_summarise_reports_what_the_level_floor_admits(self):
        null = sorted(i / 1000 for i in range(1000))
        grade2 = [0.45, 0.55, 0.65, 0.75]
        rows = {
            n: {"raw": [0.3, 0.3, 0.3, 0.6], "corrected": [0.3] * 12}
            for n in real_queries.SIZES
        }
        # At 32 concepts three of four queries clear the fixed floor.
        rows[32] = {"raw": [0.3, 0.58, 0.62, 0.7], "corrected": [0.3] * 11 + [0.9]}
        out = {e["layer_concepts"]: e for e in real_queries.summarise(rows, null, grade2)}
        # 8 concepts sets the cap (one of four) and sits on the fixed floor.
        self.assertEqual(out[8]["level_floor_uncorrected"], self.FIXED)
        self.assertEqual(out[8]["admit_level_floor_uncorrected"], 0.25)
        # 32: the floor is just above 0.62, admits one of four, loses 0.45
        # and 0.55 of the four grade-2 pairs.
        self.assertEqual(out[32]["level_floor_uncorrected"], 0.62)
        self.assertEqual(out[32]["admit_level_floor_uncorrected"], 0.25)
        self.assertEqual(out[32]["held_out_grade2_lost_at_level_floor_uncorrected"], 0.5)
        # Corrected: no query admits at 8, so the cap is zero and the floor
        # at 32 goes above the one score that would.
        self.assertEqual(out[8]["admit_fixed_corrected"], 0.0)
        self.assertEqual(out[32]["admit_level_floor_corrected"], 0.0)
        self.assertEqual(out[32]["level_floor_corrected"], 0.9)


class NullTailShares(unittest.TestCase):
    @staticmethod
    def entry(labelled, a, b):
        cell = lambda raw, corrected: {
            "pairs": 100,
            "at_or_above_uncorrected": raw,
            "at_or_above_corrected": corrected,
        }
        return {
            "labelled_dev": {"pairs": 200, "at_or_above": labelled},
            "A": cell(*a),
            "B": cell(*b),
        }

    def test_relative_share_is_null_when_nothing_clears_the_fixed_floor(self):
        # Project B has pairs but none at or above the fixed floor
        # (uncorrected): its counts and shares are kept, its ratio is null,
        # and every other population keeps its ratio.
        base = {}
        fixed = self.entry(20, (10, 30), (0, 4))
        higher = self.entry(5, (2, 3), (0, 1))
        real_queries.add_shares(fixed, base)
        real_queries.add_shares(higher, base)
        for e in (fixed, higher):
            self.assertEqual(e["B"]["at_or_above_uncorrected"], 0)
            self.assertEqual(e["B"]["share_uncorrected"], 0.0)
            self.assertIsNone(e["B"]["relative_to_fixed_uncorrected"])
        self.assertEqual(fixed["B"]["relative_to_fixed_corrected"], 1.0)
        self.assertEqual(higher["B"]["relative_to_fixed_corrected"], 0.25)
        self.assertEqual(higher["B"]["share_corrected"], 0.01)
        self.assertEqual(higher["A"]["relative_to_fixed_uncorrected"], 0.2)
        self.assertEqual(higher["A"]["relative_to_fixed_corrected"], 0.1)
        self.assertEqual(higher["labelled_dev"]["relative_to_fixed"], 0.25)
        self.assertEqual(higher["labelled_dev"]["share"], 0.025)
        json.dumps([fixed, higher])  # null, not an error


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
