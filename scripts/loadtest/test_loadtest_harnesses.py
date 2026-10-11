#!/usr/bin/env python3
"""Unit tests for the pure parts of mcp_concurrent.py and replay_parity.py.

Stdlib only. No server, no GPU, no network, no live store: every test works on
literals or a temp dir.

    python3 -m unittest scripts/loadtest/test_loadtest_harnesses.py
    python3 scripts/loadtest/test_loadtest_harnesses.py
"""

from __future__ import annotations

import io
import json
import pathlib
import sys
import tempfile
import unittest
from contextlib import redirect_stderr

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import mcp_concurrent as mc  # noqa: E402
import replay_parity as rp  # noqa: E402


class ParseMix(unittest.TestCase):
    def test_default_mix_maps_short_names_to_tools(self):
        mix = dict(mc.parse_mix(mc.DEFAULT_MIX))
        self.assertEqual(mix, {"lambo_recall": 40, "lambo_derive": 38,
                               "lambo_record_action": 18, "lambo_stats": 4})

    def test_prefixed_names_and_zero_weights_are_accepted(self):
        self.assertEqual(mc.parse_mix("lambo_derive=1, recall=0"),
                         [("lambo_derive", 1.0), ("lambo_recall", 0.0)])

    def test_bad_specs_are_refused(self):
        for spec in ("", "recall", "erase=3", "recall=-1", "recall=x",
                     "recall=1,recall=2", "recall=0,derive=0"):
            with self.subTest(spec=spec), self.assertRaises(ValueError):
                mc.parse_mix(spec)


class Determinism(unittest.TestCase):
    def test_write_payloads_are_pure_in_seed_worker_and_call(self):
        a = mc.derive_concepts(7, 2, 5, "single writer lease TTL")
        self.assertEqual(a, mc.derive_concepts(7, 2, 5, "single writer lease TTL"))
        self.assertNotEqual(a, mc.derive_concepts(8, 2, 5, "single writer lease TTL"))
        self.assertEqual([c["concept_type"] for c in a], ["observation", "logic"])
        self.assertEqual(mc.action_args(1, 0, 3, "q" * 60, 9)["action"],
                         "conc-s1-w0-3 touched " + "q" * 40)

    def test_empty_query_still_yields_a_head_word(self):
        self.assertIn(" on x handling", mc.derive_concepts(1, 0, 1, "")[1]["content"])

    def test_sample_is_seeded_ordered_and_bounded(self):
        items = list(range(100))
        s = mc.pick_sample(items, 10, 3)
        self.assertEqual(s, mc.pick_sample(items, 10, 3))
        self.assertEqual(len(s), 10)
        self.assertEqual(s, sorted(s))
        self.assertEqual(mc.pick_sample(items, 0, 3), items)
        self.assertEqual(mc.pick_sample(items, 500, 3), items)


class Reasons(unittest.TestCase):
    def test_numbers_paths_urls_and_prefix_are_normalized(self):
        a = mc.normalize_reason("FAILED, nothing was written — internal error after 8 "
                                "changes at /var/x/y.db via postgres://u@h/db (lwr1.ab.c)")
        self.assertEqual(a, "internal error after N changes at <path> via <url> (<receipt>")
        self.assertEqual(mc.normalize_reason(None), "(no reason given)")
        self.assertEqual(mc.normalize_reason("  "), "(no reason given)")
        self.assertLessEqual(len(mc.normalize_reason("x" * 500)), 160)

    def test_receipt_outcome(self):
        failed = {"receipt": {"state": "failed",
                              "detail": "FAILED, nothing was written — conflict (the detail "
                                        "was logged server-side)"}}
        self.assertEqual(mc.receipt_outcome(failed, False),
                         {"state": "failed",
                          "reason": "conflict (the detail was logged server-side)"})
        self.assertEqual(mc.receipt_outcome({"receipt": {"state": "applied"}}, False),
                         {"state": "applied", "reason": None})
        self.assertEqual(mc.receipt_outcome(None, True, "boom")["state"], "query_error")
        self.assertEqual(mc.receipt_outcome({}, False)["state"], "unknown")

    def test_summary_rate_counts_only_settled_receipts(self):
        out = [{"state": "applied", "reason": None}] * 6 + \
              [{"state": "failed", "reason": "conflict"}] * 3 + \
              [{"state": "dropped", "reason": "queue full"}] + \
              [{"state": "pending", "reason": None}, {"state": "expired", "reason": None}]
        s = mc.summarize_writes(14, ["bad params"] * 2, out, ["hybrid derive x"] * 3)
        self.assertEqual(s["settled"], 10)
        self.assertEqual(s["failed"], 4)
        self.assertEqual(s["failure_rate"], 0.4)
        self.assertEqual(s["failed_reasons"], {"failed: conflict": 3,
                                               "dropped: queue full": 1})
        self.assertEqual(s["refused_at_ack"], 2)
        self.assertEqual(s["failed_reasons_server_ledger"], {"hybrid derive x": 3})
        self.assertIsNone(mc.summarize_writes(0, [], [])["failure_rate"])
        self.assertNotIn("failed_reasons_server_ledger", mc.summarize_writes(0, [], []))

    def test_ledger_reasons_keep_only_our_failed_completions(self):
        lines = [
            json.dumps({"kind": "completion", "receipt": "r1", "state": "failed",
                        "error": "hybrid derive could not commit after 8 concurrent "
                                 "graph changes"}),
            json.dumps({"kind": "completion", "receipt": "r2", "state": "applied"}),
            json.dumps({"kind": "completion", "receipt": "other", "state": "failed",
                        "error": "x"}),
            "not json \"completion\"",
            json.dumps({"kind": "call", "tool": "lambo_derive"}),
        ]
        self.assertEqual(mc.ledger_failure_reasons(lines, {"r1", "r2"}),
                         ["hybrid derive could not commit after N concurrent graph changes"])

    def test_drain_done(self):
        self.assertTrue(mc.drain_done({"write_queue_outstanding": 0}))
        self.assertFalse(mc.drain_done({"write_queue_outstanding": 0, "flush_depth": 2}))


