"""Prevent another propagation evaluation with no labelled graph-only headroom."""

import json
from pathlib import Path
import subprocess
import tempfile
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import graph_screen as g


def question():
    return dict(
        id="q",
        query="question",
        kind="answerable",
        topic="topic",
        split="dev",
        facets=[dict(id="answer")],
        known_relevant=[dict(node="support", grade=2, facets=["answer"])],
    )


def frame(direct=("seed",), bfs=("seed", "support")):
    return dict(
        id="q", phase1=[dict(item=n, score=1) for n in direct], bfs_ids=list(bfs)
    )


class Headroom(unittest.TestCase):
    def test_graph_only_requires_labelled_support_inside_bfs(self):
        labels = dict(queries=[question()])
        row = g.screen(labels, dict(queries=[frame()]))[0]
        self.assertEqual(row["graph_only_facets"], ["answer"])
        self.assertEqual(g.aggregate([row])["graph_only_topics"], 1)
        row = g.screen(labels, dict(queries=[frame(bfs=("seed",))]))[0]
        self.assertEqual(row["category"], "unreachable_known_facets")

    def test_equivalent_direct_answer_removes_graph_only_headroom(self):
        q = question()
        q["known_relevant"].append(dict(node="equivalent", grade=2, facets=["answer"]))
        row = g.screen(
            dict(queries=[q]),
            dict(
                queries=[
                    frame(
                        direct=("seed", "equivalent"),
                        bfs=("seed", "equivalent", "support"),
                    )
                ]
            ),
        )[0]
        self.assertEqual(row["category"], "all_known_facets_direct")
        self.assertEqual(row["graph_only_facets"], [])
        q["graph_only_facets"] = ["answer"]
        with self.assertRaisesRegex(ValueError, "declared graph-only"):
            g.screen(
                dict(queries=[q]),
                dict(
                    queries=[
                        frame(
                            direct=("seed", "equivalent"),
                            bfs=("seed", "equivalent", "support"),
                        )
                    ]
                ),
            )

    def test_mixed_question_preserves_a_genuinely_missing_facet(self):
        q = question()
        q["facets"].append(dict(id="other"))
        q["known_relevant"].append(dict(node="seed", grade=2, facets=["other"]))
        row = g.screen(dict(queries=[q]), dict(queries=[frame()]))[0]
        self.assertEqual(row["direct_facets"], ["other"])
        self.assertEqual(row["graph_only_facets"], ["answer"])

    def test_missing_duplicate_or_unknown_questions_are_refused(self):
        labels = dict(queries=[question()])
        for frames in [[], [frame(), frame()], [dict(frame(), id="unknown")]]:
            with self.assertRaises(ValueError):
                g.screen(labels, dict(queries=frames))
        with self.assertRaisesRegex(ValueError, "omits a direct seed"):
            g.screen(labels, dict(queries=[frame(bfs=("support",))]))

    def test_cli_refuses_a_private_key_in_repository_before_reading_inputs(self):
        with tempfile.TemporaryDirectory() as tmp:
            command = [
                sys.executable,
                str(Path(g.__file__)),
                "--labels",
                "absent",
                "--pool",
                "absent",
                "--out",
                str(Path(tmp) / "report"),
                "--key",
                str(Path(g.__file__).resolve().parent / "private-key.json"),
            ]
            result = subprocess.run(command, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"per-query key must stay outside", result.stderr)
            self.assertFalse((Path(tmp) / "report").exists())

    def test_cli_refuses_pool_from_different_labels_or_snapshot(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            labels = root / "labels.json"
            labels.write_text(
                json.dumps(dict(queries=[question()], snapshot_sha256="frozen"))
            )
            pool = root / "pool.json"
            command = [
                sys.executable,
                str(Path(g.__file__)),
                "--labels",
                str(labels),
                "--pool",
                str(pool),
                "--out",
                str(root / "report"),
            ]
            for label_hash, snapshot, message in [
                ("wrong", "frozen", b"labels hash differs"),
                (g.digest(labels), "wrong", b"snapshot differs"),
            ]:
                pool.write_text(
                    json.dumps(
                        dict(
                            queries=[frame()],
                            dataset_sha256=label_hash,
                            snapshot_sha256=snapshot,
                        )
                    )
                )
                result = subprocess.run(command, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertFalse((root / "report").exists())

    def test_no_answer_control_does_not_become_a_graph_opportunity(self):
        q = question()
        q.update(kind="no_answer", facets=[], known_relevant=[])
        row = g.screen(dict(queries=[q]), dict(queries=[frame()]))[0]
        self.assertEqual(row["category"], "no_answer_control")
        self.assertEqual(row["graph_only_facets"], [])


if __name__ == "__main__":
    unittest.main()
