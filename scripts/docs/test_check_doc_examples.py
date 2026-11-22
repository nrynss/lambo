#!/usr/bin/env python3
"""Tests for check-doc-examples.py.

  python3 -I scripts/docs/test_check_doc_examples.py
"""

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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
        {
            "lean": "Lambo.Demo.lift",
            "rust": "demo",
            "cases": [{"input": None, "output": None}],
        },
    ],
}

COLUMNS = {
    ("Demo.json", "stage1Passes"): (
        ("GC sweeps survived", cde.NONE),
        ("Dependent interactions", cde.NONE),
        ("Stage 1", cde.PASSES),
    ),
    ("Demo.json", "gate"): (
        ("Status", cde.NONE),
        ("From", cde.NONE),
        ("To", cde.NONE),
        ("Result", cde.REFUSED),
    ),
    ("Demo.json", "lift"): (
        ("Input", cde.NONE),
        ("Output", {None: {"never"}}),
    ),
}

STAGE1_HEADER = "| GC sweeps survived | Dependent interactions | Stage 1 |\n|---|---|---|\n"

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


def page_with(anchor, header=STAGE1_HEADER, rows="| 2 | 2 | fails |\n", between="\n"):
    return f"{anchor}\n{between}{header}{rows}"


class Fixture(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch.dict(cde.COLUMNS, COLUMNS)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "formal" / "oracle").mkdir(parents=True)
        self.write_oracle(json.dumps(ORACLE))
        self.docs = self.root / "site" / "src" / "content" / "docs"
        self.docs.mkdir(parents=True)

    def write_oracle(self, text, name="Demo.json"):
        (self.root / "formal" / "oracle" / name).write_text(text, encoding="utf-8")

    def page(self, text, name="page.mdx"):
        path = self.docs / name
        if isinstance(text, bytes):
            path.write_bytes(text)
        else:
            path.write_text(text, encoding="utf-8")
        return path

    def run_check(self, text):
        return cde.check(self.root, [self.page(text)])

    def errors(self, text):
        return self.run_check(text)[0]

    def main(self, *pages):
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            code = cde.main(["--root", str(self.root), *map(str, pages)])
        return code, err.getvalue()


class Matching(Fixture):
    def test_matching_tables_pass(self):
        errors, anchors, cases = self.run_check(PASSING)
        self.assertEqual(errors, [])
        self.assertEqual((anchors, cases), (2, 3))

    def test_a_wrong_cell_is_reported(self):
        errors = self.errors(page_with("{/* oracle: Demo.json stage1Passes#1 */}", rows="| 3 | 2 | fails |\n"))
        self.assertEqual(len(errors), 1)
        self.assertIn("site/src/content/docs/page.mdx:5: Demo.json stage1Passes#1", errors[0])
        self.assertIn("column 1 ('GC sweeps survived') shows '3'", errors[0])
        self.assertIn("input 1 is 2", errors[0])

    def test_row_count_mismatch(self):
        errors = self.errors(page_with(
            "{/* oracle: Demo.json stage1Passes#1 stage1Passes#0 */}", rows="| 2 | 2 | fails |\n"
        ))
        self.assertEqual(len(errors), 1)
        self.assertIn("names 2 case(s) but the table has 1 data row(s)", errors[0])

    def test_a_short_row_is_reported(self):
        errors = self.errors(page_with("{/* oracle: Demo.json stage1Passes#1 */}", rows="| 2 | fails |\n"))
        self.assertEqual(len(errors), 1)
        self.assertIn("the row has 2 cell(s)", errors[0])

    def test_prose_between_anchor_and_table(self):
        errors = self.errors(page_with(
            "{/* oracle: Demo.json stage1Passes#1 */}", between="\nA sentence in the way.\n\n"
        ))
        # The table, cut off from its anchor, is also an unanchored example.
        self.assertEqual(len(errors), 2)
        self.assertIn("no table (header and separator rows) follows the anchor", errors[0])
        self.assertIn("no oracle anchor is directly above it", errors[1])

    def test_unknown_case_index(self):
        errors = self.errors(page_with("{/* oracle: Demo.json stage1Passes#9 */}"))
        self.assertEqual(len(errors), 1)
        self.assertIn("has 2 cases, so no case #9", errors[0])

    def test_unknown_function(self):
        errors = self.errors(page_with("{/* oracle: Demo.json stage9Passes#0 */}"))
        self.assertEqual(len(errors), 1)
        self.assertIn("0 functions named 'stage9Passes'", errors[0])

    def test_malformed_case_id(self):
        errors = self.errors(page_with("{/* oracle: Demo.json stage1Passes */}"))
        self.assertEqual(len(errors), 1)
        self.assertIn("is not `<function>#<index>`", errors[0])

    def test_missing_oracle_file_is_a_page_error(self):
        errors = self.errors(page_with("{/* oracle: Nope.json stage1Passes#0 */}"))
        self.assertEqual(len(errors), 1)
        self.assertIn("no oracle file formal/oracle/Nope.json", errors[0])

    def test_anchor_not_alone_on_its_line(self):
        errors = self.errors(page_with("Text {/* oracle: Demo.json stage1Passes#1 */}"))
        self.assertEqual(errors, [
            "site/src/content/docs/page.mdx:1: an oracle anchor must be alone on its line",
            "site/src/content/docs/page.mdx:3: this table's header is an oracle function's labels, "
            "but no oracle anchor is directly above it",
        ])


