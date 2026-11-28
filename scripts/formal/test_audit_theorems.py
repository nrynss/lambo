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


REVIEWED = """\
# name, verdict, reason
Lambo.M.headline\tkeep\tthe header's result
Lambo.M.f_zero\tkeep\ta worked example
Lambo.M.documented\tnarrowed\tits doc says what it proves (#1)
"""


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
        (root / "formal" / "audit").mkdir()
        self.dump = root / "dump.jsonl"
        self.root = root
        self.write_reviewed(REVIEWED)
        self.write_dump(DECLS)

    def write_reviewed(self, text):
        (self.root / "formal" / "audit" / "reviewed-theorems.tsv").write_text(text, encoding="utf-8")

    def check(self):
        with contextlib.redirect_stderr(io.StringIO()) as err, contextlib.redirect_stdout(io.StringIO()):
            code = at.main(["--repo", str(self.root), "--dump", str(self.dump), "--check"])
        return code, err.getvalue()

    def write_dump(self, decls):
        src = (self.root / "formal" / "LamboProofs" / "M.lean").read_bytes()
        record = {"record": "module", "module": "LamboProofs.M", "fnv1a64": at.fnv1a64(src)}
        self.dump.write_text("".join(json.dumps(d) + "\n" for d in decls) + json.dumps(record) + "\n",
                             encoding="utf-8")

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
        # Documented, cited nowhere, and reviewed against the rule: kept.
        self.assertTrue(self.row(text, "documented").startswith("- (a)"))
        self.assertIn("its doc comment narrowed: its doc says what it proves (#1)", self.row(text, "documented"))
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

    def test_fnv1a64_matches_the_reference_values(self):
        self.assertEqual(at.fnv1a64(b""), "cbf29ce484222325")
        self.assertEqual(at.fnv1a64(b"a"), "af63dc4c8601ec8c")

    def test_a_stale_dump_is_refused(self):
        # Same declaration lines, changed text: only the hash can tell.
        (self.root / "formal" / "LamboProofs" / "M.lean").write_text(
            MODEL.replace("the result.", "the result, reworded."), encoding="utf-8")
        with contextlib.redirect_stderr(io.StringIO()) as err:
            code = at.main(["--repo", str(self.root), "--dump", str(self.dump)])
        self.assertEqual(code, 1)
        self.assertIn("older than the sources", err.getvalue())
        # A dump with no module records cannot be checked.
        self.dump.write_text("".join(json.dumps(d) + "\n" for d in DECLS), encoding="utf-8")
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(at.main(["--repo", str(self.root), "--dump", str(self.dump)]), 1)

    def test_check_fails_on_an_unused_undocumented_theorem(self):
        with contextlib.redirect_stderr(io.StringIO()) as err, contextlib.redirect_stdout(io.StringIO()):
            code = at.main(["--repo", str(self.root), "--dump", str(self.dump), "--check"])
        self.assertEqual(code, 1)
        # Undocumented and unused: headline (named in the header, still no doc
        # comment), f_zero and others; `documented` has one, `twin_used` is used.
        self.assertIn("M.lean:13: headline", err.getvalue())
        self.assertNotIn(": documented ", err.getvalue())
        self.assertNotIn(": twin_used ", err.getvalue())

    def test_an_unreviewed_documented_theorem_is_c_and_fails_check(self):
        self.write_reviewed(REVIEWED.replace("Lambo.M.documented\tnarrowed\tits doc says what it proves (#1)\n", ""))
        row = self.row(self.report(), "documented")
        self.assertTrue(row.startswith("- (c)"), row)
        self.assertIn("not yet reviewed", row)
        code, err = self.check()
        self.assertEqual(code, 1)
        self.assertIn("M.lean:30: documented is referenced by nothing and its doc comment is not reviewed", err)

    def test_a_reviewed_theorem_passes_the_review_rule(self):
        code, err = self.check()
        self.assertEqual(code, 1)  # the undocumented ones still fail
        self.assertNotIn("documented is referenced by nothing", err)
        self.assertNotIn("stale", err.split("theorem(s)")[0])

    def test_stale_review_rows_fail(self):
        self.write_reviewed(REVIEWED + "Lambo.M.gone\tkeep\tonce kept\n"
                            "Lambo.M.twin_unused\tdeleted\ta twin\n")
        code, err = self.check()
        self.assertEqual(code, 1)
        self.assertIn("Lambo.M.gone is reviewed as keep but no model declares it", err)
        self.assertIn("Lambo.M.twin_unused is reviewed as deleted but still declared", err)
        self.assertIn("Lambo.M.twin_unused is reviewed as deleted but not listed", err)
        # A verdict outside the three is refused outright.
        self.write_reviewed("Lambo.M.headline\tmaybe\tunsure\n")
        with self.assertRaises(SystemExit):
            self.check()

    def test_dead_rfl_helper_is_b(self):
        model = MODEL.replace("theorem old_ruleW (n : Nat) : f n = n := rfl",
                              "theorem old_ruleV (n : Nat) : f n = n := rfl")
        (self.root / "formal" / "LamboProofs" / "M.lean").write_text(model, encoding="utf-8")
        decls = [dict(d, name="Lambo.M.old_ruleV") if d["name"] == "Lambo.M.old_ruleW" else d for d in DECLS]
        self.write_dump(decls)
        row = self.row(self.report(), "old_rule")
        self.assertTrue(row.startswith("- (b)"), row)
        self.assertIn("dead helper", row)


