#!/usr/bin/env python3
"""Synthetic-fixture tests for check_durability.py (stdlib only, no server).

Builds a tiny SQLite store and a hand-written ledger in a temp dir, runs the
checker as a subprocess and asserts its exit status, so the accounting paths a
real run rarely reaches (refused-at-ack, empty ledger, GC-explained concept
shortfall, drifted wording) are exercised without a `lambo serve`. Nothing here
touches a live store, a port or /tmp/lambo-*.

    python3 scripts/loadtest/test_check_durability.py
"""

from __future__ import annotations

import json
import pathlib
import sqlite3
import subprocess
import sys
import tempfile
import unittest

CHECK = pathlib.Path(__file__).with_name("check_durability.py")
SESSION = "synthetic"

OK, SHORT, DRIFT, EMPTY, UNVERIFIABLE, BAD_INPUT, USAGE = 0, 2, 3, 4, 5, 6, 64


def rid(n: int) -> str:
    return f"lwr1.{1:016x}.{n:x}.{n:x}"


def call(tool: str, text: str, ok: bool = True) -> dict:
    return {"kind": "call", "tool": tool, "ok": ok, "is_error": False, "text": text}


def derive_ack(n: int, concepts: int = 1) -> dict:
    return call(
        "lambo_derive",
        f"accepted {concepts} concept(s) for background write; settles later\n"
        f"receipt {rid(n)}: pending",
    )


def action_ack(n: int) -> dict:
    return call(
        "lambo_record_action",
        f"accepted action 'build' for background write; settles later\nreceipt {rid(n)}: pending",
    )


def derive_refused(concepts: int = 3) -> dict:
    return call("lambo_derive", f"{concepts} concept(s) were NOT written: write queue full")


def action_refused() -> dict:
    return call("lambo_record_action", "action 'build' was NOT recorded: write queue full")


def settled_derive(n: int, created: int) -> dict:
    return call(
        "lambo_stats",
        "write receipts (your earlier writes, now settled):\n"
        f"- {rid(n)}: applied — derived {created} concept(s): {created} created, 0 matched existing",
    )