class Labels(Fixture):
    def test_swapped_labels_fail(self):
        header = "| Dependent interactions | GC sweeps survived | Stage 1 |\n|---|---|---|\n"
        errors = self.errors(page_with("{/* oracle: Demo.json stage1Passes#1 */}", header=header))
        self.assertEqual(len(errors), 1)
        self.assertIn("the header is ['Dependent interactions', 'GC sweeps survived', 'Stage 1']", errors[0])

    def test_emphasis_around_a_label_is_allowed(self):
        header = "| **GC sweeps survived** | Dependent interactions | `Stage 1` |\n|---|---|---|\n"
        self.assertEqual(self.errors(page_with("{/* oracle: Demo.json stage1Passes#1 */}", header=header)), [])

    def test_a_function_without_labels_fails(self):
        with mock.patch.dict(cde.COLUMNS, clear=True):
            errors = self.errors(page_with("{/* oracle: Demo.json stage1Passes#1 */}"))
        self.assertEqual(len(errors), 1)
        self.assertIn("no column labels for this function", errors[0])

    def test_one_anchor_one_function(self):
        errors = self.errors(page_with("{/* oracle: Demo.json stage1Passes#1 gate#0 */}"))
        self.assertEqual(len(errors), 1)
        self.assertIn("one anchor names one function's cases", errors[0])


class Unanchored(Fixture):
    def test_a_labelled_table_without_its_anchor_fails(self):
        errors, anchors, _ = self.run_check("Prose.\n\n" + STAGE1_HEADER + "| 2 | 2 | fails |\n")
        self.assertEqual(anchors, 0)
        self.assertEqual(errors, [
            "site/src/content/docs/page.mdx:3: this table's header is an oracle function's labels, "
            "but no oracle anchor is directly above it"
        ])

    def test_losing_one_of_two_anchors_fails(self):
        text = PASSING.replace("{/* oracle: Demo.json gate#0 */}\n", "")
        errors, anchors, _ = self.run_check(text)
        self.assertEqual(anchors, 1)
        self.assertEqual(len(errors), 1)
        self.assertIn("no oracle anchor is directly above it", errors[0])

    def test_another_comment_above_does_not_count(self):
        errors = self.errors("{/* lean: Demo.stage1Passes */}\n\n" + STAGE1_HEADER + "| 2 | 2 | fails |\n")
        self.assertEqual(len(errors), 1)

    def test_other_tables_need_no_anchor(self):
        self.assertEqual(self.errors("| Setting | Default |\n|---|---|\n| a | 1 |\n"), [])


