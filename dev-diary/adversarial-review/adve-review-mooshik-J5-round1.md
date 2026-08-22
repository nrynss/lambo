# Adversarial review — mooshik J5, round 1 (REVIEW-ONLY, base `0dd15b3`)

**Reviewer**: independent adversarial reviewer, agent_id `J5Reviewer`. REVIEW-ONLY — the
only file written is this disposition; no commit amended, no code changed.
**Scope**: the three commits on `wt/j5` — `7b8b9d9` (CI gate), `6879633` (four-mirror
prose), `a778235` (docs/decision note). **Verdict**: **APPROVE** — clean and ready to
integrate. The implementer's deviation from the brief's "byte-identical pairs + raw diff
gate" framing is **faithful** to the design record, and the replacement canonical
shared-prose gate is **meaningful and false-pass-free** on the load-bearing shared prose.
**HEAD `a778235` == `origin/wt/j5`; tree clean** (no untracked/modified files apart from
this doc).

## Deviation adjudication — the central question

The brief asked for "byte-identical pairs + raw diff gate". The implementer built a
**canonical shared-prose** drift gate instead, citing
`dev-diary/lambo-for-mooshik/J-multi-client.md:838-848` (J2 round-1 review) that the four
copies are **deliberately not raw-byte-identical**.

**Faithful — verified at the design record.** `J-multi-client.md` line 838 opens "A
correction for §J5, found while looking for the drift gate it asks for": §J5's "byte-identical
pairs" claim is **"already false at HEAD, and deliberately so"** — `cli.mdx` differs on 16
lines, `mcp.mdx` on 43, because the site copies add Astro component imports, rewrite every
internal link to a `/lambo/...` prefix, and `mcp.mdx`'s site copy carries a whole "Verified
clients" / managed-CockroachDB section the docs copy does not. It explicitly warns "the
one-line `diff` gate §J5 proposes would be **red on the day it landed**". The sweep-3 table
row for `mcp.mdx` (+ mirror) likewise records the "More than one client on one machine"
section as "byte-identical prose in both copies **with each copy's own link convention**".
So the deviation is a correction to a factual overstatement in the brief, not a dodge: a raw
byte `diff` could not have landed green, and the implementer's gate is the faithful form of
the brief's intended invariant (mirror pairs stay in sync). The gate script's own header
(cites the J2 correction) and the CI job comment both carry this rationale.

## Per-check verdicts

1. **Design record says non-byte-identical** — **PASS.** `J-multi-client.md:838-848` + the
   sweep-3 `mcp.mdx` row state it explicitly (Astro imports, `/lambo/` prefix, site-only
   `mcp.mdx` "Verified clients" section); the gate script and CI job both cite it.
