#!/usr/bin/env python3
"""C5M-R1-1a — OMP swarm re-run with the narrowed toolset and the skill in
the system prompt.

The round-1 finding C5M-R1-1 was that Qwen3-0.6B's "cannot drive OMP" was
only probed under OMP's default full toolset. This driver runs the swarm
under OMP with `--no-tools` (the narrowest built-in toolset OMP offers —
read/write/edit remain; MCP tools always load) and the lambo-cloudops skill
appended to the system prompt, so the model gets the protocol and chooses
every tool call itself.

Each swarm agent is one `omp --model qwen3-0.6b -p --no-pty --no-tools
--append-system-prompt=<skill>` process in the swarm workspace (whose
.mcp.json points at the scratch `lambo serve`). The prompt hands the agent a
swarm topic and instructs it to run the lambo-cloudops protocol for several
resources; the model decides when it is done. The driver runs the agents
concurrently, captures each process's stdout/stderr, and after the window
pulls the OMP session records (under PI_CODING_AGENT_DIR/sessions) into the
committed transcript directory so the run is traceable.

The exact tool context each agent received is captured separately (see the
C5M-R1-1 remediation notes): 15 tools — read, write, edit, the 7 lambo MCP
tools, and 5 inherited openaiDeveloperDocs MCP tools (a child omp session
inherits the parent session's MCP servers; OMP offers no flag to drop them
or the read/write/edit built-ins, so a lambo-only toolset is not achievable
under OMP).

Usage:
  python3 scripts/loadtest/omp_swarm.py \
      --cwd /tmp/c-series-scratch/omp-swarm/ws \
      --agent-dir /tmp/c-series-scratch/omp-swarm/agent \
      --skill skills/lambo-cloudops/SKILL.md \
      --out /tmp/c-series-scratch/omp-swarm/out --agents 3
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

TOPICS = [
    ("auth-middleware", "the auth middleware guards the user schema"),
    ("rate-limit", "the rate limit protects the public API"),
    ("billing-retries", "the billing service retries failed charges"),
    ("session-store", "the session store persists user state"),
    ("migration-script", "the migration script upgrades the schema"),
    ("cache-layer", "the cache layer speeds up recall"),
]

PROMPT_TEMPLATE = (
    "Swarm topic: {topic}.\n"
    "You are one agent in a swarm deriving operational knowledge for a cloud-ops "
    "environment. Follow the lambo-cloudops protocol in your system prompt: before "
    "any write, run the pre-flight recall protocol with lambo_recall for the resource "
    "you will modify; then derive the concept with lambo_derive; then record the action "
    "with lambo_record_action including its depends-on edges; then recall again to "
    "re-check. Work through the resources in this topic area (at least three derives), "
    "then move to the next resource. When you have covered the topic, reply DONE."
)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--cwd", required=True, help="workspace whose .mcp.json points at lambo")
    ap.add_argument("--agent-dir", required=True,
                    help="PI_CODING_AGENT_DIR: isolated profile (models.yml + session storage)")
    ap.add_argument("--skill", required=True, help="skill file appended to the system prompt")
    ap.add_argument("--out", required=True, help="directory for stdout/stderr transcripts")
    ap.add_argument("--agents", type=int, default=3)
    ap.add_argument("--timeout", type=float, default=240.0, help="per-agent timeout")
    args = ap.parse_args()

    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env["PI_CODING_AGENT_DIR"] = args.agent_dir
    env["LAMBO_SWARM_KEY"] = env.get("LAMBO_SWARM_KEY", "lambo-swarm-local")

    procs = []
    started = time.time()
    for i in range(args.agents):
        label, topic = TOPICS[i % len(TOPICS)]
        prompt = PROMPT_TEMPLATE.format(topic=topic)
        stdout = open(out / f"agent-{i}-{label}.stdout.txt", "w", encoding="utf-8")
        stderr = open(out / f"agent-{i}-{label}.stderr.txt", "w", encoding="utf-8")
        p = subprocess.Popen(
            ["omp", "--model", "qwen3-0.6b", "-p", "--no-pty", "--no-tools",
             "--append-system-prompt", args.skill, prompt],
            cwd=args.cwd, env=env, stdout=stdout, stderr=stderr,
        )
        procs.append((p, stdout, stderr, label, i))

    # Let the window elapse; if a process finished early, record that too.
    for p, so, se, label, i in procs:
        try:
            rc = p.wait(timeout=args.timeout)
            print(f"agent-{i} ({label}): exited rc={rc} after "
                  f"{time.time() - started:.1f}s", file=sys.stderr)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
            print(f"agent-{i} ({label}): killed at {args.timeout}s timeout", file=sys.stderr)
        so.close()
        se.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
