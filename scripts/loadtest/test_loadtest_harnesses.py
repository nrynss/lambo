#!/usr/bin/env python3
"""Unit tests for the pure parts of mcp_concurrent.py and replay_parity.py.

Stdlib only. No server, no GPU, no network, no live store: every test works on
literals, a fake in-process MCP client, or a temp dir.

    python3 -m unittest scripts/loadtest/test_loadtest_harnesses.py
    python3 scripts/loadtest/test_loadtest_harnesses.py
"""

from __future__ import annotations

import argparse
import io
import json
import os
import pathlib
import shutil
import sqlite3
import stat
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import mcp_concurrent as mc  # noqa: E402
import replay_parity as rp  # noqa: E402

# Exact server texts (see the citations in mcp_concurrent.KNOWN_MESSAGES).
IMAGE_SUFFIX_TEXT = ("concepts[0].content may not contain an image suffix ([image:<id>], in "
                     "any case or spelling): that suffix names an image concept, which only "
                     "lambo_derive_image creates.")
REPLAN_TEXT = "hybrid derive could not commit after 8 concurrent graph changes"
CONTEXT_TEXT = ("concepts[1] framed with the call's text for embedding (the text, \" — \", "
                "then the call's concepts joined by \"; \") is 9000 bytes; at most 16384 "
                "bytes fit, so a concept sent alone may be at most 8189 bytes")


def tmpdir(test):
    d = pathlib.Path(tempfile.mkdtemp(prefix="lt-test-"))
    test.addCleanup(shutil.rmtree, d, True)
    return d


