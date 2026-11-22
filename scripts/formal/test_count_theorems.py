#!/usr/bin/env python3
"""Tests for count-theorems.py.

  python3 -I scripts/formal/test_count_theorems.py
"""

import importlib.util
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("count_theorems", HERE / "count-theorems.py")
ct = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ct)

LEAN = """\
/-! Module doc.
theorem inside_a_block_comment : True := trivial
-/
namespace Demo

/-- A doc comment whose second line starts with the word
theorem above stays a statement about the old version. -/
theorem plain (n : Nat) : n = n := rfl

private theorem hidden : True := trivial

@[simp] theorem with_attr : 1 = 1 := rfl

@[simp] private theorem attr_and_private : 2 = 2 := rfl

theorem name_only
    (h : True) : True := h

theorem colon_next:True := trivial

-- theorem in_a_line_comment : True := trivial

/- nested /- block -/ still a comment
theorem inside_nested : True := trivial -/

def s : String := "theorem in_a_string : True"

example : True := trivial

  theorem indented_is_not_at_column_zero : True := trivial

end Demo
"""


class CountTheorems(unittest.TestCase):
    def test_sample(self):
        # plain, hidden, with_attr, attr_and_private, name_only, colon_next
        self.assertEqual(ct.count(LEAN), 6)

    def test_prose_line_not_counted(self):
        self.assertEqual(ct.count("/-- doc\ntheorem above stays -/\n"), 0)

    def test_private_theorem(self):
        self.assertEqual(ct.count("private theorem p : True := trivial\n"), 1)

    def test_attribute_before_theorem(self):
        self.assertEqual(ct.count("@[simp, grind =] theorem a : 1 = 1 := rfl\n"), 1)

    def test_name_only_line(self):
        self.assertEqual(ct.count("theorem n\n    (h : True) : True := h\n"), 1)

    def test_strip_keeps_lines(self):
        src = "/- a\nb -/\ntheorem t : True := trivial\n"
        self.assertEqual(ct.strip_comments(src).count("\n"), src.count("\n"))

    def test_main_skips_audit(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d) / "formal" / "LamboProofs"
            root.mkdir(parents=True)
            (root / "A.lean").write_text("theorem a : True := trivial\n", encoding="utf-8")
            (root / "Audit.lean").write_text("theorem b : True := trivial\n", encoding="utf-8")
            self.assertEqual([p.name for p in ct.model_files(d)], ["A.lean"])
            self.assertEqual(ct.main(["--repo", d]), 0)

    def test_char_literal_quote_opens_no_string(self):
        src = (
            "def q : Char := '\"'\n"
            "theorem a : True := trivial\n"
            "theorem b' : True := trivial\n"
            "def h' := 1\n"
            "theorem c : True := trivial\n"
        )
        self.assertEqual(ct.count(src), 3)

    def test_raw_string_is_consumed_whole(self):
        # The quote inside r#"a"b"# does not end the string, and the closing
        # "# does not open a new one.
        src = 'def s := r#"a"b"#\ntheorem actual : True := trivial\n'
        self.assertEqual(ct.count(src), 1)

    def test_raw_string_has_no_escapes(self):
        # In r"x\" the backslash escapes nothing: the string ends at the quote.
        self.assertEqual(ct.count('def a := r"x\\"\ntheorem t1 : True := trivial\n'), 1)

    def test_raw_string_ends_only_at_matching_hashes(self):
        # "# does not close r##"..."##, so the theorem line inside is string text.
        src = (
            'def b := r##"x"#\ntheorem inside : True := trivial\n"##\n'
            "theorem t2 : True := trivial\n"
        )
        self.assertEqual(ct.count(src), 1)

    def test_check_readme(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d) / "formal" / "LamboProofs"
            root.mkdir(parents=True)
            (root / "A.lean").write_text("theorem a : True := trivial\n", encoding="utf-8")
            readme = Path(d) / "formal" / "README.md"
            readme.write_text("It has 1 theorems across\n1 model files.\n", encoding="utf-8")
            self.assertEqual(ct.main(["--repo", d, "--check-readme"]), 0)
            readme.write_text("It has 2 theorems across 1 model files.\n", encoding="utf-8")
            self.assertEqual(ct.main(["--repo", d, "--check-readme"]), 1)
            readme.write_text("No count here.\n", encoding="utf-8")
            self.assertEqual(ct.main(["--repo", d, "--check-readme"]), 1)


if __name__ == "__main__":
    unittest.main()
