#!/usr/bin/env python3
"""Tests for check-decl-equivalence.py, over hand-made dumps (no Lean needed).

  python3 -I scripts/formal/test_check_decl_equivalence.py
"""

import contextlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_decl_equivalence", HERE / "check-decl-equivalence.py")
eq = importlib.util.module_from_spec(spec)
spec.loader.exec_module(eq)


def decl(name, kind="theorem", type_="True", value=None, module="LamboProofs.A", user=True,
         axioms=None, private=False, levels=None):
    return {
        "name": name, "private": private, "module": module, "kind": kind, "user": user,
        "line": 1, "span": [1, 1], "levels": levels or [], "type": type_, "value": value,
        "uses": [], "axioms": (axioms if axioms is not None else []) if kind == "theorem" else None,
    }


BASE = [
    decl("Lambo.A.t", type_="forall (n : Nat), Eq.{1} Nat n n"),
    decl("Lambo.A.helper", type_="forall (l : List.{0} Nat), Eq.{1} Nat l.length l.length"),
    decl("Lambo.B.helper", type_="forall (l : List.{0} Nat), Eq.{1} Nat l.length l.length",
         module="LamboProofs.B"),
    decl("Lambo.A.f", kind="def", type_="Nat -> Nat",
         value="fun (x : Nat) => Lambo.A.f.match_1.{1} (fun (x : Nat) => Nat) x"),
    decl("Lambo.A.f.match_1", kind="def", user=False, type_="M", value="fun (b0 : Nat) => b0"),
]


class Equivalence(unittest.TestCase):
    def run_check(self, base, head, renames=None):
        with tempfile.TemporaryDirectory() as d:
            paths = []
            for name, rows in (("base", base), ("head", head)):
                p = Path(d) / f"{name}.jsonl"
                p.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
                paths.append(p)
            args = ["--base-dump", str(paths[0]), "--head-dump", str(paths[1])]
            if renames is not None:
                r = Path(d) / "renames.tsv"
                r.write_text(renames, encoding="utf-8")
                args += ["--renames", str(r)]
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                code = eq.main(args)
            return code, out.getvalue() + err.getvalue()

    def test_identical(self):
        self.assertEqual(self.run_check(BASE, BASE)[0], 0)

    def test_moved_module_is_reported_not_failed(self):
        head = [dict(d, module="LamboProofs.A.Part") if d["name"] == "Lambo.A.t" else d for d in BASE]
        code, out = self.run_check(BASE, head)
        self.assertEqual(code, 0, out)
        self.assertIn("LamboProofs.A -> LamboProofs.A.Part", out)

    def test_restated_theorem_fails(self):
        head = [dict(d, type="forall (n : Nat), Eq.{1} Nat n (n + 0)") if d["name"] == "Lambo.A.t" else d
                for d in BASE]
        code, out = self.run_check(BASE, head)
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.t: statement changed", out)

    def test_renamed_binder_fails(self):
        head = [dict(d, type="forall (m : Nat), Eq.{1} Nat m m") if d["name"] == "Lambo.A.t" else d
                for d in BASE]
        self.assertEqual(self.run_check(BASE, head)[0], 1)

    def test_deleted_theorem_fails(self):
        head = [d for d in BASE if d["name"] != "Lambo.A.t"]
        code, out = self.run_check(BASE, head)
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.t: theorem at the base, not declared at HEAD", out)

    def test_changed_definition_fails(self):
        head = [dict(d, value="fun (x : Nat) => x") if d["name"] == "Lambo.A.f" else d for d in BASE]
        code, out = self.run_check(BASE, head)
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.f: definition body changed", out)

    def test_added_declaration_fails(self):
        code, out = self.run_check(BASE, BASE + [decl("Lambo.A.extra")])
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.extra: theorem added at HEAD", out)

    def test_nonstandard_axiom_fails(self):
        head = [dict(d, axioms=["sorryAx"]) if d["name"] == "Lambo.A.t" else d for d in BASE]
        code, out = self.run_check(BASE, head)
        self.assertEqual(code, 1)
        self.assertIn("sorryAx", out)

    def test_reused_matcher_under_another_name_is_equal(self):
        head = [d for d in BASE if d["name"] != "Lambo.A.f.match_1"] + [
            decl("Lambo.A.g.match_1", kind="def", user=False, type_="M", value="fun (b0 : Nat) => b0")]
        head = [dict(d, value=d["value"].replace("Lambo.A.f.match_1", "Lambo.A.g.match_1"))
                if d["name"] == "Lambo.A.f" else d for d in head]
        self.assertEqual(self.run_check(BASE, head)[0], 0)

    def test_private_prefix_is_ignored(self):
        base = BASE + [decl("Lambo.A.p", kind="def", type_="Nat",
                            value="_private.LamboProofs.A.0.Lambo.A.q")]
        head = BASE + [decl("Lambo.A.p", kind="def", type_="Nat",
                            value="_private.LamboProofs.A.Part.0.Lambo.A.q")]
        self.assertEqual(self.run_check(base, head)[0], 0)

    def test_going_private_or_public_fails(self):
        base = BASE + [decl("Lambo.A.p", kind="def", type_="Nat", value="0")]
        head = BASE + [decl("Lambo.A.p", kind="def", type_="Nat", value="0", private=True)]
        code, out = self.run_check(base, head)
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.p: public at the base, private at HEAD", out)
        self.assertEqual(self.run_check(head, base)[0], 1)

    def test_listed_moves_merge_duplicates(self):
        lib = decl("Lambo.Lib.List.helper", type_=BASE[1]["type"], module="LamboProofs.Lib.List")
        head = [d for d in BASE if not d["name"].endswith(".helper")] + [lib]
        renames = "# old\tnew\nLambo.A.helper\tLambo.Lib.List.helper\nLambo.B.helper\tLambo.Lib.List.helper\n"
        code, out = self.run_check(BASE, head, renames)
        self.assertEqual(code, 0, out)
        self.assertIn("Lambo.A.helper -> Lambo.Lib.List.helper", out)
        # Unlisted, the same move is a deletion and an addition.
        self.assertEqual(self.run_check(BASE, head)[0], 1)

    def test_listed_move_must_keep_the_statement(self):
        lib = decl("Lambo.Lib.List.helper", type_="True", module="LamboProofs.Lib.List")
        head = [d for d in BASE if d["name"] != "Lambo.A.helper"] + [lib]
        code, out = self.run_check(BASE, head, "Lambo.A.helper\tLambo.Lib.List.helper\n")
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.helper: statement changed", out)


if __name__ == "__main__":
    unittest.main()
