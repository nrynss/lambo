# Video shoot (T9.3 / D2) — raw takes and how they were made

Captured 2026-08-17. Raw takes are in `evidence/video-raw/`, which is
**gitignored**: they are hundreds of megabytes of source footage, not evidence.
The finished video is the deliverable; these are what it gets cut from.

## The takes

| File | Length | What is on screen |
|---|---|---|
| `01-demo.mkv` | 36s | `lambo demo` with no config and no database, ending on the load-bearing-pillar block, then scroll-back through its 242 lines |
| `02-stats.mkv` | 32s | `lambo stats` against the live `cloudops-exhibit` session in CockroachDB |
| `03-recall.mkv` | 37s | `lambo recall "what depends on SG-Base-VPC"`: traversal answers the dependency question, `RDS-Lambo-Demo-DB` top at score 9.50 |
| `04-inspect.mkv` | 49s | `lambo inspect` on `SG-Base-VPC`: blast radius 5, load-bearing warning, typed edges, with scroll-back |
| `05-guard.mkv` | 80s | `03_crossover_protect.py`: pillar warning, `ABORTED`, and what would have been stranded |
| `06-portal-zoomed.mkv` | 54s | The portal at 1.4x: structure tree, `SG-Base-VPC` focused so the gates panel fills, query typed live, results as cards |
| `07-agent-rule-driven.mkv` | 66s | **The best one.** Bare prompt, no mention of memory; the agent consults Lambo because of its standing rule |
| `08-agent-instructed-broll.mkv` | 239s | Earlier agent run where the prompt told it to check memory. B-roll; contains long Cursor reconnect spinners |

## Three things the narration must not overclaim

1. **Lambo does not intercept anything.** It has no hook into a shell and the
   released binary calls no AWS API. The guard script refuses because it asked
   Lambo first and got a blast radius back. The refusal lives in the client.
2. **The CloudOps "agents" in `evidence/cloudops-run/` are Python scripts**
   (`01_network_agent.py`, `02_app_data_agent.py`) playing agent roles. They did
   real provisioning and real derives, but they are not model-driven. Take 07 is
   the one with an actual model in the loop.
3. **`06-portal-zoomed.mkv` shows a build newer than production.** It is a local
   `serve-web` (current `main`) against the same live session; `lambo.nryn.dev`
   still serves `0.2.1`, which predates H1/H2/H3 and has no recall cards. Either
   ship `v0.2.2` and redeploy (D1), or do not imply viewers can see cards there.

Also: `05-guard.mkv` prints `arn:aws:iam::<account-id>:user/lambo-user` in its
first seconds. Crop or blur that line before publishing.

## What made take 07 work

A bare prompt ("SG-Base-VPC looks unused. I am planning to delete it in the
cleanup. What do you think?") plus two conditions:

- **A standing rule the agent reads.** See AGENTS.md, "Consulting memory before
  infrastructure work". A copy lives in the scratch ops workspace the take was
  shot in. Without it the model grepped the repo instead of calling Lambo: the
  MCP server's own `initialize` instructions (which do say "call lambo_recall
  before acting") were not enough on their own in a code repository.
- **A working directory with no code in it.** In the repo, grep is the obvious
  move. In an ops workspace, memory is the only record of prior work. On camera
  the grep runs and returns `Found 0 matches` while `lambo_recall` and
  `lambo_inspect` answer, which is the point made better than narration could.

Lambo was attached over MCP as a `lambo` server in `~/.cursor/mcp.json`, pointed
at a throwaway `video-demo` session so nothing could touch exhibit data. Tool
approvals were left in shot deliberately: the agent holds live AWS credentials,
and `/run-everything` would have removed the only gate.

## Capture rig (KDE Wayland, KWin, one 2560x1440 output)

Four rules, each learned by losing takes to it:

1. **Pick the WINDOW, not the screen**, in the KDE share dialog. Window capture
   follows that surface even when it is behind something else or unfocused.
   Screen capture records whatever is displayed, which silently films the wrong
   window. The OBS log tells you which you got: `2560x1394` is a window,
   `2560x1440` is the whole screen.
2. **Verify frames by looking at them.** A black recording still grows at about
   50 KB/s and passes every other check. A pure black 1080p JPEG at `-q:v 3` is
   ~12,460 bytes. Sample a timestamp where content should exist, and view it.
3. **Stop on the real completion signal, never a guessed duration.**
   `kitten @ get-text --extent screen` says when a command finished or a TUI went
   idle. Record until then with a generous ceiling and trim afterwards.
4. **Gate every take on `state: "streaming"` in the OBS log.** Without it, a
   stale portal token means OBS records black while reporting success.

Mechanics: `obs --startrecording --minimize-to-tray`, stopped with `SIGINT` then
`SIGTERM`, which finalizes the file; no websocket needed. Typing and scrolling
are driven over kitty's socket (`--listen-on unix:<sock>`,
`-o allow_remote_control=yes`) because this box has no `wtype`, `ydotool` or
`xdotool`. Browser shots use Playwright headed against
`/usr/bin/google-chrome-stable` with `--force-device-scale-factor=1.4`, and must
`scrollIntoView` the results region or the payoff never enters frame.

Do not try to fix framing by editing the OBS scene transform. If the frame is
wrong, the window is wrong. Editing the scene cost several takes and produced
nothing but black files.