# ---------------------------------------------------------------------------
# mcp_concurrent
# ---------------------------------------------------------------------------

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
    def test_raw_normalization_folds_numbers_paths_urls_and_prefix(self):
        a = mc.normalize_reason("FAILED, nothing was written — internal error after 8 "
                                "changes at /var/x/y.db via postgres://u@h/db (lwr1.ab.c)")
        self.assertEqual(a, "internal error after N changes at <path> via <url> (<receipt>")
        self.assertEqual(mc.normalize_reason(None), "(no reason given)")
        self.assertLessEqual(len(mc.normalize_reason("x" * 500)), 160)

    def test_redacted_reason_is_a_known_id_or_a_class_never_text(self):
        self.assertEqual(mc.reason_key(REPLAN_TEXT), "hybrid_replan_exhausted")
        self.assertEqual(mc.reason_key(IMAGE_SUFFIX_TEXT), "image_suffix_refused")
        self.assertEqual(mc.reason_key(CONTEXT_TEXT), "hybrid_embed_context_cap")
        self.assertEqual(mc.reason_key("FAILED, nothing was written — conflict (the detail "
                                       "was logged server-side)"), "conflict")
        self.assertEqual(mc.reason_key("secret store content at /home/me/x.db"),
                         "unrecognised")
        self.assertEqual(mc.reason_key(""), "(no reason given)")
        # A loose keyword in echoed content is not a known message.
        self.assertEqual(mc.reason_key("docker image: disk I/O error"), "unrecognised")
        self.assertEqual(mc.reason_key("secret at /home/me/x", raw=True), "secret at <path>")

    def test_receipt_outcome(self):
        failed = {"receipt": {"state": "failed",
                              "detail": "FAILED, nothing was written — conflict (the detail "
                                        "was logged server-side)"}}
        self.assertEqual(mc.receipt_outcome(failed, False),
                         {"state": "failed", "reason": "conflict"})
        self.assertEqual(mc.receipt_outcome({"receipt": {"state": "applied"}}, False),
                         {"state": "applied", "reason": None})
        self.assertEqual(mc.receipt_outcome(None, True, "boom")["state"], "query_error")
        self.assertEqual(mc.receipt_outcome({}, False)["state"], "unknown")
        self.assertEqual(mc.receipt_outcome({"receipt": {"state": 3}}, False)["state"],
                         "unknown")

    def test_summary_rates(self):
        out = [{"state": "applied", "reason": None}] * 6 + \
              [{"state": "failed", "reason": "conflict"}] * 3 + \
              [{"state": "dropped", "reason": "drop_queue_full"}] + \
              [{"state": "pending", "reason": None}, {"state": "expired", "reason": None}]
        s = mc.summarize_writes(14, ["unrecognised"] * 2, out, ["hybrid_replan_exhausted"] * 3,
                                transport_failed=3)
        self.assertEqual(s["settled"], 10)
        self.assertEqual(s["failed"], 4)
        self.assertEqual(s["failure_rate"], 0.4)
        # (4 failed + 2 refused + 3 transport) / (10 settled + 2 + 3)
        self.assertEqual(s["write_failure_rate_all"], 0.6)
        self.assertEqual(s["failed_reasons"], {"failed: conflict": 3,
                                               "dropped: drop_queue_full": 1})
        self.assertEqual(s["refused_at_ack"], 2)
        self.assertEqual(s["transport_failed"], 3)
        self.assertEqual(s["failed_reasons_server_ledger"], {"hybrid_replan_exhausted": 3})
        empty = mc.summarize_writes(0, [], [])
        self.assertIsNone(empty["failure_rate"])
        self.assertIsNone(empty["write_failure_rate_all"])
        self.assertNotIn("failed_reasons_server_ledger", empty)
        # Only refusals: the settled-only rate is undefined, the all-in rate is 1.
        only_refused = mc.summarize_writes(0, ["conflict"], [])
        self.assertIsNone(only_refused["failure_rate"])
        self.assertEqual(only_refused["write_failure_rate_all"], 1.0)

    def test_ledger_reasons_keep_only_our_failed_completions(self):
        lines = [
            json.dumps({"kind": "completion", "receipt": "r1", "state": "failed",
                        "error": REPLAN_TEXT}),
            json.dumps({"kind": "completion", "receipt": "r2", "state": "applied"}),
            json.dumps({"kind": "completion", "receipt": "other", "state": "failed",
                        "error": "x"}),
            "not json \"completion\"",
            json.dumps({"kind": "call", "tool": "lambo_derive"}),
        ]
        self.assertEqual(mc.ledger_failure_reasons(lines, {"r1", "r2"}),
                         ["hybrid_replan_exhausted"])
        self.assertEqual(mc.ledger_failure_reasons(lines, {"r1"}, raw=True),
                         ["hybrid derive could not commit after N concurrent graph changes"])

    def test_drain_requires_all_three_counters_as_zero_ints(self):
        full = {"write_queue_outstanding": 0, "log_depth": 0, "flush_depth": 0}
        self.assertTrue(mc.drain_done(full))
        self.assertFalse(mc.drain_done({"node_count": 3}))
        self.assertFalse(mc.drain_done({"write_queue_outstanding": 0}))
        self.assertFalse(mc.drain_done({**full, "flush_depth": 2}))
        self.assertFalse(mc.drain_done({**full, "log_depth": "0"}))
        self.assertFalse(mc.drain_done({**full, "log_depth": None}))
        self.assertFalse(mc.drain_done({**full, "log_depth": False}))
        self.assertFalse(mc.drain_done(None))

    def test_retention_flags(self):
        f = mc.retention_flags([{"state": "expired"}, {"state": "applied"}], 120, 10)
        self.assertEqual(f["expired_receipts"], 1)
        self.assertFalse(f["retention_risk"])
        self.assertTrue(mc.retention_flags([], 120, 200)["retention_risk"])

    def test_exit_problems(self):
        self.assertEqual(mc.exit_problems(True, 5, 0), [])
        self.assertEqual(len(mc.exit_problems(False, 0, 2)), 3)


