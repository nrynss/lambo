"""#175 metric integrity: unknown candidates, held-out separation, conservative ties."""

import copy
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import graph_compare as g


def query(name="q", split="dev", topic="one"):
    return dict(
        id=name,
        query="question",
        kind="answerable",
        topic=topic,
        split=split,
        family="test",
        facets=[dict(id="f1", text="answer")],
        known_relevant=[dict(node="answer-" + name, grade=2, facets=["f1"])],
    )


def variant(arm="zero", gain=0, damping=0.5, hits=None):
    return dict(
        setting=dict(arm=arm, gain=gain, damping=damping),
        hits=hits or [],
        context_tokens=100,
        assembly_plus_propagation_us=10,
    )


def hit(node, rendered=True, truncated=False):
    return dict(id=node, rendered=rendered, truncated=truncated, graph_only=False)


class Metrics(unittest.TestCase):
    def test_unknown_is_unjudged_not_a_negative_and_truncation_is_not_full_evidence(
        self,
    ):
        q = query()
        m = g.measure(q, variant(hits=[hit("unknown"), hit("answer-q", False, True)]))
        self.assertEqual(m["known_facet_coverage"], 0)
        self.assertEqual(m["unjudged_rendered"], 1)
        self.assertEqual(m["known_answer_hit_at_10"], 1)
        self.assertNotIn("precision", m)
        self.assertEqual(
            g.measure(q, variant(hits=[hit("answer-q")]))["known_facet_coverage"], 1
        )

    def test_hub_share_uses_rendered_hits_and_does_not_change_known_labels(self):
        q = query()
        v = variant(hits=[hit("answer-q"), hit("hub")])
        m = g.measure(q, v, {"hub"})
        self.assertEqual(m["hub_rendered"], 1)
        self.assertEqual(m["unjudged_rendered"], 1)
        self.assertEqual(m["known_facet_coverage"], 1)
        row = dict(
            id="q",
            topic="one",
            family="test",
            split="dev",
            kind="answerable",
            graph_only_candidates=1,
            **m,
        )
        self.assertEqual(g.aggregate([row])["hub_share_rendered"], 0.5)

    def test_split_validation_rejects_topic_or_positive_leakage(self):
        a = query()
        b = query("b", "held_out")
        with self.assertRaisesRegex(ValueError, "topic crosses"):
            g.validate(dict(queries=[a, b]))
        b["topic"] = "two"
        b["known_relevant"] = copy.deepcopy(a["known_relevant"])
        with self.assertRaisesRegex(ValueError, "answer record crosses"):
            g.validate(dict(queries=[a, b]))

    def test_no_answer_cannot_carry_positive_labels(self):
        q = query()
        q["kind"] = "no_answer"
        with self.assertRaisesRegex(ValueError, "no-answer"):
            g.validate(dict(queries=[q]))
        q["kind"] = "misspelled"
        with self.assertRaisesRegex(ValueError, "unknown query kind"):
            g.validate(dict(queries=[q]))

    def test_selection_uses_quality_then_lower_gain_and_nearest_half_damping(self):
        q = query()
        rows = {}
        for arm in ["damped", "degree"]:
            for gain, damping in [(2, 0.5), (0.25, 0.25), (0.25, 0.5)]:
                v = variant(arm, gain, damping, [hit("answer-q")])
                rows[g.setting_key(v["setting"])] = [
                    dict(
                        id="q",
                        topic="one",
                        family="test",
                        split="dev",
                        kind="answerable",
                        graph_only_candidates=2,
                        **g.measure(q, v),
                    )
                ]
        chosen = g.select(rows)
        for s in chosen.values():
            self.assertEqual((s["gain"], s["damping"]), (0.25, 0.5))
        for rs in rows.values():
            rs[0]["split"] = "held_out"
        with self.assertRaisesRegex(ValueError, "development"):
            g.select(rows)

    def test_run_refuses_missing_queries_and_mixed_splits(self):
        labels = dict(queries=[query(), query("b", topic="two")])
        run = dict(
            queries=[dict(id="q", variants=[variant()], graph_only_candidates=0)]
        )
        with self.assertRaisesRegex(ValueError, "full labelled split"):
            g.score_rows(labels, run, "dev")
        run["queries"].append(
            dict(id="b", variants=[variant()], graph_only_candidates=0)
        )
        self.assertEqual(len(g.score_rows(labels, run, "dev")), 1)
        with self.assertRaisesRegex(ValueError, "mixes splits"):
            g.score_rows(labels, run, "held_out")

    def test_topic_bootstrap_is_paired_and_seeded(self):
        base = [
            dict(id="a", topic="one", kind="answerable", known_facet_coverage=0),
            dict(id="b", topic="two", kind="answerable", known_facet_coverage=1),
        ]
        other = [dict(r, known_facet_coverage=1) for r in base]
        a = g.paired_topic_interval(base, other, draws=100)
        self.assertEqual(a, g.paired_topic_interval(base, other, draws=100))
        self.assertEqual(a["mean"], 0.5)
        self.assertEqual(a["bootstrap_topic_clusters"], 2)

    def test_cli_refuses_held_out_selection_before_reading_inputs(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp)
            cmd = [
                sys.executable,
                str(Path(g.__file__)),
                "--labels",
                str(p / "absent"),
                "--run",
                str(p / "absent-run"),
                "--split",
                "held_out",
                "--select",
                str(p / "selection"),
                "--out",
                str(p / "result"),
            ]
            r = subprocess.run(cmd, capture_output=True)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn(b"never select", r.stderr)
            self.assertFalse((p / "result").exists())


if __name__ == "__main__":
    unittest.main()
