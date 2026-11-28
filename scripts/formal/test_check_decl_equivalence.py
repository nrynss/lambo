#!/usr/bin/env python3
"""Tests for check-decl-equivalence.py, over hand-made dumps (no Lean needed).

  python3 -I scripts/formal/test_check_decl_equivalence.py
"""

import contextlib
import importlib.util
import io
import json
import os
import tempfile
import unittest
from unittest import mock
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

    def test_a_deletion_before_the_base_is_counted_not_compared(self):
        # The deletions file keeps every deletion; against a later base, a
        # row declared on neither side was deleted before it.
        listed = "Lambo.Old.gone\tdeleted long ago\tLambo.A.helper\n"
        code, out = self.run_check(BASE, BASE, deletions=listed)
        self.assertEqual(code, 0, out)
        self.assertIn("1 listed deletion(s) predate the base", out)
        self.assertNotIn("deleted 1:", out)
        # Its KEPT must still be declared.
        head = [d for d in BASE if d["name"] != "Lambo.A.helper"]
        code, out = self.run_check(head, head, deletions=listed)
        self.assertEqual(code, 1)
        self.assertIn("listed as deleted for Lambo.A.helper, which is not declared at HEAD", out)
        # Declared at HEAD and not at the base is still an error.
        code, out = self.run_check(BASE, BASE + [decl("Lambo.Old.gone")], deletions=listed)
        self.assertEqual(code, 1)
        self.assertIn("Lambo.Old.gone: listed as deleted, but not declared at the base", out)

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
        # Listed under its own name, the change of visibility passes; its
        # statement and body are still compared.
        code, out = self.run_check(head, base, "Lambo.A.p\tLambo.A.p\n")
        self.assertEqual(code, 0, out)
        changed = BASE + [decl("Lambo.A.p", kind="def", type_="Nat", value="1")]
        code, out = self.run_check(head, changed, "Lambo.A.p\tLambo.A.p\n")
        self.assertEqual(code, 1)
        self.assertIn("Lambo.A.p: definition body changed", out)

    def test_listed_moves_merge_duplicates(self):
        lib = decl("Lambo.Lib.List.helper", type_=BASE[1]["type"], module="LamboProofs.Lib.List")
        head = [d for d in BASE if not d["name"].endswith(".helper")] + [lib]
        renames = "# old\tnew\nLambo.A.helper\tLambo.Lib.List.helper\nLambo.B.helper\tLambo.Lib.List.helper\n"
        code, out = self.run_check(BASE, head, renames)
        self.assertEqual(code, 0, out)
        self.assertIn("Lambo.A.helper -> Lambo.Lib.List.helper", out)
        # Unlisted, the same move is a deletion and an addition.
        self.assertEqual(self.run_check(BASE, head)[0], 1)

    def test_a_move_before_the_base_is_not_compared(self):
        listed = "Lambo.Old.helper\tLambo.A.helper\n"
        code, out = self.run_check(BASE, BASE, listed)
        self.assertEqual(code, 0, out)
        # Its NEW gone too, or its OLD back at HEAD, it is an error.
        head = [d for d in BASE if d["name"] != "Lambo.A.helper"]
        code, out = self.run_check(head, head, listed)
        self.assertEqual(code, 1)
        self.assertIn("Lambo.Old.helper: listed as moved, but not declared at the base", out)
        code, out = self.run_check(BASE, BASE + [decl("Lambo.Old.helper")], listed)
        self.assertEqual(code, 1)

    def test_a_move_before_the_base_needs_its_new_at_the_base(self):
        # A row declared on neither side would otherwise pass as a move made
        # before the base, and let its NEW be added at HEAD unchecked.
        sneak = decl("Lambo.A.sneak")
        code, out = self.run_check(BASE, BASE + [sneak], "Lambo.Ghost.x\tLambo.A.sneak\n")
        self.assertEqual(code, 1)
        self.assertIn("Lambo.Ghost.x: listed as moved before the base, but Lambo.A.sneak is not declared "
                      "at the base", out)
        self.assertIn("Lambo.A.sneak: theorem added at HEAD", out)

    def test_a_row_older_than_the_base_renames_nothing(self):
        # Its OLD is no declaration of the base, so it rewrites no base term:
        # `Nat.succ` read as `Lambo.A.f` would pass a restated `t`.
        base = [dict(d, type="forall (n : Nat), Eq.{1} Nat (Nat.succ n) (Nat.succ n)")
                if d["name"] == "Lambo.A.t" else d for d in BASE]
        head = [dict(d, type="forall (n : Nat), Eq.{1} Nat (Lambo.A.f n) (Lambo.A.f n)")
                if d["name"] == "Lambo.A.t" else d for d in BASE]
        for flags in ((), ("--spec-form",)):
            with self.subTest(flags=flags):
                code, out = self.run_check(base, head, "Nat.succ\tLambo.A.f\n", flags=flags)
                self.assertEqual(code, 1, out)
                self.assertIn("Lambo.A.t: statement changed", out)

    def test_a_namespace_is_not_an_old_name(self):
        # `Lambo.A` would rename the start of `Lambo.A.f`, reading it as
        # `Lambo.B.helper.f`.
        g = decl("Lambo.B.helper.f", kind="def", type_="Nat -> Nat", value="fun (x : Nat) => 0",
                 module="LamboProofs.B")
        base = [dict(d, type="Eq.{1} Nat (Lambo.A.f 0) 0") if d["name"] == "Lambo.A.t" else d
                for d in BASE] + [g]
        head = [dict(d, type="Eq.{1} Nat (Lambo.B.helper.f 0) 0") if d["name"] == "Lambo.A.t" else d
                for d in BASE] + [g]
        code, out = self.run_check(base, head, "Lambo.A\tLambo.B.helper\n")
        self.assertEqual(code, 1, out)
        self.assertIn("Lambo.A.t: statement changed", out)
        self.assertIn("Lambo.A: listed as moved, but it names a namespace", out)
        # A move made in this diff renames a whole name, a universe
        # instantiation after it included.
        moved = [dict(d, name="Lambo.Old.f") if d["name"] == "Lambo.A.f" else
                 dict(d, type="Eq.{1} Nat (Lambo.Old.f.{0} 0) 0")
                 if d["name"] == "Lambo.A.t" else d for d in BASE]
        head = [dict(d, type="Eq.{1} Nat (Lambo.A.f.{0} 0) 0")
                if d["name"] == "Lambo.A.t" else d for d in BASE]
        code, out = self.run_check(moved, head, "Lambo.Old.f\tLambo.A.f\n")
        self.assertEqual(code, 0, out)

    def test_a_malformed_or_conflicting_row_is_refused(self):
        lib = decl("Lambo.Lib.List.helper", type_=BASE[1]["type"], module="LamboProofs.Lib.List")
        head = [d for d in BASE if d["name"] != "Lambo.A.helper"] + [lib]
        for rows, msg in (("Lambo.A.helper\tLambo.Lib.List.helper\tx\n", "want OLD<TAB>NEW"),
                          ("Lambo.A.helper\n", "want OLD<TAB>NEW"),
                          ("Lambo.A.helper\tLambo.Lib.List.helper\nLambo.A.helper\tLambo.B.helper\n",
                           "Lambo.A.helper is listed as moved to both Lambo.Lib.List.helper and "
                           "Lambo.B.helper")):
            with self.subTest(rows=rows), self.assertRaises(SystemExit) as cm:
                self.run_check(BASE, head, rows)
            self.assertIn(msg, str(cm.exception))
        # The same row twice is one move.
        rows = "Lambo.A.helper\tLambo.Lib.List.helper\n" * 2
        self.assertEqual(self.run_check(BASE, head, rows)[0], 0)

    def test_a_generated_constant_is_not_an_old_name(self):
        code, out = self.run_check(BASE, BASE, "Lambo.A.f.match_1\tLambo.A.f\n")
        self.assertEqual(code, 1, out)
        self.assertIn("Lambo.A.f.match_1: listed as moved, but a constant Lean generates", out)

    def test_a_name_both_moved_and_deleted_fails(self):
        # Moved, OLD would admit NEW; deleted, OLD would skip the comparison:
        # together they let NEW be added unchecked.
        head = [d for d in BASE if d["name"] != "Lambo.A.t"] + [decl("Lambo.A.fake", type_="False")]
        code, out = self.run_check(BASE, head, "Lambo.A.t\tLambo.A.fake\n", "Lambo.A.t\twhy\t-\n")
        self.assertEqual(code, 1, out)
        self.assertIn("Lambo.A.t: listed both as moved to Lambo.A.fake and as deleted", out)
        self.assertIn("Lambo.A.fake: theorem added at HEAD", out)

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

    def test_a_changed_definition_under_a_restated_statement_fails(self):
        # `t` mentions `f`, whose body changed: the statement prints as before
        # but no longer says the same thing. `--only-form-changes` compares no
        # definition by itself, so it must follow the statement into `f`.
        stmt = "forall (n : Nat), Eq.{1} Nat (Lambo.A.f n) n"
        base = [dict(d, type=stmt) if d["name"] == "Lambo.A.t" else d for d in BASE]
        changed = [dict(d, value="fun (x : Nat) => x") if d["name"] == "Lambo.A.f" else d for d in base]
        for flag in ("--only-form-changes", "--spec-form"):
            with self.subTest(flag=flag):
                code, out = self.run_check(base, in_spec_form(base, stmt=stmt), flags=[flag])
                self.assertEqual(code, 0, out)
                code, out = self.run_check(base, in_spec_form(changed, stmt=stmt), flags=[flag])
                self.assertEqual(code, 1, out)
                self.assertIn("Lambo.A.t: statement changed: it prints as before, but a definition it "
                              "reaches changed: Lambo.A.f (body changed)", out)
                self.assertNotIn("states what it stated", out)
                self.assertNotIn("restated  Lambo.A.t", out)
        # Reached through another definition's body, and through the uses of
        # HEAD's proposition, the same.
        g = decl("Lambo.A.g", kind="def", type_="Nat -> Nat", value="fun (x : Nat) => Lambo.A.f x")
        g["uses"] = ["Lambo.A.f"]
        stmt2 = "forall (n : Nat), Eq.{1} Nat (Lambo.A.g n) n"
        base2 = [dict(d, type=stmt2) if d["name"] == "Lambo.A.t" else d for d in BASE] + [g]
        changed2 = [dict(d, value="fun (x : Nat) => x") if d["name"] == "Lambo.A.f" else d for d in base2]
        code, out = self.run_check(base2, in_spec_form(changed2, stmt=stmt2), flags=["--only-form-changes"])
        self.assertEqual(code, 1, out)
        self.assertIn("Lambo.A.f (body changed)", out)
        # A definition the statement does not reach may change.
        code, out = self.run_check(BASE, in_spec_form([dict(d, value="fun (x : Nat) => x")
                                                       if d["name"] == "Lambo.A.f" else d for d in BASE]),
                                   flags=["--only-form-changes"])
        self.assertEqual(code, 0, out)

    def test_a_move_before_the_base_does_not_rename_a_reached_definition(self):
        # `Old.f -> A.f` predates the base, so both sides declare `A.f`: the
        # restated `t`, which reaches `A.f`, compares it with itself.
        stmt = "forall (n : Nat), Eq.{1} Nat (Lambo.A.f n) n"
        base = [dict(d, type=stmt) if d["name"] == "Lambo.A.t" else d for d in BASE]
        code, out = self.run_check(base, in_spec_form(base, stmt=stmt), "Lambo.Old.f\tLambo.A.f\n",
                                   flags=["--spec-form"])
        self.assertEqual(code, 0, out)
        # A move made in this diff still compares OLD with NEW.
        base2 = [dict(d, name="Lambo.Old.f") if d["name"] == "Lambo.A.f" else
                 dict(d, type=stmt.replace("Lambo.A.f", "Lambo.Old.f")) if d["name"] == "Lambo.A.t" else d
                 for d in base]
        changed = [dict(d, value="fun (x : Nat) => x") if d["name"] == "Lambo.A.f" else d for d in base]
        code, out = self.run_check(base2, in_spec_form(changed, stmt=stmt), "Lambo.Old.f\tLambo.A.f\n",
                                   flags=["--spec-form"])
        self.assertEqual(code, 1, out)
        self.assertIn("Lambo.A.f (body changed)", out)

    def test_a_generated_old_name_does_not_hide_a_reached_definition(self):
        # `A.f.match_1 -> A.f` is no move made in this diff (its OLD is no
        # declaration), so the restated `t`, which reaches `A.f`, still
        # compares `A.f` on both sides.
        stmt = "forall (n : Nat), Eq.{1} Nat (Lambo.A.f n) n"
        base = [dict(d, type=stmt) if d["name"] == "Lambo.A.t" else d for d in BASE]
        changed = [dict(d, value="fun (x : Nat) => x") if d["name"] == "Lambo.A.f" else d for d in base]
        code, out = self.run_check(base, in_spec_form(changed, stmt=stmt), "Lambo.A.f.match_1\tLambo.A.f\n",
                                   flags=["--only-form-changes"])
        self.assertEqual(code, 1, out)
        self.assertIn("Lambo.A.f (body changed)", out)

    def test_every_definition_merged_into_a_reached_one_is_compared(self):
        # `p` and `q` both move to `r`: `t : p ≤ q` reads as `r ≤ r`, so each
        # of `p` and `q` must equal `r`, not only the first row's.
        def nat(name, value):
            return decl(name, kind="def", type_="Nat", value=value)
        stmt = "LE.le.{0} Nat instLENat Lambo.A.p Lambo.A.q"
        rows = "Lambo.A.p\tLambo.Lib.r\nLambo.A.q\tLambo.Lib.r\n"
        head = in_spec_form(BASE + [nat("Lambo.Lib.r", "0")],
                            stmt="LE.le.{0} Nat instLENat Lambo.Lib.r Lambo.Lib.r")
        base = [dict(d, type=stmt) if d["name"] == "Lambo.A.t" else d for d in BASE]
        for flag in ("--only-form-changes", "--spec-form"):
            with self.subTest(flag=flag):
                code, out = self.run_check(base + [nat("Lambo.A.p", "0"), nat("Lambo.A.q", "1")], head, rows,
                                           flags=[flag])
                self.assertEqual(code, 1, out)
                self.assertIn("Lambo.A.q" + (" -> Lambo.Lib.r (body changed)" if flag == "--only-form-changes"
                                             else ": definition body changed"), out)
                # Both equal to `r`, the merge passes.
                code, out = self.run_check(base + [nat("Lambo.A.p", "0"), nat("Lambo.A.q", "0")], head, rows,
                                           flags=[flag])
                self.assertEqual(code, 0, out)

    def test_each_sides_proposition_contributes_its_own_uses(self):
        # The printed statement names no definition (a generated constant is
        # normalised to a hash), but the proposition's body uses `g`. Whichever
        # side is in Spec form, its proposition is looked up in its own dump
        # and `g`'s change is found; leaving Spec form, it was looked up in
        # HEAD's dump, where the proposition is gone, and `g` went unchecked.
        g = decl("Lambo.A.g", kind="def", type_="Nat -> Nat", value="fun (x : Nat) => x")
        g2 = dict(g, value="fun (x : Nat) => 0")
        spec = in_spec_form(BASE + [g])
        for d in spec:
            if d["name"] == "Lambo.A.Spec.t":
                d["uses"] = ["Lambo.A.g"]
        spec2 = [dict(d, value=g2["value"]) if d["name"] == "Lambo.A.g" else d for d in spec]
        for label, base, head in (("leaving", spec, BASE + [g2]), ("entering", BASE + [g], spec2)):
            with self.subTest(label):
                code, out = self.run_check(base, head, flags=["--only-form-changes"])
                self.assertEqual(code, 1, out)
                self.assertIn("Lambo.A.t: statement changed: it prints as before, but a definition it "
                              "reaches changed: Lambo.A.g (body changed)", out)
        # Unchanged, both directions pass.
        self.assertEqual(self.run_check(spec, BASE + [g], flags=["--only-form-changes"])[0], 0)
        self.assertEqual(self.run_check(BASE + [g], spec, flags=["--only-form-changes"])[0], 0)

    def test_a_toolchain_change_skips_only_form_changes(self):
        import subprocess
        with tempfile.TemporaryDirectory() as d:
            def git(*a):
                subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", *a], cwd=d, check=True,
                               capture_output=True)
            (Path(d) / "formal").mkdir()
            (Path(d) / "formal" / "README.md").write_text("x\n", encoding="utf-8")
            git("init", "-q")
            git("add", ".")
            git("commit", "-q", "-m", "before the toolchain file")
            tc = Path(d) / "formal" / "lean-toolchain"
            tc.write_text("leanprover/lean4:v4.36.0\n", encoding="utf-8")
            git("add", ".")
            git("commit", "-q", "-m", "base")
            self.assertIsNone(eq.toolchain_change(d, "HEAD"))
            tc.write_text("leanprover/lean4:v4.37.0\n", encoding="utf-8")
            self.assertEqual(eq.toolchain_change(d, "HEAD"),
                             ("leanprover/lean4:v4.36.0", "leanprover/lean4:v4.37.0"))
            # A base without the file (a real commit, before it was added) is no change.
            self.assertIsNone(eq.toolchain_change(d, "HEAD~1"))

            def skip(env):
                out = io.StringIO()
                with mock.patch.dict(os.environ, env, clear=False), contextlib.redirect_stdout(out):
                    if "GITHUB_ACTIONS" not in env:
                        os.environ.pop("GITHUB_ACTIONS", None)
                    code = eq.main(["--repo", d, "--base", "HEAD", "--only-form-changes"])
                return code, out.getvalue()

            code, out = skip({})
            self.assertEqual(code, 0)
            self.assertIn("skipped: formal/lean-toolchain differs between HEAD (leanprover/lean4:v4.36.0)", out)
            self.assertNotIn("::warning::", out)
            # In CI the skip is a warning on the run, still exit 0.
            code, out = skip({"GITHUB_ACTIONS": "true"})
            self.assertEqual(code, 0)
            self.assertIn("::warning::the form-change check did not run: formal/lean-toolchain changed", out)

    def test_spec_name(self):
        self.assertEqual(eq.spec_name("Lambo.Drain.drain_fifo"), "Lambo.Drain.Spec.drain_fifo")
        self.assertEqual(eq.spec_name("Lambo.Cite.Reach.trans"), "Lambo.Cite.Spec.Reach.trans")
        self.assertIsNone(eq.spec_name("Lambo.loose"))


if __name__ == "__main__":
    unittest.main()
