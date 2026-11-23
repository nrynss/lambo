#!/usr/bin/env python3
"""Tests for check-doc-anchors.py.

  python3 -I scripts/docs/test_check_doc_anchors.py
"""

import contextlib
import importlib.util
import io
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("check_doc_anchors", HERE / "check-doc-anchors.py")
cda = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cda)

LEAN = """\
/-! Module doc. A line inside it that reads
def notDeclared : Nat := 0
is not a declaration. -/
namespace Lambo.Demo

inductive Status where
  | none | canonical

def Status.rank : Status → Nat
  | .none => 0
  | .canonical => 3

-- theorem commentedOut : True := trivial

/-- A doc comment. -/
@[simp] private theorem legal_irrefl (s : Status) : True := trivial

section Examples
abbrev Small := Nat
end Examples

namespace Edge
def dependence : Nat := 1
end Edge

structure Flags where
  stage1 : Bool

end Lambo.Demo

theorem outside : True := trivial
"""

CONSTANTS = """\
use crate::canon::formal_pins::{MIN_GC_SURVIVED, MIN_BLAST_RADIUS};
fn pins() {
    let w = ScoringWeights::default();
    for ty in ConceptType::ALL { ty.eviction_resistance(); }
    assert!(!Config::default().canonization_supersede);
}
"""

RUST = """\
//! Module doc. fn in_a_comment() is not a declaration.
pub const HIGH_RISK_WRITE_WINDOW: u64 = 30;
enum Credit<'a> {
    DependentOrigin,
    Writer(&'a str),
}
pub(crate) struct Probe {
    pub(crate) last_demotion_time: Option<u64>,
}
impl Default for Probe {
    fn default() -> Self { Probe { last_demotion_time: None } }
}
fn is_high_value() -> bool { let s = "fn in_a_string()"; let c = '"'; true }
fn uses(policy: u8) -> u8 { policy }
"""

TESTS = """\
//! fn in_a_module_doc() is not a test.
use super::*;

fn helper() -> u8 { 1 }

#[test]
fn a_plain_test() {
    assert_eq!(helper(), 1);
}
fn helper_after_a_test() {}

/// A doc comment between the attribute and the fn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow"]
async fn an_async_test() {}

#[cfg(test)]
fn cfg_test_only_helper() {}

mod nested {
    #[test]
    pub(crate) fn a_nested_test() {
        let s = "#[test] fn in_a_string() {}";
    }
}

#[tokio::test]
// fn commented_out() {}
async fn after_a_comment() {}
"""