class FakeClient:
    """Stands in for mcp_concurrent.Client: no network. Worker conc-0 raises
    on its first derive; derives get receipts r<n>; every third receipt
    failed; stats is drained."""

    n = 0

    def __init__(self, url, agent_id, token):
        self.agent_id = agent_id

    def call(self, name, **args):
        if name == "lambo_derive" and self.agent_id == "conc-0":
            raise AttributeError("'list' object has no attribute 'get'")
        if name in mc.WRITE_TOOLS:
            FakeClient.n += 1
            return 1.0, mc.OK, {"receipt": f"r{FakeClient.n}"}, ""
        if name == "lambo_stats" and "receipt" in args:
            k = int(args["receipt"][1:])
            st = "failed" if k % 3 == 0 else "applied"
            return 1.0, mc.OK, {"receipt": {"state": st, "detail": REPLAN_TEXT}}, ""
        if name == "lambo_stats":
            return 1.0, mc.OK, {"write_queue_outstanding": 0, "log_depth": 0,
                                "flush_depth": 0}, ""
        return 2.0, mc.OK, {"hits": []}, ""


class ConcurrentRun(unittest.TestCase):
    def opts(self, **kw):
        base = dict(endpoint="http://fake", workers=3, secs=0.3, think=0.0, seed=1,
                    drain_timeout=1.0, receipt_max=0, server_ledger=None,
                    include_raw_reasons=False)
        base.update(kw)
        return argparse.Namespace(**base)

    def test_a_dying_worker_is_counted_and_fails_the_run(self):
        FakeClient.n = 0
        with mock.patch.object(mc, "Client", FakeClient):
            res = mc.run(self.opts(), ["q one"],
                         [("lambo_derive", 1.0)], None)
        self.assertEqual(res["worker_errors"], 1)
        self.assertEqual(res["worker_error_kinds"], {"AttributeError": 1})
        self.assertTrue(res["drain"]["drained"])
        self.assertGreater(res["writes"]["acked"], 0)
        self.assertEqual(res["writes"]["failed_reasons"],
                         {"failed: hybrid_replan_exhausted": res["writes"]["failed"]})
        self.assertTrue(mc.exit_problems(res["drain"]["drained"], res["ok_calls"],
                                         res["worker_errors"]))

    def test_latency_counts_ok_calls_and_errors_separately(self):
        class Erring(FakeClient):
            def call(self, name, **args):
                if name == "lambo_recall":
                    return 9.0, mc.TOOL_ERROR, None, "conflict"
                return super().call(name, **args)
        with mock.patch.object(mc, "Client", Erring):
            res = mc.run(self.opts(workers=1, secs=0.2), ["q"],
                         [("lambo_recall", 1.0), ("lambo_stats", 1.0)], None)
        self.assertNotIn("lambo_recall", res["latency_ms"])
        self.assertGreater(res["errors"]["lambo_recall"]["count"], 0)
        self.assertEqual(res["errors"]["lambo_recall"]["latency_ms"]["p50"], 9.0)
        self.assertEqual(res["calls"], res["ok_calls"] + res["errors"]["lambo_recall"]["count"])


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

    def test_undrained_run_exits_2_with_the_json_written(self):
        out = tmpdir(self) / "c.json"
        fake = {"drain": {"drained": False, "drain_s": 1.0}, "ok_calls": 3, "worker_errors": 0,
                "wall_s": 1.0, "flags": {"expired_receipts": 0, "retention_risk": False}}
        with mock.patch.object(mc, "run", return_value=fake), \
                redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            rc = mc.main(["--endpoint", "http://127.0.0.1:17701/mcp", "--out", str(out)])
        self.assertEqual(rc, mc.EXIT_INCOMPLETE)
        self.assertTrue(json.loads(out.read_text())["problems"])


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

    def cls(self, call, sa, sb, ra=None, rb=None, prior=False):
        return rp.classify(call, sa, sb, ra or {}, rb or {}, self.T, self.cat, prior)

    def unexpected(self, ids):
        return [i for i in ids if not rp.resolve_reason(i, self.cat)["expected"]]

    def test_record_action_receipt_gaining_embedded(self):
        call = {"kind": "record_action", "arguments": {}}
        self.assertEqual(self.cls(call, row(embedded=None), row(embedded=2)),
                         ["receipt_gains_embedded"])

    def test_an_expected_reason_cannot_mask_an_edge_difference(self):
        call = {"kind": "record_action", "arguments": {}}
        ids = self.cls(call, row(embedded=None, edges=3), row(embedded=1, edges=1))
        self.assertIn("receipt_gains_embedded", ids)
        self.assertEqual(self.unexpected(ids), ["action_edges_differ"])

    def test_an_expected_reason_cannot_mask_created_or_reinforced(self):
        call = {"kind": "record_action", "arguments": {}}
        ids = self.cls(call, row(embedded=None, created_count=1, reinforced=0),
                       row(embedded=1, created_count=2, reinforced=1))
        self.assertIn("receipt_gains_embedded", ids)
        self.assertEqual(sorted(self.unexpected(ids)), ["created_differs", "reinforced_differs"])

    def test_created_count_alone_is_unexpected(self):
        call = {"kind": "derive", "arguments": {}}
        self.assertEqual(self.cls(call, row(), row(created_count=2)), ["created_differs"])
        ids = self.cls(call, row(), row(), {"created": ["a"]}, {"created": ["b"]})
        self.assertEqual(ids, ["created_differs"])

    def test_a_field_no_detector_covers_is_unexplained(self):
        call = {"kind": "derive", "arguments": {}}
        ids = self.cls(call, row(edges=1), row(edges=2))
        self.assertEqual(ids, ["unexplained:edges"])
        self.assertFalse(rp.resolve_reason(ids[0], self.cat)["expected"])

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
        sa, sb = row(semantic_merged=1, created_count=0), row(semantic_merged=0)
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
        for rid in ("canonical_match_differs", "action_edges_differ", "unclassified",
                    "state_differs", "created_differs", "reinforced_differs",
                    "no_such_reason"):
            self.assertFalse(rp.resolve_reason(rid, self.cat)["expected"], rid)

    def test_state_difference_resolved_by_exact_refusal_text(self):
        call = {"kind": "derive", "arguments": {}}
        refused = row(state="refused")
        self.assertEqual(self.cls(call, row(), refused, {}, {"detail": IMAGE_SUFFIX_TEXT}),
                         ["state_differs:image_suffix"])
        self.assertEqual(self.cls(call, row(), refused, {}, {"detail": CONTEXT_TEXT}),
                         ["state_differs:embed_context_cap"])
        r = rp.resolve_reason("state_differs:image_suffix", self.cat)
        self.assertTrue(r["expected"])
        self.assertIn("#22", r["ref"])

    def test_loose_keywords_in_refusal_text_do_not_match(self):
        call = {"kind": "derive", "arguments": {}}
        for text in ("docker image: disk I/O error", "context too large", "too many things",
                     "exceeds 4 MiB", "[IMAGE:x]", "see [image: diagram]"):
            with self.subTest(text=text):
                self.assertEqual(self.cls(call, row(), row(state="failed"), {},
                                          {"detail": text}), ["state_differs"])

    def test_state_reason_checks_direction(self):
        call = {"kind": "derive", "arguments": {}}
        # A refused, B applied: the b_refuses image entry must not fire.
        self.assertEqual(self.cls(call, row(state="refused"), row(),
                                  {"detail": IMAGE_SUFFIX_TEXT}, {}), ["state_differs"])
        # Both not applied (refused vs failed): not b_refuses either.
        self.assertEqual(self.cls(call, row(state="failed"), row(state="refused"), {},
                                  {"detail": IMAGE_SUFFIX_TEXT}), ["state_differs"])

    def test_reinforced_cascade_needs_an_earlier_difference(self):
        call = {"kind": "derive", "arguments": {}}
        self.assertEqual(self.cls(call, row(reinforced=1), row(reinforced=2), prior=True),
                         ["reinforced_cascade"])
        self.assertEqual(self.cls(call, row(reinforced=1), row(reinforced=2)),
                         ["reinforced_differs"])