SPEC = """\
/-
# Model L, specification

## Results

* `listed`: the result.

A line of prose that starts with the word
namespace is not where the header ends.
-/

namespace Lambo.L

def g (n : Nat) : Nat := n

/-- `g` is the identity, as the Rust's `g` is. -/
abbrev Spec.g_id : Prop := ∀ n, g n = n

abbrev Spec.listed : Prop := g 0 = 0

end Lambo.L
"""

PROOFS = """\
namespace Lambo.L

theorem g_id : Spec.g_id := fun _ => rfl

theorem listed : Spec.listed := rfl

end Lambo.L
"""


def ldecl(name, line, type_, module, kind="theorem", span=None, **more):
    return dict({"name": f"Lambo.L.{name}", "private": False, "module": module, "kind": kind, "user": True,
                 "line": line, "span": span or [line, line], "levels": [], "type": type_, "shape": type_,
                 "value": None, "uses": [], "axioms": [] if kind == "theorem" else None}, **more)


LDECLS = [
    ldecl("g", 14, "Nat -> Nat", "LamboProofs.L.Spec", kind="def"),
    ldecl("Spec.g_id", 17, "Prop", "LamboProofs.L.Spec", kind="def", span=[16, 17]),
    ldecl("Spec.listed", 19, "Prop", "LamboProofs.L.Spec", kind="def"),
    ldecl("g_id", 3, "Lambo.L.Spec.g_id", "LamboProofs.L.Proofs", spec="Lambo.L.Spec.g_id"),
    ldecl("listed", 5, "Lambo.L.Spec.listed", "LamboProofs.L.Proofs", spec="Lambo.L.Spec.listed"),
]


