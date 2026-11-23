# Dogfood rig runbook

This runbook sets up, upgrades and checks a dogfood rig: one long-lived `lambo serve` writer that the agents developing Lambo use as their shared memory. Two rigs run it today: a Metal rig (macOS on Apple silicon, supervised by launchd, installing the `macos-arm64-metal` release asset) and a CUDA rig (Linux with an NVIDIA GPU, supervised by systemd, building from the release tag). The rigs do not share a store: each has its own graph and its own ledger, and figures from one are never combined with figures from the other.

Run every step as written. Where a step names a placeholder in angle brackets, replace it with the rig's own value before running it, and do not guess it. The placeholders are:

| Placeholder | Meaning |
|---|---|
| `<rig dir>` | The rig's directory, an absolute path outside any Lambo checkout. It holds `bin/`, `lambo.toml`, the ledger `calls.jsonl`, `serve.log` (macOS) and the backups. |
| `<store path>` | The `path` under `[store]` in `<rig dir>/lambo.toml`. Read it from the file; after a multi-session cut-over it is not the original store file. |
| `<version>` | The release being installed, without a leading `v`, for example `0.15.0`. |
| `<old version>` | The version the writer serves now, from the binary name in the unit (`lambo-<old version>`). |
| `<lambo checkout>` | A clone of `github.com/nrynss/lambo`. |

## Rules that hold for every step

- **The writer runs a pinned binary, never a build tree.** The binary lives at `<rig dir>/bin/lambo-<version>`, copied out of any `target/` directory, so a rebuild or `cargo clean` cannot touch the serving binary. Older binaries stay in `bin/` for rollback. The unit names the binary by its full path; do not run the writer through a wrapper that picks the newest file in `bin/`, because a crash-restart between staging a binary and provisioning the store then boots the new binary against an unprovisioned store (`evidence/0.12.0-vs-0.8.0/README.md`, "Hazards the gate surfaced").
- **Never install the rig's binary with the default `LAMBO_INSTALL_DIR`.** The install script's default is `~/.local/bin`, and any other process on the machine that runs `lambo` from `PATH` would pick up the new version at its next restart, against a store nobody provisioned for it. Stage it and move it into `<rig dir>/bin/` as below.
- **The store, the ledger and the backups live outside the repository.** The ledger carries recall queries and concept text, so it has the store's hygiene rules (`docs/reference/cli.mdx`, "stdio versus HTTP for several clients"). Nothing from a rig reaches `evidence/` except through a curated export of aggregates.
- **One supervised HTTP writer per rig, and no client starts a `serve`.** The supervisor (launchd or systemd) owns the writer's lifetime, so no client's exit takes memory away from the others. Every client is registered as a URL. A client registered with `command` instead of `url` starts its own stdio `serve`, which either proxies to the writer or, if it starts while the writer is restarting, takes the lease itself and couples the session to that client's lifetime.
- **The writer binds loopback only.** On `127.0.0.1` with no `[[serve.credential]]` the serve needs no token. Binding anywhere else makes `lambo serve` refuse to start without a credential (`docs/reference/config.mdx`, "Authentication"); that is a separate change, not part of this runbook.
- **The writer is stopped for any offline verb.** `lambo provision` refuses while any session is held (`docs/reference/cli.mdx`, "Provision a store"), and the session tools (`classify`, `split`, `verify`) read only a completed backup copy.

## Configuration

`<rig dir>/lambo.toml` selects the store, the embedder and the sessions the writer pins. Both rigs use SQLite and the in-process candle BGE-M3 embedder:

```toml
[store]
kind = "sqlite"
path = "<store path>"

[embedder]
kind = "candle"
dim = 1024
device = "metal"        # "cuda" on the CUDA rig
```

`device` is pinned so the configuration says which accelerator the rig expects. With `device = "metal"` or `"cuda"` the embedder refuses to load when the device or the compiled backend is missing; `auto` also refuses rather than falling back to the CPU, and only `device = "cpu"` runs on the CPU (`src/embed/candle.rs`, `resolve_device`). The weights are the f16 safetensors cast `nrynss/bge-m3-f16-safetensors`, downloaded once into the Hugging Face cache and checked by sha256 when loaded (`src/embed/candle.rs`, `WEIGHT_REPO`). On a Metal device the writer embeds a short probe every 10 seconds to keep the weights resident; `keep_warm_secs` changes or disables it (`docs/reference/config.mdx`, "Embedder").

The embedder is part of the session's embedding contract (kind, model, dim). A writer refuses a session stamped with another contract (`docs/reference/mcp.mdx`, `lambo_stats`), so switching a rig from `kind = "candle"` to `kind = "bge_m3"` is a migration that needs `lambo re-embed`, not a configuration swap.