class Vocabulary(Fixture):
    def stage1(self, word):
        return self.errors(page_with("{/* oracle: Demo.json stage1Passes#0 */}", rows=f"| 10 | 3 | {word} |\n"))

    def test_the_column_word_matches(self):
        self.assertEqual(self.stage1("passes"), [])
        self.assertEqual(self.stage1("**Passes**"), [])

    def test_other_true_words_do_not(self):
        for word in ["true", "yes", "refused", "never", "fails"]:
            with self.subTest(word=word):
                self.assertEqual(len(self.stage1(word)), 1)

    def test_null_words_are_per_column(self):
        header = "| Status | From | To | Result |\n|---|---|---|---|\n"
        for word, ok in [("refused", True), ("never", False), ("null", False), ("fails", False)]:
            with self.subTest(word=word):
                errors = self.errors(page_with(
                    "{/* oracle: Demo.json gate#0 */}", header=header,
                    rows=f"| None | Candidate | Venerable | {word} |\n",
                ))
                self.assertEqual(errors == [], ok, errors)

    def test_null_in_an_input_column_needs_that_columns_word(self):
        header = "| Input | Output |\n|---|---|\n"
        anchor = "{/* oracle: Demo.json lift#0 */}"
        errors = self.errors(page_with(anchor, header=header, rows="| refused | never |\n"))
        self.assertEqual(len(errors), 1)
        self.assertIn("column 1 ('Input') shows 'refused', but the case's input 1 is null", errors[0])
        errors = self.errors(page_with(anchor, header=header, rows="| never | refused |\n"))
        self.assertEqual(len(errors), 2)
        self.assertIn("column 2 ('Output') shows 'refused'", errors[1])


class NearMisses(Fixture):
    def test_near_miss_anchors_fail(self):
        for anchor in [
            "{/* oracle : Demo.json stage1Passes#1 */}",
            "{/*oracle: Demo.json stage1Passes#1 */}",
            "{/* Oracle: Demo.json stage1Passes#1 */}",
            "{ /* oracle: Demo.json stage1Passes#1 */ }",
            "{/* oracle: Demo.json stage1Passes#1*/}",
            "{/* oracle: Demo.json\nstage1Passes#1 */}",
        ]:
            with self.subTest(anchor=anchor):
                errors, anchors, _ = self.run_check(page_with(anchor))
                self.assertEqual(anchors, 0)
                # The near miss, and the table it leaves unanchored.
                self.assertEqual(len(errors), 2, errors)
                self.assertIn("starts like an oracle anchor", errors[0])
                self.assertIn("no oracle anchor is directly above it", errors[1])

    def test_other_comments_are_left_alone(self):
        errors, anchors, _ = self.run_check("{/* lambo-site-only:start */}\n{/* lean: Canon.gate */}\n")
        self.assertEqual((errors, anchors), ([], 0))


class ExitCodes(Fixture):
    def test_pass_is_0(self):
        self.assertEqual(self.main(self.page(PASSING))[0], 0)

    def test_mismatch_is_1(self):
        code, err = self.main(self.page(page_with("{/* oracle: Demo.json stage1Passes#1 */}", rows="| 3 | 2 | fails |\n")))
        self.assertEqual(code, 1)
        self.assertIn("1 error(s) in 1 oracle anchor(s)", err)

    def test_missing_page_is_2(self):
        self.assertEqual(self.main(self.docs / "nope.mdx")[0], 2)

    def test_bad_arguments_are_2(self):
        self.assertEqual(self.main("--no-such-flag")[0], 2)

    def test_non_utf8_page_is_3(self):
        code, err = self.main(self.page(b"\xff\xfe not text"))
        self.assertEqual(code, 3)
        self.assertIn("not UTF-8", err)

    def test_malformed_oracles_are_3(self):
        page = self.page(PASSING)
        for text, says in [
            ("not json", "is not JSON"),
            ("[]", "has no `functions` list"),
            ('{"functions": [{"cases": []}]}', "has no `lean` name"),
            ('{"functions": [{"lean": "Lambo.Demo.gate"}]}', "has no `cases` list"),
            ('{"functions": [{"lean": "Lambo.Demo.gate", "cases": [{"input": []}]}]}', "lacks `input` or `output`"),
        ]:
            with self.subTest(text=text):
                self.write_oracle(text)
                code, err = self.main(page)
                self.assertEqual(code, 3)
                self.assertIn(says, err)

    def test_default_pages_include_the_reference_copies(self):
        ref = self.root / "docs" / "reference"
        ref.mkdir(parents=True)
        (ref / "page.mdx").write_text(page_with("{/* oracle: Demo.json stage1Passes#1 */}", rows="| 3 | 2 | fails |\n"), encoding="utf-8")
        code, err = self.main()
        self.assertEqual(code, 1)
        self.assertIn("docs/reference/page.mdx:5", err)


if __name__ == "__main__":
    unittest.main()