class SpecForm(unittest.TestCase):
    """A model in Spec and Proofs form: `L/Spec.lean` states, `L/Proofs.lean` proves."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = self.root = Path(self.tmp.name)
        (root / "formal" / "LamboProofs" / "L").mkdir(parents=True)
        for d in ("claims", "audit"):
            (root / "formal" / d).mkdir()
        (root / "docs" / "reference").mkdir(parents=True)
        (root / "formal" / "LamboProofs" / "L" / "Spec.lean").write_text(SPEC, encoding="utf-8")
        (root / "formal" / "LamboProofs" / "L" / "Proofs.lean").write_text(PROOFS, encoding="utf-8")
        (root / "formal" / "LamboProofs" / "Audit.lean").write_text("", encoding="utf-8")
        (root / "formal" / "README.md").write_text("# Proofs\n", encoding="utf-8")
        (root / "formal" / "audit" / "reviewed-theorems.tsv").write_text(
            "Lambo.L.g_id\tkeep\tits proposition's doc names the Rust\n"
            "Lambo.L.listed\tkeep\tthe header's result\n", encoding="utf-8")
        self.dump = root / "dump.jsonl"
        self.write_dump(LDECLS)

    def write_dump(self, decls):
        records = [{"record": "module", "module": f"LamboProofs.L.{p.stem}", "fnv1a64": at.fnv1a64(p.read_bytes())}
                   for p in sorted((self.root / "formal" / "LamboProofs" / "L").glob("*.lean"))]
        self.dump.write_text("".join(json.dumps(d) + "\n" for d in decls + records), encoding="utf-8")

    def check(self):
        with contextlib.redirect_stderr(io.StringIO()) as err, contextlib.redirect_stdout(io.StringIO()) as out:
            code = at.main(["--repo", str(self.root), "--dump", str(self.dump), "--check"])
        return code, err.getvalue() + out.getvalue()

    def test_a_theorems_doc_comment_is_its_propositions(self):
        m = at.Models(self.root, self.dump)
        self.assertIn("as the Rust's `g` is", m.doc("Lambo.L.g_id"))
        self.assertEqual(m.doc("Lambo.L.listed"), "")
        # `listed` has no doc comment anywhere and no proof uses it.
        code, out = self.check()
        self.assertEqual(code, 1)
        self.assertIn("L/Proofs.lean:5: listed is used by no proof and has no doc comment", out)
        self.assertNotIn("g_id is used by no proof", out)

    def test_the_results_listed_in_the_spec_header_count(self):
        m = at.Models(self.root, self.dump)
        self.assertIn("`listed`: the result", m.headers("Lambo.L.listed"))
        self.assertNotIn("def g", m.headers("Lambo.L.listed"))  # the header ends at the command

    def test_a_citation_of_the_proposition_is_a_citation_of_the_theorem(self):
        (self.root / "docs" / "reference" / "page.mdx").write_text(
            "It is the identity. {/* lean: L.Spec.g_id */}\n", encoding="utf-8")
        (self.root / "formal" / "claims" / "page.md").write_text(
            "| C-1 | PROVEN | `Spec.listed` in `L/Spec.lean` |\n", encoding="utf-8")
        m = at.Models(self.root, self.dump)
        self.assertIn("Lambo.L.g_id", at.anchor_refs(m))
        self.assertIn("Lambo.L.listed", at.ledger_refs(m))
        self.assertEqual(at.models_named("`x` in `L/Spec/Base.lean` and formal/LamboProofs/M/Proofs/A.lean"),
                         {"L", "M"})

    def test_a_proposition_nothing_proves_fails(self):
        self.write_dump(LDECLS + [ldecl("Spec.unproved", 19, "Prop", "LamboProofs.L.Spec", kind="def")])
        code, out = self.check()
        self.assertEqual(code, 1)
        self.assertIn("L/Spec.lean:19: Spec.unproved is stated and not proved: no `theorem unproved : "
                      "Spec.unproved`", out)

    def test_a_theorem_stated_as_another_names_proposition_fails(self):
        decls = [dict(d, type="Lambo.L.Spec.g_id", spec="Lambo.L.Spec.g_id") if d["name"] == "Lambo.L.listed"
                 else d for d in LDECLS]
        self.write_dump(decls)
        code, out = self.check()
        self.assertEqual(code, 1)
        self.assertIn("listed is stated as Spec.g_id, not as its own proposition Spec.listed", out)
        self.assertIn("Spec.listed is stated and not proved", out)

    def test_a_theorem_in_a_spec_file_fails(self):
        spec = SPEC.replace("abbrev Spec.listed : Prop := g 0 = 0", "theorem stray : g 0 = 0 := rfl -- no")
        (self.root / "formal" / "LamboProofs" / "L" / "Spec.lean").write_text(spec, encoding="utf-8")
        decls = [d for d in LDECLS if not d["name"].endswith("listed")]
        self.write_dump(decls + [ldecl("stray", 19, "Eq (g 0) 0", "LamboProofs.L.Spec")])
        code, out = self.check()
        self.assertEqual(code, 1)
        self.assertIn("L/Spec.lean:19: stray is a theorem in a Spec file", out)

    def test_a_proofs_lemma_no_proof_uses_fails_even_with_a_doc_comment(self):
        proofs = PROOFS.replace("end Lambo.L", "/-- One is one. -/\ntheorem helper : g 1 = 1 := rfl\n\nend Lambo.L")
        (self.root / "formal" / "LamboProofs" / "L" / "Proofs.lean").write_text(proofs, encoding="utf-8")
        helper = ldecl("helper", 8, "Eq (g 1) 1", "LamboProofs.L.Proofs", span=[7, 8])
        self.write_dump(LDECLS + [helper])
        code, out = self.check()
        self.assertEqual(code, 1)
        self.assertIn("L/Proofs.lean:8: helper is a lemma of a Proofs file that no proof outside", out)
        # A proof that uses it settles it.
        decls = [dict(d, uses=["Lambo.L.helper"]) if d["name"] == "Lambo.L.g_id" else d for d in LDECLS]
        self.write_dump(decls + [helper])
        code, out = self.check()
        self.assertNotIn("helper is a lemma", out)
        # A theorem in Spec form needs no proof to use it.
        self.assertNotIn("g_id is a lemma", out)

    def test_a_proofs_lemma_only_history_or_examples_uses_fails(self):
        proofs = PROOFS.replace("end Lambo.L", "/-- One is one. -/\ntheorem helper : g 1 = 1 := rfl\n\nend Lambo.L")
        (self.root / "formal" / "LamboProofs" / "L" / "Proofs.lean").write_text(proofs, encoding="utf-8")
        helper = ldecl("helper", 8, "Eq (g 1) 1", "LamboProofs.L.Proofs", span=[7, 8])
        for layer in ("History", "Examples"):
            (self.root / "formal" / "LamboProofs" / "L" / f"{layer}.lean").write_text(
                f"namespace Lambo.L\n\ntheorem old : g 1 = 1 := helper\n\nend Lambo.L\n", encoding="utf-8")
            old = ldecl("old", 3, "Eq (g 1) 1", f"LamboProofs.L.{layer}", uses=["Lambo.L.helper"])
            self.write_dump(LDECLS + [helper, old])
            code, out = self.check()
            self.assertEqual(code, 1)
            self.assertIn("L/Proofs.lean:8: helper is a lemma of a Proofs file that no proof outside "
                          "History.lean and Examples.lean uses", out)
            (self.root / "formal" / "LamboProofs" / "L" / f"{layer}.lean").unlink()
        # A proof of the Proofs layer that uses it settles it.
        decls = [dict(d, uses=["Lambo.L.helper"]) if d["name"] == "Lambo.L.g_id" else d for d in LDECLS]
        self.write_dump(decls + [helper])
        code, out = self.check()
        self.assertNotIn("helper is a lemma", out)

    def test_theorems_proving_one_statement_through_two_propositions_are_duplicates(self):
        # Each type names its own proposition, so the two `shape`s differ;
        # what they state is the propositions' bodies (`stmt_shape`), which
        # match each other and a theorem stating the same thing outright.
        decls = [dict(d, stmt_shape="Eq (g 0) 0") if d["name"] in ("Lambo.L.g_id", "Lambo.L.listed") else d
                 for d in LDECLS] + [ldecl("plain", 7, "Eq (g 0) 0", "LamboProofs.L.Proofs")]
        self.write_dump(decls)
        with contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()) as out:
            code = at.main(["--repo", str(self.root), "--dump", str(self.dump), "--duplicates"])
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue().split(), ["Lambo.L.g_id", "Lambo.L.listed", "Lambo.L.plain"])
        report = self.root / "report.md"
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(at.main(["--repo", str(self.root), "--dump", str(self.dump), "--out", str(report)]), 0)
        text = report.read_text(encoding="utf-8")
        self.assertIn("## Statements proved twice", text)
        self.assertIn("`L.g_id`", text.split("## Statements proved twice")[1])

    def test_a_proposition_declared_in_a_proofs_file_fails(self):
        self.write_dump(LDECLS + [ldecl("Spec.stray", 9, "Prop", "LamboProofs.L.Proofs", kind="def")])
        code, out = self.check()
        self.assertEqual(code, 1)
        self.assertIn("L/Proofs.lean:9: Spec.stray is a Spec proposition declared in a Proofs file", out)


if __name__ == "__main__":
    unittest.main()