class ConcurrentCli(unittest.TestCase):
    def run_main(self, *argv):
        err = io.StringIO()
        with redirect_stderr(err), self.assertRaises(SystemExit) as cm:
            mc.main(list(argv))
        return str(cm.exception.code)

    def test_production_port_is_refused_before_any_connection(self):
        for url in ("http://127.0.0.1:7700/mcp", "http://127.0.0.1:07700/mcp"):
            with self.subTest(url=url):
                msg = self.run_main("--endpoint", url, "--out", "/nonexistent/x.json")
                self.assertIn("refusing", msg)
                self.assertIn("--allow-production-port", msg)

    def test_bad_port_and_bad_mix_are_usage_errors(self):
        self.assertIn("invalid port", self.run_main("--endpoint", "http://h:x/mcp",
                                                    "--out", "/nonexistent/x.json"))
        self.assertIn("--mix", self.run_main("--endpoint", "http://127.0.0.1:17701/mcp",
                                             "--mix", "erase=1", "--out", "/nonexistent/x"))


# ---------------------------------------------------------------------------
# replay_parity
# ---------------------------------------------------------------------------

class SplitPrompt(unittest.TestCase):
    def test_plain_split(self):
        self.assertEqual(rp.split_prompt("a; b; c", []), ["a", "b", "c"])

    def test_known_content_containing_the_separator_stays_whole(self):
        self.assertEqual(rp.split_prompt("x; y; z; w", ["y; z"]), ["x", "y; z", "w"])

    def test_longest_known_content_wins(self):
        self.assertEqual(rp.split_prompt("a; b; c", ["a", "a; b"]), ["a; b", "c"])

    def test_known_prefix_not_ending_at_a_boundary_is_ignored(self):
        self.assertEqual(rp.split_prompt("ab; c", ["a"]), ["ab", "c"])

    def test_single_and_empty_prompt(self):
        self.assertEqual(rp.split_prompt("only", []), ["only"])
        self.assertEqual(rp.split_prompt("", []), [""])


def row(state="applied", **kw):
    base = {"state": state, "created_count": 1, "matched_count": 0, "semantic_merged": 0,
            "reinforced": 0, "edges": None, "embedded": 1}
    base.update(kw)
    return base


