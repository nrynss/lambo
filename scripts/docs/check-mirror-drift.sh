#!/usr/bin/env bash
# Fail if either hand-maintained mirror pair of docs has drifted.
#
# The `--ledger`/transport prose exists in four copies:
#   docs/reference/cli.mdx        <-> site/src/content/docs/cli.mdx
#   docs/reference/mcp.mdx        <-> site/src/content/docs/mcp.mdx
# The two copies of each pair are meant to carry the SAME shared prose, but they
# are deliberately NOT byte-identical files: the site copies add Astro component
# imports and a `/lambo/...` link prefix, and mcp.mdx's site copy carries a whole
# "Verified clients" / managed-CockroachDB section the docs copy does not (see the
# correction in dev-diary/lambo-for-mooshik/J-multi-client.md, J2 round-1 review).
# A raw `diff` of the pair would therefore be red the day it landed.
#
# So this gate compares each pair's canonical "shared prose" form: it strips the
# Astro imports, drops the site-only mcp.mdx section, and normalises the /lambo/
# link prefix and trailing slashes. If the canonical forms differ, the shared
# prose has drifted and the gate fails. The J5 rule: keep the pairs in sync, and
# re-run `scripts/docs/check-mirror-drift.sh` after touching any of the four files.
#
# CI runs this from .github/workflows/ci.yml (the `docs-mirror` job).

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

fail=0

# Canonicalise an mdx file down to its shared-prose form.
canon() {
  python3 - "$1" <<'PY'
import re, sys
with open(sys.argv[1]) as f:
    text = f.read()
lines = []
site_only = False
for ln in text.split('\n'):
    # Astro component imports (site copy only).
    if re.match(r"^import \S+ from '[^']*\.astro';$", ln):
        continue
    # The site-only mcp.mdx block, from "## Verified clients" up to (but not
    # including) the shared "## Limits" section.
    if ln.strip() == '## Verified clients':
        site_only = True
        continue
    if site_only and ln.strip() == '## Limits':
        site_only = False
    if site_only:
        continue
    lines.append(ln)
t = '\n'.join(lines)
# Normalise markdown internal links: ](/lambo/page[/][#frag]) -> ](/page[#frag]).
t = re.sub(r'\]\(/lambo/([a-z0-9-]+)/(#\S*)?\)',
           lambda m: '](/' + m.group(1) + (m.group(2) or '') + ')', t)
t = re.sub(r'\]\(/lambo/([a-z0-9-]+)(#\S*)?\)',
           lambda m: '](/' + m.group(1) + (m.group(2) or '') + ')', t)
# Normalise any remaining bare `/lambo/` prefix (e.g. in code spans).
t = t.replace('/lambo/', '/')
sys.stdout.write(t)
PY
}

check_pair() {
  name="$1"; ref="$2"; site="$3"
  if ! diff -u <(canon "$repo/$ref") <(canon "$repo/$site") >/dev/null; then
    printf '\nFAIL %s: shared prose has drifted between %s and %s\n' "$name" "$ref" "$site"
    diff -u <(canon "$repo/$ref") <(canon "$repo/$site") | sed -n '1,40p'
    fail=1
  else
    printf 'ok    %s: reference and site copies agree on the shared prose\n' "$name"
  fi
}

check_pair cli docs/reference/cli.mdx site/src/content/docs/cli.mdx
check_pair mcp docs/reference/mcp.mdx site/src/content/docs/mcp.mdx

printf '\n'
if [ "$fail" -eq 0 ]; then
  printf 'mirror drift check passed\n'
else
  printf 'mirror drift check FAILED\n'
fi
exit "$fail"
