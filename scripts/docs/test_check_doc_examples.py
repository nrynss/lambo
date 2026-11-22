#!/usr/bin/env python3
"""Tests for check-doc-examples.py.

  python3 -I scripts/docs/test_check_doc_examples.py
"""

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_doc_examples", HERE / "check-doc-examples.py")
cde = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cde)

ORACLE = {
    "model": "Demo",
    "functions": [
        {
            "lean": "Lambo.Demo.stage1Passes",
            "rust": "demo",
            "cases": [
                {"input": [10, 3], "output": True},
                {"input": [2, 2], "output": False},
            ],
        },
        {
            "lean": "Lambo.Demo.gate",
            "rust": "demo",
            "cases": [{"input": ["none", "candidate", "venerable"], "output": None}],
        },
    ],
}

PASSING = """\
Some prose.

{/* oracle: Demo.json stage1Passes#1 stage1Passes#0 */}

| GC sweeps survived | Dependent interactions | Stage 1 |
|---|---|---|
| 2 | **2** | fails |
| 10 | 3 | `passes` |

{/* oracle: Demo.json gate#0 */}

| Status | From | To | Result |
|:--|:--|:--|:--|
| None | Candidate | Venerable | refused |
"""

MISMATCHING = """\
{/* oracle: Demo.json stage1Passes#1 */}

| GC sweeps survived | Dependent interactions | Stage 1 |
|---|---|---|
| 3 | 2 | fails |
"""


class CheckDocExamples(unittest.TestCase):
    def run_check(self, page_text):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "formal" / "oracle").mkdir(parents=True)
            (root / "formal" / "oracle" / "Demo.json").write_text(json.dumps(ORACLE))
            docs = root / "site" / "src" / "content" / "docs"
            docs.mkdir(parents=True)
            page = docs / "page.mdx"
            page.write_text(page_text)
            return cde.check(root, [page])

    def test_matching_tables_pass(self):
        errors, anchors, cases = self.run_check(PASSING)
        self.assertEqual(errors, [])
        self.assertEqual((anchors, cases), (2, 3))

    def test_a_wrong_cell_is_reported(self):
        errors, anchors, _ = self.run_check(MISMATCHING)
        self.assertEqual(anchors, 1)
        self.assertEqual(len(errors), 1)
        self.assertIn("site/src/content/docs/page.mdx:5: Demo.json stage1Passes#1", errors[0])
        self.assertIn("column 1 ('GC sweeps survived') shows '3'", errors[0])
        self.assertIn("input 1 is 2", errors[0])


if __name__ == "__main__":
    unittest.main()