class Classify(unittest.TestCase):
    T = "2026-10-08T15:04:00Z"
    cat = rp.REASONS_0_4_0

    def cls(self, call, sa, sb, ra=None, rb=None):
        return rp.classify(call, sa, sb, ra or {}, rb or {}, self.T, self.cat)

    def test_record_action_receipt_gaining_embedded(self):
        call = {"kind": "record_action", "arguments": {}}
        self.assertEqual(self.cls(call, row(embedded=None), row(embedded=2)),
                         ["receipt_gains_embedded"])

    def test_parent_of_ends_embedded(self):
        call = {"kind": "derive", "parent_of_only_ends_created": 2,
                "arguments": {"parent_of": [{"parent": "p", "child": "c"}]}}
        self.assertEqual(self.cls(call, row(embedded=1), row(embedded=3)),
                         ["parent_of_ends_embedded"])

    def test_merge_into_a_replayed_concept_versus_pre_t(self):
        call = {"kind": "derive", "arguments": {}}
        fresh = {"semantic_merges": [{"from": "a", "to": "b",
                                      "to_created_at": "2026-10-08T15:05:00Z"}]}
        old = {"semantic_merges": [{"from": "a", "to": "b",
                                    "to_created_at": "2026-10-01T00:00:00Z"}]}
        sa, sb = row(semantic_merged=1), row(semantic_merged=0)
        self.assertEqual(self.cls(call, sa, sb, fresh, {}), ["merge_into_replayed_concept"])
        self.assertEqual(self.cls(call, sa, sb, old, {}), ["merge_differs_pre_t"])

    def test_invisible_codepoints_are_detected_in_raw_text(self):
        # json.dumps escapes U+200B by default; the detector must see the raw
        # character (the evidence copy dumped with ensure_ascii=True and so
        # could never match).
        call = {"kind": "derive", "arguments": {"concepts": [{"content": "a​b"}]}}
        self.assertEqual(self.cls(call, row(matched_count=1), row(matched_count=0)),
                         ["canonical_key_invisible"])

    def test_unexplained_differences_are_unexpected(self):
        derive = {"kind": "derive", "arguments": {}}
        action = {"kind": "record_action", "arguments": {}}
        self.assertEqual(self.cls(derive, row(matched_count=1), row(matched_count=0)),
                         ["canonical_match_differs"])
        self.assertEqual(self.cls(action, row(edges=2), row(edges=3)), ["action_edges_differ"])
        self.assertEqual(self.cls(derive, row(), row(created_count=2)), ["unclassified"])
        for rid in ("canonical_match_differs", "action_edges_differ", "unclassified",
                    "state_differs", "no_such_reason"):
            self.assertFalse(rp.resolve_reason(rid, self.cat)["expected"], rid)

    def test_state_difference_resolved_by_refusal_text(self):
        call = {"kind": "derive", "arguments": {}}
        rb = {"detail": "derived text ends with an [image:<id>] suffix"}
        self.assertEqual(self.cls(call, row(), row(state="refused"), {}, rb),
                         ["state_differs:image_suffix"])
        self.assertEqual(self.cls(call, row(), row(state="refused"), {}, {"detail": "??"}),
                         ["state_differs"])
        r = rp.resolve_reason("state_differs:image_suffix", self.cat)
        self.assertTrue(r["expected"])
        self.assertIn("#22", r["ref"])

    def test_reinforced_cascade_only_when_nothing_else_fired(self):
        call = {"kind": "derive", "arguments": {}}
        self.assertEqual(self.cls(call, row(reinforced=1), row(reinforced=2)),
                         ["reinforced_cascade"])


class Catalogue(unittest.TestCase):
    def write(self, d):
        tmp = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
        json.dump(d, tmp)
        tmp.close()
        self.addCleanup(pathlib.Path(tmp.name).unlink)
        return tmp.name

    def test_default_is_the_builtin_0_4_0_mapping(self):
        cat, desc = rp.load_catalogue(None)
        self.assertEqual(cat, rp.REASONS_0_4_0)
        self.assertIn("0.4.0", desc)

    def test_based_file_overrides_and_extends(self):
        path = self.write({"base": "0.4.0", "reasons": {
            "receipt_gains_embedded": {"label": "retired", "expected": False},
            "state_differs:lease": {"label": "new lease rule", "kind": "state_differs",
                                    "detail_pattern": "lease", "ref": "0.4.1 #116"}}})
        cat, _ = rp.load_catalogue(path)
        self.assertFalse(rp.resolve_reason("receipt_gains_embedded", cat)["expected"])
        self.assertTrue(rp.resolve_reason("state_differs:lease", cat)["expected"])
        self.assertIn("merge_differs_pre_t", cat)
        self.assertEqual(rp.state_reason(cat, "lease lost"), "state_differs:lease")

    def test_unbased_file_replaces(self):
        cat, _ = rp.load_catalogue(self.write({"reasons": {"x": {"label": "x"}}}))
        self.assertEqual(set(cat), {"x"})
        self.assertFalse(rp.resolve_reason("receipt_gains_embedded", cat)["expected"])

    def test_malformed_files_are_refused(self):
        for d in ({}, {"reasons": {"x": {}}}, {"base": "9.9", "reasons": {}},
                  {"reasons": {"x": {"label": "x", "detail_pattern": "("}}}):
            with self.subTest(d=d), self.assertRaises((SystemExit, Exception)):
                rp.load_catalogue(self.write(d))


