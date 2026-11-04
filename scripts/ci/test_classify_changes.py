"""Exercise trigger boundaries and real Git diffs, including unsafe-to-skip cases."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("classify_changes.py")
spec = importlib.util.spec_from_file_location("classify_changes", SCRIPT)
classifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(classifier)


class PathClassification(unittest.TestCase):
    def test_every_compile_and_test_input_runs_rust(self):
        # Keep this acceptance list independent of the implementation's lists:
        # changing the classifier must not silently remove a build input.
        inputs = [
            "src/lib.rs", "tests/regression.rs", "migrations/001.sql",
            "fixtures/golden.json", "examples/demo.rs", "web/app.js",
            "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "lambo.example.toml",
            "build.rs", ".cargo/config.toml",
            "evidence/issue-87-calibration/calibration.json",
            "evidence/issue-198-trigger/cosines-bge-m3-self.json",
            "evidence/issue-221/calibration-qwen3-4b-2560.json",
            "evidence/issue-221/calibration-qwen3-4b-1024.json",
            "evidence/issue-221/repro-8concept-qwen3-4b-2560.json",
            "evidence/issue-221/repro-8concept-qwen3-4b-1024.json",
        ]
        for path in inputs:
            with self.subTest(path=path):
                self.assertTrue(classifier.classify_paths([path])["rust"])

    def test_mirror_only_does_not_start_rust(self):
        paths = [f"{prefix}/{page}.mdx" for prefix in ["docs/reference", "site/src/content/docs"]
                 for page in ["cli", "mcp", "installation", "config"]]
        paths.append("scripts/docs/check-mirror-drift.sh")
        for path in paths:
            with self.subTest(path=path):
                self.assertEqual(classifier.classify_paths([path]), {"rust": False, "mirror": True})

    def test_unrelated_docs_and_evidence_stay_lightweight(self):
        self.assertEqual(classifier.classify_paths([
            "README.md", "dev-diary/handoff.md", "site/src/content/docs/index.mdx",
            "evidence/issue-221/README.md", "scripts/loadtest/mcp_load.py",
        ]), {"rust": False, "mirror": False})

    def test_workflow_and_ci_script_changes_validate_both(self):
        for path in [".github/workflows/release.yml", "scripts/ci/release_gate.py"]:
            with self.subTest(path=path):
                self.assertEqual(classifier.classify_paths([path]), {"rust": True, "mirror": True})


class EventClassification(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.repo = Path(self.directory.name)
        self.git("init", "-q")
        self.git("config", "user.email", "ci-test@example.invalid")
        self.git("config", "user.name", "CI classifier test")
        self.write("README.md", "initial")
        self.write("web/app.js", "initial")
        self.git("add", ".")
        self.git("commit", "-qm", "initial")
        self.base = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, text=True)

    def write(self, name, content):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-qm", "change")

    def classify(self, event_name="push", event=None):
        payload = {"before": self.base} if event is None else event
        return classifier.classify_event(event_name, payload, self.repo)

    def test_push_web_only_runs_rust(self):
        self.write("web/app.js", "changed")
        self.commit()
        self.assertEqual(self.classify(), {"rust": True, "mirror": False})

    def test_pr_compares_base_to_checked_out_head(self):
        self.write("docs/reference/cli.mdx", "changed")
        self.commit()
        event = {"pull_request": {"base": {"sha": self.base}}}
        self.assertEqual(self.classify("pull_request", event), {"rust": False, "mirror": True})

    def test_move_out_of_rust_tree_still_runs_rust(self):
        self.git("mv", "web/app.js", "notes.txt")
        self.commit()
        self.assertTrue(self.classify()["rust"])

    def test_deleted_rust_input_still_runs_rust(self):
        self.git("rm", "web/app.js")
        self.commit()
        self.assertTrue(self.classify()["rust"])

    def test_filename_with_newline_is_one_input(self):
        self.write("web/line\nbreak.js", "changed")
        self.commit()
        self.assertTrue(self.classify()["rust"])

    def test_no_changes_is_lightweight(self):
        self.assertEqual(self.classify(), {"rust": False, "mirror": False})

    def test_dispatch_forces_full_validation(self):
        self.assertEqual(self.classify("workflow_dispatch", {}), {"rust": True, "mirror": True})

    def test_missing_new_branch_or_unavailable_base_runs_all(self):
        for base in [None, "0" * 40, "f" * 40, "--help"]:
            with self.subTest(base=base):
                self.assertEqual(self.classify(event={"before": base}), {"rust": True, "mirror": True})

    def test_missing_event_fields_and_unknown_events_run_all(self):
        for event_name in ["push", "pull_request", "unknown"]:
            with self.subTest(event_name=event_name):
                self.assertEqual(self.classify(event_name, {}), {"rust": True, "mirror": True})

    def test_cli_writes_lowercase_github_outputs(self):
        event_path = self.repo / "event.json"
        event_path.write_text(json.dumps({}))
        output_path = self.repo / "outputs"
        subprocess.run([
            sys.executable, str(SCRIPT), "--event-name", "workflow_dispatch",
            "--event-path", str(event_path), "--output", str(output_path),
        ], check=True, cwd=self.repo, stdout=subprocess.PIPE)
        self.assertEqual(output_path.read_text(), "rust=true\nmirror=true\n")

    def test_malformed_event_falls_back_to_full_validation(self):
        event_path = self.repo / "event.json"
        event_path.write_text("not json")
        result = subprocess.run([
            sys.executable, str(SCRIPT), "--event-name", "push", "--event-path", str(event_path),
        ], check=True, cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(json.loads(result.stdout), {"rust": True, "mirror": True})


if __name__ == "__main__":
    unittest.main()
