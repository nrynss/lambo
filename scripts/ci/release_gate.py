#!/usr/bin/env python3
"""Require full, successful main CI for the exact commit before release builds."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import tomllib
from urllib.parse import urlencode


CI_PATH = ".github/workflows/ci.yml"
PENDING = {"queued", "in_progress", "waiting", "pending", "requested"}


class GateError(RuntimeError):
    pass


class GitHub:
    def __init__(self, repository):
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
            raise GateError("Expected GITHUB_REPOSITORY in owner/repository form")
        self.repository = repository
        self.root = f"repos/{repository}/actions"

    def request(self, endpoint, *, paginate=False):
        command = ["gh", "api", "--method", "GET", "-H",
                   "X-GitHub-Api-Version: 2022-11-28"]
        if paginate:
            command.extend(["--paginate", "--slurp"])
        command.append(endpoint)
        try:
            result = subprocess.run(command, text=True, capture_output=True, timeout=60)
        except (OSError, subprocess.TimeoutExpired) as error:
            raise GateError("Could not read GitHub Actions API") from error
        if result.returncode:
            # Do not echo captured output: authentication is supplied through
            # GH_TOKEN, and release logs need only the failing API endpoint.
            raise GateError(f"GitHub Actions API request failed: {endpoint}")
        try:
            return json.loads(result.stdout)
        except ValueError as error:
            raise GateError("GitHub Actions API returned invalid JSON") from error

    def workflow(self):
        return self.request(f"{self.root}/workflows/ci.yml")

    def runs(self, sha):
        query = urlencode({"branch": "main", "head_sha": sha, "per_page": 100})
        pages = self.request(f"{self.root}/workflows/ci.yml/runs?{query}", paginate=True)
        return [run for page in pages for run in page["workflow_runs"]]

    def jobs(self, run):
        # A successful job from an earlier attempt must never bless a rerun.
        endpoint = (f"{self.root}/runs/{run['id']}/attempts/"
                    f"{run['run_attempt']}/jobs?per_page=100")
        pages = self.request(endpoint, paginate=True)
        return [job for page in pages for job in page["jobs"]]


def trusted_run(run, *, sha, repository, workflow_id):
    """Defend the boundary even though the API query already narrows results."""
    return (
        run.get("workflow_id") == workflow_id
        and run.get("path", "").split("@", 1)[0] == CI_PATH
        and run.get("head_sha") == sha
        and run.get("head_branch") == "main"
        and run.get("event") in {"push", "workflow_dispatch"}
        and (run.get("head_repository") or {}).get("full_name", "").lower()
        == repository.lower()
        and isinstance(run.get("id"), int) and run["id"] > 0
        and isinstance(run.get("run_attempt"), int) and run["run_attempt"] > 0
    )


def full_ci_passed(jobs, run, sha):
    aggregate = [job for job in jobs if job.get("name") == "full-ci"]
    return len(aggregate) == 1 and (
        aggregate[0].get("status") == "completed"
        and aggregate[0].get("conclusion") == "success"
        and aggregate[0].get("head_sha") == sha
        and aggregate[0].get("run_id") == run["id"]
    )


def wait_for_ci(api, sha, *, timeout=1800, poll_interval=15, missing_grace=60,
                monotonic=time.monotonic, sleep=time.sleep, log=print):
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise GateError("Release commit must be a full Git SHA")
    if timeout <= 0 or poll_interval <= 0 or missing_grace < 0:
        raise GateError("Gate timeout/poll interval must be positive; grace cannot be negative")
    workflow = api.workflow()
    if (workflow.get("path") != CI_PATH or not isinstance(workflow.get("id"), int)
            or workflow["id"] <= 0):
        raise GateError("ci.yml did not resolve to the expected workflow")
    started = monotonic()
    previous = None
    while True:
        runs = [run for run in api.runs(sha) if trusted_run(
            run, sha=sha, repository=api.repository, workflow_id=workflow["id"])]
        # A newer manual run or push run supersedes old evidence. For reruns,
        # run_attempt identifies the current attempt of this same run ID.
        run = max(runs, key=lambda candidate: candidate["id"]) if runs else None
        if run and run.get("status") == "completed" and run.get("conclusion") == "success":
            if full_ci_passed(api.jobs(run), run, sha):
                log(f"Full CI passed for {sha}: https://github.com/{api.repository}/actions/runs/{run['id']}")
                return run["id"]

        elapsed = monotonic() - started
        pending = run and run.get("status") in PENDING
        if runs and not pending:
            raise GateError(f"No successful full CI for {sha}; ci.yml/full-ci must both pass on main")
        if not runs and elapsed >= missing_grace:
            raise GateError(f"No main push/manual ci.yml run for {sha}. Run full CI on main for this commit before retrying the release")
        if elapsed >= timeout:
            raise GateError(f"Timed out waiting for full CI for {sha}")
        message = (f"Waiting for full main CI for {sha}" if runs else
                   f"Waiting for the main CI run for {sha} to appear")
        if message != previous:
            log(message)
            previous = message
        sleep(min(poll_interval, timeout - elapsed))


def release_context(event, ref, dry_run, manifest=Path("Cargo.toml")):
    if event == "workflow_dispatch":
        if dry_run != "true":
            raise GateError("A manual release run must be a dry run; publish by pushing a v<version> tag")
    elif event != "push" or not ref.startswith("refs/tags/v"):
        raise GateError("Release publishing requires a v<version> tag push")
    with manifest.open("rb") as file:
        version = tomllib.load(file)["package"]["version"]
    if not isinstance(version, str) or not re.fullmatch(
            r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
        raise GateError("Cargo.toml package.version is not a release version")
    if event == "push" and ref != f"refs/tags/v{version}":
        raise GateError(f"Tag {ref.removeprefix('refs/tags/')} does not match Cargo.toml v{version}")
    # Checkout peels annotated tags; HEAD^{commit} proves we use their commit,
    # never the tag object or a subsequently moving main branch head.
    sha = subprocess.check_output(["git", "-C", str(manifest.parent), "rev-parse",
                                   "HEAD^{commit}"], text=True).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise GateError("Could not resolve the checked-out release commit")
    return version, sha


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()
    try:
        event = os.environ.get("GITHUB_EVENT_NAME", "")
        version, sha = release_context(event, os.environ.get("GITHUB_REF", ""),
                                       os.environ.get("RELEASE_DRY_RUN", ""))
        if event == "push":
            wait_for_ci(GitHub(os.environ.get("GITHUB_REPOSITORY", "")), sha,
                        timeout=args.timeout)
        else:
            print("Diagnostic dry run: bypassing the green-CI gate; publishing remains disabled")
        with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
            output.write(f"version={version}\nsha={sha}\n")
        print(f"Release preflight passed: v{version} at {sha}")
    except (GateError, OSError, KeyError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Release preflight failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
