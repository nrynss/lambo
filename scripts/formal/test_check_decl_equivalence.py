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
    def run_check(self, base, head, renames=None, deletions=None, flags=()):
        with tempfile.TemporaryDirectory() as d:
            paths = []
            for name, rows in (("base", base), ("head", head)):
                p = Path(d) / f"{name}.jsonl"
                p.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
                paths.append(p)
            args = ["--base-dump", str(paths[0]), "--head-dump", str(paths[1]), *flags]
            if renames is not None:
                r = Path(d) / "renames.tsv"
                r.write_text(renames, encoding="utf-8")
                args += ["--renames", str(r)]
            if deletions is not None:
                r = Path(d) / "deletions.tsv"
                r.write_text(deletions, encoding="utf-8")
                args += ["--deletions", str(r)]
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

    def test_syntax_extensions_are_not_compared(self):
        base = BASE + [
            decl("Lambo.A._aux_LamboProofs_A___macroRules_Lambo_A_tacticT_1", kind="def",
                 type_="Lean.Macro", value="m"),
            decl("Lambo.A.«term_<+_»", kind="def", type_="Lean.TrailingParserDescr",
                 value="_private.LamboProofs.A.0.x")]
        head = BASE + [
            decl("Lambo.A._aux_LamboProofs_A_Part___macroRules_Lambo_A_tacticT_1", kind="def",
                 type_="Lean.Macro", value="m"),
            decl("Lambo.A.«term_<+_»", kind="def", type_="Lean.TrailingParserDescr",
                 value="other")]
        self.assertEqual(self.run_check(base, head)[0], 0)
        # A statement is still compared as its kernel term.
        head2 = [dict(d, type="forall (n : Nat), Eq.{1} Nat n 0") if d["name"] == "Lambo.A.t" else d
                 for d in head]
        self.assertEqual(self.run_check(base, head2)[0], 1)

    def test_private_prefix_is_ignored(self):
        base = BASE + [decl("Lambo.A.p", kind="def", type_="Nat",
                            value="_private.LamboProofs.A.0.Lambo.A.q")]
        head = BASE + [decl("Lambo.A.p", kind="def", type_="Nat",
                            value="_private.LamboProofs.A.Part.0.Lambo.A.q")]
        self.assertEqual(self.run_check(base, head)[0], 0)

    def test_listed_deletions_pass_and_nothing_else_does(self):
        head = [d for d in BASE if d["name"] != "Lambo.B.helper"]
        listed = "# name\treason\tkept\nLambo.B.helper\tsame as A's\tLambo.A.helper\n"
        code, out = self.run_check(BASE, head, deletions=listed)
        self.assertEqual(code, 0, out)
        self.assertIn("Lambo.B.helper: same as A's (kept: Lambo.A.helper)", out)
        # A `#` inside a reason (an issue number) is text, not a comment.
        code, out = self.run_check(BASE, head, deletions="Lambo.B.helper\tdead since #280\t-\n")
        self.assertEqual(code, 0, out)
        self.assertIn("Lambo.B.helper: dead since #280", out)
        # Unlisted, the same deletion fails.
        self.assertEqual(self.run_check(BASE, head)[0], 1)
        # A second, unlisted deletion still fails.
        head2 = [d for d in head if d["name"] != "Lambo.A.t"]
        code, out = self.run_check(BASE, head2, deletions=listed)
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.t: theorem at the base, not declared at HEAD", out)

    def test_listed_deletion_must_be_gone_unused_and_keep_its_counterpart(self):
        code, out = self.run_check(BASE, BASE, deletions="Lambo.B.helper\tr\t-\n")
        self.assertEqual(code, 1)
        self.assertIn("listed as deleted, but still declared", out)
        # An unused definition can be deleted; one a kept declaration
        # mentions cannot.
        head = [d for d in BASE if d["name"] not in ("Lambo.A.f", "Lambo.A.f.match_1")]
        code, out = self.run_check(BASE, head, deletions="Lambo.A.f\tr\t-\n")
        self.assertEqual(code, 0, out)
        uses = decl("Lambo.A.uses_f", type_="Eq.{1} Nat (Lambo.A.f 0) 0")
        code, out = self.run_check(BASE + [uses], head + [uses], deletions="Lambo.A.f\tr\t-\n")
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.uses_f at HEAD still mentions", out)
        head = [d for d in BASE if d["name"] not in ("Lambo.A.helper", "Lambo.B.helper")]
        code, out = self.run_check(BASE, head,
                                   deletions="Lambo.B.helper\tr\tLambo.A.helper\nLambo.A.helper\tr\t-\n")
        self.assertEqual(code, 1)
        self.assertIn("listed as deleted for Lambo.A.helper, which is not declared at HEAD", out)

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


T_STMT = "forall (n : Nat), Eq.{1} Nat n n"


def in_spec_form(rows, stmt=T_STMT, spec="Lambo.A.Spec.t", rfl=True, prop=True):
    """`rows` with `Lambo.A.t` restated as `theorem t : Spec.t`, the proposition
    unfolding to `stmt`."""
    head = [dict(d, type=spec, spec=spec, stmt=stmt, stmt_shape=stmt, spec_rfl=rfl)
            if d["name"] == "Lambo.A.t" else d for d in rows]
    if prop:
        head.append(decl(spec, kind="def", type_="Prop", value=stmt, module="LamboProofs.A.Spec"))
    return head


