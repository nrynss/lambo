#!/usr/bin/env python3
"""Unit tests for the pure parts of mcp_concurrent.py, replay_parity.py and
mcp_ab.py's rate-limit retry.

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
import random
import shutil
import sqlite3
import stat
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import ledger_drift as ld  # noqa: E402
import mcp_ab as ab  # noqa: E402
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

    def test_close_unflushed_is_a_settled_failure_with_its_own_reason(self):
        # #117: the close's final flush did not complete, so the write may not
        # have been saved. Settled (no polling to the budget), not applied.
        body = {"receipt": {"state": "close_unflushed",
                            "detail": "close could not confirm this write was saved — its receipt "
                                      "did not settle before the session closed"}}
        self.assertEqual(mc.receipt_outcome(body, False),
                         {"state": "close_unflushed", "reason": "close_unflushed"})
        out = [{"state": "applied", "reason": None}] * 3 + \
              [{"state": "close_unflushed", "reason": "close_unflushed"}]
        s = mc.summarize_writes(4, [], out)
        self.assertEqual(s["settled"], 4)
        self.assertEqual(s["failed"], 1)
        self.assertEqual(s["write_failure_rate_all"], 0.25)
        self.assertEqual(s["failed_reasons"], {"close_unflushed: close_unflushed": 1})
        self.assertIn("close_unflushed", rp.TERMINAL_STATES)
        self.assertNotIn("close_unflushed", rp.APPLIED_STATES)

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
                    drain_timeout=1.0, receipt_max=0, receipt_rps=0, server_ledger=None,
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

    @staticmethod
    def merge(frm, to, replayed, at="2026-10-08T15:05:00Z", to_id=None):
        return {"from": frm, "to": to, "to_created_at": at, "to_replayed": replayed,
                "to_id": None if replayed else (to_id or "id-" + to)}

    def test_equal_counters_with_different_replayed_targets_are_unexpected(self):
        call = {"kind": "derive", "arguments": {}}
        sa = sb = row(semantic_merged=1, created_count=1)
        ra = {"semantic_merges": [self.merge("n", "x", True)]}
        rb = {"semantic_merges": [self.merge("n", "y", True)]}
        ids = self.cls(call, sa, sb, ra, rb)
        self.assertEqual(ids, ["merge_target_differs"])
        self.assertEqual(self.unexpected(ids), ["merge_target_differs"])
        self.assertEqual(rp.merge_diff(ra, rb, self.T), {"replay_vs_replay": 1})

    def test_the_same_merge_with_per_side_ids_and_times_is_no_difference(self):
        sa = sb = row(semantic_merged=2, created_count=2)
        ra = {"semantic_merges": [self.merge("n", "x", True, "2026-10-08T15:05:00Z"),
                                  self.merge("m", "p", False, "2026-10-01T00:00:00Z")]}
        rb = {"semantic_merges": [self.merge("m", "p", False, "2026-10-01T00:00:00Z"),
                                  self.merge("n", "x", True, "2026-10-08T16:40:12Z")]}
        self.assertEqual(rp.differing_fields(sa, sb, ra, rb, self.T), set())
        # Records from before to_replayed existed: placed by to_created_at.
        old_a = {"semantic_merges": [{"from": "n", "to": "x",
                                      "to_created_at": "2026-10-08T15:05:00Z"}]}
        old_b = {"semantic_merges": [{"from": "n", "to": "x",
                                      "to_created_at": "2026-10-08T16:40:12Z"}]}
        self.assertEqual(rp.merge_diff(old_a, old_b, self.T), {})

    def test_two_pre_t_targets_with_one_text_are_told_apart_by_id(self):
        call = {"kind": "derive", "arguments": {}}
        sa = sb = row(semantic_merged=1)
        ra = {"semantic_merges": [self.merge("n", "p", False, to_id="id-1")]}
        rb = {"semantic_merges": [self.merge("n", "p", False, to_id="id-2")]}
        self.assertEqual(self.cls(call, sa, sb, ra, rb), ["merge_differs_pre_t"])

    def test_replayed_target_against_pre_t_or_none_fits_the_mechanism(self):
        call = {"kind": "derive", "arguments": {}}
        sa = sb = row(semantic_merged=1)
        ra = {"semantic_merges": [self.merge("n", "x", True)]}
        rb = {"semantic_merges": [self.merge("n", "p", False)]}
        self.assertEqual(self.cls(call, sa, sb, ra, rb), ["merge_into_replayed_concept"])
        self.assertEqual(rp.merge_diff(ra, rb, self.T), {"replay_vs_pre_t": 1})
        self.assertEqual(self.cls(call, row(semantic_merged=0), sb, {}, ra),
                         ["merge_into_replayed_concept"])

    def test_one_call_can_fire_several_merge_reasons(self):
        call = {"kind": "derive", "arguments": {}}
        sa = sb = row(semantic_merged=2, created_count=2)
        ra = {"semantic_merges": [self.merge("n", "x", True), self.merge("m", "y", True)]}
        rb = {"semantic_merges": [self.merge("n", "p", False), self.merge("m", "z", True)]}
        ids = self.cls(call, sa, sb, ra, rb)
        self.assertEqual(sorted(ids), ["merge_into_replayed_concept", "merge_target_differs"])
        self.assertEqual(self.unexpected(ids), ["merge_target_differs"])

    def test_a_merge_count_the_stores_do_not_show_is_unexplained(self):
        call = {"kind": "derive", "arguments": {}}
        same = {"semantic_merges": [self.merge("n", "x", True)]}
        ids = self.cls(call, row(semantic_merged=1), row(semantic_merged=2), same, same)
        self.assertEqual(ids, ["unexplained:semantic_merged"])

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


class MergeReadback(unittest.TestCase):
    """semantic_targets / replay_facts on a minimal store whose Semantic edge
    directions are flipped between two copies, as random v4 NodeIds do."""
    T = "2026-10-08T15:04:00Z"
    PRE, CALL, LATER = "2026-10-01T00:00:00Z", "2026-10-08T15:05:00Z", "2026-10-08T15:06:00Z"

    def store(self, name, flip, call_at=None):
        call_at = call_at or self.CALL
        p = tmpdir(self) / name
        db = sqlite3.connect(p)
        db.executescript(
            "CREATE TABLE concepts(id TEXT PRIMARY KEY, content TEXT, concept_type TEXT, "
            "created_at TEXT, embedding BLOB);"
            "CREATE TABLE edges(source TEXT, target TEXT, edge_type TEXT, created_at TEXT);"
            "CREATE TABLE canonization_events(node_id TEXT, from_status TEXT, "
            "to_status TEXT, occurred_at TEXT);")
        # Per-side ids for replayed concepts; the pre-T one is shared.
        s = name
        rows = [("p", "pre-T target", self.PRE), (s + "n1", "new one", call_at),
                (s + "n2", "new two", call_at), (s + "L", "later concept", self.LATER)]
        db.executemany("INSERT INTO concepts VALUES (?, ?, 'Logic', ?, x'00')", rows)
        pairs = [("p", s + "n1", call_at),       # n1 merged into pre-T p
                 (s + "n1", s + "n2", call_at),  # n2 merged into n1, same call
                 (s + "n1", s + "L", self.LATER)]  # a later call merged L into n1
        db.executemany("INSERT INTO edges VALUES (?, ?, 'Semantic', ?)",
                       [((b, a) if flip else (a, b)) + (t,) for a, b, t in pairs])
        db.commit()
        db.close()
        return p, [s + "n1", s + "n2"]

    def test_a_call_owns_its_merges_whatever_the_edge_direction(self):
        pa, ida = self.store("a", flip=False)
        pb, idb = self.store("b", flip=True, call_at="2026-10-08T15:05:30Z")
        ma, mb = rp.semantic_targets(pa, ida, self.T), rp.semantic_targets(pb, idb, self.T)
        self.assertEqual(sorted((m["from"], m["to"], m["to_replayed"], m["to_id"]) for m in ma),
                         [("new one", "pre-T target", False, "p"),
                          ("new two", "new one", True, None)])
        self.assertEqual(rp.merge_diff({"semantic_merges": ma}, {"semantic_merges": mb},
                                       self.T), {})

    def tied_store(self, name, flip):
        """Two calls whose concepts share one millisecond: call 1 creates c1,
        call 2 creates c2 and merges it into c1."""
        p = tmpdir(self) / name
        db = sqlite3.connect(p)
        db.executescript(
            "CREATE TABLE concepts(id TEXT PRIMARY KEY, content TEXT, concept_type TEXT, "
            "created_at TEXT, embedding BLOB);"
            "CREATE TABLE edges(source TEXT, target TEXT, edge_type TEXT, created_at TEXT);")
        c1, c2 = name + "c1", name + "c2"
        db.executemany("INSERT INTO concepts VALUES (?, ?, 'Logic', ?, x'00')",
                       [(c1, "first call", self.CALL), (c2, "second call", self.CALL)])
        db.execute("INSERT INTO edges VALUES (?, ?, 'Semantic', ?)",
                   ((c1, c2) if flip else (c2, c1)) + (self.CALL,))
        db.commit()
        db.close()
        results = [{"idx": 0, "created_ids": [c1]}, {"idx": 1, "created_ids": [c2]}]
        return p, results

    def test_a_millisecond_tie_is_settled_by_replay_order(self):
        sides = []
        for name, flip in (("a", False), ("b", True)):
            p, results = self.tied_store(name, flip)
            order_of = rp.replay_order(results)
            self.assertEqual(order_of, {name + "c1": (0, 0), name + "c2": (1, 0)})
            per_call = [rp.semantic_targets(p, r["created_ids"], self.T, order_of)
                        for r in results]
            self.assertEqual(per_call[0], [], name)  # the earlier call made no merge
            self.assertEqual([(m["from"], m["to"], m["to_replayed"]) for m in per_call[1]],
                             [("second call", "first call", True)], name)
            sides.append(per_call)
        for ra, rb in zip(*sides):
            self.assertEqual(rp.merge_diff({"semantic_merges": ra}, {"semantic_merges": rb},
                                           self.T), {})

    def test_semantic_edge_facts_ignore_direction(self):
        pa, _ = self.store("a", flip=False)
        pb, _ = self.store("b", flip=True)
        fa, fb = rp.replay_facts(pa, self.T), rp.replay_facts(pb, self.T)
        self.assertEqual(fa["semantic"], fb["semantic"])
        self.assertEqual(sum(fa["semantic"].values()), 3)

    def test_compare_writes_merge_counts_and_no_text_once_stripped(self):
        pa, ida = self.store("a", flip=False)
        calls = [{"kind": "derive", "agent_id": "x", "arguments": {}}]
        facts = rp.replay_facts(pa, self.T)

        def side(merges):
            return {"results": [{"state": "applied", "receipt": {
                        "state": "applied", "created_count": 2, "semantic_merged": 2},
                        "created": ["new one", "new two"], "matched": [],
                        "semantic_merges": merges}],
                    "store": {"canonization_status": {}}, "final_stats_tool": {},
                    "facts": facts}
        ma = rp.semantic_targets(pa, ida, self.T)
        mb = [dict(m, to="another replayed concept") if m["to_replayed"] else m for m in ma]
        c = rp.compare(calls, side(ma), side(mb), self.T, {"canonization_status": {}},
                       rp.REASONS_0_4_0)
        d = c["differing"][0]
        self.assertEqual(d["merge_diff"], {"replay_vs_replay": 1})
        self.assertEqual([r["id"] for r in d["reasons"]], ["merge_target_differs"])
        self.assertEqual(c["unexpected_calls"], 1)
        s = json.dumps(rp.strip_content(c))
        self.assertIn("replay_vs_replay", s)
        for text in ("new one", "new two", "pre-T target", "another replayed"):
            self.assertNotIn(text, s)
        same = rp.compare(calls, side(ma), side([dict(m) for m in ma]), self.T,
                          {"canonization_status": {}}, rp.REASONS_0_4_0)
        self.assertEqual(same["differing"], [])


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


class FakeServe:
    """In-process stand-in for replay_parity.Serve. tool() raises `exc` on
    the call numbered `die_on` (1-based, counting every tool call)."""

    def __init__(self, die_on=None, exc=BrokenPipeError("[Errno 32] Broken pipe"),
                 die_in_init=False):
        self.die_on, self.exc, self.die_in_init, self.n = die_on, exc, die_in_init, 0

    def initialize(self, timeout):
        if self.die_in_init:
            raise RuntimeError("serve exited during initialize (rc=101)")
        return {"serverInfo": {"version": "fake"}}

    def tool(self, name, args, timeout=180):
        self.n += 1
        if self.n == self.die_on:
            raise self.exc
        if name == "lambo_stats" and "receipt" in args:
            return {"is_error": False, "text": "",
                    "structured": {"receipt": {"state": "applied", "created_count": 1,
                                               "matched_count": 0}}}
        if name == "lambo_stats":
            return {"is_error": False, "text": "", "structured": {}}
        return {"is_error": False, "text": "", "structured": {"receipt": f"lwr1.{self.n}"}}


def replay_opts(**kw):
    base = dict(startup_timeout=1, wait_ms=0, receipt_budget=1, canon_wait=0,
                no_final_stats=False, include_content=False)
    base.update(kw)
    return argparse.Namespace(**base)


CALLS = [{"kind": "derive", "agent_id": "ag", "tool": "lambo_derive",
          "arguments": {"agent_id": "ag", "concepts": []}}] * 3


class ReplayTransportFailure(unittest.TestCase):
    def test_a_broken_pipe_mid_run_stops_the_side_without_raising(self):
        with redirect_stdout(io.StringIO()):
            # call 1: derive, 2: its receipt wait, 3: derive #2 -> broken pipe
            results, final, err = rp.replay_calls(FakeServe(die_on=3), "A", CALLS,
                                                  replay_opts())
        self.assertEqual([r["state"] for r in results], ["applied", "transport_error"])
        self.assertEqual(results[1]["detail"], "BrokenPipeError")  # no raw text by default
        self.assertIsNone(final)
        self.assertIn("call 2 of 3", err)
        self.assertIn("BrokenPipeError", err)

    def test_failures_in_initialize_receipt_wait_and_final_stats_are_caught(self):
        cases = ((FakeServe(die_in_init=True), "initialize", 0),
                 (FakeServe(die_on=2, exc=ValueError("bad json")), "call 1 of 3", 1),
                 (FakeServe(die_on=7, exc=TimeoutError("t")), "final lambo_stats", 3))
        for serve, stage, n in cases:
            with self.subTest(stage=stage), redirect_stdout(io.StringIO()):
                results, _, err = rp.replay_calls(serve, "A", CALLS, replay_opts())
                self.assertIn(stage, err)
                self.assertEqual(len(results), n)

    def test_a_stopped_side_fails_the_verdict_and_the_report_is_written(self):
        d = tmpdir(self)
        snap = d / "backup.db"
        db = sqlite3.connect(snap)
        db.executescript("""
            CREATE TABLE interactions(id TEXT PRIMARY KEY, session_id TEXT, agent_id TEXT,
                prompt_text TEXT, created_at TEXT, event_time TEXT);
            CREATE TABLE concepts(id TEXT PRIMARY KEY, content TEXT, concept_type TEXT,
                origin_interaction TEXT, created_at TEXT, embedding BLOB,
                canonization_status TEXT);
            CREATE TABLE edges(source TEXT, target TEXT, edge_type TEXT, created_at TEXT);
            CREATE TABLE canonization_events(node_id TEXT, from_status TEXT,
                to_status TEXT, occurred_at TEXT);
            INSERT INTO interactions VALUES('i1','s','ag','alpha','2026-01-02T00:00:00Z',NULL);
            INSERT INTO concepts VALUES('c1','alpha','Logic','i1','2026-01-02T00:00:00Z',
                x'00','Candidate');
            INSERT INTO edges VALUES('i1','c1','Derives','2026-01-02T00:00:00Z');
        """)
        db.commit()
        db.close()
        tpl = d / "t.toml"
        tpl.write_text('[store]\nkind = "sqlite"\npath = "x.db"\n')
        store = {"concepts": 0, "embedded": 0, "unembedded": 0, "interactions": 0,
                 "edges": 0, "edges_by_type": {}, "concepts_by_type": {},
                 "canonization_status": {}}
        facts = {"created": rp.collections.Counter(), "semantic": rp.collections.Counter(),
                 "unembedded_new": [], "hierarchical_ends_unembedded": 0,
                 "canonization_events": {}}

        def fake_side(label, binary, base_db, template, workdir, calls, T, a):
            ok = {"state": "applied", "receipt": {"created_count": 1}, "created": [],
                  "matched": [], "semantic_merges": []}
            dead = label == "A"
            return {"binary": str(binary), "db": "x", "exit_code": 0, "tool_calls": 1,
                    "side_error": "transport failure during call 1 of 1: BrokenPipeError"
                    if dead else None,
                    "results": [{"idx": 0, "state": "transport_error",
                                 "detail": "BrokenPipeError"}] if dead else [ok],
                    "final_stats_tool": {}, "store": store, "facts": facts}

        out = d / "out"
        with mock.patch.object(rp, "run_side", fake_side), \
                redirect_stdout(io.StringIO()):
            rc = rp.main(["--bin-a", sys.executable, "--bin-b", sys.executable,
                          "--snapshot", str(snap), "--config-template", str(tpl),
                          "--session", "s", "--groups", "1", "--out", str(out),
                          "--canon-wait", "0", "--runtime-parent", str(d / "rt")])
        self.assertEqual(rc, 1)
        text = (out / "parity.txt").read_text()
        self.assertIn("VERDICT: FAIL", text)
        self.assertIn("side A stopped early: transport failure during call 1 of 1", text)
        parity = json.loads((out / "parity.json").read_text())
        self.assertFalse(parity["verdict"]["pass"])
        self.assertEqual(parity["nonterminal_results"], {"a:transport_error": 1})


class RuntimeParent(unittest.TestCase):
    def test_runtime_parent_under_a_protected_dir_is_refused(self):
        d = tmpdir(self)
        live = d / "live"
        live.mkdir()
        with self.assertRaises(SystemExit) as cm:
            rp.check_paths(d / "o", d / "w", d / "b.db", [live], False, live / "rt")
        self.assertIn("--runtime-parent", str(cm.exception.code))
        rp.check_paths(d / "o", d / "w", d / "b.db", [live], False, d / "rt")

    def test_main_refuses_it_before_creating_anything(self):
        d = tmpdir(self)
        live = d / "live"
        live.mkdir()
        with self.assertRaises(SystemExit) as cm:
            rp.main(["--bin-a", "x", "--bin-b", "y", "--snapshot", str(d / "b.db"),
                     "--config-template", "t", "--session", "s", "--groups", "1",
                     "--out", str(d / "o"), "--protect", str(live),
                     "--runtime-parent", str(live / "rt")])
        self.assertIn("--runtime-parent", str(cm.exception.code))
        self.assertFalse((live / "rt").exists())
        self.assertFalse((d / "o").exists())

    def test_runtime_parent_is_created_private_and_existing_ones_untouched(self):
        d = tmpdir(self)
        p = rp.make_runtime_parent(d / "a" / "rt")
        self.assertEqual(stat.S_IMODE(os.stat(p).st_mode), 0o700)
        existing = d / "shared"
        existing.mkdir(mode=0o755)
        existing.chmod(0o755)
        rp.make_runtime_parent(existing)
        self.assertEqual(stat.S_IMODE(os.stat(existing).st_mode), 0o755)
        f = d / "file"
        f.write_text("x")
        with self.assertRaises(SystemExit):
            rp.make_runtime_parent(f)


# ---------------------------------------------------------------------------
# mcp_ab: HTTP 429 retry
# ---------------------------------------------------------------------------

class RetryAfter(unittest.TestCase):
    def test_delta_seconds_and_absent_or_bad_values(self):
        self.assertEqual(ab.parse_retry_after("1"), 1.0)
        self.assertEqual(ab.parse_retry_after(" 5 "), 5.0)
        for v in (None, "", "soon", "-3", "1.5"):
            with self.subTest(v=v):
                self.assertIsNone(ab.parse_retry_after(v))

    def test_http_date_is_seconds_from_now_and_never_negative(self):
        now = ab.datetime(2026, 11, 18, 12, 0, 0, tzinfo=ab.timezone.utc)
        self.assertEqual(ab.parse_retry_after("Wed, 18 Nov 2026 12:00:07 GMT", now), 7.0)
        self.assertEqual(ab.parse_retry_after("Wed, 18 Nov 2026 11:59:00 GMT", now), 0.0)


class RateLimitDelay(unittest.TestCase):
    def test_backoff_doubles_from_the_base_and_is_capped(self):
        self.assertEqual([ab.rate_limit_delay(i, attempts=8) for i in range(7)],
                         [0.25, 0.5, 1.0, 2.0, 4.0, 4.0, 4.0])

    def test_last_attempt_gives_up(self):
        self.assertIsNone(ab.rate_limit_delay(ab.RATE_LIMIT_ATTEMPTS - 1))
        self.assertIsNone(ab.rate_limit_delay(ab.RATE_LIMIT_ATTEMPTS))
        self.assertIsNone(ab.rate_limit_delay(0, attempts=1))
        self.assertIsNone(ab.rate_limit_delay(0, retry_after=1.0, attempts=1))

    def test_retry_after_wins_and_is_clamped(self):
        self.assertEqual(ab.rate_limit_delay(3, retry_after=1.0), 1.0)
        self.assertEqual(ab.rate_limit_delay(0, retry_after=0.0), 0.0)
        self.assertEqual(ab.rate_limit_delay(0, retry_after=3600.0), ab.RETRY_AFTER_CAP_S)
        self.assertEqual(ab.rate_limit_delay(0, retry_after=-2.0), 0.0)

    def test_matches_mcp_concurrent_receipt_backoff(self):
        # mcp_concurrent.py's receipt loop: 6 attempts, min(0.25 * 2**i, 4.0).
        self.assertEqual(ab.RATE_LIMIT_ATTEMPTS, 6)
        self.assertEqual([ab.rate_limit_delay(i) for i in range(5)],
                         [min(0.25 * 2 ** i, 4.0) for i in range(5)])


class _Resp:
    def __init__(self, text):
        self.text, self.headers = text, {}

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False

    def read(self):
        return self.text.encode()


def _http_429(retry_after=None):
    from email.message import Message
    h = Message()
    if retry_after is not None:
        h["Retry-After"] = retry_after
    return ab.urllib.error.HTTPError("http://x/mcp", 429, "Too Many Requests", h, io.BytesIO())


class AbRetry(unittest.TestCase):
    """Mcp._post against a fake opener: no network, sleeps recorded not taken."""

    def client(self):
        c = object.__new__(ab.Mcp)
        c.url, c.token, c.sid, c.n = "http://x/mcp", None, None, 0
        c.rate_limited_retries = 0
        c._t0 = 0.0
        return c

    def run_tool(self, replies):
        """A fake clock: each request takes 0.01 s, and a sleep advances it."""
        c = self.client()
        sleeps, now = [], [100.0]
        ok = json.dumps({"result": {"structuredContent": {"hits": []}}})

        def opener(req, timeout):
            now[0] += 0.01
            r = replies.pop(0)
            if isinstance(r, Exception):
                raise r
            return _Resp(ok)

        def sleep(s):
            sleeps.append(s)
            now[0] += s
        with mock.patch.object(ab._OPENER, "open", side_effect=opener), \
                mock.patch.object(ab.time, "sleep", side_effect=sleep), \
                mock.patch.object(ab.time, "perf_counter", side_effect=lambda: now[0]):
            return c, sleeps, c.tool("lambo_recall", query="q")

    def test_429_is_retried_with_retry_after_then_backoff_and_counted(self):
        c, sleeps, (ms, body, err) = self.run_tool([_http_429("1"), _http_429(), "ok"])
        self.assertEqual(sleeps, [1.0, 0.5])
        self.assertEqual(c.rate_limited_retries, 2)
        self.assertFalse(err)
        self.assertEqual(body, {"hits": []})

    def test_waiting_on_429_is_not_counted_in_latency(self):
        _, sleeps, (ms, _, _) = self.run_tool([_http_429(), _http_429(), "ok"])
        self.assertEqual(sleeps, [0.25, 0.5])
        # Only the successful attempt (0.01 s), not the two refusals and 0.75 s of waits.
        self.assertAlmostEqual(ms, 10.0, places=6)

    def test_no_429_latency_is_the_single_attempt(self):
        c, sleeps, (ms, _, _) = self.run_tool(["ok"])
        self.assertEqual((sleeps, c.rate_limited_retries), ([], 0))
        self.assertAlmostEqual(ms, 10.0, places=6)

    def test_exhausted_retries_raise_with_the_remedy(self):
        replies = [_http_429() for _ in range(ab.RATE_LIMIT_ATTEMPTS)]
        with self.assertRaises(ab.RateLimitExhausted) as cm:
            self.run_tool(replies)
        self.assertIn("rate limit", str(cm.exception))
        self.assertIn("--rate-limit-rps 0", str(cm.exception))

    def test_other_http_errors_are_not_retried(self):
        err = ab.urllib.error.HTTPError("http://x/mcp", 500, "boom", {}, io.BytesIO())
        self.addCleanup(err.close)
        with self.assertRaises(ab.urllib.error.HTTPError):
            self.run_tool([err, "ok"])


class RateLimitDetectionTest(unittest.TestCase):
    def test_http_429_and_rate_limit_texts_are_rate_limited(self):
        self.assertTrue(mc.is_rate_limited("transport: HTTPError 429"))
        self.assertTrue(mc.is_rate_limited("Too Many Requests"))
        self.assertFalse(mc.is_rate_limited("transport: HTTPError 500"))
        self.assertFalse(mc.is_rate_limited(None))


# ---------------------------------------------------------------------------
# ledger_drift (#177)
# ---------------------------------------------------------------------------

class OrderStatMultisetTests(unittest.TestCase):
    def _tree_height(self, tree):
        h = 1
        node = tree._root
        while isinstance(node, ld._Internal):
            h += 1
            node = node.children[0]
        return h

    def _assert_matches_bruteforce(self, values):
        tree = ld.OrderStatMultiset()
        seen = []
        for v in values:
            seen.append(v)
            lt, eq, total = tree.add_and_rank(v)
            want_lt = sum(1 for x in seen if x < v)
            want_eq = sum(1 for x in seen if x == v)
            self.assertEqual((lt, eq, total), (want_lt, want_eq, len(seen)))
        self.assertEqual(len(tree), len(values))
        return tree

    def test_random_sorted_reverse_and_heavy_tie_streams_match_bruteforce(self):
        rng = random.Random(42)
        self._assert_matches_bruteforce([rng.random() for _ in range(300)])
        self._assert_matches_bruteforce([float(i) for i in range(250)])
        self._assert_matches_bruteforce([float(250 - i) for i in range(250)])
        # Heavy ties (recency floor 0.35 + discrete buckets).
        tied = [0.35 if i % 3 == 0 else round(rng.uniform(0.3, 0.9), 2) for i in range(300)]
        self._assert_matches_bruteforce(tied)

    def test_internal_node_splits_match_bruteforce_at_small_fanout(self):
        rng = random.Random(99)
        with mock.patch.object(ld, "_BTREE_MAX_KEYS", 4):
            t_rand = self._assert_matches_bruteforce([rng.random() for _ in range(300)])
            self.assertGreaterEqual(self._tree_height(t_rand), 3)
            t_asc = self._assert_matches_bruteforce([float(i) for i in range(300)])
            self.assertGreaterEqual(self._tree_height(t_asc), 3)
            t_desc = self._assert_matches_bruteforce([float(300 - i) for i in range(300)])
            self.assertGreaterEqual(self._tree_height(t_desc), 3)
            tied = [
                0.35 if i % 4 == 0 else round(rng.uniform(0.0, 10.0), 3)
                for i in range(300)
            ]
            t_tied = self._assert_matches_bruteforce(tied)
            self.assertGreaterEqual(self._tree_height(t_tied), 3)

    def test_default_fanout_reaches_height_three_and_matches_exact_ranks(self):
        rng = random.Random(1234)
        # Random inserts into a max-64-key B+ tree average ~44 keys/leaf, so
        # 4,500 distinct keys comfortably exceeds the 65-leaf threshold that
        # forces an internal-node split (height >= 3).
        n = 4500
        values = [rng.random() for _ in range(n)]
        tree = ld.OrderStatMultiset()
        sorted_seen = []
        import bisect as _bisect

        for i, v in enumerate(values):
            lt, eq, total = tree.add_and_rank(v)
            _bisect.insort(sorted_seen, v)
            if i % 50 == 0 or i >= n - 10:
                want_lt = _bisect.bisect_left(sorted_seen, v)
                want_eq = _bisect.bisect_right(sorted_seen, v) - want_lt
                self.assertEqual((lt, eq, total), (want_lt, want_eq, len(sorted_seen)))
        self.assertEqual(len(tree), n)
        self.assertGreaterEqual(self._tree_height(tree), 3)

    def test_negative_and_positive_zero_collapse_as_equal(self):
        self._assert_matches_bruteforce([-1.0, -0.0, 0.0, -0.0, +0.0, 1.0, -0.0])

    def test_rejects_nan(self):
        tree = ld.OrderStatMultiset()
        with self.assertRaises(ValueError):
            tree.add_and_rank(float("nan"))


class ExchangeabilityMartingaleTests(unittest.TestCase):
    def test_exchangeable_stream_with_ties_meets_10k_false_alarm_bar(self):
        # 10,000 exchangeable draws with 30% exact ties at 0.35 (recency floor).
        rng = random.Random(7)
        mart = ld.OnlineExchangeabilityMartingale(direction="two_sided", seed=11)
        alarms = 0
        for _ in range(10_000):
            val = 0.35 if rng.random() < 0.30 else rng.betavariate(5.0, 3.0)
            step = mart.update(val)
            if step.log10_m >= 2.0:  # M >= 100
                alarms += 1
                break
        self.assertEqual(alarms, 0, f"unexpected false alarm on 10k stable stream: {mart.log_m}")

    def test_martingale_expectation_stays_near_one_under_null(self):
        # E[M_n] = 1 identically under exchangeability; sample mean across 120 runs stays near 1.
        final_ms = []
        for run in range(120):
            rng = random.Random(1000 + run)
            mart = ld.OnlineExchangeabilityMartingale(direction="two_sided", seed=run)
            for _ in range(80):
                val = 0.35 if rng.random() < 0.25 else rng.betavariate(5.0, 3.0)
                mart.update(val)
            final_ms.append(mart.m_value)
        mean_m = sum(final_ms) / len(final_ms)
        self.assertGreater(mean_m, 0.5)
        self.assertLess(mean_m, 1.8)

    def test_detects_downward_and_upward_shifts_and_late_change(self):
        # Downward shift of -0.15 after 200 stable recalls.
        rng = random.Random(19)
        mart = ld.OnlineExchangeabilityMartingale(direction="two_sided", seed=0)
        alarm_low = None
        for i in range(600):
            val = rng.betavariate(5.0, 3.0) - (0.15 if i >= 200 else 0.0)
            step = mart.update(val)
            if alarm_low is None and step.log10_m >= 2.0:
                alarm_low = step
        self.assertIsNotNone(alarm_low)
        assert alarm_low is not None
        self.assertGreaterEqual(alarm_low.index, 200)
        self.assertLess(alarm_low.index - 200, 180)
        self.assertEqual(alarm_low.dominant_side, "low")

        # Upward shift in binary abstention flag (0.05 -> 0.45) after 200 recalls.
        rng = random.Random(23)
        mart_hi = ld.OnlineExchangeabilityMartingale(direction="two_sided", seed=0)
        alarm_hi = None
        for i in range(500):
            p = 0.45 if i >= 200 else 0.05
            val = 1.0 if rng.random() < p else 0.0
            step = mart_hi.update(val)
            if alarm_hi is None and step.log10_m >= 2.0:
                alarm_hi = step
        self.assertIsNotNone(alarm_hi)
        assert alarm_hi is not None
        self.assertGreaterEqual(alarm_hi.index, 200)
        self.assertEqual(alarm_hi.dominant_side, "high")

        # Late change after 1,500 stable steps: Sleepy Jumper recovers without reset.
        rng = random.Random(31)
        mart_late = ld.OnlineExchangeabilityMartingale(direction="low", seed=0)
        alarm_late = None
        for i in range(1900):
            val = rng.betavariate(5.0, 3.0) - (0.20 if i >= 1500 else 0.0)
            step = mart_late.update(val)
            if alarm_late is None and step.log10_m >= 2.0:
                alarm_late = step
        self.assertIsNotNone(alarm_late)
        assert alarm_late is not None
        self.assertGreaterEqual(alarm_late.index, 1500)
        self.assertLess(alarm_late.index - 1500, 200)


class LedgerDriftEndToEndTests(unittest.TestCase):
    def _write_ledger(self, records):
        d = tmpdir(self)
        p = d / "calls.jsonl"
        with open(p, "w", encoding="utf-8") as fh:
            for r in records:
                fh.write(json.dumps(r) + "\n")
        return p

    def test_reads_committed_observability_sample_with_torn_tail(self):
        sample = (
            pathlib.Path(__file__).resolve().parent.parent
            / "observability"
            / "sample"
            / "calls.jsonl"
        )
        ledger = ld.load_ledger([str(sample)])
        report = ld.analyze_ledger(ledger, min_points=1)
        self.assertEqual(report["ledger_schema"]["dropped_lines"], 4)
        self.assertEqual(report["ledger_schema"]["unparseable_lines"], 1)
        self.assertTrue(
            any(ev["kind"] == "binary_change" for ev in report["operational_events"])
        )
        text = ld.format_text(report)
        self.assertIn("4 LINES DROPPED", text)
        # Hygiene: sample query/concept strings must never leak into output.
        self.assertNotIn("paginate list endpoints", text)
        self.assertNotIn("bearer tokens", text)

    def test_contract_split_query_class_derive_join_injection_and_hygiene(self):
        rng = random.Random(101)
        base_ts = ld.parse_ts("2026-11-19T10:00:00+00:00")
        records = []

        def ts_at(mins):
            return (base_ts + ld.dt.timedelta(minutes=mins)).isoformat()

        # Heartbeat 0: contract A
        records.append({
            "v": 1, "ts": ts_at(0), "kind": "stats", "session": "proj-a",
            "uptime_secs": 100, "version": "0.13.0", "git_sha": "aaa111",
            "stats": {
                "session": "proj-a",
                "embedding_contract": {"kind": "bge_m3", "model": None, "dim": 1024},
                "ledger_dropped_lines": 0, "ledger_queued_lines": 0,
            },
        })

        # 260 vector-dominant recalls (steady), 15 structural recalls, 30 derives with J4 completions.
        for i in range(260):
            cos = round(0.65 + rng.uniform(-0.04, 0.04), 5)
            records.append({
                "v": 1, "ts": ts_at(1 + i), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "agent_id": "SECRET_AGENT_ID", "outcome": "ok",
                "duration_us": 500, "query": "SECRET_QUERY_TEXT", "top_k": 8,
                "hit_count": 3,
                "hits": [{"node_id": "n1", "content": "SECRET_CONCEPT_BODY", "score": cos,
                          "legs": {"bm25": 0.2, "vector_cosine": cos}}],
                "top_legs": {"bm25": 0.2, "vector_cosine": cos, "exact_name_match": False},
                "confidence": {"value": cos, "basis": "noisy-or-v0", "provisional": True},
                "abstained": False,
            })

        # Restart + binary upgrade near minute 150
        records.append({
            "v": 1, "ts": ts_at(150), "kind": "stats", "session": "proj-a",
            "uptime_secs": 10, "version": "0.14.0", "git_sha": "bbb222",
            "stats": {
                "session": "proj-a",
                "embedding_contract": {"kind": "bge_m3", "model": None, "dim": 1024},
                "ledger_dropped_lines": 0, "ledger_queued_lines": 0,
            },
        })

        # Traversal recalls -> separate query class via score-independent response_annotations.
        for i in range(15):
            records.append({
                "v": 1, "ts": ts_at(10 + i), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "agent_id": "SECRET_AGENT_ID", "outcome": "ok",
                "duration_us": 200, "query": "what depends on x", "top_k": 8,
                "hit_count": 2,
                "hits": [{"node_id": "n2", "content": "SECRET_CONCEPT_BODY", "score": 1.0, "legs": {}}],
                "response_annotations": ["traversal"],
                "top_legs": {"bm25": None, "vector_cosine": None, "exact_name_match": False},
            })

        # Keyword-only confidence basis -> must not mix with noisy-or-v0.
        for i in range(12):
            records.append({
                "v": 1, "ts": ts_at(20 + i), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "agent_id": "SECRET_AGENT_ID", "outcome": "ok",
                "duration_us": 250, "query": "kw only", "top_k": 8, "hit_count": 1,
                "hits": [{"node_id": "n3", "content": "SECRET_CONCEPT_BODY", "score": 0.7,
                          "legs": {"bm25": 4.0}}],
                "top_legs": {"bm25": 4.0, "vector_cosine": None, "exact_name_match": False},
                "confidence": {"value": 4.0 / 9.0, "basis": "keyword-only-v0", "provisional": True},
            })

        # Async derives joined via J4 completion lines for semantic_merged_rate.
        for i in range(25):
            rcpt = f"lwr1.test.{i}"
            records.append({
                "v": 1, "ts": ts_at(30 + i), "kind": "call", "tool": "lambo_derive",
                "session": "proj-a", "agent_id": "SECRET_AGENT_ID", "outcome": "ok",
                "duration_us": 100, "concepts_requested": 2, "admitted": True, "receipt": rcpt,
            })
            records.append({
                "v": 1, "ts": ts_at(30 + i + 0.1), "kind": "completion",
                "session": "proj-a", "agent_id": "SECRET_AGENT_ID", "receipt": rcpt,
                "state": "applied", "created_count": 2, "matched_count": 0,
                "semantic_merged": 1 if i % 4 == 0 else 0, "reinforced": 0,
            })

        # Contract change at minute 300 -> new stream bucket, no cross-contract alarm.
        records.append({
            "v": 1, "ts": ts_at(300), "kind": "stats", "session": "proj-a",
            "uptime_secs": 15, "version": "0.14.0", "git_sha": "bbb222",
            "stats": {
                "session": "proj-a",
                "embedding_contract": {"kind": "embeddinggemma2", "model": "eg2", "dim": 768},
                "ledger_dropped_lines": 0, "ledger_queued_lines": 0,
            },
        })
        for i in range(15):
            records.append({
                "v": 1, "ts": ts_at(301 + i), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "agent_id": "SECRET_AGENT_ID", "outcome": "ok",
                "duration_us": 500, "query": "SECRET_QUERY_TEXT", "top_k": 8, "hit_count": 2,
                "hits": [{"node_id": "n1", "content": "SECRET_CONCEPT_BODY", "score": 0.9,
                          "legs": {"vector_cosine": 0.9}}],
                "top_legs": {"bm25": 0.0, "vector_cosine": 0.9, "exact_name_match": False},
            })

        # Lease event with sensitive counterparty/socket must not leak.
        records.append({
            "v": 1, "ts": ts_at(151), "kind": "lease", "session": "proj-a",
            "agent_id": "SECRET_AGENT_ID", "event": "refused", "side": "loser",
            "counterparty": "SECRET_HOLDER@host#999",
        })

        path = self._write_ledger(records)
        ledger = ld.load_ledger([str(path)])

        # Without injection: stable streams trigger no alarms, and both contracts + classes exist.
        clean = ld.analyze_ledger(ledger, seed=0)
        self.assertEqual(clean["alarms"], [])
        stream_keys = {
            (s["contract"], s["query_class"], s["stream"]) for s in clean["streams"]
        }
        self.assertIn(("bge_m3/(default)/1024", "hybrid", "top_vector_cosine"), stream_keys)
        self.assertIn(("bge_m3/(default)/1024", "traversal", "hit_count"), stream_keys)
        self.assertIn(("bge_m3/(default)/1024", "hybrid", "confidence:noisy-or-v0"), stream_keys)
        self.assertIn(("bge_m3/(default)/1024", "keyword_only", "confidence:keyword-only-v0"), stream_keys)
        self.assertIn(("bge_m3/(default)/1024", "derive", "semantic_merged_rate"), stream_keys)
        self.assertIn(("embeddinggemma2/eg2/768", "hybrid", "top_vector_cosine"), stream_keys)
        self.assertTrue(
            any(ev["kind"] == "contract_change" for ev in clean["operational_events"])
        )

        # With injected -0.15 drop at index 150 on top_vector_cosine: fires alarm + nearby restart/upgrade!
        injected = ld.analyze_ledger(
            ledger,
            seed=0,
            injection=ld.ShiftInjection(stream="top_vector_cosine", after=150, delta=-0.15),
        )
        vec_alarms = [
            a for a in injected["alarms"]
            if a["stream"] == "top_vector_cosine" and a["contract"] == "bge_m3/(default)/1024"
        ]
        self.assertEqual(len(vec_alarms), 1)
        alarm = vec_alarms[0]
        self.assertEqual(alarm["direction"], "low")
        self.assertIsNotNone(alarm["detection_delay"])
        self.assertGreaterEqual(alarm["detection_delay"], 0)
        self.assertLess(alarm["detection_delay"], 80)
        self.assertGreater(alarm["before"]["median"], alarm["after"]["median"])
        nearby_kinds = {ev["kind"] for ev in alarm["nearby_events"]}
        self.assertIn("restart", nearby_kinds)
        self.assertIn("binary_change", nearby_kinds)

        # Hygiene check on both JSON and text outputs.
        rendered_json = json.dumps(injected)
        rendered_text = ld.format_text(injected)
        for secret in ("SECRET_QUERY_TEXT", "SECRET_CONCEPT_BODY", "SECRET_AGENT_ID", "SECRET_HOLDER"):
            self.assertNotIn(secret, rendered_json)
            self.assertNotIn(secret, rendered_text)

        # CLI smoke test (--json and text modes).
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = ld.main([
                str(path), "--json",
                "--inject-stream", "top_vector_cosine",
                "--inject-after", "150",
                "--inject-delta", "-0.15",
            ])
        self.assertEqual(rc, 0)
        parsed = json.loads(buf.getvalue())
        self.assertEqual(len(parsed["alarms"]), 1)

    def test_cli_validation_errors(self):
        d = tmpdir(self)
        missing = d / "nope.jsonl"
        err_buf = io.StringIO()
        with redirect_stderr(err_buf):
            rc = ld.main([str(missing)])
        self.assertEqual(rc, 2)
        self.assertIn("cannot open ledger", err_buf.getvalue())

        # Partial injection flags are refused by argparse.
        with self.assertRaises(SystemExit) as cm, redirect_stderr(io.StringIO()):
            ld.main([str(missing), "--inject-stream", "top_fused"])
        self.assertNotEqual(cm.exception.code, 0)

    def test_score_independent_query_class_prevents_selection_bias_and_misclassification(self):
        # 1. Downward shift in top_vec below kw_strength stays in "hybrid" and triggers alarm.
        rng = random.Random(202)
        base_ts = ld.parse_ts("2026-11-19T12:00:00Z")
        records = [
            {
                "v": 1, "ts": base_ts.isoformat(), "kind": "stats", "session": "s1",
                "uptime_secs": 10, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "s1",
                    "embedding_contract": {"kind": "bge_m3", "model": None, "dim": 1024},
                    "ledger_dropped_lines": 0, "ledger_queued_lines": 0,
                },
            }
        ]
        for i in range(320):
            # High BM25 (~4.0 -> kw_strength ~0.444); vector drops from ~0.65 to ~0.25 after 180.
            vec = (0.25 if i >= 180 else 0.65) + rng.uniform(-0.04, 0.04)
            records.append({
                "v": 1,
                "ts": (base_ts + ld.dt.timedelta(seconds=10 + i)).isoformat(),
                "kind": "call",
                "tool": "lambo_recall",
                "session": "s1",
                "outcome": "ok",
                "hits": [{"score": max(0.4, vec), "legs": {"bm25": 4.0, "vector_cosine": vec}}],
                "top_legs": {"bm25": 4.0, "vector_cosine": vec, "exact_name_match": False},
                "response_annotations": [],
            })
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        report = ld.analyze_ledger(ledger, seed=0)
        vec_streams = [s for s in report["streams"] if s["stream"] == "top_vector_cosine"]
        self.assertEqual(len(vec_streams), 1)
        self.assertEqual(vec_streams[0]["query_class"], "hybrid")
        self.assertEqual(vec_streams[0]["n"], 320)
        vec_alarms = [a for a in report["alarms"] if a["stream"] == "top_vector_cosine"]
        self.assertEqual(len(vec_alarms), 1)
        self.assertEqual(vec_alarms[0]["direction"], "low")
        self.assertEqual(vec_alarms[0]["query_class"], "hybrid")

        # 2. Zero-match BM25 regressions stay in hybrid / keyword_only instead of "other".
        self.assertEqual(
            ld.classify_recall_query(
                {"response_annotations": []}, 0.0, 0.7, False, True
            ),
            "hybrid",
        )
        self.assertEqual(
            ld.classify_recall_query(
                {"response_annotations": []}, 0.0, None, False, True
            ),
            "keyword_only",
        )
        self.assertEqual(
            ld.classify_recall_query(
                {"response_annotations": []}, None, 0.7, False, True
            ),
            "vector_only",
        )
        self.assertEqual(
            ld.classify_recall_query(
                {"response_annotations": []}, 3.0, 0.7, True, True
            ),
            "exact_name",
        )

        # 3. Vector-degraded image/vector recall returning only recent/empty-legs hits
        #    must NEVER be misclassified as traversal.
        self.assertEqual(
            ld.classify_recall_query(
                {
                    "by": "image",
                    "hits": [{"score": 0.35, "legs": {"recent": 0.35}}],
                    "hit_count": 1,
                    "response_annotations": ["vector_degraded"],
                },
                None, None, False, True,
            ),
            "image",
        )
        self.assertEqual(
            ld.classify_recall_query(
                {
                    "by": "image",
                    "hits": [{"score": 0.35, "legs": {}}],
                    "hit_count": 1,
                },
                None, None, False, False,
            ),
            "image",
        )
        self.assertEqual(
            ld.classify_recall_query(
                {"by": "vector", "hits": [], "response_annotations": []},
                None, 0.6, False, True,
            ),
            "vector",
        )
        # Legacy pre-0.13.0 traversal without response_annotations still classifies as traversal.
        self.assertEqual(
            ld.classify_recall_query(
                {"hits": [{"score": 1.0, "legs": {}}]},
                None, None, False, False,
            ),
            "traversal",
        )

    def test_mixed_legacy_and_013_bm25_scales_stay_separate_with_zero_alarms(self):
        rng = random.Random(303)
        base_ts = ld.parse_ts("2026-11-19T14:00:00Z")
        records = [
            {
                "v": 1, "ts": base_ts.isoformat(), "kind": "stats", "session": "s1",
                "uptime_secs": 100, "version": "0.12.0", "git_sha": "old",
                "stats": {
                    "session": "s1",
                    "embedding_contract": {"kind": "bge_m3", "model": None, "dim": 1024},
                    "ledger_dropped_lines": 0, "ledger_queued_lines": 0,
                },
            }
        ]
        # 120 pre-0.13.0 calls: normalized [0, 1] BM25 in hits[].legs (no top_legs).
        for i in range(120):
            b = round(rng.uniform(0.30, 0.70), 4)
            v = round(rng.uniform(0.55, 0.75), 4)
            records.append({
                "v": 1,
                "ts": (base_ts + ld.dt.timedelta(seconds=10 + i)).isoformat(),
                "kind": "call",
                "tool": "lambo_recall",
                "session": "s1",
                "outcome": "ok",
                "hits": [{"score": v, "legs": {"bm25": b, "vector_cosine": v}}],
                "response_annotations": [],
            })
        # 120 0.13.0+ calls: raw unbounded BM25 in top_legs.
        for i in range(120):
            b = round(rng.uniform(6.0, 12.0), 4)
            v = round(rng.uniform(0.55, 0.75), 4)
            records.append({
                "v": 1,
                "ts": (base_ts + ld.dt.timedelta(seconds=200 + i)).isoformat(),
                "kind": "call",
                "tool": "lambo_recall",
                "session": "s1",
                "outcome": "ok",
                "hits": [{"score": v, "legs": {"bm25": b, "vector_cosine": v}}],
                "top_legs": {"bm25": b, "vector_cosine": v, "exact_name_match": False},
                "response_annotations": [],
            })
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        report = ld.analyze_ledger(ledger, seed=0)
        self.assertEqual(report["alarms"], [])
        by_stream = {s["stream"]: s for s in report["streams"]}
        self.assertIn("top_bm25_legacy", by_stream)
        self.assertIn("top_bm25", by_stream)
        self.assertEqual(by_stream["top_bm25_legacy"]["n"], 120)
        self.assertEqual(by_stream["top_bm25"]["n"], 120)

    def test_derive_semantic_merged_rate_fallback_denominator_and_bounds(self):
        empty_comp = ({}, {})
        # Semantically merged concepts are already included in `created`, so denom = created + matched.
        self.assertAlmostEqual(
            ld._derive_semantic_merged_rate(
                {"created": 1, "matched": 0, "semantic_merged": 1}, "s", empty_comp
            ),
            1.0,
        )
        self.assertAlmostEqual(
            ld._derive_semantic_merged_rate(
                {"created": 1, "matched": 4, "semantic_merged": 1}, "s", empty_comp
            ),
            0.2,
        )
        # J4 completion fallback without concepts_requested.
        comps = ld._completions_by_receipt([
            {
                "ts": "2026-11-19T10:00:01Z",
                "session": "s",
                "receipt": "r1",
                "state": "applied",
                "created_count": 1,
                "matched_count": 4,
                "semantic_merged": 1,
            }
        ])
        self.assertAlmostEqual(
            ld._derive_semantic_merged_rate({"receipt": "r1"}, "s", comps),
            0.2,
        )
        # Malformed counts (sm > denom or negative or bool) are skipped cleanly.
        self.assertIsNone(
            ld._derive_semantic_merged_rate(
                {"created": 1, "matched": 0, "semantic_merged": 2}, "s", empty_comp
            )
        )
        self.assertIsNone(
            ld._derive_semantic_merged_rate(
                {"concepts_requested": 1, "semantic_merged": 2}, "s", empty_comp
            )
        )
        self.assertIsNone(
            ld._derive_semantic_merged_rate(
                {"created": 2, "matched": 0, "semantic_merged": -1}, "s", empty_comp
            )
        )
        self.assertIsNone(
            ld._derive_semantic_merged_rate(
                {"created": True, "matched": 0, "semantic_merged": True}, "s", empty_comp
            )
        )

    def test_parse_ts_formats_rejections_and_offset_sorting(self):
        # Lowercase z and 1/2/4/5/7/9 fractional digits.
        self.assertEqual(
            ld.parse_ts("2026-11-19T10:00:00z"),
            ld.dt.datetime(2026, 11, 19, 10, 0, 0, tzinfo=ld.dt.timezone.utc),
        )
        expected_us = {
            "2026-11-19T10:00:00.5z": 500_000,
            "2026-11-19T10:00:00.12Z": 120_000,
            "2026-11-19T10:00:00.1234Z": 123_400,
            "2026-11-19T10:00:00.12345+00:00": 123_450,
            "2026-11-19T10:00:00.1234567Z": 123_456,
            "2026-11-19T10:00:00.123456789+05:30": 123_456,
        }
        for raw, want_us in expected_us.items():
            parsed = ld.parse_ts(raw)
            self.assertEqual(parsed.tzinfo, ld.dt.timezone.utc, raw)
            self.assertEqual(parsed.microsecond, want_us, raw)

        # Empty fractional seconds and naive timestamps must be rejected.
        for bad in (
            "2026-11-19T10:00:00.Z",
            "2026-11-19T10:00:00.abcZ",
            "2026-11-19T10:00:00",
            "2026-11-19T10:00:00.123",
            "2026-11-19",
        ):
            with self.assertRaises(ValueError, msg=f"expected rejection for {bad!r}"):
                ld.parse_ts(bad)

        # Naive --inject-after raises LedgerDriftError and exits 2 via CLI.
        with self.assertRaises(ld.LedgerDriftError):
            ld.parse_inject_after("2026-11-19T10:00:00")
        p = self._write_ledger([
            {"v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "startup", "session": "s"}
        ])
        err_buf = io.StringIO()
        with redirect_stderr(err_buf):
            rc = ld.main([
                str(p),
                "--inject-stream", "top_fused",
                "--inject-after", "2026-11-19T10:00:00",
                "--inject-delta", "-0.1",
            ])
        self.assertEqual(rc, 2)

        # Mixed UTC offsets and microsecond ordering never suffer float ULP loss.
        recs = [
            {"ts": "2026-11-19T05:00:00.000001-05:00"},  # 10:00:00.000001Z
            {"ts": "2026-11-19T15:29:59.999999+05:30"},  # 09:59:59.999999Z
            {"ts": "2026-11-19T10:00:00.000000Z"},       # 10:00:00.000000Z
        ]
        ordered = [r["ts"] for r in sorted(recs, key=ld._ts_key)]
        self.assertEqual(
            ordered,
            [
                "2026-11-19T15:29:59.999999+05:30",
                "2026-11-19T10:00:00.000000Z",
                "2026-11-19T05:00:00.000001-05:00",
            ],
        )

    def test_provenance_handles_null_stats_multi_session_counts_and_rejects_bools(self):
        records = [
            # "stats": null must not crash provenance().
            {
                "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "stats",
                "session": "sess-null", "version": "0.13.0", "git_sha": "aaa",
                "stats": None,
            },
            # Multi-session counts: sess-a max dropped=3, latest queued=1.
            {
                "v": 1, "ts": "2026-11-19T10:01:00Z", "kind": "stats",
                "session": "sess-a", "uptime_secs": 10, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "sess-a", "ledger_dropped_lines": 1, "ledger_queued_lines": 5},
            },
            {
                "v": 1, "ts": "2026-11-19T10:02:00Z", "kind": "stats",
                "session": "sess-a", "uptime_secs": 70, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "sess-a", "ledger_dropped_lines": 3, "ledger_queued_lines": 1},
            },
            # sess-b max dropped=2, latest queued=4.
            {
                "v": 1, "ts": "2026-11-19T10:03:00Z", "kind": "stats",
                "session": "sess-b", "uptime_secs": 10, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "sess-b", "ledger_dropped_lines": 2, "ledger_queued_lines": 4},
            },
            # Booleans must be rejected for v (not dispatched to v1) and inside v1 payloads.
            {
                "v": True, "ts": "2026-11-19T10:04:00Z", "kind": "stats",
                "session": "sess-bool-v", "uptime_secs": True, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "sess-bool-v",
                    "embedding_contract": {"kind": "bge_m3", "model": None, "dim": True},
                    "ledger_dropped_lines": 99,
                    "ledger_queued_lines": 99,
                },
            },
            {
                "v": 1, "ts": "2026-11-19T10:04:30Z", "kind": "stats",
                "session": "sess-bool", "uptime_secs": True, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "sess-bool",
                    "embedding_contract": {"kind": "bge_m3", "model": None, "dim": True},
                    "ledger_dropped_lines": True,
                    "ledger_queued_lines": True,
                },
            },
            {
                "v": 1, "ts": "2026-11-19T10:05:00Z", "kind": "call", "tool": "lambo_recall",
                "session": "sess-a", "outcome": "ok", "hit_count": True,
                "hits": [{"score": 0.5}], "response_annotations": [],
            },
        ]
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        prov = ledger.provenance()
        self.assertEqual(prov["dropped_lines"], 5)
        self.assertEqual(prov["queued_lines"], 5)
        self.assertEqual(prov["versions"], [1])
        self.assertEqual(prov["unknown_version_lines"], 1)
        streams = ld.extract_streams(ledger)
        self.assertFalse(any(k.stream == "hit_count" for k in streams))
        self.assertFalse(any("True" in k.contract for k in streams))

    def test_scoped_recall_and_derive_attribute_to_addressed_layer(self):
        records = [
            {
                "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "stats", "session": "proj-a",
                "uptime_secs": 60, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-a",
                    "embedding_contract": {"kind": "bge_m3", "model": None, "dim": 1024},
                },
            },
            {
                "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "stats", "session": "general",
                "uptime_secs": 60, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "general",
                    "embedding_contract": {"kind": "embeddinggemma2", "model": "eg2", "dim": 768},
                },
            },
            {
                "v": 1, "ts": "2026-11-19T10:01:00Z", "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "scope": "general", "outcome": "ok",
                "hits": [{"score": 0.8}],
                "top_legs": {"bm25": 2.0, "vector_cosine": 0.8, "exact_name_match": False},
                "response_annotations": [],
            },
            {
                "v": 1, "ts": "2026-11-19T10:02:00Z", "kind": "call", "tool": "lambo_derive",
                "session": "proj-a", "scope": "general", "outcome": "ok",
                "concepts_requested": 2, "receipt": "lwr1.scoped.1",
            },
            {
                "v": 1, "ts": "2026-11-19T10:02:01Z", "kind": "completion",
                "session": "general", "receipt": "lwr1.scoped.1", "state": "applied",
                "created_count": 2, "matched_count": 0, "semantic_merged": 1,
            },
        ]
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        streams = ld.extract_streams(ledger)
        keys = {(k.session, k.contract, k.query_class, k.stream) for k in streams}
        self.assertIn(("general", "embeddinggemma2/eg2/768", "hybrid", "top_vector_cosine"), keys)
        self.assertIn(("general", "embeddinggemma2/eg2/768", "derive", "semantic_merged_rate"), keys)
        self.assertFalse(any(k.session == "proj-a" for k in streams))

    def test_post_restart_contract_swap_calls_before_first_heartbeat_map_to_new_contract(self):
        base = ld.parse_ts("2026-11-19T10:00:00Z")

        def ts(sec):
            return (base + ld.dt.timedelta(seconds=sec)).isoformat()

        # Case A: restart + startup marker + unrelated lease-loser startup + uptime_secs.
        records = [
            {
                "v": 1, "ts": ts(0), "kind": "stats", "session": "proj-a",
                "uptime_secs": 600, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-a",
                    "embedding_contract": {"kind": "bge_m3", "model": "old", "dim": 1024},
                },
            },
            # Call before restart -> old contract.
            {
                "v": 1, "ts": ts(60), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.6, "exact_name_match": False},
                "response_annotations": [],
            },
            # Process 2 starts at t=300 with new contract; call arrives at t=330 before first HB.
            {
                "v": 1, "ts": ts(300), "kind": "startup", "session": "proj-a",
                "transport": "http", "state": "acquiring",
            },
            {
                "v": 1, "ts": ts(330), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.9, "exact_name_match": False},
                "response_annotations": [],
            },
            # Unrelated lease loser startup at t=400 must not shift anchor past t=330.
            {
                "v": 1, "ts": ts(400), "kind": "startup", "session": "proj-a",
                "transport": "http", "state": "acquiring",
            },
            # First heartbeat of Process 2 at t=600 with uptime_secs=300 (booted at t=300).
            {
                "v": 1, "ts": ts(600), "kind": "stats", "session": "proj-a",
                "uptime_secs": 300, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-a",
                    "embedding_contract": {"kind": "embeddinggemma2", "model": "new", "dim": 768},
                },
            },
        ]
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        streams = ld.extract_streams(ledger)
        old_obs = streams[
            ld.StreamKey("proj-a", "bge_m3/old/1024", "hybrid", "top_vector_cosine", 0)
        ]
        new_obs = streams[
            ld.StreamKey("proj-a", "embeddinggemma2/new/768", "hybrid", "top_vector_cosine", 1)
        ]
        self.assertEqual([o.value for o in old_obs], [0.6])
        self.assertEqual([o.value for o in new_obs], [0.9])

        # Case B: uptime_secs only (no startup markers at all).
        no_startups = [r for r in records if r["kind"] != "startup"]
        ledger_b = ld.load_ledger([str(self._write_ledger(no_startups))])
        streams_b = ld.extract_streams(ledger_b)
        self.assertEqual(
            [
                o.value
                for o in streams_b[
                    ld.StreamKey("proj-a", "bge_m3/old/1024", "hybrid", "top_vector_cosine", 0)
                ]
            ],
            [0.6],
        )
        self.assertEqual(
            [
                o.value
                for o in streams_b[
                    ld.StreamKey("proj-a", "embeddinggemma2/new/768", "hybrid", "top_vector_cosine", 1)
                ]
            ],
            [0.9],
        )

    def test_analyze_ledger_validates_parameters_upfront_even_on_short_ledger(self):
        # 1-line ledger with min_points=10 never constructs a martingale, so validation
        # must fire upfront before the stream loop.
        path = self._write_ledger([
            {"v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "startup", "session": "s"}
        ])
        ledger = ld.load_ledger([str(path)])

        bad_kwargs_list = [
            {"direction": "sideways"},
            {"epsilons": ()},
            {"epsilons": (0.0, 0.5)},
            {"epsilons": (1.0,)},
            {"epsilons": (float("nan"),)},
            {"jump_rate": -0.01},
            {"jump_rate": 1.1},
            {"jump_rate": float("nan")},
            {"sleep_weight": -0.1},
            {"sleep_weight": 1.0},
            {"sleep_weight": float("nan")},
            {"threshold": 1.0},
            {"threshold": float("nan")},
            {"threshold": float("inf")},
            {"min_points": 0},
            {"min_points": True},
            {"event_window_minutes": -1.0},
            {"event_window_minutes": float("nan")},
            {"injection": ld.ShiftInjection(stream="top_fused", after=0, delta=float("nan"))},
            {"injection": ld.ShiftInjection(stream="top_fused", after=0, delta=float("inf"))},
        ]
        for kwargs in bad_kwargs_list:
            with self.assertRaises(ld.LedgerDriftError, msg=f"expected error for {kwargs!r}"):
                ld.analyze_ledger(ledger, **kwargs)

    def test_nearby_events_preserves_distinct_events_sharing_timestamp(self):
        ts_raw = "2026-11-19T10:05:00Z"
        ts_dt = ld.parse_ts(ts_raw)
        events = [
            ld.OperationalEvent(
                ts=ts_raw, ts_dt=ts_dt, session="s1", kind="lease",
                detail={"event": "refused", "side": "loser"},
            ),
            ld.OperationalEvent(
                ts=ts_raw, ts_dt=ts_dt, session="s1", kind="lease",
                detail={"event": "refused_takeover", "side": "holder"},
            ),
            # Exact duplicate of the first event still deduplicates.
            ld.OperationalEvent(
                ts=ts_raw, ts_dt=ts_dt, session="s1", kind="lease",
                detail={"event": "refused", "side": "loser"},
            ),
        ]
        nearby = ld._nearby_events(events, "s1", [ts_dt], 60.0)
        self.assertEqual(len(nearby), 2)
        details = {ev["detail"]["event"] for ev in nearby}
        self.assertEqual(details, {"refused", "refused_takeover"})

    def test_top_fused_ignores_nan_scores_and_keeps_finite_hits(self):
        ledger = ld.ParsedLedger(
            paths=["mem"],
            calls=[
                {
                    "v": 1,
                    "ts": "2026-11-19T10:00:00Z",
                    "kind": "call",
                    "tool": "lambo_recall",
                    "session": "s1",
                    "outcome": "ok",
                    "hits": [{"score": float("nan")}, {"score": 0.75}],
                    "top_legs": {"bm25": None, "vector_cosine": 0.75, "exact_name_match": False},
                    "response_annotations": [],
                }
            ],
        )
        streams = ld.extract_streams(ledger)
        key = ld.StreamKey("s1", "unknown", "vector_only", "top_fused")
        self.assertIn(key, streams)
        self.assertEqual([o.value for o in streams[key]], [0.75])

    def test_contract_timeline_handles_null_heartbeats_lease_losers_and_uptime_overflow(self):
        base = ld.parse_ts("2026-11-19T10:00:00Z")

        def ts(sec):
            return (base + ld.dt.timedelta(seconds=sec)).isoformat()

        # 1. Post-restart intermediate embedding_contract: null and stats: null
        #    heartbeats before contract B must not break anchoring to restart.
        records_null_hb = [
            {
                "v": 1, "ts": ts(0), "kind": "stats", "session": "proj-a",
                "uptime_secs": 600, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-a",
                    "embedding_contract": {"kind": "bge_m3", "model": "old", "dim": 1024},
                },
            },
            {
                "v": 1, "ts": ts(60), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.55, "exact_name_match": False},
                "response_annotations": [],
            },
            # Restart at t=300; call arrives at t=330; then two unstamped heartbeats
            # at t=360 (embedding_contract: null) and t=420 (stats: null) precede HB B at t=600.
            {
                "v": 1, "ts": ts(300), "kind": "startup", "session": "proj-a",
                "transport": "http", "state": "acquiring",
            },
            {
                "v": 1, "ts": ts(330), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.91, "exact_name_match": False},
                "response_annotations": [],
            },
            {
                "v": 1, "ts": ts(360), "kind": "stats", "session": "proj-a",
                "uptime_secs": 60, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "proj-a", "embedding_contract": None},
            },
            {
                "v": 1, "ts": ts(420), "kind": "stats", "session": "proj-a",
                "uptime_secs": 120, "version": "0.13.0", "git_sha": "aaa",
                "stats": None,
            },
            {
                "v": 1, "ts": ts(600), "kind": "stats", "session": "proj-a",
                "uptime_secs": 300, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-a",
                    "embedding_contract": {"kind": "embeddinggemma2", "model": "new", "dim": 768},
                },
            },
        ]
        ledger_null = ld.load_ledger([str(self._write_ledger(records_null_hb))])
        streams_null = ld.extract_streams(ledger_null)
        self.assertEqual(
            [
                o.value
                for o in streams_null[
                    ld.StreamKey("proj-a", "bge_m3/old/1024", "hybrid", "top_vector_cosine", 0)
                ]
            ],
            [0.55],
        )
        self.assertEqual(
            [
                o.value
                for o in streams_null[
                    ld.StreamKey("proj-a", "embeddinggemma2/new/768", "hybrid", "top_vector_cosine", 1)
                ]
            ],
            [0.91],
        )

        # 2. When uptime_secs proves the serving process did NOT restart across a
        #    contract change (e.g. in-place cutover), an unrelated lease-loser startup
        #    must NOT hijack the anchor, and calls in the ambiguous (prev_hb, new_hb)
        #    interval are left unattributed ("unknown") rather than guessed.
        records_loser = [
            {
                "v": 1, "ts": ts(0), "kind": "stats", "session": "proj-b",
                "uptime_secs": 1000, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-b",
                    "embedding_contract": {"kind": "bge_m3", "model": "old", "dim": 1024},
                },
            },
            # Call at t=0 (at/before HB A) stays on contract A.
            {
                "v": 1, "ts": ts(0), "kind": "call", "tool": "lambo_recall",
                "session": "proj-b", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.51, "exact_name_match": False},
                "response_annotations": [],
            },
            # Unrelated lease-loser starts at t=200 and fails to take over.
            {
                "v": 1, "ts": ts(200), "kind": "startup", "session": "proj-b",
                "transport": "http", "state": "acquiring",
            },
            # Call at t=330 falls in the ambiguous (0, 600) in-place cutover interval.
            {
                "v": 1, "ts": ts(330), "kind": "call", "tool": "lambo_recall",
                "session": "proj-b", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.62, "exact_name_match": False},
                "response_annotations": [],
            },
            # At t=600, uptime_secs=1600 proves process started at t=-1000 (<= prev_hb_ts=0).
            {
                "v": 1, "ts": ts(600), "kind": "stats", "session": "proj-b",
                "uptime_secs": 1600, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-b",
                    "embedding_contract": {"kind": "embeddinggemma2", "model": "new", "dim": 768},
                },
            },
            {
                "v": 1, "ts": ts(630), "kind": "call", "tool": "lambo_recall",
                "session": "proj-b", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.88, "exact_name_match": False},
                "response_annotations": [],
            },
        ]
        ledger_loser = ld.load_ledger([str(self._write_ledger(records_loser))])
        streams_loser = ld.extract_streams(ledger_loser)
        self.assertEqual(
            [
                o.value
                for o in streams_loser[
                    ld.StreamKey("proj-b", "bge_m3/old/1024", "hybrid", "top_vector_cosine", 0)
                ]
            ],
            [0.51],
        )
        self.assertEqual(
            [
                o.value
                for o in streams_loser[
                    ld.StreamKey("proj-b", "unknown", "hybrid", "top_vector_cosine", 1)
                ]
            ],
            [0.62],
        )
        self.assertEqual(
            [
                o.value
                for o in streams_loser[
                    ld.StreamKey("proj-b", "embeddinggemma2/new/768", "hybrid", "top_vector_cosine", 1)
                ]
            ],
            [0.88],
        )

        # 3. Overflow on (ts_dt - timedelta(seconds=uptime)) near year 0001 does not crash,
        #    marks (prev_hb, ts_dt) unattributed, and confirms B at ts_dt (not startup).
        timeline, _ = ld._build_contract_timeline(
            heartbeats=[
                {
                    "ts": "0001-01-01T00:00:01Z",
                    "session": "proj-min",
                    "uptime_secs": 0,
                    "stats": {
                        "session": "proj-min",
                        "embedding_contract": {"kind": "bge_m3", "model": "a", "dim": 1024},
                    },
                },
                {
                    "ts": "0001-01-01T00:00:10Z",
                    "session": "proj-min",
                    "uptime_secs": 100,
                    "stats": {
                        "session": "proj-min",
                        "embedding_contract": {"kind": "bge_m3", "model": "b", "dim": 1024},
                    },
                },
            ],
            startups=[
                {
                    "ts": "0001-01-01T00:00:05Z",
                    "session": "proj-min",
                    "transport": "http",
                    "state": "acquiring",
                },
            ],
        )
        self.assertEqual(
            timeline["proj-min"].contracts, ["bge_m3/a/1024", "unknown", "bge_m3/b/1024"]
        )
        self.assertEqual(timeline["proj-min"].epochs, [0, 1, 1])
        self.assertEqual(
            timeline["proj-min"].times[-1], ld.parse_ts("0001-01-01T00:00:10Z")
        )

        # 4. Huge uptime_secs (> 1e9) likewise marks (prev_hb, ts_dt) unattributed
        #    and confirms B at ts_dt rather than letting an in-gap startup hijack it.
        timeline_huge, _ = ld._build_contract_timeline(
            heartbeats=[
                {
                    "ts": ts(0),
                    "session": "proj-huge",
                    "uptime_secs": 1000,
                    "stats": {
                        "session": "proj-huge",
                        "embedding_contract": {"kind": "bge_m3", "model": "a", "dim": 1024},
                    },
                },
                {
                    "ts": ts(600),
                    "session": "proj-huge",
                    "uptime_secs": 2_000_000_000,
                    "stats": {
                        "session": "proj-huge",
                        "embedding_contract": {"kind": "bge_m3", "model": "b", "dim": 1024},
                    },
                },
            ],
            startups=[
                {
                    "ts": ts(200),
                    "session": "proj-huge",
                    "transport": "http",
                    "state": "acquiring",
                },
            ],
        )
        self.assertEqual(
            timeline_huge["proj-huge"].contracts, ["bge_m3/a/1024", "unknown", "bge_m3/b/1024"]
        )
        self.assertEqual(timeline_huge["proj-huge"].epochs, [0, 1, 1])
        self.assertEqual(timeline_huge["proj-huge"].times[-1], ld.parse_ts(ts(600)))

    def test_completion_receipt_fallback_never_cross_joins_different_session(self):
        comps = ld._completions_by_receipt([
            {
                "ts": "2026-11-19T10:00:01Z",
                "session": "other",
                "receipt": "rcpt-shared",
                "state": "applied",
                "created_count": 2,
                "matched_count": 0,
                "semantic_merged": 1,
            },
            {
                "ts": "2026-11-19T10:00:02Z",
                "receipt": "rcpt-legacy",
                "state": "applied",
                "created_count": 4,
                "matched_count": 0,
                "semantic_merged": 1,
            },
        ])
        # Completion stamped for session="other" must NOT cross-join onto "target".
        self.assertIsNone(
            ld._derive_semantic_merged_rate({"receipt": "rcpt-shared"}, "target", comps)
        )
        # Same completion still joins cleanly onto its own session="other".
        self.assertAlmostEqual(
            ld._derive_semantic_merged_rate({"receipt": "rcpt-shared"}, "other", comps),
            0.5,
        )
        # Legacy completion omitting "session" still joins cleanly as unscoped fallback.
        self.assertAlmostEqual(
            ld._derive_semantic_merged_rate({"receipt": "rcpt-legacy"}, "target", comps),
            0.25,
        )

    def test_extreme_offset_timestamps_raise_value_error_and_are_handled_cleanly(self):
        extreme_timestamps = (
            "0001-01-01T00:00:00+05:00",
            "9999-12-31T23:59:59-05:00",
        )
        for bad in extreme_timestamps:
            with self.assertRaises(ValueError, msg=f"expected ValueError for {bad!r}"):
                ld.parse_ts(bad)

        # Ledger containing extreme timestamps loads and analyzes cleanly (lines skipped).
        p = self._write_ledger([
            {
                "v": 1, "ts": "0001-01-01T00:00:00+05:00", "kind": "stats",
                "session": "s", "uptime_secs": 10, "stats": {"session": "s"},
            },
            {
                "v": 1, "ts": "9999-12-31T23:59:59-05:00", "kind": "call",
                "tool": "lambo_recall", "session": "s", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.5, "exact_name_match": False},
            },
            {
                "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "call",
                "tool": "lambo_recall", "session": "s", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.7, "exact_name_match": False},
                "response_annotations": [],
            },
        ])
        ledger = ld.load_ledger([str(p)])
        report = ld.analyze_ledger(ledger, min_points=1)
        vec_streams = [s for s in report["streams"] if s["stream"] == "top_vector_cosine"]
        self.assertEqual(len(vec_streams), 1)
        self.assertEqual(vec_streams[0]["n"], 1)

        # CLI --inject-after with extreme offset timestamps exits cleanly with code 2.
        for bad in extreme_timestamps:
            err_buf = io.StringIO()
            with redirect_stderr(err_buf):
                rc = ld.main([
                    str(p),
                    "--inject-stream", "top_vector_cosine",
                    "--inject-after", bad,
                    "--inject-delta", "-0.1",
                ])
            self.assertEqual(rc, 2, msg=f"expected exit 2 for --inject-after {bad!r}")

    def test_provenance_dropped_lines_sums_across_restarts_without_double_counting_same_second(self):
        # 1. Restart with uptime_secs reset (120 -> 5): 10 drops in run 1 + 5 in run 2 = 15.
        ledger_restart = ld.ParsedLedger(
            paths=["mem"],
            heartbeats=[
                {
                    "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "stats",
                    "session": "s1", "uptime_secs": 120,
                    "stats": {"session": "s1", "ledger_dropped_lines": 10},
                },
                {
                    "v": 1, "ts": "2026-11-19T10:02:00Z", "kind": "stats",
                    "session": "s1", "uptime_secs": 5,
                    "stats": {"session": "s1", "ledger_dropped_lines": 5},
                },
            ],
        )
        self.assertEqual(ledger_restart.provenance()["dropped_lines"], 15)
        restart_events = [
            ev for ev in ld.extract_operational_events(ledger_restart) if ev.kind == "restart"
        ]
        self.assertEqual(len(restart_events), 1)
        self.assertEqual(restart_events[0].session, "s1")

        # 2. Restart where run 2 drops MORE than run 1 (10 -> 12) with uptime reset (120 -> 5): 22.
        ledger_higher = ld.ParsedLedger(
            paths=["mem"],
            heartbeats=[
                {
                    "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "stats",
                    "session": "s1", "uptime_secs": 120,
                    "stats": {"session": "s1", "ledger_dropped_lines": 10},
                },
                {
                    "v": 1, "ts": "2026-11-19T10:02:00Z", "kind": "stats",
                    "session": "s1", "uptime_secs": 5,
                    "stats": {"session": "s1", "ledger_dropped_lines": 12},
                },
            ],
        )
        self.assertEqual(ledger_higher.provenance()["dropped_lines"], 22)

        # 3. Restart where d drops even without uptime_secs: 10 -> 4 => 14.
        ledger_no_uptime = ld.ParsedLedger(
            paths=["mem"],
            heartbeats=[
                {
                    "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "stats",
                    "session": "s1",
                    "stats": {"session": "s1", "ledger_dropped_lines": 10},
                },
                {
                    "v": 1, "ts": "2026-11-19T10:02:00Z", "kind": "stats",
                    "session": "s1",
                    "stats": {"session": "s1", "ledger_dropped_lines": 4},
                },
            ],
        )
        self.assertEqual(ledger_no_uptime.provenance()["dropped_lines"], 14)

        # 4. Same-run heartbeats with identical uptime_secs == 10 and dropped == 7 must NOT double-count
        #    and must NOT emit a spurious restart event.
        ledger_same_sec = ld.ParsedLedger(
            paths=["mem"],
            heartbeats=[
                {
                    "v": 1, "ts": "2026-11-19T10:00:10.100Z", "kind": "stats",
                    "session": "s1", "uptime_secs": 10,
                    "stats": {"session": "s1", "ledger_dropped_lines": 7},
                },
                {
                    "v": 1, "ts": "2026-11-19T10:00:10.800Z", "kind": "stats",
                    "session": "s1", "uptime_secs": 10,
                    "stats": {"session": "s1", "ledger_dropped_lines": 7},
                },
            ],
        )
        self.assertEqual(ledger_same_sec.provenance()["dropped_lines"], 7)
        self.assertEqual(
            [ev for ev in ld.extract_operational_events(ledger_same_sec) if ev.kind == "restart"],
            [],
        )

    def test_provenance_dropped_lines_clears_stale_uptime_after_uptime_omitted_restart(self):
        # Post-restart heartbeat omits uptime_secs (detected via counter drop 10 -> 5);
        # subsequent heartbeat in the same new run restores uptime_secs=60 (< pre-restart 120)
        # with d=5 and must NOT double-count (15, not 20).
        ledger = ld.ParsedLedger(
            paths=["mem"],
            heartbeats=[
                {
                    "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "stats",
                    "session": "s1", "uptime_secs": 120,
                    "stats": {"session": "s1", "ledger_dropped_lines": 10},
                },
                {
                    "v": 1, "ts": "2026-11-19T10:01:00Z", "kind": "stats",
                    "session": "s1",
                    "stats": {"session": "s1", "ledger_dropped_lines": 5},
                },
                {
                    "v": 1, "ts": "2026-11-19T10:02:00Z", "kind": "stats",
                    "session": "s1", "uptime_secs": 60,
                    "stats": {"session": "s1", "ledger_dropped_lines": 5},
                },
            ],
        )
        self.assertEqual(ledger.provenance()["dropped_lines"], 15)

    def test_analyze_ledger_rejects_extreme_offset_injection_after_datetime(self):
        ledger = ld.ParsedLedger(
            paths=["mem"],
            calls=[
                {
                    "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "call",
                    "tool": "lambo_recall", "session": "s1", "outcome": "ok",
                    "hits": [{"score": 0.8}],
                    "top_legs": {"bm25": 1.0, "vector_cosine": 0.8, "exact_name_match": False},
                    "response_annotations": [],
                },
            ],
        )
        extreme_afters = (
            ld.dt.datetime(1, 1, 1, tzinfo=ld.dt.timezone(ld.dt.timedelta(hours=5))),
            ld.dt.datetime(9999, 12, 31, 23, 59, 59, tzinfo=ld.dt.timezone(ld.dt.timedelta(hours=-5))),
        )
        for bad_after in extreme_afters:
            with self.assertRaises(ld.LedgerDriftError, msg=f"expected LedgerDriftError for {bad_after!r}"):
                ld.analyze_ledger(
                    ledger,
                    min_points=1,
                    injection=ld.ShiftInjection("top_fused", bad_after, -0.1),
                )

        # In-range non-UTC aware datetime is normalized to UTC and applied cleanly.
        in_range_after = ld.dt.datetime(
            2026, 11, 19, 12, 0, 0, tzinfo=ld.dt.timezone(ld.dt.timedelta(hours=2))
        )
        report = ld.analyze_ledger(
            ledger,
            min_points=1,
            injection=ld.ShiftInjection("top_fused", in_range_after, -0.1),
        )
        self.assertEqual(report["config"]["injection"]["after"], "2026-11-19T10:00:00+00:00")
        fused = [s for s in report["streams"] if s["stream"] == "top_fused"][0]
        self.assertAlmostEqual(fused["summary"]["mean"], 0.7)

    def test_format_text_handles_alarm_preceding_injection(self):
        report = {
            "report": "ledger_drift",
            "ledger": ["calls.jsonl"],
            "ledger_schema": {
                "versions": [1],
                "known_versions": [1],
                "unknown_version_lines": 0,
                "unparseable_lines": 0,
                "dropped_lines": 0,
                "queued_lines": 0,
                "call_lines": 50,
                "heartbeat_lines": 1,
                "completion_lines": 0,
                "startup_lines": 0,
                "lease_lines": 0,
                "unknown_kind_lines": 0,
                "binaries": [],
            },
            "config": {
                "method": "online_sleepy_jumper_conformal_martingale",
                "threshold": 100.0,
                "ville_false_alarm_bound": 0.01,
                "direction": "two_sided",
                "by_query_class": True,
                "min_points": 10,
                "seed": 0,
                "injection": {"stream": "top_vector_cosine", "after": 40, "delta": -0.1},
            },
            "note": "diagnostic only",
            "operational_events": [],
            "streams": [],
            "alarms": [
                {
                    "session": "s1",
                    "contract": "bge_m3/(default)/1024",
                    "query_class": "hybrid",
                    "stream": "top_vector_cosine",
                    "alarm_index": 25,
                    "alarm_ts": "2026-11-19T10:25:00Z",
                    "alarm_log10_m": 2.34,
                    "peak_log10_m": 3.12,
                    "peak_index": 30,
                    "direction": "low",
                    "estimated_change_index": 15,
                    "estimated_change_ts": "2026-11-19T10:15:00Z",
                    "before": {"n": 15, "median": 0.7, "mean": 0.7, "p10": 0.65, "p90": 0.75},
                    "after": {"n": 10, "median": 0.4, "mean": 0.4, "p10": 0.35, "p90": 0.45},
                    "injected_at_index": 40,
                    "injected_delta": -0.1,
                    "detection_delay": None,
                    "nearby_events": [],
                }
            ],
        }
        text = ld.format_text(report)
        self.assertNotIn("None observation", text)
        self.assertIn("n/a (alarm preceded injection", text)
        self.assertIn("at index=40, delta=-0.1)", text)

    def test_overflowing_numeric_scores_and_hit_count_are_skipped_preserving_valid_hits(self):
        huge = 10**400
        self.assertIsNone(ld._finite_number(huge))
        records = [
            {
                "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "call", "tool": "lambo_recall",
                "session": "s1", "outcome": "ok",
                "hit_count": huge,
                "hits": [{"score": huge}, {"score": 0.75}],
                "top_legs": {"bm25": huge, "vector_cosine": 0.75, "exact_name_match": False},
                "response_annotations": [],
            },
        ]
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        streams = ld.extract_streams(ledger)
        fused = [o.value for k, obs in streams.items() if k.stream == "top_fused" for o in obs]
        vec = [
            o.value for k, obs in streams.items() if k.stream == "top_vector_cosine" for o in obs
        ]
        self.assertEqual(fused, [0.75])
        self.assertEqual(vec, [0.75])
        self.assertFalse(any(k.stream == "hit_count" for k in streams))
        self.assertFalse(any(k.stream == "top_bm25" for k in streams))

    def test_torn_utf8_final_line_counted_unparseable_and_prefix_preserved(self):
        path = tmpdir(self) / "torn_utf8.jsonl"
        good_line = json.dumps({
            "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "call", "tool": "lambo_recall",
            "session": "s1", "outcome": "ok", "hit_count": 1,
            "hits": [{"score": 0.8}],
            "top_legs": {"bm25": 2.0, "vector_cosine": 0.8, "exact_name_match": False},
            "response_annotations": [],
        }).encode("utf-8")
        # Torn multibyte UTF-8 sequence at EOF (incomplete 3-byte em-dash \xe2\x80\x94).
        torn_tail = b'{"v":1,"ts":"2026-11-19T10:01:00Z","kind":"call","query":"\xe2\x80'
        path.write_bytes(good_line + b"\n" + torn_tail)

        ledger = ld.load_ledger([str(path)])
        prov = ledger.provenance()
        self.assertEqual(prov["call_lines"], 1)
        self.assertEqual(prov["unparseable_lines"], 1)
        streams = ld.extract_streams(ledger)
        self.assertEqual(
            [o.value for o in streams[ld.StreamKey("s1", "unknown", "hybrid", "top_fused", 0)]],
            [0.8],
        )

    def test_unsupported_schema_versions_counted_but_not_dispatched_to_v1(self):
        records = [
            {
                "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "call", "tool": "lambo_recall",
                "session": "s1", "outcome": "ok",
                "hits": [{"score": 0.7}],
                "top_legs": {"bm25": 2.0, "vector_cosine": 0.7, "exact_name_match": False},
                "response_annotations": [],
            },
            # Future v2 record with incompatible scale must not enter v1 streams or heartbeats.
            {
                "v": 2, "ts": "2026-11-19T10:01:00Z", "kind": "call", "tool": "lambo_recall",
                "session": "s1", "outcome": "ok",
                "hits": [{"score": 999.0}],
                "top_legs": {"bm25": 999.0, "vector_cosine": 999.0, "exact_name_match": False},
                "response_annotations": [],
            },
            {
                "v": 2, "ts": "2026-11-19T10:02:00Z", "kind": "stats", "session": "s1",
                "uptime_secs": 10, "version": "0.99.0", "git_sha": "future",
                "stats": {"session": "s1", "ledger_dropped_lines": 500},
            },
        ]
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        prov = ledger.provenance()
        self.assertEqual(prov["versions"], [1, 2])
        self.assertEqual(prov["unknown_version_lines"], 2)
        self.assertEqual(prov["call_lines"], 1)
        self.assertEqual(prov["heartbeat_lines"], 0)
        self.assertIsNone(prov["dropped_lines"])
        streams = ld.extract_streams(ledger)
        self.assertEqual(
            [o.value for o in streams[ld.StreamKey("s1", "unknown", "hybrid", "top_fused", 0)]],
            [0.7],
        )

        # An all-v2 ledger produces a provenance warning report with 0 streams instead of crashing.
        only_v2 = ld.load_ledger([str(self._write_ledger([records[1]]))])
        rep = ld.analyze_ledger(only_v2)
        self.assertEqual(rep["ledger_schema"]["unknown_version_lines"], 1)
        self.assertEqual(rep["streams"], [])
        self.assertEqual(rep["alarms"], [])

    def test_restart_detected_when_next_observed_uptime_is_higher_across_outage(self):
        # Old run: t=0, uptime=120, drops=10.
        # New run observed 300s later: t=300, uptime=180, drops=12 -> elapsed - growth = 240s.
        base = ld.parse_ts("2026-11-19T10:00:00Z")

        def ts(sec):
            return (base + ld.dt.timedelta(seconds=sec)).isoformat()

        records_gap = [
            {
                "v": 1, "ts": ts(0), "kind": "stats", "session": "s1",
                "uptime_secs": 120, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "s1", "ledger_dropped_lines": 10},
            },
            {
                "v": 1, "ts": ts(300), "kind": "stats", "session": "s1",
                "uptime_secs": 180, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "s1", "ledger_dropped_lines": 12},
            },
        ]
        ledger_gap = ld.load_ledger([str(self._write_ledger(records_gap))])
        self.assertEqual(ledger_gap.provenance()["dropped_lines"], 22)
        restarts = [ev for ev in ld.extract_operational_events(ledger_gap) if ev.kind == "restart"]
        self.assertEqual(len(restarts), 1)
        self.assertEqual(restarts[0].detail["reason"], "uptime_gap")

        # Whole-second truncation (elapsed=60.9s, uptime_growth=60s) must NOT trigger a restart.
        records_normal = [
            {
                "v": 1, "ts": "2026-11-19T10:00:00.050Z", "kind": "stats", "session": "s1",
                "uptime_secs": 60, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "s1", "ledger_dropped_lines": 10},
            },
            {
                "v": 1, "ts": "2026-11-19T10:01:00.950Z", "kind": "stats", "session": "s1",
                "uptime_secs": 120, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "s1", "ledger_dropped_lines": 12},
            },
        ]
        ledger_normal = ld.load_ledger([str(self._write_ledger(records_normal))])
        self.assertEqual(ledger_normal.provenance()["dropped_lines"], 12)
        self.assertEqual(
            [ev for ev in ld.extract_operational_events(ledger_normal) if ev.kind == "restart"],
            [],
        )

    def test_ambiguous_startup_truncation_band_and_lease_conflict_leave_calls_unattributed(self):
        base = ld.parse_ts("2026-11-19T10:00:00Z")

        def ts(sec):
            return (base + ld.dt.timedelta(seconds=sec)).isoformat()

        # Winner boots at 300.0, lease-loser startup at 301.0; first HB at 600.9 with
        # integer uptime_secs=300 (inferred uptime_start=300.9, closer to 301.0 than 300.0).
        # The recall at 300.5 lies in [299.9, 300.9) and must stay unattributed ("unknown")
        # rather than being assigned to old contract A.
        records = [
            {
                "v": 1, "ts": ts(0), "kind": "stats", "session": "proj-a",
                "uptime_secs": 600, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-a",
                    "embedding_contract": {"kind": "bge_m3", "model": "old", "dim": 1024},
                },
            },
            {
                "v": 1, "ts": ts(299.0), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.51, "exact_name_match": False},
                "response_annotations": [],
            },
            {
                "v": 1, "ts": ts(300.0), "kind": "startup", "session": "proj-a",
                "transport": "http", "state": "acquiring",
            },
            {
                "v": 1, "ts": ts(300.5), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.77, "exact_name_match": False},
                "response_annotations": [],
            },
            {
                "v": 1, "ts": ts(301.0), "kind": "startup", "session": "proj-a",
                "transport": "http", "state": "acquiring",
            },
            {
                "v": 1, "ts": ts(310.0), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.92, "exact_name_match": False},
                "response_annotations": [],
            },
            {
                "v": 1, "ts": ts(600.9), "kind": "stats", "session": "proj-a",
                "uptime_secs": 300, "version": "0.13.0", "git_sha": "aaa",
                "stats": {
                    "session": "proj-a",
                    "embedding_contract": {"kind": "embeddinggemma2", "model": "new", "dim": 768},
                },
            },
        ]
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        streams = ld.extract_streams(ledger)
        self.assertEqual(
            [
                o.value
                for o in streams[
                    ld.StreamKey("proj-a", "bge_m3/old/1024", "hybrid", "top_vector_cosine", 0)
                ]
            ],
            [0.51],
        )
        self.assertEqual(
            [
                o.value
                for o in streams[
                    ld.StreamKey("proj-a", "unknown", "hybrid", "top_vector_cosine", 1)
                ]
            ],
            [0.77],
        )
        self.assertEqual(
            [
                o.value
                for o in streams[
                    ld.StreamKey(
                        "proj-a", "embeddinggemma2/new/768", "hybrid", "top_vector_cosine", 1
                    )
                ]
            ],
            [0.92],
        )

        # If a lease refusal also occurs in [299.9, 600.9], the call at 310.0 is also left
        # unattributed until contract B is confirmed at 600.9.
        with_lease = list(records) + [
            {
                "v": 1, "ts": ts(305.0), "kind": "lease", "session": "proj-a",
                "event": "refused", "side": "loser",
            },
            {
                "v": 1, "ts": ts(610.0), "kind": "call", "tool": "lambo_recall",
                "session": "proj-a", "outcome": "ok",
                "top_legs": {"bm25": 1.0, "vector_cosine": 0.95, "exact_name_match": False},
                "response_annotations": [],
            },
        ]
        streams_lease = ld.extract_streams(ld.load_ledger([str(self._write_ledger(with_lease))]))
        self.assertEqual(
            [
                o.value
                for o in streams_lease[
                    ld.StreamKey("proj-a", "unknown", "hybrid", "top_vector_cosine", 1)
                ]
            ],
            [0.77, 0.92],
        )
        self.assertEqual(
            [
                o.value
                for o in streams_lease[
                    ld.StreamKey(
                        "proj-a", "embeddinggemma2/new/768", "hybrid", "top_vector_cosine", 1
                    )
                ]
            ],
            [0.95],
        )

    def test_contract_rollback_a_b_a_resets_detector_with_fresh_epoch(self):
        rng = random.Random(2026)
        base = ld.parse_ts("2026-11-19T10:00:00Z")

        def ts(sec):
            return (base + ld.dt.timedelta(seconds=sec)).isoformat()

        c_a = {"kind": "bge_m3", "model": "a", "dim": 1024}
        c_b = {"kind": "embeddinggemma2", "model": "b", "dim": 768}

        records = [
            # Visit 1 of A (two consecutive identical heartbeats must share epoch 0).
            {
                "v": 1, "ts": ts(0), "kind": "stats", "session": "s1",
                "uptime_secs": 60, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "s1", "embedding_contract": c_a},
            },
            {
                "v": 1, "ts": ts(60), "kind": "stats", "session": "s1",
                "uptime_secs": 120, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "s1", "embedding_contract": c_a},
            },
        ]
        # 60 calls in Visit 1 of A around 0.80.
        for i in range(60):
            val = round(0.80 + rng.uniform(-0.02, 0.02), 5)
            records.append({
                "v": 1, "ts": ts(100 + i), "kind": "call", "tool": "lambo_recall",
                "session": "s1", "outcome": "ok",
                "top_legs": {"bm25": 2.0, "vector_cosine": val, "exact_name_match": False},
                "response_annotations": [],
            })
        # Switch to B (epoch 1) with two consecutive heartbeats.
        records.extend([
            {
                "v": 1, "ts": ts(300), "kind": "stats", "session": "s1",
                "uptime_secs": 5, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "s1", "embedding_contract": c_b},
            },
            {
                "v": 1, "ts": ts(360), "kind": "stats", "session": "s1",
                "uptime_secs": 65, "version": "0.13.0", "git_sha": "aaa",
                "stats": {"session": "s1", "embedding_contract": c_b},
            },
        ])
        for i in range(20):
            val = round(0.90 + rng.uniform(-0.02, 0.02), 5)
            records.append({
                "v": 1, "ts": ts(370 + i), "kind": "call", "tool": "lambo_recall",
                "session": "s1", "outcome": "ok",
                "top_legs": {"bm25": 2.0, "vector_cosine": val, "exact_name_match": False},
                "response_annotations": [],
            })
        # Roll back to A (epoch 2): steady around 0.55. If epoch 2 wrongly resumed
        # epoch 0's 0.80 ranks, 60 draws at 0.55 would immediately trigger an alarm.
        records.append({
            "v": 1, "ts": ts(500), "kind": "stats", "session": "s1",
            "uptime_secs": 5, "version": "0.13.0", "git_sha": "aaa",
            "stats": {"session": "s1", "embedding_contract": c_a},
        })
        for i in range(60):
            val = round(0.55 + rng.uniform(-0.02, 0.02), 5)
            records.append({
                "v": 1, "ts": ts(510 + i), "kind": "call", "tool": "lambo_recall",
                "session": "s1", "outcome": "ok",
                "top_legs": {"bm25": 2.0, "vector_cosine": val, "exact_name_match": False},
                "response_annotations": [],
            })

        ledger = ld.load_ledger([str(self._write_ledger(records))])
        report = ld.analyze_ledger(ledger, seed=0)
        vec_streams = [
            s for s in report["streams"] if s["stream"] == "top_vector_cosine"
        ]
        epochs = [(s["contract"], s["contract_epoch"], s["n"]) for s in vec_streams]
        self.assertEqual(
            epochs,
            [
                ("bge_m3/a/1024", 0, 60),
                ("embeddinggemma2/b/768", 1, 20),
                ("bge_m3/a/1024", 2, 60),
            ],
        )
        self.assertEqual(report["alarms"], [])
        rendered = ld.format_text(report)
        self.assertIn("bge_m3/a/1024#2", rendered)

    def test_ville_bound_preserves_reciprocal_in_json_and_scientific_notation_in_text(self):
        records = [
            {
                "v": 1, "ts": "2026-11-19T10:00:00Z", "kind": "call", "tool": "lambo_recall",
                "session": "s1", "outcome": "ok",
                "hits": [{"score": 0.8}],
                "top_legs": {"bm25": 2.0, "vector_cosine": 0.8, "exact_name_match": False},
                "response_annotations": [],
            },
        ]
        ledger = ld.load_ledger([str(self._write_ledger(records))])
        report = ld.analyze_ledger(ledger, threshold=10_000_000)
        self.assertAlmostEqual(report["config"]["ville_false_alarm_bound"], 1e-7, places=15)
        self.assertGreater(report["config"]["ville_false_alarm_bound"], 0.0)
        text = ld.format_text(report)
        self.assertIn("1.00e-07", text)
        self.assertNotIn("Ville bound <= 0 ", text)


if __name__ == "__main__":
    unittest.main()

