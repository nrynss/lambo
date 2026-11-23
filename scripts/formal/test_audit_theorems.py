#!/usr/bin/env python3
"""Tests for audit-theorems.py, over a hand-made model and dump (no Lean needed).

  python3 -I scripts/formal/test_audit_theorems.py
"""

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("audit_theorems", HERE / "audit-theorems.py")
at = importlib.util.module_from_spec(spec)
spec.loader.exec_module(at)

# Each theorem's name is on the line given in DECLS below.
MODEL = """\
/-
# Model M

## Results

* `headline`: the result.
-/

namespace Lambo.M

def f (n : Nat) : Nat := n

theorem headline : True := trivial

theorem f_zero : f 0 = 0 := rfl

theorem old_rule (n : Nat) : f n = n := rfl

theorem old_ruleW (n : Nat) : f n = n := rfl

theorem audited_full (n : Nat) : f n = n := rfl

theorem audited_short (n : Nat) : f n = n := rfl

theorem twin_used (n : Nat) : n = n := rfl

theorem twin_unused (m : Nat) : m = m := rfl

/-- Not cited anywhere. -/
theorem documented (n : Nat) : n ≤ n + 1 := Nat.le_succ n

@[simp] theorem f_eq (n : Nat) : f n = n := rfl

namespace Sub
theorem nested_lemma (n : Nat) : n + 0 = n := by simp
end Sub

theorem user (n : Nat) : n = n := twin_used n

end Lambo.M
"""

AUDIT = """\
import LamboProofs.M
#print axioms Lambo.M.audited_full
#print axioms M.audited_short
#print axioms M.old_ruleW
"""


def decl(name, line, type_, shape=None, uses=()):
    return {"name": f"Lambo.M.{name}", "private": False, "module": "LamboProofs.M", "kind": "theorem",
            "user": True, "line": line, "span": [line, line], "levels": [], "type": type_,
            "shape": shape or type_, "value": None, "uses": list(uses), "axioms": []}


DECLS = [
    decl("headline", 13, "True"),
    decl("f_zero", 15, "Eq Nat (Lambo.M.f 0) 0"),
    decl("old_rule", 17, "forall (n : Nat), Eq (f n) n", "S1"),
    decl("old_ruleW", 19, "forall (n : Nat), Eq (f n) n", "S2"),
    decl("audited_full", 21, "forall (n : Nat), Eq (f n) n", "S3"),
    decl("audited_short", 23, "forall (n : Nat), Eq (f n) n", "S4"),
    decl("twin_used", 25, "forall (n : Nat), Eq n n", "TWIN"),
    decl("twin_unused", 27, "forall (m : Nat), Eq m m", "TWIN"),
    dict(decl("documented", 30, "forall (n : Nat), LE n (n+1)", "S5"), span=[29, 30]),
    decl("f_eq", 32, "forall (n : Nat), Eq (f n) n", "S6"),
    decl("Sub.nested_lemma", 35, "forall (n : Nat), Eq (n + 0) n", "S7"),
    decl("user", 38, "forall (n : Nat), Eq n n", "S8", uses=["Lambo.M.twin_used"]),
]


class Audit(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        (root / "formal" / "LamboProofs").mkdir(parents=True)
        (root / "formal" / "claims").mkdir()
        (root / "formal" / "LamboProofs" / "M.lean").write_text(MODEL, encoding="utf-8")
        (root / "formal" / "LamboProofs" / "Audit.lean").write_text(AUDIT, encoding="utf-8")
        (root / "formal" / "README.md").write_text("# Proofs\n", encoding="utf-8")
        self.dump = root / "dump.jsonl"
        self.dump.write_text("".join(json.dumps(d) + "\n" for d in DECLS), encoding="utf-8")
        self.root = root

    def report(self):
        out = self.root / "report.md"
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(at.main(["--repo", str(self.root), "--dump", str(self.dump), "--out", str(out)]), 0)
        return out.read_text(encoding="utf-8")

    def row(self, text, name):
        rows = [l for l in text.splitlines() if l.startswith("- (") and f"`{name}`" in l]
        self.assertEqual(len(rows), 1, (name, rows))
        return rows[0]

    def test_audit_lines_count_with_or_without_the_lambo_prefix(self):
        refs = at.audit_refs(at.Models(self.root, self.dump))
        self.assertIn("Lambo.M.audited_full", refs)
        self.assertIn("Lambo.M.audited_short", refs)
        text = self.report()
        self.assertNotIn("`audited_full`", text)
        self.assertNotIn("`audited_short`", text)

    def test_classes(self):
        text = self.report()
        self.assertIn("(a)", self.row(text, "headline"))
        self.assertTrue(self.row(text, "f_zero").startswith("- (a)"))  # a closed concrete case
        # Superseded: the referenced `old_ruleW` stays.
        self.assertTrue(self.row(text, "old_rule").startswith("- (b)"))
        self.assertIn("superseded by `old_ruleW`", self.row(text, "old_rule"))
        # The same statement as a referenced theorem.
        self.assertTrue(self.row(text, "twin_unused").startswith("- (b)"))
        self.assertIn("states the same as `twin_used`", self.row(text, "twin_unused"))
        # Documented, cited nowhere: unclear, not a deletion candidate.
        self.assertTrue(self.row(text, "documented").startswith("- (c)"))
        # simp may use it without naming it.
        self.assertTrue(self.row(text, "f_eq").startswith("- (c)"))
        # An undocumented lemma that is not an `rfl` unfolding is unclear.
        self.assertTrue(self.row(text, "Sub.nested_lemma").startswith("- (c)"))
        # Referenced by a proof: not listed.
        self.assertNotIn("`twin_used` (line", text)

    def test_names_are_qualified_below_the_model_namespace(self):
        text = self.report()
        self.assertIn("`Sub.nested_lemma`", text)
        self.assertNotIn("- (c) `nested_lemma`", text)

    def test_dead_rfl_helper_is_b(self):
        model = MODEL.replace("theorem old_ruleW (n : Nat) : f n = n := rfl",
                              "theorem old_ruleV (n : Nat) : f n = n := rfl")
        (self.root / "formal" / "LamboProofs" / "M.lean").write_text(model, encoding="utf-8")
        decls = [dict(d, name="Lambo.M.old_ruleV") if d["name"] == "Lambo.M.old_ruleW" else d for d in DECLS]
        self.dump.write_text("".join(json.dumps(d) + "\n" for d in decls), encoding="utf-8")
        row = self.row(self.report(), "old_rule")
        self.assertTrue(row.startswith("- (b)"), row)
        self.assertIn("dead helper", row)


if __name__ == "__main__":
    unittest.main()