A rig that serves several sessions adds a `[serve]` table; see [Multi-session cut-over runbook](#multi-session-cut-over-runbook) for the shape the Metal rig runs. Unknown keys are refused at startup, so a typo stops the writer instead of being ignored (`docs/reference/config.mdx`, "What lambo.toml contains").

## The supervised writer

The writer is started with these arguments. `--config` is a global flag and may come before or after `serve` (`src/main.rs`, `global = true`); the reference docs put it first.

```text
<rig dir>/bin/lambo-<version> --config <rig dir>/lambo.toml serve \
  --agent http-shared-writer --transport http --port 7700 \
  --ledger <rig dir>/calls.jsonl --ledger-heartbeat 300
```

`--agent` names only the lease holder. Each write is attributed to the `agent_id` its own call carries, so one shared writer still records which agent made each write. `--ledger` and `--ledger-heartbeat` are off by default and belong on the unit, so that every client's calls are measured; without the heartbeat the ledger has no dropped-line counts and the reports in `scripts/observability/` print `dropped: UNKNOWN` (`scripts/observability/README.md`). The sessions come from `[serve] sessions`, or from repeated `--session` flags; a first `--session` overrides `[serve] default_session` for `/mcp` (`docs/reference/cli.mdx`, "Several sessions in one serve"), so a rig that relies on `default_session` passes no `--session`.

Two timing facts shape both units. The single-writer lease lasts 45 seconds (`src/store/lease.rs`, `LEASE_TTL`), and an abrupt death does not release it, so a restart attempted inside that window is refused; the restart delay must exceed 45 seconds. A healthy shutdown takes at most 18.5 seconds, and at 20 seconds the server's watchdog logs the stage it is stuck in and aborts (`src/mcp/serve/watchdog.rs`), so the supervisor must wait longer than 20 seconds before it kills the process; 30 seconds is the documented recommendation (`docs/reference/cli.mdx`, "Stopping a writer").

### macOS: the launchd agent

`~/Library/LaunchAgents/dev.lambo.dogfood.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.lambo.dogfood</string>
  <key>ProgramArguments</key>
  <array>
    <string>&lt;rig dir&gt;/bin/lambo-&lt;version&gt;</string>
    <string>--config</string><string>&lt;rig dir&gt;/lambo.toml</string>
    <string>serve</string>
    <string>--agent</string><string>http-shared-writer</string>
    <string>--transport</string><string>http</string>
    <string>--port</string><string>7700</string>
    <string>--ledger</string><string>&lt;rig dir&gt;/calls.jsonl</string>
    <string>--ledger-heartbeat</string><string>300</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>60</integer>
  <key>ExitTimeOut</key><integer>30</integer>
  <key>ProcessType</key><string>Standard</string>
  <key>StandardOutPath</key><string>&lt;rig dir&gt;/serve.log</string>
  <key>StandardErrorPath</key><string>&lt;rig dir&gt;/serve.log</string>
  <key>WorkingDirectory</key><string>&lt;rig dir&gt;</string>
</dict>
</plist>
```

Replace every `&lt;rig dir&gt;` and `&lt;version&gt;` with real values (the binary path must be the first `ProgramArguments` entry, because the upgrade steps edit entry 0). Then:

```sh
plutil -lint ~/Library/LaunchAgents/dev.lambo.dogfood.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.lambo.dogfood.plist
```

`plutil -lint` first, because launchd reports a malformed plist only as a generic load failure. The keys that matter:

- `ThrottleInterval` 60: launchd's default of 10 seconds falls inside the 45-second lease TTL. launchd has no permanently failed state, so with `KeepAlive` it keeps retrying.
- `ExitTimeOut` 30: launchd's default is 20 seconds, the same instant the watchdog aborts, so set it explicitly.
- `ProcessType` `Standard`: without a `ProcessType`, launchd applies light resource limits (CPU and I/O throttling), which can eat the margin between the 18.5-second worst-case shutdown and the 20-second watchdog. Do not use `Adaptive`: it classifies by XPC activity, which `lambo serve` has none of.

launchd keeps the job definition it loaded, so any edit to the plist takes effect only after `launchctl bootout` and `launchctl bootstrap`.

### Linux: the systemd user unit

`~/.config/systemd/user/lambo-dogfood.service`:

```ini
[Unit]
Description=lambo dogfood writer
StartLimitIntervalSec=0

[Service]
ExecStart=<rig dir>/bin/lambo-<version> --config <rig dir>/lambo.toml serve --agent http-shared-writer --transport http --port 7700 --ledger <rig dir>/calls.jsonl --ledger-heartbeat 300
WorkingDirectory=<rig dir>
Restart=always
RestartSec=60
TimeoutStopSec=30

[Install]
WantedBy=default.target
```

systemd does not expand `~` in `ExecStart`, so every path is absolute. Add the `Environment=` lines the machine's CUDA installation needs at runtime. Then `systemctl --user daemon-reload`, `systemctl --user enable --now lambo-dogfood.service`.

- `RestartSec` 60: above the 45-second lease TTL.
- `StartLimitIntervalSec=0` in `[Unit]`, not `[Service]`. systemd reads the start-limit keys only in `[Unit]`; under `[Service]` it logs `Unknown key 'StartLimitIntervalSec' in section [Service], ignoring.` and keeps the default limiter (5 starts in 10 seconds), which leaves the unit permanently failed after a crash loop. Check the runtime value, not the file: `systemctl --user show lambo-dogfood.service -p StartLimitIntervalUSec` must print `StartLimitIntervalUSec=0`. A `daemon-reload` applies this key without restarting the writer.
- `TimeoutStopSec` 30: above the 20-second watchdog (`src/mcp/serve/shutdown.rs`).

If the rig also runs `lambo serve-web` under its own unit, that unit names the binary too, and every upgrade edits both.

### Checking the writer

```sh
# macOS
launchctl print gui/$(id -u)/dev.lambo.dogfood | grep -E 'state =|pid ='
# Linux
systemctl --user status lambo-dogfood.service

# either: lease holder per session, read-only, no embedder (lambo 0.14.0 or later)
<rig dir>/bin/lambo-<version> --config <rig dir>/lambo.toml stats --store | grep -E "^session|lease:"
```

Each pinned session's `lease:` line must name a holder that starts `http-shared-writer@<host>#<pid>`, with the writer's pid (`src/store/lease.rs`, `LeaseHolder::token`). Where the platform reports them, `;boot=…;pidns=…` follows, so match the prefix, not the whole string. `no writer` means the session is not held. A holder with another agent name means a client is still starting its own stdio `serve`; move that client to its URL (see [Client wiring](#client-wiring)). With a binary older than 0.14.0, read the table directly and filter out expired rows, which persist until the next acquire overwrites them (`migrations/sqlite/001_init.sql`):

```sh
sqlite3 "<store path>" "select session_id, holder, expires_at from session_leases where expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') order by session_id;"
```

## Upgrading the pinned binary

An upgrade (a re-pin) stops the writer for the whole window, so provisioning never races a live serve. Do it in this order and do not skip the verification: the release workflow builds the Metal asset and checks that it links only system libraries, and CI only compiles the Metal backend (`cargo check`), so nothing exercises a release's Metal runtime before the rig does (`.github/workflows/release.yml`, `.github/workflows/ci.yml` job `candle-metal`). `embed-candle-cuda` is not compiled in CI at all.

### Before you start

1. Read the `CHANGELOG.md` entries for every version after `<old version>` up to `<version>`. Note any verb an entry says to run after provisioning (a re-key, `lambo re-embed`, `lambo names migrate`), and any entry that says an older binary cannot read the upgraded store: that decides whether a rollback needs the backup restored.
2. If the owner requires a re-pin gate, run it before this procedure, against copies of the store and never against the live writer, following the method of the most recent gate in `evidence/` (for example `evidence/0.15.0-vs-0.14.0-metal/README.md` or `evidence/0.15.0-vs-0.12.0-cuda/README.md`). See [Load tests and gates on a rig machine](#load-tests-and-gates-on-a-rig-machine).
3. Set the variables used below in one shell, with real values:

```sh
V=<version>
OLD=<old version>
RIG=<rig dir>
STORE=<store path>
```

### Metal rig: install the release asset

Every release from 0.3.0 on carries `lambo-<version>-macos-arm64-metal`, built with `--features ship,embed-candle-metal` and with `LAMBO_GIT_SHA` set to the release commit (`.github/workflows/release.yml`). The install script picks it with `LAMBO_FLAVOR=metal` and verifies its SHA-256 against the published checksum (`scripts/install.sh`).

```sh
TMP=$(mktemp -d)
curl -fsSL -o "$TMP/install.sh" "https://github.com/nrynss/lambo/releases/download/v$V/install.sh"
LAMBO_VERSION=$V LAMBO_FLAVOR=metal LAMBO_INSTALL_DIR="$TMP/stage" LAMBO_DRY_RUN=1 sh "$TMP/install.sh"
```

The dry run must print a URL ending in `lambo-$V-macos-arm64-metal`. Then install for real and move the binary into the rig:

```sh
LAMBO_VERSION=$V LAMBO_FLAVOR=metal LAMBO_INSTALL_DIR="$TMP/stage" sh "$TMP/install.sh"
mv "$TMP/stage/lambo" "$RIG/bin/lambo-$V"
"$RIG/bin/lambo-$V" --version            # must print: lambo <version>
"$RIG/bin/lambo-$V" --config "$RIG/lambo.toml" embed --query "metal check" | head -c 300; echo
```

The `embed` line loads candle on the Metal device with the new binary and prints a vector with its contract; it opens no store and takes no lease (`docs/reference/cli.mdx`, "lambo embed"). An error here is the release's Metal runtime, found before the writer is stopped.

### CUDA rig: build from the release tag

No CUDA asset is published. Build the tag in a separate, clean worktree, on the CUDA rig itself: a binary built on one rig is never copied to the other.

```sh
cd <lambo checkout>
git fetch --tags origin
git worktree add --detach "../lambo-build-$V" "v$V"
cd "../lambo-build-$V"
git status --porcelain                   # must print nothing
LAMBO_GIT_SHA=$(git rev-parse --short=7 HEAD) \
  cargo build --locked --release --features store-sqlite,embed-candle-cuda,embed-bge
cp target/release/lambo "$RIG/bin/lambo-$V"
"$RIG/bin/lambo-$V" --version            # must print: lambo <version>
```

`LAMBO_GIT_SHA` is read at compile time (`src/ledger.rs`, `GIT_SHA`) and is the only thing that lets a ledger heartbeat say which binary produced a stretch of ledger; without it the heartbeat says `unknown`. Build from a clean tree, or the sha names a commit that does not hold the code. `--short=7` matches the release workflow's sha.

The candle CUDA backend's `cudarc` (0.19.10) knows CUDA toolkits up to 13.4. If the build stops with `Unsupported cuda toolkit version`, add `CUDARC_CUDA_VERSION=13040` to the build's environment; if nvcc rejects the system GCC (`Failed to preprocess host compiler properties`), add `NVCC_CCBIN=clang` (`docs/reference/installation.mdx`, `embed-candle-cuda`). The CUDA rig, on CUDA 13.5, needs both. Remove the worktree after the binary is copied: `cd <lambo checkout> && git worktree remove "../lambo-build-$V"`.

### Swap the binary in (macOS)

```sh
LABEL=dev.lambo.dogfood
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

# 1. keep the current definition for rollback
cp "$PLIST" "$RIG/$LABEL.plist.bak-$OLD"

# 2. stop the writer and wait for its process to exit. bootout returns at once,
#    while the process is still shutting down (up to 18.5 s when healthy)
PID=$(launchctl print gui/$(id -u)/$LABEL | awk '$1 == "pid" {print $3; exit}')
launchctl bootout gui/$(id -u)/$LABEL
while [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; do sleep 0.5; done
```

3. Check that the writer released every lease: `"$RIG/bin/lambo-$OLD" --config "$RIG/lambo.toml" stats --store | grep "lease:"` must show `no writer` on every line. A graceful stop releases them at once; after a crash or a stalled stop a lease stays until its 45-second TTL runs out, so wait 45 seconds and check again. Do not go on while any session is held.

4. Check for writes that were accepted but not applied:

```sh
sqlite3 "$STORE" "select session_id, count(*) from write_intents where consumed_at is null group by session_id;"
```

It must print nothing. If it prints rows, do not change versions with intents owed (`docs/reference/migration.mdx`, "Recovery and compatibility"): start the old writer again with `launchctl bootstrap gui/$(id -u) "$PLIST"`, let it replay them, then repeat steps 2 to 4.

5. Back up the store. Provisioning can add columns and re-key concepts, and an older binary may then refuse the store, so this copy is what makes a rollback possible. Use SQLite's backup, never a file copy of a `.db` that may have a WAL beside it:

```sh
BK="$RIG/backup-pre-$V-$(date +%Y%m%d-%H%M%S).db"
ls "$BK" 2>/dev/null && echo "STOP: $BK exists"
sqlite3 "$STORE" ".backup '$BK'"
sqlite3 "file:$BK?immutable=1" "pragma integrity_check;"     # must print: ok
shasum -a 256 "$BK" > "$BK.sha256"
```

Open the backup with `?immutable=1` when checking it; `mode=ro` can fail with error 14 on a fresh backup.

6. Provision the store with the new binary:

```sh
"$RIG/bin/lambo-$V" --config "$RIG/lambo.toml" provision
```

It is idempotent. It prints `sqlite schema provisioned (init_schema, idempotent)` and any re-key it did or skipped (`docs/reference/cli.mdx`, "Provision a store"). A refusal that names a held session means a writer still holds it: go back to step 3. Run any post-provision verb that step 1 of "Before you start" found, now, while the writer is stopped.

7. Point the agent at the new binary and start it:

```sh
plutil -replace ProgramArguments.0 -string "$RIG/bin/lambo-$V" "$PLIST"
plutil -lint "$PLIST"
plutil -extract ProgramArguments.0 raw "$PLIST"   # must print $RIG/bin/lambo-$V
launchctl bootstrap gui/$(id -u) "$PLIST"
```

Then run [Verify the upgrade](#verify-the-upgrade).

### Swap the binary in (Linux)

```sh
UNIT=lambo-dogfood.service
UNITFILE="$HOME/.config/systemd/user/$UNIT"
cp "$UNITFILE" "$UNITFILE.bak-$OLD"
systemctl --user stop "$UNIT"            # returns once the process has exited
```

Then run steps 3 to 6 of the macOS procedure unchanged (replace `shasum -a 256` with `sha256sum`, and the restart in step 4 with `systemctl --user start "$UNIT"`). Then:

```sh
sed -i "s#/bin/lambo-$OLD #/bin/lambo-$V #" "$UNITFILE"
systemctl --user daemon-reload
systemctl --user show "$UNIT" -p ExecStart | grep -c "/bin/lambo-$V "   # must print 1
systemctl --user start "$UNIT"
```

Edit a `serve-web` unit the same way if the rig has one, or it stays on the old binary.

### Verify the upgrade

The supervisor returns before the writer has loaded the embedder, taken its leases and bound the port, so wait for the port first (each probe capped at 2 seconds, 60 seconds in all):

```sh
deadline=$((SECONDS + 60)); code=000
while [ "$SECONDS" -lt "$deadline" ]; do
  code=$(curl -s -m 2 -o /dev/null -w '%{http_code}' http://127.0.0.1:7700/mcp)
  [ "$code" != "000" ] && break
  sleep 1
done
echo "mcp port answered HTTP $code"   # 000: never came up; read serve.log or the journal
```

Then check, in order:

1. The writer stays up: `launchctl print gui/$(id -u)/dev.lambo.dogfood` shows `state = running` and the pid does not change over a minute (Linux: `systemctl --user status` shows `active (running)` with a stable main pid). A crash at start with a pinned `device` is the accelerator backend, not the store.
2. Every pinned session is held by the new process: `"$RIG/bin/lambo-$V" --config "$RIG/lambo.toml" stats --store | grep -E "^session|lease:"` shows a holder starting `http-shared-writer@<host>#<new pid>` on every session. A session still answering `503` or showing no writer usually means the old writer did not release its lease; it clears when the 45-second TTL runs out.
3. The ledger names the new binary. The first heartbeat is written as soon as a session attaches, one `stats` line per session (`src/mcp/serve/heartbeat.rs`):

```sh
jq -r 'select(.kind=="stats") | [.ts, .session, .version, .git_sha] | @tsv' "$RIG/calls.jsonl" | tail -n 10
```

The newest lines must show `<version>` and the release commit's 7-character sha. `unknown` means the binary was built without `LAMBO_GIT_SHA`; an older version means the unit still names the old binary.

4. The log since the restart (`serve.log`, or `journalctl --user -u lambo-dogfood.service --since "<restart time>"`) has no `WARN` or `ERROR` line, in particular no embedder canary drift and no `missing_vectors` warning.
5. From an agent connected to a project URL: `lambo_stats` names that session on its first line (`session '<name>' (owner agent 'http-shared-writer')`), reports `embedded=N/N` with both numbers equal, and `degraded=false`; `lambo_recall` on a topic the session holds returns hits with a non-null `top_legs.vector_cosine` and no embedder warning. A broken embedder degrades recall to keyword-only with a warning; that warning is the failure signal.

If any check fails, roll back before debugging.

### Roll back

```sh
# macOS
PID=$(launchctl print gui/$(id -u)/dev.lambo.dogfood | awk '$1 == "pid" {print $3; exit}')
launchctl bootout gui/$(id -u)/dev.lambo.dogfood
while [ -n "$PID" ] && kill -0 "$PID" 2>/dev/null; do sleep 0.5; done
cp "$RIG/dev.lambo.dogfood.plist.bak-$OLD" "$HOME/Library/LaunchAgents/dev.lambo.dogfood.plist"
plutil -lint "$HOME/Library/LaunchAgents/dev.lambo.dogfood.plist"
launchctl bootstrap gui/$(id -u) "$HOME/Library/LaunchAgents/dev.lambo.dogfood.plist"

# Linux
systemctl --user stop lambo-dogfood.service
cp "$HOME/.config/systemd/user/lambo-dogfood.service.bak-$OLD" "$HOME/.config/systemd/user/lambo-dogfood.service"
systemctl --user daemon-reload
systemctl --user start lambo-dogfood.service
```

If the old binary refuses the store `<version>` provisioned, or step 1 of "Before you start" found that the upgrade cannot be read by older binaries, restore the pre-upgrade backup before the start line, with the writer stopped. This discards everything written since the upgrade. The earliest `backup-pre-<version>-*.db` is the pre-upgrade copy:

```sh
BK=$(ls -1 "$RIG"/backup-pre-$V-*.db | head -1)
shasum -a 256 -c "$BK.sha256"           # Linux: sha256sum -c
cp "$BK" "$STORE" && rm -f "$STORE-wal" "$STORE-shm"
```

Downgrading across a release that changes stored identities (durable names arrived in 0.12.0) is unsupported except by restoring the backup with its matching binary and configuration (`docs/reference/migration.mdx`). Then run [Verify the upgrade](#verify-the-upgrade) against the old version. Keep `lambo-<old version>`, the unit backup and the `backup-pre-<version>-*` files until the new binary has served for a while.

## Client wiring

Every client is registered under the server name `lambo-dogfood`, because the rig's protocol hooks identify that name. A client on a rig that serves several sessions has two entries of that name: a user-level fallback at `http://127.0.0.1:7700/mcp`, which serves `[serve] default_session`, and a per-project override at `http://127.0.0.1:7700/mcp/s/<session>` in each mapped project folder. The HTTP server cannot tell which folder a client runs in; the URL is the only selector (`docs/reference/config.mdx`, "Project URLs and a shared layer").

| Client | User fallback | Per-project override |
|---|---|---|
| Claude Code | `claude mcp add --transport http --scope user lambo-dogfood http://127.0.0.1:7700/mcp` | In the project folder: `claude mcp add --transport http --scope local lambo-dogfood http://127.0.0.1:7700/mcp/s/<session>` (stored privately in `~/.claude.json` under that folder; never `--scope project`, which writes `.mcp.json` into the repository) |
| Codex | `~/.codex/config.toml`: `[mcp_servers.lambo-dogfood]` with `url = "http://127.0.0.1:7700/mcp"` | `.codex/config.toml` in the project, same table, `url = "http://127.0.0.1:7700/mcp/s/<session>"` (read only for a trusted project) |
| Cursor and its CLI | `~/.cursor/mcp.json`: `{"mcpServers":{"lambo-dogfood":{"url":"http://127.0.0.1:7700/mcp"}}}` | `.cursor/mcp.json` in the project, same shape with the session URL |
| Gemini CLI | `~/.gemini/settings.json`: `{"mcpServers":{"lambo-dogfood":{"httpUrl":"http://127.0.0.1:7700/mcp"}}}` | `.gemini/settings.json` in the project, same shape with the session URL |
| opencode | `~/.config/opencode/opencode.json`: `{"mcp":{"lambo-dogfood":{"type":"remote","url":"http://127.0.0.1:7700/mcp","enabled":true}}}` | `opencode.json` in the project, same shape with the session URL |
| Copilot CLI | `~/.copilot/mcp-config.json`: `{"mcpServers":{"lambo-dogfood":{"type":"http","url":"http://127.0.0.1:7700/mcp"}}}` | none verified |

The per-project files are machine-local: never commit them, and check `git status` before every commit in a mapped project. Clients without a verified per-project override stay on the fallback, which means the default session, not the project's.

Rules for every client:

- **Change every config layer that names the server.** A layer that still holds `command` beside a new `url` is rejected even when the endpoint answers (`docs/reference/cli.mdx`, "stdio versus HTTP for several clients"). After any change, list the client's MCP servers from the project folder and from a folder outside it, and check the URL each resolves to.
- **Worktrees are separate folders.** A per-folder entry (Claude Code local scope, the project files above) does not follow a git worktree, so an agent in a worktree without its own entry uses the fallback and writes to the default session. Register the worktree folder too, or give the agent the session URL. See [Worktree inheritance check](#worktree-inheritance-check).
- **OMP** loads every globally configured MCP server and a global server shadows a workspace one of the same name (`evidence/swarm/probes/README.md`), so register `lambo-dogfood` in one place only.
- **`agent_id`** is the agent's model ID (for example `claude-opus-5-5`), kept identical for every call of that agent's life, never a task or role name; the harness name is the fallback when the model ID is unknown. A client's URL registration cannot set it, so it reaches agents through their instructions.

The tools alone do not make an agent use memory. Agents that read `AGENTS.md` get the protocol from its "Consulting memory (Lambo MCP)" section. An orchestrator that briefs subagents recalls first and puts the hits in the brief. A harness without `AGENTS.md` support (small local models under OMP or Pi) needs the protocol as its system prompt, in the pattern of `skills/lambo-cloudops/SKILL.md`.

## Smoke test

From each registered client, in a mapped project folder:

1. Call `lambo_stats`. The first line must name the project's session; in an unmapped folder it must name the default session.
2. Call `lambo_recall` with a topic the session holds. A new, empty session answers with no hits, which is correct.
3. Check that the ledger recorded the calls and that the binary is identified:

```sh
tail -n 3 <rig dir>/calls.jsonl
jq -r 'select(.kind=="stats") | [.ts, .session, .version, .git_sha] | @tsv' <rig dir>/calls.jsonl | tail -n 3
```

The tail must hold `call` lines for the two calls, with the client's `agent_id` and the addressed `session` (a heartbeat line can sit between them). A missing or empty file means the unit lacks `--ledger`; a `git_sha` of `unknown` means the binary was built without `LAMBO_GIT_SHA`. Either way the rig serves memory and measures nothing, which is the failure this check exists to catch.

## If a stop stalls

The writer logs each shutdown stage as it starts and finishes (`lambo serve: shutdown stage N/7 <name> started` and `... finished in <ms> ms`), and stage 3 logs its own steps (`close: step N/10 <name> started`) (`src/mcp/serve/stages.rs`). The last `started` with no matching `finished` is the stage that stalled. A `shutdown watchdog: stage N/7 <name> has run M ms, past its B ms bound` line means that stage's own timer never fired: the runtime was wedged. At 20 seconds the watchdog logs `shutdown still running ... aborting` and aborts the process; on macOS the abort writes a crash report with every thread's stack to `~/Library/Logs/DiagnosticReports/lambo-*.ips`. Keep that report with `serve.log` (or the journal) and the ledger from the same window. Writes that had not reached the store when the process aborted are lost; the lease lapses at its TTL.

## Load tests and gates on a rig machine

- Run harnesses against a scratch copy of the store, served by a scratch `lambo serve` on its own port with its own `XDG_RUNTIME_DIR` (`XDG_RUNTIME_DIR=$(mktemp -d) lambo serve ... --port 17701`). A scratch serve that shares the user's runtime directory with the live writer can find the live writer's session endpoint and forward to it. The harnesses in `scripts/loadtest/` refuse port 7700 unless `--allow-production-port`; do not pass it on a rig (`scripts/loadtest/README.md`).
- On the Metal rig, start scratch serves for `mcp_ab.py` with `--rate-limit-rps 0`, or the A/B runs into the default 50 requests per second and retries.
- Bound CPU-pressure generators when you start them, for example `timeout 300 sh -c 'while :; do :; done' &`, never with a trailing `kill` in the same command: an interrupted command leaves the spinners running with nothing to stop them.

## Multi-session cut-over runbook

This procedure moves a rig from one mixed session to one session per project, with a shared `general` session layered under each. The Metal rig was cut over this way on 2026-10-13 with lambo 0.6.0 (`evidence/0.6.0-cutover/README.md`); it remains the procedure for a rig that still serves one mixed session. It changes the store, the writer's configuration and every client, so the owner chooses when to run it and approves each stop point. Never perform it against the live store, and never delete the source.

### Prerequisites and stop points

- **Binary.** lambo 0.6.0 or later: per-session URLs arrived in 0.4.0, and layers, `scope` on the tools and `lambo session classify`, `split` and `verify` in 0.6.0 (`CHANGELOG.md`). Install and verify it with [Upgrading the pinned binary](#upgrading-the-pinned-binary) first, as a separate step.
- **No durable names in the source session.** `lambo session split` refuses a source that holds durable names or retired-alias history (`src/session/split.rs`; `docs/reference/migration.mdx`, "Recovery and compatibility"). Names arrived in 0.12.0. If the source holds any, stop: the split cannot run until identity remapping is implemented.
- **No owed writes.** `split` refuses a source with pending write intents. Serve the source once so they settle, then take the backup.
- **Embedding contracts agree.** Every target session and the `general` layer must have the same contract (kind, model, dim) as the source. A different model or width is not something layering works around.
- **Records kept privately, outside the repository:** the old binary, `lambo.toml` and unit; the session, default and URL map; the last write before the freeze; and where each client's registration lives. Have the rollback destination for every client before stopping any of them. Plans, label files and reports contain store ids and classification evidence: keep them private.

### Serving configuration

Add this to the rig's `lambo.toml`, keeping its `[store]` and `[embedder]` tables, with the rig's own session names:

```toml
[serve]
sessions = ["<project-a>", "<project-b>", "general", "general-usage"]
default_session = "general-usage"
layers = ["general"]

[[serve.layer]]
session = "general"
weight = 0.5
```

`general` holds operator-wide rulings and is not itself layered. Every other pinned session reads it beside its own memories. The fallback is `general-usage`, not `general`, so an unmapped folder never writes ordinary work into the ruling session. A layer and every session that reads it must be pinned; a session opts out with an empty list under `[serve.session_layers]`; one layer per session is the limit in this release (`docs/reference/config.mdx`, "Layers"). An unset `min_similarity` is 0.50 under BGE-M3. With a `[serve]` table, start the writer without `--session` so `default_session` controls `/mcp`. On SQLite the serve attaches one session at a time, and a lease lost on one session detaches only that session.

The loopback rig's implicit `local` credential reaches every pinned session. Adding a `[[serve.credential]]` removes it and changes every client's authentication; that is a separate change.

| Client location | URL | Text recall reads | Writes go to |
|---|---|---|---|
| A mapped project | `http://127.0.0.1:7700/mcp/s/<session>` | that session and `general` | that session |
| Unmapped folder | `http://127.0.0.1:7700/mcp` | `general-usage` and `general` | `general-usage` |
| Operator-wide work | `http://127.0.0.1:7700/mcp/s/general` | `general` alone | `general` |

An agent on a project URL writes an operator-wide ruling with `scope = "general"`. Writes to different sessions have no ordering between them, a write into `general` refuses relative file paths, and a scoped write's receipt answers only at the URL that wrote it (`docs/reference/mcp.mdx`).

### Client registration

Register each client as in [Client wiring](#client-wiring): the fallback at `/mcp` and a per-project override at `/mcp/s/<session>` for each mapped project. Do this only after step 5 of the checklist passes. Clients without a verified per-project override stay on `/mcp`, which is `general-usage`.

### Worktree inheritance check

Do not assume a folder inside a project inherits that project's registration. Before cut-over, test the installed client versions:

1. In a mapped project folder, open the client's MCP list and confirm `lambo-dogfood` resolves to `/mcp/s/<project>`. Call `lambo_stats`; it must name that session.
2. Repeat inside a worktree the client created (Claude Code: `.claude/worktrees/<name>`), both for a fresh launch there and for a session that moved into the worktree.
3. Repeat in a sibling worktree (`../<repo>-wt-*`) and in an unmapped folder. The unmapped folder must report `general-usage`.
4. Where a mapped worktree falls back to `/mcp`, add that folder's own entry, restart the client and test again. Do not infer success from a file existing on disk.

A failing check is a stop point. Git does not copy untracked override files into a new worktree.

### Cut-over and smoke-test checklist

1. **Freeze writes.** Pause every client. Stop the writer as in [Swap the binary in](#swap-the-binary-in-macos) steps 2 to 4 (no lease held, no owed intent). Take a backup with step 5. Copy that backup to a work directory and do all classification and splitting on the copy, never the source.
2. **Classify and review.**

```sh
lambo session classify --session <source session> --database <backup copy> \
  --rules <rules.toml> --output <plan.json>
```

The rules map projects by folder, agent-id pattern and keyword (`docs/reference/cli.mdx`, "Classify a session before splitting"). Resolve the low-confidence items with the owner through `--reviewed-assignments` or audited `--overrides`, and regenerate the plan; a plan cannot be edited by hand.

3. **Split and verify, offline.**

```sh
lambo session split --session <source session> --database <backup copy> \
  --plan <plan.json> --rules <rules.toml> \
  --output-database <rig dir>/serving/<split name>.db --report <split-report.json>
lambo --config <serving lambo.toml> session verify --session <source session> \
  --database <backup copy> --plan <plan.json> \
  --split-database <rig dir>/serving/<split name>.db \
  --queries <queries.toml> --report <verify-report.json>
```

Pass `--reviewed-assignments` and `--overrides` to `split` exactly when the plan used them. Run `split` with `--dry-run` first. The split report must account for every source row and list every dropped cross-session edge; review the dropped edges and the low-confidence and general-provenance notices with the owner. Run `split` a second time: it must write nothing. `verify` needs the serving `lambo.toml` (with `[store] kind = "sqlite"` set explicitly and the new `[serve]` table) and compares each project's layered recall with the old mixed recall; stop on any loss the owner has not accepted. Pass `--aggregate-only <session>` for a session whose figures must not appear in the report.

4. **Start the new serve.** Point `[store] path` at the split output, add the `[serve]` table, remove any `--session` from the unit, run `lambo --config <rig dir>/lambo.toml provision`, and start the writer. Before rewiring any client, check the log for contract or credential refusals, check that every pinned session's lease is held (`stats --store`), and check that `/mcp` serves `general-usage`.
5. **Validate every URL.** `lambo_stats` at each URL reports the addressed session. `lambo_recall` without `scope` shows project and `general` hits, each with its session; a query with no relevant `general` hit need not show one, because the layer floor still applies. Recall with `scope` set to the project or to `general` reads that session alone. `lambo_inspect` of a `general` hit works with its id and `scope: "general"`; a `scope` naming another project is refused.
6. **Check writes without mixing scopes.** In a disposable test session the owner approves, a derive without `scope` lands only in the addressed session, with no copy in `general`. With `scope: "general"` it lands only in `general`, its receipt answers at the project URL, and a relative file path is refused.
7. **Check degradation and the ledger.** In a staged copy, with `general` unavailable, a project's recall answers from the project alone with one warning, and a scoped read of `general` is refused. Ledger recall lines name the sessions their hits came from; each heartbeat line names its session.
8. **Resume.** Only after the owner accepts the checks, wire the clients and resume them. Make the source store read-only (`chmod 444 <old store path>`) and keep it; never erase it. Keep the configuration, backup and URL records until the owner closes the rollback window.

### Rollback and escalation

Before any write reaches the new sessions, rollback is: pause clients, stop the new writer, restore the saved single-session `lambo.toml`, unit and binary, restore every client's old URL, start the writer, and confirm `lambo_stats` reports the old session with one writer holding it. Keep the split output and reports for investigation.

After the new sessions have accepted writes, do not point clients back at the archived source: that loses the new history. Freeze clients, stop the writer, back up the split store, and ask the owner to choose between reconciling and continuing; there is no automatic merge back. An older binary may need its saved configuration, and may not read edge types a newer binary stored.

Escalate to the owner before resuming writes on any session or URL mismatch, lease conflict, contract mismatch, missing layer, or unexplained split accounting or recall difference. Never delete the source and never use `erase-session` as a rollback.

## Reference embedder for parity captures

A `llama-server` running a BGE-M3 GGUF is the independent reference the candle embedder was measured against (`evidence/mooshik-k1-metal/README.md`). The rig does not need it, and it is not a drop-in fallback: a store embedded under `kind = "candle"` refuses a writer configured with `kind = "bge_m3"` until it is re-embedded. To reproduce a parity capture, use the exact artifact the evidence names (`bge-m3-q8_0.gguf`, sha256 `aa473d51f451a22f0fcf39ba3330c14bed38a385712b1113440f69df4047a173`), bind it to `127.0.0.1`, and start it with `--batch-size 8192 --ubatch-size 8192 -c 8192`: without the two batch flags, llama-server limits a batch to 512 tokens and answers HTTP 500 for longer input, while `/health` still passes. Check with an embed of more than 512 tokens, not with `/health`. `scripts/run-llama-embed.sh` sets `-c` only and passes extra arguments through, so append the two batch flags when using it; `scripts/fetch-bge-m3.sh` downloads a different artifact (the FP16 GGUF), which is not the parity reference.