class Fixture(unittest.TestCase):
    def setUp(self):
        patcher = mock.patch.dict(cda.REQUIRED, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.write("formal/LamboProofs/Demo.lean", LEAN)
        self.write("src/formal_constants_tests.rs", CONSTANTS)
        self.write("src/daemon/events.rs", RUST)
        self.write("src/daemon/tests.rs", TESTS)
        self.docs = self.root / "site" / "src" / "content" / "docs"
        self.docs.mkdir(parents=True)

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(text, bytes):
            path.write_bytes(text)
        else:
            path.write_text(text, encoding="utf-8")
        return path

    def page(self, text, name="page.mdx"):
        return self.write(f"site/src/content/docs/{name}", text)

    def errors(self, text):
        return cda.check(self.root, [self.page(text)])[0]

    def main(self, *args):
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            code = cda.main(["--root", str(self.root), *map(str, args)])
        return code, err.getvalue()


class Lean(Fixture):
    def test_declarations_resolve_with_or_without_namespaces(self):
        for ref in [
            "Demo.Status", "Demo.Status.rank", "Demo.legal_irrefl", "Demo.Small",
            "Demo.Edge.dependence", "Demo.Flags", "Demo.outside",
        ]:
            with self.subTest(ref=ref):
                self.assertEqual(self.errors(f"Text. {{/* lean: {ref} */}}"), [])

    def test_a_list_resolves_each_name(self):
        self.assertEqual(self.errors("{/* lean: Demo.Status.rank, Demo.legal_irrefl */}"), [])

    def test_unknown_names_fail(self):
        for ref, says in [
            ("Demo.nope", "declares no nope"),
            ("Demo.notDeclared", "declares no notDeclared"),
            ("Demo.commentedOut", "declares no commentedOut"),
            ("Demo.rank.Status", "declares no rank.Status"),
            # Exact after the `Lambo.` and `Demo.` prefixes: no suffix match,
            # and no repeated model prefix.
            ("Demo.rank", "declares no rank"),
            ("Demo.dependence", "declares no dependence"),
            ("Demo.Demo.Status.rank", "declares no Demo.Status.rank"),
            ("Nope.legal", "no file formal/LamboProofs/Nope.lean"),
            ("legal", "is not `<Model>.<name>`"),
        ]:
            with self.subTest(ref=ref):
                errors = self.errors(f"{{/* lean: {ref} */}}")
                self.assertEqual(len(errors), 1, errors)
                self.assertIn(says, errors[0])

    def test_a_split_model_resolves_across_its_files_but_not_its_history_or_findings(self):
        self.write("formal/LamboProofs/Split/Base.lean",
                   "namespace Lambo.Split\ndef base : Nat := 0\nend Lambo.Split\n")
        self.write("formal/LamboProofs/Split/Close.lean",
                   "namespace Lambo.Split\ntheorem closes : True := trivial\nend Lambo.Split\n")
        self.write("formal/LamboProofs/Split/History.lean",
                   "namespace Lambo.Split\ntheorem old : True := trivial\nend Lambo.Split\n")
        self.write("formal/LamboProofs/Split/Examples.lean",
                   "namespace Lambo.Split\ntheorem finding : True := trivial\nend Lambo.Split\n")
        self.assertEqual(self.errors("{/* lean: Split.base, Split.closes */}"), [])
        for name in ("old", "finding"):
            errors = self.errors(f"{{/* lean: Split.{name} */}}")
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("formal/LamboProofs/Split/ (outside History.lean and Examples.lean) "
                          f"declares no {name}", errors[0])
        errors = self.errors("{/* lean: Split.nope */}")
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("declares no nope", errors[0])

    def test_one_bad_name_in_a_list_is_reported(self):
        errors = self.errors("{/* lean: Demo.Status, Demo.missing, Demo.Flags */}")
        self.assertEqual(len(errors), 1)
        self.assertIn("site/src/content/docs/page.mdx:1: lean: Demo.missing", errors[0])


class Const(Fixture):
    def test_pinned_names_resolve(self):
        self.assertEqual(self.errors(
            "{/* const: MIN_GC_SURVIVED, ScoringWeights::default, ConceptType::eviction_resistance, "
            "Config::default().canonization_supersede */}"
        ), [])

    def test_unpinned_names_fail(self):
        for ref, says in [
            ("MIN_DISTINCT", "does not name MIN_DISTINCT"),
            ("MIN_GC", "does not name MIN_GC"),
            ("ScoringWeights::custom", "does not name custom"),
            ("fn", "fn is a Rust keyword"),
            ("Config::default().for", "for is a Rust keyword"),
            ("not a name", "is not a Rust name"),
        ]:
            with self.subTest(ref=ref):
                errors = self.errors(f"{{/* const: {ref} */}}")
                self.assertEqual(len(errors), 1, errors)
                self.assertIn(says, errors[0])


class Code(Fixture):
    def test_paths_and_items_resolve(self):
        for body in [
            "src/daemon/events.rs",
            "src/daemon/events.rs is_high_value HIGH_RISK_WRITE_WINDOW",
            "src/daemon/events.rs is_high_value, HIGH_RISK_WRITE_WINDOW; src/formal_constants_tests.rs pins",
            "src/daemon/events.rs Credit::Writer (a note, with a comma)",
            "src/daemon/events.rs Credit::DependentOrigin, Probe::last_demotion_time, Probe::default",
            "src/daemon/events.rs Credit Probe last_demotion_time",
            "src/daemon/events.rs (module doc), is_high_value",
            "src/daemon",
        ]:
            with self.subTest(body=body):
                self.assertEqual(self.errors(f"{{/* code: {body} */}}"), [])

    def test_bad_paths_and_items_fail(self):
        for body, says in [
            ("src/daemon/nope.rs", "no path src/daemon/nope.rs"),
            ("src/daemon/events.rs is_low_value", "declares no is_low_value"),
            ("src/daemon/events.rs Credit::Reader", "declares no Reader"),
            ("src/daemon/events.rs Writer::Credit", "declares no Writer"),
            ("src/daemon/events.rs Stage::is_high_value", "declares no Stage"),
            ("src/daemon/events.rs Credit::fn", "is not a Rust name"),
            ("src/daemon/events.rs policy", "declares no policy"),
            ("src/daemon/events.rs in_a_comment", "declares no in_a_comment"),
            ("src/daemon/events.rs in_a_string", "declares no in_a_string"),
            ("src/daemon/events.rs Module", "declares no Module"),
            ("src/daemon/events.rs is_high_value()", "is not a Rust name"),
            ("is_high_value", "names no path before it"),
            ("src/daemon is_high_value", "is not a file"),
            ("src/daemon/events.rs (an open note", "parentheses"),
            ("src/daemon/events.rs is_high_value,, HIGH_RISK_WRITE_WINDOW", "an empty group"),
        ]:
            with self.subTest(body=body):
                errors = self.errors(f"{{/* code: {body} */}}")
                self.assertEqual(len(errors), 1, errors)
                self.assertIn(says, errors[0])

    def test_items_are_whole_words(self):
        errors = self.errors("{/* code: src/daemon/events.rs high_value */}")
        self.assertEqual(len(errors), 1)

    def test_prose_in_item_position_fails(self):
        errors = self.errors("{/* code: src/daemon/events.rs the policy cycle */}")
        self.assertEqual(len(errors), 3)

    def test_a_const_only_in_a_comment_or_string_fails(self):
        self.write("src/formal_constants_tests.rs", CONSTANTS + "// MIN_ONLY_COMMENTED\nconst S: &str = \"MIN_ONLY_QUOTED\";\n")
        for ref in ["MIN_ONLY_COMMENTED", "MIN_ONLY_QUOTED"]:
            with self.subTest(ref=ref):
                self.assertEqual(len(self.errors(f"{{/* const: {ref} */}}")), 1)


class Test(Fixture):
    def test_real_tests_resolve(self):
        for body in [
            "src/daemon/tests.rs a_plain_test",
            "src/daemon/tests.rs an_async_test, a_nested_test, after_a_comment",
            "src/daemon/tests.rs a_plain_test (a note, with a comma); src/daemon/tests.rs an_async_test",
            "src/daemon/tests.rs a_plain_test an_async_test",
        ]:
            with self.subTest(body=body):
                self.assertEqual(self.errors(f"{{/* test: {body} */}}"), [])

    def test_a_helper_fn_fails(self):
        for name in ["helper", "helper_after_a_test", "cfg_test_only_helper"]:
            with self.subTest(name=name):
                errors = self.errors(f"{{/* test: src/daemon/tests.rs {name} */}}")
                self.assertEqual(len(errors), 1, errors)
                self.assertIn(f"fn {name} is not a test", errors[0])

    def test_a_group_that_names_no_test_fails(self):
        # A file or a directory alone cites no test, so it must not count
        # toward a REQUIRED page's test: kind.
        for body in ["src/daemon/tests.rs", "src/daemon", "src/daemon/tests.rs (a note only)"]:
            with self.subTest(body=body):
                errors = self.errors(f"{{/* test: {body} */}}")
                self.assertTrue(errors, body)
                self.assertTrue(any("names no test fn" in e or "is not a file" in e for e in errors), errors)

    def test_a_missing_fn_fails(self):
        for name in ["no_such_test", "in_a_module_doc", "in_a_string", "commented_out", "a_plain"]:
            with self.subTest(name=name):
                errors = self.errors(f"{{/* test: src/daemon/tests.rs {name} */}}")
                self.assertEqual(len(errors), 1, errors)
                self.assertIn(f"declares no fn {name}", errors[0])

    def test_bad_paths_and_names_fail(self):
        for body, says in [
            ("src/daemon/nope.rs a_plain_test", "no path src/daemon/nope.rs"),
            ("a_plain_test", "names no path before it"),
            ("src/daemon/tests.rs nested::a_nested_test", "is not a fn name"),
            ("src/daemon/tests.rs a_plain_test()", "is not a fn name"),
            ("src/daemon/tests.rs fn", "is not a fn name"),
            ("src/daemon a_plain_test", "is not a file"),
            ("src/daemon/tests.rs (an open note", "parentheses"),
            ("src/daemon/tests.rs a_plain_test,, an_async_test", "an empty group"),
            # A non-test item that code: would accept is not a test.
            ("src/daemon/events.rs HIGH_RISK_WRITE_WINDOW", "declares no fn HIGH_RISK_WRITE_WINDOW"),
        ]:
            with self.subTest(body=body):
                errors = self.errors(f"{{/* test: {body} */}}")
                self.assertEqual(len(errors), 1, errors)
                self.assertIn(says, errors[0])
                self.assertIn("test: ", errors[0])

    def test_an_attribute_with_test_mid_path_is_not_a_test(self):
        self.write("src/daemon/mid.rs", "#[my::test::helper]\nfn not_a_test() {}\n")
        errors = self.errors("{/* test: src/daemon/mid.rs not_a_test */}")
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("is not a test", errors[0])

    def test_a_test_inside_a_macro_body_is_found(self):
        self.write("src/daemon/prop.rs", "proptest! {\n    #[test]\n    fn holds(x in 0..9u8) {}\n}\n")
        self.assertEqual(self.errors("{/* test: src/daemon/prop.rs holds */}"), [])

    def test_comment_lines_count_toward_the_window(self):
        far = "#[test]\n" + "// note\n" * (cda.ATTR_LINES + 1) + "fn far_below() {}\n"
        self.write("src/daemon/far2.rs", far)
        errors = self.errors("{/* test: src/daemon/far2.rs far_below */}")
        self.assertEqual(len(errors), 1, errors)

    def test_the_attribute_must_be_near(self):
        far = "#[test]\n" + "\n" * (cda.ATTR_LINES + 1) + "fn far_below() {}\n"
        self.write("src/daemon/far.rs", far)
        errors = self.errors("{/* test: src/daemon/far.rs far_below */}")
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("is not a test", errors[0])

    def test_attribute_paths(self):
        for path, is_test in [
            ("test", True), ("tokio::test", True), ("sqlx::test", True),
            ("cfg", False), ("testing", False), ("ignore", False),
            ("my::test::helper", False), ("test::helper", False),
        ]:
            with self.subTest(path=path):
                self.assertEqual(cda.is_test_attribute(path), is_test)


class NearMisses(Fixture):
    def test_near_miss_anchors_fail(self):
        for text in [
            "{/* lean : Demo.Status */}",
            "{/*lean: Demo.Status */}",
            "{/* Lean: Demo.Status */}",
            "{/* lean: Demo.Status*/}",
            "{ /* const: MIN_GC_SURVIVED */ }",
            "{/* Code: src/daemon/events.rs */}",
            "{/* orcale: Demo.json gate#0 */}",
            "{/* leans: Demo.Status */}",
            "{/* cnost: MIN_GC_SURVIVED */}",
            "{/* tests: src/daemon/tests.rs a_plain_test */}",
            "{/* Test: src/daemon/tests.rs a_plain_test */}",
            "{/*test: src/daemon/tests.rs a_plain_test */}",
            "{/* tset: src/daemon/tests.rs a_plain_test */}",
            "{/* lean: Demo.Status,\nDemo.Flags */}",
        ]:
            with self.subTest(text=text):
                errors = self.errors(text)
                self.assertEqual(len(errors), 1, errors)
                self.assertIn("starts like an anchor", errors[0])

    def test_other_comments_are_left_alone(self):
        self.assertEqual(self.errors(
            "{/* lambo-site-only:start */}\n{/* a note to editors */}\n{/* lambo-site-only:end */}\n"
        ), [])

    def test_near(self):
        self.assertTrue(cda.near("Lean", "lean"))
        self.assertTrue(cda.near("laen", "lean"))
        self.assertTrue(cda.near("oracl", "oracle"))
        self.assertTrue(cda.near("codes", "code"))
        self.assertTrue(cda.near("tests", "test"))
        self.assertTrue(cda.near("text", "test"))
        self.assertFalse(cda.near("lambo", "lean"))
        self.assertFalse(cda.near("a", "code"))


class Required(Fixture):
    ALL = (
        "{/* lean: Demo.Status */} {/* const: MIN_GC_SURVIVED */} "
        "{/* code: src/daemon/events.rs */}\n{/* test: src/daemon/tests.rs a_plain_test */}\n"
        "{/* oracle: Demo.json gate#0 */}\n"
    )

    def test_a_required_page_with_its_anchors_passes(self):
        with mock.patch.dict(cda.REQUIRED, {"site/src/content/docs/page.mdx": cda.KINDS}):
            self.assertEqual(self.errors(self.ALL), [])

    def test_a_required_page_that_lost_its_anchors_fails(self):
        with mock.patch.dict(cda.REQUIRED, {"site/src/content/docs/page.mdx": cda.KINDS}):
            errors = self.errors("All the anchors are gone.\n")
        self.assertEqual(len(errors), len(cda.KINDS))
        self.assertIn("carries no lean: anchor", errors[0])
        self.assertIn("carries no test: anchor", errors[3])

    def test_a_required_page_that_lost_one_kind_fails(self):
        with mock.patch.dict(cda.REQUIRED, {"site/src/content/docs/page.mdx": ("lean", "code")}):
            errors = self.errors("{/* lean: Demo.Status */}\n")
        self.assertEqual(errors, ["site/src/content/docs/page.mdx: carries no code: anchor, and REQUIRED says it must"])

    def test_a_required_page_that_is_gone_fails_a_full_run(self):
        self.page("No anchors here.\n")
        with mock.patch.dict(cda.REQUIRED, {"site/src/content/docs/gone.mdx": ("lean",)}):
            code, err = self.main()
        self.assertEqual(code, 1)
        self.assertIn("gone.mdx: REQUIRED to carry anchors, but there is no such page", err)


class ExitCodes(Fixture):
    def test_pass_is_0(self):
        self.assertEqual(self.main(self.page("{/* lean: Demo.Status */}\n"))[0], 0)

    def test_an_unresolved_anchor_is_1(self):
        code, err = self.main(self.page("{/* lean: Demo.nope */}\n"))
        self.assertEqual(code, 1)
        self.assertIn("1 error(s)", err)

    def test_missing_page_is_2(self):
        self.assertEqual(self.main(self.docs / "nope.mdx")[0], 2)

    def test_bad_arguments_are_2(self):
        self.assertEqual(self.main("--no-such-flag")[0], 2)

    def test_non_utf8_page_is_3(self):
        code, err = self.main(self.page(b"\xff\xfe {/* lean: Demo.Status */}"))
        self.assertEqual(code, 3)
        self.assertIn("not UTF-8", err)

    def test_non_utf8_source_is_3(self):
        self.write("src/daemon/events.rs", b"\xff\xfe")
        code, err = self.main(self.page("{/* code: src/daemon/events.rs is_high_value */}\n"))
        self.assertEqual(code, 3)
        self.assertIn("src/daemon/events.rs: not UTF-8", err)

    def test_default_pages_include_the_reference_copies(self):
        self.write("docs/reference/page.mdx", "{/* lean: Demo.nope */}\n")
        code, err = self.main()
        self.assertEqual(code, 1)
        self.assertIn("docs/reference/page.mdx:1: lean: Demo.nope", err)


class ScannerTest(unittest.TestCase):
    def test_lean_string_holding_a_comment_opener_starts_no_comment(self):
        names = cda.lean_names(
            'namespace Lambo.M\n'
            'def marker : String := "/-"\n'
            'def kept : Nat := 0\n'
            'def esc : String := "a\\"/-"\n'
            'def kept2 : Nat := 1\n'
            'end Lambo.M\n'
        )
        self.assertIn("Lambo.M.kept", names)
        self.assertIn("Lambo.M.kept2", names)

    def test_rust_nested_block_comment_hides_its_declarations(self):
        code = cda.rust_code("/* outer /* inner */ fn removed() {} */\nfn kept() {}\n")
        self.assertFalse(cda.declares(code, "removed"))
        self.assertTrue(cda.declares(code, "kept"))

    def test_rust_strings_raw_strings_and_chars_hide_their_text(self):
        code = cda.rust_code(
            'let s = "fn fake() {}"; let r = r#"fn raw() {}"#; let c = \'"\'; fn after() {}\n'
            "fn last<'a>(x: &'a str) {}\n"
        )
        for hidden in ("fake", "raw"):
            self.assertFalse(cda.declares(code, hidden), hidden)
        for kept in ("after", "last"):
            self.assertTrue(cda.declares(code, kept), kept)


if __name__ == "__main__":
    unittest.main()