class SpecForm(unittest.TestCase):
    """`--spec-form` and `--only-form-changes`: a theorem restated as
    `theorem foo : Spec.foo`."""

    run_check = Equivalence.run_check

    def test_a_restated_theorem_passes_with_the_flag_and_fails_without(self):
        head = in_spec_form(BASE)
        code, out = self.run_check(BASE, head, flags=["--spec-form"])
        self.assertEqual(code, 0, out)
        self.assertIn("restated 1 in Spec form", out)
        code, out = self.run_check(BASE, head)
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.t: statement changed", out)
        self.assertIn("--spec-form", out)
        self.assertIn("Lambo.A.Spec.t: def added at HEAD", out)

    def test_a_proposition_that_unfolds_to_another_statement_fails(self):
        for stmt in ("forall (n : Nat), Eq.{1} Nat n (n + 0)",          # another conclusion
                     "forall (n : Nat), (LT.lt n 3) -> Eq.{1} Nat n n",   # a hypothesis added
                     "forall (m : Nat), Eq.{1} Nat m m"):                 # a variable renamed
            with self.subTest(stmt=stmt):
                code, out = self.run_check(BASE, in_spec_form(BASE, stmt=stmt), flags=["--spec-form"])
                self.assertEqual(code, 1)
                self.assertIn("Lambo.A.t: statement changed: Lambo.A.Spec.t does not unfold to the base "
                              "statement", out)
                code, out = self.run_check(BASE, in_spec_form(BASE, stmt=stmt), flags=["--only-form-changes"])
                self.assertEqual(code, 1)
                self.assertIn("does not unfold to the base statement", out)

    def test_the_proposition_must_be_the_theorems_own(self):
        head = in_spec_form(BASE, spec="Lambo.A.Spec.other")
        code, out = self.run_check(BASE, head, flags=["--spec-form"])
        self.assertEqual(code, 1)
        self.assertIn("stated as Lambo.A.Spec.other, but its Spec proposition must be Lambo.A.Spec.t", out)

    def test_the_kernel_must_accept_the_unfolding(self):
        code, out = self.run_check(BASE, in_spec_form(BASE, rfl=False), flags=["--spec-form"])
        self.assertEqual(code, 1)
        self.assertIn("the kernel does not accept Lambo.A.Spec.t as its own body", out)

    def test_a_proposition_no_theorem_states_is_an_addition(self):
        orphan = decl("Lambo.A.Spec.unproved", kind="def", type_="Prop", value="True")
        code, out = self.run_check(BASE, in_spec_form(BASE) + [orphan], flags=["--spec-form"])
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.Spec.unproved: def added at HEAD", out)
        self.assertNotIn("Lambo.A.Spec.t: def added", out)

    def test_everything_else_is_still_compared(self):
        head = [dict(d, value="fun (x : Nat) => x") if d["name"] == "Lambo.A.f" else d
                for d in in_spec_form(BASE)]
        code, out = self.run_check(BASE, head, flags=["--spec-form"])
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.f: definition body changed", out)

    def test_a_theorem_leaving_spec_form_is_checked_the_same_way(self):
        code, out = self.run_check(in_spec_form(BASE), BASE, flags=["--only-form-changes"])
        self.assertEqual(code, 0, out)
        changed = [dict(d, type="forall (n : Nat), Eq.{1} Nat n (n + 0)") if d["name"] == "Lambo.A.t" else d
                   for d in BASE]
        code, out = self.run_check(in_spec_form(BASE), changed, flags=["--only-form-changes"])
        self.assertEqual(code, 1)
        self.assertIn("does not unfold to the HEAD statement", out)

    def test_a_private_name_in_the_statement_is_compared_by_its_user_name(self):
        base = [dict(d, type="Eq.{1} Nat _private.LamboProofs.A.0.Lambo.A.k 0") if d["name"] == "Lambo.A.t"
                else d for d in BASE]
        head = in_spec_form(BASE, stmt="Eq.{1} Nat _private.LamboProofs.A.Spec.0.Lambo.A.k 0")
        self.assertEqual(self.run_check(base, head, flags=["--spec-form"])[0], 0)

    def test_only_form_changes_ignores_what_is_not_a_form_change(self):
        # A theorem added, one deleted, one restated in place and a changed
        # definition: a model change, not this mode's business.
        head = [dict(d, value="fun (x : Nat) => x") if d["name"] == "Lambo.A.f" else
                dict(d, type="True") if d["name"] == "Lambo.A.helper" else d
                for d in BASE if d["name"] != "Lambo.B.helper"] + [decl("Lambo.A.extra")]
        code, out = self.run_check(BASE, head, flags=["--only-form-changes"])
        self.assertEqual(code, 0, out)
        self.assertIn("no theorem changed form", out)
        code, out = self.run_check(BASE, in_spec_form(head), flags=["--only-form-changes"])
        self.assertEqual(code, 0, out)
        self.assertIn("restated  Lambo.A.t", out)

    def test_spec_name(self):
        self.assertEqual(eq.spec_name("Lambo.Drain.drain_fifo"), "Lambo.Drain.Spec.drain_fifo")
        self.assertEqual(eq.spec_name("Lambo.Cite.Reach.trans"), "Lambo.Cite.Spec.Reach.trans")
        self.assertIsNone(eq.spec_name("Lambo.loose"))


if __name__ == "__main__":
    unittest.main()