2. **Gate meaningful and false-pass-free** — **PASS.** See gate table below. Verified by
   (a) mutating a shared mcp.mdx line → gate FAILS, exit 1, with the exact diff; (b)
   confirming the canonicaliser strips exactly the Astro imports, the site-only
   `## Verified clients`→`## Limits` block, and the `/lambo/` prefix upon a bare `/` — the
   reference copies carry none of these deltas (reference `mcp.mdx` has no "Verified
   clients" heading, `grep -c` = 0), so the strip affects only the site side and cannot
   hide a divergence in the shared prose. Normalisation is symmetric across a pair
   (identical Python for both files), so it cannot mask a real shared-line difference.
   One P3 observation (not a false-pass): the site-only boundary is keyed on the literal
   heading `## Verified clients`, so renaming that heading turns the gate spuriously red
   (demonstrated). Safe direction (false-fail, not false-pass); current content unaffected.
3. **Ordering held + green-before** — **PASS.** `git merge-base --is-ancestor 7b8b9d9
   6879633` → YES, and `6879633`→`a778235` → YES. The gate script + 5 files run at
   `7b8b9d9` (pre-prose-edit tree, extracted via `git show`) → green. It reconciled one
   genuine pre-existing drift in that commit: site `cli.mdx` "v0.2" → "v0.1", matching
   reference `cli.mdx` and `src/cli/demo.rs` (`demo.rs:164,513` say v0.1); both mirrors now
   read v0.1 at HEAD.
4. **CI wiring** — **PASS.** The `docs-mirror` job (`ci.yml:124-129`) runs
   `bash scripts/docs/check-mirror-drift.sh` and fails loudly (`set -euo pipefail`, exit 1
   propagates). The four mirror mdx paths **and** `scripts/docs/check-mirror-drift.sh` are
   listed in the path filter on **both `push` (lines 62-66) and `pull_request` (78-82)**,
   so a mirror-only push triggers CI. (Note: the `push` branch whitelist
   `[main, master, phase/**, lambo-for-mooshik]` is the pre-existing CI model; `wt/j5`
   itself is not whitelisted, but any merge into `lambo-for-mooshik` fires the job — same
   branch model as every other job on the branch.)
5. **Prose framing (all four mirrors)** — **PASS.** Both `cli.mdx` copies and both `mcp.mdx`
   copies carry the HTTP-for-independent-clients passage with the **single-writer** reason,
   and are **explicitly NOT** the "subagents need HTTP" framing: cli says "The reason is
   single-writer, not subagents"; mcp says "the plurality that makes stdio the wrong default
   is independent clients on one machine, **not the number of agents under a single
   orchestrator**", and both state that one orchestrator + subagents is one connection fine
   on stdio. Both carry the config-layering migration gotcha (transport touches every
   layer — project `.mcp.json`, user/global scope, per-client files; a layer still holding
   `command` beside the new `url` is rejected even when the endpoint responds; migrate by
   copying the registration into each layer). Canonical shared prose matches (gate green).
6. **`--print-client-config` decision note** — **PASS.** `a778235` documents it as
   decided-NOT-built and states why: a paste-ready emitter needs a client→registration-shape
   registry plus rig-specific operator paths (pinned binary, config, session, ledger,
   agent-ids from DOGFOOD-SETUP.md §4) that are DOGFOOD-rig configuration rather than lambo
   invariants, and the binary does not carry the resolved config path; a placeholder template
   would be a half-verb, so it is documented instead of scaffolded. Honest, reasonable
   disposition for a "consider" item.
7. **Gates spot-check / no Rust touched** — **PASS for the J5-relevant gates.** The diff
   `0dd15b3..HEAD` is **docs/CI/script only** — `git diff --name-only | grep -c '\.rs$'` = 0
   (files: ci.yml, J-multi-client.md, two docs/reference mirror mdx, two site mirror mdx,
   check-mirror-drift.sh). Because zero Rust source changed, the cargo suites are inherited
   and unimpacted; I did not re-run the full 900+ test matrices. Measured here: mirror-drift
   green before (`7b8b9d9`) and after (HEAD); `verify.sh` = **46 ok / ALL CHECKS PASSED**;
   `cargo fmt --all -- --check` clean. (The claimed fixtures 902 / sqlite 973 / cockroach
   559 / clippy ×4 are unmodified by J5 and carried from base.)

## Gate table

| Gate | Claimed | Measured (this review) |
|---|---|---|
| mirror-drift, pre-prose (`7b8b9d9`) | green | **green** (extracted tree, exit 0) |
| mirror-drift, HEAD | green | **green** (exit 0) |
| mirror-drift fails on shared drift | — | **fails** (mutated mcp line → FAIL, exit 1, correct diff) |
| mirror-drift false-pass on load-bearing | none | **none** — strips exactly Astro/`/lambo/`/Verified-clients block; reference carries no such deltas |
| `verify.sh` | 46 ok | **46 ok** (ALL CHECKS PASSED) |
| `cargo fmt --check` | clean | **clean** (exit 0) |
| fixtures 902 / sqlite 973 / cockroach 559 / clippy ×4 | claimed | unimpacted — **zero `.rs` files changed** (not re-run) |

## Findings

No blocking findings. One **P3 (informational)** observation, non-blocking:

- **Site-only boundary is keyed on the literal `## Verified clients` heading.** The
  canonicaliser's `site_only` flag arms only on the exact string `## Verified clients`.
  Renaming that heading (a legitimate site-only change) turns the gate spuriously red — I
  verified `## Verified clients` → `## Verified CLIENTSS` makes the pair FAIL even though no
  shared prose moved. Direction is false-fail (safe), and a load-bearing false-pass would
  require the marker to be duplicated into a reference copy, which it is not. A slightly more
  robust design would key the strip on a stable anchor (the `## Limits` boundary or a
  comment marker) rather than the heading text. Not a blocker; current content is green and
  the invariant is real.

## Overall

**APPROVE** — branch clean, HEAD `a778235` matches `origin/wt/j5`, tree clean apart from this
doc, no code touched. The deviation is faithful to the design record; the canonical
shared-prose gate is meaningful and false-pass-free; it landed first and green and enforced a
genuine drift fix; CI wiring covers both push and pull_request; the prose framing is correct
(single-writer, not subagents); and the `--print-client-config` not-built note is honest.

## Disposition — round-1 P3 (J5-R1-1) closed at remediation (operator-done)

The single round-1 finding (site-only strip keyed on the literal `## Verified clients`
heading -> a benign rename spuriously reds the gate) is closed:

- Wrapped the site-only mcp.mdx block in explicit markers
  `<!-- lambo-site-only:start -->` ... `<!-- lambo-site-only:end -->`
  (site mcp.mdx; invisible HTML comments; reference copy unchanged).
- `scripts/docs/check-mirror-drift.sh` now keys the site_only strip on those markers
  instead of the heading text.
- Verified: baseline drift gate green; renaming `## Verified clients` -> `## Supported
  CLIENTSS` leaves the gate GREEN (no spurious red); a genuine shared-prose mutation still
  FAILS the mcp pair (gate remains meaningful); revert -> green.