class Catalogue(unittest.TestCase):
    def write(self, d):
        p = tmpdir(self) / "r.json"
        p.write_text(json.dumps(d))
        return str(p)

    def test_default_is_the_builtin_0_4_0_mapping(self):
        cat, desc = rp.load_catalogue(None)
        self.assertEqual(cat, rp.REASONS_0_4_0)
        self.assertIn("0.4.0", desc)

    def test_based_file_overrides_and_extends(self):
        path = self.write({"base": "0.4.0", "reasons": {
            "receipt_gains_embedded": {"label": "retired", "expected": False},
            "state_differs:lease": {"label": "new lease rule", "kind": "state_differs",
                                    "detail_pattern": "lease lost", "expected": True,
                                    "ref": "0.4.1 #116"}}})
        cat, _ = rp.load_catalogue(path)
        self.assertFalse(rp.resolve_reason("receipt_gains_embedded", cat)["expected"])
        self.assertTrue(rp.resolve_reason("state_differs:lease", cat)["expected"])
        self.assertIn("merge_differs_pre_t", cat)
        self.assertEqual(rp.state_reason(cat, "lease lost", "applied", "refused"),
                         "state_differs:lease")
        self.assertEqual(rp.state_reason(cat, "lease lost", "refused", "applied"),
                         "state_differs")

    def test_file_without_base_replaces(self):
        cat, _ = rp.load_catalogue(self.write({"reasons": {"x": {"label": "x",
                                                                 "expected": True}}}))
        self.assertEqual(set(cat), {"x"})
        self.assertFalse(rp.resolve_reason("receipt_gains_embedded", cat)["expected"])

    def test_malformed_files_are_refused(self):
        bad = [
            {},
            {"reasons": {"x": {}}},
            {"reasons": {"x": {"label": "x"}}},                         # expected missing
            {"reasons": {"x": {"label": "x", "expected": "false"}}},    # string, not bool
            {"reasons": {"x": {"label": "x", "expected": 1}}},
            {"base": "9.9", "reasons": {}},
            {"reasons": {"state_differs:y": {"label": "y", "expected": True,
                                             "detail_pattern": "y"}}},   # kind missing
            {"reasons": {"y": {"label": "y", "expected": True, "kind": "state_differs"}}},
            {"reasons": {"y": {"label": "y", "expected": True, "kind": "state_differs",
                               "detail_pattern": "("}}},
            {"reasons": {"y": {"label": "y", "expected": True, "kind": "state_differs",
                               "detail_pattern": "y", "direction": "sideways"}}},
            {"reasons": {"y": {"label": "y", "expected": True, "kind": "other"}}},
        ]
        for d in bad:
            with self.subTest(d=d), self.assertRaises(SystemExit):
                rp.load_catalogue(self.write(d))