class CheckDurability(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = pathlib.Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def store(self, interactions: int, concepts: int = 0, edges: int = 0) -> pathlib.Path:
        path = self.dir / "store.db"
        con = sqlite3.connect(path)
        for table in ("interactions", "concepts", "edges", "canonization_events"):
            con.execute(f"CREATE TABLE {table} (session_id TEXT)")
        con.execute(
            "CREATE TABLE session_leases (session_id TEXT, holder TEXT, expires_at TEXT, current_token TEXT)"
        )
        con.execute("CREATE TABLE sessions (session_id TEXT, created_at TEXT, closed_at TEXT)")
        con.executemany("INSERT INTO interactions VALUES (?)", [(SESSION,)] * interactions)
        con.executemany("INSERT INTO concepts VALUES (?)", [(SESSION,)] * concepts)
        con.executemany("INSERT INTO edges VALUES (?)", [(SESSION,)] * edges)
        con.commit()
        con.close()
        return path

    def ledger(self, records: list[dict]) -> pathlib.Path:
        path = self.dir / "ledger.jsonl"
        path.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")
        return path

    def run_check(self, ledger, db, *extra: str) -> tuple[int, str]:
        proc = subprocess.run(
            [sys.executable, str(CHECK), "--ledger", str(ledger), "--db", str(db),
             "--session", SESSION, *extra],
            capture_output=True,
            text=True,
            timeout=60,
        )
        return proc.returncode, proc.stdout + proc.stderr

    # --- the accounting paths a real run rarely reaches ---

    def test_refused_at_ack_counts_as_an_interaction(self):
        # Two admitted writes and two refused at the queue: four interactions,
        # because the interaction is opened before admission.
        led = self.ledger([derive_ack(1), action_ack(2), derive_refused(), action_refused()])
        rc, out = self.run_check(led, self.store(interactions=4))
        self.assertEqual(rc, OK, out)
        self.assertIn("refused at ack (queue full)   : 2", out)
        self.assertIn("expected interactions         : 4", out)

    def test_refused_at_ack_missing_from_store_is_a_shortfall(self):
        led = self.ledger([derive_ack(1), action_ack(2), derive_refused(), action_refused()])
        rc, out = self.run_check(led, self.store(interactions=2))
        self.assertEqual(rc, SHORT, out)
        self.assertIn("SHORTFALL 2", out)

    def test_interaction_shortfall(self):
        rc, out = self.run_check(self.ledger([derive_ack(1), derive_ack(2)]), self.store(1))
        self.assertEqual(rc, SHORT, out)

    # --- nothing to verify must not pass ---

    def test_empty_ledger_is_not_a_pass(self):
        rc, out = self.run_check(self.ledger([]), self.store(0))
        self.assertEqual(rc, EMPTY, out)
        self.assertIn("NOTHING VERIFIED", out)

    def test_ledger_with_only_failed_calls_is_not_a_pass(self):
        led = self.ledger([call("lambo_derive", "boom", ok=False)])
        rc, out = self.run_check(led, self.store(0))
        self.assertEqual(rc, EMPTY, out)

    def test_allow_empty_makes_an_empty_run_pass(self):
        rc, out = self.run_check(self.ledger([]), self.store(0), "--allow-empty")
        self.assertEqual(rc, OK, out)
        self.assertIn("--allow-empty", out)

    # --- concepts: shortfall, GC, unverifiable ---

    def test_concept_shortfall_without_gc_info_is_unverifiable_not_a_pass(self):
        led = self.ledger([derive_ack(1, 5), settled_derive(1, 5)])
        rc, out = self.run_check(led, self.store(interactions=1, concepts=2))
        self.assertEqual(rc, UNVERIFIABLE, out)
        self.assertIn("UNVERIFIABLE", out)

    def test_concept_shortfall_with_gc_logged_and_no_sweeps_is_a_shortfall(self):
        led = self.ledger([derive_ack(1, 5), settled_derive(1, 5)])
        err = self.dir / "stderr.log"
        err.write_text("lambo serve: listening\n", encoding="utf-8")
        rc, out = self.run_check(
            led, self.store(interactions=1, concepts=2), "--stderr", str(err), "--gc-logged"
        )
        self.assertEqual(rc, SHORT, out)

    def test_concept_shortfall_explained_by_logged_gc_passes(self):
        led = self.ledger([derive_ack(1, 5), settled_derive(1, 5)])
        err = self.dir / "stderr.log"
        err.write_text("gc sweep concepts_collected=3 edges_removed=0\n", encoding="utf-8")
        rc, out = self.run_check(
            led, self.store(interactions=1, concepts=2), "--stderr", str(err), "--gc-logged"
        )
        self.assertEqual(rc, OK, out)
        self.assertIn("EXPLAINED", out)

    def test_store_ahead_with_unsettled_receipts_states_the_lower_bound(self):
        led = self.ledger([derive_ack(1, 2), derive_ack(2, 2), settled_derive(1, 2)])
        rc, out = self.run_check(led, self.store(interactions=2, concepts=4))
        self.assertEqual(rc, OK, out)
        self.assertIn("compared by count", out)

    # --- bad input, and the store is never created ---

    def test_missing_db_is_an_error_and_is_not_created(self):
        missing = self.dir / "nope.db"
        rc, out = self.run_check(self.ledger([derive_ack(1)]), missing)
        self.assertEqual(rc, BAD_INPUT, out)
        self.assertFalse(missing.exists(), "the checker must never create a store")

    def test_non_store_db_is_an_error(self):
        junk = self.dir / "junk.db"
        junk.write_bytes(b"not sqlite")
        rc, out = self.run_check(self.ledger([derive_ack(1)]), junk)
        self.assertEqual(rc, BAD_INPUT, out)

    def test_missing_ledger_is_an_error(self):
        rc, out = self.run_check(self.dir / "nope.jsonl", self.store(0))
        self.assertEqual(rc, BAD_INPUT, out)

    def test_store_is_opened_read_only(self):
        db = self.store(interactions=1)
        db.chmod(0o444)
        try:
            rc, out = self.run_check(self.ledger([derive_ack(1)]), db)
        finally:
            db.chmod(0o644)
        self.assertEqual(rc, OK, out)

    def test_drifted_wording_is_parser_drift(self):
        rc, out = self.run_check(
            self.ledger([call("lambo_derive", "wholly new wording")]), self.store(1)
        )
        self.assertEqual(rc, DRIFT, out)

    def test_usage_error_does_not_collide_with_shortfall(self):
        rc, out = self.run_check(self.ledger([]), self.store(0), "--gc-logged")
        self.assertEqual(rc, USAGE, out)


if __name__ == "__main__":
    unittest.main(verbosity=2)