class GroupingAndContent(unittest.TestCase):
    def cmp_(self):
        reasons = [rp.resolve_reason("merge_differs_pre_t", rp.REASONS_0_4_0),
                   rp.resolve_reason("unclassified", rp.REASONS_0_4_0)]
        d1 = {"idx": 0, "same": False, "reasons": reasons[:1], "unexpected": False,
              "detail": {"created_only_a": ["secret text"]}}
        d2 = {"idx": 1, "same": False, "reasons": reasons, "unexpected": True,
              "detail": {"matched_a": ["secret text"]}}
        same = {"idx": 2, "same": True}
        return {"per_call": [d1, d2, same], "differing": [d1, d2],
                "reasons": rp.group_reasons([d1, d2]), "unexpected_calls": 1,
                "content_diff": {
                    "concepts_created": {"only_a": [{"content": "secret text"}], "only_b": []},
                    "semantic_edges": {"only_a": [], "only_b": [{"from": "s", "to": "t"}]},
                    "unembedded_new": {"a": 0, "b": 0}}}

    def test_group_reasons_counts_and_flags(self):
        g = self.cmp_()["reasons"]
        self.assertEqual(g["merge_differs_pre_t"]["n"], 2)
        self.assertTrue(g["merge_differs_pre_t"]["expected"])
        self.assertEqual(g["unclassified"]["n"], 1)
        self.assertFalse(g["unclassified"]["expected"])
        self.assertEqual(list(g), ["merge_differs_pre_t", "unclassified"])

    def test_strip_content_leaves_no_store_text(self):
        s = rp.strip_content(self.cmp_())
        self.assertNotIn("secret text", json.dumps(s))
        self.assertEqual(s["content_diff"]["concepts_created"], {"only_a": 1, "only_b": 0})
        self.assertEqual(s["content_diff"]["semantic_edges"], {"only_a": 0, "only_b": 1})
        self.assertEqual([d["idx"] for d in s["differing"]], [0, 1])
        self.assertEqual(s["unexpected_calls"], 1)


class PathSafety(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp(prefix="rp-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(self.tmp, ignore_errors=True))
        self.live = self.tmp / "live"
        self.live.mkdir()

    def test_outputs_under_a_protected_dir_are_refused(self):
        snap = self.live / "backup.db"
        for out, work in ((self.live / "o", self.tmp / "w"), (self.tmp / "o", self.live / "w")):
            with self.subTest(out=out, work=work), self.assertRaises(SystemExit):
                rp.check_paths(out, work, snap, [self.live], False)
        # The snapshot itself may sit under a protected dir: it is read-only.
        rp.check_paths(self.tmp / "o", self.tmp / "w", snap, [self.live], False)

    def test_live_store_name_needs_the_flag(self):
        snap = self.tmp / "lambo-dev.db"
        with self.assertRaises(SystemExit):
            rp.check_paths(self.tmp / "o", self.tmp / "w", snap, [], False)
        rp.check_paths(self.tmp / "o", self.tmp / "w", snap, [], True)

    def test_rewritten_config_may_not_point_under_a_protected_dir(self):
        tpl = self.tmp / "t.toml"
        tpl.write_text('[store]\nkind = "sqlite"\npath = "/somewhere/else.db"\n')
        ok = self.tmp / "ok.toml"
        rp.write_config(tpl, self.tmp / "replay.db", ok, 5, 9, [self.live])
        text = ok.read_text()
        self.assertIn(f'path = "{self.tmp / "replay.db"}"', text)
        self.assertIn("canonization_eval_interval_secs = 5", text)
        with self.assertRaises(SystemExit):
            rp.write_config(tpl, self.live / "replay.db", self.tmp / "bad.toml", 5, 9,
                            [self.live])

    def test_template_without_a_store_path_is_refused(self):
        tpl = self.tmp / "t.toml"
        tpl.write_text('[embedder]\nkind = "fixture"\n')
        with self.assertRaises(SystemExit):
            rp.write_config(tpl, self.tmp / "r.db", self.tmp / "c.toml", 5, 9, [])


if __name__ == "__main__":
    unittest.main()
