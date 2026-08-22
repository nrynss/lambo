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
# ## The normalisation's one asymmetric input (JE2E-10)
#
# J5 round 1 argued that the /lambo/ normalisation "is symmetric ... so it cannot
# mask a real shared-line difference". That is true of the direction it was
# reasoned about and false of the other one: the strip is applied to BOTH sides,
# so a site-style `/lambo/config/#http-transport` link pasted into the
# **reference** copy normalises to the same canonical form as the site copy's
# correct link — and passes green while being a broken link on the docs site,
# which serves no /lambo/ prefix. Demonstrated live by the E2E reviewer, and
# reproduced here before the fix.
#
# So the reference copies are checked for that prefix FIRST, as their own gate,
# before anything is normalised. The site copies are not: /lambo/ is their
# correct prefix. This is the asymmetry the normalisation has to have and did
# not: the two copies are not interchangeable inputs, and only one of them may
# carry the prefix.
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
    # The site-only mcp.mdx block, delimited by explicit markers so a benign
    # rename of the site-only section's headings does not break the strip
    # (J5 round-1 P3). The site copy wraps its site-only content in
    # <!-- lambo-site-only:start --> ... <!-- lambo-site-only:end -->; the
    # reference copy carries no such block.
    if ln.strip() == '<!-- lambo-site-only:start -->':
        site_only = True
        continue
    if site_only and ln.strip() == '<!-- lambo-site-only:end -->':
        site_only = False
        continue
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

# A reference copy must carry no site-style `/lambo/` prefix (JE2E-10).
#
# Checked BEFORE `canon` runs, because `canon` is exactly what would hide it:
# the prefix strip is applied to both sides, so a site-style link in the
# reference copy canonicalises into agreement with the site copy's correct one.
# Run against the raw file, so nothing has been normalised yet.
check_no_site_prefix() {
  name="$1"; ref="$2"
  if grep -n '/lambo/' "$repo/$ref" >/dev/null 2>&1; then
    printf '\nFAIL %s: the reference copy %s carries a site-only /lambo/ link prefix\n' \
      "$name" "$ref"
    printf '      The docs site serves no /lambo/ prefix, so these are broken links there.\n'
    printf '      Only the site copy may carry it. Offending lines:\n'
    grep -n '/lambo/' "$repo/$ref" | sed -n '1,10p' | sed 's/^/        /'
    fail=1
    return 1
  fi
  printf 'ok    %s: the reference copy carries no site-only link prefix\n' "$name"
}

check_pair() {
  name="$1"; ref="$2"; site="$3"
  check_no_site_prefix "$name" "$ref" || true
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