def fake_cmp(**over):
    base = {"per_call": [{"same": True}] * 10, "differing": [], "reasons": {},
            "unexpected_calls": 0, "not_applied_both": 0, "nonterminal_results": {},
            "content_diff": {"concepts_created": {"only_a": [], "only_b": []},
                             "semantic_edges": {"only_a": [], "only_b": []},
                             "unembedded_new": {"a": 0, "b": 0},
                             "hierarchical_ends_unembedded": {"a": 0, "b": 0}},
            "canonization": {"status_counts": {"a": {"Candidate": 2}, "b": {"Candidate": 2}},
                             "events_since_T": {"a": {}, "b": {}}}}
    base.update(over)
    return base


class Verdict(unittest.TestCase):
    def test_clean_run_passes(self):
        v = rp.verdict(fake_cmp(), {"a": 0, "b": 0}, 0.02)
        self.assertTrue(v["pass"], v)
        self.assertEqual(v["notes"], [])

    def test_both_sides_refusing_everything_fails(self):
        v = rp.verdict(fake_cmp(not_applied_both=10), {"a": 0, "b": 0}, 0.02)
        self.assertFalse(v["pass"])
        self.assertIn("applied on neither side", v["failures"][0])
        self.assertTrue(rp.verdict(fake_cmp(not_applied_both=1), {"a": 0, "b": 0}, 0.2)["pass"])

    def test_pending_results_unexpected_calls_and_serve_rc_fail(self):
        for cmp_, rcs in ((fake_cmp(nonterminal_results={"b:pending": 1}), {"a": 0, "b": 0}),
                          (fake_cmp(unexpected_calls=1), {"a": 0, "b": 0}),
                          (fake_cmp(), {"a": 0, "b": 101})):
            with self.subTest(cmp=cmp_.get("nonterminal_results"), rcs=rcs):
                self.assertFalse(rp.verdict(cmp_, rcs, 0.02)["pass"])
        self.assertFalse(rp.verdict(fake_cmp(per_call=[]), {}, 0.02)["pass"])

    def test_canonization_and_unembedded_differences_are_noted(self):
        cmp_ = fake_cmp()
        cmp_["canonization"]["status_counts"]["b"] = {"Candidate": 1, "Venerable": 1}
        cmp_["content_diff"]["unembedded_new"] = {"a": 0, "b": 3}
        v = rp.verdict(cmp_, {"a": 0, "b": 0}, 0.02)
        self.assertTrue(v["pass"])
        self.assertEqual(len(v["notes"]), 2)

    def test_compare_counts_not_applied_and_nonterminal(self):
        calls = [{"kind": "derive", "agent_id": "a"}] * 2
        side = lambda states: {  # noqa: E731
            "results": [{"state": s} for s in states],
            "store": {"canonization_status": {}}, "final_stats_tool": {},
            "facts": {"created": rp.collections.Counter(), "semantic": rp.collections.Counter(),
                      "unembedded_new": [], "hierarchical_ends_unembedded": 0,
                      "canonization_events": {}}}
        c = rp.compare(calls, side(["refused", "pending"]), side(["refused"]), "T",
                       {"canonization_status": {}}, rp.REASONS_0_4_0)
        self.assertEqual(c["not_applied_both"], 2)
        self.assertEqual(c["nonterminal_results"], {"a:pending": 1, "b:not_run": 1})
        v = rp.verdict(c, {"a": 0, "b": 0}, 0.02)
        self.assertFalse(v["pass"])
        self.assertEqual(len(v["failures"]), 3)  # unexpected, not applied, never settled


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
        self.tmp = tmpdir(self)
        self.live = self.tmp / "live"
        self.live.mkdir()

    def test_outputs_under_a_protected_dir_are_refused(self):
        snap = self.live / "backup.db"
        for out, work in ((self.live / "o", self.tmp / "w"), (self.tmp / "o", self.live / "w")):
            with self.subTest(out=out, work=work), self.assertRaises(SystemExit):
                rp.check_paths(out, work, snap, [self.live], False)
        # The snapshot itself may sit under a protected dir: it is read-only.
        rp.check_paths(self.tmp / "o", self.tmp / "w", snap, [self.live], False)

    def test_protect_always_includes_the_default(self):
        dirs = rp.protect_dirs([str(self.live)])
        self.assertIn(rp.DEFAULT_PROTECT.resolve(), dirs)
        self.assertIn(self.live.resolve(), dirs)
        self.assertEqual(rp.protect_dirs(None), [rp.DEFAULT_PROTECT.resolve()])

    def test_live_store_name_needs_the_flag(self):
        snap = self.tmp / "lambo-dev.db"
        with self.assertRaises(SystemExit):
            rp.check_paths(self.tmp / "o", self.tmp / "w", snap, [], False)
        rp.check_paths(self.tmp / "o", self.tmp / "w", snap, [], True)

    def test_rewritten_config_points_at_the_copy_and_drops_dsn(self):
        tpl = self.tmp / "t.toml"
        tpl.write_text('[store]\nkind = "sqlite"\npath = "/somewhere/else.db"\n'
                       'dsn = "postgres://u@h/db"\n\n[embedder]\nkind = "fixture"\n')
        ok = self.tmp / "ok.toml"
        rp.write_config(tpl, self.tmp / "replay.db", ok, 5, 9, [self.live])
        parsed = rp.tomllib.loads(ok.read_text())
        self.assertEqual(parsed["store"], {"kind": "sqlite", "path": str(self.tmp / "replay.db")})
        self.assertEqual(parsed["daemon"]["canonization_eval_interval_secs"], 5)
        with self.assertRaises(SystemExit):
            rp.write_config(tpl, self.live / "replay.db", self.tmp / "bad.toml", 5, 9,
                            [self.live])

    def test_non_sqlite_or_storeless_templates_are_refused(self):
        for text in ('[store]\nkind = "postgres"\npath = "x"\n',
                     '[store]\npath = "x"\n',
                     '[embedder]\nkind = "fixture"\n',
                     '[store]\nkind = "sqlite"\n',
                     'not = = toml'):
            with self.subTest(text=text), self.assertRaises(SystemExit):
                tpl = self.tmp / "t.toml"
                tpl.write_text(text)
                rp.write_config(tpl, self.tmp / "r.db", self.tmp / "c.toml", 5, 9, [])

    def test_child_env_strips_store_redirects(self):
        env = rp.child_env({"PATH": "/bin", "LAMBO_CONFIG": "x", "LAMBO_STORE": "postgres",
                            "LAMBO_SQLITE_PATH": "/live.db", "LAMBO_POSTGRES_DSN": "d",
                            "LAMBO_COCKROACH_DSN": "d", "LAMBO_POSTGRES_IAM": "1",
                            "LAMBO_FOO_DSN": "d", "LAMBO_EMBEDDER": "fixture"},
                           XDG_RUNTIME_DIR="/tmp/r")
        self.assertEqual(env, {"PATH": "/bin", "LAMBO_EMBEDDER": "fixture",
                               "XDG_RUNTIME_DIR": "/tmp/r"})

    def test_groups_must_be_positive(self):
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as cm:
            rp.main(["--bin-a", "x", "--bin-b", "y", "--snapshot", "s", "--config-template",
                     "t", "--session", "s", "--groups", "0", "--out", str(self.tmp / "o")])
        self.assertEqual(cm.exception.code, 2)


