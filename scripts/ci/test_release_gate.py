"""Release trust-boundary and polling tests; no GitHub credentials or network."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import release_gate as gate


SHA = "a" * 40
REPO = "nrynss/lambo"


def run(**changes):
    value = {"id": 100, "workflow_id": 7, "path": gate.CI_PATH,
             "head_sha": SHA, "head_branch": "main", "event": "push",
             "head_repository": {"full_name": REPO}, "run_attempt": 1,
             "status": "completed", "conclusion": "success"}
    return value | changes


def job(**changes):
    return {"name": "full-ci", "run_id": 100, "head_sha": SHA,
            "status": "completed", "conclusion": "success"} | changes


class FakeAPI:
    repository = REPO

    def __init__(self, stages=None, jobs=None):
        self.stages = stages if stages is not None else [[run()]]
        self.returned_jobs = jobs if jobs is not None else [job()]
        self.calls = 0
        self.inspected = []

    def workflow(self):
        return {"path": gate.CI_PATH, "id": 7}

    def runs(self, sha):
        self.calls += 1
        return self.stages[min(self.calls - 1, len(self.stages) - 1)]

    def jobs(self, value):
        self.inspected.append((value["id"], value["run_attempt"]))
        return self.returned_jobs


class Clock:
    now = 0

    def monotonic(self):
        return self.now

    def sleep(self, duration):
        self.now += duration


class GateTests(unittest.TestCase):
    def wait(self, api, **options):
        clock = Clock()
        return gate.wait_for_ci(api, SHA, timeout=30, poll_interval=5,
                                monotonic=clock.monotonic, sleep=clock.sleep,
                                log=lambda _: None, **options)

    def test_successful_full_main_push(self):
        self.assertEqual(self.wait(FakeAPI()), 100)

    def test_successful_manual_main_ci_is_fallback(self):
        self.assertEqual(self.wait(FakeAPI([[run(event="workflow_dispatch")]])), 100)

    def test_only_trusted_run_identity_can_authorize_release(self):
        for change in ({"head_sha": "b" * 40}, {"event": "pull_request"},
                       {"event": "pull_request_target"}, {"event": "schedule"},
                       {"head_branch": "topic"}, {"workflow_id": 8},
                       {"path": ".github/workflows/docs.yml"},
                       {"head_repository": {"full_name": "attacker/lambo"}},
                       {"head_repository": None}):
            with self.subTest(change=change), self.assertRaisesRegex(gate.GateError, "No main"):
                self.wait(FakeAPI([[run(**change)]]), missing_grace=0)

    def test_workflow_and_aggregate_must_both_succeed(self):
        for conclusion in ("failure", "cancelled", "timed_out", "neutral", "skipped"):
            with self.subTest(conclusion=conclusion), self.assertRaisesRegex(gate.GateError, "both pass"):
                self.wait(FakeAPI([[run(conclusion=conclusion)]]))
        for conclusion in ("failure", "cancelled", "skipped", "neutral", None):
            with self.subTest(job=conclusion), self.assertRaisesRegex(gate.GateError, "both pass"):
                self.wait(FakeAPI(jobs=[job(conclusion=conclusion)]))

    def test_docs_only_workflow_success_does_not_count(self):
        for jobs in ([], [job(name="docs-mirror")], [job(conclusion="skipped")]):
            with self.subTest(jobs=jobs), self.assertRaisesRegex(gate.GateError, "both pass"):
                self.wait(FakeAPI(jobs=jobs))

    def test_aggregate_cannot_belong_to_another_commit_or_run(self):
        for changes in ({"head_sha": "b" * 40}, {"run_id": 99}, {"status": "in_progress"}):
            with self.subTest(changes=changes), self.assertRaises(gate.GateError):
                self.wait(FakeAPI(jobs=[job(**changes)]))

    def test_duplicate_aggregate_names_fail_closed(self):
        with self.assertRaises(gate.GateError):
            self.wait(FakeAPI(jobs=[job(), job()]))

    def test_running_ci_waits_then_passes(self):
        api = FakeAPI([[run(status="in_progress", conclusion=None)], [run()]])
        self.assertEqual(self.wait(api), 100)
        self.assertEqual(api.calls, 2)

    def test_queued_ci_times_out(self):
        api = FakeAPI([[run(status="queued", conclusion=None)]])
        with self.assertRaisesRegex(gate.GateError, "Timed out"):
            self.wait(api)
        self.assertEqual(api.calls, 7)

    def test_missing_run_has_bounded_propagation_grace(self):
        api = FakeAPI([[], [], [run()]])
        self.assertEqual(self.wait(api), 100)
        with self.assertRaisesRegex(gate.GateError, "No main"):
            self.wait(FakeAPI([[]]), missing_grace=10)

    def test_newer_failed_run_blocks_older_success(self):
        with self.assertRaisesRegex(gate.GateError, "both pass"):
            self.wait(FakeAPI([[run(), run(id=101, conclusion="failure")]]))

    def test_newer_pending_run_waits_instead_of_using_older_success(self):
        api = FakeAPI([[run(), run(id=101, status="in_progress", conclusion=None)],
                       [run(), run(id=101)]], jobs=[job(run_id=101)])
        self.assertEqual(self.wait(api), 101)
        self.assertEqual(api.calls, 2)

    def test_newer_docs_only_success_blocks_older_full_success(self):
        with self.assertRaises(gate.GateError):
            self.wait(FakeAPI([[run(), run(id=101)]], jobs=[job(conclusion="skipped", run_id=101)]))

    def test_current_rerun_attempt_is_inspected(self):
        api = FakeAPI([[run(run_attempt=2)]])
        self.wait(api)
        self.assertEqual(api.inspected, [(100, 2)])

    def test_unrelated_newer_run_is_ignored(self):
        self.assertEqual(self.wait(FakeAPI([[run(), run(id=101, event="pull_request")]])), 100)

    def test_wrong_workflow_metadata_fails_closed(self):
        api = FakeAPI()
        with patch.object(api, "workflow", return_value={"path": "docs.yml", "id": 7}):
            with self.assertRaisesRegex(gate.GateError, "expected workflow"):
                self.wait(api)


class APITransportTests(unittest.TestCase):
    @patch("release_gate.subprocess.run")
    def test_workflow_runs_paginate_and_filter_exact_sha_and_main(self, command):
        command.return_value = subprocess.CompletedProcess([], 0, json.dumps([
            {"workflow_runs": [run()]}, {"workflow_runs": [run(id=101)]}]), "")
        self.assertEqual(len(gate.GitHub(REPO).runs(SHA)), 2)
        args = command.call_args.args[0]
        self.assertIn("--paginate", args)
        self.assertIn("--slurp", args)
        self.assertEqual(args[args.index("--method") + 1], "GET")
        self.assertIn(f"workflows/ci.yml/runs?branch=main&head_sha={SHA}", args[-1])

    @patch("release_gate.subprocess.run")
    def test_jobs_use_current_attempt_not_all_attempts(self, command):
        command.return_value = subprocess.CompletedProcess([], 0, '[{"jobs": []}]', "")
        gate.GitHub(REPO).jobs(run(run_attempt=2))
        self.assertIn("/runs/100/attempts/2/jobs?", command.call_args.args[0][-1])

    @patch("release_gate.subprocess.run")
    def test_api_error_does_not_echo_sensitive_output(self, command):
        command.return_value = subprocess.CompletedProcess([], 1, "private-value", "private-value")
        with self.assertRaises(gate.GateError) as caught:
            gate.GitHub(REPO).workflow()
        self.assertNotIn("private-value", str(caught.exception))

    def test_repository_cannot_inject_endpoint_or_shell_options(self):
        for repository in ("", "../repos/attacker/repo", "owner/repo?branch=other", "owner/repo;echo bad"):
            with self.subTest(repository=repository), self.assertRaises(gate.GateError):
                gate.GitHub(repository)


class ReleaseContextTests(unittest.TestCase):
    def test_invalid_manual_dispatch_fails_before_build_or_manifest_read(self):
        with self.assertRaisesRegex(gate.GateError, "must be a dry run"):
            gate.release_context("workflow_dispatch", "refs/tags/v0.9.2", "false", Path("missing.toml"))

    def test_tag_version_mismatch_is_early_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / "Cargo.toml"
            manifest.write_text('[package]\nversion = "0.9.2"\n')
            with patch("release_gate.subprocess.check_output") as git:
                with self.assertRaisesRegex(gate.GateError, "does not match"):
                    gate.release_context("push", "refs/tags/v0.9.1", "", manifest)
                git.assert_not_called()

    def test_annotated_tag_resolves_to_commit_and_manual_dry_run_is_allowed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "Cargo.toml"
            manifest.write_text('[package]\nversion = "0.9.2"\n')
            def git(*args):
                return subprocess.check_output(["git", "-C", directory, "-c", "user.name=CI test",
                    "-c", "user.email=ci-test@example.invalid", *args], text=True, stderr=subprocess.DEVNULL).strip()
            git("init")
            git("add", "Cargo.toml")
            git("commit", "-m", "test release")
            commit = git("rev-parse", "HEAD")
            git("tag", "-a", "v0.9.2", "-m", "annotated tag")
            self.assertNotEqual(git("rev-parse", "v0.9.2"), commit)
            git("checkout", "v0.9.2")
            self.assertEqual(gate.release_context("push", "refs/tags/v0.9.2", "", manifest), ("0.9.2", commit))
            self.assertEqual(gate.release_context("workflow_dispatch", "refs/heads/topic", "true", manifest), ("0.9.2", commit))


if __name__ == "__main__":
    unittest.main()
