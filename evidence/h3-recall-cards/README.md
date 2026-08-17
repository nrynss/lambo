# H3 recall cards evidence

Live/browser evidence for H3 ("Structured recall results beside the verbatim
block"): a real writer session in SQLite, read by a local `lambo serve-web`,
captured by `scripts/recording/capture-portal.mjs` (updated for the current
DOM and for local serve-web). Captures are unedited; the `.png`/`.webm` files
are exactly what the browser produced.

## What each artifact shows

| File | What it shows |
|---|---|
| `cards-blended-blended-default.png` | Cards view for `update user schema`: the Canonical pillar card with its `Canonical` status badge, score bar, `blast radius 9`, and the `load_bearing` annotation ("⚑ Load-bearing pillar — 9 nodes depend on this…"); plain cards below (score bars, no warnings). |
| `cards-structural-structural-default.png` | Cards view for `what depends on SG-Base-VPC`: the traversal banner (`response_annotations`, kind `traversal` — "…answered by graph traversal (1 dependents)") rendered prominently above the cards; the dependent `RDS-Lambo-Demo-DB` card. |
| `cards-tiny-budget-tiny-budget-24.png` | Cards view under a forced `max_tokens=24` (Playwright route interception): every card is collapsed (`is-excluded`, "Outside the context budget"), and the persistent **"Warnings from results outside the context budget"** area lists the excluded `user schema` hit's `load_bearing` warning, labelled with the owning hit — the same warning that renders in the verbatim header. |
| `cards-xss-xss-default.png` | Cards view for the query that recalls `malicious markup <img src=x onerror=window.__h3xss=1>`: the untrusted text renders as text (the script asserts no `<img>` element is created, the marker appears verbatim, and `window.__h3xss` never fires). |
| `verbatim-context.png` | The verbatim context view (fallback toggle), showing the exact `lambo recall` block with the `[Entity, canonical]` marker and the ⚑ line — H3 keeps it byte-identical. |
| `audit-feed.png` | The canonization feed (`#audit`) on the same session. |
| `*.webm` | The full capture as a browser-context video. |
| `capture-<utc>.txt` | Capture metadata: portal URL, session, queries. |

## How to reproduce

Provision a SQLite store, write a demo session, seed the structural and
malicious concepts, serve, capture:

```sh
mkdir -p /tmp/h3-evidence && cat > /tmp/h3-evidence/lambo.toml <<'EOF'
[store]
kind = "sqlite"
path = "/tmp/h3-evidence/h3.sqlite"
[embedder]
kind = "fixture"
dim = 1024
EOF

lambo --config /tmp/h3-evidence/lambo.toml provision
lambo --config /tmp/h3-evidence/lambo.toml demo --scenario rest-api --session h3-evidence
lambo --config /tmp/h3-evidence/lambo.toml derive --session h3-evidence --agent agent-a \
  --content "SG-Base-VPC" --kind entity --parent-of "RDS-Lambo-Demo-DB:SG-Base-VPC" \
  --concept "RDS-Lambo-Demo-DB:entity"
lambo --config /tmp/h3-evidence/lambo.toml derive --session h3-evidence --agent agent-a \
  --content "malicious markup <img src=x onerror=window.__h3xss=1>" --kind observation

lambo --config /tmp/h3-evidence/lambo.toml serve-web --session h3-evidence --port 7799 &

PLAYWRIGHT_EXECUTABLE=$HOME/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome \
  PORTAL=http://127.0.0.1:7799 \
  node scripts/recording/capture-portal.mjs
```

The script exits non-zero if the browser logs an unexpected error or any of
the H3 checks fail (excluded cards collapsed, excluded warning area populated,
XSS text rendered as text, verbatim view available). It needs the `playwright`
package (resolved via an absolute import; install into the worktree with
`npm i -D playwright` and revert the import to `from 'playwright'` if a
`node_modules` is ever added here) and a chromium build in
`~/.cache/ms-playwright`.