class Storage(unittest.TestCase):
    def test_ro_uri_escapes_uri_metacharacters(self):
        uri = rp.ro_uri("/tmp/a?mode=rwc#x%y.db")
        self.assertTrue(uri.endswith("?mode=ro"))
        self.assertEqual(uri.count("?"), 1)
        self.assertNotIn("#", uri)

    def test_backup_handles_awkward_names_and_opens_source_read_only(self):
        d = tmpdir(self)
        src = d / "it's a ?#% store.db"
        db = sqlite3.connect(src)
        db.execute("CREATE TABLE t(x)")
        db.execute("INSERT INTO t VALUES (1)")
        db.commit()
        db.close()
        dst = d / "copy 'x'.db"
        rp.sqlite_backup(src, dst)
        self.assertEqual(sqlite3.connect(dst).execute("SELECT x FROM t").fetchall(), [(1,)])
        with self.assertRaises(sqlite3.OperationalError):
            rp.ro_connect(src).execute("INSERT INTO t VALUES (2)")

    def test_work_dir_is_private(self):
        d = tmpdir(self)
        w, temp = rp.make_work_dir(str(d / "new" / "work"))
        self.assertFalse(temp)
        self.assertEqual(stat.S_IMODE(os.stat(w).st_mode), 0o700)
        w2, temp2 = rp.make_work_dir(None)
        self.addCleanup(shutil.rmtree, w2, True)
        self.assertTrue(temp2)
        self.assertEqual(stat.S_IMODE(os.stat(w2).st_mode), 0o700)

    def test_serve_removes_its_runtime_dir_when_popen_fails(self):
        d = tmpdir(self)
        parent = d / "rt"
        parent.mkdir()
        with self.assertRaises(OSError):
            rp.Serve(str(d / "no-such-binary"), d / "c.toml", d, "s", str(parent))
        self.assertEqual(list(parent.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
